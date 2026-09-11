//! Canonical, revision-pinned MP4 export.
//!
//! Export deliberately has no independent composition or timing implementation:
//! it captures the native `RenderRuntime` plan, renders every full-resolution
//! project frame through the pooled canonical renderer, and asks the typed
//! FFmpeg boundary to encode that stream.  The destination is selected by a
//! native save dialog and is never accepted as IPC input.

use crate::editor::dispatcher::CallerContext;
use crate::error::{AppError, ErrorCode};
use crate::media::ffmpeg::{
    build_export_command, parse_export_metadata, probe_json, FfmpegCommand, RenderedMediaMetadata,
};
use crate::media::jobs::{JobContext, JobPriority, JobRegistry, JobSpec, JobState, JobSummary};
use crate::media::render::{CanonicalFrameRenderer, RenderCapture};
use crate::media::render_plan::sample_at_frame;
use crate::permissions::ExportDestinationGrant;
use crate::project::model::{AspectRatio, FrameRate, ProjectDocument, TextKind};
use crate::state::AppState;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashSet;
use std::ffi::{OsStr, OsString};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use ts_rs::TS;
use uuid::Uuid;

const EXPORT_JOB_KIND: &str = "video_export";
const MAX_EXPORT_RESOLUTION: u16 = 1080;
const AUDIO_CHUNK_SAMPLES: u64 = 240_000;

/// The only output heights accepted by the desktop export surface.
///
/// The value is intentionally an integer in the wire contract (`720 | 1080`)
/// rather than a caller-controlled width or arbitrary scale.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
#[ts(tag = "action", rename_all = "snake_case")]
pub enum ExportAction {
    Start {
        #[ts(type = "SafeInteger")]
        revision: u64,
        #[ts(type = "720 | 1080")]
        resolution: u16,
        srt: bool,
    },
    /// Return the current state and, after completion, the finalized output.
    Status {
        #[serde(rename = "jobId")]
        #[ts(rename = "jobId")]
        job_id: String,
    },
    /// `get` is retained as a transport spelling for clients that use the
    /// generic jobs vocabulary; it has exactly the status semantics above.
    Get {
        #[serde(rename = "jobId")]
        #[ts(rename = "jobId")]
        job_id: String,
    },
    /// Progress is a status read, not a second progress source.
    Progress {
        #[serde(rename = "jobId")]
        #[ts(rename = "jobId")]
        job_id: String,
    },
    Cancel {
        #[serde(rename = "jobId")]
        #[ts(rename = "jobId")]
        job_id: String,
    },
    Play {
        #[serde(rename = "jobId")]
        #[ts(rename = "jobId")]
        job_id: String,
    },
    ShowFile {
        #[serde(rename = "jobId")]
        #[ts(rename = "jobId")]
        job_id: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[ts(rename_all = "camelCase")]
pub struct ExportResult {
    #[ts(type = "SafeInteger")]
    pub revision: u64,
    pub plan_hash: String,
    #[ts(type = "SafeInteger")]
    pub resolution: u16,
    #[ts(type = "SafeInteger")]
    pub width: u32,
    #[ts(type = "SafeInteger")]
    pub height: u32,
    #[ts(type = "SafeInteger")]
    pub fps_num: u32,
    #[ts(type = "SafeInteger")]
    pub fps_den: u32,
    #[ts(type = "SafeInteger")]
    pub duration_frames: u64,
    pub has_audio: bool,
    /// This path is produced by the native save dialog and is only exposed
    /// after FFmpeg and ffprobe have succeeded.
    pub destination: String,
    pub destination_identity: ExportTargetIdentity,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub srt_destination: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub srt_identity: Option<ExportTargetIdentity>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[ts(rename_all = "camelCase")]
pub struct ExportStatus {
    pub job: JobSummary,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub result: Option<ExportResult>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[ts(rename_all = "camelCase")]
pub struct ExportOpenReply {
    pub job: JobSummary,
    pub destination: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub srt_destination: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(tag = "kind", content = "data", rename_all = "snake_case")]
#[ts(tag = "kind", content = "data", rename_all = "snake_case")]
pub enum ExportReply {
    Started(ExportStatus),
    Status(ExportStatus),
    Cancelled(ExportStatus),
    Played(ExportOpenReply),
    FileShown(ExportOpenReply),
}

#[derive(Clone)]
pub struct ExportRuntime {
    jobs: JobRegistry,
}

impl Default for ExportRuntime {
    fn default() -> Self {
        Self::new()
    }
}

impl ExportRuntime {
    /// Standalone construction is useful for focused runtime tests.  AppState
    /// should use `from_registry` so export, ingest and foreground preview jobs
    /// share one bounded registry.
    pub fn new() -> Self {
        Self {
            jobs: JobRegistry::new(),
        }
    }

    pub fn from_registry(jobs: JobRegistry) -> Self {
        Self { jobs }
    }

    pub fn jobs(&self) -> JobRegistry {
        self.jobs.clone()
    }

    pub async fn handle(
        &self,
        action: ExportAction,
        caller: &CallerContext,
        state: &AppState,
    ) -> Result<ExportReply, AppError> {
        state.validate_generation(caller.generation)?;
        match action {
            ExportAction::Start {
                revision,
                resolution,
                srt,
            } => self.start(revision, resolution, srt, caller, state).await,
            ExportAction::Status { job_id }
            | ExportAction::Get { job_id }
            | ExportAction::Progress { job_id } => {
                let status = self.status(&job_id, caller, state)?;
                Ok(ExportReply::Status(status))
            }
            ExportAction::Cancel { job_id } => {
                let owned = self.owned_job(&job_id, caller, state)?;
                let job = self.jobs.cancel(&owned.job_id)?;
                let result = self.read_result_if_completed(&job)?;
                Ok(ExportReply::Cancelled(ExportStatus { job, result }))
            }
            ExportAction::Play { job_id } => {
                let open = self.open_reply(&job_id, caller, state, false)?;
                Ok(ExportReply::Played(open))
            }
            ExportAction::ShowFile { job_id } => {
                let open = self.open_reply(&job_id, caller, state, true)?;
                Ok(ExportReply::FileShown(open))
            }
        }
    }

    async fn start(
        &self,
        revision: u64,
        resolution: u16,
        srt: bool,
        caller: &CallerContext,
        state: &AppState,
    ) -> Result<ExportReply, AppError> {
        validate_resolution(resolution)?;
        require_project_bound(caller, state)?;
        let snapshot = state.snapshot_at(caller.generation)?;
        if snapshot.document.revision != revision {
            return Err(AppError::new(
                ErrorCode::RevisionConflict,
                "The requested export revision is no longer current",
            ));
        }
        let srt_contents = if srt {
            Some(projected_srt(&snapshot.document)?)
        } else {
            None
        };

        // `capture` checks the requested revision against the active document
        // and copies the exact resolver/toolchain into a job-owned value.
        let capture = state.render().capture(state, caller.generation, revision)?;
        capture.plan.validate()?;
        if capture.plan.revision != revision {
            return Err(AppError::new(
                ErrorCode::RevisionConflict,
                "The render plan revision does not match the requested export",
            ));
        }
        if capture.plan.duration_frames == 0 {
            return Err(AppError::invalid_argument(
                "Cannot export an empty timeline",
            ));
        }
        let output = choose_destination(&capture.plan, srt)?;
        let (destination_grant, srt_grant) = self
            .acquire_destination_grants(&output, caller, state)
            .await?;
        let pins = pin_master_artifacts(&capture)?;
        let dimensions = output_dimensions(capture.plan.width, capture.plan.height, resolution)?;
        let expected_duration_ms = duration_ms(capture.plan.duration_frames, capture.plan.fps())?;
        let temp_dir = state.paths().temp_dir.clone();
        let spec = JobSpec::new(
            EXPORT_JOB_KIND,
            JobPriority::Background,
            caller.generation,
            Some(capture.plan.project_id.clone()),
        )?
        .with_run_id(caller.run_id().map(ToOwned::to_owned));
        let job_output = output.clone();
        let worker_capture = capture;
        let worker_srt = srt_contents;
        let job = self.jobs.submit(spec, move |context| {
            execute_export(
                &context,
                worker_capture,
                pins,
                resolution,
                dimensions,
                expected_duration_ms,
                job_output,
                worker_srt,
                destination_grant,
                srt_grant,
                temp_dir,
            )
        })?;
        Ok(ExportReply::Started(ExportStatus { job, result: None }))
    }

    async fn acquire_destination_grants(
        &self,
        output: &ExportDestination,
        caller: &CallerContext,
        state: &AppState,
    ) -> Result<(ExportDestinationGrant, Option<ExportDestinationGrant>), AppError> {
        let scope = state.permissions().scope_for_state(state)?;
        let run_id = caller.run_id().map(ToOwned::to_owned);
        let mp4_request = state.permissions().request_export_destination(
            caller,
            scope.clone(),
            run_id.clone(),
            output.destination.clone(),
            output.destination_identity.exists,
        )?;
        let srt_request = match (&output.srt_destination, &output.srt_identity) {
            (Some(path), Some(identity)) => match state.permissions().request_export_destination(
                caller,
                scope.clone(),
                run_id.clone(),
                path.clone(),
                identity.exists,
            ) {
                Ok(request) => Some(request),
                Err(error) => {
                    let _ = state
                        .permissions()
                        .revoke_operation(&mp4_request.operation_id);
                    return Err(error);
                }
            },
            (None, None) => None,
            _ => {
                let _ = state
                    .permissions()
                    .revoke_operation(&mp4_request.operation_id);
                return Err(AppError::schema(
                    "The caption destination and identity are inconsistent",
                ));
            }
        };
        let mp4_allowed = match state
            .permissions()
            .await_decision(&mp4_request.operation_id)
            .await
        {
            Ok(allowed) => allowed,
            Err(error) => {
                if let Some(request) = srt_request.as_ref() {
                    let _ = state.permissions().revoke_operation(&request.operation_id);
                }
                return Err(error);
            }
        };
        if !mp4_allowed {
            if let Some(request) = srt_request.as_ref() {
                let _ = state.permissions().revoke_operation(&request.operation_id);
            }
            return Err(AppError::new(
                ErrorCode::PermissionDenied,
                "Writing the export destination was not approved",
            ));
        }
        let srt_allowed = if let Some(request) = srt_request.as_ref() {
            match state
                .permissions()
                .await_decision(&request.operation_id)
                .await
            {
                Ok(allowed) => allowed,
                Err(error) => {
                    let _ = state
                        .permissions()
                        .revoke_operation(&mp4_request.operation_id);
                    return Err(error);
                }
            }
        } else {
            true
        };
        if !srt_allowed {
            let _ = state
                .permissions()
                .revoke_operation(&mp4_request.operation_id);
            return Err(AppError::new(
                ErrorCode::PermissionDenied,
                "Caption sidecar overwrite was not approved",
            ));
        }
        let mp4_grant = state.permissions().consume_export_destination(
            caller,
            &scope,
            caller.run_id(),
            &mp4_request.operation_id,
            &output.destination,
            None,
        )?;
        let srt_grant = match (&output.srt_destination, srt_request.as_ref()) {
            (Some(path), Some(request)) => Some(state.permissions().consume_export_destination(
                caller,
                &scope,
                caller.run_id(),
                &request.operation_id,
                path,
                None,
            )?),
            (None, None) => None,
            _ => {
                return Err(AppError::schema(
                    "The caption destination and approval are inconsistent",
                ))
            }
        };
        Ok((mp4_grant, srt_grant))
    }

    fn status(
        &self,
        job_id: &str,
        caller: &CallerContext,
        state: &AppState,
    ) -> Result<ExportStatus, AppError> {
        let job = self.owned_job(job_id, caller, state)?;
        let result = self.read_result_if_completed(&job)?;
        Ok(ExportStatus { job, result })
    }

    fn open_reply(
        &self,
        job_id: &str,
        caller: &CallerContext,
        state: &AppState,
        reveal: bool,
    ) -> Result<ExportOpenReply, AppError> {
        let job = self.owned_job(job_id, caller, state)?;
        if job.state != JobState::Completed {
            return Err(AppError::busy(
                "The export is not finalized yet; Play and Show file are available after completion",
            ));
        }
        let result = self
            .read_result_if_completed(&job)?
            .ok_or_else(|| AppError::io("The completed export has no result"))?;
        // Revalidate the exact target identity recorded at finalization before
        // the trusted opener sees it.  A replacement at the same path is not
        // the completed export and must never be opened implicitly.
        verify_finalized_file(Path::new(&result.destination))?;
        verify_target_matches(
            Path::new(&result.destination),
            &result.destination_identity,
            "The finalized export changed after completion",
        )?;
        if let (Some(path), Some(identity)) = (
            result.srt_destination.as_deref(),
            result.srt_identity.as_ref(),
        ) {
            verify_finalized_file(Path::new(path))?;
            verify_target_matches(
                Path::new(path),
                identity,
                "The finalized caption sidecar changed after completion",
            )?;
        }
        state.open_completed_output(Path::new(&result.destination), reveal)?;
        Ok(ExportOpenReply {
            job,
            destination: result.destination,
            srt_destination: result.srt_destination,
        })
    }

    fn read_result_if_completed(&self, job: &JobSummary) -> Result<Option<ExportResult>, AppError> {
        if job.state != JobState::Completed {
            return Ok(None);
        }
        let value = self
            .jobs
            .result(&job.job_id)?
            .ok_or_else(|| AppError::io("Completed export result is unavailable"))?;
        serde_json::from_value(value.data)
            .map(Some)
            .map_err(|_| AppError::schema("Completed export result is malformed"))
    }

    fn owned_job(
        &self,
        job_id: &str,
        caller: &CallerContext,
        state: &AppState,
    ) -> Result<JobSummary, AppError> {
        require_project_bound(caller, state)?;
        let job = self.jobs.get(job_id)?;
        if job.kind != EXPORT_JOB_KIND {
            return Err(AppError::new(
                ErrorCode::PermissionDenied,
                "The requested job is not an export owned by this runtime",
            ));
        }
        if job.generation != caller.generation
            || job.project_id.as_deref() != state.current_project_id().as_deref()
        {
            return Err(AppError::stale_session(
                "The export belongs to another project generation",
            ));
        }
        Ok(job)
    }
}
fn require_project_bound(caller: &CallerContext, state: &AppState) -> Result<(), AppError> {
    let project_id = state
        .current_project_id()
        .ok_or_else(|| AppError::invalid_argument("No project is open"))?;
    if caller.project_id.as_deref() != Some(project_id.as_str()) {
        return Err(AppError::stale_session(
            "The export caller is not bound to the open project",
        ));
    }
    Ok(())
}

#[derive(Clone)]
struct ExportDestination {
    destination: PathBuf,
    srt_destination: Option<PathBuf>,
    destination_identity: TargetIdentity,
    srt_identity: Option<TargetIdentity>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[ts(rename_all = "camelCase")]
pub struct ExportTargetIdentity {
    pub exists: bool,
    #[ts(type = "SafeInteger")]
    pub size: u64,
    pub token: String,
}

type TargetIdentity = ExportTargetIdentity;

fn choose_destination(
    plan: &crate::media::render_plan::RenderPlan,
    srt: bool,
) -> Result<ExportDestination, AppError> {
    let _ = AspectRatio::from_dimensions(plan.width, plan.height)
        .ok_or_else(|| AppError::schema("The render plan has an unsupported aspect ratio"))?;
    let default_name = format!("export-{}x{}.mp4", plan.width, plan.height);
    let chosen = rfd::FileDialog::new()
        .set_title("Export Cutterhoochee video")
        .set_file_name(default_name)
        .add_filter("MP4 video", &["mp4"])
        .save_file()
        .ok_or_else(|| AppError::io("Export destination selection was cancelled"))?;
    let destination = normalize_destination(chosen)?;
    let srt_destination = srt.then(|| destination.with_extension("srt"));
    let destination_identity = target_identity(&destination)?;
    let srt_identity = srt_destination
        .as_deref()
        .map(target_identity)
        .transpose()?;
    Ok(ExportDestination {
        destination,
        srt_destination,
        destination_identity,
        srt_identity,
    })
}

fn normalize_destination(mut path: PathBuf) -> Result<PathBuf, AppError> {
    if !path.is_absolute() {
        return Err(AppError::new(
            ErrorCode::PermissionDenied,
            "Export destinations must be absolute native paths",
        ));
    }
    let extension = path.extension().and_then(|value| value.to_str());
    match extension {
        None => {
            path.set_extension("mp4");
        }
        Some(value) if value.eq_ignore_ascii_case("mp4") => {}
        Some(_) => {
            return Err(AppError::invalid_argument(
                "Export destination must use the .mp4 extension",
            ));
        }
    }
    let file_name = path
        .file_name()
        .and_then(|value| value.to_str())
        .filter(|value| !value.is_empty() && !value.contains('\0') && !value.contains(['\r', '\n']))
        .ok_or_else(|| AppError::invalid_argument("Export destination filename is invalid"))?;
    let parent = path
        .parent()
        .ok_or_else(|| AppError::invalid_argument("Export destination has no parent directory"))?;
    let parent_metadata = fs::symlink_metadata(parent)
        .map_err(|_| AppError::io("Export destination directory is unavailable"))?;
    if parent_metadata.file_type().is_symlink() || !parent_metadata.is_dir() {
        return Err(AppError::new(
            ErrorCode::PermissionDenied,
            "Export destination directory must be a real directory",
        ));
    }
    let parent = fs::canonicalize(parent)
        .map_err(|_| AppError::io("Export destination directory could not be resolved"))?;
    Ok(parent.join(file_name))
}

fn target_identity(path: &Path) -> Result<TargetIdentity, AppError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() {
                return Err(AppError::new(
                    ErrorCode::PermissionDenied,
                    "Symlinked export destinations cannot be overwritten",
                ));
            }
            if !metadata.is_file() {
                return Err(AppError::invalid_argument(
                    "Export destination must be a regular file",
                ));
            }
            Ok(TargetIdentity {
                exists: true,
                size: metadata.len(),
                token: metadata_token(&metadata),
            })
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(TargetIdentity {
            exists: false,
            size: 0,
            token: "missing".to_owned(),
        }),
        Err(_) => Err(AppError::io("Export destination metadata is unavailable")),
    }
}

fn metadata_token(metadata: &fs::Metadata) -> String {
    #[cfg(unix)]
    {
        return format!(
            "unix:{}:{}:{}:{}:{}",
            metadata.dev(),
            metadata.ino(),
            metadata.len(),
            metadata.mtime(),
            metadata.mtime_nsec()
        );
    }
    #[cfg(not(unix))]
    {
        let modified = metadata
            .modified()
            .ok()
            .and_then(|value| value.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|value| value.as_nanos())
            .unwrap_or_default();
        format!("file:{}:{modified}", metadata.len())
    }
}

fn verify_finalized_file(path: &Path) -> Result<(), AppError> {
    let metadata = fs::symlink_metadata(path).map_err(|_| {
        AppError::new(
            ErrorCode::AssetUnavailable,
            "The finalized export is unavailable",
        )
    })?;
    if metadata.file_type().is_symlink() || !metadata.is_file() || metadata.len() == 0 {
        return Err(AppError::new(
            ErrorCode::AssetUnavailable,
            "The finalized export is not a regular file",
        ));
    }
    Ok(())
}

fn pin_master_artifacts(capture: &RenderCapture) -> Result<Vec<File>, AppError> {
    let mut ids = HashSet::new();
    for layer in &capture.plan.layers {
        for segment in &layer.segments {
            ids.insert(segment.artifact_id.clone());
        }
        for overlay in &layer.text_overlays {
            ids.insert(overlay.raster_artifact_id.clone());
        }
    }
    for segment in &capture.plan.audio.segments {
        ids.insert(segment.artifact_id.clone());
    }
    let mut pins = Vec::with_capacity(ids.len());
    for artifact_id in ids {
        let path = capture.artifacts.managed_path(&artifact_id)?;
        let file = File::open(path).map_err(|_| {
            AppError::new(
                ErrorCode::AssetUnavailable,
                "A referenced render artifact is unavailable",
            )
        })?;
        pins.push(file);
    }
    Ok(pins)
}

fn execute_export(
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
    let plan = capture.plan;
    let artifacts = capture.artifacts;
    let toolchain = capture.toolchain;
    let audio_path = temporary_path(&temp_dir, context.job_id(), "f32le")?;
    let mut audio_temp = TemporaryFile::new(audio_path)?;
    render_audio_to_file(context, &plan, artifacts.as_ref(), &mut audio_temp.file)?;

    let output_temp_path = temporary_sibling(&output.destination, "mp4")?;
    let mut output_temp = TemporaryFile::new(output_temp_path)?;
    let mut command = build_export_command(
        &toolchain,
        plan.width,
        plan.height,
        plan.fps(),
        &audio_temp.path,
        &output_temp.path,
        None,
    )?;
    insert_output_scale(&mut command, output_width, output_height)?;
    let mut process = std::process::Command::new(&command.executable);
    process.args(&command.argv);
    process.current_dir(
        output
            .destination
            .parent()
            .ok_or_else(|| AppError::io("Export destination parent disappeared"))?,
    );
    let renderer = Arc::new(CanonicalFrameRenderer::new(
        plan.clone(),
        artifacts.clone(),
        toolchain.clone(),
    )?);
    let producer_plan = plan.clone();
    let producer_renderer = renderer;
    let encoded = context.run_command_with_stdin_progress(
        process,
        Some(expected_duration_ms),
        move |stdin| {
            let mut rgba = Vec::new();
            for frame in 0..producer_plan.duration_frames {
                producer_renderer.render_into(frame, &mut rgba)?;
                stdin.write_all(&rgba).map_err(|_| {
                    AppError::new(
                        ErrorCode::JobCancelled,
                        "The export frame stream was cancelled",
                    )
                })?;
            }
            Ok(())
        },
    )?;
    output_temp.file.sync_all()?;
    context.check_cancelled()?;
    let metadata = probe_json(&toolchain, &output_temp.path)?;
    let expected = RenderedMediaMetadata {
        width: output_width,
        height: output_height,
        fps_num: plan.fps_num,
        fps_den: plan.fps_den,
        duration_frames: plan.duration_frames,
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
        revision: plan.revision,
        plan_hash: plan.plan_hash,
        resolution,
        width: output_width,
        height: output_height,
        fps_num: plan.fps_num,
        fps_den: plan.fps_den,
        duration_frames: plan.duration_frames,
        has_audio: true,
        destination: output.destination.to_string_lossy().into_owned(),
        destination_identity,
        srt_destination: output
            .srt_destination
            .as_ref()
            .map(|path| path.to_string_lossy().into_owned()),
        srt_identity,
    };
    let _ = encoded;
    serde_json::to_value(result)
        .map_err(|_| AppError::schema("The export result could not be encoded"))
}

fn render_audio_to_file(
    context: &JobContext,
    plan: &crate::media::render_plan::RenderPlan,
    artifacts: &dyn crate::media::render_plan::ArtifactResolver,
    file: &mut File,
) -> Result<(), AppError> {
    let mut start = 0u64;
    while start < plan.audio.total_samples {
        context.check_cancelled()?;
        let count = AUDIO_CHUNK_SAMPLES.min(plan.audio.total_samples - start);
        let pcm = crate::media::render::render_audio_window(plan, start, count, artifacts)?;
        for sample in pcm {
            file.write_all(&sample.to_le_bytes())?;
        }
        start = start
            .checked_add(count)
            .ok_or_else(|| AppError::invalid_argument("Audio export sample position overflowed"))?;
    }
    file.sync_all()?;
    Ok(())
}

fn insert_output_scale(
    command: &mut FfmpegCommand,
    width: u32,
    height: u32,
) -> Result<(), AppError> {
    if width == 0 || height == 0 {
        return Err(AppError::invalid_argument(
            "Scaled export dimensions must be positive",
        ));
    }
    let index = command
        .argv
        .iter()
        .rposition(|argument| argument == OsStr::new("-f"))
        .ok_or_else(|| AppError::schema("The typed export command has no output format"))?;
    command.argv.splice(
        index..index,
        [
            OsString::from("-vf"),
            OsString::from(format!("scale={width}:{height}:flags=bicubic")),
        ],
    );
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

fn commit_outputs(
    mp4_temp: &mut TemporaryFile,
    srt_temp: &mut Option<TemporaryFile>,
    mp4_grant: &ExportDestinationGrant,
    srt_grant: Option<&ExportDestinationGrant>,
    destination: &Path,
    srt_destination: Option<&Path>,
) -> Result<(), AppError> {
    if destination != mp4_grant.destination() {
        return Err(AppError::new(
            ErrorCode::PermissionDenied,
            "The MP4 destination no longer matches its approval",
        ));
    }
    if srt_temp.is_some() != srt_grant.is_some() || srt_temp.is_some() != srt_destination.is_some()
    {
        return Err(AppError::schema(
            "The SRT temporary output, destination, and approval are inconsistent",
        ));
    }
    if let (Some(path), Some(grant)) = (srt_destination, srt_grant) {
        if path != grant.destination() {
            return Err(AppError::new(
                ErrorCode::PermissionDenied,
                "The SRT destination no longer matches its approval",
            ));
        }
    }

    // Trust retains an app-owned backup while both exact grants are used.  It
    // also represents an originally absent target, allowing rollback to
    // remove a newly installed file only when its identity is unchanged.
    let mut mp4_backup = mp4_grant.backup_existing()?;
    let mut srt_backup = match srt_grant {
        Some(grant) => Some(grant.backup_existing()?),
        None => None,
    };
    let mut mp4_outcome = None;
    let mut srt_outcome = None;
    let install_result = (|| {
        if let (Some(temp), Some(grant)) = (srt_temp.as_ref(), srt_grant) {
            srt_outcome = Some(grant.install_with_state(&temp.path)?);
        }
        mp4_outcome = Some(mp4_grant.install_with_state(&mp4_temp.path)?);
        Ok::<(), AppError>(())
    })();
    if let Err(error) = install_result {
        let mut rollback_error = None;
        if let Some(outcome) = srt_outcome.as_ref() {
            if let Err(error) = srt_backup
                .take()
                .ok_or_else(|| AppError::io("The SRT export backup is unavailable"))
                .and_then(|backup| backup.restore_if_unchanged(outcome))
            {
                rollback_error = Some(error);
            }
        }
        if let Some(outcome) = mp4_outcome.as_ref() {
            if let Err(error) = mp4_backup.restore_if_unchanged(outcome) {
                rollback_error = Some(error);
            }
        }
        if let Some(rollback_error) = rollback_error {
            return Err(AppError::io(format!(
                "Export installation failed and rollback is incomplete: {}; {}",
                error.message, rollback_error.message
            )));
        }
        if mp4_outcome.is_none() || (srt_grant.is_some() && srt_outcome.is_none()) {
            return Err(AppError::io(format!(
                "Export installation failed before every target state could be proven; recovery backup retained: {}",
                error.message
            )));
        }
        return Err(error);
    }
    mp4_backup.discard()?;
    if let Some(backup) = srt_backup.take() {
        backup.discard()?;
    }
    mp4_temp.keep = true;
    if let Some(temp) = srt_temp.as_mut() {
        temp.keep = true;
    }
    Ok(())
}

fn verify_target_matches(
    path: &Path,
    expected: &TargetIdentity,
    message: &str,
) -> Result<(), AppError> {
    let current = target_identity(path)?;
    if &current != expected {
        return Err(AppError::new(ErrorCode::PermissionDenied, message));
    }
    Ok(())
}
struct TemporaryFile {
    path: PathBuf,
    file: File,
    keep: bool,
}

impl TemporaryFile {
    fn new(path: PathBuf) -> Result<Self, AppError> {
        let file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .read(true)
            .open(&path)
            .map_err(|_| AppError::io("The export temporary file could not be created"))?;
        Ok(Self {
            path,
            file,
            keep: false,
        })
    }
}

impl Drop for TemporaryFile {
    fn drop(&mut self) {
        if !self.keep {
            let _ = fs::remove_file(&self.path);
        }
    }
}

fn temporary_path(directory: &Path, job_id: &str, extension: &str) -> Result<PathBuf, AppError> {
    if !directory.is_absolute() {
        return Err(AppError::new(
            ErrorCode::PermissionDenied,
            "Export temporary storage must be an absolute app-owned directory",
        ));
    }
    fs::create_dir_all(directory)?;
    if job_id.is_empty() || extension.is_empty() || extension.contains(['/', '\\']) {
        return Err(AppError::invalid_argument(
            "The export temporary name is invalid",
        ));
    }
    Ok(directory.join(format!(".cutterhoochee-export-{job_id}.{extension}")))
}
fn temporary_sibling(destination: &Path, extension: &str) -> Result<PathBuf, AppError> {
    let parent = destination
        .parent()
        .ok_or_else(|| AppError::io("Export destination has no parent"))?;
    let stem = destination
        .file_name()
        .and_then(|value| value.to_str())
        .ok_or_else(|| AppError::io("Export destination filename is not UTF-8"))?;
    let nonce = Uuid::new_v4().simple().to_string();
    Ok(parent.join(format!(".{stem}.cutterhoochee-{nonce}.{extension}")))
}

fn output_dimensions(width: u32, height: u32, resolution: u16) -> Result<(u32, u32), AppError> {
    validate_resolution(resolution)?;
    let aspect = AspectRatio::from_dimensions(width, height)
        .ok_or_else(|| AppError::schema("The render plan aspect ratio is unsupported"))?;
    let value = u32::from(resolution);
    Ok(match aspect {
        AspectRatio::Landscape => (value * 16 / 9, value),
        AspectRatio::Portrait => (value, value * 16 / 9),
        AspectRatio::Square => (value, value),
    })
}

fn validate_resolution(value: u16) -> Result<(), AppError> {
    if matches!(value, 720 | MAX_EXPORT_RESOLUTION) {
        Ok(())
    } else {
        Err(AppError::invalid_argument(
            "Export resolution must be 720 or 1080",
        ))
    }
}

fn duration_ms(frames: u64, fps: FrameRate) -> Result<u64, AppError> {
    fps.validate()?;
    let samples = sample_at_frame(frames, fps)?;
    samples
        .checked_mul(1_000)
        .and_then(|value| value.checked_div(crate::project::model::AUDIO_SAMPLE_RATE as u64))
        .ok_or_else(|| AppError::invalid_argument("Export duration overflows"))
}

fn projected_srt(document: &ProjectDocument) -> Result<String, AppError> {
    let mut cues = Vec::<(u64, u64, String, String)>::new();
    for item in document
        .text_items
        .iter()
        .filter(|item| item.kind == TextKind::Caption)
    {
        if let Some(owner_clip_id) = item.owner_clip_id.as_deref() {
            let clip = document
                .clips
                .iter()
                .find(|clip| clip.id == owner_clip_id)
                .ok_or_else(|| AppError::schema("Caption owner clip is unavailable"))?;
            if let Some(projection) = item.project_on_clip(clip)? {
                cues.push((
                    projection.timeline_start_frame,
                    projection
                        .timeline_start_frame
                        .checked_add(projection.duration_frames)
                        .ok_or_else(|| AppError::schema("Caption interval overflows"))?,
                    item.id.clone(),
                    item.text.clone(),
                ));
            }
        } else if let Some(interval) = item.timeline_interval() {
            let interval = interval?;
            cues.push((
                interval.start_frame,
                interval.end_frame(),
                item.id.clone(),
                item.text.clone(),
            ));
        }
    }
    cues.sort_by(|left, right| (left.0, left.2.as_str()).cmp(&(right.0, right.2.as_str())));
    let fps = document.profile.fps();
    let mut output = String::new();
    for (index, (start, end, _, text)) in cues.iter().enumerate() {
        let start_ms = frame_time_ms(*start, fps, false)?;
        let end_ms = frame_time_ms(*end, fps, true)?;
        if end_ms <= start_ms {
            return Err(AppError::schema(
                "Caption interval is shorter than one millisecond",
            ));
        }
        output.push_str(&(index + 1).to_string());
        output.push('\n');
        output.push_str(&format_srt_timestamp(start_ms));
        output.push_str(" --> ");
        output.push_str(&format_srt_timestamp(end_ms));
        output.push('\n');
        output.push_str(&text.replace('\r', ""));
        output.push_str("\n\n");
    }
    Ok(output)
}

fn frame_time_ms(frame: u64, fps: FrameRate, ceil: bool) -> Result<u64, AppError> {
    fps.validate()?;
    let numerator = (frame as u128)
        .checked_mul(fps.den as u128)
        .and_then(|value| value.checked_mul(1_000))
        .ok_or_else(|| AppError::invalid_argument("Caption time overflows"))?;
    let denominator = fps.num as u128;
    let value = if ceil {
        numerator
            .checked_add(denominator - 1)
            .and_then(|value| value.checked_div(denominator))
    } else {
        numerator.checked_div(denominator)
    }
    .ok_or_else(|| AppError::invalid_argument("Caption time overflows"))?;
    u64::try_from(value).map_err(|_| AppError::invalid_argument("Caption time exceeds safe range"))
}

fn format_srt_timestamp(milliseconds: u64) -> String {
    let hours = milliseconds / 3_600_000;
    let minutes = (milliseconds / 60_000) % 60;
    let seconds = (milliseconds / 1_000) % 60;
    let millis = milliseconds % 1_000;
    format!("{hours:02}:{minutes:02}:{seconds:02},{millis:03}")
}

#[cfg(unix)]
use std::os::unix::fs::MetadataExt;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_only_supported_output_heights() {
        assert!(validate_resolution(720).is_ok());
        assert!(validate_resolution(1080).is_ok());
        assert_eq!(
            validate_resolution(2160).unwrap_err().code,
            ErrorCode::InvalidArgument
        );
    }

    #[test]
    fn output_dimensions_follow_project_aspect() {
        assert_eq!(output_dimensions(1920, 1080, 720).unwrap(), (1280, 720));
        assert_eq!(output_dimensions(1080, 1920, 1080).unwrap(), (1080, 1920));
        assert_eq!(output_dimensions(1080, 1080, 720).unwrap(), (720, 720));
    }

    #[test]
    fn frame_time_uses_covering_end_rounding() {
        let fps = FrameRate::FPS_24;
        assert_eq!(frame_time_ms(1, fps, false).unwrap(), 41);
        assert_eq!(frame_time_ms(1, fps, true).unwrap(), 42);
    }

    #[test]
    fn temporary_sibling_is_hidden_and_unique() {
        let destination = Path::new("/tmp/video.mp4");
        let first = temporary_sibling(destination, "mp4").unwrap();
        let second = temporary_sibling(destination, "mp4").unwrap();
        assert_ne!(first, second);
        assert!(first
            .file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with(".video.mp4.cutterhoochee-"));
    }
}
