import { useCallback, useEffect, useRef, useState } from "react";
import { AlertTriangle, Upload, X } from "lucide-react";
import { getCurrentWebviewWindow } from "@tauri-apps/api/webviewWindow";
import { eventData, stringValue, type EventPayload } from "@/lib/native";
import "./native-drop.css";

type DropState = {
  phase: "idle" | "dragging" | "processing" | "error" | "info";
  title?: string;
  message?: string;
};
const IDLE: DropState = { phase: "idle" };

export function useNativeDropFeedback({ projectOpen }: { projectOpen: boolean }) {
  const [state, setState] = useState<DropState>(IDLE);
  const projectOpenRef = useRef(projectOpen);
  const timer = useRef<number | undefined>(undefined);
  projectOpenRef.current = projectOpen;
  const dismiss = useCallback(() => {
    clearTimeout(timer.current);
    timer.current = undefined;
    setState(IDLE);
  }, []);
  const show = useCallback((next: DropState, duration?: number) => {
    clearTimeout(timer.current);
    timer.current = undefined;
    setState(next);
    if (duration) timer.current = window.setTimeout(() => setState(IDLE), duration);
  }, []);
  const handleNativeEvent = useCallback((event: EventPayload) => {
    if (event.kind !== "media_drop_failed") return;
    const data = eventData(event);
    const message = stringValue(data.message) || "Try importing the files again with Import media.";
    const cancelled = /cancelled|canceled/i.test(message);
    show({
      phase: cancelled ? "info" : "error",
      title: cancelled ? "Import cancelled" : "Couldn't add these files",
      message: cancelled ? "You can drop your files again whenever you're ready." : message,
    }, cancelled ? 4000 : undefined);
  }, [show]);

  useEffect(() => {
    let disposed = false;
    let unlisten: (() => void) | undefined;
    const native = "__TAURI_INTERNALS__" in window;
    if (native) {
      void getCurrentWebviewWindow().onDragDropEvent(({ payload }) => {
        if (disposed) return;
        if (payload.type === "enter" && payload.paths.length > 0) {
          show({
            phase: "dragging",
            title: projectOpenRef.current ? "Add to project" : "Start with your footage",
            message: projectOpenRef.current ? "Release to import videos, photos, or audio." : "Release, then choose where to save a new landscape project.",
          });
        } else if (payload.type === "leave") {
          setState((current) => current.phase === "dragging" ? IDLE : current);
        } else if (payload.type === "drop" && payload.paths.length > 0) {
          // The Rust window handler owns authorization and importing. Never
          // import again here or infer success from unrelated background jobs.
          show({
            phase: "processing",
            title: "Files received",
            message: projectOpenRef.current ? "Follow preparation in your Media library." : "Choose a folder to start your project.",
          }, 4000);
        }
      }).then((dispose) => {
        if (disposed) dispose();
        else unlisten = dispose;
      }).catch(() => {
        // The Import media button remains available if event subscription fails.
      });
    }
    // Do not let a browser preview navigate away when a file is dropped. Internal
    // asset-to-timeline drags use their own MIME type and remain untouched.
    const preventFileNavigation = (event: DragEvent) => {
      if (!event.dataTransfer?.types.includes("Files")) return;
      event.preventDefault();
      if (!native && event.type === "drop") show({ phase: "info", title: "Open the desktop app to import", message: "Drop files into Cutterhoochee's desktop window, or use Import media there." });
    };
    window.addEventListener("dragover", preventFileNavigation);
    window.addEventListener("drop", preventFileNavigation);
    return () => {
      disposed = true;
      unlisten?.();
      clearTimeout(timer.current);
      window.removeEventListener("dragover", preventFileNavigation);
      window.removeEventListener("drop", preventFileNavigation);
    };
  }, [show]);
  return { state, handleNativeEvent, dismiss };
}

export function NativeDropOverlay({ state, onDismiss }: { state: DropState; onDismiss: () => void }) {
  if (state.phase === "idle") return null;
  return <div className={`native-drop-overlay native-drop-${state.phase}`} data-drop-state={state.phase}>
    <div className="native-drop-card">
      <span className="native-drop-icon">{state.phase === "error" ? <AlertTriangle aria-hidden="true" /> : <Upload aria-hidden="true" />}</span>
      <span className="native-drop-copy" role={state.phase === "error" ? "alert" : "status"}>
        <strong>{state.title}</strong><span>{state.message}</span>
      </span>
      {state.phase !== "dragging" ? <button type="button" className="icon-button" aria-label="Dismiss import message" onClick={onDismiss}><X aria-hidden="true" /></button> : null}
    </div>
  </div>;
}
