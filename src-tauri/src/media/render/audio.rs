//! Canonical indexed PCM mixing from the immutable render audio envelope.

use crate::error::{AppError, ErrorCode};
use crate::media::ffmpeg::read_f32_stereo_window;
use crate::media::render_plan::{ArtifactResolver, RenderPlan};
use std::fs::File;

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
