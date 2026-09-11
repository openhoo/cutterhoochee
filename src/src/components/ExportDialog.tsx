import { useCallback, useEffect, useRef, useState } from "react";
import { CheckCircle2, Download, ExternalLink, Film, Play, Square, X } from "lucide-react";
import { listen } from "@tauri-apps/api/event";

import type { EditorClient, ProjectSnapshot } from "@cutterhoochee/shared";
import { callNative, eventData, numberValue, parseEvent, record, replyPayload, stringValue } from "@/lib/native";
import { Button } from "@/components/ui/button";

type JobState = "queued" | "running" | "completed" | "failed" | "cancelled";

function readJob(data: Record<string, unknown>): { id: string; state?: JobState; progress: number } {
  const job = record(data.job);
  const rawState = stringValue(job.state).toLowerCase();
  const state: JobState | undefined = rawState === "queued" || rawState === "running" || rawState === "completed" || rawState === "failed" || rawState === "cancelled"
    ? rawState
    : undefined;
  const rawProgress = numberValue(job.progress, numberValue(data.progress, 0));
  return {
    id: stringValue(job.jobId || data.jobId || data.id),
    state,
    progress: Math.max(0, Math.min(1, rawProgress > 1 ? rawProgress / 100 : rawProgress)),
  };
}

function readDestination(data: Record<string, unknown>): string {
  const result = record(data.result);
  return stringValue(result.destination || data.destination);
}

export function ExportDialog({ client, snapshot, onNotice, onClose }: { client: EditorClient; snapshot: ProjectSnapshot; onNotice: (notice: string) => void; onClose: () => void }) {
  const [resolution, setResolution] = useState<720 | 1080>(1080);
  const [srt, setSrt] = useState(false);
  const [jobId, setJobId] = useState("");
  const [progress, setProgress] = useState(0);
  const [phase, setPhase] = useState("Ready to export");
  const [outputPath, setOutputPath] = useState("");
  const [running, setRunning] = useState(false);
  const [complete, setComplete] = useState(false);
  const [cancelRequested, setCancelRequested] = useState(false);
  const cancelRequestedRef = useRef(false);

  const applyStatus = useCallback((data: Record<string, unknown>) => {
    const job = readJob(data);
    if (job.id) setJobId(job.id);
    setProgress(job.progress);
    const destination = readDestination(data);
    if (destination) setOutputPath(destination);
    if (job.state === "completed" && destination) {
      cancelRequestedRef.current = false;
      setCancelRequested(false);
      setComplete(true);
      setRunning(false);
      setProgress(1);
      setPhase("Export complete");
    } else if (job.state === "completed") {
      cancelRequestedRef.current = false;
      setCancelRequested(false);
      setComplete(false);
      setRunning(false);
      setPhase("Export completed but no output file was reported.");
    } else if (job.state === "failed") {
      cancelRequestedRef.current = false;
      setCancelRequested(false);
      setComplete(false);
      setRunning(false);
      setPhase(stringValue(record(record(data.job).error).message, "Export failed"));
    } else if (job.state === "cancelled") {
      cancelRequestedRef.current = false;
      setCancelRequested(false);
      setComplete(false);
      setRunning(false);
      setOutputPath("");
      setPhase("Export cancelled");
    } else if (job.state) {
      setRunning(true);
      setPhase(cancelRequestedRef.current ? "Cancel requested; waiting for native confirmation…" : job.state === "queued" ? "Waiting for export worker…" : "Rendering immutable revision…");
    }
  }, []);

  const refreshStatus = useCallback(async (id: string) => {
    if (!id) return;
    try {
      const reply = await callNative(client, { method: "export_video", params: { action: "status", jobId: id } });
      applyStatus(replyPayload(reply));
    } catch {
      // A transient status read must not hide the last truthful job state.
    }
  }, [applyStatus, client]);

  useEffect(() => {
    if (!jobId || complete) return;
    void refreshStatus(jobId);
    const timer = window.setInterval(() => void refreshStatus(jobId), 600);
    return () => window.clearInterval(timer);
  }, [complete, jobId, refreshStatus]);

  useEffect(() => {
    if (!(typeof window !== "undefined" && "__TAURI_INTERNALS__" in window)) return;
    let disposed = false;
    let unlisten: (() => void) | undefined;
    void listen<unknown>("cutterhoochee://event", (event) => {
      if (disposed) return;
      const parsed = parseEvent(event.payload);
      if (!parsed) return;
      const kind = parsed.kind.toLowerCase();
      if (!kind.includes("export") && !kind.includes("job")) return;
      const data = eventData(parsed);
      const eventJobId = stringValue(data.jobId || parsed.runId);
      if (jobId && eventJobId && eventJobId !== jobId) return;
      const nextProgress = numberValue(data.progress, numberValue(data.fraction, -1));
      if (nextProgress >= 0) setProgress(Math.max(0, Math.min(1, nextProgress > 1 ? nextProgress / 100 : nextProgress)));
      const nextPhase = stringValue(data.phase || data.message || data.status);
      if (nextPhase) setPhase(nextPhase);
    }).then((dispose) => { if (disposed) dispose(); else unlisten = dispose; }).catch(() => undefined);
    return () => { disposed = true; unlisten?.(); };
  }, [jobId]);

  const start = async () => {
    cancelRequestedRef.current = false;
    setCancelRequested(false);
    setRunning(true);
    setComplete(false);
    setJobId("");
    setOutputPath("");
    setProgress(0);
    setPhase("Choosing destination…");
    try {
      const reply = await callNative(client, { method: "export_video", params: { action: "start", revision: snapshot.document.revision, resolution, srt } });
      const data = replyPayload(reply);
      const job = readJob(data);
      if (!job.id) throw new Error("Native export did not return an owned job.");
      setJobId(job.id);
      applyStatus(data);
    } catch (error) {
      setRunning(false);
      setPhase(error instanceof Error ? error.message : "Export failed.");
      onNotice(error instanceof Error ? error.message : "Export failed.");
    }
  };

  const cancel = async () => {
    if (!jobId || !running || cancelRequestedRef.current) return;
    cancelRequestedRef.current = true;
    setCancelRequested(true);
    setPhase("Cancel requested; waiting for native confirmation…");
    try {
      const reply = await callNative(client, { method: "export_video", params: { action: "cancel", jobId } });
      const data = replyPayload(reply);
      const job = readJob(data);
      if (job.state !== "cancelled") applyStatus(data);
    } catch (error) {
      cancelRequestedRef.current = false;
      setCancelRequested(false);
      setPhase("Export is still running.");
      onNotice(error instanceof Error ? error.message : "Export could not be cancelled.");
    }
  };

  const showOutput = async (mode: "play" | "show_file") => {
    if (!jobId || !complete || !outputPath) return;
    try {
      await callNative(client, { method: "export_video", params: { action: mode, jobId } });
    } catch (error) {
      onNotice(error instanceof Error ? error.message : `Could not ${mode === "play" ? "play" : "show"} the exported file.`);
    }
  };

  const landscape = snapshot.document.profile.width > snapshot.document.profile.height;
  const portrait = snapshot.document.profile.height > snapshot.document.profile.width;
  const widthFor = (value: number) => landscape ? Math.floor(value * 16 / 9) : value;
  const heightFor = (value: number) => portrait ? Math.floor(value * 16 / 9) : value;
  const outputWidth = widthFor(resolution);
  const outputHeight = heightFor(resolution);

  return (
    <div className="export-dialog">
      <div className="settings-header"><div><p className="eyebrow">Deliver</p><h2>Export video</h2><p>Export captures revision {snapshot.document.revision} and will not change if you continue editing.</p></div><Button variant="ghost" size="icon" aria-label="Close export dialog" onClick={onClose}><X aria-hidden="true" /></Button></div>
      <div className="export-preview-card"><div className="export-icon"><Film aria-hidden="true" /></div><div><strong>{snapshot.document.name}</strong><span>{outputWidth} × {outputHeight} · H.264 / AAC · {snapshot.document.profile.fpsNum / snapshot.document.profile.fpsDen} fps</span></div><span className="export-revision">r{snapshot.document.revision}</span></div>
      <div className="export-options"><label className="field-group"><span className="field-label">Resolution</span><select value={resolution} onChange={(event) => setResolution(Number(event.target.value) as 720 | 1080)} disabled={running}><option value={1080}>1080p · {widthFor(1080)} × {heightFor(1080)}</option><option value={720}>720p · {widthFor(720)} × {heightFor(720)}</option></select></label><label className="toggle-row"><span>SRT caption sidecar</span><input type="checkbox" checked={srt} onChange={(event) => setSrt(event.target.checked)} disabled={running} /><small>Burn-in remains available from the captioned timeline.</small></label></div>
      <div className="export-progress" aria-live="polite"><div className="export-progress-header"><span>{phase}</span><span>{Math.round(progress * 100)}%</span></div><div className="progress-track"><span style={{ width: `${Math.round(progress * 100)}%` }} /></div>{jobId ? <small>Native job {jobId}</small> : null}{complete && outputPath ? <small className="export-output">Saved to {outputPath}</small> : null}</div>
      <div className="dialog-actions">{complete && outputPath ? <><Button variant="secondary" onClick={() => void showOutput("play")}><Play aria-hidden="true" />Play</Button><Button variant="secondary" onClick={() => void showOutput("show_file")}><ExternalLink aria-hidden="true" />Show file</Button></> : null}{running ? <Button variant="ghost" disabled={cancelRequested} onClick={() => void cancel()}><Square aria-hidden="true" />{cancelRequested ? "Cancelling…" : "Cancel"}</Button> : <Button onClick={() => void start()} disabled={complete}><Download aria-hidden="true" />{complete ? "Exported" : "Export"}</Button>}{complete ? <span className="export-success"><CheckCircle2 aria-hidden="true" />Finalized safely</span> : null}</div>
    </div>
  );
}
