import { useEffect, useRef, useState, type CSSProperties } from "react";
import { AlertTriangle, ChevronLeft, ChevronRight, Pause, Play, RotateCcw, StepBack, StepForward, Volume2 } from "lucide-react";

import type { EditorClient, ProjectSnapshot, TimelineSelection } from "@cutterhoochee/shared";
import { Button } from "@/components/ui/button";
import { formatDuration, formatTimecode } from "@/lib/native";
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

export function Preview({ client, snapshot, selection, playing, onPlayingChange, onFrameChange, onSelectionChange, onNotice }: PreviewProps) {
  const canvasRef = useRef<HTMLCanvasElement>(null);
  const engineRef = useRef<PreviewEngine | null>(null);
  const pauseSelectionFrameRef = useRef<number | null>(null);
  const pauseRequestedRef = useRef(false);
  const previousPlayingRef = useRef(playing);
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
      stateRef.current = next;
      setState(next);
      callbacks.current.onFrameChange(next.frame);
      if (next.state === "paused" && pauseRequestedRef.current) {
        pauseRequestedRef.current = false;
        if (next.frame !== callbacks.current.selection.playheadFrame && pauseSelectionFrameRef.current !== next.frame) {
          pauseSelectionFrameRef.current = next.frame;
          void callbacks.current.onSelectionChange({ ...callbacks.current.selection, playheadFrame: next.frame })
            .catch((error) => callbacks.current.onNotice(error instanceof Error ? error.message : "Playhead could not be saved."));
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
      previousPlayingRef.current = true;
      pauseRequestedRef.current = false;
      void engine.play().catch((error) => { const message = error instanceof Error ? error.message : "Playback could not start."; setEngineError(message); onNotice(message); });
    } else {
      const wasPlaying = previousPlayingRef.current;
      previousPlayingRef.current = false;
      if (wasPlaying && stateRef.current.state !== "ended" && stateRef.current.state !== "error") {
        pauseRequestedRef.current = true;
        engine.pause();
      }
    }
  }, [onNotice, playing]);

  useEffect(() => {
    const engine = engineRef.current;
    if (!engine) return;
    engine.setQuality(quality);
  }, [quality]);

  useEffect(() => {
    const engine = engineRef.current;
    const expectedPauseFrame = pauseSelectionFrameRef.current;
    if (expectedPauseFrame !== null) {
      if (selection.playheadFrame === expectedPauseFrame) pauseSelectionFrameRef.current = null;
      else return;
    }
    if (!engine || playing || stateRef.current.frame === selection.playheadFrame) return;
    void engine.seek(selection.playheadFrame).catch((error) => { const message = error instanceof Error ? error.message : "Seek failed."; setEngineError(message); onNotice(message); });
  }, [onNotice, playing, selection.playheadFrame]);

  const duration = Math.max(1, state.durationFrames || selection.playheadFrame + 1);
  const profile = snapshot.document.profile;
  const ratio = profile.width / Math.max(1, profile.height);
  const seek = async (frame: number) => {
    pauseSelectionFrameRef.current = null;
    const nextFrame = Math.max(0, Math.min(duration - 1, frame));
    const seekPromise = engineRef.current?.seek(nextFrame);
    const selectionPromise = Promise.resolve().then(() => onSelectionChange({ ...selection, playheadFrame: nextFrame }));
    await Promise.all([
      Promise.resolve(seekPromise).catch((error) => onNotice(error instanceof Error ? error.message : "Seek failed.")),
      selectionPromise.catch((error) => onNotice(error instanceof Error ? error.message : "Playhead could not be saved.")),
    ]);
  };
  const step = (delta: number) => {
    void seek(stateRef.current.frame + delta);
  };
  const togglePlaying = () => onPlayingChange(!playing);

  return <div className="preview-component"><div className="canvas-wrap" style={{ aspectRatio: `${profile.width} / ${profile.height}`, "--canvas-ratio": ratio } as CSSProperties}><canvas ref={canvasRef} width={profile.width} height={profile.height} aria-label="Video preview" />{state.state === "buffering" ? <div className="preview-overlay"><span className="spinner" />Buffering preview…</div> : null}{state.state === "error" || engineError ? <div className="preview-overlay error"><AlertTriangle aria-hidden="true" /><span>{engineError || state.error || "Preview unavailable"}</span><Button variant="secondary" size="sm" onClick={() => { setEngineError(null); void engineRef.current?.refresh().catch((error) => onNotice(error instanceof Error ? error.message : "Preview refresh failed.")); }}>Retry preview</Button></div> : null}<div className="canvas-corner-label">{profile.width}×{profile.height} · {state.quality === "software" ? "Software" : "Auto"}</div></div><div className="preview-controls"><div className="transport-buttons"><Button variant="ghost" size="icon" aria-label="Previous frame" onClick={() => step(-1)}><StepBack aria-hidden="true" /></Button><Button variant="primary" size="icon" aria-label={playing ? "Pause" : "Play"} onClick={togglePlaying}>{playing ? <Pause aria-hidden="true" /> : <Play aria-hidden="true" />}</Button><Button variant="ghost" size="icon" aria-label="Next frame" onClick={() => step(1)}><StepForward aria-hidden="true" /></Button></div><div className="preview-seek"><span>{formatTimecode(state.frame ?? selection.playheadFrame, profile.fpsNum, profile.fpsDen)}</span><input type="range" min={0} max={Math.max(0, duration - 1)} value={Math.min(duration - 1, state.frame ?? selection.playheadFrame)} onChange={(event) => void seek(Number(event.target.value))} aria-label="Preview playhead" /><span>{formatDuration(duration, profile.fpsNum, profile.fpsDen)}</span></div><div className="preview-status"><span className={`transport-state ${state.state}`}>{state.state}</span><button type="button" className="quality-select" onClick={() => setQuality((current) => current === "software" ? "auto" : "software")} title="Toggle software preview"><RotateCcw aria-hidden="true" />{state.quality}</button><span role="img" aria-label="Preview audio is scheduled by the native clock" title="Preview audio is scheduled by the native clock"><Volume2 aria-hidden="true" /></span></div></div></div>;
}
