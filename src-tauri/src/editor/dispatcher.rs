use crate::activity::{
    changes_from_delta, describe_request, extract_job_ids, ActivityHandle, ActivityPatch,
    ActivityPhase,
};
use crate::editor::operations::{apply_batch_in_place, EditOp};
use crate::error::{AppError, ErrorCode};
use crate::ipc::{EditorReply, EditorRequest, TimelineSelection};
use crate::media::EvidenceAction;
use crate::project::store::HistoryAction;
use crate::state::AppState;
use serde_json::Value;

#[derive(Debug, Clone)]
pub enum CallerKind {
    HumanWindow { label: String },
    AgentSidecar { connection_id: String },
}

/// Native-only caller context. No field is deserialized from an editor JSON
/// request; the command handler and sidecar supervisor construct it from the
/// trusted window/connection they own.
#[derive(Debug, Clone)]
pub struct CallerContext {
    pub(crate) kind: CallerKind,
    pub(crate) generation: u64,
    pub(crate) project_id: Option<String>,
    pub(crate) run_id: Option<String>,
    pub(crate) tool_call_id: Option<String>,
    pub(crate) activity_id: Option<String>,
}
impl CallerContext {
    pub(crate) fn human_window(label: String, generation: u64, project_id: Option<String>) -> Self {
        Self {
            kind: CallerKind::HumanWindow { label },
            generation,
            project_id,
            run_id: None,
            tool_call_id: None,
            activity_id: None,
        }
    }

    pub(crate) fn agent_sidecar(
        connection_id: String,
        generation: u64,
        project_id: Option<String>,
    ) -> Self {
        Self {
            kind: CallerKind::AgentSidecar { connection_id },
            generation,
            project_id,
            run_id: None,
            tool_call_id: None,
            activity_id: None,
        }
    }

    pub(crate) fn with_run_id(mut self, run_id: Option<String>) -> Self {
        self.run_id = run_id;
        self
    }

    pub(crate) fn with_tool_call_id(mut self, tool_call_id: Option<String>) -> Self {
        self.tool_call_id = tool_call_id;
        self
    }

    pub(crate) fn run_id(&self) -> Option<&str> {
        self.run_id.as_deref()
    }
    pub(crate) fn connection_id(&self) -> Option<&str> {
        match &self.kind {
            CallerKind::HumanWindow { .. } => None,
            CallerKind::AgentSidecar { connection_id } => Some(connection_id.as_str()),
        }
    }

    fn is_project_bound(&self, state: &AppState) -> bool {
        self.project_id == state.current_project_id()
    }
}

/// Single native dispatch boundary for UI and Pi editing requests. Lifecycle
/// requests intentionally have no path field: both callers invoke the same
/// native chooser, while all editing/history requests remain bound to the
/// current project and generation.
pub async fn dispatch(
    request: EditorRequest,
    caller: CallerContext,
    state: &AppState,
) -> Result<EditorReply, AppError> {
    state.validate_generation(caller.generation)?;
    let has_required_run = if request_requires_live_run(&request) {
        require_live_run_for_sidecar(&caller, state)?.is_some()
    } else {
        false
    };
    let activity_handle = start_activity(&request, &caller, state)?;
    let mut caller = caller;
    caller.activity_id = activity_handle.as_ref().map(|handle| handle.id.clone());
    let required_run = if has_required_run {
        caller.run_id()
    } else {
        None
    };
    let committed_here = std::sync::atomic::AtomicBool::new(false);
    let undo = matches!(&request, EditorRequest::ProjectHistory { action, .. } if action == "undo");
    let result = async {
        match request {
            EditorRequest::ProjectStatus {} => Ok(EditorReply::ProjectStatus(state.status()?)),
            EditorRequest::ProjectCreate {
                name,
                aspect,
                fps_num,
                fps_den,
            } => Ok(EditorReply::ProjectStatus(state.create_project_dialog_at(
                name,
                aspect,
                fps_num,
                fps_den,
                caller.generation,
                required_run,
                caller.connection_id(),
            )?)),
            EditorRequest::ProjectOpen {} => {
                Ok(EditorReply::ProjectStatus(state.open_project_dialog_at(
                    caller.generation,
                    required_run,
                    caller.connection_id(),
                )?))
            }
            EditorRequest::ProjectClose {} => {
                if !caller.is_project_bound(state) && state.current_project_id().is_some() {
                    return Err(AppError::stale_session(
                        "The close request belongs to a different project",
                    ));
                }
                Ok(EditorReply::ProjectStatus(state.close_project_at(
                    caller.generation,
                    required_run,
                    caller.connection_id(),
                )?))
            }
            EditorRequest::ProjectSave {} => {
                require_current_project(&caller, state)?;
                Ok(EditorReply::ProjectStatus(
                    state.save_project_at(caller.generation, required_run)?,
                ))
            }
            EditorRequest::ProjectSnapshot {} => {
                require_current_project(&caller, state)?;
                Ok(EditorReply::ProjectSnapshot(
                    state.snapshot_at(caller.generation)?,
                ))
            }
            EditorRequest::TimelineSnapshot {} => {
                require_current_project(&caller, state)?;
                Ok(EditorReply::TimelineSnapshot(
                    state.timeline_snapshot_at(caller.generation)?,
                ))
            }
            EditorRequest::TimelineSelection { selection } => {
                require_current_project(&caller, state)?;
                Ok(EditorReply::TimelineSnapshot(
                    state.set_timeline_selection_at(caller.generation, selection)?,
                ))
            }
            EditorRequest::ProjectHistory {
                action,
                expected_revision,
                expected_transaction_id,
            } => {
                require_current_project(&caller, state)?;
                let action = HistoryAction::parse(&action)?;
                let result = state.history_at(
                    caller.generation,
                    action,
                    expected_revision,
                    expected_transaction_id,
                    required_run,
                )?;
                committed_here.store(true, std::sync::atomic::Ordering::Relaxed);
                Ok(EditorReply::ProjectHistory(result))
            }
            EditorRequest::EditProject {
                transaction_id,
                expected_revision,
                label,
                operations,
                dry_run,
            } => {
                require_current_project(&caller, state)?;
                let dry_run = dry_run.unwrap_or(false);
                let payload_hash = canonical_payload_hash(&label, &operations, dry_run)?;
                let transcript_ids: Vec<String> = operations
                    .iter()
                    .filter_map(|operation| match operation {
                        EditOp::ReplaceCaptions { transcript_id, .. } => {
                            Some(transcript_id.clone())
                        }
                        _ => None,
                    })
                    .collect();
                let store = state.current_store()?;
                let result = if dry_run {
                    let operation_store = store.clone();
                    let operation_ids = transcript_ids.clone();
                    let operation_list = operations.clone();
                    state.dry_run_at(
                        caller.generation,
                        transaction_id,
                        expected_revision,
                        label,
                        payload_hash,
                        required_run,
                        move |document| {
                            let transcripts = operation_store
                                .load_transcripts_for_document(document, &operation_ids)?;
                            apply_batch_in_place(document, &operation_list, &transcripts)
                        },
                    )?
                } else {
                    let operation_ids = transcript_ids;
                    let applied = &committed_here;
                    state.commit_at_with_run(
                        caller.generation,
                        required_run,
                        transaction_id,
                        expected_revision,
                        label,
                        payload_hash,
                        move |document| {
                            applied.store(true, std::sync::atomic::Ordering::Relaxed);
                            let transcripts =
                                store.load_transcripts_for_document(document, &operation_ids)?;
                            apply_batch_in_place(document, &operations, &transcripts)
                        },
                    )?
                };
                Ok(EditorReply::ProjectEdit(result))
            }
            EditorRequest::Media(action) => Ok(EditorReply::Media(
                state.media().handle(&action, &caller, state).await?,
            )),
            EditorRequest::Jobs(action) => Ok(EditorReply::Jobs(
                state.jobs().handle(&action, &caller, state).await?,
            )),
            EditorRequest::Preview(action) => Ok(EditorReply::Preview(
                state.render().handle(action, &caller, state).await?,
            )),
            EditorRequest::ExportVideo(action) => Ok(EditorReply::ExportVideo(
                state.export().handle(action, &caller, state).await?,
            )),
            EditorRequest::Evidence(action) => Ok(EditorReply::Evidence(
                state.evidence().handle(action, &caller, state).await?,
            )),
            EditorRequest::Transcript(action) => Ok(EditorReply::Transcript(
                state.evidence().transcript(action, &caller, state).await?,
            )),
            EditorRequest::AnalyzeMedia(action) => {
                let evidence_action = match action {
                    crate::media::analysis::AnalysisAction::Scenes {
                        asset_id,
                        threshold,
                    } => EvidenceAction::Scenes {
                        asset_id,
                        threshold,
                    },
                    crate::media::analysis::AnalysisAction::Silence { asset_id } => {
                        EvidenceAction::Silence { asset_id }
                    }
                };
                Ok(EditorReply::AnalyzeMedia(
                    state
                        .evidence()
                        .handle(evidence_action, &caller, state)
                        .await?,
                ))
            }
            EditorRequest::SampleFrames(action) => Ok(EditorReply::SampleFrames(
                state
                    .evidence()
                    .handle(
                        EvidenceAction::SampleFrames {
                            asset_id: action.asset_id,
                            start_frame: action.start_frame,
                            end_frame: action.end_frame,
                            count: action.count,
                        },
                        &caller,
                        state,
                    )
                    .await?,
            )),
            EditorRequest::CreateGraphic(action) => Ok(EditorReply::CreateGraphic(
                state
                    .evidence()
                    .handle(
                        EvidenceAction::CreateGraphic {
                            name: action.name,
                            svg: action.svg,
                            width: action.width,
                            height: action.height,
                        },
                        &caller,
                        state,
                    )
                    .await?,
            )),
            EditorRequest::Assistant(action) => Ok(EditorReply::Assistant(
                state.assistant().handle(action, &caller, state).await?,
            )),
            EditorRequest::Providers(action) => Ok(EditorReply::Providers(
                state
                    .assistant()
                    .handle_providers(action, &caller, state)
                    .await?,
            )),
            EditorRequest::Permissions(action) => {
                ensure_permission_caller(&action, &caller)?;
                Ok(EditorReply::Permissions(
                    state.permissions().handle(&action, &caller, state).await?,
                ))
            }
        }
    }
    .await;
    // An observer failure must not turn an already committed edit into a failed reply.
    let _ = finish_activity(
        state,
        activity_handle.as_ref(),
        &result,
        committed_here.load(std::sync::atomic::Ordering::Relaxed),
        undo,
    );
    result
}

fn start_activity(
    request: &EditorRequest,
    caller: &CallerContext,
    state: &AppState,
) -> Result<Option<ActivityHandle>, AppError> {
    let Some(mut start) = describe_request(request, caller, None) else {
        return Ok(None);
    };
    if should_capture_activity_document(request) {
        if let Ok(snapshot) = state.current_store().and_then(|store| store.snapshot()) {
            if let Some(refined) = describe_request(request, caller, Some(&snapshot.document)) {
                start = refined;
            }
        }
    }
    state.activity_start(start).map(|(handle, _)| Some(handle))
}

fn should_capture_activity_document(request: &EditorRequest) -> bool {
    matches!(
        request,
        EditorRequest::Preview(
            crate::media::render::PreviewAction::RenderFrame { .. }
                | crate::media::render::PreviewAction::Inspect { .. }
        )
    )
}

fn finish_activity(
    state: &AppState,
    handle: Option<&ActivityHandle>,
    result: &Result<EditorReply, AppError>,
    committed_here: bool,
    undo: bool,
) -> Result<(), AppError> {
    let Some(handle) = handle else {
        return Ok(());
    };
    let mut patch = ActivityPatch::default();
    match result {
        Ok(reply) => {
            let job_ids = extract_job_ids(reply);
            patch.job_ids = Some(job_ids.clone());
            patch.phase = Some(if job_ids.is_empty() {
                ActivityPhase::Completed
            } else {
                ActivityPhase::Queued
            });
            match reply {
                EditorReply::ProjectEdit(edit) | EditorReply::ProjectHistory(edit) => {
                    patch.revision = Some(edit.revision);
                    patch.transaction_id = Some(edit.transaction_id.clone());
                    patch.changed = Some(edit.changed && committed_here);
                    if edit.changed && committed_here {
                        if let Ok(Some(mut delta)) = state
                            .current_store()
                            .and_then(|store| store.transaction_delta(&edit.transaction_id))
                        {
                            if undo {
                                for entity in &mut delta.changes {
                                    std::mem::swap(&mut entity.before, &mut entity.after);
                                }
                            }
                            let (changes, total) = changes_from_delta(&delta);
                            patch.total_targets = Some(total);
                            patch.changes = Some(changes.clone());
                            patch.targets =
                                Some(changes.into_iter().map(|change| change.target).collect());
                            patch.message = Some(Some(format!("{total} committed change(s)")));
                        }
                    } else {
                        patch.message = Some(Some("No project changes committed".to_owned()));
                    }
                }
                _ => {}
            }
        }
        Err(error) => {
            let cancelled = error.code == crate::error::ErrorCode::JobCancelled;
            patch.phase = Some(if cancelled {
                ActivityPhase::Cancelled
            } else {
                ActivityPhase::Failed
            });
            patch.error = Some(Some(error.clone()));
            patch.message = Some(Some(
                if cancelled {
                    "Native operation cancelled"
                } else {
                    "Native operation failed"
                }
                .to_owned(),
            ));
        }
    }
    state.activity_patch(handle, patch)?;
    state
        .activity_snapshot_at(handle.generation, handle.project_id.as_deref())
        .map(|_| ())
}

fn canonical_payload_hash(
    label: &str,
    operations: &[EditOp],
    dry_run: bool,
) -> Result<String, AppError> {
    let payload = serde_json::to_vec(&(label, operations, dry_run))
        .map_err(|_| AppError::schema("The edit payload could not be encoded"))?;
    let mut hash = 0xcbf29ce484222325u64;
    for byte in payload {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    Ok(format!("{hash:016x}"))
}
fn require_live_run_for_sidecar<'a>(
    caller: &'a CallerContext,
    state: &AppState,
) -> Result<Option<&'a str>, AppError> {
    if matches!(&caller.kind, CallerKind::HumanWindow { .. }) {
        return Ok(None);
    }
    let run_id = caller
        .run_id()
        .ok_or_else(|| AppError::stale_session("A live assistant run is required"))?;
    state.require_active_run_at(caller.generation, run_id)?;
    Ok(Some(run_id))
}

fn request_requires_live_run(request: &EditorRequest) -> bool {
    match request {
        EditorRequest::ProjectCreate { .. }
        | EditorRequest::ProjectOpen {}
        | EditorRequest::ProjectClose {}
        | EditorRequest::ProjectSave {}
        | EditorRequest::ProjectHistory { .. }
        | EditorRequest::EditProject { .. }
        | EditorRequest::TimelineSelection { .. }
        | EditorRequest::SampleFrames(_)
        | EditorRequest::CreateGraphic(_) => true,
        EditorRequest::Media(action) => matches!(
            action,
            crate::media::MediaAction::Import { .. }
                | crate::media::MediaAction::Relink { .. }
                | crate::media::MediaAction::Remove { .. }
                | crate::media::MediaAction::Thumbnail { .. }
                | crate::media::MediaAction::Waveform { .. }
        ),
        EditorRequest::Jobs(action) => {
            matches!(action, crate::media::JobsAction::Cancel { .. })
        }
        EditorRequest::Preview(action) => matches!(
            action,
            crate::media::render::PreviewAction::RenderFrame { .. }
                | crate::media::render::PreviewAction::RenderAudioWindow { .. }
                | crate::media::render::PreviewAction::Seek { .. }
                | crate::media::render::PreviewAction::Play {}
                | crate::media::render::PreviewAction::Pause {}
                | crate::media::render::PreviewAction::Inspect { .. }
        ),
        EditorRequest::ExportVideo(action) => matches!(
            action,
            crate::media::export::ExportAction::Start { .. }
                | crate::media::export::ExportAction::Cancel { .. }
                | crate::media::export::ExportAction::Play { .. }
                | crate::media::export::ExportAction::ShowFile { .. }
        ),
        EditorRequest::Evidence(action) => matches!(
            action,
            crate::media::EvidenceAction::Transcribe { .. }
                | crate::media::EvidenceAction::ImportSrt { .. }
                | crate::media::EvidenceAction::ExportSrt {}
                | crate::media::EvidenceAction::Scenes { .. }
                | crate::media::EvidenceAction::Silence { .. }
                | crate::media::EvidenceAction::SampleFrames { .. }
                | crate::media::EvidenceAction::CreateGraphic { .. }
        ),
        EditorRequest::Transcript(action) => matches!(
            action,
            crate::media::evidence::TranscriptAction::Transcribe { .. }
                | crate::media::evidence::TranscriptAction::ImportSrt { .. }
                | crate::media::evidence::TranscriptAction::ExportSrt {}
        ),
        EditorRequest::AnalyzeMedia(_) => true,
        EditorRequest::Assistant(action) => matches!(
            action,
            crate::assistant::AssistantAction::Prompt { .. }
                | crate::assistant::AssistantAction::Stop {}
                | crate::assistant::AssistantAction::Restart {}
                | crate::assistant::AssistantAction::NewSession {}
        ),
        EditorRequest::Providers(action) => matches!(
            action,
            crate::assistant::ProvidersAction::Login { .. }
                | crate::assistant::ProvidersAction::Answer { .. }
                | crate::assistant::ProvidersAction::Logout { .. }
                | crate::assistant::ProvidersAction::Select { .. }
                | crate::assistant::ProvidersAction::Refresh {}
        ),
        EditorRequest::Permissions(action) => {
            !matches!(action, crate::permissions::PermissionsAction::Pending {})
        }
        EditorRequest::ProjectStatus {}
        | EditorRequest::ProjectSnapshot {}
        | EditorRequest::TimelineSnapshot {} => false,
    }
}

fn ensure_permission_caller(
    action: &crate::permissions::PermissionsAction,
    caller: &CallerContext,
) -> Result<(), AppError> {
    if !matches!(caller.kind, CallerKind::HumanWindow { .. }) {
        return Ok(());
    }
    let action_name = serde_json::to_value(action)
        .ok()
        .and_then(|value| {
            value
                .get("action")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
        })
        .unwrap_or_default();
    if matches!(
        action_name.as_str(),
        "pending" | "answer" | "evidence" | "revoke"
    ) {
        return Ok(());
    }
    Err(AppError::new(
        ErrorCode::PermissionDenied,
        "This permission operation is available only to the supervised assistant",
    ))
}

fn require_current_project(caller: &CallerContext, state: &AppState) -> Result<(), AppError> {
    if !caller.is_project_bound(state) {
        return Err(AppError::stale_session(
            "The request belongs to a different project",
        ));
    }
    if state.current_project_id().is_none() {
        return Err(AppError::invalid_argument("No project is open"));
    }
    Ok(())
}

fn human_caller(window: &tauri::Window, state: &AppState) -> CallerContext {
    CallerContext::human_window(
        window.label().to_owned(),
        state.generation(),
        state.current_project_id(),
    )
}

/// Tauri's invoke handler accepts the raw generated editor request envelope
/// and validates it through the same strict `EditorRequest::from_wire` path
/// used by the supervised sidecar before dispatch. Caller identity and
/// project/generation context are still derived from the native window and
/// current state, never from JSON supplied by the WebView.
#[tauri::command]
pub async fn editor_call(
    window: tauri::Window,
    state: tauri::State<'_, AppState>,
    request: Value,
) -> Result<EditorReply, AppError> {
    let request = EditorRequest::from_native_wire(&request)?;
    dispatch(request, human_caller(&window, &state), &state).await
}
