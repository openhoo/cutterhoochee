use crate::error::AppError;
use crate::project::model::{FrameRate, ProjectDocument, TextKind};

pub(super) fn projected_srt(document: &ProjectDocument) -> Result<String, AppError> {
    let mut cues = Vec::<(u64, u64, String, String)>::new();
    for item in document
        .text_items
        .iter()
        .filter(|item| item.kind == TextKind::Caption)
    {
        if let Some(owner_clip_id) = item.owner_clip_id.as_deref() {
            let clip = document
                .clips
                .iter()
                .find(|clip| clip.id == owner_clip_id)
                .ok_or_else(|| AppError::schema("Caption owner clip is unavailable"))?;
            if let Some(projection) = item.project_on_clip(clip)? {
                cues.push((
                    projection.timeline_start_frame,
                    projection
                        .timeline_start_frame
                        .checked_add(projection.duration_frames)
                        .ok_or_else(|| AppError::schema("Caption interval overflows"))?,
                    item.id.clone(),
                    item.text.clone(),
                ));
            }
        } else if let Some(interval) = item.timeline_interval() {
            let interval = interval?;
            cues.push((
                interval.start_frame,
                interval.end_frame(),
                item.id.clone(),
                item.text.clone(),
            ));
        }
    }
    cues.sort_by(|left, right| (left.0, left.2.as_str()).cmp(&(right.0, right.2.as_str())));
    let fps = document.profile.fps();
    let mut output = String::new();
    for (index, (start, end, _, text)) in cues.iter().enumerate() {
        let start_ms = frame_time_ms(*start, fps, false)?;
        let end_ms = frame_time_ms(*end, fps, true)?;
        if end_ms <= start_ms {
            return Err(AppError::schema(
                "Caption interval is shorter than one millisecond",
            ));
        }
        output.push_str(&(index + 1).to_string());
        output.push('\n');
        output.push_str(&format_srt_timestamp(start_ms));
        output.push_str(" --> ");
        output.push_str(&format_srt_timestamp(end_ms));
        output.push('\n');
        output.push_str(&text.replace('\r', ""));
        output.push_str("\n\n");
    }
    Ok(output)
}

pub(super) fn frame_time_ms(frame: u64, fps: FrameRate, ceil: bool) -> Result<u64, AppError> {
    fps.validate()?;
    let numerator = (frame as u128)
        .checked_mul(fps.den as u128)
        .and_then(|value| value.checked_mul(1_000))
        .ok_or_else(|| AppError::invalid_argument("Caption time overflows"))?;
    let denominator = fps.num as u128;
    let value = if ceil {
        numerator
            .checked_add(denominator - 1)
            .and_then(|value| value.checked_div(denominator))
    } else {
        numerator.checked_div(denominator)
    }
    .ok_or_else(|| AppError::invalid_argument("Caption time overflows"))?;
    u64::try_from(value).map_err(|_| AppError::invalid_argument("Caption time exceeds safe range"))
}

fn format_srt_timestamp(milliseconds: u64) -> String {
    let hours = milliseconds / 3_600_000;
    let minutes = (milliseconds / 60_000) % 60;
    let seconds = (milliseconds / 1_000) % 60;
    let millis = milliseconds % 1_000;
    format!("{hours:02}:{minutes:02}:{seconds:02},{millis:03}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_time_uses_covering_end_rounding() {
        let fps = FrameRate::FPS_24;
        assert_eq!(frame_time_ms(1, fps, false).unwrap(), 41);
        assert_eq!(frame_time_ms(1, fps, true).unwrap(), 42);
    }
}
