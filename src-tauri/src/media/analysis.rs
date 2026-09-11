//! Local FFmpeg evidence for scene boundaries and silence intervals.
//!
//! Analysis only reads app-managed normalized masters/PCM. It never mutates
//! media; silence removal is represented as explicit descending `remove_range`
//! operations for the normal transactional editor path.

use crate::editor::dispatcher::CallerContext;
use crate::editor::operations::EditOp;
use crate::error::{AppError, ErrorCode};
use crate::media::artifacts::ArtifactStore;
use crate::media::evidence::run_evidence_job;
use crate::media::jobs::JobContext;
use crate::media::probe::resolve_packaged_binary;
use crate::project::model::FrameRate;
use crate::state::AppState;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::Path;
use std::process::Command;
use ts_rs::TS;

pub const DEFAULT_SCENE_THRESHOLD: f64 = 0.30;
const SPEECH_EDGE_PADDING_MS: i64 = 120;
const MEDIA_DEMUXERS: &str =
    "mov,matroska,avi,mpegts,mpeg,flv,ogg,mp3,wav,flac,aac,image2,png_pipe,jpeg_pipe,webp_pipe";
const RAW_MEDIA_DEMUXERS: &str =
    "f32le,mov,matroska,avi,mpegts,mpeg,flv,ogg,mp3,wav,flac,aac,image2,png_pipe,jpeg_pipe,webp_pipe";
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
#[ts(tag = "action", rename_all = "snake_case")]
pub enum AnalysisAction {
    Scenes {
        #[serde(rename = "assetId")]
        #[ts(rename = "assetId")]
        asset_id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        #[ts(optional)]
        threshold: Option<f64>,
    },
    Silence {
        #[serde(rename = "assetId")]
        #[ts(rename = "assetId")]
        asset_id: String,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[ts(rename_all = "camelCase")]
pub struct SceneBoundary {
    #[ts(type = "SafeInteger")]
    pub frame: u64,
    #[ts(type = "SafeInteger")]
    pub source_time_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[ts(rename_all = "camelCase")]
pub struct SceneAnalysisReply {
    pub asset_id: String,
    pub threshold: f64,
    pub boundaries: Vec<SceneBoundary>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[ts(rename_all = "camelCase")]
pub struct SilenceInterval {
    pub asset_id: String,
    #[ts(type = "SafeInteger")]
    pub start_frame: u64,
    #[ts(type = "SafeInteger")]
    pub end_frame: u64,
    #[ts(type = "SafeInteger")]
    pub start_time_ms: u64,
    #[ts(type = "SafeInteger")]
    pub end_time_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(
    tag = "kind",
    content = "data",
    rename_all = "snake_case",
    deny_unknown_fields
)]
#[ts(tag = "kind", content = "data", rename_all = "snake_case")]
pub enum AnalysisReply {
    Scenes(SceneAnalysisReply),
    Silence {
        #[serde(rename = "assetId")]
        #[ts(rename = "assetId")]
        asset_id: String,
        intervals: Vec<SilenceInterval>,
    },
}

#[derive(Debug, Clone)]
pub struct AnalysisRuntime;

impl AnalysisRuntime {
    pub fn new() -> Self {
        Self
    }

    pub async fn handle(
        &self,
        action: AnalysisAction,
        caller: &CallerContext,
        state: &AppState,
    ) -> Result<AnalysisReply, AppError> {
        state.validate_generation(caller.generation)?;
        match action {
            AnalysisAction::Scenes {
                asset_id,
                threshold,
            } => Ok(AnalysisReply::Scenes(
                self.scenes(state, caller, &asset_id, threshold).await?,
            )),
            AnalysisAction::Silence { asset_id } => {
                let intervals = self.silence(state, caller, &asset_id).await?;
                Ok(AnalysisReply::Silence {
                    asset_id,
                    intervals,
                })
            }
        }
    }

    async fn scenes(
        &self,
        state: &AppState,
        caller: &CallerContext,
        asset_id: &str,
        threshold: Option<f64>,
    ) -> Result<SceneAnalysisReply, AppError> {
        let threshold = threshold.unwrap_or(DEFAULT_SCENE_THRESHOLD);
        if !threshold.is_finite() || !(0.0..=1.0).contains(&threshold) {
            return Err(AppError::invalid_argument(
                "The scene threshold must be finite and in the range 0..1",
            ));
        }
        let store = state.current_store()?;
        let snapshot = store.snapshot()?;
        let asset = snapshot
            .document
            .assets
            .iter()
            .find(|asset| asset.id == asset_id)
            .ok_or_else(|| {
                AppError::new(
                    ErrorCode::AssetUnavailable,
                    "The requested media asset is unavailable",
                )
            })?;
        let normalization = asset.normalization.as_ref().ok_or_else(|| {
            AppError::new(
                ErrorCode::AssetUnavailable,
                "The media asset has no normalized master yet",
            )
        })?;
        let video = normalization.video.as_ref().ok_or_else(|| {
            AppError::new(
                ErrorCode::MediaUnsupported,
                "Scene analysis requires a video stream",
            )
        })?;
        let artifacts = ArtifactStore::for_project(store.root(), store.workspace_id())?;
        let master = artifacts.managed_path(&video.master_artifact_id)?;
        let state_for_job = state.clone();
        let master_for_job = master.clone();
        let output = run_evidence_job(
            state,
            caller,
            "scene_analysis",
            Some(snapshot.project_id.clone()),
            move |context| run_scene_filter(&context, &state_for_job, &master_for_job, threshold),
        )
        .await?;
        let boundaries =
            parse_scene_boundaries(&output, snapshot.document.profile.fps(), video.frame_count)?;
        Ok(SceneAnalysisReply {
            asset_id: asset_id.to_owned(),
            threshold,
            boundaries,
        })
    }

    async fn silence(
        &self,
        state: &AppState,
        caller: &CallerContext,
        asset_id: &str,
    ) -> Result<Vec<SilenceInterval>, AppError> {
        let store = state.current_store()?;
        let snapshot = store.snapshot()?;
        let asset = snapshot
            .document
            .assets
            .iter()
            .find(|asset| asset.id == asset_id)
            .ok_or_else(|| {
                AppError::new(
                    ErrorCode::AssetUnavailable,
                    "The requested media asset is unavailable",
                )
            })?;
        let normalization = asset.normalization.as_ref().ok_or_else(|| {
            AppError::new(
                ErrorCode::AssetUnavailable,
                "The media asset has no normalized PCM yet",
            )
        })?;
        let audio = normalization.audio.as_ref().ok_or_else(|| {
            AppError::new(
                ErrorCode::MediaUnsupported,
                "Silence analysis requires an audio stream",
            )
        })?;
        let artifacts = ArtifactStore::for_project(store.root(), store.workspace_id())?;
        let pcm = artifacts.managed_path(&audio.pcm_artifact_id)?;
        let state_for_job = state.clone();
        let pcm_for_job = pcm.clone();
        let output = run_evidence_job(
            state,
            caller,
            "silence_analysis",
            Some(snapshot.project_id.clone()),
            move |context| run_silence_filter(&context, &state_for_job, &pcm_for_job),
        )
        .await?;
        let fps = snapshot.document.profile.fps();
        let frame_count = asset.frame_count().unwrap_or(audio.duration_frames);
        let intervals = parse_silencedetect(&output, fps, frame_count)?;
        let mut result = Vec::with_capacity(intervals.len());
        for (start_frame, end_frame) in intervals {
            result.push(SilenceInterval {
                asset_id: asset_id.to_owned(),
                start_time_ms: frame_to_ms(start_frame, fps)?,
                end_time_ms: frame_to_ms(end_frame, fps)?,
                start_frame,
                end_frame,
            });
        }
        Ok(result)
    }
}

impl Default for AnalysisRuntime {
    fn default() -> Self {
        Self::new()
    }
}

fn run_scene_filter(
    context: &JobContext,
    state: &AppState,
    master: &Path,
    threshold: f64,
) -> Result<Vec<u8>, AppError> {
    let ffmpeg = resolve_packaged_binary(&state.paths().resource_dir, "ffmpeg")?;
    let filter = format!("select=gt(scene\\,{threshold:.6}),showinfo");
    let mut command = Command::new(&ffmpeg);
    command
        .arg("-hide_banner")
        .arg("-nostdin")
        .arg("-protocol_whitelist")
        .arg("file,pipe")
        .arg("-format_whitelist")
        .arg(MEDIA_DEMUXERS)
        .arg("-i")
        .arg(master)
        .arg("-vf")
        .arg(filter)
        .args(["-an", "-f", "null", "-"]);
    let output = match context.run_command(command) {
        Ok(output) => output,
        Err(error) => {
            return Err(if error.code == ErrorCode::JobCancelled {
                error
            } else {
                AppError::new(
                    ErrorCode::MediaUnsupported,
                    "Scene analysis could not decode the normalized master",
                )
            })
        }
    };
    context.check_cancelled()?;
    context.progress(0.95)?;
    Ok(output.stderr)
}

fn run_silence_filter(
    context: &JobContext,
    state: &AppState,
    pcm: &Path,
) -> Result<Vec<u8>, AppError> {
    let is_wav = fs::File::open(pcm)
        .and_then(|mut file| {
            let mut header = [0u8; 12];
            std::io::Read::read_exact(&mut file, &mut header)?;
            Ok(&header[0..4] == b"RIFF" && &header[8..12] == b"WAVE")
        })
        .unwrap_or(false);
    let ffmpeg = resolve_packaged_binary(&state.paths().resource_dir, "ffmpeg")?;
    let mut command = Command::new(&ffmpeg);
    command
        .arg("-hide_banner")
        .arg("-nostdin")
        .arg("-protocol_whitelist")
        .arg("file,pipe")
        .arg("-format_whitelist")
        .arg(if is_wav {
            MEDIA_DEMUXERS
        } else {
            RAW_MEDIA_DEMUXERS
        });
    if !is_wav {
        command.args(["-f", "f32le", "-ar", "48000", "-ac", "2"]);
    }
    command
        .arg("-i")
        .arg(pcm)
        .arg("-af")
        .arg("silencedetect=noise=-35dB:d=0.5")
        .args(["-f", "null", "-"]);
    let output = match context.run_command(command) {
        Ok(output) => output,
        Err(error) => {
            return Err(if error.code == ErrorCode::JobCancelled {
                error
            } else {
                AppError::new(
                    ErrorCode::MediaUnsupported,
                    "Silence analysis could not decode normalized PCM",
                )
            })
        }
    };
    context.check_cancelled()?;
    context.progress(0.95)?;
    Ok(output.stderr)
}

/// Parse `showinfo` timestamps into exact normalized frame indices. `pts_time`
/// is parsed as a decimal rational rather than through binary floating point.
pub fn parse_scene_boundaries(
    output: &[u8],
    fps: FrameRate,
    frame_count: u64,
) -> Result<Vec<SceneBoundary>, AppError> {
    fps.validate()?;
    let text = String::from_utf8_lossy(output);
    let mut frames = Vec::new();
    for line in text.lines() {
        let Some(value) = find_log_value(line, "pts_time:") else {
            continue;
        };
        let (numerator, denominator) = parse_decimal(value)?;
        let frame = (numerator
            .checked_mul(i128::from(fps.num))
            .ok_or_else(|| AppError::schema("Scene timestamp overflows"))?)
        .div_euclid(
            denominator
                .checked_mul(i128::from(fps.den))
                .ok_or_else(|| AppError::schema("Scene timestamp overflows"))?,
        );
        if frame < 0 {
            continue;
        }
        let frame =
            u64::try_from(frame).map_err(|_| AppError::schema("Scene frame is out of range"))?;
        if frame >= frame_count
            || frames
                .iter()
                .any(|boundary: &SceneBoundary| boundary.frame == frame)
        {
            continue;
        }
        let source_time_ms = numerator
            .checked_mul(1_000)
            .ok_or_else(|| AppError::schema("Scene timestamp overflows"))?
            .div_euclid(denominator);
        let source_time_ms = u64::try_from(source_time_ms)
            .map_err(|_| AppError::schema("Scene timestamp is out of range"))?;
        frames.push(SceneBoundary {
            frame,
            source_time_ms,
        });
    }
    frames.sort_by_key(|boundary| boundary.frame);
    Ok(frames)
}

/// Parse FFmpeg `silencedetect` output, merge adjacent/overlapping intervals,
/// and retain 120 ms at each speech edge before projecting to frames.
pub fn parse_silencedetect(
    output: &[u8],
    fps: FrameRate,
    frame_count: u64,
) -> Result<Vec<(u64, u64)>, AppError> {
    fps.validate()?;
    let text = String::from_utf8_lossy(output);
    let mut raw = Vec::<(i64, i64)>::new();
    let mut open: Option<i64> = None;
    for line in text.lines() {
        if let Some(value) = find_log_value(line, "silence_start:") {
            let (numerator, denominator) = parse_decimal(value)?;
            let milliseconds = numerator
                .checked_mul(1_000)
                .ok_or_else(|| AppError::schema("Silence timestamp overflows"))?
                .div_euclid(denominator);
            let milliseconds = i64::try_from(milliseconds)
                .map_err(|_| AppError::schema("Silence timestamp is out of range"))?;
            if milliseconds >= 0 {
                open = Some(milliseconds);
            }
        }
        if let Some(value) = find_log_value(line, "silence_end:") {
            let (numerator, denominator) = parse_decimal(value)?;
            let milliseconds = numerator
                .checked_mul(1_000)
                .ok_or_else(|| AppError::schema("Silence timestamp overflows"))?
                .div_euclid(denominator);
            let milliseconds = i64::try_from(milliseconds)
                .map_err(|_| AppError::schema("Silence timestamp is out of range"))?;
            if let Some(start) = open.take() {
                if milliseconds > start {
                    raw.push((start, milliseconds));
                }
            }
        }
    }
    if let Some(start) = open {
        let duration_ms = i64::try_from(frame_to_ms(frame_count, fps)?)
            .map_err(|_| AppError::schema("Silence timestamp is out of range"))?;
        if duration_ms > start {
            raw.push((start, duration_ms));
        }
    }
    raw.sort_unstable();
    let mut merged = Vec::<(i64, i64)>::new();
    for interval in raw {
        if let Some(last) = merged.last_mut() {
            if interval.0 <= last.1 {
                last.1 = last.1.max(interval.1);
                continue;
            }
        }
        merged.push(interval);
    }
    let mut projected = Vec::new();
    for (start, end) in merged {
        let start = start
            .checked_add(SPEECH_EDGE_PADDING_MS)
            .ok_or_else(|| AppError::schema("Silence edge padding overflows"))?;
        let end = end
            .checked_sub(SPEECH_EDGE_PADDING_MS)
            .ok_or_else(|| AppError::schema("Silence edge padding underflows"))?;
        if end <= start {
            continue;
        }
        // Ceil the removal start and floor the removal end so speech at either
        // edge is retained; the interval remains half-open.
        let start_frame = ms_to_frame_ceil(start, fps)?;
        let end_frame = ms_to_frame_floor(end, fps)?;
        if start_frame < end_frame && start_frame < frame_count {
            projected.push((start_frame, end_frame.min(frame_count)));
        }
    }
    Ok(projected)
}

/// Build the editor's explicit descending ripple operations. No destructive
/// source rewrite is performed by analysis.
pub fn silence_remove_operations(intervals: &[(u64, u64)]) -> Result<Vec<EditOp>, AppError> {
    let mut sorted = intervals.to_vec();
    sorted.sort_by(|left, right| right.0.cmp(&left.0).then(right.1.cmp(&left.1)));
    let mut operations = Vec::with_capacity(sorted.len());
    for (start_frame, end_frame) in sorted {
        if end_frame <= start_frame {
            return Err(AppError::invalid_argument(
                "A silence removal interval must be positive",
            ));
        }
        operations.push(EditOp::RemoveRange {
            start_frame,
            end_frame,
            ripple: true,
        });
    }
    Ok(operations)
}

fn find_log_value<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    let mut parts = line.split_whitespace();
    while let Some(part) = parts.next() {
        if let Some(value) = part.strip_prefix(key) {
            return if value.is_empty() {
                parts.next()
            } else {
                Some(value)
            };
        }
        if part == key {
            return parts.next();
        }
    }
    None
}

fn parse_decimal(value: &str) -> Result<(i128, i128), AppError> {
    let value = value.trim();
    if value.is_empty() || value.starts_with('-') {
        return Err(AppError::schema("FFmpeg timestamp is invalid"));
    }
    let (whole, fraction) = value.split_once('.').unwrap_or((value, ""));
    if whole.is_empty()
        || !whole.bytes().all(|byte| byte.is_ascii_digit())
        || !fraction.bytes().all(|byte| byte.is_ascii_digit())
    {
        return Err(AppError::schema("FFmpeg timestamp is invalid"));
    }
    let denominator = 10_i128
        .checked_pow(
            u32::try_from(fraction.len())
                .map_err(|_| AppError::schema("FFmpeg timestamp is too precise"))?,
        )
        .ok_or_else(|| AppError::schema("FFmpeg timestamp is too precise"))?;
    let whole = whole
        .parse::<i128>()
        .map_err(|_| AppError::schema("FFmpeg timestamp is out of range"))?;
    let fractional = if fraction.is_empty() {
        0
    } else {
        fraction
            .parse::<i128>()
            .map_err(|_| AppError::schema("FFmpeg timestamp is out of range"))?
    };
    let numerator = whole
        .checked_mul(denominator)
        .and_then(|value| value.checked_add(fractional))
        .ok_or_else(|| AppError::schema("FFmpeg timestamp is out of range"))?;
    Ok((numerator, denominator))
}

fn ms_to_frame_floor(milliseconds: i64, fps: FrameRate) -> Result<u64, AppError> {
    if milliseconds < 0 {
        return Err(AppError::invalid_argument(
            "A media timestamp cannot be negative",
        ));
    }
    let denominator = i128::from(1_000u64)
        .checked_mul(i128::from(fps.den))
        .ok_or_else(|| AppError::schema("Media frame denominator overflows"))?;
    let numerator = i128::from(milliseconds)
        .checked_mul(i128::from(fps.num))
        .ok_or_else(|| AppError::schema("Media timestamp overflows"))?;
    let value = numerator.div_euclid(denominator);
    u64::try_from(value).map_err(|_| AppError::schema("Media frame is out of range"))
}

fn ms_to_frame_ceil(milliseconds: i64, fps: FrameRate) -> Result<u64, AppError> {
    if milliseconds < 0 {
        return Err(AppError::invalid_argument(
            "A media timestamp cannot be negative",
        ));
    }
    let denominator = i128::from(1_000u64)
        .checked_mul(i128::from(fps.den))
        .ok_or_else(|| AppError::schema("Media frame denominator overflows"))?;
    let numerator = i128::from(milliseconds)
        .checked_mul(i128::from(fps.num))
        .ok_or_else(|| AppError::schema("Media timestamp overflows"))?;
    let rounded = numerator
        .checked_add(
            denominator
                .checked_sub(1)
                .ok_or_else(|| AppError::schema("Media frame denominator is invalid"))?,
        )
        .ok_or_else(|| AppError::schema("Media timestamp overflows"))?;
    let value = rounded.div_euclid(denominator);
    u64::try_from(value).map_err(|_| AppError::schema("Media frame is out of range"))
}

fn frame_to_ms(frame: u64, fps: FrameRate) -> Result<u64, AppError> {
    fps.validate()?;
    let numerator = u128::from(frame)
        .checked_mul(1_000)
        .and_then(|value| value.checked_mul(u128::from(fps.den)))
        .ok_or_else(|| AppError::schema("Media timestamp overflows"))?;
    let value = numerator
        .checked_div(u128::from(fps.num))
        .ok_or_else(|| AppError::schema("Media frame rate is invalid"))?;
    u64::try_from(value).map_err(|_| AppError::schema("Media timestamp is out of range"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scenes_parse_exact_cfr_frames_and_ignore_duplicates() {
        let output = b"[Parsed_showinfo_0] n:0 pts_time:0.000000\n[Parsed_showinfo_0] n:1 pts_time:1.400000\n[Parsed_showinfo_0] n:2 pts_time:1.400000\n";
        let result = parse_scene_boundaries(output, FrameRate::FPS_30, 120).expect("scene parse");
        assert_eq!(
            result
                .iter()
                .map(|boundary| boundary.frame)
                .collect::<Vec<_>>(),
            vec![0, 42]
        );
        assert_eq!(result[1].source_time_ms, 1_400);
    }

    #[test]
    fn silence_intervals_merge_and_keep_edge_padding() {
        let output = b"[silencedetect] silence_start: 0.000\n[silencedetect] silence_end: 1.000 | silence_duration: 1.000\n[silencedetect] silence_start:0.900\n[silencedetect] silence_end:2.000 | silence_duration: 1.100\n";
        let result = parse_silencedetect(output, FrameRate::FPS_30, 90).expect("silence parse");
        assert_eq!(result, vec![(4, 56)]);
    }

    #[test]
    fn unterminated_silence_is_bounded_by_normalized_duration() {
        let output = b"[silencedetect] silence_start: 1.000\n";
        let result = parse_silencedetect(output, FrameRate::FPS_30, 90).expect("silence parse");
        assert_eq!(result, vec![(34, 86)]);
    }

    #[test]
    fn silence_removal_operations_are_descending() {
        let operations = silence_remove_operations(&[(10, 20), (40, 45)]).expect("operations");
        assert!(matches!(
            operations[0],
            EditOp::RemoveRange {
                start_frame: 40,
                ..
            }
        ));
        assert!(matches!(
            operations[1],
            EditOp::RemoveRange {
                start_frame: 10,
                ..
            }
        ));
    }
}
