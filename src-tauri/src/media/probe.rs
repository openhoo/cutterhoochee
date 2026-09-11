use crate::error::{AppError, ErrorCode};
use crate::project::model::{
    AssetKind, OriginalMediaMetadata, OriginalStreamKind, OriginalStreamMetadata,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};
use ts_rs::TS;

pub const PROTOCOL_WHITELIST: &str = "file,pipe";
pub const FORMAT_WHITELIST: &str =
    "mov,matroska,avi,mpegts,mpeg,flv,ogg,mp3,wav,flac,aac,image2,png_pipe,jpeg_pipe,webp_pipe";
const MAX_PROBE_OUTPUT_BYTES: usize = 16 * 1024 * 1024;

fn invalid(message: impl Into<String>) -> AppError {
    AppError::invalid_argument(message)
}

fn unavailable(message: impl Into<String>) -> AppError {
    AppError::new(ErrorCode::AssetUnavailable, message)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase")]
pub struct OriginalIdentity {
    pub canonical_path: String,
    #[ts(type = "SafeInteger")]
    pub byte_size: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional, type = "number")]
    pub modified_time_ms: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional, type = "SafeInteger")]
    pub device: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional, type = "SafeInteger")]
    pub inode: Option<u64>,
    pub content_hash: String,
}

impl OriginalIdentity {
    pub fn validate(&self) -> Result<(), AppError> {
        if self.canonical_path.is_empty()
            || self.canonical_path.contains('\0')
            || self.canonical_path.contains('\r')
            || self.canonical_path.contains('\n')
        {
            return Err(invalid("Original identity path is invalid"));
        }
        if self.content_hash.len() != 64
            || !self
                .content_hash
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit())
        {
            return Err(invalid("Original identity hash is invalid"));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase")]
pub struct ProbeStream {
    pub index: u32,
    pub kind: OriginalStreamKind,
    pub codec: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional, type = "SafeInteger")]
    pub duration_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional, type = "number")]
    pub start_time_ms: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub width: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub height: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sample_rate: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub channels: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rotation_degrees: Option<i16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sample_aspect_num: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sample_aspect_den: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub color_space: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub color_transfer: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub color_primaries: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub color_range: Option<String>,
}

impl ProbeStream {
    pub fn original_metadata(&self) -> OriginalStreamMetadata {
        OriginalStreamMetadata {
            kind: self.kind,
            codec: self.codec.clone(),
            duration_ms: self.duration_ms,
            start_time_ms: self.start_time_ms,
            width: self.width,
            height: self.height,
            sample_rate: self.sample_rate,
            channels: self.channels,
            rotation_degrees: self.rotation_degrees,
            sample_aspect_num: self.sample_aspect_num,
            sample_aspect_den: self.sample_aspect_den,
            color_space: self.color_space.clone(),
            color_transfer: self.color_transfer.clone(),
            color_primaries: self.color_primaries.clone(),
            color_range: self.color_range.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase")]
pub struct ProbeResult {
    pub path: String,
    pub format_name: String,
    pub asset_kind: AssetKind,
    pub streams: Vec<ProbeStream>,
    pub original: OriginalMediaMetadata,
    pub identity: OriginalIdentity,
    #[ts(type = "number")]
    pub epoch_ms: i64,
    #[ts(type = "number")]
    pub source_end_ms: i64,
    pub has_hdr: bool,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub video_stream_index: Option<u32>,
    pub audio_stream_index: Option<u32>,
}

impl ProbeResult {
    pub fn video(&self) -> Option<&ProbeStream> {
        self.streams
            .iter()
            .find(|stream| stream.kind == OriginalStreamKind::Video)
    }

    pub fn audio(&self) -> Option<&ProbeStream> {
        self.streams
            .iter()
            .find(|stream| stream.kind == OriginalStreamKind::Audio)
    }

    pub fn video_start_ms(&self) -> i64 {
        self.video()
            .and_then(|stream| stream.start_time_ms)
            .unwrap_or(self.epoch_ms)
    }

    pub fn audio_start_ms(&self) -> i64 {
        self.audio()
            .and_then(|stream| stream.start_time_ms)
            .unwrap_or(self.epoch_ms)
    }
}

/// Probe a granted, single-file input with an absolute packaged ffprobe path.
/// The demuxer and protocol allowlists are passed on every invocation; no input
/// may redirect ffprobe to a URL, playlist, concat file, or sibling path.
pub fn probe_media(ffprobe: &Path, input: &Path) -> Result<ProbeResult, AppError> {
    let identity = capture_identity(input)?;
    let output = run_ffprobe(ffprobe, input)?;
    let parsed: Value = serde_json::from_slice(&output)
        .map_err(|_| unavailable("ffprobe returned malformed metadata"))?;
    let format = parsed
        .get("format")
        .and_then(|value| value.as_object())
        .ok_or_else(|| unavailable("ffprobe did not return a format"))?;
    let format_name = format
        .get("format_name")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    ensure_allowed_format(&format_name)?;
    let streams_json = parsed
        .get("streams")
        .and_then(Value::as_array)
        .ok_or_else(|| unavailable("ffprobe did not return streams"))?;
    if streams_json.is_empty() {
        return Err(AppError::new(
            ErrorCode::MediaUnsupported,
            "The file contains no decodable media streams",
        ));
    }

    let mut streams = Vec::with_capacity(streams_json.len());
    for stream in streams_json {
        let object = stream
            .as_object()
            .ok_or_else(|| unavailable("ffprobe returned an invalid stream"))?;
        let codec_type = object
            .get("codec_type")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let kind = match codec_type {
            "video" => OriginalStreamKind::Video,
            "audio" => OriginalStreamKind::Audio,
            _ => OriginalStreamKind::Other,
        };
        let codec = object
            .get("codec_name")
            .and_then(Value::as_str)
            .or_else(|| object.get("codec_long_name").and_then(Value::as_str))
            .unwrap_or("unknown")
            .to_owned();
        let duration_ms = parse_duration_ms(object.get("duration"));
        let start_time_ms = parse_signed_ms(object.get("start_time"));
        let width = object
            .get("width")
            .and_then(Value::as_u64)
            .and_then(|value| u32::try_from(value).ok());
        let height = object
            .get("height")
            .and_then(Value::as_u64)
            .and_then(|value| u32::try_from(value).ok());
        let sample_rate = object
            .get("sample_rate")
            .and_then(Value::as_str)
            .and_then(|value| value.parse::<u32>().ok());
        let channels = object
            .get("channels")
            .and_then(Value::as_u64)
            .and_then(|value| u16::try_from(value).ok());
        let rotation_degrees = parse_rotation(object);
        let sample_aspect_num = object
            .get("sample_aspect_ratio")
            .and_then(Value::as_str)
            .and_then(|value| parse_ratio(value).map(|ratio| ratio.0));
        let sample_aspect_den = object
            .get("sample_aspect_ratio")
            .and_then(Value::as_str)
            .and_then(|value| parse_ratio(value).map(|ratio| ratio.1));
        let color_space = object
            .get("color_space")
            .and_then(Value::as_str)
            .map(str::to_owned);
        let color_transfer = object
            .get("color_transfer")
            .and_then(Value::as_str)
            .map(str::to_owned);
        let color_primaries = object
            .get("color_primaries")
            .and_then(Value::as_str)
            .map(str::to_owned);
        let color_range = object
            .get("color_range")
            .and_then(Value::as_str)
            .map(str::to_owned);
        streams.push(ProbeStream {
            index: object
                .get("index")
                .and_then(Value::as_u64)
                .and_then(|value| u32::try_from(value).ok())
                .unwrap_or(streams.len() as u32),
            kind,
            codec,
            duration_ms,
            start_time_ms,
            width,
            height,
            sample_rate,
            channels,
            rotation_degrees,
            sample_aspect_num,
            sample_aspect_den,
            color_space,
            color_transfer,
            color_primaries,
            color_range,
        });
    }

    let video = streams
        .iter()
        .find(|stream| stream.kind == OriginalStreamKind::Video);
    let audio = streams
        .iter()
        .find(|stream| stream.kind == OriginalStreamKind::Audio);
    if video.is_none() && audio.is_none() {
        return Err(AppError::new(
            ErrorCode::MediaUnsupported,
            "The file contains no supported video or audio stream",
        ));
    }
    let is_image = is_still_format(&format_name) && video.is_some() && audio.is_none();
    let asset_kind = if is_image {
        AssetKind::StillImage
    } else if video.is_some() {
        AssetKind::Video
    } else {
        AssetKind::Audio
    };
    let epoch_ms = streams
        .iter()
        .filter(|stream| {
            matches!(
                stream.kind,
                OriginalStreamKind::Video | OriginalStreamKind::Audio
            )
        })
        .filter_map(|stream| stream.start_time_ms)
        .min()
        .unwrap_or(0);
    let source_end_ms = streams
        .iter()
        .filter(|stream| {
            matches!(
                stream.kind,
                OriginalStreamKind::Video | OriginalStreamKind::Audio
            )
        })
        .filter_map(|stream| {
            stream
                .start_time_ms
                .zip(stream.duration_ms)
                .and_then(|(start, duration)| start.checked_add(duration as i64))
        })
        .max()
        .unwrap_or_else(|| epoch_ms.saturating_add(1));
    let original = OriginalMediaMetadata {
        file_name: input
            .file_name()
            .and_then(|value| value.to_str())
            .unwrap_or("media")
            .to_owned(),
        location: Some(identity.canonical_path.clone()),
        byte_size: Some(identity.byte_size),
        modified_time_ms: identity.modified_time_ms,
        streams: streams.iter().map(ProbeStream::original_metadata).collect(),
    };
    original.validate()?;
    let has_hdr = streams.iter().any(|stream| {
        stream.color_transfer.as_deref().is_some_and(|value| {
            let value = value.to_ascii_lowercase();
            value.contains("2084") || value.contains("hlg") || value.contains("arib")
        })
    });
    Ok(ProbeResult {
        path: identity.canonical_path.clone(),
        format_name,
        asset_kind,
        width: video.and_then(|stream| stream.width),
        height: video.and_then(|stream| stream.height),
        video_stream_index: video.map(|stream| stream.index),
        audio_stream_index: audio.map(|stream| stream.index),
        streams,
        original,
        identity,
        epoch_ms,
        source_end_ms,
        has_hdr,
    })
}

pub fn capture_identity(path: &Path) -> Result<OriginalIdentity, AppError> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|_| unavailable("The granted original file is unavailable"))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(AppError::new(
            ErrorCode::PermissionDenied,
            "Only the granted regular file may be imported",
        ));
    }
    let canonical = fs::canonicalize(path)
        .map_err(|_| unavailable("The granted original file could not be canonicalized"))?;
    let metadata = fs::symlink_metadata(&canonical)?;
    let modified_time_ms = metadata.modified().ok().and_then(system_time_ms);
    let content_hash = hash_file(&canonical)?;
    #[cfg(unix)]
    let (device, inode) = {
        use std::os::unix::fs::MetadataExt;
        (Some(metadata.dev()), Some(metadata.ino()))
    };
    #[cfg(not(unix))]
    let (device, inode) = (None, None);
    let identity = OriginalIdentity {
        canonical_path: canonical.to_string_lossy().into_owned(),
        byte_size: metadata.len(),
        modified_time_ms,
        device,
        inode,
        content_hash,
    };
    identity.validate()?;
    Ok(identity)
}

/// Revalidate the exact imported file before every regeneration. A replaced
/// path cannot inherit the old grant merely by retaining the same filename.
pub fn revalidate_identity(identity: &OriginalIdentity) -> Result<(), AppError> {
    if identity.device.is_none() || identity.inode.is_none() {
        return Err(AppError::new(
            ErrorCode::PermissionDenied,
            "The original file grant is unavailable; relink is required",
        ));
    }
    let current = capture_identity(Path::new(&identity.canonical_path))?;
    if current.canonical_path != identity.canonical_path
        || current.byte_size != identity.byte_size
        || current.modified_time_ms != identity.modified_time_ms
        || identity
            .device
            .zip(current.device)
            .is_some_and(|(expected, actual)| expected != actual)
        || identity
            .inode
            .zip(current.inode)
            .is_some_and(|(expected, actual)| expected != actual)
        || current.content_hash != identity.content_hash
    {
        return Err(AppError::new(
            ErrorCode::AssetUnavailable,
            "The imported original changed; relink is required",
        ));
    }
    Ok(())
}

pub fn hash_file(path: &Path) -> Result<String, AppError> {
    let mut file =
        File::open(path).map_err(|_| unavailable("The original file could not be read"))?;
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 1024 * 1024];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

pub fn resolve_packaged_binary(resource_dir: &Path, stem: &str) -> Result<PathBuf, AppError> {
    if stem.is_empty() || stem.contains('/') || stem.contains('\\') {
        return Err(invalid("Packaged binary name is invalid"));
    }
    let target = env!("CUTTERHOOCHEE_TARGET_TRIPLE");
    let name = format!("{stem}-{target}");
    let bundled = resource_dir.join("binaries").join(if cfg!(windows) {
        format!("{name}.exe")
    } else {
        name
    });
    if bundled.is_absolute() && bundled.is_file() {
        let metadata = fs::symlink_metadata(&bundled)?;
        if metadata.file_type().is_symlink() {
            return Err(AppError::new(
                ErrorCode::PermissionDenied,
                "A packaged media binary must not be a symlink",
            ));
        }
        return Ok(bundled);
    }
    #[cfg(debug_assertions)]
    {
        for candidate in [
            PathBuf::from(format!("/usr/bin/{stem}")),
            PathBuf::from(format!("/usr/local/bin/{stem}")),
        ] {
            if candidate.is_file() && !fs::symlink_metadata(&candidate)?.file_type().is_symlink() {
                return Ok(candidate);
            }
        }
    }
    Err(AppError::io(format!(
        "The packaged {stem} binary is unavailable"
    )))
}

fn run_ffprobe(ffprobe: &Path, input: &Path) -> Result<Vec<u8>, AppError> {
    if !ffprobe.is_absolute()
        || !ffprobe.is_file()
        || fs::symlink_metadata(ffprobe)?.file_type().is_symlink()
    {
        return Err(AppError::io("The packaged ffprobe binary is unavailable"));
    }
    let mut child = Command::new(ffprobe)
        .args([
            "-v",
            "error",
            "-protocol_whitelist",
            PROTOCOL_WHITELIST,
            "-format_whitelist",
            FORMAT_WHITELIST,
            "-print_format",
            "json",
            "-show_format",
            "-show_streams",
        ])
        .arg(input)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|_| unavailable("ffprobe could not be started"))?;
    let output = child
        .wait_with_output()
        .map_err(|_| unavailable("ffprobe failed"))?;
    if !output.status.success() {
        return Err(AppError::new(
            ErrorCode::MediaUnsupported,
            "ffprobe could not decode the selected file",
        ));
    }
    if output.stdout.len() > MAX_PROBE_OUTPUT_BYTES {
        return Err(AppError::new(
            ErrorCode::MediaUnsupported,
            "ffprobe metadata exceeds the supported limit",
        ));
    }
    Ok(output.stdout)
}

fn ensure_allowed_format(format_name: &str) -> Result<(), AppError> {
    let normalized = format_name.to_ascii_lowercase();
    if normalized.contains("hls")
        || normalized.contains("dash")
        || normalized.contains("concat")
        || normalized.contains("segment")
        || normalized.contains("playlist")
        || normalized.contains("http")
        || normalized.contains("rtsp")
    {
        return Err(AppError::new(
            ErrorCode::MediaUnsupported,
            "Playlists and network media are not supported for import",
        ));
    }
    let accepted = [
        "mov",
        "mp4",
        "m4a",
        "3gp",
        "3g2",
        "mj2",
        "matroska",
        "webm",
        "avi",
        "mpegts",
        "mpeg",
        "flv",
        "ogg",
        "ogv",
        "opus",
        "mp3",
        "wav",
        "flac",
        "aac",
        "image2",
        "png_pipe",
        "jpeg_pipe",
        "webp_pipe",
    ];
    if !normalized
        .split(',')
        .any(|item| accepted.contains(&item.trim()))
    {
        return Err(AppError::new(
            ErrorCode::MediaUnsupported,
            "The selected demuxer is not allowed for standalone import",
        ));
    }
    Ok(())
}

fn is_still_format(format_name: &str) -> bool {
    let value = format_name.to_ascii_lowercase();
    value.split(',').any(|item| {
        matches!(
            item.trim(),
            "image2" | "png_pipe" | "jpeg_pipe" | "webp_pipe"
        )
    })
}

fn parse_duration_ms(value: Option<&Value>) -> Option<u64> {
    value
        .and_then(Value::as_str)
        .and_then(parse_seconds)
        .and_then(|seconds| {
            if seconds >= 0.0 {
                Some((seconds * 1000.0).round() as u64)
            } else {
                None
            }
        })
}

fn parse_signed_ms(value: Option<&Value>) -> Option<i64> {
    value
        .and_then(Value::as_str)
        .and_then(parse_seconds)
        .and_then(|seconds| {
            let millis = seconds * 1000.0;
            (millis.is_finite() && millis >= i64::MIN as f64 && millis <= i64::MAX as f64)
                .then_some(millis.round() as i64)
        })
}

fn parse_seconds(value: &str) -> Option<f64> {
    let parsed = value.parse::<f64>().ok()?;
    parsed.is_finite().then_some(parsed)
}

fn parse_ratio(value: &str) -> Option<(u32, u32)> {
    let (num, den) = value.split_once(':')?;
    let num = num.parse().ok()?;
    let den = den.parse().ok()?;
    (num > 0 && den > 0).then_some((num, den))
}

fn parse_rotation(stream: &serde_json::Map<String, Value>) -> Option<i16> {
    if let Some(value) = stream
        .get("tags")
        .and_then(Value::as_object)
        .and_then(|tags| tags.get("rotate"))
        .and_then(Value::as_str)
        .and_then(|value| value.parse::<i16>().ok())
    {
        return Some(value);
    }
    stream
        .get("side_data_list")
        .and_then(Value::as_array)
        .and_then(|items| {
            items.iter().find_map(|item| {
                item.get("rotation")
                    .and_then(Value::as_i64)
                    .and_then(|value| i16::try_from(value).ok())
            })
        })
}

fn system_time_ms(value: SystemTime) -> Option<i64> {
    let duration = value.duration_since(UNIX_EPOCH).ok()?;
    i64::try_from(duration.as_millis()).ok()
}
