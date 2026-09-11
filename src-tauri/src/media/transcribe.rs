//! Local Whisper evidence: pinned model activation, normalized PCM conversion,
//! and strict full-JSON timestamp projection.
//!
//! The parser in this module is deliberately independent from process startup.
//! That keeps malformed or adversarial Whisper output from becoming a caption
//! and makes the timestamp contract easy to exercise without a model download.

use crate::editor::dispatcher::{CallerContext, CallerKind};
use crate::editor::operations::{Transcript, TranscriptSpan};
use crate::error::{AppError, ErrorCode};
use crate::media::artifacts::ArtifactStore;
use crate::media::evidence::run_evidence_job;
use crate::media::jobs::{JobContext, JobPriority, JobSpec};
use crate::media::probe::resolve_packaged_binary;
use crate::project::model::FrameRate;
use crate::state::AppState;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;
use uuid::Uuid;

pub const WHISPER_VERSION: &str = "1.9.2";
pub const WHISPER_COMMIT: &str = "306c88f4d1286aec1bf96e544632897886af5501";
pub const WHISPER_MODEL_REPOSITORY: &str = "ggerganov/whisper.cpp";
pub const WHISPER_MODEL_REVISION: &str = "5359861c739e955e79d9a303bcbc70fb988958b1";
pub const WHISPER_MODEL_FILE: &str = "ggml-small.bin";
pub const WHISPER_MODEL_URL: &str = "https://huggingface.co/ggerganov/whisper.cpp/resolve/5359861c739e955e79d9a303bcbc70fb988958b1/ggml-small.bin";
pub const WHISPER_MODEL_BYTES: u64 = 487_601_967;
pub const WHISPER_MODEL_SHA256: &str =
    "1be3a9b2063867b937e64e2ec7483364a79917e157fa98c5d94b5c1fffea987b";
pub const MODEL_LANGUAGE: &str = "auto";
const MAX_WHISPER_JSON_BYTES: u64 = 64 * 1024 * 1024;
const MAX_TRANSCRIPT_CACHE_BYTES: u64 = 64 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ts_rs::TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[ts(rename_all = "camelCase")]
pub struct SpeechModelInfo {
    pub version: String,
    pub commit: String,
    pub repository: String,
    pub revision: String,
    pub source_url: String,
    pub file: String,
    #[ts(type = "SafeInteger")]
    pub expected_bytes: u64,
    pub sha256: String,
    pub available: bool,
    pub download_required: bool,
    pub local_only: bool,
}

impl Default for SpeechModelInfo {
    fn default() -> Self {
        Self {
            version: WHISPER_VERSION.to_owned(),
            commit: WHISPER_COMMIT.to_owned(),
            repository: WHISPER_MODEL_REPOSITORY.to_owned(),
            revision: WHISPER_MODEL_REVISION.to_owned(),
            source_url: WHISPER_MODEL_URL.to_owned(),
            file: WHISPER_MODEL_FILE.to_owned(),
            expected_bytes: WHISPER_MODEL_BYTES,
            sha256: WHISPER_MODEL_SHA256.to_owned(),
            available: false,
            download_required: true,
            local_only: true,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ts_rs::TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[ts(rename_all = "camelCase")]
pub struct TranscribeRequest {
    pub asset_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional, type = "SafeInteger")]
    pub start_frame: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional, type = "SafeInteger")]
    pub end_frame: Option<u64>,
    /// A missing/false value never starts a model download. The consent is
    /// action-specific and is not persisted in the project document.
    #[serde(default)]
    pub model_consent: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ts_rs::TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[ts(rename_all = "camelCase")]
pub struct TranscribeReply {
    pub transcript: Transcript,
    pub model: SpeechModelInfo,
    pub cached: bool,
    pub approximate_timing: bool,
}

#[derive(Debug, Clone)]
pub struct TranscribeRuntime;

impl TranscribeRuntime {
    pub fn new() -> Self {
        Self
    }
    /// Return pinned model metadata without downloading, creating directories,
    /// or requiring consent. The UI uses this read-only preflight to explain
    /// the first-use download before asking for action-specific consent.
    pub fn model_status(&self, state: &AppState) -> Result<SpeechModelInfo, AppError> {
        let available = valid_model_file(&self.model_path(state))?;
        Ok(model_info(available))
    }

    fn model_path(&self, state: &AppState) -> PathBuf {
        state
            .paths()
            .app_data_dir
            .join("speech-models")
            .join(WHISPER_MODEL_REVISION)
            .join(WHISPER_MODEL_FILE)
    }

    pub async fn handle(
        &self,
        request: TranscribeRequest,
        caller: &CallerContext,
        state: &AppState,
    ) -> Result<TranscribeReply, AppError> {
        state.validate_generation(caller.generation)?;
        let store = state.current_store()?;
        let snapshot = store.snapshot()?;
        let asset = snapshot
            .document
            .assets
            .iter()
            .find(|asset| asset.id == request.asset_id)
            .ok_or_else(|| {
                AppError::new(
                    ErrorCode::AssetUnavailable,
                    "The requested media asset is unavailable",
                )
            })?
            .clone();
        let normalization = asset.normalization.clone().ok_or_else(|| {
            AppError::new(
                ErrorCode::AssetUnavailable,
                "The media asset is still being prepared and has no normalized PCM",
            )
        })?;
        normalization.validate(asset.kind)?;
        let audio = normalization.audio.clone().ok_or_else(|| {
            AppError::new(
                ErrorCode::MediaUnsupported,
                "The media asset has no normalized audio stream to transcribe",
            )
        })?;
        if audio.sample_rate != 48_000 || audio.channels != 2 {
            return Err(AppError::new(
                ErrorCode::MediaUnsupported,
                "The normalized transcription input is not 48 kHz stereo PCM",
            ));
        }
        let fps = snapshot.document.profile.fps();
        fps.validate()?;
        let frame_count = asset.frame_count().unwrap_or(audio.duration_frames);
        let (start_frame, end_frame) =
            checked_source_range(request.start_frame, request.end_frame, frame_count)?;
        let source_hash = asset.content_hash.clone();
        let cache_key = cache_key(&source_hash, start_frame, end_frame, fps);
        if let Some(cached) = load_cached_transcript(state, &cache_key, &asset.id, &source_hash)? {
            state.write_transcript_at(caller.generation, caller.run_id(), &cached)?;
            let model = self.model_status(state)?;
            return Ok(TranscribeReply {
                approximate_timing: cached.segments.iter().any(|segment| segment.approximate),
                transcript: cached,
                model,
                cached: true,
            });
        }
        let trusted_model_consent =
            request.model_consent && matches!(&caller.kind, CallerKind::HumanWindow { .. });
        let model_path = self
            .ensure_model(state, caller, trusted_model_consent)
            .await?;
        let model = model_info(true);

        let artifacts = ArtifactStore::for_project(store.root(), store.workspace_id())?;
        let pcm = artifacts.managed_path(&audio.pcm_artifact_id)?;
        let (input_start_ms, duration_ms, base_ms) = transcription_window(
            normalization.epoch_ms,
            audio.source_start_ms,
            fps,
            start_frame,
            end_frame,
        )?;
        let state_for_job = state.clone();
        let model_for_job = model_path.clone();
        let pcm_for_job = pcm.clone();
        let whisper_output = run_evidence_job(
            state,
            caller,
            "transcription",
            Some(snapshot.project_id.clone()),
            move |context| {
                let wav = extract_wav(
                    &context,
                    &state_for_job,
                    &pcm_for_job,
                    input_start_ms,
                    duration_ms,
                )?;
                let result = Self::run_whisper(&context, &state_for_job, &model_for_job, &wav);
                let _ = fs::remove_file(&wav);
                result
            },
        )
        .await?;
        let whisper_json = match fs::read(&whisper_output) {
            Ok(bytes) => {
                let _ = fs::remove_file(&whisper_output);
                bytes
            }
            Err(_) => {
                let _ = fs::remove_file(&whisper_output);
                return Err(AppError::io("The transcript JSON could not be read"));
            }
        };
        let transcript_id = Uuid::new_v4().to_string();
        let transcript = parse_whisper_json_for_range(
            &whisper_json,
            transcript_id,
            asset.id.clone(),
            source_hash.clone(),
            fps,
            frame_count,
            base_ms,
            start_frame,
            end_frame,
        )?;

        state.write_transcript_at(caller.generation, caller.run_id(), &transcript)?;
        save_cached_transcript(state, &cache_key, &transcript)?;
        Ok(TranscribeReply {
            approximate_timing: transcript
                .segments
                .iter()
                .any(|segment| segment.approximate),
            transcript,
            model,
            cached: false,
        })
    }

    async fn ensure_model(
        &self,
        state: &AppState,
        caller: &CallerContext,
        consent: bool,
    ) -> Result<PathBuf, AppError> {
        let model_path = self.model_path(state);
        let model_dir = model_path.parent().ok_or_else(|| {
            AppError::io("The app-owned speech model path has no parent directory")
        })?;
        if valid_model_file(&model_path)? {
            return Ok(model_path);
        }
        if !consent {
            let details = serde_json::to_value(model_info(false))
                .map_err(|_| AppError::schema("The speech model metadata could not be encoded"))?;
            return Err(AppError::new(
                ErrorCode::PermissionDenied,
                "Automatic captions need explicit HumanWindow consent to download the pinned multilingual Whisper small model",
            )
            .with_details(details));
        }
        fs::create_dir_all(model_dir).map_err(|_| {
            AppError::io("The app-owned speech model directory could not be created")
        })?;
        let run_id = caller.run_id().map(str::to_owned);
        if let Some(run_id) = run_id.as_deref() {
            state.require_active_run_at(caller.generation, run_id)?;
        }
        let spec = JobSpec::new(
            "speech_model_download",
            JobPriority::Background,
            caller.generation,
            state.current_project_id(),
        )?
        .with_run_id(run_id.clone());
        let registry = state.jobs().registry();
        let state_for_worker = state.clone();
        let generation = caller.generation;
        let path_for_job = model_path.clone();
        let job = registry.submit(spec, move |context| {
            context.check_cancelled()?;
            if let Some(run_id) = run_id.as_deref() {
                state_for_worker.require_active_run_at(generation, run_id)?;
            }
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|_| {
                    AppError::io("The speech model download runtime could not be started")
                })?;
            runtime.block_on(download_model(&path_for_job, &context))?;
            context.check_cancelled()?;
            Ok(serde_json::Value::Null)
        })?;
        let job_id = job.job_id;
        let wait_registry = registry.clone();
        tokio::task::spawn_blocking(move || {
            wait_registry.wait_blocking(&job_id, Duration::from_secs(60 * 60))
        })
        .await
        .map_err(|_| AppError::io("The speech model download worker stopped unexpectedly"))??;
        Ok(model_path)
    }

    fn run_whisper(
        context: &JobContext,
        state: &AppState,
        model: &Path,
        wav: &Path,
    ) -> Result<PathBuf, AppError> {
        let binary = locate_whisper_binary(state)?;
        let prefix = state
            .paths()
            .temp_dir
            .join(format!("whisper-{}", Uuid::new_v4().simple()));
        let output_path = PathBuf::from(format!("{}.json", prefix.display()));
        let mut command = Command::new(&binary);
        command
            .arg("-m")
            .arg(model)
            .arg("-f")
            .arg(wav)
            .arg("-l")
            .arg(MODEL_LANGUAGE)
            .arg("-ojf")
            .arg("-of")
            .arg(&prefix)
            .arg("-ml")
            .arg("1")
            .arg("-sow");
        if let Err(error) = context.run_command(command) {
            let _ = fs::remove_file(&output_path);
            return Err(if error.code == ErrorCode::JobCancelled {
                error
            } else {
                AppError::new(
                    ErrorCode::MediaUnsupported,
                    "whisper-cli could not transcribe the normalized audio",
                )
            });
        }
        if let Err(error) = context.check_cancelled() {
            let _ = fs::remove_file(&output_path);
            return Err(error);
        }
        let metadata = match fs::metadata(&output_path) {
            Ok(metadata) => metadata,
            Err(_) => {
                let _ = fs::remove_file(&output_path);
                return Err(AppError::new(
                    ErrorCode::MediaUnsupported,
                    "whisper-cli did not produce full JSON output",
                ));
            }
        };
        if metadata.len() > MAX_WHISPER_JSON_BYTES {
            let _ = fs::remove_file(&output_path);
            return Err(AppError::new(
                ErrorCode::MediaUnsupported,
                "The transcript JSON exceeds the supported size",
            ));
        }
        Ok(output_path)
    }
}

impl Default for TranscribeRuntime {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Deserialize)]
struct WhisperDocument {
    #[serde(default)]
    transcription: Vec<WhisperSegment>,
}

#[derive(Debug, Deserialize)]
struct WhisperSegment {
    text: String,
    offsets: WhisperOffsets,
    #[serde(default)]
    tokens: Vec<WhisperToken>,
}

#[derive(Debug, Deserialize, Clone, Copy)]
struct WhisperOffsets {
    from: i64,
    to: i64,
}

#[derive(Debug, Deserialize)]
struct WhisperToken {
    text: String,
    #[serde(default)]
    offsets: Option<WhisperOffsets>,
}
#[derive(Debug)]
struct ProjectedToken {
    span: TranscriptSpan,
    raw_text: String,
    token_text: bool,
}

/// Parse whisper.cpp's verified `-ojf` format. Token timestamps are used only
/// if every retained token has a valid, ordered interval; otherwise the
/// segment interval is kept with `approximate=true`. No timing is invented.
pub fn parse_whisper_json(
    bytes: &[u8],
    transcript_id: String,
    asset_id: String,
    source_hash: String,
    fps: FrameRate,
    asset_frame_count: u64,
    base_ms: i64,
) -> Result<Transcript, AppError> {
    parse_whisper_json_for_range(
        bytes,
        transcript_id,
        asset_id,
        source_hash,
        fps,
        asset_frame_count,
        base_ms,
        0,
        asset_frame_count,
    )
}

fn parse_whisper_json_for_range(
    bytes: &[u8],
    transcript_id: String,
    asset_id: String,
    source_hash: String,
    fps: FrameRate,
    asset_frame_count: u64,
    base_ms: i64,
    source_start_frame: u64,
    source_end_frame: u64,
) -> Result<Transcript, AppError> {
    fps.validate()?;
    if bytes.len() as u64 > MAX_WHISPER_JSON_BYTES {
        return Err(AppError::schema(
            "The transcript JSON exceeds the supported size",
        ));
    }
    let (source_start_frame, source_end_frame) = checked_source_range(
        Some(source_start_frame),
        Some(source_end_frame),
        asset_frame_count,
    )?;
    let document: WhisperDocument = serde_json::from_slice(bytes)
        .map_err(|_| AppError::schema("The whisper full JSON output is malformed"))?;
    let mut segments = Vec::new();
    let mut previous_segment_end_ms = None;
    for segment in document.transcription {
        let segment_raw_text = segment.text;
        let segment_text = clean_transcript_text(&segment_raw_text);
        let segment_offsets = segment.offsets;
        let segment_interval =
            project_ms_interval(segment_offsets, base_ms, fps, asset_frame_count)?;
        validate_source_order(&mut previous_segment_end_ms, segment_offsets)?;
        let mut token_spans = Vec::new();
        let mut token_valid = !segment.tokens.is_empty();
        let mut previous_token_end_ms = None;
        for token in segment.tokens {
            let raw_text = token.text;
            let text = clean_transcript_text(&raw_text);
            if text.is_empty() {
                continue;
            }
            let Some(offsets) = token.offsets else {
                token_valid = false;
                break;
            };
            let Ok((start_frame, end_frame)) =
                project_ms_interval(offsets, base_ms, fps, asset_frame_count)
            else {
                token_valid = false;
                break;
            };
            if offsets.from < segment_offsets.from
                || offsets.to > segment_offsets.to
                || validate_source_order(&mut previous_token_end_ms, offsets).is_err()
            {
                token_valid = false;
                break;
            }
            let Some((start_frame, end_frame, clipped)) = clip_frame_interval(
                (start_frame, end_frame),
                source_start_frame,
                source_end_frame,
            ) else {
                continue;
            };
            token_spans.push(ProjectedToken {
                span: TranscriptSpan {
                    start_frame,
                    end_frame,
                    text,
                    approximate: clipped,
                },
                raw_text,
                token_text: true,
            });
        }
        let token_spans = merge_projected_tokens(token_spans);
        if token_valid
            && !token_spans.is_empty()
            && token_spans
                .iter()
                .all(|span| span.span.start_frame < span.span.end_frame)
            && token_spans
                .windows(2)
                .all(|pair| pair[0].span.end_frame <= pair[1].span.start_frame)
        {
            segments.extend(token_spans);
        } else if !segment_text.is_empty() {
            let Some((start_frame, end_frame, _)) =
                clip_frame_interval(segment_interval, source_start_frame, source_end_frame)
            else {
                continue;
            };
            segments.push(ProjectedToken {
                span: TranscriptSpan {
                    start_frame,
                    end_frame,
                    text: segment_text,
                    approximate: true,
                },
                raw_text: segment_raw_text,
                token_text: false,
            });
        }
    }
    let segments = merge_projected_spans(segments);
    if segments.is_empty() {
        return Err(AppError::new(
            ErrorCode::MediaUnsupported,
            "whisper-cli returned no speech text; no captions were created",
        ));
    }
    let mut segments = segments;
    segments.sort_by_key(|segment| (segment.start_frame, segment.end_frame));
    if !segments
        .windows(2)
        .all(|pair| pair[0].end_frame <= pair[1].start_frame)
    {
        return Err(AppError::schema(
            "The whisper timestamps overlap after frame projection",
        ));
    }
    let transcript = Transcript {
        transcript_id,
        asset_id,
        source_hash,
        segments,
    };
    transcript.validate()?;
    Ok(transcript)
}

fn clip_frame_interval(
    (start_frame, end_frame): (u64, u64),
    source_start_frame: u64,
    source_end_frame: u64,
) -> Option<(u64, u64, bool)> {
    let clipped_start = start_frame.max(source_start_frame);
    let clipped_end = end_frame.min(source_end_frame);
    (clipped_start < clipped_end).then(|| {
        (
            clipped_start,
            clipped_end,
            clipped_start != start_frame || clipped_end != end_frame,
        )
    })
}

fn project_ms_interval(
    offsets: WhisperOffsets,
    base_ms: i64,
    fps: FrameRate,
    frame_count: u64,
) -> Result<(u64, u64), AppError> {
    if offsets.from < 0 || offsets.to <= offsets.from {
        return Err(AppError::schema(
            "Whisper offsets must be increasing non-negative milliseconds",
        ));
    }
    let from_ms = i128::from(base_ms)
        .checked_add(i128::from(offsets.from))
        .ok_or_else(|| AppError::schema("Whisper start timestamp overflows"))?;
    let to_ms = i128::from(base_ms)
        .checked_add(i128::from(offsets.to))
        .ok_or_else(|| AppError::schema("Whisper end timestamp overflows"))?;
    if from_ms < 0 || to_ms <= from_ms {
        return Err(AppError::schema(
            "Whisper offsets fall outside the source interval",
        ));
    }
    let denominator = i128::from(1_000u64) * i128::from(fps.den);
    let start = (from_ms * i128::from(fps.num)).div_euclid(denominator);
    let end = (to_ms * i128::from(fps.num) + denominator - 1).div_euclid(denominator);
    if start < 0 || end <= start || end > i128::from(frame_count) {
        return Err(AppError::schema(
            "Whisper source interval exceeds normalized frame bounds",
        ));
    }
    let start = u64::try_from(start)
        .map_err(|_| AppError::schema("Whisper start frame is out of range"))?;
    let end =
        u64::try_from(end).map_err(|_| AppError::schema("Whisper end frame is out of range"))?;
    Ok((start, end))
}

fn validate_source_order(
    previous_end_ms: &mut Option<i64>,
    offsets: WhisperOffsets,
) -> Result<(), AppError> {
    if previous_end_ms.is_some_and(|previous_end_ms| offsets.from < previous_end_ms) {
        return Err(AppError::schema(
            "The whisper timestamps overlap in source time",
        ));
    }
    *previous_end_ms = Some(offsets.to);
    Ok(())
}

/// Covering projection can overlap adjacent source intervals. Merge those
/// intervals only when they overlap or token text proves that the next span is
/// a subword continuation; Whisper's raw whitespace remains authoritative.
fn merge_projected_tokens(tokens: Vec<ProjectedToken>) -> Vec<ProjectedToken> {
    let mut spans = Vec::new();
    for token in tokens {
        if let Some(previous) = spans.last_mut() {
            if should_merge_projected_spans(previous, &token) {
                previous.raw_text.push_str(&token.raw_text);
                previous.span.text = clean_transcript_text(&previous.raw_text);
                previous.span.end_frame = previous.span.end_frame.max(token.span.end_frame);
                previous.span.approximate = true;
                continue;
            }
        }
        spans.push(token);
    }
    spans
}

fn merge_projected_spans(spans: Vec<ProjectedToken>) -> Vec<TranscriptSpan> {
    let mut result = Vec::new();
    for span in spans {
        if let Some(previous) = result.last_mut() {
            if should_merge_projected_spans(previous, &span) {
                let has_boundary = previous.raw_text.ends_with(char::is_whitespace)
                    || span.raw_text.starts_with(char::is_whitespace);
                if !has_boundary && !(previous.token_text && span.token_text) {
                    previous.raw_text.push(' ');
                }
                previous.raw_text.push_str(&span.raw_text);
                previous.span.text = clean_transcript_text(&previous.raw_text);
                previous.span.end_frame = previous.span.end_frame.max(span.span.end_frame);
                previous.span.approximate = true;
                continue;
            }
        }
        result.push(span);
    }
    result.into_iter().map(|projected| projected.span).collect()
}

fn should_merge_projected_spans(previous: &ProjectedToken, next: &ProjectedToken) -> bool {
    previous.span.end_frame > next.span.start_frame
        || (previous.token_text
            && next.token_text
            && !previous.raw_text.ends_with(char::is_whitespace)
            && !next.raw_text.starts_with(char::is_whitespace))
}

fn clean_transcript_text(input: &str) -> String {
    let trimmed = input.trim();
    if is_special_token(trimmed) {
        return String::new();
    }
    let mut cleaned = String::with_capacity(input.len());
    for character in input.chars() {
        if character.is_control() && character != '\n' && character != '\t' {
            continue;
        }
        cleaned.push(character);
    }
    cleaned.trim().to_owned()
}

fn is_special_token(value: &str) -> bool {
    (value.starts_with("<|") && value.ends_with("|>"))
        || (value.starts_with("[|") && value.ends_with("|]"))
        || value == "[BLANK_AUDIO]"
        || value == "[SOT]"
        || value == "[EOT]"
}

fn checked_source_range(
    start_frame: Option<u64>,
    end_frame: Option<u64>,
    asset_frame_count: u64,
) -> Result<(u64, u64), AppError> {
    let start = start_frame.unwrap_or(0);
    let end = end_frame.unwrap_or(asset_frame_count);
    if start >= end || end > asset_frame_count {
        return Err(AppError::invalid_argument(
            "The transcription source range is outside the normalized asset",
        ));
    }
    Ok((start, end))
}

fn frame_to_ms(frame: u64, fps: FrameRate) -> Result<i64, AppError> {
    let value = (u128::from(frame) * 1_000u128 * u128::from(fps.den)) / u128::from(fps.num);
    i64::try_from(value)
        .map_err(|_| AppError::invalid_argument("The transcription range is too large"))
}

fn model_info(available: bool) -> SpeechModelInfo {
    SpeechModelInfo {
        available,
        download_required: !available,
        ..SpeechModelInfo::default()
    }
}

fn valid_model_file(path: &Path) -> Result<bool, AppError> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(_) => return Err(AppError::io("The speech model could not be inspected")),
    };
    if metadata.file_type().is_symlink()
        || !metadata.is_file()
        || metadata.len() != WHISPER_MODEL_BYTES
    {
        return Ok(false);
    }
    let mut file =
        File::open(path).map_err(|_| AppError::io("The speech model could not be opened"))?;
    let digest = digest_reader(&mut file)?;
    Ok(digest == WHISPER_MODEL_SHA256)
}

async fn download_model(path: &Path, context: &JobContext) -> Result<(), AppError> {
    context.check_cancelled()?;
    let response = reqwest::get(WHISPER_MODEL_URL)
        .await
        .map_err(|_| AppError::io("The pinned speech model could not be downloaded"))?;
    if !response.status().is_success() {
        return Err(AppError::io(
            "The pinned speech model download was rejected",
        ));
    }
    if let Some(length) = response.content_length() {
        if length != WHISPER_MODEL_BYTES {
            return Err(AppError::schema("The pinned speech model size changed"));
        }
    }
    let parent = path
        .parent()
        .ok_or_else(|| AppError::io("The speech model path has no parent directory"))?;
    let temporary = parent.join(format!(
        ".{}.{}.tmp",
        WHISPER_MODEL_FILE,
        Uuid::new_v4().simple()
    ));
    let result = async {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
            .map_err(|_| AppError::io("The temporary speech model could not be created"))?;
        let mut hasher = Sha256::new();
        let mut size = 0u64;
        let mut response = response;
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_| AppError::io("The speech model download was interrupted"))?
        {
            context.check_cancelled()?;
            size = size
                .checked_add(chunk.len() as u64)
                .ok_or_else(|| AppError::schema("The speech model size overflows"))?;
            if size > WHISPER_MODEL_BYTES {
                return Err(AppError::schema(
                    "The speech model is larger than its pinned size",
                ));
            }
            file.write_all(&chunk)
                .map_err(|_| AppError::io("The temporary speech model could not be written"))?;
            hasher.update(&chunk);
            context.progress(0.05 + (size as f64 / WHISPER_MODEL_BYTES as f64) * 0.9)?;
        }
        context.check_cancelled()?;
        file.sync_all()
            .map_err(|_| AppError::io("The temporary speech model could not be synchronized"))?;
        if size != WHISPER_MODEL_BYTES
            || hex_digest(hasher.finalize().as_slice()) != WHISPER_MODEL_SHA256
        {
            return Err(AppError::schema(
                "The downloaded speech model failed its pinned checksum",
            ));
        }
        context.check_cancelled()?;
        fs::rename(&temporary, path)
            .map_err(|_| AppError::io("The speech model could not be atomically activated"))?;
        if let Ok(directory) = File::open(parent) {
            let _ = directory.sync_all();
        }
        Ok::<(), AppError>(())
    }
    .await;
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn locate_whisper_binary(state: &AppState) -> Result<PathBuf, AppError> {
    resolve_packaged_binary(&state.paths().resource_dir, "whisper-cli")
}

fn transcription_window(
    epoch_ms: i64,
    audio_source_start_ms: i64,
    fps: FrameRate,
    start_frame: u64,
    end_frame: u64,
) -> Result<(i64, i64, i64), AppError> {
    let relative_start_ms = i128::from(frame_to_ms(start_frame, fps)?);
    let relative_end_ms = i128::from(frame_to_ms(end_frame, fps)?);
    let epoch = i128::from(epoch_ms);
    let audio_start = i128::from(audio_source_start_ms);
    let common_start = epoch
        .checked_add(relative_start_ms)
        .ok_or_else(|| AppError::invalid_argument("The transcription epoch overflows"))?;
    let common_end = epoch
        .checked_add(relative_end_ms)
        .ok_or_else(|| AppError::invalid_argument("The transcription epoch overflows"))?;
    let input_start = (common_start - audio_start).max(0);
    let input_end = (common_end - audio_start).max(input_start);
    let duration = input_end
        .checked_sub(input_start)
        .ok_or_else(|| AppError::invalid_argument("The transcription range is invalid"))?;
    let base = audio_start
        .checked_sub(epoch)
        .and_then(|value| value.checked_add(input_start))
        .ok_or_else(|| AppError::invalid_argument("The transcription source origin overflows"))?;
    if duration <= 0 || base < 0 {
        return Err(AppError::invalid_argument(
            "The transcription range is invalid",
        ));
    }
    Ok((
        i64::try_from(input_start)
            .map_err(|_| AppError::invalid_argument("The transcription range is too large"))?,
        i64::try_from(duration)
            .map_err(|_| AppError::invalid_argument("The transcription range is too large"))?,
        i64::try_from(base).map_err(|_| {
            AppError::invalid_argument("The transcription source origin is too large")
        })?,
    ))
}

fn extract_wav(
    context: &JobContext,
    state: &AppState,
    pcm: &Path,
    start_ms: i64,
    duration_ms: i64,
) -> Result<PathBuf, AppError> {
    let output = state
        .paths()
        .temp_dir
        .join(format!("transcribe-{}.wav", Uuid::new_v4().simple()));
    if start_ms < 0 || duration_ms <= 0 {
        return Err(AppError::invalid_argument(
            "The transcription range is invalid",
        ));
    }
    let is_wav = fs::File::open(pcm)
        .and_then(|mut file| {
            let mut header = [0u8; 12];
            file.read_exact(&mut header)?;
            Ok(&header[0..4] == b"RIFF" && &header[8..12] == b"WAVE")
        })
        .unwrap_or(false);
    let ffmpeg = resolve_packaged_binary(&state.paths().resource_dir, "ffmpeg")?;
    let mut command = Command::new(&ffmpeg);
    command.args([
        "-hide_banner",
        "-nostdin",
        "-y",
        "-protocol_whitelist",
        "file,pipe",
        "-format_whitelist",
        "f32le,mov,matroska,avi,mpegts,mpeg,flv,ogg,mp3,wav,flac,aac,image2,png_pipe,jpeg_pipe,webp_pipe",
    ]);
    if !is_wav {
        command.args(["-f", "f32le", "-ar", "48000", "-ac", "2"]);
    }
    command
        .arg("-i")
        .arg(pcm)
        .arg("-ss")
        .arg(format_millis(start_ms))
        .arg("-t")
        .arg(format_millis(duration_ms))
        .args([
            "-vn",
            "-ac",
            "1",
            "-ar",
            "16000",
            "-sample_fmt",
            "s16",
            "-f",
            "wav",
        ])
        .arg(&output);
    if let Err(error) = context.run_command(command) {
        let _ = fs::remove_file(&output);
        return Err(if error.code == ErrorCode::JobCancelled {
            error
        } else {
            AppError::new(
                ErrorCode::MediaUnsupported,
                "The normalized PCM could not be converted for transcription",
            )
        });
    }
    if let Err(error) = context.check_cancelled() {
        let _ = fs::remove_file(&output);
        return Err(error);
    }
    if let Err(error) = context.progress(0.20) {
        let _ = fs::remove_file(&output);
        return Err(error);
    }
    Ok(output)
}

fn format_millis(milliseconds: i64) -> String {
    let seconds = milliseconds.div_euclid(1_000);
    let remainder = milliseconds.rem_euclid(1_000);
    format!("{seconds}.{remainder:03}")
}

fn cache_key(source_hash: &str, start_frame: u64, end_frame: u64, fps: FrameRate) -> String {
    let mut hasher = Sha256::new();
    hasher.update(source_hash.as_bytes());
    hasher.update(b"\0whisper.cpp-");
    hasher.update(WHISPER_VERSION.as_bytes());
    hasher.update(b"\0auto\0covering-spans-v3\0");
    hasher.update(WHISPER_MODEL_SHA256.as_bytes());
    hasher.update(fps.num.to_le_bytes());
    hasher.update(fps.den.to_le_bytes());
    hasher.update(start_frame.to_le_bytes());
    hasher.update(end_frame.to_le_bytes());
    hex_digest(hasher.finalize().as_slice())
}

fn load_cached_transcript(
    state: &AppState,
    key: &str,
    asset_id: &str,
    source_hash: &str,
) -> Result<Option<Transcript>, AppError> {
    let path = state
        .paths()
        .app_cache_dir
        .join("transcripts")
        .join(format!("{key}.json"));
    let metadata = match fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err(AppError::io("The transcript cache could not be inspected")),
    };
    if metadata.file_type().is_symlink()
        || !metadata.is_file()
        || metadata.len() > MAX_TRANSCRIPT_CACHE_BYTES
    {
        return Ok(None);
    }
    let bytes =
        fs::read(&path).map_err(|_| AppError::io("The transcript cache could not be read"))?;
    let transcript: Transcript = serde_json::from_slice(&bytes)
        .map_err(|_| AppError::schema("The transcript cache is malformed"))?;
    if transcript.asset_id != asset_id || transcript.source_hash != source_hash {
        return Ok(None);
    }
    transcript.validate()?;
    Ok(Some(transcript))
}

fn save_cached_transcript(
    state: &AppState,
    key: &str,
    transcript: &Transcript,
) -> Result<(), AppError> {
    let directory = state.paths().app_cache_dir.join("transcripts");
    fs::create_dir_all(&directory)
        .map_err(|_| AppError::io("The transcript cache directory could not be created"))?;
    let path = directory.join(format!("{key}.json"));
    let temporary = directory.join(format!(".{key}.{}.tmp", Uuid::new_v4().simple()));
    let bytes = serde_json::to_vec(transcript)?;
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)
        .map_err(|_| AppError::io("The transcript cache temporary file could not be created"))?;
    file.write_all(&bytes)
        .and_then(|_| file.sync_all())
        .map_err(|_| AppError::io("The transcript cache could not be written"))?;
    fs::rename(&temporary, &path).map_err(|_| {
        let _ = fs::remove_file(&temporary);
        AppError::io("The transcript cache could not be activated")
    })
}

fn digest_reader(reader: &mut impl Read) -> Result<String, AppError> {
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 128 * 1024];
    loop {
        let count = reader
            .read(&mut buffer)
            .map_err(|_| AppError::io("The speech model could not be hashed"))?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
    }
    Ok(hex_digest(hasher.finalize().as_slice()))
}

fn hex_digest(bytes: &[u8]) -> String {
    let mut result = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        result.push(char::from(b"0123456789abcdef"[(byte >> 4) as usize]));
        result.push(char::from(b"0123456789abcdef"[(byte & 0x0f) as usize]));
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fps() -> FrameRate {
        FrameRate::FPS_30
    }

    fn ids() -> (String, String) {
        (Uuid::new_v4().to_string(), Uuid::new_v4().to_string())
    }

    #[test]
    fn full_json_uses_token_offsets_without_inventing_alignment() {
        let json = br#"{
          "transcription": [{
            "text": " Hello world",
            "offsets": {"from": 0, "to": 1000},
            "tokens": [
              {"text": " Hello", "offsets": {"from": 0, "to": 500}},
              {"text": " world", "offsets": {"from": 500, "to": 1000}}
            ]
          }]
        }"#;
        let (transcript_id, asset_id) = ids();
        let transcript = parse_whisper_json(
            json,
            transcript_id,
            asset_id,
            "hash".to_owned(),
            fps(),
            120,
            0,
        )
        .expect("valid whisper JSON");
        assert_eq!(transcript.segments.len(), 2);
        assert!(!transcript.segments[0].approximate);
        assert_eq!(transcript.segments[0].start_frame, 0);
        assert_eq!(transcript.segments[0].end_frame, 15);
        assert_eq!(transcript.segments[1].start_frame, 15);
    }

    #[test]
    fn missing_token_offset_keeps_segment_as_approximate() {
        let json = br#"{"transcription":[{"text":"hello","offsets":{"from":100,"to":900},"tokens":[{"text":"hello"}]}]}"#;
        let (transcript_id, asset_id) = ids();
        let transcript = parse_whisper_json(
            json,
            transcript_id,
            asset_id,
            "hash".to_owned(),
            fps(),
            120,
            0,
        )
        .expect("segment fallback");
        assert_eq!(transcript.segments.len(), 1);
        assert!(transcript.segments[0].approximate);
        assert_eq!(transcript.segments[0].start_frame, 3);
        assert_eq!(transcript.segments[0].end_frame, 27);
    }

    #[test]
    fn special_tokens_and_empty_speech_do_not_become_fake_captions() {
        let json = br#"{"transcription":[{"text":"<|nospeech|>","offsets":{"from":0,"to":1000},"tokens":[{"text":"<|nospeech|>","offsets":{"from":0,"to":1000}}]}]}"#;
        let (transcript_id, asset_id) = ids();
        let error = parse_whisper_json(
            json,
            transcript_id,
            asset_id,
            "hash".to_owned(),
            fps(),
            120,
            0,
        )
        .expect_err("special-only output must not be a caption");
        assert_eq!(error.code, ErrorCode::MediaUnsupported);
    }

    #[test]
    fn source_offset_is_added_before_frame_projection() {
        let json = br#"{"transcription":[{"text":"word","offsets":{"from":0,"to":1000}}]}"#;
        let (transcript_id, asset_id) = ids();
        let transcript = parse_whisper_json(
            json,
            transcript_id,
            asset_id,
            "hash".to_owned(),
            fps(),
            180,
            2_000,
        )
        .expect("offset projection");
        assert_eq!(transcript.segments[0].start_frame, 60);
        assert_eq!(transcript.segments[0].end_frame, 90);
    }

    #[test]
    fn adjacent_subframe_tokens_keep_covering_bounds_and_all_text() {
        let json = br#"{
          "transcription": [{
            "text": "first second",
            "offsets": {"from": 0, "to": 34},
            "tokens": [
              {"text": "first", "offsets": {"from": 0, "to": 17}},
              {"text": " second", "offsets": {"from": 17, "to": 34}}
            ]
          }]
        }"#;
        let (transcript_id, asset_id) = ids();
        let transcript = parse_whisper_json(
            json,
            transcript_id,
            asset_id,
            "hash".to_owned(),
            fps(),
            120,
            0,
        )
        .expect("adjacent token timestamps");
        assert_eq!(transcript.segments.len(), 1);
        assert_eq!(transcript.segments[0].text, "first second");
        assert!(transcript.segments[0].approximate);
        assert_eq!(
            (
                transcript.segments[0].start_frame,
                transcript.segments[0].end_frame
            ),
            (0, 2)
        );
    }

    #[test]
    fn subframe_token_text_is_merged_without_inventing_timing() {
        let json = br#"{
          "transcription": [{
            "text": "a b c",
            "offsets": {"from": 0, "to": 100},
            "tokens": [
              {"text": "a", "offsets": {"from": 0, "to": 34}},
              {"text": " b", "offsets": {"from": 34, "to": 35}},
              {"text": " c", "offsets": {"from": 35, "to": 100}}
            ]
          }]
        }"#;
        let (transcript_id, asset_id) = ids();
        let transcript = parse_whisper_json(
            json,
            transcript_id,
            asset_id,
            "hash".to_owned(),
            fps(),
            120,
            0,
        )
        .expect("subframe token text is retained");
        assert_eq!(transcript.segments.len(), 1);
        assert_eq!(transcript.segments[0].text, "a b c");
        assert_eq!(
            (
                transcript.segments[0].start_frame,
                transcript.segments[0].end_frame
            ),
            (0, 3)
        );
    }

    #[test]
    fn overlapping_source_timestamps_are_rejected_before_projection() {
        let json = br#"{
          "transcription": [
            {"text": "first", "offsets": {"from": 0, "to": 17}},
            {"text": "second", "offsets": {"from": 10, "to": 34}}
          ]
        }"#;
        let (transcript_id, asset_id) = ids();
        let error = parse_whisper_json(
            json,
            transcript_id,
            asset_id,
            "hash".to_owned(),
            fps(),
            120,
            0,
        )
        .expect_err("source-overlapping timestamps must fail");
        assert_eq!(error.code, ErrorCode::SchemaUnsupported);
        assert!(error.message.contains("source time"));
    }

    #[test]
    fn adjacent_subframe_segments_coalesce_but_invalid_tokens_fall_back() {
        let json = br#"{"transcription":[
            {"text":"Hello","offsets":{"from":0,"to":17},
             "tokens":[{"text":"Hel","offsets":{"from":0,"to":12}},
                       {"text":"lo","offsets":{"from":10,"to":17}}]},
            {"text":"world","offsets":{"from":17,"to":34}}
        ]}"#;
        let (transcript_id, asset_id) = ids();
        let transcript = parse_whisper_json(
            json,
            transcript_id,
            asset_id,
            "hash".to_owned(),
            fps(),
            120,
            0,
        )
        .expect("source-adjacent segment fallback");
        assert_eq!(transcript.segments.len(), 1);
        assert_eq!(transcript.segments[0].text, "Hello world");
        assert_eq!(transcript.segments[0].start_frame, 0);
        assert_eq!(transcript.segments[0].end_frame, 2);
        assert!(transcript.segments[0].approximate);
    }
    #[test]
    fn requested_range_clips_decoder_padding_and_preserves_subword_boundaries() {
        let json = r#"{"transcription":[
            {"text":"Test. Z","offsets":{"from":0,"to":34},
             "tokens":[{"text":"Test. Z","offsets":{"from":0,"to":34}}]},
            {"text":"uer","offsets":{"from":34,"to":35},
             "tokens":[{"text":"uer","offsets":{"from":34,"to":35}}]},
            {"text":"st","offsets":{"from":35,"to":36},
             "tokens":[{"text":"st","offsets":{"from":35,"to":36}}]},
            {"text":"entfer","offsets":{"from":36,"to":37},
             "tokens":[{"text":" entfer","offsets":{"from":36,"to":37}}]},
            {"text":"nen wir","offsets":{"from":37,"to":38},
             "tokens":[{"text":"nen wir","offsets":{"from":37,"to":38}}]},
            {"text":"erg","offsets":{"from":38,"to":39},
             "tokens":[{"text":" erg","offsets":{"from":38,"to":39}}]},
            {"text":"änzen wir","offsets":{"from":39,"to":100},
             "tokens":[{"text":"änzen wir","offsets":{"from":39,"to":100}}]},
            {"text":"tail","offsets":{"from":7900,"to":8100},
             "tokens":[{"text":" tail","offsets":{"from":7900,"to":8100}}]}
        ]}"#
        .as_bytes();
        let (transcript_id, asset_id) = ids();
        let transcript = parse_whisper_json_for_range(
            json,
            transcript_id,
            asset_id,
            "hash".to_owned(),
            fps(),
            300,
            0,
            0,
            240,
        )
        .expect("range-aware token projection");

        assert_eq!(transcript.segments.len(), 2);
        assert_eq!(
            transcript.segments[0].text,
            "Test. Zuerst entfernen wir ergänzen wir"
        );
        assert!(transcript.segments[0].approximate);
        assert_eq!(
            (
                transcript.segments[0].start_frame,
                transcript.segments[0].end_frame
            ),
            (0, 3)
        );
        assert_eq!(
            (
                transcript.segments[1].start_frame,
                transcript.segments[1].end_frame
            ),
            (237, 240)
        );
        assert!(transcript.segments[1].approximate);
        assert!(transcript
            .segments
            .iter()
            .all(|segment| segment.start_frame < 240 && segment.end_frame <= 240));
    }

    #[test]
    fn transcription_window_uses_common_epoch_for_different_stream_starts() {
        let (input_start_ms, duration_ms, base_ms) =
            transcription_window(10_000, 10_500, fps(), 0, 60).expect("window");
        assert_eq!(input_start_ms, 0);
        assert_eq!(duration_ms, 1_500);
        assert_eq!(base_ms, 500);

        let (input_start_ms, duration_ms, base_ms) =
            transcription_window(10_000, 10_500, fps(), 30, 60).expect("window");
        assert_eq!(input_start_ms, 500);
        assert_eq!(duration_ms, 1_000);
        assert_eq!(base_ms, 1_000);
    }
    #[test]
    fn model_status_reports_pinned_local_download_metadata() {
        let info = model_info(false);
        assert_eq!(info.source_url, WHISPER_MODEL_URL);
        assert_eq!(info.expected_bytes, WHISPER_MODEL_BYTES);
        assert_eq!(info.sha256, WHISPER_MODEL_SHA256);
        assert!(!info.available);
        assert!(info.download_required);
        assert!(info.local_only);
    }
}
