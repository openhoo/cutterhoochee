import { access, mkdir, readFile, realpath, writeFile } from "node:fs/promises";
import { Buffer } from "node:buffer";
import { isAbsolute, join, relative, resolve } from "node:path";

import type { EditorReply, EditorRequest } from "@cutterhoochee/shared/generated";
import {
  EditorClient,
  type ArtifactRange,
  type EditorTransport,
} from "@cutterhoochee/shared/client";
import { isEditorReply } from "@cutterhoochee/shared/protocol";
import type { Api, Model } from "@earendil-works/pi-ai";
import {
  DefaultResourceLoader,
  type AgentSession,
  createAgentSession,
  ModelRuntime,
  SessionManager,
  SettingsManager,
} from "@earendil-works/pi-coding-agent";
import type { AgentSessionEvent } from "@earendil-works/pi-coding-agent";
import { BridgeCredentialStore } from "./credentials.js";
import { BridgeProtocolError, type BridgeResponseMessage, type NdjsonBridge } from "./bridge.js";
import { EDITOR_SYSTEM_PROMPT, EDITOR_SYSTEM_PROMPT_APPEND } from "./system-prompt.js";
import { createEditorTools, EDITOR_TOOL_NAMES, type AgentEditorPort, type ToolRunContext } from "./tools.js";
import {
  ProvidersRuntime,
  restrictProviderMatrix,
  type ProvidersAction,
  type ProvidersReply,
} from "./providers.js";

export type AssistantEventSink = {
  emit(event: string, data?: unknown, runId?: string): void;
};
export type PiRuntimeOptions = {
  bridge: NdjsonBridge;
  agentDir: string;
  cacheDir: string;
  sessionDir: string;
  projectId: string | null;
  workspaceId: string | null;
  generation: number;
  events?: AssistantEventSink;
};

export type SessionHistoryMessage = {
  role: "user" | "assistant" | "tool";
  text: string;
  timestamp?: number;
  toolName?: string;
  isError?: boolean;
};

type SessionMessage = AgentSession["state"]["messages"][number];

export type AssistantStatus = {
  active: boolean;
  configured: boolean;
  providerId?: string;
  modelId?: string;
  sessionId?: string;
  usage?: {
    input: number;
    output: number;
    cacheRead: number;
    cacheWrite: number;
    total: number;
    cost?: number;
  };
};
type SessionRecipient = {
  providerId: string;
  accountId: string;
};

function safeProjectComponent(projectId: string | null): string {
  if (projectId === null || projectId.length === 0) return "unassigned";
  if (!/^[A-Za-z0-9._-]{1,128}$/.test(projectId)) {
    throw new BridgeProtocolError("INVALID_ARGUMENT", "The project context is invalid.");
  }
  return projectId;
}

function safeWorkspaceComponent(workspaceId: string | null): string {
  if (workspaceId === null || workspaceId.length === 0) return "unassigned";
  if (!/^[A-Za-z0-9._-]{1,128}$/.test(workspaceId)) {
    throw new BridgeProtocolError("INVALID_ARGUMENT", "The workspace context is invalid.");
  }
  return workspaceId;
}
function protocolError(response: BridgeResponseMessage): never {
  if (response.ok) throw new BridgeProtocolError("SCHEMA_UNSUPPORTED", "Unexpected successful response error.");
  throw new BridgeProtocolError(response.error.code, response.error.message);
}

function textFromMessage(message: SessionMessage): string {
  if (!("content" in message)) return "";
  const content = message.content;
  if (typeof content === "string") return content;
  if (!Array.isArray(content)) return "";
  let text = "";
  for (const part of content) {
    if (typeof part === "object" && part !== null && "type" in part && part.type === "text" && "text" in part && typeof part.text === "string") {
      text += part.text;
    }
  }
  return text;
}


function sanitizeEventData(value: unknown): unknown {
  if (value === null || typeof value !== "object") return value;
  if (Array.isArray(value)) return value.map(sanitizeEventData);
  const object = value as Record<string, unknown>;
  const result: Record<string, unknown> = {};
  for (const [key, item] of Object.entries(object)) {
    if (/token|secret|password|api[_-]?key|authorization|cookie|thinking|signature/i.test(key)) continue;
    if (key === "args" || key === "input" || key === "command" || key === "argv") continue;
    result[key] = sanitizeEventData(item);
  }
  return result;
}
const MAX_ENCODED_EVIDENCE_BYTES = 4 * 1024 * 1024;
type EvidenceArtifactRange = ArtifactRange & {
  maxEdge?: number;
  maxBytes?: number;
};
function decodeNativeBase64(value: unknown): Uint8Array | undefined {
  if (
    typeof value !== "string" ||
    value.length % 4 !== 0 ||
    !/^(?:[A-Za-z0-9+/]{4})*(?:[A-Za-z0-9+/]{2}==|[A-Za-z0-9+/]{3}=)?$/.test(value)
  ) {
    return undefined;
  }
  const decoded = Buffer.from(value, "base64");
  if (decoded.toString("base64") !== value) return undefined;
  return Uint8Array.from(decoded);
}


export class BridgeEditorPort implements AgentEditorPort {
  constructor(private readonly bridge: NdjsonBridge) {}

  clientFor(toolCallId: string, signal?: AbortSignal, nativeRunId?: string): EditorClient {
    const transport: EditorTransport = {
      call: async (request) => this.callEditor(request, { signal, runId: nativeRunId }),
      readArtifact: async (artifactId, range, context) =>
        this.readEvidenceArtifact(
          artifactId,
          range,
          context === undefined ? this.bridge.projectId : context.projectId,
          context === undefined ? this.bridge.generation : context.generation,
          context?.runId ?? nativeRunId,
          signal,
        ),
    };
    return new EditorClient(
      transport,
      {
        projectId: this.bridge.projectId,
        generation: this.bridge.generation,
        ...(nativeRunId === undefined ? {} : { runId: nativeRunId }),
        transactionIdFactory: () => toolCallId,
      },
    );
  }

  async callEditor(
    request: EditorRequest,
    options: { signal?: AbortSignal; runId?: string } = {},
  ): Promise<EditorReply> {
    const response = await this.call(request.method, request.params, options);
    if (!isEditorReply(response)) {
      throw new BridgeProtocolError("SCHEMA_UNSUPPORTED", "The native editor reply is invalid.");
    }
    return response;
  }

  private async call(method: string, params: unknown, options: { signal?: AbortSignal; runId?: string }): Promise<unknown> {
    const pending = this.bridge.request(method, params, {
      projectId: this.bridge.projectId,
      generation: this.bridge.generation,
      ...(options.runId === undefined ? {} : { runId: options.runId }),
    });
    const response = options.signal === undefined
      ? await pending
      : await new Promise<BridgeResponseMessage>((resolve, reject) => {
          if (options.signal?.aborted) {
            reject(new BridgeProtocolError("JOB_CANCELLED", "The native editor operation was cancelled."));
            return;
          }
          const onAbort = () => reject(new BridgeProtocolError("JOB_CANCELLED", "The native editor operation was cancelled."));
          options.signal?.addEventListener("abort", onAbort, { once: true });
          pending.then(
            (value) => { options.signal?.removeEventListener("abort", onAbort); resolve(value); },
            (error: unknown) => { options.signal?.removeEventListener("abort", onAbort); reject(error); },
          );
        });
    if (!response.ok) protocolError(response);
    if (options.runId !== undefined && response.runId !== options.runId) {
      throw new BridgeProtocolError("STALE_SESSION", "The response belongs to another assistant run.");
    }
    return response.data;
  }

  private async readEvidenceArtifact(
    artifactId: string,
    range: ArtifactRange | undefined,
    projectId: string | null,
    generation: number,
    runId: string | undefined,
    signal: AbortSignal | undefined,
  ): Promise<Uint8Array> {
    if (!/^[A-Za-z0-9._-]{1,256}$/.test(artifactId)) {
      throw new BridgeProtocolError("INVALID_ARGUMENT", "The evidence artifact ID is invalid.");
    }
    if (!Number.isSafeInteger(generation) || generation < 0 || generation !== this.bridge.generation || projectId !== this.bridge.projectId) {
      throw new BridgeProtocolError("STALE_SESSION", "The evidence artifact context is retired.");
    }
    const evidenceRange = range as EvidenceArtifactRange | undefined;
    const hasSizingOptions = evidenceRange?.maxEdge !== undefined || evidenceRange?.maxBytes !== undefined;
    if (
      hasSizingOptions &&
      (evidenceRange?.offset !== undefined || evidenceRange?.length !== undefined)
    ) {
      throw new BridgeProtocolError("INVALID_ARGUMENT", "Image derivative sizing cannot be combined with an artifact range.");
    }
    if (evidenceRange?.offset !== undefined && (!Number.isSafeInteger(evidenceRange.offset) || evidenceRange.offset < 0)) {
      throw new BridgeProtocolError("INVALID_ARGUMENT", "The evidence artifact offset is invalid.");
    }
    if (evidenceRange?.length !== undefined && (!Number.isSafeInteger(evidenceRange.length) || evidenceRange.length < 0)) {
      throw new BridgeProtocolError("INVALID_ARGUMENT", "The evidence artifact length is invalid.");
    }
    const maxRawBytes = 3 * 1024 * 1024;
    const maxEdge = evidenceRange?.maxEdge ?? 1_280;
    const maxBytes = evidenceRange?.maxBytes ?? maxRawBytes;
    if (
      !Number.isSafeInteger(maxEdge) ||
      maxEdge < 1 ||
      maxEdge > 1_280 ||
      !Number.isSafeInteger(maxBytes) ||
      maxBytes < 1 ||
      maxBytes > maxRawBytes
    ) {
      throw new BridgeProtocolError("INVALID_ARGUMENT", "The evidence image sizing bounds are invalid.");
    }
    if (!hasSizingOptions && evidenceRange?.length !== undefined && evidenceRange.length > maxBytes) {
      throw new BridgeProtocolError(
        "MEDIA_UNSUPPORTED",
        "The evidence image is too large for the assistant payload; request fewer frames or a narrower range.",
      );
    }
    const value = await this.call(
      "evidence_image_read",
      {
        artifactId,
        ...(evidenceRange?.offset === undefined ? {} : { offset: evidenceRange.offset }),
        ...(evidenceRange?.length === undefined ? {} : { length: evidenceRange.length }),
        ...(hasSizingOptions ? { maxEdge, maxBytes } : {}),
      },
      { signal, runId },
    );
    if (typeof value !== "object" || value === null || Array.isArray(value)) {
      throw new BridgeProtocolError("SCHEMA_UNSUPPORTED", "The native evidence image response is invalid.");
    }
    const object = value as Record<string, unknown>;
    const expectedOffset = 0;
    const encoded = object.base64;
    if (typeof encoded === "string" && encoded.length > MAX_ENCODED_EVIDENCE_BYTES) {
      throw new BridgeProtocolError(
        "MEDIA_UNSUPPORTED",
        "The evidence image is too large for the assistant payload; request fewer frames or a narrower range.",
      );
    }
    const decoded = decodeNativeBase64(encoded);
    const byteSize = object.byteSize;
    if (
      object.artifactId !== artifactId ||
      (object.mimeType !== "image/png" && object.mimeType !== "image/jpeg") ||
      object.offset !== expectedOffset ||
      typeof byteSize !== "number" ||
      !Number.isSafeInteger(byteSize) ||
      byteSize < 0 ||
      decoded === undefined
    ) {
      throw new BridgeProtocolError("SCHEMA_UNSUPPORTED", "The native evidence image metadata is invalid.");
    }
    if (decoded.length > maxRawBytes || decoded.length !== byteSize) {
      throw new BridgeProtocolError(
        "MEDIA_UNSUPPORTED",
        "The evidence image is too large for the assistant payload; request fewer frames or a narrower range.",
      );
    }
    return decoded;
  }
}
const MAX_RETIRED_RUN_IDS = 64;

export class PiRuntime {
  readonly credentials: BridgeCredentialStore;
  readonly modelRuntime: ModelRuntime;
  providers!: ProvidersRuntime;
  readonly editorPort: BridgeEditorPort;
  private readonly events: AssistantEventSink;
  private readonly options: PiRuntimeOptions;
  private readonly workspaceCwd: string;
  private readonly projectSessionDir: string;
  private readonly sessionMetadataPath: string;
  private session?: AgentSession;
  private sessionRecipient?: SessionRecipient;
  private unsubscribe?: () => void;
  private currentRunId?: string;
  private readonly retiredRunIds = new Set<string>();
  private initialized = false;
  private disposed = false;

  private constructor(
    options: PiRuntimeOptions,
    credentials: BridgeCredentialStore,
    modelRuntime: ModelRuntime,
  ) {
    this.options = options;
    this.credentials = credentials;
    this.modelRuntime = modelRuntime;
    this.editorPort = new BridgeEditorPort(options.bridge);
    this.events = options.events ?? { emit: () => undefined };
    if (options.projectId !== null && options.workspaceId === null) {
      throw new BridgeProtocolError("STALE_SESSION", "The native workspace binding is unavailable for this project.");
    }
    const workspace = safeWorkspaceComponent(options.workspaceId);
    const project = safeProjectComponent(options.projectId);
    this.workspaceCwd = resolve(options.agentDir, "workspaces", workspace, project);
    this.projectSessionDir = resolve(options.sessionDir, workspace, project);
    this.sessionMetadataPath = join(this.projectSessionDir, "session.json");
  }

  static async create(options: PiRuntimeOptions): Promise<PiRuntime> {
    await mkdir(options.agentDir, { recursive: true });
    await mkdir(options.cacheDir, { recursive: true });
    await mkdir(options.sessionDir, { recursive: true });
    const credentials = new BridgeCredentialStore(options.bridge);
    const modelRuntime = await ModelRuntime.create({
      credentials,
      modelsPath: null,
      modelsStorePath: resolve(options.cacheDir, "models.json"),
      allowModelNetwork: false,
      refreshOnCreate: false,
    });
    restrictProviderMatrix(modelRuntime);
    const runtime = new PiRuntime(options, credentials, modelRuntime);
    const providers = new ProvidersRuntime(
      modelRuntime,
      credentials,
      { emit: (event, data, runId) => runtime.emit(event, data, runId) },
      resolve(options.cacheDir, "selection.json"),
      async () => { await runtime.resetSession(); },
    );
    runtime.providers = providers;
    await providers.initialize();
    runtime.initialized = true;
    return runtime;
  }

  async prompt(text: string, signal?: AbortSignal, runId?: string): Promise<void> {
    this.assertInitialized();
    if (this.options.projectId === null) {
      throw new BridgeProtocolError("STALE_SESSION", "Open a project before using the assistant.");
    }
    if (this.options.workspaceId === null) {
      throw new BridgeProtocolError("STALE_SESSION", "The native workspace binding is unavailable for this project.");
    }
    if (runId === undefined || runId.trim().length === 0) {
      throw new BridgeProtocolError("INVALID_ARGUMENT", "The assistant run is missing its native correlation ID.");
    }
    if (this.retiredRunIds.has(runId)) {
      throw new BridgeProtocolError("STALE_SESSION", "The assistant run has already been retired.");
    }
    const selection = this.providers.getSelected();
    if (selection === undefined) {
      throw new BridgeProtocolError("AUTH_REQUIRED", "Connect a provider and select a model before prompting.");
    }
    const recipient = await this.providers.selectedAccount(signal);
    if (
      recipient === undefined ||
      recipient.providerId !== selection.providerId ||
      recipient.accountId.length === 0
    ) {
      throw new BridgeProtocolError("AUTH_REQUIRED", "The selected provider account identity is unavailable.");
    }
    const model = this.modelRuntime.getModel(selection.providerId, selection.modelId);
    if (model === undefined) {
      throw new BridgeProtocolError("AUTH_REQUIRED", "The selected model is no longer available.");
    }
    const configured = await this.modelRuntime.checkAuth(selection.providerId, { signal });
    if (configured === undefined) {
      throw new BridgeProtocolError("AUTH_REQUIRED", "Connect the selected provider before prompting.");
    }
    if (this.retiredRunIds.has(runId)) {
      throw new BridgeProtocolError("STALE_SESSION", "The assistant run has already been retired.");
    }
    this.currentRunId = runId;
    const session = await this.ensureSession(model, {
      providerId: recipient.providerId,
      accountId: recipient.accountId,
    });
    if (signal?.aborted) {
      throw new BridgeProtocolError("JOB_CANCELLED", "The assistant run was cancelled.");
    }
    if (this.retiredRunIds.has(runId)) {
      throw new BridgeProtocolError("STALE_SESSION", "The assistant run has already been retired.");
    }
    const promptPromise = session.prompt(text, { expandPromptTemplates: false });
    if (signal === undefined) {
      await promptPromise;
      return;
    }
    let rejectAbort: ((reason: unknown) => void) | undefined;
    const abortPromise = new Promise<never>((_, reject) => {
      rejectAbort = reject;
    });
    const onAbort = () => {
      void session.abort().catch(() => undefined);
      rejectAbort?.(new BridgeProtocolError("JOB_CANCELLED", "The assistant run was cancelled."));
    };
    signal.addEventListener("abort", onAbort, { once: true });
    try {
      await Promise.race([promptPromise, abortPromise]);
    } finally {
      signal.removeEventListener("abort", onAbort);
    }
  }

  async stop(runId?: string): Promise<void> {
    if (runId !== undefined) {
      this.retireRunId(runId);
      if (this.currentRunId !== runId) return;
    } else if (this.currentRunId !== undefined) {
      this.retireRunId(this.currentRunId);
    }
    this.currentRunId = undefined;
    if (this.session !== undefined) await this.session.abort();
  }

  async newSession(): Promise<void> {
    await this.stop();
    this.disposeSession();
    try { await writeFile(this.sessionMetadataPath, "", { mode: 0o600 }); } catch { /* app-owned session directory may be unavailable during shutdown */ }
  }
  /**
   * Stop any active run before releasing the SDK session and its event
   * subscription. Disposal is idempotent because signal handlers can race
   * with initialization failure or a second shutdown signal.
   */
  async dispose(): Promise<void> {
    if (this.disposed) return;
    this.disposed = true;
    this.initialized = false;
    try {
      await this.stop();
    } finally {
      this.disposeSession();
      this.currentRunId = undefined;
    }
  }

  async restart(): Promise<void> {
    await this.newSession();
    this.emit("assistant_restarted", {});
  }
  async history(): Promise<readonly SessionHistoryMessage[]> {
    this.assertInitialized();
    let messages = this.session?.state.messages;
    if (messages === undefined) {
      const recipient = await this.providers.selectedAccount();
      if (recipient === undefined || this.options.projectId === null || this.options.workspaceId === null) return [];
      const manager = await this.openOrCreateSessionManager(recipient);
      messages = manager.buildSessionContext().messages;
    }
    return messages.flatMap((message): SessionHistoryMessage[] => {
      if (message.role === "user") {
        return [{ role: "user", text: textFromMessage(message), timestamp: message.timestamp }];
      }
      if (message.role === "assistant") {
        return [{ role: "assistant", text: textFromMessage(message), timestamp: message.timestamp }];
      }
      if (message.role === "toolResult") {
        return [{
          role: "tool",
          text: textFromMessage(message),
          timestamp: message.timestamp,
          toolName: message.toolName,
          isError: message.isError,
        }];
      }
      return [];
    });
  }

  async status(): Promise<AssistantStatus> {
    const selected = this.providers.getSelected();
    let configured = false;
    if (selected !== undefined) configured = (await this.modelRuntime.checkAuth(selected.providerId)) !== undefined;
    const stats = this.session?.getSessionStats();
    return {
      active: this.session?.isStreaming ?? false,
      configured,
      ...(selected === undefined ? {} : { providerId: selected.providerId, modelId: selected.modelId }),
      ...(stats === undefined ? {} : {
        sessionId: stats.sessionId,
        usage: {
          input: stats.tokens.input,
          output: stats.tokens.output,
          cacheRead: stats.tokens.cacheRead,
          cacheWrite: stats.tokens.cacheWrite,
          total: stats.tokens.total,
          cost: stats.cost,
        },
      }),
    };
  }

  async providersAction(action: ProvidersAction, signal?: AbortSignal, authOperationId?: string): Promise<ProvidersReply> {
    return this.providers.handle(action, signal, authOperationId);

  }
  private async ensureSession(model: Model<Api>, recipient: SessionRecipient): Promise<AgentSession> {
    const activeRecipient = this.sessionRecipient;
    if (
      this.session !== undefined &&
      activeRecipient !== undefined &&
      activeRecipient.providerId === recipient.providerId &&
      activeRecipient.accountId === recipient.accountId
    ) {
      return this.session;
    }
    if (this.session !== undefined) {
      await this.stop();
      this.disposeSession();
    }
    await mkdir(this.workspaceCwd, { recursive: true });
    await mkdir(this.projectSessionDir, { recursive: true });
    const settingsManager = SettingsManager.inMemory({
      packages: [],
      extensions: [],
      skills: [],
      prompts: [],
      themes: [],
      defaultProjectTrust: "never",
      enableSkillCommands: false,
      enableInstallTelemetry: false,
      enableAnalytics: false,
    });
    const resourceLoader = new DefaultResourceLoader({
      cwd: this.workspaceCwd,
      agentDir: this.options.agentDir,
      settingsManager,
      noExtensions: true,
      noSkills: true,
      noPromptTemplates: true,
      noThemes: true,
      noContextFiles: true,
      systemPrompt: EDITOR_SYSTEM_PROMPT,
      appendSystemPrompt: [...EDITOR_SYSTEM_PROMPT_APPEND],
      extensionsOverride: (base) => ({ ...base, extensions: [], errors: [] }),
      skillsOverride: () => ({ skills: [], diagnostics: [] }),
      promptsOverride: () => ({ prompts: [], diagnostics: [] }),
      themesOverride: () => ({ themes: [], diagnostics: [] }),
      agentsFilesOverride: () => ({ agentsFiles: [] }),
      systemPromptOverride: () => EDITOR_SYSTEM_PROMPT,
      appendSystemPromptOverride: () => [...EDITOR_SYSTEM_PROMPT_APPEND],
    });
    await resourceLoader.reload();
    const sessionManager = await this.openOrCreateSessionManager(recipient);
    const run: ToolRunContext = {
      toolCallId: this.currentRunId ?? "assistant",
      nativeRunId: () => this.currentRunId,
      port: this.editorPort,
    };
    const tools = createEditorTools(run);
    const created = await createAgentSession({
      cwd: this.workspaceCwd,
      agentDir: this.options.agentDir,
      modelRuntime: this.modelRuntime,
      model,
      thinkingLevel: "medium",
      noTools: "builtin",
      tools: [...EDITOR_TOOL_NAMES],
      customTools: tools,
      resourceLoader,
      settingsManager,
      sessionManager,
    });
    this.unsubscribe = created.session.subscribe((event) => this.forwardEvent(event));
    this.session = created.session;
    this.sessionRecipient = { ...recipient };
    return this.session;
  }

  private async openOrCreateSessionManager(recipient: SessionRecipient): Promise<SessionManager> {
    const projectId = this.options.projectId;
    const workspaceId = this.options.workspaceId;
    if (projectId === null || workspaceId === null) {
      throw new BridgeProtocolError("STALE_SESSION", "A native workspace binding is required for chat sessions.");
    }
    let sessionFile: string | undefined;
    try {
      const raw = await readFile(this.sessionMetadataPath, "utf8");
      if (raw.trim().length > 0) {
        const parsed: unknown = JSON.parse(raw);
        if (
          typeof parsed === "object" &&
          parsed !== null &&
          !Array.isArray(parsed) &&
          "sessionFile" in parsed &&
          "workspaceId" in parsed &&
          "projectId" in parsed &&
          "providerId" in parsed &&
          "accountId" in parsed &&
          typeof parsed.sessionFile === "string" &&
          parsed.workspaceId === workspaceId &&
          parsed.projectId === projectId &&
          parsed.providerId === recipient.providerId &&
          parsed.accountId === recipient.accountId
        ) {
          const root = await realpath(this.projectSessionDir);
          const candidate = resolve(root, parsed.sessionFile);
          const candidatePath = relative(root, candidate);
          if (candidatePath.length > 0 && !candidatePath.startsWith("..") && !isAbsolute(candidatePath)) {
            const candidateReal = await realpath(candidate);
            const realPath = relative(root, candidateReal);
            if (realPath.length > 0 && !realPath.startsWith("..") && !isAbsolute(realPath)) {
              await access(candidateReal);
              sessionFile = candidateReal;
            }
          }
        }
      }
    } catch {
      sessionFile = undefined;
    }
    if (sessionFile !== undefined) return SessionManager.open(sessionFile, this.projectSessionDir, this.workspaceCwd);
    const manager = SessionManager.create(this.workspaceCwd, this.projectSessionDir);
    const generated = manager.getSessionFile();
    if (generated !== undefined) {
      const root = await realpath(this.projectSessionDir);
      const generatedPath = resolve(generated);
      const generatedRelative = relative(root, generatedPath);
      if (generatedRelative.length > 0 && !generatedRelative.startsWith("..") && !isAbsolute(generatedRelative)) {
        await writeFile(
          this.sessionMetadataPath,
          `${JSON.stringify({
            workspaceId,
            projectId,
            providerId: recipient.providerId,
            accountId: recipient.accountId,
            sessionFile: generatedRelative,
          })}\n`,
          { mode: 0o600 },
        );
      }
    }
    return manager;
  }

  private retireRunId(runId: string): void {
    if (runId.length === 0) return;
    this.retiredRunIds.add(runId);
    while (this.retiredRunIds.size > MAX_RETIRED_RUN_IDS) {
      const oldest = this.retiredRunIds.values().next().value;
      if (typeof oldest !== "string") break;
      this.retiredRunIds.delete(oldest);
    }
  }
  private disposeSession(): void {
    this.unsubscribe?.();
    this.unsubscribe = undefined;
    this.session?.dispose();
    this.session = undefined;
    this.sessionRecipient = undefined;
  }

  private async resetSession(): Promise<void> {
    await this.stop();
    this.disposeSession();
  }

  private forwardEvent(event: AgentSessionEvent): void {
    switch (event.type) {
      case "agent_start":
        this.emit("assistant_start", {});
        break;
      case "agent_end":
        this.emit("assistant_end", {});
        break;
      case "agent_settled":
        this.emit("assistant_settled", {});
        break;
      case "message_update": {
        const update = event.assistantMessageEvent;
        if (update.type === "text_delta") this.emit("assistant_text_delta", { delta: update.delta });
        break;
      }
      case "message_end": {
        const message = event.message;
        if (message.role === "assistant") {
          this.emit("assistant_message", {
            text: textFromMessage(message),
            usage: sanitizeEventData(message.usage),
          });
        }
        break;
      }
      case "tool_execution_start":
        this.emit("assistant_tool_start", { toolCallId: event.toolCallId, toolName: event.toolName });
        break;
      case "tool_execution_update":
        this.emit("assistant_tool_update", { toolCallId: event.toolCallId, toolName: event.toolName });
        break;
      case "tool_execution_end":
        this.emit("assistant_tool_end", { toolCallId: event.toolCallId, toolName: event.toolName, isError: event.isError });
        break;
      case "compaction_start":
        this.emit("assistant_compaction_start", { reason: event.reason });
        break;
      case "compaction_end":
        this.emit("assistant_compaction_end", { reason: event.reason, aborted: event.aborted, ...(event.errorMessage === undefined ? {} : { error: event.errorMessage }) });
        break;
      case "auto_retry_start":
      case "auto_retry_end":
      case "summarization_retry_scheduled":
      case "summarization_retry_attempt_start":
      case "summarization_retry_finished":
        this.emit("assistant_retry", { type: event.type });
        break;
      default:
        break;
    }
  }

  private emit(event: string, data?: unknown, runId?: string): void {
    this.events.emit(event, sanitizeEventData(data), runId ?? this.currentRunId);
  }

  private assertInitialized(): void {
    if (!this.initialized) throw new BridgeProtocolError("BUSY", "The assistant runtime is still starting.");
  }
}
