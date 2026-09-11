import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { Channel, invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { Loader2 } from "lucide-react";

import {
  EditorClient,
  createTauriTransport,
  decodeSoftwarePreviewPacket,
  type PreviewSoftwareContext,
  type ProjectSnapshot,
  type ProjectStatus,
  type SoftwarePreviewPacket,
  type SoftwarePreviewTransport,
  type TimelineSnapshot,
} from "@cutterhoochee/shared";
import { StartScreen, Workspace, type Theme } from "@/components/Workspace";
import { callNative, parseEvent } from "@/lib/native";
const SOFTWARE_PREVIEW_START_TIMEOUT_MS = 5_000;

type NativeInvoke = <T>(
  command: string,
  args?: Record<string, unknown>,
) => Promise<T>;

function softwareContextArgs(context: PreviewSoftwareContext): Record<string, unknown> {
  return {
    projectId: context.projectId,
    generation: context.generation,
    revision: context.revision,
    planHash: context.planHash,
  };
}

function sameSoftwareContext(
  left: PreviewSoftwareContext,
  right: PreviewSoftwareContext,
): boolean {
  return (
    left.projectId === right.projectId &&
    left.generation === right.generation &&
    left.revision === right.revision &&
    left.planHash === right.planHash
  );
}

function ignoredNativeCommand(
  nativeInvoke: NativeInvoke,
  command: string,
  args: Record<string, unknown>,
): Promise<void> {
  try {
    return Promise.resolve(nativeInvoke<void>(command, args)).then(
      () => undefined,
      () => undefined,
    );
  } catch {
    return Promise.resolve();
  }
}

class NativeSoftwareSubscription {
  readonly context: PreviewSoftwareContext;
  readonly channel: Channel<ArrayBuffer>;
  readonly ready: Promise<() => void>;
  readonly settled: Promise<void>;

  private readonly disposer = (): void => {
    this.cancel();
  };
  private readonly listener: (packet: SoftwarePreviewPacket) => void;
  private readonly nativeInvoke: NativeInvoke;
  private readonly startFrame: number;
  private readonly onSettled: (subscription: NativeSoftwareSubscription) => void;
  private readyTimer: ReturnType<typeof setTimeout> | undefined;
  private resolveReady!: (value: (() => void) | PromiseLike<() => void>) => void;
  private rejectReady!: (reason?: unknown) => void;
  private resolveSettled!: () => void;
  private readySettled = false;
  private readyObserved = false;
  private active = true;
  private cancellationRequested = false;
  private cancelIssued = false;
  private finished = false;
  private cancelPromise: Promise<void> = Promise.resolve();

  constructor(
    nativeInvoke: NativeInvoke,
    listener: (packet: SoftwarePreviewPacket) => void,
    context: PreviewSoftwareContext,
    startFrame: number,
    onSettled: (subscription: NativeSoftwareSubscription) => void,
    retainChannel: (channel: Channel<ArrayBuffer>) => void,
  ) {
    this.nativeInvoke = nativeInvoke;
    this.listener = listener;
    this.context = { ...context };
    this.startFrame = startFrame;
    this.onSettled = onSettled;
    this.ready = new Promise<() => void>((resolve, reject) => {
      this.resolveReady = resolve;
      this.rejectReady = reject;
    });
    this.settled = new Promise<void>((resolve) => {
      this.resolveSettled = resolve;
    });
    this.channel = new Channel<ArrayBuffer>((rawPacket) => {
      this.handlePacket(rawPacket);
    });
    retainChannel(this.channel);
    this.readyTimer = setTimeout(() => {
      if (this.finished || this.readyObserved || this.readySettled) return;
      this.failReady(new Error("The native software preview stream did not produce its first packet in time."));
      this.cancel();
    }, SOFTWARE_PREVIEW_START_TIMEOUT_MS);
  }

  start(after: Promise<void>): void {
    void after.then(() => {
      if (this.cancellationRequested) {
        this.finish();
        return;
      }
      let request: Promise<void>;
      try {
        request = this.nativeInvoke<void>("preview_software_subscribe", {
          channel: this.channel,
          ...softwareContextArgs(this.context),
          startFrame: this.startFrame,
        });
      } catch (error) {
        this.finish(error);
        return;
      }
      void request.then(
        () => this.finish(),
        (error) => this.finish(error),
      ).catch(() => undefined);
    }).catch((error) => {
      this.finish(error);
    });
  }

  waitForTermination(): Promise<void> {
    return Promise.all([this.settled, this.cancelPromise]).then(() => undefined);
  }

  cancel(): void {
    if (this.cancellationRequested) return;
    this.cancellationRequested = true;
    this.active = false;
    this.mute();
    if (!this.readyObserved && !this.readySettled) this.succeedReady();
    this.issueCancel();
  }

  private handlePacket(rawPacket: ArrayBuffer): void {
    if (!this.active) return;
    try {
      const packet = decodeSoftwarePreviewPacket(rawPacket);
      if (!this.readyObserved) {
        this.readyObserved = true;
        this.succeedReady();
      }
      this.listener(packet);
    } catch (error) {
      if (!this.readyObserved) this.failReady(error);
      this.cancel();
    }
  }

  private succeedReady(): void {
    if (this.readySettled) return;
    this.readySettled = true;
    this.clearReadyTimer();
    this.resolveReady(this.disposer);
  }

  private failReady(error: unknown): void {
    if (this.readySettled) return;
    this.readySettled = true;
    this.clearReadyTimer();
    this.rejectReady(error);
  }

  private clearReadyTimer(): void {
    if (this.readyTimer === undefined) return;
    clearTimeout(this.readyTimer);
    this.readyTimer = undefined;
  }

  private mute(): void {
    this.channel.onmessage = () => {};
  }

  private issueCancel(): Promise<void> {
    if (this.cancelIssued) return this.cancelPromise;
    this.cancelIssued = true;
    this.cancelPromise = ignoredNativeCommand(
      this.nativeInvoke,
      "preview_software_cancel",
      softwareContextArgs(this.context),
    );
    return this.cancelPromise;
  }

  private finish(error?: unknown): void {
    if (this.finished) return;
    this.finished = true;
    this.active = false;
    this.mute();
    if (!this.readyObserved && !this.readySettled) {
      if (this.cancellationRequested) this.succeedReady();
      else this.failReady(error ?? new Error("The native software preview stream ended before its first packet."));
    }
    this.issueCancel();
    try {
      this.onSettled(this);
    } finally {
      this.resolveSettled();
    }
  }
}

function createSoftwarePreviewTransport(nativeInvoke: NativeInvoke): SoftwarePreviewTransport {
  const pendingControls = new Set<Promise<unknown>>();
  const trackedInvoke: NativeInvoke = <T,>(command: string, args?: Record<string, unknown>): Promise<T> => {
    const request = nativeInvoke<T>(command, args);
    if (command === "preview_software_ack" || command === "preview_software_cancel") {
      pendingControls.add(request);
      void request.then(
        () => pendingControls.delete(request),
        () => pendingControls.delete(request),
      );
    }
    return request;
  };
  const retainedChannels = new Set<Channel<ArrayBuffer>>();
  const subscriptions = new Set<NativeSoftwareSubscription>();
  let current: NativeSoftwareSubscription | undefined;
  const onSettled = (subscription: NativeSoftwareSubscription): void => {
    subscriptions.delete(subscription);
    retainedChannels.delete(subscription.channel);
    if (current === subscription) current = undefined;
  };

  return {
    subscribePreviewSoftware(listener, context, startFrame = 0) {
      const previous = current;
      previous?.cancel();
      const subscription = new NativeSoftwareSubscription(
        trackedInvoke,
        listener,
        context,
        startFrame,
        onSettled,
        (channel) => {
          retainedChannels.add(channel);
        },
      );
      subscriptions.add(subscription);
      current = subscription;
      subscription.start(Promise.all([
        previous?.waitForTermination() ?? Promise.resolve(),
        ...Array.from(pendingControls, (request) => request.catch(() => undefined)),
      ]).then(() => undefined));
      return subscription.ready;
    },
    acknowledgePreviewSoftware(sequence, context) {
      return ignoredNativeCommand(trackedInvoke, "preview_software_ack", {
        ...softwareContextArgs(context),
        sequence,
      });
    },
    cancelPreviewSoftware(context) {
      const capturedContext = { ...context };
      let matched = false;
      for (const subscription of subscriptions) {
        if (!sameSoftwareContext(subscription.context, capturedContext)) continue;
        matched = true;
        subscription.cancel();
      }
      if (!matched) {
        void ignoredNativeCommand(
          trackedInvoke,
          "preview_software_cancel",
          softwareContextArgs(capturedContext),
        );
      }
    },
  };
}

const initialStatus: ProjectStatus = { generation: 0, open: false };

type ConnectionState = "checking" | "connected" | "unavailable";


export function App() {
  const client = useMemo(() => new EditorClient(
    createTauriTransport(invoke, {}, createSoftwarePreviewTransport(invoke)),
  ), []);
  const [status, setStatus] = useState<ProjectStatus>(initialStatus);
  const [snapshot, setSnapshot] = useState<ProjectSnapshot | null>(null);
  const [timeline, setTimeline] = useState<TimelineSnapshot | null>(null);
  const [connection, setConnection] = useState<ConnectionState>("checking");
  const [theme, setTheme] = useState<Theme>(() => readTheme());
  useEffect(() => {
    document.documentElement.dataset.theme = theme;
    try {
      window.localStorage.setItem("cutterhoochee.theme", theme);
    } catch {
      // The theme still applies when storage is unavailable.
    }
  }, [theme]);

  const [loadingProject, setLoadingProject] = useState(false);
  const loadSequence = useRef(0);
  const statusRef = useRef(status);
  const snapshotRef = useRef(snapshot);
  statusRef.current = status;
  snapshotRef.current = snapshot;

  const loadProject = useCallback(async (nextStatus: ProjectStatus) => {
    const sequence = ++loadSequence.current;
    const currentStatus = statusRef.current;
    const currentSnapshot = snapshotRef.current;
    const nextProjectId = nextStatus.open && typeof nextStatus.projectId === "string"
      ? nextStatus.projectId
      : null;
    client.adoptNativeScope({
      projectId: nextProjectId,
      generation: nextStatus.generation,
    });
    const currentContext = client.getContext();
    if (
      currentContext.generation !== nextStatus.generation ||
      currentContext.projectId !== nextProjectId
    ) {
      return;
    }
    const sameProject =
      nextProjectId !== null &&
      currentStatus.open &&
      currentStatus.projectId === nextProjectId &&
      currentSnapshot?.document.projectId === nextProjectId;
    if (!nextProjectId) {
      if (sequence !== loadSequence.current) return;
      setStatus(nextStatus);
      setSnapshot(null);
      setTimeline(null);
      setLoadingProject(false);
      return;
    }
    if (!sameProject) setLoadingProject(true);
    try {
      const [nextSnapshot, nextTimeline] = await Promise.all([
        client.projectSnapshot(),
        client.timelineSnapshot(),
      ]);
      if (sequence !== loadSequence.current) return;
      const loadedContext = client.getContext();
      if (
        loadedContext.generation !== nextStatus.generation ||
        loadedContext.projectId !== nextProjectId
      ) {
        return;
      }
      setStatus(nextStatus);
      setSnapshot(nextSnapshot);
      setTimeline(nextTimeline);
    } finally {
      if (sequence === loadSequence.current) setLoadingProject(false);
    }
  }, [client]);

  const refresh = useCallback(async () => {
    const nextStatus = await client.projectStatus();
    setConnection("connected");
    await loadProject(nextStatus);
  }, [client, loadProject]);
  useEffect(() => {
    if (typeof window === "undefined" || !("__TAURI_INTERNALS__" in window)) return;
    let disposed = false;
    let unlisten: (() => void) | undefined;
    void listen<unknown>("cutterhoochee://event", ({ payload }) => {
      if (disposed) return;
      const event = parseEvent(payload);
      if (
        !event ||
        event.kind.toLowerCase() !== "project_changed" ||
        !Object.prototype.hasOwnProperty.call(event, "projectId") ||
        event.generation === undefined ||
        !Number.isSafeInteger(event.generation) ||
        event.generation < 0
      ) {
        return;
      }
      const projectId = event.projectId ?? null;
      const generation = event.generation;
      const contextBefore = client.getContext();
      let adopted = false;
      try {
        adopted = client.adoptNativeScope({
          projectId,
          generation,
        });
      } catch {
        return;
      }
      const sameScope =
        contextBefore.generation === generation &&
        contextBefore.projectId === projectId;
      if (!adopted && !sameScope) return;
      const currentStatus = statusRef.current;
      const currentSnapshot = snapshotRef.current;
      const sameProject =
        projectId !== null &&
        currentStatus.open &&
        currentStatus.projectId === projectId &&
        currentSnapshot?.document.projectId === projectId;
      if (projectId === null) {
        ++loadSequence.current;
        setLoadingProject(false);
        setSnapshot(null);
        setTimeline(null);
        setStatus({ generation: event.generation, open: false });
        return;
      }
      if (!sameProject) {
        setSnapshot(null);
        setTimeline(null);
        setLoadingProject(true);
      }
      setStatus((current) => (
        current.open && current.projectId === projectId
          ? { ...current, generation }
          : { generation, open: true, projectId }
      ));
      void refresh().catch(() => undefined);
    }).then((dispose) => {
      if (disposed) dispose();
      else unlisten = dispose;
    }).catch(() => {
      // A browser tab has no native event bridge.
    });
    return () => {
      disposed = true;
      unlisten?.();
    };
  }, [client, refresh]);

  useEffect(() => {
    let mounted = true;
    void client.projectStatus().then(async (nextStatus) => {
      if (!mounted) return;
      setConnection("connected");
      await loadProject(nextStatus);
    }).catch(() => {
      if (mounted) setConnection("unavailable");
    });
    return () => { mounted = false; };
  }, [client, loadProject]);

  const createOrOpen = useCallback(async (nextStatus: ProjectStatus) => {
    await loadProject(nextStatus);
  }, [loadProject]);

  const openProject = useCallback(async () => {
    await callNative(client, { method: "project_open", params: {} });
    const nextStatus = await client.projectStatus();
    await loadProject(nextStatus);
  }, [client, loadProject]);

  const closeProject = useCallback(() => {
    ++loadSequence.current;
    setSnapshot(null);
    setTimeline(null);
    setLoadingProject(false);
    const context = client.getContext();
    client.retireGeneration(context.generation);
    setStatus({ generation: context.generation, open: false });
  }, [client]);

  if (loadingProject) return <div className="loading-shell"><Loader2 className="spin" aria-hidden="true" /><span>Opening project…</span></div>;
  const connectionBadge: "connected" | "unavailable" = connection === "connected" ? "connected" : "unavailable";
  if (status.open && snapshot) return <Workspace key={`${status.generation}:${status.projectId ?? ""}:${snapshot.workspaceId}`} client={client} status={status} snapshot={snapshot} timeline={timeline} connection={connectionBadge} theme={theme} onThemeChange={setTheme} onRefresh={refresh} onProjectStatus={setStatus} onProjectSnapshot={setSnapshot} onTimeline={setTimeline} onClose={closeProject} onOpenProject={openProject} />;
  return <StartScreen client={client} status={status} connection={connectionBadge} theme={theme} onThemeChange={setTheme} onProjectReady={createOrOpen} />;
}

function readTheme(): Theme {
  try {
    const stored = window.localStorage.getItem("cutterhoochee.theme");
    if (stored === "dark" || stored === "light") return stored;
  } catch { /* Use the OS preference when storage is unavailable. */ }
  return window.matchMedia?.("(prefers-color-scheme: light)").matches ? "light" : "dark";
}
