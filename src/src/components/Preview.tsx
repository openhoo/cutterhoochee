import { memo, useEffect, useRef, useState, type CSSProperties } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { AlertTriangle, ChevronLeft, ChevronRight, Pause, Play, RotateCcw, StepBack, StepForward, Volume2 } from "lucide-react";

import type {
  EditorClient,
  PreviewTransportCommand,
  PreviewTransportCompletion,
  ProjectSnapshot,
  TimelineSelection,
} from "@cutterhoochee/shared";
import { Button } from "@/components/ui/button";
import { formatDuration, formatTimecode, eventData, parseEvent, record, stringValue } from "@/lib/native";
import { PreviewEngine, type PreviewEngineState as PreviewState } from "@/preview";

export type PreviewProps = {
  client: EditorClient;
  snapshot: ProjectSnapshot;
  selection: TimelineSelection;
  playing: boolean;
  onPlayingChange: (playing: boolean) => void;
  onFrameChange: (frame: number) => void;
  onSelectionChange: (selection: TimelineSelection) => Promise<void>;
  onNotice: (notice: string) => void;
};
function isTauriRuntime(): boolean {
  return typeof window !== "undefined" && "__TAURI_INTERNALS__" in window;
}

function parseTransportCommand(value: unknown): PreviewTransportCommand | null {
  const object = record(value);
  const sequence = object.sequence;
  const generation = object.generation;
  const revision = object.revision;
  const projectId = stringValue(object.projectId);
  const action = stringValue(object.action);
  if (
    typeof sequence !== "number" || !Number.isSafeInteger(sequence)
    || sequence < 0
    || typeof generation !== "number" || !Number.isSafeInteger(generation)
    || generation < 0
    || typeof revision !== "number" || !Number.isSafeInteger(revision)
    || revision < 0
    || projectId.length === 0
    || (action !== "play" && action !== "pause" && action !== "seek")
  ) {
    return null;
  }
  const frame = object.frame;
  if (action === "seek" && (!Number.isSafeInteger(frame) || (frame as number) < 0)) return null;
  return {
    sequence: sequence as number,
    projectId,
    generation: generation as number,
    revision: revision as number,
    action,
    ...(action === "seek" ? { frame: frame as number } : {}),
  };
}


export const Preview = memo(function Preview({ client, snapshot, selection, playing, onPlayingChange, onFrameChange, onSelectionChange, onNotice }: PreviewProps) {
  const canvasRef = useRef<HTMLCanvasElement>(null);
  const engineRef = useRef<PreviewEngine | null>(null);
  const pauseSelectionFrameRef = useRef<number | null>(null);
  const pauseRequestedRef = useRef(false);
  const previousPlayingRef = useRef(playing);
  const pauseSelectionPromiseRef = useRef<Promise<void> | null>(null);
  const [state, setState] = useState<PreviewState>({ state: "paused", frame: selection.playheadFrame, durationFrames: 0, quality: "auto" });
  const [quality, setQuality] = useState<"auto" | "software">("auto");
  const [engineError, setEngineError] = useState<string | null>(null);
  const callbacks = useRef({ onPlayingChange, onFrameChange, onSelectionChange, onNotice, selection });
  callbacks.current = { onPlayingChange, onFrameChange, onSelectionChange, onNotice, selection };
  const stateRef = useRef(state);
  stateRef.current = state;

  useEffect(() => {
    const canvas = canvasRef.current;
    if (!canvas) return;
    const engine = new PreviewEngine({ canvas, client, onState: (next) => {
      const previous = stateRef.current;
      stateRef.current = next;
      if (previous.state !== next.state || previous.frame !== next.frame || previous.durationFrames !== next.durationFrames || previous.quality !== next.quality || previous.error !== next.error) setState(next);
      callbacks.current.onFrameChange(next.frame);
      if (previous.state !== next.state) {
        if (next.state === "playing") {
          previousPlayingRef.current = true;
          callbacks.current.onPlayingChange(true);
        }
        if (next.state === "paused" || next.state === "ended" || next.state === "error") {
          previousPlayingRef.current = false;
          callbacks.current.onPlayingChange(false);
        }
      }
      if (next.state === "paused" && pauseRequestedRef.current) {
        pauseRequestedRef.current = false;
        if (next.frame !== callbacks.current.selection.playheadFrame && pauseSelectionFrameRef.current !== next.frame) {
          pauseSelectionFrameRef.current = next.frame;
          const pending = callbacks.current.onSelectionChange({ ...callbacks.current.selection, playheadFrame: next.frame });
          pauseSelectionPromiseRef.current = pending;
          void pending.catch((error) => callbacks.current.onNotice(error instanceof Error ? error.message : "Playhead could not be saved."));
        }
      }
      if (next.state === "ended" || next.state === "error") {
        if (next.frame !== callbacks.current.selection.playheadFrame && pauseSelectionFrameRef.current !== next.frame) {
          pauseSelectionFrameRef.current = next.frame;
          void callbacks.current.onSelectionChange({ ...callbacks.current.selection, playheadFrame: next.frame })
            .catch((error) => callbacks.current.onNotice(error instanceof Error ? error.message : "Playhead could not be saved."));
        }
        callbacks.current.onPlayingChange(false);
      }
    } });
    engineRef.current = engine;
    return () => {
      engine.dispose();
      engineRef.current = null;
    };
  }, [client]);
  useEffect(() => {
    if (!isTauriRuntime()) return;
    let disposed = false;
    let unlisten: (() => void) | undefined;
    void listen<unknown>("cutterhoochee://event", ({ payload }) => {
      if (disposed) return;
      const event = parseEvent(payload);
      if (!event) return;
      const eventKind = event.kind.toLowerCase();
      if (eventKind !== "preview_transport" && eventKind !== "preview_transport_cancel") return;
      const context = client.getContext();
      if (event.projectId !== context.projectId || event.generation !== context.generation) return;
      const command = parseTransportCommand(eventData(event));
      if (!command) return;
      const engine = engineRef.current;
      if (eventKind === "preview_transport_cancel") {
        engine?.cancelTransportCommand(command);
        return;
      }
      if (command.action === "pause") {
        pauseRequestedRef.current = true;
        pauseSelectionPromiseRef.current = null;
      }
      const apply = engine
        ? engine.applyTransportCommand(command)
        : Promise.resolve<PreviewTransportCompletion>({
          sequence: command.sequence,
          projectId: command.projectId,
          generation: command.generation,
          error: "The visible preview is not mounted.",
        });
      void apply.then(async (result) => {
        let completion = result;
        if (!completion.error && (command.action === "seek" || command.action === "pause") && engine) {
          try {
            if (command.action === "pause" && pauseSelectionPromiseRef.current) await pauseSelectionPromiseRef.current;
            const acceptedFrame = engine.getState().frame;
            if (acceptedFrame !== callbacks.current.selection.playheadFrame) {
              await callbacks.current.onSelectionChange({
                ...callbacks.current.selection,
                playheadFrame: acceptedFrame,
              });
            }
          } catch (error) {
            completion = {
              ...completion,
              error: error instanceof Error ? error.message : "Playhead could not be saved.",
            };
          }
        }
        if (disposed) return;
        try {
          await invoke("preview_transport_complete", { completion });
        } catch (error) {
          callbacks.current.onNotice(error instanceof Error ? error.message : "Preview transport acknowledgement failed.");
        }
      }).catch((error) => {
        callbacks.current.onNotice(error instanceof Error ? error.message : "Preview transport failed.");
      });
    }).then((dispose) => {
      if (disposed) dispose();
      else unlisten = dispose;
    }).catch(() => {
      // A browser tab has no native transport event bridge.
    });
    return () => {
      disposed = true;
      unlisten?.();
    };
  }, [client]);


  useEffect(() => {
    const engine = engineRef.current;
    if (!engine) return;
    void engine.setProject(snapshot).catch((error) => {
      const message = error instanceof Error ? error.message : "Preview could not load this revision.";
      setEngineError(message);
      onNotice(message);
    });
  }, [onNotice, snapshot]);

  useEffect(() => {
    const engine = engineRef.current;
    if (!engine) return;
    if (playing) {
      if (previousPlayingRef.current === true) return;
      previousPlayingRef.current = true;
      pauseRequestedRef.current = false;
      void engine.play().catch((error) => {
        const message = error instanceof Error ? error.message : "Playback could not start.";
        setEngineError(message);
        onNotice(message);
      });
    } else {
      if (previousPlayingRef.current === false) return;
      previousPlayingRef.current = false;
      if (stateRef.current.state !== "ended" && stateRef.current.state !== "error") {
        pauseRequestedRef.current = true;
        void engine.pause().catch((error) => {
          const message = error instanceof Error ? error.message : "Playback could not pause.";
          setEngineError(message);
          onNotice(message);
        });
      }
    }
  }, [onNotice, playing]);

  useEffect(() => {
    const engine = engineRef.current;
    if (!engine) return;
    void engine.setQuality(quality);
  }, [quality]);
  useEffect(() => {
    const engine = engineRef.current;
    const expectedPauseFrame = pauseSelectionFrameRef.current;
    if (expectedPauseFrame !== null) {
      if (selection.playheadFrame === expectedPauseFrame) pauseSelectionFrameRef.current = null;
      else return;
    }
    if (!engine || playing || stateRef.current.frame === selection.playheadFrame) return;
    void engine.seek(selection.playheadFrame).catch((error) => {
      const message = error instanceof Error ? error.message : "Seek failed.";
      setEngineError(message);
      onNotice(message);
    });
  }, [onNotice, playing, selection.playheadFrame]);

  const duration = Math.max(1, state.durationFrames || selection.playheadFrame + 1);
  const profile = snapshot.document.profile;
  const ratio = profile.width / Math.max(1, profile.height);
  const seek = async (frame: number) => {
    pauseSelectionFrameRef.current = null;
    const engine = engineRef.current;
    if (!engine) return;
    const nextFrame = Math.max(0, Math.min(duration - 1, frame));
    try {
      await engine.seek(nextFrame);
      const acceptedFrame = engine.getState().frame;
      await callbacks.current.onSelectionChange({
        ...callbacks.current.selection,
        playheadFrame: acceptedFrame,
      });
    } catch (error) {
      onNotice(error instanceof Error ? error.message : "Seek failed.");
    }
  };
  const step = (delta: number) => {
    void seek(stateRef.current.frame + delta);
  };
  const togglePlaying = () => {
    const engine = engineRef.current;
    const nextPlaying = !playing;
    previousPlayingRef.current = nextPlaying;
    if (!engine) {
      onPlayingChange(nextPlaying);
      return;
    }
    if (nextPlaying) {
      void engine.play().catch((error) => onNotice(error instanceof Error ? error.message : "Playback could not start."));
    } else {
      pauseRequestedRef.current = true;
      void engine.pause().catch((error) => onNotice(error instanceof Error ? error.message : "Playback could not pause."));
    }
  };

  return <div className="preview-component"><div className="canvas-wrap" style={{ aspectRatio: `${profile.width} / ${profile.height}`, "--canvas-ratio": ratio } as CSSProperties}><canvas ref={canvasRef} width={profile.width} height={profile.height} aria-label="Video preview" />{state.state === "buffering" ? <div className="preview-overlay"><span className="spinner" />Buffering preview…</div> : null}{state.state === "error" || engineError ? <div className="preview-overlay error"><AlertTriangle aria-hidden="true" /><span>{engineError || state.error || "Preview unavailable"}</span><Button variant="secondary" size="sm" onClick={() => { setEngineError(null); void engineRef.current?.refresh().catch((error) => onNotice(error instanceof Error ? error.message : "Preview refresh failed.")); }}>Retry preview</Button></div> : null}<div className="canvas-corner-label">{profile.width}×{profile.height} · {state.quality === "software" ? "Software" : "Auto"}</div></div><div className="preview-controls"><div className="transport-buttons"><Button variant="ghost" size="icon" aria-label="Previous frame" onClick={() => step(-1)}><StepBack aria-hidden="true" /></Button><Button variant="primary" size="icon" aria-label={playing ? "Pause" : "Play"} onClick={togglePlaying}>{playing ? <Pause aria-hidden="true" /> : <Play aria-hidden="true" />}</Button><Button variant="ghost" size="icon" aria-label="Next frame" onClick={() => step(1)}><StepForward aria-hidden="true" /></Button></div><div className="preview-seek"><span>{formatTimecode(state.frame ?? selection.playheadFrame, profile.fpsNum, profile.fpsDen)}</span><input type="range" min={0} max={Math.max(0, duration - 1)} value={Math.min(duration - 1, state.frame ?? selection.playheadFrame)} onChange={(event) => void seek(Number(event.target.value))} aria-label="Preview playhead" /><span>{formatDuration(duration, profile.fpsNum, profile.fpsDen)}</span></div><div className="preview-status"><span className={`transport-state ${state.state}`}>{state.state}</span><button type="button" className="quality-select" aria-label={"Use software preview"} aria-pressed={quality === "software"} onClick={() => setQuality((current) => current === "software" ? "auto" : "software")} title="Preview quality · toggle Auto/Software"><RotateCcw aria-hidden="true" /><span>Quality · {state.quality}</span></button><span role="img" aria-label="Preview audio is scheduled by the native clock" title="Preview audio is scheduled by the native clock"><Volume2 aria-hidden="true" /></span></div></div></div>;
});
