use crate::editor::history::{HistoryState, Receipt};
use crate::error::AppError;
use crate::ipc::{validate_safe_integer, MAX_SAFE_INTEGER};
use serde::{Deserialize, Serialize};
use std::borrow::Borrow;
use std::collections::{HashMap, HashSet};
use std::hash::Hash;
use ts_rs::TS;
use uuid::Uuid;

pub const PROJECT_SCHEMA_VERSION: u32 = 1;
pub const MAX_HISTORY_ENTRIES: usize = 100;
pub const MAX_RECEIPTS: usize = 1_000;
pub const AUDIO_SAMPLE_RATE: u32 = 48_000;

fn invalid(message: impl Into<String>) -> AppError {
    AppError::invalid_argument(message)
}

fn validate_text(value: &str, field: &str, required: bool) -> Result<(), AppError> {
    if required && value.trim().is_empty() {
        return Err(invalid(format!("{field} must not be empty")));
    }
    if value.as_bytes().contains(&0) {
        return Err(invalid(format!("{field} contains a NUL byte")));
    }
    if value.len() > 1024 * 1024 {
        return Err(invalid(format!("{field} exceeds the supported size")));
    }
    Ok(())
}

fn validate_id(value: &str, field: &str) -> Result<(), AppError> {
    if value.len() > 256 || value.contains('\r') || value.contains('\n') {
        return Err(invalid(format!("{field} is invalid")));
    }
    let id = Uuid::parse_str(value).map_err(|_| invalid(format!("{field} must be a UUID")))?;
    if id.is_nil() {
        return Err(invalid(format!("{field} must not be the nil UUID")));
    }
    Ok(())
}

fn validate_artifact_id(value: &str, field: &str) -> Result<(), AppError> {
    if value.trim().is_empty()
        || value.len() > 512
        || value.as_bytes().contains(&0)
        || value.contains('\r')
        || value.contains('\n')
    {
        return Err(invalid(format!("{field} is invalid")));
    }
    Ok(())
}

fn validate_hash(value: &str, field: &str) -> Result<(), AppError> {
    if value.trim().is_empty() || value.len() > 256 || value.contains('\r') || value.contains('\n')
    {
        return Err(invalid(format!("{field} is invalid")));
    }
    Ok(())
}

fn validate_safe_i64(value: i64, field: &str) -> Result<(), AppError> {
    if value.unsigned_abs() > MAX_SAFE_INTEGER {
        return Err(invalid(format!("{field} exceeds the safe integer range")));
    }
    Ok(())
}

fn checked_add(start: u64, duration: u64, field: &str) -> Result<u64, AppError> {
    let end = start
        .checked_add(duration)
        .ok_or_else(|| invalid(format!("{field} overflows the safe integer range")))?;
    validate_safe_integer(end, field)?;
    Ok(end)
}

fn validate_positive_interval(start: u64, duration: u64, field: &str) -> Result<u64, AppError> {
    validate_safe_integer(start, &format!("{field}.start"))?;
    validate_safe_integer(duration, &format!("{field}.duration"))?;
    if duration == 0 {
        return Err(invalid(format!("{field} must have a positive duration")));
    }
    checked_add(start, duration, &format!("{field}.end"))
}

fn validate_uuid_set<T>(items: &[T], id: impl Fn(&T) -> &str, field: &str) -> Result<(), AppError> {
    let mut ids = HashSet::with_capacity(items.len());
    for item in items {
        let value = id(item);
        validate_id(value, field)?;
        if !ids.insert(value) {
            return Err(invalid(format!("Duplicate {field}: {value}")));
        }
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "lowercase")]
#[ts(rename_all = "lowercase")]
pub enum TrackKind {
    Video,
    Audio,
    Text,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
pub enum AssetKind {
    #[serde(rename = "video")]
    #[ts(rename = "video")]
    Video,
    #[serde(rename = "audio")]
    #[ts(rename = "audio")]
    Audio,
    #[serde(rename = "stillImage")]
    #[ts(rename = "stillImage")]
    StillImage,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
pub enum FitMode {
    #[serde(rename = "contain")]
    #[ts(rename = "contain")]
    Contain,
    #[serde(rename = "cover")]
    #[ts(rename = "cover")]
    Cover,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
pub enum TextKind {
    #[serde(rename = "title")]
    #[ts(rename = "title")]
    Title,
    #[serde(rename = "caption")]
    #[ts(rename = "caption")]
    Caption,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
pub enum TextStyle {
    #[serde(rename = "clean")]
    #[ts(rename = "clean")]
    Clean,
    #[serde(rename = "boxed")]
    #[ts(rename = "boxed")]
    Boxed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
pub enum OriginalStreamKind {
    #[serde(rename = "video")]
    #[ts(rename = "video")]
    Video,
    #[serde(rename = "audio")]
    #[ts(rename = "audio")]
    Audio,
    #[serde(rename = "other")]
    #[ts(rename = "other")]
    Other,
}

impl Default for OriginalStreamKind {
    fn default() -> Self {
        Self::Other
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
pub enum AspectRatio {
    #[serde(rename = "16:9")]
    #[ts(rename = "16:9")]
    Landscape,
    #[serde(rename = "9:16")]
    #[ts(rename = "9:16")]
    Portrait,
    #[serde(rename = "1:1")]
    #[ts(rename = "1:1")]
    Square,
}

impl Default for AspectRatio {
    fn default() -> Self {
        Self::Landscape
    }
}

impl AspectRatio {
    pub const SIXTEEN_BY_NINE: Self = Self::Landscape;
    pub const NINE_BY_SIXTEEN: Self = Self::Portrait;
    pub const ONE_BY_ONE: Self = Self::Square;

    pub fn dimensions(self) -> (u32, u32) {
        match self {
            Self::Landscape => (1_920, 1_080),
            Self::Portrait => (1_080, 1_920),
            Self::Square => (1_080, 1_080),
        }
    }

    pub fn from_dimensions(width: u32, height: u32) -> Option<Self> {
        if width == 0 || height == 0 {
            return None;
        }
        if width as u64 * 9 == height as u64 * 16 {
            Some(Self::Landscape)
        } else if width as u64 * 16 == height as u64 * 9 {
            Some(Self::Portrait)
        } else if width == height {
            Some(Self::Square)
        } else {
            None
        }
    }
}
impl TryFrom<&str> for AspectRatio {
    type Error = AppError;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        match value {
            "16:9" => Ok(Self::Landscape),
            "9:16" => Ok(Self::Portrait),
            "1:1" => Ok(Self::Square),
            _ => Err(invalid("Aspect ratio must be 16:9, 9:16, or 1:1")),
        }
    }
}

impl TryFrom<String> for AspectRatio {
    type Error = AppError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::try_from(value.as_str())
    }
}

impl From<AspectRatio> for &'static str {
    fn from(value: AspectRatio) -> Self {
        match value {
            AspectRatio::Landscape => "16:9",
            AspectRatio::Portrait => "9:16",
            AspectRatio::Square => "1:1",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase")]
pub struct FrameRate {
    #[ts(type = "SafeInteger")]
    pub num: u32,
    #[ts(type = "SafeInteger")]
    pub den: u32,
}

impl Default for FrameRate {
    fn default() -> Self {
        Self { num: 30, den: 1 }
    }
}

impl FrameRate {
    pub const FPS_24: Self = Self { num: 24, den: 1 };
    pub const FPS_25: Self = Self { num: 25, den: 1 };
    pub const FPS_30: Self = Self { num: 30, den: 1 };
    pub const FPS_60: Self = Self { num: 60, den: 1 };
    #[allow(non_upper_case_globals)]
    pub const Fps24: Self = Self::FPS_24;
    #[allow(non_upper_case_globals)]
    pub const Fps25: Self = Self::FPS_25;
    #[allow(non_upper_case_globals)]
    pub const Fps30: Self = Self::FPS_30;
    #[allow(non_upper_case_globals)]
    pub const Fps60: Self = Self::FPS_60;

    pub fn new(num: u32, den: u32) -> Result<Self, AppError> {
        let value = Self { num, den };
        value.validate()?;
        Ok(value)
    }

    pub fn validate(self) -> Result<(), AppError> {
        if self.den == 0 || self.num == 0 {
            return Err(invalid(
                "Frame rate numerator and denominator must be positive",
            ));
        }
        if !matches!((self.num, self.den), (24, 1) | (25, 1) | (30, 1) | (60, 1)) {
            return Err(invalid(
                "Only 24/1, 25/1, 30/1, and 60/1 project frame rates are supported",
            ));
        }
        if (AUDIO_SAMPLE_RATE as u64 * self.den as u64) % self.num as u64 != 0 {
            return Err(invalid("The project frame rate must divide 48 kHz exactly"));
        }
        Ok(())
    }

    pub fn samples_per_frame(self) -> u64 {
        AUDIO_SAMPLE_RATE as u64 * self.den as u64 / self.num as u64
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase")]
pub struct RgbaColor {
    pub red: u8,
    pub green: u8,
    pub blue: u8,
    pub alpha: u8,
}

impl Default for RgbaColor {
    fn default() -> Self {
        Self {
            red: 0,
            green: 0,
            blue: 0,
            alpha: 255,
        }
    }
}

impl RgbaColor {
    pub fn opaque_black() -> Self {
        Self::default()
    }

    fn validate_background(self) -> Result<(), AppError> {
        if self.alpha != 255 {
            return Err(invalid("The project background must be opaque"));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase")]
pub struct ProjectProfile {
    pub width: u32,
    pub height: u32,
    pub fps_num: u32,
    pub fps_den: u32,
    pub background: RgbaColor,
}

impl Default for ProjectProfile {
    fn default() -> Self {
        Self::for_aspect(AspectRatio::Landscape, FrameRate::FPS_30)
            .expect("default profile is supported")
    }
}

impl ProjectProfile {
    pub fn for_aspect(aspect: AspectRatio, fps: FrameRate) -> Result<Self, AppError> {
        let (width, height) = aspect.dimensions();
        let profile = Self {
            width,
            height,
            fps_num: fps.num,
            fps_den: fps.den,
            background: RgbaColor::opaque_black(),
        };
        profile.validate()?;
        Ok(profile)
    }

    pub fn fps(&self) -> FrameRate {
        FrameRate {
            num: self.fps_num,
            den: self.fps_den,
        }
    }

    pub fn aspect(&self) -> Option<AspectRatio> {
        AspectRatio::from_dimensions(self.width, self.height)
    }

    pub fn validate(&self) -> Result<(), AppError> {
        if self.width == 0 || self.height == 0 || self.width > 16_384 || self.height > 16_384 {
            return Err(invalid(
                "Project dimensions are outside the supported range",
            ));
        }
        if self.aspect().is_none() {
            return Err(invalid("Project dimensions must use 16:9, 9:16, or 1:1"));
        }
        self.fps().validate()?;
        self.background.validate_background()
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase")]
pub struct OriginalStreamMetadata {
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

impl OriginalStreamMetadata {
    pub fn validate(&self, index: usize) -> Result<(), AppError> {
        validate_text(&self.codec, &format!("streams[{index}].codec"), true)?;
        if let Some(value) = self.duration_ms {
            validate_safe_integer(value, &format!("streams[{index}].durationMs"))?;
        }
        if let Some(value) = self.start_time_ms {
            validate_safe_i64(value, &format!("streams[{index}].startTimeMs"))?;
        }
        if let Some(value) = self.sample_rate {
            if value == 0 {
                return Err(invalid(format!(
                    "streams[{index}].sampleRate must be positive"
                )));
            }
        }
        if let Some(value) = self.channels {
            if value == 0 {
                return Err(invalid(format!(
                    "streams[{index}].channels must be positive"
                )));
            }
        }
        if let Some(value) = self.sample_aspect_num {
            if value == 0 {
                return Err(invalid(format!(
                    "streams[{index}].sampleAspectNum must be positive"
                )));
            }
        }
        if let Some(value) = self.sample_aspect_den {
            if value == 0 {
                return Err(invalid(format!(
                    "streams[{index}].sampleAspectDen must be positive"
                )));
            }
        }
        if self
            .width
            .zip(self.height)
            .is_some_and(|(w, h)| w == 0 || h == 0)
        {
            return Err(invalid(format!(
                "streams[{index}] dimensions must be positive"
            )));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase")]
pub struct OriginalMediaMetadata {
    pub file_name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub location: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional, type = "SafeInteger")]
    pub byte_size: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional, type = "number")]
    pub modified_time_ms: Option<i64>,
    pub streams: Vec<OriginalStreamMetadata>,
}

impl OriginalMediaMetadata {
    pub fn validate(&self) -> Result<(), AppError> {
        validate_text(&self.file_name, "original.fileName", true)?;
        if let Some(value) = self.byte_size {
            validate_safe_integer(value, "original.byteSize")?;
        }
        if let Some(value) = self.modified_time_ms {
            validate_safe_i64(value, "original.modifiedTimeMs")?;
        }
        if self.streams.is_empty() {
            return Err(invalid("An asset must retain at least one original stream"));
        }
        for (index, stream) in self.streams.iter().enumerate() {
            stream.validate(index)?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase")]
pub struct NormalizedVideo {
    pub master_artifact_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub proxy_artifact_id: Option<String>,
    #[ts(type = "SafeInteger")]
    pub frame_count: u64,
    pub width: u32,
    pub height: u32,
    pub fps_num: u32,
    pub fps_den: u32,
    #[ts(type = "SafeInteger")]
    pub active_start_frame: u64,
    #[ts(type = "SafeInteger")]
    pub active_end_frame: u64,
    #[ts(type = "number")]
    pub source_start_ms: i64,
    #[ts(type = "number")]
    pub source_end_ms: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional, type = "SafeInteger")]
    pub proxy_frame_count: Option<u64>,
}

impl NormalizedVideo {
    pub fn validate(&self, field: &str) -> Result<(), AppError> {
        validate_artifact_id(
            &self.master_artifact_id,
            &format!("{field}.masterArtifactId"),
        )?;
        if let Some(proxy) = self.proxy_artifact_id.as_deref() {
            validate_artifact_id(proxy, &format!("{field}.proxyArtifactId"))?;
        }
        validate_safe_integer(self.frame_count, &format!("{field}.frameCount"))?;
        validate_safe_integer(
            self.active_start_frame,
            &format!("{field}.activeStartFrame"),
        )?;
        validate_safe_integer(self.active_end_frame, &format!("{field}.activeEndFrame"))?;
        if self.frame_count == 0 || self.active_start_frame >= self.active_end_frame {
            return Err(invalid(format!(
                "{field} must have a positive active frame interval"
            )));
        }
        if self.active_end_frame > self.frame_count {
            return Err(invalid(format!(
                "{field}.activeEndFrame exceeds frameCount"
            )));
        }
        if let Some(proxy_frames) = self.proxy_frame_count {
            validate_safe_integer(proxy_frames, &format!("{field}.proxyFrameCount"))?;
            if proxy_frames != self.frame_count {
                return Err(invalid(format!(
                    "{field}.proxyFrameCount must equal frameCount"
                )));
            }
        }
        if self.width == 0 || self.height == 0 {
            return Err(invalid(format!("{field} dimensions must be positive")));
        }
        FrameRate::new(self.fps_num, self.fps_den)?;
        validate_safe_i64(self.source_start_ms, &format!("{field}.sourceStartMs"))?;
        validate_safe_i64(self.source_end_ms, &format!("{field}.sourceEndMs"))?;
        if self.source_start_ms >= self.source_end_ms {
            return Err(invalid(format!("{field} source interval must be positive")));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase")]
pub struct NormalizedAudio {
    pub pcm_artifact_id: String,
    #[ts(type = "SafeInteger")]
    pub sample_count: u64,
    pub sample_rate: u32,
    pub channels: u8,
    #[ts(type = "SafeInteger")]
    pub duration_frames: u64,
    #[ts(type = "SafeInteger")]
    pub active_start_sample: u64,
    #[ts(type = "SafeInteger")]
    pub active_end_sample: u64,
    #[ts(type = "number")]
    pub source_start_ms: i64,
    #[ts(type = "number")]
    pub source_end_ms: i64,
}

impl NormalizedAudio {
    pub fn validate(&self, field: &str) -> Result<(), AppError> {
        validate_artifact_id(&self.pcm_artifact_id, &format!("{field}.pcmArtifactId"))?;
        validate_safe_integer(self.sample_count, &format!("{field}.sampleCount"))?;
        validate_safe_integer(self.duration_frames, &format!("{field}.durationFrames"))?;
        validate_safe_integer(
            self.active_start_sample,
            &format!("{field}.activeStartSample"),
        )?;
        validate_safe_integer(self.active_end_sample, &format!("{field}.activeEndSample"))?;
        if self.sample_count == 0 || self.active_start_sample >= self.active_end_sample {
            return Err(invalid(format!(
                "{field} must have a positive active sample interval"
            )));
        }
        if self.active_end_sample > self.sample_count {
            return Err(invalid(format!(
                "{field}.activeEndSample exceeds sampleCount"
            )));
        }
        if self.sample_rate != AUDIO_SAMPLE_RATE || self.channels != 2 {
            return Err(invalid(format!("{field} must be 48 kHz stereo PCM")));
        }
        if self.duration_frames == 0 {
            return Err(invalid(format!("{field}.durationFrames must be positive")));
        }
        validate_safe_i64(self.source_start_ms, &format!("{field}.sourceStartMs"))?;
        validate_safe_i64(self.source_end_ms, &format!("{field}.sourceEndMs"))?;
        if self.source_start_ms >= self.source_end_ms {
            return Err(invalid(format!("{field} source interval must be positive")));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase")]
pub struct NormalizedAsset {
    pub renderer_version: String,
    #[ts(type = "number")]
    pub epoch_ms: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub video: Option<NormalizedVideo>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub audio: Option<NormalizedAudio>,
}

impl NormalizedAsset {
    pub fn validate(&self, kind: AssetKind) -> Result<(), AppError> {
        validate_text(
            &self.renderer_version,
            "normalization.rendererVersion",
            true,
        )?;
        validate_safe_i64(self.epoch_ms, "normalization.epochMs")?;
        match kind {
            AssetKind::Video => {
                let video = self
                    .video
                    .as_ref()
                    .ok_or_else(|| invalid("A video asset needs a normalized video master"))?;
                video.validate("normalization.video")?;
            }
            AssetKind::Audio => {
                let audio = self
                    .audio
                    .as_ref()
                    .ok_or_else(|| invalid("An audio asset needs normalized PCM"))?;
                audio.validate("normalization.audio")?;
            }
            AssetKind::StillImage => {
                let video = self
                    .video
                    .as_ref()
                    .ok_or_else(|| invalid("A still image needs a normalized raster master"))?;
                video.validate("normalization.video")?;
                if self.audio.is_some() {
                    return Err(invalid("A still image cannot contain normalized audio"));
                }
            }
        }
        if let Some(audio) = self.audio.as_ref() {
            audio.validate("normalization.audio")?;
        }
        Ok(())
    }

    pub fn frame_count(&self, kind: AssetKind) -> Option<u64> {
        match kind {
            AssetKind::Video | AssetKind::StillImage => {
                self.video.as_ref().map(|value| value.frame_count)
            }
            AssetKind::Audio => self.audio.as_ref().map(|value| value.duration_frames),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase")]
pub struct AssetManifest {
    pub id: String,
    pub kind: AssetKind,
    pub content_hash: String,
    pub original: OriginalMediaMetadata,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub normalization: Option<NormalizedAsset>,
}

impl AssetManifest {
    pub fn is_ready(&self) -> bool {
        self.normalization.is_some()
    }

    pub fn frame_count(&self) -> Option<u64> {
        self.normalization.as_ref()?.frame_count(self.kind)
    }

    pub fn has_audio(&self) -> bool {
        self.normalization
            .as_ref()
            .and_then(|normalization| normalization.audio.as_ref())
            .is_some()
    }

    pub fn validate(&self) -> Result<(), AppError> {
        validate_id(&self.id, "asset.id")?;
        validate_hash(&self.content_hash, "asset.contentHash")?;
        self.original.validate()?;
        if let Some(normalization) = self.normalization.as_ref() {
            normalization.validate(self.kind)?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase")]
pub struct Track {
    pub id: String,
    pub kind: TrackKind,
    pub name: String,
    pub muted: bool,
    pub locked: bool,
}

impl Track {
    pub fn new(id: String, kind: TrackKind, name: String) -> Result<Self, AppError> {
        let track = Self {
            id,
            kind,
            name,
            muted: false,
            locked: false,
        };
        track.validate()?;
        Ok(track)
    }

    pub fn validate(&self) -> Result<(), AppError> {
        validate_id(&self.id, "track.id")?;
        validate_text(&self.name, "track.name", true)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase")]
pub struct MediaClip {
    pub id: String,
    pub track_id: String,
    pub asset_id: String,
    #[ts(type = "SafeInteger")]
    pub start_frame: u64,
    #[ts(type = "SafeInteger")]
    pub in_frame: u64,
    #[ts(type = "SafeInteger")]
    pub duration_frames: u64,
    pub fit: FitMode,
    pub center_x: u16,
    pub center_y: u16,
    pub scale: u32,
    pub opacity: u16,
    #[ts(type = "number")]
    pub gain_db: f64,
    pub audio_enabled: bool,
    #[ts(type = "SafeInteger")]
    pub fade_in_frames: u64,
    #[ts(type = "SafeInteger")]
    pub fade_out_frames: u64,
}

impl MediaClip {
    pub fn end_frame(&self) -> Result<u64, AppError> {
        checked_add(self.start_frame, self.duration_frames, "clip.endFrame")
    }

    pub fn source_end_frame(&self) -> Result<u64, AppError> {
        checked_add(self.in_frame, self.duration_frames, "clip.sourceEndFrame")
    }

    pub fn interval(&self) -> Result<FrameInterval, AppError> {
        Ok(FrameInterval::from_start_duration(
            self.start_frame,
            self.duration_frames,
        )?)
    }

    pub fn validate(&self, track: &Track, asset: &AssetManifest) -> Result<(), AppError> {
        validate_id(&self.id, "clip.id")?;
        validate_id(&self.track_id, "clip.trackId")?;
        validate_id(&self.asset_id, "clip.assetId")?;
        validate_positive_interval(self.start_frame, self.duration_frames, "clip")?;
        validate_safe_integer(self.in_frame, "clip.inFrame")?;
        validate_safe_integer(self.fade_in_frames, "clip.fadeInFrames")?;
        validate_safe_integer(self.fade_out_frames, "clip.fadeOutFrames")?;
        if self.fade_in_frames > self.duration_frames || self.fade_out_frames > self.duration_frames
        {
            return Err(invalid("Clip fades cannot exceed clip duration"));
        }
        if self.center_x > 10_000 || self.center_y > 10_000 {
            return Err(invalid(
                "Clip center coordinates must be in basis points 0..10000",
            ));
        }
        if !(100..=40_000).contains(&self.scale) {
            return Err(invalid("Clip scale must be in basis points 100..40000"));
        }
        if self.opacity > 10_000 {
            return Err(invalid("Clip opacity must be in basis points 0..10000"));
        }
        if !self.gain_db.is_finite() || !(-60.0..=12.0).contains(&self.gain_db) {
            return Err(invalid(
                "Clip gain must be finite and in the range -60..12 dB",
            ));
        }
        let expected_kind = match asset.kind {
            AssetKind::Video | AssetKind::StillImage => TrackKind::Video,
            AssetKind::Audio => TrackKind::Audio,
        };
        if track.kind != expected_kind {
            return Err(invalid(
                "The clip asset is incompatible with its track kind",
            ));
        }
        let normalization = asset
            .normalization
            .as_ref()
            .ok_or_else(|| invalid("A clip can only reference a ready normalized asset"))?;
        let frame_count = normalization
            .frame_count(asset.kind)
            .ok_or_else(|| invalid("The clip asset has no normalized frame bounds"))?;
        if asset.kind == AssetKind::StillImage {
            if self.in_frame != 0 {
                return Err(invalid("Still-image clips must start at source frame zero"));
            }
        } else if self.source_end_frame()? > frame_count {
            return Err(invalid("Clip source interval exceeds the normalized asset"));
        }
        if asset.kind == AssetKind::StillImage && self.audio_enabled {
            return Err(invalid("Still-image clips cannot enable audio"));
        }
        if self.audio_enabled && asset.kind == AssetKind::Video && !asset.has_audio() {
            return Err(invalid(
                "A video clip cannot enable audio when the asset has no audio stream",
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase")]
pub struct FrameInterval {
    #[ts(type = "SafeInteger")]
    pub start_frame: u64,
    #[ts(type = "SafeInteger")]
    pub duration_frames: u64,
}

impl FrameInterval {
    pub fn from_start_duration(start_frame: u64, duration_frames: u64) -> Result<Self, AppError> {
        validate_positive_interval(start_frame, duration_frames, "frameInterval")?;
        Ok(Self {
            start_frame,
            duration_frames,
        })
    }

    pub fn from_bounds(start_frame: u64, end_frame: u64) -> Result<Self, AppError> {
        if end_frame <= start_frame {
            return Err(invalid("Frame interval end must be greater than start"));
        }
        Self::from_start_duration(start_frame, end_frame - start_frame)
    }

    pub fn end_frame(self) -> u64 {
        self.start_frame + self.duration_frames
    }

    pub fn contains(self, frame: u64) -> bool {
        frame >= self.start_frame && frame < self.end_frame()
    }

    pub fn intersection(self, other: Self) -> Option<Self> {
        let start = self.start_frame.max(other.start_frame);
        let end = self.end_frame().min(other.end_frame());
        (start < end).then(|| Self {
            start_frame: start,
            duration_frames: end - start,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase")]
pub struct TextItem {
    pub id: String,
    pub track_id: String,
    pub kind: TextKind,
    pub text: String,
    pub style: TextStyle,
    pub color: RgbaColor,
    pub font_size: u32,
    pub position_x: u16,
    pub position_y: u16,
    pub line_breaks: Vec<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional, type = "SafeInteger")]
    pub start_frame: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional, type = "SafeInteger")]
    pub duration_frames: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub owner_clip_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional, type = "SafeInteger")]
    pub source_start_frame: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional, type = "SafeInteger")]
    pub source_duration_frames: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase")]
pub struct CaptionProjection {
    pub caption_id: String,
    pub clip_id: String,
    #[ts(type = "SafeInteger")]
    pub timeline_start_frame: u64,
    #[ts(type = "SafeInteger")]
    pub duration_frames: u64,
    #[ts(type = "SafeInteger")]
    pub source_start_frame: u64,
}

impl TextItem {
    pub fn timeline_interval(&self) -> Option<Result<FrameInterval, AppError>> {
        self.start_frame
            .zip(self.duration_frames)
            .map(|(start, duration)| FrameInterval::from_start_duration(start, duration))
    }

    pub fn source_interval(&self) -> Option<Result<FrameInterval, AppError>> {
        self.source_start_frame
            .zip(self.source_duration_frames)
            .map(|(start, duration)| FrameInterval::from_start_duration(start, duration))
    }

    pub fn project_on_clip(&self, clip: &MediaClip) -> Result<Option<CaptionProjection>, AppError> {
        let Some(source) = self.source_interval() else {
            return Ok(None);
        };
        let source = source?;
        let clip_source = FrameInterval::from_start_duration(clip.in_frame, clip.duration_frames)?;
        let Some(overlap) = source.intersection(clip_source) else {
            return Ok(None);
        };
        let offset = overlap
            .start_frame
            .checked_sub(clip.in_frame)
            .ok_or_else(|| invalid("Caption source interval is before clip source"))?;
        let timeline_start = clip
            .start_frame
            .checked_add(offset)
            .ok_or_else(|| invalid("Projected caption timeline position overflows"))?;
        validate_safe_integer(timeline_start, "captionProjection.timelineStartFrame")?;
        Ok(Some(CaptionProjection {
            caption_id: self.id.clone(),
            clip_id: clip.id.clone(),
            timeline_start_frame: timeline_start,
            duration_frames: overlap.duration_frames,
            source_start_frame: overlap.start_frame,
        }))
    }

    pub fn validate<TrackId, ClipId>(
        &self,
        tracks: &HashMap<TrackId, &Track>,
        clips: &HashMap<ClipId, &MediaClip>,
    ) -> Result<(), AppError>
    where
        TrackId: Borrow<str> + Eq + Hash,
        ClipId: Borrow<str> + Eq + Hash,
    {
        validate_id(&self.id, "text.id")?;
        validate_id(&self.track_id, "text.trackId")?;
        validate_text(&self.text, "text.text", true)?;
        let track = tracks
            .get(self.track_id.as_str())
            .ok_or_else(|| invalid("Text item references an unknown track"))?;
        if track.kind != TrackKind::Text {
            return Err(invalid("Text items must belong to a text track"));
        }
        if self.font_size == 0 || self.font_size > 1_024 {
            return Err(invalid("Text font size is outside the supported range"));
        }
        if self.position_x > 10_000 || self.position_y > 10_000 {
            return Err(invalid("Text position must be in basis points 0..10000"));
        }
        let mut previous = None;
        for line_break in &self.line_breaks {
            if previous.is_some_and(|old| *line_break <= old) {
                return Err(invalid("Text line breaks must be strictly increasing"));
            }
            if *line_break as usize > self.text.chars().count() {
                return Err(invalid("Text line break is outside the text"));
            }
            previous = Some(*line_break);
        }

        let has_owner = self.owner_clip_id.is_some();
        let has_source = self.source_start_frame.is_some() || self.source_duration_frames.is_some();
        let has_timeline = self.start_frame.is_some() || self.duration_frames.is_some();
        if has_owner || has_source {
            if self.kind != TextKind::Caption || !has_owner || !has_source || has_timeline {
                return Err(invalid(
                    "Clip-owned text must be a caption with a source interval only",
                ));
            }
            let clip_id = self.owner_clip_id.as_deref().unwrap();
            let clip = clips
                .get(clip_id)
                .ok_or_else(|| invalid("Caption references an unknown owner clip"))?;
            let source = self
                .source_interval()
                .ok_or_else(|| invalid("Owned caption source interval is incomplete"))??;
            let clip_source =
                FrameInterval::from_start_duration(clip.in_frame, clip.duration_frames)?;
            if source.intersection(clip_source) != Some(source) {
                return Err(invalid(
                    "Owned caption source interval must be contained by its clip",
                ));
            }
        } else {
            if self.start_frame.is_none() || self.duration_frames.is_none() {
                return Err(invalid("Standalone text needs a timeline interval"));
            }
            self.timeline_interval().unwrap()?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase")]
pub struct Transition {
    pub id: String,
    pub left_clip_id: String,
    pub right_clip_id: String,
    #[ts(type = "SafeInteger")]
    pub duration_frames: u64,
}

impl Transition {
    pub fn validate_ids(&self) -> Result<(), AppError> {
        validate_id(&self.id, "transition.id")?;
        validate_id(&self.left_clip_id, "transition.leftClipId")?;
        validate_id(&self.right_clip_id, "transition.rightClipId")?;
        validate_safe_integer(self.duration_frames, "transition.durationFrames")?;
        if self.duration_frames < 2 {
            return Err(invalid("Dissolve duration must be at least two frames"));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase")]
pub struct ProjectDocument {
    pub project_id: String,
    pub name: String,
    #[ts(type = "SafeInteger")]
    pub revision: u64,
    pub profile: ProjectProfile,
    pub assets: Vec<AssetManifest>,
    pub tracks: Vec<Track>,
    pub clips: Vec<MediaClip>,
    pub text_items: Vec<TextItem>,
    pub transitions: Vec<Transition>,
}

impl ProjectDocument {
    pub fn new(
        name: impl Into<String>,
        aspect: AspectRatio,
        fps: FrameRate,
    ) -> Result<Self, AppError> {
        let profile = ProjectProfile::for_aspect(aspect, fps)?;
        let document = Self {
            project_id: Uuid::new_v4().to_string(),
            name: name.into(),
            revision: 0,
            profile,
            assets: Vec::new(),
            tracks: vec![
                Track::new(
                    Uuid::new_v4().to_string(),
                    TrackKind::Video,
                    "Main Video".to_owned(),
                )?,
                Track::new(
                    Uuid::new_v4().to_string(),
                    TrackKind::Audio,
                    "Main Audio".to_owned(),
                )?,
                Track::new(
                    Uuid::new_v4().to_string(),
                    TrackKind::Text,
                    "Text".to_owned(),
                )?,
            ],
            clips: Vec::new(),
            text_items: Vec::new(),
            transitions: Vec::new(),
        };
        document.validate()?;
        Ok(document)
    }
    /// Numeric constructor used by the native create-project command. It
    /// deliberately funnels through the same supported-rate validation as the
    /// typed constructor.
    pub fn new_with_rate(
        name: impl Into<String>,
        aspect: impl TryInto<AspectRatio>,
        fps_num: u64,
        fps_den: u64,
    ) -> Result<Self, AppError> {
        let aspect = aspect
            .try_into()
            .map_err(|_| invalid("Aspect ratio must be 16:9, 9:16, or 1:1"))?;
        let num = u32::try_from(fps_num).map_err(|_| invalid("Frame rate numerator is invalid"))?;
        let den =
            u32::try_from(fps_den).map_err(|_| invalid("Frame rate denominator is invalid"))?;
        Self::new(name, aspect, FrameRate::new(num, den)?)
    }

    pub fn validate(&self) -> Result<(), AppError> {
        validate_id(&self.project_id, "projectId")?;
        validate_text(&self.name, "name", true)?;
        validate_safe_integer(self.revision, "revision")?;
        self.profile.validate()?;
        validate_uuid_set(&self.assets, |asset| asset.id.as_str(), "asset.id")?;
        validate_uuid_set(&self.tracks, |track| track.id.as_str(), "track.id")?;
        validate_uuid_set(&self.clips, |clip| clip.id.as_str(), "clip.id")?;
        validate_uuid_set(&self.text_items, |text| text.id.as_str(), "text.id")?;
        validate_uuid_set(
            &self.transitions,
            |transition| transition.id.as_str(),
            "transition.id",
        )?;

        let mut all_ids = HashSet::new();
        for id in self
            .assets
            .iter()
            .map(|asset| asset.id.as_str())
            .chain(self.tracks.iter().map(|track| track.id.as_str()))
            .chain(self.clips.iter().map(|clip| clip.id.as_str()))
            .chain(self.text_items.iter().map(|text| text.id.as_str()))
            .chain(
                self.transitions
                    .iter()
                    .map(|transition| transition.id.as_str()),
            )
        {
            if !all_ids.insert(id) {
                return Err(invalid(format!("Entity IDs must be globally unique: {id}")));
            }
        }

        for asset in &self.assets {
            asset.validate()?;
        }
        for track in &self.tracks {
            track.validate()?;
        }
        let tracks: HashMap<_, _> = self
            .tracks
            .iter()
            .map(|track| (track.id.as_str(), track))
            .collect();
        let assets: HashMap<_, _> = self
            .assets
            .iter()
            .map(|asset| (asset.id.as_str(), asset))
            .collect();
        let clips: HashMap<_, _> = self
            .clips
            .iter()
            .map(|clip| (clip.id.as_str(), clip))
            .collect();
        for clip in &self.clips {
            let track = tracks
                .get(clip.track_id.as_str())
                .ok_or_else(|| invalid("Clip references an unknown track"))?;
            let asset = assets
                .get(clip.asset_id.as_str())
                .ok_or_else(|| invalid("Clip references an unknown asset"))?;
            clip.validate(track, asset)?;
        }
        for text in &self.text_items {
            text.validate(&tracks, &clips)?;
        }
        self.validate_transition_graph(&tracks, &clips)?;
        Ok(())
    }

    fn validate_transition_graph<TrackId, ClipId>(
        &self,
        tracks: &HashMap<TrackId, &Track>,
        clips: &HashMap<ClipId, &MediaClip>,
    ) -> Result<(), AppError>
    where
        TrackId: Borrow<str> + Eq + Hash,
        ClipId: Borrow<str> + Eq + Hash,
    {
        let mut transitions_by_pair: HashMap<(&str, &str), Vec<&Transition>> = HashMap::new();
        for transition in &self.transitions {
            transition.validate_ids()?;
            let left = clips
                .get(transition.left_clip_id.as_str())
                .ok_or_else(|| invalid("Transition references an unknown left clip"))?;
            let right = clips
                .get(transition.right_clip_id.as_str())
                .ok_or_else(|| invalid("Transition references an unknown right clip"))?;
            if transition.left_clip_id == transition.right_clip_id {
                return Err(invalid("A transition needs two different clips"));
            }
            if left.track_id != right.track_id {
                return Err(invalid("Transition clips must belong to the same track"));
            }
            let track = tracks
                .get(left.track_id.as_str())
                .ok_or_else(|| invalid("Transition references an unknown track"))?;
            if track.kind != TrackKind::Video {
                return Err(invalid("Dissolves are supported only on video tracks"));
            }
            if left.start_frame >= right.start_frame {
                return Err(invalid("Transition left clip must precede right clip"));
            }
            let left_end = left.end_frame()?;
            if right.start_frame >= left_end {
                return Err(invalid(
                    "Transition clips must overlap after the dissolve shift",
                ));
            }
            let overlap = left_end - right.start_frame;
            if overlap != transition.duration_frames {
                return Err(invalid("Transition duration must equal the clip overlap"));
            }
            if overlap < 2 || overlap >= left.duration_frames.min(right.duration_frames) {
                return Err(invalid(
                    "Transition duration must be at least two frames and shorter than both clips",
                ));
            }
            transitions_by_pair
                .entry((
                    transition.left_clip_id.as_str(),
                    transition.right_clip_id.as_str(),
                ))
                .or_default()
                .push(transition);
        }

        let mut video_by_track: HashMap<&str, Vec<&MediaClip>> = HashMap::new();
        for clip in self.clips.iter().filter(|clip| {
            tracks
                .get(clip.track_id.as_str())
                .is_some_and(|track| track.kind == TrackKind::Video)
        }) {
            video_by_track
                .entry(clip.track_id.as_str())
                .or_default()
                .push(clip);
        }
        for (track_id, track_clips) in video_by_track {
            let mut events = Vec::with_capacity(track_clips.len() * 2);
            for clip in &track_clips {
                let end = clip.end_frame()?;
                events.push((clip.start_frame, true));
                events.push((end, false));
            }
            events.sort_by_key(|(frame, starts)| (*frame, *starts));
            let mut active = 0i32;
            for (_, starts) in events {
                if starts {
                    active += 1;
                    if active > 2 {
                        return Err(invalid(format!(
                            "Video track {track_id} has a triple overlap"
                        )));
                    }
                } else {
                    active -= 1;
                }
            }

            let mut ordered = track_clips;
            ordered.sort_by_key(|clip| (clip.start_frame, clip.id.as_str()));
            for (left_index, left) in ordered.iter().enumerate() {
                let left_end = left.end_frame()?;
                for right in ordered.iter().skip(left_index + 1) {
                    if right.start_frame >= left_end {
                        break;
                    }
                    let pair = (left.id.as_str(), right.id.as_str());
                    let Some(transitions) = transitions_by_pair.get(&pair) else {
                        return Err(invalid(
                            "Overlapping video clips require an explicit dissolve",
                        ));
                    };
                    if transitions.len() != 1 {
                        return Err(invalid("A clip pair can have only one dissolve"));
                    }
                }
            }
        }
        Ok(())
    }

    pub fn timeline_end_frame(&self) -> Result<u64, AppError> {
        let mut end = 0u64;
        for clip in &self.clips {
            end = end.max(clip.end_frame()?);
        }
        for text in &self.text_items {
            if let Some(interval) = text.timeline_interval() {
                end = end.max(interval?.end_frame());
            }
        }
        Ok(end)
    }

    pub fn duration_frames(&self) -> Result<u64, AppError> {
        self.timeline_end_frame()
    }

    pub fn projected_captions(&self) -> Result<Vec<CaptionProjection>, AppError> {
        let clips: HashMap<_, _> = self
            .clips
            .iter()
            .map(|clip| (clip.id.as_str(), clip))
            .collect();
        let mut result = Vec::new();
        for text in self
            .text_items
            .iter()
            .filter(|text| text.owner_clip_id.is_some())
        {
            let clip_id = text.owner_clip_id.as_deref().unwrap();
            let clip = clips
                .get(clip_id)
                .ok_or_else(|| invalid("Caption references an unknown owner clip"))?;
            if let Some(projection) = text.project_on_clip(clip)? {
                result.push(projection);
            }
        }
        result.sort_unstable_by(|left, right| {
            left.timeline_start_frame
                .cmp(&right.timeline_start_frame)
                .then_with(|| left.caption_id.cmp(&right.caption_id))
        });
        Ok(result)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase")]
pub struct ProjectEnvelope {
    pub schema_version: u32,
    pub document: ProjectDocument,
    pub history: HistoryState,
    pub receipts: Vec<Receipt>,
}

impl ProjectEnvelope {
    pub fn new(document: ProjectDocument) -> Result<Self, AppError> {
        document.validate()?;
        Ok(Self {
            schema_version: PROJECT_SCHEMA_VERSION,
            document,
            history: HistoryState::default(),
            receipts: Vec::new(),
        })
    }

    pub fn validate(&self) -> Result<(), AppError> {
        if self.schema_version != PROJECT_SCHEMA_VERSION {
            return Err(AppError::schema(format!(
                "Unsupported project schema version: {}",
                self.schema_version
            )));
        }
        self.document.validate()?;
        self.history.validate()?;
        if self.receipts.len() > MAX_RECEIPTS {
            return Err(invalid("Project receipt history exceeds its capacity"));
        }
        let mut ids = HashSet::with_capacity(self.receipts.len());
        for receipt in &self.receipts {
            receipt.validate()?;
            if !ids.insert(receipt.transaction_id.as_str()) {
                return Err(invalid("Duplicate transaction receipt"));
            }
        }
        Ok(())
    }

    /// Validate a persisted envelope's reachable history without imposing a
    /// logical revision chain on the monotonic document revision. Undo and
    /// redo entries retain the revision at which their edit originally
    /// committed, while undo/redo operations themselves advance the current
    /// revision.
    pub fn validate_open(&self) -> Result<(), AppError> {
        self.validate()?;
        for entry in self.history.undo.iter().chain(self.history.redo.iter()) {
            let expected_revision = entry
                .expected_revision
                .checked_add(1)
                .filter(|revision| *revision <= MAX_SAFE_INTEGER)
                .ok_or_else(|| AppError::schema("History entry revision exceeds the safe range"))?;
            if entry.revision != expected_revision {
                return Err(AppError::schema(
                    "History entry revision does not follow its expected revision",
                ));
            }
        }
        for pair in self.history.undo.windows(2) {
            if pair[0].revision >= pair[1].revision {
                return Err(AppError::schema("Undo history entries are out of order"));
            }
        }
        for pair in self.history.redo.windows(2) {
            if pair[0].revision <= pair[1].revision {
                return Err(AppError::schema("Redo history entries are out of order"));
            }
        }

        // The current document is the post-state of the newest undo entry.
        // Replay backwards to the base state, then forwards to prove that
        // every retained undo entry is ordered and mutually compatible.
        let mut candidate = self.document.clone();
        for entry in self.history.undo.iter().rev() {
            entry
                .delta
                .apply_validated(&mut candidate, false)
                .map_err(|error| {
                    AppError::schema(format!("Undo history cannot be replayed: {error}"))
                })?;
        }
        for entry in &self.history.undo {
            entry
                .delta
                .apply_validated(&mut candidate, true)
                .map_err(|error| {
                    AppError::schema(format!("Undo history cannot be replayed: {error}"))
                })?;
        }
        // The redo stack is popped from the end, so replay it in reverse
        // storage order from the current document.
        for entry in self.history.redo.iter().rev() {
            entry
                .delta
                .apply_validated(&mut candidate, true)
                .map_err(|error| {
                    AppError::schema(format!("Redo history cannot be replayed: {error}"))
                })?;
        }
        Ok(())
    }

    pub fn trim_receipts(&mut self) {
        if self.receipts.len() > MAX_RECEIPTS {
            let remove = self.receipts.len() - MAX_RECEIPTS;
            self.receipts.drain(0..remove);
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    fn id(value: u64) -> String {
        format!("10000000-0000-4000-8000-{value:012x}")
    }

    fn video_document() -> ProjectDocument {
        ProjectDocument::new(
            "transition fixture",
            AspectRatio::Landscape,
            FrameRate::FPS_30,
        )
        .expect("document")
    }

    fn video_track(document: &ProjectDocument) -> String {
        document
            .tracks
            .iter()
            .find(|track| track.kind == TrackKind::Video)
            .expect("video track")
            .id
            .clone()
    }

    fn clip(
        clip_id: String,
        track_id: String,
        start_frame: u64,
        duration_frames: u64,
    ) -> MediaClip {
        MediaClip {
            id: clip_id,
            track_id,
            asset_id: id(100),
            start_frame,
            in_frame: 0,
            duration_frames,
            fit: FitMode::Contain,
            center_x: 5_000,
            center_y: 5_000,
            scale: 10_000,
            opacity: 10_000,
            gain_db: 0.0,
            audio_enabled: false,
            fade_in_frames: 0,
            fade_out_frames: 0,
        }
    }

    fn video_asset() -> AssetManifest {
        AssetManifest {
            id: id(100),
            kind: AssetKind::Video,
            content_hash: "hash".to_owned(),
            original: OriginalMediaMetadata {
                file_name: "fixture.mp4".to_owned(),
                streams: vec![OriginalStreamMetadata {
                    kind: OriginalStreamKind::Video,
                    codec: "h264".to_owned(),
                    duration_ms: Some(8_000),
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
                    master_artifact_id: "master".to_owned(),
                    proxy_artifact_id: None,
                    frame_count: 240,
                    width: 1_920,
                    height: 1_080,
                    fps_num: 30,
                    fps_den: 1,
                    active_start_frame: 0,
                    active_end_frame: 240,
                    source_start_ms: 0,
                    source_end_ms: 8_000,
                    proxy_frame_count: Some(240),
                }),
                audio: None,
            }),
        }
    }

    fn graph_result(
        base: &ProjectDocument,
        clips: Vec<MediaClip>,
        transitions: Vec<Transition>,
    ) -> Result<(), AppError> {
        let mut document = base.clone();
        document.assets.push(video_asset());
        document.clips = clips;
        document.transitions = transitions;
        document.validate()
    }

    #[test]
    fn transition_graph_sorts_arbitrary_clip_order_before_pair_scan() {
        let document = video_document();
        let track_id = video_track(&document);
        let clips = vec![
            clip(id(1), track_id.clone(), 200, 10),
            clip(id(2), track_id.clone(), 0, 10),
            clip(id(3), track_id, 100, 10),
        ];
        assert!(graph_result(&document, clips, Vec::new()).is_ok());
    }

    #[test]
    fn transition_graph_accepts_allowed_dissolve_overlap() {
        let document = video_document();
        let track_id = video_track(&document);
        let left_id = id(10);
        let right_id = id(11);
        let clips = vec![
            clip(left_id.clone(), track_id.clone(), 0, 100),
            clip(right_id.clone(), track_id, 90, 100),
        ];
        let transitions = vec![Transition {
            id: id(12),
            left_clip_id: left_id,
            right_clip_id: right_id,
            duration_frames: 10,
        }];
        assert!(graph_result(&document, clips, transitions).is_ok());
    }

    #[test]
    fn transition_graph_rejects_undeclared_and_triple_overlaps() {
        let document = video_document();
        let track_id = video_track(&document);
        assert!(graph_result(
            &document,
            vec![
                clip(id(20), track_id.clone(), 0, 100),
                clip(id(21), track_id.clone(), 90, 100),
            ],
            Vec::new(),
        )
        .is_err());
        assert!(graph_result(
            &document,
            vec![
                clip(id(30), track_id.clone(), 0, 100),
                clip(id(31), track_id.clone(), 10, 100),
                clip(id(32), track_id, 20, 100),
            ],
            Vec::new(),
        )
        .is_err());
    }
}
