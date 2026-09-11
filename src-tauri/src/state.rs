use crate::agent_bridge::{
    AgentBridge, AgentPaths, BridgeLifecycle, PrivateRequestContext, PrivateRequestHandler,
};
use crate::assistant::AssistantRuntime;
use crate::credentials::CredentialsRuntime;
use crate::editor::operations::Transcript;
use crate::error::AppError;
use crate::ipc::{
    validate_safe_integer, EditorEvent, ProjectSnapshot, ProjectStatus, TimelineSelection,
    TimelineSnapshot, MAX_SAFE_INTEGER,
};
use crate::media::artifacts::ArtifactStore;
use crate::media::export::ExportRuntime;
use crate::media::ffmpeg::FfmpegToolchain;
use crate::media::probe::resolve_packaged_binary;
use crate::media::render::RenderRuntime;
use crate::media::{EvidenceRuntime, JobsRuntime, MediaRuntime};
use crate::permissions::PermissionsRuntime;
use crate::project::model::{AssetManifest, ProjectDocument, ProjectEnvelope};
use crate::project::store::{EditResult, HistoryAction, ProjectStore};
use serde_json::{json, Value};
use std::collections::HashSet;
use std::fs;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use tauri::Emitter;
use tauri_plugin_opener::OpenerExt;
use uuid::Uuid;

struct OpenProject {
    store: ProjectStore,
    selection: Mutex<TimelineSelection>,
}
struct AppStateInner {
    paths: AgentPaths,
    generation: AtomicU64,
    lifecycle: Mutex<()>,
    project: RwLock<Option<OpenProject>>,
    bridge: Mutex<Option<AgentBridge>>,
    bridge_generation: Mutex<Option<u64>>,
    bridge_start: tokio::sync::Mutex<()>,
    app_handle: Mutex<Option<tauri::AppHandle>>,
    media: MediaRuntime,
    jobs: JobsRuntime,
    render: RenderRuntime,
    export: ExportRuntime,
    evidence: EvidenceRuntime,
    permissions: PermissionsRuntime,
    credentials: CredentialsRuntime,
    assistant: AssistantRuntime,
}

/// Process-wide state shared by the Tauri command handler and the supervised
/// agent bridge. A ProjectStore owns the serialized writer and OS lock; this
/// state owns only the active lifecycle and generation binding.
#[derive(Clone)]
pub struct AppState {
    inner: Arc<AppStateInner>,
}

impl AppState {
    pub fn new(paths: AgentPaths) -> Result<Self, AppError> {
        Self::new_with_handle(paths, None)
    }

    pub fn new_with_handle(
        paths: AgentPaths,
        app_handle: Option<tauri::AppHandle>,
    ) -> Result<Self, AppError> {
        paths.ensure_owned_directories()?;
        let permission_data_dir = paths.app_data_dir.clone();
        let media = MediaRuntime::new();
        let registry = media.jobs();
        let jobs = JobsRuntime::from_registry(registry.clone());
        Ok(Self {
            inner: Arc::new(AppStateInner {
                paths,
                generation: AtomicU64::new(0),
                lifecycle: Mutex::new(()),
                project: RwLock::new(None),
                bridge: Mutex::new(None),
                bridge_generation: Mutex::new(None),
                bridge_start: tokio::sync::Mutex::new(()),
                app_handle: Mutex::new(app_handle),
                media,
                jobs,
                render: RenderRuntime::new(),
                export: ExportRuntime::from_registry(registry),
                evidence: EvidenceRuntime::new(),
                permissions: PermissionsRuntime::new(permission_data_dir.clone())?,
                credentials: CredentialsRuntime::new(permission_data_dir)?,
                assistant: AssistantRuntime::new(),
            }),
        })
    }
    pub fn attach_app_handle(&self, app_handle: tauri::AppHandle) -> Result<(), AppError> {
        *self
            .inner
            .app_handle
            .lock()
            .map_err(|_| AppError::io("The application event handle is unavailable"))? =
            Some(app_handle);
        Ok(())
    }

    /// Open a provider's already validated HTTPS authentication destination
    /// through the native opener.  This is intentionally not a Tauri command:
    /// only the native assistant event path can invoke it.
    pub fn open_provider_auth_url(&self, url: &str) -> Result<(), AppError> {
        let destination = AssistantRuntime::validate_auth_destination(url)?;
        let handle = self
            .inner
            .app_handle
            .lock()
            .map_err(|_| AppError::io("The application opener handle is unavailable"))?
            .clone()
            .ok_or_else(|| AppError::io("The application opener is not initialized"))?;
        handle
            .opener()
            .open_url(destination, None::<String>)
            .map_err(|_| {
                AppError::new(
                    crate::error::ErrorCode::ProviderError,
                    "The provider authentication page could not be opened",
                )
            })
    }

    /// Provider-scoped form used by trusted native callers that have retained
    /// the provider identity alongside an authentication event.
    pub fn open_provider_auth_url_for_provider(
        &self,
        provider_id: &str,
        url: &str,
    ) -> Result<(), AppError> {
        if provider_id != "openai-codex" {
            return Err(AppError::new(
                crate::error::ErrorCode::PermissionDenied,
                "Only the OpenAI Codex provider supports native browser authentication",
            ));
        }
        self.open_provider_auth_url(url)
    }

    /// Open or reveal an output only after the export runtime has revalidated
    /// the finalized target identity.  This helper remains native-only and
    /// never appears in the frontend command registry.
    pub fn open_completed_output(&self, path: &Path, reveal: bool) -> Result<(), AppError> {
        if !path.is_absolute() {
            return Err(AppError::invalid_argument(
                "The completed output path must be absolute",
            ));
        }
        let metadata = fs::symlink_metadata(path).map_err(|_| {
            AppError::new(
                crate::error::ErrorCode::AssetUnavailable,
                "The completed output is unavailable",
            )
        })?;
        if metadata.file_type().is_symlink() || !metadata.is_file() || metadata.len() == 0 {
            return Err(AppError::new(
                crate::error::ErrorCode::AssetUnavailable,
                "The completed output is unavailable",
            ));
        }
        let handle = self
            .inner
            .app_handle
            .lock()
            .map_err(|_| AppError::io("The application opener handle is unavailable"))?
            .clone()
            .ok_or_else(|| AppError::io("The application opener is not initialized"))?;
        let path = path.to_string_lossy().into_owned();
        if reveal {
            handle
                .opener()
                .reveal_item_in_dir(path)
                .map_err(|_| AppError::io("The completed output could not be revealed"))
        } else {
            handle
                .opener()
                .open_path(path, None::<String>)
                .map_err(|_| AppError::io("The completed output could not be opened"))
        }
    }

    pub fn paths(&self) -> &AgentPaths {
        &self.inner.paths
    }

    pub fn media(&self) -> &MediaRuntime {
        &self.inner.media
    }

    pub fn jobs(&self) -> &JobsRuntime {
        &self.inner.jobs
    }

    pub fn render(&self) -> &RenderRuntime {
        &self.inner.render
    }

    pub fn export(&self) -> &ExportRuntime {
        &self.inner.export
    }

    pub fn evidence(&self) -> &EvidenceRuntime {
        &self.inner.evidence
    }

    pub fn permissions(&self) -> &PermissionsRuntime {
        &self.inner.permissions
    }

    pub fn credentials(&self) -> &CredentialsRuntime {
        &self.inner.credentials
    }

    pub fn assistant(&self) -> &AssistantRuntime {
        &self.inner.assistant
    }

    pub fn generation(&self) -> u64 {
        self.inner.generation.load(Ordering::Acquire)
    }

    /// Advance the session generation for project open/close and sidecar
    /// replacement. Checked arithmetic prevents a value that JavaScript
    /// cannot represent exactly from crossing the IPC boundary.
    pub fn advance_generation(&self) -> Result<u64, AppError> {
        let (generation, project_id) = {
            let _lifecycle = self
                .inner
                .lifecycle
                .lock()
                .map_err(|_| AppError::io("The application lifecycle lock is unavailable"))?;
            let project_id = self
                .inner
                .project
                .read()
                .map_err(|_| AppError::io("The project state lock is unavailable"))?
                .as_ref()
                .map(|open| open.store.project_id());
            let generation = self.advance_generation_unlocked()?;
            (generation, project_id)
        };
        self.emit_generation_changed(generation, project_id);
        Ok(generation)
    }

    fn emit_generation_changed(&self, generation: u64, project_id: Option<String>) {
        let event = EditorEvent {
            kind: "project_changed".to_owned(),
            project_id,
            generation,
            run_id: None,
            data: Some(json!({"reason": "generation_changed"})),
        };
        let handle = self
            .inner
            .app_handle
            .lock()
            .ok()
            .and_then(|handle| handle.clone());
        if let Some(handle) = handle {
            let _ = handle.emit("cutterhoochee://event", &event);
        }
    }

    fn configure_bridge(&self, bridge: &AgentBridge) -> Result<(), AppError> {
        let state = self.clone();
        bridge.register_private_handler(Arc::new(move |method, params, context| {
            let state = state.clone();
            Box::pin(async move {
                state
                    .handle_private_bridge_request(method, params, context)
                    .await
            })
        }))?;

        let state = self.clone();
        bridge.register_lifecycle_hook(Arc::new(move |lifecycle| {
            let BridgeLifecycle::Terminated {
                connection_id,
                generation,
            } = lifecycle;
            let _ = state
                .credentials()
                .retire_context(&connection_id, generation);
            let _ = state.assistant().retire_generation(&state, generation);
            let _ = state.permissions().revoke_generation(generation);
            state.cancel_project_jobs(generation, None);
            state.clear_agent_bridge(&connection_id);
        }))?;
        Ok(())
    }

    async fn handle_private_bridge_request(
        &self,
        method: String,
        params: Value,
        context: PrivateRequestContext,
    ) -> Result<Value, AppError> {
        self.validate_generation(context.generation)?;
        let caller = &context.caller;
        match method.as_str() {
            "evidence_image_read" => {
                crate::agent_bridge::read_evidence_image(self, &context, params).await
            }
            "credential_read" => {
                let provider_id = private_required_string(&params, "providerId")?;
                let credential = self.credentials().read(&provider_id, caller).await?;
                serde_json::to_value(json!({ "credential": credential }))
                    .map_err(|_| AppError::schema("The credential response could not be encoded"))
            }
            "credential_list" => {
                private_require_empty_object(&params)?;
                let credentials = self.credentials().list(caller).await?;
                serde_json::to_value(credentials)
                    .map_err(|_| AppError::schema("The credential list could not be encoded"))
            }
            "credential_lease_acquire" => {
                let provider_id = private_required_string(&params, "providerId")?;
                let lease = self
                    .credentials()
                    .acquire_lease(&provider_id, caller)
                    .await?;
                serde_json::to_value(lease)
                    .map_err(|_| AppError::schema("The credential lease could not be encoded"))
            }
            "credential_lease_commit" => {
                let lease_id = private_required_string(&params, "leaseId")?;
                let auth_generation = private_required_u64(&params, "authGeneration")?;
                let credential_value = params
                    .get("credential")
                    .cloned()
                    .ok_or_else(|| AppError::invalid_argument("credential is required"))?;
                let credential = serde_json::from_value(credential_value)
                    .map_err(|_| AppError::invalid_argument("The credential payload is invalid"))?;
                let committed = self
                    .credentials()
                    .commit_lease_for_caller(&lease_id, auth_generation, credential, caller)
                    .await?;
                serde_json::to_value(json!({ "credential": committed }))
                    .map_err(|_| AppError::schema("The credential response could not be encoded"))
            }
            "credential_lease_release" => {
                let lease_id = private_required_string(&params, "leaseId")?;
                self.credentials()
                    .release_lease_for_caller(&lease_id, caller)
                    .await?;
                Ok(Value::Null)
            }
            "credential_delete" => {
                let provider_id = private_required_string(&params, "providerId")?;
                self.credentials().delete(&provider_id, caller).await?;
                Ok(Value::Null)
            }
            _ => Err(AppError::schema(
                "The private bridge method is not supported",
            )),
        }
    }

    fn subscribe_private_events(&self, bridge: &AgentBridge) {
        let mut events = bridge.subscribe_private_events();
        let state = self.clone();
        tauri::async_runtime::spawn(async move {
            loop {
                match events.recv().await {
                    Ok(event) => {
                        if state.validate_generation(event.generation).is_err() {
                            continue;
                        }
                        let _ = state.assistant().forward_private_event(&state, &event);
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        });
    }

    fn advance_generation_unlocked(&self) -> Result<u64, AppError> {
        let result =
            self.inner
                .generation
                .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
                    current
                        .checked_add(1)
                        .filter(|next| *next <= MAX_SAFE_INTEGER)
                });
        match result {
            Ok(previous) => {
                if let Ok(mut marker) = self.inner.bridge_generation.lock() {
                    *marker = None;
                }
                let _ = self.permissions().revoke_generation(previous);
                self.cancel_project_jobs(previous, None);
                self.inner.render.stop_software_preview();
                Ok(previous + 1)
            }
            Err(_) => Err(AppError::io(
                "The application generation exhausted its safe range",
            )),
        }
    }

    pub fn validate_generation(&self, generation: u64) -> Result<(), AppError> {
        if generation != self.generation() {
            return Err(AppError::stale_session(
                "The request belongs to a retired application generation",
            ));
        }
        Ok(())
    }
    pub fn require_active_run_at(&self, generation: u64, run_id: &str) -> Result<(), AppError> {
        let _lifecycle = self
            .inner
            .lifecycle
            .lock()
            .map_err(|_| AppError::io("The application lifecycle lock is unavailable"))?;
        self.validate_generation(generation)?;
        self.require_active_run_unlocked(generation, run_id)
    }

    fn require_active_run_unlocked(&self, generation: u64, run_id: &str) -> Result<(), AppError> {
        let project_id = self
            .inner
            .project
            .read()
            .map_err(|_| AppError::io("The project state lock is unavailable"))?
            .as_ref()
            .map(|open| open.store.project_id());
        self.permissions()
            .require_active_run(generation, project_id.as_deref(), run_id)
    }
    fn finish_project_transition(
        &self,
        expected_generation: u64,
        expected_project_id: Option<&str>,
    ) -> Result<(u64, Option<String>, bool), AppError> {
        let _lifecycle = self
            .inner
            .lifecycle
            .lock()
            .map_err(|_| AppError::io("The application lifecycle lock is unavailable"))?;
        let project_id = self
            .inner
            .project
            .read()
            .map_err(|_| AppError::io("The project state lock is unavailable"))?
            .as_ref()
            .map(|open| open.store.project_id());
        if project_id.as_deref() != expected_project_id {
            return Err(AppError::stale_session(
                "The project transition was superseded by another lifecycle change",
            ));
        }
        let current_generation = self.generation();
        if current_generation == expected_generation {
            let generation = self.advance_generation_unlocked()?;
            Ok((generation, project_id, true))
        } else {
            Ok((current_generation, project_id, false))
        }
    }
    fn stop_bridge_before_lifecycle(&self) {
        let bridge = self
            .inner
            .bridge
            .lock()
            .ok()
            .and_then(|mut current| current.take());
        if let Some(bridge) = bridge {
            bridge.stop();
        }
    }

    fn schedule_bridge_restart(&self) {
        let has_handle = self
            .inner
            .app_handle
            .lock()
            .map(|handle| handle.is_some())
            .unwrap_or(false);
        if !has_handle {
            return;
        }
        let state = self.clone();
        tauri::async_runtime::spawn(async move {
            let _ = state.ensure_agent_bridge().await;
        });
    }

    fn cancel_project_jobs(&self, generation: u64, project_id: Option<&str>) {
        let registry = self.inner.jobs.registry();
        for job in registry.list() {
            if job.generation != generation
                || project_id.is_some_and(|expected| job.project_id.as_deref() != Some(expected))
            {
                continue;
            }
            let _ = registry.cancel(&job.job_id);
        }
    }

    fn prepare_render_for_store(&self, store: &ProjectStore) -> Result<ArtifactStore, AppError> {
        let artifacts = ArtifactStore::for_project(store.root(), store.workspace_id())?
            .with_font_resource_dir(&self.paths().resource_dir)?;
        let ffmpeg = resolve_packaged_binary(&self.paths().resource_dir, "ffmpeg")?;
        let ffprobe = resolve_packaged_binary(&self.paths().resource_dir, "ffprobe")?;
        self.inner
            .render
            .set_artifacts(Arc::new(artifacts.clone()))?;
        self.inner
            .render
            .set_toolchain(FfmpegToolchain::new(ffmpeg, ffprobe))?;
        Ok(artifacts)
    }
    pub fn artifact_url_at(&self, generation: u64, artifact_id: &str) -> Result<String, AppError> {
        self.validate_generation(generation)?;
        let store = self.inner.media.artifact_store(self)?;
        store.url(artifact_id, generation)
    }

    pub fn artifact_metadata_at(
        &self,
        generation: u64,
        workspace_id: &str,
        artifact_id: &str,
    ) -> Result<(u64, String), AppError> {
        self.validate_generation(generation)?;
        let store = self.inner.media.artifact_store(self)?;
        if store.workspace_id() != workspace_id {
            return Err(AppError::stale_session(
                "The managed artifact belongs to a retired workspace",
            ));
        }
        let path = store.managed_path(artifact_id)?;
        let metadata = fs::metadata(&path)?;
        if !metadata.is_file() {
            return Err(AppError::new(
                crate::error::ErrorCode::AssetUnavailable,
                "The managed artifact is unavailable",
            ));
        }
        let extension = path
            .extension()
            .and_then(|value| value.to_str())
            .unwrap_or("bin")
            .to_ascii_lowercase();
        Ok((metadata.len(), extension))
    }

    pub fn read_artifact_at(
        &self,
        generation: u64,
        workspace_id: &str,
        artifact_id: &str,
        offset: Option<u64>,
        length: Option<u64>,
    ) -> Result<(Vec<u8>, u64, String), AppError> {
        let store = self.inner.media.artifact_store(self)?;
        let (size, extension) = self.artifact_metadata_at(generation, workspace_id, artifact_id)?;
        let offset = offset.unwrap_or(0);
        if offset > size {
            return Err(AppError::invalid_argument(
                "The managed artifact range starts past the end of the file",
            ));
        }
        let length = length.unwrap_or_else(|| size.saturating_sub(offset));
        if length > 8 * 1024 * 1024 {
            return Err(AppError::invalid_argument(
                "The managed artifact range exceeds the supported limit",
            ));
        }
        let bytes = store.range(artifact_id, offset, length as usize)?;
        Ok((bytes, size, extension))
    }
    /// Emit one native-owned event to the desktop. The payload is sanitized
    /// before crossing the WebView boundary; callers cannot supply authority
    /// or alter the generation/project identity attached here.
    pub fn emit_sanitized_event(
        &self,
        kind: impl Into<String>,
        run_id: Option<String>,
        data: Option<Value>,
    ) -> Result<(), AppError> {
        let event = EditorEvent {
            kind: kind.into(),
            project_id: self.current_project_id(),
            generation: self.generation(),
            run_id,
            data: sanitize_event_data(data),
        };
        let handle = self
            .inner
            .app_handle
            .lock()
            .map_err(|_| AppError::io("The application event handle is unavailable"))?
            .clone();
        if let Some(handle) = handle {
            handle
                .emit("cutterhoochee://event", &event)
                .map_err(|_| AppError::io("The application event could not be emitted"))?;
        }
        Ok(())
    }

    /// Forward a bridge event only when it belongs to the current native
    /// generation. Bridge payloads are treated as untrusted data.
    pub fn emit_bridge_event(
        &self,
        event: &crate::agent_bridge::BridgeEvent,
    ) -> Result<(), AppError> {
        self.validate_generation(event.generation)?;
        if event.project_id != self.current_project_id() {
            return Err(AppError::stale_session(
                "The bridge event belongs to a different project",
            ));
        }
        self.emit_sanitized_event(
            event.event.clone(),
            event.run_id.clone(),
            event.data.clone(),
        )
    }

    fn with_project_at<R>(
        &self,
        expected_generation: u64,
        f: impl FnOnce(&ProjectStore) -> R,
    ) -> Result<R, AppError> {
        let _lifecycle = self
            .inner
            .lifecycle
            .lock()
            .map_err(|_| AppError::io("The application lifecycle lock is unavailable"))?;
        self.validate_generation(expected_generation)?;
        let project = self
            .inner
            .project
            .read()
            .map_err(|_| AppError::io("The project state lock is unavailable"))?;
        let open = project
            .as_ref()
            .ok_or_else(|| AppError::invalid_argument("No project is open"))?;
        Ok(f(&open.store))
    }

    fn with_project<R>(&self, f: impl FnOnce(&ProjectStore) -> R) -> Result<R, AppError> {
        self.with_project_at(self.generation(), f)
    }

    pub fn current_project_id(&self) -> Option<String> {
        let _lifecycle = self.inner.lifecycle.lock().ok()?;
        let project = self.inner.project.read().ok()?;
        project.as_ref().map(|open| open.store.project_id())
    }

    pub fn current_workspace_id(&self) -> Option<String> {
        let _lifecycle = self.inner.lifecycle.lock().ok()?;
        let project = self.inner.project.read().ok()?;
        project
            .as_ref()
            .map(|open| open.store.workspace_id().to_owned())
    }

    pub fn current_store(&self) -> Result<ProjectStore, AppError> {
        let _lifecycle = self
            .inner
            .lifecycle
            .lock()
            .map_err(|_| AppError::io("The application lifecycle lock is unavailable"))?;
        self.inner
            .project
            .read()
            .map_err(|_| AppError::io("The project state lock is unavailable"))?
            .as_ref()
            .map(|open| open.store.clone())
            .ok_or_else(|| AppError::invalid_argument("No project is open"))
    }

    pub fn status(&self) -> Result<ProjectStatus, AppError> {
        let _lifecycle = self
            .inner
            .lifecycle
            .lock()
            .map_err(|_| AppError::io("The application lifecycle lock is unavailable"))?;
        let generation = self.generation();
        let project = self
            .inner
            .project
            .read()
            .map_err(|_| AppError::io("The project state lock is unavailable"))?;
        let Some(open) = project.as_ref() else {
            return Ok(ProjectStatus::closed(generation));
        };
        let snapshot = open.store.snapshot()?;
        Ok(ProjectStatus::open(
            generation,
            snapshot.project_id,
            snapshot.workspace_id,
            snapshot.document.name,
            snapshot.document.revision,
        ))
    }

    pub fn snapshot_at(&self, generation: u64) -> Result<ProjectSnapshot, AppError> {
        self.with_project_at(generation, |store| store.snapshot())?
            .map(|snapshot| ProjectSnapshot {
                document: snapshot.document,
                workspace_id: snapshot.workspace_id,
            })
    }

    pub fn snapshot(&self) -> Result<ProjectSnapshot, AppError> {
        self.snapshot_at(self.generation())
    }

    pub fn timeline_snapshot_at(&self, generation: u64) -> Result<TimelineSnapshot, AppError> {
        let _lifecycle = self
            .inner
            .lifecycle
            .lock()
            .map_err(|_| AppError::io("The application lifecycle lock is unavailable"))?;
        self.validate_generation(generation)?;
        let project = self
            .inner
            .project
            .read()
            .map_err(|_| AppError::io("The project state lock is unavailable"))?;
        let open = project
            .as_ref()
            .ok_or_else(|| AppError::invalid_argument("No project is open"))?;
        timeline_snapshot_for(open)
    }

    pub fn timeline_snapshot(&self) -> Result<TimelineSnapshot, AppError> {
        self.timeline_snapshot_at(self.generation())
    }

    pub fn set_timeline_selection_at(
        &self,
        generation: u64,
        selection: TimelineSelection,
    ) -> Result<TimelineSnapshot, AppError> {
        let _lifecycle = self
            .inner
            .lifecycle
            .lock()
            .map_err(|_| AppError::io("The application lifecycle lock is unavailable"))?;
        self.validate_generation(generation)?;
        let project = self
            .inner
            .project
            .read()
            .map_err(|_| AppError::io("The project state lock is unavailable"))?;
        let open = project
            .as_ref()
            .ok_or_else(|| AppError::invalid_argument("No project is open"))?;
        let snapshot = open.store.snapshot()?;
        validate_timeline_selection(&snapshot.document, &selection)?;
        *open
            .selection
            .lock()
            .map_err(|_| AppError::io("The timeline selection lock is unavailable"))? = selection;
        timeline_snapshot_for(open)
    }

    pub fn set_timeline_selection(
        &self,
        selection: TimelineSelection,
    ) -> Result<TimelineSnapshot, AppError> {
        self.set_timeline_selection_at(self.generation(), selection)
    }

    pub fn envelope_at(&self, generation: u64) -> Result<ProjectEnvelope, AppError> {
        self.with_project_at(generation, ProjectStore::envelope)?
    }

    pub fn envelope(&self) -> Result<ProjectEnvelope, AppError> {
        self.envelope_at(self.generation())
    }

    pub fn commit_at<F>(
        &self,
        generation: u64,
        transaction_id: String,
        expected_revision: u64,
        label: String,
        payload_hash: String,
        apply: F,
    ) -> Result<EditResult, AppError>
    where
        F: FnOnce(&mut ProjectDocument) -> Result<(), AppError>,
    {
        self.with_project_at(generation, |store| {
            store.commit(
                transaction_id,
                expected_revision,
                label,
                payload_hash,
                apply,
            )
        })?
    }

    /// Commit a sidecar/media publication against the revision that is
    /// current when the serialized writer is acquired.  Media preparation
    /// does not carry a stale UI revision: generation and (when supplied)
    /// run authority are checked immediately before the atomic project write.
    pub fn commit_at_with_run<F>(
        &self,
        generation: u64,
        run_id: Option<&str>,
        transaction_id: String,
        expected_revision: u64,
        label: String,
        payload_hash: String,
        apply: F,
    ) -> Result<EditResult, AppError>
    where
        F: FnOnce(&mut ProjectDocument) -> Result<(), AppError>,
    {
        let _lifecycle = self
            .inner
            .lifecycle
            .lock()
            .map_err(|_| AppError::io("The application lifecycle lock is unavailable"))?;
        self.validate_generation(generation)?;
        let project = self
            .inner
            .project
            .read()
            .map_err(|_| AppError::io("The project state lock is unavailable"))?;
        let open = project
            .as_ref()
            .ok_or_else(|| AppError::invalid_argument("No project is open"))?;

        if let Some(run_id) = run_id {
            let project_id = open.store.project_id();
            self.permissions()
                .require_active_run(generation, Some(&project_id), run_id)?;
        }
        open.store.commit(
            transaction_id,
            expected_revision,
            label,
            payload_hash,
            apply,
        )
    }

    /// Publish a fully prepared media manifest without substituting an
    /// outdated caller revision.  The current revision is selected while the
    /// lifecycle lock is held, then ProjectStore performs its normal atomic
    /// receipt/history commit.
    pub fn commit_asset_at(
        &self,
        generation: u64,
        run_id: Option<&str>,
        transaction_id: String,
        manifest: AssetManifest,
        relink_asset_id: Option<String>,
    ) -> Result<EditResult, AppError> {
        manifest.validate()?;
        if !manifest.is_ready() {
            return Err(AppError::invalid_argument(
                "Only a prepared media asset can be published",
            ));
        }
        let relink = relink_asset_id.is_some();
        let target_id = relink_asset_id.unwrap_or_else(|| manifest.id.clone());
        let payload_hash = format!(
            "media-{}:{}:{}",
            if relink { "relink" } else { "import" },
            target_id,
            manifest.content_hash
        );
        let label = if relink {
            "Relink media"
        } else {
            "Import media"
        }
        .to_owned();
        let mut published = manifest;
        published.id = target_id.clone();
        published.validate()?;

        let _lifecycle = self
            .inner
            .lifecycle
            .lock()
            .map_err(|_| AppError::io("The application lifecycle lock is unavailable"))?;
        self.validate_generation(generation)?;
        let project = self
            .inner
            .project
            .read()
            .map_err(|_| AppError::io("The project state lock is unavailable"))?;
        let open = project
            .as_ref()
            .ok_or_else(|| AppError::invalid_argument("No project is open"))?;
        let project_id = open.store.project_id();
        if let Some(run_id) = run_id {
            self.permissions()
                .require_active_run(generation, Some(&project_id), run_id)?;
        }
        let expected_revision = open.store.snapshot()?.document.revision;
        let content_hash = published.content_hash.clone();
        open.store.commit(
            transaction_id,
            expected_revision,
            label,
            payload_hash,
            move |document| {
                if relink {
                    let existing = document
                        .assets
                        .iter_mut()
                        .find(|asset| asset.id == target_id)
                        .ok_or_else(|| {
                            AppError::invalid_argument("The relinked asset no longer exists")
                        })?;
                    *existing = published;
                } else if document
                    .assets
                    .iter()
                    .any(|asset| asset.content_hash == content_hash)
                {
                    return Ok(());
                } else {
                    if document.assets.iter().any(|asset| asset.id == target_id) {
                        return Err(AppError::invalid_argument(
                            "The imported asset ID already exists",
                        ));
                    }
                    document.assets.push(published);
                }
                document.validate()
            },
        )
    }
    pub fn dry_run_at<F>(
        &self,
        generation: u64,
        transaction_id: String,
        expected_revision: u64,
        label: String,
        payload_hash: String,
        run_id: Option<&str>,
        apply: F,
    ) -> Result<EditResult, AppError>
    where
        F: FnOnce(&mut ProjectDocument) -> Result<(), AppError>,
    {
        self.with_project_at(generation, |store| {
            if let Some(run_id) = run_id {
                let project_id = store.project_id();
                self.permissions()
                    .require_active_run(generation, Some(&project_id), run_id)?;
            }
            store.dry_run(
                transaction_id,
                expected_revision,
                label,
                payload_hash,
                apply,
            )
        })?
    }

    /// Retire a Pi run while holding the same lifecycle mutex used by
    /// run-bound project writers.  Callers use this before aborting the
    /// sidecar so queued jobs cannot pass their final authority check.
    pub fn retire_run(&self, generation: u64, run_id: &str) -> Result<(), AppError> {
        let _lifecycle = self
            .inner
            .lifecycle
            .lock()
            .map_err(|_| AppError::io("The application lifecycle lock is unavailable"))?;
        self.validate_generation(generation)?;
        self.permissions().revoke_run(generation, run_id)?;
        self.credentials().retire_run(run_id)?;
        self.inner.jobs.registry().cancel_run(generation, run_id)?;
        Ok(())
    }

    pub fn write_transcript_at(
        &self,
        generation: u64,
        run_id: Option<&str>,
        transcript: &Transcript,
    ) -> Result<(), AppError> {
        self.with_project_at(generation, |store| {
            if let Some(run_id) = run_id {
                let project_id = store.project_id();
                self.permissions()
                    .require_active_run(generation, Some(&project_id), run_id)?;
            }
            store.write_transcript(transcript)
        })?
    }

    pub fn commit<F>(
        &self,
        transaction_id: String,
        expected_revision: u64,
        label: String,
        payload_hash: String,
        apply: F,
    ) -> Result<EditResult, AppError>
    where
        F: FnOnce(&mut ProjectDocument) -> Result<(), AppError>,
    {
        self.commit_at(
            self.generation(),
            transaction_id,
            expected_revision,
            label,
            payload_hash,
            apply,
        )
    }

    pub fn history_at(
        &self,
        generation: u64,
        action: HistoryAction,
        expected_revision: u64,
        expected_transaction_id: Option<String>,
        run_id: Option<&str>,
    ) -> Result<EditResult, AppError> {
        self.with_project_at(generation, |store| {
            if let Some(run_id) = run_id {
                let project_id = store.project_id();
                self.permissions()
                    .require_active_run(generation, Some(&project_id), run_id)?;
            }
            store.history(action, expected_revision, expected_transaction_id)
        })?
    }

    pub fn history(
        &self,
        action: HistoryAction,
        expected_revision: u64,
        expected_transaction_id: Option<String>,
    ) -> Result<EditResult, AppError> {
        self.history_at(
            self.generation(),
            action,
            expected_revision,
            expected_transaction_id,
            None,
        )
    }

    pub fn flush_at(&self, generation: u64) -> Result<(), AppError> {
        self.with_project_at(generation, ProjectStore::flush)?
    }

    pub fn flush(&self) -> Result<(), AppError> {
        self.flush_at(self.generation())
    }

    pub fn open_project_at(
        &self,
        root: &Path,
        expected_generation: u64,
        run_id: Option<&str>,
        connection_id: Option<&str>,
    ) -> Result<ProjectStatus, AppError> {
        if let Some(run_id) = run_id {
            self.require_active_run_at(expected_generation, run_id)?;
        }
        let store = ProjectStore::open(root, &self.paths().app_data_dir)?;
        if connection_id.is_none() {
            self.stop_bridge_before_lifecycle();
        }
        let _lifecycle = self
            .inner
            .lifecycle
            .lock()
            .map_err(|_| AppError::io("The application lifecycle lock is unavailable"))?;
        self.validate_generation(expected_generation)?;
        if let Some(run_id) = run_id {
            self.require_active_run_unlocked(expected_generation, run_id)?;
        }
        let old_project_id = self
            .inner
            .project
            .read()
            .map_err(|_| AppError::io("The project state lock is unavailable"))?
            .as_ref()
            .map(|open| open.store.project_id());
        self.cancel_project_jobs(expected_generation, old_project_id.as_deref());
        self.inner.render.retire_project()?;
        let _artifacts = self.prepare_render_for_store(&store)?;
        let mut active = self
            .inner
            .project
            .write()
            .map_err(|_| AppError::io("The project state lock is unavailable"))?;
        *active = Some(OpenProject {
            store,
            selection: Mutex::new(TimelineSelection::empty()),
        });
        let target_project_id = active.as_ref().map(|open| open.store.project_id());
        let (generation, project_id, should_emit) = if let Some(connection_id) = connection_id {
            drop(active);
            drop(_lifecycle);
            self.stop_bridge_if_connection(connection_id);
            self.finish_project_transition(expected_generation, target_project_id.as_deref())?
        } else {
            let generation = self.advance_generation_unlocked()?;
            drop(active);
            drop(_lifecycle);
            (generation, target_project_id, true)
        };
        if should_emit {
            self.emit_generation_changed(generation, project_id);
        }
        self.schedule_bridge_restart();
        self.status()
    }
    pub fn create_project_at(
        &self,
        root: &Path,
        name: String,
        aspect: Option<String>,
        fps_num: Option<u64>,
        fps_den: Option<u64>,
        expected_generation: u64,
        run_id: Option<&str>,
        connection_id: Option<&str>,
    ) -> Result<ProjectStatus, AppError> {
        if let Some(run_id) = run_id {
            self.require_active_run_at(expected_generation, run_id)?;
        }
        let (fps_num, fps_den) = (fps_num.unwrap_or(30), fps_den.unwrap_or(1));
        let store = ProjectStore::create(
            root,
            &self.paths().app_data_dir,
            &name,
            aspect.as_deref(),
            fps_num,
            fps_den,
        )?;
        if connection_id.is_none() {
            self.stop_bridge_before_lifecycle();
        }
        let _lifecycle = self
            .inner
            .lifecycle
            .lock()
            .map_err(|_| AppError::io("The application lifecycle lock is unavailable"))?;
        self.validate_generation(expected_generation)?;
        if let Some(run_id) = run_id {
            self.require_active_run_unlocked(expected_generation, run_id)?;
        }
        self.cancel_project_jobs(expected_generation, None);
        self.inner.render.retire_project()?;
        let _artifacts = self.prepare_render_for_store(&store)?;
        let mut active = self
            .inner
            .project
            .write()
            .map_err(|_| AppError::io("The project state lock is unavailable"))?;
        *active = Some(OpenProject {
            store,
            selection: Mutex::new(TimelineSelection::empty()),
        });
        let target_project_id = active.as_ref().map(|open| open.store.project_id());
        let (generation, project_id, should_emit) = if let Some(connection_id) = connection_id {
            drop(active);
            drop(_lifecycle);
            self.stop_bridge_if_connection(connection_id);
            self.finish_project_transition(expected_generation, target_project_id.as_deref())?
        } else {
            let generation = self.advance_generation_unlocked()?;
            drop(active);
            drop(_lifecycle);
            (generation, target_project_id, true)
        };
        if should_emit {
            self.emit_generation_changed(generation, project_id);
        }
        self.schedule_bridge_restart();
        self.status()
    }

    pub fn create_project(
        &self,
        root: &Path,
        name: String,
        aspect: Option<String>,
        fps_num: Option<u64>,
        fps_den: Option<u64>,
    ) -> Result<ProjectStatus, AppError> {
        self.create_project_at(
            root,
            name,
            aspect,
            fps_num,
            fps_den,
            self.generation(),
            None,
            None,
        )
    }

    pub fn close_project_at(
        &self,
        expected_generation: u64,
        run_id: Option<&str>,
        connection_id: Option<&str>,
    ) -> Result<ProjectStatus, AppError> {
        if let Some(run_id) = run_id {
            self.require_active_run_at(expected_generation, run_id)?;
        }
        if connection_id.is_none() {
            self.stop_bridge_before_lifecycle();
        }
        let _lifecycle = self
            .inner
            .lifecycle
            .lock()
            .map_err(|_| AppError::io("The application lifecycle lock is unavailable"))?;
        self.validate_generation(expected_generation)?;
        if let Some(run_id) = run_id {
            self.require_active_run_unlocked(expected_generation, run_id)?;
        }
        self.cancel_project_jobs(expected_generation, None);
        self.inner.render.retire_project()?;
        let mut active = self
            .inner
            .project
            .write()
            .map_err(|_| AppError::io("The project state lock is unavailable"))?;
        *active = None;
        let (generation, project_id, should_emit) = if let Some(connection_id) = connection_id {
            drop(active);
            drop(_lifecycle);
            self.stop_bridge_if_connection(connection_id);
            self.finish_project_transition(expected_generation, None)?
        } else {
            let generation = self.advance_generation_unlocked()?;
            drop(active);
            drop(_lifecycle);
            (generation, None, true)
        };
        if should_emit {
            self.emit_generation_changed(generation, project_id);
        }
        self.schedule_bridge_restart();
        self.status()
    }

    pub fn close_project(&self) -> Result<ProjectStatus, AppError> {
        self.close_project_at(self.generation(), None, None)
    }

    pub fn save_project_at(
        &self,
        generation: u64,
        run_id: Option<&str>,
    ) -> Result<ProjectStatus, AppError> {
        {
            let _lifecycle = self
                .inner
                .lifecycle
                .lock()
                .map_err(|_| AppError::io("The application lifecycle lock is unavailable"))?;
            self.validate_generation(generation)?;
            let project = self
                .inner
                .project
                .read()
                .map_err(|_| AppError::io("The project state lock is unavailable"))?;
            let open = project
                .as_ref()
                .ok_or_else(|| AppError::invalid_argument("No project is open"))?;
            if let Some(run_id) = run_id {
                self.require_active_run_unlocked(generation, run_id)?;
            }
            open.store.flush()?;
        }
        self.status()
    }

    pub fn save_project(&self) -> Result<ProjectStatus, AppError> {
        self.save_project_at(self.generation(), None)
    }

    pub fn create_project_dialog_at(
        &self,
        name: String,
        aspect: Option<String>,
        fps_num: Option<u64>,
        fps_den: Option<u64>,
        expected_generation: u64,
        run_id: Option<&str>,
        connection_id: Option<&str>,
    ) -> Result<ProjectStatus, AppError> {
        let name = validate_project_name(&name)?;
        let parent = rfd::FileDialog::new()
            .set_title("Choose a folder for the new Cutterhoochee project")
            .pick_folder()
            .ok_or_else(|| AppError::io("Project creation was cancelled"))?;
        let root = parent.join(format!("{name}.cutproj"));
        self.create_project_at(
            &root,
            name,
            aspect,
            fps_num,
            fps_den,
            expected_generation,
            run_id,
            connection_id,
        )
    }

    pub fn create_project_dialog(
        &self,
        name: String,
        aspect: Option<String>,
        fps_num: Option<u64>,
        fps_den: Option<u64>,
    ) -> Result<ProjectStatus, AppError> {
        self.create_project_dialog_at(
            name,
            aspect,
            fps_num,
            fps_den,
            self.generation(),
            None,
            None,
        )
    }

    pub fn open_project_dialog_at(
        &self,
        expected_generation: u64,
        run_id: Option<&str>,
        connection_id: Option<&str>,
    ) -> Result<ProjectStatus, AppError> {
        let root = rfd::FileDialog::new()
            .set_title("Open Cutterhoochee project")
            .pick_folder()
            .ok_or_else(|| AppError::io("Project opening was cancelled"))?;
        self.open_project_at(&root, expected_generation, run_id, connection_id)
    }

    pub fn open_project_dialog(&self) -> Result<ProjectStatus, AppError> {
        self.open_project_dialog_at(self.generation(), None, None)
    }
    fn advance_generation_for_bridge_replacement(
        &self,
        expected_generation: u64,
    ) -> Result<Option<(u64, Option<String>)>, AppError> {
        let _lifecycle = self
            .inner
            .lifecycle
            .lock()
            .map_err(|_| AppError::io("The application lifecycle lock is unavailable"))?;
        if self.generation() != expected_generation {
            return Ok(None);
        }
        let project_id = self
            .inner
            .project
            .read()
            .map_err(|_| AppError::io("The project state lock is unavailable"))?
            .as_ref()
            .map(|open| open.store.project_id());
        let generation = self.advance_generation_unlocked()?;
        Ok(Some((generation, project_id)))
    }

    pub async fn ensure_agent_bridge(&self) -> Result<AgentBridge, AppError> {
        let _start_guard = self.inner.bridge_start.lock().await;
        let existing = {
            let mut current = self
                .inner
                .bridge
                .lock()
                .map_err(|_| AppError::io("The agent bridge state lock is unavailable"))?;
            match current.as_ref() {
                Some(bridge)
                    if !bridge.is_terminated() && bridge.generation() == self.generation() =>
                {
                    return Ok(bridge.clone());
                }
                Some(_) => current.take(),
                None => None,
            }
        };
        if let Some(bridge) = existing {
            bridge.stop();
        }

        let observed_generation = self.generation();
        let replace_generation = {
            let marker = self
                .inner
                .bridge_generation
                .lock()
                .map_err(|_| AppError::io("The bridge generation marker is unavailable"))?;
            marker
                .as_ref()
                .is_some_and(|value| *value == observed_generation)
        };
        if replace_generation {
            if let Some((generation, project_id)) =
                self.advance_generation_for_bridge_replacement(observed_generation)?
            {
                self.emit_generation_changed(generation, project_id);
            }
        }

        let bridge = AgentBridge::spawn(self.clone(), self.paths().clone())?;
        if bridge.is_terminated() {
            return Err(AppError::io("The agent bridge ended during startup"));
        }
        if let Err(error) = self.configure_bridge(&bridge) {
            bridge.stop();
            return Err(error);
        }
        self.subscribe_private_events(&bridge);

        let winner =
            {
                let _lifecycle =
                    self.inner.lifecycle.lock().map_err(|_| {
                        AppError::io("The application lifecycle lock is unavailable")
                    })?;
                if bridge.generation() != self.generation() {
                    Err(AppError::stale_session(
                        "The agent bridge belongs to a retired generation",
                    ))
                } else {
                    let mut current =
                        self.inner.bridge.lock().map_err(|_| {
                            AppError::io("The agent bridge state lock is unavailable")
                        })?;
                    if let Some(existing) = current.as_ref() {
                        if !existing.is_terminated() && existing.generation() == bridge.generation()
                        {
                            Ok(Some(existing.clone()))
                        } else {
                            *current = Some(bridge.clone());
                            let mut marker = self.inner.bridge_generation.lock().map_err(|_| {
                                AppError::io("The bridge generation marker is unavailable")
                            })?;
                            *marker = Some(bridge.generation());
                            Ok(None)
                        }
                    } else {
                        *current = Some(bridge.clone());
                        let mut marker = self.inner.bridge_generation.lock().map_err(|_| {
                            AppError::io("The bridge generation marker is unavailable")
                        })?;
                        *marker = Some(bridge.generation());
                        Ok(None)
                    }
                }
            };
        match winner {
            Err(error) => {
                bridge.stop();
                Err(error)
            }
            Ok(Some(existing)) => {
                bridge.stop();
                Ok(existing)
            }
            Ok(None) => Ok(bridge),
        }
    }

    pub fn bridge(&self) -> Option<AgentBridge> {
        self.inner
            .bridge
            .lock()
            .ok()
            .and_then(|bridge| bridge.clone())
    }
    fn take_bridge_if_connection(&self, connection_id: &str) -> Option<AgentBridge> {
        self.inner.bridge.lock().ok().and_then(|mut current| {
            if current
                .as_ref()
                .is_some_and(|bridge| bridge.connection_id() == connection_id)
            {
                current.take()
            } else {
                None
            }
        })
    }

    fn stop_bridge_if_connection(&self, connection_id: &str) {
        if let Some(bridge) = self.take_bridge_if_connection(connection_id) {
            bridge.stop();
        }
    }

    pub fn clear_agent_bridge(&self, connection_id: &str) {
        if let Ok(mut bridge) = self.inner.bridge.lock() {
            if bridge
                .as_ref()
                .is_some_and(|current| current.connection_id() == connection_id)
            {
                *bridge = None;
            }
        }
    }
}

fn validate_timeline_selection(
    document: &ProjectDocument,
    selection: &TimelineSelection,
) -> Result<(), AppError> {
    validate_safe_integer(selection.playhead_frame, "selection.playheadFrame")?;
    let duration = document.duration_frames()?;
    if selection.playhead_frame > duration {
        return Err(AppError::invalid_argument(
            "selection.playheadFrame must be within the timeline",
        ));
    }
    let mut ids = HashSet::with_capacity(selection.clip_ids.len() + selection.text_ids.len());
    for id in &selection.clip_ids {
        let parsed = Uuid::parse_str(id)
            .map_err(|_| AppError::invalid_argument("selection.clipIds must contain UUIDs"))?;
        if parsed.is_nil() || !ids.insert(id.as_str()) {
            return Err(AppError::invalid_argument(
                "selection IDs must be unique UUIDs",
            ));
        }
        if !document.clips.iter().any(|clip| clip.id == *id) {
            return Err(AppError::invalid_argument(
                "selection.clipIds references an unknown clip",
            ));
        }
    }
    for id in &selection.text_ids {
        let parsed = Uuid::parse_str(id)
            .map_err(|_| AppError::invalid_argument("selection.textIds must contain UUIDs"))?;
        if parsed.is_nil() || !ids.insert(id.as_str()) {
            return Err(AppError::invalid_argument(
                "selection IDs must be unique UUIDs",
            ));
        }
        if !document.text_items.iter().any(|text| text.id == *id) {
            return Err(AppError::invalid_argument(
                "selection.textIds references an unknown text item",
            ));
        }
    }
    if let Some(range) = selection.range.as_ref() {
        validate_safe_integer(range.start_frame, "selection.range.startFrame")?;
        validate_safe_integer(range.end_frame, "selection.range.endFrame")?;
        if range.end_frame <= range.start_frame {
            return Err(AppError::invalid_argument(
                "selection.range endFrame must be greater than startFrame",
            ));
        }
        if range.end_frame > duration {
            return Err(AppError::invalid_argument(
                "selection.range must be within the timeline",
            ));
        }
    }
    Ok(())
}

fn timeline_snapshot_for(open: &OpenProject) -> Result<TimelineSnapshot, AppError> {
    let snapshot = open.store.snapshot()?;
    let duration_frames = snapshot.document.duration_frames()?;
    let mut selection = open
        .selection
        .lock()
        .map_err(|_| AppError::io("The timeline selection lock is unavailable"))?;
    selection
        .clip_ids
        .retain(|id| snapshot.document.clips.iter().any(|clip| clip.id == *id));
    selection.text_ids.retain(|id| {
        snapshot
            .document
            .text_items
            .iter()
            .any(|text| text.id == *id)
    });
    if selection.playhead_frame > duration_frames {
        selection.playhead_frame = duration_frames;
    }
    if selection.range.as_ref().is_some_and(|range| {
        range.start_frame >= range.end_frame || range.end_frame > duration_frames
    }) {
        selection.range = None;
    }
    Ok(TimelineSnapshot {
        revision: snapshot.document.revision,
        duration_frames,
        selection: selection.clone(),
        document: snapshot.document,
    })
}

fn validate_project_name(name: &str) -> Result<String, AppError> {
    let trimmed = name.trim();
    if trimmed.is_empty()
        || trimmed == "."
        || trimmed == ".."
        || trimmed.contains('/')
        || trimmed.contains('\\')
        || trimmed.chars().any(char::is_control)
    {
        return Err(AppError::invalid_argument(
            "Project name is not a valid folder name",
        ));
    }
    Ok(trimmed.to_owned())
}

fn sanitize_event_data(data: Option<Value>) -> Option<Value> {
    data.and_then(sanitize_event_value)
}

fn sanitize_event_value(value: Value) -> Option<Value> {
    match value {
        Value::Object(fields) => {
            let mut sanitized = serde_json::Map::with_capacity(fields.len());
            for (key, value) in fields {
                if is_sensitive_event_key(&key) {
                    continue;
                }
                if let Some(value) = sanitize_event_value(value) {
                    sanitized.insert(key, value);
                }
            }
            Some(Value::Object(sanitized))
        }
        Value::Array(values) => Some(Value::Array(
            values
                .into_iter()
                .filter_map(sanitize_event_value)
                .collect(),
        )),
        Value::String(value) if looks_like_secret(&value) => {
            Some(Value::String("[REDACTED]".to_owned()))
        }
        other => Some(other),
    }
}

fn is_sensitive_event_key(key: &str) -> bool {
    let key = key.to_ascii_lowercase();
    key == "thinking"
        || key == "reasoning"
        || key == "authorization"
        || key == "cookie"
        || key == "password"
        || key == "secret"
        || key == "api_key"
        || key == "apikey"
        || key == "access_token"
        || key == "refresh_token"
        || key.ends_with("_token")
}

fn looks_like_secret(value: &str) -> bool {
    let trimmed = value.trim();
    trimmed.starts_with("sk-")
        || trimmed.starts_with("sk-ant-")
        || trimmed.starts_with("ghp_")
        || trimmed.starts_with("github_pat_")
        || trimmed.starts_with("xoxb-")
        || trimmed
            .get(..7)
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case("bearer "))
}

fn private_required_string(params: &Value, field: &str) -> Result<String, AppError> {
    params
        .as_object()
        .and_then(|object| object.get(field))
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
        .ok_or_else(|| AppError::invalid_argument(format!("{field} is required")))
}

fn private_required_u64(params: &Value, field: &str) -> Result<u64, AppError> {
    let value = params
        .as_object()
        .and_then(|object| object.get(field))
        .and_then(Value::as_u64)
        .ok_or_else(|| AppError::invalid_argument(format!("{field} must be a safe integer")))?;
    validate_safe_integer(value, field)?;
    Ok(value)
}

fn private_require_empty_object(params: &Value) -> Result<(), AppError> {
    if params.as_object().is_some_and(serde_json::Map::is_empty) {
        Ok(())
    } else {
        Err(AppError::invalid_argument(
            "The private credential request parameters are invalid",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::editor::dispatcher::{dispatch, CallerContext};
    use crate::editor::operations::EditOp;
    use crate::ipc::{EditorReply, EditorRequest};
    use crate::project::model::{
        AssetKind, FitMode, MediaClip, NormalizedAsset, NormalizedVideo, OriginalMediaMetadata,
        OriginalStreamKind, OriginalStreamMetadata, TrackKind,
    };
    use std::path::{Path, PathBuf};
    use std::sync::atomic::Ordering;
    use std::sync::mpsc;
    use std::thread;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    fn fixture_root() -> PathBuf {
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        std::env::temp_dir().join(format!("cutterhoochee-dispatch-{suffix}"))
    }

    fn fixture_paths(root: &Path) -> AgentPaths {
        let resource_dir = root.join("resources");
        AgentPaths {
            node_path: resource_dir.join("node"),
            agent_entrypoint: resource_dir.join("agent.js"),
            resource_dir,
            app_data_dir: root.join("app-data"),
            app_cache_dir: root.join("app-cache"),
            agent_home: root.join("app-data/agent-home"),
            agent_config_dir: root.join("app-data/agent-config"),
            agent_cache_dir: root.join("app-cache/agent-cache"),
            session_dir: root.join("app-data/sessions"),
            temp_dir: root.join("app-cache/tmp"),
            artifact_dir: root.join("app-cache/artifacts"),
        }
    }

    fn ready_video_asset(id: &str) -> AssetManifest {
        AssetManifest {
            id: id.to_owned(),
            kind: AssetKind::Video,
            content_hash: "dispatch-test-hash".to_owned(),
            original: OriginalMediaMetadata {
                file_name: "red.mp4".to_owned(),
                streams: vec![OriginalStreamMetadata {
                    kind: OriginalStreamKind::Video,
                    codec: "h264".to_owned(),
                    duration_ms: Some(1_000),
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
                    master_artifact_id: "dispatch-test-master".to_owned(),
                    proxy_artifact_id: None,
                    frame_count: 30,
                    width: 1_920,
                    height: 1_080,
                    fps_num: 30,
                    fps_den: 1,
                    active_start_frame: 0,
                    active_end_frame: 30,
                    source_start_ms: 0,
                    source_end_ms: 1_000,
                    proxy_frame_count: Some(30),
                }),
                audio: None,
            }),
        }
    }

    fn dispatch_fixture() -> (AppState, PathBuf, String, String) {
        let root = fixture_root();
        let paths = fixture_paths(&root);
        let state = AppState::new(paths).expect("app state");
        let project_root = root.join("Manual Proof.cutproj");
        let store = ProjectStore::create(
            &project_root,
            &state.paths().app_data_dir,
            "Manual Proof",
            Some("16:9"),
            30,
            1,
        )
        .expect("project store");
        let asset_id = "10000000-0000-4000-8000-000000000001";
        store
            .commit(
                "seed-asset".to_owned(),
                0,
                "Seed asset".to_owned(),
                "seed-asset-payload".to_owned(),
                move |document| {
                    document.assets.push(ready_video_asset(asset_id));
                    Ok(())
                },
            )
            .expect("seed asset");
        let snapshot = store.snapshot().expect("seed snapshot");
        let track_id = snapshot
            .document
            .tracks
            .iter()
            .find(|track| track.kind == TrackKind::Video)
            .expect("main video track")
            .id
            .clone();
        let project_id = store.project_id();
        {
            let mut project = state.inner.project.write().expect("project state");
            *project = Some(OpenProject {
                store,
                selection: Mutex::new(TimelineSelection::empty()),
            });
        }
        state.inner.generation.store(1, Ordering::Release);
        (state, root, project_id, track_id)
    }

    fn dispatch_with_timeout(
        state: AppState,
        caller: CallerContext,
        request: EditorRequest,
    ) -> Result<EditorReply, AppError> {
        let (sender, receiver) = mpsc::channel();
        let worker = thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("test runtime");
            let result = runtime.block_on(dispatch(request, caller, &state));
            let _ = sender.send(result);
        });
        let result = receiver
            .recv_timeout(Duration::from_secs(2))
            .expect("edit_project dispatch did not complete");
        worker.join().expect("dispatch worker");
        result
    }

    #[test]
    fn edit_project_insert_clip_dispatch_commits_and_status_remains_responsive() {
        let (state, _root, project_id, track_id) = dispatch_fixture();
        let generation = state.generation();
        let caller = CallerContext::human_window(
            "dispatch-regression".to_owned(),
            generation,
            Some(project_id),
        );
        let request = EditorRequest::EditProject {
            transaction_id: "dispatch-insert-clip".to_owned(),
            expected_revision: 1,
            label: "Insert clip".to_owned(),
            operations: vec![EditOp::InsertClip {
                clip: MediaClip {
                    id: "20000000-0000-4000-8000-000000000001".to_owned(),
                    track_id,
                    asset_id: "10000000-0000-4000-8000-000000000001".to_owned(),
                    start_frame: 0,
                    in_frame: 0,
                    duration_frames: 30,
                    fit: FitMode::Contain,
                    center_x: 5_000,
                    center_y: 5_000,
                    scale: 10_000,
                    opacity: 10_000,
                    gain_db: 0.0,
                    audio_enabled: false,
                    fade_in_frames: 0,
                    fade_out_frames: 0,
                },
            }],
            dry_run: None,
        };
        let edit =
            dispatch_with_timeout(state.clone(), caller.clone(), request).expect("insert clip");
        match edit {
            EditorReply::ProjectEdit(result) => {
                assert!(result.changed);
                assert_eq!(result.revision, 2);
            }
            other => panic!("unexpected edit reply: {other:?}"),
        }

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("status runtime");
        let status = runtime
            .block_on(dispatch(
                EditorRequest::ProjectStatus {},
                caller.clone(),
                &state,
            ))
            .expect("status");
        match status {
            EditorReply::ProjectStatus(status) => {
                assert!(status.open);
                assert_eq!(status.revision, Some(2));
            }
            other => panic!("unexpected status reply: {other:?}"),
        }
        let snapshot = runtime
            .block_on(dispatch(EditorRequest::ProjectSnapshot {}, caller, &state))
            .expect("snapshot");
        match snapshot {
            EditorReply::ProjectSnapshot(snapshot) => {
                assert_eq!(snapshot.document.revision, 2);
                assert_eq!(snapshot.document.clips.len(), 1);
            }
            other => panic!("unexpected snapshot reply: {other:?}"),
        }
    }
}
