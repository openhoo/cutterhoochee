import type {
  EditResult,
  EditorError,
  EditorErrorCode,
  EditorEvent,
  EditorReply,
  EditorRequest,
  EditorResponseEnvelope,
  ProjectSnapshot,
  ProjectStatus,
  SafeInteger,
  TimelineSelection,
  TimelineSnapshot,
} from "./generated.js";

export const IPC_VERSION = 1 as const;
export type IpcVersion = typeof IPC_VERSION;

export type {
  EditorEnvelope,
  EditorError,
  EditorErrorCode,
  EditorEvent,
  EditorEventEnvelope,
  EditorReply,
  EditorRequest,
  EditorRequestEnvelope,
  EditorResponseEnvelope,
  EditorResponseErrorEnvelope,
  EditorResponseSuccessEnvelope,
  IpcEnvelopeBase,
  ProjectStatus,
  SafeInteger,
} from "./generated.js";

export interface EditorCallContext {
  projectId: string | null;
  generation: SafeInteger;
  runId?: string;
  requestId?: string;
}

const ERROR_CODES: readonly string[] = [
  "INVALID_ARGUMENT",
  "REVISION_CONFLICT",
  "IDEMPOTENCY_CONFLICT",
  "STALE_SESSION",
  "PERMISSION_DENIED",
  "ASSET_UNAVAILABLE",
  "MEDIA_UNSUPPORTED",
  "JOB_CANCELLED",
  "IO_ERROR",
  "AUTH_REQUIRED",
  "PROVIDER_ERROR",
  "BUSY",
  "SCHEMA_UNSUPPORTED",
];

export function isEditorErrorCode(value: unknown): value is EditorErrorCode {
  return typeof value === "string" && ERROR_CODES.includes(value);
}

export function isSafeInteger(value: unknown): value is SafeInteger {
  return typeof value === "number" && Number.isSafeInteger(value);
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function hasOnlyKeys(
  value: Record<string, unknown>,
  keys: readonly string[],
): boolean {
  return Object.keys(value).every((key) => keys.includes(key));
}

function hasKeys(
  value: Record<string, unknown>,
  keys: readonly string[],
): boolean {
  return keys.every((key) => Object.prototype.hasOwnProperty.call(value, key));
}

function isNonNegativeSafeInteger(value: unknown): value is SafeInteger {
  return isSafeInteger(value) && value >= 0;
}

function isFiniteNumber(value: unknown): value is number {
  return typeof value === "number" && Number.isFinite(value);
}

function isStringArray(value: unknown): value is string[] {
  return Array.isArray(value) && value.every((item) => typeof item === "string");
}

function isByteArray(value: unknown): boolean {
  return (
    Array.isArray(value) &&
    value.every(
      (item) =>
        typeof item === "number" &&
        Number.isInteger(item) &&
        item >= 0 &&
        item <= 255,
    )
  );
}

function isEmptyParams(value: unknown): value is Record<string, never> {
  return isRecord(value) && Object.keys(value).length === 0;
}

function isTimelineRange(value: unknown): boolean {
  if (
    !isRecord(value) ||
    !hasOnlyKeys(value, ["startFrame", "endFrame"]) ||
    !hasKeys(value, ["startFrame", "endFrame"]) ||
    !isNonNegativeSafeInteger(value.startFrame) ||
    !isNonNegativeSafeInteger(value.endFrame)
  ) {
    return false;
  }
  return value.endFrame > value.startFrame;
}

export function isTimelineSelection(value: unknown): value is TimelineSelection {
  if (
    !isRecord(value) ||
    !hasOnlyKeys(value, ["clipIds", "textIds", "playheadFrame", "range"]) ||
    !hasKeys(value, ["clipIds", "textIds", "playheadFrame"]) ||
    !isStringArray(value.clipIds) ||
    !isStringArray(value.textIds) ||
    !isNonNegativeSafeInteger(value.playheadFrame)
  ) {
    return false;
  }
  return value.range === undefined || isTimelineRange(value.range);
}

export function isTimelineSnapshot(value: unknown): value is TimelineSnapshot {
  return (
    isRecord(value) &&
    hasOnlyKeys(value, ["revision", "durationFrames", "selection", "document"]) &&
    hasKeys(value, ["revision", "durationFrames", "selection", "document"]) &&
    isNonNegativeSafeInteger(value.revision) &&
    isNonNegativeSafeInteger(value.durationFrames) &&
    isTimelineSelection(value.selection) &&
    isRecord(value.document)
  );
}

function isEditOperation(value: unknown): boolean {
  if (!isRecord(value) || typeof value.op !== "string") return false;
  switch (value.op) {
    case "set_project":
      return (
        hasOnlyKeys(value, ["op", "name", "aspect"]) &&
        (value.name === undefined || typeof value.name === "string") &&
        (value.aspect === undefined || typeof value.aspect === "string")
      );
    case "add_track":
      return (
        hasOnlyKeys(value, ["op", "id", "kind", "name", "index"]) &&
        hasKeys(value, ["op", "id", "kind", "name", "index"]) &&
        typeof value.id === "string" &&
        typeof value.kind === "string" &&
        typeof value.name === "string" &&
        isNonNegativeSafeInteger(value.index)
      );
    case "update_track":
      return (
        hasOnlyKeys(value, ["op", "trackId", "name", "muted", "locked"]) &&
        hasKeys(value, ["op", "trackId"]) &&
        typeof value.trackId === "string" &&
        (value.name === undefined || typeof value.name === "string") &&
        (value.muted === undefined || typeof value.muted === "boolean") &&
        (value.locked === undefined || typeof value.locked === "boolean")
      );
    case "remove_track":
      return (
        hasOnlyKeys(value, ["op", "trackId", "deleteItems"]) &&
        hasKeys(value, ["op", "trackId", "deleteItems"]) &&
        typeof value.trackId === "string" &&
        typeof value.deleteItems === "boolean"
      );
    case "insert_clip":
      return (
        hasOnlyKeys(value, ["op", "clip"]) &&
        hasKeys(value, ["op", "clip"]) &&
        isRecord(value.clip)
      );
    case "move_clip":
      return (
        hasOnlyKeys(value, ["op", "clipId", "trackId", "startFrame"]) &&
        hasKeys(value, ["op", "clipId", "trackId", "startFrame"]) &&
        typeof value.clipId === "string" &&
        typeof value.trackId === "string" &&
        isNonNegativeSafeInteger(value.startFrame)
      );
    case "trim_clip":
      return (
        hasOnlyKeys(value, ["op", "clipId", "inFrame", "startFrame", "durationFrames"]) &&
        hasKeys(value, ["op", "clipId", "inFrame", "startFrame", "durationFrames"]) &&
        typeof value.clipId === "string" &&
        isNonNegativeSafeInteger(value.inFrame) &&
        isNonNegativeSafeInteger(value.startFrame) &&
        isNonNegativeSafeInteger(value.durationFrames) &&
        value.durationFrames > 0
      );
    case "split_clip":
      return (
        hasOnlyKeys(value, ["op", "clipId", "frame", "rightClipId"]) &&
        hasKeys(value, ["op", "clipId", "frame", "rightClipId"]) &&
        typeof value.clipId === "string" &&
        isNonNegativeSafeInteger(value.frame) &&
        typeof value.rightClipId === "string"
      );
    case "update_clip":
      return (
        hasOnlyKeys(value, ["op", "clipId", "patch"]) &&
        hasKeys(value, ["op", "clipId", "patch"]) &&
        typeof value.clipId === "string" &&
        isRecord(value.patch) &&
        hasOnlyKeys(value.patch, [
          "fit",
          "centerX",
          "centerY",
          "scale",
          "opacity",
          "gainDb",
          "audioEnabled",
          "fadeInFrames",
          "fadeOutFrames",
        ])
      );
    case "remove_clips":
      return (
        hasOnlyKeys(value, ["op", "clipIds"]) &&
        hasKeys(value, ["op", "clipIds"]) &&
        isStringArray(value.clipIds)
      );
    case "remove_range":
      return (
        hasOnlyKeys(value, ["op", "startFrame", "endFrame", "ripple"]) &&
        hasKeys(value, ["op", "startFrame", "endFrame", "ripple"]) &&
        isNonNegativeSafeInteger(value.startFrame) &&
        isNonNegativeSafeInteger(value.endFrame) &&
        value.endFrame > value.startFrame &&
        typeof value.ripple === "boolean"
      );
    case "add_text":
      return (
        hasOnlyKeys(value, ["op", "item"]) &&
        hasKeys(value, ["op", "item"]) &&
        isRecord(value.item)
      );
    case "update_text":
      return (
        hasOnlyKeys(value, ["op", "textId", "patch"]) &&
        hasKeys(value, ["op", "textId", "patch"]) &&
        typeof value.textId === "string" &&
        isRecord(value.patch) &&
        hasOnlyKeys(value.patch, [
          "text",
          "style",
          "color",
          "fontSize",
          "positionX",
          "positionY",
          "lineBreaks",
          "startFrame",
          "durationFrames",
          "sourceStartFrame",
          "sourceDurationFrames",
        ])
      );
    case "remove_text":
      return (
        hasOnlyKeys(value, ["op", "textId"]) &&
        hasKeys(value, ["op", "textId"]) &&
        typeof value.textId === "string"
      );
    case "add_transition":
      return (
        hasOnlyKeys(value, ["op", "leftClipId", "rightClipId", "durationFrames"]) &&
        hasKeys(value, ["op", "leftClipId", "rightClipId", "durationFrames"]) &&
        typeof value.leftClipId === "string" &&
        typeof value.rightClipId === "string" &&
        isNonNegativeSafeInteger(value.durationFrames)
      );
    case "remove_transition":
      return (
        hasOnlyKeys(value, ["op", "transitionId"]) &&
        hasKeys(value, ["op", "transitionId"]) &&
        typeof value.transitionId === "string"
      );
    case "replace_captions":
      return (
        hasOnlyKeys(value, ["op", "clipId", "transcriptId", "style"]) &&
        hasKeys(value, ["op", "clipId", "transcriptId", "style"]) &&
        typeof value.clipId === "string" &&
        typeof value.transcriptId === "string" &&
        typeof value.style === "string"
      );
    default:
      return false;
  }
}

type ActionSpec = { keys: readonly string[]; required: readonly string[] };

function actionSpec(
  keys: readonly string[],
  required: readonly string[] = keys,
): ActionSpec {
  return { keys, required };
}

const CATEGORY_ACTIONS: Record<string, Record<string, ActionSpec>> = {
  media: {
    list: actionSpec([]),
    inspect: actionSpec(["assetId"]),
    import: actionSpec(["paths"], []),
    relink: actionSpec(["assetId"]),
    remove: actionSpec(["assetId"]),
    thumbnail: actionSpec(["assetId", "frame"], ["assetId"]),
    waveform: actionSpec(["assetId"]),
  },
  jobs: {
    list: actionSpec([]),
    get: actionSpec(["jobId"]),
    cancel: actionSpec(["jobId"]),
  },
  preview: {
    plan: actionSpec(["revision"]),
    render_frame: actionSpec(["revision", "frame"]),
    render_audio_window: actionSpec(["planHash", "startSample", "sampleCount"]),
    seek: actionSpec(["frame"]),
    play: actionSpec([]),
    pause: actionSpec([]),
    inspect: actionSpec(["revision", "frames"]),
  },
  export_video: {
    start: actionSpec(["revision", "resolution", "srt"]),
    get: actionSpec(["jobId"]),
    progress: actionSpec(["jobId"]),
    cancel: actionSpec(["jobId"]),
    status: actionSpec(["jobId"]),
    play: actionSpec(["jobId"]),
    show_file: actionSpec(["jobId"]),
  },
  evidence: {
    transcribe: actionSpec(["assetId", "startFrame", "endFrame", "modelConsent"], ["assetId"]),
    read: actionSpec(["transcriptId"]),
    search: actionSpec(["query", "assetId"], ["query"]),
    import_srt: actionSpec(["playheadFrame", "style"], ["playheadFrame"]),
    export_srt: actionSpec([]),
    scenes: actionSpec(["assetId", "threshold"], ["assetId"]),
    sample_frames: actionSpec(["assetId", "startFrame", "endFrame", "count"], [
      "assetId",
      "startFrame",
      "endFrame",
    ]),
    create_graphic: actionSpec(["name", "svg", "width", "height"]),
    model_status: actionSpec([]),
  },
  transcript: {
    transcribe: actionSpec(["assetId", "startFrame", "endFrame", "modelConsent"], ["assetId"]),
    read: actionSpec(["transcriptId"]),
    search: actionSpec(["query", "assetId"], ["query"]),
    import_srt: actionSpec(["playheadFrame", "style"], ["playheadFrame"]),
    export_srt: actionSpec([]),
    model_status: actionSpec([]),
  },
  analyze_media: {
    scenes: actionSpec(["assetId", "threshold"], ["assetId"]),
    silence: actionSpec(["assetId"]),
  },
  sample_frames: {
    sample_frames: actionSpec(["assetId", "startFrame", "endFrame", "count"], [
      "assetId",
      "startFrame",
      "endFrame",
    ]),
  },
  create_graphic: {
    create_graphic: actionSpec(["name", "svg", "width", "height"]),
  },
  assistant: {
    status: actionSpec([]),
    history: actionSpec([]),
    prompt: actionSpec(["text"]),
    stop: actionSpec([]),
    restart: actionSpec([]),
    new_session: actionSpec([]),
  },
  providers: {
    list: actionSpec([]),
    models: actionSpec(["providerId"]),
    login: actionSpec(["providerId", "authType", "sessionOnly"], ["providerId", "authType"]),
    answer: actionSpec(["promptId", "value"]),
    logout: actionSpec(["providerId"]),
    select: actionSpec(["providerId", "modelId"]),
    refresh: actionSpec([]),
  },
  permissions: {
    pending: actionSpec([]),
    answer: actionSpec(["operationId", "allow"]),
    evidence: actionSpec(["providerId", "accountId", "allow"]),
    revoke: actionSpec(["providerId", "accountId"], []),
    grant_files: actionSpec(["paths", "purpose"], ["purpose"]),
    system_read: actionSpec(
      ["path", "offset", "length", "operationId"],
      ["path", "offset", "length"],
    ),
    system_write: actionSpec(["path", "data", "overwrite", "operationId"], [
      "path",
      "data",
      "overwrite",
    ]),
    system_execute: actionSpec(
      ["executable", "arguments", "cwd", "environment", "timeoutMs", "operationId"],
      ["executable", "arguments", "cwd"],
    ),
    system_http: actionSpec(
      ["url", "method", "headers", "body", "timeoutMs", "operationId"],
      ["url", "method", "headers", "body"],
    ),
  },
};

function isActionEnvelope(
  value: unknown,
  category: string,
): boolean {
  if (!isRecord(value) || !hasKeys(value, ["action"]) || typeof value.action !== "string") {
    return false;
  }
  const spec = CATEGORY_ACTIONS[category]?.[value.action];
  if (!spec) return false;
  const nested = category === "permissions";
  const payload =
    nested && value.params === undefined
      ? {}
      : nested
        ? isRecord(value.params)
          ? value.params
          : undefined
        : value;
  if (nested) {
    if (!hasOnlyKeys(value, ["action", "params"]) || !payload) return false;
    if (!hasOnlyKeys(payload, spec.keys)) return false;
  } else if (!hasOnlyKeys(value, ["action", ...spec.keys])) {
    return false;
  }
  if (!payload || !hasKeys(payload, spec.required)) return false;
  for (const key of spec.required) {
    if (
      key === "offset" ||
      key === "length" ||
      key === "startFrame" ||
      key === "endFrame" ||
      key === "count" ||
      key === "revision" ||
      key === "frame" ||
      key === "startSample" ||
      key === "sampleCount" ||
      key === "playheadFrame" ||
      key === "width" ||
      key === "height" ||
      key === "timeoutMs"
    ) {
      if (!isNonNegativeSafeInteger(payload?.[key])) return false;
    }
    if (key === "allow" || key === "srt" || key === "overwrite" || key === "sessionOnly") {
      if (typeof payload?.[key] !== "boolean") return false;
    }
    if (key === "paths" || key === "arguments") {
      if (!isStringArray(payload?.[key]) && !isByteArray(payload?.[key])) return false;
    }
    if (
      [
        "assetId",
        "jobId",
        "planHash",
        "transcriptId",
        "query",
        "text",
        "providerId",
        "authType",
        "promptId",
        "operationId",
        "purpose",
        "path",
        "executable",
        "cwd",
        "url",
        "method",
        "modelId",
        "name",
        "svg",
      ].includes(key) &&
      typeof payload?.[key] !== "string"
    ) {
      return false;

    }
  }
  if (value.action === "inspect" && category === "preview") {
    if (!payload || !Array.isArray(payload.frames) || !payload.frames.every(isNonNegativeSafeInteger)) return false;
  }
  if (value.action === "sample_frames" && category === "evidence") {
    if (
      !payload ||
      !isNonNegativeSafeInteger(payload.startFrame) ||
      !isNonNegativeSafeInteger(payload.endFrame)
    ) return false;
  }
  if (value.action === "system_write" && category === "permissions") {
    if (!payload || !isByteArray(payload.data)) return false;
  }
  return true;
}

function isCategoryRequestParams(value: unknown, category: string): boolean {
  return isActionEnvelope(value, category);
}
function isSampleFramesParams(value: unknown): boolean {
  return (
    isRecord(value) &&
    hasOnlyKeys(value, ["assetId", "startFrame", "endFrame", "count"]) &&
    hasKeys(value, ["assetId", "startFrame", "endFrame"]) &&
    typeof value.assetId === "string" &&
    isNonNegativeSafeInteger(value.startFrame) &&
    isNonNegativeSafeInteger(value.endFrame) &&
    (value.count === undefined || isNonNegativeSafeInteger(value.count))
  );
}

function isCreateGraphicParams(value: unknown): boolean {
  return (
    isRecord(value) &&
    hasOnlyKeys(value, ["name", "svg", "width", "height"]) &&
    hasKeys(value, ["name", "svg", "width", "height"]) &&
    typeof value.name === "string" &&
    typeof value.svg === "string" &&
    isNonNegativeSafeInteger(value.width) &&
    isNonNegativeSafeInteger(value.height)
  );
}

export function isEditorRequest(value: unknown): value is EditorRequest {
  if (
    !isRecord(value) ||
    !hasOnlyKeys(value, ["method", "params"]) ||
    !hasKeys(value, ["method", "params"]) ||
    typeof value.method !== "string"
  ) {
    return false;
  }

  switch (value.method) {
    case "project_status":
    case "project_open":
    case "project_save":
    case "project_close":
    case "project_snapshot":
    case "timeline_snapshot":
      return isEmptyParams(value.params);
    case "project_create":
      return (
        isRecord(value.params) &&
        hasOnlyKeys(value.params, ["name", "aspect", "fpsNum", "fpsDen"]) &&
        hasKeys(value.params, ["name"]) &&
        typeof value.params.name === "string" &&
        (value.params.aspect === undefined || typeof value.params.aspect === "string") &&
        (value.params.fpsNum === undefined || isNonNegativeSafeInteger(value.params.fpsNum)) &&
        (value.params.fpsDen === undefined || isNonNegativeSafeInteger(value.params.fpsDen))
      );
    case "timeline_selection":
      return (
        isRecord(value.params) &&
        hasOnlyKeys(value.params, ["selection"]) &&
        hasKeys(value.params, ["selection"]) &&
        isTimelineSelection(value.params.selection)
      );
    case "project_history":
      return (
        isRecord(value.params) &&
        hasOnlyKeys(value.params, ["action", "expectedRevision", "expectedTransactionId"]) &&
        hasKeys(value.params, ["action", "expectedRevision"]) &&
        (value.params.action === "undo" || value.params.action === "redo") &&
        isNonNegativeSafeInteger(value.params.expectedRevision) &&
        (value.params.expectedTransactionId === undefined || typeof value.params.expectedTransactionId === "string")
      );
    case "edit_project":
      return (
        isRecord(value.params) &&
        hasOnlyKeys(value.params, ["transactionId", "expectedRevision", "label", "operations", "dryRun"]) &&
        hasKeys(value.params, ["transactionId", "expectedRevision", "label", "operations"]) &&
        typeof value.params.transactionId === "string" &&
        isNonNegativeSafeInteger(value.params.expectedRevision) &&
        typeof value.params.label === "string" &&
        Array.isArray(value.params.operations) &&
        value.params.operations.every(isEditOperation) &&
        (value.params.dryRun === undefined || typeof value.params.dryRun === "boolean")
      );
    case "media":
    case "jobs":
    case "preview":
    case "export_video":
    case "evidence":
    case "transcript":
    case "analyze_media":
    case "assistant":
    case "providers":
    case "permissions":
      return isCategoryRequestParams(value.params, value.method);
    case "sample_frames":
      return isSampleFramesParams(value.params);
    case "create_graphic":
      return isCreateGraphicParams(value.params);
    default:
      return false;
  }
}

export function isProjectStatus(value: unknown): value is ProjectStatus {
  if (!isRecord(value) || !hasKeys(value, ["generation", "open"])) return false;
  if (!isSafeInteger(value.generation) || value.generation < 0 || typeof value.open !== "boolean") return false;
  if (value.projectId !== undefined && typeof value.projectId !== "string") return false;
  if (value.workspaceId !== undefined && typeof value.workspaceId !== "string") return false;
  if (value.name !== undefined && typeof value.name !== "string") return false;
  if (value.revision !== undefined && !isNonNegativeSafeInteger(value.revision)) return false;
  return true;
}

function isEditResult(value: unknown): value is EditResult {
  if (!isRecord(value)) return false;
  return (
    hasKeys(value, ["transactionId", "label", "revision", "changed", "affectedEntities"]) &&
    typeof value.transactionId === "string" &&
    typeof value.label === "string" &&
    isNonNegativeSafeInteger(value.revision) &&
    typeof value.changed === "boolean" &&
    Array.isArray(value.affectedEntities)
  );
}

function isProjectSnapshot(value: unknown): value is ProjectSnapshot {
  return (
    isRecord(value) &&
    hasKeys(value, ["workspaceId", "document"]) &&
    typeof value.workspaceId === "string" &&
    isRecord(value.document)
  );
}

const CATEGORY_REPLY_KINDS: Record<string, readonly string[]> = {
  media: ["list", "inspect", "import", "relink", "remove", "thumbnail", "waveform"],
  jobs: ["list", "get", "cancel"],
  preview: ["plan", "frame", "audio", "inspect", "ack"],
  export_video: [
    "started",
    "status",
    "cancelled",
    "played",
    "file_shown",
  ],
  evidence: [
    "transcript",
    "transcript_read",
    "transcript_search",
    "transcript_model_status",
    "srt_imported",
    "srt_exported",
    "analysis",
    "sample_frames",
    "graphic",
  ],
  transcript: [
    "transcript",
    "transcript_read",
    "transcript_search",
    "transcript_model_status",
    "srt_imported",
    "srt_exported",
  ],
  analyze_media: ["analysis"],
  sample_frames: ["sample_frames"],
  create_graphic: ["graphic"],
  permissions: [
    "pending",
    "answer",
    "evidence",
    "files",
    "system_read",
    "system_write",
    "system_execute",
    "system_http",
  ],
};

const CATEGORY_REPLY_ACTIONS: Record<string, readonly string[]> = {
  assistant: ["status", "history", "prompt", "stop", "restart", "new_session"],
  providers: ["list", "models", "login", "answer", "logout", "select", "refresh"],
};

const NESTED_REPLY_KINDS: Record<string, readonly string[]> = {
  analysis: ["scenes", "silence"],
};

const CATEGORY_REPLY_ARRAY_KINDS: Record<string, readonly string[]> = {
  permissions: ["pending", "files"],
};

function ownReplyKinds(
  table: Record<string, readonly string[]>,
  key: string,
): readonly string[] | undefined {
  return Object.prototype.hasOwnProperty.call(table, key) ? table[key] : undefined;
}

function isTaggedCategoryReply(
  value: unknown,
  category: string,
): boolean {
  if (
    !isRecord(value) ||
    !hasKeys(value, ["kind", "data"]) ||
    !hasOnlyKeys(value, ["kind", "data"]) ||
    value.kind !== category ||
    !isRecord(value.data)
  ) {
    return false;
  }

  const nested = value.data;
  const categoryKinds = ownReplyKinds(CATEGORY_REPLY_KINDS, category);
  if (
    !hasKeys(nested, ["kind", "data"]) ||
    !hasOnlyKeys(nested, ["kind", "data"]) ||
    typeof nested.kind !== "string" ||
    !categoryKinds?.includes(nested.kind)
  ) {
    return false;
  }

  const leaf = nested.data;
  const nestedKinds = ownReplyKinds(NESTED_REPLY_KINDS, nested.kind);
  if (nestedKinds) {
    return (
      isRecord(leaf) &&
      hasKeys(leaf, ["kind", "data"]) &&
      hasOnlyKeys(leaf, ["kind", "data"]) &&
      typeof leaf.kind === "string" &&
      nestedKinds.includes(leaf.kind) &&
      isRecord(leaf.data)
    );
  }

  const arrayKinds = ownReplyKinds(CATEGORY_REPLY_ARRAY_KINDS, category);
  if (arrayKinds?.includes(nested.kind)) {
    return Array.isArray(leaf);
  }
  return isRecord(leaf);
}

function isActionCategoryReply(
  value: unknown,
  category: string,
): boolean {
  if (
    !isRecord(value) ||
    !hasKeys(value, ["kind", "data"]) ||
    !hasOnlyKeys(value, ["kind", "data"]) ||
    value.kind !== category ||
    !isRecord(value.data)
  ) {
    return false;
  }

  const data = value.data;
  const actionKinds = ownReplyKinds(CATEGORY_REPLY_ACTIONS, category);
  return (
    typeof data.action === "string" &&
    actionKinds?.includes(data.action) === true
  );
}

function isCategoryReply(value: unknown, category: string): boolean {
  if (category === "assistant" || category === "providers") {
    return isActionCategoryReply(value, category);
  }
  return isTaggedCategoryReply(value, category);
}

export function isEditorReply(value: unknown): value is EditorReply {
  if (!isRecord(value) || !hasKeys(value, ["kind", "data"])) return false;
  if (!hasOnlyKeys(value, ["kind", "data"])) return false;
  if (value.kind === "project_status") return isProjectStatus(value.data);
  if (value.kind === "project_snapshot") return isProjectSnapshot(value.data);
  if (value.kind === "timeline_snapshot") return isTimelineSnapshot(value.data);
  if (value.kind === "project_edit" || value.kind === "project_history") return isEditResult(value.data);
  if (typeof value.kind !== "string") return false;
  return isCategoryReply(value, value.kind);
}

export function isEditorError(value: unknown): value is EditorError {
  if (!isRecord(value) || !hasKeys(value, ["code", "message"]) || !hasOnlyKeys(value, ["code", "message", "details"])) return false;
  return isEditorErrorCode(value.code) && typeof value.message === "string";
}

export function isEditorEvent(value: unknown): value is EditorEvent {
  if (!isRecord(value) || !hasOnlyKeys(value, ["kind", "projectId", "generation", "runId", "data"]) || !hasKeys(value, ["kind", "projectId", "generation"])) return false;
  return (
    typeof value.kind === "string" &&
    (typeof value.projectId === "string" || value.projectId === null) &&
    isNonNegativeSafeInteger(value.generation) &&
    (value.runId === undefined || typeof value.runId === "string")
  );
}

export function isEditorResponseEnvelope(
  value: unknown,
): value is EditorResponseEnvelope {
  if (!isRecord(value) || !hasKeys(value, ["v", "id", "projectId", "generation", "kind", "ok"])) return false;
  if (
    value.v !== IPC_VERSION ||
    typeof value.id !== "string" ||
    value.id.length === 0 ||
    (typeof value.projectId !== "string" && value.projectId !== null) ||
    !isNonNegativeSafeInteger(value.generation) ||
    value.kind !== "response" ||
    typeof value.ok !== "boolean"
  ) return false;
  if (value.ok) return isEditorReply(value.data);
  return isEditorError(value.error);
}
