//! Authoritative frame/audio rendering and the Linux software-preview worker.
//!
//! The browser preview is allowed to be approximate, but this module is not:
//! every still and every exported sample is composed from the immutable
//! RenderPlan.  Source media is decoded from managed normalized artifacts;
//! no thumbnails or synthetic colors can satisfy a render request.

use crate::editor::dispatcher::CallerContext;
use crate::error::{AppError, ErrorCode};
use crate::media::ffmpeg::{decode_rgba_frame, read_f32_stereo_window, FfmpegToolchain};
use crate::media::render_plan::{
    encode_rgba_png, sample_at_frame, ArtifactResolver, AudioSegment, AudioTransitionSide,
    DestRect, RenderLayerKind, RenderPlan, RenderSegment, RenderTextOverlay, RenderTransition,
    SourceRect,
};
use crate::project::model::{FitMode, RgbaColor, TextStyle, AUDIO_SAMPLE_RATE};
use crate::state::AppState;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet, VecDeque};
use std::fs::File;
use std::io::{BufReader, Cursor, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};
use ts_rs::TS;

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
const SOFTWARE_PREVIEW_MAX_WIDTH: u32 = 960;
const SOFTWARE_PREVIEW_MAX_HEIGHT: u32 = 540;
const SOFTWARE_PREVIEW_MAX_FPS_NUM: u128 = 30;

/// The software fallback renders a private derived plan.  Its identity and
/// temporal/audio coordinates intentionally remain those of the canonical
/// plan; only the canvas and destination geometry are reduced.
fn software_raster_plan(plan: &RenderPlan) -> Result<RenderPlan, AppError> {
    let (width, height, scale) = software_raster_dimensions(plan.width, plan.height)?;
    let mut raster = plan.clone();
    raster.width = width;
    raster.height = height;
    for layer in &mut raster.layers {
        for segment in &mut layer.segments {
            segment.dest_rect = scale_dest_rect(&segment.dest_rect, scale)?;
        }
        for overlay in &mut layer.text_overlays {
            overlay.raster_dest_rect = scale_dest_rect(&overlay.raster_dest_rect, scale)?;
        }
    }
    Ok(raster)
}

fn software_raster_dimensions(width: u32, height: u32) -> Result<(u32, u32, f64), AppError> {
    if width == 0 || height == 0 {
        return Err(AppError::invalid_argument(
            "Software preview dimensions must be positive",
        ));
    }
    let long_side = f64::from(width.max(height));
    let short_side = f64::from(width.min(height));
    let scale = 1.0f64
        .min(f64::from(SOFTWARE_PREVIEW_MAX_WIDTH) / long_side)
        .min(f64::from(SOFTWARE_PREVIEW_MAX_HEIGHT) / short_side);
    let raster_width = round_positive_dimension(width, scale)?;
    let raster_height = round_positive_dimension(height, scale)?;
    Ok((raster_width, raster_height, scale))
}

fn round_positive_dimension(value: u32, scale: f64) -> Result<u32, AppError> {
    let rounded = (f64::from(value) * scale).round().max(1.0);
    if !rounded.is_finite() || rounded > f64::from(u32::MAX) {
        return Err(AppError::invalid_argument(
            "Software preview dimensions overflowed",
        ));
    }
    Ok(rounded as u32)
}

fn scale_dest_rect(rect: &DestRect, scale: f64) -> Result<DestRect, AppError> {
    let x = (f64::from(rect.x) * scale).round();
    let y = (f64::from(rect.y) * scale).round();
    if !x.is_finite()
        || !y.is_finite()
        || x < f64::from(i32::MIN)
        || x > f64::from(i32::MAX)
        || y < f64::from(i32::MIN)
        || y > f64::from(i32::MAX)
    {
        return Err(AppError::invalid_argument(
            "Software preview destination geometry overflowed",
        ));
    }
    let width = if rect.width == 0 {
        0
    } else {
        round_positive_dimension(rect.width, scale)?
    };
    let height = if rect.height == 0 {
        0
    } else {
        round_positive_dimension(rect.height, scale)?
    };
    Ok(DestRect {
        x: x as i32,
        y: y as i32,
        width,
        height,
    })
}

fn software_preview_sample_frame(
    start_frame: u64,
    output_tick: u64,
    fps_num: u32,
    fps_den: u32,
) -> Result<u64, AppError> {
    if fps_num == 0 || fps_den == 0 {
        return Err(AppError::invalid_argument(
            "Software preview frame rate must be positive",
        ));
    }
    let cadence_denominator = SOFTWARE_PREVIEW_MAX_FPS_NUM * u128::from(fps_den);
    let offset = if u128::from(fps_num) <= cadence_denominator {
        u128::from(output_tick)
    } else {
        // Sample the canonical frame nearest each 30 fps presentation tick.
        // Integer half-up rounding is exact for positive rational values and
        // gives 60 fps the required 0, 2, 4, ... sequence.
        let numerator = u128::from(output_tick)
            .checked_mul(u128::from(fps_num))
            .ok_or_else(|| AppError::invalid_argument("Software preview cadence overflowed"))?;
        numerator
            .checked_add(cadence_denominator / 2)
            .and_then(|value| value.checked_div(cadence_denominator))
            .ok_or_else(|| AppError::invalid_argument("Software preview cadence overflowed"))?
    };
    let offset = u64::try_from(offset)
        .map_err(|_| AppError::invalid_argument("Software preview frame index overflowed"))?;
    start_frame
        .checked_add(offset)
        .ok_or_else(|| AppError::invalid_argument("Software preview frame index overflowed"))
}

fn software_preview_target_nanos(
    output_tick: u64,
    fps_num: u32,
    fps_den: u32,
) -> Result<u128, AppError> {
    if fps_num == 0 || fps_den == 0 {
        return Err(AppError::invalid_argument(
            "Software preview frame rate must be positive",
        ));
    }
    let (numerator, denominator) = if u128::from(fps_num)
        <= SOFTWARE_PREVIEW_MAX_FPS_NUM * u128::from(fps_den)
    {
        (
            u128::from(output_tick)
                .checked_mul(u128::from(fps_den))
                .and_then(|value| value.checked_mul(1_000_000_000))
                .ok_or_else(|| AppError::invalid_argument("Software preview cadence overflowed"))?,
            u128::from(fps_num),
        )
    } else {
        (
            u128::from(output_tick)
                .checked_mul(1_000_000_000)
                .ok_or_else(|| AppError::invalid_argument("Software preview cadence overflowed"))?,
            SOFTWARE_PREVIEW_MAX_FPS_NUM,
        )
    };
    numerator
        .checked_div(denominator)
        .ok_or_else(|| AppError::invalid_argument("Software preview cadence overflowed"))
}

fn software_preview_max_frame_step(fps_num: u32, fps_den: u32) -> u64 {
    // A late visual frame must not trigger a seek/restart feedback loop.
    // Discard up to one second of forward frames through the existing decoder;
    // larger jumps still use the bounded one-second seek preroll.
    let step = u128::from(fps_num).div_ceil(u128::from(fps_den).max(1));
    u64::try_from(step).unwrap_or(u64::MAX).max(2)
}

fn wait_for_software_target(
    anchor: Instant,
    target_nanos: u128,
    cancel: &AtomicBool,
    requested_frame: &AtomicU64,
    expected_request: u64,
) -> bool {
    loop {
        if cancel.load(Ordering::Acquire)
            || requested_frame.load(Ordering::Acquire) != expected_request
        {
            return false;
        }
        let elapsed = anchor.elapsed().as_nanos();
        if elapsed >= target_nanos {
            return true;
        }
        let remaining = target_nanos - elapsed;
        let sleep_nanos = remaining.min(2_000_000);
        thread::sleep(Duration::from_nanos(sleep_nanos as u64));
    }
}
struct RuntimeInner {
    artifacts: Mutex<Option<Arc<dyn ArtifactResolver>>>,
    toolchain: Mutex<FfmpegToolchain>,
    plan: Mutex<Option<RenderPlan>>,
    plan_generation: AtomicU64,
    transport_generation: AtomicU64,
    playing: AtomicBool,
    software: Mutex<Option<SoftwarePreviewSession>>,
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
                playing: AtomicBool::new(false),
                software: Mutex::new(None),
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
        Ok(())
    }

    pub fn set_toolchain(&self, toolchain: FfmpegToolchain) -> Result<(), AppError> {
        *self
            .inner
            .toolchain
            .lock()
            .map_err(|_| AppError::io("The renderer toolchain lock is unavailable"))? = toolchain;
        Ok(())
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

    fn invalidate_transport(&self) {
        self.inner.playing.store(false, Ordering::Release);
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
                let bytes = render_rgba_frame(
                    &plan,
                    frame,
                    self.artifacts()?.as_ref(),
                    &self.toolchain()?,
                )?;
                let key = format!(
                    "frame:{}:{}:{}:{}",
                    plan.project_id, plan.revision, plan.plan_hash, frame
                );
                let artifact_id = self.artifacts()?.put_bytes(
                    &key,
                    "png",
                    "image/png",
                    &encode_rgba_png(plan.width, plan.height, &bytes)?,
                )?;
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
                self.invalidate_transport();
                Ok(PreviewReply::Ack {
                    revision: plan.revision,
                    plan_hash: plan.plan_hash,
                })
            }
            PreviewAction::Play {} => {
                self.inner.playing.store(true, Ordering::Release);
                let snapshot = state.snapshot_at(generation)?;
                let plan = self.current_plan(state, generation, snapshot.document.revision)?;
                Ok(PreviewReply::Ack {
                    revision: plan.revision,
                    plan_hash: plan.plan_hash,
                })
            }
            PreviewAction::Pause {} => {
                self.inner.playing.store(false, Ordering::Release);
                let snapshot = state.snapshot_at(generation)?;
                let plan = self.current_plan(state, generation, snapshot.document.revision)?;
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
                    let bytes = render_rgba_frame(&plan, frame, artifacts.as_ref(), &toolchain)?;
                    let key = format!(
                        "inspect:{}:{}:{}:{}",
                        plan.project_id, plan.revision, plan.plan_hash, frame
                    );
                    let artifact_id = artifacts.put_bytes(
                        &key,
                        "png",
                        "image/png",
                        &encode_rgba_png(plan.width, plan.height, &bytes)?,
                    )?;
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
        if session.generation != generation
            || session.project_id != project_id
            || session.revision != revision
            || session.plan_hash != plan_hash
        {
            return Err(AppError::stale_session(
                "The software preview packet belongs to a retired session",
            ));
        }
        acknowledge_software_preview(
            &session.ack_sequence,
            &session.presentation_start_clock,
            sequence,
            Instant::now(),
        );
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
            if current.generation != generation
                || current.project_id != project_id
                || current.revision != revision
                || current.plan_hash != plan_hash
            {
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
        self.invalidate_transport();
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

#[derive(Default)]
struct RenderScratch {
    canvas: Vec<PmPixel>,
}

/// A pooled canonical frame renderer.  One decoder process is retained per
/// active clip mapping for sequential playback, created on demand at the
/// requested source frame and retired when the clip leaves the current frame.
/// Export should use this instead of spawning FFmpeg once per frame.
pub struct CanonicalFrameRenderer {
    plan: RenderPlan,
    pool: Mutex<PersistentDecoderPool>,
    scratch: Mutex<RenderScratch>,
}

impl CanonicalFrameRenderer {
    pub fn new(
        plan: RenderPlan,
        artifacts: Arc<dyn ArtifactResolver>,
        toolchain: FfmpegToolchain,
    ) -> Result<Self, AppError> {
        plan.validate()?;
        let pool = PersistentDecoderPool::new(&plan, artifacts, &toolchain)?;
        Ok(Self {
            plan,
            pool: Mutex::new(pool),
            scratch: Mutex::new(RenderScratch::default()),
        })
    }

    pub fn plan(&self) -> &RenderPlan {
        &self.plan
    }

    pub fn render(&self, frame: u64) -> Result<Vec<u8>, AppError> {
        if frame >= self.plan.duration_frames {
            return Err(AppError::invalid_argument(
                "The requested frame is outside the render plan",
            ));
        }
        let mut pool = self
            .pool
            .lock()
            .map_err(|_| AppError::io("The pooled renderer lock is unavailable"))?;
        let mut scratch = self
            .scratch
            .lock()
            .map_err(|_| AppError::io("The pooled renderer scratch lock is unavailable"))?;
        let mut packed_rgba = Vec::new();
        let rgba = render_rgba_frame_with_pool(
            &self.plan,
            frame,
            &mut pool,
            &mut scratch.canvas,
            &mut packed_rgba,
        )?;
        Ok(rgba)
    }
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

/// Render one canonical RGBA frame.  The source frame decoder is called only
/// for normalized master artifacts; composition, transitions, and text all
/// happen on integer premultiplied pixels here.
pub fn render_rgba_frame(
    plan: &RenderPlan,
    frame: u64,
    artifacts: &dyn ArtifactResolver,
    toolchain: &FfmpegToolchain,
) -> Result<Vec<u8>, AppError> {
    plan.validate()?;
    if frame >= plan.duration_frames {
        return Err(AppError::invalid_argument(
            "The requested frame is outside the render plan",
        ));
    }
    let canvas_len = usize::try_from(plan.width)
        .ok()
        .and_then(|width| {
            usize::try_from(plan.height)
                .ok()
                .and_then(|height| width.checked_mul(height))
        })
        .ok_or_else(|| AppError::invalid_argument("Render canvas is too large"))?;
    let background = PmPixel::from_color(plan.background);
    let mut canvas = vec![background; canvas_len];
    for layer in &plan.layers {
        if layer.kind == RenderLayerKind::Video {
            let mut transitioned = HashSet::new();
            for transition in layer.transitions.iter().filter(|transition| {
                transition.start_frame <= frame && frame < transition.end_frame
            }) {
                transitioned.insert(transition.left_clip_id.as_str());
                transitioned.insert(transition.right_clip_id.as_str());
                let left = layer
                    .segments
                    .iter()
                    .find(|segment| segment.clip_id == transition.left_clip_id)
                    .ok_or_else(|| AppError::schema("A render transition has no left segment"))?;
                let right = layer
                    .segments
                    .iter()
                    .find(|segment| segment.clip_id == transition.right_clip_id)
                    .ok_or_else(|| AppError::schema("A render transition has no right segment"))?;
                let left_layer = compose_segment(plan, left, frame, artifacts, toolchain)?;
                let right_layer = compose_segment(plan, right, frame, artifacts, toolchain)?;
                let relative = frame - transition.start_frame;
                let mut group = vec![PmPixel::transparent(); canvas_len];
                for index in 0..canvas_len {
                    group[index] = PmPixel::weighted_pair(
                        left_layer[index],
                        right_layer[index],
                        relative,
                        transition.duration_frames,
                    );
                }
                blend_buffer(&mut canvas, &group);
            }
            for segment in &layer.segments {
                if transitioned.contains(segment.clip_id.as_str()) {
                    continue;
                }
                if segment.start_frame <= frame && frame < segment.end_frame {
                    let image = compose_segment(plan, segment, frame, artifacts, toolchain)?;
                    blend_buffer(&mut canvas, &image);
                }
            }
        }
        for overlay in layer
            .text_overlays
            .iter()
            .filter(|overlay| overlay.start_frame <= frame && frame < overlay.end_frame)
        {
            let image = compose_text_overlay(plan, overlay, frame, artifacts)?;
            blend_buffer(&mut canvas, &image);
        }
    }
    let mut rgba = Vec::with_capacity(canvas_len * 4);
    for pixel in canvas {
        rgba.extend_from_slice(&pixel.to_straight_rgba());
    }
    Ok(rgba)
}

fn compose_segment(
    plan: &RenderPlan,
    segment: &RenderSegment,
    frame: u64,
    artifacts: &dyn ArtifactResolver,
    toolchain: &FfmpegToolchain,
) -> Result<Vec<PmPixel>, AppError> {
    if frame < segment.start_frame || frame >= segment.end_frame {
        return Ok(vec![PmPixel::transparent(); canvas_len(plan)?]);
    }
    let source_frame = if segment.is_still_image {
        segment.source_start_frame
    } else {
        segment
            .source_start_frame
            .checked_add(frame - segment.start_frame)
            .ok_or_else(|| AppError::invalid_argument("Source frame mapping overflowed"))?
    };
    if source_frame < segment.active_start_frame || source_frame >= segment.active_end_frame {
        return Ok(vec![PmPixel::transparent(); canvas_len(plan)?]);
    }
    let master = artifacts.managed_path(&segment.artifact_id)?;
    let source = decode_rgba_frame(
        toolchain,
        &master,
        source_frame,
        plan.fps(),
        segment.source_width,
        segment.source_height,
    )?;
    let mut output = vec![PmPixel::transparent(); canvas_len(plan)?];
    blit_source(
        &mut output,
        plan.width,
        plan.height,
        &source,
        segment.source_width,
        segment.source_height,
        &segment.source_rect,
        &segment.dest_rect,
        segment.opacity,
    );
    Ok(output)
}

fn compose_text_overlay(
    plan: &RenderPlan,
    overlay: &RenderTextOverlay,
    _frame: u64,
    artifacts: &dyn ArtifactResolver,
) -> Result<Vec<PmPixel>, AppError> {
    let path = artifacts.managed_path(&overlay.raster_artifact_id)?;
    let mut file = File::open(&path).map_err(|_| {
        AppError::new(
            ErrorCode::AssetUnavailable,
            "The text raster artifact is unavailable",
        )
    })?;
    let size = file
        .metadata()
        .map_err(|_| {
            AppError::new(
                ErrorCode::AssetUnavailable,
                "The text raster artifact is unavailable",
            )
        })?
        .len();
    if size > MAX_RASTER_PNG_BYTES as u64 {
        return Err(AppError::new(
            ErrorCode::MediaUnsupported,
            "The raster PNG exceeds the supported size",
        ));
    }
    let capacity = usize::try_from(size).map_err(|_| {
        AppError::new(
            ErrorCode::MediaUnsupported,
            "The raster PNG allocation is too large",
        )
    })?;
    let mut bytes = Vec::with_capacity(capacity);
    file.take(MAX_RASTER_PNG_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| {
            AppError::new(
                ErrorCode::AssetUnavailable,
                "The text raster artifact is unavailable",
            )
        })?;
    if bytes.len() > MAX_RASTER_PNG_BYTES {
        return Err(AppError::new(
            ErrorCode::MediaUnsupported,
            "The raster PNG exceeds the supported size",
        ));
    }
    let (width, height, pixels) = decode_rgba_png(&bytes)?;
    let mut output = vec![PmPixel::transparent(); canvas_len(plan)?];
    blit_source(
        &mut output,
        plan.width,
        plan.height,
        &pixels,
        width,
        height,
        &overlay.raster_source_rect,
        &overlay.raster_dest_rect,
        10_000,
    );
    Ok(output)
}

fn canvas_len(plan: &RenderPlan) -> Result<usize, AppError> {
    usize::try_from(plan.width)
        .ok()
        .and_then(|width| {
            usize::try_from(plan.height)
                .ok()
                .and_then(|height| width.checked_mul(height))
        })
        .ok_or_else(|| AppError::invalid_argument("Render canvas is too large"))
}

fn blit_source(
    destination: &mut [PmPixel],
    canvas_width: u32,
    canvas_height: u32,
    source: &[u8],
    source_width: u32,
    source_height: u32,
    source_rect: &SourceRect,
    dest_rect: &DestRect,
    opacity: u16,
) {
    let x0 = i64::from(dest_rect.x).max(0) as u32;
    let y0 = i64::from(dest_rect.y).max(0) as u32;
    let x1 = (i64::from(dest_rect.x) + i64::from(dest_rect.width))
        .min(i64::from(canvas_width))
        .max(0) as u32;
    let y1 = (i64::from(dest_rect.y) + i64::from(dest_rect.height))
        .min(i64::from(canvas_height))
        .max(0) as u32;
    if x1 <= x0 || y1 <= y0 || source_rect.width == 0 || source_rect.height == 0 {
        return;
    }
    let source_columns: Vec<usize> = (x0..x1)
        .map(|x| {
            let rel_x = (i64::from(x) - i64::from(dest_rect.x)).max(0) as u64;
            (u64::from(source_rect.x)
                + rel_x.saturating_mul(u64::from(source_rect.width))
                    / u64::from(dest_rect.width.max(1)))
            .min(u64::from(
                source_rect
                    .x
                    .saturating_add(source_rect.width)
                    .saturating_sub(1),
            )) as usize
        })
        .collect();
    for y in y0..y1 {
        let rel_y = (i64::from(y) - i64::from(dest_rect.y)).max(0) as u64;
        let sy = (u64::from(source_rect.y)
            + rel_y.saturating_mul(u64::from(source_rect.height))
                / u64::from(dest_rect.height.max(1)))
        .min(u64::from(
            source_rect
                .y
                .saturating_add(source_rect.height)
                .saturating_sub(1),
        ));
        let source_row = sy as usize * source_width as usize;
        let destination_row = y as usize * canvas_width as usize + x0 as usize;
        for (column, &sx) in source_columns.iter().enumerate() {
            let source_index = (source_row + sx) * 4;
            if source_index + 3 >= source.len() {
                continue;
            }
            let pixel = PmPixel::from_rgba(
                source[source_index],
                source[source_index + 1],
                source[source_index + 2],
                source[source_index + 3],
                opacity,
            );
            let destination_index = destination_row + column;
            destination[destination_index] = destination[destination_index].over(pixel);
        }
    }
}

fn blend_buffer(destination: &mut [PmPixel], source: &[PmPixel]) {
    for (dst, src) in destination.iter_mut().zip(source.iter().copied()) {
        *dst = dst.over(src);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PmPixel {
    r: u32,
    g: u32,
    b: u32,
    a: u32,
}

impl PmPixel {
    fn transparent() -> Self {
        Self {
            r: 0,
            g: 0,
            b: 0,
            a: 0,
        }
    }

    fn from_color(color: RgbaColor) -> Self {
        Self::from_rgba(color.red, color.green, color.blue, color.alpha, 10_000)
    }

    fn from_rgba(red: u8, green: u8, blue: u8, alpha: u8, opacity: u16) -> Self {
        let alpha_8 = ((u32::from(alpha) * u32::from(opacity) + 5_000) / 10_000).min(255);
        if alpha_8 == 0 {
            return Self::transparent();
        }
        if alpha_8 == 255 {
            return Self {
                r: u32::from(red) * 257,
                g: u32::from(green) * 257,
                b: u32::from(blue) * 257,
                a: 65_535,
            };
        }
        let alpha = (alpha_8 * 65_535 + 127) / 255;
        Self {
            r: (u32::from(red) * alpha + 127) / 255,
            g: (u32::from(green) * alpha + 127) / 255,
            b: (u32::from(blue) * alpha + 127) / 255,
            a: alpha,
        }
    }

    fn over(self, source: Self) -> Self {
        // These identities hold for canonical premultiplied pixels and avoid
        // all four rounded divisions without changing their result.  Keep
        // the component checks so malformed private values still use the
        // original saturating arithmetic below.
        if source.a == 0 && source.r == 0 && source.g == 0 && source.b == 0 && self.is_u16() {
            return self;
        }
        if self.a == 0 && self.r == 0 && self.g == 0 && self.b == 0 && source.is_u16() {
            return source;
        }
        if source.a == 65_535 && source.is_u16() {
            return source;
        }
        let inverse = 65_535u32.saturating_sub(source.a);
        Self {
            r: source
                .r
                .saturating_add((self.r * inverse + 32_767) / 65_535)
                .min(65_535),
            g: source
                .g
                .saturating_add((self.g * inverse + 32_767) / 65_535)
                .min(65_535),
            b: source
                .b
                .saturating_add((self.b * inverse + 32_767) / 65_535)
                .min(65_535),
            a: source
                .a
                .saturating_add((self.a * inverse + 32_767) / 65_535)
                .min(65_535),
        }
    }

    fn is_u16(self) -> bool {
        self.r <= 65_535 && self.g <= 65_535 && self.b <= 65_535 && self.a <= 65_535
    }

    fn weighted_pair(left: Self, right: Self, numerator: u64, denominator: u64) -> Self {
        let denominator = denominator.max(1);
        let incoming = numerator.min(denominator);
        let outgoing = denominator - incoming;
        let blend = |a: u32, b: u32| {
            ((u128::from(a) * u128::from(outgoing) + u128::from(b) * u128::from(incoming))
                / u128::from(denominator)) as u32
        };
        Self {
            r: blend(left.r, right.r),
            g: blend(left.g, right.g),
            b: blend(left.b, right.b),
            a: blend(left.a, right.a),
        }
    }

    fn to_straight_rgba(self) -> [u8; 4] {
        if self.a == 0 {
            return [0, 0, 0, 0];
        }
        if self.a == 65_535 {
            return [
                ((self.r * 255 + 32_767) / 65_535).min(255) as u8,
                ((self.g * 255 + 32_767) / 65_535).min(255) as u8,
                ((self.b * 255 + 32_767) / 65_535).min(255) as u8,
                255,
            ];
        }
        [
            ((self.r * 255 + self.a / 2) / self.a).min(255) as u8,
            ((self.g * 255 + self.a / 2) / self.a).min(255) as u8,
            ((self.b * 255 + self.a / 2) / self.a).min(255) as u8,
            ((self.a * 255 + 32_767) / 65_535).min(255) as u8,
        ]
    }
}

/// Mix an arbitrary indexed 48 kHz window from the immutable audio envelope.
pub fn render_audio_window(
    plan: &RenderPlan,
    start_sample: u64,
    sample_count: u64,
    artifacts: &dyn ArtifactResolver,
) -> Result<Vec<f32>, AppError> {
    plan.validate()?;
    let end_sample = start_sample
        .checked_add(sample_count)
        .ok_or_else(|| AppError::invalid_argument("Audio window end overflowed"))?;
    if end_sample > plan.audio.total_samples {
        return Err(AppError::invalid_argument(
            "Audio window exceeds the project duration",
        ));
    }
    let output_len = usize::try_from(sample_count)
        .ok()
        .and_then(|value| value.checked_mul(2))
        .ok_or_else(|| AppError::invalid_argument("Audio window is too large"))?;
    let mut mixed = vec![0.0f32; output_len];
    for segment in &plan.audio.segments {
        let overlap_start = start_sample.max(segment.start_sample);
        let overlap_end = end_sample.min(segment.end_sample);
        if overlap_start >= overlap_end {
            continue;
        }
        let source_start = segment
            .source_start_sample
            .checked_add(overlap_start - segment.start_sample)
            .ok_or_else(|| AppError::invalid_argument("Audio source mapping overflowed"))?;
        let count = overlap_end - overlap_start;
        let path = artifacts.managed_path(&segment.artifact_id)?;
        let mut file = File::open(path).map_err(|_| {
            AppError::new(
                ErrorCode::AssetUnavailable,
                "The PCM artifact is unavailable",
            )
        })?;
        let source = read_f32_stereo_window(&mut file, source_start, count)?;
        let gain = 10.0f32.powf((segment.gain_db as f32) / 20.0);
        for sample_offset in 0..count as usize {
            let absolute = overlap_start + sample_offset as u64;
            let mut envelope = 1.0f32;
            if segment.fade_in_samples > 0 {
                envelope *= ((absolute - segment.start_sample) as f32
                    / segment.fade_in_samples as f32)
                    .clamp(0.0, 1.0);
            }
            if segment.fade_out_samples > 0 {
                envelope *= ((segment.end_sample - absolute) as f32
                    / segment.fade_out_samples as f32)
                    .clamp(0.0, 1.0);
            }
            if let Some(transition) = segment.transition_in.as_ref() {
                if absolute >= transition.start_sample && absolute < transition.end_sample {
                    let numerator = absolute - transition.start_sample;
                    let denominator = (transition.end_sample - transition.start_sample).max(1);
                    envelope *= numerator as f32 / denominator as f32;
                }
            }
            if let Some(transition) = segment.transition_out.as_ref() {
                if absolute >= transition.start_sample && absolute < transition.end_sample {
                    let numerator = absolute - transition.start_sample;
                    let denominator = (transition.end_sample - transition.start_sample).max(1);
                    envelope *= 1.0 - numerator as f32 / denominator as f32;
                }
            }
            let index = ((absolute - start_sample) as usize) * 2;
            mixed[index] += source[sample_offset * 2] * gain * envelope;
            mixed[index + 1] += source[sample_offset * 2 + 1] * gain * envelope;
        }
    }
    for value in &mut mixed {
        if !value.is_finite() {
            return Err(AppError::new(
                ErrorCode::MediaUnsupported,
                "The audio mix produced a non-finite sample",
            ));
        }
        *value = value.clamp(-1.0, 1.0);
    }
    Ok(mixed)
}

const MAX_RASTER_PNG_BYTES: usize = 64 * 1024 * 1024;
const MAX_RASTER_PNG_DIMENSION: u32 = 4_096;
const MAX_RASTER_PNG_PIXELS: u64 = 16_000_000;
const MAX_TEXT_RASTER_CACHE_BYTES: usize = 64 * 1024 * 1024;
const MAX_TEXT_RASTER_CACHE_ENTRIES: usize = 256;

fn decode_rgba_png(bytes: &[u8]) -> Result<(u32, u32, Vec<u8>), AppError> {
    if bytes.len() < 8 || &bytes[..8] != b"\x89PNG\r\n\x1a\n" {
        return Err(AppError::new(
            ErrorCode::MediaUnsupported,
            "The raster artifact is not a PNG",
        ));
    }
    if bytes.len() > MAX_RASTER_PNG_BYTES {
        return Err(AppError::new(
            ErrorCode::MediaUnsupported,
            "The raster PNG exceeds the supported size",
        ));
    }

    let mut decoder = png::Decoder::new_with_limits(
        Cursor::new(bytes),
        png::Limits {
            bytes: MAX_RASTER_PNG_BYTES,
        },
    );
    decoder.set_transformations(
        png::Transformations::EXPAND | png::Transformations::STRIP_16 | png::Transformations::ALPHA,
    );
    let (width, height) = {
        let header = decoder.read_header_info().map_err(|_| {
            AppError::new(
                ErrorCode::MediaUnsupported,
                "The raster artifact is not a valid PNG",
            )
        })?;
        (header.width, header.height)
    };
    if width == 0
        || height == 0
        || width > MAX_RASTER_PNG_DIMENSION
        || height > MAX_RASTER_PNG_DIMENSION
    {
        return Err(AppError::new(
            ErrorCode::MediaUnsupported,
            "The raster PNG dimensions exceed the supported range",
        ));
    }
    let pixel_count = u64::from(width)
        .checked_mul(u64::from(height))
        .ok_or_else(|| {
            AppError::new(
                ErrorCode::MediaUnsupported,
                "The raster PNG dimensions overflowed",
            )
        })?;
    if pixel_count > MAX_RASTER_PNG_PIXELS {
        return Err(AppError::new(
            ErrorCode::MediaUnsupported,
            "The raster PNG exceeds the 16 megapixel limit",
        ));
    }
    let rgba_len = usize::try_from(pixel_count)
        .ok()
        .and_then(|count| count.checked_mul(4))
        .ok_or_else(|| {
            AppError::new(
                ErrorCode::MediaUnsupported,
                "The raster PNG allocation is too large",
            )
        })?;

    let mut reader = decoder.read_info().map_err(|_| {
        AppError::new(
            ErrorCode::MediaUnsupported,
            "The raster artifact is not a valid PNG",
        )
    })?;
    if reader.info().animation_control.is_some() {
        return Err(AppError::new(
            ErrorCode::MediaUnsupported,
            "Animated PNG raster artifacts are unsupported",
        ));
    }
    let (output_color_type, output_bit_depth) = reader.output_color_type();
    if output_bit_depth != png::BitDepth::Eight {
        return Err(AppError::new(
            ErrorCode::MediaUnsupported,
            "The raster PNG could not be normalized to 8-bit pixels",
        ));
    }
    let output_size = reader.output_buffer_size();
    if output_size > rgba_len {
        return Err(AppError::new(
            ErrorCode::MediaUnsupported,
            "The raster PNG decoded allocation is too large",
        ));
    }
    let mut decoded = vec![0u8; output_size];
    let output_info = reader
        .next_frame(&mut decoded)
        .map_err(|_| AppError::schema("The raster PNG pixel data is invalid"))?;
    if output_info.width != width || output_info.height != height {
        return Err(AppError::schema(
            "The raster PNG frame dimensions are invalid",
        ));
    }

    let width_usize =
        usize::try_from(width).map_err(|_| AppError::schema("The raster PNG width is invalid"))?;
    let height_usize = usize::try_from(height)
        .map_err(|_| AppError::schema("The raster PNG height is invalid"))?;
    let channels = match output_color_type {
        png::ColorType::Grayscale => 1,
        png::ColorType::GrayscaleAlpha => 2,
        png::ColorType::Rgb => 3,
        png::ColorType::Rgba => 4,
        png::ColorType::Indexed => {
            return Err(AppError::new(
                ErrorCode::MediaUnsupported,
                "The raster PNG palette could not be expanded",
            ));
        }
    };
    let row_bytes = width_usize
        .checked_mul(channels)
        .ok_or_else(|| AppError::schema("The raster PNG row size overflowed"))?;
    if output_info.line_size != row_bytes
        || output_info
            .line_size
            .checked_mul(height_usize)
            .is_none_or(|size| size != decoded.len())
    {
        return Err(AppError::schema("The raster PNG scanlines are invalid"));
    }

    let mut rgba = vec![0u8; rgba_len];
    for row in 0..height_usize {
        let source = &decoded[row * row_bytes..(row + 1) * row_bytes];
        let destination = &mut rgba[row * width_usize * 4..(row + 1) * width_usize * 4];
        match output_color_type {
            png::ColorType::Grayscale => {
                for (gray, pixel) in source.iter().zip(destination.chunks_exact_mut(4)) {
                    pixel.copy_from_slice(&[*gray, *gray, *gray, 255]);
                }
            }
            png::ColorType::GrayscaleAlpha => {
                for (pair, pixel) in source.chunks_exact(2).zip(destination.chunks_exact_mut(4)) {
                    pixel.copy_from_slice(&[pair[0], pair[0], pair[0], pair[1]]);
                }
            }
            png::ColorType::Rgb => {
                for (triplet, pixel) in source.chunks_exact(3).zip(destination.chunks_exact_mut(4))
                {
                    pixel[..3].copy_from_slice(triplet);
                    pixel[3] = 255;
                }
            }
            png::ColorType::Rgba => {
                destination.copy_from_slice(source);
            }
            png::ColorType::Indexed => {
                return Err(AppError::new(
                    ErrorCode::MediaUnsupported,
                    "The raster PNG palette could not be expanded",
                ));
            }
        }
    }
    Ok((width, height, rgba))
}

struct SoftwarePreviewProcessRegistry {
    cancel: Arc<AtomicBool>,
    pids: Mutex<HashSet<u32>>,
}

impl SoftwarePreviewProcessRegistry {
    fn new(cancel: Arc<AtomicBool>) -> Self {
        Self {
            cancel,
            pids: Mutex::new(HashSet::new()),
        }
    }

    fn register(&self, pid: u32) {
        let mut pids = self
            .pids
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if self.cancel.load(Ordering::Acquire) {
            kill_software_process(pid);
        } else {
            pids.insert(pid);
        }
    }

    fn unregister(&self, pid: u32) {
        let mut pids = self
            .pids
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        pids.remove(&pid);
    }

    fn kill_all(&self) {
        let mut pids = self
            .pids
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        for pid in pids.iter().copied() {
            kill_software_process(pid);
        }
        pids.clear();
    }
}

#[cfg(unix)]
fn kill_software_process(pid: u32) {
    if let Ok(pid) = libc::pid_t::try_from(pid) {
        // SAFETY: the PID is registered immediately after spawning an owned
        // FFmpeg child and remains registered until termination is initiated.
        unsafe {
            let _ = libc::kill(pid, libc::SIGKILL);
        }
    }
}

#[cfg(not(unix))]
fn kill_software_process(_pid: u32) {}

fn terminate_software_child(child: &mut Child, processes: &SoftwarePreviewProcessRegistry) {
    let pid = child.id();
    let _ = child.kill();
    // Remove ownership before wait can reap the child, so a later PID reuse
    // can never be mistaken for this process by kill_all.
    processes.unregister(pid);
    let _ = child.wait();
}

const INITIAL_SOFTWARE_PREVIEW_SEQUENCE: u64 = 1;

#[derive(Debug)]
struct PresentationStartClock {
    expected_first_sequence: u64,
    receipt: Option<Instant>,
}

impl PresentationStartClock {
    fn new(expected_first_sequence: u64) -> Self {
        Self {
            expected_first_sequence,
            receipt: None,
        }
    }

    fn reset(&mut self, expected_first_sequence: u64) {
        self.expected_first_sequence = expected_first_sequence;
        self.receipt = None;
    }

    fn record(&mut self, sequence: u64, receipt: Instant) -> bool {
        if sequence < self.expected_first_sequence || self.receipt.is_some() {
            return false;
        }
        self.receipt = Some(receipt);
        true
    }

    fn receipt(&self) -> Option<Instant> {
        self.receipt
    }
}

fn acknowledge_software_preview(
    ack_sequence: &AtomicU64,
    presentation_start_clock: &Mutex<PresentationStartClock>,
    sequence: u64,
    receipt: Instant,
) {
    let mut clock = presentation_start_clock
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let _ = clock.record(sequence, receipt);
    ack_sequence.fetch_max(sequence, Ordering::AcqRel);
}

fn reset_presentation_start_clock(
    presentation_start_clock: &Mutex<PresentationStartClock>,
    expected_first_sequence: u64,
) {
    let mut clock = presentation_start_clock
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    clock.reset(expected_first_sequence);
}

fn presentation_start_receipt(
    presentation_start_clock: &Mutex<PresentationStartClock>,
) -> Option<Instant> {
    let clock = presentation_start_clock
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    clock.receipt()
}

struct SoftwarePreviewSession {
    generation: u64,
    project_id: String,
    revision: u64,
    plan_hash: String,
    cancel: Arc<AtomicBool>,
    processes: Arc<SoftwarePreviewProcessRegistry>,
    requested_frame: Arc<AtomicU64>,
    ack_sequence: Arc<AtomicU64>,
    presentation_start_clock: Arc<Mutex<PresentationStartClock>>,
    sender: mpsc::SyncSender<SoftwarePreviewPacket>,
    receiver: Arc<Mutex<Option<mpsc::Receiver<SoftwarePreviewPacket>>>>,
    worker: Option<thread::JoinHandle<()>>,
}

impl SoftwarePreviewSession {
    fn start(
        plan: RenderPlan,
        generation: u64,
        start_frame: u64,
        artifacts: Arc<dyn ArtifactResolver>,
        toolchain: FfmpegToolchain,
    ) -> Result<Self, AppError> {
        if start_frame >= plan.duration_frames {
            return Err(AppError::invalid_argument(
                "Software preview start is outside the plan",
            ));
        }
        let (sender, receiver) = mpsc::sync_channel(3);
        let cancel = Arc::new(AtomicBool::new(false));
        let processes = Arc::new(SoftwarePreviewProcessRegistry::new(cancel.clone()));
        let requested_frame = Arc::new(AtomicU64::new(start_frame));
        let ack_sequence = Arc::new(AtomicU64::new(0));
        let presentation_start_clock = Arc::new(Mutex::new(PresentationStartClock::new(
            INITIAL_SOFTWARE_PREVIEW_SEQUENCE,
        )));
        let worker_cancel = cancel.clone();
        let worker_processes = processes.clone();
        let worker_request = requested_frame.clone();
        let worker_ack = ack_sequence.clone();
        let worker_presentation_start_clock = presentation_start_clock.clone();
        let worker_sender = sender.clone();
        let worker_plan = plan.clone();
        let worker = thread::Builder::new()
            .name("cutterhoochee-software-preview".to_owned())
            .spawn(move || {
                software_worker(
                    worker_plan,
                    generation,
                    worker_cancel,
                    worker_processes,
                    worker_request,
                    worker_ack,
                    worker_presentation_start_clock,
                    worker_sender,
                    artifacts,
                    toolchain,
                );
            })
            .map_err(|_| AppError::io("The software preview worker could not start"))?;
        Ok(Self {
            generation,
            project_id: plan.project_id,
            revision: plan.revision,
            plan_hash: plan.plan_hash,
            cancel,
            processes,
            requested_frame,
            ack_sequence,
            presentation_start_clock,
            sender,
            receiver: Arc::new(Mutex::new(Some(receiver))),
            worker: Some(worker),
        })
    }

    fn receiver(&self) -> SoftwarePreviewReceiver {
        SoftwarePreviewReceiver {
            receiver: self
                .receiver
                .lock()
                .ok()
                .and_then(|mut receiver| receiver.take()),
            requested_frame: self.requested_frame.clone(),
            ack_sequence: self.ack_sequence.clone(),
            presentation_start_clock: self.presentation_start_clock.clone(),
            cancel: self.cancel.clone(),
        }
    }

    fn stop(mut self) {
        self.cancel.store(true, Ordering::Release);
        self.processes.kill_all();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

pub struct SoftwarePreviewReceiver {
    receiver: Option<mpsc::Receiver<SoftwarePreviewPacket>>,
    requested_frame: Arc<AtomicU64>,
    ack_sequence: Arc<AtomicU64>,
    presentation_start_clock: Arc<Mutex<PresentationStartClock>>,
    cancel: Arc<AtomicBool>,
}

impl SoftwarePreviewReceiver {
    pub fn recv_timeout(
        &self,
        timeout: Duration,
    ) -> Result<SoftwarePreviewPacket, mpsc::RecvTimeoutError> {
        match self.receiver.as_ref() {
            Some(receiver) => receiver.recv_timeout(timeout),
            None => Err(mpsc::RecvTimeoutError::Disconnected),
        }
    }

    pub fn seek(&self, frame: u64) {
        self.requested_frame.store(frame, Ordering::Release);
    }

    pub fn acknowledge(&self, sequence: u64) {
        acknowledge_software_preview(
            &self.ack_sequence,
            &self.presentation_start_clock,
            sequence,
            Instant::now(),
        );
    }

    pub fn stop(&self) {
        self.cancel.store(true, Ordering::Release);
    }
}

fn software_worker(
    plan: RenderPlan,
    generation: u64,
    cancel: Arc<AtomicBool>,
    processes: Arc<SoftwarePreviewProcessRegistry>,
    requested_frame: Arc<AtomicU64>,
    ack_sequence: Arc<AtomicU64>,
    presentation_start_clock: Arc<Mutex<PresentationStartClock>>,
    sender: mpsc::SyncSender<SoftwarePreviewPacket>,
    artifacts: Arc<dyn ArtifactResolver>,
    toolchain: FfmpegToolchain,
) {
    let raster_plan = match software_raster_plan(&plan) {
        Ok(plan) => plan,
        Err(_) => return,
    };
    let mut sequence = INITIAL_SOFTWARE_PREVIEW_SEQUENCE;
    let mut current_frame = requested_frame.load(Ordering::Acquire);
    let mut last_requested_frame = current_frame;
    let mut cadence_start = current_frame;
    let mut output_tick = 0u64;
    let mut presentation_started = false;
    let mut pace_anchor = None;
    let mut decoder_pool = match PersistentDecoderPool::new_with_processes(
        &raster_plan,
        artifacts.clone(),
        &toolchain,
        processes.clone(),
    ) {
        Ok(pool) => pool,
        Err(_) => return,
    };
    let mut encoder = match PersistentJpegEncoder::new(&raster_plan, &toolchain, processes) {
        Ok(encoder) => encoder,
        Err(_) => return,
    };
    let mut canvas = Vec::new();
    let mut packed_rgba = Vec::new();
    while !cancel.load(Ordering::Acquire) {
        let desired = requested_frame.load(Ordering::Acquire);
        if desired != last_requested_frame {
            last_requested_frame = desired;
            current_frame = desired.min(plan.duration_frames.saturating_sub(1));
            cadence_start = current_frame;
            output_tick = 0;
            pace_anchor = None;
            reset_presentation_start_clock(&presentation_start_clock, sequence);
            presentation_started = false;
            let _ = decoder_pool.restart();
            let _ = encoder.restart();
        }
        if current_frame >= plan.duration_frames {
            break;
        }
        if let Some(anchor) = pace_anchor {
            let target_nanos = match software_preview_target_nanos(
                // Fill the bounded three-frame queue ahead of presentation so
                // a newly active decoder can start before its frame is due.
                output_tick.saturating_sub(2),
                raster_plan.fps_num,
                raster_plan.fps_den,
            ) {
                Ok(target) => target,
                Err(_) => break,
            };
            if !wait_for_software_target(
                anchor,
                target_nanos,
                &cancel,
                &requested_frame,
                last_requested_frame,
            ) {
                return;
            }
        }
        let rgba = match render_rgba_frame_with_pool(
            &raster_plan,
            current_frame,
            &mut decoder_pool,
            &mut canvas,
            &mut packed_rgba,
        ) {
            Ok(bytes) => bytes,
            Err(_) => break,
        };
        let jpeg = match encoder.encode(&rgba, raster_plan.width, raster_plan.height) {
            Ok(bytes) => bytes,
            Err(_) => break,
        };
        packed_rgba = rgba;
        // Do not count decoder/encoder startup against playback time.  The
        // initial frame gates audio in the presentation layer.
        if pace_anchor.is_none() {
            pace_anchor = Some(Instant::now());
        }
        let packet = SoftwarePreviewPacket {
            // Identity always comes from the canonical plan.  Only the
            // payload dimensions describe the private software raster.
            generation,
            project_id: plan.project_id.clone(),
            revision: plan.revision,
            plan_hash: plan.plan_hash.clone(),
            frame: current_frame,
            sequence,
            width: raster_plan.width,
            height: raster_plan.height,
            content_type: "image/jpeg".to_owned(),
            data: jpeg,
        };
        // A bounded queue is intentional backpressure, but `try_send` lets a
        // seek or cancellation interrupt a producer even when all three
        // presentation slots are occupied.
        let requested_before = last_requested_frame;
        loop {
            match sender.try_send(packet.clone()) {
                Ok(()) => break,
                Err(mpsc::TrySendError::Full(_)) => {
                    if cancel.load(Ordering::Acquire)
                        || requested_frame.load(Ordering::Acquire) != requested_before
                    {
                        return;
                    }
                    thread::sleep(Duration::from_millis(2));
                }
                Err(mpsc::TrySendError::Disconnected(_)) => return,
            }
        }
        if requested_frame.load(Ordering::Acquire) != requested_before {
            return;
        }
        sequence = sequence.wrapping_add(1);
        while sequence.saturating_sub(ack_sequence.load(Ordering::Acquire)) > 3 {
            if cancel.load(Ordering::Acquire)
                || requested_frame.load(Ordering::Acquire) != requested_before
            {
                return;
            }
            thread::sleep(Duration::from_millis(2));
        }
        // The first presentation ACK is withheld until PCM playback starts.
        // Decoder startup and the initial three-slot prefetch must not make
        // visual catch-up skip ahead of the newly started audio clock.
        if !presentation_started {
            if let Some(receipt) = presentation_start_receipt(&presentation_start_clock) {
                pace_anchor = Some(receipt);
                presentation_started = true;
            }
        }

        let Some(mut next_tick) = output_tick.checked_add(1) else {
            break;
        };
        let mut next_frame = match software_preview_sample_frame(
            cadence_start,
            next_tick,
            raster_plan.fps_num,
            raster_plan.fps_den,
        ) {
            Ok(frame) => frame,
            Err(_) => break,
        };
        // If composition fell behind its wall-clock target, skip only visual
        // ticks.  The decoder pool remains sequential for the small skips
        // implied by the cadence; audio is never restarted for them.
        if let Some(anchor) = pace_anchor {
            while next_frame < plan.duration_frames {
                if cancel.load(Ordering::Acquire)
                    || requested_frame.load(Ordering::Acquire) != requested_before
                {
                    return;
                }
                let target_nanos = match software_preview_target_nanos(
                    next_tick,
                    raster_plan.fps_num,
                    raster_plan.fps_den,
                ) {
                    Ok(target) => target,
                    Err(_) => break,
                };
                if target_nanos > anchor.elapsed().as_nanos() {
                    break;
                }
                next_tick = match next_tick.checked_add(1) {
                    Some(tick) => tick,
                    None => break,
                };
                next_frame = match software_preview_sample_frame(
                    cadence_start,
                    next_tick,
                    raster_plan.fps_num,
                    raster_plan.fps_den,
                ) {
                    Ok(frame) => frame,
                    Err(_) => break,
                };
            }
        }
        output_tick = next_tick;
        current_frame = next_frame;
    }
}
#[derive(Debug, Clone, PartialEq, Eq)]
struct TextRasterCacheKey {
    raster_artifact_id: String,
    canvas_width: u32,
    canvas_height: u32,
    source_x: u32,
    source_y: u32,
    source_width: u32,
    source_height: u32,
    dest_x: i32,
    dest_y: i32,
    dest_width: u32,
    dest_height: u32,
}

impl TextRasterCacheKey {
    fn new(plan: &RenderPlan, overlay: &RenderTextOverlay) -> Self {
        Self {
            raster_artifact_id: overlay.raster_artifact_id.clone(),
            canvas_width: plan.width,
            canvas_height: plan.height,
            source_x: overlay.raster_source_rect.x,
            source_y: overlay.raster_source_rect.y,
            source_width: overlay.raster_source_rect.width,
            source_height: overlay.raster_source_rect.height,
            dest_x: overlay.raster_dest_rect.x,
            dest_y: overlay.raster_dest_rect.y,
            dest_width: overlay.raster_dest_rect.width,
            dest_height: overlay.raster_dest_rect.height,
        }
    }

    fn matches(&self, plan: &RenderPlan, overlay: &RenderTextOverlay) -> bool {
        self.canvas_width == plan.width
            && self.canvas_height == plan.height
            && self.raster_artifact_id == overlay.raster_artifact_id
            && self.source_x == overlay.raster_source_rect.x
            && self.source_y == overlay.raster_source_rect.y
            && self.source_width == overlay.raster_source_rect.width
            && self.source_height == overlay.raster_source_rect.height
            && self.dest_x == overlay.raster_dest_rect.x
            && self.dest_y == overlay.raster_dest_rect.y
            && self.dest_width == overlay.raster_dest_rect.width
            && self.dest_height == overlay.raster_dest_rect.height
    }
}

struct TextRasterCacheEntry {
    key: TextRasterCacheKey,
    pixels: Vec<PmPixel>,
    bytes: usize,
}

#[derive(Default)]
struct TextRasterCache {
    entries: VecDeque<TextRasterCacheEntry>,
    bytes: usize,
}

impl TextRasterCache {
    fn get(&mut self, plan: &RenderPlan, overlay: &RenderTextOverlay) -> Option<&[PmPixel]> {
        let index = self
            .entries
            .iter()
            .position(|entry| entry.key.matches(plan, overlay))?;
        if index != 0 {
            let entry = self.entries.remove(index)?;
            self.entries.push_front(entry);
        }
        self.entries.front().map(|entry| entry.pixels.as_slice())
    }

    fn insert(&mut self, key: TextRasterCacheKey, pixels: Vec<PmPixel>) {
        let Some(bytes) = pixels
            .capacity()
            .checked_mul(std::mem::size_of::<PmPixel>())
        else {
            return;
        };
        if bytes > MAX_TEXT_RASTER_CACHE_BYTES {
            return;
        }
        if let Some(index) = self.entries.iter().position(|entry| entry.key == key) {
            if let Some(entry) = self.entries.remove(index) {
                self.bytes = self.bytes.saturating_sub(entry.bytes);
            }
        }
        while self.entries.len() >= MAX_TEXT_RASTER_CACHE_ENTRIES
            || self.bytes.saturating_add(bytes) > MAX_TEXT_RASTER_CACHE_BYTES
        {
            let Some(entry) = self.entries.pop_back() else {
                break;
            };
            self.bytes = self.bytes.saturating_sub(entry.bytes);
        }
        if self.entries.len() >= MAX_TEXT_RASTER_CACHE_ENTRIES
            || self.bytes.saturating_add(bytes) > MAX_TEXT_RASTER_CACHE_BYTES
        {
            return;
        }
        self.bytes += bytes;
        self.entries
            .push_front(TextRasterCacheEntry { key, pixels, bytes });
    }
}

#[derive(Clone)]
struct PersistentDecoderSpec {
    artifact_id: String,
    width: u32,
    height: u32,
    is_still_image: bool,
}

struct PersistentDecoder {
    mapping_id: String,
    path: PathBuf,
    width: u32,
    height: u32,
    is_still_image: bool,
    next_frame: u64,
    cached_still: bool,
    frame_buffer: Vec<u8>,
    child: Child,
    stdout: ChildStdout,
}

struct PersistentDecoderPool {
    // `entries` contains only decoders that are currently alive.  `specs`
    // keeps cheap per-clip metadata so an inactive timeline clip owns no
    // process until its source frame is requested.
    entries: HashMap<String, PersistentDecoder>,
    specs: HashMap<String, PersistentDecoderSpec>,
    active_clips: HashSet<String>,
    text_cache: TextRasterCache,
    artifacts: Arc<dyn ArtifactResolver>,
    toolchain: FfmpegToolchain,
    processes: Arc<SoftwarePreviewProcessRegistry>,
    fps_num: u32,
    fps_den: u32,
    max_frame_step: u64,
}

impl PersistentDecoderPool {
    fn new(
        plan: &RenderPlan,
        artifacts: Arc<dyn ArtifactResolver>,
        toolchain: &FfmpegToolchain,
    ) -> Result<Self, AppError> {
        let cancel = Arc::new(AtomicBool::new(false));
        let processes = Arc::new(SoftwarePreviewProcessRegistry::new(cancel));
        Self::new_with_processes(plan, artifacts, toolchain, processes)
    }

    fn new_with_processes(
        plan: &RenderPlan,
        artifacts: Arc<dyn ArtifactResolver>,
        toolchain: &FfmpegToolchain,
        processes: Arc<SoftwarePreviewProcessRegistry>,
    ) -> Result<Self, AppError> {
        let mut specs = HashMap::<String, PersistentDecoderSpec>::new();
        for layer in &plan.layers {
            for segment in &layer.segments {
                // A clip/source mapping gets its own decoder.  Two copies of
                // one asset may be at different source offsets simultaneously.
                // Keep only metadata here; the process and managed path are
                // both acquired when this clip first becomes active.
                specs
                    .entry(segment.clip_id.clone())
                    .or_insert_with(|| PersistentDecoderSpec {
                        artifact_id: segment.artifact_id.clone(),
                        width: segment.source_width,
                        height: segment.source_height,
                        is_still_image: segment.is_still_image,
                    });
            }
        }
        Ok(Self {
            entries: HashMap::new(),
            specs,
            active_clips: HashSet::new(),
            text_cache: TextRasterCache::default(),
            artifacts,
            toolchain: toolchain.clone(),
            processes,
            fps_num: plan.fps_num,
            fps_den: plan.fps_den,
            max_frame_step: software_preview_max_frame_step(plan.fps_num, plan.fps_den),
        })
    }

    fn restart(&mut self) -> Result<(), AppError> {
        let processes = self.processes.clone();
        for decoder in self.entries.values_mut() {
            terminate_software_child(&mut decoder.child, &processes);
        }
        self.entries.clear();
        self.active_clips.clear();
        Ok(())
    }

    fn begin_frame(&mut self) {
        self.active_clips.clear();
    }

    fn retire_inactive(&mut self) {
        let inactive = self
            .entries
            .keys()
            .filter(|mapping_id| !self.active_clips.contains(*mapping_id))
            .cloned()
            .collect::<Vec<_>>();
        let processes = self.processes.clone();
        for mapping_id in inactive {
            if let Some(mut decoder) = self.entries.remove(&mapping_id) {
                terminate_software_child(&mut decoder.child, &processes);
            }
        }
        self.active_clips.clear();
    }

    fn start_decoder(&mut self, mapping_id: &str, start_frame: u64) -> Result<(), AppError> {
        let spec = self.specs.get(mapping_id).cloned().ok_or_else(|| {
            AppError::new(
                ErrorCode::AssetUnavailable,
                "A software-preview decoder is unavailable",
            )
        })?;
        let path = self.artifacts.managed_path(&spec.artifact_id)?;
        let processes = self.processes.clone();
        if let Some(mut decoder) = self.entries.remove(mapping_id) {
            terminate_software_child(&mut decoder.child, &processes);
        }
        let decoder = spawn_persistent_decoder(
            mapping_id,
            path,
            spec.width,
            spec.height,
            spec.is_still_image,
            start_frame,
            self.fps_num,
            self.fps_den,
            &self.toolchain,
            &processes,
        )?;
        self.entries.insert(mapping_id.to_owned(), decoder);
        Ok(())
    }

    fn frame(
        &mut self,
        mapping_id: &str,
        frame: u64,
        width: u32,
        height: u32,
    ) -> Result<&[u8], AppError> {
        self.active_clips.insert(mapping_id.to_owned());
        let needs_restart = match self.entries.get(mapping_id) {
            Some(decoder) => {
                let cached_still = decoder.is_still_image
                    && decoder.cached_still
                    && width == decoder.width
                    && height == decoder.height;
                // A 60 fps source sampled at 30 fps requests every other
                // source frame.  Keep that small sequential skip on the
                // existing decoder; only a larger discontinuity seeks.
                !cached_still
                    && (decoder.next_frame > frame
                        || frame.saturating_sub(decoder.next_frame) >= self.max_frame_step)
            }
            None => true,
        };
        if needs_restart {
            self.start_decoder(mapping_id, frame)?;
        }
        let decoder = self.entries.get_mut(mapping_id).ok_or_else(|| {
            AppError::new(
                ErrorCode::AssetUnavailable,
                "A software-preview decoder is unavailable",
            )
        })?;
        if decoder.is_still_image && decoder.cached_still {
            if width == decoder.width && height == decoder.height {
                return Ok(decoder.frame_buffer.as_slice());
            }
        }
        let skip = frame.saturating_sub(decoder.next_frame);
        for _ in 0..=skip {
            decoder
                .stdout
                .read_exact(&mut decoder.frame_buffer)
                .map_err(|_| {
                    AppError::new(
                        ErrorCode::AssetUnavailable,
                        "The software-preview decoder ended early",
                    )
                })?;
        }
        decoder.next_frame = frame.saturating_add(1);
        if decoder.is_still_image {
            decoder.cached_still = true;
        }
        if width == decoder.width && height == decoder.height {
            return Ok(decoder.frame_buffer.as_slice());
        }
        Err(AppError::invalid_argument(
            "Software-preview source dimensions differ from the plan",
        ))
    }
}
fn persistent_decoder_frame_size(width: u32, height: u32) -> Result<usize, AppError> {
    usize::try_from(width)
        .ok()
        .and_then(|value| {
            usize::try_from(height)
                .ok()
                .and_then(|height| value.checked_mul(height))
        })
        .and_then(|value| value.checked_mul(4))
        .ok_or_else(|| AppError::invalid_argument("Software preview frame is too large"))
}

impl Drop for PersistentDecoderPool {
    fn drop(&mut self) {
        let processes = self.processes.clone();
        for decoder in self.entries.values_mut() {
            terminate_software_child(&mut decoder.child, &processes);
        }
    }
}

fn spawn_persistent_decoder(
    mapping_id: &str,
    path: PathBuf,
    width: u32,
    height: u32,
    is_still_image: bool,
    start_frame: u64,
    fps_num: u32,
    fps_den: u32,
    toolchain: &FfmpegToolchain,
    processes: &SoftwarePreviewProcessRegistry,
) -> Result<PersistentDecoder, AppError> {
    let frame_size = persistent_decoder_frame_size(width, height)?;
    let pre_roll_frames = u64::from(fps_num / fps_den.max(1)).max(1);
    let pre_roll_frame = start_frame.saturating_sub(pre_roll_frames);
    let delta_frame = start_frame - pre_roll_frame;
    let pre_roll = frame_timestamp(pre_roll_frame, fps_num, fps_den)?;
    let delta = frame_timestamp(delta_frame, fps_num, fps_den)?;
    let mut command = Command::new(&toolchain.ffmpeg);
    command
        .arg("-hide_banner")
        .arg("-nostdin")
        .arg("-loglevel")
        .arg("error")
        .arg("-threads")
        .arg("4")
        .arg("-filter_threads")
        .arg("1")
        .arg("-protocol_whitelist")
        .arg("file,pipe")
        .arg("-format_whitelist")
        .arg("mov,matroska,avi,mpegts,mpeg,flv,ogg,mp3,wav,flac,aac,image2,png_pipe,jpeg_pipe,webp_pipe")
        .arg("-ss")
        .arg(pre_roll)
        .arg("-i")
        .arg(&path)
        .arg("-ss")
        .arg(delta)
        .arg("-map")
        .arg("0:v:0")
        .arg("-vf")
        .arg(format!("scale={width}:{height}:flags=bicubic,format=rgba"))
        .arg("-f")
        .arg("rawvideo")
        .arg("pipe:1")
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut child = command.spawn().map_err(|_| {
        AppError::new(
            ErrorCode::MediaUnsupported,
            "The software-preview FFmpeg worker could not start",
        )
    })?;
    let process_id = child.id();
    let stdout = match child.stdout.take() {
        Some(stdout) => stdout,
        None => {
            terminate_software_child(&mut child, processes);
            return Err(AppError::io(
                "The software-preview decoder pipe is unavailable",
            ));
        }
    };
    processes.register(process_id);
    Ok(PersistentDecoder {
        mapping_id: mapping_id.to_owned(),
        path,
        width,
        height,
        is_still_image,
        next_frame: start_frame,
        cached_still: false,
        frame_buffer: vec![0u8; frame_size],
        child,
        stdout,
    })
}

fn frame_timestamp(frame: u64, fps_num: u32, fps_den: u32) -> Result<String, AppError> {
    let micros = u128::from(frame)
        .checked_mul(u128::from(fps_den))
        .and_then(|value| value.checked_mul(1_000_000))
        .and_then(|value| value.checked_div(u128::from(fps_num.max(1))))
        .ok_or_else(|| AppError::invalid_argument("Software-preview timestamp overflowed"))?;
    Ok(format!("{}.{:06}", micros / 1_000_000, micros % 1_000_000))
}

fn render_rgba_frame_with_pool(
    plan: &RenderPlan,
    frame: u64,
    decoders: &mut PersistentDecoderPool,
    canvas: &mut Vec<PmPixel>,
    packed_rgba: &mut Vec<u8>,
) -> Result<Vec<u8>, AppError> {
    // The persistent decoder path mirrors render_rgba_frame's composition but
    // keeps only active per-clip processes, creating each at its requested
    // source frame.  Keeping this path separate prevents a software fallback
    // from accidentally changing canonical geometry.
    let canvas_len = canvas_len(plan)?;
    decoders.begin_frame();
    let background = PmPixel::from_color(plan.background);
    canvas.resize(canvas_len, background);
    canvas.fill(background);
    for layer in &plan.layers {
        if layer.kind == RenderLayerKind::Video {
            let mut transitioned = HashSet::new();
            for transition in layer
                .transitions
                .iter()
                .filter(|value| value.start_frame <= frame && frame < value.end_frame)
            {
                transitioned.insert(transition.left_clip_id.as_str());
                transitioned.insert(transition.right_clip_id.as_str());
                let left = layer
                    .segments
                    .iter()
                    .find(|segment| segment.clip_id == transition.left_clip_id)
                    .ok_or_else(|| AppError::schema("A render transition has no left segment"))?;
                let right = layer
                    .segments
                    .iter()
                    .find(|segment| segment.clip_id == transition.right_clip_id)
                    .ok_or_else(|| AppError::schema("A render transition has no right segment"))?;
                let a = compose_segment_with_pool(plan, left, frame, decoders)?;
                let b = compose_segment_with_pool(plan, right, frame, decoders)?;
                let mut group = vec![PmPixel::transparent(); canvas_len];
                let relative = frame - transition.start_frame;
                for index in 0..canvas_len {
                    group[index] = PmPixel::weighted_pair(
                        a[index],
                        b[index],
                        relative,
                        transition.duration_frames,
                    );
                }
                blend_buffer(canvas, &group);
            }
            for segment in &layer.segments {
                if !transitioned.contains(segment.clip_id.as_str())
                    && segment.start_frame <= frame
                    && frame < segment.end_frame
                {
                    render_segment_into_pool(plan, segment, frame, decoders, canvas)?;
                }
            }
        }
        // Text artifacts remain managed PNGs and are shared by both paths.
        for overlay in layer
            .text_overlays
            .iter()
            .filter(|overlay| overlay.start_frame <= frame && frame < overlay.end_frame)
        {
            if let Some(image) = decoders.text_cache.get(plan, overlay) {
                blend_buffer(canvas, image);
            } else {
                let image =
                    compose_text_overlay(plan, overlay, frame, decoders.artifacts.as_ref())?;
                blend_buffer(canvas, &image);
                decoders
                    .text_cache
                    .insert(TextRasterCacheKey::new(plan, overlay), image);
            }
        }
    }
    decoders.retire_inactive();
    let packed_len = canvas_len
        .checked_mul(4)
        .ok_or_else(|| AppError::invalid_argument("Render canvas is too large"))?;
    packed_rgba.resize(packed_len, 0);
    for (index, pixel) in canvas.iter().enumerate() {
        let offset = index * 4;
        packed_rgba[offset..offset + 4].copy_from_slice(&pixel.to_straight_rgba());
    }
    Ok(std::mem::take(packed_rgba))
}

fn render_segment_into_pool(
    plan: &RenderPlan,
    segment: &RenderSegment,
    frame: u64,
    decoders: &mut PersistentDecoderPool,
    destination: &mut [PmPixel],
) -> Result<(), AppError> {
    let source_frame = if segment.is_still_image {
        segment.source_start_frame
    } else {
        segment
            .source_start_frame
            .checked_add(frame - segment.start_frame)
            .ok_or_else(|| AppError::invalid_argument("Source frame mapping overflowed"))?
    };
    if source_frame < segment.active_start_frame || source_frame >= segment.active_end_frame {
        return Ok(());
    }
    let source = decoders.frame(
        &segment.clip_id,
        source_frame,
        segment.source_width,
        segment.source_height,
    )?;
    blit_source(
        destination,
        plan.width,
        plan.height,
        source,
        segment.source_width,
        segment.source_height,
        &segment.source_rect,
        &segment.dest_rect,
        segment.opacity,
    );
    Ok(())
}

fn compose_segment_with_pool(
    plan: &RenderPlan,
    segment: &RenderSegment,
    frame: u64,
    decoders: &mut PersistentDecoderPool,
) -> Result<Vec<PmPixel>, AppError> {
    let mut output = vec![PmPixel::transparent(); canvas_len(plan)?];
    render_segment_into_pool(plan, segment, frame, decoders, &mut output)?;
    Ok(output)
}

struct PersistentJpegEncoder {
    toolchain: FfmpegToolchain,
    width: u32,
    height: u32,
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    processes: Arc<SoftwarePreviewProcessRegistry>,
}

impl PersistentJpegEncoder {
    fn new(
        plan: &RenderPlan,
        toolchain: &FfmpegToolchain,
        processes: Arc<SoftwarePreviewProcessRegistry>,
    ) -> Result<Self, AppError> {
        spawn_jpeg_encoder(plan.width, plan.height, toolchain, &processes)
    }

    fn restart(&mut self) -> Result<(), AppError> {
        let toolchain = self.toolchain.clone();
        let processes = self.processes.clone();
        terminate_software_child(&mut self.child, &processes);
        let replacement = spawn_jpeg_encoder(self.width, self.height, &toolchain, &processes)?;
        *self = replacement;
        Ok(())
    }

    fn encode(&mut self, rgba: &[u8], width: u32, height: u32) -> Result<Vec<u8>, AppError> {
        if width != self.width || height != self.height {
            return Err(AppError::invalid_argument(
                "Software-preview encoder dimensions changed",
            ));
        }
        self.stdin
            .write_all(rgba)
            .map_err(|_| AppError::io("The software-preview encoder input failed"))?;
        self.stdin
            .flush()
            .map_err(|_| AppError::io("The software-preview encoder input failed"))?;
        read_jpeg(&mut self.stdout)
    }
}
impl Drop for PersistentJpegEncoder {
    fn drop(&mut self) {
        let processes = self.processes.clone();
        terminate_software_child(&mut self.child, &processes);
    }
}

fn spawn_jpeg_encoder(
    width: u32,
    height: u32,
    toolchain: &FfmpegToolchain,
    processes: &Arc<SoftwarePreviewProcessRegistry>,
) -> Result<PersistentJpegEncoder, AppError> {
    let mut command = Command::new(&toolchain.ffmpeg);
    command
        .arg("-hide_banner")
        .arg("-nostdin")
        .arg("-loglevel")
        .arg("error")
        .arg("-f")
        .arg("rawvideo")
        .arg("-pix_fmt")
        .arg("rgba")
        .arg("-video_size")
        .arg(format!("{width}x{height}"))
        .arg("-framerate")
        .arg("30")
        .arg("-i")
        .arg("pipe:0")
        .arg("-f")
        .arg("image2pipe")
        .arg("-vcodec")
        .arg("mjpeg")
        .arg("-thread_type")
        .arg("slice")
        .arg("-threads")
        .arg("4")
        .arg("-q:v")
        .arg("5")
        .arg("pipe:1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut child = command.spawn().map_err(|_| {
        AppError::new(
            ErrorCode::MediaUnsupported,
            "The software-preview JPEG encoder could not start",
        )
    })?;
    let process_id = child.id();
    let stdin = match child.stdin.take() {
        Some(stdin) => stdin,
        None => {
            terminate_software_child(&mut child, processes);
            return Err(AppError::io(
                "The software-preview encoder input is unavailable",
            ));
        }
    };
    let stdout = match child.stdout.take() {
        Some(stdout) => stdout,
        None => {
            terminate_software_child(&mut child, processes);
            return Err(AppError::io(
                "The software-preview encoder output is unavailable",
            ));
        }
    };
    processes.register(process_id);
    Ok(PersistentJpegEncoder {
        toolchain: toolchain.clone(),
        width,
        height,
        child,
        stdin,
        stdout: BufReader::with_capacity(64 * 1024, stdout),
        processes: processes.clone(),
    })
}

fn read_jpeg(stdout: &mut impl Read) -> Result<Vec<u8>, AppError> {
    let mut output = Vec::new();
    let mut byte = [0u8; 1];
    let mut started = false;
    loop {
        stdout
            .read_exact(&mut byte)
            .map_err(|_| AppError::io("The software-preview JPEG encoder ended early"))?;
        output.push(byte[0]);
        if !started {
            if output.len() >= 2 && output[output.len() - 2..] == [0xff, 0xd8] {
                started = true;
                if output.len() > 2 {
                    output.drain(..output.len() - 2);
                }
            }
        } else if output.len() >= 2 && output[output.len() - 2..] == [0xff, 0xd9] {
            return Ok(output);
        }
        if output.len() > 4 * 1024 * 1024 {
            return Err(AppError::new(
                ErrorCode::MediaUnsupported,
                "The software-preview JPEG exceeded its size limit",
            ));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clipped_scaled_and_cropped_sources_preserve_pixel_mapping() {
        let source: Vec<u8> = (1u8..=8).flat_map(|red| [red, 0, 0, 255]).collect();
        let cases = [
            (
                SourceRect {
                    x: 1,
                    y: 0,
                    width: 3,
                    height: 2,
                },
                DestRect {
                    x: -1,
                    y: -1,
                    width: 6,
                    height: 4,
                },
                [2, 3, 3, 4, 6, 7, 7, 8],
            ),
            (
                SourceRect {
                    x: 0,
                    y: 0,
                    width: 4,
                    height: 2,
                },
                DestRect {
                    x: 0,
                    y: 0,
                    width: 2,
                    height: 1,
                },
                [1, 3, 99, 99, 99, 99, 99, 99],
            ),
            (
                SourceRect {
                    x: 1,
                    y: 0,
                    width: 2,
                    height: 2,
                },
                DestRect {
                    x: 0,
                    y: 0,
                    width: 4,
                    height: 2,
                },
                [2, 2, 3, 3, 6, 6, 7, 7],
            ),
        ];
        for (source_rect, dest_rect, expected) in cases {
            let mut destination = vec![PmPixel::from_rgba(99, 0, 0, 255, 10_000); 8];
            blit_source(
                &mut destination,
                4,
                2,
                &source,
                4,
                2,
                &source_rect,
                &dest_rect,
                10_000,
            );
            assert_eq!(
                destination
                    .iter()
                    .map(|pixel| pixel.to_straight_rgba()[0])
                    .collect::<Vec<_>>(),
                expected,
            );
        }
    }

    #[test]
    fn jpeg_stream_preserves_prefetched_frame_boundaries() {
        let first = [0xff, 0xd8, 1, 2, 0xff, 0x00, 3, 0xff, 0xd9];
        let second = [0xff, 0xd8, 4, 5, 0xff, 0xd9];
        let bytes = [first.as_slice(), second.as_slice()].concat();
        let mut reader = BufReader::with_capacity(64, Cursor::new(bytes));
        assert_eq!(read_jpeg(&mut reader).unwrap(), first);
        assert_eq!(read_jpeg(&mut reader).unwrap(), second);
        assert!(read_jpeg(&mut reader).is_err());
    }

    #[test]
    fn premultiplied_dissolve_composes_group_once() {
        let lower = PmPixel::from_rgba(20, 40, 60, 255, 10_000);
        let left = PmPixel::from_rgba(255, 0, 0, 128, 10_000);
        let right = PmPixel::from_rgba(0, 0, 255, 128, 10_000);
        let group = PmPixel::weighted_pair(left, right, 1, 2);
        let result = lower.over(group);
        assert!(result.r > 0 && result.b > 0);
        assert!(result.a >= lower.a);
    }

    #[test]
    fn audio_final_mix_clips_only_after_sum() {
        let left = PmPixel::from_rgba(255, 0, 0, 128, 10_000);
        let right = PmPixel::from_rgba(0, 255, 0, 128, 10_000);
        let group = PmPixel::weighted_pair(left, right, 1, 2);
        assert!(group.r > 0 && group.g > 0);
    }
    #[test]
    fn decodes_compressed_png_with_straight_rgba_and_alpha() {
        let pixels = [
            255, 0, 0, 128, 12, 34, 56, 255, 255, 0, 0, 128, 12, 34, 56, 255, 255, 0, 0, 128, 12,
            34, 56, 255, 255, 0, 0, 128, 12, 34, 56, 255,
        ];
        let mut encoded = Vec::new();
        {
            let mut encoder = png::Encoder::new(&mut encoded, 4, 2);
            encoder.set_color(png::ColorType::Rgba);
            encoder.set_depth(png::BitDepth::Eight);
            encoder.set_compression(png::Compression::Best);
            let mut writer = encoder.write_header().unwrap();
            writer.write_image_data(&pixels).unwrap();
            writer.finish().unwrap();
        }

        let (width, height, decoded) = decode_rgba_png(&encoded).unwrap();
        assert_eq!((width, height), (4, 2));
        assert_eq!(decoded, pixels);
    }
    #[test]
    fn premultiplied_fast_paths_match_original_arithmetic() {
        let original_from_rgba =
            |red: u8, green: u8, blue: u8, alpha: u8, opacity: u16| -> PmPixel {
                let alpha_8 = ((u32::from(alpha) * u32::from(opacity) + 5_000) / 10_000).min(255);
                let alpha = (alpha_8 * 65_535 + 127) / 255;
                PmPixel {
                    r: (u32::from(red) * alpha + 127) / 255,
                    g: (u32::from(green) * alpha + 127) / 255,
                    b: (u32::from(blue) * alpha + 127) / 255,
                    a: alpha,
                }
            };
        let original_over = |destination: PmPixel, source: PmPixel| -> PmPixel {
            let inverse = 65_535u32.saturating_sub(source.a);
            PmPixel {
                r: source
                    .r
                    .saturating_add((destination.r * inverse + 32_767) / 65_535)
                    .min(65_535),
                g: source
                    .g
                    .saturating_add((destination.g * inverse + 32_767) / 65_535)
                    .min(65_535),
                b: source
                    .b
                    .saturating_add((destination.b * inverse + 32_767) / 65_535)
                    .min(65_535),
                a: source
                    .a
                    .saturating_add((destination.a * inverse + 32_767) / 65_535)
                    .min(65_535),
            }
        };
        let original_to_straight = |pixel: PmPixel| -> [u8; 4] {
            if pixel.a == 0 {
                return [0, 0, 0, 0];
            }
            [
                ((pixel.r * 255 + pixel.a / 2) / pixel.a).min(255) as u8,
                ((pixel.g * 255 + pixel.a / 2) / pixel.a).min(255) as u8,
                ((pixel.b * 255 + pixel.a / 2) / pixel.a).min(255) as u8,
                ((pixel.a * 255 + 32_767) / 65_535).min(255) as u8,
            ]
        };

        let samples = [
            PmPixel::transparent(),
            PmPixel::from_rgba(17, 31, 43, 255, 10_000),
            PmPixel::from_rgba(255, 0, 128, 128, 10_000),
            PmPixel::from_rgba(3, 7, 11, 1, 10_000),
        ];
        for destination in samples.iter().copied() {
            for source in samples.iter().copied() {
                assert_eq!(destination.over(source), original_over(destination, source));
            }
        }

        for (red, green, blue, alpha, opacity) in [
            (0, 0, 0, 0, 10_000),
            (255, 128, 1, 255, 10_000),
            (255, 128, 1, 255, 9_981),
            (21, 34, 55, 128, 5_000),
            (21, 34, 55, 255, 0),
        ] {
            assert_eq!(
                PmPixel::from_rgba(red, green, blue, alpha, opacity),
                original_from_rgba(red, green, blue, alpha, opacity)
            );
        }

        for pixel in samples {
            assert_eq!(pixel.to_straight_rgba(), original_to_straight(pixel));
        }
    }
    #[test]
    fn software_raster_dimensions_and_geometry_follow_contract() {
        assert_eq!(software_raster_dimensions(1_920, 1_080).unwrap().0, 960);
        assert_eq!(software_raster_dimensions(1_920, 1_080).unwrap().1, 540);
        assert_eq!(software_raster_dimensions(1_080, 1_920).unwrap().0, 540);
        assert_eq!(software_raster_dimensions(1_080, 1_920).unwrap().1, 960);
        assert_eq!(software_raster_dimensions(1_080, 1_080).unwrap().0, 540);
        assert_eq!(software_raster_dimensions(1_080, 1_080).unwrap().1, 540);
        assert_eq!(software_raster_dimensions(320, 240).unwrap().0, 320);
        assert_eq!(software_raster_dimensions(320, 240).unwrap().1, 240);

        let original = DestRect {
            x: 100,
            y: 200,
            width: 801,
            height: 401,
        };
        assert_eq!(
            scale_dest_rect(&original, 0.5).unwrap(),
            DestRect {
                x: 50,
                y: 100,
                width: 401,
                height: 201,
            }
        );
        assert_eq!(original.x, 100);
        assert_eq!(original.y, 200);
        assert_eq!(original.width, 801);
        assert_eq!(original.height, 401);
    }

    #[test]
    fn presentation_start_clock_preserves_receipt_across_delayed_observation_and_reset() {
        let base = Instant::now();
        let first_receipt = base + Duration::from_millis(7);
        let later_receipt = base + Duration::from_millis(29);
        let stale_receipt = base + Duration::from_millis(41);
        let new_receipt = base + Duration::from_millis(53);
        let mut clock = PresentationStartClock::new(7);

        assert!(clock.record(7, first_receipt));
        assert!(!clock.record(8, later_receipt));
        assert_eq!(clock.receipt(), Some(first_receipt));

        clock.reset(11);
        assert_eq!(clock.receipt(), None);
        assert!(!clock.record(10, stale_receipt));
        assert_eq!(clock.receipt(), None);
        assert!(clock.record(11, new_receipt));
        assert_eq!(clock.receipt(), Some(new_receipt));
    }

    #[test]
    fn software_preview_cadence_uses_canonical_rational_frames() {
        let frames = |fps_num: u32, fps_den: u32| {
            (0..5)
                .map(|tick| software_preview_sample_frame(0, tick, fps_num, fps_den).unwrap())
                .collect::<Vec<_>>()
        };
        assert_eq!(frames(24, 1), vec![0, 1, 2, 3, 4]);
        assert_eq!(frames(30_000, 1_001), vec![0, 1, 2, 3, 4]);
        assert_eq!(frames(30, 1), vec![0, 1, 2, 3, 4]);
        assert_eq!(frames(60, 1), vec![0, 2, 4, 6, 8]);
        assert_eq!(software_preview_target_nanos(1, 24, 1).unwrap(), 41_666_666);
        assert_eq!(
            software_preview_target_nanos(1, 30_000, 1_001).unwrap(),
            33_366_666
        );
        assert_eq!(software_preview_target_nanos(1, 30, 1).unwrap(), 33_333_333);
        assert_eq!(software_preview_target_nanos(1, 60, 1).unwrap(), 33_333_333);
    }
}
