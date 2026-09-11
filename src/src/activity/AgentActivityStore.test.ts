import { describe, expect, it } from "vitest";

import type { AgentActivity } from "@cutterhoochee/shared";
import { AgentActivityStore, isAgentActivity } from "./AgentActivityStore";

function activity(overrides: Partial<AgentActivity> = {}): AgentActivity {
  return {
    id: "activity-1",
    sequence: 1,
    origin: "agent",
    projectId: "project-1",
    generation: 4,
    tool: "edit_project",
    action: "remove_range",
    label: "Remove range",
    phase: "running",
    changed: false,
    dryRun: false,
    targets: [],
    totalTargets: 0,
    changes: [],
    jobIds: [],
    ...overrides,
  } as AgentActivity;
}

describe("AgentActivityStore", () => {
  it("rejects malformed nested targets before they reach presentation", () => {
    expect(isAgentActivity({ ...activity(), targets: [null] })).toBe(false);
    expect(isAgentActivity({ ...activity(), changes: [{ target: { kind: "clip", id: "clip" }, fields: [null] }] })).toBe(false);
  });

  it("keeps stable snapshots for duplicate and older sequence updates", () => {
    const store = new AgentActivityStore({ projectId: "project-1", generation: 4 });
    store.ingest(activity());
    const first = store.getSnapshot();
    store.ingest(activity({ sequence: 0, phase: "failed" }));
    store.ingest(activity({ sequence: 1, phase: "failed" }));
    expect(store.getSnapshot()).toBe(first);
    expect(store.getSnapshot()[0]?.phase).toBe("running");
  });

  it("rejects activities from another project or generation", () => {
    const store = new AgentActivityStore({ projectId: "project-1", generation: 4 });
    store.ingest(activity({ projectId: "project-2" }));
    store.ingest(activity({ generation: 5 }));
    expect(store.getSnapshot()).toHaveLength(0);
  });

  it("merges bounded snapshots without deleting retained terminal receipts", () => {
    const store = new AgentActivityStore({ projectId: "project-1", generation: 4 });
    store.ingest(activity({ sequence: 2, phase: "failed", error: { code: "IO_ERROR", message: "worker failed" } }));
    store.reconcile([activity({ sequence: 3, phase: "completed", changed: true })]);
    expect(store.getSnapshot().map((item) => item.id)).toEqual(["activity-1"]);
    expect(store.getSnapshot()[0]?.phase).toBe("completed");
    expect(store.isHydrated("activity-1")).toBe(false);
  });

  it("does not turn a dry-run receipt into a committed change", () => {
    const store = new AgentActivityStore({ projectId: "project-1", generation: 4 });
    store.ingest(activity({ phase: "completed", changed: false, dryRun: true }));
    expect(store.getSnapshot()[0]?.changed).toBe(false);
    expect(store.getSnapshot()[0]?.dryRun).toBe(true);
  });

  it("retains cancellation and job error terminal states verbatim", () => {
    const store = new AgentActivityStore({ projectId: "project-1", generation: 4 });
    store.ingest(activity({ phase: "cancelled", sequence: 5, jobIds: ["job-1"] }));
    store.ingest(activity({ phase: "failed", sequence: 4, error: { code: "JOB_CANCELLED", message: "cancelled" }, jobIds: ["job-1"] }));
    expect(store.getSnapshot()[0]?.phase).toBe("cancelled");
    expect(store.getSnapshot()[0]?.jobIds).toEqual(["job-1"]);
  });

  it("hydrates old terminal receipts without suppressing newly observed work", () => {
    const store = new AgentActivityStore({ projectId: "project-1", generation: 4 });
    store.reconcile([activity({ phase: "completed" })]);
    expect(store.isHydrated("activity-1")).toBe(true);
    store.reconcile([activity({ id: "next", sequence: 2, phase: "completed" })]);
    expect(store.isHydrated("next")).toBe(false);
  });

  it("bounds retained history and rejects evicted stale receipts", () => {
    const store = new AgentActivityStore({ projectId: "project-1", generation: 4 });
    for (let sequence = 1; sequence <= 256; sequence++) store.ingest(activity({ id: `entry-${sequence}`, sequence }));
    expect(store.getSnapshot()).toHaveLength(128);
    const snapshot = store.getSnapshot();
    store.ingest(activity({ id: "entry-1", sequence: 1 }));
    expect(store.getSnapshot()).toBe(snapshot);
    store.ingest(activity({ id: "entry-1", sequence: 257, phase: "completed" }));
    expect(store.getSnapshot()[0]?.id).toBe("entry-1");
    expect(store.getSnapshot()).toHaveLength(128);
  });
});
