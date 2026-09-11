import type { AssistantStatus, PiRuntime, SessionHistoryMessage } from "./session.js";

export type AssistantAction =
  | { action: "status" }
  | { action: "history" }
  | { action: "prompt"; text: string }
  | { action: "stop" }
  | { action: "restart" }
  | { action: "new_session" };

export type AssistantReply =
  | { action: "status"; status: AssistantStatus }
  | { action: "history"; messages: readonly SessionHistoryMessage[] }
  | { action: "prompt"; accepted: true }
  | { action: "stop"; stopped: true }
  | { action: "restart"; restarted: true }
  | { action: "new_session"; created: true };

export class AssistantRuntime {
  constructor(private readonly runtime: PiRuntime) {}

  async handle(action: AssistantAction, signal?: AbortSignal, runId?: string): Promise<AssistantReply> {
    switch (action.action) {
      case "status":
        return { action: "status", status: await this.runtime.status() };
      case "history":
        return { action: "history", messages: await this.runtime.history() };
      case "prompt":
        if (action.text.trim().length === 0) throw new Error("The assistant prompt must not be empty.");
        await this.runtime.prompt(action.text, signal, runId);
        return { action: "prompt", accepted: true };
      case "stop":
        await this.runtime.stop(runId);
        return { action: "stop", stopped: true };
      case "restart":
        await this.runtime.restart();
        return { action: "restart", restarted: true };
      case "new_session":
        await this.runtime.newSession();
        return { action: "new_session", created: true };
    }
  }
}
