import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { AlertCircle, Check, ChevronRight, CircleStop, Clock3, Eye, Loader2, ShieldAlert, UserRound, Wrench } from "lucide-react";
import { listen } from "@tauri-apps/api/event";

import type { EditorClient } from "@cutterhoochee/shared";
import { Button } from "@/components/ui/button";
import {
  AgentActivityStore,
  isAgentActivity,
  isTerminalActivity,
  type ActivityReveal,
  useAgentActivities,
} from "@/activity/AgentActivityStore";
import {
  callNative,
  eventData,
  parseEvent,
  readAgentActivitySnapshot,
  record,
} from "@/lib/native";

const PHASE_LABEL: Record<string, string> = {
  queued: "Queued",
  running: "Running",
  awaiting_approval: "Waiting for approval",
  cancelling: "Cancelling",
  completed: "Completed",
  cancelled: "Cancelled",
  failed: "Failed",
};

const TARGET_LABEL: Record<string, string> = {
  project: "project",
  asset: "asset",
  track: "track",
  clip: "clip",
  text: "text",
  transition: "transition",
};

function activityText(activity: { label: string; action: string; tool: string }): string {
  return activity.label || activity.action || activity.tool;
}
function activityKindLabel(activity: { changed: boolean; dryRun: boolean; tool: string; action: string; jobIds: readonly string[] }): string {
  if (activity.dryRun) return "Dry run · no changes committed";
  if (activity.changed) return "Committed change";
  if (activity.tool.startsWith("system_") || activity.action.startsWith("system_")) return "Approval-gated system operation";
  if (activity.jobIds.length > 0) return "Native job";
  if (["status", "snapshot", "inspect", "render_frame", "inspect_frames", "list", "get", "search"].includes(activity.action) || activity.tool === "sample_frames") return "Inspection";
  return "Native operation";
}

function progressValue(value: number | undefined): number | null {
  if (value === undefined || !Number.isFinite(value)) return null;
  return Math.max(0, Math.min(1, value));
}

function activityError(activity: { error?: { message: string } }): string | null {
  return activity.error?.message || null;
}

export type ActivityPanelProps = {
  client: EditorClient;
  store: AgentActivityStore;
  onReveal: (reveal: ActivityReveal) => void;
};

export function ActivityPanel({ client, store, onReveal }: ActivityPanelProps) {
  const activities = useAgentActivities(store);
  const [canceling, setCanceling] = useState<ReadonlySet<string>>(() => new Set());
  const [cancelNotes, setCancelNotes] = useState<Record<string, string>>({});
  const [historyOpen, setHistoryOpen] = useState(false);
  const revealSequence = useRef(0);
  const context = client.getContext();
  const currentScope = `${context.generation}:${context.projectId ?? ""}`;

  const foregroundActivities = useMemo(
    () => activities.filter((activity) => !isTerminalActivity(activity) || activity.phase === "failed"),
    [activities],
  );
  const historyActivities = useMemo(
    () => activities.filter((activity) => isTerminalActivity(activity) && activity.phase !== "failed"),
    [activities],
  );
  const hasActive = foregroundActivities.some((activity) => !isTerminalActivity(activity));

  useEffect(() => {
    let disposed = false;
    let unlisten: (() => void) | undefined;
    const scope = client.getContext();

    const consume = (value: unknown) => {
      const event = parseEvent(value);
      if (!event || event.kind.toLowerCase() !== "agent_activity") return;
      if (event.generation !== scope.generation || event.projectId !== scope.projectId) return;
      const data = record(eventData(event));
      const candidate = data.activity ?? data;
      if (isAgentActivity(candidate)) store.ingest(candidate);
    };

    const poll = async () => {
      try {
        const snapshot = await readAgentActivitySnapshot(scope.generation, scope.projectId);
        if (!disposed) store.reconcile(snapshot);
      } catch {
        // Event receipts remain visible when a transient snapshot poll fails.
      }
    };

    void poll();
    if (typeof window !== "undefined" && "__TAURI_INTERNALS__" in window) {
      void listen<unknown>("cutterhoochee://event", ({ payload }) => {
        if (!disposed) consume(payload);
      }).then((dispose) => {
        if (disposed) dispose();
        else unlisten = dispose;
      }).catch(() => undefined);
    }
    const timer = window.setInterval(() => void poll(), hasActive ? 750 : 2500);
    return () => {
      disposed = true;
      window.clearInterval(timer);
      unlisten?.();
    };
  }, [client, currentScope, hasActive, store]);

  const cancelJob = useCallback(async (activityId: string, jobId: string) => {
    setCancelNotes((current) => {
      const next = { ...current };
      delete next[activityId];
      return next;
    });
    setCanceling((current) => new Set(current).add(jobId));
    try {
      await callNative(client, { method: "jobs", params: { action: "cancel", jobId } });
      setCancelNotes((current) => ({ ...current, [activityId]: "Cancel requested; waiting for native confirmation." }));
    } catch (error) {
      setCancelNotes((current) => ({ ...current, [activityId]: error instanceof Error ? error.message : "Cancel request failed." }));
    } finally {
      setCanceling((current) => {
        const next = new Set(current);
        next.delete(jobId);
        return next;
      });
    }
  }, [client]);

  const renderActivity = (activity: (typeof activities)[number]) => {
    const progress = progressValue(activity.progress);
    const error = activityError(activity);
    const canCancel = !isTerminalActivity(activity) && activity.jobIds.length > 0;
    const shownTargetCount = Math.min(activity.targets.length, activity.totalTargets);
    return <article className={`activity-entry activity-entry-${activity.phase}`} key={activity.id}>
      <div className="activity-entry-heading">
        <span className="activity-entry-kind">{activity.origin === "agent" ? <Wrench aria-hidden="true" /> : <UserRound aria-hidden="true" />}{activity.origin === "agent" ? "Agent" : "You"}</span>
        <span className={`activity-phase activity-phase-${activity.phase}`}>{activity.phase === "running" || activity.phase === "queued" || activity.phase === "cancelling" ? <Loader2 className="spin" aria-hidden="true" /> : activity.phase === "failed" ? <AlertCircle aria-hidden="true" /> : activity.phase === "awaiting_approval" ? <ShieldAlert aria-hidden="true" /> : <Check aria-hidden="true" />}{PHASE_LABEL[activity.phase] ?? activity.phase}</span>
      </div>
      <strong className="activity-entry-label">{activityText(activity)}</strong>
      <div className="activity-entry-meta">
        <span>{activityKindLabel(activity)}</span>
        <span>{shownTargetCount < activity.totalTargets ? `${shownTargetCount} of ${activity.totalTargets} targets` : `${activity.totalTargets} ${activity.totalTargets === 1 ? "target" : "targets"}`}</span>
      </div>
      {activity.phase === "awaiting_approval" ? <p className="activity-entry-note">Approval is required before this operation can run.</p> : null}
      {activity.message ? <p className="activity-entry-note">{activity.message}</p> : null}
      {error ? <p className="activity-entry-error" role="alert"><AlertCircle aria-hidden="true" />{error}</p> : null}
      {progress !== null && !isTerminalActivity(activity) ? <div className="activity-progress" aria-label={`${Math.round(progress * 100)} percent complete`}><div className="progress-track"><span style={{ width: `${Math.round(progress * 100)}%` }} /></div><span>{Math.round(progress * 100)}%</span></div> : null}
      {activity.jobIds.length > 0 ? <div className="activity-jobs"><span>{activity.jobIds.length === 1 ? "1 native job" : `${activity.jobIds.length} native jobs`}</span>{canCancel ? activity.jobIds.map((jobId: string) => <Button key={jobId} variant="ghost" size="sm" disabled={canceling.has(jobId)} onClick={() => void cancelJob(activity.id, jobId)}>{canceling.has(jobId) ? <Loader2 className="spin" aria-hidden="true" /> : <CircleStop aria-hidden="true" />}Cancel</Button>) : null}</div> : null}
      {cancelNotes[activity.id] ? <p className="activity-entry-note">{cancelNotes[activity.id]}</p> : null}
      <div className="activity-entry-footer">
        <span className="activity-target-summary">{shownTargetCount > 0 ? `${TARGET_LABEL[activity.targets[0]?.kind] ?? "item"}${shownTargetCount > 1 ? ` +${shownTargetCount - 1}` : ""}` : "No highlighted target"}</span>
        {activity.targets.length > 0 ? <Button className="activity-reveal" variant="secondary" size="sm" onClick={() => onReveal({ requestId: ++revealSequence.current, activityId: activity.id, targets: activity.targets })}><Eye aria-hidden="true" />Show</Button> : null}
      </div>
    </article>;
  };

  return <section className="activity-panel" aria-label="Activity">
    <div className="activity-panel-header">
      <div>
        <p className="eyebrow">Activity</p>
        <h2>Native work</h2>
      </div>
      {hasActive ? <span className="activity-live"><Loader2 className="spin" aria-hidden="true" />Live</span> : null}
    </div>
    <p className="sr-only" role="status">{activities[0] ? `${activities[0].label}: ${PHASE_LABEL[activities[0].phase]}. ${activities[0].totalTargets} targets.` : ""}</p>
    {activities.length === 0 ? <div className="activity-empty"><Clock3 aria-hidden="true" /><span>Agent and media work will appear here with its real status.</span></div> : <>
      {foregroundActivities.length > 0 ? <div className="activity-list">{foregroundActivities.map(renderActivity)}</div> : null}
      {historyActivities.length > 0 ? <div className="activity-history">
        <button
          className="activity-history-toggle"
          type="button"
          aria-expanded={historyOpen}
          aria-controls="activity-history-list"
          onClick={() => setHistoryOpen((open) => !open)}
        >
          <ChevronRight className={historyOpen ? "activity-history-chevron activity-history-chevron--open" : "activity-history-chevron"} aria-hidden="true" />
          <span>{historyOpen ? "Hide history" : "Show history"}</span>
          <span className="activity-history-count">({historyActivities.length})</span>
        </button>
        <div className="activity-list activity-history-list" id="activity-history-list" hidden={!historyOpen}>{historyActivities.map(renderActivity)}</div>
      </div> : null}
    </>}
  </section>;
}
