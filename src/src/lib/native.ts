import type {
  EditOp,
  EditorClient,
  EditorClientError,
  EditorReply,
  EditorReplyForRequest,
  EditorRequest,
  ProjectSnapshot,
  TimelineSelection,
} from "@cutterhoochee/shared";

/**
 * The shared client is the only UI/native boundary. This generic helper keeps
 * request and reply inference tied to the generated Rust union; it does not
 * widen, re-encode, or emulate leaf commands in the browser.
 */
export async function callNative<Request extends EditorRequest>(
  client: EditorClient,
  request: Request,
): Promise<EditorReplyForRequest<Request>> {
  return client.call(request);
}

export function replyData<T>(reply: { kind: string; data: unknown }, kind?: string): T {
  if (kind && reply.kind !== kind) {
    throw new Error(`Native reply kind ${reply.kind} does not match ${kind}.`);
  }
  return reply.data as T;
}

export function record(value: unknown): Record<string, unknown> {
  if (typeof value !== "object" || value === null || Array.isArray(value)) return {};
  return value as Record<string, unknown>;
}

export function replyPayload(reply: { data: unknown }): Record<string, unknown> {
  const outer = record(reply.data);
  const nested = record(outer.data);
  if (Object.keys(nested).length > 0) return nested;
  return outer;
}

export function eventData(event: EventPayload): Record<string, unknown> {
  const outer = record(event.data);
  const nested = record(outer.data);
  if (Object.keys(nested).length > 0) return nested;
  return outer;
}

export function stringValue(value: unknown, fallback = ""): string {
  if (typeof value === "string") return value;
  return fallback;
}

export function numberValue(value: unknown, fallback = 0): number {
  if (typeof value === "number" && Number.isFinite(value)) return value;
  return fallback;
}

export function booleanValue(value: unknown, fallback = false): boolean {
  if (typeof value === "boolean") return value;
  return fallback;
}

export function arrayValue<T = unknown>(value: unknown): T[] {
  if (Array.isArray(value)) return value as T[];
  return [];
}

export function formatTimecode(frame: number, fpsNum: number, fpsDen: number): string {
  const safeFps = fpsNum > 0 && fpsDen > 0 ? fpsNum / fpsDen : 30;
  const totalSeconds = Math.max(0, Math.floor(frame / safeFps));
  const hours = Math.floor(totalSeconds / 3600);
  const minutes = Math.floor((totalSeconds % 3600) / 60);
  const seconds = totalSeconds % 60;
  const frames = Math.max(0, Math.floor(frame - totalSeconds * safeFps));
  return `${hours.toString().padStart(2, "0")}:${minutes.toString().padStart(2, "0")}:${seconds
    .toString()
    .padStart(2, "0")}:${frames.toString().padStart(2, "0")}`;
}

export function formatDuration(frame: number, fpsNum: number, fpsDen: number): string {
  const safeFps = fpsNum > 0 && fpsDen > 0 ? fpsNum / fpsDen : 30;
  const seconds = Math.max(0, frame / safeFps);
  const minutes = Math.floor(seconds / 60);
  const remainder = seconds - minutes * 60;
  return `${minutes}:${remainder.toFixed(1).padStart(4, "0")}`;
}

export function findTransitionForClip(
  snapshot: ProjectSnapshot,
  clipId: string,
): ProjectSnapshot["document"]["transitions"] {
  return snapshot.document.transitions.filter(
    (transition) => transition.leftClipId === clipId || transition.rightClipId === clipId,
  );
}

export function transitionRemovalOps(snapshot: ProjectSnapshot, clipId: string): EditOp[] {
  return findTransitionForClip(snapshot, clipId).map((transition) => ({
    op: "remove_transition",
    transitionId: transition.id,
  }));
}

export function timelineEnd(snapshot: ProjectSnapshot): number {
  let end = 0;
  for (const clip of snapshot.document.clips) {
    end = Math.max(end, clip.startFrame + clip.durationFrames);
  }
  for (const item of snapshot.document.textItems) {
    if (item.startFrame !== undefined && item.durationFrames !== undefined) {
      end = Math.max(end, item.startFrame + item.durationFrames);
    }
  }
  return end;
}

export function fps(snapshot: ProjectSnapshot | undefined): number {
  if (!snapshot) return 30;
  const { fpsNum, fpsDen } = snapshot.document.profile;
  if (fpsNum > 0 && fpsDen > 0) return fpsNum / fpsDen;
  return 30;
}

export function isEditorClientError(error: unknown): error is EditorClientError {
  return error instanceof Error && "code" in error;
}

export type EventPayload = {
  kind: string;
  projectId?: string | null;
  generation?: number;
  runId?: string;
  data?: unknown;
};

export function parseEvent(value: unknown): EventPayload | null {
  const object = record(value);
  const envelopeKind = stringValue(object.kind);
  const kind = stringValue(object.event) || (envelopeKind !== "event" ? envelopeKind : "");
  if (!kind) return null;
  const projectId = object.projectId;
  return {
    kind,
    ...(typeof projectId === "string" || projectId === null ? { projectId } : {}),
    ...(typeof object.generation === "number" ? { generation: object.generation } : {}),
    ...(typeof object.runId === "string" ? { runId: object.runId } : {}),
    data: object.data,
  };
}

export type { EditOp, EditorReply, EditorRequest, ProjectSnapshot, TimelineSelection };
