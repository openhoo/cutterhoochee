use crate::editor::dispatcher::CallerContext;
use crate::error::{AppError, ErrorCode};
use crate::media::jobs::{JobContext, JobPriority, JobRegistry, JobSpec, JobState, JobSummary};
use crate::permissions::ExportDestinationGrant;
use crate::state::AppState;

use super::encode::execute_export;
use super::output;
use super::subtitle::projected_srt;
use super::{
    ExportAction, ExportOpenReply, ExportReply, ExportResult, ExportRuntime, ExportStatus,
    EXPORT_JOB_KIND,
};

impl Default for ExportRuntime {
    fn default() -> Self {
        Self::new()
    }
}

impl ExportRuntime {
    /// Standalone construction is useful for focused runtime tests. AppState
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
        output::validate_resolution(resolution)?;
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
        let output = output::choose_destination(&capture.plan, srt)?;
        let (destination_grant, srt_grant) = self
            .acquire_destination_grants(&output, caller, state)
            .await?;
        let pins = output::pin_master_artifacts(&capture)?;
        let dimensions =
            output::output_dimensions(capture.plan.width, capture.plan.height, resolution)?;
        let expected_duration_ms =
            output::duration_ms(capture.plan.duration_frames, capture.plan.fps())?;
        let temp_dir = state.paths().temp_dir.clone();
        let spec = JobSpec::new(
            EXPORT_JOB_KIND,
            JobPriority::Background,
            caller.generation,
            Some(capture.plan.project_id.clone()),
        )?
        .with_run_id(caller.run_id().map(ToOwned::to_owned))
        .with_activity_id(caller.activity_id.clone());
        let job_output = output.clone();
        let worker_capture = capture;
        let worker_srt = srt_contents;
        let job = self.jobs.submit(spec, move |context: JobContext| {
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
        output: &output::ExportDestination,
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
        // the trusted opener sees it. A replacement at the same path is not
        // the completed export and must never be opened implicitly.
        output::verify_finalized_file(std::path::Path::new(&result.destination))?;
        output::verify_target_matches(
            std::path::Path::new(&result.destination),
            &result.destination_identity,
            "The finalized export changed after completion",
        )?;
        if let (Some(path), Some(identity)) = (
            result.srt_destination.as_deref(),
            result.srt_identity.as_ref(),
        ) {
            output::verify_finalized_file(std::path::Path::new(path))?;
            output::verify_target_matches(
                std::path::Path::new(path),
                identity,
                "The finalized caption sidecar changed after completion",
            )?;
        }
        state.open_completed_output(std::path::Path::new(&result.destination), reveal)?;
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
