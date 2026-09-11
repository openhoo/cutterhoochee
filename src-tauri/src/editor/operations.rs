use crate::editor::temporal;
use crate::error::{AppError, ErrorCode};
use crate::project::model::{
    AspectRatio, AssetKind, AssetManifest, FitMode, FrameInterval, MediaClip, ProjectDocument,
    RgbaColor, TextItem, TextKind, TextStyle, Track, TrackKind,
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use ts_rs::TS;
use uuid::Uuid;

fn invalid(message: impl Into<String>) -> AppError {
    AppError::invalid_argument(message)
}

fn safe_add(left: u64, right: u64, field: &str) -> Result<u64, AppError> {
    let result = left
        .checked_add(right)
        .ok_or_else(|| invalid(format!("{field} overflows the safe integer range")))?;
    crate::ipc::validate_safe_integer(result, field)?;
    Ok(result)
}

fn parse_uuid(value: &str, field: &str) -> Result<(), AppError> {
    let id = Uuid::parse_str(value).map_err(|_| invalid(format!("{field} must be a UUID")))?;
    if id.is_nil() {
        return Err(invalid(format!("{field} must not be the nil UUID")));
    }
    Ok(())
}

fn validate_text(value: &str, field: &str, required: bool) -> Result<(), AppError> {
    if required && value.trim().is_empty() {
        return Err(invalid(format!("{field} must not be empty")));
    }
    if value.as_bytes().contains(&0) {
        return Err(invalid(format!("{field} contains a NUL byte")));
    }
    if value.len() > 1024 * 1024 {
        return Err(invalid(format!("{field} exceeds the supported size")));
    }
    Ok(())
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[ts(rename_all = "camelCase")]
pub struct ClipPatch {
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub fit: Option<FitMode>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub center_x: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub center_y: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub scale: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub opacity: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional, type = "number")]
    pub gain_db: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub audio_enabled: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional, type = "SafeInteger")]
    pub fade_in_frames: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional, type = "SafeInteger")]
    pub fade_out_frames: Option<u64>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[ts(rename_all = "camelCase")]
pub struct TextPatch {
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub style: Option<TextStyle>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub color: Option<RgbaColor>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub font_size: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub position_x: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub position_y: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub line_breaks: Option<Vec<u32>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional, type = "SafeInteger")]
    pub start_frame: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional, type = "SafeInteger")]
    pub duration_frames: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional, type = "SafeInteger")]
    pub source_start_frame: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional, type = "SafeInteger")]
    pub source_duration_frames: Option<u64>,
}

/// A local transcript is a source-coordinate contract. The operation layer
/// projects its spans into a clip; no caller-supplied path is accepted here.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[ts(rename_all = "camelCase")]
pub struct Transcript {
    pub transcript_id: String,
    pub asset_id: String,
    pub source_hash: String,
    pub segments: Vec<TranscriptSpan>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[ts(rename_all = "camelCase")]
pub struct TranscriptSpan {
    #[ts(type = "SafeInteger")]
    pub start_frame: u64,
    #[ts(type = "SafeInteger")]
    pub end_frame: u64,
    pub text: String,
    pub approximate: bool,
}

impl Transcript {
    pub fn validate(&self) -> Result<(), AppError> {
        parse_uuid(&self.transcript_id, "transcriptId")?;
        parse_uuid(&self.asset_id, "assetId")?;
        validate_text(&self.source_hash, "sourceHash", true)?;
        if self.segments.is_empty() {
            return Err(invalid("A transcript must contain at least one segment"));
        }
        let mut previous_end = 0;
        for (index, segment) in self.segments.iter().enumerate() {
            crate::ipc::validate_safe_integer(
                segment.start_frame,
                &format!("segments[{index}].startFrame"),
            )?;
            crate::ipc::validate_safe_integer(
                segment.end_frame,
                &format!("segments[{index}].endFrame"),
            )?;
            if segment.start_frame >= segment.end_frame {
                return Err(invalid(format!(
                    "segments[{index}] must have a positive interval"
                )));
            }
            if index > 0 && segment.start_frame < previous_end {
                return Err(invalid(
                    "Transcript segments must be ordered and non-overlapping",
                ));
            }
            validate_text(&segment.text, &format!("segments[{index}].text"), true)?;
            previous_end = segment.end_frame;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(tag = "op", rename_all = "snake_case")]
#[ts(tag = "op", rename_all = "snake_case")]
pub enum EditOp {
    SetProject {
        #[serde(skip_serializing_if = "Option::is_none")]
        #[ts(optional)]
        name: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        #[ts(optional)]
        aspect: Option<AspectRatio>,
    },
    AddTrack {
        id: String,
        kind: TrackKind,
        name: String,
        #[ts(type = "SafeInteger")]
        index: u32,
    },
    UpdateTrack {
        #[serde(rename = "trackId")]
        #[ts(rename = "trackId")]
        track_id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        #[ts(optional)]
        name: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        #[ts(optional)]
        muted: Option<bool>,
        #[serde(skip_serializing_if = "Option::is_none")]
        #[ts(optional)]
        locked: Option<bool>,
    },
    RemoveTrack {
        #[serde(rename = "trackId")]
        #[ts(rename = "trackId")]
        track_id: String,
        #[serde(rename = "deleteItems")]
        #[ts(rename = "deleteItems")]
        delete_items: bool,
    },
    InsertClip {
        clip: MediaClip,
    },
    MoveClip {
        #[serde(rename = "clipId")]
        #[ts(rename = "clipId")]
        clip_id: String,
        #[serde(rename = "trackId")]
        #[ts(rename = "trackId")]
        track_id: String,
        #[serde(rename = "startFrame")]
        #[ts(rename = "startFrame", type = "SafeInteger")]
        start_frame: u64,
    },
    TrimClip {
        #[serde(rename = "clipId")]
        #[ts(rename = "clipId")]
        clip_id: String,
        #[serde(rename = "inFrame")]
        #[ts(rename = "inFrame", type = "SafeInteger")]
        in_frame: u64,
        #[serde(rename = "startFrame")]
        #[ts(rename = "startFrame", type = "SafeInteger")]
        start_frame: u64,
        #[serde(rename = "durationFrames")]
        #[ts(rename = "durationFrames", type = "SafeInteger")]
        duration_frames: u64,
    },
    SplitClip {
        #[serde(rename = "clipId")]
        #[ts(rename = "clipId")]
        clip_id: String,
        #[ts(type = "SafeInteger")]
        frame: u64,
        #[serde(rename = "rightClipId")]
        #[ts(rename = "rightClipId")]
        right_clip_id: String,
    },
    UpdateClip {
        #[serde(rename = "clipId")]
        #[ts(rename = "clipId")]
        clip_id: String,
        patch: ClipPatch,
    },
    RemoveClips {
        #[serde(rename = "clipIds")]
        #[ts(rename = "clipIds")]
        clip_ids: Vec<String>,
    },
    RemoveRange {
        #[serde(rename = "startFrame")]
        #[ts(rename = "startFrame", type = "SafeInteger")]
        start_frame: u64,
        #[serde(rename = "endFrame")]
        #[ts(rename = "endFrame", type = "SafeInteger")]
        end_frame: u64,
        ripple: bool,
    },
    AddText {
        item: TextItem,
    },
    UpdateText {
        #[serde(rename = "textId")]
        #[ts(rename = "textId")]
        text_id: String,
        patch: TextPatch,
    },
    RemoveText {
        #[serde(rename = "textId")]
        #[ts(rename = "textId")]
        text_id: String,
    },
    AddTransition {
        #[serde(rename = "leftClipId")]
        #[ts(rename = "leftClipId")]
        left_clip_id: String,
        #[serde(rename = "rightClipId")]
        #[ts(rename = "rightClipId")]
        right_clip_id: String,
        #[serde(rename = "durationFrames")]
        #[ts(rename = "durationFrames", type = "SafeInteger")]
        duration_frames: u64,
    },
    RemoveTransition {
        #[serde(rename = "transitionId")]
        #[ts(rename = "transitionId")]
        transition_id: String,
    },
    ReplaceCaptions {
        #[serde(rename = "clipId")]
        #[ts(rename = "clipId")]
        clip_id: String,
        #[serde(rename = "transcriptId")]
        #[ts(rename = "transcriptId")]
        transcript_id: String,
        style: TextStyle,
    },
}

fn id_already_used(document: &ProjectDocument, id: &str) -> bool {
    document.assets.iter().any(|item| item.id == id)
        || document.tracks.iter().any(|item| item.id == id)
        || document.clips.iter().any(|item| item.id == id)
        || document.text_items.iter().any(|item| item.id == id)
        || document.transitions.iter().any(|item| item.id == id)
}

fn ensure_new_id(document: &ProjectDocument, id: &str, field: &str) -> Result<(), AppError> {
    parse_uuid(id, field)?;
    if id_already_used(document, id) {
        return Err(invalid(format!(
            "{field} is already used by another entity"
        )));
    }
    Ok(())
}

fn track_index(document: &ProjectDocument, id: &str) -> Result<usize, AppError> {
    parse_uuid(id, "trackId")?;
    document
        .tracks
        .iter()
        .position(|track| track.id == id)
        .ok_or_else(|| invalid("The requested track does not exist"))
}

fn clip_index(document: &ProjectDocument, id: &str) -> Result<usize, AppError> {
    parse_uuid(id, "clipId")?;
    document
        .clips
        .iter()
        .position(|clip| clip.id == id)
        .ok_or_else(|| invalid("The requested clip does not exist"))
}

fn text_index(document: &ProjectDocument, id: &str) -> Result<usize, AppError> {
    parse_uuid(id, "textId")?;
    document
        .text_items
        .iter()
        .position(|text| text.id == id)
        .ok_or_else(|| invalid("The requested text item does not exist"))
}

fn transition_index(document: &ProjectDocument, id: &str) -> Result<usize, AppError> {
    parse_uuid(id, "transitionId")?;
    document
        .transitions
        .iter()
        .position(|transition| transition.id == id)
        .ok_or_else(|| invalid("The requested transition does not exist"))
}

fn clip_asset<'a>(
    document: &'a ProjectDocument,
    clip: &MediaClip,
) -> Result<&'a AssetManifest, AppError> {
    document
        .assets
        .iter()
        .find(|asset| asset.id == clip.asset_id)
        .ok_or_else(|| invalid("The clip asset does not exist"))
}

fn clip_track<'a>(document: &'a ProjectDocument, clip: &MediaClip) -> Result<&'a Track, AppError> {
    document
        .tracks
        .iter()
        .find(|track| track.id == clip.track_id)
        .ok_or_else(|| invalid("The clip track does not exist"))
}

fn ensure_track_unlocked(document: &ProjectDocument, id: &str) -> Result<(), AppError> {
    let track = document
        .tracks
        .iter()
        .find(|track| track.id == id)
        .ok_or_else(|| invalid("The requested track does not exist"))?;
    if track.locked {
        return Err(invalid(format!("Track {} is locked", track.name)));
    }
    Ok(())
}

/// A clip mutation can move source-owned captions even when the caption is on
/// a separate text track. Both the clip track and every owned caption track
/// therefore participate in the lock check.
fn ensure_clip_mutable(document: &ProjectDocument, clip_id: &str) -> Result<usize, AppError> {
    let index = clip_index(document, clip_id)?;
    let track_id = document.clips[index].track_id.clone();
    ensure_track_unlocked(document, &track_id)?;
    let owned_track_ids: Vec<String> = document
        .text_items
        .iter()
        .filter(|text| text.owner_clip_id.as_deref() == Some(clip_id))
        .map(|text| text.track_id.clone())
        .collect();
    for track_id in owned_track_ids {
        ensure_track_unlocked(document, &track_id)?;
    }
    Ok(index)
}

fn ensure_text_mutable(document: &ProjectDocument, index: usize) -> Result<(), AppError> {
    let text = &document.text_items[index];
    ensure_track_unlocked(document, &text.track_id)?;
    if let Some(clip_id) = text.owner_clip_id.as_deref() {
        let clip_index = clip_index(document, clip_id)?;
        ensure_track_unlocked(document, &document.clips[clip_index].track_id)?;
    }
    Ok(())
}

fn validate_clip_against_document(
    document: &ProjectDocument,
    clip: &MediaClip,
) -> Result<(), AppError> {
    let track = clip_track(document, clip)?;
    let asset = clip_asset(document, clip)?;
    clip.validate(track, asset)
}

fn validate_text_against_document(
    document: &ProjectDocument,
    text: &TextItem,
) -> Result<(), AppError> {
    let tracks: HashMap<_, _> = document
        .tracks
        .iter()
        .map(|track| (track.id.clone(), track))
        .collect();
    let clips: HashMap<_, _> = document
        .clips
        .iter()
        .map(|clip| (clip.id.clone(), clip))
        .collect();
    text.validate(&tracks, &clips)
}

fn transition_clip_ids(document: &ProjectDocument, clip_id: &str) -> bool {
    document
        .transitions
        .iter()
        .any(|transition| transition.left_clip_id == clip_id || transition.right_clip_id == clip_id)
}

fn ensure_transition_track_mutable(
    document: &ProjectDocument,
    right_clip_id: &str,
) -> Result<(), AppError> {
    let index = clip_index(document, right_clip_id)?;
    let track_id = document.clips[index].track_id.clone();
    let shift_start_frame = document.clips[index].start_frame;
    let mut affected: Vec<String> = document
        .clips
        .iter()
        .filter(|clip| clip.track_id == track_id && clip.start_frame >= shift_start_frame)
        .map(|clip| clip.id.clone())
        .collect();
    affected.sort();
    for id in affected {
        ensure_clip_mutable(document, &id)?;
    }
    Ok(())
}

fn apply_set_project(
    document: &mut ProjectDocument,
    name: &Option<String>,
    aspect: &Option<AspectRatio>,
) -> Result<(), AppError> {
    if let Some(name) = name {
        validate_text(name, "name", true)?;
        document.name = name.clone();
    }
    if let Some(aspect) = aspect {
        document.profile =
            crate::project::model::ProjectProfile::for_aspect(*aspect, document.profile.fps())?;
    }
    Ok(())
}

fn apply_add_track(
    document: &mut ProjectDocument,
    id: &str,
    kind: TrackKind,
    name: &str,
    index: u32,
) -> Result<(), AppError> {
    ensure_new_id(document, id, "track.id")?;
    validate_text(name, "track.name", true)?;
    let index = usize::try_from(index).map_err(|_| invalid("Track index is invalid"))?;
    if index > document.tracks.len() {
        return Err(invalid("Track index is outside the track list"));
    }
    let track = Track::new(id.to_owned(), kind, name.to_owned())?;
    document.tracks.insert(index, track);
    Ok(())
}

fn apply_update_track(
    document: &mut ProjectDocument,
    track_id: &str,
    name: &Option<String>,
    muted: &Option<bool>,
    locked: &Option<bool>,
) -> Result<(), AppError> {
    let index = track_index(document, track_id)?;
    if document.tracks[index].locked
        && (name.is_some() || muted.is_some() || locked.is_none() || locked == &Some(true))
    {
        return Err(invalid("A locked track can only be unlocked"));
    }
    if let Some(name) = name {
        validate_text(name, "track.name", true)?;
        document.tracks[index].name = name.clone();
    }
    if let Some(muted) = muted {
        document.tracks[index].muted = *muted;
    }
    if let Some(locked) = locked {
        document.tracks[index].locked = *locked;
    }
    Ok(())
}

fn apply_remove_track(
    document: &mut ProjectDocument,
    track_id: &str,
    delete_items: bool,
) -> Result<(), AppError> {
    let index = track_index(document, track_id)?;
    if document.tracks[index].locked {
        return Err(invalid("A locked track cannot be removed"));
    }
    let clip_ids: Vec<String> = document
        .clips
        .iter()
        .filter(|clip| clip.track_id == track_id)
        .map(|clip| clip.id.clone())
        .collect();
    let text_ids: Vec<String> = document
        .text_items
        .iter()
        .filter(|text| text.track_id == track_id)
        .map(|text| text.id.clone())
        .collect();
    if (!clip_ids.is_empty() || !text_ids.is_empty()) && !delete_items {
        return Err(invalid(
            "The track is not empty; set deleteItems to remove its items",
        ));
    }
    for clip_id in &clip_ids {
        ensure_clip_mutable(document, clip_id)?;
    }
    for text_id in &text_ids {
        ensure_text_mutable(document, text_index(document, text_id)?)?;
    }
    if delete_items {
        let deleted: std::collections::HashSet<&str> =
            clip_ids.iter().map(String::as_str).collect();
        document
            .clips
            .retain(|clip| !deleted.contains(clip.id.as_str()));
        document.text_items.retain(|text| {
            text.track_id != track_id
                && text
                    .owner_clip_id
                    .as_deref()
                    .is_none_or(|owner| !deleted.contains(owner))
        });
        document.transitions.retain(|transition| {
            !deleted.contains(transition.left_clip_id.as_str())
                && !deleted.contains(transition.right_clip_id.as_str())
        });
    }
    document.tracks.remove(index);
    Ok(())
}

fn apply_insert_clip(document: &mut ProjectDocument, clip: &MediaClip) -> Result<(), AppError> {
    ensure_new_id(document, &clip.id, "clip.id")?;
    ensure_track_unlocked(document, &clip.track_id)?;
    validate_clip_against_document(document, clip).map_err(|error| {
        if error.code == ErrorCode::InvalidArgument {
            error
        } else {
            error
        }
    })?;
    document.clips.push(clip.clone());
    Ok(())
}

fn apply_move_clip(
    document: &mut ProjectDocument,
    clip_id: &str,
    track_id: &str,
    start_frame: u64,
) -> Result<(), AppError> {
    let index = ensure_clip_mutable(document, clip_id)?;
    if transition_clip_ids(document, clip_id) {
        return Err(invalid(
            "Remove the clip's transition in the same batch before moving it",
        ));
    }
    ensure_track_unlocked(document, track_id)?;
    let mut clip = document.clips[index].clone();
    clip.track_id = track_id.to_owned();
    clip.start_frame = start_frame;
    validate_clip_against_document(document, &clip)?;
    document.clips[index] = clip;
    Ok(())
}
fn apply_trim_clip(
    document: &mut ProjectDocument,
    clip_id: &str,
    in_frame: u64,
    start_frame: u64,
    duration_frames: u64,
) -> Result<(), AppError> {
    let index = clip_index(document, clip_id)?;
    if transition_clip_ids(document, clip_id) {
        return Err(invalid(
            "Remove the clip's transition in the same batch before trimming it",
        ));
    }

    let original = document.clips[index].clone();
    ensure_track_unlocked(document, &original.track_id)?;
    let mut clip = original.clone();
    clip.in_frame = in_frame;
    clip.start_frame = start_frame;
    clip.duration_frames = duration_frames;
    validate_clip_against_document(document, &clip)?;
    let retained_source = FrameInterval::from_start_duration(clip.in_frame, clip.duration_frames)?;

    let mut caption_updates: HashMap<String, Option<(u64, u64)>> = HashMap::new();
    for text in document
        .text_items
        .iter()
        .filter(|text| text.owner_clip_id.as_deref() == Some(clip_id))
    {
        let source = text
            .source_interval()
            .ok_or_else(|| invalid("Owned caption source interval is incomplete"))??;
        let old_projection = text
            .project_on_clip(&original)?
            .ok_or_else(|| invalid("Owned caption source interval does not intersect its clip"))?;
        let Some(overlap) = source.intersection(retained_source) else {
            ensure_track_unlocked(document, &text.track_id)?;
            caption_updates.insert(text.id.clone(), None);
            continue;
        };

        let mut updated = text.clone();
        updated.source_start_frame = Some(overlap.start_frame);
        updated.source_duration_frames = Some(overlap.duration_frames);
        let new_projection = updated.project_on_clip(&clip)?.ok_or_else(|| {
            invalid("Trimmed caption source interval does not intersect its clip")
        })?;
        if updated != *text || old_projection != new_projection {
            ensure_track_unlocked(document, &text.track_id)?;
        }
        caption_updates.insert(
            text.id.clone(),
            Some((overlap.start_frame, overlap.duration_frames)),
        );
    }

    document.clips[index] = clip;
    document.text_items.retain_mut(|text| {
        if text.owner_clip_id.as_deref() != Some(clip_id) {
            return true;
        }
        match caption_updates.get(&text.id) {
            Some(Some((start_frame, duration_frames))) => {
                text.source_start_frame = Some(*start_frame);
                text.source_duration_frames = Some(*duration_frames);
                true
            }
            Some(None) => false,
            None => true,
        }
    });
    Ok(())
}
fn apply_split_clip(
    document: &mut ProjectDocument,
    clip_id: &str,
    frame: u64,
    right_clip_id: &str,
) -> Result<(), AppError> {
    ensure_new_id(document, right_clip_id, "rightClipId")?;
    if right_clip_id == clip_id {
        return Err(invalid("The split right clip must have a different ID"));
    }
    temporal::split_clip(document, clip_id, frame, right_clip_id)
}

fn apply_update_clip(
    document: &mut ProjectDocument,
    clip_id: &str,
    patch: &ClipPatch,
) -> Result<(), AppError> {
    let index = ensure_clip_mutable(document, clip_id)?;
    let mut clip = document.clips[index].clone();
    if let Some(value) = patch.fit {
        clip.fit = value;
    }
    if let Some(value) = patch.center_x {
        clip.center_x = value;
    }
    if let Some(value) = patch.center_y {
        clip.center_y = value;
    }
    if let Some(value) = patch.scale {
        clip.scale = value;
    }
    if let Some(value) = patch.opacity {
        clip.opacity = value;
    }
    if let Some(value) = patch.gain_db {
        clip.gain_db = value;
    }
    if let Some(value) = patch.audio_enabled {
        clip.audio_enabled = value;
    }
    if let Some(value) = patch.fade_in_frames {
        clip.fade_in_frames = value;
    }
    if let Some(value) = patch.fade_out_frames {
        clip.fade_out_frames = value;
    }
    validate_clip_against_document(document, &clip)?;
    document.clips[index] = clip;
    Ok(())
}

fn apply_remove_clips(document: &mut ProjectDocument, clip_ids: &[String]) -> Result<(), AppError> {
    let mut seen = std::collections::HashSet::new();
    let mut indices = Vec::with_capacity(clip_ids.len());
    for id in clip_ids {
        if !seen.insert(id.as_str()) {
            return Err(invalid("removeClips cannot contain duplicate IDs"));
        }
        indices.push(ensure_clip_mutable(document, id)?);
    }
    let deleted: std::collections::HashSet<&str> = clip_ids.iter().map(String::as_str).collect();
    document
        .clips
        .retain(|clip| !deleted.contains(clip.id.as_str()));
    document.text_items.retain(|text| {
        text.owner_clip_id
            .as_deref()
            .is_none_or(|owner| !deleted.contains(owner))
    });
    document.transitions.retain(|transition| {
        !deleted.contains(transition.left_clip_id.as_str())
            && !deleted.contains(transition.right_clip_id.as_str())
    });
    let _ = indices;
    Ok(())
}

fn apply_remove_range(
    document: &mut ProjectDocument,
    start_frame: u64,
    end_frame: u64,
    ripple: bool,
) -> Result<(), AppError> {
    if end_frame <= start_frame {
        return Err(invalid(
            "removeRange endFrame must be greater than startFrame",
        ));
    }
    crate::ipc::validate_safe_integer(start_frame, "startFrame")?;
    crate::ipc::validate_safe_integer(end_frame, "endFrame")?;
    let interval = crate::project::model::FrameInterval::from_bounds(start_frame, end_frame)?;
    let affected_clip_ids: Vec<String> = document
        .clips
        .iter()
        .filter(|clip| {
            clip.interval()
                .ok()
                .and_then(|clip_interval| clip_interval.intersection(interval))
                .is_some()
                || (ripple && clip.start_frame >= end_frame)
        })
        .map(|clip| clip.id.clone())
        .collect();
    for clip_id in &affected_clip_ids {
        ensure_clip_mutable(document, clip_id)?;
    }
    for text in &document.text_items {
        let affected = text
            .timeline_interval()
            .and_then(Result::ok)
            .and_then(|text_interval| text_interval.intersection(interval))
            .is_some()
            || (ripple && text.start_frame.is_some_and(|start| start >= end_frame));
        if affected && text.owner_clip_id.is_none() {
            ensure_track_unlocked(document, &text.track_id)?;
        }
    }
    temporal::remove_range(document, start_frame, end_frame, ripple)
}

fn apply_add_text(document: &mut ProjectDocument, item: &TextItem) -> Result<(), AppError> {
    ensure_new_id(document, &item.id, "text.id")?;
    ensure_track_unlocked(document, &item.track_id)?;
    if let Some(owner_clip_id) = item.owner_clip_id.as_deref() {
        ensure_clip_mutable(document, owner_clip_id)?;
    }
    validate_text_against_document(document, item)?;
    document.text_items.push(item.clone());
    Ok(())
}

fn apply_update_text(
    document: &mut ProjectDocument,
    text_id: &str,
    patch: &TextPatch,
) -> Result<(), AppError> {
    let index = text_index(document, text_id)?;
    ensure_text_mutable(document, index)?;
    let mut text = document.text_items[index].clone();
    if let Some(value) = patch.text.clone() {
        text.text = value;
    }
    if let Some(value) = patch.style {
        text.style = value;
    }
    if let Some(value) = patch.color {
        text.color = value;
    }
    if let Some(value) = patch.font_size {
        text.font_size = value;
    }
    if let Some(value) = patch.position_x {
        text.position_x = value;
    }
    if let Some(value) = patch.position_y {
        text.position_y = value;
    }
    if let Some(value) = patch.line_breaks.clone() {
        text.line_breaks = value;
    }
    if let Some(value) = patch.start_frame {
        text.start_frame = Some(value);
    }
    if let Some(value) = patch.duration_frames {
        text.duration_frames = Some(value);
    }
    if let Some(value) = patch.source_start_frame {
        text.source_start_frame = Some(value);
    }
    if let Some(value) = patch.source_duration_frames {
        text.source_duration_frames = Some(value);
    }
    validate_text_against_document(document, &text)?;
    document.text_items[index] = text;
    Ok(())
}

fn apply_remove_text(document: &mut ProjectDocument, text_id: &str) -> Result<(), AppError> {
    let index = text_index(document, text_id)?;
    ensure_text_mutable(document, index)?;
    document.text_items.remove(index);
    Ok(())
}
fn apply_add_transition(
    document: &mut ProjectDocument,
    left_clip_id: &str,
    right_clip_id: &str,
    duration_frames: u64,
) -> Result<(), AppError> {
    ensure_transition_track_mutable(document, right_clip_id)?;
    temporal::add_transition(document, left_clip_id, right_clip_id, duration_frames)
}
fn apply_remove_transition(
    document: &mut ProjectDocument,
    transition_id: &str,
) -> Result<(), AppError> {
    let index = transition_index(document, transition_id)?;
    let transition = document.transitions[index].clone();
    ensure_transition_track_mutable(document, &transition.right_clip_id)?;
    temporal::remove_transition(document, transition_id)
}

fn transcript_for<'a>(
    transcripts: &'a [Transcript],
    transcript_id: &str,
) -> Result<&'a Transcript, AppError> {
    parse_uuid(transcript_id, "transcriptId")?;
    transcripts
        .iter()
        .find(|transcript| transcript.transcript_id == transcript_id)
        .ok_or_else(|| {
            AppError::new(
                ErrorCode::AssetUnavailable,
                "The requested transcript is unavailable",
            )
        })
}

fn apply_replace_captions(
    document: &mut ProjectDocument,
    clip_id: &str,
    transcript_id: &str,
    style: TextStyle,
    transcripts: &[Transcript],
) -> Result<(), AppError> {
    let clip_index = ensure_clip_mutable(document, clip_id)?;
    let clip = document.clips[clip_index].clone();
    let asset = clip_asset(document, &clip)?;
    let transcript = transcript_for(transcripts, transcript_id)?;
    transcript.validate()?;
    if transcript.asset_id != clip.asset_id {
        return Err(invalid("The transcript belongs to a different asset"));
    }
    if transcript.source_hash != asset.content_hash {
        return Err(invalid(
            "The transcript source hash does not match the asset",
        ));
    }
    let frame_count = asset
        .frame_count()
        .ok_or_else(|| invalid("The transcript asset has no normalized frame bounds"))?;
    let clip_source_end = safe_add(clip.in_frame, clip.duration_frames, "clip.sourceEndFrame")?;
    for segment in &transcript.segments {
        if segment.end_frame > frame_count {
            return Err(invalid(
                "Transcript source interval exceeds the normalized asset",
            ));
        }
    }
    let text_track_id = document
        .tracks
        .iter()
        .find(|track| track.kind == TrackKind::Text)
        .map(|track| track.id.clone())
        .ok_or_else(|| invalid("A text track is required for captions"))?;
    ensure_track_unlocked(document, &text_track_id)?;
    document
        .text_items
        .retain(|text| text.owner_clip_id.as_deref() != Some(clip_id));

    let mut additions = Vec::new();
    for segment in &transcript.segments {
        let start = segment.start_frame.max(clip.in_frame);
        let end = segment.end_frame.min(clip_source_end);
        if start >= end {
            continue;
        }
        let item = TextItem {
            id: Uuid::new_v4().to_string(),
            track_id: text_track_id.clone(),
            kind: TextKind::Caption,
            text: segment.text.clone(),
            style,
            color: RgbaColor {
                red: 255,
                green: 255,
                blue: 255,
                alpha: 255,
            },
            font_size: 48,
            position_x: 5_000,
            position_y: 8_500,
            line_breaks: Vec::new(),
            start_frame: None,
            duration_frames: None,
            owner_clip_id: Some(clip_id.to_owned()),
            source_start_frame: Some(start),
            source_duration_frames: Some(end - start),
        };
        validate_text_against_document(document, &item)?;
        additions.push(item);
    }
    document.text_items.extend(additions);
    Ok(())
}

fn apply_one(
    document: &mut ProjectDocument,
    operation: &EditOp,
    transcripts: &[Transcript],
) -> Result<(), AppError> {
    match operation {
        EditOp::SetProject { name, aspect } => apply_set_project(document, name, aspect),
        EditOp::AddTrack {
            id,
            kind,
            name,
            index,
        } => apply_add_track(document, id, *kind, name, *index),
        EditOp::UpdateTrack {
            track_id,
            name,
            muted,
            locked,
        } => apply_update_track(document, track_id, name, muted, locked),
        EditOp::RemoveTrack {
            track_id,
            delete_items,
        } => apply_remove_track(document, track_id, *delete_items),
        EditOp::InsertClip { clip } => apply_insert_clip(document, clip),
        EditOp::MoveClip {
            clip_id,
            track_id,
            start_frame,
        } => apply_move_clip(document, clip_id, track_id, *start_frame),
        EditOp::TrimClip {
            clip_id,
            in_frame,
            start_frame,
            duration_frames,
        } => apply_trim_clip(document, clip_id, *in_frame, *start_frame, *duration_frames),
        EditOp::SplitClip {
            clip_id,
            frame,
            right_clip_id,
        } => apply_split_clip(document, clip_id, *frame, right_clip_id),
        EditOp::UpdateClip { clip_id, patch } => apply_update_clip(document, clip_id, patch),
        EditOp::RemoveClips { clip_ids } => apply_remove_clips(document, clip_ids),
        EditOp::RemoveRange {
            start_frame,
            end_frame,
            ripple,
        } => apply_remove_range(document, *start_frame, *end_frame, *ripple),
        EditOp::AddText { item } => apply_add_text(document, item),
        EditOp::UpdateText { text_id, patch } => apply_update_text(document, text_id, patch),
        EditOp::RemoveText { text_id } => apply_remove_text(document, text_id),
        EditOp::AddTransition {
            left_clip_id,
            right_clip_id,
            duration_frames,
        } => apply_add_transition(document, left_clip_id, right_clip_id, *duration_frames),
        EditOp::RemoveTransition { transition_id } => {
            apply_remove_transition(document, transition_id)
        }
        EditOp::ReplaceCaptions {
            clip_id,
            transcript_id,
            style,
        } => apply_replace_captions(document, clip_id, transcript_id, *style, transcripts),
    }
}

/// Apply a complete operation batch to a candidate document. The candidate is
/// intentionally validated only after all operations: an explicit transition
/// removal followed by a move/trim is valid even though the first operation's
/// intermediate graph would not be.
pub fn apply_batch(
    document: &mut ProjectDocument,
    operations: &[EditOp],
    transcripts: &[Transcript],
) -> Result<(), AppError> {
    let mut candidate = document.clone();
    for operation in operations {
        apply_one(&mut candidate, operation, transcripts)?;
    }
    candidate.validate()?;
    *document = candidate;
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::project::model::{
        NormalizedAsset, NormalizedVideo, OriginalMediaMetadata, OriginalStreamKind,
        OriginalStreamMetadata,
    };
    use crate::project::store::{HistoryAction, ProjectStore};

    fn ready_video(id: &str, hash: &str, frame_count: u64) -> AssetManifest {
        AssetManifest {
            id: id.to_owned(),
            kind: AssetKind::Video,
            content_hash: hash.to_owned(),
            original: OriginalMediaMetadata {
                file_name: format!("{id}.mp4"),
                streams: vec![OriginalStreamMetadata {
                    kind: OriginalStreamKind::Video,
                    codec: "h264".to_owned(),
                    duration_ms: Some(frame_count * 1_000 / 30),
                    width: Some(1_920),
                    height: Some(1_080),
                    ..Default::default()
                }],
                ..Default::default()
            },
            normalization: Some(NormalizedAsset {
                renderer_version: "test".to_owned(),
                epoch_ms: 0,
                video: Some(NormalizedVideo {
                    master_artifact_id: format!("{id}-master"),
                    proxy_artifact_id: None,
                    frame_count,
                    width: 1_920,
                    height: 1_080,
                    fps_num: 30,
                    fps_den: 1,
                    active_start_frame: 0,
                    active_end_frame: frame_count,
                    source_start_ms: 0,
                    source_end_ms: frame_count as i64 * 1_000 / 30,
                    proxy_frame_count: Some(frame_count),
                }),
                audio: None,
            }),
        }
    }

    fn base_document() -> ProjectDocument {
        let mut document = ProjectDocument::new(
            "Operations test",
            AspectRatio::Landscape,
            crate::project::model::FrameRate::FPS_30,
        )
        .expect("document");
        document.assets.push(ready_video(
            "10000000-0000-4000-8000-000000000001",
            "red",
            240,
        ));
        document.assets.push(ready_video(
            "10000000-0000-4000-8000-000000000002",
            "blue",
            240,
        ));
        document.validate().expect("valid test document");
        document
    }

    fn video_track(document: &ProjectDocument) -> String {
        document
            .tracks
            .iter()
            .find(|track| track.kind == TrackKind::Video)
            .expect("video track")
            .id
            .clone()
    }

    fn clip(
        id: &str,
        asset_id: &str,
        track_id: &str,
        start_frame: u64,
        duration_frames: u64,
    ) -> MediaClip {
        MediaClip {
            id: id.to_owned(),
            track_id: track_id.to_owned(),
            asset_id: asset_id.to_owned(),
            start_frame,
            in_frame: 0,
            duration_frames,
            fit: FitMode::Contain,
            center_x: 5_000,
            center_y: 5_000,
            scale: 10_000,
            opacity: 10_000,
            gain_db: 0.0,
            audio_enabled: false,
            fade_in_frames: 0,
            fade_out_frames: 0,
        }
    }

    fn caption(
        id: &str,
        track_id: &str,
        owner_clip_id: &str,
        source_start_frame: u64,
        source_duration_frames: u64,
        text: &str,
    ) -> TextItem {
        TextItem {
            id: id.to_owned(),
            track_id: track_id.to_owned(),
            kind: TextKind::Caption,
            text: text.to_owned(),
            style: TextStyle::Clean,
            color: RgbaColor {
                red: 255,
                green: 255,
                blue: 255,
                alpha: 255,
            },
            font_size: 48,
            position_x: 5_000,
            position_y: 8_500,
            line_breaks: Vec::new(),
            start_frame: None,
            duration_frames: None,
            owner_clip_id: Some(owner_clip_id.to_owned()),
            source_start_frame: Some(source_start_frame),
            source_duration_frames: Some(source_duration_frames),
        }
    }

    #[test]
    fn split_and_ripple_remove_produce_expected_red_blue_timeline() {
        let mut document = base_document();
        let track_id = video_track(&document);
        let red_id = "20000000-0000-4000-8000-000000000001";
        let blue_id = "20000000-0000-4000-8000-000000000002";
        let right_id = "20000000-0000-4000-8000-000000000003";
        let operations = vec![
            EditOp::InsertClip {
                clip: clip(
                    red_id,
                    "10000000-0000-4000-8000-000000000001",
                    &track_id,
                    0,
                    120,
                ),
            },
            EditOp::InsertClip {
                clip: clip(
                    blue_id,
                    "10000000-0000-4000-8000-000000000002",
                    &track_id,
                    120,
                    120,
                ),
            },
            EditOp::SplitClip {
                clip_id: red_id.to_owned(),
                frame: 60,
                right_clip_id: right_id.to_owned(),
            },
            EditOp::RemoveRange {
                start_frame: 0,
                end_frame: 60,
                ripple: true,
            },
        ];
        apply_batch(&mut document, &operations, &[]).expect("split and ripple");
        assert_eq!(document.duration_frames().expect("duration"), 180);
        let red = document
            .clips
            .iter()
            .find(|clip| clip.id == right_id)
            .expect("remaining red clip");
        let blue = document
            .clips
            .iter()
            .find(|clip| clip.id == blue_id)
            .expect("blue clip");
        assert_eq!(
            (red.start_frame, red.in_frame, red.duration_frames),
            (0, 60, 60)
        );
        assert_eq!(
            (blue.start_frame, blue.in_frame, blue.duration_frames),
            (60, 0, 120)
        );
    }

    #[test]
    fn invalid_batch_is_atomic_and_locked_track_rejects_mutation() {
        let mut document = base_document();
        let before = document.clone();
        let track_id = video_track(&document);
        let operations = vec![
            EditOp::AddTrack {
                id: "30000000-0000-4000-8000-000000000001".to_owned(),
                kind: TrackKind::Video,
                name: "Temporary".to_owned(),
                index: 0,
            },
            EditOp::MoveClip {
                clip_id: "30000000-0000-4000-8000-000000000002".to_owned(),
                track_id,
                start_frame: 0,
            },
        ];
        assert!(apply_batch(&mut document, &operations, &[]).is_err());
        assert_eq!(document, before);

        let mut locked = base_document();
        let track_id = video_track(&locked);
        locked
            .tracks
            .iter_mut()
            .find(|track| track.id == track_id)
            .expect("track")
            .locked = true;
        let before = locked.clone();
        let insert = EditOp::InsertClip {
            clip: clip(
                "30000000-0000-4000-8000-000000000003",
                "10000000-0000-4000-8000-000000000001",
                &track_id,
                0,
                30,
            ),
        };
        assert!(apply_batch(&mut locked, &[insert], &[]).is_err());
        assert_eq!(locked, before);
    }

    #[test]
    fn empty_batch_is_a_no_change() {
        let mut document = base_document();
        let before = document.clone();
        apply_batch(&mut document, &[], &[]).expect("empty batch");
        assert_eq!(document, before);
    }

    #[test]
    fn owned_caption_cannot_become_a_standalone_timeline_item() {
        let mut document = base_document();
        let track_id = video_track(&document);
        let clip_id = "40000000-0000-4000-8000-000000000001";
        let insert = EditOp::InsertClip {
            clip: clip(
                clip_id,
                "10000000-0000-4000-8000-000000000001",
                &track_id,
                0,
                120,
            ),
        };
        apply_batch(&mut document, &[insert], &[]).expect("clip");
        let text_track_id = document
            .tracks
            .iter()
            .find(|track| track.kind == TrackKind::Text)
            .expect("text track")
            .id
            .clone();
        let caption_id = "40000000-0000-4000-8000-000000000002";
        let caption = TextItem {
            id: caption_id.to_owned(),
            track_id: text_track_id,
            kind: TextKind::Caption,
            text: "caption".to_owned(),
            style: TextStyle::Clean,
            color: RgbaColor {
                red: 255,
                green: 255,
                blue: 255,
                alpha: 255,
            },
            font_size: 48,
            position_x: 5_000,
            position_y: 8_500,
            line_breaks: Vec::new(),
            start_frame: None,
            duration_frames: None,
            owner_clip_id: Some(clip_id.to_owned()),
            source_start_frame: Some(0),
            source_duration_frames: Some(30),
        };
        apply_batch(&mut document, &[EditOp::AddText { item: caption }], &[])
            .expect("owned caption");
        let before = document.clone();
        let patch = TextPatch {
            start_frame: Some(0),
            ..Default::default()
        };
        assert!(apply_batch(
            &mut document,
            &[EditOp::UpdateText {
                text_id: caption_id.to_owned(),
                patch
            }],
            &[],
        )
        .is_err());
        assert_eq!(document, before);
    }
    #[test]
    fn equal_start_video_overlap_requires_an_explicit_dissolve() {
        let mut document = base_document();
        let track_id = video_track(&document);
        let first = EditOp::InsertClip {
            clip: clip(
                "50000000-0000-4000-8000-000000000001",
                "10000000-0000-4000-8000-000000000001",
                &track_id,
                0,
                60,
            ),
        };
        let second = EditOp::InsertClip {
            clip: clip(
                "50000000-0000-4000-8000-000000000002",
                "10000000-0000-4000-8000-000000000002",
                &track_id,
                0,
                60,
            ),
        };
        let before = document.clone();
        assert!(apply_batch(&mut document, &[first, second], &[]).is_err());
        assert_eq!(document, before);
    }

    #[test]
    fn trim_clips_owned_captions_to_the_retained_source_interval() {
        let mut document = base_document();
        let video_track_id = video_track(&document);
        let text_track_id = document
            .tracks
            .iter()
            .find(|track| track.kind == TrackKind::Text)
            .expect("text track")
            .id
            .clone();
        let clip_id = "50000000-0000-4000-8000-000000000003";
        apply_batch(
            &mut document,
            &[EditOp::InsertClip {
                clip: clip(
                    clip_id,
                    "10000000-0000-4000-8000-000000000001",
                    &video_track_id,
                    0,
                    120,
                ),
            }],
            &[],
        )
        .expect("clip");
        let retained_caption_id = "50000000-0000-4000-8000-000000000004";
        let removed_caption_id = "50000000-0000-4000-8000-000000000005";
        apply_batch(
            &mut document,
            &[
                EditOp::AddText {
                    item: caption(
                        retained_caption_id,
                        &text_track_id,
                        clip_id,
                        20,
                        40,
                        "keep this text",
                    ),
                },
                EditOp::AddText {
                    item: caption(
                        removed_caption_id,
                        &text_track_id,
                        clip_id,
                        100,
                        10,
                        "remove this text",
                    ),
                },
            ],
            &[],
        )
        .expect("captions");

        apply_batch(
            &mut document,
            &[EditOp::TrimClip {
                clip_id: clip_id.to_owned(),
                in_frame: 30,
                start_frame: 0,
                duration_frames: 60,
            }],
            &[],
        )
        .expect("trim");
        let retained = document
            .text_items
            .iter()
            .find(|text| text.id == retained_caption_id)
            .expect("retained caption");
        assert_eq!(retained.text, "keep this text");
        assert_eq!(retained.owner_clip_id.as_deref(), Some(clip_id));
        assert_eq!(
            (retained.source_start_frame, retained.source_duration_frames),
            (Some(30), Some(30))
        );
        assert!(document
            .text_items
            .iter()
            .all(|text| text.id != removed_caption_id));
    }

    #[test]
    fn trim_rejects_caption_projection_changes_on_a_locked_text_track() {
        let mut document = base_document();
        let video_track_id = video_track(&document);
        let text_track_id = document
            .tracks
            .iter()
            .find(|track| track.kind == TrackKind::Text)
            .expect("text track")
            .id
            .clone();
        let clip_id = "50000000-0000-4000-8000-000000000006";
        let caption_id = "50000000-0000-4000-8000-000000000007";
        apply_batch(
            &mut document,
            &[
                EditOp::InsertClip {
                    clip: clip(
                        clip_id,
                        "10000000-0000-4000-8000-000000000001",
                        &video_track_id,
                        0,
                        120,
                    ),
                },
                EditOp::AddText {
                    item: caption(
                        caption_id,
                        &text_track_id,
                        clip_id,
                        20,
                        40,
                        "locked caption",
                    ),
                },
            ],
            &[],
        )
        .expect("clip and caption");
        document
            .tracks
            .iter_mut()
            .find(|track| track.id == text_track_id)
            .expect("text track")
            .locked = true;
        let before = document.clone();
        assert!(apply_batch(
            &mut document,
            &[EditOp::TrimClip {
                clip_id: clip_id.to_owned(),
                in_frame: 30,
                start_frame: 0,
                duration_frames: 60,
            }],
            &[],
        )
        .is_err());
        assert_eq!(document, before);
    }
    #[test]
    fn ripple_owned_caption_survives_project_store_undo_redo() {
        let fixture_id = Uuid::new_v4().to_string();
        let root = std::env::temp_dir().join(format!(
            "cutterhoochee-caption-history-{fixture_id}.cutproj"
        ));
        let app_data =
            std::env::temp_dir().join(format!("cutterhoochee-caption-history-data-{fixture_id}"));
        let store = ProjectStore::create(&root, &app_data, "Caption history", Some("16:9"), 30, 1)
            .expect("project store");

        let preceding_asset_id = "60000000-0000-4000-8000-000000000001";
        let owner_asset_id = "60000000-0000-4000-8000-000000000002";
        store
            .commit(
                "seed-caption-assets".to_owned(),
                0,
                "Seed caption assets".to_owned(),
                "seed-caption-assets-payload".to_owned(),
                |document| {
                    document
                        .assets
                        .push(ready_video(preceding_asset_id, "red", 240));
                    document
                        .assets
                        .push(ready_video(owner_asset_id, "blue", 240));
                    Ok(())
                },
            )
            .expect("seed assets");

        let seeded = store.snapshot().expect("seed snapshot");
        let video_track_id = video_track(&seeded.document);
        let preceding_clip_id = "70000000-0000-4000-8000-000000000001";
        let owner_clip_id = "70000000-0000-4000-8000-000000000002";
        let transcript_id = "70000000-0000-4000-8000-000000000003";
        let mut owner_clip = clip(owner_clip_id, owner_asset_id, &video_track_id, 90, 120);
        owner_clip.in_frame = 40;
        let transcript = Transcript {
            transcript_id: transcript_id.to_owned(),
            asset_id: owner_asset_id.to_owned(),
            source_hash: "blue".to_owned(),
            segments: vec![TranscriptSpan {
                start_frame: 55,
                end_frame: 70,
                text: "字幕 — café Grüße".to_owned(),
                approximate: false,
            }],
        };
        let setup_operations = vec![
            EditOp::InsertClip {
                clip: clip(
                    preceding_clip_id,
                    preceding_asset_id,
                    &video_track_id,
                    0,
                    30,
                ),
            },
            EditOp::InsertClip { clip: owner_clip },
            EditOp::ReplaceCaptions {
                clip_id: owner_clip_id.to_owned(),
                transcript_id: transcript_id.to_owned(),
                style: TextStyle::Clean,
            },
        ];
        store
            .commit(
                "seed-owned-caption".to_owned(),
                1,
                "Seed owned caption".to_owned(),
                "seed-owned-caption-payload".to_owned(),
                move |document| {
                    apply_batch(
                        document,
                        &setup_operations,
                        std::slice::from_ref(&transcript),
                    )
                },
            )
            .expect("insert clip and replace captions");

        let before = store.snapshot().expect("before snapshot");
        let before_owner = before
            .document
            .clips
            .iter()
            .find(|clip| clip.id == owner_clip_id)
            .expect("owner clip before ripple");
        assert_eq!(
            (
                before_owner.start_frame,
                before_owner.in_frame,
                before_owner.duration_frames
            ),
            (90, 40, 120)
        );
        let before_caption = before
            .document
            .text_items
            .iter()
            .find(|text| text.owner_clip_id.as_deref() == Some(owner_clip_id))
            .expect("owned caption before ripple");
        assert_eq!(before_caption.text, "字幕 — café Grüße");
        assert_eq!(
            (
                before_caption.owner_clip_id.as_deref(),
                before_caption.source_start_frame,
                before_caption.source_duration_frames,
            ),
            (Some(owner_clip_id), Some(55), Some(15))
        );
        let before_projection = before
            .document
            .projected_captions()
            .expect("before caption projection");
        assert_eq!(before_projection.len(), 1);
        assert_eq!(
            (
                before_projection[0].timeline_start_frame,
                before_projection[0].duration_frames,
                before_projection[0].source_start_frame,
            ),
            (105, 15, 55)
        );

        let ripple_transaction_id = "ripple-owned-caption".to_owned();
        let ripple = EditOp::RemoveRange {
            start_frame: 0,
            end_frame: 30,
            ripple: true,
        };
        store
            .commit(
                ripple_transaction_id.clone(),
                before.document.revision,
                "Ripple before owned caption".to_owned(),
                "ripple-owned-caption-payload".to_owned(),
                move |document| apply_batch(document, std::slice::from_ref(&ripple), &[]),
            )
            .expect("ripple remove");

        let after = store.snapshot().expect("after snapshot");
        assert_eq!(after.document.clips.len(), 1);
        let after_owner = after
            .document
            .clips
            .iter()
            .find(|clip| clip.id == owner_clip_id)
            .expect("owner clip after ripple");
        assert_eq!(
            (
                after_owner.start_frame,
                after_owner.in_frame,
                after_owner.duration_frames
            ),
            (60, 40, 120)
        );
        let after_caption = after
            .document
            .text_items
            .iter()
            .find(|text| text.owner_clip_id.as_deref() == Some(owner_clip_id))
            .expect("owned caption after ripple");
        assert_eq!(after_caption.text, "字幕 — café Grüße");
        assert_eq!(
            (
                after_caption.owner_clip_id.as_deref(),
                after_caption.source_start_frame,
                after_caption.source_duration_frames,
            ),
            (Some(owner_clip_id), Some(55), Some(15))
        );
        let after_projection = after
            .document
            .projected_captions()
            .expect("after caption projection");
        assert_eq!(after_projection.len(), 1);
        assert_eq!(
            (
                after_projection[0].timeline_start_frame,
                after_projection[0].duration_frames,
                after_projection[0].source_start_frame,
            ),
            (75, 15, 55)
        );

        store
            .history(
                HistoryAction::Undo,
                after.document.revision,
                Some(ripple_transaction_id.clone()),
            )
            .expect("undo ripple");
        let undone = store.snapshot().expect("undo snapshot");
        let mut undone_content = undone.document.clone();
        undone_content.revision = before.document.revision;
        assert_eq!(undone_content, before.document);
        assert_eq!(
            undone
                .document
                .projected_captions()
                .expect("undo caption projection"),
            before_projection
        );

        store
            .history(
                HistoryAction::Redo,
                undone.document.revision,
                Some(ripple_transaction_id),
            )
            .expect("redo ripple");
        let redone = store.snapshot().expect("redo snapshot");
        let mut redone_content = redone.document.clone();
        redone_content.revision = after.document.revision;
        assert_eq!(redone_content, after.document);
        assert_eq!(
            redone
                .document
                .projected_captions()
                .expect("redo caption projection"),
            after_projection
        );
        drop(store);
        std::fs::remove_dir_all(root).expect("remove caption fixture");
        std::fs::remove_dir_all(app_data).expect("remove caption app-data fixture");
    }
}
