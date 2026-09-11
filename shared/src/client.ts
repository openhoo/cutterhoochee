import {
  type EditorCallContext,
  type EditorErrorCode,
  type EditorReply,
  type EditorRequest,
  type EditorResponseEnvelope,
  isEditorErrorCode,
  isEditorReply,
  isEditorRequest,
  isEditorResponseEnvelope,
} from "./protocol.js";
import type {
  EditOp,
  EditResult,
  MediaClip,
  ProjectSnapshot,
  ProjectStatus,
  SoftwarePreviewPacket,
  TimelineSelection,
  TimelineSnapshot,
  TrackKind,
} from "./generated.js";

export type ReplyKindByMethod = {
  project_status: "project_status";
  project_create: "project_status";
  project_open: "project_status";
  project_save: "project_status";
  project_close: "project_status";
  project_snapshot: "project_snapshot";
  timeline_snapshot: "timeline_snapshot";
  timeline_selection: "timeline_snapshot";
  project_history: "project_history";
  edit_project: "project_edit";
  media: "media";
  jobs: "jobs";
  preview: "preview";
  export_video: "export_video";
  evidence: "evidence";
  transcript: "transcript";
  analyze_media: "analyze_media";
  sample_frames: "sample_frames";
  create_graphic: "create_graphic";
  assistant: "assistant";
  providers: "providers";
  permissions: "permissions";
};

export type EditorReplyForRequest<Request extends EditorRequest> =
  Request extends { method: infer Method }
    ? Method extends keyof ReplyKindByMethod
      ? Extract<EditorReply, { kind: ReplyKindByMethod[Method] }>
      : never
    : never;

type ActionMethod =
  | "media"
  | "jobs"
  | "preview"
  | "export_video"
  | "evidence"
  | "transcript"
  | "analyze_media"
  | "sample_frames"
  | "create_graphic"
  | "assistant"
  | "providers"
  | "permissions";

type RequestParams<Method extends ActionMethod> = Extract<
  EditorRequest,
  { method: Method }
>["params"];

type CategoryReply<Method extends ActionMethod> = Extract<
  EditorReply,
  { kind: ReplyKindByMethod[Method] }
>;
export interface ArtifactRange {
  offset?: number;
  length?: number;
  /**
   * Request a bounded PNG derivative for assistant evidence. Transports that
   * do not implement this private image path must reject these options rather
   * than treating them as ordinary byte ranges.
   */
  maxEdge?: number;
  maxBytes?: number;
}

export interface ArtifactTransport {
  readArtifact?(
    artifactId: string,
    range?: ArtifactRange,
    context?: EditorCallContext,
  ): Promise<Uint8Array>;
  artifactUrl?(
    artifactId: string,
    context?: EditorCallContext,
  ): string | Promise<string>;
}

export interface PreviewSoftwareContext {
  readonly projectId: string;
  readonly generation: number;
  readonly revision: number;
  readonly planHash: string;
}

export interface SoftwarePreviewTransport {
  subscribePreviewSoftware(
    listener: (packet: SoftwarePreviewPacket) => void,
    context: PreviewSoftwareContext,
    startFrame?: number,
  ): Promise<(() => void) | undefined> | (() => void) | undefined;
  acknowledgePreviewSoftware(
    sequence: number,
    context: PreviewSoftwareContext,
  ): Promise<void> | void;
  cancelPreviewSoftware(
    context: PreviewSoftwareContext,
  ): Promise<void> | void;
}

export interface EditorTransport extends ArtifactTransport {
  call(request: EditorRequest, context?: EditorCallContext): Promise<EditorReply>;
  subscribePreviewSoftware?: SoftwarePreviewTransport["subscribePreviewSoftware"];
  acknowledgePreviewSoftware?: SoftwarePreviewTransport["acknowledgePreviewSoftware"];
  cancelPreviewSoftware?: SoftwarePreviewTransport["cancelPreviewSoftware"];
}
const MAX_SOFTWARE_HEADER_BYTES = 4096;
const MAX_SOFTWARE_PAYLOAD_BYTES = 4 * 1024 * 1024;

function softwareInteger(value: unknown): value is number {
  return typeof value === "number" && Number.isSafeInteger(value) && value >= 0;
}

/**
 * Decode the raw Tauri Channel packet format:
 * `[u32 little-endian metadata length][UTF-8 JSON metadata][JPEG bytes]`.
 * This intentionally rejects JSON byte arrays and oversized/unbounded frames.
 */
export function decodeSoftwarePreviewPacket(
  value: ArrayBuffer | Uint8Array,
): SoftwarePreviewPacket {
  const bytes = value instanceof Uint8Array ? value : new Uint8Array(value);
  if (bytes.byteLength < 5) {
    throw new EditorClientError(
      "SCHEMA_UNSUPPORTED",
      "The software preview packet is truncated.",
    );
  }
  const headerLength =
    bytes[0] |
    (bytes[1] << 8) |
    (bytes[2] << 16) |
    (bytes[3] << 24);
  if (
    headerLength <= 0 ||
    headerLength > MAX_SOFTWARE_HEADER_BYTES ||
    headerLength + 4 >= bytes.byteLength
  ) {
    throw new EditorClientError(
      "SCHEMA_UNSUPPORTED",
      "The software preview packet header is invalid.",
    );
  }
  const payload = bytes.subarray(4 + headerLength);
  if (
    payload.byteLength === 0 ||
    payload.byteLength > MAX_SOFTWARE_PAYLOAD_BYTES
  ) {
    throw new EditorClientError(
      "SCHEMA_UNSUPPORTED",
      "The software preview packet payload is invalid.",
    );
  }
  let metadata: unknown;
  try {
    metadata = JSON.parse(
      new TextDecoder().decode(bytes.subarray(4, 4 + headerLength)),
    );
  } catch {
    throw new EditorClientError(
      "SCHEMA_UNSUPPORTED",
      "The software preview packet metadata is not valid UTF-8 JSON.",
    );
  }
  if (typeof metadata !== "object" || metadata === null) {
    throw new EditorClientError(
      "SCHEMA_UNSUPPORTED",
      "The software preview packet metadata is invalid.",
    );
  }
  const fields = metadata as Record<string, unknown>;
  if (
    typeof fields.projectId !== "string" ||
    typeof fields.planHash !== "string" ||
    typeof fields.contentType !== "string" ||
    !softwareInteger(fields.generation) ||
    !softwareInteger(fields.revision) ||
    !softwareInteger(fields.frame) ||
    !softwareInteger(fields.sequence) ||
    !softwareInteger(fields.width) ||
    !softwareInteger(fields.height)
  ) {
    throw new EditorClientError(
      "SCHEMA_UNSUPPORTED",
      "The software preview packet metadata fields are invalid.",
    );
  }
  return {
    generation: fields.generation,
    projectId: fields.projectId,
    revision: fields.revision,
    planHash: fields.planHash,
    frame: fields.frame,
    sequence: fields.sequence,
    width: fields.width,
    height: fields.height,
    contentType: fields.contentType,
    data: payload.slice(),
  };
}


export interface EditorClientOptions {
  projectId?: string | null;
  generation?: number;
  runId?: string;
  /** Adapters mint IDs from a gesture or Pi run/tool-call boundary. */
  transactionIdFactory?: () => string;
}

export interface EditProjectOptions {
  transactionId?: string;
  dryRun?: boolean;
}

export class EditorClientError extends Error {
  readonly code: EditorErrorCode;
  readonly details?: unknown;

  constructor(code: EditorErrorCode, message: string, details?: unknown) {
    super(message);
    this.name = "EditorClientError";
    this.code = code;
    this.details = details;
  }
}

let fallbackTransactionSequence = 0;

function defaultTransactionId(): string {
  const randomUUID = globalThis.crypto?.randomUUID;
  if (randomUUID) return randomUUID.call(globalThis.crypto);
  fallbackTransactionSequence += 1;
  return `tx-${Date.now().toString(36)}-${fallbackTransactionSequence.toString(36)}`;
}

function checkedEnd(start: number, duration: number, field: string): number {
  const end = start + duration;
  if (!Number.isSafeInteger(end) || end < 0) {
    throw new EditorClientError("INVALID_ARGUMENT", `${field} exceeds the safe integer range.`);
  }
  return end;
}
function timelineEnd(snapshot: ProjectSnapshot): number {
  let end = 0;
  for (const clip of snapshot.document.clips) {
    end = Math.max(end, checkedEnd(clip.startFrame, clip.durationFrames, "clip end"));
  }
  for (const text of snapshot.document.textItems) {
    if (text.startFrame !== undefined && text.durationFrames !== undefined) {
      end = Math.max(end, checkedEnd(text.startFrame, text.durationFrames, "text end"));
    }
  }
  return end;
}

function firstTrack(snapshot: ProjectSnapshot, kind: TrackKind): string {
  const track = snapshot.document.tracks.find((candidate) => candidate.kind === kind);
  if (!track) {
    throw new EditorClientError("INVALID_ARGUMENT", `No ${kind} track is available.`);
  }
  return track.id;
}
export class EditorClient {
  private context: EditorCallContext;
  private softwareContext?: PreviewSoftwareContext;
  private readonly contextListeners = new Set<(context: EditorCallContext) => void>();
  private readonly transactionIdFactory: () => string;
  constructor(
    private readonly transport: EditorTransport,
    options: EditorClientOptions = {},
  ) {
    this.context = {
      projectId: options.projectId ?? null,
      generation: options.generation ?? 0,
      ...(options.runId === undefined ? {} : { runId: options.runId }),
    };
    this.transactionIdFactory = options.transactionIdFactory ?? defaultTransactionId;
  }

  getContext(): EditorCallContext {
    return { ...this.context };
  }

  setContext(context: EditorCallContext): void {
    if (!Number.isSafeInteger(context.generation) || context.generation < 0) {
      throw new EditorClientError(
        "INVALID_ARGUMENT",
        "Editor generation must be a non-negative safe integer.",
      );
    }
    const scopeChanged =
      context.generation !== this.context.generation ||
      context.projectId !== this.context.projectId;
    this.context = { ...context };
    if (!scopeChanged) return;
    this.softwareContext = undefined;
    const current = this.getContext();
    for (const listener of this.contextListeners) {
      try {
        listener(current);
      } catch {
        // A presentation subscriber cannot prevent the native scope update.
      }
    }
  }

  subscribeContext(listener: (context: EditorCallContext) => void): () => void {
    this.contextListeners.add(listener);
    return () => {
      this.contextListeners.delete(listener);
    };
  }

  /**
   * Adopt a scope asserted by a native lifecycle/status response. Native
   * generation changes are monotonic from the client's point of view: a late
   * response/event from a retired generation must not move the client back
   * into that generation.
   */
  adoptNativeScope(scope: Pick<EditorCallContext, "projectId" | "generation">): boolean {
    if (
      !Number.isSafeInteger(scope.generation) ||
      scope.generation < 0 ||
      (scope.projectId !== null && typeof scope.projectId !== "string")
    ) {
      throw new EditorClientError(
        "INVALID_ARGUMENT",
        "The native editor scope is invalid.",
      );
    }
    if (
      scope.generation < this.context.generation ||
      (scope.generation === this.context.generation &&
        scope.projectId === this.context.projectId)
    ) {
      return false;
    }
    this.setContext({ ...this.context, ...scope });
    return true;
  }

  retireGeneration(generation: number): void {
    this.setContext({ ...this.context, generation, projectId: null });
  }

  /**
   * Send a typed request through the one editor boundary. The request and
   * reply are guarded at runtime as well as statically, so a transport cannot
   * silently widen the editor protocol.
   */
  async call<Request extends EditorRequest>(
    request: Request,
    context: EditorCallContext = this.getContext(),
  ): Promise<EditorReplyForRequest<Request>> {
    if (!isEditorRequest(request)) {
      throw new EditorClientError(
        "INVALID_ARGUMENT",
        "The editor request does not match the supported schema.",
      );
    }
    const reply = await this.transport.call(request, context);
    if (!isEditorReply(reply)) {
      throw new EditorClientError(
        "SCHEMA_UNSUPPORTED",
        "The editor returned an unsupported reply.",
      );
    }
    if (reply.kind === "project_status") {
      const projectId = reply.data.open && typeof reply.data.projectId === "string"
        ? reply.data.projectId
        : null;
      this.adoptNativeScope({
        projectId,
        generation: reply.data.generation,
      });
    }
    return reply as EditorReplyForRequest<Request>;
  }

  private async categoryCall<Method extends ActionMethod>(
    method: Method,
    params: RequestParams<Method>,
  ): Promise<CategoryReply<Method>> {
    return this.call({
      method,
      params,
    } as Extract<EditorRequest, { method: Method }>);
  }

  async callMedia(
    params: RequestParams<"media">,
  ): Promise<CategoryReply<"media">> {
    return this.categoryCall("media", params);
  }

  async callJobs(
    params: RequestParams<"jobs">,
  ): Promise<CategoryReply<"jobs">> {
    return this.categoryCall("jobs", params);
  }

  async callPreview(
    params: RequestParams<"preview">,
  ): Promise<CategoryReply<"preview">> {
    return this.categoryCall("preview", params);
  }

  async callExport(
    params: RequestParams<"export_video">,
  ): Promise<CategoryReply<"export_video">> {
    return this.categoryCall("export_video", params);
  }

  async callEvidence(
    params: RequestParams<"evidence">,
  ): Promise<CategoryReply<"evidence">> {
    return this.categoryCall("evidence", params);
  }

  async callAssistant(
    params: RequestParams<"assistant">,
  ): Promise<CategoryReply<"assistant">> {
    return this.categoryCall("assistant", params);
  }

  async callProviders(
    params: RequestParams<"providers">,
  ): Promise<CategoryReply<"providers">> {
    return this.categoryCall("providers", params);
  }

  async callPermissions(
    params: RequestParams<"permissions">,
  ): Promise<CategoryReply<"permissions">> {
    return this.categoryCall("permissions", params);
  }

  async callTranscript(
    params: RequestParams<"transcript">,
  ): Promise<CategoryReply<"transcript">> {
    return this.categoryCall("transcript", params);
  }

  async callAnalyzeMedia(
    params: RequestParams<"analyze_media">,
  ): Promise<CategoryReply<"analyze_media">> {
    return this.categoryCall("analyze_media", params);
  }

  async callSampleFrames(
    params: RequestParams<"sample_frames">,
  ): Promise<CategoryReply<"sample_frames">> {
    return this.categoryCall("sample_frames", params);
  }

  async callCreateGraphic(
    params: RequestParams<"create_graphic">,
  ): Promise<CategoryReply<"create_graphic">> {
    return this.categoryCall("create_graphic", params);
  }

  async fetchArtifact(
    artifactId: string,
    range?: ArtifactRange,
  ): Promise<Uint8Array> {
    if (!/^[A-Za-z0-9._-]{1,256}$/.test(artifactId)) {
      throw new EditorClientError("INVALID_ARGUMENT", "artifactId is invalid.");
    }
    if (!this.transport.readArtifact) {
      throw new EditorClientError(
        "SCHEMA_UNSUPPORTED",
        "This transport does not expose managed artifact bytes.",
      );
    }
    const hasDerivativeOptions = range?.maxEdge !== undefined || range?.maxBytes !== undefined;
    if (
      hasDerivativeOptions &&
      (range?.offset !== undefined || range?.length !== undefined)
    ) {
      throw new EditorClientError(
        "INVALID_ARGUMENT",
        "Image derivative sizing cannot be combined with an artifact range.",
      );
    }
    if (
      range?.offset !== undefined &&
      (!Number.isSafeInteger(range.offset) || range.offset < 0)
    ) {
      throw new EditorClientError("INVALID_ARGUMENT", "artifact offset is invalid.");
    }
    if (
      range?.length !== undefined &&
      (!Number.isSafeInteger(range.length) || range.length < 0)
    ) {
      throw new EditorClientError("INVALID_ARGUMENT", "artifact length is invalid.");
    }
    if (
      range?.maxEdge !== undefined &&
      (!Number.isSafeInteger(range.maxEdge) || range.maxEdge < 1 || range.maxEdge > 1_280)
    ) {
      throw new EditorClientError("INVALID_ARGUMENT", "image derivative edge is invalid.");
    }
    if (
      range?.maxBytes !== undefined &&
      (!Number.isSafeInteger(range.maxBytes) || range.maxBytes < 1 || range.maxBytes > 3 * 1024 * 1024)
    ) {
      throw new EditorClientError("INVALID_ARGUMENT", "image derivative byte limit is invalid.");
    }
    return this.transport.readArtifact(artifactId, range, this.getContext());
  }

  artifactUrl(artifactId: string): string | Promise<string> {
    if (!/^[A-Za-z0-9._-]{1,256}$/.test(artifactId)) {
      throw new EditorClientError("INVALID_ARGUMENT", "artifactId is invalid.");
    }
    if (!this.transport.artifactUrl) {
      throw new EditorClientError(
        "SCHEMA_UNSUPPORTED",
        "This transport does not expose managed artifact URLs.",
      );
    }
    return this.transport.artifactUrl(artifactId, this.getContext());
  }

  async resolveArtifactUrl(artifactId: string): Promise<string> {
    const value = await this.artifactUrl(artifactId);
    if (typeof value !== "string" || value.length === 0) {
      throw new EditorClientError(
        "SCHEMA_UNSUPPORTED",
        "The artifact URL response was invalid.",
      );
    }
    return value;
  }

  async subscribePreviewSoftware(
    listener: (packet: SoftwarePreviewPacket) => void,
    context: PreviewSoftwareContext,
    startFrame = 0,
  ): Promise<(() => void) | undefined> {
    if (!this.transport.subscribePreviewSoftware) {
      throw new EditorClientError(
        "SCHEMA_UNSUPPORTED",
        "This transport does not expose software preview channels.",
      );
    }
    if (
      !context.projectId ||
      !Number.isSafeInteger(context.generation) ||
      context.generation < 0 ||
      !Number.isSafeInteger(context.revision) ||
      context.revision < 0 ||
      !context.planHash ||
      !Number.isSafeInteger(startFrame) ||
      startFrame < 0
    ) {
      throw new EditorClientError(
        "INVALID_ARGUMENT",
        "The software preview identity is invalid.",
      );
    }
    this.softwareContext = { ...context };
    return this.transport.subscribePreviewSoftware(listener, context, startFrame);
  }

  async acknowledgePreviewSoftware(sequence: number): Promise<void> {
    const context = this.softwareContext;
    if (!context) {
      throw new EditorClientError(
        "STALE_SESSION",
        "No software preview session is active.",
      );
    }
    if (!this.transport.acknowledgePreviewSoftware) {
      throw new EditorClientError(
        "SCHEMA_UNSUPPORTED",
        "This transport does not expose software preview acknowledgements.",
      );
    }
    if (!Number.isSafeInteger(sequence) || sequence < 0) {
      throw new EditorClientError("INVALID_ARGUMENT", "The preview sequence is invalid.");
    }
    await this.transport.acknowledgePreviewSoftware(sequence, context);
  }

  async cancelPreviewSoftware(): Promise<void> {
    const context = this.softwareContext;
    if (!context) return;
    if (!this.transport.cancelPreviewSoftware) {
      throw new EditorClientError(
        "SCHEMA_UNSUPPORTED",
        "This transport does not expose software preview cancellation.",
      );
    }
    try {
      await this.transport.cancelPreviewSoftware(context);
    } finally {
      if (this.softwareContext === context) this.softwareContext = undefined;
    }
  }

  async projectStatus(): Promise<ProjectStatus> {
    const reply = await this.transport.call(
      {
        method: "project_status",
        params: {},
      },
      this.getContext(),
    );
    if (!isEditorReply(reply) || reply.kind !== "project_status") {
      throw new EditorClientError(
        "SCHEMA_UNSUPPORTED",
        "The editor returned an unsupported project status reply.",
      );
    }
    const projectId = reply.data.open && typeof reply.data.projectId === "string"
      ? reply.data.projectId
      : null;
    this.adoptNativeScope({
      projectId,
      generation: reply.data.generation,
    });
    return reply.data;
  }

  async projectSnapshot(): Promise<ProjectSnapshot> {
    const reply = await this.transport.call(
      {
        method: "project_snapshot",
        params: {},
      },
      this.getContext(),
    );
    if (!isEditorReply(reply) || reply.kind !== "project_snapshot") {
      throw new EditorClientError(
        "SCHEMA_UNSUPPORTED",
        "The editor returned an unsupported project snapshot reply.",
      );
    }
    return reply.data;
  }

  async timelineSnapshot(): Promise<TimelineSnapshot> {
    const reply = await this.transport.call(
      {
        method: "timeline_snapshot",
        params: {},
      },
      this.getContext(),
    );
    if (!isEditorReply(reply) || reply.kind !== "timeline_snapshot") {
      throw new EditorClientError(
        "SCHEMA_UNSUPPORTED",
        "The editor returned an unsupported timeline snapshot reply.",
      );
    }
    return reply.data;
  }

  async setTimelineSelection(
    selection: TimelineSelection,
  ): Promise<TimelineSnapshot> {
    const params = { selection } satisfies Extract<
      EditorRequest,
      { method: "timeline_selection" }
    >["params"];
    const reply = await this.transport.call(
      { method: "timeline_selection", params },
      this.getContext(),
    );
    if (!isEditorReply(reply) || reply.kind !== "timeline_snapshot") {
      throw new EditorClientError(
        "SCHEMA_UNSUPPORTED",
        "The editor returned an unsupported timeline selection reply.",
      );
    }
    return reply.data;
  }

  async editProject(
    label: string,
    operations: readonly EditOp[],
    expectedRevision: number,
    options: EditProjectOptions = {},
  ): Promise<EditResult> {
    if (!Number.isSafeInteger(expectedRevision) || expectedRevision < 0) {
      throw new EditorClientError(
        "INVALID_ARGUMENT",
        "expectedRevision must be a non-negative safe integer.",
      );
    }
    const transactionId = options.transactionId ?? this.transactionIdFactory();
    if (transactionId.trim().length === 0) {
      throw new EditorClientError("INVALID_ARGUMENT", "transactionId must not be empty.");
    }
    const params = {
      transactionId,
      expectedRevision,
      label,
      operations: [...operations],
      ...(options.dryRun === undefined ? {} : { dryRun: options.dryRun }),
    } satisfies Extract<EditorRequest, { method: "edit_project" }>["params"];
    const reply = await this.transport.call(
      { method: "edit_project", params },
      this.getContext(),
    );
    if (!isEditorReply(reply) || reply.kind !== "project_edit") {
      throw new EditorClientError(
        "SCHEMA_UNSUPPORTED",
        "The editor returned an unsupported edit reply.",
      );
    }
    return reply.data;
  }

  /** Append a complete clip to the first video track without reimplementing edits. */
  async appendClip(
    clip: MediaClip,
    label = "Insert clip",
    options: EditProjectOptions = {},
  ): Promise<EditResult> {
    const snapshot = await this.projectSnapshot();
    const trackId = firstTrack(snapshot, "video");
    let startFrame = 0;
    for (const existing of snapshot.document.clips) {
      if (existing.trackId === trackId) {
        startFrame = Math.max(
          startFrame,
          checkedEnd(existing.startFrame, existing.durationFrames, "clip end"),
        );
      }
    }
    const operation: EditOp = {
      op: "insert_clip",
      clip: { ...clip, trackId, startFrame },
    };
    return this.editProject(label, [operation], snapshot.document.revision, options);
  }

  /** Insert music at zero; trim it to the existing actual timeline when one exists. */
  async appendMusic(
    clip: MediaClip,
    label = "Insert music",
    options: EditProjectOptions = {},
  ): Promise<EditResult> {
    const snapshot = await this.projectSnapshot();
    const trackId = firstTrack(snapshot, "audio");
    const existingDuration = timelineEnd(snapshot);
    const durationFrames =
      existingDuration === 0
        ? clip.durationFrames
        : Math.min(clip.durationFrames, existingDuration);
    const operation: EditOp = {
      op: "insert_clip",
      clip: { ...clip, trackId, startFrame: 0, durationFrames },
    };
    return this.editProject(label, [operation], snapshot.document.revision, options);
  }
}

type NativeErrorFields = {
  code: EditorErrorCode;
  message: string;
  details?: unknown;
};

function nativeErrorFields(value: unknown): NativeErrorFields | undefined {
  if (typeof value !== "object" || value === null) return undefined;
  const fields = value as Record<string, unknown>;
  if (!isEditorErrorCode(fields.code) || typeof fields.message !== "string") {
    return undefined;
  }
  return {
    code: fields.code,
    message: fields.message,
    ...(fields.details === undefined ? {} : { details: fields.details }),
  };
}

/**
 * Tauri rejects a command's `Result::Err` with the serialized AppError rather
 * than resolving an editor response envelope. Normalize that rejection once at
 * the transport boundary so every caller receives the same actionable error.
 */
function normalizeTransportError(error: unknown): EditorClientError {
  if (error instanceof EditorClientError) return error;

  const candidates: unknown[] = [error];
  if (typeof error === "object" && error !== null) {
    const fields = error as Record<string, unknown>;
    if ("error" in fields) candidates.push(fields.error);
    if ("cause" in fields) candidates.push(fields.cause);
  }
  for (const candidate of candidates) {
    const fields = nativeErrorFields(candidate);
    if (fields) return new EditorClientError(fields.code, fields.message, fields.details);
  }

  if (error instanceof Error && error.message.length > 0) {
    return new EditorClientError("IO_ERROR", error.message);
  }
  if (typeof error === "string" && error.trim().length > 0) {
    return new EditorClientError("IO_ERROR", error);
  }
  return new EditorClientError(
    "IO_ERROR",
    "The native editor operation could not be completed.",
  );
}

async function normalizeTransportCall<T>(
  operation: () => T | Promise<T>,
): Promise<T> {
  try {
    return await operation();
  } catch (error) {
    throw normalizeTransportError(error);
  }
}

function requireArtifactContext(
  context: EditorCallContext | undefined,
): EditorCallContext {
  if (
    context === undefined ||
    !Number.isSafeInteger(context.generation) ||
    context.generation < 0
  ) {
    throw new EditorClientError(
      "STALE_SESSION",
      "Managed artifact requests require an explicit native generation.",
    );
  }
  return context;
}

function requireSoftwareContext(
  context: PreviewSoftwareContext,
): PreviewSoftwareContext {
  if (
    !context.projectId ||
    !Number.isSafeInteger(context.generation) ||
    context.generation < 0 ||
    !Number.isSafeInteger(context.revision) ||
    context.revision < 0 ||
    !context.planHash
  ) {
    throw new EditorClientError(
      "STALE_SESSION",
      "Software preview requests require a current native scope.",
    );
  }
  return context;
}

export function createTauriTransport(
  invoke: <T>(command: string, args?: Record<string, unknown>) => Promise<T>,
  artifactTransport: ArtifactTransport = {},
  softwareTransport?: SoftwarePreviewTransport,
): EditorTransport {
  return {
    async call(request) {
      const response = await normalizeTransportCall(() => invoke<unknown>("editor_call", {
        request,
      }));
      if (isEditorReply(response)) return response;
      if (isEditorResponseEnvelope(response)) {
        if (response.ok) return response.data;
        throw new EditorClientError(
          response.error.code,
          response.error.message,
          response.error.details,
        );
      }
      throw new EditorClientError(
        "SCHEMA_UNSUPPORTED",
        "The editor returned an invalid response.",
      );
    },
    async readArtifact(artifactId, range, context) {
      if (range?.maxEdge !== undefined || range?.maxBytes !== undefined) {
        throw new EditorClientError(
          "SCHEMA_UNSUPPORTED",
          "This transport does not expose bounded evidence image derivatives.",
        );
      }
      const requestContext = requireArtifactContext(context);
      if (artifactTransport.readArtifact) {
        return normalizeTransportCall(() => artifactTransport.readArtifact!(artifactId, range, requestContext));
      }
      const response = await normalizeTransportCall(() => invoke<unknown>("artifact_read", {
        artifactId,
        generation: requestContext.generation,
        ...(range?.offset === undefined ? {} : { offset: range.offset }),
        ...(range?.length === undefined ? {} : { length: range.length }),
      }));
      if (response instanceof ArrayBuffer) return new Uint8Array(response);
      if (response instanceof Uint8Array) return response;
      if (Array.isArray(response) && response.every((byte) => Number.isInteger(byte))) {
        return Uint8Array.from(response as number[]);
      }
      if (
        typeof response === "object" &&
        response !== null &&
        "bytes" in response &&
        Array.isArray(response.bytes) &&
        response.bytes.every((byte) => Number.isInteger(byte))
      ) {
        return Uint8Array.from(response.bytes as number[]);
      }
      throw new EditorClientError(
        "SCHEMA_UNSUPPORTED",
        "The artifact response did not contain bounded binary data.",
      );
    },
    async artifactUrl(artifactId, context) {
      const requestContext = requireArtifactContext(context);
      if (artifactTransport.artifactUrl) {
        return normalizeTransportCall(() => artifactTransport.artifactUrl!(artifactId, requestContext));
      }
      const response = await normalizeTransportCall(() => invoke<unknown>("artifact_url", {
        artifactId,
        generation: requestContext.generation,
      }));
      if (typeof response !== "string" || !response.startsWith("artifact://")) {
        throw new EditorClientError(
          "SCHEMA_UNSUPPORTED",
          "The artifact URL response was invalid.",
        );
      }
      return response;
    },
    ...(softwareTransport === undefined
      ? {}
      : {
          subscribePreviewSoftware(listener, context, startFrame) {
            return normalizeTransportCall(() =>
              softwareTransport.subscribePreviewSoftware(
                listener,
                requireSoftwareContext(context),
                startFrame,
              ),
            );
          },
          acknowledgePreviewSoftware(sequence, context) {
            return normalizeTransportCall(() =>
              softwareTransport.acknowledgePreviewSoftware(
                sequence,
                requireSoftwareContext(context),
              ),
            );
          },
          cancelPreviewSoftware(context) {
            return normalizeTransportCall(() =>
              softwareTransport.cancelPreviewSoftware(
                requireSoftwareContext(context),
              ),
            );
          },
        }),
  };

}
export function createEnvelopeTransport(
  send: (
    request: EditorRequest,
    context: EditorCallContext,
  ) => Promise<EditorResponseEnvelope>,
  artifactTransport: ArtifactTransport = {},
): EditorTransport {
  return {
    async call(request, context) {
      const response = await normalizeTransportCall(() => send(request, context ?? { projectId: null, generation: 0 }));
      if (!isEditorResponseEnvelope(response)) {
        throw new EditorClientError(
          "SCHEMA_UNSUPPORTED",
          "The editor bridge returned an invalid response envelope.",
        );
      }
      if (!response.ok) {
        throw new EditorClientError(
          response.error.code,
          response.error.message,
          response.error.details,
        );
      }
      return response.data;
    },
    ...(artifactTransport.readArtifact === undefined
      ? {}
      : { readArtifact: artifactTransport.readArtifact }),
    ...(artifactTransport.artifactUrl === undefined
      ? {}
      : { artifactUrl: artifactTransport.artifactUrl }),
  };
}
