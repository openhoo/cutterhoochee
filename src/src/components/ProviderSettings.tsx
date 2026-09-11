import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { Check, ExternalLink, KeyRound, LogIn, LogOut, RefreshCw, Shield, Sparkles, X } from "lucide-react";
import { listen } from "@tauri-apps/api/event";

import type { EditorClient, ProjectSnapshot } from "@cutterhoochee/shared";
import { Button } from "@/components/ui/button";
import { callNative, eventData, parseEvent, record, replyPayload, stringValue } from "@/lib/native";

type AuthType = "api_key" | "oauth";
type ProviderSummary = { id: string; label: string; connected: boolean; authType?: AuthType; modelCount: number; modelId?: string };
type ModelSummary = { id: string; label: string; image: boolean; input: string[] };
type PromptState = { id: string; text: string; secret: boolean; link?: string };

export function ProviderSettings({ client, snapshot: _snapshot, onNotice, onClose }: { client: EditorClient; snapshot: ProjectSnapshot; onNotice: (notice: string) => void; onClose: () => void }) {
  const [providers, setProviders] = useState<ProviderSummary[]>([]);
  const [selectedProvider, setSelectedProvider] = useState("anthropic");
  const [selectedModel, setSelectedModel] = useState("");
  const [models, setModels] = useState<ModelSummary[]>([]);
  const [authType, setAuthType] = useState<AuthType>("api_key");
  const [secret, setSecret] = useState("");
  const [sessionOnly, setSessionOnly] = useState(false);
  const [prompt, setPrompt] = useState<PromptState | null>(null);
  const [promptValue, setPromptValue] = useState("");
  const [authMessage, setAuthMessage] = useState<string | null>(null);
  const [busy, setBusy] = useState<string | null>(null);
  const secretRef = useRef("");

  const selected = useMemo(() => providers.find((provider) => provider.id === selectedProvider), [providers, selectedProvider]);
  const selectedIsCodex = selectedProvider === "openai-codex";

  const refresh = useCallback(async () => {
    setBusy("refresh");
    try {
      const reply = await callNative(client, { method: "providers", params: { action: "list" } });
      const data = replyPayload(reply);
      const values = Array.isArray(data.providers) ? data.providers : [];
      const next = values.map(readProvider).filter((provider): provider is ProviderSummary => provider !== null);
      const selectedModelData = record(data.selected);
      setProviders(next.map((provider) => ({
        ...provider,
        modelId: provider.id === stringValue(selectedModelData.providerId) ? stringValue(selectedModelData.modelId) || undefined : provider.modelId,
      })));
      if (next.length > 0 && !next.some((provider) => provider.id === selectedProvider)) setSelectedProvider(next[0].id);
      if (stringValue(selectedModelData.providerId) === selectedProvider) setSelectedModel(stringValue(selectedModelData.modelId));
    } catch (error) {
      onNotice(error instanceof Error ? error.message : "Provider list could not be loaded.");
    } finally {
      setBusy(null);
    }
  }, [client, onNotice, selectedProvider]);

  const loadModels = useCallback(async (providerId: string) => {
    setBusy("models");
    try {
      const reply = await callNative(client, { method: "providers", params: { action: "models", providerId } });
      const data = replyPayload(reply);
      const values = Array.isArray(data.models) ? data.models : [];
      const next = values.map(readModel).filter((model): model is ModelSummary => model !== null);
      setModels(next);
      setSelectedModel((current) => current && next.some((model) => model.id === current) ? current : next[0]?.id ?? "");
    } catch (error) {
      onNotice(error instanceof Error ? error.message : "Model catalog could not be loaded.");
    } finally {
      setBusy(null);
    }
  }, [client, onNotice]);

  const answerPrompt = useCallback(async (promptId: string, value: string) => {
    if (!promptId || !value) return;
    try {
      await callNative(client, { method: "providers", params: { action: "answer", promptId, value } });
      setPrompt(null);
      setPromptValue("");
    } catch (error) {
      onNotice(error instanceof Error ? error.message : "Authentication answer failed.");
    }
  }, [client, onNotice]);

  useEffect(() => { void refresh(); }, [refresh]);
  useEffect(() => {
    setAuthType(selectedIsCodex ? "oauth" : "api_key");
    setSecret("");
    secretRef.current = "";
    setPrompt(null);
    setPromptValue("");
    if (selectedProvider) void loadModels(selectedProvider);
  }, [loadModels, selectedIsCodex, selectedProvider]);

  useEffect(() => {
    if (!(typeof window !== "undefined" && "__TAURI_INTERNALS__" in window)) return;
    let disposed = false;
    let unlisten: (() => void) | undefined;
    void listen<unknown>("cutterhoochee://event", (event) => {
      if (disposed) return;
      const parsed = parseEvent(event.payload);
      if (!parsed) return;
      const kind = parsed.kind.toLowerCase();
      if (!kind.includes("providers_auth_prompt") && !kind.includes("providers_auth_event") && !kind.includes("auth_prompt") && !kind.includes("auth_event")) return;
      const data = eventData(parsed);
      const eventProviderId = stringValue(data.providerId);
      if (eventProviderId && eventProviderId !== selectedProvider) return;
      if (kind.includes("prompt")) {
        const id = stringValue(data.promptId);
        const detail = record(data.prompt);
        const type = stringValue(detail.type).toLowerCase();
        const message = stringValue(detail.message, "Complete provider authentication");
        const link = stringValue(detail.url || detail.verificationUri) || undefined;
        const isSecret = type.includes("key") || type.includes("secret") || type.includes("password") || type === "token";
        if (!id) return;
        setPrompt({ id, text: message, secret: isSecret, link });
        if (isSecret && secretRef.current) {
          const value = secretRef.current;
          secretRef.current = "";
          setSecret("");
          void answerPrompt(id, value);
        }
      } else {
        const message = stringValue(data.message || data.instructions || data.type);
        const link = stringValue(data.url || data.verificationUri);
        if (message || link) setAuthMessage([message || "Continue provider authentication", link ? `Open ${link}` : ""].filter(Boolean).join(" · "));
      }
    }).then((dispose) => { if (disposed) dispose(); else unlisten = dispose; }).catch(() => undefined);
    return () => { disposed = true; unlisten?.(); };
  }, [answerPrompt, selectedProvider]);

  const login = async () => {
    const submittedSecret = authType === "api_key" ? secret.trim() : "";
    secretRef.current = submittedSecret;
    setSecret("");
    setAuthMessage(null);
    setBusy("login");
    try {
      await callNative(client, { method: "providers", params: { action: "login", providerId: selectedProvider, authType: selectedIsCodex ? "oauth" : authType, sessionOnly } });
      secretRef.current = "";
      await refresh();
    } catch (error) {
      secretRef.current = "";
      const text = error instanceof Error ? error.message : "Provider login failed.";
      if (/keyring|secret service|credential store|session-only/i.test(text)) {
        setAuthMessage("The native credential store is unavailable. Choose “Session-only credential” and retry; no plaintext credential was persisted.");
      }
      onNotice(text);
    } finally {
      secretRef.current = "";
      setSecret("");
      setBusy(null);
    }
  };

  const answer = async () => {
    if (!prompt?.id || !promptValue) return;
    const value = promptValue;
    setPromptValue("");
    setBusy("answer");
    await answerPrompt(prompt.id, value);
    setBusy(null);
    if (!prompt?.secret) await refresh();
  };

  const logout = async () => {
    setBusy("logout");
    setPrompt(null);
    setPromptValue("");
    try { await callNative(client, { method: "providers", params: { action: "logout", providerId: selectedProvider } }); await refresh(); } catch (error) { onNotice(error instanceof Error ? error.message : "Provider logout failed."); } finally { setBusy(null); }
  };

  const selectModel = async (modelId: string) => {
    setSelectedModel(modelId);
    try { await callNative(client, { method: "providers", params: { action: "select", providerId: selectedProvider, modelId } }); await refresh(); } catch (error) { onNotice(error instanceof Error ? error.message : "Model selection failed."); }
  };

  return <div className="settings-dialog"><div className="settings-header"><div><p className="eyebrow">Assistant</p><h2>Provider settings</h2><p>Connect an account only when you want AI assistance. Credentials stay in the native keyring or explicitly session-only memory.</p></div><Button variant="ghost" size="icon" aria-label="Close provider settings" onClick={onClose}><X aria-hidden="true" /></Button></div><div className="settings-grid"><section className="settings-provider-list"><div className="section-label"><span>Connections</span><Button variant="ghost" size="icon" aria-label="Refresh providers" onClick={() => void refresh()}><RefreshCw className={busy === "refresh" ? "spin" : ""} aria-hidden="true" /></Button></div>{providers.length === 0 ? <div className="empty-panel compact"><Shield aria-hidden="true" /><span>No provider status available yet.</span></div> : providers.map((provider) => <button type="button" className={provider.id === selectedProvider ? "provider-row selected" : "provider-row"} key={provider.id} onClick={() => setSelectedProvider(provider.id)}><span className="provider-icon">{provider.id === "openai-codex" ? "OC" : provider.id.slice(0, 2).toUpperCase()}</span><span><strong>{provider.label}</strong><small>{provider.connected ? `${provider.authType || "Connected"} · ${provider.modelCount} models` : "Not connected"}</small></span>{provider.connected ? <Check aria-hidden="true" /> : null}</button>)}</section><section className="settings-form"><div className="settings-section-title"><div><span className="eyebrow">Connection</span><h3>{selected?.label || selectedProvider}</h3></div>{selected?.connected ? <Button variant="ghost" size="sm" disabled={busy !== null} onClick={() => void logout()}><LogOut aria-hidden="true" />Disconnect</Button> : null}</div><label className="field-group"><span className="field-label">Authentication</span><select value={authType} onChange={(event) => setAuthType(event.target.value as AuthType)} disabled={selectedIsCodex}><option value="api_key">API key</option><option value="oauth">OAuth sign-in</option></select></label>{selectedIsCodex ? <p className="field-help">Codex sign-in is a separate subscription provider and uses its own PKCE flow.</p> : authType === "api_key" ? <label className="field-group"><span className="field-label">API key</span><input type="password" value={secret} onChange={(event) => { setSecret(event.target.value); secretRef.current = event.target.value; }} placeholder="Paste a key for this session" autoComplete="off" /></label> : <p className="field-help">The native provider flow validates its HTTPS destination. Codes never enter chat.</p>}<label className="toggle-row"><span>Session-only credential</span><input type="checkbox" checked={sessionOnly} onChange={(event) => setSessionOnly(event.target.checked)} /><small>Keep this credential in native memory only; retry this mode if the OS keyring is unavailable.</small></label><Button onClick={() => void login()} disabled={busy !== null || (authType === "api_key" && !secretRef.current && !selected?.connected)}><LogIn aria-hidden="true" />{busy === "login" ? "Connecting…" : selected?.connected ? "Refresh connection" : "Connect"}</Button>{authMessage ? <p className="panel-message" role="status">{authMessage}</p> : null}{prompt ? <div className="auth-prompt"><ExternalLink aria-hidden="true" /><strong>{prompt.text}</strong>{prompt.link ? <code>{prompt.link}</code> : null}<input type={prompt.secret ? "password" : "text"} value={promptValue} onChange={(event) => setPromptValue(event.target.value)} autoComplete="off" placeholder={prompt.secret ? "Enter secret" : "Enter the requested value"} /><Button size="sm" disabled={!promptValue || busy === "answer"} onClick={() => void answer()}>Continue</Button></div> : null}<div className="model-section"><div className="section-label"><span>Model</span><Sparkles aria-hidden="true" /></div><select value={selectedModel} onChange={(event) => void selectModel(event.target.value)} disabled={busy === "models" || models.length === 0}><option value="">{busy === "models" ? "Loading models…" : models.length === 0 ? "No models available" : "Choose a model"}</option>{models.map((model) => <option value={model.id} key={model.id}>{model.label}{model.image ? " · image evidence" : ""}</option>)}</select><p className="field-help">Only models reported by the connected provider are shown. Image input support is derived from the provider’s declared capabilities.</p></div></section></div><div className="settings-footer"><KeyRound aria-hidden="true" /><span>Claude connections use API keys only. OpenAI Codex is not reused as an OpenAI API key.</span></div></div>;
}

function readProvider(value: unknown): ProviderSummary | null {
  const item = record(value);
  const id = stringValue(item.id);
  if (!id) return null;
  const rawAuth = stringValue(item.authType);
  const authType: AuthType | undefined = rawAuth === "api_key" || rawAuth === "oauth" ? rawAuth : undefined;
  return { id, label: stringValue(item.name, id), connected: Boolean(item.configured), authType, modelCount: typeof item.modelCount === "number" ? item.modelCount : 0 };
}

function readModel(value: unknown): ModelSummary | null {
  const item = record(value);
  const id = stringValue(item.id);
  if (!id) return null;
  const input = Array.isArray(item.input) ? item.input.filter((entry): entry is string => typeof entry === "string") : [];
  return { id, label: stringValue(item.name, id), image: input.some((entry) => entry.toLowerCase() === "image"), input };
}
