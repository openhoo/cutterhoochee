//! Linux software-preview rasterization, pacing, sessions, and JPEG transport.

use super::decoder::{
    terminate_software_child, PersistentDecoderPool, SoftwarePreviewProcessRegistry,
};
use super::frame::render_rgba_frame_with_pool;
use super::SoftwarePreviewPacket;
use crate::error::{AppError, ErrorCode};
use crate::media::ffmpeg::FfmpegToolchain;
use crate::media::render_plan::{ArtifactResolver, DestRect, RenderPlan};
use std::io::{BufReader, Read, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

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

pub(super) struct SoftwarePreviewSession {
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
    pub(super) fn start(
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

    pub(super) fn matches_identity(
        &self,
        generation: u64,
        project_id: &str,
        revision: u64,
        plan_hash: &str,
    ) -> bool {
        self.generation == generation
            && self.project_id == project_id
            && self.revision == revision
            && self.plan_hash == plan_hash
    }

    pub(super) fn acknowledge(&self, sequence: u64) {
        acknowledge_software_preview(
            &self.ack_sequence,
            &self.presentation_start_clock,
            sequence,
            Instant::now(),
        );
    }

    pub(super) fn receiver(&self) -> SoftwarePreviewReceiver {
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

    pub(super) fn stop(mut self) {
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
    let mut transition_left = Vec::new();
    let mut transition_right = Vec::new();
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
        if render_rgba_frame_with_pool(
            &raster_plan,
            current_frame,
            &mut decoder_pool,
            &mut canvas,
            &mut transition_left,
            &mut transition_right,
            &mut packed_rgba,
        )
        .is_err()
        {
            break;
        }
        let jpeg = match encoder.encode(&packed_rgba, raster_plan.width, raster_plan.height) {
            Ok(bytes) => bytes,
            Err(_) => break,
        };
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
    use std::io::{BufReader, Cursor};

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
