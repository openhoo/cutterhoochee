import type { EditOp, EditorReply, EditorRequest, TimelineSelection } from "@cutterhoochee/shared/generated";
import { EditorClient, EditorClientError, type ArtifactRange } from "@cutterhoochee/shared/client";
import { Type, type Static, type TObject, type TSchema } from "typebox";
import { Value } from "typebox/value";
import type {
  AgentToolResult,
  AgentToolUpdateCallback,
  ExtensionContext,
  ToolDefinition,
} from "@earendil-works/pi-coding-agent";
import { defineTool } from "@earendil-works/pi-coding-agent";

import { BridgeProtocolError } from "./bridge.js";

export interface AgentEditorPort {
  clientFor(toolCallId: string, signal?: AbortSignal, nativeRunId?: string): EditorClient;
  callEditor(
    request: EditorRequest,
    options?: { signal?: AbortSignal; runId?: string },
  ): Promise<EditorReply>;
}

export type ToolRunContext = {
  toolCallId: string;
  nativeRunId: () => string | undefined;
  port: AgentEditorPort;
};

const safeInteger = Type.Integer({ minimum: 0, maximum: Number.MAX_SAFE_INTEGER });
const positiveSafeInteger = Type.Integer({ minimum: 1, maximum: Number.MAX_SAFE_INTEGER });
const stringId = Type.String({ minLength: 1 });
const DEFAULT_SYSTEM_TIMEOUT_MS = 60_000;
const emptyObject = () => Type.Object({}, { additionalProperties: false });

const colorSchema = Type.Object(
  {
    red: Type.Integer({ minimum: 0, maximum: 255 }),
    green: Type.Integer({ minimum: 0, maximum: 255 }),
    blue: Type.Integer({ minimum: 0, maximum: 255 }),
    alpha: Type.Integer({ minimum: 0, maximum: 255 }),
  },
  { additionalProperties: false },
);

const clipSchema = Type.Object(
  {
    id: stringId,
    trackId: stringId,
    assetId: stringId,
    startFrame: safeInteger,
    inFrame: safeInteger,
    durationFrames: positiveSafeInteger,
    fit: Type.Union([Type.Literal("contain"), Type.Literal("cover")]),
    centerX: Type.Integer({ minimum: 0, maximum: 10000 }),
    centerY: Type.Integer({ minimum: 0, maximum: 10000 }),
    scale: Type.Integer({ minimum: 100, maximum: 40000 }),
    opacity: Type.Integer({ minimum: 0, maximum: 10000 }),
    gainDb: Type.Number({ minimum: -60, maximum: 12 }),
    audioEnabled: Type.Boolean(),
    fadeInFrames: safeInteger,
    fadeOutFrames: safeInteger,
  },
  { additionalProperties: false },
);

const clipPatchSchema = Type.Object(
  {
    fit: Type.Optional(Type.Union([Type.Literal("contain"), Type.Literal("cover")])),
    centerX: Type.Optional(Type.Integer({ minimum: 0, maximum: 10000 })),
    centerY: Type.Optional(Type.Integer({ minimum: 0, maximum: 10000 })),
    scale: Type.Optional(Type.Integer({ minimum: 100, maximum: 40000 })),
    opacity: Type.Optional(Type.Integer({ minimum: 0, maximum: 10000 })),
    gainDb: Type.Optional(Type.Number({ minimum: -60, maximum: 12 })),
    audioEnabled: Type.Optional(Type.Boolean()),
    fadeInFrames: Type.Optional(safeInteger),
    fadeOutFrames: Type.Optional(safeInteger),
  },
  { additionalProperties: false },
);

const textItemSchema = Type.Object(
  {
    id: stringId,
    trackId: stringId,
    kind: Type.Union([Type.Literal("title"), Type.Literal("caption")]),
    text: Type.String(),
    style: Type.Union([Type.Literal("clean"), Type.Literal("boxed")]),
    color: colorSchema,
    fontSize: Type.Number({ exclusiveMinimum: 0 }),
    positionX: Type.Integer({ minimum: 0, maximum: 10000 }),
    positionY: Type.Integer({ minimum: 0, maximum: 10000 }),
    lineBreaks: Type.Array(safeInteger),
    startFrame: Type.Optional(safeInteger),
    durationFrames: Type.Optional(safeInteger),
    ownerClipId: Type.Optional(stringId),
    sourceStartFrame: Type.Optional(safeInteger),
    sourceDurationFrames: Type.Optional(safeInteger),
  },
  { additionalProperties: false },
);

const textPatchSchema = Type.Object(
  {
    text: Type.Optional(Type.String()),
    style: Type.Optional(Type.Union([Type.Literal("clean"), Type.Literal("boxed")])),
    color: Type.Optional(colorSchema),
    fontSize: Type.Optional(Type.Number({ exclusiveMinimum: 0 })),
    positionX: Type.Optional(Type.Integer({ minimum: 0, maximum: 10000 })),
    positionY: Type.Optional(Type.Integer({ minimum: 0, maximum: 10000 })),
    lineBreaks: Type.Optional(Type.Array(safeInteger)),
    startFrame: Type.Optional(safeInteger),
    durationFrames: Type.Optional(safeInteger),
    sourceStartFrame: Type.Optional(safeInteger),
    sourceDurationFrames: Type.Optional(safeInteger),
  },
  { additionalProperties: false },
);

const editOperationSchema = Type.Union([
  Type.Object(
    {
      op: Type.Literal("set_project"),
      name: Type.Optional(Type.String()),
      aspect: Type.Optional(Type.Union([Type.Literal("16:9"), Type.Literal("9:16"), Type.Literal("1:1")])),
    },
    { additionalProperties: false },
  ),
  Type.Object(
    {
      op: Type.Literal("add_track"),
      id: stringId,
      kind: Type.Union([Type.Literal("video"), Type.Literal("audio"), Type.Literal("text")]),
      name: Type.String(),
      index: safeInteger,
    },
    { additionalProperties: false },
  ),
  Type.Object(
    {
      op: Type.Literal("update_track"),
      trackId: stringId,
      name: Type.Optional(Type.String()),
      muted: Type.Optional(Type.Boolean()),
      locked: Type.Optional(Type.Boolean()),
    },
    { additionalProperties: false },
  ),
  Type.Object(
    { op: Type.Literal("remove_track"), trackId: stringId, deleteItems: Type.Boolean() },
    { additionalProperties: false },
  ),
  Type.Object({ op: Type.Literal("insert_clip"), clip: clipSchema }, { additionalProperties: false }),
  Type.Object(
    { op: Type.Literal("move_clip"), clipId: stringId, trackId: stringId, startFrame: safeInteger },
    { additionalProperties: false },
  ),
  Type.Object(
    {
      op: Type.Literal("trim_clip"),
      clipId: stringId,
      inFrame: safeInteger,
      startFrame: safeInteger,
      durationFrames: positiveSafeInteger,
    },
    { additionalProperties: false },
  ),
  Type.Object(
    { op: Type.Literal("split_clip"), clipId: stringId, frame: safeInteger, rightClipId: stringId },
    { additionalProperties: false },
  ),
  Type.Object(
    { op: Type.Literal("update_clip"), clipId: stringId, patch: clipPatchSchema },
    { additionalProperties: false },
  ),
  Type.Object(
    { op: Type.Literal("remove_clips"), clipIds: Type.Array(stringId, { minItems: 1 }) },
    { additionalProperties: false },
  ),
  Type.Object(
    {
      op: Type.Literal("remove_range"),
      startFrame: safeInteger,
      endFrame: positiveSafeInteger,
      ripple: Type.Boolean(),
    },
    { additionalProperties: false },
  ),
  Type.Object({ op: Type.Literal("add_text"), item: textItemSchema }, { additionalProperties: false }),
  Type.Object(
    { op: Type.Literal("update_text"), textId: stringId, patch: textPatchSchema },
    { additionalProperties: false },
  ),
  Type.Object({ op: Type.Literal("remove_text"), textId: stringId }, { additionalProperties: false }),
  Type.Object(
    {
      op: Type.Literal("add_transition"),
      leftClipId: stringId,
      rightClipId: stringId,
      durationFrames: Type.Integer({ minimum: 2, maximum: Number.MAX_SAFE_INTEGER }),
    },
    { additionalProperties: false },
  ),
  Type.Object({ op: Type.Literal("remove_transition"), transitionId: stringId }, { additionalProperties: false }),
  Type.Object(
    {
      op: Type.Literal("replace_captions"),
      clipId: stringId,
      transcriptId: stringId,
      style: Type.Union([Type.Literal("clean"), Type.Literal("boxed")]),
    },
    { additionalProperties: false },
  ),
]);

type SchemaStatic<T extends TSchema> = Static<T>;
type SchemaExtends<Actual, Expected> = [Actual] extends [Expected] ? true : false;
type Assert<T extends true> = T;
type EditOperationSchemaMatchesGenerated = Assert<SchemaExtends<SchemaStatic<typeof editOperationSchema>, EditOp>>;
function providerActionSchema(strictSchema: TSchema): TObject<any> {
  const branches = (strictSchema as TSchema & { anyOf?: TSchema[] }).anyOf;
  if (branches === undefined || branches.length === 0) {
    throw new Error("Provider action schema must contain at least one action branch.");
  }

  const fields = new Map<string, { schema: TSchema; actions: string[] }>();
  const actionSchemas: TSchema[] = [];
  for (const branch of branches) {
    const properties = (branch as TObject<Record<string, TSchema>>).properties;
    const action = properties?.action as (TSchema & { const?: unknown }) | undefined;
    if (typeof action?.const !== "string") {
      throw new Error("Provider action branch must have a literal action.");
    }
    const actionName = action.const;
    actionSchemas.push(Type.Literal(actionName, { description: `Perform the ${actionName} action.` }));
    for (const [name, schema] of Object.entries(properties ?? {})) {
      if (name === "action") continue;
      const current = fields.get(name);
      if (current === undefined) {
        fields.set(name, { schema, actions: [actionName] });
      } else if (!current.actions.includes(actionName)) {
        current.actions.push(actionName);
      }
    }
  }

  const properties: Record<string, TSchema> = {
    action: Type.Union(actionSchemas as [TSchema, ...TSchema[]], {
      description: "Action to perform. Supply only fields supported by the selected action.",
    }),
  };
  for (const [name, field] of fields) {
    const description = (field.schema as TSchema & { description?: string }).description
      ?? `Field used by the ${field.actions.join(", ")} action${field.actions.length === 1 ? "" : "s"}.`;
    properties[name] = Object.assign(Type.Optional(field.schema), { description });
  }
  return Type.Object(properties, { additionalProperties: false }) as TObject<any>;
}

function validateActionInput<T extends TSchema>(
  toolName: string,
  value: unknown,
  strictSchema: T,
): Static<T> {
  if (!Value.Check(strictSchema, value)) {
    throw new BridgeProtocolError("SCHEMA_UNSUPPORTED", `Invalid ${toolName} parameters.`);
  }
  return value as Static<T>;
}


const projectActionSchema = Type.Union([
  Type.Object({ action: Type.Literal("status") }, { additionalProperties: false }),
  Type.Object({ action: Type.Literal("snapshot") }, { additionalProperties: false }),
  Type.Object(
    {
      action: Type.Literal("create"),
      name: stringId,
      aspect: Type.Optional(Type.Union([Type.Literal("16:9"), Type.Literal("9:16"), Type.Literal("1:1")])),
      fpsNum: Type.Optional(safeInteger),
      fpsDen: Type.Optional(safeInteger),
    },
    { additionalProperties: false },
  ),
  Type.Object({ action: Type.Literal("open") }, { additionalProperties: false }),
  Type.Object({ action: Type.Literal("save") }, { additionalProperties: false }),
]);
const projectSchema = providerActionSchema(projectActionSchema);

const mediaActionSchema = Type.Union([
  Type.Object({ action: Type.Literal("list") }, { additionalProperties: false }),
  Type.Object({ action: Type.Literal("inspect"), assetId: stringId }, { additionalProperties: false }),
  Type.Object(
    { action: Type.Literal("import"), paths: Type.Optional(Type.Array(stringId, { minItems: 1 })) },
    { additionalProperties: false },
  ),
  Type.Object({ action: Type.Literal("relink"), assetId: stringId }, { additionalProperties: false }),
  Type.Object({ action: Type.Literal("remove"), assetId: stringId }, { additionalProperties: false }),
  Type.Object(
    { action: Type.Literal("thumbnail"), assetId: stringId, frame: Type.Optional(safeInteger) },
    { additionalProperties: false },
  ),
  Type.Object({ action: Type.Literal("waveform"), assetId: stringId }, { additionalProperties: false }),
]);
const mediaSchema = providerActionSchema(mediaActionSchema);

const timelineRangeSchema = Type.Object(
  { startFrame: safeInteger, endFrame: positiveSafeInteger },
  { additionalProperties: false },
);
const timelineSelectionSchema = Type.Object(
  {
    clipIds: Type.Array(stringId),
    textIds: Type.Array(stringId),
    playheadFrame: safeInteger,
    range: Type.Optional(timelineRangeSchema),
  },
  { additionalProperties: false },
);
const timelineActionSchema = Type.Union([
  Type.Object({ action: Type.Literal("snapshot") }, { additionalProperties: false }),
  Type.Object({ action: Type.Literal("selection"), selection: timelineSelectionSchema }, { additionalProperties: false }),
]);
const timelineSchema = providerActionSchema(timelineActionSchema);


type TimelineSchemaMatchesGenerated = Assert<SchemaExtends<SchemaStatic<typeof timelineSelectionSchema>, TimelineSelection>>;


const editProjectSchema = Type.Object(
  {
    label: stringId,
    expectedRevision: safeInteger,
    operations: Type.Array(editOperationSchema, { minItems: 1 }),
    dryRun: Type.Optional(Type.Boolean()),
  },
  { additionalProperties: false },
);

const historySchema = Type.Object(
  {
    action: Type.Union([Type.Literal("undo"), Type.Literal("redo")]),
    expectedRevision: safeInteger,
    expectedTransactionId: Type.Optional(stringId),
  },
  { additionalProperties: false },
);

const sampleFramesSchema = Type.Object(
  {
    assetId: stringId,
    startFrame: safeInteger,
    endFrame: positiveSafeInteger,
    count: Type.Optional(Type.Integer({ minimum: 1, maximum: 32 })),
  },
  { additionalProperties: false },
);

const transcriptActionSchema = Type.Union([
  Type.Object({ action: Type.Literal("model_status") }, { additionalProperties: false }),
  Type.Object(
    {
      action: Type.Literal("transcribe"),
      assetId: stringId,
      startFrame: Type.Optional(safeInteger),
      endFrame: Type.Optional(safeInteger),
      // False explicitly declines downloading/activating the local model.
      modelConsent: Type.Optional(Type.Boolean()),
    },
    { additionalProperties: false },
  ),
  Type.Object({ action: Type.Literal("read"), transcriptId: stringId }, { additionalProperties: false }),
  Type.Object(
    { action: Type.Literal("search"), query: stringId, assetId: Type.Optional(stringId) },
    { additionalProperties: false },
  ),
  Type.Object(
    {
      action: Type.Literal("import_srt"),
      playheadFrame: safeInteger,
      style: Type.Optional(Type.Union([Type.Literal("clean"), Type.Literal("boxed")])),
    },
    { additionalProperties: false },
  ),
  Type.Object({ action: Type.Literal("export_srt") }, { additionalProperties: false }),
]);
const transcriptSchema = providerActionSchema(transcriptActionSchema);

const analyzeActionSchema = Type.Union([
  Type.Object(
    {
      action: Type.Literal("scenes"),
      assetId: stringId,
      threshold: Type.Optional(Type.Number({ minimum: 0, maximum: 1 })),
    },
    { additionalProperties: false },
  ),
  Type.Object({ action: Type.Literal("silence"), assetId: stringId }, { additionalProperties: false }),
]);
const analyzeSchema = providerActionSchema(analyzeActionSchema);

const previewActionSchema = Type.Union([
  Type.Object({ action: Type.Literal("plan"), revision: safeInteger }, { additionalProperties: false }),
  Type.Object({ action: Type.Literal("render_frame"), revision: safeInteger, frame: safeInteger }, { additionalProperties: false }),
  Type.Object(
    {
      action: Type.Literal("render_audio_window"),
      planHash: stringId,
      startSample: safeInteger,
      sampleCount: positiveSafeInteger,
    },
    { additionalProperties: false },
  ),
  Type.Object({ action: Type.Literal("seek"), frame: safeInteger }, { additionalProperties: false }),
  Type.Object({ action: Type.Literal("play") }, { additionalProperties: false }),
  Type.Object({ action: Type.Literal("pause") }, { additionalProperties: false }),
  Type.Object(
    {
      action: Type.Literal("inspect"),
      revision: safeInteger,
      frames: Type.Array(safeInteger, { minItems: 1, maxItems: 32 }),
    },
    { additionalProperties: false },
  ),
]);
const previewSchema = providerActionSchema(previewActionSchema);

const graphicSchema = Type.Object(
  {
    name: stringId,
    svg: Type.String({ minLength: 1, maxLength: 262144 }),
    width: Type.Integer({ minimum: 1, maximum: 4096 }),
    height: Type.Integer({ minimum: 1, maximum: 4096 }),
  },
  { additionalProperties: false },
);

const exportSchema = Type.Object(
  {
    action: Type.Literal("start"),
    revision: safeInteger,
    resolution: Type.Union([Type.Literal(720), Type.Literal(1080)]),
    srt: Type.Boolean(),
  },
  { additionalProperties: false },
);
const jobsActionSchema = Type.Union([
  Type.Object({ action: Type.Literal("list") }, { additionalProperties: false }),
  Type.Object({ action: Type.Literal("get"), jobId: stringId }, { additionalProperties: false }),
  Type.Object({ action: Type.Literal("cancel"), jobId: stringId }, { additionalProperties: false }),
]);
const jobsSchema = providerActionSchema(jobsActionSchema);

const systemReadSchema = Type.Object(
  {
    path: stringId,
    offset: Type.Optional(safeInteger),
    limit: Type.Optional(Type.Integer({ minimum: 1, maximum: 1048576 })),
    operationId: Type.Optional(stringId),
  },
  { additionalProperties: false },
);
const systemWriteSchema = Type.Object(
  {
    path: stringId,
    content: Type.String({ maxLength: 1048576 }),
    overwrite: Type.Optional(Type.Boolean()),
    operationId: Type.Optional(stringId),
  },
  { additionalProperties: false },
);
const systemExecuteSchema = Type.Object(
  {
    executable: stringId,
    argv: Type.Array(Type.String(), { maxItems: 256 }),
    cwd: stringId,
    environment: Type.Optional(Type.Record(Type.String(), Type.String())),
    timeoutMs: Type.Optional(Type.Integer({ minimum: 1, maximum: 600000 })),
    operationId: Type.Optional(stringId),
  },
  { additionalProperties: false },
);
const systemHttpSchema = Type.Object(
  {
    method: Type.Union([
      Type.Literal("GET"),
      Type.Literal("POST"),
      Type.Literal("PUT"),
      Type.Literal("PATCH"),
      Type.Literal("DELETE"),
    ]),
    url: stringId,
    headers: Type.Optional(Type.Record(Type.String(), Type.String())),
    body: Type.String({ maxLength: 1048576 }),
    timeoutMs: Type.Optional(Type.Integer({ minimum: 1, maximum: 600000 })),
    operationId: Type.Optional(stringId),
  },
  { additionalProperties: false },
);

function abortIfNeeded(signal: AbortSignal | undefined): void {
  if (signal?.aborted) {
    throw new BridgeProtocolError("JOB_CANCELLED", "The tool run was cancelled.");
  }
}


function redact(value: unknown, seen = new Set<object>()): unknown {
  if (value === null || typeof value !== "object") return value;
  if (seen.has(value)) return "[circular]";
  seen.add(value);
  if (Array.isArray(value)) {
    const array = value.map((item) => redact(item, seen));
    seen.delete(value);
    return array;
  }
  const object = value as Record<string, unknown>;
  const redacted: Record<string, unknown> = {};
  for (const [key, item] of Object.entries(object)) {
    if (/token|secret|password|api[_-]?key|authorization|cookie|refresh|access/i.test(key)) {
      redacted[key] = "[redacted]";
    } else if (key === "data" && typeof object.mimeType === "string") {
      redacted[key] = "[image omitted from text]";
    } else {
      redacted[key] = redact(item, seen);
    }
  }
  seen.delete(value);
  return redacted;
}

function textFor(value: unknown): string {
  if (value === undefined) return "Completed.";
  const safe = redact(value);
  if (typeof safe === "string") return safe;
  try {
    return JSON.stringify(safe) ?? "Completed.";
  } catch {
    return "The native operation returned a result that could not be displayed.";
  }
}

type ImageBlock = { type: "image"; data: string; mimeType: "image/png" | "image/jpeg" };
type ImageArtifactReference = { artifactId: string };
const MAX_ENCODED_IMAGE_BYTES = 4 * 1024 * 1024;
const MAX_RAW_IMAGE_BYTES = 3 * 1024 * 1024;
const MAX_IMAGE_EDGE = 1_280;

function collectImageArtifactReferences(
  value: unknown,
  output: Map<string, ImageArtifactReference>,
  seen: Set<object>,
): void {
  if (value === null || typeof value !== "object") return;
  if (seen.has(value)) return;
  seen.add(value);
  if (Array.isArray(value)) {
    for (const item of value) collectImageArtifactReferences(item, output, seen);
    seen.delete(value);
    return;
  }
  const object = value as Record<string, unknown>;
  if (typeof object.artifactId === "string" && object.artifactId.length > 0) {
    output.set(object.artifactId, { artifactId: object.artifactId });
  }
  for (const item of Object.values(object)) collectImageArtifactReferences(item, output, seen);
  seen.delete(value);
}

function imageArtifactReferences(value: unknown): ImageArtifactReference[] {
  const references = new Map<string, ImageArtifactReference>();
  collectImageArtifactReferences(value, references, new Set<object>());
  return [...references.values()];
}

function imageMimeType(bytes: Uint8Array): "image/png" | "image/jpeg" | undefined {
  if (
    bytes.length >= 8 &&
    bytes[0] === 0x89 &&
    bytes[1] === 0x50 &&
    bytes[2] === 0x4e &&
    bytes[3] === 0x47 &&
    bytes[4] === 0x0d &&
    bytes[5] === 0x0a &&
    bytes[6] === 0x1a &&
    bytes[7] === 0x0a
  ) {
    return "image/png";
  }
  if (bytes.length >= 2 && bytes[0] === 0xff && bytes[1] === 0xd8) return "image/jpeg";
  return undefined;
}
async function imageBlocks(
  value: unknown,
  client: EditorClient,
  signal: AbortSignal | undefined,
): Promise<ImageBlock[]> {
  const blocks: ImageBlock[] = [];
  let encodedBytes = 0;
  const references = imageArtifactReferences(value);
  for (let index = 0; index < references.length; index += 1) {
    abortIfNeeded(signal);
    const reference = references[index];
    const remainingReferences = references.length - index;
    const remainingEncodedBytes = MAX_ENCODED_IMAGE_BYTES - encodedBytes;
    const perImageEncodedBudget = Math.floor(remainingEncodedBytes / remainingReferences);
    const perImageRawBudget = Math.min(
      MAX_RAW_IMAGE_BYTES,
      Math.floor(perImageEncodedBudget / 4) * 3,
    );
    if (perImageRawBudget < 3) {
      throw new BridgeProtocolError(
        "MEDIA_UNSUPPORTED",
        "The requested evidence exceeds the 4 MiB assistant image limit; request fewer frames or a narrower range.",
      );
    }
    const bytes = await client.fetchArtifact(
      reference.artifactId,
      {
        maxEdge: MAX_IMAGE_EDGE,
        maxBytes: perImageRawBudget,
      } as ArtifactRange,
    );
    abortIfNeeded(signal);
    const mimeType = imageMimeType(bytes);
    if (mimeType === undefined) {
      throw new BridgeProtocolError(
        "MEDIA_UNSUPPORTED",
        `Evidence artifact ${reference.artifactId} is not a PNG or JPEG image.`,
      );
    }
    const data = Buffer.from(bytes).toString("base64");
    if (
      data.length === 0 ||
      data.length > perImageEncodedBudget ||
      data.length > MAX_ENCODED_IMAGE_BYTES - encodedBytes
    ) {
      throw new BridgeProtocolError(
        "MEDIA_UNSUPPORTED",
        "The requested evidence exceeds the 4 MiB assistant image limit; request fewer frames or a narrower range.",
      );
    }
    blocks.push({ type: "image", data, mimeType });
    encodedBytes += data.length;
  }
  return blocks;
}

async function result(
  value: unknown,
  options: {
    includeImages?: boolean;
    evidence?: boolean;
    client?: EditorClient;
    signal?: AbortSignal;
  } = {},
): Promise<AgentToolResult<unknown>> {
  const content: Array<{ type: "text"; text: string } | ImageBlock> = [
    { type: "text", text: textFor(value) },
  ];
  if (options.includeImages !== false && options.evidence === true) {
    const references = imageArtifactReferences(value);
    if (references.length > 0 && options.client === undefined) {
      throw new BridgeProtocolError(
        "SCHEMA_UNSUPPORTED",
        "The evidence result has image artifacts but no managed artifact transport.",
      );
    }
    if (options.client !== undefined) {
      content.push(...await imageBlocks(value, options.client, options.signal));
    }
  }
  return { content, details: redact(value) };
}

function errorResult(error: unknown): never {
  if (error instanceof BridgeProtocolError) throw error;
  if (error instanceof EditorClientError) {
    throw new BridgeProtocolError(error.code, error.message);
  }
  throw new BridgeProtocolError("IO_ERROR", "The editor tool failed.");
}

async function callWithSignal<T>(signal: AbortSignal | undefined, action: () => Promise<T>): Promise<T> {
  abortIfNeeded(signal);
  const value = await action();
  abortIfNeeded(signal);
  return value;
}

export function createEditorTools(run: ToolRunContext): ToolDefinition[] {
  const clientFor = (toolCallId: string, signal: AbortSignal | undefined): EditorClient =>
    run.port.clientFor(toolCallId, signal, run.nativeRunId());
  const callEditor = async <Request extends EditorRequest>(
    toolCallId: string,
    request: Request,
    signal: AbortSignal | undefined,
  ): Promise<EditorReply> => {
    return callWithSignal(signal, () => clientFor(toolCallId, signal).call(request));
  };
  const tools: ToolDefinition[] = [
    defineTool({
      name: "project",
      label: "Project",
      description: "Inspect project state or request a native project create/open/save action.",
      promptSnippet: "Inspect or manage the current Cutterhoochee project.",
      parameters: projectSchema,
      executionMode: "sequential",
      async execute(toolCallId, params, signal) {
        try {
          const input = validateActionInput("project", params, projectActionSchema);
          const client = clientFor(toolCallId, signal);
          switch (input.action) {
            case "status": return result(await callWithSignal(signal, () => client.projectStatus()));
            case "snapshot": return result(await callWithSignal(signal, () => client.projectSnapshot()));
            case "create": {
              const reply = await callEditor(toolCallId, {
                method: "project_create",
                params: {
                  name: input.name,
                  ...(input.aspect === undefined ? {} : { aspect: input.aspect }),
                  ...(input.fpsNum === undefined ? {} : { fpsNum: input.fpsNum }),
                  ...(input.fpsDen === undefined ? {} : { fpsDen: input.fpsDen }),
                },
              }, signal);
              return result(reply);
            }
            case "open": return result(await callEditor(toolCallId, { method: "project_open", params: {} }, signal));
            case "save": return result(await callEditor(toolCallId, { method: "project_save", params: {} }, signal));
          }
        } catch (error) {
          return errorResult(error);
        }
      },
    }),
    defineTool({
      name: "media",
      label: "Media",
      description: "List, inspect, import, relink, remove, or prepare managed media assets through native grants.",
      parameters: mediaSchema,
      executionMode: "sequential",
      async execute(toolCallId, params, signal) {
        try {
          const input = validateActionInput("media", params, mediaActionSchema);
          const reply = await callWithSignal(signal, () => clientFor(toolCallId, signal).callMedia(input));
          return result(reply.data);
        } catch (error) {
          return errorResult(error);
        }
      },
    }),
    defineTool({
      name: "timeline",
      label: "Timeline",
      description: "Inspect the current timeline and selection.",
      parameters: timelineSchema,
      executionMode: "sequential",
      async execute(toolCallId, params, signal) {
        try {
          const input = validateActionInput("timeline", params, timelineActionSchema);
          const client = clientFor(toolCallId, signal);
          if (input.action === "snapshot") return result(await callWithSignal(signal, () => client.timelineSnapshot()));
          return result(await callWithSignal(signal, () => client.setTimelineSelection(input.selection)));
        } catch (error) {
          return errorResult(error);
        }
      },
    }),
    defineTool({
      name: "edit_project",
      label: "Edit project",
      description: "Apply one validated, transactional batch of the complete Cutterhoochee EditOp union.",
      parameters: editProjectSchema,
      executionMode: "sequential",
      async execute(toolCallId, params, signal, onUpdate) {
        try {
          onUpdate?.({ content: [{ type: "text", text: "Validating the requested edit…" }], details: undefined });
          const reply = await callEditor(toolCallId, {
            method: "edit_project",
            params: {
              transactionId: toolCallId,
              expectedRevision: params.expectedRevision,
              label: params.label,
              operations: params.operations,
              ...(params.dryRun === undefined ? {} : { dryRun: params.dryRun }),
            },
          }, signal);
          return result(reply);
        } catch (error) {
          return errorResult(error);
        }
      },
    }),
    defineTool({
      name: "history",
      label: "History",
      description: "Undo or redo the global chronological project history with an expected top transaction.",
      parameters: historySchema,
      executionMode: "sequential",
      async execute(toolCallId, params, signal) {
        try {
          return result(await callEditor(toolCallId, { method: "project_history", params }, signal));
        } catch (error) {
          return errorResult(error);
        }
      },
    }),
    defineTool({
      name: "sample_frames",
      label: "Sample frames",
      description: "Return backend-labeled source evidence for an imported asset; image blocks require an explicit evidence grant.",
      parameters: sampleFramesSchema,
      executionMode: "parallel",
      async execute(toolCallId, params, signal) {
        try {
          const client = clientFor(toolCallId, signal);
          const reply = await callWithSignal(signal, () => client.callSampleFrames(params));
          return result(reply.data, { evidence: true, client, signal });
        } catch (error) {
          return errorResult(error);
        }
      },
    }),
    defineTool({
      name: "transcript",
      label: "Transcript",
      description: "Transcribe, search, import, read, or export real local transcript evidence.",
      parameters: transcriptSchema,
      executionMode: "sequential",
      async execute(toolCallId, params, signal) {
        try {
          const input = validateActionInput("transcript", params, transcriptActionSchema);
          const nativeParams = input.action === "transcribe"
            ? { ...input, modelConsent: input.modelConsent ?? false }
            : input;
          const client = clientFor(toolCallId, signal);
          const reply = await callWithSignal(signal, () => client.callTranscript(nativeParams));
          return result(reply.data, { evidence: true, client, signal });
        } catch (error) {
          return errorResult(error);
        }
      },
    }),
    defineTool({
      name: "analyze_media",
      label: "Analyze media",
      description: "Run native scene-boundary or silence analysis on imported media.",
      parameters: analyzeSchema,
      executionMode: "parallel",
      async execute(toolCallId, params, signal) {
        try {
          const input = validateActionInput("analyze_media", params, analyzeActionSchema);
          const client = clientFor(toolCallId, signal);
          const reply = await callWithSignal(signal, () => client.callAnalyzeMedia(input));
          return result(reply.data, { evidence: true, client, signal });
        } catch (error) {
          return errorResult(error);
        }
      },
    }),
    defineTool({
      name: "preview",
      label: "Preview",
      description: "Build an authoritative render plan, render canonical frames/audio, or control native preview transport.",
      parameters: previewSchema,
      executionMode: "sequential",
      async execute(toolCallId, params, signal) {
        try {
          const input = validateActionInput("preview", params, previewActionSchema);
          const client = clientFor(toolCallId, signal);
          const reply = await callWithSignal(signal, () => client.callPreview(input));
          return result(reply.data, {
            evidence: input.action === "render_frame" || input.action === "inspect",
            client,
            signal,
          });
        } catch (error) {
          return errorResult(error);
        }
      },
    }),
    defineTool({
      name: "create_graphic",
      label: "Create graphic",
      description: "Create a safe local SVG graphic through native validation and rasterization.",
      parameters: graphicSchema,
      executionMode: "sequential",
      async execute(toolCallId, params, signal) {
        try {
          const reply = await callWithSignal(signal, () => clientFor(toolCallId, signal).callCreateGraphic(params));
          return result(reply.data);
        } catch (error) {
          return errorResult(error);
        }
      },
    }),
    defineTool({
      name: "export_video",
      label: "Export video",
      description: "Start an immutable, approval-gated native export for an exact revision.",
      parameters: exportSchema,
      executionMode: "sequential",
      async execute(toolCallId, params, signal) {
        try {
          const reply = await callWithSignal(signal, () => clientFor(toolCallId, signal).callExport(params));
          return result(reply.data);
        } catch (error) {
          return errorResult(error);
        }
      },
    }),
    defineTool({
      name: "jobs",
      label: "Jobs",
      description: "Inspect or cancel owned native media and export jobs.",
      parameters: jobsSchema,
      executionMode: "sequential",
      async execute(toolCallId, params, signal) {
        try {
          const input = validateActionInput("jobs", params, jobsActionSchema);
          const reply = await callWithSignal(signal, () => clientFor(toolCallId, signal).callJobs(input));
          return result(reply.data);
        } catch (error) {
          return errorResult(error);
        }
      },
    }),
    defineTool({
      name: "system_read",
      label: "Read approved file",
      description: "Read a bounded, approval-gated external file through native permissions.",
      parameters: systemReadSchema,
      executionMode: "sequential",
      async execute(toolCallId, params, signal) {
        try {
          const request = {
            action: "system_read",
            params: {
              path: params.path,
              offset: params.offset ?? 0,
              length: params.limit ?? 1048576,
              operationId: params.operationId ?? null,
            },
          } satisfies Extract<EditorRequest, { method: "permissions" }>["params"];
          const reply = await callWithSignal(signal, () => clientFor(toolCallId, signal).callPermissions(request));
          return result(reply.data, { includeImages: false });
        } catch (error) {
          return errorResult(error);
        }
      },
    }),
    defineTool({
      name: "system_write",
      label: "Write approved file",
      description: "Write an approved external file atomically through native permissions.",
      parameters: systemWriteSchema,
      executionMode: "sequential",
      async execute(toolCallId, params, signal) {
        try {
          const request = {
            action: "system_write",
            params: {
              path: params.path,
              data: Array.from(new TextEncoder().encode(params.content)),
              overwrite: params.overwrite ?? false,
              operationId: params.operationId ?? null,
            },
          } satisfies Extract<EditorRequest, { method: "permissions" }>["params"];
          const reply = await callWithSignal(signal, () => clientFor(toolCallId, signal).callPermissions(request));
          return result(reply.data, { includeImages: false });
        } catch (error) {
          return errorResult(error);
        }
      },
    }),
    defineTool({
      name: "system_execute",
      label: "Execute approved command",
      description: "Run an explicitly approved executable and argv with user-level authority; cwd is not a sandbox.",
      parameters: systemExecuteSchema,
      executionMode: "sequential",
      async execute(toolCallId, params, signal) {
        try {
          const request = {
            action: "system_execute",
            params: {
              executable: params.executable,
              arguments: params.argv,
              cwd: params.cwd,
              environment: params.environment ?? {},
              timeoutMs: params.timeoutMs ?? DEFAULT_SYSTEM_TIMEOUT_MS,
              operationId: params.operationId ?? null,
            },
          } satisfies Extract<EditorRequest, { method: "permissions" }>["params"];
          const reply = await callWithSignal(signal, () => clientFor(toolCallId, signal).callPermissions(request));
          return result(reply.data, { includeImages: false });
        } catch (error) {
          return errorResult(error);
        }
      },
    }),
    defineTool({
      name: "system_http",
      label: "HTTP request",
      description: "Make an explicitly approved bounded HTTP request through native policy and redirect checks.",
      parameters: systemHttpSchema,
      executionMode: "sequential",
      async execute(toolCallId, params, signal) {
        try {
          const request = {
            action: "system_http",
            params: {
              url: params.url,
              method: params.method,
              headers: params.headers ?? {},
              body: Array.from(new TextEncoder().encode(params.body)),
              timeoutMs: params.timeoutMs ?? DEFAULT_SYSTEM_TIMEOUT_MS,
              operationId: params.operationId ?? null,
            },
          } satisfies Extract<EditorRequest, { method: "permissions" }>["params"];
          const reply = await callWithSignal(signal, () => clientFor(toolCallId, signal).callPermissions(request));
          return result(reply.data, { includeImages: false });
        } catch (error) {
          return errorResult(error);
        }
      },
    }),
  ];
  return tools;
}

export const EDITOR_TOOL_NAMES = [
  "project",
  "media",
  "timeline",
  "edit_project",
  "history",
  "sample_frames",
  "transcript",
  "analyze_media",
  "preview",
  "create_graphic",
  "export_video",
  "jobs",
  "system_read",
  "system_write",
  "system_execute",
  "system_http",
] as const;

export type { AgentToolResult, AgentToolUpdateCallback, ExtensionContext, ToolDefinition };
