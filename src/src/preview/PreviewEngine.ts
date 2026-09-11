import type {
  EditorClient,
  PreviewAction,
  PreviewAudioReply,
  PreviewFrameReply,
  PreviewInspectFrame,
  ProjectSnapshot,
  RenderPlan,
  RenderSegment,
  RenderTextOverlay,
  SoftwarePreviewPacket,
} from "@cutterhoochee/shared";

import { AudioClock, type AudioClockState, type AudioWindow, type AudioWindowRequest } from "./AudioClock";
import { CanvasComposer, type RasterSource } from "./composition";
import { VideoPool, type VideoMapping, type VideoSnapshot } from "./VideoPool";

interface PreviewClientHooks {
  readonly callPreview?: EditorClient["callPreview"];
  readonly artifactUrl?: EditorClient["artifactUrl"];
  readonly fetchArtifact?: EditorClient["fetchArtifact"];
  readonly subscribePreviewSoftware?: EditorClient["subscribePreviewSoftware"];
  readonly acknowledgePreviewSoftware?: EditorClient["acknowledgePreviewSoftware"];
  readonly cancelPreviewSoftware?: EditorClient["cancelPreviewSoftware"];
}

interface NormalizedPreviewReply {
  readonly kind: "plan" | "frame" | "audio" | "inspect" | "ack";
  readonly plan?: RenderPlan;
  readonly frame?: PreviewFrameReply;
  readonly audio?: PreviewAudioReply;
  readonly revision?: number;
  readonly frames?: readonly PreviewInspectFrame[];
  readonly planHash?: string;
}

export type PreviewTransportState = "paused" | "buffering" | "playing" | "ended" | "error";
export type PreviewQuality = "auto" | "software";

export interface PreviewEngineState {
  readonly state: PreviewTransportState;
  readonly frame: number;
  readonly durationFrames: number;
  readonly quality: PreviewQuality;
  readonly error?: string;
}

export interface PreviewEngineOptions {
  readonly canvas: HTMLCanvasElement;
  readonly client: EditorClient;
  readonly onState: (state: PreviewEngineState) => void;
}

interface CachedImage {
  readonly source: CanvasImageSource;
  lastUsed: number;
}

interface SoftwareFrame {
  readonly packet: SoftwarePreviewPacket;
  readonly source: CanvasImageSource;
}
interface SoftwareStreamIdentity {
  readonly projectId: string;
  readonly generation: number;
  readonly revision: number;
  readonly planHash: string;
  readonly frame: number;
}
interface SyncedInputs {
  readonly token: number;
  readonly frame: number;
  readonly playing: boolean;
  readonly quality: PreviewQuality;
  readonly planHash: string;
}

interface SoftwareFrameWait {
  readonly token: number;
  readonly epoch: number;
  readonly frame: number;
  readonly promise: Promise<void>;
  readonly resolve: () => void;
  readonly reject: (error: Error) => void;
  readonly timer: number | NodeJS.Timeout;
}

export interface SoftwarePreviewDimensions {
  readonly width: number;
  readonly height: number;
}

export function softwarePreviewDimensions(width: number, height: number): SoftwarePreviewDimensions {
  if (!Number.isSafeInteger(width) || width <= 0 || !Number.isSafeInteger(height) || height <= 0) {
    throw new Error("Software preview dimensions require positive safe integer canvas dimensions.");
  }
  const scale = Math.min(1, 960 / Math.max(width, height), 540 / Math.min(width, height));
  return {
    width: Math.max(1, Math.round(width * scale)),
    height: Math.max(1, Math.round(height * scale)),
  };
}






const SOFTWARE_QUEUE_LIMIT = 3;
const IMAGE_CACHE_LIMIT = 24;
const URL_CACHE_LIMIT = 48;
const STILL_DEBOUNCE_MS = 80;
const PLAYHEAD_SAMPLE_RATE = 48_000;
const SOFTWARE_FRAME_TIMEOUT_MS = 1_500;
const LIVE_SNAPSHOT_TOLERANCE_FRAMES = 1;

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null;
}

function isFiniteNumber(value: unknown): value is number {
  return typeof value === "number" && Number.isFinite(value);
}

function isSafeNonNegativeInteger(value: unknown): value is number {
  return isFiniteNumber(value) && Number.isSafeInteger(value) && value >= 0;
}

function checkedFrame(value: number, label: string): number {
  if (!Number.isSafeInteger(value) || value < 0) throw new Error(`${label} must be a non-negative safe integer.`);
  return value;
}

function clampFrame(frame: number, durationFrames: number): number {
  const last = Math.max(0, durationFrames - 1);
  return Math.max(0, Math.min(last, frame));
}

function sampleAtFrame(frame: number, fpsNum: number, fpsDen: number): number {
  const numerator = BigInt(frame) * BigInt(PLAYHEAD_SAMPLE_RATE) * BigInt(fpsDen);
  return Number(numerator / BigInt(fpsNum));
}

function frameAtSample(sample: number, fpsNum: number, fpsDen: number, durationFrames: number): number {
  const numerator = BigInt(Math.max(0, sample)) * BigInt(fpsNum);
  const denominator = BigInt(PLAYHEAD_SAMPLE_RATE) * BigInt(fpsDen);
  return clampFrame(Number(numerator / denominator), durationFrames);
}

function isAbortError(error: unknown): boolean {
  return error instanceof DOMException && error.name === "AbortError";
}

function staleError(): DOMException {
  return new DOMException("The preview response belongs to a retired generation.", "AbortError");
}

function closeImage(source: CanvasImageSource | undefined): void {
  if (typeof ImageBitmap !== "undefined" && source instanceof ImageBitmap) source.close();
}

function clearCanvasBackground(
  canvas: HTMLCanvasElement,
  background: RenderPlan["background"],
): void {
  const context = canvas.getContext("2d", { alpha: false });
  if (!context) return;
  const alpha = Math.max(0, Math.min(1, background.alpha / 255));
  context.setTransform(1, 0, 0, 1, 0, 0);
  context.globalCompositeOperation = "copy";
  context.globalAlpha = 1;
  context.fillStyle = `rgba(${background.red}, ${background.green}, ${background.blue}, ${alpha})`;
  context.fillRect(0, 0, canvas.width, canvas.height);
  context.globalCompositeOperation = "source-over";
}

function normalizePreviewReply(value: unknown): NormalizedPreviewReply {
  if (!isRecord(value)) throw new Error("Native preview returned a non-object reply.");
  if (value.kind === "preview" && isRecord(value.data)) return normalizePreviewReply(value.data);
  if (value.kind === "plan" && isRecord(value.data)) {
    return { kind: "plan", plan: value.data.plan && isRecord(value.data.plan) ? value.data.plan as unknown as RenderPlan : value.data as unknown as RenderPlan };
  }
  if (value.kind === "frame" && isRecord(value.data)) {
    return { kind: "frame", frame: value.data.frame && isRecord(value.data.frame) ? value.data.frame as unknown as PreviewFrameReply : value.data as unknown as PreviewFrameReply };
  }
  if (value.kind === "audio" && isRecord(value.data)) {
    return { kind: "audio", audio: value.data.audio && isRecord(value.data.audio) ? value.data.audio as unknown as PreviewAudioReply : value.data as unknown as PreviewAudioReply };
  }
  if (value.kind === "inspect" && isRecord(value.data)) {
    return { kind: "inspect", revision: isFiniteNumber(value.data.revision) ? value.data.revision : undefined, frames: Array.isArray(value.data.frames) ? value.data.frames as unknown as PreviewInspectFrame[] : [] };
  }
  if (value.kind === "ack") return { kind: "ack", revision: isFiniteNumber(value.revision) ? value.revision : undefined, planHash: typeof value.planHash === "string" ? value.planHash : undefined };
  if (value.kind === "plan" || value.kind === "frame" || value.kind === "audio" || value.kind === "inspect") return value as unknown as NormalizedPreviewReply;
  if (isRecord(value.data)) return normalizePreviewReply(value.data);
  throw new Error("Native preview returned an unsupported reply kind.");
}

function artifactData(value: unknown): unknown {
  if (isRecord(value) && "data" in value) return value.data;
  return value;
}

async function toArrayBuffer(value: unknown): Promise<ArrayBuffer> {
  const data = artifactData(value);
  if (data instanceof ArrayBuffer) return data;
  if (typeof ArrayBuffer !== "undefined" && ArrayBuffer.isView(data)) {
    const buffer = data.buffer;
    if (buffer instanceof ArrayBuffer && data.byteOffset === 0 && data.byteLength === buffer.byteLength) {
      return buffer;
    }
    const bytes = new Uint8Array(data.byteLength);
    bytes.set(new Uint8Array(buffer, data.byteOffset, data.byteLength));
    return bytes.buffer;
  }
  if (typeof Blob !== "undefined" && data instanceof Blob) return data.arrayBuffer();
  if (typeof Response !== "undefined" && data instanceof Response) return data.arrayBuffer();
  throw new Error("Native artifact did not provide binary bytes.");
}


function validateReplyArtifact(reply: NormalizedPreviewReply, kind: "frame" | "audio"): string {
  const artifact = kind === "frame" ? reply.frame?.artifactId : reply.audio?.artifactId;
  if (typeof artifact !== "string" || artifact.length === 0) throw new Error(`Native ${kind} reply omitted its artifact ID.`);
  return artifact;
}

function validatePlan(value: unknown): RenderPlan {
  if (!isRecord(value)) throw new Error("Native preview plan is not an object.");
  const requiredStrings = ["rendererVersion", "projectId", "planHash", "fontFamily"];
  for (const field of requiredStrings) {
    if (typeof value[field] !== "string" || value[field].length === 0) throw new Error(`Render plan field ${field} is invalid.`);
  }
  const requiredIntegers = ["revision", "width", "height", "fpsNum", "fpsDen", "durationFrames"];
  for (const field of requiredIntegers) {
    if (!isSafeNonNegativeInteger(value[field]) || value[field] === 0 && ["width", "height", "fpsNum", "fpsDen"].includes(field)) throw new Error(`Render plan field ${field} is invalid.`);
  }
  if (!isRecord(value.background) || !Array.isArray(value.layers) || !isRecord(value.audio)) throw new Error("Render plan omitted composition or audio data.");
  if (value.audio.sampleRate !== PLAYHEAD_SAMPLE_RATE || value.audio.channels !== 2 || !isSafeNonNegativeInteger(value.audio.totalSamples) || !Array.isArray(value.audio.segments)) throw new Error("Render plan audio layout is invalid.");
  for (const layer of value.layers) validateLayer(layer);
  return value as unknown as RenderPlan;
}

function validateLayer(value: unknown): void {
  if (!isRecord(value) || typeof value.trackId !== "string" || (value.kind !== "video" && value.kind !== "text") || !isSafeNonNegativeInteger(value.order) || !Array.isArray(value.segments) || !Array.isArray(value.transitions) || !Array.isArray(value.textOverlays)) throw new Error("Render plan layer is invalid.");
  for (const segment of value.segments) validateSegment(segment);
  for (const transition of value.transitions) {
    if (!isRecord(transition) || typeof transition.id !== "string" || typeof transition.leftClipId !== "string" || typeof transition.rightClipId !== "string" || !isSafeNonNegativeInteger(transition.startFrame) || !isSafeNonNegativeInteger(transition.endFrame) || !isSafeNonNegativeInteger(transition.durationFrames) || transition.endFrame <= transition.startFrame || transition.durationFrames === 0) throw new Error("Render plan transition is invalid.");
  }
  for (const overlay of value.textOverlays) {
    if (!isRecord(overlay) || typeof overlay.id !== "string" || typeof overlay.trackId !== "string" || typeof overlay.text !== "string" || typeof overlay.rasterArtifactId !== "string" || overlay.rasterArtifactId.length === 0 || !isSafeNonNegativeInteger(overlay.startFrame) || !isSafeNonNegativeInteger(overlay.endFrame) || overlay.endFrame <= overlay.startFrame || !isRecord(overlay.rasterSourceRect) || !isRecord(overlay.rasterDestRect)) throw new Error("Render plan text overlay is invalid.");
  }
}
function validateSegment(value: unknown): void {
  if (!isRecord(value) || typeof value.clipId !== "string" || typeof value.assetId !== "string" || typeof value.artifactId !== "string" || value.artifactId.length === 0 || typeof value.isStillImage !== "boolean" || !isSafeNonNegativeInteger(value.startFrame) || !isSafeNonNegativeInteger(value.endFrame) || value.endFrame <= value.startFrame || !isSafeNonNegativeInteger(value.activeStartFrame) || !isSafeNonNegativeInteger(value.activeEndFrame) || value.activeEndFrame <= value.activeStartFrame || !isSafeNonNegativeInteger(value.sourceStartFrame) || !isSafeNonNegativeInteger(value.sourceEndFrame) || value.sourceEndFrame <= value.sourceStartFrame || !isRecord(value.sourceRect) || !isRecord(value.destRect) || !isFiniteNumber(value.opacity)) throw new Error("Render plan segment is invalid.");
}

export class PreviewEngine {
  private readonly canvas: HTMLCanvasElement;
  private readonly client: EditorClient;
  private readonly onState: PreviewEngineOptions["onState"];
  private readonly hooks: PreviewClientHooks;
  private readonly composer: CanvasComposer;
  private readonly videoPool: VideoPool;
  private readonly audioClock: AudioClock;
  private readonly unsubscribeContext: () => void;
  private readonly artifactUrls = new Map<string, string>();
  private readonly artifactUrlPromises = new Map<string, Promise<string>>();
  private readonly ownedObjectUrls = new Set<string>();
  private readonly images = new Map<string, CachedImage>();
  private readonly softwareFrames = new Map<number, SoftwareFrame>();
  private readonly softwareImageUrls = new Map<CanvasImageSource, string>();

  private project: ProjectSnapshot | undefined;
  private plan: RenderPlan | undefined;
  private artifactCacheScope: string | undefined;
  private quality: PreviewQuality = "auto";
  private softwareFallbackActive = false;
  private transport: PreviewTransportState = "paused";
  private frame = 0;
  // This is the latest user transport intent. Preparation pauses the audio/video
  // producers temporarily, but must not overwrite an intent to keep playing.
  private desiredPlaying = false;
  private disposed = false;
  private operation = 0;
  private imageAccess = 0;
  private animationHandle: number | undefined;
  private stillTimer: ReturnType<typeof setTimeout> | undefined;
  private stillRequest = 0;
  private movingUpdate: Promise<void> | undefined;
  private lastSyncedInputs: SyncedInputs | undefined;
  private activeSegmentCache: { plan: RenderPlan; frame: number; segments: RenderSegment[] } | undefined;
  private activeRasterCache: { plan: RenderPlan; frame: number; overlays: RenderTextOverlay[] } | undefined;
  private prewarmedNextFrame: { token: number; frame: number; planHash: string } | undefined;
  private softwareUnsubscribe: (() => void) | undefined;
  private softwareSubscriptionPromise: Promise<(() => void) | undefined> | undefined;
  private softwareStreamIdentity: SoftwareStreamIdentity | undefined;
  private softwareEpoch = 0;
  private softwarePresentedFrame = -1;
  private softwareFrameWait: SoftwareFrameWait | undefined;
  private softwareAudioStartPending = false;
  private softwareAudioStarted = false;
  private softwareSequenceStart = -1;
  private softwareAcknowledgedThrough = -1;
  private readonly softwareInFlightSequences = new Set<number>();
  private readonly softwareRetiredSequences = new Set<number>();
  private softwareBusy = false;


  constructor(options: PreviewEngineOptions) {
    this.canvas = options.canvas;
    this.client = options.client;
    this.onState = options.onState;
    this.hooks = options.client;
    this.composer = new CanvasComposer(options.canvas);
    this.videoPool = new VideoPool({
      onSnapshot: () => {
        if (this.desiredPlaying && this.effectiveQuality() === "auto") this.drawApproximate();
      },
      onDrift: (drift) => {
        if (this.desiredPlaying && drift.consecutiveFrames >= 2) this.enterVideoBuffering();
      },
      onUnavailable: (failure) => {
        if (this.desiredPlaying && this.effectiveQuality() === "auto") {
          this.enterVideoBuffering(new Error(`${failure.reason} (${failure.key}).`));
        }
      },
    });
    this.audioClock = new AudioClock({
      getWindow: (request) => this.getAudioWindow(request),
      onState: (state, sample, error) => this.handleAudioState(state, sample, error),
      onPlayhead: (sample) => this.handleAudioPlayhead(sample),
    });
    this.unsubscribeContext = this.client.subscribeContext(() => {
      this.handleContextChange();
    });
    this.publish("paused", 0);
  }

  async setProject(snapshot: ProjectSnapshot): Promise<void> {
    if (this.disposed) return;
    const previousProjectId = this.project?.document.projectId;
    const preserveFrame = previousProjectId === snapshot.document.projectId;
    const wasPlaying = this.desiredPlaying;
    this.softwareFallbackActive = false;
    this.softwareBusy = false;
    this.stopAnimation();
    this.cancelStill();
    this.stopSoftware();
    this.videoPool.pause();
    this.audioClock.pause();
    const token = ++this.operation;
    this.project = snapshot;
    this.plan = undefined;
    this.ensureArtifactScope();
    clearCanvasBackground(this.canvas, snapshot.document.profile.background);
    if (!preserveFrame) this.frame = 0;
    this.publish("buffering", this.frame);
    try {
      await this.refreshInternal(token, wasPlaying);
    } catch (error) {
      if (this.isCurrent(token) && !isAbortError(error)) this.fail(error);
    }
  }
  async refresh(): Promise<void> {
    if (this.disposed) return;
    if (!this.project) {
      this.fail(new Error("Open a project before refreshing its preview."));
      return;
    }
    this.ensureArtifactScope();
    this.softwareFallbackActive = false;
    this.softwareBusy = false;
    const wasPlaying = this.desiredPlaying;
    this.stopAnimation();
    this.cancelStill();
    this.stopSoftware();
    this.videoPool.pause();
    this.audioClock.pause();
    const token = ++this.operation;
    this.plan = undefined;
    clearCanvasBackground(this.canvas, this.project.document.profile.background);
    this.publish("buffering", this.frame);
    try {
      await this.refreshInternal(token, wasPlaying);
    } catch (error) {
      if (this.isCurrent(token) && !isAbortError(error)) this.fail(error);
    }

  }

  async play(): Promise<void> {
    if (this.disposed) return;
    if (!this.project) {
      this.fail(new Error("Open a project before playing preview."));
      return;
    }
    this.desiredPlaying = true;
    if (!this.plan) {
      await this.refresh();
      return;
    }
    this.cancelStill();
    const token = ++this.operation;
    if (this.plan.durationFrames === 0) {
      this.desiredPlaying = false;
      this.drawApproximate();
      this.publish("paused", 0);
      return;
    }
    if (this.transport === "ended") this.frame = 0;
    try {
      await this.callPreview({ action: "play" });
      if (!this.isCurrent(token) || !this.desiredPlaying) return;
      if (this.effectiveQuality() === "software") await this.startSoftware(this.frame);
      if (!this.isCurrent(token) || !this.desiredPlaying) return;
      await this.syncInputs(this.frame, true, token);
      if (!this.isCurrent(token) || !this.desiredPlaying) return;
      if (this.effectiveQuality() === "software") await this.awaitSoftwareFrame(token, this.frame);
      if (!this.isCurrent(token) || !this.desiredPlaying) return;
      const startSample = sampleAtFrame(this.frame, this.plan.fpsNum, this.plan.fpsDen);
      await this.audioClock.play(startSample);
      if (!this.isCurrent(token) || !this.desiredPlaying) return;
      this.startAnimation();
      if (this.transport !== "error") this.publish("playing", this.frame);
    } catch (error) {
      if (this.isCurrent(token) && !isAbortError(error)) {
        this.desiredPlaying = false;
        this.fail(error);
      }
    }
  }

  pause(): void {
    if (this.disposed) return;
    const wasPlaying = this.desiredPlaying;
    this.desiredPlaying = false;
    ++this.operation;
    this.stopAnimation();
    this.cancelStill();
    this.audioClock.pause();
    this.videoPool.pause();
    if (wasPlaying) this.stopSoftware();
    this.softwareBusy = false;
    void this.callPreview({ action: "pause" }).catch(() => undefined);
    if (this.plan) {
      this.drawApproximate();
      this.scheduleStill();
      this.publish("paused", this.frame);
    } else {
      this.publish("paused", this.frame);
    }
  }

  async seek(frame: number): Promise<void> {
    if (this.disposed) return;
    checkedFrame(frame, "frame");
    if (!this.plan) {
      this.frame = frame;
      this.publish(this.desiredPlaying ? "buffering" : "paused", frame);
      return;
    }
    const target = clampFrame(frame, this.plan.durationFrames);
    const wasPlaying = this.desiredPlaying;
    this.stopAnimation();
    this.cancelStill();
    this.stopSoftware();
    this.audioClock.pause();
    this.videoPool.pause();
    const token = ++this.operation;
    this.frame = target;
    if (this.plan.durationFrames === 0) {
      this.drawApproximate();
      this.publish("paused", 0);
      return;
    }
    this.publish("buffering", target);
    try {
      await this.callPreview({ action: "seek", frame: target });
      if (!this.isCurrent(token)) return;
      await this.audioClock.seek(sampleAtFrame(target, this.plan.fpsNum, this.plan.fpsDen));
      if (!this.isCurrent(token)) return;
      await this.syncInputs(target, false, token);
      if (!this.isCurrent(token)) return;
      if (wasPlaying && this.desiredPlaying) {
        if (this.effectiveQuality() === "software") await this.startSoftware(target);
        if (!this.isCurrent(token) || !this.desiredPlaying) return;
        await this.syncInputs(target, true, token);
        if (!this.isCurrent(token) || !this.desiredPlaying) return;
        if (this.effectiveQuality() === "software") await this.awaitSoftwareFrame(token, target);
        if (!this.isCurrent(token) || !this.desiredPlaying) return;
        await this.audioClock.play(sampleAtFrame(target, this.plan.fpsNum, this.plan.fpsDen));
        if (!this.isCurrent(token) || !this.desiredPlaying) return;
        this.startAnimation();
        this.publish("playing", target);
      } else {
        this.desiredPlaying = false;
        this.drawApproximate();
        this.scheduleStill();
        this.publish("paused", target);
      }
    } catch (error) {
      if (this.isCurrent(token) && !isAbortError(error)) this.fail(error);
    }
  }

  async step(delta: number): Promise<void> {
    if (!Number.isSafeInteger(delta)) throw new Error("step delta must be a safe integer.");
    await this.seek(this.frame + delta);
  }

  async setQuality(quality: PreviewQuality): Promise<void> {
    if (this.disposed || (this.quality === quality && !this.softwareFallbackActive)) return;
    const wasPlaying = this.desiredPlaying;
    this.stopAnimation();
    this.cancelStill();
    this.stopSoftware();
    this.videoPool.pause();
    this.audioClock.pause();
    this.quality = quality;
    this.softwareFallbackActive = false;
    this.softwareBusy = false;
    const token = ++this.operation;
    if (!this.plan || this.plan.durationFrames === 0) {
      this.desiredPlaying = false;
      this.drawApproximate();
      this.publish("paused", this.frame);
      return;
    }
    try {
      await this.syncInputs(this.frame, false, token);
      if (!this.isCurrent(token)) return;
      if (wasPlaying && this.desiredPlaying) {
        if (this.effectiveQuality() === "software") await this.startSoftware(this.frame);
        if (!this.isCurrent(token) || !this.desiredPlaying) return;
        await this.syncInputs(this.frame, true, token);
        if (!this.isCurrent(token) || !this.desiredPlaying) return;
        if (this.effectiveQuality() === "software") await this.awaitSoftwareFrame(token, this.frame);
        if (!this.isCurrent(token) || !this.desiredPlaying) return;
        await this.audioClock.play(sampleAtFrame(this.frame, this.plan.fpsNum, this.plan.fpsDen));
        if (!this.isCurrent(token) || !this.desiredPlaying) return;
        this.startAnimation();
        this.publish("playing", this.frame);
      } else {
        this.drawApproximate();
        this.scheduleStill();
        this.publish("paused", this.frame);
      }
    } catch (error) {
      if (this.isCurrent(token) && !isAbortError(error)) this.fail(error);
    }
  }

  dispose(): void {
    if (this.disposed) return;
    this.disposed = true;
    this.unsubscribeContext();
    this.desiredPlaying = false;
    ++this.operation;
    this.stopAnimation();
    this.cancelStill();
    this.stopSoftware();
    this.videoPool.dispose();
    this.audioClock.dispose();
    this.composer.dispose();
    for (const source of this.images.values()) closeImage(source.source);
    this.images.clear();
    for (const url of this.ownedObjectUrls) URL.revokeObjectURL(url);
    this.ownedObjectUrls.clear();
    this.artifactUrls.clear();
    this.artifactUrlPromises.clear();
    this.artifactCacheScope = undefined;
    this.project = undefined;
    this.plan = undefined;
  }
  private handleContextChange(): void {
    if (this.disposed) return;
    this.desiredPlaying = false;
    this.stopAnimation();
    this.cancelStill();
    this.stopSoftware();
    this.softwareFallbackActive = false;
    this.softwareBusy = false;
    this.videoPool.pause();
    this.audioClock.pause();
    ++this.operation;
    this.plan = undefined;
    this.invalidateArtifactCache();
    this.artifactCacheScope = undefined;
    this.publish("buffering", this.frame);
  }

  private artifactScope(): string {
    const context = this.client.getContext();
    const project = this.project;
    return [
      context.generation,
      context.projectId ?? "",
      project?.workspaceId ?? "",
      project?.document.projectId ?? "",
    ].join(":");
  }

  private ensureArtifactScope(): string {
    const scope = this.artifactScope();
    if (this.artifactCacheScope === scope) return scope;
    this.invalidateArtifactCache();
    this.artifactCacheScope = scope;
    return scope;
  }

  private invalidateArtifactCache(): void {
    for (const source of this.images.values()) closeImage(source.source);
    this.images.clear();
    for (const url of this.artifactUrls.values()) {
      if (this.ownedObjectUrls.has(url)) {
        URL.revokeObjectURL(url);
        this.ownedObjectUrls.delete(url);
      }
    }
    this.artifactUrls.clear();
    this.artifactUrlPromises.clear();
  }


  private async refreshInternal(token: number, resume: boolean): Promise<void> {
    const project = this.project;
    if (!project || !this.isCurrent(token)) return;
    this.ensureArtifactScope();
    const reply = await this.callPreview({ action: "plan", revision: project.document.revision });
    if (!this.isCurrent(token)) throw staleError();
    if (reply.kind !== "plan" || !reply.plan) throw new Error("Native preview did not return a render plan.");
    const plan = validatePlan(reply.plan);
    if (plan.projectId !== project.document.projectId || plan.revision !== project.document.revision) {
      throw new Error("Native preview plan belongs to a different project revision.");
    }
    if (!this.isCurrent(token)) throw staleError();
    this.plan = plan;
    this.frame = clampFrame(this.frame, plan.durationFrames);
    this.composer.setPlan(plan);
    clearCanvasBackground(this.canvas, plan.background);
    this.audioClock.setPlan(plan.planHash, plan.audio.totalSamples);
    if (plan.durationFrames === 0) {
      this.desiredPlaying = false;
      this.publish("paused", 0);
      return;
    }
    await this.syncInputs(this.frame, false, token);
    if (!this.isCurrent(token)) throw staleError();
    if (resume && this.desiredPlaying && this.frame < plan.durationFrames) {
      if (this.effectiveQuality() === "software") await this.startSoftware(this.frame);
      if (!this.isCurrent(token) || !this.desiredPlaying) return;
      await this.syncInputs(this.frame, true, token);
      if (!this.isCurrent(token) || !this.desiredPlaying) return;
      if (this.effectiveQuality() === "software") await this.awaitSoftwareFrame(token, this.frame);
      if (!this.isCurrent(token) || !this.desiredPlaying) return;
      await this.audioClock.play(sampleAtFrame(this.frame, plan.fpsNum, plan.fpsDen));
      if (!this.isCurrent(token) || !this.desiredPlaying) return;
      this.startAnimation();
      this.publish("playing", this.frame);
    } else {
      this.desiredPlaying = false;
      this.drawApproximate();
      this.scheduleStill();
      this.publish("paused", this.frame);
    }
  }

  private async callPreview(request: PreviewAction, signal?: AbortSignal): Promise<NormalizedPreviewReply> {
    if (this.disposed) throw staleError();
    if (typeof this.hooks.callPreview !== "function") throw new Error("The desktop bridge does not expose the native preview action.");
    void signal;
    const value = await this.hooks.callPreview(request);
    return normalizePreviewReply(value);
  }

  private async getAudioWindow(request: AudioWindowRequest): Promise<AudioWindow> {
    const reply = await this.callPreview({ action: "render_audio_window", planHash: request.planHash, startSample: request.startSample, sampleCount: request.sampleCount }, request.signal);
    if (reply.kind !== "audio" || !reply.audio) throw new Error("Native preview did not return an audio window.");
    if (reply.audio.planHash !== request.planHash || reply.audio.startSample !== request.startSample || reply.audio.sampleCount !== request.sampleCount || reply.audio.sampleRate !== PLAYHEAD_SAMPLE_RATE || reply.audio.channels !== 2) throw new Error("Native audio window coordinates do not match the request.");
    const artifactId = validateReplyArtifact(reply, "audio");
    const bytes = await this.fetchArtifactBytes(artifactId, request.signal);
    if (bytes.byteLength !== request.sampleCount * 2 * Float32Array.BYTES_PER_ELEMENT) throw new Error("Native audio artifact has an invalid PCM length.");
    const pcm = new Float32Array(bytes);
    return { planHash: request.planHash, startSample: request.startSample, sampleCount: request.sampleCount, sampleRate: PLAYHEAD_SAMPLE_RATE, channels: 2, pcm };
  }
  private async syncInputs(frame: number, playing: boolean, token: number): Promise<void> {
    const plan = this.plan;
    if (!plan || !this.isCurrent(token)) return;
    const quality = this.effectiveQuality();
    const previous = this.lastSyncedInputs;
    if (previous && previous.token === token && previous.frame === frame && previous.playing === playing && previous.quality === quality && previous.planHash === plan.planHash) return;
    const useBrowserVideo = quality === "auto";
    const activeSegments = this.activeSegments(plan, frame);
    if (useBrowserVideo) {
      const mappings = await this.videoMappings(plan, frame, token, activeSegments);
      if (!this.isCurrent(token)) throw staleError();
      await this.videoPool.sync(mappings, playing);
    } else {
      this.videoPool.pause();
    }
    const rasters = this.activeRasters(plan, frame);
    const stillSegments = activeSegments.filter((segment) => segment.isStillImage);
    await Promise.all([
      ...rasters.map(async (overlay) => {
        await this.loadImageArtifact(overlay.rasterArtifactId, token);
      }),
      ...stillSegments.map(async (segment) => {
        await this.loadImageArtifact(segment.artifactId, token);
      }),
    ]);
    if (!this.isCurrent(token) || this.effectiveQuality() !== quality) throw staleError();
    const nextFrame = clampFrame(frame + 1, plan.durationFrames);
    const prewarm = this.prewarmedNextFrame;
    if (useBrowserVideo && nextFrame !== frame && (!prewarm || prewarm.token !== token || prewarm.frame !== nextFrame || prewarm.planHash !== plan.planHash)) {
      this.prewarmedNextFrame = { token, frame: nextFrame, planHash: plan.planHash };
      void this.videoMappings(plan, nextFrame, token)
        .then((nextMappings) => this.videoPool.prewarm(nextMappings))
        .catch(() => {
          if (this.prewarmedNextFrame?.token === token && this.prewarmedNextFrame.frame === nextFrame && this.prewarmedNextFrame.planHash === plan.planHash) this.prewarmedNextFrame = undefined;
        });
    }
    if (!(playing && this.effectiveQuality() === "software")) this.drawApproximate();
    this.lastSyncedInputs = { token, frame, playing, quality: this.effectiveQuality(), planHash: plan.planHash };
  }

  private async videoMappings(plan: RenderPlan, frame: number, token: number, activeSegments = this.activeSegments(plan, frame)): Promise<VideoMapping[]> {
    const segments = activeSegments.filter((segment) => !segment.isStillImage);
    const mappings = await Promise.all(segments.map(async (segment) => ({
      key: segment.clipId,
      src: await this.resolveArtifactUrl(segment.artifactId, undefined),
      sourceTimeSeconds: (segment.sourceStartFrame + Math.max(0, frame - segment.startFrame)) * plan.fpsDen / plan.fpsNum,
      frame,
      frameDurationSeconds: plan.fpsDen / plan.fpsNum,
    })));
    if (!this.isCurrent(token)) throw staleError();
    return mappings;
  }

  private activeSegments(plan: RenderPlan, frame: number): RenderSegment[] {
    const cached = this.activeSegmentCache;
    if (cached && cached.plan === plan && cached.frame === frame) return cached.segments;
    const segments: RenderSegment[] = [];
    for (const layer of plan.layers) {
      if (layer.kind !== "video") continue;
      for (const segment of layer.segments) {
        const active = frame >= segment.startFrame && frame < segment.endFrame;
        if (active && (segment.isStillImage || (frame >= segment.activeStartFrame && frame < segment.activeEndFrame))) segments.push(segment);
      }
    }
    this.activeSegmentCache = { plan, frame, segments };
    return segments;
  }

  private activeRasters(plan: RenderPlan, frame: number): RenderTextOverlay[] {
    const cached = this.activeRasterCache;
    if (cached && cached.plan === plan && cached.frame === frame) return cached.overlays;
    const overlays: RenderTextOverlay[] = [];
    for (const layer of plan.layers) {
      for (const overlay of layer.textOverlays) {
        if (frame >= overlay.startFrame && frame < overlay.endFrame) overlays.push(overlay);
      }
    }
    this.activeRasterCache = { plan, frame, overlays };
    return overlays;
  }

  private drawApproximate(): void {
    if (!this.plan || this.disposed) return;
    const rasters = new Map<string, RasterSource>();
    for (const [artifactId, image] of this.images) rasters.set(artifactId, image.source);
    const videos = new Map<string, VideoSnapshot>();
    const activeSegments = this.activeSegments(this.plan, this.frame);
    for (const segment of activeSegments) {
      if (segment.isStillImage) {
        const image = this.images.get(segment.artifactId);
        if (!image) continue;
        videos.set(segment.clipId, {
          key: segment.clipId,
          source: image.source,
          mediaTime: 0,
          capturedAtFrame: 0,
        });
        continue;
      }
      const snapshot = this.videoPool.snapshot(segment.clipId);
      if (!snapshot) continue;
      const frameDistance = this.frame - snapshot.capturedAtFrame;
      if (this.desiredPlaying && (frameDistance < 0 || frameDistance > LIVE_SNAPSHOT_TOLERANCE_FRAMES)) continue;
      videos.set(segment.clipId, snapshot);
    }
    const result = this.composer.drawFrame(this.frame, videos, rasters);
    if (this.desiredPlaying && this.effectiveQuality() === "auto" && result.missingVideoKeys.length > 0) {
      this.enterVideoBuffering();
    }
  }

  private async renderPausedStill(request: number, token: number, revision: number, planHash: string, frame: number): Promise<void> {
    if (request !== this.stillRequest || !this.isCurrent(token) || !this.plan || this.plan.durationFrames === 0 || this.desiredPlaying) return;
    try {
      const reply = await this.callPreview({ action: "render_frame", revision, frame });
      if (reply.kind !== "frame" || !reply.frame || reply.frame.planHash !== planHash || reply.frame.revision !== revision || reply.frame.frame !== frame) throw new Error("Native paused frame did not match the requested revision.");
      const source = await this.loadImageArtifact(validateReplyArtifact(reply, "frame"), token);
      if (request !== this.stillRequest || !this.isCurrent(token) || !this.plan || this.plan.planHash !== planHash || this.plan.revision !== revision || this.frame !== frame || this.desiredPlaying) {
        return;
      }
      const context = this.canvas.getContext("2d", { alpha: false });
      if (!context) throw new Error("Canvas 2D context is unavailable.");
      context.setTransform(1, 0, 0, 1, 0, 0);
      context.globalAlpha = 1;
      context.globalCompositeOperation = "copy";
      context.drawImage(source, 0, 0, this.plan.width, this.plan.height);
      context.globalCompositeOperation = "source-over";
      // The full-resolution still is authoritative while paused. Software JPEG
      // packets are only decoded into the retained playback queue below.
      this.preparePausedSoftware(token, frame);
    } catch (error) {
      if (request === this.stillRequest && this.isCurrent(token) && !isAbortError(error)) this.fail(error);
    }
  }

  private scheduleStill(): void {
    if (!this.plan || this.plan.durationFrames === 0 || this.desiredPlaying || this.disposed) return;
    this.cancelStill();
    const request = ++this.stillRequest;
    const token = this.operation;
    const revision = this.plan.revision;
    const planHash = this.plan.planHash;
    const frame = this.frame;
    this.preparePausedSoftware(token, frame);
    this.stillTimer = setTimeout(() => {
      this.stillTimer = undefined;
      void this.renderPausedStill(request, token, revision, planHash, frame);
    }, STILL_DEBOUNCE_MS);
  }

  private cancelStill(): void {
    ++this.stillRequest;
    clearTimeout(this.stillTimer);
    this.stillTimer = undefined;
  }

  private startAnimation(): void {
    this.stopAnimation();
    const tick = (): void => {
      this.animationHandle = undefined;
      if (this.disposed || !this.desiredPlaying || !this.plan || this.plan.durationFrames === 0) return;
      const next = frameAtSample(this.audioClock.sample, this.plan.fpsNum, this.plan.fpsDen, this.plan.durationFrames);
      const synced = this.lastSyncedInputs;
      const frameChanged = !synced
        || synced.token !== this.operation
        || synced.frame !== next
        || synced.playing !== true
        || synced.quality !== this.effectiveQuality()
        || synced.planHash !== this.plan.planHash;
      this.frame = next;
      if (this.effectiveQuality() === "software") this.pumpSoftware();
      else if (frameChanged) void this.updateMovingFrame(this.operation, next);
      this.animationHandle = requestAnimationFrame(tick);
    };
    this.animationHandle = requestAnimationFrame(tick);
  }

  private stopAnimation(): void {
    if (this.animationHandle !== undefined) cancelAnimationFrame(this.animationHandle);
    this.animationHandle = undefined;
    this.movingUpdate = undefined;
  }

  private async updateMovingFrame(token: number, frame: number): Promise<void> {
    if (this.movingUpdate || !this.plan || !this.desiredPlaying) return;
    const update = (async () => {
      try {
        await this.syncInputs(frame, true, token);
      } catch (error) {
        if (this.isCurrent(token) && !isAbortError(error)) {
          this.enterVideoBuffering(error instanceof Error ? error : new Error(String(error)));
        }
      }
    })();
    this.movingUpdate = update;
    try {
      await update;
    } finally {
      if (this.movingUpdate === update) this.movingUpdate = undefined;
    }
  }

  private handleAudioState(state: AudioClockState, sample: number, error?: Error): void {
    if (this.disposed) return;
    if (this.plan) this.frame = frameAtSample(sample, this.plan.fpsNum, this.plan.fpsDen, this.plan.durationFrames);
    if (this.plan?.durationFrames === 0 && state !== "error") {
      this.desiredPlaying = false;
      this.stopAnimation();
      this.videoPool.pause();
      this.stopSoftware();
      this.publish("paused", 0);
      return;
    }
    if (state === "error") {
      this.fail(error ?? new Error("Audio playback failed."));
      return;
    }
    if (state === "ended") {
      this.desiredPlaying = false;
      ++this.operation;
      this.stopAnimation();
      this.softwareBusy = false;
      this.stopSoftware();
      this.scheduleStill();
      this.publish("ended", this.frame);
      return;
    }
    if (state === "buffering" && this.desiredPlaying) this.publish("buffering", this.frame);
    if (state === "playing" && this.desiredPlaying) {
      if (this.effectiveQuality() === "software") {
        this.softwareAudioStarted = true;
        this.flushSoftwareAcknowledgements(this.operation, this.softwareEpoch);
      }
      this.publish("playing", this.frame);
    }
    if (state === "paused" && !this.desiredPlaying) this.publish("paused", this.frame);
  }

  private handleAudioPlayhead(sample: number): void {
    if (!this.plan || !this.desiredPlaying) return;
    const nextFrame = frameAtSample(sample, this.plan.fpsNum, this.plan.fpsDen, this.plan.durationFrames);
    if (nextFrame !== this.frame) {
      this.frame = nextFrame;
      this.publish(this.transport, this.frame);
    }
  }

  private effectiveQuality(): PreviewQuality {
    return this.quality === "software" || this.softwareFallbackActive ? "software" : "auto";
  }

  private enterVideoBuffering(error?: Error): void {
    if (!this.desiredPlaying || this.disposed) return;
    if (this.effectiveQuality() === "software") {
      this.publish("buffering", this.frame, error);
      return;
    }
    this.softwareFallbackActive = true;
    this.audioClock.pause();
    this.videoPool.pause();
    this.stopSoftware();
    const token = this.operation;
    if (typeof this.hooks.subscribePreviewSoftware !== "function") {
      this.fail(new Error("Browser video decoding is unavailable and the native software preview channel is unavailable."));
      return;
    }
    this.publish("buffering", this.frame, error);
    if (this.softwareBusy) return;
    this.softwareBusy = true;
    void this.startSoftware(this.frame)
      .then(async () => {
        if (!this.isCurrent(token) || !this.desiredPlaying || !this.plan) return;
        await this.syncInputs(this.frame, true, token);
        await this.awaitSoftwareFrame(token, this.frame);
        if (!this.isCurrent(token) || !this.desiredPlaying || !this.plan) return;
        await this.audioClock.play(sampleAtFrame(this.frame, this.plan.fpsNum, this.plan.fpsDen));
        if (!this.isCurrent(token) || !this.desiredPlaying) return;
        this.startAnimation();
        this.publish("playing", this.frame);
      })
      .catch((fallbackError: unknown) => {
        if (this.isCurrent(token) && !isAbortError(fallbackError)) {
          this.fail(fallbackError instanceof Error ? fallbackError : new Error(String(fallbackError)));
        }
      })
      .finally(() => {
        if (this.isCurrent(token)) this.softwareBusy = false;
      });
  }

  private publish(state: PreviewTransportState, frame: number, error?: Error): void {
    if (this.disposed) return;
    this.transport = state;
    const value: PreviewEngineState = {
      state,
      frame,
      durationFrames: this.plan?.durationFrames ?? 0,
      quality: this.effectiveQuality(),
      ...(error ? { error: error.message } : {}),
    };
    this.onState(value);
  }


  private fail(error: unknown): void {
    const value = error instanceof Error ? error : new Error(String(error));
    this.desiredPlaying = false;
    ++this.operation;
    this.softwareBusy = false;
    this.videoPool.pause();
    this.audioClock.pause();
    this.stopSoftware();
    this.publish("error", this.frame, value);
  }

  private isCurrent(token: number): boolean {
    return !this.disposed && this.operation === token;
  }

  private async resolveArtifactUrl(artifactId: string, signal: AbortSignal | undefined): Promise<string> {
    const scope = this.ensureArtifactScope();
    const cached = this.artifactUrls.get(artifactId);
    if (cached) return cached;
    const pending = this.artifactUrlPromises.get(artifactId);
    if (pending) {
      const url = await pending;
      if (this.ensureArtifactScope() !== scope) throw staleError();
      return url;
    }
    const promise = (async (): Promise<string> => {
      if (typeof this.hooks.artifactUrl === "function") {
        const value = await this.hooks.artifactUrl(artifactId);
        if (typeof value === "string" && value.length > 0) return value;
      }
      if (typeof this.hooks.fetchArtifact !== "function") throw new Error(`No app-managed URL is available for artifact ${artifactId}.`);
      const raw = await this.hooks.fetchArtifact(artifactId);
      const bytes = await toArrayBuffer(raw);
      const url = URL.createObjectURL(new Blob([bytes]));
      this.ownedObjectUrls.add(url);
      return url;
    })();
    this.artifactUrlPromises.set(artifactId, promise);
    try {
      const url = await promise;
      if (this.ensureArtifactScope() !== scope) {
        if (this.ownedObjectUrls.has(url)) {
          URL.revokeObjectURL(url);
          this.ownedObjectUrls.delete(url);
        }
        throw staleError();
      }
      this.artifactUrls.set(artifactId, url);
      this.trimArtifactUrls();
      return url;
    } finally {
      if (this.artifactUrlPromises.get(artifactId) === promise) this.artifactUrlPromises.delete(artifactId);
    }
  }

  private async fetchArtifactBytes(artifactId: string, signal: AbortSignal | undefined): Promise<ArrayBuffer> {
    const scope = this.ensureArtifactScope();
    if (typeof this.hooks.fetchArtifact === "function") {
      const raw = await this.hooks.fetchArtifact(artifactId);
      const bytes = await toArrayBuffer(raw);
      if (this.ensureArtifactScope() !== scope) throw staleError();
      return bytes;
    }
    const url = await this.resolveArtifactUrl(artifactId, signal);
    const response = await fetch(url, { signal });
    if (!response.ok) throw new Error(`Artifact fetch failed with HTTP ${response.status}.`);
    const bytes = await response.arrayBuffer();
    if (this.ensureArtifactScope() !== scope) throw staleError();
    return bytes;
  }

  private async loadImageArtifact(artifactId: string, token: number): Promise<CanvasImageSource> {
    const scope = this.ensureArtifactScope();
    const cached = this.images.get(artifactId);
    if (cached) {
      cached.lastUsed = ++this.imageAccess;
      return cached.source;
    }
    const url = await this.resolveArtifactUrl(artifactId, undefined);
    if (!this.isCurrent(token) || this.ensureArtifactScope() !== scope) throw staleError();
    let source: CanvasImageSource;
    try {
      const response = await fetch(url);
      if (!response.ok) throw new Error(`Image artifact fetch failed with HTTP ${response.status}.`);
      const blob = await response.blob();
      if (typeof createImageBitmap === "function") source = await createImageBitmap(blob);
      else throw new Error("ImageBitmap unavailable.");
    } catch {
      const image = new Image();
      image.decoding = "async";
      image.src = url;
      if (typeof image.decode === "function") await image.decode();
      else await new Promise<void>((resolve, reject) => {
        image.addEventListener("load", () => resolve(), { once: true });
        image.addEventListener("error", () => reject(new Error("Image artifact could not be decoded.")), { once: true });
      });
      source = image;
    }
    if (!this.isCurrent(token) || this.ensureArtifactScope() !== scope) {
      closeImage(source);
      throw staleError();
    }
    this.images.set(artifactId, { source, lastUsed: ++this.imageAccess });
    this.trimImages();
    return source;
  }

  private trimImages(): void {
    while (this.images.size > IMAGE_CACHE_LIMIT) {
      let candidateId: string | undefined;
      let candidate: CachedImage | undefined;
      for (const [artifactId, image] of this.images) {
        if (!candidate || image.lastUsed < candidate.lastUsed) {
          candidateId = artifactId;
          candidate = image;
        }
      }
      if (!candidateId || !candidate) break;
      this.images.delete(candidateId);
      closeImage(candidate.source);
    }
  }

  private trimArtifactUrls(): void {
    while (this.artifactUrls.size > URL_CACHE_LIMIT) {
      const first = this.artifactUrls.keys().next().value as string | undefined;
      if (!first) break;
      const url = this.artifactUrls.get(first);
      this.artifactUrls.delete(first);
      if (url && this.ownedObjectUrls.has(url)) {
        URL.revokeObjectURL(url);
        this.ownedObjectUrls.delete(url);
      }
    }
  }

  private async awaitSoftwareFrame(token: number, frame: number): Promise<void> {
    if (this.effectiveQuality() !== "software") return;
    const epoch = this.softwareEpoch;
    this.softwareAudioStartPending = true;
    try {
      await this.waitForSoftwareFrame(token, epoch, frame);
    } finally {
      if (this.softwareEpoch === epoch) this.softwareAudioStartPending = false;
    }
  }

  private waitForSoftwareFrame(token: number, epoch: number, frame: number): Promise<void> {
    if (!this.isCurrent(token) || !this.desiredPlaying || this.effectiveQuality() !== "software" || epoch !== this.softwareEpoch) {
      return Promise.reject(staleError());
    }
    if (this.softwarePresentedFrame >= 0) return Promise.resolve();
    const existing = this.softwareFrameWait;
    if (existing && existing.token === token && existing.epoch === epoch && existing.frame === frame) return existing.promise;
    this.cancelSoftwareFrameWait();
    let resolveWait: () => void = () => undefined;
    let rejectWait: (error: Error) => void = () => undefined;
    const promise = new Promise<void>((resolve, reject) => {
      resolveWait = resolve;
      rejectWait = (error) => reject(error);
    });
    const timer = setTimeout(() => {
      const wait = this.softwareFrameWait;
      if (!wait || wait.promise !== promise) return;
      this.softwareFrameWait = undefined;
      rejectWait(new Error(`Native software preview stalled before presenting a frame at or before ${frame}.`));
    }, SOFTWARE_FRAME_TIMEOUT_MS);
    this.softwareFrameWait = { token, epoch, frame, promise, resolve: resolveWait, reject: rejectWait, timer };
    return promise;
  }

  private cancelSoftwareFrameWait(): void {
    const wait = this.softwareFrameWait;
    if (!wait) return;
    clearTimeout(wait.timer);
    this.softwareFrameWait = undefined;
    wait.reject(staleError());
  }

  private acknowledgeSoftware(sequence: number): void {
    void Promise.resolve(this.hooks.acknowledgePreviewSoftware?.(sequence)).catch(() => undefined);
  }

  private retireSoftwareSequence(sequence: number, token: number, epoch: number): void {
    if (!this.isCurrent(token) || epoch !== this.softwareEpoch || this.effectiveQuality() !== "software" || !this.desiredPlaying) return;
    if (!this.softwareInFlightSequences.delete(sequence)) return;
    this.softwareRetiredSequences.add(sequence);
    this.flushSoftwareAcknowledgements(token, epoch);
  }

  private flushSoftwareAcknowledgements(token: number, epoch: number): void {
    if (!this.isCurrent(token) || epoch !== this.softwareEpoch || this.effectiveQuality() !== "software" || !this.desiredPlaying || this.softwareAudioStartPending || !this.softwareAudioStarted || this.softwareSequenceStart < 0) return;
    while (true) {
      const next = this.softwareAcknowledgedThrough + 1;
      if (!Number.isSafeInteger(next) || !this.softwareRetiredSequences.delete(next)) return;
      this.softwareAcknowledgedThrough = next;
      this.acknowledgeSoftware(next);
    }
  }
  private softwareStreamMatches(frame: number): boolean {
    const identity = this.softwareStreamIdentity;
    const plan = this.plan;
    const project = this.project;
    if (!identity || !plan || !project || frame < 0 || frame >= plan.durationFrames) return false;
    return identity.frame === frame
      && identity.generation === this.client.getContext().generation
      && identity.projectId === project.document.projectId
      && identity.revision === plan.revision
      && identity.planHash === plan.planHash;
  }

  private isCurrentSoftwareEpoch(epoch: number): boolean {
    return !this.disposed
      && epoch === this.softwareEpoch
      && this.softwareStreamIdentity !== undefined
      && this.softwareStreamMatches(this.softwareStreamIdentity.frame)
      && this.effectiveQuality() === "software";
  }

  private preparePausedSoftware(token: number, frame: number): void {
    const plan = this.plan;
    if (!this.isCurrent(token) || this.disposed || this.desiredPlaying || this.effectiveQuality() !== "software" || !plan || plan.durationFrames === 0) return;
    const target = clampFrame(frame, plan.durationFrames);
    if (target >= plan.durationFrames) return;
    const preparation = this.startSoftware(target);
    const epoch = this.softwareEpoch;
    void preparation.catch((error: unknown) => {
      if (this.disposed || this.desiredPlaying || this.softwareEpoch !== epoch || !this.softwareStreamMatches(target)) return;
      if (!isAbortError(error)) this.stopSoftware();
    });
  }

  private async startSoftware(frame: number): Promise<void> {
    if (this.effectiveQuality() !== "software" || !this.plan || !this.project || frame < 0 || frame >= this.plan.durationFrames) return;
    if (this.softwareStreamMatches(frame)) {
      if (!this.desiredPlaying || !this.softwareAudioStarted) {
        this.softwareAudioStartPending = true;
        this.softwareAudioStarted = false;
      }
      const pending = this.softwareSubscriptionPromise;
      if (pending) await pending;
      if (this.desiredPlaying) this.pumpSoftware();
      return;
    }
    if (typeof this.hooks.subscribePreviewSoftware !== "function") throw new Error("The native software preview channel is unavailable.");
    this.stopSoftware();
    const epoch = this.softwareEpoch;
    const context = {
      projectId: this.plan.projectId,
      generation: this.client.getContext().generation,
      revision: this.plan.revision,
      planHash: this.plan.planHash,
    };
    this.softwareStreamIdentity = { ...context, frame };
    this.softwareAudioStartPending = true;
    this.softwareAudioStarted = false;
    const listener = (packet: SoftwarePreviewPacket): void => {
      void this.acceptSoftwarePacket(packet, epoch).catch((error: unknown) => {
        if (!this.isCurrentSoftwareEpoch(epoch) || isAbortError(error)) return;
        if (this.desiredPlaying) this.fail(error);
        else this.stopSoftware();
      });
    };
    let result: (() => void) | undefined | Promise<(() => void) | undefined>;
    try {
      result = this.hooks.subscribePreviewSoftware(listener, context, frame);
    } catch (error) {
      if (this.isCurrentSoftwareEpoch(epoch)) this.stopSoftware();
      throw error;
    }
    const subscription = Promise.resolve(result);
    this.softwareSubscriptionPromise = subscription;
    try {
      const disposer = await subscription;
      if (!this.isCurrentSoftwareEpoch(epoch) || !this.softwareStreamMatches(frame)) {
        disposer?.();
        return;
      }
      this.softwareUnsubscribe = disposer;
    } catch (error) {
      if (this.isCurrentSoftwareEpoch(epoch)) this.stopSoftware();
      throw error;
    } finally {
      if (this.softwareSubscriptionPromise === subscription) this.softwareSubscriptionPromise = undefined;
    }
  }
  private closeSoftwareSource(source: CanvasImageSource | undefined): void {
    if (!source) return;
    if (typeof HTMLImageElement !== "undefined" && source instanceof HTMLImageElement) {
      source.removeAttribute("src");
    }
    closeImage(source);
    const url = this.softwareImageUrls.get(source);
    if (!url) return;
    this.softwareImageUrls.delete(source);
    URL.revokeObjectURL(url);
    this.ownedObjectUrls.delete(url);
  }

  private stopSoftware(): void {
    this.softwareEpoch += 1;
    this.cancelSoftwareFrameWait();
    this.softwareAudioStartPending = false;
    this.softwareAudioStarted = false;
    try {
      this.softwareUnsubscribe?.();
    } catch {
      // The native channel is already being retired.
    }
    this.softwareUnsubscribe = undefined;
    this.softwareSubscriptionPromise = undefined;
    this.softwareStreamIdentity = undefined;
    void Promise.resolve(this.hooks.cancelPreviewSoftware?.()).catch(() => undefined);
    for (const frame of this.softwareFrames.values()) this.closeSoftwareSource(frame.source);
    this.softwareFrames.clear();
    this.softwareInFlightSequences.clear();
    this.softwareRetiredSequences.clear();
    this.softwareSequenceStart = -1;
    this.softwareAcknowledgedThrough = -1;
    this.softwarePresentedFrame = -1;
  }

  private async acceptSoftwarePacket(packet: SoftwarePreviewPacket, epoch: number): Promise<void> {
    const plan = this.plan;
    const project = this.project;
    const identity = this.softwareStreamIdentity;
    if (!plan || !project || !identity || !this.isCurrentSoftwareEpoch(epoch)) return;
    const expected = softwarePreviewDimensions(plan.width, plan.height);
    const identityMatches = packet.generation === identity.generation
      && packet.projectId === identity.projectId
      && packet.revision === identity.revision
      && packet.planHash === identity.planHash
      && packet.contentType === "image/jpeg"
      && packet.width === expected.width
      && packet.height === expected.height;
    const sequenceValid = Number.isSafeInteger(packet.sequence) && packet.sequence >= 0;
    const frameValid = Number.isSafeInteger(packet.frame)
      && packet.frame >= identity.frame
      && packet.frame < plan.durationFrames;
    if (!identityMatches || !sequenceValid || !frameValid) {
      throw new Error("Native software preview returned a malformed frame for the active render plan.");
    }
    if (this.softwareSequenceStart < 0) {
      this.softwareSequenceStart = packet.sequence;
      this.softwareAcknowledgedThrough = packet.sequence - 1;
    }
    if (packet.sequence <= this.softwareAcknowledgedThrough
      || packet.sequence < this.softwareSequenceStart
      || this.softwareInFlightSequences.has(packet.sequence)
      || this.softwareRetiredSequences.has(packet.sequence)) {
      return;
    }
    if (this.softwareInFlightSequences.size >= SOFTWARE_QUEUE_LIMIT) {
      throw new Error("Native software preview exceeded its frame queue capacity.");
    }
    this.softwareInFlightSequences.add(packet.sequence);
    let source: CanvasImageSource;
    try {
      source = await this.decodeSoftwarePacket(packet);
    } catch (error) {
      if (this.isCurrentSoftwareEpoch(epoch)) this.softwareInFlightSequences.delete(packet.sequence);
      throw error;
    }
    if (!this.isCurrentSoftwareEpoch(epoch)) {
      this.closeSoftwareSource(source);
      return;
    }
    this.softwareFrames.set(packet.sequence, { packet, source });
    if (this.desiredPlaying) this.pumpSoftware();
  }
  private async decodeSoftwarePacket(packet: SoftwarePreviewPacket): Promise<CanvasImageSource> {
    const source = packet.data;
    if (source.byteLength === 0) throw new Error("Software preview packet omitted its binary payload.");
    const backing = source.buffer;
    let blobPart: ArrayBuffer | Uint8Array<ArrayBuffer>;
    if (backing instanceof ArrayBuffer) {
      blobPart = new Uint8Array(backing, source.byteOffset, source.byteLength);
    } else {
      const copy = new Uint8Array(new ArrayBuffer(source.byteLength));
      copy.set(source);
      blobPart = copy;
    }
    const blob = new Blob([blobPart], { type: packet.contentType });
    const url = URL.createObjectURL(blob);
    this.ownedObjectUrls.add(url);
    let image: HTMLImageElement | undefined;
    try {
      image = new Image();
      const decodedImage = image;
      const decoded = new Promise<void>((resolve, reject) => {
        decodedImage.addEventListener("load", () => resolve(), { once: true });
        decodedImage.addEventListener("error", () => reject(new Error("Software preview frame could not be decoded.")), { once: true });
      });
      decodedImage.src = url;
      await decoded;
      this.softwareImageUrls.set(decodedImage, url);
      return decodedImage;
    } catch (error) {
      image?.removeAttribute("src");
      URL.revokeObjectURL(url);
      this.ownedObjectUrls.delete(url);
      throw error;
    }
  }

  private pumpSoftware(): void {
    if (!this.plan || this.effectiveQuality() !== "software" || !this.desiredPlaying) return;
    const target = this.audioClock.state === "playing"
      ? frameAtSample(this.audioClock.sample, this.plan.fpsNum, this.plan.fpsDen, this.plan.durationFrames)
      : this.frame;
    const token = this.operation;
    const epoch = this.softwareEpoch;
    const readyFrames = [...this.softwareFrames.values()].sort((left, right) => (
      left.packet.frame - right.packet.frame || left.packet.sequence - right.packet.sequence
    ));
    let candidate: SoftwareFrame | undefined;
    for (const ready of readyFrames) {
      if (ready.packet.frame > target) break;
      if (ready.packet.frame <= this.softwarePresentedFrame) {
        this.softwareFrames.delete(ready.packet.sequence);
        this.closeSoftwareSource(ready.source);
        this.retireSoftwareSequence(ready.packet.sequence, token, epoch);
        continue;
      }
      if (candidate) {
        this.softwareFrames.delete(candidate.packet.sequence);
        this.closeSoftwareSource(candidate.source);
        this.retireSoftwareSequence(candidate.packet.sequence, token, epoch);
      }
      candidate = ready;
    }
    if (candidate) {
      if (this.canvas.width !== candidate.packet.width || this.canvas.height !== candidate.packet.height) {
        this.canvas.width = candidate.packet.width;
        this.canvas.height = candidate.packet.height;
      }
      const context = this.canvas.getContext("2d", { alpha: false });
      if (!context) return;
      this.softwareFrames.delete(candidate.packet.sequence);
      context.setTransform(1, 0, 0, 1, 0, 0);
      context.globalCompositeOperation = "copy";
      context.globalAlpha = 1;
      context.drawImage(candidate.source, 0, 0, candidate.packet.width, candidate.packet.height);
      context.globalCompositeOperation = "source-over";
      this.closeSoftwareSource(candidate.source);
      this.retireSoftwareSequence(candidate.packet.sequence, token, epoch);
      this.softwarePresentedFrame = candidate.packet.frame;
      const wait = this.softwareFrameWait;
      if (wait && wait.token === token && wait.epoch === epoch) {
        clearTimeout(wait.timer);
        this.softwareFrameWait = undefined;
        wait.resolve();
      }
    }
    if (this.softwarePresentedFrame < 0) {
      this.publish("buffering", target);
      const stalledFrame = this.frame;
      void this.waitForSoftwareFrame(token, epoch, stalledFrame).catch((error: unknown) => {
        if (this.isCurrent(token) && epoch === this.softwareEpoch && this.desiredPlaying && !isAbortError(error)) {
          this.fail(error);
        }
      });
      return;
    }
    if (this.softwareAudioStartPending || this.audioClock.state === "buffering") {
      this.publish("buffering", target);
    } else if (this.audioClock.state === "playing") {
      this.publish("playing", target);
    } else if (candidate) {
      const playToken = this.operation;
      void this.audioClock.play(sampleAtFrame(target, this.plan.fpsNum, this.plan.fpsDen))
        .then(() => {
          if (this.isCurrent(playToken) && this.desiredPlaying && this.effectiveQuality() === "software") this.publish("playing", this.frame);
        })
        .catch((error: unknown) => {
          if (this.isCurrent(playToken) && !isAbortError(error)) this.fail(error);
        });
    }
  }
}
