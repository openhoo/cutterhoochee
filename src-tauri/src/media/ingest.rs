use super::artifacts::{ArtifactKind, ArtifactRecord, ArtifactStore};
use super::jobs::{JobContext, JobPriority, JobRegistry, JobSpec};
use super::probe::{capture_identity, probe_media, revalidate_identity, ProbeResult};
use crate::error::{AppError, ErrorCode};
use crate::project::model::{
    AssetKind, AssetManifest, NormalizedAsset, NormalizedAudio, NormalizedVideo, ProjectProfile,
    AUDIO_SAMPLE_RATE,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use ts_rs::TS;
use uuid::Uuid;

const RENDERER_VERSION: &str = "cutterhoochee-media-v1";
const MAX_WAVEFORM_POINTS: usize = 4096;

fn invalid(message: impl Into<String>) -> AppError {
    AppError::invalid_argument(message)
}

#[derive(Debug, Clone)]
pub struct IngestOptions {
    pub profile: ProjectProfile,
    pub renderer_version: String,
    pub ffmpeg: PathBuf,
    pub ffprobe: PathBuf,
    pub store: ArtifactStore,
}

impl IngestOptions {
    pub fn validate(&self) -> Result<(), AppError> {
        self.profile.validate()?;
        if self.renderer_version.trim().is_empty() || self.renderer_version.len() > 128 {
            return Err(invalid("Media renderer version is invalid"));
        }
        validate_binary(&self.ffmpeg, "ffmpeg")?;
        validate_binary(&self.ffprobe, "ffprobe")?;
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase")]
pub struct PreparedAsset {
    pub asset: AssetManifest,
    pub artifacts: Vec<ArtifactRecord>,
}

impl PreparedAsset {
    pub fn validate(&self) -> Result<(), AppError> {
        self.asset.validate()?;
        for artifact in &self.artifacts {
            artifact.validate()?;
        }
        Ok(())
    }
}

/// Prepare one ordinary imported file. All coordinates are tied to the
/// selected project profile; no later preview/export pass re-normalizes the
/// original VFR timestamps.
pub fn prepare_asset(
    context: &JobContext,
    options: &IngestOptions,
    input: &Path,
) -> Result<PreparedAsset, AppError> {
    options.validate()?;
    if !input.is_absolute() {
        return Err(AppError::new(
            ErrorCode::PermissionDenied,
            "Imported media paths must be absolute",
        ));
    }
    context.progress(0.01)?;
    let identity = capture_identity(input)?;
    prepare_asset_with_identity(context, options, input, &identity)
}

/// Continue preparation with an identity captured after native authorization.
/// The caller may use the content hash to claim shared normalization work
/// before any probe/encode work starts.
pub fn prepare_asset_with_identity(
    context: &JobContext,
    options: &IngestOptions,
    input: &Path,
    identity: &super::probe::OriginalIdentity,
) -> Result<PreparedAsset, AppError> {
    options.validate()?;
    if !input.is_absolute() {
        return Err(AppError::new(
            ErrorCode::PermissionDenied,
            "Imported media paths must be absolute",
        ));
    }
    let identity = identity.clone();
    context.progress(0.01)?;
    let probe = probe_media(&options.ffprobe, Path::new(&identity.canonical_path))?;
    context.event(
        "media_job_probe",
        json!({
            "jobId": context.job_id(),
            "format": probe.format_name,
            "assetKind": probe.asset_kind,
            "width": probe.width,
            "height": probe.height,
            "durationMs": probe.source_end_ms.saturating_sub(probe.epoch_ms),
            "epochMs": probe.epoch_ms,
            "sourceEndMs": probe.source_end_ms,
            "hasAudio": probe.audio_stream_index.is_some(),
            "hasHdr": probe.has_hdr,
        }),
    );
    revalidate_identity(&identity)?;
    context.progress(0.08)?;
    let asset_id = Uuid::new_v4().to_string();
    let base_key = format!(
        "{}|renderer={}|profile={}x{}@{}/{}|epoch={}",
        identity.content_hash,
        options.renderer_version,
        options.profile.width,
        options.profile.height,
        options.profile.fps_num,
        options.profile.fps_den,
        probe.epoch_ms
    );
    let mut artifacts = Vec::new();
    let normalization = match probe.asset_kind {
        AssetKind::StillImage => {
            let master = render_still(context, options, &probe, &base_key)?;
            artifacts.push(master.clone());
            let frame_count = 1;
            let width = probe.width.unwrap_or(1);
            let height = probe.height.unwrap_or(1);
            Some(NormalizedAsset {
                renderer_version: options.renderer_version.clone(),
                epoch_ms: probe.epoch_ms,
                video: Some(NormalizedVideo {
                    master_artifact_id: master.artifact_id,
                    proxy_artifact_id: None,
                    frame_count,
                    width,
                    height,
                    fps_num: options.profile.fps_num,
                    fps_den: options.profile.fps_den,
                    active_start_frame: 0,
                    active_end_frame: 1,
                    source_start_ms: probe.epoch_ms,
                    source_end_ms: probe.source_end_ms.max(probe.epoch_ms.saturating_add(1)),
                    proxy_frame_count: None,
                }),
                audio: None,
            })
        }
        AssetKind::Video => {
            let master = render_video_master(context, options, &probe, &base_key)?;
            context.progress(0.48)?;
            let proxy = render_video_proxy(context, options, &probe, &master, &base_key)?;
            context.progress(0.66)?;
            let master_path = options.store.managed_path(&master.artifact_id)?;
            let proxy_path = options.store.managed_path(&proxy.artifact_id)?;
            let frame_count = probe_frame_count(&options.ffprobe, &master_path)?;
            let proxy_frame_count = probe_frame_count(&options.ffprobe, &proxy_path)?;
            if frame_count == 0 || proxy_frame_count == 0 {
                return Err(AppError::new(
                    ErrorCode::MediaUnsupported,
                    "The normalized video contains no frames",
                ));
            }
            if proxy_frame_count != frame_count {
                return Err(AppError::new(
                    ErrorCode::MediaUnsupported,
                    "The normalized proxy frame count differs from its master",
                ));
            }
            let normalized_probe = probe_media(&options.ffprobe, &master_path)?;
            let video = probe.video().ok_or_else(|| {
                AppError::new(ErrorCode::MediaUnsupported, "The file has no video stream")
            })?;
            let active_start_frame = frame_offset_frames(
                video.start_time_ms.unwrap_or(probe.epoch_ms),
                probe.epoch_ms,
                options.profile.fps_num,
                options.profile.fps_den,
            );
            if active_start_frame >= frame_count {
                return Err(AppError::new(
                    ErrorCode::MediaUnsupported,
                    "The normalized video presentation starts outside its frame bounds",
                ));
            }
            let active_duration_frames = video
                .duration_ms
                .map(|duration| {
                    duration_to_frames(duration, options.profile.fps_num, options.profile.fps_den)
                })
                .unwrap_or(frame_count.saturating_sub(active_start_frame));
            let active_end_frame = active_start_frame
                .saturating_add(active_duration_frames)
                .min(frame_count);
            let audio = if probe.audio().is_some() {
                let (normalized, artifact) = render_audio(context, options, &probe, &base_key)?;
                artifacts.push(artifact);
                Some(normalized)
            } else {
                None
            };
            let width = normalized_probe.width.unwrap_or(probe.width.unwrap_or(1));
            let height = normalized_probe.height.unwrap_or(probe.height.unwrap_or(1));
            let normalized_video = NormalizedVideo {
                master_artifact_id: master.artifact_id,
                proxy_artifact_id: Some(proxy.artifact_id),
                frame_count,
                width,
                height,
                fps_num: options.profile.fps_num,
                fps_den: options.profile.fps_den,
                active_start_frame,
                active_end_frame: active_end_frame
                    .max(active_start_frame.saturating_add(1))
                    .min(frame_count),
                source_start_ms: video.start_time_ms.unwrap_or(probe.epoch_ms),
                source_end_ms: video
                    .start_time_ms
                    .unwrap_or(probe.epoch_ms)
                    .saturating_add(video.duration_ms.unwrap_or(1) as i64),
                proxy_frame_count: Some(proxy_frame_count),
            };
            Some(NormalizedAsset {
                renderer_version: options.renderer_version.clone(),
                epoch_ms: probe.epoch_ms,
                video: Some(normalized_video),
                audio,
            })
        }
        AssetKind::Audio => {
            let audio = render_audio(context, options, &probe, &base_key)?;
            artifacts.push(audio.1.clone());
            Some(NormalizedAsset {
                renderer_version: options.renderer_version.clone(),
                epoch_ms: probe.epoch_ms,
                video: None,
                audio: Some(audio.0),
            })
        }
    };
    context.progress(0.96)?;
    let asset = AssetManifest {
        id: asset_id,
        kind: probe.asset_kind,
        content_hash: identity.content_hash,
        original: probe.original,
        normalization,
    };
    let prepared = PreparedAsset { asset, artifacts };
    prepared.validate()?;
    context.progress(1.0)?;
    Ok(prepared)
}

/// Submit and synchronously await a preparation job. The job remains visible
/// through `JobRegistry::list/get` while the caller waits, so another UI call
/// can show real progress or cancel it.
pub fn prepare_asset_job(
    jobs: &JobRegistry,
    options: IngestOptions,
    input: PathBuf,
    generation: u64,
    project_id: Option<String>,
) -> Result<PreparedAsset, AppError> {
    let spec = JobSpec::new(
        "media_import",
        JobPriority::Background,
        generation,
        project_id,
    )?;
    let job = jobs.submit(spec, move |context| {
        let prepared = prepare_asset(&context, &options, &input)?;
        serde_json::to_value(prepared)
            .map_err(|_| AppError::schema("The prepared asset could not be encoded"))
    })?;
    let result = jobs.wait_blocking(&job.job_id, std::time::Duration::from_secs(30 * 60))?;
    serde_json::from_value(result.data)
        .map_err(|_| AppError::schema("The prepared asset result was malformed"))
}

pub fn make_thumbnail(
    context: &JobContext,
    options: &IngestOptions,
    asset: &AssetManifest,
    frame: u64,
) -> Result<ArtifactRecord, AppError> {
    let normalization = asset
        .normalization
        .as_ref()
        .ok_or_else(|| unavailable("The asset is not normalized"))?;
    let video = normalization
        .video
        .as_ref()
        .ok_or_else(|| unavailable("The asset has no video master"))?;
    if frame >= video.frame_count {
        return Err(invalid("Thumbnail frame is outside the normalized asset"));
    }
    let master = options.store.managed_path(&video.master_artifact_id)?;
    let output = options.store.staging_path(ArtifactKind::Thumbnail, "png")?;
    let timestamp = frame as f64 * options.profile.fps_den as f64 / options.profile.fps_num as f64;
    let mut command = Command::new(&options.ffmpeg);
    command.args([
        "-hide_banner",
        "-loglevel",
        "error",
        "-nostdin",
        "-protocol_whitelist",
        "file,pipe",
        "-ss",
    ]);
    command.arg(format!("{timestamp:.6}"));
    command.args([
        "-i",
        &master.to_string_lossy(),
        "-frames:v",
        "1",
        "-vf",
        "scale='min(320,iw)':-2:force_original_aspect_ratio=decrease",
        "-c:v",
        "png",
        "-y",
    ]);
    command.arg(&output);
    if let Err(error) = context.run_command_with_progress(command, Some(1_000)) {
        let _ = fs::remove_file(&output);
        return Err(error);
    }
    let cache_key = format!(
        "thumbnail|{}|{}|frame={frame}",
        asset.content_hash, normalization.renderer_version
    );
    let record = match options
        .store
        .put_file(&cache_key, "png", ArtifactKind::Thumbnail, &output)
    {
        Ok(record) => record,
        Err(error) => {
            let _ = fs::remove_file(&output);
            return Err(error);
        }
    };
    let _ = fs::remove_file(output);
    Ok(record)
}

pub fn make_waveform(
    options: &IngestOptions,
    asset: &AssetManifest,
) -> Result<ArtifactRecord, AppError> {
    let normalization = asset
        .normalization
        .as_ref()
        .ok_or_else(|| unavailable("The asset is not normalized"))?;
    let audio = normalization
        .audio
        .as_ref()
        .ok_or_else(|| unavailable("The asset has no normalized audio"))?;
    let bytes = options
        .store
        .read_bytes(&audio.pcm_artifact_id, 512 * 1024 * 1024)?;
    if bytes.len() % 8 != 0 {
        return Err(AppError::schema(
            "Normalized PCM has an invalid stereo sample width",
        ));
    }
    let frames = bytes.len() / 8;
    let points = frames.clamp(1, MAX_WAVEFORM_POINTS);
    let bucket = (frames / points).max(1);
    let mut peaks = Vec::with_capacity(points);
    for point in 0..points {
        let start = point.saturating_mul(bucket);
        let end = if point + 1 == points {
            frames
        } else {
            (start + bucket).min(frames)
        };
        let mut peak = 0.0f32;
        for frame in start..end {
            let offset = frame * 8;
            let left =
                f32::from_le_bytes(bytes[offset..offset + 4].try_into().expect("left sample"));
            let right = f32::from_le_bytes(
                bytes[offset + 4..offset + 8]
                    .try_into()
                    .expect("right sample"),
            );
            peak = peak.max(left.abs()).max(right.abs());
        }
        peaks.push(peak.min(1.0));
    }
    let data = serde_json::to_vec(&json!({
        "sampleRate": AUDIO_SAMPLE_RATE,
        "channels": 2,
        "sampleCount": frames,
        "peaks": peaks,
    }))
    .map_err(|_| AppError::schema("The waveform could not be encoded"))?;
    let cache_key = format!(
        "waveform|{}|{}",
        asset.content_hash, normalization.renderer_version
    );
    options
        .store
        .put_bytes(&cache_key, "json", ArtifactKind::Waveform, &data)
}

fn render_still(
    context: &JobContext,
    options: &IngestOptions,
    probe: &ProbeResult,
    base_key: &str,
) -> Result<ArtifactRecord, AppError> {
    let input = Path::new(&probe.identity.canonical_path);
    revalidate_identity(&probe.identity)?;
    let output = options
        .store
        .staging_path(ArtifactKind::StillImage, "png")?;
    let mut command = Command::new(&options.ffmpeg);
    command.args([
        "-hide_banner",
        "-loglevel",
        "error",
        "-nostdin",
        "-noautorotate",
        "-protocol_whitelist",
        "file,pipe",
        "-i",
    ]);
    command.arg(input);
    command.args([
        "-frames:v",
        "1",
        "-map",
        "0:v:0",
        "-c:v",
        "png",
        "-pix_fmt",
        "rgba",
        "-y",
    ]);
    if let Some(filter) =
        orientation_filter(probe.video().and_then(|stream| stream.rotation_degrees))?
    {
        command.args(["-vf", filter]);
    }
    command.arg(&output);
    if let Err(error) = context.run_command_with_progress(command, Some(1_000)) {
        let _ = fs::remove_file(&output);
        return Err(error);
    }
    let record = match options.store.put_file(
        &format!("{base_key}|still"),
        "png",
        ArtifactKind::StillImage,
        &output,
    ) {
        Ok(record) => record,
        Err(error) => {
            let _ = fs::remove_file(&output);
            return Err(error);
        }
    };
    let _ = fs::remove_file(output);
    Ok(record)
}

fn render_video_master(
    context: &JobContext,
    options: &IngestOptions,
    probe: &ProbeResult,
    base_key: &str,
) -> Result<ArtifactRecord, AppError> {
    revalidate_identity(&probe.identity)?;
    let output = options
        .store
        .staging_path(ArtifactKind::MasterVideo, "mp4")?;
    let video = probe.video().ok_or_else(|| {
        AppError::new(ErrorCode::MediaUnsupported, "The file has no video stream")
    })?;
    let delay_ms = video
        .start_time_ms
        .unwrap_or(probe.epoch_ms)
        .saturating_sub(probe.epoch_ms);
    let mut filters = vec!["setpts=PTS-STARTPTS".to_owned()];
    if let Some(filter) = orientation_filter(video.rotation_degrees)? {
        filters.push(filter.to_owned());
    }
    if probe.has_hdr {
        filters.push("zscale=t=linear:npl=100".to_owned());
        filters.push("tonemap=tonemap=hable:desat=0".to_owned());
        filters.push("zscale=t=bt709:m=bt709:r=tv".to_owned());
    }
    if delay_ms > 0 {
        filters.push(format!(
            "tpad=start_mode=add:start_duration={:.6}",
            delay_ms as f64 / 1000.0
        ));
    }
    filters.push(format!(
        "fps=fps={}/{}:round=near:start_time=0",
        options.profile.fps_num, options.profile.fps_den
    ));
    let mut command = Command::new(&options.ffmpeg);
    command.args([
        "-hide_banner",
        "-loglevel",
        "error",
        "-nostdin",
        "-noautorotate",
        "-protocol_whitelist",
        "file,pipe",
        "-i",
    ]);
    command.arg(&probe.identity.canonical_path);
    command.args(["-map", "0:v:0", "-vf"]);
    command.arg(filters.join(","));
    command.args([
        "-an",
        "-fps_mode",
        "cfr",
        "-c:v",
        "libx264",
        "-preset",
        "medium",
        "-crf",
        "12",
        "-pix_fmt",
        "yuv420p",
        "-movflags",
        "+faststart",
        "-progress",
        "pipe:1",
        "-nostats",
        "-y",
    ]);
    command.arg(&output);
    if let Err(error) = context.run_command_with_progress(
        command,
        Some(probe.source_end_ms.saturating_sub(probe.epoch_ms).max(1) as u64),
    ) {
        let _ = fs::remove_file(&output);
        return Err(error);
    }
    let record = match options.store.put_file(
        &format!("{base_key}|master"),
        "mp4",
        ArtifactKind::MasterVideo,
        &output,
    ) {
        Ok(record) => record,
        Err(error) => {
            let _ = fs::remove_file(&output);
            return Err(error);
        }
    };
    let _ = fs::remove_file(output);
    Ok(record)
}

fn render_video_proxy(
    context: &JobContext,
    options: &IngestOptions,
    probe: &ProbeResult,
    master: &ArtifactRecord,
    base_key: &str,
) -> Result<ArtifactRecord, AppError> {
    let input = options.store.managed_path(&master.artifact_id)?;
    let output = options
        .store
        .staging_path(ArtifactKind::ProxyVideo, "mp4")?;
    let mut command = Command::new(&options.ffmpeg);
    command.args([
        "-hide_banner",
        "-loglevel",
        "error",
        "-nostdin",
        "-protocol_whitelist",
        "file,pipe",
        "-i",
    ]);
    command.arg(&input);
    command.args([
        "-map",
        "0:v:0",
        "-vf",
        "scale=1280:720:force_original_aspect_ratio=decrease:force_divisible_by=2",
        "-fps_mode",
        "passthrough",
        "-c:v",
        "libx264",
        "-preset",
        "veryfast",
        "-crf",
        "23",
        "-pix_fmt",
        "yuv420p",
        "-g",
        "15",
        "-keyint_min",
        "15",
        "-sc_threshold",
        "0",
        "-movflags",
        "+faststart",
        "-progress",
        "pipe:1",
        "-nostats",
        "-y",
    ]);
    command.arg(&output);
    if let Err(error) = context.run_command_with_progress(
        command,
        Some(probe.source_end_ms.saturating_sub(probe.epoch_ms).max(1) as u64),
    ) {
        let _ = fs::remove_file(&output);
        return Err(error);
    }
    let record = match options.store.put_file(
        &format!("{base_key}|proxy"),
        "mp4",
        ArtifactKind::ProxyVideo,
        &output,
    ) {
        Ok(record) => record,
        Err(error) => {
            let _ = fs::remove_file(&output);
            return Err(error);
        }
    };
    let _ = fs::remove_file(output);
    let _ = probe;
    Ok(record)
}

fn render_audio(
    context: &JobContext,
    options: &IngestOptions,
    probe: &ProbeResult,
    base_key: &str,
) -> Result<(NormalizedAudio, ArtifactRecord), AppError> {
    revalidate_identity(&probe.identity)?;
    let audio_stream = probe.audio().ok_or_else(|| {
        AppError::new(ErrorCode::MediaUnsupported, "The file has no audio stream")
    })?;
    let output = options
        .store
        .staging_path(ArtifactKind::PcmAudio, "f32le")?;
    let delay_ms = audio_stream
        .start_time_ms
        .unwrap_or(probe.epoch_ms)
        .saturating_sub(probe.epoch_ms);
    let delay_samples = ((delay_ms.max(0) as u128) * AUDIO_SAMPLE_RATE as u128 / 1000) as u64;
    let mut filter = format!("aresample={AUDIO_SAMPLE_RATE}:async=0:first_pts=0,aformat=sample_fmts=fltp:sample_rates={AUDIO_SAMPLE_RATE}:channel_layouts=stereo,asetpts=PTS-STARTPTS");
    if delay_ms > 0 {
        filter.push_str(&format!(",adelay={delay_ms}:all=1"));
    }
    let mut command = Command::new(&options.ffmpeg);
    command.args([
        "-hide_banner",
        "-loglevel",
        "error",
        "-nostdin",
        "-protocol_whitelist",
        "file,pipe",
        "-i",
    ]);
    command.arg(&probe.identity.canonical_path);
    command.args(["-map", "0:a:0", "-vn", "-af"]);
    command.arg(filter);
    command.args([
        "-ar",
        "48000",
        "-ac",
        "2",
        "-c:a",
        "pcm_f32le",
        "-f",
        "f32le",
        "-progress",
        "pipe:1",
        "-nostats",
        "-y",
    ]);
    command.arg(&output);
    if let Err(error) =
        context.run_command_with_progress(command, Some(audio_stream.duration_ms.unwrap_or(1)))
    {
        let _ = fs::remove_file(&output);
        return Err(error);
    }
    let bytes = fs::metadata(&output)?.len();
    if bytes == 0 || bytes % 8 != 0 {
        return Err(AppError::new(
            ErrorCode::MediaUnsupported,
            "Audio normalization produced no valid stereo PCM",
        ));
    }
    let sample_count = bytes / 8;
    let source_start_ms = audio_stream.start_time_ms.unwrap_or(probe.epoch_ms);
    let source_end_ms =
        source_start_ms.saturating_add(audio_stream.duration_ms.unwrap_or(1) as i64);
    let duration_frames = samples_to_frames(
        sample_count,
        options.profile.fps_num,
        options.profile.fps_den,
    );
    let normalized = NormalizedAudio {
        pcm_artifact_id: String::new(),
        sample_count,
        sample_rate: AUDIO_SAMPLE_RATE,
        channels: 2,
        duration_frames,
        active_start_sample: delay_samples,
        active_end_sample: sample_count,
        source_start_ms,
        source_end_ms: source_end_ms.max(source_start_ms.saturating_add(1)),
    };
    let record = match options.store.put_file(
        &format!("{base_key}|pcm"),
        "f32le",
        ArtifactKind::PcmAudio,
        &output,
    ) {
        Ok(record) => record,
        Err(error) => {
            let _ = fs::remove_file(&output);
            return Err(error);
        }
    };
    let _ = fs::remove_file(output);
    let normalized = NormalizedAudio {
        pcm_artifact_id: record.artifact_id.clone(),
        ..normalized
    };
    context.progress(0.84)?;
    Ok((normalized, record))
}

fn probe_frame_count(ffprobe: &Path, input: &Path) -> Result<u64, AppError> {
    let output = Command::new(ffprobe)
        .args([
            "-v",
            "error",
            "-protocol_whitelist",
            "file,pipe",
            "-format_whitelist",
            super::probe::FORMAT_WHITELIST,
            "-count_frames",
            "-select_streams",
            "v:0",
            "-show_entries",
            "stream=nb_read_frames,nb_frames",
            "-of",
            "json",
        ])
        .arg(input)
        .output()
        .map_err(|_| AppError::io("ffprobe could not count normalized frames"))?;
    if !output.status.success() {
        return Err(AppError::new(
            ErrorCode::MediaUnsupported,
            "The normalized video frame count could not be verified",
        ));
    }
    let value: serde_json::Value = serde_json::from_slice(&output.stdout)
        .map_err(|_| AppError::schema("The normalized frame metadata was malformed"))?;
    let stream = value
        .get("streams")
        .and_then(|value| value.as_array())
        .and_then(|streams| streams.first())
        .ok_or_else(|| AppError::schema("The normalized video has no stream metadata"))?;
    stream
        .get("nb_read_frames")
        .and_then(serde_json::Value::as_str)
        .or_else(|| stream.get("nb_frames").and_then(serde_json::Value::as_str))
        .and_then(|value| value.parse().ok())
        .ok_or_else(|| AppError::schema("The normalized video frame count was unavailable"))
}

fn orientation_filter(rotation_degrees: Option<i16>) -> Result<Option<&'static str>, AppError> {
    let Some(rotation) = rotation_degrees else {
        return Ok(None);
    };
    match rotation.rem_euclid(360) {
        0 => Ok(None),
        90 => Ok(Some("transpose=clock")),
        180 => Ok(Some("hflip,vflip")),
        270 => Ok(Some("transpose=cclock")),
        _ => Err(AppError::new(
            ErrorCode::MediaUnsupported,
            "The media rotation metadata is not a supported quarter-turn",
        )),
    }
}

fn duration_to_frames(duration_ms: u64, fps_num: u32, fps_den: u32) -> u64 {
    ((duration_ms as u128 * fps_num as u128 + (1000u128 * fps_den as u128).saturating_sub(1))
        / (1000u128 * fps_den as u128)) as u64
}

fn samples_to_frames(sample_count: u64, fps_num: u32, fps_den: u32) -> u64 {
    ((sample_count as u128 * fps_num as u128
        + (AUDIO_SAMPLE_RATE as u128 * fps_den as u128).saturating_sub(1))
        / (AUDIO_SAMPLE_RATE as u128 * fps_den as u128)) as u64
}

fn frame_offset_frames(start_ms: i64, epoch_ms: i64, fps_num: u32, fps_den: u32) -> u64 {
    let delta = start_ms.saturating_sub(epoch_ms).max(0) as u128;
    ((delta * fps_num as u128 + (1000u128 * fps_den as u128).saturating_sub(1))
        / (1000u128 * fps_den as u128)) as u64
}

fn parse_progress(bytes: &[u8], context: &JobContext) -> Result<(), AppError> {
    let text = String::from_utf8_lossy(bytes);
    let mut out_time_ms = None;
    for line in text.lines() {
        if let Some(value) = line.strip_prefix("out_time_ms=") {
            out_time_ms = value.parse::<f64>().ok();
        }
        if line == "progress=end" {
            context.progress(0.99)?;
        }
    }
    if let Some(value) = out_time_ms {
        context.progress((value / 1_000_000.0 / 60_000.0).clamp(0.05, 0.98))?;
    }
    Ok(())
}

fn validate_binary(path: &Path, name: &str) -> Result<(), AppError> {
    if !path.is_absolute()
        || !path.is_file()
        || fs::symlink_metadata(path)?.file_type().is_symlink()
    {
        return Err(AppError::io(format!(
            "The packaged {name} binary is unavailable"
        )));
    }
    Ok(())
}

fn unavailable(message: impl Into<String>) -> AppError {
    AppError::new(ErrorCode::AssetUnavailable, message)
}

pub fn default_renderer_version() -> &'static str {
    RENDERER_VERSION
}
