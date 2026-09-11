import { resolve } from "node:path";

import { BridgeProtocolError, DEFAULT_METHODS, NdjsonBridge, PRIVATE_METHODS, type BridgeRequestMessage } from "./bridge.js";
import { AssistantRuntime, type AssistantAction } from "./assistant.js";
import { PiRuntime } from "./session.js";
import type { ProvidersAction } from "./providers.js";

function writeDiagnostic(code: string): void {
  process.stderr.write(`[cutterhoochee-agent] ${code}\n`);
}

function readGeneration(): number | undefined {
  const raw = process.env.CUTTERHOOCHEE_GENERATION;
  if (raw === undefined || raw.length === 0) return 0;
  const generation = Number(raw);
  if (!Number.isSafeInteger(generation) || generation < 0) {
    writeDiagnostic("INVALID_ARGUMENT");
    process.exitCode = 1;
    return undefined;
  }
  return generation;
}

function readWorkspaceId(): string | null {
  const raw = process.env.CUTTERHOOCHEE_WORKSPACE_ID;
  return raw === undefined || raw.length === 0 ? null : raw;
}
function readProjectId(): string | null {
  const raw = process.env.CUTTERHOOCHEE_PROJECT_ID;
  return raw === undefined || raw.length === 0 ? null : raw;
}

function requiredObject(value: unknown, message: string): Record<string, unknown> {
  if (typeof value !== "object" || value === null || Array.isArray(value)) {
    throw new BridgeProtocolError("INVALID_ARGUMENT", message);
  }
  return value as Record<string, unknown>;
}

function parseAssistantAction(value: unknown): AssistantAction {
  const object = requiredObject(value, "Assistant params must be an object.");
  const action = object.action;
  if (action === "status" || action === "history" || action === "stop" || action === "restart" || action === "new_session") {
    if (Object.keys(object).length !== 1) throw new BridgeProtocolError("INVALID_ARGUMENT", "The assistant action has unknown fields.");
    return { action };
  }
  if (action === "prompt" && typeof object.text === "string" && object.text.trim().length > 0 && Object.keys(object).every((key) => key === "action" || key === "text")) {
    return { action, text: object.text };
  }
  throw new BridgeProtocolError("INVALID_ARGUMENT", "The assistant action is invalid.");
}

function parseProvidersAction(value: unknown): ProvidersAction {
  const object = requiredObject(value, "Provider params must be an object.");
  const action = object.action;
  if (action === "list" || action === "refresh") {
    if (Object.keys(object).length !== 1) throw new BridgeProtocolError("INVALID_ARGUMENT", "The provider action has unknown fields.");
    return { action };
  }
  if (action === "models" && typeof object.providerId === "string" && Object.keys(object).every((key) => key === "action" || key === "providerId")) {
    return { action, providerId: object.providerId };
  }
  if (action === "logout" && typeof object.providerId === "string" && Object.keys(object).every((key) => key === "action" || key === "providerId")) {
    return { action, providerId: object.providerId };
  }
  if (action === "select" && typeof object.providerId === "string" && typeof object.modelId === "string" && Object.keys(object).every((key) => key === "action" || key === "providerId" || key === "modelId")) {
    return { action, providerId: object.providerId, modelId: object.modelId };
  }
  if (
    action === "answer" &&
    typeof object.promptId === "string" &&
    typeof object.value === "string" &&
    Object.keys(object).every((key) => key === "action" || key === "promptId" || key === "value")
  ) {
    return { action, promptId: object.promptId, value: object.value };
  }
  if (
    action === "login" &&
    typeof object.providerId === "string" &&
    (object.authType === "api_key" || object.authType === "oauth") &&
    (object.sessionOnly === undefined || typeof object.sessionOnly === "boolean") &&
    Object.keys(object).every((key) => key === "action" || key === "providerId" || key === "authType" || key === "sessionOnly")
  ) {
    return { action, providerId: object.providerId, authType: object.authType, ...(object.sessionOnly === undefined ? {} : { sessionOnly: object.sessionOnly }) };
  }
  throw new BridgeProtocolError("INVALID_ARGUMENT", "The provider action is invalid.");
}

export function startAgent(): NdjsonBridge | undefined {
  const generation = readGeneration();
  if (generation === undefined) return undefined;
  const agentDir = resolve(process.env.CUTTERHOOCHEE_AGENT_DIR ?? process.cwd());
  const cacheDir = resolve(process.env.XDG_CACHE_HOME ?? agentDir, "cutterhoochee");
  const sessionDir = resolve(process.env.CUTTERHOOCHEE_SESSION_DIR ?? agentDir, "sessions");
  let requestHandler: (request: BridgeRequestMessage, signal: AbortSignal) => Promise<unknown> = async () => {
    throw new BridgeProtocolError("BUSY", "The agent runtime is still initializing.");
  };
  const bridge = new NdjsonBridge(process.stdin, process.stdout, {
    generation,
    projectId: readProjectId(),
    methods: DEFAULT_METHODS,
    privateMethods: [
      ...PRIVATE_METHODS,
      "media",
      "jobs",
      "preview",
      "transcript",
      "analyze_media",
      "sample_frames",
      "create_graphic",
      "export_video",
      "system_read",
      "system_write",
      "system_execute",
      "permissions",
      "system_http",
    ],
    handleRequest: (request, signal) => requestHandler(request, signal),
    diagnostic: writeDiagnostic,
  });

  const runtimePromise = PiRuntime.create({
    bridge,
    agentDir,
    cacheDir,
    sessionDir,
    projectId: readProjectId(),
    workspaceId: readWorkspaceId(),
    generation,
    events: {
      emit: (event, data, runId) => bridge.event(event, data, { runId }),
    },
  });
  const assistantPromise = runtimePromise.then((runtime) => new AssistantRuntime(runtime));
  const dispatchRequest = async (request: BridgeRequestMessage, signal: AbortSignal): Promise<unknown> => {
    const assistant = await assistantPromise;
    if (request.method === "assistant") {
      return assistant.handle(parseAssistantAction(request.params), signal, request.runId);
    }
    if (request.method === "providers") {
      const runtime = await runtimePromise;
      return runtime.providersAction(parseProvidersAction(request.params), signal, request.runId);
    }
    throw new BridgeProtocolError("SCHEMA_UNSUPPORTED", "The requested private method is unavailable to the agent.");
  };
  // The bridge starts after the handler is installed so no assistant request
  // can observe a partially initialized runtime.
  requestHandler = dispatchRequest;
  bridge.start();

  const stop = (): void => {
    process.stdin.pause();
    void runtimePromise.then((runtime) => runtime.dispose()).catch(() => undefined);
    bridge.close();
  };
  process.once("SIGTERM", stop);
  process.once("SIGINT", stop);
  process.once("uncaughtException", () => {
    writeDiagnostic("IO_ERROR");
    stop();
    process.exitCode = 1;
  });
  process.once("unhandledRejection", () => {
    writeDiagnostic("IO_ERROR");
    stop();
    process.exitCode = 1;
  });
  void runtimePromise.catch(() => writeDiagnostic("IO_ERROR"));
  return bridge;
}

startAgent();
