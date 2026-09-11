//! Bounded artifact and raster caches used by canonical rendering.

use super::frame::PmPixel;
use crate::error::{AppError, ErrorCode};
use crate::media::render_plan::{ArtifactResolver, RenderPlan, RenderTextOverlay};
use std::collections::VecDeque;
use std::fs::{self, File};
use std::io::{Cursor, Read};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct RenderFrameCacheKey {
    project_id: String,
    revision: u64,
    plan_hash: String,
    frame: u64,
}

impl RenderFrameCacheKey {
    pub(super) fn new(plan: &RenderPlan, frame: u64) -> Self {
        Self {
            project_id: plan.project_id.clone(),
            revision: plan.revision,
            plan_hash: plan.plan_hash.clone(),
            frame,
        }
    }

    pub(super) fn storage_key(&self) -> String {
        format!(
            "frame:{}:{}:{}:{}",
            self.project_id, self.revision, self.plan_hash, self.frame
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct RenderFrameCacheCandidate {
    pub(super) artifact_id: String,
    pub(super) artifact_len: u64,
}

struct RenderFrameArtifactCacheEntry {
    key: RenderFrameCacheKey,
    artifact_id: String,
    artifact_len: u64,
    bytes: usize,
}

#[derive(Default)]
pub(super) struct RenderFrameArtifactCache {
    epoch: u64,
    entries: VecDeque<RenderFrameArtifactCacheEntry>,
    bytes: usize,
}

impl RenderFrameArtifactCache {
    pub(super) fn candidate(
        &self,
        key: &RenderFrameCacheKey,
    ) -> (u64, Option<RenderFrameCacheCandidate>) {
        (
            self.epoch,
            self.entries
                .iter()
                .find(|entry| entry.key.eq(key))
                .map(|entry| RenderFrameCacheCandidate {
                    artifact_id: entry.artifact_id.clone(),
                    artifact_len: entry.artifact_len,
                }),
        )
    }

    pub(super) fn confirm_hit(
        &mut self,
        key: &RenderFrameCacheKey,
        epoch: u64,
        artifact_id: &str,
        artifact_len: u64,
    ) -> bool {
        if self.epoch != epoch {
            return false;
        }
        let Some(index) = self.entries.iter().position(|entry| {
            entry.key.eq(key)
                && entry.artifact_id == artifact_id
                && entry.artifact_len == artifact_len
        }) else {
            return false;
        };
        if index != 0 {
            if let Some(entry) = self.entries.remove(index) {
                self.entries.push_front(entry);
            }
        }
        true
    }

    pub(super) fn evict_if_matches(
        &mut self,
        key: &RenderFrameCacheKey,
        epoch: u64,
        artifact_id: &str,
        artifact_len: u64,
    ) {
        if self.epoch != epoch {
            return;
        }
        let Some(index) = self.entries.iter().position(|entry| {
            entry.key.eq(key)
                && entry.artifact_id == artifact_id
                && entry.artifact_len == artifact_len
        }) else {
            return;
        };
        if let Some(entry) = self.entries.remove(index) {
            self.bytes = self.bytes.saturating_sub(entry.bytes);
        }
    }

    pub(super) fn insert_if_current(
        &mut self,
        key: RenderFrameCacheKey,
        artifact_id: String,
        artifact_len: u64,
        epoch: u64,
    ) {
        if self.epoch != epoch || artifact_len == 0 {
            return;
        }
        let Some(bytes) = key
            .project_id
            .len()
            .checked_add(key.plan_hash.len())
            .and_then(|value| value.checked_add(artifact_id.len()))
        else {
            return;
        };
        if bytes > MAX_RENDER_FRAME_CACHE_BYTES {
            return;
        }
        if let Some(index) = self.entries.iter().position(|entry| entry.key.eq(&key)) {
            if let Some(entry) = self.entries.remove(index) {
                self.bytes = self.bytes.saturating_sub(entry.bytes);
            }
        }
        while self.entries.len() >= MAX_RENDER_FRAME_CACHE_ENTRIES
            || self.bytes.saturating_add(bytes) > MAX_RENDER_FRAME_CACHE_BYTES
        {
            let Some(entry) = self.entries.pop_back() else {
                break;
            };
            self.bytes = self.bytes.saturating_sub(entry.bytes);
        }
        if self.entries.len() >= MAX_RENDER_FRAME_CACHE_ENTRIES
            || self.bytes.saturating_add(bytes) > MAX_RENDER_FRAME_CACHE_BYTES
        {
            return;
        }
        self.bytes = self.bytes.saturating_add(bytes);
        self.entries.push_front(RenderFrameArtifactCacheEntry {
            key,
            artifact_id,
            artifact_len,
            bytes,
        });
    }

    pub(super) fn invalidate(&mut self) {
        self.entries.clear();
        self.bytes = 0;
        self.epoch = self.epoch.wrapping_add(1);
    }
}

pub(super) const MAX_RASTER_PNG_BYTES: usize = 64 * 1024 * 1024;
const MAX_RASTER_PNG_DIMENSION: u32 = 4_096;
const MAX_RASTER_PNG_PIXELS: u64 = 16_000_000;
pub(super) const MAX_TEXT_RASTER_CACHE_BYTES: usize = 64 * 1024 * 1024;
const MAX_TEXT_RASTER_CACHE_ENTRIES: usize = 256;
const MAX_RENDER_FRAME_CACHE_ENTRIES: usize = 32;
const MAX_RENDER_FRAME_CACHE_BYTES: usize = 64 * 1024;

pub(super) fn validate_cached_frame_artifact(
    artifacts: &dyn ArtifactResolver,
    artifact_id: &str,
    expected_len: u64,
    width: u32,
    height: u32,
) -> bool {
    let Ok(path) = artifacts.managed_path(artifact_id) else {
        return false;
    };
    let Ok(metadata) = fs::symlink_metadata(&path) else {
        return false;
    };
    if !metadata.file_type().is_file() || metadata.len() != expected_len {
        return false;
    }
    let Ok(mut file) = File::open(path) else {
        return false;
    };
    let mut header = [0u8; 24];
    if file.read_exact(&mut header).is_err() {
        return false;
    }
    let width_bytes = [header[16], header[17], header[18], header[19]];
    let height_bytes = [header[20], header[21], header[22], header[23]];
    &header[..8] == b"\x89PNG\r\n\x1a\n"
        && &header[12..16] == b"IHDR"
        && u32::from_be_bytes(width_bytes) == width
        && u32::from_be_bytes(height_bytes) == height
}

pub(super) fn decode_rgba_png(bytes: &[u8]) -> Result<(u32, u32, Vec<u8>), AppError> {
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct TextRasterCacheKey {
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
    pub(super) fn new(plan: &RenderPlan, overlay: &RenderTextOverlay) -> Self {
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
    width: u32,
    height: u32,
    pixels: Vec<PmPixel>,
    bytes: usize,
}

#[derive(Default)]
pub(super) struct TextRasterCache {
    entries: VecDeque<TextRasterCacheEntry>,
    bytes: usize,
}

impl TextRasterCache {
    pub(super) fn get(
        &mut self,
        plan: &RenderPlan,
        overlay: &RenderTextOverlay,
    ) -> Option<(u32, u32, &[PmPixel])> {
        let index = self
            .entries
            .iter()
            .position(|entry| entry.key.matches(plan, overlay))?;
        if index != 0 {
            let entry = self.entries.remove(index)?;
            self.entries.push_front(entry);
        }
        self.entries
            .front()
            .map(|entry| (entry.width, entry.height, entry.pixels.as_slice()))
    }

    pub(super) fn insert(
        &mut self,
        key: TextRasterCacheKey,
        width: u32,
        height: u32,
        pixels: Vec<PmPixel>,
    ) {
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
        self.entries.push_front(TextRasterCacheEntry {
            key,
            width,
            height,
            pixels,
            bytes,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::render_plan::encode_rgba_png;
    use std::fs as std_fs;
    use std::path::PathBuf;
    use std::sync::Arc;

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
    fn rendered_frame_cache_reuses_only_authoritative_artifacts() {
        struct CacheArtifacts {
            path: PathBuf,
        }

        impl ArtifactResolver for CacheArtifacts {
            fn managed_path(&self, artifact_id: &str) -> Result<PathBuf, AppError> {
                if artifact_id == "frame.png" {
                    Ok(self.path.clone())
                } else {
                    Err(AppError::new(
                        ErrorCode::AssetUnavailable,
                        "Managed artifact is unavailable",
                    ))
                }
            }

            fn put_bytes(
                &self,
                _cache_key: &str,
                _extension: &str,
                _content_type: &str,
                _bytes: &[u8],
            ) -> Result<String, AppError> {
                Ok("frame.png".to_owned())
            }

            fn bundled_font_catalog(
                &self,
            ) -> Result<Arc<crate::media::graphics::BundledFontCatalog>, AppError> {
                panic!("font catalog is not needed by frame-cache tests")
            }
        }

        let root = std::env::temp_dir().join(format!(
            "cutterhoochee-frame-cache-{}",
            uuid::Uuid::new_v4()
        ));
        std_fs::create_dir_all(&root).unwrap();
        let path = root.join("frame.png");
        let rgba = vec![
            32u8, 64, 96, 255, 8, 16, 24, 255, 3, 6, 9, 255, 200, 150, 100, 255,
        ];
        let png = encode_rgba_png(2, 2, &rgba).unwrap();
        assert!(png.len() > 24);
        std_fs::write(&path, &png).unwrap();
        let artifacts = CacheArtifacts { path: path.clone() };
        let key = RenderFrameCacheKey {
            project_id: "project".to_owned(),
            revision: 4,
            plan_hash: "plan".to_owned(),
            frame: 7,
        };
        let mut cache = RenderFrameArtifactCache::default();
        let epoch = cache.epoch;
        let artifact_len = png.len() as u64;
        cache.insert_if_current(key.clone(), "frame.png".to_owned(), artifact_len, epoch);

        let (candidate_epoch, candidate) = cache.candidate(&key);
        assert_eq!(candidate_epoch, epoch);
        let candidate = candidate.expect("cached frame");
        assert_eq!(candidate.artifact_len, artifact_len);
        assert!(validate_cached_frame_artifact(
            &artifacts,
            &candidate.artifact_id,
            candidate.artifact_len,
            2,
            2
        ));
        assert!(cache.confirm_hit(&key, epoch, &candidate.artifact_id, candidate.artifact_len));

        std_fs::write(&path, &png[..24]).unwrap();
        assert!(!validate_cached_frame_artifact(
            &artifacts,
            &candidate.artifact_id,
            candidate.artifact_len,
            2,
            2
        ));
        std_fs::remove_file(&path).unwrap();
        assert!(!validate_cached_frame_artifact(
            &artifacts,
            &candidate.artifact_id,
            candidate.artifact_len,
            2,
            2
        ));
        cache.evict_if_matches(&key, epoch, &candidate.artifact_id, candidate.artifact_len);
        assert!(cache.candidate(&key).1.is_none());
        std_fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn rendered_frame_cache_invalidation_and_lru_eviction_are_bounded() {
        let mut cache = RenderFrameArtifactCache::default();
        let initial_epoch = cache.epoch;
        let key = |frame| RenderFrameCacheKey {
            project_id: "project".to_owned(),
            revision: 1,
            plan_hash: "plan".to_owned(),
            frame,
        };
        for frame in 0..=MAX_RENDER_FRAME_CACHE_ENTRIES as u64 {
            let frame_key = key(frame);
            cache.insert_if_current(frame_key, format!("frame-{frame}.png"), 1, initial_epoch);
        }
        assert_eq!(cache.entries.len(), MAX_RENDER_FRAME_CACHE_ENTRIES);
        assert!(cache.candidate(&key(0)).1.is_none());
        let newest_artifact = format!("frame-{}.png", MAX_RENDER_FRAME_CACHE_ENTRIES);
        let newest = cache
            .candidate(&key(MAX_RENDER_FRAME_CACHE_ENTRIES as u64))
            .1
            .expect("newest cached frame");
        assert_eq!(newest.artifact_id, newest_artifact);
        let mut changed_plan = key(1);
        changed_plan.plan_hash = "changed-plan".to_owned();
        assert!(cache.candidate(&changed_plan).1.is_none());

        cache.invalidate();
        assert_ne!(cache.epoch, initial_epoch);
        assert!(cache.entries.is_empty());
        let stale_key = key(99);
        cache.insert_if_current(stale_key.clone(), "stale.png".to_owned(), 1, initial_epoch);
        assert!(cache.candidate(&stale_key).1.is_none());
    }
}
