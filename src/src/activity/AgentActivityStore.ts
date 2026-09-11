import { useSyncExternalStore } from "react";

import type { AgentActivity, ActivityTarget } from "@cutterhoochee/shared";

export type ActivityScope = { projectId: string | null; generation: number };

export type ActivityReveal = {
  requestId: number;
  activityId: string;
  targets: readonly ActivityTarget[];
};

const MAX_ACTIVITIES = 128;
const TERMINAL_PHASES = new Set<AgentActivity["phase"]>(["completed", "cancelled", "failed"]);

function validScope(scope: ActivityScope): boolean {
  return Number.isSafeInteger(scope.generation) && scope.generation >= 0 && (scope.projectId === null || typeof scope.projectId === "string");
}


function isSafeSequence(value: unknown): value is number {
  return typeof value === "number" && Number.isSafeInteger(value) && value >= 0;
}

function objectValue(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function validTarget(value: unknown): boolean {
  if (!objectValue(value) || !["project", "asset", "track", "clip", "text", "transition"].includes(String(value.kind))) return false;
  return typeof value.id === "string" && value.id.length > 0 && value.id.length <= 256
    && (value.trackId === undefined || typeof value.trackId === "string")
    && (value.space === undefined || value.space === "timeline" || value.space === "source")
    && (value.startFrame === undefined || isSafeSequence(value.startFrame))
    && (value.endFrame === undefined || isSafeSequence(value.endFrame));
}

function validGeometry(value: unknown): boolean {
  return value === undefined || (objectValue(value) && isSafeSequence(value.startFrame) && isSafeSequence(value.durationFrames)
    && (value.trackId === undefined || typeof value.trackId === "string"));
}

function validChange(value: unknown): boolean {
  return objectValue(value) && validTarget(value.target) && validGeometry(value.before) && validGeometry(value.after)
    && Array.isArray(value.fields) && value.fields.length <= 64
    && value.fields.every((field) => objectValue(field) && ["field", "before", "after"].every((key) => typeof field[key] === "string" && field[key].length <= 2048));
}

/**
 * Runtime guard for the native activity envelope. The native snapshot/event
 * path is authoritative, but malformed browser events must never poison the
 * stable external-store snapshot or render unrestricted payloads.
 */
export function isAgentActivity(value: unknown): value is AgentActivity {
  if (typeof value !== "object" || value === null || Array.isArray(value)) return false;
  const item = value as Record<string, unknown>;
  if (typeof item.id !== "string" || item.id.length === 0 || item.id.length > 256) return false;
  if (!isSafeSequence(item.sequence) || !isSafeSequence(item.generation)) return false;
  if (item.projectId !== null && typeof item.projectId !== "string") return false;
  if (item.origin !== "agent" && item.origin !== "user") return false;
  if (typeof item.runId !== "undefined" && typeof item.runId !== "string") return false;
  if (typeof item.toolCallId !== "undefined" && typeof item.toolCallId !== "string") return false;
  if (typeof item.tool !== "string" || typeof item.action !== "string" || typeof item.label !== "string") return false;
  if (item.phase !== "running" && item.phase !== "awaiting_approval" && item.phase !== "queued" && item.phase !== "cancelling" && item.phase !== "completed" && item.phase !== "cancelled" && item.phase !== "failed") return false;
  if (!isSafeSequence(item.totalTargets) || typeof item.changed !== "boolean" || typeof item.dryRun !== "boolean") return false;
  if (!Array.isArray(item.targets) || !Array.isArray(item.changes) || !Array.isArray(item.jobIds)) return false;
  if (item.targets.length > 64 || item.changes.length > 64 || item.jobIds.length > 64) return false;
  if (!item.targets.every(validTarget) || !item.changes.every(validChange)) return false;
  if (item.revision !== undefined && !isSafeSequence(item.revision)) return false;
  if (item.error !== undefined && (!objectValue(item.error) || typeof item.error.code !== "string" || typeof item.error.message !== "string")) return false;
  if (!item.jobIds.every((jobId) => typeof jobId === "string" && jobId.length > 0 && jobId.length <= 256)) return false;
  if (typeof item.progress !== "undefined" && (typeof item.progress !== "number" || !Number.isFinite(item.progress) || item.progress < 0 || item.progress > 1)) return false;
  if (typeof item.message !== "undefined" && typeof item.message !== "string") return false;
  return true;
}

function compareActivities(left: AgentActivity, right: AgentActivity): number {
  return right.sequence - left.sequence || right.id.localeCompare(left.id);
}

/**
 * Native-backed activity receipt store. It is deliberately independent from
 * React and keeps one immutable array identity between updates, so Timeline,
 * Inspector, and the activity area do not rerender on every parent render.
 */
export class AgentActivityStore {
  private scopeValue: ActivityScope;
  private readonly activities = new Map<string, AgentActivity>();
  private evictedSequence = -1;
  private reconciled = false;
  private readonly hydratedIds = new Set<string>();
  private listeners = new Set<() => void>();
  private snapshotValue: readonly AgentActivity[] = [];
  public constructor(scope: ActivityScope) {
    if (!validScope(scope)) throw new Error("Invalid activity scope.");
    this.scopeValue = { ...scope };
  }

  public get scope(): ActivityScope {
    return { ...this.scopeValue };
  }

  public subscribe = (listener: () => void): (() => void) => {
    this.listeners.add(listener);
    return () => this.listeners.delete(listener);
  };

  public getSnapshot = (): readonly AgentActivity[] => this.snapshotValue;

  public isHydrated(activityId: string): boolean {
    return this.hydratedIds.has(activityId);
  }

  public reset(scope: ActivityScope): void {
    if (!validScope(scope)) throw new Error("Invalid activity scope.");
    this.scopeValue = { ...scope };
    this.activities.clear();
    this.evictedSequence = -1;
    this.reconciled = false;
    this.hydratedIds.clear();
    this.publish();
  }

  public ingest(activity: AgentActivity): void {
    if (!this.accepts(activity)) return;
    const prior = this.activities.get(activity.id);
    if (prior && activity.sequence <= prior.sequence) return;
    if (!prior && activity.sequence <= this.evictedSequence) return;
    this.activities.set(activity.id, activity);
    this.hydratedIds.delete(activity.id);
    this.trim();
    this.publish();
  }

  public reconcile(activities: readonly AgentActivity[]): void {
    let changed = false;
    const hydrating = !this.reconciled;
    this.reconciled = true;
    for (const activity of activities) {
      if (!this.accepts(activity)) continue;
      const prior = this.activities.get(activity.id);
      if (prior && activity.sequence <= prior.sequence) continue;
      if (!prior && activity.sequence <= this.evictedSequence) continue;
      this.activities.set(activity.id, activity);
      if (hydrating && !prior && isTerminalActivity(activity)) this.hydratedIds.add(activity.id);
      else this.hydratedIds.delete(activity.id);
      changed = true;
    }
    if (!changed) return;
    this.trim();
    this.publish();
  }
  private accepts(activity: AgentActivity): boolean {
    return isAgentActivity(activity) && activity.generation === this.scopeValue.generation && activity.projectId === this.scopeValue.projectId;
  }

  private trim(): void {
    if (this.activities.size <= MAX_ACTIVITIES) return;
    const entries = [...this.activities.values()].sort(compareActivities);
    const keep = new Set(entries.slice(0, MAX_ACTIVITIES).map((activity) => activity.id));
    for (const id of this.activities.keys()) {
      if (!keep.has(id)) {
        this.evictedSequence = Math.max(this.evictedSequence, this.activities.get(id)!.sequence);
        this.activities.delete(id);
        this.hydratedIds.delete(id);
      }
    }
  }

  private publish(): void {
    this.snapshotValue = Object.freeze([...this.activities.values()].sort(compareActivities));
    for (const listener of this.listeners) listener();
  }
}

export function useAgentActivities(store: AgentActivityStore): readonly AgentActivity[] {
  return useSyncExternalStore(store.subscribe, store.getSnapshot, store.getSnapshot);
}

export function isTerminalActivity(activity: AgentActivity): boolean {
  return TERMINAL_PHASES.has(activity.phase);
}
