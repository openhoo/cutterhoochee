export interface VideoMapping {
  /** One key per active clip/source-time mapping, not merely per asset. */
  readonly key: string;
  readonly src: string;
  readonly sourceTimeSeconds: number;
  readonly frame: number;
  readonly frameDurationSeconds: number;
}

export interface VideoSnapshot {
  readonly key: string;
  readonly source: CanvasImageSource;
  readonly mediaTime: number;
  readonly capturedAtFrame: number;
}

export interface VideoDrift {
  readonly key: string;
  readonly expectedTimeSeconds: number;
  readonly actualTimeSeconds: number;
  readonly frameDurationSeconds: number;
  readonly consecutiveFrames: number;
}
export interface VideoUnavailable {
  readonly key: string;
  readonly source: string;
  readonly reason: string;
}

export interface VideoPoolOptions {
  readonly onSnapshot?: (snapshot: VideoSnapshot) => void;
  readonly onDrift?: (drift: VideoDrift) => void;
  readonly onUnavailable?: (failure: VideoUnavailable) => void;
  readonly maxEntries?: number;
}

interface DecoderEntry {
  readonly key: string;
  readonly video: HTMLVideoElement;
  mapping: VideoMapping;
  sourceIdentity: string;
  active: boolean;
  playing: boolean;
  ready: boolean;
  forceSeek: boolean;
  lastUsed: number;
  lastSeekTarget: number | undefined;
  lastSeekAt: number;
  consecutiveDrift: number;
  needsCapture: boolean;
  rvfcHandle: number | undefined;
  fallbackFrameHandle: number | undefined;
  capturePending: boolean;
  snapshot: VideoSnapshot | undefined;
  disposed: boolean;
}
const DEFAULT_MAX_ENTRIES = 12;
const SEEK_DEBOUNCE_MS = 180;
const INACTIVE_EVICTION_MS = 15_000;
const METADATA_TIMEOUT_MS = 1_000;

function nowMs(): number {
  return typeof performance !== "undefined" ? performance.now() : Date.now();
}
function closeSnapshot(snapshot: VideoSnapshot | undefined): void {
  const source = snapshot?.source;
  if (typeof ImageBitmap !== "undefined" && source instanceof ImageBitmap) source.close();
}

function stopVideo(video: HTMLVideoElement): void {
  video.pause();
  try {
    video.removeAttribute("src");
    video.load();
  } catch {
    // Teardown should remain best effort for a decoder that is already closed.
  }
}

/**
 * Owns muted HTML video decoders for active clip/source mappings. The pool is
 * intentionally independent of the React tree: decoder activation, RVFC
 * snapshots, and seeks survive ordinary component renders.
 */
export class VideoPool {
  private readonly onSnapshot?: VideoPoolOptions["onSnapshot"];
  private readonly onUnavailable?: VideoPoolOptions["onUnavailable"];
  private readonly onDrift?: VideoPoolOptions["onDrift"];
  private readonly maxEntries: number;
  private readonly entries = new Map<string, DecoderEntry>();
  private disposed = false;
  private playing = false;

  constructor(options: VideoPoolOptions = {}) {
    this.onSnapshot = options.onSnapshot;
    this.onDrift = options.onDrift;
    this.onUnavailable = options.onUnavailable;
    this.maxEntries = Math.max(2, options.maxEntries ?? DEFAULT_MAX_ENTRIES);
  }

  /**
   * Reconcile active mappings. Calling this once per render tick does not seek
   * continuously: a decoder seeks only on activation or when drift exceeds one
   * project frame.
   */
  async sync(mappings: readonly VideoMapping[], playing: boolean): Promise<void> {
    if (this.disposed) return;
    this.playing = playing;
    const activeKeys = new Set<string>();
    const readyOperations: Promise<void>[] = [];
    for (const mapping of mappings) {
      if (!Number.isFinite(mapping.sourceTimeSeconds) || mapping.sourceTimeSeconds < 0) continue;
      if (!Number.isFinite(mapping.frameDurationSeconds) || mapping.frameDurationSeconds <= 0) continue;
      activeKeys.add(mapping.key);
      const entry = this.getOrCreate(mapping);
      entry.active = true;
      entry.lastUsed = nowMs();
      readyOperations.push(this.updateEntry(entry));
    }
    for (const entry of this.entries.values()) {
      if (activeKeys.has(entry.key)) continue;
      entry.active = false;
      entry.playing = false;
      this.stopCallbacks(entry);
      entry.video.pause();
    }
    await Promise.allSettled(readyOperations);
    this.evictInactive();
  }

  /** Start loading incoming clips without making them audible or visible. */
  async prewarm(mappings: readonly VideoMapping[]): Promise<void> {
    if (this.disposed) return;
    const operations: Promise<void>[] = [];
    for (const mapping of mappings) {
      if (!Number.isFinite(mapping.sourceTimeSeconds) || mapping.sourceTimeSeconds < 0) continue;
      const entry = this.getOrCreate(mapping);
      entry.lastUsed = nowMs();
      operations.push(this.loadMetadata(entry));
    }
    await Promise.allSettled(operations);
    this.evictInactive();
  }

  snapshot(key: string): VideoSnapshot | undefined {
    return this.entries.get(key)?.snapshot;
  }

  snapshots(): ReadonlyMap<string, VideoSnapshot> {
    const result = new Map<string, VideoSnapshot>();
    for (const [key, entry] of this.entries) {
      if (entry.snapshot) result.set(key, entry.snapshot);
    }
    return result;
  }

  pause(): void {
    this.playing = false;
    for (const entry of this.entries.values()) {
      entry.playing = false;
      entry.video.pause();
      this.stopCallbacks(entry);
    }
  }

  dispose(): void {
    if (this.disposed) return;
    this.disposed = true;
    this.playing = false;
    for (const entry of this.entries.values()) this.disposeEntry(entry);
    this.entries.clear();
  }

  private getOrCreate(mapping: VideoMapping): DecoderEntry {
    const existing = this.entries.get(mapping.key);
    if (existing && existing.sourceIdentity === mapping.src) {
      const mappingChanged = existing.mapping.sourceTimeSeconds !== mapping.sourceTimeSeconds || existing.mapping.frame !== mapping.frame;
      if (mappingChanged && !this.playing) {
        existing.forceSeek = true;
        existing.needsCapture = true;
      }
      existing.mapping = mapping;
      existing.active = true;
      existing.disposed = false;
      return existing;
    }
    if (existing) this.disposeEntry(existing);
    const video = document.createElement("video");
    video.muted = true;
    video.defaultMuted = true;
    video.playsInline = true;
    video.autoplay = false;
    video.preload = "auto";
    video.controls = false;
    video.setAttribute("aria-hidden", "true");
    video.style.display = "none";
    video.src = mapping.src;
    const entry: DecoderEntry = {
      key: mapping.key,
      video,
      mapping,
      sourceIdentity: mapping.src,
      active: true,
      playing: false,
      ready: false,
      forceSeek: true,
      lastUsed: nowMs(),
      lastSeekTarget: undefined,
      lastSeekAt: Number.NEGATIVE_INFINITY,
      consecutiveDrift: 0,
      needsCapture: true,
      rvfcHandle: undefined,
      fallbackFrameHandle: undefined,
      capturePending: false,
      snapshot: undefined,
      disposed: false,
    };
    video.addEventListener("loadedmetadata", () => {
      if (entry.disposed) return;
      entry.ready = true;
    });
    video.addEventListener("canplay", () => {
      if (entry.disposed) return;
      entry.ready = true;
    });
    video.addEventListener("error", () => {
      if (entry.disposed) return;
      entry.ready = false;
      entry.consecutiveDrift = 0;
      this.reportUnavailable(entry, "The browser video decoder reported an error.");
    });
    this.entries.set(mapping.key, entry);
    return entry;
  }

  private async updateEntry(entry: DecoderEntry): Promise<void> {
    await this.loadMetadata(entry);
    if (entry.disposed || !entry.active) return;
    const seekRequested = entry.forceSeek;
    this.correctDrift(entry, false);
    if (this.playing) {
      if (!entry.playing) {
        try {
          await entry.video.play();
          if (entry.disposed || !entry.active) {
            entry.video.pause();
            return;
          }
          entry.playing = true;
          this.startCallbacks(entry);
        } catch (error) {
          entry.playing = false;
          this.stopCallbacks(entry);
          this.reportUnavailable(entry, error instanceof Error ? error.message : "The browser video decoder could not start playback.");
        }
      }
      return;
    }
    if (entry.playing) {
      entry.playing = false;
      entry.video.pause();
      this.stopCallbacks(entry);
    }
    if (entry.needsCapture || !entry.snapshot) {
      if (seekRequested) await this.settleSeek(entry);
      await this.capture(entry, entry.video.currentTime).catch(() => undefined);
    }
  }

  private async settleSeek(entry: DecoderEntry): Promise<void> {
    const video = entry.video;
    if (!video.seeking && Math.abs(video.currentTime - entry.mapping.sourceTimeSeconds) <= entry.mapping.frameDurationSeconds) {
      await new Promise<void>((resolve) => requestAnimationFrame(() => resolve()));
      return;
    }
    await new Promise<void>((resolve) => {
      let finished = false;
      const finish = (): void => {
        if (finished) return;
        finished = true;
        clearTimeout(timeout);
        video.removeEventListener("seeked", finish);
        resolve();
      };
      const timeout = setTimeout(finish, 250);
      video.addEventListener("seeked", finish, { once: true });
      if (!video.seeking) queueMicrotask(finish);
    });
  }

  private loadMetadata(entry: DecoderEntry): Promise<void> {
    if (entry.disposed || entry.ready) return Promise.resolve();
    const video = entry.video;
    if (video.readyState >= HTMLMediaElement.HAVE_METADATA) {
      entry.ready = true;
      return Promise.resolve();
    }
    return new Promise<void>((resolve) => {
      let finished = false;
      const finish = (): void => {
        if (finished) return;
        finished = true;
        clearTimeout(timeout);
        video.removeEventListener("loadedmetadata", finish);
        video.removeEventListener("error", finish);
        entry.ready = video.readyState >= HTMLMediaElement.HAVE_METADATA;
        resolve();
      };
      const timeout = setTimeout(() => {
        this.reportUnavailable(entry, "The browser video decoder did not provide metadata.");
        finish();
      }, METADATA_TIMEOUT_MS);
      video.addEventListener("loadedmetadata", finish, { once: true });
      video.addEventListener("error", finish, { once: true });
      video.load();
    });
  }

  private correctDrift(entry: DecoderEntry, fromCallback: boolean): void {
    if (entry.disposed || !entry.ready) return;
    const expected = entry.mapping.sourceTimeSeconds;
    const actual = entry.video.currentTime;
    const tolerance = entry.mapping.frameDurationSeconds;
    const drift = Math.abs(actual - expected);
    if (!Number.isFinite(actual) || drift > tolerance) {
      entry.consecutiveDrift += 1;
      const canSeek = entry.forceSeek || (entry.consecutiveDrift >= 2 && nowMs() - entry.lastSeekAt >= SEEK_DEBOUNCE_MS);
      if (canSeek) {
        try {
          entry.video.currentTime = expected;
          entry.lastSeekAt = nowMs();
          entry.forceSeek = false;
        } catch {
          // The next ready/callback pass will retry after native media recovers.
        }
      }
      if (fromCallback || entry.consecutiveDrift >= 2) {
        this.onDrift?.({
          key: entry.key,
          expectedTimeSeconds: expected,
          actualTimeSeconds: actual,
          frameDurationSeconds: tolerance,
          consecutiveFrames: entry.consecutiveDrift,
        });
      }
      return;
    }
    entry.consecutiveDrift = 0;
    entry.forceSeek = false;
  }
  private reportUnavailable(entry: DecoderEntry, reason: string): void {
    if (entry.disposed || !entry.active) return;
    this.onUnavailable?.({ key: entry.key, source: entry.sourceIdentity, reason });
  }

  private startCallbacks(entry: DecoderEntry): void {
    this.stopCallbacks(entry);
    const requestVideoFrameCallback = entry.video.requestVideoFrameCallback?.bind(entry.video);
    if (requestVideoFrameCallback) {
      const callback = (_now: number, metadata: VideoFrameCallbackMetadata): void => {
        entry.rvfcHandle = undefined;
        if (entry.disposed || !entry.active || !entry.playing || !this.playing) return;
        this.correctDrift(entry, true);
        void this.capture(entry, metadata.mediaTime)
          .catch((error) => {
            this.reportUnavailable(entry, error instanceof Error ? error.message : "The browser video decoder did not produce a frame.");
          })
          .finally(() => {
            if (!entry.disposed && entry.active && entry.playing && this.playing) {
              entry.rvfcHandle = entry.video.requestVideoFrameCallback!(callback);
            }
          });
      };
      entry.rvfcHandle = requestVideoFrameCallback(callback);
      return;
    }
    const fallback = (): void => {
      entry.fallbackFrameHandle = undefined;
      if (entry.disposed || !entry.active || !entry.playing || !this.playing) return;
      this.correctDrift(entry, true);
      void this.capture(entry, entry.video.currentTime)
        .catch((error) => {
          this.reportUnavailable(entry, error instanceof Error ? error.message : "The browser video decoder did not produce a frame.");
        })
        .finally(() => {
          if (!entry.disposed && entry.active && entry.playing && this.playing) {
            entry.fallbackFrameHandle = requestAnimationFrame(fallback);
          }
        });
    };
    entry.fallbackFrameHandle = requestAnimationFrame(fallback);
  }

  private stopCallbacks(entry: DecoderEntry): void {
    if (entry.rvfcHandle !== undefined) {
      entry.video.cancelVideoFrameCallback?.(entry.rvfcHandle);
      entry.rvfcHandle = undefined;
    }
    if (entry.fallbackFrameHandle !== undefined) {
      cancelAnimationFrame(entry.fallbackFrameHandle);
      entry.fallbackFrameHandle = undefined;
    }
  }

  private async capture(entry: DecoderEntry, mediaTime: number): Promise<void> {
    if (entry.disposed || !entry.active || entry.capturePending) return;
    if (entry.video.readyState < HTMLMediaElement.HAVE_CURRENT_DATA || entry.video.videoWidth <= 0 || entry.video.videoHeight <= 0) {
      entry.needsCapture = true;
      if (entry.playing) this.reportUnavailable(entry, "The browser video decoder has metadata but no current frame.");
      return;
    }
    try {
      let source: CanvasImageSource;
      try {
        source = await createImageBitmap(entry.video);
      } catch {
        source = await this.captureCanvas(entry.video);
      }
      if (entry.disposed || !entry.active) {
        closeSnapshot({ key: entry.key, source, mediaTime, capturedAtFrame: entry.mapping.frame });
        return;
      }
      closeSnapshot(entry.snapshot);
      entry.snapshot = {
        key: entry.key,
        source,
        mediaTime: Number.isFinite(mediaTime) ? mediaTime : entry.video.currentTime,
        capturedAtFrame: entry.mapping.frame,
      };
      entry.needsCapture = false;
      this.onSnapshot?.(entry.snapshot);
    } finally {
      entry.capturePending = false;
    }
  }

  private async captureCanvas(video: HTMLVideoElement): Promise<HTMLCanvasElement> {
    const width = Math.max(1, video.videoWidth || 1);
    const height = Math.max(1, video.videoHeight || 1);
    const canvas = document.createElement("canvas");
    canvas.width = width;
    canvas.height = height;
    const context = canvas.getContext("2d", { alpha: true });
    if (!context) throw new Error("Canvas 2D context is unavailable for video capture.");
    context.drawImage(video, 0, 0, width, height);
    return canvas;
  }

  private evictInactive(): void {
    const cutoff = nowMs() - INACTIVE_EVICTION_MS;
    const candidates = [...this.entries.values()]
      .filter((entry) => !entry.active && entry.lastUsed < cutoff)
      .sort((left, right) => left.lastUsed - right.lastUsed);
    while (this.entries.size > this.maxEntries && candidates.length > 0) {
      const entry = candidates.shift();
      if (!entry) break;
      this.disposeEntry(entry);
      this.entries.delete(entry.key);
    }
    while (this.entries.size > this.maxEntries) {
      let candidate: DecoderEntry | undefined;
      for (const entry of this.entries.values()) {
        if (entry.active) continue;
        if (!candidate || entry.lastUsed < candidate.lastUsed) candidate = entry;
      }
      if (!candidate) break;
      this.disposeEntry(candidate);
      this.entries.delete(candidate.key);
    }
  }

  private disposeEntry(entry: DecoderEntry): void {
    entry.disposed = true;
    entry.active = false;
    entry.playing = false;
    this.stopCallbacks(entry);
    stopVideo(entry.video);
    closeSnapshot(entry.snapshot);
    entry.snapshot = undefined;
  }
}
