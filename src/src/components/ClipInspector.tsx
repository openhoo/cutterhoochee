import { memo, useCallback, useEffect, useMemo, useState } from "react";
import { AlignCenter, AudioLines, Captions, ChevronDown, Crop, Film, Lock, Plus, SlidersHorizontal, Trash2, Unlock } from "lucide-react";

import type { AgentActivity, ClipPatch, EditOp, EditorClient, MediaClip, ProjectSnapshot, TextItem, TimelineSelection, Track, Transition } from "@cutterhoochee/shared";
import { Button } from "@/components/ui/button";
import { formatTimecode, transitionRemovalOps } from "@/lib/native";
import { useAgentActivities, type ActivityReveal, type AgentActivityStore } from "@/activity/AgentActivityStore";

type ConfirmedFieldChange = {
  activityId: string;
  sequence: number;
  before: string;
  after: string;
};

function committedActivity(activity: AgentActivity): boolean {
  return activity.phase === "completed" && activity.changed && !activity.dryRun;
}

function inspectorFieldValue(entity: MediaClip | TextItem, field: string): string | undefined {
  switch (field) {
    case "startFrame":
      return "startFrame" in entity ? String(entity.startFrame) : undefined;
    case "inFrame":
      return "inFrame" in entity ? String(entity.inFrame) : undefined;
    case "durationFrames":
      return "durationFrames" in entity ? String(entity.durationFrames) : undefined;
    case "fit":
      return "fit" in entity ? entity.fit : undefined;
    case "centerX":
      return "centerX" in entity ? String(entity.centerX) : undefined;
    case "centerY":
      return "centerY" in entity ? String(entity.centerY) : undefined;
    case "scale":
      return "scale" in entity ? String(entity.scale) : undefined;
    case "opacity":
      return "opacity" in entity ? String(entity.opacity) : undefined;
    case "gainDb":
      return "gainDb" in entity ? String(entity.gainDb) : undefined;
    case "audioEnabled":
      return "audioEnabled" in entity ? String(entity.audioEnabled) : undefined;
    case "fadeInFrames":
      return "fadeInFrames" in entity ? String(entity.fadeInFrames) : undefined;
    case "fadeOutFrames":
      return "fadeOutFrames" in entity ? String(entity.fadeOutFrames) : undefined;
    case "text":
      return "text" in entity ? entity.text : undefined;
    case "style":
      return "style" in entity ? entity.style : undefined;
    case "fontSize":
      return "fontSize" in entity ? String(entity.fontSize) : undefined;
    case "positionX":
      return "positionX" in entity ? String(entity.positionX) : undefined;
    case "positionY":
      return "positionY" in entity ? String(entity.positionY) : undefined;
    case "sourceStartFrame":
      return "sourceStartFrame" in entity && entity.sourceStartFrame !== undefined ? String(entity.sourceStartFrame) : undefined;
    case "sourceDurationFrames":
      return "sourceDurationFrames" in entity && entity.sourceDurationFrames !== undefined ? String(entity.sourceDurationFrames) : undefined;
    default:
      return undefined;
  }
}

function confirmedFieldChanges(activities: readonly AgentActivity[], kind: "clip" | "text", entity: MediaClip | TextItem | undefined): ReadonlyMap<string, ConfirmedFieldChange> {
  const changes = new Map<string, ConfirmedFieldChange>();
  if (!entity) return changes;
  for (let activityIndex = 0; activityIndex < Math.min(48, activities.length); activityIndex += 1) {
    const activity = activities[activityIndex];
    if (!committedActivity(activity)) continue;
    for (const change of activity.changes) {
      if (change.target.kind !== kind || change.target.id !== entity.id) continue;
      for (const field of change.fields) {
        const current = inspectorFieldValue(entity, field.field);
        if (current === undefined || current !== field.after || changes.has(field.field)) continue;
        changes.set(field.field, { activityId: activity.id, sequence: activity.sequence, before: field.before, after: field.after });
      }
    }
  }
  return changes;
}

function fieldClassName(activity?: ConfirmedFieldChange, locked = false): string {
  return ["field-group", activity ? "agent-inspector-field" : "", locked ? "agent-locked-field" : ""].filter(Boolean).join(" ");
}

function ActivityDelta({ change }: { change?: ConfirmedFieldChange }) {
  if (!change) return null;
  return <span className="agent-inspector-delta" aria-label={`Confirmed value changed from ${change.before} to ${change.after}`}>{change.before} → {change.after}</span>;
}

export const ClipInspector = memo(function ClipInspector({
  client: _client,
  snapshot,
  clip,
  track,
  transitions,
  selection,
  activityStore,
  revealActivity,
  onEdit,
  onNotice,
}: {
  client: EditorClient;
  snapshot: ProjectSnapshot;
  clip?: MediaClip;
  track?: Track;
  transitions: Transition[];
  selection: TimelineSelection;
  activityStore: AgentActivityStore;
  revealActivity?: ActivityReveal | null;
  onEdit: (label: string, operations: readonly EditOp[]) => Promise<void>;
  onNotice: (notice: string) => void;
}) {
  const selectedText = useMemo(() => {
    const selectedTextIds = new Set(selection.textIds);
    return snapshot.document.textItems.find((item) => selectedTextIds.has(item.id));
  }, [selection.textIds, snapshot.document.textItems]);
  const activities = useAgentActivities(activityStore);
  const visibleActivities = useMemo(() => activities.filter((activity) => !activityStore.isHydrated(activity.id)), [activities, activityStore]);
  const clipActivityFields = useMemo(() => confirmedFieldChanges(visibleActivities, "clip", clip), [visibleActivities, clip]);
  const textActivityFields = useMemo(() => confirmedFieldChanges(visibleActivities, "text", selectedText), [visibleActivities, selectedText]);
  const revealRequestId = revealActivity?.requestId;
  if (selectedText) return <TextInspector snapshot={snapshot} item={selectedText} track={snapshot.document.tracks.find((candidate) => candidate.id === selectedText.trackId)} activityFields={textActivityFields} revealRequestId={revealRequestId} onEdit={onEdit} onNotice={onNotice} />;
  if (!clip) return <EmptyInspector />;
  return <ClipFields snapshot={snapshot} clip={clip} track={track} transitions={transitions} activityFields={clipActivityFields} revealRequestId={revealRequestId} onEdit={onEdit} onNotice={onNotice} />;
});

function EmptyInspector() {
  return <div className="empty-panel inspector-empty"><SlidersHorizontal aria-hidden="true" /><strong>Nothing selected</strong><span>Select a clip, title, or caption to edit its timing and properties.</span></div>;
}

function ClipFields({ snapshot, clip, track, transitions, activityFields, revealRequestId, onEdit, onNotice }: { snapshot: ProjectSnapshot; clip: MediaClip; track?: Track; transitions: Transition[]; activityFields: ReadonlyMap<string, ConfirmedFieldChange>; revealRequestId?: number; onEdit: (label: string, operations: readonly EditOp[]) => Promise<void>; onNotice: (notice: string) => void }) {
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
  const locked = Boolean(track?.locked);

  const resetDraft = useCallback(() => {
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
  }, [clip.audioEnabled, clip.centerX, clip.centerY, clip.durationFrames, clip.fadeInFrames, clip.fadeOutFrames, clip.fit, clip.gainDb, clip.id, clip.inFrame, clip.opacity, clip.scale, clip.startFrame]);

  useEffect(() => {
    resetDraft();
  }, [resetDraft]);

  const sourceAsset = snapshot.document.assets.find((asset) => asset.id === clip.assetId);
  const isVideoTrack = track?.kind === "video";
  const nextClip = useMemo(() => snapshot.document.clips.filter((candidate) => candidate.trackId === clip.trackId && candidate.id !== clip.id).sort((a, b) => a.startFrame - b.startFrame).find((candidate) => candidate.startFrame >= clip.startFrame + clip.durationFrames), [clip, snapshot.document.clips]);
  const commitPatch = async (patch: ClipPatch) => {
    if (locked) return;
    try {
      await onEdit("Update clip", [{ op: "update_clip", clipId: clip.id, patch }]);
    } catch {
      resetDraft();
    }
  };
  const commitTrim = async () => {
    if (locked) return;
    const nextStart = parseFrame(startFrame);
    const nextIn = parseFrame(inFrame);
    const nextDuration = parseFrame(durationFrames);
    if (nextStart === null || nextIn === null || nextDuration === null || nextDuration <= 0) {
      resetDraft();
      onNotice("Start, source in, and duration must be non-negative whole frames.");
      return;
    }
    try {
      await onEdit("Trim clip", [...transitionRemovalOps(snapshot, clip.id), { op: "trim_clip", clipId: clip.id, inFrame: nextIn, startFrame: nextStart, durationFrames: nextDuration }]);
    } catch {
      resetDraft();
    }
  };
  const addTransition = async () => {
    if (locked) return;
    if (!nextClip) {
      onNotice("A dissolve needs the next clip on this video track.");
      return;
    }
    const duration = parseFrame(transitionDuration);
    if (duration === null || duration < 2 || duration >= Math.min(clip.durationFrames, nextClip.durationFrames)) {
      setTransitionDuration("15");
      onNotice("Transition duration must be at least 2 frames and shorter than both clips.");
      return;
    }
    try {
      await onEdit("Add dissolve", [{ op: "add_transition", leftClipId: clip.id, rightClipId: nextClip.id, durationFrames: duration }]);
    } catch {
      setTransitionDuration("15");
    }
  };
  const removeTransition = async (transition: Transition) => {
    if (locked) return;
    try {
      await onEdit("Remove dissolve", [{ op: "remove_transition", transitionId: transition.id }]);
    } catch {
      // Workspace owns the authoritative error notice.
    }
  };

  return <div className="panel-stack inspector-panel" data-reveal-request-id={revealRequestId}>
    <div className="panel-heading"><div><p className="eyebrow">Inspector</p><h2>{sourceAsset?.original.fileName ?? "Clip"}</h2></div><Film aria-hidden="true" className="panel-heading-icon" /></div>
    <div className="inspector-section"><div className="section-label"><span>Timing</span><span className="section-hint">{formatTimecode(clip.startFrame, snapshot.document.profile.fpsNum, snapshot.document.profile.fpsDen)}</span></div><div className="field-row"><NumberField label="Start" value={startFrame} activity={activityFields.get("startFrame")} disabled={locked} onChange={setStartFrame} /><NumberField label="Source in" value={inFrame} activity={activityFields.get("inFrame")} disabled={locked} onChange={setInFrame} /><NumberField label="Duration" value={durationFrames} activity={activityFields.get("durationFrames")} disabled={locked} onChange={setDurationFrames} /></div><Button variant="secondary" size="sm" disabled={locked} onClick={() => void commitTrim()}>Apply timing</Button></div>
    {isVideoTrack ? <div className="inspector-section"><div className="section-label"><span>Canvas</span><Crop aria-hidden="true" /></div><label className={fieldClassName(activityFields.get("fit"), locked)}><span className="field-label">Fit</span><select value={fit} disabled={locked} onChange={(event) => { const nextFit = event.target.value as typeof fit; setFit(nextFit); void commitPatch({ fit: nextFit }); }}><option value="contain">Contain</option><option value="cover">Cover</option></select><ActivityDelta change={activityFields.get("fit")} /></label><div className="field-row"><NumberField label="X · bp" value={centerX} activity={activityFields.get("centerX")} min={0} max={10000} disabled={locked} onChange={setCenterX} onBlur={() => { const parsed = parseFrame(centerX); if (parsed === null) { resetDraft(); return; } void commitPatch({ centerX: parsed }); }} /><NumberField label="Y · bp" value={centerY} activity={activityFields.get("centerY")} min={0} max={10000} disabled={locked} onChange={setCenterY} onBlur={() => { const parsed = parseFrame(centerY); if (parsed === null) { resetDraft(); return; } void commitPatch({ centerY: parsed }); }} /></div><NumberField label="Scale · bp" value={scale} activity={activityFields.get("scale")} min={100} max={40000} disabled={locked} onChange={setScale} onBlur={() => { const parsed = parseFrame(scale); if (parsed === null) { resetDraft(); return; } void commitPatch({ scale: parsed }); }} /><NumberField label="Opacity · bp" value={opacity} activity={activityFields.get("opacity")} min={0} max={10000} disabled={locked} onChange={setOpacity} onBlur={() => { const parsed = parseFrame(opacity); if (parsed === null) { resetDraft(); return; } void commitPatch({ opacity: parsed }); }} /></div> : null}
    <div className="inspector-section"><div className="section-label"><span>Audio</span><AudioLines aria-hidden="true" /></div><label className={fieldClassName(activityFields.get("audioEnabled"), locked)}><span className="field-label">Clip audio</span><input type="checkbox" checked={audioEnabled} disabled={locked} onChange={(event) => { const checked = event.target.checked; setAudioEnabled(checked); void commitPatch({ audioEnabled: checked }); }} /><ActivityDelta change={activityFields.get("audioEnabled")} /></label><NumberField label="Gain · dB" value={gainDb} activity={activityFields.get("gainDb")} min={-60} max={12} step="0.1" disabled={locked} onChange={setGainDb} onBlur={() => { const parsed = Number(gainDb); if (!Number.isFinite(parsed)) { resetDraft(); return; } void commitPatch({ gainDb: parsed }); }} /><div className="field-row"><NumberField label="Fade in" value={fadeInFrames} activity={activityFields.get("fadeInFrames")} min={0} disabled={locked} onChange={setFadeInFrames} onBlur={() => { const parsed = parseFrame(fadeInFrames); if (parsed === null) { resetDraft(); return; } void commitPatch({ fadeInFrames: parsed }); }} /><NumberField label="Fade out" value={fadeOutFrames} activity={activityFields.get("fadeOutFrames")} min={0} disabled={locked} onChange={setFadeOutFrames} onBlur={() => { const parsed = parseFrame(fadeOutFrames); if (parsed === null) { resetDraft(); return; } void commitPatch({ fadeOutFrames: parsed }); }} /></div></div>
    {isVideoTrack ? <div className="inspector-section"><div className="section-label"><span>Transitions</span><ChevronDown aria-hidden="true" /></div>{transitions.length > 0 ? transitions.map((transition) => <div className="transition-row" key={transition.id}><span>Dissolve · {transition.durationFrames}f</span><Button variant="ghost" size="icon" aria-label="Remove dissolve" disabled={locked} onClick={() => void removeTransition(transition)}><Trash2 aria-hidden="true" /></Button></div>) : <p className="small-note">No explicit dissolve on this clip.</p>}{nextClip ? <div className="transition-add"><NumberField label="Frames" value={transitionDuration} min={2} disabled={locked} onChange={setTransitionDuration} /><Button variant="secondary" size="sm" disabled={locked} onClick={() => void addTransition()}><Plus aria-hidden="true" />Dissolve next</Button></div> : null}</div> : null}
    {track ? <div className="inspector-section"><div className="section-label"><span>Track</span>{track.locked ? <Lock aria-hidden="true" /> : <Unlock aria-hidden="true" />}</div><p className="small-note">{track.name} · {track.locked ? "Locked" : track.muted ? "Muted" : "Active"}</p></div> : null}
  </div>;
}

function TextInspector({ snapshot: _snapshot, item, track, activityFields, revealRequestId, onEdit, onNotice: _onNotice }: { snapshot: ProjectSnapshot; item: TextItem; track?: Track; activityFields: ReadonlyMap<string, ConfirmedFieldChange>; revealRequestId?: number; onEdit: (label: string, operations: readonly EditOp[]) => Promise<void>; onNotice: (notice: string) => void }) {
  const [text, setText] = useState(item.text);
  const [style, setStyle] = useState(item.style);
  const [fontSize, setFontSize] = useState(String(item.fontSize));
  const [positionX, setPositionX] = useState(String(item.positionX));
  const [positionY, setPositionY] = useState(String(item.positionY));
  const locked = Boolean(track?.locked);
  const resetDraft = useCallback(() => {
    setText(item.text);
    setStyle(item.style);
    setFontSize(String(item.fontSize));
    setPositionX(String(item.positionX));
    setPositionY(String(item.positionY));
  }, [item.fontSize, item.id, item.positionX, item.positionY, item.style, item.text]);
  useEffect(() => {
    resetDraft();
  }, [resetDraft]);
  const update = async (patch: Extract<EditOp, { op: "update_text" }>["patch"]) => {
    if (locked) return;
    try {
      await onEdit("Update text", [{ op: "update_text", textId: item.id, patch }]);
    } catch {
      resetDraft();
    }
  };
  const remove = async () => {
    if (locked) return;
    try {
      await onEdit("Remove text", [{ op: "remove_text", textId: item.id }]);
    } catch {
      // Workspace owns the authoritative error notice.
    }
  };
  const numeric = (value: string): number | null => {
    const parsed = Number(value);
    if (!Number.isFinite(parsed)) {
      resetDraft();
      return null;
    }
    return parsed;
  };

  return <div className="panel-stack inspector-panel" data-reveal-request-id={revealRequestId}>
    <div className="panel-heading"><div><p className="eyebrow">Inspector</p><h2>{item.kind === "caption" ? "Caption" : "Title"}</h2></div><Captions aria-hidden="true" className="panel-heading-icon" /></div>
    <div className="inspector-section"><label className={fieldClassName(activityFields.get("text"), locked)}><span className="field-label">Text</span><textarea rows={4} value={text} disabled={locked} onChange={(event) => setText(event.target.value)} onBlur={() => void update({ text })} /><ActivityDelta change={activityFields.get("text")} /></label><label className={fieldClassName(activityFields.get("style"), locked)}><span className="field-label">Style</span><select value={style} disabled={locked} onChange={(event) => { const nextStyle = event.target.value as typeof style; setStyle(nextStyle); void update({ style: nextStyle }); }}><option value="clean">Clean</option><option value="boxed">Boxed</option></select><ActivityDelta change={activityFields.get("style")} /></label></div>
    <div className="inspector-section"><div className="field-row"><NumberField label="Size" value={fontSize} activity={activityFields.get("fontSize")} min={8} max={240} disabled={locked} onChange={setFontSize} onBlur={() => { const parsed = numeric(fontSize); if (parsed !== null) void update({ fontSize: parsed }); }} /><NumberField label="X · bp" value={positionX} activity={activityFields.get("positionX")} min={0} max={10000} disabled={locked} onChange={setPositionX} onBlur={() => { const parsed = numeric(positionX); if (parsed !== null) void update({ positionX: parsed }); }} /><NumberField label="Y · bp" value={positionY} activity={activityFields.get("positionY")} min={0} max={10000} disabled={locked} onChange={setPositionY} onBlur={() => { const parsed = numeric(positionY); if (parsed !== null) void update({ positionY: parsed }); }} /></div></div>
    <Button variant="ghost" size="sm" disabled={locked} onClick={() => void remove()}><Trash2 aria-hidden="true" />Remove {item.kind}</Button>
  </div>;
}

function NumberField({ label, value, activity, onChange, onBlur, min, max, step, disabled }: { label: string; value: string; activity?: ConfirmedFieldChange; onChange: (value: string) => void; onBlur?: () => void; min?: number; max?: number; step?: string; disabled?: boolean }) {
  return <label className={fieldClassName(activity, disabled)}><span className="field-label">{label}</span><input type="number" value={value} min={min} max={max} step={step ?? "1"} disabled={disabled} onChange={(event) => onChange(event.target.value)} onBlur={onBlur} /><ActivityDelta change={activity} /></label>;
}

function parseFrame(value: string): number | null {
  const parsed = Number(value);
  if (!Number.isSafeInteger(parsed) || parsed < 0) return null;
  return parsed;
}
