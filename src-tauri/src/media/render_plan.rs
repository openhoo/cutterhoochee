//! Immutable canonical render specifications.
//!
//! `ProjectDocument` is the only edit coordinate system.  This module resolves
//! every clip, source frame, crop rectangle, caption raster, dissolve interval,
//! and audio envelope once so preview and export cannot silently implement
//! different timing rules.

use crate::error::{AppError, ErrorCode};
use crate::media::graphics::{self, BundledFontCatalog};
use crate::project::model::{
    AssetKind, FitMode, FrameRate, MediaClip, ProjectDocument, RgbaColor, TextItem, TextKind,
    TextStyle, TrackKind, AUDIO_SAMPLE_RATE,
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use ts_rs::TS;

pub const RENDERER_VERSION: &str = "cutterhoochee-renderer-3-resvg-inter";
pub const RASTER_CACHE_VERSION: &str = "text-raster-3-resvg-inter";

/// The renderer uses this narrow adapter instead of knowing the media package's
/// concrete store.  NativeMedia can implement it for its ArtifactStore while
/// unit tests can use an in-memory map.  Large bytes never cross the UI event
/// stream: only the returned managed artifact id enters a RenderPlan.
pub trait ArtifactResolver: Send + Sync {
    fn managed_path(&self, artifact_id: &str) -> Result<std::path::PathBuf, AppError>;

    fn put_bytes(
        &self,
        cache_key: &str,
        extension: &str,
        content_type: &str,
        bytes: &[u8],
    ) -> Result<String, AppError>;

    fn bundled_font_catalog(&self) -> Result<Arc<BundledFontCatalog>, AppError>;
}

/// NativeMedia's concrete store is adapted here so all renderers share the
/// same symlink-free, workspace-bound artifact checks and font catalog.
impl ArtifactResolver for crate::media::artifacts::ArtifactStore {
    fn managed_path(&self, artifact_id: &str) -> Result<std::path::PathBuf, AppError> {
        crate::media::artifacts::ArtifactStore::managed_path(self, artifact_id)
    }

    fn put_bytes(
        &self,
        cache_key: &str,
        extension: &str,
        content_type: &str,
        bytes: &[u8],
    ) -> Result<String, AppError> {
        let kind = match (extension.to_ascii_lowercase().as_str(), content_type) {
            ("png", "image/png") | ("jpg", "image/jpeg") | ("jpeg", "image/jpeg") => {
                crate::media::artifacts::ArtifactKind::Frame
            }
            ("f32", _) | ("f32le", _) => crate::media::artifacts::ArtifactKind::PcmAudio,
            _ => crate::media::artifacts::ArtifactKind::Other,
        };
        Ok(crate::media::artifacts::ArtifactStore::put_bytes(
            self, cache_key, extension, kind, bytes,
        )?
        .artifact_id)
    }

    fn bundled_font_catalog(&self) -> Result<Arc<BundledFontCatalog>, AppError> {
        crate::media::artifacts::ArtifactStore::bundled_font_catalog(self)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
#[ts(rename_all = "snake_case")]
pub enum RenderLayerKind {
    Video,
    Text,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
#[ts(rename_all = "snake_case")]
pub enum AudioTransitionSide {
    Incoming,
    Outgoing,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase")]
pub struct SourceRect {
    #[ts(type = "SafeInteger")]
    pub x: u32,
    #[ts(type = "SafeInteger")]
    pub y: u32,
    #[ts(type = "SafeInteger")]
    pub width: u32,
    #[ts(type = "SafeInteger")]
    pub height: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase")]
pub struct DestRect {
    #[ts(type = "number")]
    pub x: i32,
    #[ts(type = "number")]
    pub y: i32,
    #[ts(type = "SafeInteger")]
    pub width: u32,
    #[ts(type = "SafeInteger")]
    pub height: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase")]
pub struct RenderSegment {
    pub clip_id: String,
    pub asset_id: String,
    pub artifact_id: String,
    #[ts(type = "number")]
    pub epoch_ms: i64,
    #[ts(type = "SafeInteger")]
    pub active_start_frame: u64,
    #[ts(type = "SafeInteger")]
    pub active_end_frame: u64,
    pub is_still_image: bool,
    #[ts(type = "SafeInteger")]
    pub start_frame: u64,
    #[ts(type = "SafeInteger")]
    pub end_frame: u64,
    #[ts(type = "SafeInteger")]
    pub source_start_frame: u64,
    #[ts(type = "SafeInteger")]
    pub source_end_frame: u64,
    pub source_width: u32,
    pub source_height: u32,
    pub source_rect: SourceRect,
    pub dest_rect: DestRect,
    pub opacity: u16,
    pub fit: FitMode,
    pub center_x: u16,
    pub center_y: u16,
    pub scale: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase")]
pub struct RenderTransition {
    pub id: String,
    pub track_id: String,
    pub left_clip_id: String,
    pub right_clip_id: String,
    #[ts(type = "SafeInteger")]
    pub start_frame: u64,
    #[ts(type = "SafeInteger")]
    pub end_frame: u64,
    #[ts(type = "SafeInteger")]
    pub duration_frames: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase")]
pub struct RenderLayer {
    pub track_id: String,
    pub kind: RenderLayerKind,
    #[ts(type = "SafeInteger")]
    pub order: u32,
    pub segments: Vec<RenderSegment>,
    pub transitions: Vec<RenderTransition>,
    pub text_overlays: Vec<RenderTextOverlay>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase")]
pub struct RenderTextOverlay {
    pub id: String,
    pub track_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub owner_clip_id: Option<String>,
    #[ts(type = "SafeInteger")]
    pub start_frame: u64,
    #[ts(type = "SafeInteger")]
    pub end_frame: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional, type = "SafeInteger")]
    pub source_start_frame: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional, type = "SafeInteger")]
    pub source_duration_frames: Option<u64>,
    pub kind: TextKind,
    pub text: String,
    pub style: TextStyle,
    pub color: RgbaColor,
    pub font_size: u32,
    pub position_x: u16,
    pub position_y: u16,
    #[ts(type = "SafeInteger[]")]
    pub line_breaks: Vec<u32>,
    pub lines: Vec<String>,
    /// Required: a plan is not ready until the exact transparent PNG raster is
    /// managed.  The Canvas and export paths consume this same artifact.
    pub raster_artifact_id: String,
    pub raster_source_rect: SourceRect,
    pub raster_dest_rect: DestRect,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub font_warning: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase")]
pub struct AudioTransition {
    pub id: String,
    pub side: AudioTransitionSide,
    #[ts(type = "SafeInteger")]
    pub start_sample: u64,
    #[ts(type = "SafeInteger")]
    pub end_sample: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase")]
pub struct AudioSegment {
    pub clip_id: String,
    pub track_id: String,
    pub asset_id: String,
    pub artifact_id: String,
    #[ts(type = "number")]
    pub epoch_ms: i64,
    #[ts(type = "SafeInteger")]
    pub active_start_sample: u64,
    #[ts(type = "SafeInteger")]
    pub active_end_sample: u64,
    #[ts(type = "SafeInteger")]
    pub start_sample: u64,
    #[ts(type = "SafeInteger")]
    pub end_sample: u64,
    #[ts(type = "SafeInteger")]
    pub source_start_sample: u64,
    #[ts(type = "SafeInteger")]
    pub source_end_sample: u64,
    pub gain_db: f64,
    #[ts(type = "SafeInteger")]
    pub fade_in_samples: u64,
    #[ts(type = "SafeInteger")]
    pub fade_out_samples: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub transition_in: Option<AudioTransition>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub transition_out: Option<AudioTransition>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase")]
pub struct AudioPlan {
    pub sample_rate: u32,
    pub channels: u8,
    #[ts(type = "SafeInteger")]
    pub total_samples: u64,
    pub segments: Vec<AudioSegment>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase")]
pub struct RenderPlan {
    pub renderer_version: String,
    pub project_id: String,
    #[ts(type = "SafeInteger")]
    pub revision: u64,
    pub plan_hash: String,
    pub width: u32,
    pub height: u32,
    pub fps_num: u32,
    pub fps_den: u32,
    #[ts(type = "SafeInteger")]
    pub duration_frames: u64,
    pub background: RgbaColor,
    pub font_family: String,
    /// Content identity of the pinned Inter resources and rasterizer release.
    /// It participates in the plan hash so preview/export cannot share pixels
    /// produced by a different font resource.
    pub font_identity: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub font_warning: Option<String>,
    pub layers: Vec<RenderLayer>,
    pub audio: AudioPlan,
}

impl RenderPlan {
    pub fn fps(&self) -> FrameRate {
        FrameRate {
            num: self.fps_num,
            den: self.fps_den,
        }
    }

    pub fn sample_at_frame(&self, frame: u64) -> Result<u64, AppError> {
        sample_at_frame(frame, self.fps())
    }

    pub fn validate(&self) -> Result<(), AppError> {
        self.fps().validate()?;
        if self.font_family.trim().is_empty() || self.font_identity.trim().is_empty() {
            return Err(AppError::new(
                ErrorCode::MediaUnsupported,
                "A render plan needs a pinned bundled font identity",
            ));
        }
        if self.width == 0 || self.height == 0 {
            return Err(AppError::invalid_argument(
                "A render plan needs positive dimensions",
            ));
        }
        if self.duration_frames > 0 {
            self.sample_at_frame(self.duration_frames)?;
        }
        if self.audio.sample_rate != AUDIO_SAMPLE_RATE || self.audio.channels != 2 {
            return Err(AppError::invalid_argument(
                "A render plan needs 48 kHz stereo audio",
            ));
        }
        Ok(())
    }
}

/// Exact integer project-frame to 48 kHz sample conversion.  All supported
/// project rates divide 48 kHz and therefore this never rounds.
pub fn sample_at_frame(frame: u64, fps: FrameRate) -> Result<u64, AppError> {
    fps.validate()?;
    let value = (frame as u128)
        .checked_mul(AUDIO_SAMPLE_RATE as u128)
        .and_then(|value| value.checked_mul(fps.den as u128))
        .and_then(|value| value.checked_div(fps.num as u128))
        .ok_or_else(|| AppError::invalid_argument("Frame/sample conversion overflowed"))?;
    u64::try_from(value)
        .map_err(|_| AppError::invalid_argument("Frame/sample conversion is too large"))
}

/// Compile an immutable plan and persist every text raster through the managed
/// artifact adapter.  A missing or failed artifact is an error, never a plan
/// with a fake/late fallback.
pub fn compile_render_plan(
    document: &ProjectDocument,
    artifacts: &dyn ArtifactResolver,
) -> Result<RenderPlan, AppError> {
    document.validate()?;
    let profile = &document.profile;
    let fps = profile.fps();
    let duration_frames = document.duration_frames()?;
    let tracks: HashMap<&str, (&crate::project::model::Track, usize)> = document
        .tracks
        .iter()
        .enumerate()
        .map(|(index, track)| (track.id.as_str(), (track, index)))
        .collect();
    let assets: HashMap<&str, &crate::project::model::AssetManifest> = document
        .assets
        .iter()
        .map(|asset| (asset.id.as_str(), asset))
        .collect();
    let clips: HashMap<&str, &MediaClip> = document
        .clips
        .iter()
        .map(|clip| (clip.id.as_str(), clip))
        .collect();
    let font_catalog = artifacts.bundled_font_catalog()?;
    let font_identity = font_catalog.identity.clone();

    let mut layers = Vec::new();
    for (order, track) in document.tracks.iter().enumerate() {
        let (kind, segments) = match track.kind {
            TrackKind::Video => {
                let mut segments = Vec::new();
                for clip in document
                    .clips
                    .iter()
                    .filter(|clip| clip.track_id == track.id)
                {
                    let asset = assets.get(clip.asset_id.as_str()).copied().ok_or_else(|| {
                        AppError::invalid_argument("Render plan references an unknown asset")
                    })?;
                    let normalization = asset.normalization.as_ref().ok_or_else(|| {
                        AppError::new(
                            ErrorCode::AssetUnavailable,
                            "A clip asset is not ready for rendering",
                        )
                    })?;
                    let video = normalization.video.as_ref().ok_or_else(|| {
                        AppError::new(
                            ErrorCode::AssetUnavailable,
                            "A video clip has no normalized master",
                        )
                    })?;
                    if video.fps_num != fps.num || video.fps_den != fps.den {
                        return Err(AppError::new(
                            ErrorCode::MediaUnsupported,
                            "The normalized master frame rate differs from the project rate",
                        ));
                    }
                    let end_frame = clip.end_frame()?;
                    let (source_rect, dest_rect) = geometry(
                        video.width,
                        video.height,
                        profile.width,
                        profile.height,
                        clip.fit,
                        clip.center_x,
                        clip.center_y,
                        clip.scale,
                    )?;
                    let source_end_frame = clip
                        .in_frame
                        .checked_add(clip.duration_frames)
                        .ok_or_else(|| {
                            AppError::invalid_argument("Clip source frame range overflowed")
                        })?;
                    if source_end_frame > video.frame_count {
                        return Err(AppError::new(
                            ErrorCode::AssetUnavailable,
                            "A clip source range exceeds the normalized master",
                        ));
                    }
                    segments.push(RenderSegment {
                        clip_id: clip.id.clone(),
                        asset_id: clip.asset_id.clone(),
                        artifact_id: video.master_artifact_id.clone(),
                        epoch_ms: normalization.epoch_ms,
                        active_start_frame: video.active_start_frame,
                        active_end_frame: video.active_end_frame,
                        is_still_image: asset.kind == AssetKind::StillImage,
                        start_frame: clip.start_frame,
                        end_frame,
                        source_start_frame: clip.in_frame,
                        source_end_frame,
                        source_width: video.width,
                        source_height: video.height,
                        source_rect,
                        dest_rect,
                        opacity: clip.opacity,
                        fit: clip.fit,
                        center_x: clip.center_x,
                        center_y: clip.center_y,
                        scale: clip.scale,
                    });
                }
                (RenderLayerKind::Video, segments)
            }
            TrackKind::Text => (RenderLayerKind::Text, Vec::new()),
            TrackKind::Audio => continue,
        };
        let transitions = document
            .transitions
            .iter()
            .filter_map(|transition| {
                let left = clips.get(transition.left_clip_id.as_str())?;
                let right = clips.get(transition.right_clip_id.as_str())?;
                if left.track_id != track.id || right.track_id != track.id {
                    return None;
                }
                let end = left.end_frame().ok()?;
                Some(RenderTransition {
                    id: transition.id.clone(),
                    track_id: track.id.clone(),
                    left_clip_id: transition.left_clip_id.clone(),
                    right_clip_id: transition.right_clip_id.clone(),
                    start_frame: right.start_frame,
                    end_frame: end,
                    duration_frames: transition.duration_frames,
                })
            })
            .collect();
        let text_overlays = if track.kind == TrackKind::Text {
            compile_text_overlays(
                document,
                track.id.as_str(),
                artifacts,
                profile.width,
                profile.height,
                &font_catalog,
            )?
        } else {
            Vec::new()
        };
        layers.push(RenderLayer {
            track_id: track.id.clone(),
            kind,
            order: order as u32,
            segments,
            transitions,
            text_overlays,
        });
    }
    let mut audio_segments = Vec::new();
    // Preserve the document's explicit track order when summing.  Keeping
    // this order (rather than sorting IDs) gives both renderers identical
    // floating-point accumulation for overlapping music and clip audio.
    for track in &document.tracks {
        let include_audio = matches!(track.kind, TrackKind::Video | TrackKind::Audio);
        if track.muted || !include_audio {
            continue;
        }
        for clip in document
            .clips
            .iter()
            .filter(|clip| clip.track_id == track.id && clip.audio_enabled)
        {
            append_audio_segment(&mut audio_segments, clip, track.id.as_str(), &assets, fps)?;
        }
    }
    let total_samples = sample_at_frame(duration_frames, fps)?;
    attach_audio_transitions(&mut audio_segments, document, &clips, fps)?;
    let font_warning = layers
        .iter()
        .flat_map(|layer| layer.text_overlays.iter())
        .find_map(|overlay| overlay.font_warning.clone());
    let mut plan = RenderPlan {
        renderer_version: RENDERER_VERSION.to_owned(),
        project_id: document.project_id.clone(),
        revision: document.revision,
        plan_hash: String::new(),
        width: profile.width,
        height: profile.height,
        fps_num: profile.fps_num,
        fps_den: profile.fps_den,
        duration_frames,
        background: profile.background,
        font_family: graphics::INTER_FONT_FAMILY.to_owned(),
        font_identity,
        font_warning,
        layers,
        audio: AudioPlan {
            sample_rate: AUDIO_SAMPLE_RATE,
            channels: 2,
            total_samples,
            segments: audio_segments,
        },
    };
    plan.plan_hash = plan_hash(&plan)?;
    plan.validate()?;
    Ok(plan)
}

fn append_audio_segment(
    output: &mut Vec<AudioSegment>,
    clip: &MediaClip,
    track_id: &str,
    assets: &HashMap<&str, &crate::project::model::AssetManifest>,
    fps: FrameRate,
) -> Result<(), AppError> {
    let asset = assets
        .get(clip.asset_id.as_str())
        .copied()
        .ok_or_else(|| AppError::invalid_argument("Audio clip references an unknown asset"))?;
    let normalization = asset.normalization.as_ref().ok_or_else(|| {
        AppError::new(
            ErrorCode::AssetUnavailable,
            "An audio clip asset is not ready for rendering",
        )
    })?;
    let audio = normalization.audio.as_ref().ok_or_else(|| {
        AppError::new(
            ErrorCode::AssetUnavailable,
            "An audio clip has no normalized PCM",
        )
    })?;
    let start_sample = sample_at_frame(clip.start_frame, fps)?;
    let end_sample = sample_at_frame(clip.end_frame()?, fps)?;
    let source_start_sample = sample_at_frame(clip.in_frame, fps)?;
    let source_end_sample = sample_at_frame(
        clip.in_frame
            .checked_add(clip.duration_frames)
            .ok_or_else(|| AppError::invalid_argument("Audio source frame range overflowed"))?,
        fps,
    )?;
    if source_end_sample > audio.sample_count {
        return Err(AppError::new(
            ErrorCode::AssetUnavailable,
            "An audio clip exceeds its normalized PCM bounds",
        ));
    }
    let fade_in_samples = sample_at_frame(clip.fade_in_frames, fps)?;
    let fade_out_samples = sample_at_frame(clip.fade_out_frames, fps)?;
    output.push(AudioSegment {
        clip_id: clip.id.clone(),
        track_id: track_id.to_owned(),
        asset_id: clip.asset_id.clone(),
        artifact_id: audio.pcm_artifact_id.clone(),
        epoch_ms: normalization.epoch_ms,
        active_start_sample: audio.active_start_sample,
        active_end_sample: audio.active_end_sample,
        start_sample,
        end_sample,
        source_start_sample,
        source_end_sample,
        gain_db: clip.gain_db,
        fade_in_samples,
        fade_out_samples,
        transition_in: None,
        transition_out: None,
    });
    Ok(())
}

fn attach_audio_transitions(
    segments: &mut [AudioSegment],
    document: &ProjectDocument,
    clips: &HashMap<&str, &MediaClip>,
    fps: FrameRate,
) -> Result<(), AppError> {
    for transition in &document.transitions {
        let left = clips
            .get(transition.left_clip_id.as_str())
            .copied()
            .ok_or_else(|| AppError::invalid_argument("A transition references an unknown clip"))?;
        let right = clips
            .get(transition.right_clip_id.as_str())
            .copied()
            .ok_or_else(|| AppError::invalid_argument("A transition references an unknown clip"))?;
        let start_sample = sample_at_frame(right.start_frame, fps)?;
        let end_sample = sample_at_frame(left.end_frame()?, fps)?;
        for segment in segments.iter_mut() {
            if segment.clip_id == left.id {
                segment.transition_out = Some(AudioTransition {
                    id: transition.id.clone(),
                    side: AudioTransitionSide::Outgoing,
                    start_sample,
                    end_sample,
                });
            } else if segment.clip_id == right.id {
                segment.transition_in = Some(AudioTransition {
                    id: transition.id.clone(),
                    side: AudioTransitionSide::Incoming,
                    start_sample,
                    end_sample,
                });
            }
        }
    }
    Ok(())
}

fn compile_text_overlays(
    document: &ProjectDocument,
    track_id: &str,
    artifacts: &dyn ArtifactResolver,
    canvas_width: u32,
    canvas_height: u32,
    font_catalog: &BundledFontCatalog,
) -> Result<Vec<RenderTextOverlay>, AppError> {
    let clips: HashMap<&str, &MediaClip> = document
        .clips
        .iter()
        .map(|clip| (clip.id.as_str(), clip))
        .collect();
    let mut overlays = Vec::new();
    for text in document
        .text_items
        .iter()
        .filter(|text| text.track_id == track_id)
    {
        let (start_frame, end_frame, source_start_frame, source_duration_frames) =
            if let Some(owner) = text.owner_clip_id.as_deref() {
                let clip = clips.get(owner).copied().ok_or_else(|| {
                    AppError::invalid_argument("Caption references an unknown owner clip")
                })?;
                let projection = text.project_on_clip(clip)?.ok_or_else(|| {
                    AppError::invalid_argument("Caption source interval is outside its clip")
                })?;
                (
                    projection.timeline_start_frame,
                    projection
                        .timeline_start_frame
                        .checked_add(projection.duration_frames)
                        .ok_or_else(|| {
                            AppError::invalid_argument("Caption timeline interval overflowed")
                        })?,
                    Some(projection.source_start_frame),
                    Some(projection.duration_frames),
                )
            } else {
                let interval = text.timeline_interval().ok_or_else(|| {
                    AppError::invalid_argument("Standalone text has no timeline interval")
                })??;
                (interval.start_frame, interval.end_frame(), None, None)
            };
        if end_frame <= start_frame {
            continue;
        }
        let safe_width = u64::from(canvas_width).saturating_mul(9) / 10;
        let font_size = u64::from(text.font_size.max(1));
        let max_chars = safe_width
            .saturating_mul(10)
            .checked_div(font_size.saturating_mul(6).max(1))
            .unwrap_or(8)
            .clamp(8, 96) as usize;
        let lines = wrap_text(&text.text, &text.line_breaks, max_chars);
        let (raster, raster_width, raster_height) =
            rasterize_text(&lines, text, canvas_width, canvas_height, font_catalog)?;
        let cache_key = format!(
            "{RASTER_CACHE_VERSION}:{}:{}:{}:{}:{}:{}:{}",
            font_catalog.identity,
            text.id,
            text.font_size,
            text.position_x,
            text.position_y,
            serde_json::to_string(&text.style).unwrap_or_default(),
            stable_bytes_hash(&raster),
        );
        let raster_artifact_id = artifacts.put_bytes(&cache_key, "png", "image/png", &raster)?;
        let dest_width = raster_width.min((canvas_width * 9) / 10).max(1);
        let dest_height = raster_height.min((canvas_height * 9) / 10).max(1);
        let center_x = (u64::from(text.position_x) * u64::from(canvas_width) / 10_000) as i64;
        let center_y = (u64::from(text.position_y) * u64::from(canvas_height) / 10_000) as i64;
        let dest_rect = DestRect {
            x: (center_x - i64::from(dest_width) / 2)
                .clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32,
            y: (center_y - i64::from(dest_height) / 2)
                .clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32,
            width: dest_width,
            height: dest_height,
        };
        let missing = font_catalog.missing_glyphs(&text.text);
        overlays.push(RenderTextOverlay {
            id: text.id.clone(),
            track_id: track_id.to_owned(),
            owner_clip_id: text.owner_clip_id.clone(),
            start_frame,
            end_frame,
            source_start_frame,
            source_duration_frames,
            kind: text.kind,
            text: text.text.clone(),
            style: text.style,
            color: text.color,
            font_size: text.font_size,
            position_x: text.position_x,
            position_y: text.position_y,
            line_breaks: text.line_breaks.clone(),
            lines,
            raster_artifact_id,
            raster_source_rect: SourceRect {
                x: 0,
                y: 0,
                width: raster_width,
                height: raster_height,
            },
            raster_dest_rect: dest_rect,
            font_warning: missing_glyph_warning(&missing),
        });
    }
    overlays.sort_by_key(|overlay| (overlay.start_frame, overlay.id.clone()));
    Ok(overlays)
}

fn missing_glyph_warning(missing: &[char]) -> Option<String> {
    if missing.is_empty() {
        return None;
    }
    let listed = missing
        .iter()
        .take(6)
        .map(|character| format!("U+{:04X}", *character as u32))
        .collect::<Vec<_>>()
        .join(", ");
    let suffix = if missing.len() > 6 { ", …" } else { "" };
    Some(format!(
        "Inter is missing glyphs ({listed}{suffix}); text may be incomplete"
    ))
}

fn geometry(
    source_width: u32,
    source_height: u32,
    canvas_width: u32,
    canvas_height: u32,
    fit: FitMode,
    center_x: u16,
    center_y: u16,
    scale: u32,
) -> Result<(SourceRect, DestRect), AppError> {
    if source_width == 0 || source_height == 0 || canvas_width == 0 || canvas_height == 0 {
        return Err(AppError::invalid_argument(
            "Render geometry dimensions must be positive",
        ));
    }
    let sw = u128::from(source_width);
    let sh = u128::from(source_height);
    let cw = u128::from(canvas_width);
    let ch = u128::from(canvas_height);
    let (mut crop_w, mut crop_h, base_w, base_h) = match fit {
        FitMode::Contain => {
            if cw * sh <= ch * sw {
                let h = ((cw * sh) / sw).max(1);
                (
                    source_width,
                    source_height,
                    canvas_width,
                    u32::try_from(h).unwrap_or(canvas_height),
                )
            } else {
                let w = ((ch * sw) / sh).max(1);
                (
                    source_width,
                    source_height,
                    u32::try_from(w).unwrap_or(canvas_width),
                    canvas_height,
                )
            }
        }
        FitMode::Cover => {
            if cw * sh >= ch * sw {
                let h = ((sw * ch) / cw).max(1).min(sh);
                (
                    source_width,
                    u32::try_from(h).unwrap_or(source_height),
                    canvas_width,
                    canvas_height,
                )
            } else {
                let w = ((sh * cw) / ch).max(1).min(sw);
                (
                    u32::try_from(w).unwrap_or(source_width),
                    source_height,
                    canvas_width,
                    canvas_height,
                )
            }
        }
    };
    crop_w = crop_w.max(1).min(source_width);
    crop_h = crop_h.max(1).min(source_height);
    let crop_x = (source_width - crop_w) / 2;
    let crop_y = (source_height - crop_h) / 2;
    let scaled_w = ((u128::from(base_w) * u128::from(scale) + 5_000) / 10_000).max(1);
    let scaled_h = ((u128::from(base_h) * u128::from(scale) + 5_000) / 10_000).max(1);
    let dest_width = u32::try_from(scaled_w)
        .map_err(|_| AppError::invalid_argument("Destination width overflowed"))?;
    let dest_height = u32::try_from(scaled_h)
        .map_err(|_| AppError::invalid_argument("Destination height overflowed"))?;
    let cx = (u64::from(center_x) * u64::from(canvas_width) / 10_000) as i64;
    let cy = (u64::from(center_y) * u64::from(canvas_height) / 10_000) as i64;
    Ok((
        SourceRect {
            x: crop_x,
            y: crop_y,
            width: crop_w,
            height: crop_h,
        },
        DestRect {
            x: (cx - i64::from(dest_width) / 2).clamp(i64::from(i32::MIN), i64::from(i32::MAX))
                as i32,
            y: (cy - i64::from(dest_height) / 2).clamp(i64::from(i32::MIN), i64::from(i32::MAX))
                as i32,
            width: dest_width,
            height: dest_height,
        },
    ))
}

fn wrap_text(text: &str, line_breaks: &[u32], max_chars: usize) -> Vec<String> {
    let max_chars = max_chars.max(1);
    let mut explicit_lines = Vec::new();
    let mut current = String::new();
    let mut char_index = 0u32;
    for character in text.chars() {
        char_index = char_index.saturating_add(1);
        if character == '\n' {
            explicit_lines.push(std::mem::take(&mut current));
        } else {
            current.push(character);
            if line_breaks.binary_search(&char_index).is_ok() {
                explicit_lines.push(std::mem::take(&mut current));
            }
        }
    }
    explicit_lines.push(current);
    if explicit_lines.is_empty() {
        explicit_lines.push(String::new());
    }

    let mut lines = Vec::new();
    for explicit in explicit_lines {
        let mut current = String::new();
        for word in explicit.split_whitespace() {
            let word_len = word.chars().count();
            if word_len > max_chars && current.is_empty() {
                let mut chunk = String::new();
                for character in word.chars() {
                    chunk.push(character);
                    if chunk.chars().count() == max_chars {
                        lines.push(std::mem::take(&mut chunk));
                    }
                }
                if !chunk.is_empty() {
                    current = chunk;
                }
                continue;
            }
            let fits = current.is_empty() || current.chars().count() + 1 + word_len <= max_chars;
            if fits {
                if !current.is_empty() {
                    current.push(' ');
                }
                current.push_str(word);
            } else {
                lines.push(std::mem::take(&mut current));
                current.push_str(word);
            }
        }
        if !current.is_empty() || explicit.is_empty() {
            lines.push(current);
        }
    }
    if lines.is_empty() {
        lines.push(String::new());
    }
    if lines.len() > 2 {
        // Captions are deliberately capped at two visual lines. Re-pack the
        // complete word sequence when a document contains more hard breaks;
        // no source words are discarded, and the fit calculation can then
        // reduce the font size while keeping the entire caption visible.
        let words = lines
            .iter()
            .flat_map(|line| line.split_whitespace())
            .collect::<Vec<_>>();
        let target = words
            .iter()
            .map(|word| word.chars().count())
            .sum::<usize>()
            .saturating_add(words.len().saturating_sub(1))
            / 2;
        let mut first = String::new();
        let mut second = String::new();
        for word in words {
            let goes_first =
                second.is_empty() && (!first.is_empty() && first.chars().count() < target);
            let destination = if first.is_empty() || goes_first {
                &mut first
            } else {
                &mut second
            };
            if !destination.is_empty() {
                destination.push(' ');
            }
            destination.push_str(word);
        }
        lines = vec![first, second];
    }
    lines
}
fn rasterize_text(
    lines: &[String],
    text: &TextItem,
    canvas_width: u32,
    canvas_height: u32,
    font_catalog: &BundledFontCatalog,
) -> Result<(Vec<u8>, u32, u32), AppError> {
    let safe_width = u64::from(canvas_width)
        .saturating_mul(9)
        .checked_div(10)
        .unwrap_or(1)
        .clamp(1, 4_096) as u32;
    let safe_height = u64::from(canvas_height)
        .saturating_mul(9)
        .checked_div(10)
        .unwrap_or(1)
        .clamp(1, 4_096) as u32;
    let line_count = lines.len().max(1) as f64;
    let max_line_chars = lines
        .iter()
        .map(|line| line.chars().count())
        .max()
        .unwrap_or(1)
        .max(1) as f64;
    let requested_font_size = text.font_size.max(1) as f64;
    let natural_line_height = requested_font_size * 1.25;
    let natural_padding = (requested_font_size * 0.20).max(2.0);
    let natural_width =
        (max_line_chars * requested_font_size * 0.90 + natural_padding * 2.0).ceil();
    let natural_height = (line_count * natural_line_height + natural_padding * 2.0).ceil();
    let fit_scale = (f64::from(safe_width) / natural_width.max(1.0))
        .min(f64::from(safe_height) / natural_height.max(1.0))
        .min(1.0);
    let raster_width = (natural_width * fit_scale)
        .ceil()
        .max(1.0)
        .min(f64::from(safe_width)) as u32;
    let raster_height = (natural_height * fit_scale)
        .ceil()
        .max(1.0)
        .min(f64::from(safe_height)) as u32;
    let font_size = (requested_font_size * fit_scale).max(1.0);
    let padding = (natural_padding * fit_scale).max(1.0);
    let line_height = (font_size * 1.25).max(font_size + 1.0);
    let baseline = padding + font_size;

    let mut svg = String::with_capacity(512 + lines.iter().map(String::len).sum::<usize>());
    use std::fmt::Write as _;
    write!(
        &mut svg,
        r#"<svg xmlns="http://www.w3.org/2000/svg" width="{raster_width}" height="{raster_height}" viewBox="0 0 {raster_width} {raster_height}">"#,
    )
    .map_err(|_| AppError::io("The text SVG could not be constructed"))?;
    if text.style == TextStyle::Boxed {
        write!(
            &mut svg,
            r##"<rect x="0" y="0" width="{raster_width}" height="{raster_height}" fill="#000000" fill-opacity="0.745098"/>"##,
        )
        .map_err(|_| AppError::io("The text SVG could not be constructed"))?;
    }
    let fill_opacity = f32::from(text.color.alpha) / 255.0;
    let font_family = graphics::INTER_FONT_FAMILY;
    write!(
        &mut svg,
        r##"<text x="{padding:.3}" y="{baseline:.3}" font-family="{font_family}" font-size="{font_size:.3}" font-weight="400" fill="#{:02x}{:02x}{:02x}" fill-opacity="{fill_opacity:.6}">"##,
        text.color.red,
        text.color.green,
        text.color.blue,
    )
    .map_err(|_| AppError::io("The text SVG could not be constructed"))?;
    for (index, line) in lines.iter().enumerate() {
        let escaped = escape_xml_text(line);
        if index == 0 {
            write!(
                &mut svg,
                r#"<tspan x="{padding:.3}" y="{baseline:.3}">{escaped}</tspan>"#,
            )
        } else {
            write!(
                &mut svg,
                r#"<tspan x="{padding:.3}" dy="{line_height:.3}">{escaped}</tspan>"#,
            )
        }
        .map_err(|_| AppError::io("The text SVG could not be constructed"))?;
    }
    svg.push_str("</text></svg>");
    let (png, output_width, output_height) = if text.style == TextStyle::Clean {
        graphics::rasterize_svg_with_catalog_trimmed(
            &svg,
            raster_width,
            raster_height,
            font_catalog,
        )?
    } else {
        (
            graphics::rasterize_svg_with_catalog(&svg, raster_width, raster_height, font_catalog)?,
            raster_width,
            raster_height,
        )
    };
    if png.len() > 4 * 1024 * 1024 {
        return Err(AppError::new(
            ErrorCode::MediaUnsupported,
            "The text raster exceeds the 4 MiB encoded image limit",
        ));
    }
    Ok((png, output_width, output_height))
}

fn escape_xml_text(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    for character in text.chars() {
        match character {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' => escaped.push_str("&quot;"),
            '\'' => escaped.push_str("&apos;"),
            _ => escaped.push(character),
        }
    }
    escaped
}

fn plan_hash(plan: &RenderPlan) -> Result<String, AppError> {
    let mut canonical = plan.clone();
    canonical.plan_hash.clear();
    let bytes = serde_json::to_vec(&canonical)
        .map_err(|_| AppError::schema("Render plan could not be encoded"))?;
    Ok(format!("{:016x}", stable_bytes_hash(&bytes)))
}

fn stable_bytes_hash(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf29ce484222325u64;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

/// Encode straight RGBA pixels as a standards-compliant PNG.
pub(crate) fn encode_rgba_png(width: u32, height: u32, rgba: &[u8]) -> Result<Vec<u8>, AppError> {
    if width == 0 || height == 0 {
        return Err(AppError::invalid_argument(
            "PNG dimensions must be positive",
        ));
    }
    let pixel_count = u64::from(width)
        .checked_mul(u64::from(height))
        .ok_or_else(|| AppError::invalid_argument("PNG dimensions overflowed"))?;
    let expected = usize::try_from(pixel_count)
        .ok()
        .and_then(|count| count.checked_mul(4))
        .ok_or_else(|| AppError::invalid_argument("PNG pixel allocation is too large"))?;
    if rgba.len() != expected {
        return Err(AppError::invalid_argument(
            "RGBA payload does not match PNG dimensions",
        ));
    }

    let mut output = Vec::new();
    let mut encoder = png::Encoder::new(&mut output, width, height);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    encoder.set_compression(png::Compression::Default);
    let mut writer = encoder
        .write_header()
        .map_err(|_| AppError::io("The RGBA PNG header could not be encoded"))?;
    writer
        .write_image_data(rgba)
        .map_err(|_| AppError::io("The RGBA PNG pixels could not be encoded"))?;
    writer
        .finish()
        .map_err(|_| AppError::io("The RGBA PNG could not be finalized"))?;
    Ok(output)
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::artifacts::ArtifactStore;
    use crate::project::model::{
        AspectRatio, AssetKind, AssetManifest, FitMode, FrameRate, MediaClip, NormalizedAsset,
        NormalizedAudio, NormalizedVideo, OriginalMediaMetadata, OriginalStreamKind,
        OriginalStreamMetadata, ProjectDocument, TrackKind, AUDIO_SAMPLE_RATE,
    };
    use std::path::PathBuf;
    use uuid::Uuid;

    #[test]
    fn frame_sample_conversion_is_exact_for_all_supported_rates() {
        for fps in [
            FrameRate::FPS_24,
            FrameRate::FPS_25,
            FrameRate::FPS_30,
            FrameRate::FPS_60,
        ] {
            assert_eq!(sample_at_frame(0, fps).unwrap(), 0);
            assert_eq!(sample_at_frame(fps.num as u64, fps).unwrap(), 48_000);
        }
    }

    #[test]
    fn geometry_cover_crops_source_width_for_portrait_canvas() {
        let (source, dest) =
            geometry(1920, 1080, 1080, 1920, FitMode::Cover, 5000, 5000, 10_000).unwrap();
        assert_eq!(source.width, 607);
        assert_eq!(source.height, 1080);
        assert_eq!(source.x, 656);
        assert_eq!(source.y, 0);
        assert_eq!(dest.width, 1080);
        assert_eq!(dest.height, 1920);
        assert_eq!(dest.x, 0);
        assert_eq!(dest.y, 0);
    }

    #[test]
    fn png_encoder_produces_valid_signature_and_dimensions() {
        let bytes = encode_rgba_png(2, 1, &[255, 0, 0, 255, 0, 0, 255, 128]).unwrap();
        assert_eq!(&bytes[..8], b"\x89PNG\r\n\x1a\n");
        let decoder = png::Decoder::new(std::io::Cursor::new(bytes));
        let reader = decoder.read_info().unwrap();
        assert_eq!((reader.info().width, reader.info().height), (2, 1));
    }

    const VIDEO_ASSET_ID: &str = "10000000-0000-4000-8000-000000000001";
    const AUDIO_ASSET_ID: &str = "10000000-0000-4000-8000-000000000002";
    const VIDEO_CLIP_ID: &str = "20000000-0000-4000-8000-000000000001";
    const AUDIO_CLIP_ID: &str = "20000000-0000-4000-8000-000000000002";
    const CLIP_DURATION_FRAMES: u64 = 30;

    struct ArtifactFixture {
        root: PathBuf,
        store: ArtifactStore,
    }

    impl ArtifactFixture {
        fn new() -> Self {
            let root =
                std::env::temp_dir().join(format!("cutterhoochee-render-plan-{}", Uuid::new_v4()));
            let font_resource_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("resources");
            let store = ArtifactStore::for_project(&root, Uuid::new_v4().to_string())
                .expect("artifact store")
                .with_font_resource_dir(&font_resource_dir)
                .expect("bundled font resources");
            Self { root, store }
        }
    }

    impl Drop for ArtifactFixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    fn normalized_audio(pcm_artifact_id: &str) -> NormalizedAudio {
        NormalizedAudio {
            pcm_artifact_id: pcm_artifact_id.to_owned(),
            sample_count: AUDIO_SAMPLE_RATE as u64,
            sample_rate: AUDIO_SAMPLE_RATE,
            channels: 2,
            duration_frames: CLIP_DURATION_FRAMES,
            active_start_sample: 0,
            active_end_sample: AUDIO_SAMPLE_RATE as u64,
            source_start_ms: 0,
            source_end_ms: 1_000,
        }
    }

    fn ready_video_asset() -> AssetManifest {
        AssetManifest {
            id: VIDEO_ASSET_ID.to_owned(),
            kind: AssetKind::Video,
            content_hash: "video-hash".to_owned(),
            original: OriginalMediaMetadata {
                file_name: "video.mp4".to_owned(),
                streams: vec![
                    OriginalStreamMetadata {
                        kind: OriginalStreamKind::Video,
                        codec: "h264".to_owned(),
                        duration_ms: Some(1_000),
                        width: Some(1_920),
                        height: Some(1_080),
                        ..Default::default()
                    },
                    OriginalStreamMetadata {
                        kind: OriginalStreamKind::Audio,
                        codec: "aac".to_owned(),
                        duration_ms: Some(1_000),
                        sample_rate: Some(AUDIO_SAMPLE_RATE),
                        channels: Some(2),
                        ..Default::default()
                    },
                ],
                ..Default::default()
            },
            normalization: Some(NormalizedAsset {
                renderer_version: "test".to_owned(),
                epoch_ms: 0,
                video: Some(NormalizedVideo {
                    master_artifact_id: "video-master".to_owned(),
                    proxy_artifact_id: None,
                    frame_count: CLIP_DURATION_FRAMES,
                    width: 1_920,
                    height: 1_080,
                    fps_num: 30,
                    fps_den: 1,
                    active_start_frame: 0,
                    active_end_frame: CLIP_DURATION_FRAMES,
                    source_start_ms: 0,
                    source_end_ms: 1_000,
                    proxy_frame_count: Some(CLIP_DURATION_FRAMES),
                }),
                audio: Some(normalized_audio("video-pcm")),
            }),
        }
    }

    fn ready_audio_asset() -> AssetManifest {
        AssetManifest {
            id: AUDIO_ASSET_ID.to_owned(),
            kind: AssetKind::Audio,
            content_hash: "audio-hash".to_owned(),
            original: OriginalMediaMetadata {
                file_name: "audio.wav".to_owned(),
                streams: vec![OriginalStreamMetadata {
                    kind: OriginalStreamKind::Audio,
                    codec: "pcm_s16le".to_owned(),
                    duration_ms: Some(1_000),
                    sample_rate: Some(AUDIO_SAMPLE_RATE),
                    channels: Some(2),
                    ..Default::default()
                }],
                ..Default::default()
            },
            normalization: Some(NormalizedAsset {
                renderer_version: "test".to_owned(),
                epoch_ms: 0,
                video: None,
                audio: Some(normalized_audio("audio-pcm")),
            }),
        }
    }

    fn media_clip(id: &str, track_id: &str, asset_id: &str, audio_enabled: bool) -> MediaClip {
        MediaClip {
            id: id.to_owned(),
            track_id: track_id.to_owned(),
            asset_id: asset_id.to_owned(),
            start_frame: 0,
            in_frame: 0,
            duration_frames: CLIP_DURATION_FRAMES,
            fit: FitMode::Contain,
            center_x: 5_000,
            center_y: 5_000,
            scale: 10_000,
            opacity: 10_000,
            gain_db: 0.0,
            audio_enabled,
            fade_in_frames: 0,
            fade_out_frames: 0,
        }
    }

    fn track_id(document: &ProjectDocument, kind: TrackKind) -> String {
        document
            .tracks
            .iter()
            .find(|track| track.kind == kind)
            .expect("fixture track")
            .id
            .clone()
    }

    fn fixture_document(video_audio_enabled: bool, audio_audio_enabled: bool) -> ProjectDocument {
        let mut document = ProjectDocument::new(
            "Render plan audio controls",
            AspectRatio::Landscape,
            FrameRate::FPS_30,
        )
        .expect("project document");
        let video_track_id = track_id(&document, TrackKind::Video);
        let audio_track_id = track_id(&document, TrackKind::Audio);
        document.assets = vec![ready_video_asset(), ready_audio_asset()];
        document.clips = vec![
            media_clip(
                VIDEO_CLIP_ID,
                &video_track_id,
                VIDEO_ASSET_ID,
                video_audio_enabled,
            ),
            media_clip(
                AUDIO_CLIP_ID,
                &audio_track_id,
                AUDIO_ASSET_ID,
                audio_audio_enabled,
            ),
        ];
        document.validate().expect("valid render plan fixture");
        document
    }

    fn audio_clip_ids(plan: &RenderPlan) -> Vec<&str> {
        plan.audio
            .segments
            .iter()
            .map(|segment| segment.clip_id.as_str())
            .collect()
    }

    #[test]
    fn muted_tracks_omit_audio_and_unmuting_restores_it() {
        let artifacts = ArtifactFixture::new();
        let mut document = fixture_document(true, true);
        let baseline = compile_render_plan(&document, &artifacts.store).expect("baseline plan");
        assert_eq!(
            audio_clip_ids(&baseline),
            vec![VIDEO_CLIP_ID, AUDIO_CLIP_ID]
        );
        let baseline_layers = baseline.layers.clone();

        let audio_track_id = track_id(&document, TrackKind::Audio);
        document
            .tracks
            .iter_mut()
            .find(|track| track.id == audio_track_id)
            .expect("audio track")
            .muted = true;
        let muted_audio =
            compile_render_plan(&document, &artifacts.store).expect("muted audio-track plan");
        assert_eq!(audio_clip_ids(&muted_audio), vec![VIDEO_CLIP_ID]);
        assert_eq!(muted_audio.layers, baseline_layers);
        assert_eq!(muted_audio.duration_frames, baseline.duration_frames);
        assert_eq!(
            muted_audio.audio.total_samples,
            baseline.audio.total_samples
        );

        document
            .tracks
            .iter_mut()
            .find(|track| track.id == audio_track_id)
            .expect("audio track")
            .muted = false;
        let unmuted_audio =
            compile_render_plan(&document, &artifacts.store).expect("unmuted audio-track plan");
        assert_eq!(
            audio_clip_ids(&unmuted_audio),
            vec![VIDEO_CLIP_ID, AUDIO_CLIP_ID]
        );
        assert_eq!(unmuted_audio.layers, baseline_layers);

        let video_track_id = track_id(&document, TrackKind::Video);
        document
            .tracks
            .iter_mut()
            .find(|track| track.id == video_track_id)
            .expect("video track")
            .muted = true;
        let muted_video =
            compile_render_plan(&document, &artifacts.store).expect("muted video-track plan");
        assert_eq!(audio_clip_ids(&muted_video), vec![AUDIO_CLIP_ID]);
        assert_eq!(muted_video.layers, baseline_layers);
        assert_eq!(muted_video.duration_frames, baseline.duration_frames);
        assert_eq!(
            muted_video.audio.total_samples,
            baseline.audio.total_samples
        );
    }

    #[test]
    fn disabled_audio_clips_are_omitted_on_video_and_audio_tracks_and_reenabled_restores_segments()
    {
        let artifacts = ArtifactFixture::new();
        let mut document = fixture_document(false, false);
        let disabled = compile_render_plan(&document, &artifacts.store).expect("disabled plan");
        assert!(audio_clip_ids(&disabled).is_empty());
        let video_layer = disabled
            .layers
            .iter()
            .find(|layer| layer.kind == RenderLayerKind::Video)
            .expect("video layer");
        assert_eq!(
            video_layer
                .segments
                .iter()
                .map(|segment| segment.clip_id.as_str())
                .collect::<Vec<_>>(),
            vec![VIDEO_CLIP_ID]
        );
        assert_eq!(disabled.duration_frames, CLIP_DURATION_FRAMES);
        assert_eq!(
            disabled.audio.total_samples,
            sample_at_frame(CLIP_DURATION_FRAMES, FrameRate::FPS_30).expect("sample count")
        );
        let disabled_layers = disabled.layers.clone();
        let revision = document.revision;
        assert!(document.clips.iter().all(|clip| !clip.audio_enabled));

        for clip in &mut document.clips {
            clip.audio_enabled = true;
        }
        let reenabled = compile_render_plan(&document, &artifacts.store).expect("re-enabled plan");
        assert_eq!(
            audio_clip_ids(&reenabled),
            vec![VIDEO_CLIP_ID, AUDIO_CLIP_ID]
        );
        assert_eq!(reenabled.layers, disabled_layers);
        assert_eq!(reenabled.duration_frames, disabled.duration_frames);
        assert_eq!(reenabled.audio.total_samples, disabled.audio.total_samples);
        assert_eq!(document.revision, revision);
    }
}
