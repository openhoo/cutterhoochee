import { mkdir, readFile, writeFile } from "node:fs/promises";
import { dirname, resolve } from "node:path";

import type {
  Api,
  ApiKeyCredential,
  AuthEvent,
  AuthInteraction,
  AuthPrompt,
  AuthType,
  Credential,
  Model,
  OAuthCredential,
  Provider,
} from "@earendil-works/pi-ai";
import { createProvider } from "@earendil-works/pi-ai";
import { ModelRuntime } from "@earendil-works/pi-coding-agent";

import { BridgeProtocolError } from "./bridge.js";
import { BridgeCredentialStore } from "./credentials.js";

export type ProvidersAction =
  | { action: "list" }
  | { action: "models"; providerId: string }
  | { action: "login"; providerId: string; authType: AuthType; sessionOnly?: boolean }
  | { action: "answer"; promptId: string; value: string }
  | { action: "logout"; providerId: string }
  | { action: "select"; providerId: string; modelId: string }
  | { action: "refresh" };

export type ProviderModel = {
  id: string;
  name: string;
  reasoning: boolean;
  input: readonly ("text" | "image")[];
  contextWindow: number;
  maxTokens: number;
};

export type ProvidersReply =
  | { action: "list"; providers: readonly ProviderStatus[]; selected?: SelectedModel }
  | { action: "models"; providerId: string; models: readonly ProviderModel[] }
  | { action: "login"; providerId: string; type: AuthType; configured: boolean }
  | { action: "answer"; promptId: string; accepted: true }
  | { action: "logout"; providerId: string; configured: false }
  | { action: "select"; selected: SelectedModel }
  | { action: "refresh"; refreshed: true };

export type ProviderStatus = {
  id: string;
  name: string;
  configured: boolean;
  /** Native-derived recipient identity; null means no usable account. */
  accountId: string | null;
  authType?: AuthType;
  modelCount: number;
};

export type SelectedModel = { providerId: string; modelId: string };

export type ProviderAccountIdentity = { providerId: string; accountId: string };

export type AuthPromptEvent = {
  authOperationId: string;
  providerId: string;
  authType: AuthType;
  promptId: string;
  prompt: AuthPrompt;
};

export type ProviderAuthEvent =
  | { type: "info"; message: string; links?: readonly { url: string; label?: string }[] }
  | { type: "auth_url"; url: string; instructions?: string }
  | { type: "device_code"; userCode: string; verificationUri: string; intervalSeconds?: number; expiresInSeconds?: number }
  | { type: "progress"; message: string };

export interface ProviderEventSink {
  emit(event: string, data: unknown, runId?: string): void;
}

const SUPPORTED_PROVIDER_IDS = ["anthropic", "openai", "openai-codex"] as const;
const API_KEY_PROVIDERS = new Set(["anthropic", "openai"]);
const AUTH_URL_HOST_ALLOWLIST = new Set([
  "accounts.google.com",
  "auth.openai.com",
  "login.microsoftonline.com",
]);

function providerId(value: string): void {
  if (!(SUPPORTED_PROVIDER_IDS as readonly string[]).includes(value)) {
    throw new BridgeProtocolError("INVALID_ARGUMENT", "The provider is not supported by Cutterhoochee.");
  }
}

function expectedAuthType(id: string): AuthType {
  providerId(id);
  return id === "openai-codex" ? "oauth" : "api_key";
}

function authDestinationAllowed(url: string): boolean {
  try {
    const parsed = new URL(url);
    return parsed.protocol === "https:" && AUTH_URL_HOST_ALLOWLIST.has(parsed.hostname);
  } catch {
    return false;
  }
}

function makeApiKeyAuth(providerIdValue: string) {
  return {
    name: providerIdValue === "anthropic" ? "Anthropic API key" : "OpenAI API key",
    async login(interaction: { signal: AbortSignal; prompt(prompt: AuthPrompt): Promise<string> }): Promise<ApiKeyCredential> {
      const key = (await interaction.prompt({
        type: "secret",
        message: `Enter the ${providerIdValue === "anthropic" ? "Anthropic" : "OpenAI"} API key`,
        placeholder: "API key",
        signal: interaction.signal,
      })).trim();
      if (key.length === 0) throw new BridgeProtocolError("AUTH_REQUIRED", "An API key is required.");
      return { type: "api_key", key };
    },
    async check({ credential }: { credential?: ApiKeyCredential; signal: AbortSignal }) {
      if (credential?.type === "api_key" && typeof credential.key === "string" && credential.key.length > 0) {
        return { type: "api_key" as const, source: "stored" };
      }
      return undefined;
    },
    async resolve({ credential }: { credential?: ApiKeyCredential; signal: AbortSignal }) {
      if (credential?.type !== "api_key" || typeof credential.key !== "string" || credential.key.length === 0) {
        return undefined;
      }
      return { auth: { apiKey: credential.key }, source: "stored" };
    },
  };
}

function preserveOAuthAccountIdentity(
  oauth: NonNullable<Provider["auth"]["oauth"]>,
): NonNullable<Provider["auth"]["oauth"]> {
  return {
    ...oauth,
    async refresh(credential, signal) {
      const refreshed = await oauth.refresh(credential, signal);
      const accountId = credential.accountId;
      if (typeof accountId === "string" && accountId.length > 0 && refreshed.accountId === undefined) {
        return { ...refreshed, accountId };
      }
      return refreshed;
    },
  };
}

/**
 * Remove every built-in provider/auth path that is not explicitly approved for
 * this app.  Anthropic and OpenAI retain only app-store API-key auth; Codex
 * retains only the SDK's subscription OAuth implementation.
 */
export function restrictProviderMatrix(runtime: ModelRuntime): void {
  const originals = new Map<string, Provider>();
  for (const provider of runtime.getProviders()) originals.set(provider.id, provider);
  for (const id of originals.keys()) {
    if (!(SUPPORTED_PROVIDER_IDS as readonly string[]).includes(id)) runtime.unregisterProvider(id);
  }
  for (const id of SUPPORTED_PROVIDER_IDS) {
    const original = originals.get(id);
    if (original === undefined) continue;
    const auth = id === "openai-codex"
      ? original.auth.oauth === undefined ? undefined : { oauth: preserveOAuthAccountIdentity(original.auth.oauth) }
      : { apiKey: makeApiKeyAuth(id) };
    if (auth === undefined) {
      runtime.unregisterProvider(id);
      continue;
    }
    const restricted = createProvider({
      id,
      name: original.name,
      baseUrl: original.baseUrl,
      headers: original.headers,
      auth,
      models: original.getModels(),
      // Delegate only the stream surface.  The original provider's auth object
      // is never retained for anthropic/openai and is replaced above.
      api: {
        stream: (model, context, options) => original.stream(model, context, options),
        streamSimple: (model, context, options) => original.streamSimple(model, context, options),
      },
    });
    runtime.unregisterProvider(id);
    runtime.registerNativeProvider(restricted);
  }
}
type AuthOperation = {
  authOperationId: string;
  providerId: string;
  authType: AuthType;
};

type PendingPrompt = {
  resolve: (value: string) => void;
  reject: (error: unknown) => void;
  signal: AbortSignal;
  operation: AuthOperation;
};

export class ProvidersRuntime {
  private readonly pendingPrompts = new Map<string, PendingPrompt>();
  private selected?: SelectedModel;
  private activeAuth?: AuthOperation;

  constructor(
    private readonly runtime: ModelRuntime,
    private readonly credentials: BridgeCredentialStore,
    private readonly events: ProviderEventSink,
    private readonly selectionPath: string,
    private readonly onAccountChanged: (providerId: string) => Promise<void>,
  ) {}

  async initialize(): Promise<void> {
    await mkdir(dirname(this.selectionPath), { recursive: true });
    try {
      const parsed: unknown = JSON.parse(await readFile(this.selectionPath, "utf8"));
      if (typeof parsed === "object" && parsed !== null) {
        const object = parsed as Record<string, unknown>;
        if (typeof object.providerId === "string" && typeof object.modelId === "string") {
          providerId(object.providerId);
          if (this.runtime.getModel(object.providerId, object.modelId) !== undefined) {
            this.selected = { providerId: object.providerId, modelId: object.modelId };
          }
        }
      }
    } catch {
      // Missing/corrupt selection is treated as no selection; it never changes
      // credentials or silently falls back to another model.
    }
  }
  async handle(action: ProvidersAction, signal?: AbortSignal, authOperationId?: string): Promise<ProvidersReply> {
    switch (action.action) {
      case "list": return this.list(signal);
      case "models": return this.models(action.providerId, signal);
      case "login": return this.login(action.providerId, action.authType, action.sessionOnly === true, signal, authOperationId);
      case "answer": return this.answer(action.promptId, action.value, authOperationId);
      case "logout": return this.logout(action.providerId, signal);
      case "select": return this.select(action.providerId, action.modelId, signal);
      case "refresh": return this.refresh(signal);
    }
  }

  getSelected(): SelectedModel | undefined {
    return this.selected === undefined ? undefined : { ...this.selected };
  }

  /**
   * Return native account metadata for one provider. Identity is never
   * derived from the SDK credential object in Node.
   */
  async accountIdentity(providerIdValue: string, signal?: AbortSignal): Promise<string | null> {
    providerId(providerIdValue);
    return this.credentials.accountId(providerIdValue, { signal });
  }

  /**
   * Resolve the selected provider's native recipient identity. Missing
   * metadata deliberately produces no identity so evidence callers fail
   * closed rather than inventing a recipient.
   */
  async selectedAccount(signal?: AbortSignal): Promise<ProviderAccountIdentity | undefined> {
    const selected = this.selected;
    if (selected === undefined) return undefined;
    const accountId = await this.accountIdentity(selected.providerId, signal);
    return accountId === null ? undefined : { providerId: selected.providerId, accountId };
  }

  private async list(signal?: AbortSignal): Promise<ProvidersReply> {
    const metadata = await this.credentials.list({ signal });
    const metadataByProvider = new Map(metadata.map((entry) => [entry.providerId, entry]));
    const providers: ProviderStatus[] = [];
    for (const id of SUPPORTED_PROVIDER_IDS) {
      const provider = this.runtime.getProvider(id);
      if (provider === undefined) continue;
      const auth = await this.runtime.checkAuth(id, { signal });
      const native = metadataByProvider.get(id);
      const nativeType = native?.type;
      const accountId = native?.accountId ?? null;
      const configured = auth !== undefined && accountId !== null && nativeType === auth.type;
      const statusAuthType = configured ? nativeType : undefined;
      providers.push({
        id,
        name: provider.name,
        configured,
        accountId: configured ? accountId : null,
        ...(statusAuthType === undefined ? {} : { authType: statusAuthType }),
        modelCount: provider.getModels().length,
      });
    }
    return { action: "list", providers, ...(this.selected === undefined ? {} : { selected: this.selected }) };
  }

  private async models(id: string, signal?: AbortSignal): Promise<ProvidersReply> {
    providerId(id);
    const models = await this.runtime.getAvailable(id, { signal });
    return {
      action: "models",
      providerId: id,
      models: models.map((model) => this.modelInfo(model)),
    };
  }

  private async login(
    id: string,
    type: AuthType,
    _sessionOnly: boolean,
    signal: AbortSignal | undefined,
    authOperationId: string | undefined,
  ): Promise<ProvidersReply> {
    providerId(id);
    if (type !== expectedAuthType(id)) {
      throw new BridgeProtocolError("INVALID_ARGUMENT", `The ${id} provider only supports ${expectedAuthType(id)} authentication.`);
    }
    if (authOperationId === undefined || authOperationId.trim().length === 0 || authOperationId.length > 256) {
      throw new BridgeProtocolError("INVALID_ARGUMENT", "The native authentication operation is missing.");
    }
    if (this.activeAuth !== undefined) {
      throw new BridgeProtocolError("BUSY", "Another provider authentication is already in progress.");
    }
    const operation: AuthOperation = { authOperationId, providerId: id, authType: type };
    this.activeAuth = operation;
    this.credentials.setRunId(operation.authOperationId);
    try {
      const interaction = this.interaction(operation, signal);
      await this.runtime.login(id, type, interaction);
      const accountId = await this.credentials.accountId(id, { signal });
      if (accountId === null) {
        throw new BridgeProtocolError("AUTH_REQUIRED", "The connected provider account identity is unavailable.");
      }
      await this.onAccountChanged(id);
      return { action: "login", providerId: id, type, configured: true };
    } finally {
      this.credentials.setRunId(undefined);
      this.cancelPrompts(operation, new BridgeProtocolError("JOB_CANCELLED", "Authentication is no longer pending."));
      if (this.activeAuth?.authOperationId === operation.authOperationId) this.activeAuth = undefined;
    }
  }
  private async answer(promptId: string, value: string, authOperationId: string | undefined): Promise<ProvidersReply> {
    if (promptId.length === 0 || promptId.length > 256 || value.length > 16384) {
      throw new BridgeProtocolError("INVALID_ARGUMENT", "The authentication answer is invalid.");
    }
    if (authOperationId === undefined || authOperationId.trim().length === 0 || authOperationId.length > 256) {
      throw new BridgeProtocolError("INVALID_ARGUMENT", "The native authentication operation is missing.");
    }
    const pending = this.pendingPrompts.get(promptId);
    if (pending === undefined) throw new BridgeProtocolError("STALE_SESSION", "The authentication prompt is no longer pending.");
    if (pending.operation.authOperationId !== authOperationId) {
      throw new BridgeProtocolError("STALE_SESSION", "The authentication answer belongs to another login.");
    }
    this.pendingPrompts.delete(promptId);
    pending.resolve(value);
    return { action: "answer", promptId, accepted: true };
  }

  private cancelPrompts(operation: AuthOperation | undefined, error: unknown): void {
    for (const [promptId, pending] of this.pendingPrompts) {
      if (operation !== undefined && pending.operation.authOperationId !== operation.authOperationId) continue;
      this.pendingPrompts.delete(promptId);
      pending.reject(error);
    }
  }

  private async logout(id: string, signal?: AbortSignal): Promise<ProvidersReply> {
    providerId(id);
    this.cancelPrompts(undefined, new BridgeProtocolError("JOB_CANCELLED", "Authentication was cancelled by logout."));
    if (this.activeAuth?.providerId === id) this.activeAuth = undefined;
    await this.runtime.logout(id, { signal });
    if (this.selected?.providerId === id) this.selected = undefined;
    await this.onAccountChanged(id);
    return { action: "logout", providerId: id, configured: false };
  }

  private async select(id: string, modelId: string, signal?: AbortSignal): Promise<ProvidersReply> {
    providerId(id);
    if (modelId.trim().length === 0) throw new BridgeProtocolError("INVALID_ARGUMENT", "modelId is required.");
    const available = await this.runtime.getAvailable(id, { signal });
    const model = available.find((candidate) => candidate.id === modelId);
    if (model === undefined) {
      const configured = await this.runtime.checkAuth(id, { signal });
      if (configured === undefined) throw new BridgeProtocolError("AUTH_REQUIRED", `Connect ${id} before selecting a model.`);
      throw new BridgeProtocolError("PROVIDER_ERROR", "The selected model is not currently available.");
    }
    this.selected = { providerId: id, modelId };
    await writeFile(this.selectionPath, `${JSON.stringify(this.selected)}\n`, { mode: 0o600 });
    await this.onAccountChanged(id);
    return { action: "select", selected: this.selected };
  }

  private async refresh(signal?: AbortSignal): Promise<ProvidersReply> {
    const refreshed = await this.runtime.refresh({ allowNetwork: false, signal });
    if (refreshed.errors.size > 0) {
      throw new BridgeProtocolError("PROVIDER_ERROR", "One or more provider catalogs could not be refreshed.");
    }
    return { action: "refresh", refreshed: true };
  }

  private interaction(operation: AuthOperation, signal?: AbortSignal): AuthInteraction {
    const authSignal = signal ?? new AbortController().signal;
    return {
      signal: authSignal,
      prompt: (prompt) => this.prompt(prompt, authSignal, operation),
      notify: (event) => this.notify(event, operation),
    };
  }

  private prompt(prompt: AuthPrompt, signal: AbortSignal, operation: AuthOperation): Promise<string> {
    if (this.activeAuth !== operation) {
      return Promise.reject(new BridgeProtocolError("STALE_SESSION", "The authentication operation is no longer active."));
    }
    const promptId = crypto.randomUUID();
    const promptData = prompt.type === "select"
      ? { type: prompt.type, message: prompt.message, options: prompt.options }
      : {
          type: prompt.type,
          message: prompt.message,
          ...(prompt.placeholder === undefined ? {} : { placeholder: prompt.placeholder }),
        };
    this.events.emit(
      "providers_auth_prompt",
      { authOperationId: operation.authOperationId, providerId: operation.providerId, authType: operation.authType, promptId, prompt: promptData },
      operation.authOperationId,
    );
    return new Promise<string>((resolve, reject) => {
      const onAbort = () => {
        this.pendingPrompts.delete(promptId);
        reject(new BridgeProtocolError("JOB_CANCELLED", "Authentication was cancelled."));
      };
      signal.addEventListener("abort", onAbort, { once: true });
      this.pendingPrompts.set(promptId, {
        resolve: (value) => { signal.removeEventListener("abort", onAbort); resolve(value); },
        reject: (error) => { signal.removeEventListener("abort", onAbort); reject(error); },
        signal,
        operation,
      });
    });
  }

  private notify(event: AuthEvent, operation: AuthOperation): void {
    if (this.activeAuth !== operation) {
      throw new BridgeProtocolError("STALE_SESSION", "The authentication operation is no longer active.");
    }
    if (
      (event.type === "auth_url" && !authDestinationAllowed(event.url)) ||
      (event.type === "device_code" && !authDestinationAllowed(event.verificationUri)) ||
      (event.type === "info" && event.links?.some((link) => !authDestinationAllowed(link.url)) === true)
    ) {
      throw new BridgeProtocolError("PROVIDER_ERROR", "The provider returned an untrusted authentication destination.");
    }
    this.events.emit(
      "providers_auth_event",
      { authOperationId: operation.authOperationId, providerId: operation.providerId, authType: operation.authType, ...event },
      operation.authOperationId,
    );
  }

  private modelInfo(model: Model<Api>): ProviderModel {
    return {
      id: model.id,
      name: model.name,
      reasoning: model.reasoning,
      input: model.input,
      contextWindow: model.contextWindow,
      maxTokens: model.maxTokens,
    };
  }
}

export { SUPPORTED_PROVIDER_IDS };
export type { ApiKeyCredential, Credential, OAuthCredential };
