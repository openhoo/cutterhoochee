import type { Credential, CredentialInfo, CredentialStore } from "@earendil-works/pi-ai";

import { BridgeProtocolError, type BridgeResponseMessage, type NdjsonBridge } from "./bridge.js";

const PROVIDER_IDS = new Set(["anthropic", "openai", "openai-codex"]);
const MAX_ACCOUNT_ID_LENGTH = 512;

function assertProvider(providerId: string): void {
  if (!PROVIDER_IDS.has(providerId)) {
    throw new BridgeProtocolError("INVALID_ARGUMENT", "The provider is not supported by Cutterhoochee.");
  }
}

function isCredential(value: unknown): value is Credential {
  if (typeof value !== "object" || value === null || Array.isArray(value) || !("type" in value)) return false;
  if (value.type === "api_key") {
    return !("key" in value) || value.key === undefined || typeof value.key === "string";
  }
  if (value.type !== "oauth" || !("refresh" in value) || !("access" in value) || !("expires" in value)) return false;
  return typeof value.refresh === "string" && typeof value.access === "string" && typeof value.expires === "number" && Number.isFinite(value.expires);
}

export type NativeCredentialInfo = CredentialInfo & {
  /** Native-only stable recipient identity; never credential material. */
  accountId: string | null;
};

function isCredentialInfo(value: unknown): value is NativeCredentialInfo {
  if (typeof value !== "object" || value === null || Array.isArray(value)) return false;
  if (!("providerId" in value) || !("type" in value) || !("accountId" in value)) return false;
  return (
    typeof value.providerId === "string" &&
    PROVIDER_IDS.has(value.providerId) &&
    (value.type === "api_key" || value.type === "oauth") &&
    (value.accountId === null || (typeof value.accountId === "string" && value.accountId.length > 0 && value.accountId.length <= MAX_ACCOUNT_ID_LENGTH))
  );
}

function credentialOrUndefined(value: unknown, field: string): Credential | undefined {
  if (value === undefined || value === null) return undefined;
  if (!isCredential(value)) {
    throw new BridgeProtocolError("SCHEMA_UNSUPPORTED", `The credential response field ${field} is invalid.`);
  }
  return value;
}

function responseData(response: BridgeResponseMessage): unknown {
  if (!response.ok) throw new BridgeProtocolError(response.error.code, response.error.message);
  return response.data;
}

/** CredentialStore backed exclusively by the native keyring bridge. */
export class BridgeCredentialStore implements CredentialStore {
  private readonly bridge: NdjsonBridge;
  private runId: string | undefined;

  constructor(bridge: NdjsonBridge, runId?: string) {
    this.bridge = bridge;
    this.runId = runId;
  }

  setRunId(runId: string | undefined): void {
    this.runId = runId;
  }

  async read(providerId: string, options: { signal?: AbortSignal } = {}): Promise<Credential | undefined> {
    assertProvider(providerId);
    const data = responseData(await this.call("credential_read", { providerId }, options.signal));
    if (data === undefined || data === null) return undefined;
    if (typeof data !== "object" || Array.isArray(data) || !("credential" in data)) {
      throw new BridgeProtocolError("SCHEMA_UNSUPPORTED", "The native credential response is invalid.");
    }
    return credentialOrUndefined(data.credential, "credential");
  }

  async list(options: { signal?: AbortSignal } = {}): Promise<readonly NativeCredentialInfo[]> {
    const data = responseData(await this.call("credential_list", {}, options.signal));
    if (!Array.isArray(data) || !data.every(isCredentialInfo)) {
      throw new BridgeProtocolError("SCHEMA_UNSUPPORTED", "The native credential list is invalid.");
    }
    return data;
  }

  /**
   * Read only the native recipient identity for one provider. The list
   * metadata is the sole source of this value; this sidecar never hashes or
   * otherwise derives identity from credential material.
   */
  async accountId(providerId: string, options: { signal?: AbortSignal } = {}): Promise<string | null> {
    assertProvider(providerId);
    const metadata = (await this.list(options)).find((entry) => entry.providerId === providerId);
    return metadata?.accountId ?? null;
  }

  async modify(
    providerId: string,
    fn: (current: Credential | undefined) => Promise<Credential | undefined>,
    options: { signal?: AbortSignal } = {},
  ): Promise<Credential | undefined> {
    assertProvider(providerId);
    const leaseData = responseData(await this.call("credential_lease_acquire", { providerId }, options.signal));
    if (typeof leaseData !== "object" || leaseData === null || Array.isArray(leaseData) || !("leaseId" in leaseData) || !("authGeneration" in leaseData)) {
      throw new BridgeProtocolError("SCHEMA_UNSUPPORTED", "The native credential lease is invalid.");
    }
    const leaseId = leaseData.leaseId;
    const authGeneration = leaseData.authGeneration;
    if (typeof leaseId !== "string" || leaseId.length === 0 || !Number.isSafeInteger(authGeneration)) {
      throw new BridgeProtocolError("SCHEMA_UNSUPPORTED", "The native credential lease is invalid.");
    }
    const current = credentialOrUndefined("current" in leaseData ? leaseData.current : undefined, "current");
    try {
      const next = await this.withAbort(fn(current), options.signal);
      if (next !== undefined && !isCredential(next)) {
        throw new BridgeProtocolError("INVALID_ARGUMENT", "The credential callback returned an invalid value.");
      }
      if (next === undefined) {
        await this.call("credential_lease_release", { leaseId });
        return current;
      }
      const committedData = responseData(await this.call("credential_lease_commit", { leaseId, authGeneration, credential: next }, options.signal));
      if (committedData === undefined || committedData === null) return next;
      if (isCredential(committedData)) return committedData;
      if (typeof committedData === "object" && !Array.isArray(committedData) && "credential" in committedData) {
        return credentialOrUndefined(committedData.credential, "credential") ?? next;
      }
      throw new BridgeProtocolError("SCHEMA_UNSUPPORTED", "The native credential commit response is invalid.");
    } catch (error) {
      try {
        await this.call("credential_lease_release", { leaseId });
      } catch {
        // A retired sidecar expires its native lease.
      }
      throw error;
    }
  }

  async delete(providerId: string, options: { signal?: AbortSignal } = {}): Promise<void> {
    assertProvider(providerId);
    responseData(await this.call("credential_delete", { providerId }, options.signal));
  }

  private async call(method: string, params: unknown, signal?: AbortSignal): Promise<BridgeResponseMessage> {
    const request = this.bridge.request(method, params, this.runId === undefined ? {} : { runId: this.runId });
    return this.withAbort(request, signal);
  }

  private async withAbort<T>(promise: Promise<T>, signal?: AbortSignal): Promise<T> {
    if (signal === undefined) return promise;
    if (signal.aborted) throw new BridgeProtocolError("JOB_CANCELLED", "The credential operation was cancelled.");
    return new Promise<T>((resolve, reject) => {
      const onAbort = () => reject(new BridgeProtocolError("JOB_CANCELLED", "The credential operation was cancelled."));
      signal.addEventListener("abort", onAbort, { once: true });
      promise.then(
        (value) => { signal.removeEventListener("abort", onAbort); resolve(value); },
        (error: unknown) => { signal.removeEventListener("abort", onAbort); reject(error); },
      );
    });
  }
}

export function providerIds(): readonly string[] {
  return ["anthropic", "openai", "openai-codex"];
}

export type { Credential, CredentialInfo };
