import { describe, expect, it, vi } from "vitest";
import {
  AUDIO_CHANNELS,
  AUDIO_SAMPLE_RATE,
  AudioClock,
  type AudioWindow,
  type AudioWindowRequest,
  WINDOW_SAMPLES,
} from "./AudioClock";

type RecordedBuffer = {
  readonly numberOfChannels: number;
  readonly length: number;
  readonly sampleRate: number;
  readonly duration: number;
  readonly channelData: Float32Array[];
  copyToChannel(source: Float32Array, channel: number, bufferOffset?: number): void;
};

type RecordedStart = {
  readonly at: number;
  readonly offset: number;
  readonly duration: number;
};

type RecordedSource = {
  buffer: RecordedBuffer | null;
  readonly starts: RecordedStart[];
  onended: (() => void) | null;
  connect(): void;
  disconnect(): void;
  stop(): void;
  start(at: number, offset?: number, duration?: number): void;
};

type TestAudioContext = {
  currentTime: number;
  state: AudioContextState;
  close(): Promise<void>;
  readonly sampleRate: number;
  readonly buffers: RecordedBuffer[];
  readonly sources: RecordedSource[];
};

function createAudioContext(sampleRate: number, currentTime = 10): TestAudioContext {
  const buffers: RecordedBuffer[] = [];
  const sources: RecordedSource[] = [];
  const context: TestAudioContext & {
    state: AudioContextState;
    outputLatency: number;
    baseLatency: number;
    destination: object;
    getOutputTimestamp(): { contextTime: number; performanceTime: number };
    resume(): Promise<void>;
    createBuffer(numberOfChannels: number, length: number, rate: number): AudioBuffer;
    createBufferSource(): AudioBufferSourceNode;
  } = {
    currentTime,
    sampleRate,
    buffers,
    sources,
    state: "running",
    outputLatency: 0,
    baseLatency: 0,
    destination: {},
    getOutputTimestamp: () => ({ contextTime: context.currentTime, performanceTime: 0 }),
    resume: async () => {},
    close: async () => { context.state = "closed"; },
    createBuffer: (numberOfChannels, length, rate) => {
      const channelData = Array.from({ length: numberOfChannels }, () => new Float32Array(length));
      const buffer: RecordedBuffer = {
        numberOfChannels,
        length,
        sampleRate: rate,
        duration: length / rate,
        channelData,
        copyToChannel: (source, channel, bufferOffset = 0) => {
          channelData[channel]?.set(source, bufferOffset);
        },
      };
      buffers.push(buffer);
      return buffer as unknown as AudioBuffer;
    },
    createBufferSource: () => {
      const source: RecordedSource = {
        buffer: null,
        starts: [],
        onended: null,
        connect: () => {},
        disconnect: () => {},
        stop: () => {},
        start: (at, offset = 0, duration = Number.NaN) => {
          source.starts.push({ at, offset, duration });
        },
      };
      sources.push(source);
      return source as unknown as AudioBufferSourceNode;
    },
  };
  return context;
}

function makePcm(startSample: number, sampleCount: number): Float32Array {
  const pcm = new Float32Array(sampleCount * AUDIO_CHANNELS);
  for (let index = 0; index < sampleCount; index += 1) {
    const phase = (2 * Math.PI * 440 * (startSample + index)) / AUDIO_SAMPLE_RATE;
    pcm[index * AUDIO_CHANNELS] = 0.18 * Math.sin(phase);
    pcm[index * AUDIO_CHANNELS + 1] = 0.15 * Math.sin(phase + Math.PI / 5);
  }
  return pcm;
}

function makeWindow(planHash: string, startSample: number, sampleCount: number): AudioWindow {
  return {
    planHash,
    startSample,
    sampleCount,
    sampleRate: AUDIO_SAMPLE_RATE,
    channels: AUDIO_CHANNELS,
    pcm: makePcm(startSample, sampleCount),
  };
}

function deferred<T>(): { promise: Promise<T>; resolve(value: T): void } {
  let resolve!: (value: T) => void;
  const promise = new Promise<T>((promiseResolve) => {
    resolve = promiseResolve;
  });
  return { promise, resolve };
}

async function settleMicrotasks(): Promise<void> {
  await Promise.resolve();
  await Promise.resolve();
  await Promise.resolve();
  await Promise.resolve();
}

describe("AudioClock scheduling", () => {
  it.each([false, true])("releases only owned contexts on disposal (borrowed=%s)", async (borrowed) => {
    const context = createAudioContext(AUDIO_SAMPLE_RATE);
    vi.stubGlobal("AudioContext", class { constructor() { return context; } });
    const clock = new AudioClock({
      ...(borrowed ? { audioContext: context as unknown as AudioContext } : {}),
      getWindow: async (request) => makeWindow("ownership", request.startSample, request.sampleCount),
    });
    try {
      clock.setPlan("ownership", 1);
      await clock.play(0);
      clock.dispose();
      clock.dispose();
      expect(context.state).toBe(borrowed ? "running" : "closed");
    } finally {
      clock.dispose();
      vi.unstubAllGlobals();
    }
  });

  it("anchors a mid-window seek at a future context time with exact offset and boundary timing", async () => {
    const context = createAudioContext(AUDIO_SAMPLE_RATE, 12.25);
    const planHash = "mid-window-plan";
    const anchorSample = WINDOW_SAMPLES + 12_345;
    const totalSamples = WINDOW_SAMPLES * 3;
    const clock = new AudioClock({
      audioContext: context as unknown as AudioContext,
      getWindow: async (request) => makeWindow(planHash, request.startSample, request.sampleCount),
    });

    try {
      clock.setPlan(planHash, totalSamples);
      await clock.play(anchorSample);

      expect(clock.state).toBe("playing");
      expect(context.sources).toHaveLength(2);
      const first = context.sources[0]?.starts[0];
      const second = context.sources[1]?.starts[0];
      expect(first).toBeDefined();
      expect(second).toBeDefined();
      if (!first || !second) return;

      const firstWindowRemainder = WINDOW_SAMPLES - (anchorSample - WINDOW_SAMPLES);
      const expectedOffset = (anchorSample - WINDOW_SAMPLES) / AUDIO_SAMPLE_RATE;
      const expectedBoundaryDelta = firstWindowRemainder / AUDIO_SAMPLE_RATE;

      expect(first.at).toBeGreaterThanOrEqual(context.currentTime);
      expect(first.offset).toBeCloseTo(expectedOffset, 12);
      expect(first.duration).toBeCloseTo(expectedBoundaryDelta, 12);
      expect(second.offset).toBe(0);
      expect(second.duration).toBeCloseTo(5, 12);
      expect(second.at - first.at).toBeCloseTo(expectedBoundaryDelta, 12);
      // No refill timer advances; presentation must read the live device position.
      context.currentTime = first.at + 0.01;
      expect(Math.abs(clock.sample - (anchorSample + 480))).toBeLessThanOrEqual(1);
      context.currentTime = first.at + 0.02;
      expect(Math.abs(clock.sample - (anchorSample + 960))).toBeLessThanOrEqual(1);
    } finally {
      clock.dispose();
    }
  });

  it("keeps 48 kHz buffer coordinates and durations on a 44.1 kHz output context", async () => {
    const context = createAudioContext(44_100, 3.5);
    const planHash = "device-rate-plan";
    const finalRemainder = 1_234;
    const totalSamples = WINDOW_SAMPLES + finalRemainder;
    const clock = new AudioClock({
      audioContext: context as unknown as AudioContext,
      getWindow: async (request) => makeWindow(planHash, request.startSample, request.sampleCount),
    });

    try {
      clock.setPlan(planHash, totalSamples);
      await clock.play(0);

      expect(context.sampleRate).toBe(44_100);
      expect(context.buffers.map((buffer) => [buffer.numberOfChannels, buffer.length, buffer.sampleRate, buffer.duration])).toEqual([
        [AUDIO_CHANNELS, WINDOW_SAMPLES, AUDIO_SAMPLE_RATE, WINDOW_SAMPLES / AUDIO_SAMPLE_RATE],
        [AUDIO_CHANNELS, finalRemainder, AUDIO_SAMPLE_RATE, finalRemainder / AUDIO_SAMPLE_RATE],
      ]);
      expect(context.sources.map((source) => source.starts[0]?.duration)).toEqual([
        WINDOW_SAMPLES / AUDIO_SAMPLE_RATE,
        finalRemainder / AUDIO_SAMPLE_RATE,
      ]);
    } finally {
      clock.dispose();
    }
  });

  it("does not schedule a deferred window from a superseded seek generation", async () => {
    const context = createAudioContext(AUDIO_SAMPLE_RATE, 6);
    const planHash = "stale-window-plan";
    const oldWindow = deferred<AudioWindow>();
    let firstRequest = true;
    const clock = new AudioClock({
      audioContext: context as unknown as AudioContext,
      getWindow: async (request) => {
        if (request.startSample === 0 && firstRequest) {
          firstRequest = false;
          return oldWindow.promise;
        }
        return makeWindow(planHash, request.startSample, request.sampleCount);
      },
    });

    try {
      clock.setPlan(planHash, WINDOW_SAMPLES);
      const supersededPlay = clock.play(0);
      await settleMicrotasks();

      await clock.seek(0);
      expect(clock.state).toBe("playing");
      expect(context.sources).toHaveLength(1);

      oldWindow.resolve(makeWindow(planHash, 0, WINDOW_SAMPLES));
      await supersededPlay;
      await settleMicrotasks();

      expect(context.sources).toHaveLength(1);
      expect(context.sources.flatMap((source) => source.starts)).toHaveLength(1);
    } finally {
      clock.dispose();
    }
  });

  it("waits at the contiguous boundary instead of scheduling a later window first", async () => {
    vi.useFakeTimers();
    const context = createAudioContext(AUDIO_SAMPLE_RATE, 10);
    const planHash = "contiguous-window-plan";
    const missingWindow = deferred<AudioWindow>();
    const requests: AudioWindowRequest[] = [];
    const clock = new AudioClock({
      audioContext: context as unknown as AudioContext,
      getWindow: async (request) => {
        requests.push(request);
        if (request.startSample === WINDOW_SAMPLES * 2) return missingWindow.promise;
        return makeWindow(planHash, request.startSample, request.sampleCount);
      },
    });

    try {
      clock.setPlan(planHash, WINDOW_SAMPLES * 4);
      await clock.play(0);

      expect(requests.map((request) => request.startSample)).toEqual([
        0,
        WINDOW_SAMPLES,
        WINDOW_SAMPLES * 2,
        WINDOW_SAMPLES * 3,
      ]);
      expect(context.sources).toHaveLength(2);

      const firstScheduledAt = context.sources[0]?.starts[0]?.at;
      expect(firstScheduledAt).toBeDefined();
      if (firstScheduledAt === undefined) return;
      const initialContextTime = context.currentTime;
      const scheduleLead = firstScheduledAt - initialContextTime;

      const secondScheduledAt = context.sources[1]?.starts[0]?.at;
      expect(secondScheduledAt).toBeDefined();
      if (secondScheduledAt === undefined) return;
      expect(secondScheduledAt).toBeCloseTo(firstScheduledAt + WINDOW_SAMPLES / AUDIO_SAMPLE_RATE, 12);

      // Both five-second windows 0 and 1 are already scheduled. The missing
      // window starts at their actual contiguous boundary, so advance the
      // reported audible clock to that boundary rather than treating the
      // first window as the whole scheduled prefix.
      const contiguousBoundary = WINDOW_SAMPLES * 2;
      context.currentTime = firstScheduledAt + contiguousBoundary / AUDIO_SAMPLE_RATE + 1e-6;
      await vi.advanceTimersByTimeAsync(50);

      expect(clock.state).toBe("buffering");
      expect(clock.sample).toBe(contiguousBoundary);
      expect(context.sources).toHaveLength(2);
      expect(context.sources.flatMap((source) => source.starts)).toHaveLength(2);

      missingWindow.resolve(makeWindow(planHash, WINDOW_SAMPLES * 2, WINDOW_SAMPLES));
      await vi.advanceTimersByTimeAsync(0);

      expect(clock.state).toBe("playing");
      expect(clock.sample).toBe(contiguousBoundary);
      expect(context.sources).toHaveLength(4);
      expect(context.sources.flatMap((source) => source.starts)).toHaveLength(4);
      const resumedFirstAt = context.sources[2]?.starts[0]?.at;
      const resumedSecondAt = context.sources[3]?.starts[0]?.at;
      expect(resumedFirstAt).toBeDefined();
      expect(resumedSecondAt).toBeDefined();
      if (resumedFirstAt === undefined || resumedSecondAt === undefined) return;
      expect(resumedFirstAt).toBeCloseTo(context.currentTime + scheduleLead, 12);
      expect(resumedSecondAt).toBeCloseTo(resumedFirstAt + WINDOW_SAMPLES / AUDIO_SAMPLE_RATE, 12);
    } finally {
      clock.dispose();
      vi.useRealTimers();
    }
  });
});
