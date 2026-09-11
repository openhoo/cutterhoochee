import { useEffect, useMemo, useRef, useState } from "react";
import type { FormEvent } from "react";
import { Bot, Check, ChevronDown, CircleStop, KeyRound, Loader2, MessageCircle, RotateCcw, Send, ShieldCheck, Square, Wrench, X } from "lucide-react";

import type { EditorCallContext, EditorClient, ProjectSnapshot } from "@cutterhoochee/shared";
import { eventData, record, replyPayload, stringValue, type EventPayload } from "@/lib/native";
import { Button } from "@/components/ui/button";

type ChatMessage = { id: string; role: "user" | "assistant" | "system"; text: string; pending?: boolean; tool?: string; toolCallId?: string; status?: string; usage?: string };
type ChatRole = ChatMessage["role"];
type EvidenceRequest = { providerId: string; accountId: string; generation: number; projectId: string; scope?: string };

function chatRole(value: unknown): ChatRole {
  if (value === "user") return "user";
  if (value === "system") return "system";
  return "assistant";
}

function usageLabel(value: unknown): string | undefined {
  const usage = record(value);
  const input = typeof usage.input === "number" ? usage.input : typeof usage.inputTokens === "number" ? usage.inputTokens : typeof usage.input_tokens === "number" ? usage.input_tokens : undefined;
  const output = typeof usage.output === "number" ? usage.output : typeof usage.outputTokens === "number" ? usage.outputTokens : typeof usage.output_tokens === "number" ? usage.output_tokens : undefined;
  if (input === undefined && output === undefined) return undefined;
  return `Tokens · ${input ?? "?"} in / ${output ?? "?"} out`;
}

function evidenceScope(value: unknown): string | undefined {
  const scope = record(value);
  const parts = [
    stringValue(scope.workspaceId) ? `workspace ${stringValue(scope.workspaceId)}` : "",
    stringValue(scope.projectId) ? `project ${stringValue(scope.projectId)}` : "",
    typeof scope.generation === "number" ? `generation ${scope.generation}` : "",
  ].filter(Boolean);
  return parts.length > 0 ? parts.join(" · ") : stringValue(value) || undefined;
}

type NativeScope = EditorCallContext;

function sameScope(left: NativeScope, right: NativeScope): boolean {
  return left.generation === right.generation && left.projectId === right.projectId;
}

function eventInScope(event: EventPayload, scope: NativeScope): boolean {
  return event.generation === scope.generation && event.projectId === scope.projectId;
}

export function ChatPanel({ client, snapshot, eventLog, onRefresh, onNotice, onProviderSettings }: { client: EditorClient; snapshot: ProjectSnapshot; eventLog: EventPayload[]; onRefresh: () => Promise<void>; onNotice: (notice: string) => void; onProviderSettings: () => void }) {
  const nativeContext = client.getContext();
  const [messages, setMessages] = useState<ChatMessage[]>([]);
  const [draft, setDraft] = useState("");
  const [sending, setSending] = useState(false);
  const [evidence, setEvidence] = useState<EvidenceRequest | null>(null);
  const [expandedTools, setExpandedTools] = useState<Record<string, boolean>>({});
  const processedEvents = useRef<Set<EventPayload> | null>(null);
  const historyScope = useRef<NativeScope | null>(null);
  const mounted = useRef(false);
  const scopeRef = useRef(nativeContext);
  scopeRef.current = nativeContext;
  const isCurrentScope = (expected: NativeScope) => (
    mounted.current && sameScope(scopeRef.current, expected) && sameScope(client.getContext(), expected)
  );

  useEffect(() => {
    mounted.current = true;
    return () => {
      mounted.current = false;
    };
  }, []);

  useEffect(() => {
    const projectId = snapshot.document.projectId;
    const requestScope = client.getContext();
    if (requestScope.projectId !== projectId) return;
    historyScope.current = requestScope;
    let cancelled = false;
    void client.call({ method: "assistant", params: { action: "history" } }, requestScope).then((reply) => {
      if (
        cancelled ||
        !historyScope.current ||
        !sameScope(historyScope.current, requestScope) ||
        !isCurrentScope(requestScope)
      ) return;
      const data = replyPayload(reply);
      const history = Array.isArray(data.messages) ? data.messages : [];
      const loaded = history.map((value, index): ChatMessage => {
        const item = record(value);
        const tool = stringValue(item.toolName || item.tool) || undefined;
        const role = tool ? "system" : chatRole(item.role);
        const usage = usageLabel(item.usage);
        return { id: `history-${index}`, role, text: `${stringValue(item.text || item.content)}${usage ? `\n${usage}` : ""}`, tool, status: tool ? (item.isError ? "failed" : "complete") : undefined, usage };
      }).filter((message) => message.text || message.tool);
      setMessages((current) => current.some((message) => message.pending) ? current : loaded);
    }).catch(() => undefined);
    return () => {
      cancelled = true;
      if (historyScope.current && sameScope(historyScope.current, requestScope)) historyScope.current = null;
    };
  }, [client, nativeContext.generation, nativeContext.projectId, snapshot.document.projectId]);

  useEffect(() => {
    const eventScope = client.getContext();
    if (processedEvents.current === null) {
      processedEvents.current = new Set(eventLog);
      return;
    }
    const seen = processedEvents.current;
    const pendingEvents = eventLog.filter((event) => !seen.has(event) && eventInScope(event, eventScope));
    for (const activity of pendingEvents) {
      if (!isCurrentScope(eventScope)) break;
      const data = eventData(activity);
      const eventKind = activity.kind.toLowerCase();
      if (eventKind === "assistant_start") setSending(true);
      if (eventKind === "assistant_error") {
        setSending(false);
        const text = stringValue(record(data.error).message || data.message || data.error, "Assistant request failed.");
        setMessages((current) => [...current.filter((message) => !message.pending), { id: `${Date.now()}-error`, role: "system", text, status: "failed" }]);
      }
      const pendingEvidence = eventKind.includes("evidence") && (eventKind.includes("pending") || eventKind.includes("requested"));
      if (pendingEvidence) {
        const providerId = stringValue(data.providerId);
        const accountId = stringValue(data.accountId);
        const scope = record(data.scope);
        const context = client.getContext();
        if (providerId && accountId && typeof context.projectId === "string"
          && scope.generation === context.generation && scope.projectId === context.projectId) {
          setEvidence({ providerId, accountId, generation: context.generation, projectId: context.projectId, scope: evidenceScope(data.scope) });
        }
      }

      const assistantDelta = eventKind === "assistant_text_delta" || eventKind === "message_update" || eventKind === "assistant_message_delta";
      if (assistantDelta) {
        const text = stringValue(data.delta || data.text || data.content);
        if (text) {
          setMessages((current) => {
            const last = current[current.length - 1];
            if (last?.role === "assistant" && last.pending) {
              return [...current.slice(0, -1), { ...last, text: `${last.text}${text}` }];
            }
            return [...current, { id: `${Date.now()}-${current.length}`, role: "assistant", text, pending: true }];
          });
        }
      }
      const assistantMessage = eventKind === "assistant_message" || eventKind === "message_end";
      if (assistantMessage) {
        const text = stringValue(data.text || data.content);
        const usage = usageLabel(data.usage);
        if (text || usage) {
          setMessages((current) => {
            const last = current[current.length - 1];
            if (last?.role === "assistant" && last.pending) {
              return [
                ...current.slice(0, -1),
                {
                  ...last,
                  text: text || last.text,
                  pending: false,
                  usage: usage || last.usage,
                },
              ];
            }
            if (last?.role === "assistant" && !last.pending && (!text || last.text === text)) return current;
            return [...current, { id: `${Date.now()}-${current.length}`, role: "assistant", text: `${text}${usage ? `\n${usage}` : ""}`, pending: false, usage }];
          });
        }
      }

      const toolStart = eventKind === "assistant_tool_start" || eventKind.includes("tool_execution_start") || eventKind.includes("tool_start");
      if (toolStart) {
        const tool = stringValue(data.toolName || data.tool || data.name, "editor tool");
        const toolCallId = stringValue(data.toolCallId || data.id) || undefined;
        setMessages((current) => {
          if (toolCallId && current.some((message) => message.toolCallId === toolCallId)) return current;
          const previous = current.filter((message) => !message.pending || message.text).map((message) => message.pending ? { ...message, pending: false } : message);
          return [...previous, { id: toolCallId || `${Date.now()}-${current.length}`, role: "system", text: stringValue(data.text, `Running ${tool}`), tool, toolCallId, status: "running" }];
        });
      }
      const toolUpdate = eventKind === "assistant_tool_update" || eventKind.includes("tool_execution_update") || eventKind.includes("tool_update");
      if (toolUpdate) {
        const tool = stringValue(data.toolName || data.tool || data.name);
        const toolCallId = stringValue(data.toolCallId || data.id);
        const text = stringValue(data.text || data.message || data.progress);
        if (tool || toolCallId || text) setMessages((current) => current.map((message) => (message.status === "running" && ((toolCallId && message.toolCallId === toolCallId) || (!toolCallId && tool && message.tool === tool))) ? { ...message, text: text || message.text } : message));
      }
      const toolEnd = eventKind === "assistant_tool_end" || eventKind.includes("tool_execution_end") || eventKind.includes("tool_end");
      if (toolEnd) {
        const tool = stringValue(data.toolName || data.tool || data.name);
        const toolCallId = stringValue(data.toolCallId || data.id);
        const isError = Boolean(data.isError) || Boolean(data.error);
        setMessages((current) => current.map((message) => (message.status === "running" && ((toolCallId && message.toolCallId === toolCallId) || (!toolCallId && tool && message.tool === tool))) ? { ...message, status: isError ? "failed" : "complete", text: stringValue(data.text || data.error, `${message.tool || "Tool"} ${isError ? "failed" : "completed"}`) } : message));
      }
      if (eventKind === "assistant_settled" || eventKind === "assistant_end" || eventKind === "agent_settled" || eventKind === "agent_end") {
        setSending(false);
        setMessages((current) => current.filter((message) => !message.pending || message.text).map((message) => ({ ...message, pending: false, status: message.status === "running" ? "complete" : message.status })));
      }
    }
    if (isCurrentScope(eventScope)) processedEvents.current = new Set(eventLog);
  }, [client, eventLog, nativeContext.generation, nativeContext.projectId]);

  const toolEvents = useMemo(() => eventLog.filter((event) => eventInScope(event, nativeContext) && (event.kind.toLowerCase().includes("tool") || event.kind.toLowerCase().includes("job"))), [eventLog, nativeContext.generation, nativeContext.projectId]);

  const submit = async (event: FormEvent) => {
    event.preventDefault();
    const text = draft.trim();
    if (!text || sending) return;
    const requestScope = client.getContext();
    if (!isCurrentScope(requestScope) || requestScope.projectId !== snapshot.document.projectId) return;
    setDraft("");
    setSending(true);
    setMessages((current) => [...current, { id: `${Date.now()}-user`, role: "user", text }, { id: `${Date.now()}-pending`, role: "assistant", text: "", pending: true }]);
    try {
      await client.call({ method: "assistant", params: { action: "prompt", text } }, requestScope);
      if (!isCurrentScope(requestScope)) return;
    } catch (error) {
      if (!isCurrentScope(requestScope)) return;
      setDraft((current) => current || text);
      setSending(false);
      setMessages((current) => [...current.filter((message) => !message.pending), { id: `${Date.now()}-error`, role: "system", text: error instanceof Error ? error.message : "Assistant request failed.", status: "failed" }]);
      onNotice(error instanceof Error ? error.message : "Assistant request failed.");
    }
  };

  const stop = async () => {
    const requestScope = client.getContext();
    if (!isCurrentScope(requestScope)) return;
    try {
      await client.call({ method: "assistant", params: { action: "stop" } }, requestScope);
    } catch (error) {
      if (!isCurrentScope(requestScope)) return;
      onNotice(error instanceof Error ? error.message : "Assistant could not stop.");
    }
    if (!isCurrentScope(requestScope)) return;
    setSending(false);
    setMessages((current) => current.map((message) => message.pending ? { ...message, pending: false, text: "Stopped." } : message));
  };

  const restart = async () => {
    const requestScope = client.getContext();
    if (!isCurrentScope(requestScope)) return;
    try {
      await client.call({ method: "assistant", params: { action: "new_session" } }, requestScope);
      if (!isCurrentScope(requestScope)) return;
      setMessages([]);
      await onRefresh();
      if (!isCurrentScope(requestScope)) return;
    } catch (error) {
      if (!isCurrentScope(requestScope)) return;
      onNotice(error instanceof Error ? error.message : "Assistant session could not restart.");
    }
  };

  const answerEvidence = async (allow: boolean) => {
    if (!evidence) return;
    const current = evidence;
    const requestScope = client.getContext();
    if (!isCurrentScope(requestScope)) return;
    setEvidence(null);
    if (requestScope.generation !== current.generation || requestScope.projectId !== current.projectId) {
      onNotice("The project changed. Send the prompt again to review its current evidence scope.");
      return;
    }
    try {
      await client.call({ method: "permissions", params: { action: "evidence", params: { providerId: current.providerId, accountId: current.accountId, allow } } }, requestScope);
      if (!isCurrentScope(requestScope)) return;
    } catch (error) {
      if (!isCurrentScope(requestScope)) return;
      onNotice(error instanceof Error ? error.message : "Evidence permission could not be recorded.");
    }
  };
  return <div className="chat-content"><div className="chat-intent"><MessageCircle aria-hidden="true" /><span>Ask for an edit, or inspect your footage with local evidence.</span><button type="button" onClick={onProviderSettings} aria-label="Connect an assistant provider"><KeyRound aria-hidden="true" /></button></div><div className="chat-messages" aria-live="polite">{messages.length === 0 ? <div className="chat-empty"><Bot aria-hidden="true" /><strong>What should we make?</strong><span>Try “remove the first two seconds”, “find the setup explanation”, or “make this vertical”.</span><div className="prompt-chips"><button type="button" onClick={() => setDraft("Remove the first two seconds")}>Remove a moment</button><button type="button" onClick={() => setDraft("Add captions")}>Add captions</button><button type="button" onClick={() => setDraft("Make this vertical")}>Make it vertical</button></div></div> : messages.map((message) => <div className={`chat-message ${message.role} ${message.pending ? "pending" : ""}`} key={message.id}>{message.role === "assistant" ? <span className="message-avatar"><Bot aria-hidden="true" /></span> : null}<div className="message-bubble">{message.tool ? <button type="button" className="tool-card" onClick={() => setExpandedTools((current) => ({ ...current, [message.id]: !(current[message.id] ?? message.status === "failed") }))}><span className="tool-card-leading"><Wrench aria-hidden="true" /><strong>{message.tool}</strong></span><span className={`tool-status ${message.status}`}>{message.status === "running" ? <Loader2 className="spin" aria-hidden="true" /> : message.status === "complete" ? <Check aria-hidden="true" /> : <CircleStop aria-hidden="true" />}</span><ChevronDown className={(expandedTools[message.id] ?? message.status === "failed") ? "rotate-180" : ""} aria-hidden="true" /></button> : null}{!message.tool && message.text ? <p>{message.text}</p> : message.pending ? <span className="typing-indicator"><i /><i /><i /></span> : null}{message.status === "failed" ? <small className="message-error">Failed · retry from the prompt</small> : null}{message.tool && (expandedTools[message.id] ?? message.status === "failed") ? <div className="tool-details">{message.text || "No additional tool details."}</div> : null}</div></div>)}</div>{toolEvents.length > 0 ? <div className="chat-activity"><Wrench aria-hidden="true" /><span>{toolEvents.length} tool updates</span><span className="activity-dot" /></div> : null}{evidence ? <div className="evidence-consent" role="dialog" aria-label="Evidence consent"><ShieldCheck aria-hidden="true" /><div><strong>Share project evidence?</strong><p>Allow {evidence.providerId} ({evidence.accountId}) to receive sampled frames and transcript spans from this project{evidence.scope ? ` · ${evidence.scope}` : ""}?</p><div className="consent-actions"><Button variant="ghost" size="sm" onClick={() => void answerEvidence(false)}><X aria-hidden="true" />Deny</Button><Button size="sm" onClick={() => void answerEvidence(true)}><ShieldCheck aria-hidden="true" />Allow evidence</Button></div></div></div> : null}<form className="chat-composer" onSubmit={(event) => void submit(event)}><textarea value={draft} onChange={(event) => setDraft(event.target.value)} placeholder="Describe an edit…" rows={2} aria-label="Message assistant" onKeyDown={(event) => { if (event.key === "Enter" && !event.shiftKey) { event.preventDefault(); event.currentTarget.form?.requestSubmit(); } }} /><div className="composer-footer"><span>Enter to send · Shift+Enter for a new line</span><div className="composer-actions">{sending ? <Button variant="ghost" size="sm" type="button" onClick={() => void stop()}><Square aria-hidden="true" />Stop</Button> : null}<Button variant="primary" size="icon" type="submit" aria-label="Send message" disabled={!draft.trim() || sending}><Send aria-hidden="true" /></Button></div></div></form><button className="new-session-button" type="button" onClick={() => void restart()}><RotateCcw aria-hidden="true" />New assistant session</button></div>;
}
