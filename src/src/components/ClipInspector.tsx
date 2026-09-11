import { memo, useEffect, useMemo, useState } from "react";
import { AlignCenter, AudioLines, Captions, ChevronDown, Crop, Film, Lock, Plus, SlidersHorizontal, Trash2, Unlock } from "lucide-react";

import type { ClipPatch, EditOp, EditorClient, MediaClip, ProjectSnapshot, TextItem, TimelineSelection, Track, Transition } from "@cutterhoochee/shared";
import { Button } from "@/components/ui/button";
import { formatTimecode, transitionRemovalOps } from "@/lib/native";

export const ClipInspector = memo(function ClipInspector({
  client: _client,
  snapshot,
  clip,
  track,
  transitions,
  selection,
  onEdit,
  onNotice,
}: {
  client: EditorClient;
  snapshot: ProjectSnapshot;
  clip?: MediaClip;
  track?: Track;
  transitions: Transition[];
  selection: TimelineSelection;
  onEdit: (label: string, operations: readonly EditOp[]) => Promise<void>;
  onNotice: (notice: string) => void;
}) {
  const selectedText = useMemo(() => {
    const selectedTextIds = new Set(selection.textIds);
    return snapshot.document.textItems.find((item) => selectedTextIds.has(item.id));
  }, [selection.textIds, snapshot.document.textItems]);
  if (selectedText) return <TextInspector snapshot={snapshot} item={selectedText} onEdit={onEdit} onNotice={onNotice} />;
  if (!clip) return <EmptyInspector />;
  return <ClipFields snapshot={snapshot} clip={clip} track={track} transitions={transitions} onEdit={onEdit} onNotice={onNotice} />;
});

function EmptyInspector() {
  return <div className="empty-panel inspector-empty"><SlidersHorizontal aria-hidden="true" /><strong>Nothing selected</strong><span>Select a clip, title, or caption to edit its timing and properties.</span></div>;
}

function ClipFields({ snapshot, clip, track, transitions, onEdit, onNotice }: { snapshot: ProjectSnapshot; clip: MediaClip; track?: Track; transitions: Transition[]; onEdit: (label: string, operations: readonly EditOp[]) => Promise<void>; onNotice: (notice: string) => void }) {
  const [startFrame, setStartFrame] = useState(String(clip.startFrame));
  const [inFrame, setInFrame] = useState(String(clip.inFrame));
  const [durationFrames, setDurationFrames] = useState(String(clip.durationFrames));
  const [scale, setScale] = useState(String(clip.scale));
  const [opacity, setOpacity] = useState(String(clip.opacity));
  const [gainDb, setGainDb] = useState(String(clip.gainDb));
  const [centerX, setCenterX] = useState(String(clip.centerX));
  const [centerY, setCenterY] = useState(String(clip.centerY));
  const [fit, setFit] = useState(clip.fit);
  const [audioEnabled, setAudioEnabled] = useState(clip.audioEnabled);
  const [fadeInFrames, setFadeInFrames] = useState(String(clip.fadeInFrames));
  const [fadeOutFrames, setFadeOutFrames] = useState(String(clip.fadeOutFrames));
  const [transitionDuration, setTransitionDuration] = useState("15");

  useEffect(() => {
    setStartFrame(String(clip.startFrame));
    setInFrame(String(clip.inFrame));
    setDurationFrames(String(clip.durationFrames));
    setScale(String(clip.scale));
    setOpacity(String(clip.opacity));
    setGainDb(String(clip.gainDb));
    setCenterX(String(clip.centerX));
    setCenterY(String(clip.centerY));
    setFit(clip.fit);
    setAudioEnabled(clip.audioEnabled);
    setFadeInFrames(String(clip.fadeInFrames));
    setFadeOutFrames(String(clip.fadeOutFrames));
  }, [clip.id, clip.startFrame, clip.inFrame, clip.durationFrames, clip.scale, clip.opacity, clip.gainDb, clip.centerX, clip.centerY, clip.fit, clip.audioEnabled, clip.fadeInFrames, clip.fadeOutFrames]);

  const sourceAsset = snapshot.document.assets.find((asset) => asset.id === clip.assetId);
  const isVideoTrack = track?.kind === "video";
  const nextClip = useMemo(() => snapshot.document.clips.filter((candidate) => candidate.trackId === clip.trackId && candidate.id !== clip.id).sort((a, b) => a.startFrame - b.startFrame).find((candidate) => candidate.startFrame >= clip.startFrame + clip.durationFrames), [clip, snapshot.document.clips]);
  const commitPatch = async (patch: ClipPatch) => {
    try {
      await onEdit("Update clip", [{ op: "update_clip", clipId: clip.id, patch }]);
    } catch {
      // Workspace owns the authoritative error notice.
    }
  };
  const commitTrim = async () => {
    const nextStart = parseFrame(startFrame);
    const nextIn = parseFrame(inFrame);
    const nextDuration = parseFrame(durationFrames);
    if (nextStart === null || nextIn === null || nextDuration === null || nextDuration <= 0) {
      onNotice("Start, source in, and duration must be non-negative whole frames.");
      return;
    }
    try {
      await onEdit("Trim clip", [...transitionRemovalOps(snapshot, clip.id), { op: "trim_clip", clipId: clip.id, inFrame: nextIn, startFrame: nextStart, durationFrames: nextDuration }]);
    } catch {
      // Workspace owns the authoritative error notice.
    }
  };
  const addTransition = async () => {
    if (!nextClip) {
      onNotice("A dissolve needs the next clip on this video track.");
      return;
    }
    const duration = parseFrame(transitionDuration);
    if (duration === null || duration < 2 || duration >= Math.min(clip.durationFrames, nextClip.durationFrames)) {
      onNotice("Transition duration must be at least 2 frames and shorter than both clips.");
      return;
    }
    try {
      await onEdit("Add dissolve", [{ op: "add_transition", leftClipId: clip.id, rightClipId: nextClip.id, durationFrames: duration }]);
    } catch {
      // Workspace owns the authoritative error notice.
    }
  };
  const removeTransition = async (transition: Transition) => {
    try {
      await onEdit("Remove dissolve", [{ op: "remove_transition", transitionId: transition.id }]);
    } catch {
      // Workspace owns the authoritative error notice.
    }
  };

  return <div className="panel-stack inspector-panel">
    <div className="panel-heading"><div><p className="eyebrow">Inspector</p><h2>{sourceAsset?.original.fileName ?? "Clip"}</h2></div><Film aria-hidden="true" className="panel-heading-icon" /></div>
    <div className="inspector-section"><div className="section-label"><span>Timing</span><span className="section-hint">{formatTimecode(clip.startFrame, snapshot.document.profile.fpsNum, snapshot.document.profile.fpsDen)}</span></div><div className="field-row"><NumberField label="Start" value={startFrame} onChange={setStartFrame} /><NumberField label="Source in" value={inFrame} onChange={setInFrame} /><NumberField label="Duration" value={durationFrames} onChange={setDurationFrames} /></div><Button variant="secondary" size="sm" onClick={() => void commitTrim()}>Apply timing</Button></div>
    {isVideoTrack ? <div className="inspector-section"><div className="section-label"><span>Canvas</span><Crop aria-hidden="true" /></div><label className="field-group"><span className="field-label">Fit</span><select value={fit} onChange={(event) => { const nextFit = event.target.value as typeof fit; setFit(nextFit); void commitPatch({ fit: nextFit }); }}><option value="contain">Contain</option><option value="cover">Cover</option></select></label><div className="field-row"><NumberField label="X · bp" value={centerX} min={0} max={10000} onChange={(value) => { setCenterX(value); }} onBlur={() => void commitPatch({ centerX: parseFrame(centerX) ?? clip.centerX })} /><NumberField label="Y · bp" value={centerY} min={0} max={10000} onChange={setCenterY} onBlur={() => void commitPatch({ centerY: parseFrame(centerY) ?? clip.centerY })} /></div><NumberField label="Scale · bp" value={scale} min={100} max={40000} onChange={setScale} onBlur={() => void commitPatch({ scale: parseFrame(scale) ?? clip.scale })} /><NumberField label="Opacity · bp" value={opacity} min={0} max={10000} onChange={setOpacity} onBlur={() => void commitPatch({ opacity: parseFrame(opacity) ?? clip.opacity })} /></div> : null}
    <div className="inspector-section"><div className="section-label"><span>Audio</span><AudioLines aria-hidden="true" /></div><label className="toggle-row"><span>Clip audio</span><input type="checkbox" checked={audioEnabled} onChange={(event) => { const checked = event.target.checked; setAudioEnabled(checked); void commitPatch({ audioEnabled: checked }); }} /></label><NumberField label="Gain · dB" value={gainDb} min={-60} max={12} step="0.1" onChange={setGainDb} onBlur={() => { const parsed = Number(gainDb); if (Number.isFinite(parsed)) void commitPatch({ gainDb: parsed }); }} /><div className="field-row"><NumberField label="Fade in" value={fadeInFrames} min={0} onChange={setFadeInFrames} onBlur={() => void commitPatch({ fadeInFrames: parseFrame(fadeInFrames) ?? clip.fadeInFrames })} /><NumberField label="Fade out" value={fadeOutFrames} min={0} onChange={setFadeOutFrames} onBlur={() => void commitPatch({ fadeOutFrames: parseFrame(fadeOutFrames) ?? clip.fadeOutFrames })} /></div></div>
    {isVideoTrack ? <div className="inspector-section"><div className="section-label"><span>Transitions</span><ChevronDown aria-hidden="true" /></div>{transitions.length > 0 ? transitions.map((transition) => <div className="transition-row" key={transition.id}><span>Dissolve · {transition.durationFrames}f</span><Button variant="ghost" size="icon" aria-label="Remove dissolve" onClick={() => void removeTransition(transition)}><Trash2 aria-hidden="true" /></Button></div>) : <p className="small-note">No explicit dissolve on this clip.</p>}{nextClip ? <div className="transition-add"><NumberField label="Frames" value={transitionDuration} min={2} onChange={setTransitionDuration} /><Button variant="secondary" size="sm" onClick={() => void addTransition()}><Plus aria-hidden="true" />Dissolve next</Button></div> : null}</div> : null}
    {track ? <div className="inspector-section"><div className="section-label"><span>Track</span>{track.locked ? <Lock aria-hidden="true" /> : <Unlock aria-hidden="true" />}</div><p className="small-note">{track.name} · {track.locked ? "Locked" : track.muted ? "Muted" : "Active"}</p></div> : null}
  </div>;
}

function TextInspector({ snapshot, item, onEdit, onNotice }: { snapshot: ProjectSnapshot; item: TextItem; onEdit: (label: string, operations: readonly EditOp[]) => Promise<void>; onNotice: (notice: string) => void }) {
  const [text, setText] = useState(item.text);
  const [style, setStyle] = useState(item.style);
  const [fontSize, setFontSize] = useState(String(item.fontSize));
  const [positionX, setPositionX] = useState(String(item.positionX));
  const [positionY, setPositionY] = useState(String(item.positionY));
  useEffect(() => { setText(item.text); setStyle(item.style); setFontSize(String(item.fontSize)); setPositionX(String(item.positionX)); setPositionY(String(item.positionY)); }, [item.id, item.text, item.style, item.fontSize, item.positionX, item.positionY]);
  const update = async (patch: Extract<EditOp, { op: "update_text" }>["patch"]) => {
    try { await onEdit("Update text", [{ op: "update_text", textId: item.id, patch }]); } catch { /* Workspace owns error notice. */ }
  };
  const remove = async () => {
    try { await onEdit("Remove text", [{ op: "remove_text", textId: item.id }]); } catch { /* Workspace owns error notice. */ }
  };
  const numeric = (value: string, fallback: number) => { const parsed = Number(value); return Number.isFinite(parsed) ? parsed : fallback; };
  return <div className="panel-stack inspector-panel"><div className="panel-heading"><div><p className="eyebrow">Inspector</p><h2>{item.kind === "caption" ? "Caption" : "Title"}</h2></div><Captions aria-hidden="true" className="panel-heading-icon" /></div><div className="inspector-section"><label className="field-group"><span className="field-label">Text</span><textarea rows={4} value={text} onChange={(event) => setText(event.target.value)} onBlur={() => void update({ text })} /></label><label className="field-group"><span className="field-label">Style</span><select value={style} onChange={(event) => { const nextStyle = event.target.value as typeof style; setStyle(nextStyle); void update({ style: nextStyle }); }}><option value="clean">Clean</option><option value="boxed">Boxed</option></select></label></div><div className="inspector-section"><div className="field-row"><NumberField label="Size" value={fontSize} min={8} max={240} onChange={setFontSize} onBlur={() => void update({ fontSize: numeric(fontSize, item.fontSize) })} /><NumberField label="X · bp" value={positionX} min={0} max={10000} onChange={setPositionX} onBlur={() => void update({ positionX: numeric(positionX, item.positionX) })} /><NumberField label="Y · bp" value={positionY} min={0} max={10000} onChange={setPositionY} onBlur={() => void update({ positionY: numeric(positionY, item.positionY) })} /></div></div><Button variant="ghost" size="sm" onClick={() => void remove()}><Trash2 aria-hidden="true" />Remove {item.kind}</Button></div>;
}

function NumberField({ label, value, onChange, onBlur, min, max, step }: { label: string; value: string; onChange: (value: string) => void; onBlur?: () => void; min?: number; max?: number; step?: string }) {
  return <label className="field-group"><span className="field-label">{label}</span><input type="number" value={value} min={min} max={max} step={step ?? "1"} onChange={(event) => onChange(event.target.value)} onBlur={onBlur} /></label>;
}

function parseFrame(value: string): number | null {
  const parsed = Number(value);
  if (!Number.isSafeInteger(parsed) || parsed < 0) return null;
  return parsed;
}
