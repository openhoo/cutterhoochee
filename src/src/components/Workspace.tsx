import { useCallback, useEffect, useRef, useState } from "react";
import { listen } from "@tauri-apps/api/event";
import {
  ChevronDown,
  ChevronRight,
  CircleHelp,
  Download,
  Film,
  FolderOpen,
  History,
  SlidersHorizontal,
  Moon,
  PanelLeftClose,
  PanelLeftOpen,
  PanelRightClose,
  PanelRightOpen,
  Redo2,
  Settings2,
  Sun,
  Undo2,
  X,
} from "lucide-react";

import type {
  EditOp,
  EditorClient,
  EditorRequest,
  ProjectSnapshot,
  ProjectStatus,
  TimelineSelection,
  TimelineSnapshot,
} from "@cutterhoochee/shared";
import { Button } from "@/components/ui/button";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import {
  callNative,
  eventData,
  findTransitionForClip,
  formatDuration,
  formatTimecode,
  isEditorClientError,
  parseEvent,
  record,
  replyData,
  replyPayload,
  stringValue,
  transitionRemovalOps,
  type EventPayload,
} from "@/lib/native";
import { ChatPanel } from "@/components/ChatPanel";
import { ClipInspector } from "@/components/ClipInspector";
import { ExportDialog } from "@/components/ExportDialog";
import { MediaLibrary } from "@/components/MediaLibrary";
import { Preview } from "@/components/Preview";
import { ProviderSettings } from "@/components/ProviderSettings";
import { Timeline } from "@/components/Timeline";
import { TranscriptPanel } from "@/components/TranscriptPanel";

export type Theme = "dark" | "light";
export type WorkspaceTab = "media" | "transcript" | "inspector";

export type WorkspaceProps = {
  client: EditorClient;
  status: ProjectStatus;
  snapshot: ProjectSnapshot;
  timeline: TimelineSnapshot | null;
  connection: "connected" | "unavailable";
  theme: Theme;
  onThemeChange: (theme: Theme) => void;
  onRefresh: () => Promise<void>;
  onProjectStatus: (status: ProjectStatus) => void;
  onProjectSnapshot: (snapshot: ProjectSnapshot) => void;
  onTimeline: (timeline: TimelineSnapshot) => void;
  onClose: () => void;
  onOpenProject: () => Promise<void>;
};

type WorkspaceEvent = EventPayload & { event?: string };

type PermissionDetails = {
  operation?: string;
  path?: string;
  paths?: string[];
  canonicalExecutable?: string;
  arguments?: string[];
  cwd?: string;
  url?: string;
  method?: string;
  bodySha256?: string;
  bodyBytes?: number;
  targetIdentity?: { canonicalPath?: string; size?: number };
  offset?: number;
  length?: number;
  timeoutMs?: number;
  overwrite?: boolean;
};

type PermissionRequest = {
  operationId: string;
  scope?: { workspaceId?: string; projectId?: string | null; generation?: number };
  runId?: string;
  details: PermissionDetails;
  expiresAtMs?: number;
};

function permissionFromValue(value: unknown): PermissionRequest | null {
  const root = record(value);
  const candidate = record(root.requested || root.permission || root.data || root);
  const operationId = stringValue(candidate.operationId || candidate.id);
  if (!operationId) return null;
  const rawDetails = record(candidate.details);
  const details: PermissionDetails = {
    operation: stringValue(rawDetails.operation || candidate.operation) || undefined,
    path: stringValue(rawDetails.path || candidate.path) || undefined,
    paths: Array.isArray(rawDetails.paths)
      ? rawDetails.paths.filter((item): item is string => typeof item === "string")
      : undefined,
    canonicalExecutable: stringValue(rawDetails.canonicalExecutable || candidate.executable) || undefined,
    arguments: Array.isArray(rawDetails.arguments)
      ? rawDetails.arguments.filter((item): item is string => typeof item === "string")
      : Array.isArray(candidate.arguments)
        ? candidate.arguments.filter((item): item is string => typeof item === "string")
        : undefined,
    cwd: stringValue(rawDetails.cwd || candidate.cwd) || undefined,
    url: stringValue(rawDetails.url || candidate.url) || undefined,
    method: stringValue(rawDetails.method || candidate.method) || undefined,
    bodySha256: stringValue(rawDetails.bodySha256 || candidate.bodyHash) || undefined,
    bodyBytes: typeof rawDetails.bodyBytes === "number" ? rawDetails.bodyBytes : undefined,
    targetIdentity: rawDetails.targetIdentity && typeof rawDetails.targetIdentity === "object"
      ? {
        canonicalPath: stringValue(record(rawDetails.targetIdentity).canonicalPath) || undefined,
        size: typeof record(rawDetails.targetIdentity).size === "number" ? record(rawDetails.targetIdentity).size as number : undefined,
      }
      : undefined,
    offset: typeof rawDetails.offset === "number" ? rawDetails.offset : undefined,
    length: typeof rawDetails.length === "number" ? rawDetails.length : undefined,
    timeoutMs: typeof rawDetails.timeoutMs === "number" ? rawDetails.timeoutMs : undefined,
    overwrite: typeof rawDetails.overwrite === "boolean" ? rawDetails.overwrite : undefined,
  };
  const rawScope = record(candidate.scope);
  return {
    operationId,
    scope: Object.keys(rawScope).length > 0
      ? {
        workspaceId: stringValue(rawScope.workspaceId) || undefined,
        projectId: typeof rawScope.projectId === "string" ? rawScope.projectId : null,
        generation: typeof rawScope.generation === "number" ? rawScope.generation : undefined,
      }
      : undefined,
    runId: stringValue(candidate.runId) || undefined,
    details,
    expiresAtMs: typeof candidate.expiresAtMs === "number" ? candidate.expiresAtMs : undefined,
  };
}

function errorMessage(error: unknown): string {
  if (isEditorClientError(error)) return `${error.code}: ${error.message}`;
  if (error instanceof Error) return error.message;
  return "The native operation failed.";
}

function isTauriRuntime(): boolean {
  return typeof window !== "undefined" && "__TAURI_INTERNALS__" in window;
}

function eventText(event: WorkspaceEvent): string {
  const data = record(event.data);
  return stringValue(data.text || data.message || data.error || data.status);
}

export function Workspace({
  client,
  status,
  snapshot,
  timeline,
  connection,
  theme,
  onThemeChange,
  onRefresh,
  onProjectStatus,
  onProjectSnapshot,
  onTimeline,
  onClose,
  onOpenProject,
}: WorkspaceProps) {
  const [leftOpen, setLeftOpen] = useState(true);
  const [rightOpen, setRightOpen] = useState(true);
  const [leftTab, setLeftTab] = useState<WorkspaceTab>("media");
  const [selection, setSelection] = useState<TimelineSelection>(
    timeline?.selection ?? { clipIds: [], textIds: [], playheadFrame: 0 },
  );
  const [panelWidth, setPanelWidth] = useState(340);
  const [timelineHeight, setTimelineHeight] = useState(260);
  const [isDraggingChat, setIsDraggingChat] = useState(false);
  const [isDraggingTimeline, setIsDraggingTimeline] = useState(false);
  const [notice, setNotice] = useState<string | null>(null);
  const [permission, setPermission] = useState<PermissionRequest | null>(null);
  const [providerSettingsOpen, setProviderSettingsOpen] = useState(false);
  const [exportOpen, setExportOpen] = useState(false);
  const [eventLog, setEventLog] = useState<WorkspaceEvent[]>([]);
  const [transportPlaying, setTransportPlaying] = useState(false);
  const [transportFrame, setTransportFrame] = useState(selection.playheadFrame);
  const chatResizeStart = useRef<{ x: number; width: number } | null>(null);
  const timelineResizeStart = useRef<{ y: number; height: number } | null>(null);
  const permissionAnswering = useRef<string | null>(null);
  const permissionPollToken = useRef(0);

  const refresh = useCallback(async () => {
    const expectedContext = client.getContext();
    await onRefresh();
    const loadedContext = client.getContext();
    if (
      loadedContext.generation !== expectedContext.generation ||
      loadedContext.projectId !== expectedContext.projectId
    ) {
      return;
    }
    try {
      const nextTimeline = await client.timelineSnapshot();
      const currentContext = client.getContext();
      if (
        currentContext.generation !== expectedContext.generation ||
        currentContext.projectId !== expectedContext.projectId
      ) {
        return;
      }
      onTimeline(nextTimeline);
      setSelection(nextTimeline.selection);
    } catch {
      // The parent refresh already presents the authoritative native error.
    }
  }, [client, onRefresh, onTimeline]);

  const commit = useCallback(
    async (label: string, operations: readonly EditOp[]) => {
      if (operations.length === 0) return;
      try {
        await client.editProject(label, operations, snapshot.document.revision);
        await refresh();
      } catch (error) {
        if (isEditorClientError(error) && error.code === "REVISION_CONFLICT") {
          await refresh();
          setNotice("This project changed elsewhere. The local gesture was discarded; the latest timeline is shown.");
        } else {
          setNotice(errorMessage(error));
        }
        throw error;
      }
    },
    [client, refresh, snapshot.document.revision],
  );

  const updateSelection = useCallback(
    async (next: TimelineSelection) => {
      setSelection(next);
      try {
        const updated = await client.setTimelineSelection(next);
        onTimeline(updated);
      } catch (error) {
        setNotice(errorMessage(error));
      }
    },
    [client, onTimeline],
  );

  const projectCommand = useCallback(
    async (request: EditorRequest) => {
      try {
        return await client.call(request);
      } catch (error) {
        setNotice(errorMessage(error));
        throw error;
      }
    },
    [client],
  );

  useEffect(() => {
    setSelection(timeline?.selection ?? { clipIds: [], textIds: [], playheadFrame: 0 });
  }, [timeline]);

  useEffect(() => {
    if (!isTauriRuntime()) return;
    let disposed = false;
    let unlisten: (() => void) | undefined;
    void listen<unknown>("cutterhoochee://event", (event) => {
      if (disposed) return;
      const parsed = parseEvent(event.payload);
      if (!parsed) return;
      const context = client.getContext();
      if (
        parsed.generation === undefined ||
        !Number.isSafeInteger(parsed.generation) ||
        parsed.generation < 0 ||
        parsed.projectId !== context.projectId ||
        parsed.generation !== context.generation
      ) {
        return;
      }
      const nextEvent = parsed as WorkspaceEvent;
      setEventLog((current) => [...current.slice(-255), nextEvent]);
      const data = eventData(nextEvent);
      const eventKind = nextEvent.kind.toLowerCase();
      if (eventKind.includes("permission") && (eventKind.includes("pending") || eventKind.includes("requested") || eventKind === "permission")) {
        const request = permissionFromValue(data);
        if (request && permissionAnswering.current !== request.operationId) setPermission(request);
      }
      if (eventKind.includes("job") || eventKind.includes("media") || eventKind.includes("export")) {
        const text = eventText(nextEvent);
        if (text) setNotice(text);
        const eventStatus = stringValue(data.status || data.state).toLowerCase();
        const terminal = eventKind.includes("completed") || eventKind.includes("ready") || eventKind.includes("failed") || eventKind.includes("cancelled")
          || eventStatus === "completed" || eventStatus === "ready" || eventStatus === "failed" || eventStatus === "cancelled";
        if (terminal && (eventKind.includes("job") || eventKind.includes("media"))) void refresh();
      }
      const assistantToolCompleted =
        eventKind === "assistant_tool_end" &&
        data.isError !== true &&
        typeof data.error !== "string";
      const assistantSettled =
        eventKind === "assistant_settled" || eventKind === "assistant_end";
      if (assistantToolCompleted || assistantSettled) {
        void refresh().catch(() => undefined);
      }
    }).then((dispose) => {
      if (disposed) dispose();
      else unlisten = dispose;
    }).catch(() => {
      // A browser tab has no native event bridge; the status badge remains truthful.
    });
    return () => {
      disposed = true;
      unlisten?.();
    };
  }, [client, refresh]);

  useEffect(() => {
    const onMove = (event: PointerEvent) => {
      if (chatResizeStart.current) {
        const delta = chatResizeStart.current.x - event.clientX;
        const next = Math.min(520, Math.max(280, chatResizeStart.current.width + delta));
        setPanelWidth(next);
      }
      if (timelineResizeStart.current) {
        const delta = timelineResizeStart.current.y - event.clientY;
        const next = Math.min(520, Math.max(180, timelineResizeStart.current.height + delta));
        setTimelineHeight(next);
      }
    };

    const onUp = () => {
      chatResizeStart.current = null;
      timelineResizeStart.current = null;
      setIsDraggingChat(false);
      setIsDraggingTimeline(false);
    };
    window.addEventListener("pointermove", onMove);
    window.addEventListener("pointerup", onUp);
    return () => {
      window.removeEventListener("pointermove", onMove);
      window.removeEventListener("pointerup", onUp);
    };
  }, []);
  useEffect(() => {
    if (!isTauriRuntime()) return;
    let disposed = false;
    const poll = async () => {
      const pollToken = ++permissionPollToken.current;
      try {
        const reply = await callNative(client, {
          method: "permissions",
          params: { action: "pending", params: {} },
        });
        if (pollToken !== permissionPollToken.current) return;
        const payload = replyPayload(reply);
        const entries = Array.isArray(payload.data)
          ? payload.data
          : Array.isArray(payload.pending)
            ? payload.pending
            : [];
        const next = entries.map(permissionFromValue).find((value): value is PermissionRequest => value !== null) ?? null;
        if (!disposed && permissionAnswering.current !== next?.operationId) setPermission(next);
      } catch {
        // Permission polling is best-effort; native errors remain attached to the originating operation.
      }
    };
    void poll();
    const timer = window.setInterval(() => void poll(), 750);
    return () => {
      disposed = true;
      window.clearInterval(timer);
    };
  }, [client]);

  const answerPermission = useCallback(async (allow: boolean) => {
    const current = permission;
    if (!current || permissionAnswering.current === current.operationId) return;
    permissionAnswering.current = current.operationId;
    setPermission(null);
    try {
      await projectCommand({
        method: "permissions",
        params: {
          action: "answer",
          params: { operationId: current.operationId, allow },
        },
      });
    } catch {
      // projectCommand presents the authoritative native error.
    } finally {
      permissionAnswering.current = null;
    }
  }, [permission, projectCommand]);

  const selectedClip = snapshot.document.clips.find((clip) => selection.clipIds.includes(clip.id));
  const activeTrack = selectedClip
    ? snapshot.document.tracks.find((track) => track.id === selectedClip.trackId)
    : undefined;
  const selectedTransitions = selectedClip ? findTransitionForClip(snapshot, selectedClip.id) : [];
  const appendAsset = useCallback(async (assetId: string) => {
    const asset = snapshot.document.assets.find((candidate) => candidate.id === assetId);
    if (!asset) return;
    const trackKind = asset.kind === "audio" ? "audio" : "video";
    const track = snapshot.document.tracks.find((candidate) => candidate.kind === trackKind);
    if (!track) {
      setNotice(`Add a ${trackKind} track before inserting this asset.`);
      return;
    }
    const sourceFrames = asset.normalization?.video?.frameCount ?? asset.normalization?.audio?.durationFrames ?? Math.max(1, Math.round((snapshot.document.profile.fpsNum / snapshot.document.profile.fpsDen) * 3));
    const existingEnd = snapshot.document.clips
      .filter((candidate) => candidate.trackId === track.id)
      .reduce((end, candidate) => Math.max(end, candidate.startFrame + candidate.durationFrames), 0);
    const durationFrames = asset.kind === "audio" && (timeline?.durationFrames ?? 0) > 0
      ? Math.min(sourceFrames, timeline?.durationFrames ?? sourceFrames)
      : sourceFrames;
    const clip = {
      id: crypto.randomUUID(),
      trackId: track.id,
      assetId,
      startFrame: asset.kind === "audio" ? 0 : existingEnd,
      inFrame: 0,
      durationFrames: Math.max(1, durationFrames),
      fit: "contain" as const,
      centerX: 5000,
      centerY: 5000,
      scale: 10000,
      opacity: 10000,
      gainDb: 0,
      audioEnabled: asset.kind === "audio" || Boolean(asset.normalization?.audio),
      fadeInFrames: 0,
      fadeOutFrames: 0,
    };
    await commit("Insert clip", [{ op: "insert_clip", clip }]);
  }, [commit, snapshot.document.assets, snapshot.document.clips, snapshot.document.profile.fpsDen, snapshot.document.profile.fpsNum, snapshot.document.tracks, timeline?.durationFrames]);

  const handleHistory = useCallback(
    async (action: "undo" | "redo") => {
      try {
        const reply = await projectCommand({
          method: "project_history",
          params: {
            action,
            expectedRevision: snapshot.document.revision,
          },
        });
        const result = replyData<{ revision?: number }>(reply, "project_history");
        if (typeof result?.revision === "number") {
          await refresh();
        }
      } catch (error) {
        if (isEditorClientError(error) && error.code === "REVISION_CONFLICT") {
          await refresh().catch(() => undefined);
          setNotice("This project changed elsewhere. The latest timeline is shown; no undo was replayed.");
        }
        // projectCommand has already shown the actionable native error.
      }
    },
    [projectCommand, refresh, snapshot.document.revision],
  );

  const handlePlayState = useCallback((playing: boolean) => {
    setTransportPlaying(playing);
  }, []);

  useEffect(() => {
    const onKeyDown = (event: KeyboardEvent) => {
      const target = event.target as HTMLElement | null;
      const tag = target?.tagName.toLowerCase();
      const editing = tag === "input" || tag === "textarea" || tag === "select" || target?.isContentEditable;
      if (editing) return;
      const modifier = event.metaKey || event.ctrlKey;
      if (event.code === "Space") {
        event.preventDefault();
        setTransportPlaying((playing) => !playing);
      } else if (event.key === "ArrowLeft") {
        event.preventDefault();
        void updateSelection({ ...selection, playheadFrame: Math.max(0, selection.playheadFrame - 1) });
      } else if (event.key === "ArrowRight") {
        event.preventDefault();
        void updateSelection({ ...selection, playheadFrame: selection.playheadFrame + 1 });
      } else if (!modifier && event.key.toLowerCase() === "s") {
        event.preventDefault();
        if (selectedClip && selection.playheadFrame > selectedClip.startFrame && selection.playheadFrame < selectedClip.startFrame + selectedClip.durationFrames) {
          const transition = snapshot.document.transitions.find((candidate) => candidate.leftClipId === selectedClip.id || candidate.rightClipId === selectedClip.id);
          if (transition) {
            setNotice("Remove the dissolve before splitting this clip so the transition graph stays explicit.");
          } else {
            void commit("Split clip", [{ op: "split_clip", clipId: selectedClip.id, frame: selection.playheadFrame, rightClipId: crypto.randomUUID() }]);
          }
        }
      } else if (event.key === "Delete") {
        event.preventDefault();
        if (event.shiftKey && selection.range && selection.range.endFrame > selection.range.startFrame) {
          void commit("Remove range", [{ op: "remove_range", startFrame: selection.range.startFrame, endFrame: selection.range.endFrame, ripple: true }]);
        } else if (selection.clipIds.length > 0) {
          void commit("Remove clips", [{ op: "remove_clips", clipIds: selection.clipIds }]);
        }
      } else if (modifier && event.key.toLowerCase() === "z") {
        event.preventDefault();
        void handleHistory(event.shiftKey ? "redo" : "undo");
      } else if (modifier && event.key.toLowerCase() === "i") {
        event.preventDefault();
        void projectCommand({ method: "media", params: { action: "import" } }).catch(() => undefined);
      } else if (modifier && event.key.toLowerCase() === "s") {
        event.preventDefault();
        void projectCommand({ method: "project_save", params: {} }).catch(() => undefined);
      } else if (modifier && event.key.toLowerCase() === "e") {
        event.preventDefault();
        setExportOpen(true);
      }
    };
    window.addEventListener("keydown", onKeyDown);
    return () => window.removeEventListener("keydown", onKeyDown);
  }, [commit, handleHistory, projectCommand, selection, selectedClip, snapshot, updateSelection]);

  const statusLabel = connection === "connected" ? "Saved locally" : "Native bridge unavailable";
  const duration = timeline?.durationFrames ?? 0;

  return (
    <div className="editor-shell" style={{ ["--chat-width" as string]: `${panelWidth}px`, ["--timeline-height" as string]: `${timelineHeight}px` }}>
      <header className="editor-topbar">
        <div className="topbar-leading">
          <Button variant="ghost" size="icon" aria-label="Toggle media pane" onClick={() => setLeftOpen((open) => !open)}>
            {leftOpen ? <PanelLeftClose aria-hidden="true" /> : <PanelLeftOpen aria-hidden="true" />}
          </Button>
          <div className="brand-lockup">
            <span className="brand-mark"><Film aria-hidden="true" /></span>
            <span className="brand-name">Cutterhoochee</span>
          </div>
          <span className="topbar-divider" aria-hidden="true" />
          <button className="project-name-button" type="button" onClick={() => void onOpenProject().catch((error) => setNotice(errorMessage(error)))} title="Open another project">
            {snapshot.document.name}
            <ChevronDown aria-hidden="true" />
          </button>
          <span className="save-state"><span className="save-dot" />{statusLabel}</span>
        </div>
        <div className="topbar-actions">
          <Button variant="ghost" size="icon" aria-label="Undo" onClick={() => void handleHistory("undo")}><Undo2 aria-hidden="true" /></Button>
          <Button variant="ghost" size="icon" aria-label="Redo" onClick={() => void handleHistory("redo")}><Redo2 aria-hidden="true" /></Button>
          <Button variant="primary" size="sm" aria-label="Export video" title="Export video (Ctrl/Cmd+E)" onClick={() => setExportOpen(true)}><Download aria-hidden="true" />Export</Button>
          <span className="topbar-divider" aria-hidden="true" />
          <Button variant="ghost" size="icon" aria-label="Toggle theme" onClick={() => onThemeChange(theme === "dark" ? "light" : "dark")}>
            {theme === "dark" ? <Sun aria-hidden="true" /> : <Moon aria-hidden="true" />}
          </Button>
          <Button variant="ghost" size="icon" aria-label="Provider settings" onClick={() => setProviderSettingsOpen(true)}><Settings2 aria-hidden="true" /></Button>
          <Button variant="ghost" size="icon" aria-label="Close project" onClick={() => void projectCommand({ method: "project_close", params: {} }).then(onClose).catch(() => undefined)}><X aria-hidden="true" /></Button>
        </div>
      </header>

      <div className={`editor-body${leftOpen ? "" : " left-collapsed"}${rightOpen ? "" : " right-collapsed"}`}>
        {leftOpen ? (
          <aside className="left-pane" aria-label="Project tools">
            <div className="pane-tabs" role="tablist" aria-label="Project tools">
              <button className={leftTab === "media" ? "pane-tab active" : "pane-tab"} role="tab" aria-selected={leftTab === "media"} type="button" onClick={() => setLeftTab("media")}><FolderOpen aria-hidden="true" />Media</button>
              <button className={leftTab === "transcript" ? "pane-tab active" : "pane-tab"} role="tab" aria-selected={leftTab === "transcript"} type="button" onClick={() => setLeftTab("transcript")}><History aria-hidden="true" />Transcript</button>
              <button className={leftTab === "inspector" ? "pane-tab active" : "pane-tab"} role="tab" aria-selected={leftTab === "inspector"} type="button" onClick={() => setLeftTab("inspector")}><SlidersHorizontal aria-hidden="true" />Inspector</button>
            </div>
            <div className="pane-content">
              {leftTab === "media" ? <MediaLibrary client={client} snapshot={snapshot} onRefresh={refresh} onNotice={setNotice} onImport={() => void projectCommand({ method: "media", params: { action: "import" } }).catch(() => undefined)} onInsert={(assetId) => appendAsset(assetId).catch((error) => setNotice(errorMessage(error)))} /> : null}
              {leftTab === "transcript" ? <TranscriptPanel client={client} snapshot={snapshot} selection={selection} onEdit={commit} onRefresh={refresh} onNotice={setNotice} onSeek={(frame) => void updateSelection({ ...selection, playheadFrame: frame })} /> : null}
              {leftTab === "inspector" ? <ClipInspector client={client} snapshot={snapshot} clip={selectedClip} track={activeTrack} transitions={selectedTransitions} selection={selection} onEdit={commit} onNotice={setNotice} /> : null}
            </div>
          </aside>
        ) : null}

        <main className="editor-main">
          <div className="preview-toolbar">
            <div className="preview-breadcrumb"><span>Preview</span><span className="toolbar-separator">/</span><span className="muted">{snapshot.document.profile.width} × {snapshot.document.profile.height}</span></div>
            <div className="preview-meta"><span className="quality-pill">{transportPlaying ? "Playing" : "Ready"}</span><span>{formatTimecode(transportFrame, snapshot.document.profile.fpsNum, snapshot.document.profile.fpsDen)} / {formatDuration(duration, snapshot.document.profile.fpsNum, snapshot.document.profile.fpsDen)}</span></div>
          </div>
          <div className="preview-stage"><Preview client={client} snapshot={snapshot} selection={selection} playing={transportPlaying} onPlayingChange={handlePlayState} onFrameChange={setTransportFrame} onSelectionChange={updateSelection} onNotice={setNotice} /></div>
          <div className={isDraggingTimeline ? "resize-handle horizontal dragging" : "resize-handle horizontal"} role="separator" aria-label="Resize timeline" aria-orientation="horizontal" tabIndex={0} onPointerDown={(event) => { event.currentTarget.setPointerCapture(event.pointerId); timelineResizeStart.current = { y: event.clientY, height: timelineHeight }; setIsDraggingTimeline(true); }} onKeyDown={(event) => { if (event.key === "ArrowUp") setTimelineHeight((height) => Math.min(520, height + 16)); if (event.key === "ArrowDown") setTimelineHeight((height) => Math.max(180, height - 16)); }} />
          <section className="timeline-dock" aria-label="Timeline"><Timeline client={client} snapshot={snapshot} timeline={timeline} selection={{ ...selection, playheadFrame: transportFrame }} onSelectionChange={updateSelection} onEdit={commit} onNotice={setNotice} /></section>
        </main>

        {rightOpen ? (
          <aside className="chat-pane" aria-label="Assistant chat">
            <div className={isDraggingChat ? "resize-handle vertical dragging" : "resize-handle vertical"} role="separator" aria-label="Resize assistant" aria-orientation="vertical" tabIndex={0} onPointerDown={(event) => { event.currentTarget.setPointerCapture(event.pointerId); chatResizeStart.current = { x: event.clientX, width: panelWidth }; setIsDraggingChat(true); }} onKeyDown={(event) => { if (event.key === "ArrowLeft") setPanelWidth((width) => Math.min(520, width + 16)); if (event.key === "ArrowRight") setPanelWidth((width) => Math.max(280, width - 16)); }} />
            <ChatPanel client={client} snapshot={snapshot} eventLog={eventLog} onRefresh={refresh} onNotice={setNotice} onProviderSettings={() => setProviderSettingsOpen(true)} />
          </aside>
        ) : <button className="collapsed-pane-button right" type="button" onClick={() => setRightOpen(true)} aria-label="Open assistant"><PanelRightOpen aria-hidden="true" /></button>}
      </div>
      {notice ? <div className="notice-toast" role="status"><span>{notice}</span><button type="button" aria-label="Dismiss notice" onClick={() => setNotice(null)}><X aria-hidden="true" /></button></div> : null}
      <Dialog open={permission !== null} onOpenChange={(open) => { if (!open && permission) void answerPermission(false); }}>
        <DialogContent className="approval-dialog">
          <DialogHeader><DialogTitle>Permission required</DialogTitle><DialogDescription>Review the exact native operation before it can begin. Dismissing this request denies it.</DialogDescription></DialogHeader>
          {permission ? <div className="approval-details">
            <div className="approval-row"><span>Operation</span><strong>{permission.details.operation || "External operation"}</strong></div>
            {permission.details.operation === "system_execute" ? <p role="note">This command runs with your user account's filesystem and network access. The working directory is <strong>not a sandbox</strong>. Completed effects are not reversed by timeline Undo.</p> : null}
            {permission.details.canonicalExecutable ? <div className="approval-row"><span>Executable</span><code>{permission.details.canonicalExecutable}</code></div> : null}
            {permission.details.arguments?.length ? <div className="approval-row"><span>Arguments</span><code>{JSON.stringify(permission.details.arguments)}</code></div> : null}
            {permission.details.cwd ? <div className="approval-row"><span>Working directory</span><code>{permission.details.cwd}</code></div> : null}
            {permission.details.path ? <div className="approval-row"><span>Path</span><code>{permission.details.path}</code></div> : null}
            {permission.details.paths?.length ? <div className="approval-row"><span>Paths</span><code>{permission.details.paths.join("\n")}</code></div> : null}
            {permission.details.targetIdentity?.canonicalPath ? <div className="approval-row"><span>Target identity</span><code>{permission.details.targetIdentity.canonicalPath} · {permission.details.targetIdentity.size ?? 0} bytes</code></div> : null}
            {permission.details.url ? <div className="approval-row"><span>Destination</span><code>{permission.details.method ? `${permission.details.method} ` : ""}{permission.details.url}</code></div> : null}
            {permission.details.bodySha256 ? <div className="approval-row"><span>Body</span><code>{permission.details.bodyBytes ?? 0} bytes · SHA-256 {permission.details.bodySha256}</code></div> : null}
            {permission.details.offset !== undefined || permission.details.length !== undefined ? <div className="approval-row"><span>Read range</span><code>{permission.details.offset ?? 0}–{(permission.details.offset ?? 0) + (permission.details.length ?? 0)}</code></div> : null}
            {permission.details.timeoutMs !== undefined ? <div className="approval-row"><span>Timeout</span><span>{Math.round(permission.details.timeoutMs / 1000)} seconds</span></div> : null}
            {permission.details.overwrite !== undefined ? <div className="approval-row"><span>Overwrite existing target</span><strong>{permission.details.overwrite ? "Yes" : "No"}</strong></div> : null}
            {permission.scope ? <div className="approval-row"><span>Authority scope</span><code>{permission.scope.workspaceId || "workspace"}{permission.scope.projectId ? ` · project ${permission.scope.projectId}` : ""}{permission.scope.generation !== undefined ? ` · generation ${permission.scope.generation}` : ""}</code></div> : null}
            {permission.runId ? <div className="approval-row"><span>Assistant run</span><code>{permission.runId}</code></div> : null}
            {permission.expiresAtMs ? <div className="approval-row"><span>Expires</span><span>{new Date(permission.expiresAtMs).toLocaleTimeString()}</span></div> : null}
          </div> : null}
          <div className="dialog-actions"><Button variant="ghost" onClick={() => void answerPermission(false)}>Deny</Button><Button onClick={() => void answerPermission(true)}>Allow once</Button></div>
        </DialogContent>
      </Dialog>
      <Dialog open={providerSettingsOpen} onOpenChange={setProviderSettingsOpen}><DialogContent className="wide-dialog"><ProviderSettings client={client} snapshot={snapshot} onNotice={setNotice} onClose={() => setProviderSettingsOpen(false)} /></DialogContent></Dialog>
      <Dialog open={exportOpen} onOpenChange={setExportOpen}><DialogContent className="wide-dialog"><ExportDialog client={client} snapshot={snapshot} onNotice={setNotice} onClose={() => setExportOpen(false)} /></DialogContent></Dialog>
    </div>
  );
}

export function StartScreen({
  client,
  status,
  connection,
  theme,
  onThemeChange,
  onProjectReady,
}: {
  client: EditorClient;
  status: ProjectStatus;
  connection: "connected" | "unavailable";
  theme: Theme;
  onThemeChange: (theme: Theme) => void;
  onProjectReady: (status: ProjectStatus) => Promise<void>;
}) {
  const [name, setName] = useState("Untitled project");
  const [aspect, setAspect] = useState<"16:9" | "9:16" | "1:1">("16:9");
  const [fpsValue, setFpsValue] = useState("30");
  const [advanced, setAdvanced] = useState(false);
  const [busy, setBusy] = useState<"create" | "open" | "import" | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [recentProjects, setRecentProjects] = useState<string[]>([]);
  useEffect(() => {
    try {
      const raw = window.localStorage.getItem("cutterhoochee.recent-projects");
      const parsed: unknown = raw ? JSON.parse(raw) : [];
      if (Array.isArray(parsed)) setRecentProjects(parsed.filter((value): value is string => typeof value === "string").slice(0, 5));
    } catch {
      setRecentProjects([]);
    }
  }, []);

  const run = async (action: "create" | "open" | "import", paths?: string[]) => {
    setBusy(action);
    setError(null);
    try {
      if (action === "create") {
        await callNative(client, { method: "project_create", params: { name: name.trim() || "Untitled project", aspect, fpsNum: Number(fpsValue), fpsDen: 1 } });
      } else if (action === "open") {
        await callNative(client, { method: "project_open", params: {} });
      } else {
        if (!status.open) {
          await callNative(client, { method: "project_create", params: { name: name.trim() || "Untitled project", aspect, fpsNum: Number(fpsValue), fpsDen: 1 } });
        }
        const validPaths = paths?.filter((path) => path.trim().length > 0);
        await callNative(client, { method: "media", params: validPaths && validPaths.length > 0 ? { action: "import", paths: validPaths } : { action: "import" } });
      }
      const nextStatus = await client.projectStatus();
      if (nextStatus.name) {
        const next = [nextStatus.name, ...recentProjects.filter((projectName) => projectName !== nextStatus.name)].slice(0, 5);
        setRecentProjects(next);
        try { window.localStorage.setItem("cutterhoochee.recent-projects", JSON.stringify(next)); } catch { /* Recent names are a convenience only. */ }
      }
      await onProjectReady(nextStatus);
    } catch (caught) {
      setError(errorMessage(caught));
    } finally {
      setBusy(null);
    }
  };

  return (
    <div className="start-shell">
      <header className="start-topbar"><div className="brand-lockup"><span className="brand-mark"><Film aria-hidden="true" /></span><span className="brand-name">Cutterhoochee</span></div><div className="topbar-actions"><span className="connection-chip"><span className={connection === "connected" ? "status-dot" : "status-dot muted"} />{connection === "connected" ? "Desktop ready" : "Browser preview · native required"}</span><Button variant="ghost" size="icon" aria-label="Toggle theme" onClick={() => onThemeChange(theme === "dark" ? "light" : "dark")}>{theme === "dark" ? <Sun aria-hidden="true" /> : <Moon aria-hidden="true" />}</Button></div></header>
      <main className="start-content"><div className="start-intro"><p className="eyebrow">Local-first video editing</p><h1>Make something worth watching.</h1><p>Shape footage, sound, captions, and ideas in one calm timeline. Your project stays on this device until you explicitly share evidence.</p></div>
        <div className="start-grid"><section className="start-card primary"><div className="start-card-icon"><PlusIcon /></div><h2>New project</h2><p>Start with a clean timeline and choose the format that fits your story.</p><label className="field-label" htmlFor="project-name">Project name</label><input id="project-name" value={name} onChange={(event) => setName(event.target.value)} placeholder="Untitled project" /><div className="field-row"><label className="field-group"><span className="field-label">Aspect</span><select value={aspect} onChange={(event) => setAspect(event.target.value as typeof aspect)}><option value="16:9">Landscape · 16:9</option><option value="9:16">Portrait · 9:16</option><option value="1:1">Square · 1:1</option></select></label><label className="field-group"><span className="field-label">Frame rate</span><select value={fpsValue} onChange={(event) => setFpsValue(event.target.value)}><option value="30">30 fps</option>{advanced ? <><option value="24">24 fps</option><option value="25">25 fps</option><option value="60">60 fps</option></> : null}</select></label></div><button className="advanced-toggle" type="button" onClick={() => setAdvanced((open) => !open)}><ChevronRight className={advanced ? "rotate-90" : ""} aria-hidden="true" />Advanced format options</button><Button size="lg" className="start-action" disabled={busy !== null} onClick={() => void run("create")}><PlusIcon />{busy === "create" ? "Creating…" : "Create project"}</Button></section>
          <section className="start-card"><div className="start-card-icon muted"><FolderOpen aria-hidden="true" /></div><h2>Continue editing</h2><p>Open a saved <code>.cutproj</code> directory. Native dialogs keep file access explicit.</p><div className="stack-actions"><Button variant="secondary" disabled={busy !== null} onClick={() => void run("open")}><FolderOpen aria-hidden="true" />{busy === "open" ? "Opening…" : "Open project"}</Button><Button variant="ghost" disabled={busy !== null} onClick={() => void run("import")}><Download aria-hidden="true" />Import media</Button></div>{status.open ? <p className="small-note">A project is already open; refresh the desktop window to reconnect it.</p> : <p className="small-note">Recent projects appear here after the first native save.</p>}</section>
            {recentProjects.length > 0 ? <div className="recent-projects"><span className="field-label">Recent projects</span>{recentProjects.map((projectName) => <button type="button" key={projectName} onClick={() => void run("open")}><span className="recent-project-icon"><FolderOpen aria-hidden="true" /></span><span>{projectName}</span><ChevronRight aria-hidden="true" /></button>)}</div> : null}
        </div>
        <div className="drop-target" onDragOver={(event) => event.preventDefault()} onDrop={(event) => { event.preventDefault(); const paths = Array.from(event.dataTransfer.files).map((file) => { if ("path" in file && typeof file.path === "string") return file.path; return ""; }).filter((path) => path.length > 0); void run("import", paths); }}><Download aria-hidden="true" /><div><strong>Drop media to import</strong><span>Video, audio, PNG, JPEG, or WebP · native grant required</span></div><ChevronRight aria-hidden="true" /></div>
        {error ? <div className="start-error" role="alert"><CircleHelp aria-hidden="true" /><span>{error}</span></div> : null}
      </main><footer className="start-footer"><span>Offline by default · no telemetry</span><span>Ctrl/Cmd+I Import&nbsp;&nbsp;Ctrl/Cmd+S Save&nbsp;&nbsp;Ctrl/Cmd+E Export</span></footer>
    </div>
  );
}

function PlusIcon() {
  return <span className="plus-glyph" aria-hidden="true">+</span>;
}
