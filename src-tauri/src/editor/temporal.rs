use crate::error::AppError;
use crate::ipc::validate_safe_integer;
use crate::project::model::{
    AspectRatio, AssetKind, AssetManifest, FitMode, FrameRate, MediaClip, NormalizedAsset,
    NormalizedVideo, OriginalMediaMetadata, OriginalStreamKind, OriginalStreamMetadata,
    ProjectDocument, TextItem, TextKind, TextStyle, Track, TrackKind, Transition,
};
use std::collections::{HashMap, HashSet};
use uuid::Uuid;

fn invalid(message: impl Into<String>) -> AppError {
    AppError::invalid_argument(message)
}

fn checked_end(start: u64, duration: u64, field: &str) -> Result<u64, AppError> {
    let end = start
        .checked_add(duration)
        .ok_or_else(|| invalid(format!("{field} overflows")))?;
    validate_safe_integer(end, field)?;
    Ok(end)
}

fn checked_sub(value: u64, amount: u64, field: &str) -> Result<u64, AppError> {
    let result = value
        .checked_sub(amount)
        .ok_or_else(|| invalid(format!("{field} would become negative")))?;
    validate_safe_integer(result, field)?;
    Ok(result)
}

fn ensure_uuid(value: &str, field: &str) -> Result<(), AppError> {
    let parsed = Uuid::parse_str(value).map_err(|_| invalid(format!("{field} must be a UUID")))?;
    if parsed.is_nil() {
        return Err(invalid(format!("{field} must not be the nil UUID")));
    }
    Ok(())
}

fn entity_ids(document: &ProjectDocument) -> HashSet<String> {
    document
        .assets
        .iter()
        .map(|item| item.id.clone())
        .chain(document.tracks.iter().map(|item| item.id.clone()))
        .chain(document.clips.iter().map(|item| item.id.clone()))
        .chain(document.text_items.iter().map(|item| item.id.clone()))
        .chain(document.transitions.iter().map(|item| item.id.clone()))
        .collect()
}

fn fresh_id(used: &mut HashSet<String>) -> String {
    loop {
        let id = Uuid::new_v4().to_string();
        if used.insert(id.clone()) {
            return id;
        }
    }
}

fn clip_index(document: &ProjectDocument, clip_id: &str) -> Result<usize, AppError> {
    document
        .clips
        .iter()
        .position(|clip| clip.id == clip_id)
        .ok_or_else(|| invalid(format!("Unknown clip: {clip_id}")))
}

fn transition_index(document: &ProjectDocument, transition_id: &str) -> Result<usize, AppError> {
    document
        .transitions
        .iter()
        .position(|transition| transition.id == transition_id)
        .ok_or_else(|| invalid(format!("Unknown transition: {transition_id}")))
}

fn track_for_clip<'a>(
    document: &'a ProjectDocument,
    clip: &MediaClip,
) -> Result<&'a Track, AppError> {
    document
        .tracks
        .iter()
        .find(|track| track.id == clip.track_id)
        .ok_or_else(|| invalid(format!("Clip {} references an unknown track", clip.id)))
}

fn track_locked(document: &ProjectDocument, track_id: &str) -> Result<bool, AppError> {
    document
        .tracks
        .iter()
        .find(|track| track.id == track_id)
        .map(|track| track.locked)
        .ok_or_else(|| invalid(format!("Unknown track: {track_id}")))
}

/// Moving a video clip also moves every source-owned caption projected from
/// it. Keep direct temporal callers subject to the same lock boundary as the
/// operation wrapper, rather than treating the derived caption as immutable
/// merely because its stored source interval is unchanged.
fn ensure_shifted_caption_tracks_unlocked(
    document: &ProjectDocument,
    video_track_id: &str,
    shift_start_frame: u64,
) -> Result<(), AppError> {
    for text in &document.text_items {
        let Some(owner_clip_id) = text.owner_clip_id.as_deref() else {
            continue;
        };
        let owner = document
            .clips
            .iter()
            .find(|clip| clip.id == owner_clip_id)
            .ok_or_else(|| invalid("Caption references an unknown owner clip"))?;
        if owner.track_id == video_track_id && owner.start_frame >= shift_start_frame {
            if track_locked(document, &text.track_id)? {
                return Err(invalid(
                    "Cannot shift clips whose captions belong to a locked text track",
                ));
            }
        }
    }
    Ok(())
}

fn transition_overlap(
    transition: &Transition,
    clips: &HashMap<&str, &MediaClip>,
) -> Result<(u64, u64), AppError> {
    let left = clips
        .get(transition.left_clip_id.as_str())
        .ok_or_else(|| invalid("Transition references an unknown left clip"))?;
    let right = clips
        .get(transition.right_clip_id.as_str())
        .ok_or_else(|| invalid("Transition references an unknown right clip"))?;
    let left_end = left.end_frame()?;
    let overlap_start = left_end
        .checked_sub(transition.duration_frames)
        .ok_or_else(|| invalid("Transition duration exceeds the left clip"))?;
    let overlap_end = left_end;
    if right.start_frame != overlap_start {
        return Err(invalid(
            "Transition graph is inconsistent: clips do not meet at the dissolve boundary",
        ));
    }
    Ok((overlap_start, overlap_end))
}

/// Split one media clip at a timeline frame while preserving its source-frame
/// partition. The original ID remains on the left piece; `right_clip_id` is
/// caller-minted for the new right piece. Captions are split in source space,
/// and a dissolve that belongs to the right-hand split piece is retargeted.
pub fn split_clip(
    document: &mut ProjectDocument,
    clip_id: &str,
    frame: u64,
    right_clip_id: &str,
) -> Result<(), AppError> {
    ensure_uuid(right_clip_id, "rightClipId")?;
    let clip_position = clip_index(document, clip_id)?;
    let original = document.clips[clip_position].clone();
    if right_clip_id == original.id {
        return Err(invalid("The split pieces must have different IDs"));
    }
    let mut used = entity_ids(document);
    if used.contains(right_clip_id) {
        return Err(invalid(format!(
            "Entity ID is already in use: {right_clip_id}"
        )));
    }
    used.insert(right_clip_id.to_owned());
    let track = track_for_clip(document, &original)?;
    if track.locked {
        return Err(invalid("Cannot split a clip on a locked track"));
    }
    let still_image = document
        .assets
        .iter()
        .find(|asset| asset.id == original.asset_id)
        .map(|asset| asset.kind == AssetKind::StillImage)
        .ok_or_else(|| invalid("The clip asset does not exist"))?;

    let original_end = original.end_frame()?;
    if frame <= original.start_frame || frame >= original_end {
        return Err(invalid(format!(
            "Split frame must be strictly inside clip {} [{}, {})",
            original.id, original.start_frame, original_end
        )));
    }
    let left_duration = frame - original.start_frame;
    let right_duration = original_end - frame;
    let caption_split_source_start = original
        .in_frame
        .checked_add(left_duration)
        .ok_or_else(|| invalid("Split source frame overflows"))?;
    validate_safe_integer(caption_split_source_start, "caption.splitSourceFrame")?;
    let right_source_start = if still_image {
        0
    } else {
        caption_split_source_start
    };
    validate_safe_integer(right_source_start, "rightClip.inFrame")?;

    let clip_map: HashMap<_, _> = document
        .clips
        .iter()
        .map(|clip| (clip.id.as_str(), clip))
        .collect();
    let mut transition_left_updates = Vec::new();
    for (index, transition) in document.transitions.iter().enumerate() {
        let (overlap_start, overlap_end) = transition_overlap(transition, &clip_map)?;
        if transition.left_clip_id == original.id {
            if frame >= overlap_start && frame < overlap_end {
                return Err(invalid(format!(
                    "Cannot split clip {} inside or at the start of its dissolve overlap [{}, {}); remove the transition first",
                    original.id, overlap_start, overlap_end
                )));
            }
            // The part adjacent to an outgoing dissolve is the right split
            // piece only when the split is strictly before that dissolve.
            if frame < overlap_start {
                transition_left_updates.push(index);
            }
        }
        if transition.right_clip_id == original.id && frame > overlap_start && frame <= overlap_end
        {
            return Err(invalid(format!(
                "Cannot split clip {} inside or at the end of its dissolve overlap [{}, {}); remove the transition first",
                original.id, overlap_start, overlap_end
            )));
        }
    }

    let mut left = original.clone();
    left.duration_frames = left_duration;
    left.fade_in_frames = original.fade_in_frames.min(left_duration);
    // A split before the original tail cannot retain the old fade-out window
    // on the left piece without changing its source/timeline relationship.
    left.fade_out_frames = 0;

    let mut right = original.clone();
    right.id = right_clip_id.to_owned();
    right.start_frame = frame;
    right.in_frame = right_source_start;
    right.duration_frames = right_duration;
    right.fade_in_frames = 0;
    right.fade_out_frames = original.fade_out_frames.min(right_duration);

    let original_text_items = document.text_items.clone();
    let mut text_items = Vec::with_capacity(original_text_items.len());
    for text in original_text_items {
        if text.owner_clip_id.as_deref() != Some(original.id.as_str()) {
            text_items.push(text);
            continue;
        }
        let source_start = text
            .source_start_frame
            .ok_or_else(|| invalid("Owned caption source start is missing"))?;
        let source_duration = text
            .source_duration_frames
            .ok_or_else(|| invalid("Owned caption source duration is missing"))?;
        let source_end = checked_end(source_start, source_duration, "caption.sourceEndFrame")?;
        if source_end <= caption_split_source_start {
            text_items.push(text);
        } else if source_start >= caption_split_source_start {
            if track_locked(document, &text.track_id)? {
                return Err(invalid("Cannot split captions on a locked text track"));
            }
            let mut moved = text;
            moved.owner_clip_id = Some(right_clip_id.to_owned());
            if still_image {
                moved.source_start_frame = Some(checked_sub(
                    source_start,
                    caption_split_source_start,
                    "caption.sourceStartFrame",
                )?);
            }
            text_items.push(moved);
        } else {
            let left_caption_duration = caption_split_source_start - source_start;
            let right_caption_duration = source_end - caption_split_source_start;
            if track_locked(document, &text.track_id)? {
                return Err(invalid("Cannot split captions on a locked text track"));
            }
            let mut left_caption = text.clone();
            left_caption.source_duration_frames = Some(left_caption_duration);
            text_items.push(left_caption);
            let mut right_caption = text;
            right_caption.id = fresh_id(&mut used);
            right_caption.owner_clip_id = Some(right_clip_id.to_owned());
            right_caption.source_start_frame = Some(if still_image {
                0
            } else {
                caption_split_source_start
            });
            right_caption.source_duration_frames = Some(right_caption_duration);
            text_items.push(right_caption);
        }
    }
    document.clips[clip_position] = left;
    document.clips.insert(clip_position + 1, right);
    for transition_position in transition_left_updates {
        document.transitions[transition_position].left_clip_id = right_clip_id.to_owned();
    }
    document.text_items = text_items;
    Ok(())
}

#[derive(Debug, Clone)]
struct ClipSegment {
    old_clip_id: String,
    old_start_frame: u64,
    old_end_frame: u64,
    source_start_frame: u64,
    caption_source_start_frame: u64,
    caption_source_end_frame: u64,
    clip: MediaClip,
}

fn make_clip_segment(
    original: &MediaClip,
    id: String,
    old_start_frame: u64,
    output_start_frame: u64,
    source_start_frame: u64,
    caption_source_start_frame: u64,
    duration_frames: u64,
    keep_fade_in: bool,
    keep_fade_out: bool,
) -> Result<ClipSegment, AppError> {
    if duration_frames == 0 {
        return Err(invalid(
            "A retained clip segment must have a positive duration",
        ));
    }
    let old_end_frame = checked_end(old_start_frame, duration_frames, "segment.endFrame")?;
    let _source_end_frame = checked_end(
        source_start_frame,
        duration_frames,
        "segment.sourceEndFrame",
    )?;
    let caption_source_end_frame = checked_end(
        caption_source_start_frame,
        duration_frames,
        "segment.captionSourceEndFrame",
    )?;
    let mut clip = original.clone();
    clip.id = id;
    clip.start_frame = output_start_frame;
    clip.in_frame = source_start_frame;
    clip.duration_frames = duration_frames;
    clip.fade_in_frames = if keep_fade_in {
        original.fade_in_frames.min(duration_frames)
    } else {
        0
    };
    clip.fade_out_frames = if keep_fade_out {
        original.fade_out_frames.min(duration_frames)
    } else {
        0
    };
    Ok(ClipSegment {
        old_clip_id: original.id.clone(),
        old_start_frame,
        old_end_frame,
        source_start_frame,
        caption_source_start_frame,
        caption_source_end_frame,
        clip,
    })
}

fn build_clip_segments(
    original: &MediaClip,
    start_frame: u64,
    end_frame: u64,
    ripple: bool,
    still_image: bool,
    used: &mut HashSet<String>,
) -> Result<Vec<ClipSegment>, AppError> {
    let original_end = original.end_frame()?;
    let delta = end_frame - start_frame;
    let intersects = original.start_frame < end_frame && start_frame < original_end;
    if !intersects {
        let output_start = if ripple && original.start_frame >= end_frame {
            checked_sub(original.start_frame, delta, "clip.startFrame")?
        } else {
            original.start_frame
        };
        return Ok(vec![make_clip_segment(
            original,
            original.id.clone(),
            original.start_frame,
            output_start,
            original.in_frame,
            original.in_frame,
            original.duration_frames,
            true,
            true,
        )?]);
    }

    let mut segments = Vec::with_capacity(2);
    if original.start_frame < start_frame {
        let left_end = original_end.min(start_frame);
        let left_duration = left_end - original.start_frame;
        segments.push(make_clip_segment(
            original,
            original.id.clone(),
            original.start_frame,
            original.start_frame,
            original.in_frame,
            original.in_frame,
            left_duration,
            true,
            false,
        )?);
    }

    if original_end > end_frame {
        let right_original_start = end_frame.max(original.start_frame);
        let right_duration = original_end - right_original_start;
        let source_offset = right_original_start - original.start_frame;
        let caption_source_start = original
            .in_frame
            .checked_add(source_offset)
            .ok_or_else(|| invalid("Range removal source frame overflows"))?;
        validate_safe_integer(caption_source_start, "caption.sourceStartFrame")?;
        let right_source_start = if still_image { 0 } else { caption_source_start };
        validate_safe_integer(right_source_start, "clip.inFrame")?;
        let output_start = if ripple {
            checked_sub(right_original_start, delta, "clip.startFrame")?
        } else {
            right_original_start
        };
        let id = if segments.is_empty() {
            original.id.clone()
        } else {
            fresh_id(used)
        };
        segments.push(make_clip_segment(
            original,
            id,
            right_original_start,
            output_start,
            right_source_start,
            caption_source_start,
            right_duration,
            false,
            true,
        )?);
    }
    Ok(segments)
}

fn retained_timeline_pieces(
    old_start_frame: u64,
    old_end_frame: u64,
    start_frame: u64,
    end_frame: u64,
    ripple: bool,
) -> Result<Vec<(u64, u64, u64)>, AppError> {
    let delta = end_frame - start_frame;
    let intersects = old_start_frame < end_frame && start_frame < old_end_frame;
    if !intersects {
        let output_start = if ripple && old_start_frame >= end_frame {
            checked_sub(old_start_frame, delta, "text.startFrame")?
        } else {
            old_start_frame
        };
        return Ok(vec![(
            old_start_frame,
            output_start,
            old_end_frame - old_start_frame,
        )]);
    }
    let mut pieces = Vec::with_capacity(2);
    if old_start_frame < start_frame {
        pieces.push((
            old_start_frame,
            old_start_frame,
            start_frame - old_start_frame,
        ));
    }
    if old_end_frame > end_frame {
        let original_start = end_frame.max(old_start_frame);
        let output_start = if ripple {
            checked_sub(original_start, delta, "text.startFrame")?
        } else {
            original_start
        };
        pieces.push((original_start, output_start, old_end_frame - original_start));
    }
    Ok(pieces)
}

fn segment_for_frame<'a>(segments: &'a [ClipSegment], frame: u64) -> Option<&'a ClipSegment> {
    segments
        .iter()
        .find(|segment| frame >= segment.old_start_frame && frame < segment.old_end_frame)
}

fn shifted_text_items(
    text: &TextItem,
    start_frame: u64,
    end_frame: u64,
    ripple: bool,
    used: &mut HashSet<String>,
) -> Result<Vec<TextItem>, AppError> {
    let old_start = text
        .start_frame
        .ok_or_else(|| invalid("Standalone text start is missing"))?;
    let old_duration = text
        .duration_frames
        .ok_or_else(|| invalid("Standalone text duration is missing"))?;
    let old_end = checked_end(old_start, old_duration, "text.endFrame")?;
    let pieces = retained_timeline_pieces(old_start, old_end, start_frame, end_frame, ripple)?;
    let mut result = Vec::with_capacity(pieces.len());
    for (index, (_, output_start, duration)) in pieces.into_iter().enumerate() {
        let mut item = text.clone();
        if index > 0 {
            item.id = fresh_id(used);
        }
        item.start_frame = Some(output_start);
        item.duration_frames = Some(duration);
        result.push(item);
    }
    Ok(result)
}

fn map_kept_frame(frame: u64, start_frame: u64, end_frame: u64, ripple: bool) -> Option<u64> {
    if frame < start_frame {
        Some(frame)
    } else if frame >= end_frame {
        if ripple {
            frame.checked_sub(end_frame - start_frame)
        } else {
            Some(frame)
        }
    } else {
        None
    }
}

/// Remove a half-open timeline range on every track. Media clips and
/// standalone text are represented by retained pieces; owned captions are
/// projected against those source pieces. Ripple shifts all later timeline
/// material by exactly `end_frame - start_frame`.
pub fn remove_range(
    document: &mut ProjectDocument,
    start_frame: u64,
    end_frame: u64,
    ripple: bool,
) -> Result<(), AppError> {
    validate_safe_integer(start_frame, "startFrame")?;
    validate_safe_integer(end_frame, "endFrame")?;
    if end_frame <= start_frame {
        return Err(invalid("Range end must be greater than range start"));
    }
    let mut used = entity_ids(document);

    let mut segments_by_old: HashMap<String, Vec<ClipSegment>> = HashMap::new();
    let mut all_segments = Vec::new();
    for original in &document.clips {
        let still_image = document
            .assets
            .iter()
            .find(|asset| asset.id == original.asset_id)
            .map(|asset| asset.kind == AssetKind::StillImage)
            .ok_or_else(|| invalid("The clip asset does not exist"))?;
        let segments = build_clip_segments(
            original,
            start_frame,
            end_frame,
            ripple,
            still_image,
            &mut used,
        )?;
        let changed = segments.len() != 1 || segments[0].clip != *original;
        if changed && track_locked(document, &original.track_id)? {
            return Err(invalid(format!(
                "Cannot remove a range that changes locked track {}",
                original.track_id
            )));
        }
        segments_by_old.insert(original.id.clone(), segments.clone());
        all_segments.extend(segments);
    }

    let original_text_items = document.text_items.clone();
    let mut text_items = Vec::with_capacity(original_text_items.len());
    for text in original_text_items {
        if let Some(owner_clip_id) = text.owner_clip_id.as_deref() {
            let source_start = text
                .source_start_frame
                .ok_or_else(|| invalid("Owned caption source start is missing"))?;
            let source_duration = text
                .source_duration_frames
                .ok_or_else(|| invalid("Owned caption source duration is missing"))?;
            let source_end = checked_end(source_start, source_duration, "caption.sourceEndFrame")?;
            let segments = segments_by_old
                .get(owner_clip_id)
                .ok_or_else(|| invalid("Caption references an unknown owner clip"))?;
            let mut pieces = Vec::new();
            for segment in segments {
                let overlap_start = source_start.max(segment.caption_source_start_frame);
                let overlap_end = source_end.min(segment.caption_source_end_frame);
                if overlap_start >= overlap_end {
                    continue;
                }
                pieces.push((segment, overlap_start, overlap_end));
            }
            let locked = track_locked(document, &text.track_id)?;
            if locked
                && (pieces.len() != 1
                    || pieces[0].1 != source_start
                    || pieces[0].2 != source_end
                    || pieces[0].0.clip.id != owner_clip_id
                    || document
                        .clips
                        .iter()
                        .find(|clip| clip.id == owner_clip_id)
                        .is_some_and(|clip| pieces[0].0.clip.start_frame != clip.start_frame))
            {
                return Err(invalid(
                    "Cannot remove a range that changes captions on a locked text track",
                ));
            }
            for (index, (segment, piece_start, piece_end)) in pieces.into_iter().enumerate() {
                let local_start = checked_sub(
                    piece_start,
                    segment.caption_source_start_frame,
                    "caption.sourceStartFrame",
                )?;
                let local_end = checked_sub(
                    piece_end,
                    segment.caption_source_start_frame,
                    "caption.sourceEndFrame",
                )?;
                let mapped_start = segment
                    .source_start_frame
                    .checked_add(local_start)
                    .ok_or_else(|| invalid("Caption source start overflows"))?;
                let mapped_end = segment
                    .source_start_frame
                    .checked_add(local_end)
                    .ok_or_else(|| invalid("Caption source end overflows"))?;
                validate_safe_integer(mapped_start, "caption.sourceStartFrame")?;
                validate_safe_integer(mapped_end, "caption.sourceEndFrame")?;
                let mut item = text.clone();
                if index > 0 {
                    item.id = fresh_id(&mut used);
                }
                item.owner_clip_id = Some(segment.clip.id.clone());
                item.start_frame = None;
                item.duration_frames = None;
                item.source_start_frame = Some(mapped_start);
                item.source_duration_frames = Some(mapped_end - mapped_start);
                text_items.push(item);
            }
        } else {
            let transformed = shifted_text_items(&text, start_frame, end_frame, ripple, &mut used)?;
            if track_locked(document, &text.track_id)?
                && (transformed.len() != 1 || transformed[0] != text)
            {
                return Err(invalid(
                    "Cannot remove a range that changes text on a locked track",
                ));
            }
            text_items.extend(transformed);
        }
    }

    let original_clips: HashMap<_, _> = document
        .clips
        .iter()
        .map(|clip| (clip.id.as_str(), clip))
        .collect();
    let mut transitions = Vec::with_capacity(document.transitions.len());
    for transition in &document.transitions {
        let (overlap_start, overlap_end) = transition_overlap(transition, &original_clips)?;
        if start_frame < overlap_end && overlap_start < end_frame {
            return Err(invalid(format!(
                "Range [{start_frame}, {end_frame}) intersects dissolve overlap [{overlap_start}, {overlap_end}); remove transition {} first",
                transition.id
            )));
        }
        let left_segments = segments_by_old
            .get(transition.left_clip_id.as_str())
            .ok_or_else(|| {
                invalid("Transition endpoint was removed; remove the transition explicitly first")
            })?;
        let right_segments = segments_by_old
            .get(transition.right_clip_id.as_str())
            .ok_or_else(|| {
                invalid("Transition endpoint was removed; remove the transition explicitly first")
            })?;
        let left_sample = overlap_end
            .checked_sub(1)
            .ok_or_else(|| invalid("Transition overlap has no frames"))?;
        let right_sample = map_kept_frame(overlap_start, start_frame, end_frame, ripple)
            .ok_or_else(|| {
                invalid(
                    "Range removes a transition endpoint; remove the transition explicitly first",
                )
            })?;
        let mapped_left_sample = map_kept_frame(left_sample, start_frame, end_frame, ripple)
            .ok_or_else(|| {
                invalid(
                    "Range removes a transition endpoint; remove the transition explicitly first",
                )
            })?;
        let left_segment = segment_for_frame(left_segments, left_sample).ok_or_else(|| {
            invalid("Range changes a transition endpoint; remove the transition explicitly first")
        })?;
        let right_segment = segment_for_frame(right_segments, overlap_start).ok_or_else(|| {
            invalid("Range changes a transition endpoint; remove the transition explicitly first")
        })?;
        // Evaluate transformed coordinates against the retained output
        // intervals; a missing endpoint means this range invalidates the
        // dissolve and requires an explicit remove_transition first.
        if !left_segment.clip.interval()?.contains(mapped_left_sample)
            || !right_segment.clip.interval()?.contains(right_sample)
        {
            return Err(invalid(
                "Range changes a transition endpoint; remove the transition explicitly first",
            ));
        }
        let mut mapped = transition.clone();
        mapped.left_clip_id = left_segment.clip.id.clone();
        mapped.right_clip_id = right_segment.clip.id.clone();
        transitions.push(mapped);
    }
    document.clips = all_segments
        .into_iter()
        .map(|segment| segment.clip)
        .collect();
    document.text_items = text_items;
    document.transitions = transitions;
    Ok(())
}

/// Add an explicit dissolve between two touching video clips. The right clip
/// and all later clips on that video track move left by the overlap duration;
/// source coordinates and unrelated tracks remain unchanged.
pub fn add_transition(
    document: &mut ProjectDocument,
    left_clip_id: &str,
    right_clip_id: &str,
    duration_frames: u64,
) -> Result<(), AppError> {
    if left_clip_id == right_clip_id {
        return Err(invalid("A transition needs two different clips"));
    }
    let left = document
        .clips
        .iter()
        .find(|clip| clip.id == left_clip_id)
        .cloned()
        .ok_or_else(|| invalid(format!("Unknown clip: {left_clip_id}")))?;
    let right = document
        .clips
        .iter()
        .find(|clip| clip.id == right_clip_id)
        .cloned()
        .ok_or_else(|| invalid(format!("Unknown clip: {right_clip_id}")))?;
    let left_track = track_for_clip(document, &left)?;
    let right_track = track_for_clip(document, &right)?;
    if left_track.kind != TrackKind::Video || right_track.kind != TrackKind::Video {
        return Err(invalid("Dissolves are supported only on video tracks"));
    }
    if left.track_id != right.track_id {
        return Err(invalid("Transition clips must belong to the same track"));
    }
    if left_track.locked {
        return Err(invalid("Cannot add a transition on a locked track"));
    }
    let left_end = left.end_frame()?;
    if right.start_frame != left_end {
        return Err(invalid(
            "Dissolve clips must touch before adding a transition",
        ));
    }
    ensure_shifted_caption_tracks_unlocked(document, &left.track_id, right.start_frame)?;
    if duration_frames < 2 {
        return Err(invalid("Dissolve duration must be at least two frames"));
    }
    if duration_frames >= left.duration_frames.min(right.duration_frames) {
        return Err(invalid("Dissolve duration must be shorter than both clips"));
    }
    if document.transitions.iter().any(|transition| {
        transition.left_clip_id == left_clip_id && transition.right_clip_id == right_clip_id
    }) {
        return Err(invalid("The clip pair already has a dissolve"));
    }

    let mut used = entity_ids(document);
    for clip in document
        .clips
        .iter_mut()
        .filter(|clip| clip.track_id == left.track_id && clip.start_frame >= right.start_frame)
    {
        clip.start_frame = checked_sub(clip.start_frame, duration_frames, "clip.startFrame")?;
    }
    document.transitions.push(Transition {
        id: fresh_id(&mut used),
        left_clip_id: left_clip_id.to_owned(),
        right_clip_id: right_clip_id.to_owned(),
        duration_frames,
    });
    Ok(())
}

/// Remove a dissolve and reverse the exact track shift made by
/// [`add_transition`].
pub fn remove_transition(
    document: &mut ProjectDocument,
    transition_id: &str,
) -> Result<(), AppError> {
    let transition_position = transition_index(document, transition_id)?;
    let transition = document.transitions[transition_position].clone();
    let left = document
        .clips
        .iter()
        .find(|clip| clip.id == transition.left_clip_id)
        .cloned()
        .ok_or_else(|| invalid("Transition references an unknown left clip"))?;
    let right = document
        .clips
        .iter()
        .find(|clip| clip.id == transition.right_clip_id)
        .cloned()
        .ok_or_else(|| invalid("Transition references an unknown right clip"))?;
    let track = track_for_clip(document, &left)?;
    if track.kind != TrackKind::Video || right.track_id != left.track_id {
        return Err(invalid("Dissolves are supported only on one video track"));
    }
    if track.locked {
        return Err(invalid("Cannot remove a transition on a locked track"));
    }
    let left_end = left.end_frame()?;
    let expected_start = left_end
        .checked_sub(transition.duration_frames)
        .ok_or_else(|| invalid("Transition duration exceeds the left clip"))?;
    if right.start_frame != expected_start {
        return Err(invalid(
            "Transition graph is inconsistent at the dissolve boundary",
        ));
    }
    ensure_shifted_caption_tracks_unlocked(document, &left.track_id, right.start_frame)?;

    document.transitions.remove(transition_position);
    for clip in document
        .clips
        .iter_mut()
        .filter(|clip| clip.track_id == left.track_id && clip.start_frame >= right.start_frame)
    {
        clip.start_frame = clip
            .start_frame
            .checked_add(transition.duration_frames)
            .ok_or_else(|| invalid("Clip start overflows while removing transition"))?;
        validate_safe_integer(clip.start_frame, "clip.startFrame")?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn video_asset(id: String, frames: u64) -> AssetManifest {
        AssetManifest {
            id,
            kind: AssetKind::Video,
            content_hash: "hash".to_owned(),
            original: OriginalMediaMetadata {
                file_name: "fixture.mp4".to_owned(),
                streams: vec![OriginalStreamMetadata {
                    kind: OriginalStreamKind::Video,
                    codec: "h264".to_owned(),
                    ..Default::default()
                }],
                ..Default::default()
            },
            normalization: Some(NormalizedAsset {
                renderer_version: "test".to_owned(),
                epoch_ms: 0,
                video: Some(NormalizedVideo {
                    master_artifact_id: "master".to_owned(),
                    proxy_artifact_id: Some("proxy".to_owned()),
                    frame_count: frames,
                    width: 1_920,
                    height: 1_080,
                    fps_num: 30,
                    fps_den: 1,
                    active_start_frame: 0,
                    active_end_frame: frames,
                    source_start_ms: 0,
                    source_end_ms: 4_000,
                    proxy_frame_count: Some(frames),
                }),
                audio: None,
            }),
        }
    }

    fn clip(
        id: String,
        track_id: String,
        asset_id: String,
        start: u64,
        duration: u64,
    ) -> MediaClip {
        MediaClip {
            id,
            track_id,
            asset_id,
            start_frame: start,
            in_frame: 0,
            duration_frames: duration,
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

    fn text_caption(
        id: String,
        track_id: String,
        owner_clip_id: String,
        source_start: u64,
        source_duration: u64,
    ) -> TextItem {
        TextItem {
            id,
            track_id,
            kind: TextKind::Caption,
            text: "caption".to_owned(),
            style: TextStyle::Clean,
            color: Default::default(),
            font_size: 32,
            position_x: 5_000,
            position_y: 8_000,
            line_breaks: Vec::new(),
            start_frame: None,
            duration_frames: None,
            owner_clip_id: Some(owner_clip_id),
            source_start_frame: Some(source_start),
            source_duration_frames: Some(source_duration),
        }
    }

    fn fixture() -> (ProjectDocument, String, String, String) {
        let mut document =
            ProjectDocument::new("fixture", AspectRatio::Landscape, FrameRate::FPS_30).unwrap();
        let video_track = document
            .tracks
            .iter()
            .find(|track| track.kind == TrackKind::Video)
            .unwrap()
            .id
            .clone();
        let text_track = document
            .tracks
            .iter()
            .find(|track| track.kind == TrackKind::Text)
            .unwrap()
            .id
            .clone();
        let asset_id = Uuid::new_v4().to_string();
        let clip_id = Uuid::new_v4().to_string();
        document.assets.push(video_asset(asset_id.clone(), 240));
        document
            .clips
            .push(clip(clip_id.clone(), video_track.clone(), asset_id, 0, 120));
        document.text_items.push(text_caption(
            Uuid::new_v4().to_string(),
            text_track.clone(),
            clip_id.clone(),
            20,
            50,
        ));
        document.validate().unwrap();
        (document, clip_id, video_track, text_track)
    }

    #[test]
    fn split_conserves_source_frames_and_splits_captions() {
        let (mut document, clip_id, _, _) = fixture();
        let right_id = Uuid::new_v4().to_string();
        split_clip(&mut document, &clip_id, 40, &right_id).unwrap();
        let left = document
            .clips
            .iter()
            .find(|clip| clip.id == clip_id)
            .unwrap();
        let right = document
            .clips
            .iter()
            .find(|clip| clip.id == right_id)
            .unwrap();
        assert_eq!((left.in_frame, left.duration_frames), (0, 40));
        assert_eq!((right.in_frame, right.duration_frames), (40, 80));
        assert_eq!(left.end_frame().unwrap(), right.start_frame);
        let mut source_ranges: Vec<_> = document
            .text_items
            .iter()
            .map(|text| {
                (
                    text.owner_clip_id.clone().unwrap(),
                    text.source_start_frame.unwrap(),
                    text.source_duration_frames.unwrap(),
                )
            })
            .collect();
        source_ranges.sort_by_key(|(_, start, _)| *start);
        assert_eq!(source_ranges.len(), 2);
        assert_eq!((source_ranges[0].1, source_ranges[0].2), (20, 20));
        assert_eq!((source_ranges[1].1, source_ranges[1].2), (40, 30));
        assert_eq!(source_ranges[0].0, clip_id);
        assert_eq!(source_ranges[1].0, right_id);
    }

    #[test]
    fn range_is_half_open_and_locked_mutation_is_rejected() {
        let (mut document, clip_id, video_track, text_track) = fixture();
        let second_asset_id = Uuid::new_v4().to_string();
        let second_clip_id = Uuid::new_v4().to_string();
        document
            .assets
            .push(video_asset(second_asset_id.clone(), 240));
        document.clips.push(clip(
            second_clip_id,
            video_track.clone(),
            second_asset_id,
            120,
            60,
        ));
        document.validate().unwrap();
        remove_range(&mut document, 120, 150, false).unwrap();
        let first = document
            .clips
            .iter()
            .find(|clip| clip.id == clip_id)
            .unwrap();
        let second = document
            .clips
            .iter()
            .find(|clip| clip.start_frame == 150)
            .unwrap();
        assert_eq!(first.duration_frames, 120);
        assert_eq!((second.in_frame, second.duration_frames), (30, 30));

        let before = document.clone();
        document
            .tracks
            .iter_mut()
            .find(|track| track.id == text_track)
            .unwrap()
            .locked = true;
        assert!(remove_range(&mut document, 20, 21, false).is_err());
        document
            .tracks
            .iter_mut()
            .find(|track| track.id == text_track)
            .unwrap()
            .locked = false;
        assert_eq!(document.clips, before.clips);
    }

    #[test]
    fn dissolve_shifts_and_reverses_only_one_video_track_and_blocks_overlap_split() {
        let (mut document, first_id, video_track, _) = fixture();
        let asset_id = document.assets[0].id.clone();
        let second_id = Uuid::new_v4().to_string();
        let third_id = Uuid::new_v4().to_string();
        document.clips.push(clip(
            second_id.clone(),
            video_track.clone(),
            asset_id.clone(),
            120,
            120,
        ));
        document
            .clips
            .push(clip(third_id.clone(), video_track, asset_id, 240, 60));
        document.validate().unwrap();
        add_transition(&mut document, &first_id, &second_id, 10).unwrap();
        assert_eq!(
            document
                .clips
                .iter()
                .find(|clip| clip.id == second_id)
                .unwrap()
                .start_frame,
            110
        );
        assert_eq!(
            document
                .clips
                .iter()
                .find(|clip| clip.id == third_id)
                .unwrap()
                .start_frame,
            230
        );
        let transition_id = document.transitions[0].id.clone();
        assert!(split_clip(&mut document, &first_id, 115, &Uuid::new_v4().to_string()).is_err());
        remove_transition(&mut document, &transition_id).unwrap();
        assert_eq!(
            document
                .clips
                .iter()
                .find(|clip| clip.id == second_id)
                .unwrap()
                .start_frame,
            120
        );
        assert_eq!(
            document
                .clips
                .iter()
                .find(|clip| clip.id == third_id)
                .unwrap()
                .start_frame,
            240
        );
        assert!(document.transitions.is_empty());
    }
    #[test]
    fn still_image_split_restarts_source_at_zero_and_rebases_captions() {
        let mut document =
            ProjectDocument::new("still", AspectRatio::Landscape, FrameRate::FPS_30).unwrap();
        let video_track = document
            .tracks
            .iter()
            .find(|track| track.kind == TrackKind::Video)
            .unwrap()
            .id
            .clone();
        let text_track = document
            .tracks
            .iter()
            .find(|track| track.kind == TrackKind::Text)
            .unwrap()
            .id
            .clone();
        let asset_id = Uuid::new_v4().to_string();
        let clip_id = Uuid::new_v4().to_string();
        let mut asset = video_asset(asset_id.clone(), 240);
        asset.kind = AssetKind::StillImage;
        document.assets.push(asset);
        document
            .clips
            .push(clip(clip_id.clone(), video_track, asset_id, 0, 120));
        let caption_id = Uuid::new_v4().to_string();
        document.text_items.push(text_caption(
            caption_id.clone(),
            text_track,
            clip_id.clone(),
            80,
            20,
        ));
        document.validate().unwrap();

        let right_id = Uuid::new_v4().to_string();
        split_clip(&mut document, &clip_id, 60, &right_id).unwrap();
        let left = document
            .clips
            .iter()
            .find(|clip| clip.id == clip_id)
            .unwrap();
        let right = document
            .clips
            .iter()
            .find(|clip| clip.id == right_id)
            .unwrap();
        assert_eq!((left.in_frame, left.duration_frames), (0, 60));
        assert_eq!((right.in_frame, right.duration_frames), (0, 60));
        let caption = document
            .text_items
            .iter()
            .find(|text| text.id == caption_id)
            .unwrap();
        assert_eq!(caption.owner_clip_id.as_deref(), Some(right_id.as_str()));
        assert_eq!(
            (caption.source_start_frame, caption.source_duration_frames),
            (Some(20), Some(20))
        );
        document.validate().unwrap();
    }

    #[test]
    fn still_image_range_restarts_retained_source_and_rebases_captions() {
        let mut document =
            ProjectDocument::new("still range", AspectRatio::Landscape, FrameRate::FPS_30).unwrap();
        let video_track = document
            .tracks
            .iter()
            .find(|track| track.kind == TrackKind::Video)
            .unwrap()
            .id
            .clone();
        let text_track = document
            .tracks
            .iter()
            .find(|track| track.kind == TrackKind::Text)
            .unwrap()
            .id
            .clone();
        let asset_id = Uuid::new_v4().to_string();
        let clip_id = Uuid::new_v4().to_string();
        let mut asset = video_asset(asset_id.clone(), 240);
        asset.kind = AssetKind::StillImage;
        document.assets.push(asset);
        document
            .clips
            .push(clip(clip_id.clone(), video_track, asset_id, 0, 120));
        let caption_id = Uuid::new_v4().to_string();
        document.text_items.push(text_caption(
            caption_id.clone(),
            text_track,
            clip_id,
            80,
            20,
        ));
        document.validate().unwrap();

        remove_range(&mut document, 30, 60, false).unwrap();
        let left = document
            .clips
            .iter()
            .find(|clip| clip.start_frame == 0)
            .unwrap();
        let right = document
            .clips
            .iter()
            .find(|clip| clip.start_frame == 60)
            .unwrap();
        assert_eq!((left.in_frame, left.duration_frames), (0, 30));
        assert_eq!((right.in_frame, right.duration_frames), (0, 60));
        let caption = document
            .text_items
            .iter()
            .find(|text| text.id == caption_id)
            .unwrap();
        assert_eq!(caption.owner_clip_id.as_deref(), Some(right.id.as_str()));
        assert_eq!(
            (caption.source_start_frame, caption.source_duration_frames),
            (Some(20), Some(20))
        );
        document.validate().unwrap();
    }
}
