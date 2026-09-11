import { describe, expect, it, vi } from "vitest";
import {
  EditorClient,
  type EditorReply,
  type EditorTransport,
  type ProjectSnapshot,
  type RenderPlan,
  type SoftwarePreviewPacket,
} from "@cutterhoochee/shared";
import { PreviewEngine } from "./PreviewEngine";

type TestAudioSource = {
  buffer: AudioBuffer | null;
  readonly starts: Array<{ at: number; offset: number; duration: number }>;
  stopCount: number;
  onended: (() => void) | null;
  connect(): void;
  disconnect(): void;
  stop(): void;
  start(at: number, offset?: number, duration?: number): void;
};

type TestAudioContext = {
  currentTime: number;
  state: AudioContextState;
  readonly sampleRate: number;
  readonly outputLatency: number;
  readonly baseLatency: number;
  readonly destination: object;
  readonly sources: TestAudioSource[];
  getOutputTimestamp(): { contextTime: number; performanceTime: number };
  resume(): Promise<void>;
  close(): Promise<void>;
  createBuffer(numberOfChannels: number, length: number, sampleRate: number): AudioBuffer;
  createBufferSource(): AudioBufferSourceNode;
};

function createAudioContext(): TestAudioContext {
  const sources: TestAudioSource[] = [];
  const context = {
    currentTime: 0,
    state: "running" as AudioContextState,
    sampleRate: 48_000,
    outputLatency: 0,
    baseLatency: 0,
    destination: {},
    sources,
    getOutputTimestamp: () => ({ contextTime: context.currentTime, performanceTime: 0 }),
    resume: async () => {},
    close: async () => {
      context.state = "closed";
    },
    createBuffer: (numberOfChannels: number, length: number, sampleRate: number) => {
      const channelData = Array.from({ length: numberOfChannels }, () => new Float32Array(length));
      return {
        numberOfChannels,
        length,
        sampleRate,
        duration: length / sampleRate,
        copyToChannel: (source: Float32Array, channel: number, offset = 0) => {
          channelData[channel]?.set(source, offset);
        },
      } as unknown as AudioBuffer;
    },
    createBufferSource: () => {
      const source: TestAudioSource = {
        buffer: null,
        starts: [],
        stopCount: 0,
        onended: null,
        connect: () => {},
        disconnect: () => {},
        stop: () => {
          source.stopCount += 1;
        },
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

function makeProject(): ProjectSnapshot {
  return {
    workspaceId: "workspace-1",
    document: {
      projectId: "project-1",
      name: "software preview regression",
      revision: 1,
      profile: {
        width: 2,
        height: 2,
        fpsNum: 30,
        fpsDen: 1,
        background: { red: 17, green: 19, blue: 21, alpha: 255 },
      },
      assets: [],
      tracks: [],
      clips: [],
      textItems: [],
      transitions: [],
    },
  } as ProjectSnapshot;
}

function makePlan(): RenderPlan {
  return {
    rendererVersion: "test-renderer",
    projectId: "project-1",
    revision: 1,
    planHash: "software-retention-plan",
    width: 2,
    height: 2,
    fpsNum: 30,
    fpsDen: 1,
    durationFrames: 4,
    background: { red: 17, green: 19, blue: 21, alpha: 255 },
    fontFamily: "sans-serif",
    fontIdentity: "test-font",
    layers: [],
    audio: {
      sampleRate: 48_000,
      channels: 2,
      totalSamples: 6_400,
      segments: [],
    },
  } as RenderPlan;
}

function makeSoftwarePacket(plan: RenderPlan, frame = 0, sequence = 0): SoftwarePreviewPacket {
  return {
    generation: 7,
    projectId: plan.projectId,
    revision: plan.revision,
    planHash: plan.planHash,
    frame,
    sequence,
    width: plan.width,
    height: plan.height,
    contentType: "image/jpeg",
    data: new Uint8Array([1]),
  };
}

async function settleMicrotasks(): Promise<void> {
  await Promise.resolve();
  await Promise.resolve();
  await Promise.resolve();
  await Promise.resolve();
}
type ControlledImage = {
  readonly src: string;
  readonly removeSrcCount: number;
  removeAttribute(name: string): void;
  emitLoad(): void;
  emitError(): void;
};

function createImageElementDouble(): { readonly Image: new () => ControlledImage; readonly images: ControlledImage[] } {
  const images: ControlledImage[] = [];
  class TestImage extends EventTarget implements ControlledImage {
    private currentSrc = "";
    removeSrcCount = 0;

    constructor() {
      super();
      images.push(this);
    }

    get src(): string {
      return this.currentSrc;
    }

    set src(value: string) {
      this.currentSrc = value;
    }

    removeAttribute(name: string): void {
      if (name !== "src") return;
      this.removeSrcCount += 1;
      this.currentSrc = "";
    }

    emitLoad(): void {
      this.dispatchEvent(new Event("load"));
    }

    emitError(): void {
      this.dispatchEvent(new Event("error"));
    }
  }
  return { Image: TestImage, images };
}

type PendingSeek = {
  readonly target: number;
  readonly resolve: () => void;
};

type PreviewRaceHarness = {
  readonly engine: PreviewEngine;
  readonly audioContext: TestAudioContext;
  readonly states: Array<{ state: string; frame: number; quality: string }>;
  readonly pendingSeeks: PendingSeek[];
  readonly cleanup: () => void;
};

function createPreviewRaceHarness(): PreviewRaceHarness {
  vi.useFakeTimers();
  const audioContext = createAudioContext();
  const plan = makePlan();
  const project = makeProject();
  const states: Array<{ state: string; frame: number; quality: string }> = [];
  const canvasContext = {
    globalAlpha: 1,
    globalCompositeOperation: "source-over" as GlobalCompositeOperation,
    fillStyle: "",
    setTransform: vi.fn(),
    fillRect: vi.fn(),
    drawImage: vi.fn(),
  };
  const canvas = {
    width: 2,
    height: 2,
    getContext: () => canvasContext,
  } as unknown as HTMLCanvasElement;
  const pendingSeeks: PendingSeek[] = [];
  const ack: EditorReply = {
    kind: "preview",
    data: { kind: "ack", data: { revision: plan.revision, planHash: plan.planHash } },
  };
  const transport: EditorTransport = {
    call: async (request) => {
      if (request.method !== "preview") throw new Error("Unexpected editor method.");
      const action = request.params.action;
      if (action === "plan") {
        return { kind: "preview", data: { kind: "plan", data: plan } };
      }
      if (action === "seek") {
        const target = request.params.frame;
        return new Promise<EditorReply>((resolve) => {
          pendingSeeks.push({ target, resolve: () => resolve(ack) });
        });
      }
      if (action === "render_audio_window") {
        return {
          kind: "preview",
          data: {
            kind: "audio",
            data: {
              planHash: plan.planHash,
              startSample: request.params.startSample,
              sampleCount: request.params.sampleCount,
              sampleRate: 48_000,
              channels: 2,
              artifactId: "audio-window",
            },
          },
        };
      }
      return ack;
    },
    readArtifact: async () => new Uint8Array(new Float32Array(plan.audio.totalSamples * 2).buffer),
  };
  const client = new EditorClient(transport, { projectId: project.document.projectId, generation: 7 });
  vi.stubGlobal("AudioContext", class {
    constructor() {
      return audioContext;
    }
  });
  vi.stubGlobal("requestAnimationFrame", () => 1);
  vi.stubGlobal("cancelAnimationFrame", () => {});
  const engine = new PreviewEngine({
    canvas,
    client,
    onState: (value) => states.push({ state: value.state, frame: value.frame, quality: value.quality }),
  });
  return {
    engine,
    audioContext,
    states,
    pendingSeeks,
    cleanup: () => {
      engine.dispose();
      vi.unstubAllGlobals();
      vi.useRealTimers();
    },
  };
}

describe("PreviewEngine software playback", () => {
  it("retains a presented frame while animation ticks await the first audio advance", async () => {
    vi.useFakeTimers();
    const audioContext = createAudioContext();
    const plan = makePlan();
    const project = makeProject();
    const states: Array<{ state: string; frame: number; quality: string }> = [];
    const canvasContext = {
      globalAlpha: 1,
      globalCompositeOperation: "source-over" as GlobalCompositeOperation,
      fillStyle: "",
      setTransform: vi.fn(),
      fillRect: vi.fn(),
      drawImage: vi.fn(),
    };
    const canvas = {
      width: 2,
      height: 2,
      getContext: () => canvasContext,
    } as unknown as HTMLCanvasElement;

    let softwareListener: ((packet: SoftwarePreviewPacket) => void) | undefined;
    let subscriptionCount = 0;
    const acknowledgedSequences: number[] = [];
    const transport: EditorTransport = {
      call: async (request) => {
        if (request.method !== "preview") throw new Error("Unexpected editor method.");
        const action = request.params.action;
        if (action === "plan") {
          return { kind: "preview", data: { kind: "plan", data: plan } };
        }
        if (action === "render_audio_window") {
          return {
            kind: "preview",
            data: {
              kind: "audio",
              data: {
                planHash: plan.planHash,
                startSample: request.params.startSample,
                sampleCount: request.params.sampleCount,
                sampleRate: 48_000,
                channels: 2,
                artifactId: "audio-window",
              },
            },
          };
        }
        return {
          kind: "preview",
          data: { kind: "ack", data: { revision: plan.revision, planHash: plan.planHash } },
        };
      },
      readArtifact: async () => new Uint8Array(new Float32Array(plan.audio.totalSamples * 2).buffer),
      subscribePreviewSoftware: (listener) => {
        subscriptionCount += 1;
        softwareListener = listener;
        return () => {
          if (softwareListener === listener) softwareListener = undefined;
        };
      },
      acknowledgePreviewSoftware: async (sequence) => {
        acknowledgedSequences.push(sequence);
      },
      cancelPreviewSoftware: async () => {},
    };
    const client = new EditorClient(transport, { projectId: project.document.projectId, generation: 7 });
    const pendingFrames = new Map<number, FrameRequestCallback>();
    let nextFrameId = 0;

    const imageDouble = createImageElementDouble();
    vi.stubGlobal("AudioContext", class {
      constructor() {
        return audioContext;
      }
    });
    vi.stubGlobal("Image", imageDouble.Image);
    vi.stubGlobal("HTMLImageElement", imageDouble.Image);
    vi.stubGlobal("requestAnimationFrame", (callback: FrameRequestCallback) => {
      const id = ++nextFrameId;
      pendingFrames.set(id, callback);
      return id;
    });
    vi.stubGlobal("cancelAnimationFrame", (id: number) => {
      pendingFrames.delete(id);
    });

    const engine = new PreviewEngine({
      canvas,
      client,
      onState: (value) => states.push({ state: value.state, frame: value.frame, quality: value.quality }),
    });

    try {
      await engine.setProject(project);
      await engine.setQuality("software");
      await settleMicrotasks();

      expect(subscriptionCount).toBe(1);
      expect(canvasContext.drawImage).not.toHaveBeenCalled();
      expect(acknowledgedSequences).toEqual([]);
      expect(softwareListener).toBeDefined();
      if (!softwareListener) throw new Error("The paused software preview subscription was not established.");
      softwareListener(makeSoftwarePacket(plan));
      await settleMicrotasks();
      expect(imageDouble.images).toHaveLength(1);
      expect(canvasContext.drawImage).not.toHaveBeenCalled();
      expect(acknowledgedSequences).toEqual([]);
      const image = imageDouble.images[0];
      if (!image) throw new Error("The software preview image was not created.");
      image.emitLoad();
      await settleMicrotasks();
      expect(canvasContext.drawImage).not.toHaveBeenCalled();
      expect(acknowledgedSequences).toEqual([]);

      const playPromise = engine.play();
      await playPromise;
      expect(subscriptionCount).toBe(1);
      expect(acknowledgedSequences).toEqual([0]);

      expect(canvasContext.drawImage).toHaveBeenCalledTimes(1);
      expect(states.at(-1)).toMatchObject({ state: "playing", frame: 0, quality: "software" });
      const stateCountBeforeTicks = states.length;
      const sourceStartsBeforeTicks = audioContext.sources.flatMap((source) => source.starts).length;
      const sourceStopsBeforeTicks = audioContext.sources.reduce((count, source) => count + source.stopCount, 0);

      for (let tick = 0; tick < 3; tick += 1) {
        const next = pendingFrames.entries().next();
        if (next.done) throw new Error("The animation loop did not schedule its next tick.");
        const [id, callback] = next.value;
        pendingFrames.delete(id);
        callback(0);
      }

      expect(audioContext.currentTime).toBe(0);
      expect(canvasContext.drawImage).toHaveBeenCalledTimes(1);
      expect(states.slice(stateCountBeforeTicks).every((value) => value.state === "playing")).toBe(true);
      expect(states.at(-1)).toMatchObject({ state: "playing", frame: 0, quality: "software" });
      expect(audioContext.sources.flatMap((source) => source.starts)).toHaveLength(sourceStartsBeforeTicks);
      expect(audioContext.sources.reduce((count, source) => count + source.stopCount, 0)).toBe(sourceStopsBeforeTicks);
    } finally {
      engine.dispose();
      vi.unstubAllGlobals();
      vi.useRealTimers();
    }
  });
});
describe("PreviewEngine software preparation invalidation", () => {
  it("does not present or acknowledge paused packets and retires them across seek/project invalidation", async () => {
    vi.useFakeTimers();
    const audioContext = createAudioContext();
    const plan = makePlan();
    const project = makeProject();
    const listeners: Array<(packet: SoftwarePreviewPacket) => void> = [];
    const acknowledgedSequences: number[] = [];
    const canvasContext = {
      globalAlpha: 1,
      globalCompositeOperation: "source-over" as GlobalCompositeOperation,
      fillStyle: "",
      setTransform: vi.fn(),
      fillRect: vi.fn(),
      drawImage: vi.fn(),
    };
    const canvas = {
      width: 2,
      height: 2,
      getContext: () => canvasContext,
    } as unknown as HTMLCanvasElement;
    const transport: EditorTransport = {
      call: async (request) => {
        if (request.method !== "preview") throw new Error("Unexpected editor method.");
        if (request.params.action === "plan") {
          return { kind: "preview", data: { kind: "plan", data: plan } };
        }
        if (request.params.action === "render_audio_window") {
          return {
            kind: "preview",
            data: {
              kind: "audio",
              data: {
                planHash: plan.planHash,
                startSample: request.params.startSample,
                sampleCount: request.params.sampleCount,
                sampleRate: 48_000,
                channels: 2,
                artifactId: "audio-window",
              },
            },
          };
        }
        return {
          kind: "preview",
          data: { kind: "ack", data: { revision: plan.revision, planHash: plan.planHash } },
        };
      },
      readArtifact: async () => new Uint8Array(new Float32Array(plan.audio.totalSamples * 2).buffer),
      subscribePreviewSoftware: (listener) => {
        listeners.push(listener);
        return () => {};
      },
      acknowledgePreviewSoftware: async (sequence) => {
        acknowledgedSequences.push(sequence);
      },
      cancelPreviewSoftware: async () => {},
    };
    const client = new EditorClient(transport, { projectId: project.document.projectId, generation: 7 });

    const imageDouble = createImageElementDouble();
    vi.stubGlobal("AudioContext", class {
      constructor() {
        return audioContext;
      }
    });
    vi.stubGlobal("Image", imageDouble.Image);
    vi.stubGlobal("HTMLImageElement", imageDouble.Image);
    vi.stubGlobal("requestAnimationFrame", () => 1);
    vi.stubGlobal("cancelAnimationFrame", () => {});

    const engine = new PreviewEngine({ canvas, client, onState: () => {} });
    try {
      await engine.setProject(project);
      await engine.setQuality("software");
      await settleMicrotasks();
      expect(listeners).toHaveLength(1);
      const initialListener = listeners[0];
      if (!initialListener) throw new Error("The initial paused software listener was not established.");
      initialListener(makeSoftwarePacket(plan, 0, 0));
      await settleMicrotasks();
      expect(imageDouble.images).toHaveLength(1);
      const initialImage = imageDouble.images[0];
      if (!initialImage) throw new Error("The initial software preview image was not created.");
      expect(canvasContext.drawImage).not.toHaveBeenCalled();
      expect(acknowledgedSequences).toEqual([]);

      await engine.seek(1);
      await settleMicrotasks();
      expect(listeners).toHaveLength(2);
      const seekListener = listeners[1];
      if (!seekListener) throw new Error("The seek software listener was not established.");
      initialListener(makeSoftwarePacket(plan, 0, 1));
      await settleMicrotasks();
      expect(canvasContext.drawImage).not.toHaveBeenCalled();
      expect(acknowledgedSequences).toEqual([]);
      seekListener(makeSoftwarePacket(plan, 1, 0));
      await settleMicrotasks();
      expect(imageDouble.images).toHaveLength(2);
      const seekImage = imageDouble.images[1];
      if (!seekImage) throw new Error("The seek software preview image was not created.");
      expect(canvasContext.drawImage).not.toHaveBeenCalled();
      expect(acknowledgedSequences).toEqual([]);

      await engine.setProject(project);
      await settleMicrotasks();
      expect(listeners).toHaveLength(3);
      const projectListener = listeners[2];
      if (!projectListener) throw new Error("The refreshed project software listener was not established.");
      seekListener(makeSoftwarePacket(plan, 1, 1));
      await settleMicrotasks();
      expect(canvasContext.drawImage).not.toHaveBeenCalled();
      expect(acknowledgedSequences).toEqual([]);
      projectListener(makeSoftwarePacket(plan, 1, 0));
      await settleMicrotasks();
      expect(imageDouble.images).toHaveLength(3);
      const projectImage = imageDouble.images[2];
      if (!projectImage) throw new Error("The refreshed software preview image was not created.");
      expect(canvasContext.drawImage).not.toHaveBeenCalled();
      expect(acknowledgedSequences).toEqual([]);

      seekImage.emitLoad();
      await settleMicrotasks();
      expect(seekImage.removeSrcCount).toBe(1);
      expect(seekImage.src).toBe("");
      expect(canvasContext.drawImage).not.toHaveBeenCalled();
      expect(acknowledgedSequences).toEqual([]);

      initialImage.emitError();
      await settleMicrotasks();
      expect(initialImage.removeSrcCount).toBe(1);
      expect(initialImage.src).toBe("");
      expect(canvasContext.drawImage).not.toHaveBeenCalled();
      expect(acknowledgedSequences).toEqual([]);

      projectImage.emitLoad();
      await settleMicrotasks();
      expect(canvasContext.drawImage).not.toHaveBeenCalled();
      expect(acknowledgedSequences).toEqual([]);

      await engine.play();
      expect(listeners).toHaveLength(3);
      expect(canvasContext.drawImage).toHaveBeenCalledTimes(1);
      expect(acknowledgedSequences).toEqual([0]);
    } finally {
      engine.dispose();
      vi.unstubAllGlobals();
      vi.useRealTimers();
    }
  });
});
describe("PreviewEngine transport intent", () => {
  it("lets Play supersede an unresolved seek without a late pause", async () => {
    const harness = createPreviewRaceHarness();
    try {
      await harness.engine.setProject(makeProject());
      const seekPromise = harness.engine.seek(1);
      await settleMicrotasks();
      expect(harness.pendingSeeks.map((request) => request.target)).toEqual([1]);

      const playPromise = harness.engine.play();
      await playPromise;
      expect(harness.states.at(-1)).toMatchObject({ state: "playing", frame: 1 });
      const startsAfterPlay = harness.audioContext.sources.flatMap((source) => source.starts).length;

      const pendingSeek = harness.pendingSeeks[0];
      if (!pendingSeek) throw new Error("The unresolved seek was not captured.");
      pendingSeek.resolve();
      await seekPromise;

      expect(harness.states.at(-1)).toMatchObject({ state: "playing", frame: 1 });
      expect(harness.audioContext.sources.flatMap((source) => source.starts)).toHaveLength(startsAfterPlay);
    } finally {
      harness.cleanup();
    }
  });

  it("keeps the latest target and playing intent across rapid seeks", async () => {
    const harness = createPreviewRaceHarness();
    try {
      await harness.engine.setProject(makeProject());
      await harness.engine.play();
      const startsBeforeSeeks = harness.audioContext.sources.flatMap((source) => source.starts).length;

      const firstSeek = harness.engine.seek(1);
      await settleMicrotasks();
      const secondSeek = harness.engine.seek(3);
      await settleMicrotasks();
      expect(harness.pendingSeeks.map((request) => request.target)).toEqual([1, 3]);

      const newestSeek = harness.pendingSeeks[1];
      if (!newestSeek) throw new Error("The newest seek was not captured.");
      newestSeek.resolve();
      await secondSeek;
      expect(harness.states.at(-1)).toMatchObject({ state: "playing", frame: 3 });
      const startsAfterNewestSeek = harness.audioContext.sources.flatMap((source) => source.starts).length;
      expect(startsAfterNewestSeek).toBe(startsBeforeSeeks + 1);

      const olderSeek = harness.pendingSeeks[0];
      if (!olderSeek) throw new Error("The older seek was not captured.");
      olderSeek.resolve();
      await firstSeek;

      expect(harness.states.at(-1)).toMatchObject({ state: "playing", frame: 3 });
      expect(harness.audioContext.sources.flatMap((source) => source.starts)).toHaveLength(startsAfterNewestSeek);
    } finally {
      harness.cleanup();
    }
  });
});
