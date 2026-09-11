import { useEffect, useMemo, useRef, useState } from "react";
import { Captions, ChevronRight, FileText, Search, Sparkles, Upload } from "lucide-react";

import type { EditOp, EditorClient, ProjectSnapshot, TimelineSelection } from "@cutterhoochee/shared";
import { Button } from "@/components/ui/button";
import { Dialog, DialogContent, DialogDescription, DialogHeader, DialogTitle } from "@/components/ui/dialog";
import { callNative, isEditorClientError, numberValue, record, replyPayload, stringValue } from "@/lib/native";
import { isTerminalActivity, useAgentActivities, type AgentActivityStore } from "@/activity/AgentActivityStore";

export type TranscriptPanelProps = {
  client: EditorClient;
  snapshot: ProjectSnapshot;
  selection: TimelineSelection;
  activityStore: AgentActivityStore;
  onEdit: (label: string, operations: readonly EditOp[]) => Promise<void>;
  onRefresh: () => Promise<void>;
  onNotice: (notice: string) => void;
  onSeek?: (frame: number) => void;
};

type SearchHit = { transcriptId: string; assetId: string; startFrame: number; endFrame: number; text: string; approximate: boolean };

function parseSearchHit(value: unknown, fallbackAssetId: string, fallbackTranscriptId: string): SearchHit | null {
  const item = record(value);
  const startFrame = numberValue(item.startFrame, Number.NaN);
  const endFrame = numberValue(item.endFrame, Number.NaN);
  const assetId = stringValue(item.assetId, fallbackAssetId);
  const transcriptId = stringValue(item.transcriptId, fallbackTranscriptId);
  if (!assetId || !transcriptId || !Number.isSafeInteger(startFrame) || !Number.isSafeInteger(endFrame) || startFrame < 0 || endFrame <= startFrame) {
    return null;
  }
  return { transcriptId, assetId, startFrame, endFrame, text: stringValue(item.text, "Transcript span"), approximate: Boolean(item.approximate) };
}
export function TranscriptPanel({ client, snapshot, selection, activityStore, onEdit, onRefresh, onNotice, onSeek }: TranscriptPanelProps) {
  const [assetId, setAssetId] = useState(() => snapshot.document.assets.find((asset) => (asset.kind === "video" || asset.kind === "audio") && Boolean(asset.normalization?.audio))?.id ?? "");
  const [transcriptId, setTranscriptId] = useState("");
  const [transcriptAssetId, setTranscriptAssetId] = useState<string | null>(null);
  const [query, setQuery] = useState("");
  const [hits, setHits] = useState<SearchHit[]>([]);
  const [busy, setBusy] = useState<"transcribe" | "search" | "srt" | "captions" | null>(null);
  const [modelInfo, setModelInfo] = useState<Record<string, unknown> | null>(null);
  const [modelConsentOpen, setModelConsentOpen] = useState(false);
  const [consentAssetId, setConsentAssetId] = useState<string | null>(null);
  const [message, setMessage] = useState<string | null>(null);
  const sourceAssets = useMemo(() => snapshot.document.assets.filter((asset) => (asset.kind === "video" || asset.kind === "audio") && Boolean(asset.normalization?.audio)), [snapshot.document.assets]);
  const selectedAsset = sourceAssets.find((asset) => asset.id === assetId);
  const activities = useAgentActivities(activityStore);
  const sourceWork = activities.find((activity) => activity.jobIds.length > 0 && activity.targets.some((target) => target.kind === "asset" && target.id === assetId));
  const cancelSourceWork = async () => {
    if (!sourceWork) return;
    try {
      for (const jobId of sourceWork.jobIds) await callNative(client, { method: "jobs", params: { action: "cancel", jobId } });
      onNotice("Cancel requested; waiting for the native job to finish.");
    } catch (error) {
      onNotice(error instanceof Error ? error.message : "The native cancellation request failed.");
    }
  };
  const selectedSourceKey = selectedAsset ? `${selectedAsset.id}:${selectedAsset.contentHash}` : "";
  const sourceIdentityRef = useRef(selectedSourceKey);
  const currentSourceKeyRef = useRef(selectedSourceKey);
  currentSourceKeyRef.current = selectedSourceKey;
  const requestVersionRef = useRef(0);

  const switchAsset = (nextAssetId: string) => {
    if (nextAssetId === assetId) return;
    requestVersionRef.current += 1;
    setAssetId(nextAssetId);
    setTranscriptId("");
    setTranscriptAssetId(null);
    setHits([]);
    setMessage(null);
    setModelConsentOpen(false);
    setConsentAssetId(null);
  };

  useEffect(() => {
    if (!sourceAssets.some((asset) => asset.id === assetId)) {
      setAssetId(sourceAssets[0]?.id ?? "");
    }
  }, [assetId, sourceAssets]);

  useEffect(() => {
    if (sourceIdentityRef.current === selectedSourceKey) return;
    sourceIdentityRef.current = selectedSourceKey;
    requestVersionRef.current += 1;
    setTranscriptId("");
    setTranscriptAssetId(null);
    setHits([]);
    setMessage(null);
    setModelConsentOpen(false);
    setConsentAssetId(null);
  }, [selectedSourceKey]);

  const readModelStatus = async () => {
    const reply = await callNative(client, { method: "transcript", params: { action: "model_status" } });
    const info = replyPayload(reply);
    setModelInfo(info);
    return info;
  };

  const transcribe = async (requestedAssetId: string, modelConsent: boolean) => {
    const requestedAsset = sourceAssets.find((asset) => asset.id === requestedAssetId);
    if (!requestedAsset) throw new Error("Choose an audio or video source with normalized audio before transcribing.");
    const requestVersion = requestVersionRef.current;
    const requestedSourceKey = `${requestedAsset.id}:${requestedAsset.contentHash}`;
    const reply = await callNative(client, {
      method: "transcript",
      params: { action: "transcribe", assetId: requestedAssetId, modelConsent },
    });
    if (requestVersionRef.current !== requestVersion || sourceIdentityRef.current !== requestedSourceKey || currentSourceKeyRef.current !== requestedSourceKey) return;
    const data = replyPayload(reply);
    const transcript = record(data.transcript);
    const returnedAssetId = stringValue(transcript.assetId, requestedAssetId);
    if (returnedAssetId !== requestedAssetId) throw new Error("Native transcript belongs to a different source.");
    const id = stringValue(transcript.transcriptId);
    if (!id) {
      setTranscriptId("");
      setTranscriptAssetId(null);
      setHits([]);
      setMessage("Transcription completed; no transcript identifier was returned.");
      return;
    }
    const values = Array.isArray(transcript.segments) ? transcript.segments : [];
    const spans = values.map((value) => parseSearchHit(value, returnedAssetId, id)).filter((value): value is SearchHit => value !== null);
    setTranscriptId(id);
    setTranscriptAssetId(returnedAssetId);
    setHits(spans);
    setMessage(spans.length > 0 ? `Transcript ready with ${spans.length} spans. Review a span below or apply captions to a matching clip.` : "Transcription completed; no transcript spans were returned.");
  };

  const run = async (kind: "transcribe" | "search" | "srt") => {
    const requestVersion = requestVersionRef.current;
    const requestedSourceKey = sourceIdentityRef.current;
    setBusy(kind);
    setMessage(null);
    try {
      if (kind === "transcribe") {
        if (!assetId) throw new Error("Choose an audio or video source with normalized audio before transcribing.");
        const info = await readModelStatus();
        if (Boolean(info.downloadRequired) && !Boolean(info.available)) {
          setConsentAssetId(assetId);
          setModelConsentOpen(true);
          setMessage("Review the native speech-model details before downloading.");
          return;
        }
        try {
          await transcribe(assetId, false);
        } catch (error) {
          if (requestVersionRef.current !== requestVersion || sourceIdentityRef.current !== requestedSourceKey || currentSourceKeyRef.current !== requestedSourceKey) return;
          if (isEditorClientError(error) && error.code === "PERMISSION_DENIED") {
            setConsentAssetId(assetId);
            try { await readModelStatus(); } catch { /* Metadata is advisory; the native error remains authoritative. */ }
            setModelConsentOpen(true);
            setMessage("Review the native speech-model details before downloading.");
            return;
          }
          throw error;
        }
      } else if (kind === "search") {
        if (!assetId) throw new Error("Choose an audio or video source with normalized audio before searching.");
        if (!query.trim()) return;
        const reply = await callNative(client, { method: "transcript", params: { action: "search", query: query.trim(), assetId } });
        if (requestVersionRef.current !== requestVersion || sourceIdentityRef.current !== requestedSourceKey || currentSourceKeyRef.current !== requestedSourceKey) return;
        const data = replyPayload(reply);
        const values = Array.isArray(data.hits) ? data.hits : [];
        const nextHits = values.map((value) => parseSearchHit(value, assetId, "")).filter((value): value is SearchHit => value !== null);
        setHits(nextHits);
        if (nextHits.length === 0) setMessage("No matching transcript spans were found for this source.");
      } else {
        await callNative(client, { method: "transcript", params: { action: "import_srt", playheadFrame: selection.playheadFrame, style: "clean" } });
        setTranscriptId("");
        setTranscriptAssetId(null);
        setHits([]);
        await onRefresh();
        setMessage("SRT cues imported as editable caption items at the current playhead.");
      }
    } catch (error) {
      if (requestVersionRef.current !== requestVersion || sourceIdentityRef.current !== requestedSourceKey || currentSourceKeyRef.current !== requestedSourceKey) return;
      const text = error instanceof Error ? error.message : "Evidence operation failed.";
      setMessage(text);
      onNotice(text);
    } finally {
      setBusy(null);
    }
  };


  const confirmModelDownload = async () => {
    const requestedAssetId = consentAssetId;
    if (!requestedAssetId) return;
    const requestVersion = requestVersionRef.current;
    const requestedSourceKey = sourceIdentityRef.current;
    setModelConsentOpen(false);
    setBusy("transcribe");
    setMessage(null);
    try {
      await transcribe(requestedAssetId, true);
    } catch (error) {
      if (requestVersionRef.current !== requestVersion || sourceIdentityRef.current !== requestedSourceKey || currentSourceKeyRef.current !== requestedSourceKey) return;
      const text = error instanceof Error ? error.message : "Local transcription failed.";
      setMessage(text);
      onNotice(text);
    } finally {
      setBusy(null);
      setConsentAssetId(null);
    }
  };

  const applyCaptions = async () => {
    const clip = snapshot.document.clips.find((candidate) => selection.clipIds.includes(candidate.id) && candidate.assetId === assetId);
    if (!clip) {
      onNotice("Select an audio or video clip from the current source before applying captions.");
      return;
    }
    if (!transcriptId.trim() || transcriptAssetId !== assetId) {
      onNotice("Choose a transcript from the current source before applying captions.");
      return;
    }
    setBusy("captions");
    try {
      await onEdit("Apply captions", [{ op: "replace_captions", clipId: clip.id, transcriptId: transcriptId.trim(), style: "boxed" }]);
      setMessage("Captions applied to the selected clip.");
    } catch {
      // Workspace displays the authoritative conflict/error notice.
    } finally {
      setBusy(null);
    }
  };
  const seekToHit = (hit: SearchHit) => {
    if (hit.assetId !== assetId) {
      const notice = "This transcript hit belongs to another source. Select that source before seeking.";
      setMessage(notice);
      onNotice(notice);
      return;
    }
    setTranscriptId(hit.transcriptId);
    setTranscriptAssetId(hit.assetId);
    const matchingClips = snapshot.document.clips
      .filter((clip) => {
        if (clip.assetId !== hit.assetId) return false;
        const sourceEnd = clip.inFrame + clip.durationFrames;
        return hit.startFrame >= clip.inFrame && hit.startFrame < sourceEnd;
      })
      .sort((left, right) => {
        const leftSelected = selection.clipIds.indexOf(left.id);
        const rightSelected = selection.clipIds.indexOf(right.id);
        const leftRank = leftSelected < 0 ? Number.MAX_SAFE_INTEGER : leftSelected;
        const rightRank = rightSelected < 0 ? Number.MAX_SAFE_INTEGER : rightSelected;
        return leftRank - rightRank || left.startFrame - right.startFrame || left.inFrame - right.inFrame || left.id.localeCompare(right.id);
      });
    const clip = matchingClips[0];
    if (!clip) {
      const sourceName = sourceAssets.find((asset) => asset.id === hit.assetId)?.original.fileName ?? "this source";
      const notice = `No placed clip contains source range ${hit.startFrame}–${hit.endFrame} for ${sourceName}.`;
      setMessage(notice);
      onNotice(notice);
      return;
    }
    setMessage(null);
    onSeek?.(clip.startFrame + hit.startFrame - clip.inFrame);
  };

  const modelBytes = numberValue(modelInfo?.expectedBytes);
  const modelSize = modelBytes > 0 ? `${(modelBytes / (1024 * 1024)).toFixed(1)} MiB` : "reported by native";
  const modelFile = stringValue(modelInfo?.file, "multilingual Whisper model");
  const modelSource = stringValue(modelInfo?.sourceUrl, "native pinned source");
  const modelRevision = stringValue(modelInfo?.revision);
  const modelSha = stringValue(modelInfo?.sha256);

  return <div className="panel-stack transcript-panel">
    <div className="panel-heading"><div><p className="eyebrow">Evidence</p><h2>Transcript</h2></div><Captions aria-hidden="true" className="panel-heading-icon" /></div>
    {sourceAssets.length > 0 ? <label className="field-group"><span className="field-label">Audio/video source</span><select value={assetId} onChange={(event) => switchAsset(event.target.value)}>{sourceAssets.map((asset) => <option value={asset.id} key={asset.id}>{asset.original.fileName}</option>)}</select></label> : <div className="empty-panel"><FileText aria-hidden="true" /><strong>No normalized audio source</strong><span>Import a video or audio asset with normalized audio to create local transcript evidence.</span></div>}
    {sourceWork ? <section className={`activity-entry activity-entry-${sourceWork.phase}`} aria-label="Source activity">
      <strong>{sourceWork.label}</strong>
      <span role="status">{sourceWork.phase.replaceAll("_", " ")}</span>
      {sourceWork.progress !== undefined && !isTerminalActivity(sourceWork) ? <progress max={1} value={sourceWork.progress} aria-label="Source job progress" /> : null}
      {sourceWork.message ? <p>{sourceWork.message}</p> : null}
      {sourceWork.error ? <p role="alert">{sourceWork.error.message}</p> : null}
      {!isTerminalActivity(sourceWork) ? <Button variant="ghost" size="sm" disabled={sourceWork.phase === "cancelling"} onClick={() => void cancelSourceWork()}>Cancel</Button> : null}
    </section> : null}
    <div className="transcript-actions"><Button variant="secondary" size="sm" disabled={!assetId || busy !== null} onClick={() => void run("transcribe")}><Sparkles aria-hidden="true" />{busy === "transcribe" ? "Transcribing…" : "Transcribe locally"}</Button><Button variant="ghost" size="sm" disabled={busy !== null} onClick={() => void run("srt")}><Upload aria-hidden="true" />Import SRT</Button></div>
    <div className="search-box"><Search aria-hidden="true" /><input value={query} onChange={(event) => setQuery(event.target.value)} onKeyDown={(event) => { if (event.key === "Enter") void run("search"); }} placeholder="Find a phrase" aria-label="Search transcript" /><button type="button" onClick={() => void run("search")} aria-label="Search transcript"><ChevronRight aria-hidden="true" /></button></div>
    {transcriptId ? <div className="transcript-id"><span>Transcript</span><code>{transcriptId}</code><Button variant="secondary" size="sm" disabled={busy !== null} onClick={() => void applyCaptions()}>{busy === "captions" ? "Applying…" : "Apply captions"}</Button></div> : null}
    <div className="transcript-results" aria-live="polite">{hits.length === 0 ? <div className="empty-panel compact"><Search aria-hidden="true" /><span>Search local evidence for a phrase and jump to its source range.</span></div> : hits.map((hit, index) => <button type="button" className="transcript-hit" key={`${hit.assetId}-${hit.transcriptId}-${hit.startFrame}-${hit.endFrame}-${index}`} onClick={() => seekToHit(hit)}><span className="transcript-hit-time">{hit.startFrame}–{hit.endFrame}</span><span>{hit.text}</span>{hit.approximate ? <small>approx.</small> : null}</button>)}</div>
    {message ? <p className="panel-message" role="status">{message}</p> : null}
    <div className="panel-footnote"><span className="status-dot" />Audio stays local unless you explicitly approve evidence for an assistant provider.</div>
    <Dialog open={modelConsentOpen} onOpenChange={(open) => { if (!open) { setModelConsentOpen(false); setConsentAssetId(null); } }}>
      <DialogContent>
        <DialogHeader>
          <DialogTitle>Download local speech model?</DialogTitle>
          <DialogDescription>Automatic captions need the native-pinned multilingual model below. It is downloaded to app-owned storage and processed locally; no video or audio is uploaded. Declining keeps manual editing and SRT captions available.</DialogDescription>
        </DialogHeader>
        <div className="approval-details">
          <div className="approval-row"><span>File</span><code>{modelFile}</code></div>
          <div className="approval-row"><span>Download size</span><span>{modelSize}</span></div>
          <div className="approval-row"><span>Source</span><code>{modelSource}</code></div>
          {modelRevision ? <div className="approval-row"><span>Revision</span><code>{modelRevision}</code></div> : null}
          {modelSha ? <div className="approval-row"><span>SHA-256</span><code>{modelSha}</code></div> : null}
          <div className="approval-row"><span>Network/use</span><span>One-time model download; transcription remains local-only.</span></div>
        </div>
        <div className="dialog-actions">
          <Button variant="ghost" onClick={() => { setModelConsentOpen(false); setConsentAssetId(null); }}>Not now</Button>
          <Button onClick={() => void confirmModelDownload()}>Download and transcribe</Button>
        </div>
      </DialogContent>
    </Dialog>
  </div>;
}
