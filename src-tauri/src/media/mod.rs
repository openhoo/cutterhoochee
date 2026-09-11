//! Native media ingress, artifact ownership, evidence and runtime actions.
//!
//! This module is the leaf boundary used by the shared editor dispatcher. It
//! owns media preparation and commits only verified manifests through the
//! existing AppState serialized writer before reporting a job as completed.

pub mod analysis;
pub mod artifacts;
pub mod evidence;
pub mod export;
pub mod ffmpeg;
pub mod graphics;
pub mod ingest;
pub mod jobs;
pub mod probe;
pub mod render;
pub mod render_plan;
pub mod transcribe;

use crate::editor::dispatcher::{CallerContext, CallerKind};
use crate::error::{AppError, ErrorCode};
use crate::permissions::{FileGrant, FileGrantPurpose};
use crate::project::model::{AssetManifest, ProjectProfile};
use crate::state::AppState;
use ingest::{make_thumbnail, make_waveform, prepare_asset_with_identity};
use probe::{capture_identity, resolve_packaged_binary, revalidate_identity};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;
use ts_rs::TS;
use uuid::Uuid;

pub use artifacts::{ArtifactKind, ArtifactRecord, ArtifactStore};
pub use evidence::{EvidenceAction, EvidenceReply, EvidenceRuntime};
pub use graphics::{CreateGraphicRequest, GraphicReply, GraphicsRuntime};
pub use ingest::{IngestOptions, PreparedAsset};
pub use jobs::{
    JobContext, JobEventHook, JobNotificationHook, JobPriority, JobRegistry, JobSpec, JobState,
    JobSummary,
};
pub use probe::{OriginalIdentity, ProbeResult, ProbeStream};
pub use render::{
    PreviewAction, PreviewAudioReply, PreviewFrameReply, PreviewReply, RenderRuntime,
};
pub use render_plan::{ArtifactResolver, RenderPlan};
pub use transcribe::{SpeechModelInfo, TranscribeReply, TranscribeRequest, TranscribeRuntime};
#[derive(Debug)]
enum NormalizationState {
    Running,
    Ready(PreparedAsset),
    Failed(Option<AppError>),
}
fn transient_normalization_error(error: &AppError) -> bool {
    matches!(
        error.code,
        ErrorCode::JobCancelled | ErrorCode::StaleSession
    )
}

struct NormalizationClaim {
    state: Mutex<NormalizationState>,
    wake: Condvar,
}

impl NormalizationClaim {
    fn new() -> Self {
        Self {
            state: Mutex::new(NormalizationState::Running),
            wake: Condvar::new(),
        }
    }
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
#[ts(tag = "action", rename_all = "snake_case")]
pub enum MediaAction {
    List {},
    Inspect {
        #[serde(rename = "assetId")]
        #[ts(rename = "assetId")]
        asset_id: String,
    },
    Import {
        #[serde(skip_serializing_if = "Option::is_none")]
        #[ts(optional)]
        paths: Option<Vec<String>>,
    },
    Relink {
        #[serde(rename = "assetId")]
        #[ts(rename = "assetId")]
        asset_id: String,
    },
    Remove {
        #[serde(rename = "assetId")]
        #[ts(rename = "assetId")]
        asset_id: String,
    },
    Thumbnail {
        #[serde(rename = "assetId")]
        #[ts(rename = "assetId")]
        asset_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[ts(optional, type = "SafeInteger")]
        frame: Option<u64>,
    },
    Waveform {
        #[serde(rename = "assetId")]
        #[ts(rename = "assetId")]
        asset_id: String,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(tag = "kind", content = "data", rename_all = "snake_case")]
#[ts(tag = "kind", content = "data", rename_all = "snake_case")]
pub enum MediaReply {
    List(MediaListReply),
    Inspect(MediaInspectReply),
    Import(MediaImportReply),
    Relink(MediaAssetReply),
    Remove(MediaAssetReply),
    Thumbnail(MediaArtifactReply),
    Waveform(MediaArtifactReply),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[ts(rename_all = "camelCase")]
pub struct MediaListReply {
    pub assets: Vec<AssetManifest>,
    pub jobs: Vec<JobSummary>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[ts(rename_all = "camelCase")]
pub struct MediaInspectReply {
    pub asset: AssetManifest,
    pub identity: OriginalIdentity,
    pub original_available: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[ts(rename_all = "camelCase")]
pub struct MediaImportReply {
    /// Already-ready assets returned from content/profile dedupe. Newly
    /// authorized media is represented by `job_ids` until completion.
    pub assets: Vec<AssetManifest>,
    pub job_ids: Vec<String>,
    pub failed: Vec<ImportFailure>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[ts(rename_all = "camelCase")]
pub struct ImportFailure {
    pub path: String,
    pub error: AppError,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[ts(rename_all = "camelCase")]
pub struct MediaAssetReply {
    pub asset_id: String,
    pub removed: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub asset: Option<AssetManifest>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub job_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[ts(rename_all = "camelCase")]
pub struct MediaArtifactReply {
    pub asset_id: String,
    pub artifact: ArtifactRecord,
    pub artifact_url: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
#[ts(tag = "action", rename_all = "snake_case")]
pub enum JobsAction {
    List {},
    Get {
        #[serde(rename = "jobId")]
        #[ts(rename = "jobId")]
        job_id: String,
    },
    Cancel {
        #[serde(rename = "jobId")]
        #[ts(rename = "jobId")]
        job_id: String,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(tag = "kind", content = "data", rename_all = "snake_case")]
#[ts(tag = "kind", content = "data", rename_all = "snake_case")]
pub enum JobsReply {
    List(JobListReply),
    Get(JobGetReply),
    Cancel(JobCancelReply),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[ts(rename_all = "camelCase")]
pub struct JobListReply {
    pub jobs: Vec<JobSummary>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[ts(rename_all = "camelCase")]
pub struct JobGetReply {
    pub job: JobSummary,
    pub result_available: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[ts(rename_all = "camelCase")]
pub struct JobCancelReply {
    pub job: JobSummary,
}

#[derive(Clone)]
pub struct JobsRuntime {
    registry: JobRegistry,
}

impl Default for JobsRuntime {
    fn default() -> Self {
        Self::new()
    }
}

impl JobsRuntime {
    pub fn new() -> Self {
        Self {
            registry: JobRegistry::new(),
        }
    }

    pub fn from_registry(registry: JobRegistry) -> Self {
        Self { registry }
    }

    pub fn registry(&self) -> JobRegistry {
        self.registry.clone()
    }

    pub async fn handle(
        &self,
        action: &JobsAction,
        caller: &CallerContext,
        state: &AppState,
    ) -> Result<JobsReply, AppError> {
        state.validate_generation(caller.generation)?;
        match action {
            JobsAction::List {} => Ok(JobsReply::List(JobListReply {
                jobs: self.registry.list(),
            })),
            JobsAction::Get { job_id } => {
                let job = self.registry.get(job_id)?;
                Ok(JobsReply::Get(JobGetReply {
                    result_available: self.registry.result(job_id)?.is_some(),
                    job,
                }))
            }
            JobsAction::Cancel { job_id } => Ok(JobsReply::Cancel(JobCancelReply {
                job: self.registry.cancel(job_id)?,
            })),
        }
    }
}

pub struct MediaRuntime {
    jobs: JobRegistry,
    /// Normalization identity -> in-flight job ID. The lock is held while a
    /// job is submitted and registered so a repeated request shares it.
    pending: Arc<Mutex<HashMap<String, String>>>,
    ready: Arc<Mutex<HashMap<String, AssetManifest>>>,
    normalizing: Arc<Mutex<HashMap<String, Arc<NormalizationClaim>>>>,
    publish: Arc<Mutex<()>>,
}

impl Default for MediaRuntime {
    fn default() -> Self {
        Self::new()
    }
}

impl MediaRuntime {
    pub fn new() -> Self {
        Self {
            jobs: JobRegistry::new(),
            pending: Arc::new(Mutex::new(HashMap::new())),
            ready: Arc::new(Mutex::new(HashMap::new())),
            normalizing: Arc::new(Mutex::new(HashMap::new())),
            publish: Arc::new(Mutex::new(())),
        }
    }

    pub fn jobs(&self) -> JobRegistry {
        self.jobs.clone()
    }

    pub fn artifact_store(&self, state: &AppState) -> Result<ArtifactStore, AppError> {
        let store = state.current_store()?;
        ArtifactStore::for_project(store.root(), store.workspace_id())
    }

    pub async fn handle(
        &self,
        action: &MediaAction,
        caller: &CallerContext,
        state: &AppState,
    ) -> Result<MediaReply, AppError> {
        state.validate_generation(caller.generation)?;
        match action {
            MediaAction::List {} => {
                let snapshot = state.snapshot_at(caller.generation)?;
                Ok(MediaReply::List(MediaListReply {
                    assets: snapshot.document.assets,
                    jobs: self.jobs.list(),
                }))
            }
            MediaAction::Inspect { asset_id } => {
                let asset = find_asset(state, caller.generation, asset_id)?;
                let identity = identity_from_manifest(&asset)?;
                let original_available = state
                    .permissions()
                    .scope_for_state(state)
                    .ok()
                    .and_then(|scope| {
                        state
                            .permissions()
                            .find_valid_grant(
                                &scope,
                                Path::new(&identity.canonical_path),
                                FileGrantPurpose::Import,
                            )
                            .or_else(|_| {
                                state.permissions().find_valid_grant(
                                    &scope,
                                    Path::new(&identity.canonical_path),
                                    FileGrantPurpose::Relink,
                                )
                            })
                            .ok()
                    })
                    .is_some();
                Ok(MediaReply::Inspect(MediaInspectReply {
                    asset,
                    identity,
                    original_available,
                }))
            }
            MediaAction::Import { paths } => self.import(paths.clone(), caller, state).await,
            MediaAction::Relink { asset_id } => self.relink(asset_id.clone(), caller, state).await,
            MediaAction::Remove { asset_id } => {
                let snapshot = state.snapshot_at(caller.generation)?;
                let asset_id_for_commit = asset_id.clone();
                let transaction_id = Uuid::new_v4().to_string();
                let run_id = caller.run_id().map(str::to_owned);
                state.commit_at_with_run(
                    caller.generation,
                    run_id.as_deref(),
                    transaction_id,
                    snapshot.document.revision,
                    "Remove media".to_owned(),
                    format!("media-remove:{asset_id}"),
                    move |document| {
                        if document
                            .clips
                            .iter()
                            .any(|clip| clip.asset_id == asset_id_for_commit)
                        {
                            return Err(AppError::invalid_argument(
                                "Remove the asset's clips before removing its media",
                            ));
                        }
                        let before = document.assets.len();
                        document
                            .assets
                            .retain(|asset| asset.id != asset_id_for_commit);
                        if document.assets.len() == before {
                            return Err(AppError::invalid_argument(
                                "The media asset does not exist",
                            ));
                        }
                        document.validate()
                    },
                )?;
                Ok(MediaReply::Remove(MediaAssetReply {
                    asset_id: asset_id.clone(),
                    removed: true,
                    asset: None,
                    job_id: None,
                }))
            }
            MediaAction::Thumbnail { asset_id, frame } => {
                self.thumbnail(
                    asset_id.clone(),
                    frame.as_ref().copied().unwrap_or(0),
                    caller,
                    state,
                )
                .await
            }
            MediaAction::Waveform { asset_id } => {
                self.waveform(asset_id.clone(), caller, state).await
            }
        }
    }

    async fn import(
        &self,
        requested_paths: Option<Vec<String>>,
        caller: &CallerContext,
        state: &AppState,
    ) -> Result<MediaReply, AppError> {
        let paths = requested_paths
            .as_ref()
            .map(|items| items.iter().map(PathBuf::from).collect::<Vec<_>>());
        let display_paths = paths
            .as_ref()
            .map(|items| {
                items
                    .iter()
                    .map(|path| path.to_string_lossy().into_owned())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let grants = match self
            .authorize_paths(paths, caller, state, FileGrantPurpose::Import)
            .await
        {
            Ok(grants) => grants,
            Err(error) if requested_paths.is_some() => {
                return Ok(MediaReply::Import(MediaImportReply {
                    assets: Vec::new(),
                    job_ids: Vec::new(),
                    failed: display_paths
                        .into_iter()
                        .map(|path| ImportFailure {
                            path,
                            error: error.clone(),
                        })
                        .collect(),
                }))
            }
            Err(error) => return Err(error),
        };
        if grants.is_empty() {
            return Err(AppError::io("Media import was cancelled"));
        }
        let snapshot = state.snapshot_at(caller.generation)?;
        let options = self.options(
            state,
            snapshot.document.profile.clone(),
            self.artifact_store(state)?,
        )?;
        let mut assets = Vec::new();
        let mut job_ids = Vec::new();
        let mut failed = Vec::new();
        for grant in grants {
            let display = grant.path.clone();
            let generation = caller.generation;
            let run_id = caller.run_id().map(str::to_owned);
            let project_id = snapshot.document.project_id.clone();
            let key = pending_key(
                &grant,
                &options,
                None,
                generation,
                Some(&project_id),
                run_id.as_deref(),
            );
            let mut pending = self
                .pending
                .lock()
                .map_err(|_| AppError::io("The media import registry is unavailable"))?;
            if let Some(asset) = self
                .ready
                .lock()
                .map_err(|_| AppError::io("The media import registry is unavailable"))?
                .get(&key)
                .cloned()
            {
                if snapshot
                    .document
                    .assets
                    .iter()
                    .find(|current| current.id == asset.id)
                    .is_some_and(|current| same_normalization(current, &asset, &options))
                {
                    assets.push(asset);
                    continue;
                }
            }
            if let Some(job_id) = pending.get(&key).cloned() {
                job_ids.push(job_id);
                continue;
            }
            let scope = state.permissions().scope_for_state(state)?;
            let grant_id = grant.grant_id.clone();
            let permissions = state.permissions().clone();
            let state_for_job = state.clone();
            let options_for_job = options.clone();
            let pending_for_job = self.pending.clone();
            let ready_for_job = self.ready.clone();
            let normalizing_for_job = self.normalizing.clone();
            let publish = self.publish.clone();
            let key_for_job = key.clone();
            let notification = notification_hook(state.clone(), generation, run_id.clone());
            let event = event_hook(state.clone(), generation, run_id.clone());
            let run_id_for_job = run_id.clone();
            let spec = JobSpec::new(
                "media_import",
                JobPriority::Background,
                generation,
                Some(project_id.clone()),
            )?
            .with_run_id(run_id.clone());
            let job = match self.jobs.submit_with_hooks(
                spec,
                Some(notification),
                Some(event),
                move |context| {
                    let result = (|| -> Result<Value, AppError> {
                        state_for_job.validate_generation(generation)?;
                        if let Some(run_id) = run_id_for_job.as_deref() {
                            permissions.require_active_run(
                                generation,
                                Some(&project_id),
                                run_id,
                            )?;
                        }
                        let approved = permissions.validate_grant(
                            &scope,
                            &grant_id,
                            Some(FileGrantPurpose::Import),
                        )?;
                        let input = Path::new(&approved.path);
                        let identity = capture_identity(input)?;
                        revalidate_identity(&identity)?;
                        let normalization_id = normalization_key(
                            &identity,
                            &options_for_job,
                            generation,
                            Some(&project_id),
                        );
                        let mut prepared = prepare_shared_asset(
                            &context,
                            &options_for_job,
                            input,
                            &identity,
                            &normalization_id,
                            &normalizing_for_job,
                        )?;
                        context.check_cancelled()?;
                        let _publish = publish.lock().map_err(|_| {
                            AppError::io("The media publication lock is unavailable")
                        })?;
                        state_for_job.validate_generation(generation)?;
                        if let Some(run_id) = run_id_for_job.as_deref() {
                            permissions.require_active_run(
                                generation,
                                Some(&project_id),
                                run_id,
                            )?;
                        }
                        let existing = state_for_job
                            .snapshot_at(generation)?
                            .document
                            .assets
                            .into_iter()
                            .find(|asset| {
                                same_normalization(asset, &prepared.asset, &options_for_job)
                            });
                        if let Some(existing) = existing {
                            prepared.asset = existing;
                        } else {
                            state_for_job.commit_asset_at(
                                generation,
                                run_id_for_job.as_deref(),
                                Uuid::new_v4().to_string(),
                                prepared.asset.clone(),
                                None,
                            )?;
                        }
                        {
                            if let Ok(mut pending_entries) = pending_for_job.lock() {
                                if let Ok(mut ready_entries) = ready_for_job.lock() {
                                    ready_entries
                                        .insert(key_for_job.clone(), prepared.asset.clone());
                                }
                                pending_entries.remove(&key_for_job);
                            }
                        }
                        serde_json::to_value(prepared).map_err(|_| {
                            AppError::schema("The prepared asset could not be encoded")
                        })
                    })();
                    if let Ok(mut entries) = pending_for_job.lock() {
                        entries.remove(&key_for_job);
                    }
                    result
                },
            ) {
                Ok(job) => job,
                Err(error) => {
                    pending.remove(&key);
                    failed.push(ImportFailure {
                        path: display,
                        error,
                    });
                    continue;
                }
            };
            let job_id = job.job_id.clone();
            pending.insert(key, job_id.clone());
            drop(pending);
            let _ = state.emit_sanitized_event(
                "media_job_queued",
                run_id,
                Some(json!({
                    "jobId": job.job_id,
                    "kind": job.kind,
                    "state": job.state,
                    "progress": job.progress,
                    "generation": job.generation,
                    "assetPath": display,
                })),
            );
            job_ids.push(job_id);
        }
        Ok(MediaReply::Import(MediaImportReply {
            assets,
            job_ids,
            failed,
        }))
    }

    async fn relink(
        &self,
        asset_id: String,
        caller: &CallerContext,
        state: &AppState,
    ) -> Result<MediaReply, AppError> {
        let _existing = find_asset(state, caller.generation, &asset_id)?;
        let grants = self
            .authorize_paths(None, caller, state, FileGrantPurpose::Relink)
            .await?;
        let grant = grants
            .into_iter()
            .next()
            .ok_or_else(|| AppError::io("Media relink was cancelled"))?;
        let snapshot = state.snapshot_at(caller.generation)?;
        let options = self.options(
            state,
            snapshot.document.profile.clone(),
            self.artifact_store(state)?,
        )?;
        let generation = caller.generation;
        let run_id = caller.run_id().map(str::to_owned);
        let project_id = snapshot.document.project_id.clone();
        let key = pending_key(
            &grant,
            &options,
            Some(&asset_id),
            generation,
            Some(&project_id),
            run_id.as_deref(),
        );
        let mut pending = self
            .pending
            .lock()
            .map_err(|_| AppError::io("The media import registry is unavailable"))?;
        if let Some(job_id) = pending.get(&key).cloned() {
            return Ok(MediaReply::Relink(MediaAssetReply {
                asset_id,
                removed: false,
                asset: None,
                job_id: Some(job_id),
            }));
        }
        let scope = state.permissions().scope_for_state(state)?;
        let grant_id = grant.grant_id.clone();
        let permissions = state.permissions().clone();
        let state_for_job = state.clone();
        let options_for_job = options.clone();
        let asset_id_for_job = asset_id.clone();
        let pending_for_job = self.pending.clone();
        let normalizing_for_job = self.normalizing.clone();
        let key_for_job = key.clone();
        let run_id_for_job = run_id.clone();
        let notification = notification_hook(state.clone(), generation, run_id.clone());
        let event = event_hook(state.clone(), generation, run_id.clone());
        let spec = JobSpec::new(
            "media_relink",
            JobPriority::Background,
            generation,
            Some(project_id.clone()),
        )?
        .with_run_id(run_id.clone());
        let job = match self.jobs.submit_with_hooks(
            spec,
            Some(notification),
            Some(event),
            move |context| {
                let result = (|| -> Result<Value, AppError> {
                    state_for_job.validate_generation(generation)?;
                    if let Some(run_id) = run_id_for_job.as_deref() {
                        permissions.require_active_run(generation, Some(&project_id), run_id)?;
                    }
                    let approved = permissions.validate_grant(
                        &scope,
                        &grant_id,
                        Some(FileGrantPurpose::Relink),
                    )?;
                    let input = Path::new(&approved.path);
                    let identity = capture_identity(input)?;
                    revalidate_identity(&identity)?;
                    let normalization_id = normalization_key(
                        &identity,
                        &options_for_job,
                        generation,
                        Some(&project_id),
                    );
                    let mut prepared = prepare_shared_asset(
                        &context,
                        &options_for_job,
                        input,
                        &identity,
                        &normalization_id,
                        &normalizing_for_job,
                    )?;
                    context.check_cancelled()?;
                    prepared.asset.id = asset_id_for_job.clone();
                    state_for_job.validate_generation(generation)?;
                    if let Some(run_id) = run_id_for_job.as_deref() {
                        permissions.require_active_run(generation, Some(&project_id), run_id)?;
                    }
                    state_for_job.commit_asset_at(
                        generation,
                        run_id_for_job.as_deref(),
                        Uuid::new_v4().to_string(),
                        prepared.asset.clone(),
                        Some(asset_id_for_job.clone()),
                    )?;
                    serde_json::to_value(prepared)
                        .map_err(|_| AppError::schema("The relinked asset could not be encoded"))
                })();
                if let Ok(mut entries) = pending_for_job.lock() {
                    entries.remove(&key_for_job);
                }
                result
            },
        ) {
            Ok(job) => job,
            Err(error) => {
                pending.remove(&key);
                return Err(error);
            }
        };
        let job_id = job.job_id.clone();
        pending.insert(key, job_id.clone());
        drop(pending);
        let _ = state.emit_sanitized_event(
            "media_job_queued",
            run_id,
            Some(json!({
                "jobId": job.job_id,
                "kind": job.kind,
                "state": job.state,
                "progress": job.progress,
                "generation": job.generation,
                "assetId": asset_id,
            })),
        );
        Ok(MediaReply::Relink(MediaAssetReply {
            asset_id,
            removed: false,
            asset: None,
            job_id: Some(job_id),
        }))
    }

    async fn authorize_paths(
        &self,
        paths: Option<Vec<PathBuf>>,
        caller: &CallerContext,
        state: &AppState,
        purpose: FileGrantPurpose,
    ) -> Result<Vec<FileGrant>, AppError> {
        state.validate_generation(caller.generation)?;
        let scope = state.permissions().scope_for_state(state)?;
        if scope.generation != caller.generation || scope.project_id != caller.project_id {
            return Err(AppError::stale_session(
                "The file approval belongs to another project generation",
            ));
        }
        match paths {
            None => state
                .permissions()
                .grant_files(caller, scope, None, purpose),
            Some(paths) => {
                if paths.is_empty() {
                    return Err(AppError::invalid_argument("At least one file is required"));
                }
                if paths.iter().any(|path| !path.is_absolute()) {
                    return Err(AppError::new(
                        ErrorCode::PermissionDenied,
                        "Imported media paths must be absolute",
                    ));
                }
                match &caller.kind {
                    CallerKind::HumanWindow { .. } => paths
                        .iter()
                        .map(|path| state.permissions().find_valid_grant(&scope, path, purpose))
                        .collect(),
                    CallerKind::AgentSidecar { .. } => {
                        let operation = state.permissions().request_file_grant(
                            caller,
                            scope.clone(),
                            caller.run_id().map(str::to_owned),
                            paths,
                            purpose,
                        )?;
                        let allowed = match state
                            .permissions()
                            .await_decision(&operation.operation_id)
                            .await
                        {
                            Ok(allowed) => allowed,
                            Err(error) => {
                                let _ = state
                                    .permissions()
                                    .revoke_operation(&operation.operation_id);
                                return Err(error);
                            }
                        };
                        if !allowed {
                            let _ = state
                                .permissions()
                                .revoke_operation(&operation.operation_id);
                            return Err(AppError::new(
                                ErrorCode::PermissionDenied,
                                "The native file approval was denied",
                            ));
                        }
                        state.permissions().consume_file_grant(
                            caller,
                            &scope,
                            caller.run_id(),
                            &operation.operation_id,
                        )
                    }
                }
            }
        }
    }

    async fn thumbnail(
        &self,
        asset_id: String,
        frame: u64,
        caller: &CallerContext,
        state: &AppState,
    ) -> Result<MediaReply, AppError> {
        let asset = find_asset(state, caller.generation, &asset_id)?;
        let options = self.options(
            state,
            state.snapshot_at(caller.generation)?.document.profile,
            self.artifact_store(state)?,
        )?;
        let spec = JobSpec::new(
            "thumbnail",
            JobPriority::Foreground,
            caller.generation,
            caller.project_id.clone(),
        )?
        .with_run_id(caller.run_id().map(str::to_owned));
        let options_for_job = options.clone();
        let asset_for_job = asset.clone();
        let job = self.jobs.submit(spec, move |context| {
            let artifact = make_thumbnail(&context, &options_for_job, &asset_for_job, frame)?;
            serde_json::to_value(artifact)
                .map_err(|_| AppError::schema("The thumbnail result was malformed"))
        })?;
        let result = self
            .jobs
            .wait_blocking(&job.job_id, std::time::Duration::from_secs(5 * 60))?;
        let artifact: ArtifactRecord = serde_json::from_value(result.data)
            .map_err(|_| AppError::schema("The thumbnail result was malformed"))?;
        let url = options
            .store
            .url(&artifact.artifact_id, caller.generation)?;
        Ok(MediaReply::Thumbnail(MediaArtifactReply {
            asset_id,
            artifact,
            artifact_url: url,
        }))
    }

    async fn waveform(
        &self,
        asset_id: String,
        caller: &CallerContext,
        state: &AppState,
    ) -> Result<MediaReply, AppError> {
        let asset = find_asset(state, caller.generation, &asset_id)?;
        let options = self.options(
            state,
            state.snapshot_at(caller.generation)?.document.profile,
            self.artifact_store(state)?,
        )?;
        let spec = JobSpec::new(
            "waveform",
            JobPriority::Foreground,
            caller.generation,
            caller.project_id.clone(),
        )?
        .with_run_id(caller.run_id().map(str::to_owned));
        let options_for_job = options.clone();
        let asset_for_job = asset.clone();
        let job = self.jobs.submit(spec, move |_context| {
            let artifact = make_waveform(&options_for_job, &asset_for_job)?;
            serde_json::to_value(artifact)
                .map_err(|_| AppError::schema("The waveform result was malformed"))
        })?;
        let result = self
            .jobs
            .wait_blocking(&job.job_id, std::time::Duration::from_secs(5 * 60))?;
        let artifact: ArtifactRecord = serde_json::from_value(result.data)
            .map_err(|_| AppError::schema("The waveform result was malformed"))?;
        let url = options
            .store
            .url(&artifact.artifact_id, caller.generation)?;
        Ok(MediaReply::Waveform(MediaArtifactReply {
            asset_id,
            artifact,
            artifact_url: url,
        }))
    }

    fn options(
        &self,
        state: &AppState,
        profile: ProjectProfile,
        store: ArtifactStore,
    ) -> Result<IngestOptions, AppError> {
        let ffmpeg = resolve_packaged_binary(&state.paths().resource_dir, "ffmpeg")?;
        let ffprobe = resolve_packaged_binary(&state.paths().resource_dir, "ffprobe")?;
        Ok(IngestOptions {
            profile,
            renderer_version: ingest::default_renderer_version().to_owned(),
            ffmpeg,
            ffprobe,
            store,
        })
    }
}

fn prepare_shared_asset(
    context: &JobContext,
    options: &IngestOptions,
    input: &Path,
    identity: &OriginalIdentity,
    normalization_id: &str,
    claims: &Arc<Mutex<HashMap<String, Arc<NormalizationClaim>>>>,
) -> Result<PreparedAsset, AppError> {
    loop {
        let (claim, owner) = {
            let mut entries = claims
                .lock()
                .map_err(|_| AppError::io("The media normalization registry is unavailable"))?;
            if let Some(claim) = entries.get(normalization_id) {
                (claim.clone(), false)
            } else {
                let claim = Arc::new(NormalizationClaim::new());
                entries.insert(normalization_id.to_owned(), claim.clone());
                (claim, true)
            }
        };
        if owner {
            let result = prepare_asset_with_identity(context, options, input, identity);
            {
                let mut state = claim
                    .state
                    .lock()
                    .map_err(|_| AppError::io("The media normalization state is unavailable"))?;
                *state = match &result {
                    Ok(prepared) => NormalizationState::Ready(prepared.clone()),
                    Err(error) if transient_normalization_error(error) => {
                        NormalizationState::Failed(None)
                    }
                    Err(error) => NormalizationState::Failed(Some(error.clone())),
                };
            }
            if result.is_err() {
                if let Ok(mut entries) = claims.lock() {
                    entries.remove(normalization_id);
                }
            }
            claim.wake.notify_all();
            return result;
        }

        loop {
            let state = claim
                .state
                .lock()
                .map_err(|_| AppError::io("The media normalization state is unavailable"))?;
            match &*state {
                NormalizationState::Ready(prepared) => return Ok(prepared.clone()),
                NormalizationState::Failed(Some(error)) => return Err(error.clone()),
                NormalizationState::Failed(None) => break,
                NormalizationState::Running => {
                    let (state, _) = claim
                        .wake
                        .wait_timeout(state, Duration::from_millis(50))
                        .map_err(|_| {
                            AppError::io("The media normalization state is unavailable")
                        })?;
                    drop(state);
                    context.check_cancelled()?;
                }
            }
        }
    }
}

fn normalization_key(
    identity: &OriginalIdentity,
    options: &IngestOptions,
    generation: u64,
    project_id: Option<&str>,
) -> String {
    format!(
        "{}|{}|{}|{}|{}|{}x{}@{}/{}",
        options.store.workspace_id(),
        generation,
        project_id.unwrap_or(""),
        identity.content_hash,
        options.renderer_version,
        options.profile.width,
        options.profile.height,
        options.profile.fps_num,
        options.profile.fps_den,
    )
}

fn pending_key(
    grant: &FileGrant,
    options: &IngestOptions,
    relink_asset_id: Option<&str>,
    generation: u64,
    project_id: Option<&str>,
    run_id: Option<&str>,
) -> String {
    format!(
        "{}|{}|{}|{}|{}|{}|{}|{}x{}@{}/{}|{}",
        options.store.workspace_id(),
        generation,
        project_id.unwrap_or(""),
        run_id.unwrap_or(""),
        relink_asset_id.unwrap_or("import"),
        grant.identity.canonical_path,
        grant.identity.token,
        options.profile.width,
        options.profile.height,
        options.profile.fps_num,
        options.profile.fps_den,
        options.renderer_version,
    )
}

fn same_normalization(
    existing: &AssetManifest,
    prepared: &AssetManifest,
    options: &IngestOptions,
) -> bool {
    if !existing.is_ready()
        || existing.content_hash != prepared.content_hash
        || existing.kind != prepared.kind
        || existing
            .normalization
            .as_ref()
            .is_none_or(|normalization| normalization.renderer_version != options.renderer_version)
    {
        return false;
    }
    let Some(existing_normalization) = existing.normalization.as_ref() else {
        return false;
    };
    let Some(prepared_normalization) = prepared.normalization.as_ref() else {
        return false;
    };
    existing.frame_count() == prepared.frame_count()
        && match (&existing_normalization.video, &prepared_normalization.video) {
            (Some(existing), Some(prepared)) => {
                existing.width == prepared.width
                    && existing.height == prepared.height
                    && existing.fps_num == options.profile.fps_num
                    && existing.fps_den == options.profile.fps_den
                    && prepared.fps_num == options.profile.fps_num
                    && prepared.fps_den == options.profile.fps_den
            }
            (None, None) => true,
            _ => false,
        }
}

fn notification_hook(
    state: AppState,
    generation: u64,
    run_id: Option<String>,
) -> jobs::JobNotificationHook {
    Arc::new(move |summary, result| {
        if state.validate_generation(generation).is_err() {
            return;
        }
        let kind = match summary.state {
            JobState::Completed => "media_job_completed",
            JobState::Failed => "media_job_failed",
            JobState::Cancelled => "media_job_cancelled",
            JobState::Queued | JobState::Running => "media_job_progress",
        };
        let mut payload = json!({
            "jobId": summary.job_id,
            "kind": summary.kind,
            "state": summary.state,
            "progress": summary.progress,
            "generation": summary.generation,
        });
        if let Some(error) = summary.error {
            payload["error"] = json!(error);
        }
        if let Some(result) = result {
            if let Ok(prepared) = serde_json::from_value::<PreparedAsset>(result) {
                payload["asset"] = json!(prepared.asset);
                payload["artifactIds"] = json!(prepared
                    .artifacts
                    .into_iter()
                    .map(|artifact| artifact.artifact_id)
                    .collect::<Vec<_>>());
                payload["ready"] = Value::Bool(true);
            }
        }
        let _ = state.emit_sanitized_event(kind, run_id.clone(), Some(payload));
    })
}

fn event_hook(state: AppState, generation: u64, run_id: Option<String>) -> jobs::JobEventHook {
    Arc::new(move |kind, data| {
        if state.validate_generation(generation).is_err() {
            return;
        }
        let _ = state.emit_sanitized_event(kind, run_id.clone(), Some(data));
    })
}

fn find_asset(
    state: &AppState,
    generation: u64,
    asset_id: &str,
) -> Result<AssetManifest, AppError> {
    let snapshot = state.snapshot_at(generation)?;
    snapshot
        .document
        .assets
        .into_iter()
        .find(|asset| asset.id == asset_id)
        .ok_or_else(|| {
            AppError::new(
                ErrorCode::AssetUnavailable,
                "The requested asset is unavailable",
            )
        })
}

fn identity_from_manifest(asset: &AssetManifest) -> Result<OriginalIdentity, AppError> {
    let path = asset.original.location.as_deref().ok_or_else(|| {
        AppError::new(
            ErrorCode::AssetUnavailable,
            "The asset has no original location",
        )
    })?;
    let identity = OriginalIdentity {
        canonical_path: path.to_owned(),
        byte_size: asset.original.byte_size.unwrap_or(0),
        modified_time_ms: asset.original.modified_time_ms,
        device: None,
        inode: None,
        content_hash: asset.content_hash.clone(),
    };
    identity.validate()?;
    Ok(identity)
}
