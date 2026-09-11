import type * as React from "react";
import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { Lock, Magnet, Plus, Scissors, SlidersHorizontal, Volume2, VolumeX } from "lucide-react";

import type { EditOp, EditorClient, MediaClip, ProjectSnapshot, TextItem, TimelineSelection, TimelineSnapshot, Track } from "@cutterhoochee/shared";
import { callNative, formatDuration, formatTimecode, numberValue, record, replyPayload, stringValue, transitionRemovalOps } from "@/lib/native";
import { Button } from "@/components/ui/button";

export type TimelineProps = {
  client: EditorClient;
  snapshot: ProjectSnapshot;
  timeline: TimelineSnapshot | null;
  selection: TimelineSelection;
  onSelectionChange: (selection: TimelineSelection) => Promise<void>;
  onEdit: (label: string, operations: readonly EditOp[]) => Promise<void>;
  onNotice: (notice: string) => void;
};

type DragState = { clipId: string; originFrame: number; ghostFrame: number };
type ThumbnailFrame = { frame: number; artifactId: string; url: string };
type WaveformData = { artifactId: string; sampleRate: number; channels: number; sampleCount: number; peaks: readonly number[] };
type ArtifactPreview = {
  scope: string;
  sourceArtifactId?: string;
  frames?: Record<string, ThumbnailFrame>;
  waveform?: WaveformData;
  loading?: boolean;
  error?: string;
};
type ThumbnailRequest = { key: string; assetId: string; sourceArtifactId: string; frame: number };
type WaveformRequest = { key: string; assetId: string; sourceArtifactId: string };
type PreviewRequest =
  | { kind: "thumbnail"; request: ThumbnailRequest }
  | { kind: "waveform"; request: WaveformRequest };
type ContextClip = { clip: MediaClip; x: number; y: number; scope: string };

type VisibleClip = { clip: MediaClip; track: Track };
type VisibleText = {
  item: TextItem;
  startFrame: number;
  durationFrames: number;
  sourceStartFrame?: number;
};

const TRACK_ROW_HEIGHT = 58;
const RULER_HEIGHT = 30;
const TIMELINE_LABEL_WIDTH = 120;
const DEFAULT_SCALE = 1;
const MIN_SCALE = 0.1;
const MAX_SCALE = 4;
const SCALE_STEP = 0.05;
const HORIZONTAL_OVERSCAN_PX = 320;
const RULER_MIN_SPACING_PX = 56;
const TARGET_THUMBNAIL_WIDTH_PX = 64;
const MAX_THUMBNAILS_PER_CLIP = 12;
const MAX_THUMBNAILS_PER_ASSET = 96;
const MAX_WAVEFORM_BARS = 128;
const MAX_WAVEFORM_BYTES = 2 * 1024 * 1024;
const MEDIA_REQUEST_CONCURRENCY = 3;

export function Timeline({ client, snapshot, timeline, selection, onSelectionChange, onEdit, onNotice }: TimelineProps) {
  const [scale, setScale] = useState(DEFAULT_SCALE);
  const [scrollLeft, setScrollLeft] = useState(0);
  const [scrollTop, setScrollTop] = useState(0);
  const [viewportSize, setViewportSize] = useState({ width: 960, height: 420 });
  const [snapEnabled, setSnapEnabled] = useState(true);
  const [drag, setDrag] = useState<DragState | null>(null);
  const [rangeGhost, setRangeGhost] = useState<{ startFrame: number; endFrame: number; playheadFrame: number } | null>(null);
  const rangeGhostRef = useRef<{ startFrame: number; endFrame: number; playheadFrame: number } | null>(null);
  const [previews, setPreviews] = useState<Record<string, ArtifactPreview>>({});
  const [contextClip, setContextClip] = useState<ContextClip | null>(null);
  const viewportRef = useRef<HTMLDivElement>(null);
  const didRulerDrag = useRef(false);
  const rulerDragStart = useRef<number | null>(null);
  const requestedMediaKeysRef = useRef(new Set<string>());

  const tracks = snapshot.document.tracks;
  const duration = Math.max(timeline?.durationFrames ?? 0, 1);
  const contentWidth = Math.max(900, TIMELINE_LABEL_WIDTH + duration * scale + 32);
  const fpsNum = snapshot.document.profile.fpsNum;
  const fpsDen = snapshot.document.profile.fpsDen;
  const fps = fpsNum > 0 && fpsDen > 0 ? fpsNum / fpsDen : 30;
  const selectedClipId = selection.clipIds[0];
  const clientContext = client.getContext();
  const previewScope = `${clientContext.generation}:${clientContext.projectId ?? ""}:${snapshot.workspaceId}:${snapshot.document.projectId}:${snapshot.document.revision}`;
  const previewScopeRef = useRef(previewScope);
  previewScopeRef.current = previewScope;

  const clipByTrack = useMemo(() => {
    const map = new Map<string, MediaClip[]>();
    for (const clip of snapshot.document.clips) {
      const list = map.get(clip.trackId) ?? [];
      list.push(clip);
      map.set(clip.trackId, list);
    }
    for (const list of map.values()) list.sort((left, right) => left.startFrame - right.startFrame || left.id.localeCompare(right.id));
    return map;
  }, [snapshot.document.clips]);
  const clipById = useMemo(() => new Map(snapshot.document.clips.map((clip) => [clip.id, clip])), [snapshot.document.clips]);
  const textByTrack = useMemo(() => {
    const map = new Map<string, VisibleText[]>();
    for (const item of snapshot.document.textItems) {
      const range = projectTextRange(item, clipById);
      if (!range) continue;
      const list = map.get(item.trackId) ?? [];
      list.push({ item, ...range });
      map.set(item.trackId, list);
    }
    for (const list of map.values()) list.sort((left, right) => left.startFrame - right.startFrame || left.item.id.localeCompare(right.item.id));
    return map;
  }, [clipById, snapshot.document.textItems]);
  const assetById = useMemo(() => new Map(snapshot.document.assets.map((asset) => [asset.id, asset])), [snapshot.document.assets]);

  const visibleTop = Math.max(0, Math.floor(Math.max(0, scrollTop - RULER_HEIGHT) / TRACK_ROW_HEIGHT) - 2);
  const visibleBottom = Math.min(
    tracks.length,
    Math.ceil((Math.max(0, scrollTop - RULER_HEIGHT) + viewportSize.height) / TRACK_ROW_HEIGHT) + 2,
  );
  const visibleTracks = useMemo(() => tracks.slice(visibleTop, visibleBottom), [tracks, visibleTop, visibleBottom]);
  const visibleFrameStart = Math.max(0, Math.floor((scrollLeft - TIMELINE_LABEL_WIDTH - HORIZONTAL_OVERSCAN_PX) / scale));
  const visibleFrameEnd = Math.min(
    duration,
    Math.ceil((scrollLeft + viewportSize.width - TIMELINE_LABEL_WIDTH + HORIZONTAL_OVERSCAN_PX) / scale),
  );
  const visibleClipEntries = useMemo<VisibleClip[]>(() => {
    const entries: VisibleClip[] = [];
    for (const track of visibleTracks) {
      for (const clip of clipByTrack.get(track.id) ?? []) {
        const clipEnd = clip.startFrame + clip.durationFrames;
        if (clip.startFrame < visibleFrameEnd && clipEnd > visibleFrameStart) entries.push({ clip, track });
      }
    }
    return entries;
  }, [clipByTrack, visibleFrameEnd, visibleFrameStart, visibleTracks]);

  const thumbnailFramesByClip = useMemo(() => {
    const map = new Map<string, readonly number[]>();
    for (const { clip } of visibleClipEntries) {
      const video = assetById.get(clip.assetId)?.normalization?.video;
      if (video?.masterArtifactId && video.frameCount > 0) map.set(clip.id, thumbnailFrameNumbers(clip, video.frameCount, scale));
    }
    return map;
  }, [assetById, scale, visibleClipEntries]);
  const thumbnailRequests = useMemo(() => {
    const requests = new Map<string, ThumbnailRequest>();
    for (const { clip } of visibleClipEntries) {
      const video = assetById.get(clip.assetId)?.normalization?.video;
      if (!video?.masterArtifactId || video.frameCount <= 0) continue;
      for (const frame of thumbnailFramesByClip.get(clip.id) ?? []) {
        const key = `thumbnail:${clip.assetId}:${video.masterArtifactId}:${frame}`;
        requests.set(key, { key, assetId: clip.assetId, sourceArtifactId: video.masterArtifactId, frame });
      }
    }
    return [...requests.values()];
  }, [assetById, thumbnailFramesByClip, visibleClipEntries]);
  const waveformRequests = useMemo(() => {
    const requests = new Map<string, WaveformRequest>();
    for (const { clip } of visibleClipEntries) {
      const audio = assetById.get(clip.assetId)?.normalization?.audio;
      if (!audio?.pcmArtifactId || audio.sampleCount <= 0) continue;
      const key = `waveform:${clip.assetId}:${audio.pcmArtifactId}`;
      requests.set(key, { key, assetId: clip.assetId, sourceArtifactId: audio.pcmArtifactId });
    }
    return [...requests.values()];
  }, [assetById, visibleClipEntries]);
  const previewRequestKey = useMemo(
    () => [...thumbnailRequests.map((request) => request.key), ...waveformRequests.map((request) => request.key)].join("\u001f"),
    [thumbnailRequests, waveformRequests],
  );

  useEffect(() => {
    const element = viewportRef.current;
    if (!element) return;
    const update = () => {
      const next = { width: Math.max(1, element.clientWidth), height: Math.max(1, element.clientHeight) };
      setViewportSize((current) => current.width === next.width && current.height === next.height ? current : next);
    };
    update();
    if (typeof ResizeObserver === "undefined") return;
    const observer = new ResizeObserver(update);
    observer.observe(element);
    return () => observer.disconnect();
  }, []);

  const loadThumbnail = useCallback(async (request: ThumbnailRequest, scope: string, isCurrent: () => boolean) => {
    const requestKey = `${scope}:${request.key}`;
    if (requestedMediaKeysRef.current.has(requestKey)) return;
    requestedMediaKeysRef.current.add(requestKey);
    const stateKey = `${request.assetId}:thumbnail`;
    if (!isCurrent()) return;
    setPreviews((current) => {
      const previous = current[stateKey];
      const next = previous?.scope === scope && previous.sourceArtifactId === request.sourceArtifactId
        ? { ...previous }
        : { scope, sourceArtifactId: request.sourceArtifactId };
      next.loading = true;
      next.error = undefined;
      return { ...current, [stateKey]: next };
    });
    try {
      const reply = await callNative(client, { method: "media", params: { action: "thumbnail", assetId: request.assetId, frame: request.frame } });
      if (!isCurrent()) return;
      const data = replyPayload(reply);
      const artifact = record(data.artifact);
      const artifactId = stringValue(artifact.artifactId || data.artifactId);
      if (!artifactId) throw new Error("The thumbnail response omitted its managed artifact.");
      let url = stringValue(data.artifactUrl);
      if (!url) url = await client.resolveArtifactUrl(artifactId);
      if (!url) throw new Error("The thumbnail response omitted its artifact URL.");
      if (!isCurrent()) return;
      setPreviews((current) => {
        const previous = current[stateKey];
        if (!previous || previous.scope !== scope || previous.sourceArtifactId !== request.sourceArtifactId) return current;
        const frames = { ...(previous.frames ?? {}), [String(request.frame)]: { frame: request.frame, artifactId, url } };
        const boundedFrames = Object.fromEntries(
          Object.entries(frames).sort(([left], [right]) => Number(left) - Number(right)).slice(-MAX_THUMBNAILS_PER_ASSET),
        );
        return { ...current, [stateKey]: { ...previous, frames: boundedFrames, loading: false, error: undefined } };
      });
    } catch (error) {
      if (!isCurrent()) return;
      setPreviews((current) => {
        const previous = current[stateKey];
        if (!previous || previous.scope !== scope || previous.sourceArtifactId !== request.sourceArtifactId) return current;
        return { ...current, [stateKey]: { ...previous, loading: false, error: error instanceof Error ? error.message : "Thumbnail unavailable" } };
      });
    }
  }, [client]);

  const loadWaveform = useCallback(async (request: WaveformRequest, scope: string, isCurrent: () => boolean) => {
    const requestKey = `${scope}:${request.key}`;
    if (requestedMediaKeysRef.current.has(requestKey)) return;
    requestedMediaKeysRef.current.add(requestKey);
    const stateKey = `${request.assetId}:waveform`;
    if (!isCurrent()) return;
    setPreviews((current) => {
      const previous = current[stateKey];
      const next = previous?.scope === scope && previous.sourceArtifactId === request.sourceArtifactId
        ? { ...previous }
        : { scope, sourceArtifactId: request.sourceArtifactId };
      next.loading = true;
      next.error = undefined;
      return { ...current, [stateKey]: next };
    });
    try {
      const reply = await callNative(client, { method: "media", params: { action: "waveform", assetId: request.assetId } });
      if (!isCurrent()) return;
      const data = replyPayload(reply);
      const artifact = record(data.artifact);
      const artifactId = stringValue(artifact.artifactId || data.artifactId);
      if (!artifactId) throw new Error("The waveform response omitted its managed artifact.");
      const bytes = await client.fetchArtifact(artifactId);
      if (!isCurrent()) return;
      const waveform = parseWaveform(bytes, artifactId);
      if (!isCurrent()) return;
      setPreviews((current) => {
        const previous = current[stateKey];
        if (!previous || previous.scope !== scope || previous.sourceArtifactId !== request.sourceArtifactId) return current;
        return { ...current, [stateKey]: { ...previous, waveform, loading: false, error: undefined } };
      });
    } catch (error) {
      if (!isCurrent()) return;
      setPreviews((current) => {
        const previous = current[stateKey];
        if (!previous || previous.scope !== scope || previous.sourceArtifactId !== request.sourceArtifactId) return current;
        return { ...current, [stateKey]: { ...previous, loading: false, error: error instanceof Error ? error.message : "Waveform unavailable" } };
      });
    }
  }, [client]);

  useEffect(() => {
    requestedMediaKeysRef.current.clear();
    setPreviews({});
  }, [previewScope]);

  useEffect(() => {
    let stale = false;
    const scope = previewScope;
    const requestContext = client.getContext();
    const isCurrent = () => {
      const context = client.getContext();
      return !stale
        && previewScopeRef.current === scope
        && context.generation === requestContext.generation
        && context.projectId === requestContext.projectId;
    };
    const requests: PreviewRequest[] = [
      ...thumbnailRequests.map((request) => ({ kind: "thumbnail" as const, request })),
      ...waveformRequests.map((request) => ({ kind: "waveform" as const, request })),
    ];
    let cursor = 0;
    const worker = async () => {
      while (isCurrent()) {
        const next = requests[cursor++];
        if (!next) return;
        if (next.kind === "thumbnail") await loadThumbnail(next.request, scope, isCurrent);
        else await loadWaveform(next.request, scope, isCurrent);
      }
    };
    const workers = Math.min(MEDIA_REQUEST_CONCURRENCY, requests.length);
    void Promise.all(Array.from({ length: workers }, () => worker()));
    return () => {
      stale = true;
    };
  }, [client, loadThumbnail, loadWaveform, previewRequestKey, previewScope, thumbnailRequests, waveformRequests]);

  useEffect(() => {
    setContextClip((current) => {
      if (!current || current.scope !== previewScope) return null;
      const clip = snapshot.document.clips.find((candidate) => candidate.id === current.clip.id);
      return clip ? { ...current, clip } : null;
    });
  }, [previewScope, snapshot.document.clips]);

  const frameAtX = (clientX: number) => {
    const rect = viewportRef.current?.getBoundingClientRect();
    if (!rect) return 0;
    return Math.max(0, Math.round((clientX - rect.left + scrollLeft - TIMELINE_LABEL_WIDTH) / scale));
  };
  const beginRulerSelection = (event: React.PointerEvent<HTMLDivElement>) => {
    if (event.button !== 0) return;
    const startFrame = frameAtX(event.clientX);
    rulerDragStart.current = startFrame;
    const initialGhost = { startFrame, endFrame: startFrame, playheadFrame: startFrame };
    rangeGhostRef.current = initialGhost;
    setRangeGhost(initialGhost);
    event.currentTarget.setPointerCapture(event.pointerId);
    const move = (moveEvent: PointerEvent) => {
      const currentFrame = frameAtX(moveEvent.clientX);
      if (currentFrame !== startFrame) didRulerDrag.current = true;
      const start = Math.min(startFrame, currentFrame);
      const end = Math.max(startFrame, currentFrame);
      const nextGhost = { startFrame: start, endFrame: end, playheadFrame: currentFrame };
      rangeGhostRef.current = nextGhost;
      setRangeGhost(nextGhost);
    };
    const up = () => {
      const ghost = rangeGhostRef.current;
      rulerDragStart.current = null;
      rangeGhostRef.current = null;
      setRangeGhost(null);
      if (ghost && ghost.endFrame > ghost.startFrame) void onSelectionChange({ ...selection, playheadFrame: ghost.playheadFrame, range: { startFrame: ghost.startFrame, endFrame: ghost.endFrame } });
      else void onSelectionChange({ ...selection, playheadFrame: startFrame, range: undefined });
      window.removeEventListener("pointermove", move);
      window.removeEventListener("pointerup", up);
    };
    window.addEventListener("pointermove", move);
    window.addEventListener("pointerup", up);
  };

  const snapFrame = (frame: number, clipId?: string) => {
    if (!snapEnabled) return Math.max(0, frame);
    const candidates = [0, selection.playheadFrame, ...snapshot.document.clips.flatMap((clip) => clip.id === clipId ? [] : [clip.startFrame, clip.startFrame + clip.durationFrames])];
    const threshold = 6 / scale;
    const nearest = candidates.reduce<{ frame: number; distance: number } | null>((best, candidate) => {
      const distance = Math.abs(candidate - frame);
      return distance < threshold && (!best || distance < best.distance) ? { frame: candidate, distance } : best;
    }, null);
    return Math.max(0, nearest?.frame ?? frame);
  };

  const selectClip = async (clip: MediaClip, additive = false) => {
    const clipIds = additive ? selection.clipIds.includes(clip.id) ? selection.clipIds.filter((id) => id !== clip.id) : [...selection.clipIds, clip.id] : [clip.id];
    await onSelectionChange({ ...selection, clipIds, textIds: [], playheadFrame: clip.startFrame });
  };
  const selectText = async (entry: VisibleText, additive = false) => {
    const textIds = additive ? selection.textIds.includes(entry.item.id) ? selection.textIds.filter((id) => id !== entry.item.id) : [...selection.textIds, entry.item.id] : [entry.item.id];
    await onSelectionChange({ ...selection, clipIds: [], textIds, playheadFrame: entry.startFrame });
  };

  const toggleTrack = async (track: Track, field: "muted" | "locked") => {
    try {
      await onEdit(`Update ${track.name}`, [{ op: "update_track", trackId: track.id, [field]: !track[field] }]);
    } catch {
      // Workspace reports the native error.
    }
  };

  const addAssetAt = async (assetId: string, frame: number, track: Track) => {
    const asset = snapshot.document.assets.find((candidate) => candidate.id === assetId);
    if (!asset) return;
    const sourceFrames = asset.normalization?.video?.frameCount ?? asset.normalization?.audio?.durationFrames ?? Math.round(fps * 3);
    const durationFrames = Math.max(1, Math.min(sourceFrames, track.kind === "audio" ? (timeline?.durationFrames || sourceFrames) : sourceFrames));
    const clip: MediaClip = { id: crypto.randomUUID(), trackId: track.id, assetId, startFrame: snapFrame(frame), inFrame: 0, durationFrames, fit: "contain", centerX: 5000, centerY: 5000, scale: 10000, opacity: 10000, gainDb: 0, audioEnabled: track.kind !== "video" || Boolean(asset.normalization?.audio), fadeInFrames: 0, fadeOutFrames: 0 };
    try {
      await onEdit("Insert clip", [{ op: "insert_clip", clip }]);
    } catch {
      // Workspace reports the native error.
    }
  };

  const onDrop = async (event: React.DragEvent<HTMLDivElement>, track: Track) => {
    event.preventDefault();
    const assetId = event.dataTransfer.getData("application/x-cutterhoochee-asset");
    if (!assetId) return;
    await addAssetAt(assetId, frameAtX(event.clientX), track);
  };

  const onClipPointerDown = (event: React.PointerEvent<HTMLDivElement>, clip: MediaClip) => {
    if (event.button !== 0) return;
    event.stopPropagation();
    void selectClip(clip, event.shiftKey).catch(() => undefined);
    setDrag({ clipId: clip.id, originFrame: clip.startFrame, ghostFrame: clip.startFrame });
    event.currentTarget.setPointerCapture(event.pointerId);
    const move = (moveEvent: PointerEvent) => {
      const delta = Math.round((moveEvent.clientX - event.clientX) / scale);
      setDrag((current) => current && current.clipId === clip.id ? { ...current, ghostFrame: Math.max(0, current.originFrame + delta) } : current);
    };
    const up = (upEvent: PointerEvent) => {
      const delta = Math.round((upEvent.clientX - event.clientX) / scale);
      const frame = snapFrame(clip.startFrame + delta, clip.id);
      setDrag(null);
      event.currentTarget.releasePointerCapture(event.pointerId);
      if (frame !== clip.startFrame) {
        void onEdit("Move clip", [...transitionRemovalOps(snapshot, clip.id), { op: "move_clip", clipId: clip.id, trackId: clip.trackId, startFrame: frame }]).catch(() => undefined);
      }
      window.removeEventListener("pointermove", move);
      window.removeEventListener("pointerup", up);
    };
    window.addEventListener("pointermove", move);
    window.addEventListener("pointerup", up);
  };

  const splitClip = async (clip: MediaClip) => {
    const frame = selection.playheadFrame;
    if (frame <= clip.startFrame || frame >= clip.startFrame + clip.durationFrames) {
      onNotice("Place the playhead inside the selected clip to split it.");
      return;
    }
    const transition = snapshot.document.transitions.find((candidate) => candidate.leftClipId === clip.id || candidate.rightClipId === clip.id);
    if (transition) {
      onNotice("Remove the dissolve before splitting this clip so the transition graph stays explicit.");
      return;
    }
    try {
      await onEdit("Split clip", [{ op: "split_clip", clipId: clip.id, frame, rightClipId: crypto.randomUUID() }]);
      setContextClip(null);
    } catch {
      // Workspace reports the native error.
    }
  };

  const deleteClip = async (clip: MediaClip) => {
    try {
      await onEdit("Remove clip", [{ op: "remove_clips", clipIds: [clip.id] }]);
      setContextClip(null);
    } catch {
      // Workspace reports the native error.
    }
  };

  const changeZoom = (requestedScale: number) => {
    const nextScale = Math.min(MAX_SCALE, Math.max(MIN_SCALE, requestedScale));
    const viewport = viewportRef.current;
    const currentScrollLeft = viewport?.scrollLeft ?? scrollLeft;
    const playheadViewportX = TIMELINE_LABEL_WIDTH + selection.playheadFrame * scale - currentScrollLeft;
    setScale(nextScale);
    requestAnimationFrame(() => {
      if (!viewportRef.current) return;
      const nextScrollLeft = TIMELINE_LABEL_WIDTH + selection.playheadFrame * nextScale - playheadViewportX;
      const maxScrollLeft = Math.max(0, viewportRef.current.scrollWidth - viewportRef.current.clientWidth);
      viewportRef.current.scrollLeft = Math.min(maxScrollLeft, Math.max(0, nextScrollLeft));
    });
  };

  const addTitle = async () => {
    const textTrack = snapshot.document.tracks.find((track) => track.kind === "text");
    if (!textTrack) {
      onNotice("Add a text track before creating a title.");
      return;
    }
    try {
      await onEdit("Add title", [{ op: "add_text", item: { id: crypto.randomUUID(), trackId: textTrack.id, kind: "title", text: "Your title", style: "clean", color: { red: 241, green: 240, blue: 235, alpha: 255 }, fontSize: 64, positionX: 5000, positionY: 1800, lineBreaks: [], startFrame: selection.playheadFrame, durationFrames: Math.max(1, Math.round(fps * 3)) } }]);
    } catch {
      // Workspace reports the native error.
    }
  };

  return <div className="timeline-shell" onClick={() => setContextClip(null)}>
    <div className="timeline-toolbar"><div className="timeline-title"><span className="eyebrow">Edit</span><strong>Timeline</strong><span className="timeline-duration">{formatDuration(duration, fpsNum, fpsDen)}</span></div><div className="timeline-controls"><Button variant="ghost" size="icon" aria-label="Split selected clip" disabled={!selectedClipId} onClick={() => { const clip = snapshot.document.clips.find((candidate) => candidate.id === selectedClipId); if (clip) void splitClip(clip); }}><Scissors aria-hidden="true" /></Button><Button variant="ghost" size="icon" aria-label="Add title" onClick={() => void addTitle()}><Plus aria-hidden="true" /></Button><button type="button" className={snapEnabled ? "snap-toggle active" : "snap-toggle"} onClick={() => setSnapEnabled((enabled) => !enabled)} aria-pressed={snapEnabled}><Magnet aria-hidden="true" />Snap</button><label className="zoom-control"><SlidersHorizontal aria-hidden="true" /><input type="range" min={MIN_SCALE} max={MAX_SCALE} step={SCALE_STEP} value={scale} onChange={(event) => changeZoom(Number(event.target.value))} aria-label="Timeline zoom" /></label></div></div>
    <div className="timeline-scroll" ref={viewportRef} onScroll={(event) => { setScrollLeft(event.currentTarget.scrollLeft); setScrollTop(event.currentTarget.scrollTop); }} onClick={(event) => { if (event.target === event.currentTarget) void onSelectionChange({ ...selection, playheadFrame: frameAtX(event.clientX) }); }}>
      <div className="timeline-canvas" style={{ width: contentWidth, height: Math.max(220, RULER_HEIGHT + tracks.length * TRACK_ROW_HEIGHT) }}>
        <div className="timeline-ruler" style={{ width: contentWidth, left: 0 }} onPointerDown={beginRulerSelection} onClick={(event) => { if (didRulerDrag.current) { didRulerDrag.current = false; return; } void onSelectionChange({ ...selection, playheadFrame: frameAtX(event.clientX) }); }}>{rulerTicks(duration, fpsNum, fpsDen, scale).map((tick) => <span key={tick.frame} data-frame={tick.frame} style={{ left: TIMELINE_LABEL_WIDTH + tick.frame * scale }}>{tick.label}</span>)}</div>
        {rangeGhost ? <div className="timeline-range-selection" style={{ left: TIMELINE_LABEL_WIDTH + rangeGhost.startFrame * scale, width: Math.max(1, (rangeGhost.endFrame - rangeGhost.startFrame) * scale) }} /> : null}
        <div className="timeline-playhead" style={{ left: TIMELINE_LABEL_WIDTH + selection.playheadFrame * scale }} aria-label={`Playhead at ${formatTimecode(selection.playheadFrame, fpsNum, fpsDen)}`} />
        {visibleTracks.map((track, visibleIndex) => {
          const trackIndex = visibleTop + visibleIndex;
          const clips = clipByTrack.get(track.id) ?? [];
          const visibleClips = clips.filter((clip) => clip.startFrame < visibleFrameEnd && clip.startFrame + clip.durationFrames > visibleFrameStart);
          const textItems = textByTrack.get(track.id) ?? [];
          const visibleTextItems = textItems.filter((entry) => entry.startFrame < visibleFrameEnd && entry.startFrame + entry.durationFrames > visibleFrameStart);
          return <div className="track-row" key={track.id} style={{ top: RULER_HEIGHT + trackIndex * TRACK_ROW_HEIGHT }} onDragOver={(event) => { event.preventDefault(); event.dataTransfer.dropEffect = "copy"; }} onDrop={(event) => void onDrop(event, track)}><div className="track-label"><div className="track-label-name"><span className={`track-kind-dot ${track.kind}`} />{track.name}</div><div className="track-actions"><button type="button" className="track-icon" aria-label={track.muted ? `Unmute ${track.name}` : `Mute ${track.name}`} onClick={() => void toggleTrack(track, "muted")}>{track.muted ? <VolumeX aria-hidden="true" /> : <Volume2 aria-hidden="true" />}</button><button type="button" className="track-icon" aria-label={track.locked ? `Unlock ${track.name}` : `Lock ${track.name}`} onClick={() => void toggleTrack(track, "locked")}>{track.locked ? <Lock aria-hidden="true" /> : <UnlockIcon />}</button></div></div><div className="track-lane">{visibleClips.map((clip) => {
            const asset = assetById.get(clip.assetId);
            const videoSourceArtifactId = asset?.normalization?.video?.masterArtifactId;
            const audioSourceArtifactId = asset?.normalization?.audio?.pcmArtifactId;
            const thumbnailPreview = previewFor(previews[`${clip.assetId}:thumbnail`], previewScope, videoSourceArtifactId);
            const waveformPreview = previewFor(previews[`${clip.assetId}:waveform`], previewScope, audioSourceArtifactId);
            return <TimelineClip key={clip.id} clip={clip} displayLabel={asset?.original.fileName ?? "Offline media"} selected={selection.clipIds.includes(clip.id)} ghostFrame={drag?.clipId === clip.id ? drag.ghostFrame : undefined} scale={scale} thumbnailFrames={thumbnailFramesByClip.get(clip.id) ?? []} thumbnailPreview={thumbnailPreview} waveform={waveformPreview?.waveform} waveformLoading={waveformPreview?.loading} waveformError={waveformPreview?.error} showWaveform={track.kind !== "text" && Boolean(audioSourceArtifactId)} fpsNum={fpsNum} fpsDen={fpsDen} onPointerDown={onClipPointerDown} onContextMenu={(event) => { event.preventDefault(); event.stopPropagation(); setContextClip({ clip, x: event.clientX, y: event.clientY, scope: previewScope }); }} onClick={(event) => { event.stopPropagation(); void selectClip(clip, event.shiftKey); }} />;
          })}
          {visibleTextItems.map((entry) => <TimelineTextItem key={entry.item.id} entry={entry} selected={selection.textIds.includes(entry.item.id)} scale={scale} onClick={(event) => { event.stopPropagation(); void selectText(entry, event.shiftKey).catch(() => undefined); }} />)}
          {clips.length === 0 && textItems.length === 0 ? <span className="track-empty">Drop {track.kind === "text" ? "titles or captions" : "media"} here</span> : null}</div></div>;
        })}
      </div>
    </div>
    {contextClip ? <div className="timeline-context" style={{ left: contextClip.x, top: contextClip.y }} onClick={(event) => event.stopPropagation()}><button type="button" onClick={() => void splitClip(contextClip.clip).catch(() => undefined)}><Scissors aria-hidden="true" />Split at playhead</button><button type="button" onClick={() => void deleteClip(contextClip.clip).catch(() => undefined)}><TrashIcon />Remove clip</button>{snapshot.document.transitions.filter((transition) => transition.leftClipId === contextClip.clip.id || transition.rightClipId === contextClip.clip.id).map((transition) => <button type="button" key={transition.id} onClick={() => { setContextClip(null); void onEdit("Remove dissolve", [{ op: "remove_transition", transitionId: transition.id }]).catch(() => undefined); }}><XIcon />Remove dissolve</button>)}</div> : null}
  </div>;
}

type TimelineTextItemProps = {
  entry: VisibleText;
  selected: boolean;
  scale: number;
  onClick: (event: React.MouseEvent<HTMLButtonElement>) => void;
};

function TimelineTextItem({ entry, selected, scale, onClick }: TimelineTextItemProps) {
  const { item, startFrame, durationFrames, sourceStartFrame } = entry;
  const left = startFrame * scale;
  const width = Math.max(28, durationFrames * scale);
  const label = item.kind === "caption" ? "Caption" : "Title";
  return <button type="button" className={selected ? "timeline-text-item selected" : "timeline-text-item"} style={{ left, width }} data-text-id={item.id} data-start-frame={startFrame} data-duration-frames={durationFrames} data-source-start-frame={sourceStartFrame} aria-label={`${label}: ${item.text}`} title={`${item.id} · timeline ${startFrame}–${startFrame + durationFrames}${sourceStartFrame === undefined ? "" : ` · source ${sourceStartFrame}–${sourceStartFrame + durationFrames}`} `} onPointerDown={(event) => event.stopPropagation()} onClick={onClick}>
    <span className="timeline-text-kind">{item.kind === "caption" ? "CC" : "T"}</span><span className="timeline-text-label">{item.text}</span><span className="timeline-text-duration">{durationFrames}f</span>
  </button>;
}

type TimelineClipProps = {
  clip: MediaClip;
  displayLabel: string;
  selected: boolean;
  ghostFrame?: number;
  scale: number;
  thumbnailFrames: readonly number[];
  thumbnailPreview?: ArtifactPreview;
  waveform?: WaveformData;
  waveformLoading?: boolean;
  waveformError?: string;
  showWaveform: boolean;
  fpsNum: number;
  fpsDen: number;
  onPointerDown: (event: React.PointerEvent<HTMLDivElement>, clip: MediaClip) => void;
  onContextMenu: (event: React.MouseEvent<HTMLDivElement>, clip: MediaClip) => void;
  onClick: (event: React.MouseEvent<HTMLDivElement>) => void;
};

function TimelineClip({ clip, displayLabel, selected, ghostFrame, scale, thumbnailFrames, thumbnailPreview, waveform, waveformLoading, waveformError, showWaveform, fpsNum, fpsDen, onPointerDown, onContextMenu, onClick }: TimelineClipProps) {
  const left = (ghostFrame ?? clip.startFrame) * scale;
  const width = Math.max(28, clip.durationFrames * scale);
  const thumbnailImages: ThumbnailFrame[] = [];
  for (const frame of thumbnailFrames) {
    const image = thumbnailPreview?.frames?.[String(frame)];
    if (image) thumbnailImages.push(image);
  }
  const hasThumbnailSurface = thumbnailFrames.length > 0 || Boolean(thumbnailPreview);
  const thumbnailStatus = thumbnailImages.length > 0
    ? undefined
    : thumbnailPreview?.loading
      ? "Preparing thumbnails…"
      : thumbnailPreview?.error
        ? "Thumbnail unavailable"
        : "No thumbnail";
  const waveformPeaks = showWaveform && waveform ? waveformPeaksForClip(waveform, clip, fpsNum, fpsDen, Math.min(MAX_WAVEFORM_BARS, Math.max(1, Math.floor(width / 2)))) : [];
  return <div className={selected ? "timeline-clip selected" : "timeline-clip"} style={{ left, width, opacity: ghostFrame === undefined ? 1 : 0.62 }} data-start-frame={clip.startFrame} data-source-in-frame={clip.inFrame} onPointerDown={(event) => onPointerDown(event, clip)} onContextMenu={(event) => onContextMenu(event, clip)} onClick={onClick} title={`${clip.id} · timeline ${clip.startFrame}–${clip.startFrame + clip.durationFrames} · source ${clip.inFrame}–${clip.inFrame + clip.durationFrames}`}>
    {hasThumbnailSurface ? <div className="clip-preview-strip" aria-hidden="true">{thumbnailImages.map((image) => <img className="clip-preview-cell" key={`${image.artifactId}:${image.frame}`} src={image.url} alt="" draggable={false} data-source-frame={image.frame} />)}{thumbnailStatus ? <span className="clip-media-status">{thumbnailStatus}</span> : null}{thumbnailImages.length > 0 ? <span className="artifact-badge">native</span> : null}</div> : null}
    <div className="clip-info"><strong>{displayLabel}</strong><span>{clip.durationFrames}f</span></div>
    {showWaveform && waveformPeaks.length > 0 ? <div className="clip-waveform" aria-label="Audio waveform">{waveformPeaks.map((peak, index) => <span className="clip-waveform-bar" key={`${clip.id}:peak:${index}`} style={{ height: `${Math.round(peak * 100)}%` }} />)}</div> : null}{showWaveform && waveformPeaks.length === 0 && (waveformLoading || waveformError) ? <span className="clip-media-status">{waveformLoading ? "Preparing waveform…" : "Waveform unavailable"}</span> : null}
  </div>;
}

function projectTextRange(item: TextItem, clipById: ReadonlyMap<string, MediaClip>): { startFrame: number; durationFrames: number; sourceStartFrame?: number } | undefined {
  const validFrame = (value: number | undefined): value is number => typeof value === "number" && Number.isSafeInteger(value) && value >= 0;
  if (item.ownerClipId !== undefined) {
    const clip = clipById.get(item.ownerClipId);
    if (!clip || !validFrame(item.sourceStartFrame) || !validFrame(item.sourceDurationFrames) || item.sourceDurationFrames <= 0) return undefined;
    const clipSourceEnd = clip.inFrame + clip.durationFrames;
    const sourceEnd = item.sourceStartFrame + item.sourceDurationFrames;
    const sourceStart = Math.max(item.sourceStartFrame, clip.inFrame);
    const visibleSourceEnd = Math.min(sourceEnd, clipSourceEnd);
    if (!validFrame(sourceStart) || !validFrame(visibleSourceEnd) || visibleSourceEnd <= sourceStart) return undefined;
    const timelineStart = clip.startFrame + sourceStart - clip.inFrame;
    const timelineEnd = Math.min(clip.startFrame + clip.durationFrames, timelineStart + visibleSourceEnd - sourceStart);
    const visibleStart = Math.max(clip.startFrame, timelineStart);
    const visibleEnd = Math.min(clip.startFrame + clip.durationFrames, timelineEnd);
    if (!validFrame(visibleStart) || !validFrame(visibleEnd) || visibleEnd <= visibleStart) return undefined;
    return {
      startFrame: visibleStart,
      durationFrames: visibleEnd - visibleStart,
      sourceStartFrame: sourceStart + (visibleStart - timelineStart),
    };
  }
  if (!validFrame(item.startFrame) || !validFrame(item.durationFrames) || item.durationFrames <= 0) return undefined;
  return { startFrame: item.startFrame, durationFrames: item.durationFrames };
}

function thumbnailFrameNumbers(clip: MediaClip, frameCount: number, scale: number): number[] {
  if (!Number.isSafeInteger(frameCount) || frameCount <= 0 || clip.durationFrames <= 0) return [];
  const width = Math.max(28, clip.durationFrames * scale);
  const count = Math.min(MAX_THUMBNAILS_PER_CLIP, Math.max(1, Math.ceil(width / TARGET_THUMBNAIL_WIDTH_PX)));
  const maxOffset = Math.max(0, clip.durationFrames - 1);
  return [...new Set(Array.from({ length: count }, (_, index) => {
    const offset = count === 1 ? 0 : Math.floor((index * maxOffset) / (count - 1));
    return Math.max(0, Math.min(frameCount - 1, clip.inFrame + offset));
  }))];
}

function parseWaveform(bytes: Uint8Array, artifactId: string): WaveformData {
  if (bytes.byteLength === 0 || bytes.byteLength > MAX_WAVEFORM_BYTES) throw new Error("The waveform artifact exceeds the bounded timeline preview size.");
  let parsed: unknown;
  try {
    parsed = JSON.parse(new TextDecoder().decode(bytes)) as unknown;
  } catch {
    throw new Error("The waveform artifact is not valid JSON.");
  }
  const data = record(parsed);
  const sampleRate = numberValue(data.sampleRate);
  const channels = numberValue(data.channels);
  const sampleCount = numberValue(data.sampleCount);
  if (!Number.isSafeInteger(sampleRate) || sampleRate <= 0 || !Number.isSafeInteger(channels) || channels <= 0 || !Number.isSafeInteger(sampleCount) || sampleCount <= 0) throw new Error("The waveform artifact has invalid sample metadata.");
  if (!Array.isArray(data.peaks) || data.peaks.length === 0 || data.peaks.length > 65_536) throw new Error("The waveform artifact has no bounded peak data.");
  const peaks = data.peaks.map((value) => {
    if (typeof value !== "number" || !Number.isFinite(value) || value < 0 || value > 1) throw new Error("The waveform artifact contains an invalid peak.");
    return value;
  });
  return { artifactId, sampleRate, channels, sampleCount, peaks };
}

function waveformPeaksForClip(waveform: WaveformData, clip: MediaClip, fpsNum: number, fpsDen: number, barCount: number): number[] {
  if (fpsNum <= 0 || fpsDen <= 0 || waveform.sampleRate <= 0 || waveform.sampleCount <= 0 || waveform.peaks.length === 0 || clip.durationFrames <= 0) return [];
  const sourceStartSample = Math.max(0, Math.min(waveform.sampleCount, Math.floor(clip.inFrame * waveform.sampleRate * fpsDen / fpsNum)));
  const sourceEndSample = Math.max(sourceStartSample, Math.min(waveform.sampleCount, Math.ceil((clip.inFrame + clip.durationFrames) * waveform.sampleRate * fpsDen / fpsNum)));
  if (sourceEndSample <= sourceStartSample) return [];
  const firstPeak = Math.max(0, Math.min(waveform.peaks.length - 1, Math.floor(sourceStartSample * waveform.peaks.length / waveform.sampleCount)));
  const lastPeak = Math.max(firstPeak + 1, Math.min(waveform.peaks.length, Math.ceil(sourceEndSample * waveform.peaks.length / waveform.sampleCount)));
  const count = Math.min(Math.max(1, barCount), lastPeak - firstPeak);
  const peaks: number[] = [];
  for (let index = 0; index < count; index += 1) {
    const start = firstPeak + Math.floor(index * (lastPeak - firstPeak) / count);
    const end = Math.max(start + 1, firstPeak + Math.ceil((index + 1) * (lastPeak - firstPeak) / count));
    let peak = 0;
    for (let peakIndex = start; peakIndex < Math.min(lastPeak, end); peakIndex += 1) peak = Math.max(peak, waveform.peaks[peakIndex] ?? 0);
    peaks.push(peak);
  }
  return peaks;
}

function previewFor(preview: ArtifactPreview | undefined, scope: string, sourceArtifactId: string | undefined): ArtifactPreview | undefined {
  return preview && sourceArtifactId && preview.scope === scope && preview.sourceArtifactId === sourceArtifactId ? preview : undefined;
}

function rulerTicks(duration: number, fpsNum: number, fpsDen: number, scale: number): { frame: number; label: string }[] {
  const fps = fpsNum > 0 && fpsDen > 0 ? fpsNum / fpsDen : 30;
  const minimumSeconds = RULER_MIN_SPACING_PX / Math.max(0.01, scale * fps);
  const niceSeconds = [0.25, 0.5, 1, 2, 5, 10, 15, 30, 60, 120, 300, 600, 1_200, 3_600];
  const intervalSeconds = niceSeconds.find((candidate) => candidate >= minimumSeconds) ?? Math.ceil(minimumSeconds / 3_600) * 3_600;
  const stepFrames = Math.max(1, Math.round(intervalSeconds * fps), Math.ceil(duration / 500));
  const ticks: { frame: number; label: string }[] = [];
  for (let frame = 0; frame <= duration && ticks.length < 500; frame += stepFrames) ticks.push({ frame, label: formatDuration(frame, fpsNum, fpsDen) });
  const lastFrame = ticks[ticks.length - 1]?.frame;
  const minimumFrameSpacing = Math.max(1, Math.ceil(RULER_MIN_SPACING_PX / Math.max(0.01, scale)));
  if (lastFrame === undefined || (lastFrame !== duration && duration - lastFrame >= minimumFrameSpacing)) ticks.push({ frame: duration, label: formatDuration(duration, fpsNum, fpsDen) });
  return ticks;
}

function UnlockIcon() {
  return <span className="unlock-glyph" aria-hidden="true">⌁</span>;
}
function TrashIcon() {
  return <span aria-hidden="true">×</span>;
}
function XIcon() {
  return <span aria-hidden="true">×</span>;
}
