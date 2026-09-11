use crate::error::{AppError, ErrorCode};
use crate::media::ffmpeg::{
    build_export_command, parse_export_metadata, probe_json, ExportVideoSettings,
    RenderedMediaMetadata,
};
use crate::media::jobs::JobContext;
use crate::media::render::audio::render_audio_window;
use crate::media::render::frame::CanonicalFrameRenderer;
use crate::media::render::RenderCapture;
use crate::media::render_plan::ArtifactResolver;
use crate::permissions::ExportDestinationGrant;
use serde_json::Value;
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::PathBuf;

use super::output::{
    commit_outputs, target_identity, temporary_path, temporary_sibling, verify_finalized_file,
    ExportDestination, TemporaryFile,
};
use super::{ExportResult, AUDIO_CHUNK_SAMPLES, AUDIO_PCM_BUFFER_BYTES};

pub(super) fn execute_export(
    context: &JobContext,
    capture: RenderCapture,
    _pins: Vec<File>,
    resolution: u16,
    (output_width, output_height): (u32, u32),
    expected_duration_ms: u64,
    output: ExportDestination,
    srt_contents: Option<String>,
    destination_grant: ExportDestinationGrant,
    srt_grant: Option<ExportDestinationGrant>,
    temp_dir: PathBuf,
) -> Result<Value, AppError> {
    let RenderCapture {
        plan,
        artifacts,
        toolchain,
    } = capture;
    let revision = plan.revision;
    let plan_hash = plan.plan_hash.clone();
    let source_width = plan.width;
    let source_height = plan.height;
    let fps_num = plan.fps_num;
    let fps_den = plan.fps_den;
    let duration_frames = plan.duration_frames;

    let audio_path = temporary_path(&temp_dir, context.job_id(), "f32le")?;
    let mut audio_temp = TemporaryFile::new(audio_path)?;
    render_audio_to_file(context, &plan, artifacts.as_ref(), &mut audio_temp.file)?;

    let output_temp_path = temporary_sibling(&output.destination, "mp4")?;
    let mut output_temp = TemporaryFile::new(output_temp_path)?;
    let settings = ExportVideoSettings {
        source_width,
        source_height,
        output_width,
        output_height,
        fps: plan.fps(),
    };
    let command = build_export_command(&toolchain, settings, &audio_temp.path, &output_temp.path)?;
    let mut process = std::process::Command::new(&command.executable);
    process.args(&command.argv);
    process.current_dir(
        output
            .destination
            .parent()
            .ok_or_else(|| AppError::io("Export destination parent disappeared"))?,
    );

    // The renderer owns the moved immutable plan. The producer reads its frame
    // count through that owner, avoiding a second plan and Arc clone merely to
    // satisfy the stdin worker's move boundary.
    let renderer = CanonicalFrameRenderer::new(plan, artifacts, toolchain.clone())?;
    context.run_command_with_stdin_progress(process, Some(expected_duration_ms), move |stdin| {
        let mut rgba = Vec::new();
        for frame in 0..renderer.plan().duration_frames {
            renderer.render_into(frame, &mut rgba)?;
            stdin.write_all(&rgba).map_err(|_| {
                AppError::new(
                    ErrorCode::JobCancelled,
                    "The export frame stream was cancelled",
                )
            })?;
        }
        Ok(())
    })?;
    output_temp.file.sync_all()?;
    context.check_cancelled()?;

    let metadata = probe_json(&toolchain, &output_temp.path)?;
    let expected = RenderedMediaMetadata {
        width: output_width,
        height: output_height,
        fps_num,
        fps_den,
        duration_frames,
        // The audio input is always present, including intentional silence.
        has_audio: true,
    };
    parse_export_metadata(&metadata, &expected)?;
    validate_export_streams(&metadata, &expected, expected_duration_ms)?;

    let has_srt = srt_contents.is_some();
    let mut srt_temp = srt_contents
        .map(|contents| {
            let path = temporary_sibling(
                output
                    .srt_destination
                    .as_deref()
                    .ok_or_else(|| AppError::io("SRT destination is unavailable"))?,
                "srt",
            )?;
            let mut temporary = TemporaryFile::new(path)?;
            temporary.file.write_all(contents.as_bytes())?;
            temporary.file.sync_all()?;
            Ok::<TemporaryFile, AppError>(temporary)
        })
        .transpose()?;
    if srt_temp.is_some() != srt_grant.is_some() {
        return Err(AppError::schema(
            "The SRT temporary output and destination grant are inconsistent",
        ));
    }
    commit_outputs(
        &mut output_temp,
        &mut srt_temp,
        &destination_grant,
        srt_grant.as_ref(),
        &output.destination,
        output.srt_destination.as_deref(),
    )?;
    verify_finalized_file(&output.destination)?;
    if let Some(path) = output.srt_destination.as_deref().filter(|_| has_srt) {
        verify_finalized_file(path)?;
    }
    let destination_identity = target_identity(&output.destination)?;
    let srt_identity = output
        .srt_destination
        .as_deref()
        .map(target_identity)
        .transpose()?;
    let result = ExportResult {
        revision,
        plan_hash,
        resolution,
        width: output_width,
        height: output_height,
        fps_num,
        fps_den,
        duration_frames,
        has_audio: true,
        destination: output.destination.to_string_lossy().into_owned(),
        destination_identity,
        srt_destination: output
            .srt_destination
            .as_ref()
            .map(|path| path.to_string_lossy().into_owned()),
        srt_identity,
    };
    serde_json::to_value(result)
        .map_err(|_| AppError::schema("The export result could not be encoded"))
}

fn render_audio_to_file(
    context: &JobContext,
    plan: &crate::media::render_plan::RenderPlan,
    artifacts: &dyn ArtifactResolver,
    file: &mut File,
) -> Result<(), AppError> {
    let mut writer = BufWriter::with_capacity(AUDIO_PCM_BUFFER_BYTES, &mut *file);
    let mut start = 0u64;
    while start < plan.audio.total_samples {
        context.check_cancelled()?;
        let count = AUDIO_CHUNK_SAMPLES.min(plan.audio.total_samples - start);
        let pcm = render_audio_window(plan, start, count, artifacts)?;
        for sample in pcm {
            writer
                .write_all(&sample.to_le_bytes())
                .map_err(|_| AppError::io("The temporary PCM file could not be written"))?;
        }
        start = start
            .checked_add(count)
            .ok_or_else(|| AppError::invalid_argument("Audio export sample position overflowed"))?;
    }
    writer
        .flush()
        .map_err(|_| AppError::io("The temporary PCM file could not be flushed"))?;
    drop(writer);
    file.sync_all()
        .map_err(|_| AppError::io("The temporary PCM file could not be synchronized"))?;
    Ok(())
}

fn validate_export_streams(
    value: &Value,
    expected: &RenderedMediaMetadata,
    expected_duration_ms: u64,
) -> Result<(), AppError> {
    let streams = value
        .get("streams")
        .and_then(Value::as_array)
        .ok_or_else(|| {
            AppError::new(
                ErrorCode::MediaUnsupported,
                "Export metadata has no streams",
            )
        })?;
    let video = streams
        .iter()
        .find(|stream| stream.get("codec_type").and_then(Value::as_str) == Some("video"))
        .ok_or_else(|| AppError::new(ErrorCode::MediaUnsupported, "Export has no video stream"))?;
    if video.get("codec_name").and_then(Value::as_str) != Some("h264") {
        return Err(AppError::new(
            ErrorCode::MediaUnsupported,
            "Export video is not H.264",
        ));
    }
    let audio = streams
        .iter()
        .find(|stream| stream.get("codec_type").and_then(Value::as_str) == Some("audio"))
        .ok_or_else(|| AppError::new(ErrorCode::MediaUnsupported, "Export has no audio stream"))?;
    if audio.get("codec_name").and_then(Value::as_str) != Some("aac") {
        return Err(AppError::new(
            ErrorCode::MediaUnsupported,
            "Export audio is not AAC",
        ));
    }
    if audio.get("sample_rate").and_then(Value::as_str) != Some("48000")
        || audio.get("channels").and_then(Value::as_u64) != Some(2)
    {
        return Err(AppError::new(
            ErrorCode::MediaUnsupported,
            "Export audio is not 48 kHz stereo",
        ));
    }
    let actual_duration = video
        .get("duration")
        .and_then(Value::as_str)
        .or_else(|| {
            value
                .get("format")
                .and_then(|format| format.get("duration"))
                .and_then(Value::as_str)
        })
        .and_then(|duration| duration.parse::<f64>().ok())
        .ok_or_else(|| {
            AppError::new(
                ErrorCode::MediaUnsupported,
                "Export duration is unavailable",
            )
        })?;
    if !actual_duration.is_finite() || actual_duration <= 0.0 {
        return Err(AppError::new(
            ErrorCode::MediaUnsupported,
            "Export duration is invalid",
        ));
    }
    let expected_seconds = expected_duration_ms as f64 / 1_000.0;
    let frame_seconds = expected.fps_den as f64 / expected.fps_num as f64;
    if (actual_duration - expected_seconds).abs() > frame_seconds {
        return Err(AppError::new(
            ErrorCode::MediaUnsupported,
            "Export duration does not match the immutable render plan",
        ));
    }
    Ok(())
}
