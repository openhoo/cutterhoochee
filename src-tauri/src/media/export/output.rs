use crate::error::{AppError, ErrorCode};
use crate::media::render::RenderCapture;
use crate::media::render_plan::{sample_at_frame, RenderPlan};
use crate::permissions::ExportDestinationGrant;
use crate::project::model::{AspectRatio, FrameRate};
use std::collections::HashSet;
use std::fs::{self, File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};
use uuid::Uuid;

use super::{ExportTargetIdentity, MAX_EXPORT_RESOLUTION};

#[derive(Clone)]
pub(super) struct ExportDestination {
    pub(super) destination: PathBuf,
    pub(super) srt_destination: Option<PathBuf>,
    pub(super) destination_identity: ExportTargetIdentity,
    pub(super) srt_identity: Option<ExportTargetIdentity>,
}

pub(super) fn choose_destination(
    plan: &RenderPlan,
    srt: bool,
) -> Result<ExportDestination, AppError> {
    let _ = AspectRatio::from_dimensions(plan.width, plan.height)
        .ok_or_else(|| AppError::schema("The render plan has an unsupported aspect ratio"))?;
    let default_name = format!("export-{}x{}.mp4", plan.width, plan.height);
    let chosen = rfd::FileDialog::new()
        .set_title("Export Cutterhoochee video")
        .set_file_name(default_name)
        .add_filter("MP4 video", &["mp4"])
        .save_file()
        .ok_or_else(|| AppError::io("Export destination selection was cancelled"))?;
    let destination = normalize_destination(chosen)?;
    let srt_destination = srt.then(|| destination.with_extension("srt"));
    let destination_identity = target_identity(&destination)?;
    let srt_identity = srt_destination
        .as_deref()
        .map(target_identity)
        .transpose()?;
    Ok(ExportDestination {
        destination,
        srt_destination,
        destination_identity,
        srt_identity,
    })
}

pub(super) fn normalize_destination(mut path: PathBuf) -> Result<PathBuf, AppError> {
    if !path.is_absolute() {
        return Err(AppError::new(
            ErrorCode::PermissionDenied,
            "Export destinations must be absolute native paths",
        ));
    }
    let extension = path.extension().and_then(|value| value.to_str());
    match extension {
        None => {
            path.set_extension("mp4");
        }
        Some(value) if value.eq_ignore_ascii_case("mp4") => {}
        Some(_) => {
            return Err(AppError::invalid_argument(
                "Export destination must use the .mp4 extension",
            ));
        }
    }
    let file_name = path
        .file_name()
        .and_then(|value| value.to_str())
        .filter(|value| !value.is_empty() && !value.contains('\0') && !value.contains(['\r', '\n']))
        .ok_or_else(|| AppError::invalid_argument("Export destination filename is invalid"))?;
    let parent = path
        .parent()
        .ok_or_else(|| AppError::invalid_argument("Export destination has no parent directory"))?;
    let parent_metadata = fs::symlink_metadata(parent)
        .map_err(|_| AppError::io("Export destination directory is unavailable"))?;
    if parent_metadata.file_type().is_symlink() || !parent_metadata.is_dir() {
        return Err(AppError::new(
            ErrorCode::PermissionDenied,
            "Export destination directory must be a real directory",
        ));
    }
    let parent = fs::canonicalize(parent)
        .map_err(|_| AppError::io("Export destination directory could not be resolved"))?;
    Ok(parent.join(file_name))
}

pub(super) fn target_identity(path: &Path) -> Result<ExportTargetIdentity, AppError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() {
                return Err(AppError::new(
                    ErrorCode::PermissionDenied,
                    "Symlinked export destinations cannot be overwritten",
                ));
            }
            if !metadata.is_file() {
                return Err(AppError::invalid_argument(
                    "Export destination must be a regular file",
                ));
            }
            Ok(ExportTargetIdentity {
                exists: true,
                size: metadata.len(),
                token: metadata_token(&metadata),
            })
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(ExportTargetIdentity {
            exists: false,
            size: 0,
            token: "missing".to_owned(),
        }),
        Err(_) => Err(AppError::io("Export destination metadata is unavailable")),
    }
}

fn metadata_token(metadata: &fs::Metadata) -> String {
    #[cfg(unix)]
    {
        return format!(
            "unix:{}:{}:{}:{}:{}",
            metadata.dev(),
            metadata.ino(),
            metadata.len(),
            metadata.mtime(),
            metadata.mtime_nsec()
        );
    }
    #[cfg(not(unix))]
    {
        let modified = metadata
            .modified()
            .ok()
            .and_then(|value| value.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|value| value.as_nanos())
            .unwrap_or_default();
        format!("file:{}:{modified}", metadata.len())
    }
}

pub(super) fn verify_finalized_file(path: &Path) -> Result<(), AppError> {
    let metadata = fs::symlink_metadata(path).map_err(|_| {
        AppError::new(
            ErrorCode::AssetUnavailable,
            "The finalized export is unavailable",
        )
    })?;
    if metadata.file_type().is_symlink() || !metadata.is_file() || metadata.len() == 0 {
        return Err(AppError::new(
            ErrorCode::AssetUnavailable,
            "The finalized export is not a regular file",
        ));
    }
    Ok(())
}

pub(super) fn pin_master_artifacts(capture: &RenderCapture) -> Result<Vec<File>, AppError> {
    let mut ids = HashSet::new();
    for layer in &capture.plan.layers {
        for segment in &layer.segments {
            ids.insert(segment.artifact_id.clone());
        }
        for overlay in &layer.text_overlays {
            ids.insert(overlay.raster_artifact_id.clone());
        }
    }
    for segment in &capture.plan.audio.segments {
        ids.insert(segment.artifact_id.clone());
    }
    let mut pins = Vec::with_capacity(ids.len());
    for artifact_id in ids {
        let path = capture.artifacts.managed_path(&artifact_id)?;
        let file = File::open(path).map_err(|_| {
            AppError::new(
                ErrorCode::AssetUnavailable,
                "A referenced render artifact is unavailable",
            )
        })?;
        pins.push(file);
    }
    Ok(pins)
}

pub(super) fn commit_outputs(
    mp4_temp: &mut TemporaryFile,
    srt_temp: &mut Option<TemporaryFile>,
    mp4_grant: &ExportDestinationGrant,
    srt_grant: Option<&ExportDestinationGrant>,
    destination: &Path,
    srt_destination: Option<&Path>,
) -> Result<(), AppError> {
    if destination != mp4_grant.destination() {
        return Err(AppError::new(
            ErrorCode::PermissionDenied,
            "The MP4 destination no longer matches its approval",
        ));
    }
    if srt_temp.is_some() != srt_grant.is_some() || srt_temp.is_some() != srt_destination.is_some()
    {
        return Err(AppError::schema(
            "The SRT temporary output, destination, and approval are inconsistent",
        ));
    }
    if let (Some(path), Some(grant)) = (srt_destination, srt_grant) {
        if path != grant.destination() {
            return Err(AppError::new(
                ErrorCode::PermissionDenied,
                "The SRT destination no longer matches its approval",
            ));
        }
    }

    // Trust retains an app-owned backup while both exact grants are used. It
    // also represents an originally absent target, allowing rollback to
    // remove a newly installed file only when its identity is unchanged.
    let mut mp4_backup = mp4_grant.backup_existing()?;
    let mut srt_backup = match srt_grant {
        Some(grant) => Some(grant.backup_existing()?),
        None => None,
    };
    let mut mp4_outcome = None;
    let mut srt_outcome = None;
    let install_result = (|| {
        if let (Some(temp), Some(grant)) = (srt_temp.as_ref(), srt_grant) {
            srt_outcome = Some(grant.install_with_state(&temp.path)?);
        }
        mp4_outcome = Some(mp4_grant.install_with_state(&mp4_temp.path)?);
        Ok::<(), AppError>(())
    })();
    if let Err(error) = install_result {
        let mut rollback_error = None;
        if let Some(outcome) = srt_outcome.as_ref() {
            if let Err(error) = srt_backup
                .take()
                .ok_or_else(|| AppError::io("The SRT export backup is unavailable"))
                .and_then(|backup| backup.restore_if_unchanged(outcome))
            {
                rollback_error = Some(error);
            }
        }
        if let Some(outcome) = mp4_outcome.as_ref() {
            if let Err(error) = mp4_backup.restore_if_unchanged(outcome) {
                rollback_error = Some(error);
            }
        }
        if let Some(rollback_error) = rollback_error {
            return Err(AppError::io(format!(
                "Export installation failed and rollback is incomplete: {}; {}",
                error.message, rollback_error.message
            )));
        }
        if mp4_outcome.is_none() || (srt_grant.is_some() && srt_outcome.is_none()) {
            return Err(AppError::io(format!(
                "Export installation failed before every target state could be proven; recovery backup retained: {}",
                error.message
            )));
        }
        return Err(error);
    }
    mp4_backup.discard()?;
    if let Some(backup) = srt_backup.take() {
        backup.discard()?;
    }
    mp4_temp.keep = true;
    if let Some(temp) = srt_temp.as_mut() {
        temp.keep = true;
    }
    Ok(())
}

pub(super) fn verify_target_matches(
    path: &Path,
    expected: &ExportTargetIdentity,
    message: &str,
) -> Result<(), AppError> {
    let current = target_identity(path)?;
    if &current != expected {
        return Err(AppError::new(ErrorCode::PermissionDenied, message));
    }
    Ok(())
}

pub(super) struct TemporaryFile {
    pub(super) path: PathBuf,
    pub(super) file: File,
    pub(super) keep: bool,
}

impl TemporaryFile {
    pub(super) fn new(path: PathBuf) -> Result<Self, AppError> {
        let file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .read(true)
            .open(&path)
            .map_err(|_| AppError::io("The export temporary file could not be created"))?;
        Ok(Self {
            path,
            file,
            keep: false,
        })
    }
}

impl Drop for TemporaryFile {
    fn drop(&mut self) {
        if !self.keep {
            let _ = fs::remove_file(&self.path);
        }
    }
}

pub(super) fn temporary_path(
    directory: &Path,
    job_id: &str,
    extension: &str,
) -> Result<PathBuf, AppError> {
    if !directory.is_absolute() {
        return Err(AppError::new(
            ErrorCode::PermissionDenied,
            "Export temporary storage must be an absolute app-owned directory",
        ));
    }
    fs::create_dir_all(directory)?;
    if job_id.is_empty() || extension.is_empty() || extension.contains(['/', '\\']) {
        return Err(AppError::invalid_argument(
            "The export temporary name is invalid",
        ));
    }
    Ok(directory.join(format!(".cutterhoochee-export-{job_id}.{extension}")))
}

pub(super) fn temporary_sibling(destination: &Path, extension: &str) -> Result<PathBuf, AppError> {
    let parent = destination
        .parent()
        .ok_or_else(|| AppError::io("Export destination has no parent"))?;
    let stem = destination
        .file_name()
        .and_then(|value| value.to_str())
        .ok_or_else(|| AppError::io("Export destination filename is not UTF-8"))?;
    let nonce = Uuid::new_v4().simple().to_string();
    Ok(parent.join(format!(".{stem}.cutterhoochee-{nonce}.{extension}")))
}

pub(super) fn output_dimensions(
    width: u32,
    height: u32,
    resolution: u16,
) -> Result<(u32, u32), AppError> {
    validate_resolution(resolution)?;
    let aspect = AspectRatio::from_dimensions(width, height)
        .ok_or_else(|| AppError::schema("The render plan aspect ratio is unsupported"))?;
    let value = u32::from(resolution);
    Ok(match aspect {
        AspectRatio::Landscape => (value * 16 / 9, value),
        AspectRatio::Portrait => (value, value * 16 / 9),
        AspectRatio::Square => (value, value),
    })
}

pub(super) fn validate_resolution(value: u16) -> Result<(), AppError> {
    if matches!(value, 720 | MAX_EXPORT_RESOLUTION) {
        Ok(())
    } else {
        Err(AppError::invalid_argument(
            "Export resolution must be 720 or 1080",
        ))
    }
}

pub(super) fn duration_ms(frames: u64, fps: FrameRate) -> Result<u64, AppError> {
    fps.validate()?;
    let samples = sample_at_frame(frames, fps)?;
    samples
        .checked_mul(1_000)
        .and_then(|value| value.checked_div(crate::project::model::AUDIO_SAMPLE_RATE as u64))
        .ok_or_else(|| AppError::invalid_argument("Export duration overflows"))
}

#[cfg(unix)]
use std::os::unix::fs::MetadataExt;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_only_supported_output_heights() {
        assert!(validate_resolution(720).is_ok());
        assert!(validate_resolution(1080).is_ok());
        assert_eq!(
            validate_resolution(2160).unwrap_err().code,
            ErrorCode::InvalidArgument
        );
    }

    #[test]
    fn output_dimensions_follow_project_aspect() {
        assert_eq!(output_dimensions(1920, 1080, 720).unwrap(), (1280, 720));
        assert_eq!(output_dimensions(1080, 1920, 1080).unwrap(), (1080, 1920));
        assert_eq!(output_dimensions(1080, 1080, 720).unwrap(), (720, 720));
    }

    #[test]
    fn temporary_sibling_is_hidden_and_unique() {
        let destination = Path::new("/tmp/video.mp4");
        let first = temporary_sibling(destination, "mp4").unwrap();
        let second = temporary_sibling(destination, "mp4").unwrap();
        assert_ne!(first, second);
        assert!(first
            .file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with(".video.mp4.cutterhoochee-"));
    }
}
