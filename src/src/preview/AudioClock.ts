const AUDIO_SAMPLE_RATE = 48_000;
const AUDIO_CHANNELS = 2;
const WINDOW_SECONDS = 5;
const WINDOW_SAMPLES = AUDIO_SAMPLE_RATE * WINDOW_SECONDS;
const LOOKAHEAD_WINDOWS = 3;
const SCHEDULE_LEAD_SECONDS = 0.035;
const REFILL_LEAD_SAMPLES = Math.floor(AUDIO_SAMPLE_RATE * 0.75);
const TICK_MS = 50;
const MAX_CACHED_WINDOWS = 8;

export type AudioClockState = "paused" | "buffering" | "playing" | "ended" | "error";

export interface AudioWindowRequest {
  readonly planHash: string;
  readonly startSample: number;
  readonly sampleCount: number;
  readonly signal: AbortSignal;
}

/**
 * A native audio-window result. PCM is interleaved stereo float32 at 48 kHz.
 * The native renderer, rather than this class, owns artifact/channel decoding.
 */
export interface AudioWindow {
  readonly planHash: string;
  readonly startSample: number;
  readonly sampleCount: number;
  readonly sampleRate: 48_000;
  readonly channels: 2;
  readonly pcm: Float32Array;
}

export interface AudioClockOptions {
  readonly getWindow: (request: AudioWindowRequest) => Promise<AudioWindow>;
  readonly onState?: (state: AudioClockState, sample: number, error?: Error) => void;
  readonly onPlayhead?: (sample: number) => void;
  readonly audioContext?: AudioContext;
}

interface CachedWindow extends AudioWindow {
  lastUsed: number;
}
interface PendingWindow {
  readonly operation: number;
  readonly promise: Promise<AudioWindow>;
}

interface ScheduledBuffer {
  readonly startSample: number;
  readonly endSample: number;
  readonly source: AudioBufferSourceNode;
}

function isAbortError(error: unknown): boolean {
  return error instanceof DOMException && error.name === "AbortError";
}

function checkedSample(value: number, label: string): number {
  if (!Number.isSafeInteger(value) || value < 0) {
    throw new Error(`${label} must be a non-negative safe integer.`);
  }
  return value;
}

function windowStart(sample: number): number {
  return Math.floor(sample / WINDOW_SAMPLES) * WINDOW_SAMPLES;
}

function stopSource(source: AudioBufferSourceNode): void {
  try {
    source.onended = null;
    source.stop();
  } catch {
    // A source that already ended is harmless.
  }
  try {
    source.disconnect();
  } catch {
    // Disconnecting an already-disconnected source is harmless.
  }
}

/**
 * Schedules the authoritative 48 kHz PCM timeline on WebAudio. It deliberately
 * does not use decodeAudioData: the browser is allowed to resample each 48 kHz
 * AudioBuffer to the device rate, but application sample coordinates remain
 * 48 kHz throughout.
 */
export class AudioClock {
  static readonly sampleRate = AUDIO_SAMPLE_RATE;
  static readonly channels = AUDIO_CHANNELS;
  static readonly windowSamples = WINDOW_SAMPLES;

  private readonly getWindow: AudioClockOptions["getWindow"];
  private readonly onState?: AudioClockOptions["onState"];
  private readonly onPlayhead?: AudioClockOptions["onPlayhead"];
  private readonly suppliedContext?: AudioContext;

  private context: AudioContext | undefined;
  private planHash = "";
  private totalSamples = 0;
  private current = 0;
  private currentState: AudioClockState = "paused";
  private desiredPlaying = false;
  private disposed = false;
  private operation = 0;
  private runAbort: AbortController | undefined;
  private timer: ReturnType<typeof setTimeout> | undefined;
  private contextStart = 0;
  private anchorSample = 0;
  private scheduledThrough = 0;
  private scheduled: ScheduledBuffer[] = [];
  private readonly cache = new Map<number, CachedWindow>();
  private readonly pending = new Map<number, PendingWindow>();
  private accessCounter = 0;
  private error: Error | undefined;

  constructor(options: AudioClockOptions) {
    this.getWindow = options.getWindow;
    this.onState = options.onState;
    this.onPlayhead = options.onPlayhead;
    this.suppliedContext = options.audioContext;
  }

  get state(): AudioClockState {
    return this.currentState;
  }

  get sample(): number {
    return this.readAudibleSample();
  }

  get durationSamples(): number {
    return this.totalSamples;
  }

  get activePlanHash(): string {
    return this.planHash;
  }

  /** Replace the immutable render-plan audio identity and sample duration. */
  setPlan(planHash: string, totalSamples: number): void {
    if (this.disposed) return;
    if (typeof planHash !== "string" || planHash.length === 0) {
      throw new Error("Audio plan hash must not be empty.");
    }
    checkedSample(totalSamples, "totalSamples");
    const changed = this.planHash !== planHash || this.totalSamples !== totalSamples;
    this.planHash = planHash;
    this.totalSamples = totalSamples;
    this.current = Math.min(this.current, totalSamples);
    if (!changed) return;

    const shouldResume = this.desiredPlaying;
    this.abortOperation();
    this.stopScheduled();
    this.cache.clear();
    this.current = Math.min(this.current, totalSamples);
    this.scheduledThrough = this.current;
    if (this.current >= totalSamples) {
      this.desiredPlaying = false;
      this.setState("ended", this.current);
    } else if (shouldResume) {
      this.setState("buffering", this.current);
      void this.beginPlayback(this.current);
    } else {
      this.setState("paused", this.current);
    }
  }

  /**
   * Start or resume from the current 48 kHz sample coordinate. A fresh anchor
   * is established for every run, so a refill never skips a missing window.
   */
  async play(anchorSample = this.current): Promise<void> {
    if (this.disposed) return;
    if (this.planHash.length === 0) {
      this.fail(new Error("Cannot play without a render plan."));
      return;
    }
    checkedSample(anchorSample, "anchorSample");
    this.current = Math.min(anchorSample, this.totalSamples);
    if (this.current >= this.totalSamples) {
      this.desiredPlaying = false;
      this.setState("ended", this.current);
      return;
    }
    this.desiredPlaying = true;
    await this.beginPlayback(this.current);
  }

  pause(): void {
    if (this.disposed) return;
    this.current = this.readAudibleSample();
    this.desiredPlaying = false;
    this.abortOperation();
    this.stopScheduled();
    this.setState(this.current >= this.totalSamples ? "ended" : "paused", this.current);
  }

  /** Seek and, when already playing, resume from an exact new anchor. */
  async seek(sample: number): Promise<void> {
    if (this.disposed) return;
    checkedSample(sample, "sample");
    const wasPlaying = this.desiredPlaying;
    this.current = Math.min(sample, this.totalSamples);
    this.abortOperation();
    this.stopScheduled();
    this.scheduledThrough = this.current;
    this.error = undefined;
    if (this.current >= this.totalSamples) {
      this.desiredPlaying = false;
      this.setState("ended", this.current);
      return;
    }
    if (wasPlaying) {
      this.setState("buffering", this.current);
      await this.beginPlayback(this.current);
    } else {
      this.setState("paused", this.current);
    }
  }

  /** Invalidate all in-flight native responses without changing plan identity. */
  invalidate(): void {
    if (this.disposed) return;
    const wasPlaying = this.desiredPlaying;
    this.current = this.readAudibleSample();
    this.abortOperation();
    this.stopScheduled();
    this.scheduledThrough = this.current;
    this.cache.clear();
    this.error = undefined;
    if (wasPlaying && this.current < this.totalSamples) {
      this.setState("buffering", this.current);
      void this.beginPlayback(this.current);
    } else {
      this.setState(this.current >= this.totalSamples ? "ended" : "paused", this.current);
    }
  }

  dispose(): void {
    if (this.disposed) return;
    this.disposed = true;
    this.desiredPlaying = false;
    this.abortOperation();
    this.stopScheduled();
    this.cache.clear();
    this.pending.clear();
    if (this.timer !== undefined) clearTimeout(this.timer);
    this.timer = undefined;
    // Borrowed contexts may be shared; internally-created contexts belong to
    // this clock and must release their device stream when the preview exits.
    const context = this.context;
    this.context = undefined;
    if (context && context !== this.suppliedContext && context.state !== "closed") {
      void context.close().catch(() => undefined);
    }
  }

  private getContext(): AudioContext {
    if (this.context) return this.context;
    if (this.suppliedContext) {
      this.context = this.suppliedContext;
      return this.context;
    }
    const AudioContextCtor = globalThis.AudioContext;
    if (!AudioContextCtor) {
      throw new Error("This environment has no WebAudio AudioContext.");
    }
    // The requested rate is a hint for the context. Buffers below are always
    // created explicitly at 48 kHz and are never interpreted at device rate.
    this.context = new AudioContextCtor({ latencyHint: "interactive", sampleRate: AUDIO_SAMPLE_RATE });
    return this.context;
  }

  private async beginPlayback(anchor: number): Promise<void> {
    this.abortOperation();
    const token = this.operation;
    const abort = new AbortController();
    this.runAbort = abort;
    this.current = Math.min(anchor, this.totalSamples);
    this.anchorSample = this.current;
    this.scheduledThrough = this.current;
    this.error = undefined;
    this.stopScheduled();
    if (!this.desiredPlaying || this.current >= this.totalSamples) return;

    let context: AudioContext;
    try {
      context = this.getContext();
      if (context.state === "closed") throw new Error("The WebAudio context is closed.");
      if (context.state === "suspended") await context.resume();
    } catch (error) {
      if (!this.isCurrent(token, abort)) return;
      this.fail(error instanceof Error ? error : new Error(String(error)));
      return;
    }
    if (!this.isCurrent(token, abort)) return;

    this.setState("buffering", this.current);
    const first = windowStart(this.current);
    const required: Promise<AudioWindow>[] = [];
    // The requested remainder and the next window must be ready before audible
    // playback starts. At the project end, the first remainder is sufficient.
    for (let index = 0; index <= LOOKAHEAD_WINDOWS; index += 1) {
      const start = first + index * WINDOW_SAMPLES;
      if (start >= this.totalSamples) break;
      required.push(this.ensureWindow(start, token, abort));
    }
    try {
      await Promise.all(required.slice(0, Math.min(2, required.length)));
    } catch (error) {
      if (!this.isCurrent(token, abort) || isAbortError(error)) return;
      this.fail(error instanceof Error ? error : new Error(String(error)));
      return;
    }
    if (!this.isCurrent(token, abort) || !this.desiredPlaying) return;

    this.contextStart = context.currentTime + SCHEDULE_LEAD_SECONDS;
    this.scheduleAvailable(context, token, abort);
    if (this.scheduledThrough <= this.current) {
      this.enterBuffering(this.current, token, abort);
      return;
    }
    this.setState("playing", this.current);
    this.scheduleTick(token, abort);
  }

  private scheduleAvailable(context: AudioContext, token: number, abort: AbortController): void {
    if (!this.isCurrent(token, abort) || !this.desiredPlaying) return;
    let cursor = this.scheduledThrough;
    while (cursor < this.totalSamples) {
      const start = windowStart(cursor);
      const window = this.cache.get(start);
      if (!window) break;
      const offsetSamples = Math.max(0, cursor - window.startSample);
      if (offsetSamples >= window.sampleCount) {
        cursor = window.startSample + window.sampleCount;
        continue;
      }
      const end = Math.min(window.startSample + window.sampleCount, this.totalSamples);
      const startAtSample = Math.max(window.startSample, cursor);
      const source = this.createSource(context, window);
      const startTime = this.contextStart + (startAtSample - this.anchorSample) / AUDIO_SAMPLE_RATE;
      const sourceOffset = offsetSamples / AUDIO_SAMPLE_RATE;
      const audibleDuration = (end - startAtSample) / AUDIO_SAMPLE_RATE;
      try {
        source.start(startTime, sourceOffset, audibleDuration);
      } catch (error) {
        stopSource(source);
        this.fail(error instanceof Error ? error : new Error(String(error)));
        return;
      }
      this.scheduled.push({ startSample: startAtSample, endSample: end, source });
      cursor = end;
      this.scheduledThrough = end;
      if (end >= this.totalSamples) break;
    }
  }

  private createSource(context: AudioContext, window: CachedWindow): AudioBufferSourceNode {
    const buffer = context.createBuffer(AUDIO_CHANNELS, window.sampleCount, AUDIO_SAMPLE_RATE);
    const left = new Float32Array(window.sampleCount);
    const right = new Float32Array(window.sampleCount);
    for (let index = 0, channelIndex = 0; index < window.sampleCount; index += 1, channelIndex += 2) {
      left[index] = window.pcm[channelIndex] ?? 0;
      right[index] = window.pcm[channelIndex + 1] ?? 0;
    }
    buffer.copyToChannel(left, 0);
    buffer.copyToChannel(right, 1);
    const source = context.createBufferSource();
    source.buffer = buffer;
    source.connect(context.destination);
    return source;
  }

  private scheduleTick(token: number, abort: AbortController): void {
    if (this.timer !== undefined) clearTimeout(this.timer);
    this.timer = setTimeout(() => {
      this.timer = undefined;
      this.tick(token, abort);
    }, TICK_MS);
  }

  private tick(token: number, abort: AbortController): void {
    if (!this.isCurrent(token, abort) || !this.desiredPlaying || this.disposed) return;
    const context = this.context;
    if (!context || context.state === "closed") {
      this.enterBuffering(this.current, token, abort);
      return;
    }
    if (context.state === "suspended") {
      this.current = this.readAudibleSample();
      this.stopScheduled();
      this.enterBuffering(this.current, token, abort);
      return;
    }
    this.current = this.readAudibleSample();
    this.onPlayhead?.(this.current);
    if (this.current >= this.totalSamples) {
      this.current = this.totalSamples;
      this.desiredPlaying = false;
      this.stopScheduled();
      this.setState("ended", this.current);
      return;
    }

    // Refill before the last contiguous window ends, without seeking any
    // decoder. In-flight calls are deduplicated by ensureWindow().
    if (this.scheduledThrough - this.current <= REFILL_LEAD_SAMPLES) {
      const nextStart = windowStart(this.scheduledThrough);
      if (nextStart < this.totalSamples) {
        void this.ensureWindow(nextStart, token, abort).then(() => {
          if (!this.isCurrent(token, abort) || !this.desiredPlaying || this.currentState !== "playing") return;
          this.scheduleAvailable(context, token, abort);
        }).catch((error: unknown) => {
          if (!this.isCurrent(token, abort) || isAbortError(error)) return;
          if (this.current >= this.scheduledThrough) this.enterBuffering(this.scheduledThrough, token, abort);
        });
      }
    }

    this.pruneScheduled();
    if (this.current >= this.scheduledThrough && this.scheduledThrough < this.totalSamples) {
      this.enterBuffering(this.scheduledThrough, token, abort);
      return;
    }
    this.scheduleTick(token, abort);
  }

  private pruneScheduled(): void {
    const audible = this.readAudibleSample();
    const keep: ScheduledBuffer[] = [];
    for (const buffer of this.scheduled) {
      if (buffer.endSample <= audible) {
        stopSource(buffer.source);
      } else {
        keep.push(buffer);
      }
    }
    this.scheduled = keep;
  }

  private enterBuffering(sample: number, token: number, abort: AbortController): void {
    if (!this.isCurrent(token, abort) || !this.desiredPlaying) return;
    this.current = Math.min(sample, this.totalSamples);
    this.stopScheduled();
    this.scheduledThrough = this.current;
    this.setState(this.current >= this.totalSamples ? "ended" : "buffering", this.current);
    if (this.current >= this.totalSamples) {
      this.desiredPlaying = false;
      return;
    }
    const context = this.context;
    if (!context) return;
    const start = windowStart(this.current);
    void this.ensureWindow(start, token, abort).then(() => {
      if (!this.isCurrent(token, abort) || !this.desiredPlaying) return;
      const nextStart = start + WINDOW_SAMPLES;
      const next = nextStart < this.totalSamples ? this.ensureWindow(nextStart, token, abort) : Promise.resolve();
      return next.then(() => {
        if (!this.isCurrent(token, abort) || !this.desiredPlaying) return;
        this.anchorSample = this.current;
        this.contextStart = context.currentTime + SCHEDULE_LEAD_SECONDS;
        this.scheduleAvailable(context, token, abort);
        if (this.scheduledThrough > this.current) {
          this.setState("playing", this.current);
          this.scheduleTick(token, abort);
        }
      });
    }).catch((error: unknown) => {
      if (!this.isCurrent(token, abort) || isAbortError(error)) return;
      this.fail(error instanceof Error ? error : new Error(String(error)));
    });
  }

  private ensureWindow(startSample: number, token: number, abort: AbortController): Promise<AudioWindow> {
    const existing = this.cache.get(startSample);
    if (existing && existing.planHash === this.planHash) {
      existing.lastUsed = ++this.accessCounter;
      return Promise.resolve(existing);
    }
    const pending = this.pending.get(startSample);
    if (pending && pending.operation === token) return pending.promise;
    if (pending) this.pending.delete(startSample);
    const request: AudioWindowRequest = {
      planHash: this.planHash,
      startSample,
      sampleCount: Math.min(WINDOW_SAMPLES, this.totalSamples - startSample),
      signal: abort.signal,
    };
    let promise: Promise<AudioWindow>;
    promise = this.getWindow(request).then((window) => {
      if (!this.isCurrent(token, abort)) throw new DOMException("Stale audio window", "AbortError");
      this.validateWindow(window, request);
      const cached: CachedWindow = { ...window, lastUsed: ++this.accessCounter };
      this.cache.set(window.startSample, cached);
      this.trimCache();
      return cached;
    }).finally(() => {
      const current = this.pending.get(startSample);
      if (current?.promise === promise) this.pending.delete(startSample);
    });
    this.pending.set(startSample, { operation: token, promise });
    return promise;
  }

  private validateWindow(window: AudioWindow, request: AudioWindowRequest): void {
    if (window.planHash !== request.planHash || window.startSample !== request.startSample) {
      throw new Error("Native audio window belongs to a different render plan.");
    }
    if (window.sampleCount !== request.sampleCount || window.sampleRate !== AUDIO_SAMPLE_RATE || window.channels !== AUDIO_CHANNELS) {
      throw new Error("Native audio window has an unsupported sample layout.");
    }
    if (!(window.pcm instanceof Float32Array) || window.pcm.length !== window.sampleCount * AUDIO_CHANNELS) {
      throw new Error("Native audio window has an invalid PCM length.");
    }
    for (const sample of window.pcm) {
      if (!Number.isFinite(sample)) throw new Error("Native audio window contains a non-finite sample.");
    }
  }

  private trimCache(): void {
    if (this.cache.size <= MAX_CACHED_WINDOWS) return;
    const protectedStarts = new Set(this.scheduled.map((buffer) => windowStart(buffer.startSample)));
    protectedStarts.add(windowStart(this.current));
    while (this.cache.size > MAX_CACHED_WINDOWS) {
      let candidate: number | undefined;
      let oldest = Number.POSITIVE_INFINITY;
      for (const [start, window] of this.cache) {
        if (protectedStarts.has(start)) continue;
        if (window.lastUsed < oldest) {
          oldest = window.lastUsed;
          candidate = start;
        }
      }
      if (candidate === undefined) break;
      this.cache.delete(candidate);
    }
  }

  private readAudibleSample(): number {
    const context = this.context;
    if (!context || this.currentState !== "playing") return this.current;
    let audibleContextTime = context.currentTime;
    try {
      const timestamp = context.getOutputTimestamp?.();
      const timestampContextTime = timestamp?.contextTime;
      if (typeof timestampContextTime === "number" && Number.isFinite(timestampContextTime)) {
        audibleContextTime = timestampContextTime;
      } else {
        const outputLatency = Number.isFinite(context.outputLatency) ? context.outputLatency : 0;
        const baseLatency = Number.isFinite(context.baseLatency) ? context.baseLatency : 0;
        audibleContextTime = context.currentTime - Math.max(outputLatency, baseLatency);
      }
    } catch {
      const outputLatency = Number.isFinite(context.outputLatency) ? context.outputLatency : 0;
      const baseLatency = Number.isFinite(context.baseLatency) ? context.baseLatency : 0;
      audibleContextTime = context.currentTime - Math.max(outputLatency, baseLatency);
    }
    const sample = this.anchorSample + Math.floor((audibleContextTime - this.contextStart) * AUDIO_SAMPLE_RATE);
    return Math.max(this.anchorSample, Math.min(this.totalSamples, sample));
  }

  private abortOperation(): void {
    this.operation += 1;
    this.runAbort?.abort();
    this.runAbort = undefined;
    this.pending.clear();
  }

  private isCurrent(token: number, abort: AbortController): boolean {
    return !this.disposed && this.operation === token && !abort.signal.aborted;
  }

  private stopScheduled(): void {
    for (const buffer of this.scheduled) stopSource(buffer.source);
    this.scheduled = [];
    if (this.timer !== undefined) clearTimeout(this.timer);
    this.timer = undefined;
  }

  private setState(state: AudioClockState, sample: number, error?: Error): void {
    if (this.currentState === state && this.current === sample && this.error === error) return;
    this.currentState = state;
    this.current = Math.max(0, Math.min(this.totalSamples, sample));
    this.error = error;
    this.onState?.(state, this.current, error);
  }

  private fail(error: Error): void {
    this.desiredPlaying = false;
    this.abortOperation();
    this.stopScheduled();
    this.error = error;
    this.setState("error", this.current, error);
  }
}

export { AUDIO_CHANNELS, AUDIO_SAMPLE_RATE, LOOKAHEAD_WINDOWS, WINDOW_SAMPLES };
