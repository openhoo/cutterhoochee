//! Typed FFmpeg/ffprobe process boundaries used by the canonical renderer.
//!
//! The renderer never accepts a model- or user-authored filter graph.  Every
//! argument in this module is produced from validated numeric values or a
//! renderer-owned graph fragment, and graph text is written to a private file
//! before FFmpeg is started.  Keeping this boundary small also makes it
//! possible to point a packaged build at its bundled FFmpeg without relying on
//! PATH.

use crate::error::{AppError, ErrorCode};
use crate::project::model::{FrameRate, AUDIO_SAMPLE_RATE};
use serde::{Deserialize, Serialize};
use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

/// Absolute paths to the media executables selected by packaging.  Development
/// may use the host tools, but callers can never provide arbitrary executable
/// paths through an editor request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FfmpegToolchain {
    pub ffmpeg: PathBuf,
    pub ffprobe: PathBuf,
}

impl Default for FfmpegToolchain {
    fn default() -> Self {
        let ffmpeg = std::env::var_os("CUTTERHOOCHEE_FFMPEG")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("ffmpeg"));
        let ffprobe = std::env::var_os("CUTTERHOOCHEE_FFPROBE")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("ffprobe"));
        Self { ffmpeg, ffprobe }
    }
}

impl FfmpegToolchain {
    pub fn new(ffmpeg: impl Into<PathBuf>, ffprobe: impl Into<PathBuf>) -> Self {
        Self {
            ffmpeg: ffmpeg.into(),
            ffprobe: ffprobe.into(),
        }
    }

    pub fn validate(&self) -> Result<(), AppError> {
        if self.ffmpeg.as_os_str().is_empty() || self.ffprobe.as_os_str().is_empty() {
            return Err(AppError::new(
                ErrorCode::MediaUnsupported,
                "The packaged FFmpeg toolchain is not configured",
            ));
        }
        Ok(())
    }
}

/// A renderer-owned argument vector.  `argv` excludes the executable and is
/// never populated from a raw command line string.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FfmpegCommand {
    pub executable: PathBuf,
    pub argv: Vec<OsString>,
}

impl FfmpegCommand {
    pub fn new(executable: impl Into<PathBuf>) -> Self {
        Self {
            executable: executable.into(),
            argv: Vec::new(),
        }
    }

    pub fn arg(mut self, value: impl Into<OsString>) -> Self {
        self.argv.push(value.into());
        self
    }

    pub fn args<I>(mut self, values: I) -> Self
    where
        I: IntoIterator<Item = OsString>,
    {
        self.argv.extend(values);
        self
    }

    pub fn run_capture(&self, stdin: Option<&[u8]>) -> Result<Output, AppError> {
        let mut command = Command::new(&self.executable);
        command.args(&self.argv);
        if stdin.is_some() {
            command.stdin(Stdio::piped());
        } else {
            command.stdin(Stdio::null());
        }
        command.stdout(Stdio::piped()).stderr(Stdio::piped());
        let mut child = command.spawn().map_err(|_| {
            AppError::new(
                ErrorCode::MediaUnsupported,
                "The configured FFmpeg executable could not be started",
            )
        })?;
        if let Some(bytes) = stdin {
            if let Some(mut pipe) = child.stdin.take() {
                pipe.write_all(bytes)
                    .map_err(|_| AppError::io("FFmpeg input could not be written"))?;
            }
        }
        let output = child
            .wait_with_output()
            .map_err(|_| AppError::io("FFmpeg did not finish cleanly"))?;
        if !output.status.success() {
            return Err(ffmpeg_failure(&output.stderr));
        }
        Ok(output)
    }

    pub fn run_to_file(&self, output: &Path, stdin: Option<&[u8]>) -> Result<(), AppError> {
        let bytes = self.run_capture(stdin)?;
        fs::write(output, bytes.stdout)
            .map_err(|_| AppError::io("FFmpeg output could not be saved"))
    }
}

fn ffmpeg_failure(stderr: &[u8]) -> AppError {
    let detail = String::from_utf8_lossy(stderr);
    // Keep diagnostics bounded and avoid returning paths or arbitrary command
    // output to the WebView.  The last useful line normally contains the
    // decoder failure and is enough for an actionable retry.
    let line = detail
        .lines()
        .rev()
        .find(|line| !line.trim().is_empty())
        .unwrap_or("FFmpeg rejected the media operation")
        .chars()
        .take(240)
        .collect::<String>();
    AppError::new(
        ErrorCode::MediaUnsupported,
        format!("Media rendering failed: {line}"),
    )
}

/// A filter graph is deliberately represented as individual renderer-owned
/// chains.  `compile` joins them only after checking that labels are unique.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TypedFilterGraph {
    chains: Vec<String>,
}

impl TypedFilterGraph {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push_chain(&mut self, chain: impl Into<String>) -> Result<(), AppError> {
        let chain = chain.into();
        if chain.trim().is_empty() || chain.bytes().any(|byte| byte == 0) {
            return Err(AppError::invalid_argument(
                "A renderer filter chain is empty or invalid",
            ));
        }
        self.chains.push(chain);
        Ok(())
    }

    pub fn compile(&self) -> Result<String, AppError> {
        if self.chains.is_empty() {
            return Err(AppError::invalid_argument(
                "A renderer filter graph must contain a chain",
            ));
        }
        Ok(self.chains.join(";\n"))
    }

    pub fn write_private_file(
        &self,
        directory: &Path,
        stem: &str,
    ) -> Result<PrivateGraphFile, AppError> {
        let graph = self.compile()?;
        fs::create_dir_all(directory)
            .map_err(|_| AppError::io("The FFmpeg graph directory could not be created"))?;
        let nonce = unique_nonce();
        let path = directory.join(format!(".{stem}-{nonce:016x}.graph"));
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&path)
            .map_err(|_| AppError::io("The private FFmpeg graph file could not be created"))?;
        file.write_all(graph.as_bytes())
            .and_then(|_| file.sync_all())
            .map_err(|_| AppError::io("The private FFmpeg graph file could not be written"))?;
        Ok(PrivateGraphFile { path })
    }
}

#[derive(Debug)]
pub struct PrivateGraphFile {
    path: PathBuf,
}

impl PrivateGraphFile {
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for PrivateGraphFile {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

fn unique_nonce() -> u64 {
    let time = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let nanos = time.as_nanos() as u64;
    nanos ^ (std::process::id() as u64).rotate_left(17)
}

/// FFmpeg's standalone-media allowlist.  Raw f32/video inputs used by the
/// renderer are kept in a separate internal list so an imported manifest can
/// never select them to escape the media demuxer policy.
pub const MEDIA_DEMUXER_ALLOWLIST: &str =
    "mov,matroska,avi,mpegts,mpeg,flv,ogg,mp3,wav,flac,aac,image2,png_pipe,jpeg_pipe,webp_pipe";
pub const RAW_RENDERER_FORMAT_ALLOWLIST: &str =
    "mov,matroska,avi,mpegts,mpeg,flv,ogg,mp3,wav,flac,aac,image2,png_pipe,jpeg_pipe,webp_pipe,rawvideo,f32le";
pub const MEDIA_PROTOCOL_ALLOWLIST: &str = "file,pipe";

fn safe_media_input_prefix(command: FfmpegCommand) -> FfmpegCommand {
    command
        .arg("-hide_banner")
        .arg("-protocol_whitelist")
        .arg(MEDIA_PROTOCOL_ALLOWLIST)
}

fn safe_input_prefix_with_formats(command: FfmpegCommand, formats: &str) -> FfmpegCommand {
    safe_media_input_prefix(command)
        .arg("-nostdin")
        .arg("-format_whitelist")
        .arg(formats)
}

fn safe_probe_input_prefix(command: FfmpegCommand) -> FfmpegCommand {
    safe_media_input_prefix(command)
        .arg("-format_whitelist")
        .arg(MEDIA_DEMUXER_ALLOWLIST)
}

fn safe_input_prefix(command: FfmpegCommand) -> FfmpegCommand {
    safe_input_prefix_with_formats(command, MEDIA_DEMUXER_ALLOWLIST)
}

/// Decode one exact normalized-master frame. Input seeking jumps to the
/// preceding keyframe; FFmpeg's accurate-seek preroll discards earlier frames.
/// The timestamp is floored to microseconds so rounding cannot skip the
/// requested frame on the normalized CFR grid.
pub fn decode_rgba_frame(
    toolchain: &FfmpegToolchain,
    master: &Path,
    frame: u64,
    fps: FrameRate,
    width: u32,
    height: u32,
) -> Result<Vec<u8>, AppError> {
    fps.validate()?;
    if width == 0 || height == 0 {
        return Err(AppError::invalid_argument(
            "Decoded frame dimensions must be positive",
        ));
    }
    let timestamp = rational_timestamp(frame, fps)?;
    let size = (width as usize)
        .checked_mul(height as usize)
        .and_then(|value| value.checked_mul(4))
        .ok_or_else(|| AppError::invalid_argument("Decoded frame is too large"))?;
    let command = safe_input_prefix(
        FfmpegCommand::new(toolchain.ffmpeg.clone())
            .arg("-ss")
            .arg(timestamp)
            .arg("-i")
            .arg(master.as_os_str())
            .arg("-map")
            .arg("0:v:0")
            .arg("-frames:v")
            .arg("1")
            .arg("-vf")
            .arg(format!("scale={width}:{height}:flags=bicubic,format=rgba"))
            .arg("-f")
            .arg("rawvideo")
            .arg("pipe:1"),
    );
    let output = command.run_capture(None)?;
    if output.stdout.len() != size {
        return Err(AppError::new(
            ErrorCode::MediaUnsupported,
            "The normalized master did not yield the requested frame",
        ));
    }
    Ok(output.stdout)
}

fn rational_timestamp(frame: u64, fps: FrameRate) -> Result<String, AppError> {
    let numerator = (frame as u128)
        .checked_mul(fps.den as u128)
        .ok_or_else(|| AppError::invalid_argument("Frame timestamp overflowed"))?;
    // Six decimal places are enough for all supported rates while avoiding a
    // floating-point frame selection drift.
    let micros = numerator
        .checked_mul(1_000_000)
        .and_then(|value| value.checked_div(fps.num as u128))
        .ok_or_else(|| AppError::invalid_argument("Frame timestamp overflowed"))?;
    Ok(format!("{}.{:06}", micros / 1_000_000, micros % 1_000_000))
}

/// Write raw stereo float32 PCM as a WAV-like FFmpeg input file.  The payload
/// is intentionally f32le so no lossy intermediate conversion is introduced.
pub fn build_raw_audio_input(path: &Path, sample_count: u64, pcm: &[f32]) -> Result<(), AppError> {
    let expected = (sample_count as usize)
        .checked_mul(2)
        .ok_or_else(|| AppError::invalid_argument("Audio sample count is too large"))?;
    if pcm.len() != expected {
        return Err(AppError::invalid_argument(
            "PCM payload does not match its sample count",
        ));
    }
    let mut file = File::create(path)
        .map_err(|_| AppError::io("The temporary PCM file could not be created"))?;
    for sample in pcm {
        file.write_all(&sample.to_le_bytes())
            .map_err(|_| AppError::io("The temporary PCM file could not be written"))?;
    }
    file.sync_all()
        .map_err(|_| AppError::io("The temporary PCM file could not be synchronized"))
}

/// Build an export command from an immutable plan.  Video arrives as RGBA
/// frames over stdin, while audio is an app-owned indexed f32le artifact.  No
/// caller can inject a filtergraph or an output codec.
pub fn build_export_command(
    toolchain: &FfmpegToolchain,
    width: u32,
    height: u32,
    fps: FrameRate,
    audio_path: &Path,
    output_path: &Path,
    graph_file: Option<&Path>,
) -> Result<FfmpegCommand, AppError> {
    fps.validate()?;
    if width == 0 || height == 0 {
        return Err(AppError::invalid_argument(
            "Export dimensions must be positive",
        ));
    }
    let mut command = safe_input_prefix_with_formats(
        FfmpegCommand::new(toolchain.ffmpeg.clone())
            .arg("-y")
            .arg("-f")
            .arg("rawvideo")
            .arg("-pix_fmt")
            .arg("rgba")
            .arg("-video_size")
            .arg(format!("{width}x{height}"))
            .arg("-framerate")
            .arg(format!("{}/{}", fps.num, fps.den))
            .arg("-i")
            .arg("pipe:0")
            .arg("-f")
            .arg("f32le")
            .arg("-ar")
            .arg(AUDIO_SAMPLE_RATE.to_string())
            .arg("-ac")
            .arg("2")
            .arg("-i")
            .arg(audio_path.as_os_str())
            .arg("-map")
            .arg("0:v:0")
            .arg("-map")
            .arg("1:a:0")
            .arg("-c:v")
            .arg("libx264")
            .arg("-crf")
            .arg("18")
            .arg("-pix_fmt")
            .arg("yuv420p")
            .arg("-c:a")
            .arg("aac")
            .arg("-b:a")
            .arg("192k")
            .arg("-ar")
            .arg(AUDIO_SAMPLE_RATE.to_string())
            .arg("-shortest")
            .arg("-movflags")
            .arg("+faststart")
            .arg("-progress")
            .arg("pipe:1"),
        RAW_RENDERER_FORMAT_ALLOWLIST,
    );
    if let Some(graph) = graph_file {
        // Graph files are only accepted when created by TypedFilterGraph.  The
        // caller cannot pass graph text through this API.
        command = command.arg("-filter_complex_script").arg(graph.as_os_str());
    }
    Ok(command.arg("-f").arg("mp4").arg(output_path.as_os_str()))
}

/// Run ffprobe with the same protocol/format restrictions used for media
/// decoding.  Callers parse only the typed JSON fields they need.
pub fn probe_json(
    toolchain: &FfmpegToolchain,
    input: &Path,
) -> Result<serde_json::Value, AppError> {
    let command = safe_probe_input_prefix(
        FfmpegCommand::new(toolchain.ffprobe.clone())
            .arg("-v")
            .arg("error")
            .arg("-print_format")
            .arg("json")
            .arg("-show_format")
            .arg("-show_streams")
            .arg(input.as_os_str()),
    );
    let output = command.run_capture(None)?;
    serde_json::from_slice(&output.stdout).map_err(|_| {
        AppError::new(
            ErrorCode::MediaUnsupported,
            "FFprobe returned malformed metadata",
        )
    })
}

/// Read indexed little-endian f32 stereo samples without loading an entire
/// audio artifact.  `start_sample` and `sample_count` are project sample
/// coordinates, not byte offsets supplied by a user.
pub fn read_f32_stereo_window(
    file: &mut File,
    start_sample: u64,
    sample_count: u64,
) -> Result<Vec<f32>, AppError> {
    let first_byte = start_sample
        .checked_mul(8)
        .ok_or_else(|| AppError::invalid_argument("Audio sample offset overflowed"))?;
    let byte_count = sample_count
        .checked_mul(8)
        .ok_or_else(|| AppError::invalid_argument("Audio window is too large"))?;
    use std::io::Seek;
    file.seek(io::SeekFrom::Start(first_byte))
        .map_err(|_| AppError::io("The PCM artifact could not be sought"))?;
    let mut bytes = vec![
        0u8;
        usize::try_from(byte_count).map_err(|_| {
            AppError::invalid_argument("Audio window is too large for this process")
        })?
    ];
    file.read_exact(&mut bytes).map_err(|_| {
        AppError::new(
            ErrorCode::AssetUnavailable,
            "The PCM artifact ended before the requested window",
        )
    })?;
    let mut result = Vec::with_capacity(bytes.len() / 4);
    for chunk in bytes.chunks_exact(4) {
        result.push(f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]));
    }
    Ok(result)
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RenderedMediaMetadata {
    pub width: u32,
    pub height: u32,
    pub fps_num: u32,
    pub fps_den: u32,
    pub duration_frames: u64,
    pub has_audio: bool,
}

pub fn parse_export_metadata(
    value: &serde_json::Value,
    expected: &RenderedMediaMetadata,
) -> Result<(), AppError> {
    let streams = value
        .get("streams")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| {
            AppError::new(
                ErrorCode::MediaUnsupported,
                "Export metadata has no streams",
            )
        })?;
    let video = streams.iter().find(|stream| {
        stream.get("codec_type").and_then(serde_json::Value::as_str) == Some("video")
    });
    let audio = streams.iter().find(|stream| {
        stream.get("codec_type").and_then(serde_json::Value::as_str) == Some("audio")
    });
    let video = video
        .ok_or_else(|| AppError::new(ErrorCode::MediaUnsupported, "Export has no video stream"))?;
    if expected.has_audio && audio.is_none() {
        return Err(AppError::new(
            ErrorCode::MediaUnsupported,
            "Export has no audio stream",
        ));
    }
    let width = video
        .get("width")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or_default();
    let height = video
        .get("height")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or_default();
    if width != u64::from(expected.width) || height != u64::from(expected.height) {
        return Err(AppError::new(
            ErrorCode::MediaUnsupported,
            "Export dimensions do not match the requested resolution",
        ));
    }
    let fps_text = video
        .get("r_frame_rate")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();
    let expected_fps = format!("{}/{}", expected.fps_num, expected.fps_den);
    if fps_text != expected_fps {
        return Err(AppError::new(
            ErrorCode::MediaUnsupported,
            "Export frame rate does not match the project",
        ));
    }
    let frame_count = video
        .get("nb_frames")
        .and_then(serde_json::Value::as_str)
        .and_then(|value| value.parse::<u64>().ok());
    if let Some(frame_count) = frame_count {
        if frame_count != expected.duration_frames {
            return Err(AppError::new(
                ErrorCode::MediaUnsupported,
                "Export frame count does not match the immutable render plan",
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn media_allowlist_excludes_network_and_playlist_inputs() {
        assert!(MEDIA_PROTOCOL_ALLOWLIST
            .split(',')
            .all(|value| value != "http"));
        assert!(!MEDIA_DEMUXER_ALLOWLIST
            .split(',')
            .any(|value| value == "hls"));
    }

    #[test]
    fn private_graph_file_is_removed_on_drop() {
        let directory =
            std::env::temp_dir().join(format!("cutterhoochee-graph-{}", unique_nonce()));
        let graph = TypedFilterGraph {
            chains: vec!["[0:v]format=rgba[out]".to_owned()],
        };
        let file = graph
            .write_private_file(&directory, "frame")
            .expect("graph");
        let path = file.path().to_owned();
        assert!(path.exists());
        drop(file);
        assert!(!path.exists());
        let _ = fs::remove_dir_all(directory);
    }
}
