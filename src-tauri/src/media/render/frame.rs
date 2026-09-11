//! Canonical RGBA composition shared by stateless and pooled rendering.

use super::cache::{
    decode_rgba_png, TextRasterCacheKey, MAX_RASTER_PNG_BYTES, MAX_TEXT_RASTER_CACHE_BYTES,
};
use super::decoder::PersistentDecoderPool;
use crate::error::{AppError, ErrorCode};
use crate::media::ffmpeg::{decode_rgba_frame, FfmpegToolchain};
use crate::media::render_plan::{
    ArtifactResolver, DestRect, RenderLayerKind, RenderPlan, RenderSegment, RenderTextOverlay,
    RenderTransition, SourceRect,
};
use crate::project::model::RgbaColor;
use std::fs::File;
use std::io::Read;
use std::sync::{Arc, Mutex};

#[derive(Default)]
struct RenderScratch {
    canvas: Vec<PmPixel>,
    transition_left: Vec<u8>,
    transition_right: Vec<u8>,
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
        let mut rgba = Vec::new();
        self.render_into(frame, &mut rgba)?;
        Ok(rgba)
    }

    pub fn render_into(&self, frame: u64, packed_rgba: &mut Vec<u8>) -> Result<(), AppError> {
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
        let scratch = &mut *scratch;
        render_rgba_frame_with_pool(
            &self.plan,
            frame,
            &mut pool,
            &mut scratch.canvas,
            &mut scratch.transition_left,
            &mut scratch.transition_right,
            packed_rgba,
        )
    }
}

enum FrameCompositionSource<'a> {
    Stateless {
        artifacts: &'a dyn ArtifactResolver,
        toolchain: &'a FfmpegToolchain,
    },
    Pooled {
        decoders: &'a mut PersistentDecoderPool,
        transition_left: &'a mut Vec<u8>,
        transition_right: &'a mut Vec<u8>,
    },
}

impl<'a> FrameCompositionSource<'a> {
    fn compose_transition(
        &mut self,
        plan: &RenderPlan,
        transition: &RenderTransition,
        left: &RenderSegment,
        right: &RenderSegment,
        frame: u64,
        destination: &mut [PmPixel],
    ) -> Result<(), AppError> {
        match self {
            Self::Stateless {
                artifacts,
                toolchain,
            } => compose_transition_into(
                plan,
                transition,
                left,
                right,
                frame,
                *artifacts,
                *toolchain,
                destination,
            ),
            Self::Pooled {
                decoders,
                transition_left,
                transition_right,
            } => {
                decode_segment_source_into_pool(left, frame, decoders, transition_left)?;
                decode_segment_source_into_pool(right, frame, decoders, transition_right)?;
                let left_source = (!transition_left.is_empty()).then_some(TransitionSource {
                    pixels: transition_left.as_slice(),
                    source_width: left.source_width,
                    source_height: left.source_height,
                    source_rect: &left.source_rect,
                    dest_rect: &left.dest_rect,
                    opacity: left.opacity,
                });
                let right_source = (!transition_right.is_empty()).then_some(TransitionSource {
                    pixels: transition_right.as_slice(),
                    source_width: right.source_width,
                    source_height: right.source_height,
                    source_rect: &right.source_rect,
                    dest_rect: &right.dest_rect,
                    opacity: right.opacity,
                });
                blend_transition_sources(
                    destination,
                    plan.width,
                    plan.height,
                    left_source,
                    right_source,
                    frame - transition.start_frame,
                    transition.duration_frames,
                );
                Ok(())
            }
        }
    }

    fn render_segment(
        &mut self,
        plan: &RenderPlan,
        segment: &RenderSegment,
        frame: u64,
        destination: &mut [PmPixel],
    ) -> Result<(), AppError> {
        match self {
            Self::Stateless {
                artifacts,
                toolchain,
            } => {
                let Some(source) =
                    decode_segment_source(plan, segment, frame, *artifacts, *toolchain)?
                else {
                    return Ok(());
                };
                blit_source(
                    destination,
                    plan.width,
                    plan.height,
                    &source,
                    segment.source_width,
                    segment.source_height,
                    &segment.source_rect,
                    &segment.dest_rect,
                    segment.opacity,
                );
                Ok(())
            }
            Self::Pooled { decoders, .. } => {
                render_segment_into_pool(plan, segment, frame, decoders, destination)
            }
        }
    }

    fn compose_text_overlay(
        &mut self,
        plan: &RenderPlan,
        overlay: &RenderTextOverlay,
        destination: &mut [PmPixel],
    ) -> Result<(), AppError> {
        match self {
            Self::Stateless {
                artifacts,
                toolchain: _,
            } => compose_text_overlay(plan, overlay, *artifacts, destination),
            Self::Pooled { decoders, .. } => {
                if let Some((width, height, image)) = decoders.text_cache().get(plan, overlay) {
                    blit_premultiplied(
                        destination,
                        plan.width,
                        plan.height,
                        image,
                        width,
                        height,
                        &overlay.raster_source_rect,
                        &overlay.raster_dest_rect,
                    );
                } else {
                    let (width, height, rgba) = load_text_overlay(overlay, decoders.artifacts())?;
                    let cacheable = usize::try_from(width)
                        .ok()
                        .and_then(|width| {
                            usize::try_from(height)
                                .ok()
                                .and_then(|height| width.checked_mul(height))
                        })
                        .and_then(|pixels| pixels.checked_mul(std::mem::size_of::<PmPixel>()))
                        .is_some_and(|bytes| bytes <= MAX_TEXT_RASTER_CACHE_BYTES);
                    if cacheable {
                        let image = rgba
                            .chunks_exact(4)
                            .map(|pixel| {
                                PmPixel::from_rgba(pixel[0], pixel[1], pixel[2], pixel[3], 10_000)
                            })
                            .collect::<Vec<_>>();
                        blit_premultiplied(
                            destination,
                            plan.width,
                            plan.height,
                            &image,
                            width,
                            height,
                            &overlay.raster_source_rect,
                            &overlay.raster_dest_rect,
                        );
                        decoders.text_cache().insert(
                            TextRasterCacheKey::new(plan, overlay),
                            width,
                            height,
                            image,
                        );
                    } else {
                        blit_source(
                            destination,
                            plan.width,
                            plan.height,
                            &rgba,
                            width,
                            height,
                            &overlay.raster_source_rect,
                            &overlay.raster_dest_rect,
                            10_000,
                        );
                    }
                }
                Ok(())
            }
        }
    }
}

fn compose_layers(
    plan: &RenderPlan,
    frame: u64,
    canvas: &mut [PmPixel],
    source: &mut FrameCompositionSource<'_>,
) -> Result<(), AppError> {
    for layer in &plan.layers {
        if layer.kind == RenderLayerKind::Video {
            for transition in layer.transitions.iter().filter(|transition| {
                transition.start_frame <= frame && frame < transition.end_frame
            }) {
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
                source.compose_transition(plan, transition, left, right, frame, canvas)?;
            }
            for segment in &layer.segments {
                let in_transition = layer.transitions.iter().any(|transition| {
                    transition.start_frame <= frame
                        && frame < transition.end_frame
                        && (transition.left_clip_id == segment.clip_id
                            || transition.right_clip_id == segment.clip_id)
                });
                if !in_transition && segment.start_frame <= frame && frame < segment.end_frame {
                    source.render_segment(plan, segment, frame, canvas)?;
                }
            }
        }
        for overlay in layer
            .text_overlays
            .iter()
            .filter(|overlay| overlay.start_frame <= frame && frame < overlay.end_frame)
        {
            source.compose_text_overlay(plan, overlay, canvas)?;
        }
    }
    Ok(())
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
    let canvas_len = canvas_len(plan)?;
    let background = PmPixel::from_color(plan.background);
    let mut canvas = vec![background; canvas_len];
    let mut source = FrameCompositionSource::Stateless {
        artifacts,
        toolchain,
    };
    compose_layers(plan, frame, &mut canvas, &mut source)?;
    let packed_len = canvas_len
        .checked_mul(4)
        .ok_or_else(|| AppError::invalid_argument("Render canvas is too large"))?;
    let mut rgba = vec![0u8; packed_len];
    for (chunk, pixel) in rgba.chunks_exact_mut(4).zip(canvas) {
        chunk.copy_from_slice(&pixel.to_straight_rgba());
    }
    Ok(rgba)
}

fn segment_source_frame(segment: &RenderSegment, frame: u64) -> Result<Option<u64>, AppError> {
    if frame < segment.start_frame || frame >= segment.end_frame {
        return Ok(None);
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
        return Ok(None);
    }
    Ok(Some(source_frame))
}

fn decode_segment_source(
    plan: &RenderPlan,
    segment: &RenderSegment,
    frame: u64,
    artifacts: &dyn ArtifactResolver,
    toolchain: &FfmpegToolchain,
) -> Result<Option<Vec<u8>>, AppError> {
    let Some(source_frame) = segment_source_frame(segment, frame)? else {
        return Ok(None);
    };
    let master = artifacts.managed_path(&segment.artifact_id)?;
    decode_rgba_frame(
        toolchain,
        &master,
        source_frame,
        plan.fps(),
        segment.source_width,
        segment.source_height,
    )
    .map(Some)
}

fn compose_transition_into(
    plan: &RenderPlan,
    transition: &RenderTransition,
    left: &RenderSegment,
    right: &RenderSegment,
    frame: u64,
    artifacts: &dyn ArtifactResolver,
    toolchain: &FfmpegToolchain,
    destination: &mut [PmPixel],
) -> Result<(), AppError> {
    let left_source = decode_segment_source(plan, left, frame, artifacts, toolchain)?;
    let right_source = decode_segment_source(plan, right, frame, artifacts, toolchain)?;
    let left_source = left_source.as_deref().map(|pixels| TransitionSource {
        pixels,
        source_width: left.source_width,
        source_height: left.source_height,
        source_rect: &left.source_rect,
        dest_rect: &left.dest_rect,
        opacity: left.opacity,
    });
    let right_source = right_source.as_deref().map(|pixels| TransitionSource {
        pixels,
        source_width: right.source_width,
        source_height: right.source_height,
        source_rect: &right.source_rect,
        dest_rect: &right.dest_rect,
        opacity: right.opacity,
    });
    blend_transition_sources(
        destination,
        plan.width,
        plan.height,
        left_source,
        right_source,
        frame - transition.start_frame,
        transition.duration_frames,
    );
    Ok(())
}

fn compose_text_overlay(
    plan: &RenderPlan,
    overlay: &RenderTextOverlay,
    artifacts: &dyn ArtifactResolver,
    destination: &mut [PmPixel],
) -> Result<(), AppError> {
    let (width, height, pixels) = load_text_overlay(overlay, artifacts)?;
    blit_source(
        destination,
        plan.width,
        plan.height,
        &pixels,
        width,
        height,
        &overlay.raster_source_rect,
        &overlay.raster_dest_rect,
        10_000,
    );
    Ok(())
}

fn load_text_overlay(
    overlay: &RenderTextOverlay,
    artifacts: &dyn ArtifactResolver,
) -> Result<(u32, u32, Vec<u8>), AppError> {
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
    decode_rgba_png(&bytes)
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

#[derive(Debug, Clone, Copy)]
struct BlitBounds {
    x0: u32,
    y0: u32,
    x1: u32,
    y1: u32,
}

impl BlitBounds {
    fn new(
        canvas_width: u32,
        canvas_height: u32,
        source_rect: &SourceRect,
        dest_rect: &DestRect,
    ) -> Option<Self> {
        let x0 = i64::from(dest_rect.x).max(0) as u32;
        let y0 = i64::from(dest_rect.y).max(0) as u32;
        let x1 = (i64::from(dest_rect.x) + i64::from(dest_rect.width))
            .min(i64::from(canvas_width))
            .max(0) as u32;
        let y1 = (i64::from(dest_rect.y) + i64::from(dest_rect.height))
            .min(i64::from(canvas_height))
            .max(0) as u32;
        (x1 > x0 && y1 > y0 && source_rect.width > 0 && source_rect.height > 0).then_some(Self {
            x0,
            y0,
            x1,
            y1,
        })
    }
}

#[inline]
fn mapped_source_x(source_rect: &SourceRect, dest_rect: &DestRect, x: u32) -> usize {
    let rel_x = (i64::from(x) - i64::from(dest_rect.x)).max(0) as u64;
    (u64::from(source_rect.x)
        + rel_x.saturating_mul(u64::from(source_rect.width)) / u64::from(dest_rect.width.max(1)))
    .min(u64::from(
        source_rect
            .x
            .saturating_add(source_rect.width)
            .saturating_sub(1),
    )) as usize
}

#[inline]
fn mapped_source_y(source_rect: &SourceRect, dest_rect: &DestRect, y: u32) -> usize {
    let rel_y = (i64::from(y) - i64::from(dest_rect.y)).max(0) as u64;
    (u64::from(source_rect.y)
        + rel_y.saturating_mul(u64::from(source_rect.height)) / u64::from(dest_rect.height.max(1)))
    .min(u64::from(
        source_rect
            .y
            .saturating_add(source_rect.height)
            .saturating_sub(1),
    )) as usize
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
    let Some(bounds) = BlitBounds::new(canvas_width, canvas_height, source_rect, dest_rect) else {
        return;
    };
    let source_columns = (bounds.x0..bounds.x1)
        .map(|x| mapped_source_x(source_rect, dest_rect, x))
        .collect::<Vec<_>>();
    for y in bounds.y0..bounds.y1 {
        let source_y = mapped_source_y(source_rect, dest_rect, y);
        if source_y >= source_height as usize {
            continue;
        }
        let Some(source_row) = source_y.checked_mul(source_width as usize) else {
            continue;
        };
        let destination_row = y as usize * canvas_width as usize;
        for (column, source_x) in source_columns.iter().copied().enumerate() {
            if source_x >= source_width as usize {
                continue;
            }
            let Some(source_index) = source_row
                .checked_add(source_x)
                .and_then(|index| index.checked_mul(4))
            else {
                continue;
            };
            let Some(source_end) = source_index.checked_add(3) else {
                continue;
            };
            if source_end >= source.len() {
                continue;
            }
            let pixel = PmPixel::from_rgba(
                source[source_index],
                source[source_index + 1],
                source[source_index + 2],
                source[source_index + 3],
                opacity,
            );
            let destination_index = destination_row + bounds.x0 as usize + column;
            destination[destination_index] = destination[destination_index].over(pixel);
        }
    }
}

fn blit_premultiplied(
    destination: &mut [PmPixel],
    canvas_width: u32,
    canvas_height: u32,
    source: &[PmPixel],
    source_width: u32,
    source_height: u32,
    source_rect: &SourceRect,
    dest_rect: &DestRect,
) {
    let Some(bounds) = BlitBounds::new(canvas_width, canvas_height, source_rect, dest_rect) else {
        return;
    };
    let source_columns = (bounds.x0..bounds.x1)
        .map(|x| mapped_source_x(source_rect, dest_rect, x))
        .collect::<Vec<_>>();
    for y in bounds.y0..bounds.y1 {
        let source_y = mapped_source_y(source_rect, dest_rect, y);
        if source_y >= source_height as usize {
            continue;
        }
        let Some(source_row) = source_y.checked_mul(source_width as usize) else {
            continue;
        };
        let destination_row = y as usize * canvas_width as usize;
        for (column, source_x) in source_columns.iter().copied().enumerate() {
            if source_x >= source_width as usize {
                continue;
            }
            let Some(source_index) = source_row.checked_add(source_x) else {
                continue;
            };
            let Some(&pixel) = source.get(source_index) else {
                continue;
            };
            let destination_index = destination_row + bounds.x0 as usize + column;
            destination[destination_index] = destination[destination_index].over(pixel);
        }
    }
}

#[derive(Clone, Copy)]
struct TransitionSource<'a> {
    pixels: &'a [u8],
    source_width: u32,
    source_height: u32,
    source_rect: &'a SourceRect,
    dest_rect: &'a DestRect,
    opacity: u16,
}

#[inline]
fn transition_pixel_mapped(
    source: TransitionSource<'_>,
    source_x: usize,
    source_y: usize,
) -> PmPixel {
    if source_x >= source.source_width as usize || source_y >= source.source_height as usize {
        return PmPixel::transparent();
    }
    let Some(source_index) = source_y
        .checked_mul(source.source_width as usize)
        .and_then(|row| row.checked_add(source_x))
        .and_then(|index| index.checked_mul(4))
    else {
        return PmPixel::transparent();
    };
    let Some(source_end) = source_index.checked_add(3) else {
        return PmPixel::transparent();
    };
    if source_end >= source.pixels.len() {
        return PmPixel::transparent();
    }
    PmPixel::from_rgba(
        source.pixels[source_index],
        source.pixels[source_index + 1],
        source.pixels[source_index + 2],
        source.pixels[source_index + 3],
        source.opacity,
    )
}

fn blend_transition_sources(
    destination: &mut [PmPixel],
    canvas_width: u32,
    canvas_height: u32,
    left: Option<TransitionSource<'_>>,
    right: Option<TransitionSource<'_>>,
    relative: u64,
    duration: u64,
) {
    let left_bounds = left.and_then(|source| {
        BlitBounds::new(
            canvas_width,
            canvas_height,
            source.source_rect,
            source.dest_rect,
        )
    });
    let right_bounds = right.and_then(|source| {
        BlitBounds::new(
            canvas_width,
            canvas_height,
            source.source_rect,
            source.dest_rect,
        )
    });
    let Some((x0, y0, x1, y1)) = union_bounds(left_bounds, right_bounds) else {
        return;
    };
    let left_columns = left.zip(left_bounds).map(|(source, bounds)| {
        (bounds.x0..bounds.x1)
            .map(|x| mapped_source_x(source.source_rect, source.dest_rect, x))
            .collect::<Vec<_>>()
    });
    let right_columns = right.zip(right_bounds).map(|(source, bounds)| {
        (bounds.x0..bounds.x1)
            .map(|x| mapped_source_x(source.source_rect, source.dest_rect, x))
            .collect::<Vec<_>>()
    });
    for y in y0..y1 {
        let left_source_y = left.zip(left_bounds).and_then(|(source, bounds)| {
            (y >= bounds.y0 && y < bounds.y1)
                .then(|| mapped_source_y(source.source_rect, source.dest_rect, y))
        });
        let right_source_y = right.zip(right_bounds).and_then(|(source, bounds)| {
            (y >= bounds.y0 && y < bounds.y1)
                .then(|| mapped_source_y(source.source_rect, source.dest_rect, y))
        });
        let destination_row = y as usize * canvas_width as usize;
        for x in x0..x1 {
            let left_pixel = match (left, left_bounds, left_source_y, left_columns.as_deref()) {
                (Some(source), Some(bounds), Some(source_y), Some(columns))
                    if x >= bounds.x0 && x < bounds.x1 =>
                {
                    transition_pixel_mapped(source, columns[(x - bounds.x0) as usize], source_y)
                }
                _ => PmPixel::transparent(),
            };
            let right_pixel = match (
                right,
                right_bounds,
                right_source_y,
                right_columns.as_deref(),
            ) {
                (Some(source), Some(bounds), Some(source_y), Some(columns))
                    if x >= bounds.x0 && x < bounds.x1 =>
                {
                    transition_pixel_mapped(source, columns[(x - bounds.x0) as usize], source_y)
                }
                _ => PmPixel::transparent(),
            };
            let blended = PmPixel::weighted_pair(left_pixel, right_pixel, relative, duration);
            let destination_index = destination_row + x as usize;
            destination[destination_index] = destination[destination_index].over(blended);
        }
    }
}

fn union_bounds(
    left: Option<BlitBounds>,
    right: Option<BlitBounds>,
) -> Option<(u32, u32, u32, u32)> {
    match (left, right) {
        (Some(left), Some(right)) => Some((
            left.x0.min(right.x0),
            left.y0.min(right.y0),
            left.x1.max(right.x1),
            left.y1.max(right.y1),
        )),
        (Some(bounds), None) | (None, Some(bounds)) => {
            Some((bounds.x0, bounds.y0, bounds.x1, bounds.y1))
        }
        (None, None) => None,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct PmPixel {
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

pub(super) fn render_rgba_frame_with_pool(
    plan: &RenderPlan,
    frame: u64,
    decoders: &mut PersistentDecoderPool,
    canvas: &mut Vec<PmPixel>,
    transition_left: &mut Vec<u8>,
    transition_right: &mut Vec<u8>,
    packed_rgba: &mut Vec<u8>,
) -> Result<(), AppError> {
    // The persistent decoder path uses the same layer ordering and pixel
    // composition routine as stateless rendering.  Only source acquisition
    // and text-raster caching vary between the two paths.
    let canvas_len = canvas_len(plan)?;
    decoders.begin_frame();
    let background = PmPixel::from_color(plan.background);
    canvas.resize(canvas_len, background);
    canvas.fill(background);
    {
        let mut source = FrameCompositionSource::Pooled {
            decoders,
            transition_left,
            transition_right,
        };
        compose_layers(plan, frame, canvas, &mut source)?;
    }
    decoders.retire_inactive();
    let packed_len = canvas_len
        .checked_mul(4)
        .ok_or_else(|| AppError::invalid_argument("Render canvas is too large"))?;
    packed_rgba.resize(packed_len, 0);
    for (chunk, pixel) in packed_rgba.chunks_exact_mut(4).zip(canvas.iter()) {
        chunk.copy_from_slice(&pixel.to_straight_rgba());
    }
    Ok(())
}

fn decode_segment_source_into_pool(
    segment: &RenderSegment,
    frame: u64,
    decoders: &mut PersistentDecoderPool,
    output: &mut Vec<u8>,
) -> Result<(), AppError> {
    output.clear();
    let Some(source_frame) = segment_source_frame(segment, frame)? else {
        return Ok(());
    };
    let source = decoders.frame(
        &segment.clip_id,
        source_frame,
        segment.source_width,
        segment.source_height,
    )?;
    output.extend_from_slice(source);
    Ok(())
}

fn render_segment_into_pool(
    plan: &RenderPlan,
    segment: &RenderSegment,
    frame: u64,
    decoders: &mut PersistentDecoderPool,
    destination: &mut [PmPixel],
) -> Result<(), AppError> {
    let Some(source_frame) = segment_source_frame(segment, frame)? else {
        return Ok(());
    };
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
    fn optimized_blit_and_transition_paths_match_canonical_composition() {
        let source: Vec<u8> = (0u8..8)
            .flat_map(|value| {
                [
                    value.wrapping_mul(17),
                    255 - value * 7,
                    value * 3,
                    64 + value * 20,
                ]
            })
            .collect();
        let cases = [
            (
                SourceRect {
                    x: 0,
                    y: 0,
                    width: 4,
                    height: 2,
                },
                DestRect {
                    x: -1,
                    y: 1,
                    width: 6,
                    height: 3,
                },
                8_731,
            ),
            (
                SourceRect {
                    x: 1,
                    y: 0,
                    width: 2,
                    height: 2,
                },
                DestRect {
                    x: 2,
                    y: -1,
                    width: 5,
                    height: 4,
                },
                10_000,
            ),
        ];
        for (source_rect, dest_rect, opacity) in cases {
            let mut canonical = vec![PmPixel::from_rgba(9, 11, 13, 255, 10_000); 20];
            let mut optimized = canonical.clone();
            let x0 = i64::from(dest_rect.x).max(0) as u32;
            let y0 = i64::from(dest_rect.y).max(0) as u32;
            let x1 = (i64::from(dest_rect.x) + i64::from(dest_rect.width))
                .min(5)
                .max(0) as u32;
            let y1 = (i64::from(dest_rect.y) + i64::from(dest_rect.height))
                .min(4)
                .max(0) as u32;
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
                let source_row = sy as usize * 4;
                let destination_row = y as usize * 5 + x0 as usize;
                for (column, &sx) in source_columns.iter().enumerate() {
                    let source_index = (source_row + sx) * 4;
                    let pixel = PmPixel::from_rgba(
                        source[source_index],
                        source[source_index + 1],
                        source[source_index + 2],
                        source[source_index + 3],
                        opacity,
                    );
                    let destination_index = destination_row + column;
                    canonical[destination_index] = canonical[destination_index].over(pixel);
                }
            }
            blit_source(
                &mut optimized,
                5,
                4,
                &source,
                4,
                2,
                &source_rect,
                &dest_rect,
                opacity,
            );
            assert_eq!(optimized, canonical);
        }

        let left_source: Vec<u8> = (0u8..16)
            .flat_map(|value| [value * 7, 20 + value * 3, 200 - value * 5, 100 + value * 4])
            .collect();
        let right_source: Vec<u8> = (0u8..16)
            .flat_map(|value| [180 - value * 4, value * 5, 30 + value * 6, 60 + value * 8])
            .collect();
        let left_rect = SourceRect {
            x: 0,
            y: 0,
            width: 4,
            height: 4,
        };
        let right_rect = left_rect.clone();
        let left_dest = DestRect {
            x: 0,
            y: 0,
            width: 4,
            height: 4,
        };
        let right_dest = DestRect {
            x: 1,
            y: 1,
            width: 3,
            height: 3,
        };
        let left = TransitionSource {
            pixels: &left_source,
            source_width: 4,
            source_height: 4,
            source_rect: &left_rect,
            dest_rect: &left_dest,
            opacity: 9_000,
        };
        let right = TransitionSource {
            pixels: &right_source,
            source_width: 4,
            source_height: 4,
            source_rect: &right_rect,
            dest_rect: &right_dest,
            opacity: 6_000,
        };
        let background = PmPixel::from_rgba(10, 20, 30, 255, 10_000);
        let mut canonical = vec![background; 25];
        let mut left_layer = vec![PmPixel::transparent(); 25];
        let mut right_layer = vec![PmPixel::transparent(); 25];
        blit_source(
            &mut left_layer,
            5,
            5,
            &left_source,
            4,
            4,
            &left_rect,
            &left_dest,
            left.opacity,
        );
        blit_source(
            &mut right_layer,
            5,
            5,
            &right_source,
            4,
            4,
            &right_rect,
            &right_dest,
            right.opacity,
        );
        for (destination, (left, right)) in canonical
            .iter_mut()
            .zip(left_layer.into_iter().zip(right_layer.into_iter()))
        {
            *destination = destination.over(PmPixel::weighted_pair(left, right, 2, 5));
        }
        let mut optimized = vec![background; 25];
        blend_transition_sources(&mut optimized, 5, 5, Some(left), Some(right), 2, 5);
        assert_eq!(optimized, canonical);
    }
}
