//! Runtime, IPC-facing preview declarations, and generation-scoped orchestration.

pub mod audio;
mod cache;
mod decoder;
pub mod frame;
pub mod software;

use crate::editor::dispatcher::{CallerContext, CallerKind};
use crate::error::{AppError, ErrorCode};
use crate::ipc::MAX_SAFE_INTEGER;
use crate::media::ffmpeg::FfmpegToolchain;
use crate::media::render_plan::{encode_rgba_png, ArtifactResolver, RenderPlan};
use crate::project::model::AUDIO_SAMPLE_RATE;
use crate::state::AppState;
use audio::render_audio_window;
use cache::{
    validate_cached_frame_artifact, RenderFrameArtifactCache, RenderFrameCacheCandidate,
    RenderFrameCacheKey,
};
use frame::render_rgba_frame;
use serde::{Deserialize, Serialize};
use software::{SoftwarePreviewReceiver, SoftwarePreviewSession};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::oneshot;
use ts_rs::TS;

const PREVIEW_TRANSPORT_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_TRANSPORT_ERROR_BYTES: usize = 2 * 1024;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
#[ts(tag = "action", rename_all = "snake_case")]
pub enum PreviewAction {
    Plan {
        #[serde(rename = "revision")]
        #[ts(rename = "revision", type = "SafeInteger")]
        revision: u64,
    },
    RenderFrame {
        #[serde(rename = "revision")]
        #[ts(rename = "revision", type = "SafeInteger")]
        revision: u64,
        #[serde(rename = "frame")]
        #[ts(rename = "frame", type = "SafeInteger")]
        frame: u64,
    },
    RenderAudioWindow {
        #[serde(rename = "planHash")]
        #[ts(rename = "planHash")]
        plan_hash: String,
        #[serde(rename = "startSample")]
        #[ts(rename = "startSample", type = "SafeInteger")]
        start_sample: u64,
        #[serde(rename = "sampleCount")]
        #[ts(rename = "sampleCount", type = "SafeInteger")]
        sample_count: u64,
    },
    Seek {
        #[serde(rename = "frame")]
        #[ts(rename = "frame", type = "SafeInteger")]
        frame: u64,
    },
    Play {},
    Pause {},
    Inspect {
        #[serde(rename = "revision")]
        #[ts(rename = "revision", type = "SafeInteger")]
        revision: u64,
        #[serde(rename = "frames")]
        #[ts(rename = "frames", type = "SafeInteger[]")]
        frames: Vec<u64>,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase")]
pub struct PreviewFrameReply {
    #[ts(type = "SafeInteger")]
    pub revision: u64,
    #[ts(type = "SafeInteger")]
    pub frame: u64,
    pub plan_hash: String,
    pub artifact_id: String,
    pub width: u32,
    pub height: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase")]
pub struct PreviewAudioReply {
    pub plan_hash: String,
    #[ts(type = "SafeInteger")]
    pub start_sample: u64,
    #[ts(type = "SafeInteger")]
    pub sample_count: u64,
    pub sample_rate: u32,
    pub channels: u8,
    pub artifact_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase")]
pub struct PreviewInspectFrame {
    #[ts(type = "SafeInteger")]
    pub frame: u64,
    pub artifact_id: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(
    tag = "kind",
    content = "data",
    rename_all = "snake_case",
    deny_unknown_fields
)]
#[ts(tag = "kind", content = "data", rename_all = "snake_case")]
pub enum PreviewReply {
    Plan(RenderPlan),
    Frame(PreviewFrameReply),
    Audio(PreviewAudioReply),
    Inspect {
        #[serde(rename = "revision")]
        #[ts(rename = "revision", type = "SafeInteger")]
        revision: u64,
        frames: Vec<PreviewInspectFrame>,
    },
    Ack {
        #[serde(rename = "revision")]
        #[ts(rename = "revision", type = "SafeInteger")]
        revision: u64,
        #[serde(rename = "planHash")]
        #[ts(rename = "planHash")]
        plan_hash: String,
    },
}

/// A binary software-preview packet.  The packet is sent through a Tauri
/// Channel by NativeGlue, not through JSON events; `data` is therefore a
/// bounded binary payload and never base64 application data.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase")]
pub struct SoftwarePreviewPacket {
    #[ts(type = "SafeInteger")]
    pub generation: u64,
    pub project_id: String,
    #[ts(type = "SafeInteger")]
    pub revision: u64,
    pub plan_hash: String,
    #[ts(type = "SafeInteger")]
    pub frame: u64,
    #[ts(type = "SafeInteger")]
    pub sequence: u64,
    pub width: u32,
    pub height: u32,
    pub content_type: String,
    #[ts(type = "Uint8Array")]
    pub data: Vec<u8>,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "lowercase")]
#[ts(rename_all = "lowercase")]
pub enum PreviewTransportAction {
    Play,
    Pause,
    Seek,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase")]
pub struct PreviewTransportCommand {
    #[ts(type = "SafeInteger")]
    pub sequence: u64,
    pub project_id: String,
    #[ts(type = "SafeInteger")]
    pub generation: u64,
    #[ts(type = "SafeInteger")]
    pub revision: u64,
    pub action: PreviewTransportAction,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional, type = "SafeInteger")]
    pub frame: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase")]
pub struct PreviewTransportCompletion {
    #[ts(type = "SafeInteger")]
    pub sequence: u64,
    pub project_id: String,
    #[ts(type = "SafeInteger")]
    pub generation: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub error: Option<String>,
}

struct PendingTransport {
    command: PreviewTransportCommand,
    sender: oneshot::Sender<PreviewTransportCompletion>,
}

struct RuntimeInner {
    artifacts: Mutex<Option<Arc<dyn ArtifactResolver>>>,
    toolchain: Mutex<FfmpegToolchain>,
    plan: Mutex<Option<RenderPlan>>,
    plan_generation: AtomicU64,
    transport_generation: AtomicU64,
    transport_sequence: AtomicU64,
    remote_intent_epoch: AtomicU64,
    pending_transports: Mutex<HashMap<u64, PendingTransport>>,
    playing: AtomicBool,
    software: Mutex<Option<SoftwarePreviewSession>>,
    frame_cache: Mutex<RenderFrameArtifactCache>,
}

#[derive(Clone)]
pub struct RenderRuntime {
    inner: Arc<RuntimeInner>,
}

impl Default for RenderRuntime {
    fn default() -> Self {
        Self::new()
    }
}

impl RenderRuntime {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(RuntimeInner {
                artifacts: Mutex::new(None),
                toolchain: Mutex::new(FfmpegToolchain::default()),
                plan: Mutex::new(None),
                plan_generation: AtomicU64::new(0),
                transport_generation: AtomicU64::new(0),
                transport_sequence: AtomicU64::new(0),
                remote_intent_epoch: AtomicU64::new(0),
                pending_transports: Mutex::new(HashMap::new()),
                playing: AtomicBool::new(false),
                software: Mutex::new(None),
                frame_cache: Mutex::new(RenderFrameArtifactCache::default()),
            }),
        }
    }

    pub fn set_artifacts(&self, artifacts: Arc<dyn ArtifactResolver>) -> Result<(), AppError> {
        *self
            .inner
            .artifacts
            .lock()
            .map_err(|_| AppError::io("The renderer artifact lock is unavailable"))? =
            Some(artifacts);
        self.invalidate_frame_cache()
    }

    pub fn set_toolchain(&self, toolchain: FfmpegToolchain) -> Result<(), AppError> {
        *self
            .inner
            .toolchain
            .lock()
            .map_err(|_| AppError::io("The renderer toolchain lock is unavailable"))? = toolchain;
        self.invalidate_frame_cache()
    }

    fn invalidate_frame_cache(&self) -> Result<(), AppError> {
        self.inner
            .frame_cache
            .lock()
            .map_err(|_| AppError::io("The rendered-frame cache lock is unavailable"))?
            .invalidate();
        Ok(())
    }

    fn frame_cache_candidate(
        &self,
        key: &RenderFrameCacheKey,
    ) -> Result<(u64, Option<RenderFrameCacheCandidate>), AppError> {
        Ok(self
            .inner
            .frame_cache
            .lock()
            .map_err(|_| AppError::io("The rendered-frame cache lock is unavailable"))?
            .candidate(key))
    }

    fn frame_cache_confirm_hit(
        &self,
        key: &RenderFrameCacheKey,
        epoch: u64,
        artifact_id: &str,
        artifact_len: u64,
    ) -> Result<bool, AppError> {
        Ok(self
            .inner
            .frame_cache
            .lock()
            .map_err(|_| AppError::io("The rendered-frame cache lock is unavailable"))?
            .confirm_hit(key, epoch, artifact_id, artifact_len))
    }

    fn frame_cache_evict(
        &self,
        key: &RenderFrameCacheKey,
        epoch: u64,
        artifact_id: &str,
        artifact_len: u64,
    ) -> Result<(), AppError> {
        self.inner
            .frame_cache
            .lock()
            .map_err(|_| AppError::io("The rendered-frame cache lock is unavailable"))?
            .evict_if_matches(key, epoch, artifact_id, artifact_len);
        Ok(())
    }

    fn frame_cache_insert(
        &self,
        key: RenderFrameCacheKey,
        artifact_id: String,
        artifact_len: u64,
        epoch: u64,
    ) -> Result<(), AppError> {
        self.inner
            .frame_cache
            .lock()
            .map_err(|_| AppError::io("The rendered-frame cache lock is unavailable"))?
            .insert_if_current(key, artifact_id, artifact_len, epoch);
        Ok(())
    }

    fn cached_frame_artifact(
        &self,
        plan: &RenderPlan,
        frame: u64,
        artifacts: &dyn ArtifactResolver,
    ) -> Result<(u64, Option<String>), AppError> {
        let key = RenderFrameCacheKey::new(plan, frame);
        let (epoch, candidate) = self.frame_cache_candidate(&key)?;
        let Some(candidate) = candidate else {
            return Ok((epoch, None));
        };
        if !validate_cached_frame_artifact(
            artifacts,
            &candidate.artifact_id,
            candidate.artifact_len,
            plan.width,
            plan.height,
        ) {
            self.frame_cache_evict(&key, epoch, &candidate.artifact_id, candidate.artifact_len)?;
            return Ok((epoch, None));
        }
        if self.frame_cache_confirm_hit(
            &key,
            epoch,
            &candidate.artifact_id,
            candidate.artifact_len,
        )? {
            Ok((epoch, Some(candidate.artifact_id)))
        } else {
            Ok((epoch, None))
        }
    }

    fn render_frame_artifact(
        &self,
        plan: &RenderPlan,
        frame: u64,
        artifacts: &dyn ArtifactResolver,
        toolchain: &FfmpegToolchain,
    ) -> Result<String, AppError> {
        if frame >= plan.duration_frames {
            return Err(AppError::invalid_argument(
                "The requested frame is outside the render plan",
            ));
        }
        let key = RenderFrameCacheKey::new(plan, frame);
        let (epoch, cached) = self.cached_frame_artifact(plan, frame, artifacts)?;
        if let Some(artifact_id) = cached {
            return Ok(artifact_id);
        }
        let bytes = render_rgba_frame(plan, frame, artifacts, toolchain)?;
        let png = encode_rgba_png(plan.width, plan.height, &bytes)?;
        let artifact_len = png.len() as u64;
        let artifact_id = artifacts.put_bytes(&key.storage_key(), "png", "image/png", &png)?;
        if !validate_cached_frame_artifact(
            artifacts,
            &artifact_id,
            artifact_len,
            plan.width,
            plan.height,
        ) {
            return Err(AppError::new(
                ErrorCode::AssetUnavailable,
                "The rendered frame artifact is unavailable",
            ));
        }
        self.frame_cache_insert(key, artifact_id.clone(), artifact_len, epoch)?;
        Ok(artifact_id)
    }

    fn artifacts(&self) -> Result<Arc<dyn ArtifactResolver>, AppError> {
        self.inner
            .artifacts
            .lock()
            .map_err(|_| AppError::io("The renderer artifact lock is unavailable"))?
            .clone()
            .ok_or_else(|| {
                AppError::new(
                    ErrorCode::AssetUnavailable,
                    "The managed artifact store is unavailable",
                )
            })
    }

    fn toolchain(&self) -> Result<FfmpegToolchain, AppError> {
        Ok(self
            .inner
            .toolchain
            .lock()
            .map_err(|_| AppError::io("The renderer toolchain lock is unavailable"))?
            .clone())
    }

    fn compile_current(&self, state: &AppState, generation: u64) -> Result<RenderPlan, AppError> {
        let snapshot = state.snapshot_at(generation)?;
        let plan = crate::media::render_plan::compile_render_plan(
            &snapshot.document,
            self.artifacts()?.as_ref(),
        )?;
        self.invalidate_frame_cache()?;
        *self
            .inner
            .plan
            .lock()
            .map_err(|_| AppError::io("The renderer plan lock is unavailable"))? =
            Some(plan.clone());
        self.inner
            .plan_generation
            .store(generation, Ordering::Release);
        Ok(plan)
    }

    fn current_plan(
        &self,
        state: &AppState,
        generation: u64,
        revision: u64,
    ) -> Result<RenderPlan, AppError> {
        let snapshot = state.snapshot_at(generation)?;
        if snapshot.document.revision != revision {
            return Err(AppError::new(
                ErrorCode::RevisionConflict,
                "The requested render revision is no longer current",
            ));
        }
        if self.inner.plan_generation.load(Ordering::Acquire) == generation {
            if let Some(plan) = self
                .inner
                .plan
                .lock()
                .map_err(|_| AppError::io("The renderer plan lock is unavailable"))?
                .clone()
                .filter(|plan| {
                    plan.revision == revision && plan.project_id == snapshot.document.project_id
                })
            {
                return Ok(plan);
            }
        }
        self.compile_current(state, generation)
    }

    fn invalidate_transport(&self, state: Option<&AppState>) {
        self.inner.playing.store(false, Ordering::Release);
        self.inner
            .remote_intent_epoch
            .fetch_add(1, Ordering::AcqRel);
        let cancelled = self.cancel_pending_transports("Superseded by a newer preview intent");
        if let Some(state) = state {
            self.emit_transport_cancellations(state, &cancelled);
        }
        let session = self.inner.software.lock().ok().and_then(|mut software| {
            self.inner
                .transport_generation
                .fetch_add(1, Ordering::AcqRel);
            software.take()
        });
        if let Some(session) = session {
            session.stop();
        }
    }
    fn cancel_pending_transports(&self, message: &str) -> Vec<PreviewTransportCommand> {
        let pending = self
            .inner
            .pending_transports
            .lock()
            .ok()
            .map(|mut entries| {
                entries
                    .drain()
                    .map(|(_, pending)| pending)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let mut commands = Vec::with_capacity(pending.len());
        for pending in pending {
            commands.push(pending.command.clone());
            let _ = pending.sender.send(PreviewTransportCompletion {
                sequence: pending.command.sequence,
                project_id: pending.command.project_id,
                generation: pending.command.generation,
                error: Some(message.to_owned()),
            });
        }
        commands
    }

    fn emit_transport_cancellations(&self, state: &AppState, commands: &[PreviewTransportCommand]) {
        for command in commands {
            let _ = state.emit_sanitized_event(
                "preview_transport_cancel",
                None,
                Some(serde_json::to_value(command).unwrap_or_default()),
            );
        }
    }

    fn remove_pending_transport(&self, sequence: u64) {
        if let Ok(mut pending) = self.inner.pending_transports.lock() {
            pending.remove(&sequence);
        }
    }
    fn retire_pending_transport(&self, state: &AppState, command: &PreviewTransportCommand) {
        self.emit_transport_cancellations(state, std::slice::from_ref(command));
        self.remove_pending_transport(command.sequence);
    }

    fn next_transport_sequence(&self) -> Result<u64, AppError> {
        self.inner
            .transport_sequence
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
                current
                    .checked_add(1)
                    .filter(|next| *next <= MAX_SAFE_INTEGER)
            })
            .map_err(|_| AppError::io("The preview transport sequence is exhausted"))
    }

    async fn request_transport(
        &self,
        action: PreviewTransportAction,
        frame: Option<u64>,
        plan: &RenderPlan,
        caller: &CallerContext,
        state: &AppState,
    ) -> Result<(), AppError> {
        if caller.project_id.as_deref() != Some(plan.project_id.as_str()) {
            return Err(AppError::stale_session(
                "The preview transport caller belongs to a different project",
            ));
        }
        let intent_epoch = self.inner.remote_intent_epoch.load(Ordering::Acquire);
        let sequence = self.next_transport_sequence()?;
        let command = PreviewTransportCommand {
            sequence,
            project_id: plan.project_id.clone(),
            generation: caller.generation,
            revision: plan.revision,
            action,
            frame,
        };
        let cancelled =
            self.cancel_pending_transports("Superseded by a newer preview transport command");
        self.emit_transport_cancellations(state, &cancelled);
        if self.inner.remote_intent_epoch.load(Ordering::Acquire) != intent_epoch {
            return Err(AppError::stale_session(
                "The preview transport was superseded by a newer human intent",
            ));
        }
        let (sender, mut receiver) = oneshot::channel();
        {
            let mut pending = self
                .inner
                .pending_transports
                .lock()
                .map_err(|_| AppError::io("The preview transport lock is unavailable"))?;
            pending.insert(
                command.sequence,
                PendingTransport {
                    command: command.clone(),
                    sender,
                },
            );
        }
        if self.inner.remote_intent_epoch.load(Ordering::Acquire) != intent_epoch {
            self.retire_pending_transport(state, &command);
            return Err(AppError::stale_session(
                "The preview transport was superseded by a newer human intent",
            ));
        }
        if let Err(error) = state.emit_sanitized_event(
            "preview_transport",
            caller.run_id().map(ToOwned::to_owned),
            Some(serde_json::to_value(&command)?),
        ) {
            self.retire_pending_transport(state, &command);
            return Err(error);
        }

        let deadline = Instant::now() + PREVIEW_TRANSPORT_TIMEOUT;
        loop {
            if Instant::now() >= deadline {
                self.retire_pending_transport(state, &command);
                return Err(AppError::io(
                    "The visible preview did not accept the transport command in time",
                ));
            }
            tokio::select! {
                result = &mut receiver => {
                    let completion = result.map_err(|_| {
                        AppError::stale_session("The preview transport request was retired")
                    })?;
                    if completion.sequence != command.sequence
                        || completion.project_id != command.project_id
                        || completion.generation != command.generation
                    {
                        return Err(AppError::stale_session(
                            "The preview transport completion belongs to a different scope",
                        ));
                    }
                    if let Some(error) = completion.error {
                        let message = if error.len() > MAX_TRANSPORT_ERROR_BYTES {
                            "The visible preview rejected the transport command".to_owned()
                        } else {
                            format!("The visible preview rejected the transport command: {error}")
                        };
                        return Err(AppError::new(ErrorCode::IoError, message));
                    }
                    let status = state.status()?;
                    if status.generation != command.generation
                        || status.project_id.as_deref() != Some(command.project_id.as_str())
                        || status.revision != Some(command.revision)
                        || self.inner.remote_intent_epoch.load(Ordering::Acquire) != intent_epoch
                    {
                        self.retire_pending_transport(state, &command);
                        return Err(AppError::stale_session("The preview transport was superseded"));
                    }
                    if let Some(run_id) = caller.run_id() {
                        state.require_active_run_at(caller.generation, run_id)?;
                    }
                    return Ok(());
                }
                _ = tokio::time::sleep(Duration::from_millis(50)) => {}
            }
            if self.inner.remote_intent_epoch.load(Ordering::Acquire) != intent_epoch {
                self.retire_pending_transport(state, &command);
                return Err(AppError::stale_session(
                    "The preview transport was superseded by a newer human intent",
                ));
            }
            if let Err(error) = state.validate_generation(caller.generation) {
                self.retire_pending_transport(state, &command);
                return Err(error);
            }
            let status = match state.status() {
                Ok(status) => status,
                Err(error) => {
                    self.retire_pending_transport(state, &command);
                    return Err(error);
                }
            };
            if status.generation != command.generation
                || status.project_id.as_deref() != Some(command.project_id.as_str())
                || status.revision != Some(command.revision)
            {
                self.retire_pending_transport(state, &command);
                return Err(AppError::stale_session(
                    "The preview transport belongs to a retired project revision",
                ));
            }
            if let Some(run_id) = caller.run_id() {
                if let Err(error) = state.require_active_run_at(caller.generation, run_id) {
                    self.retire_pending_transport(state, &command);
                    return Err(error);
                }
            }
        }
    }

    pub fn complete_transport(
        &self,
        completion: PreviewTransportCompletion,
        state: &AppState,
    ) -> Result<(), AppError> {
        if completion.sequence > MAX_SAFE_INTEGER || completion.generation > MAX_SAFE_INTEGER {
            return Err(AppError::invalid_argument(
                "The preview transport completion is outside the safe range",
            ));
        }
        if completion.project_id.is_empty() || completion.project_id.len() > 256 {
            return Err(AppError::invalid_argument(
                "The preview transport completion project is invalid",
            ));
        }
        if completion
            .error
            .as_ref()
            .is_some_and(|error| error.is_empty() || error.len() > MAX_TRANSPORT_ERROR_BYTES)
        {
            return Err(AppError::invalid_argument(
                "The preview transport completion error is invalid",
            ));
        }
        let command = {
            let pending = self
                .inner
                .pending_transports
                .lock()
                .map_err(|_| AppError::io("The preview transport lock is unavailable"))?;
            pending
                .get(&completion.sequence)
                .map(|entry| entry.command.clone())
                .ok_or_else(|| {
                    AppError::stale_session(
                        "The preview transport completion has no pending command",
                    )
                })?
        };
        if command.project_id != completion.project_id
            || command.generation != completion.generation
        {
            return Err(AppError::stale_session(
                "The preview transport completion belongs to a different scope",
            ));
        }
        state.validate_generation(command.generation)?;
        let status = state.status()?;
        if status.generation != command.generation
            || status.project_id.as_deref() != Some(command.project_id.as_str())
            || status.revision != Some(command.revision)
        {
            self.retire_pending_transport(state, &command);
            return Err(AppError::stale_session(
                "The preview transport command belongs to a retired project revision",
            ));
        }
        let pending = self
            .inner
            .pending_transports
            .lock()
            .map_err(|_| AppError::io("The preview transport lock is unavailable"))?
            .remove(&completion.sequence)
            .ok_or_else(|| {
                AppError::stale_session("The preview transport command was already retired")
            })?;
        pending.sender.send(completion).map_err(|_| {
            AppError::stale_session("The preview transport request is no longer waiting")
        })
    }

    /// Unified native preview handler.  NativeGlue supplies the trusted caller
    /// context; the action itself has no project/path authority fields.
    pub async fn handle(
        &self,
        request: PreviewAction,
        caller: &CallerContext,
        state: &AppState,
    ) -> Result<PreviewReply, AppError> {
        state.validate_generation(caller.generation)?;
        let generation = caller.generation;
        match request {
            PreviewAction::Plan { revision } => {
                let plan = self.current_plan(state, generation, revision)?;
                Ok(PreviewReply::Plan(plan))
            }
            PreviewAction::RenderFrame { revision, frame } => {
                let plan = self.current_plan(state, generation, revision)?;
                let artifacts = self.artifacts()?;
                let toolchain = self.toolchain()?;
                let artifact_id =
                    self.render_frame_artifact(&plan, frame, artifacts.as_ref(), &toolchain)?;
                Ok(PreviewReply::Frame(PreviewFrameReply {
                    revision,
                    frame,
                    plan_hash: plan.plan_hash,
                    artifact_id,
                    width: plan.width,
                    height: plan.height,
                }))
            }
            PreviewAction::RenderAudioWindow {
                plan_hash,
                start_sample,
                sample_count,
            } => {
                let snapshot = state.snapshot_at(generation)?;
                let plan = self.current_plan(state, generation, snapshot.document.revision)?;
                if plan.plan_hash != plan_hash {
                    return Err(AppError::new(
                        ErrorCode::RevisionConflict,
                        "The requested audio plan is stale",
                    ));
                }
                let pcm = render_audio_window(
                    &plan,
                    start_sample,
                    sample_count,
                    self.artifacts()?.as_ref(),
                )?;
                let mut bytes = Vec::with_capacity(pcm.len() * 4);
                for sample in pcm {
                    bytes.extend_from_slice(&sample.to_le_bytes());
                }
                let key = format!(
                    "audio:{}:{}:{}:{}:{}",
                    plan.project_id, plan.revision, plan.plan_hash, start_sample, sample_count
                );
                let artifact_id = self.artifacts()?.put_bytes(
                    &key,
                    "f32le",
                    "application/octet-stream",
                    &bytes,
                )?;
                Ok(PreviewReply::Audio(PreviewAudioReply {
                    plan_hash,
                    start_sample,
                    sample_count,
                    sample_rate: AUDIO_SAMPLE_RATE,
                    channels: 2,
                    artifact_id,
                }))
            }
            PreviewAction::Seek { frame } => {
                let snapshot = state.snapshot_at(generation)?;
                let plan = self.current_plan(state, generation, snapshot.document.revision)?;
                if frame > plan.duration_frames {
                    return Err(AppError::invalid_argument(
                        "The requested seek frame is outside the project",
                    ));
                }
                if matches!(&caller.kind, CallerKind::AgentSidecar { .. }) {
                    self.request_transport(
                        PreviewTransportAction::Seek,
                        Some(frame),
                        &plan,
                        caller,
                        state,
                    )
                    .await?;
                } else {
                    self.invalidate_transport(Some(state));
                }
                Ok(PreviewReply::Ack {
                    revision: plan.revision,
                    plan_hash: plan.plan_hash,
                })
            }
            PreviewAction::Play {} => {
                let snapshot = state.snapshot_at(generation)?;
                let plan = self.current_plan(state, generation, snapshot.document.revision)?;
                if matches!(&caller.kind, CallerKind::AgentSidecar { .. }) {
                    self.request_transport(
                        PreviewTransportAction::Play,
                        None,
                        &plan,
                        caller,
                        state,
                    )
                    .await?;
                } else {
                    self.inner
                        .remote_intent_epoch
                        .fetch_add(1, Ordering::AcqRel);
                    let cancelled = self
                        .cancel_pending_transports("Superseded by a newer human preview intent");
                    self.emit_transport_cancellations(state, &cancelled);
                }
                self.inner.playing.store(true, Ordering::Release);
                Ok(PreviewReply::Ack {
                    revision: plan.revision,
                    plan_hash: plan.plan_hash,
                })
            }
            PreviewAction::Pause {} => {
                let snapshot = state.snapshot_at(generation)?;
                let plan = self.current_plan(state, generation, snapshot.document.revision)?;
                if matches!(&caller.kind, CallerKind::AgentSidecar { .. }) {
                    self.request_transport(
                        PreviewTransportAction::Pause,
                        None,
                        &plan,
                        caller,
                        state,
                    )
                    .await?;
                } else {
                    self.invalidate_transport(Some(state));
                }
                self.inner.playing.store(false, Ordering::Release);
                Ok(PreviewReply::Ack {
                    revision: plan.revision,
                    plan_hash: plan.plan_hash,
                })
            }
            PreviewAction::Inspect { revision, frames } => {
                if frames.len() > 32 {
                    return Err(AppError::invalid_argument(
                        "Preview inspection accepts at most 32 frames",
                    ));
                }
                let plan = self.current_plan(state, generation, revision)?;
                let artifacts = self.artifacts()?;
                let toolchain = self.toolchain()?;
                let mut results = Vec::with_capacity(frames.len());
                for frame in frames {
                    let artifact_id =
                        self.render_frame_artifact(&plan, frame, artifacts.as_ref(), &toolchain)?;
                    results.push(PreviewInspectFrame { frame, artifact_id });
                }
                Ok(PreviewReply::Inspect {
                    revision,
                    frames: results,
                })
            }
        }
    }
    /// Start a persistent FFmpeg image2pipe worker.  The worker owns one
    /// bounded queue and one encoder process for its entire seek generation;
    /// visual frames are paced by the queue and stale sequences are dropped
    /// before they can reach the WebView.
    pub fn start_software_preview(
        &self,
        plan: RenderPlan,
        generation: u64,
        start_frame: u64,
    ) -> Result<SoftwarePreviewReceiver, AppError> {
        let transport_epoch = self.inner.transport_generation.load(Ordering::Acquire);
        plan.validate()?;
        let artifacts = self.artifacts()?;
        let toolchain = self.toolchain()?;
        let session =
            SoftwarePreviewSession::start(plan, generation, start_frame, artifacts, toolchain)?;
        let receiver = session.receiver();
        let (previous, retired) = {
            let mut software = self
                .inner
                .software
                .lock()
                .map_err(|_| AppError::io("The software preview lock is unavailable"))?;
            if self.inner.transport_generation.load(Ordering::Acquire) == transport_epoch {
                self.inner
                    .transport_generation
                    .fetch_add(1, Ordering::AcqRel);
                (software.replace(session), None)
            } else {
                (None, Some(session))
            }
        };
        if let Some(previous) = previous {
            previous.stop();
        }
        if let Some(retired) = retired {
            retired.stop();
            return Err(AppError::stale_session(
                "The software preview was invalidated before it started",
            ));
        }
        Ok(receiver)
    }

    pub fn stop_software_preview(&self) {
        let session = self.inner.software.lock().ok().and_then(|mut software| {
            self.inner
                .transport_generation
                .fetch_add(1, Ordering::AcqRel);
            software.take()
        });
        if let Some(session) = session {
            session.stop();
        }
    }
    /// Acknowledge a packet only for the currently owned software session.
    /// Identity checks prevent a late packet from a retired project from
    /// extending backpressure on a replacement session.
    pub fn ack_software_preview(
        &self,
        generation: u64,
        project_id: &str,
        revision: u64,
        plan_hash: &str,
        sequence: u64,
    ) -> Result<(), AppError> {
        let software = self
            .inner
            .software
            .lock()
            .map_err(|_| AppError::io("The software preview lock is unavailable"))?;
        let session = software.as_ref().ok_or_else(|| {
            AppError::stale_session("The software preview session is no longer active")
        })?;
        if !session.matches_identity(generation, project_id, revision, plan_hash) {
            return Err(AppError::stale_session(
                "The software preview packet belongs to a retired session",
            ));
        }
        session.acknowledge(sequence);
        Ok(())
    }

    /// Cancel an owned software session after checking its immutable identity.
    /// Cancellation is idempotent when the session has already retired.
    pub fn cancel_software_preview(
        &self,
        generation: u64,
        project_id: &str,
        revision: u64,
        plan_hash: &str,
    ) -> Result<(), AppError> {
        let session = {
            let mut software = self
                .inner
                .software
                .lock()
                .map_err(|_| AppError::io("The software preview lock is unavailable"))?;
            let Some(current) = software.as_ref() else {
                return Ok(());
            };
            if !current.matches_identity(generation, project_id, revision, plan_hash) {
                return Err(AppError::stale_session(
                    "The software preview session belongs to a retired project",
                ));
            }
            self.inner
                .transport_generation
                .fetch_add(1, Ordering::AcqRel);
            software.take()
        };
        if let Some(session) = session {
            session.stop();
        }
        Ok(())
    }

    /// Retire all project-bound renderer state before the owning generation is
    /// closed or replaced.  In particular, stop the software worker before
    /// dropping its artifact resolver so no stale packets can escape.
    pub fn retire_project(&self) -> Result<(), AppError> {
        self.invalidate_transport(None);
        self.inner.plan_generation.store(0, Ordering::Release);
        *self
            .inner
            .plan
            .lock()
            .map_err(|_| AppError::io("The renderer plan lock is unavailable"))? = None;
        *self
            .inner
            .artifacts
            .lock()
            .map_err(|_| AppError::io("The renderer artifact lock is unavailable"))? = None;
        *self
            .inner
            .toolchain
            .lock()
            .map_err(|_| AppError::io("The renderer toolchain lock is unavailable"))? =
            FfmpegToolchain::default();
        self.invalidate_frame_cache()?;
        Ok(())
    }
}
/// Immutable export/preview capture.  ExportRuntime uses this value to pin a
/// revision and the exact artifact resolver/toolchain for the whole job.
pub struct RenderCapture {
    pub plan: RenderPlan,
    pub artifacts: Arc<dyn ArtifactResolver>,
    pub toolchain: FfmpegToolchain,
}

impl RenderRuntime {
    pub fn capture(
        &self,
        state: &AppState,
        generation: u64,
        revision: u64,
    ) -> Result<RenderCapture, AppError> {
        let plan = self.current_plan(state, generation, revision)?;
        Ok(RenderCapture {
            plan,
            artifacts: self.artifacts()?,
            toolchain: self.toolchain()?,
        })
    }
}
