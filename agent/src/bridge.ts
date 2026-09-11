import { randomUUID } from "node:crypto";
import type { Readable, Writable } from "node:stream";

import {
  IPC_VERSION,
  type EditorErrorCode,
  isEditorErrorCode,
  isEditorReply,
  isEditorRequest,
  isSafeInteger,
} from "@cutterhoochee/shared/protocol";

export const MAX_NDJSON_LINE_BYTES = 16 * 1024 * 1024;

export const DEFAULT_METHODS = [
  "project_status",
  "project_create",
  "project_open",
  "project_save",
  "project_close",
  "project_snapshot",
  "timeline_snapshot",
  "timeline_selection",
  "project_history",
  "edit_project",
] as const;

/**
 * Private methods use the same v1 request/response/event envelope as editor
 * calls, but are intentionally kept out of the editor schema validator.  The
 * native side owns the allowlist and caller checks for these methods.
 */
export const PRIVATE_METHODS = [
  "assistant",
  "providers",
  "credential_read",
  "credential_list",
  "credential_lease_acquire",
  "credential_lease_commit",
  "credential_lease_release",
  "credential_delete",
  "evidence_image_read",
] as const;

export type BridgeRequestMessage = {
  v: typeof IPC_VERSION;
  id: string;
  projectId: string | null;
  generation: number;
  runId?: string;
  kind: "request";
  method: string;
  params: unknown;
};

export type BridgeResponseMessage =
  | {
      v: typeof IPC_VERSION;
      id: string;
      projectId: string | null;
      generation: number;
      runId?: string;
      kind: "response";
      ok: true;
      data: unknown;
    }
  | {
      v: typeof IPC_VERSION;
      id: string;
      projectId: string | null;
      generation: number;
      runId?: string;
      kind: "response";
      ok: false;
      error: {
        code: EditorErrorCode;
        message: string;
        details?: unknown;
      };
    };

export type BridgeEventMessage = {
  v: typeof IPC_VERSION;
  id: string;
  projectId: string | null;
  generation: number;
  runId?: string;
  kind: "event";
  event: string;
  data?: unknown;
};

type BridgeMessage =
  | BridgeRequestMessage
  | BridgeResponseMessage
  | BridgeEventMessage;

type IncomingRequestHandler = (
  request: BridgeRequestMessage,
  signal: AbortSignal,
) => Promise<unknown>;

type PendingRequest = {
  generation: number;
  runId?: string;
  resolve: (response: BridgeResponseMessage) => void;
  reject: (error: BridgeProtocolError) => void;
  timer?: NodeJS.Timeout;
};

export interface NdjsonBridgeOptions {
  generation?: number;
  projectId?: string | null;
  runId?: string;
  methods?: readonly string[];
  privateMethods?: readonly string[];
  handleRequest?: IncomingRequestHandler;
  handleEvent?: (event: BridgeEventMessage) => void;
  createId?: () => string;
  diagnostic?: (code: string) => void;
}

export class BridgeProtocolError extends Error {
  readonly code: EditorErrorCode;
  readonly fatal: boolean;

  constructor(code: EditorErrorCode, message: string, fatal = false) {
    super(message);
    this.name = "BridgeProtocolError";
    this.code = code;
    this.fatal = fatal;
  }
}

/**
 * Owns exactly one NDJSON stdin/stdout connection. It never writes diagnostics
 * to stdout: stdout is reserved for protocol frames, while callers may route
 * the redacted diagnostic code to stderr.
 */
export class NdjsonBridge {
  private readonly input: Readable;
  private readonly output: Writable;
  private readonly methods: ReadonlySet<string>;
  private readonly privateMethods: ReadonlySet<string>;
  private readonly handleRequest: IncomingRequestHandler;
  private readonly handleEvent: (event: BridgeEventMessage) => void;
  private readonly createId: () => string;
  private readonly diagnostic: (code: string) => void;
  private readonly pending = new Map<string, PendingRequest>();
  private readonly incoming = new Map<string, AbortController>();
  private lineBuffer = Buffer.alloc(0);
  private started = false;
  private closed = false;
  private _generation: number;
  private _projectId: string | null;
  private readonly runId?: string;

  constructor(input: Readable, output: Writable, options: NdjsonBridgeOptions = {}) {
    this.input = input;
    this.output = output;
    this.methods = new Set(options.methods ?? DEFAULT_METHODS);
    this.privateMethods = new Set(options.privateMethods ?? PRIVATE_METHODS);
    this.handleRequest =
      options.handleRequest ??
      (async () => {
        throw new BridgeProtocolError(
          "SCHEMA_UNSUPPORTED",
          "No handler is configured for this bridge request.",
        );
      });
    this.handleEvent = options.handleEvent ?? (() => undefined);
    this.createId = options.createId ?? randomUUID;
    this._generation = options.generation ?? 0;
    this._projectId = options.projectId ?? null;
    this.runId = options.runId;
    this.diagnostic = options.diagnostic ?? (() => undefined);
    if (!isSafeInteger(this._generation) || this._generation < 0) {
      throw new BridgeProtocolError(
        "INVALID_ARGUMENT",
        "Initial bridge generation must be a non-negative safe integer.",
      );
    }
  }

  get generation(): number {
    return this._generation;
  }

  get projectId(): string | null {
    return this._projectId;
  }

  get isClosed(): boolean {
    return this.closed;
  }

  start(): void {
    if (this.started || this.closed) return;
    this.started = true;
    this.input.on("data", this.onData);
    this.input.on("end", this.onEnd);
    this.input.on("error", this.onInputError);
    this.output.on("error", this.onOutputError);
  }

  /** Retire every request in the old generation before accepting the next one. */
  retireGeneration(nextGeneration: number, projectId: string | null = null): void {
    if (!isSafeInteger(nextGeneration) || nextGeneration <= this._generation) {
      throw new BridgeProtocolError(
        "INVALID_ARGUMENT",
        "A retired bridge generation must increase monotonically.",
      );
    }
    this._generation = nextGeneration;
    this._projectId = projectId;
    this.abortIncoming();
    const stale = new BridgeProtocolError(
      "STALE_SESSION",
      "The bridge generation has been retired.",
    );
    for (const [id, request] of this.pending) {
      if (request.generation < nextGeneration) {
        clearTimeout(request.timer!);
        request.reject(stale);
        this.pending.delete(id);
      }
    }
  }

  request(
    method: string,
    params: unknown,
    options: {
      projectId?: string | null;
      generation?: number;
      runId?: string;
      timeoutMs?: number;
      id?: string;
    } = {},
  ): Promise<BridgeResponseMessage> {
    if (this.closed) {
      return Promise.reject(
        new BridgeProtocolError("STALE_SESSION", "The bridge is closed."),
      );
    }
    this.assertMethod(method);
    const generation = options.generation ?? this._generation;
    if (generation !== this._generation) {
      return Promise.reject(
        new BridgeProtocolError(
          "STALE_SESSION",
          "The requested bridge generation is retired.",
        ),
      );
    }
    const projectId = options.projectId ?? this._projectId;
    if (projectId !== this._projectId) {
      return Promise.reject(
        new BridgeProtocolError(
          "STALE_SESSION",
          "The requested project context is retired.",
        ),
      );
    }
    const id = options.id ?? this.createId();
    this.assertId(id);
    if (this.pending.has(id) || this.incoming.has(id)) {
      return Promise.reject(
        new BridgeProtocolError(
          "IDEMPOTENCY_CONFLICT",
          "The bridge request ID is already in flight.",
        ),
      );
    }

    const message: BridgeRequestMessage = {
      v: IPC_VERSION,
      id,
      projectId,
      generation,
      kind: "request",
      method,
      params,
      ...(options.runId === undefined
        ? this.runId === undefined
          ? {}
          : { runId: this.runId }
        : { runId: options.runId }),
    };

    return new Promise<BridgeResponseMessage>((resolve, reject) => {
      const pending: PendingRequest = {
        generation,
        ...(message.runId === undefined ? {} : { runId: message.runId }),
        resolve,
        reject,
      };
      if (options.timeoutMs !== undefined) {
        if (!Number.isFinite(options.timeoutMs) || options.timeoutMs <= 0) {
          reject(
            new BridgeProtocolError(
              "INVALID_ARGUMENT",
              "Bridge request timeout must be positive.",
            ),
          );
          return;
        }
        pending.timer = setTimeout(() => {
          this.pending.delete(id);
          reject(
            new BridgeProtocolError(
              "BUSY",
              "The bridge request timed out.",
            ),
          );
        }, options.timeoutMs);
      }
      this.pending.set(id, pending);
      try {
        this.writeMessage(message);
      } catch (error) {
        this.pending.delete(id);
        clearTimeout(pending.timer!);
        reject(this.asBridgeError(error, "IO_ERROR"));
      }
    });
  }

  event(
    event: string,
    data?: unknown,
    options: { runId?: string; projectId?: string | null; generation?: number } = {},
  ): void {
    if (this.closed) {
      throw new BridgeProtocolError("STALE_SESSION", "The bridge is closed.");
    }
    if (!/^[a-z][a-z0-9_.-]*$/.test(event)) {
      throw new BridgeProtocolError(
        "INVALID_ARGUMENT",
        "Bridge event names must be lowercase identifiers.",
      );
    }
    const generation = options.generation ?? this._generation;
    const projectId = options.projectId ?? this._projectId;
    if (generation !== this._generation || projectId !== this._projectId) {
      throw new BridgeProtocolError(
        "STALE_SESSION",
        "The event belongs to a retired bridge context.",
      );
    }
    const message: BridgeEventMessage = {
      v: IPC_VERSION,
      id: this.createId(),
      projectId,
      generation,
      kind: "event",
      event,
      ...(options.runId === undefined
        ? this.runId === undefined
          ? {}
          : { runId: this.runId }
        : { runId: options.runId }),
      ...(data === undefined ? {} : { data }),
    };
    this.writeMessage(message);
  }

  close(error?: BridgeProtocolError): void {
    if (this.closed) return;
    this.closed = true;
    this.input.removeListener("data", this.onData);
    this.input.removeListener("end", this.onEnd);
    this.input.removeListener("error", this.onInputError);
    this.output.removeListener("error", this.onOutputError);
    this.abortIncoming();
    const reason =
      error ?? new BridgeProtocolError("STALE_SESSION", "The bridge closed.");
    for (const [id, request] of this.pending) {
      clearTimeout(request.timer!);
      request.reject(reason);
      this.pending.delete(id);
    }
  }

  private readonly onData = (chunk: Buffer | string): void => {
    if (this.closed) return;
    const bytes = Buffer.isBuffer(chunk) ? chunk : Buffer.from(chunk);
    if (bytes.length === 0) return;
    this.lineBuffer = Buffer.concat([this.lineBuffer, bytes]);
    if (this.lineBuffer.length > MAX_NDJSON_LINE_BYTES && !this.lineBuffer.includes(10)) {
      this.failProtocol("NDJSON line exceeds the 16 MiB limit.");
      return;
    }

    let newlineIndex = this.lineBuffer.indexOf(10);
    while (newlineIndex !== -1) {
      const line = this.lineBuffer.subarray(0, newlineIndex);
      this.lineBuffer = this.lineBuffer.subarray(newlineIndex + 1);
      if (line.length > MAX_NDJSON_LINE_BYTES) {
        this.failProtocol("NDJSON line exceeds the 16 MiB limit.");
        return;
      }
      this.processLine(line);
      if (this.closed) return;
      newlineIndex = this.lineBuffer.indexOf(10);
    }
    if (this.lineBuffer.length > MAX_NDJSON_LINE_BYTES) {
      this.failProtocol("NDJSON line exceeds the 16 MiB limit.");
    }
  };

  private readonly onEnd = (): void => {
    if (this.closed) return;
    if (this.lineBuffer.length > 0) {
      this.diagnostic("INVALID_ARGUMENT");
    }
    this.close(new BridgeProtocolError("STALE_SESSION", "The bridge input ended."));
  };

  private readonly onInputError = (): void => {
    this.failProtocol("Bridge input failed.");
  };

  private readonly onOutputError = (): void => {
    this.failProtocol("Bridge output failed.");
  };

  private processLine(line: Buffer): void {
    const trimmed = line.length > 0 && line[line.length - 1] === 13
      ? line.subarray(0, line.length - 1)
      : line;
    if (trimmed.length === 0) {
      this.diagnostic("INVALID_ARGUMENT");
      return;
    }

    let value: unknown;
    try {
      value = JSON.parse(trimmed.toString("utf8"));
    } catch {
      this.diagnostic("INVALID_ARGUMENT");
      return;
    }
    const parsed = this.parseMessage(value);
    if (parsed === undefined) return;
    if (parsed.kind === "request") {
      void this.handleIncomingRequest(parsed);
      return;
    }
    if (parsed.kind === "response") {
      this.handleIncomingResponse(parsed);
      return;
    }
    this.handleIncomingEvent(parsed);
  }

  private parseMessage(value: unknown): BridgeMessage | undefined {
    if (typeof value !== "object" || value === null) {
      this.diagnostic("SCHEMA_UNSUPPORTED");
      return undefined;
    }
    const object = value as Record<string, unknown>;
    if (
      object.v !== IPC_VERSION ||
      typeof object.id !== "string" ||
      object.id.length === 0 ||
      (typeof object.projectId !== "string" && object.projectId !== null) ||
      !isSafeInteger(object.generation) ||
      object.generation < 0 ||
      typeof object.kind !== "string"
    ) {
      this.sendValidationError(object);
      return undefined;
    }
    if (object.kind === "request") {
      const method = object.method;
      const validMethod =
        typeof method === "string" &&
        /^[a-z][a-z0-9_]*$/.test(method) &&
        ("params" in object) &&
        (isEditorRequest({ method, params: object.params }) ||
          this.privateMethods.has(method));
      if (!validMethod) {
        this.sendEnvelopeError(object, "SCHEMA_UNSUPPORTED", "Invalid request envelope.");
        return undefined;
      }
      return object as unknown as BridgeRequestMessage;
    }
    if (object.kind === "response") {
      if (
        typeof object.ok !== "boolean" ||
        (object.ok && !("data" in object)) ||
        (!object.ok && !this.isErrorObject(object.error))
      ) {
        this.sendEnvelopeError(object, "SCHEMA_UNSUPPORTED", "Invalid response envelope.");
        return undefined;
      }
      return object as unknown as BridgeResponseMessage;
    }
    if (
      object.kind === "event" &&
      typeof object.event === "string" &&
      /^[a-z][a-z0-9_.-]*$/.test(object.event)
    ) {
      return object as unknown as BridgeEventMessage;
    }
    this.sendValidationError(object);
    return undefined;
  }

  private async handleIncomingRequest(request: BridgeRequestMessage): Promise<void> {
    if (request.generation !== this._generation) {
      this.sendEnvelopeError(
        request,
        "STALE_SESSION",
        "The request belongs to a retired bridge generation.",
      );
      return;
    }
    if (request.projectId !== this._projectId) {
      this.sendEnvelopeError(
        request,
        "STALE_SESSION",
        "The request belongs to another project context.",
      );
      return;
    }
    if (!this.methods.has(request.method) && !this.privateMethods.has(request.method)) {
      this.sendEnvelopeError(
        request,
        "SCHEMA_UNSUPPORTED",
        "The requested bridge method is unavailable.",
      );
      return;
    }
    if (this.incoming.has(request.id) || this.pending.has(request.id)) {
      this.sendEnvelopeError(
        request,
        "IDEMPOTENCY_CONFLICT",
        "The bridge request ID is already in flight.",
      );
      return;
    }

    const controller = new AbortController();
    this.incoming.set(request.id, controller);
    try {
      const data = await this.handleRequest(request, controller.signal);
      if (this.closed || controller.signal.aborted || request.generation !== this._generation) {
        return;
      }
      this.writeMessage({
        v: IPC_VERSION,
        id: request.id,
        projectId: this._projectId,
        generation: this._generation,
        kind: "response",
        ok: true,
        data,
        ...(request.runId === undefined ? {} : { runId: request.runId }),
      });
    } catch (error) {
      if (this.closed || controller.signal.aborted || request.generation !== this._generation) {
        return;
      }
      const bridgeError = this.asBridgeError(error, "IO_ERROR");
      this.writeMessage({
        v: IPC_VERSION,
        id: request.id,
        projectId: this._projectId,
        generation: this._generation,
        kind: "response",
        ok: false,
        error: {
          code: bridgeError.code,
          message: bridgeError.message,
        },
        ...(request.runId === undefined ? {} : { runId: request.runId }),
      });
    } finally {
      this.incoming.delete(request.id);
    }
  }

  private handleIncomingResponse(response: BridgeResponseMessage): void {
    const pending = this.pending.get(response.id);
    if (!pending) {
      this.diagnostic("STALE_SESSION");
      return;
    }
    this.pending.delete(response.id);
    clearTimeout(pending.timer);
    if (
      response.generation !== this._generation ||
      response.generation !== pending.generation
    ) {
      pending.reject(
        new BridgeProtocolError(
          "STALE_SESSION",
          "The response belongs to a retired bridge generation.",
        ),
      );
      return;
    }
    if (response.projectId !== this._projectId) {
      pending.reject(
        new BridgeProtocolError(
          "STALE_SESSION",
          "The response belongs to another project context.",
        ),
      );
      return;
    }
    if (response.runId !== pending.runId) {
      pending.reject(
        new BridgeProtocolError(
          "STALE_SESSION",
          "The response belongs to another assistant run.",
        ),
      );
      return;
    }
    pending.resolve(response);
  }

  private handleIncomingEvent(event: BridgeEventMessage): void {
    if (event.generation !== this._generation || event.projectId !== this._projectId) {
      this.diagnostic("STALE_SESSION");
      return;
    }
    this.handleEvent(event);
  }

  private writeMessage(message: BridgeMessage): void {
    if (this.closed) {
      throw new BridgeProtocolError("STALE_SESSION", "The bridge is closed.");
    }
    const line = Buffer.from(`${JSON.stringify(message)}\n`, "utf8");
    if (line.length - 1 > MAX_NDJSON_LINE_BYTES) {
      throw new BridgeProtocolError(
        "INVALID_ARGUMENT",
        "The bridge message exceeds the 16 MiB limit.",
      );
    }
    this.output.write(line);
  }

  private sendEnvelopeError(
    incoming: Record<string, unknown>,
    code: EditorErrorCode,
    message: string,
  ): void {
    const id = typeof incoming.id === "string" && incoming.id.length > 0
      ? incoming.id
      : this.createId();
    const generation = isSafeInteger(incoming.generation)
      ? incoming.generation
      : this._generation;
    const projectId =
      typeof incoming.projectId === "string" || incoming.projectId === null
        ? incoming.projectId
        : this._projectId;
    try {
      this.writeMessage({
        v: IPC_VERSION,
        id,
        projectId,
        generation,
        kind: "response",
        ok: false,
        error: { code, message },
      });
    } catch {
      this.failProtocol("Unable to write bridge validation response.");
    }
  }

  private sendValidationError(incoming: Record<string, unknown>): void {
    this.sendEnvelopeError(
      incoming,
      "SCHEMA_UNSUPPORTED",
      "The bridge message does not match protocol v1.",
    );
  }

  private assertMethod(method: string): void {
    if (
      !/^[a-z][a-z0-9_]*$/.test(method) ||
      (!this.methods.has(method) && !this.privateMethods.has(method))
    ) {
      throw new BridgeProtocolError(
        "SCHEMA_UNSUPPORTED",
        "The requested bridge method is unavailable.",
      );
    }
  }

  private assertId(id: string): void {
    if (id.length === 0 || id.length > 256 || /[\r\n]/.test(id)) {
      throw new BridgeProtocolError(
        "INVALID_ARGUMENT",
        "Bridge request IDs must be non-empty single-line strings.",
      );
    }
  }

  private isErrorObject(value: unknown): boolean {
    if (typeof value !== "object" || value === null) return false;
    const error = value as Record<string, unknown>;
    return (
      isEditorErrorCode(error.code) &&
      typeof error.message === "string" &&
      (error.details === undefined || typeof error.details === "object")
    );
  }

  private abortIncoming(): void {
    for (const controller of this.incoming.values()) controller.abort();
    this.incoming.clear();
  }

  private failProtocol(message: string): void {
    this.diagnostic("IO_ERROR");
    this.close(new BridgeProtocolError("IO_ERROR", message, true));
  }

  private asBridgeError(error: unknown, fallback: EditorErrorCode): BridgeProtocolError {
    if (error instanceof BridgeProtocolError) return error;
    if (
      typeof error === "object" &&
      error !== null &&
      "code" in error &&
      "message" in error &&
      typeof error.code === "string" &&
      isEditorErrorCode(error.code) &&
      typeof error.message === "string"
    ) {
      return new BridgeProtocolError(error.code, error.message);
    }
    return new BridgeProtocolError(fallback, "The bridge operation failed.");
  }
}
