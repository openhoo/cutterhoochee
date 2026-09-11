//! Owned FFmpeg decoder processes and their per-plan pool.

use super::cache::TextRasterCache;
use crate::error::{AppError, ErrorCode};
use crate::media::ffmpeg::FfmpegToolchain;
use crate::media::render_plan::{ArtifactResolver, RenderPlan};
use std::collections::{HashMap, HashSet};
use std::io::Read;
use std::path::PathBuf;
use std::process::{Child, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

fn software_preview_max_frame_step(fps_num: u32, fps_den: u32) -> u64 {
    // A late visual frame must not trigger a seek/restart feedback loop.
    // Discard up to one second of forward frames through the existing decoder;
    // larger jumps still use the bounded one-second seek preroll.
    let step = u128::from(fps_num).div_ceil(u128::from(fps_den).max(1));
    u64::try_from(step).unwrap_or(u64::MAX).max(2)
}

pub(super) struct SoftwarePreviewProcessRegistry {
    cancel: Arc<AtomicBool>,
    pids: Mutex<HashSet<u32>>,
}

impl SoftwarePreviewProcessRegistry {
    pub(super) fn new(cancel: Arc<AtomicBool>) -> Self {
        Self {
            cancel,
            pids: Mutex::new(HashSet::new()),
        }
    }

    pub(super) fn register(&self, pid: u32) {
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

    pub(super) fn unregister(&self, pid: u32) {
        let mut pids = self
            .pids
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        pids.remove(&pid);
    }

    pub(super) fn kill_all(&self) {
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

pub(super) fn terminate_software_child(
    child: &mut Child,
    processes: &SoftwarePreviewProcessRegistry,
) {
    let pid = child.id();
    let _ = child.kill();
    // Remove ownership before wait can reap the child, so a later PID reuse
    // can never be mistaken for this process by kill_all.
    processes.unregister(pid);
    let _ = child.wait();
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

pub(super) struct PersistentDecoderPool {
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
    pub(super) fn new(
        plan: &RenderPlan,
        artifacts: Arc<dyn ArtifactResolver>,
        toolchain: &FfmpegToolchain,
    ) -> Result<Self, AppError> {
        let cancel = Arc::new(AtomicBool::new(false));
        let processes = Arc::new(SoftwarePreviewProcessRegistry::new(cancel));
        Self::new_with_processes(plan, artifacts, toolchain, processes)
    }

    pub(super) fn new_with_processes(
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

    pub(super) fn text_cache(&mut self) -> &mut TextRasterCache {
        &mut self.text_cache
    }

    pub(super) fn artifacts(&self) -> &dyn ArtifactResolver {
        self.artifacts.as_ref()
    }

    pub(super) fn restart(&mut self) -> Result<(), AppError> {
        let processes = self.processes.clone();
        for decoder in self.entries.values_mut() {
            terminate_software_child(&mut decoder.child, &processes);
        }
        self.entries.clear();
        self.active_clips.clear();
        Ok(())
    }

    pub(super) fn begin_frame(&mut self) {
        self.active_clips.clear();
    }

    pub(super) fn retire_inactive(&mut self) {
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

    pub(super) fn frame(
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
