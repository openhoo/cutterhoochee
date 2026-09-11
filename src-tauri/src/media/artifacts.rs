use crate::error::AppError;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use ts_rs::TS;
use uuid::Uuid;

const MAX_CACHE_KEY_BYTES: usize = 4096;
const MAX_ARTIFACT_BYTES: u64 = 8 * 1024 * 1024 * 1024;

fn invalid(message: impl Into<String>) -> AppError {
    AppError::invalid_argument(message)
}

/// Kinds are deliberately finite: callers cannot turn an artifact ID into an
/// arbitrary path or ask the asset protocol to expose an unowned file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
#[ts(rename_all = "snake_case")]
pub enum ArtifactKind {
    MasterVideo,
    ProxyVideo,
    PcmAudio,
    StillImage,
    Thumbnail,
    Waveform,
    Frame,
    Transcript,
    Other,
}

impl ArtifactKind {
    pub fn directory(self) -> &'static str {
        match self {
            Self::MasterVideo => "masters",
            Self::ProxyVideo => "proxies",
            Self::PcmAudio => "pcm",
            Self::StillImage => "images",
            Self::Thumbnail => "thumbnails",
            Self::Waveform => "waveforms",
            Self::Frame => "frames",
            Self::Transcript => "transcripts",
            Self::Other => "other",
        }
    }

    pub fn content_type(self, extension: &str) -> &'static str {
        match extension.to_ascii_lowercase().as_str() {
            "mp4" | "m4v" => "video/mp4",
            "png" => "image/png",
            "jpg" | "jpeg" => "image/jpeg",
            "wav" => "audio/wav",
            "f32" | "f32le" => "application/octet-stream",
            "json" => "application/json",
            _ => match self {
                Self::PcmAudio => "application/octet-stream",
                Self::Waveform => "application/octet-stream",
                _ => "application/octet-stream",
            },
        }
    }
}

/// The portable identity of one app-owned artifact. `path` is intentionally
/// omitted from the IPC shape; native renderers use `ArtifactStore::managed_path`
/// after revalidating containment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase")]
pub struct ArtifactRecord {
    pub artifact_id: String,
    pub cache_key: String,
    pub kind: ArtifactKind,
    pub content_type: String,
    #[ts(type = "SafeInteger")]
    pub byte_size: u64,
}

impl ArtifactRecord {
    pub fn validate(&self) -> Result<(), AppError> {
        validate_artifact_id(&self.artifact_id)?;
        validate_cache_key(&self.cache_key)?;
        if self.content_type.is_empty() || self.content_type.len() > 128 {
            return Err(invalid("Artifact content type is invalid"));
        }
        if self.byte_size > MAX_ARTIFACT_BYTES {
            return Err(invalid("Artifact is larger than the supported limit"));
        }
        Ok(())
    }
}

/// A content-addressed artifact directory with workspace-scoped authority.
/// Every operation rechecks symlink-free containment, even when a project
/// store has already canonicalized its root: project folders may be replaced by
/// another process between two native calls.
#[derive(Debug, Clone)]
pub struct ArtifactStore {
    root: PathBuf,
    workspace_root: PathBuf,
    workspace_id: String,
    font_resource_dir: Option<PathBuf>,
    font_catalog: Arc<OnceLock<Result<Arc<crate::media::graphics::BundledFontCatalog>, AppError>>>,
}

impl ArtifactStore {
    pub fn new(root: PathBuf, workspace_id: impl Into<String>) -> Result<Self, AppError> {
        let workspace_id = workspace_id.into();
        validate_component(&workspace_id, "workspaceId")?;
        let root = ensure_directory_tree(&root)?;
        let workspace_root = root.join(&workspace_id);
        ensure_directory_tree(&workspace_root)?;
        let store = Self {
            root,
            workspace_root,
            workspace_id,
            font_resource_dir: None,
            font_catalog: Arc::new(OnceLock::new()),
        };
        store.ensure_layout()?;
        Ok(store)
    }

    /// Project bytes are portable; the installation's workspace ID is authority,
    /// not an on-disk media namespace.
    pub fn for_project(
        project_root: &Path,
        workspace_id: impl Into<String>,
    ) -> Result<Self, AppError> {
        static MIGRATION_LOCK: Mutex<()> = Mutex::new(());
        let _migration = MIGRATION_LOCK
            .lock()
            .map_err(|_| AppError::io("Project media migration is unavailable"))?;
        let workspace_id = workspace_id.into();
        validate_component(&workspace_id, "workspaceId")?;
        let root = ensure_directory_tree(&project_root.join("media"))?;
        let store = Self {
            workspace_root: root.clone(),
            root,
            workspace_id,
            font_resource_dir: None,
            font_catalog: Arc::new(OnceLock::new()),
        };
        store.ensure_layout()?;
        migrate_project_media(&store.workspace_root)?;
        Ok(store)
    }

    /// Bind this store's native renderer to the trusted, app-bundled font
    /// resources.  The portable project never supplies this directory.
    pub fn with_font_resource_dir(&self, resource_dir: &Path) -> Result<Self, AppError> {
        if !resource_dir.is_absolute() {
            return Err(AppError::invalid_argument(
                "The bundled font resource directory must be absolute",
            ));
        }
        let mut configured = self.clone();
        configured.font_resource_dir = Some(resource_dir.to_path_buf());
        configured.font_catalog = Arc::new(OnceLock::new());
        Ok(configured)
    }

    pub(crate) fn bundled_font_catalog(
        &self,
    ) -> Result<Arc<crate::media::graphics::BundledFontCatalog>, AppError> {
        let resource_dir = self.font_resource_dir.as_deref().ok_or_else(|| {
            AppError::new(
                crate::error::ErrorCode::MediaUnsupported,
                "The bundled Inter font resources are not configured",
            )
        })?;
        self.font_catalog
            .get_or_init(|| crate::media::graphics::load_bundled_font_catalog(resource_dir))
            .clone()
    }

    pub fn workspace_id(&self) -> &str {
        &self.workspace_id
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn workspace_root(&self) -> Result<PathBuf, AppError> {
        self.verify_workspace_root()
    }

    pub fn ensure_layout(&self) -> Result<(), AppError> {
        self.verify_workspace_root()?;
        for kind in [
            ArtifactKind::MasterVideo,
            ArtifactKind::ProxyVideo,
            ArtifactKind::PcmAudio,
            ArtifactKind::StillImage,
            ArtifactKind::Thumbnail,
            ArtifactKind::Waveform,
            ArtifactKind::Frame,
            ArtifactKind::Transcript,
            ArtifactKind::Other,
        ] {
            let dir = self.workspace_root.join(kind.directory());
            ensure_directory_tree(&dir)?;
        }
        Ok(())
    }

    pub fn put_bytes(
        &self,
        cache_key: &str,
        extension: &str,
        kind: ArtifactKind,
        bytes: &[u8],
    ) -> Result<ArtifactRecord, AppError> {
        validate_cache_key(cache_key)?;
        validate_extension(extension)?;
        if bytes.len() as u64 > MAX_ARTIFACT_BYTES {
            return Err(invalid("Artifact is larger than the supported limit"));
        }
        let record = self.record_for(cache_key, extension, kind, bytes.len() as u64)?;
        let destination = self.path_for(&record)?;
        if destination.is_file() {
            verify_regular_file(&destination)?;
            let existing = fs::metadata(&destination)?.len();
            if existing == bytes.len() as u64 {
                return Ok(record);
            }
            return Err(AppError::io(
                "A content-addressed artifact has an unexpected size",
            ));
        }
        self.atomic_write(&destination, bytes)?;
        Ok(record)
    }

    pub fn put_file(
        &self,
        cache_key: &str,
        extension: &str,
        kind: ArtifactKind,
        source: &Path,
    ) -> Result<ArtifactRecord, AppError> {
        validate_cache_key(cache_key)?;
        validate_extension(extension)?;
        let identity = verify_regular_file(source)?;
        if identity.len > MAX_ARTIFACT_BYTES {
            return Err(invalid("Artifact is larger than the supported limit"));
        }
        let record = self.record_for(cache_key, extension, kind, identity.len)?;
        let destination = self.path_for(&record)?;
        if destination.is_file() {
            verify_regular_file(&destination)?;
            if fs::metadata(&destination)?.len() == identity.len {
                return Ok(record);
            }
            return Err(AppError::io(
                "A content-addressed artifact has an unexpected size",
            ));
        }
        let mut input = File::open(source)?;
        let (temp, mut output) = self.atomic_destination(&destination)?;
        if let Err(error) = std::io::copy(&mut input, &mut output).and_then(|_| {
            output.sync_all()?;
            Ok(())
        }) {
            let _ = fs::remove_file(&temp);
            return Err(error.into());
        }
        drop(output);
        if let Err(error) = fs::rename(&temp, &destination) {
            let _ = fs::remove_file(&temp);
            return Err(error.into());
        }
        sync_directory(destination.parent().expect("artifact parent"))?;
        Ok(record)
    }

    pub fn record_for(
        &self,
        cache_key: &str,
        extension: &str,
        kind: ArtifactKind,
        byte_size: u64,
    ) -> Result<ArtifactRecord, AppError> {
        validate_cache_key(cache_key)?;
        validate_extension(extension)?;
        let digest = digest_hex(cache_key.as_bytes());
        let extension = extension.trim_start_matches('.').to_ascii_lowercase();
        let artifact_id = format!("{digest}.{extension}");
        let record = ArtifactRecord {
            artifact_id,
            cache_key: cache_key.to_owned(),
            kind,
            content_type: kind.content_type(&extension).to_owned(),
            byte_size,
        };
        record.validate()?;
        Ok(record)
    }

    pub fn managed_path(&self, artifact_id: &str) -> Result<PathBuf, AppError> {
        let record = ArtifactRecord {
            artifact_id: artifact_id.to_owned(),
            cache_key: "managed-path".to_owned(),
            kind: ArtifactKind::Other,
            content_type: "application/octet-stream".to_owned(),
            byte_size: 0,
        };
        validate_artifact_id(&record.artifact_id)?;
        let extension = artifact_id
            .rsplit_once('.')
            .map(|(_, ext)| ext)
            .ok_or_else(|| invalid("Artifact ID must include an extension"))?;
        let kind = self.kind_for_existing(artifact_id).ok_or_else(|| {
            AppError::new(
                crate::error::ErrorCode::AssetUnavailable,
                "Managed artifact is unavailable",
            )
        })?;
        let path = self.workspace_root.join(kind.directory()).join(artifact_id);
        self.verify_managed_path(&path, kind, extension)?;
        Ok(path)
    }

    pub fn open(&self, artifact_id: &str) -> Result<File, AppError> {
        let path = self.managed_path(artifact_id)?;
        verify_regular_file(&path)?;
        OpenOptions::new().read(true).open(path).map_err(|_| {
            AppError::new(
                crate::error::ErrorCode::AssetUnavailable,
                "Managed artifact is unavailable",
            )
        })
    }

    pub fn read_bytes(&self, artifact_id: &str, max_bytes: usize) -> Result<Vec<u8>, AppError> {
        let mut file = self.open(artifact_id)?;
        let size = file.metadata()?.len();
        if size > max_bytes as u64 {
            return Err(invalid("Managed artifact exceeds the requested read limit"));
        }
        let mut bytes = Vec::with_capacity(size as usize);
        file.read_to_end(&mut bytes)?;
        Ok(bytes)
    }
    /// Read one bounded byte range from a managed artifact. Callers identify
    /// only the artifact ID; the store revalidates canonical containment.
    pub fn range(&self, artifact_id: &str, start: u64, len: usize) -> Result<Vec<u8>, AppError> {
        const MAX_RANGE_BYTES: usize = 8 * 1024 * 1024;
        if len > MAX_RANGE_BYTES {
            return Err(invalid(
                "Managed artifact range exceeds the supported limit",
            ));
        }
        let mut file = self.open(artifact_id)?;
        let size = file.metadata()?.len();
        if start > size {
            return Err(invalid(
                "Managed artifact range starts past the end of the file",
            ));
        }
        file.seek(SeekFrom::Start(start))?;
        let available = size.saturating_sub(start).min(len as u64) as usize;
        let mut bytes = vec![0u8; available];
        file.read_exact(&mut bytes)?;
        Ok(bytes)
    }

    /// Return an opaque, revocable native artifact URI. The protocol handler
    /// validates workspace, generation, and artifact identity before serving
    /// bounded bytes; no filesystem path crosses the WebView boundary.
    pub fn url(&self, artifact_id: &str, generation: u64) -> Result<String, AppError> {
        self.managed_path(artifact_id)?;
        if generation > 9_007_199_254_740_991 {
            return Err(invalid("Artifact generation exceeds the supported range"));
        }
        Ok(format!(
            "artifact://localhost/{}/{}/{}",
            self.workspace_id, generation, artifact_id
        ))
    }

    pub fn remove(&self, artifact_id: &str) -> Result<(), AppError> {
        let path = self.managed_path(artifact_id)?;
        fs::remove_file(path)?;
        Ok(())
    }

    pub fn contains(&self, artifact_id: &str) -> bool {
        self.managed_path(artifact_id)
            .map(|path| path.is_file())
            .unwrap_or(false)
    }

    fn kind_for_existing(&self, artifact_id: &str) -> Option<ArtifactKind> {
        [
            ArtifactKind::MasterVideo,
            ArtifactKind::ProxyVideo,
            ArtifactKind::PcmAudio,
            ArtifactKind::StillImage,
            ArtifactKind::Thumbnail,
            ArtifactKind::Waveform,
            ArtifactKind::Frame,
            ArtifactKind::Transcript,
            ArtifactKind::Other,
        ]
        .into_iter()
        .find(|kind| {
            let path = self.workspace_root.join(kind.directory()).join(artifact_id);
            path.is_file() && verify_managed_path_inner(&path, &self.workspace_root, *kind).is_ok()
        })
    }

    fn path_for(&self, record: &ArtifactRecord) -> Result<PathBuf, AppError> {
        let path = self
            .workspace_root
            .join(record.kind.directory())
            .join(&record.artifact_id);
        self.verify_managed_path(
            &path,
            record.kind,
            record
                .artifact_id
                .rsplit_once('.')
                .map(|(_, ext)| ext)
                .unwrap_or(""),
        )?;
        Ok(path)
    }

    fn verify_workspace_root(&self) -> Result<PathBuf, AppError> {
        verify_directory(&self.root)?;
        verify_directory_containment(&self.workspace_root, &self.root)?;
        Ok(self.workspace_root.clone())
    }

    fn verify_managed_path(
        &self,
        path: &Path,
        kind: ArtifactKind,
        extension: &str,
    ) -> Result<(), AppError> {
        self.verify_workspace_root()?;
        validate_extension(extension)?;
        verify_managed_path_inner(path, &self.workspace_root, kind)
    }

    pub fn staging_path(&self, kind: ArtifactKind, extension: &str) -> Result<PathBuf, AppError> {
        validate_extension(extension)?;
        self.verify_workspace_root()?;
        let directory = self.workspace_root.join(kind.directory());
        verify_directory_containment(&directory, &self.workspace_root)?;
        let path = directory.join(format!(
            ".staging-{}.{}",
            Uuid::new_v4(),
            extension.trim_start_matches('.')
        ));
        verify_managed_temp_path(&path, &self.workspace_root, kind)?;
        Ok(path)
    }

    fn temp_path(&self, destination: &Path) -> PathBuf {
        destination.with_file_name(format!(
            ".{}.{}.tmp",
            destination.file_name().unwrap().to_string_lossy(),
            Uuid::new_v4()
        ))
    }

    fn atomic_destination(&self, destination: &Path) -> Result<(PathBuf, File), AppError> {
        let temp = self.temp_path(destination);
        let kind = self
            .kind_for_directory(destination.parent())
            .unwrap_or(ArtifactKind::Other);
        verify_managed_temp_path(&temp, &self.workspace_root, kind)?;
        let file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temp)
            .map_err(AppError::from)?;
        Ok((temp, file))
    }

    fn atomic_write(&self, destination: &Path, bytes: &[u8]) -> Result<(), AppError> {
        let temp = self.temp_path(destination);
        let kind = self
            .kind_for_directory(destination.parent())
            .unwrap_or(ArtifactKind::Other);
        verify_managed_temp_path(&temp, &self.workspace_root, kind)?;
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temp)?;
        if let Err(error) = file.write_all(bytes).and_then(|_| file.sync_all()) {
            let _ = fs::remove_file(&temp);
            return Err(error.into());
        }
        drop(file);
        if let Err(error) = fs::rename(&temp, destination) {
            let _ = fs::remove_file(&temp);
            return Err(error.into());
        }
        sync_directory(destination.parent().expect("artifact parent"))?;
        Ok(())
    }

    fn kind_for_directory(&self, directory: Option<&Path>) -> Option<ArtifactKind> {
        let directory = directory?;
        [
            ArtifactKind::MasterVideo,
            ArtifactKind::ProxyVideo,
            ArtifactKind::PcmAudio,
            ArtifactKind::StillImage,
            ArtifactKind::Thumbnail,
            ArtifactKind::Waveform,
            ArtifactKind::Frame,
            ArtifactKind::Transcript,
            ArtifactKind::Other,
        ]
        .into_iter()
        .find(|kind| directory == self.workspace_root.join(kind.directory()))
    }
}

/// Old namespaces remain recovery copies, never read fallbacks. Publication is
/// no-clobber and a completion marker avoids re-reading large legacy media.
fn migrate_project_media(root: &Path) -> Result<(), AppError> {
    const MARKER: &str = ".portable-layout-v1";
    let marker = root.join(MARKER);
    if fs::symlink_metadata(&marker).is_ok() {
        verify_regular_file(&marker)?;
        return Ok(());
    }
    let mut namespaces = Vec::new();
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        if entry
            .file_name()
            .to_str()
            .is_some_and(|name| Uuid::parse_str(name).is_ok())
        {
            verify_directory(&entry.path())?;
            namespaces.push(entry.file_name());
        }
    }
    if namespaces.is_empty() {
        return Ok(());
    }
    #[cfg(not(unix))]
    return Err(AppError::io(
        "Safe legacy project media migration is unavailable on this platform",
    ));
    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd;
        use std::os::unix::fs::OpenOptionsExt;
        let root_fd = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(root)?;
        for namespace in namespaces {
            let namespace_fd = migration_open_at(&root_fd, &namespace, true, false)?;
            for kind in [
                ArtifactKind::MasterVideo,
                ArtifactKind::ProxyVideo,
                ArtifactKind::PcmAudio,
                ArtifactKind::StillImage,
                ArtifactKind::Thumbnail,
                ArtifactKind::Waveform,
                ArtifactKind::Frame,
                ArtifactKind::Transcript,
                ArtifactKind::Other,
            ] {
                let directory = root.join(&namespace).join(kind.directory());
                if fs::symlink_metadata(&directory)
                    .is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound)
                {
                    continue;
                }
                verify_directory(&directory)?;
                let source_directory =
                    migration_open_at(&namespace_fd, kind.directory().as_ref(), true, false)?;
                let destination_directory =
                    migration_open_at(&root_fd, kind.directory().as_ref(), true, false)?;
                for entry in fs::read_dir(&directory)? {
                    let entry = entry?;
                    let name = entry.file_name();
                    let name_str = name
                        .to_str()
                        .ok_or_else(|| invalid("Legacy media filename is invalid"))?;
                    if name_str.starts_with('.') {
                        continue;
                    }
                    validate_artifact_id(name_str)?;
                    let mut source = migration_open_at(&source_directory, &name, false, false)?;
                    let name_c = std::ffi::CString::new(name_str)
                        .map_err(|_| invalid("Legacy media filename is invalid"))?;
                    let linked = unsafe {
                        libc::linkat(
                            source_directory.as_raw_fd(),
                            name_c.as_ptr(),
                            destination_directory.as_raw_fd(),
                            name_c.as_ptr(),
                            0,
                        )
                    };
                    if linked != 0 {
                        let error = std::io::Error::last_os_error();
                        if error.kind() != std::io::ErrorKind::AlreadyExists {
                            return Err(error.into());
                        }
                    }
                    let mut destination =
                        migration_open_at(&destination_directory, &name, false, false)?;
                    if !migration_files_equal(&mut source, &mut destination)? {
                        return Err(AppError::io(
                            "Legacy project media conflicts with an existing portable artifact",
                        ));
                    }
                }
                destination_directory.sync_all()?;
            }
        }
        let marker_file = migration_open_at(&root_fd, MARKER.as_ref(), false, true)?;
        marker_file.sync_all()?;
        root_fd.sync_all()?;
        Ok(())
    }
}

#[cfg(unix)]
fn migration_open_at(
    parent: &File,
    name: &std::ffi::OsStr,
    directory: bool,
    create: bool,
) -> Result<File, AppError> {
    use std::os::fd::{AsRawFd, FromRawFd};
    use std::os::unix::ffi::OsStrExt;
    let name = std::ffi::CString::new(name.as_bytes())
        .map_err(|_| invalid("Legacy media filename is invalid"))?;
    let mut flags = libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK;
    flags |= if create {
        libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL
    } else {
        libc::O_RDONLY
    };
    if directory {
        flags |= libc::O_DIRECTORY;
    }
    let fd = unsafe { libc::openat(parent.as_raw_fd(), name.as_ptr(), flags, 0o600) };
    if fd < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let file = unsafe { File::from_raw_fd(fd) };
    let metadata = file.metadata()?;
    if (directory && !metadata.is_dir())
        || (!directory && (!metadata.is_file() || metadata.len() > MAX_ARTIFACT_BYTES))
    {
        return Err(AppError::permission_denied(
            "Legacy media must be a regular managed file or directory",
        ));
    }
    Ok(file)
}

#[cfg(unix)]
fn migration_files_equal(left: &mut File, right: &mut File) -> Result<bool, AppError> {
    use std::os::unix::fs::MetadataExt;
    let left_metadata = left.metadata()?;
    let right_metadata = right.metadata()?;
    if left_metadata.dev() == right_metadata.dev() && left_metadata.ino() == right_metadata.ino() {
        return Ok(true);
    }
    if left_metadata.len() != right_metadata.len() {
        return Ok(false);
    }
    let mut left_bytes = [0u8; 64 * 1024];
    let mut right_bytes = [0u8; 64 * 1024];
    loop {
        let count = left.read(&mut left_bytes)?;
        if count == 0 {
            return Ok(true);
        }
        right.read_exact(&mut right_bytes[..count])?;
        if left_bytes[..count] != right_bytes[..count] {
            return Ok(false);
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct FileIdentity {
    len: u64,
}

fn validate_component(value: &str, field: &str) -> Result<(), AppError> {
    if value.is_empty()
        || value == "."
        || value == ".."
        || value.contains('/')
        || value.contains('\\')
        || value.contains('\0')
        || value.contains('\r')
        || value.contains('\n')
    {
        return Err(invalid(format!("{field} is invalid")));
    }
    Ok(())
}

fn validate_cache_key(value: &str) -> Result<(), AppError> {
    if value.is_empty()
        || value.len() > MAX_CACHE_KEY_BYTES
        || value.contains('\0')
        || value.contains('\r')
        || value.contains('\n')
    {
        return Err(invalid("Artifact cache key is invalid"));
    }
    Ok(())
}

fn validate_extension(value: &str) -> Result<(), AppError> {
    let value = value.trim_start_matches('.');
    if value.is_empty()
        || value.len() > 16
        || !value.bytes().all(|byte| byte.is_ascii_alphanumeric())
    {
        return Err(invalid("Artifact extension is invalid"));
    }
    Ok(())
}

fn validate_artifact_id(value: &str) -> Result<(), AppError> {
    if value.is_empty()
        || value.len() > 256
        || value.contains('/')
        || value.contains('\\')
        || value.contains('\0')
        || value.contains("..")
    {
        return Err(invalid("Artifact ID is invalid"));
    }
    let (digest, extension) = value
        .rsplit_once('.')
        .ok_or_else(|| invalid("Artifact ID must include an extension"))?;
    if digest.len() != 64 || !digest.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(invalid("Artifact ID digest is invalid"));
    }
    validate_extension(extension)
}

fn digest_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

fn ensure_directory_tree(path: &Path) -> Result<PathBuf, AppError> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    let mut current = PathBuf::new();
    for component in absolute.components() {
        match component {
            Component::Prefix(prefix) => current.push(prefix.as_os_str()),
            Component::RootDir => current.push(std::path::MAIN_SEPARATOR.to_string()),
            Component::CurDir => {}
            Component::ParentDir => {
                return Err(invalid(
                    "Managed artifact root cannot contain parent traversal",
                ))
            }
            Component::Normal(name) => {
                current.push(name);
                match fs::symlink_metadata(&current) {
                    Ok(metadata) => {
                        if metadata.file_type().is_symlink() || !metadata.is_dir() {
                            return Err(AppError::permission_denied(
                                "Managed artifact roots must not contain symlinks or files",
                            ));
                        }
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                        fs::create_dir(&current)?
                    }
                    Err(error) => return Err(error.into()),
                }
            }
        }
    }
    verify_directory(&absolute)?;
    fs::canonicalize(&absolute).map_err(|_| {
        AppError::permission_denied("Managed artifact root could not be canonicalized")
    })
}

fn verify_directory(path: &Path) -> Result<(), AppError> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|_| AppError::permission_denied("Managed artifact directory is unavailable"))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(AppError::permission_denied(
            "Managed artifact directory is not a real directory",
        ));
    }
    Ok(())
}

fn verify_directory_containment(path: &Path, root: &Path) -> Result<(), AppError> {
    verify_directory(path)?;
    let canonical_root = fs::canonicalize(root)
        .map_err(|_| AppError::permission_denied("Managed artifact root is unavailable"))?;
    let canonical_path = fs::canonicalize(path)
        .map_err(|_| AppError::permission_denied("Managed artifact directory is unavailable"))?;
    if !canonical_path.starts_with(&canonical_root) {
        return Err(AppError::permission_denied(
            "Managed artifact path escapes its workspace",
        ));
    }
    Ok(())
}

fn verify_managed_path_inner(
    path: &Path,
    workspace_root: &Path,
    kind: ArtifactKind,
) -> Result<(), AppError> {
    let directory = workspace_root.join(kind.directory());
    verify_directory_containment(&directory, workspace_root)?;
    let file_name = path
        .file_name()
        .ok_or_else(|| invalid("Managed artifact path has no file name"))?;
    validate_artifact_id(&file_name.to_string_lossy())?;
    if path.parent() != Some(directory.as_path()) {
        return Err(AppError::permission_denied(
            "Managed artifact path is outside its kind directory",
        ));
    }
    if let Ok(metadata) = fs::symlink_metadata(path) {
        if metadata.file_type().is_symlink() || (!metadata.is_file() && !metadata.is_dir()) {
            return Err(AppError::permission_denied(
                "Managed artifact path must not be a symlink",
            ));
        }
    }
    if let Some(parent) = path.parent() {
        let canonical_parent = fs::canonicalize(parent)
            .map_err(|_| AppError::permission_denied("Managed artifact parent is unavailable"))?;
        let canonical_workspace = fs::canonicalize(workspace_root).map_err(|_| {
            AppError::permission_denied("Managed artifact workspace is unavailable")
        })?;
        if !canonical_parent.starts_with(&canonical_workspace) {
            return Err(AppError::permission_denied(
                "Managed artifact path escapes its workspace",
            ));
        }
    }
    Ok(())
}

fn verify_managed_temp_path(
    path: &Path,
    workspace_root: &Path,
    kind: ArtifactKind,
) -> Result<(), AppError> {
    let directory = workspace_root.join(kind.directory());
    verify_directory_containment(&directory, workspace_root)?;
    let file_name = path
        .file_name()
        .and_then(|value| value.to_str())
        .ok_or_else(|| invalid("Managed temporary path has no file name"))?;
    if !file_name.starts_with('.')
        || file_name.len() > 320
        || file_name.contains('/')
        || file_name.contains('\\')
        || file_name.contains('\0')
    {
        return Err(invalid("Managed temporary artifact path is invalid"));
    }
    if path.parent() != Some(directory.as_path()) {
        return Err(AppError::permission_denied(
            "Managed temporary path is outside its kind directory",
        ));
    }
    Ok(())
}

fn verify_regular_file(path: &Path) -> Result<FileIdentity, AppError> {
    let metadata = fs::symlink_metadata(path).map_err(|_| {
        AppError::new(
            crate::error::ErrorCode::AssetUnavailable,
            "Managed media file is unavailable",
        )
    })?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(AppError::permission_denied(
            "Managed media files must not be symlinks",
        ));
    }
    Ok(FileIdentity {
        len: metadata.len(),
    })
}

fn sync_directory(path: &Path) -> Result<(), AppError> {
    File::open(path)?.sync_all()?;
    Ok(())
}

impl AppError {
    fn permission_denied(message: impl Into<String>) -> Self {
        Self::new(crate::error::ErrorCode::PermissionDenied, message)
    }
}

#[cfg(all(test, unix))]
mod portability_tests {
    use super::*;
    use std::os::unix::fs::symlink;

    struct Fixture(PathBuf);

    impl Fixture {
        fn new() -> Self {
            Self(std::env::temp_dir().join(format!(
                "cutterhoochee-media-portability-{}",
                Uuid::new_v4()
            )))
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn project_media_survives_new_authority_without_reusing_old_urls() {
        let fixture = Fixture::new();
        let first = ArtifactStore::for_project(&fixture.0, Uuid::new_v4().to_string()).unwrap();
        let artifact = first
            .put_bytes(
                "portable-master",
                "mp4",
                ArtifactKind::MasterVideo,
                b"owned media",
            )
            .unwrap();
        let old_url = first.url(&artifact.artifact_id, 1).unwrap();
        let second = ArtifactStore::for_project(&fixture.0, Uuid::new_v4().to_string()).unwrap();
        assert_eq!(
            second.read_bytes(&artifact.artifact_id, 64).unwrap(),
            b"owned media"
        );
        assert_eq!(
            second.managed_path(&artifact.artifact_id).unwrap(),
            fixture.0.join("media/masters").join(&artifact.artifact_id)
        );
        assert_ne!(old_url, second.url(&artifact.artifact_id, 1).unwrap());
    }

    #[test]
    fn legacy_media_migration_is_idempotent_and_preserves_recovery_bytes() {
        let fixture = Fixture::new();
        let legacy =
            ArtifactStore::new(fixture.0.join("media"), Uuid::new_v4().to_string()).unwrap();
        let artifact = legacy
            .put_bytes(
                "legacy-pcm",
                "f32le",
                ArtifactKind::PcmAudio,
                b"original pcm",
            )
            .unwrap();
        let legacy_path = legacy.managed_path(&artifact.artifact_id).unwrap();
        for _ in 0..2 {
            let portable =
                ArtifactStore::for_project(&fixture.0, Uuid::new_v4().to_string()).unwrap();
            assert_eq!(
                portable.read_bytes(&artifact.artifact_id, 64).unwrap(),
                b"original pcm"
            );
            assert_eq!(fs::read(&legacy_path).unwrap(), b"original pcm");
        }
    }

    #[test]
    fn legacy_migration_rejects_equal_length_conflicting_bytes_without_clobber() {
        let fixture = Fixture::new();
        let portable = ArtifactStore::for_project(&fixture.0, Uuid::new_v4().to_string()).unwrap();
        let existing = portable
            .put_bytes("same-key", "bin", ArtifactKind::Other, b"keep")
            .unwrap();
        let existing_path = portable.managed_path(&existing.artifact_id).unwrap();
        let legacy =
            ArtifactStore::new(fixture.0.join("media"), Uuid::new_v4().to_string()).unwrap();
        legacy
            .put_bytes("same-key", "bin", ArtifactKind::Other, b"deny")
            .unwrap();
        assert!(ArtifactStore::for_project(&fixture.0, Uuid::new_v4().to_string()).is_err());
        assert_eq!(fs::read(existing_path).unwrap(), b"keep");
        assert_eq!(
            legacy.read_bytes(&existing.artifact_id, 64).unwrap(),
            b"deny"
        );
        assert!(!fixture.0.join("media/.portable-layout-v1").exists());
    }

    #[test]
    fn copied_project_requires_its_own_media_bytes() {
        let original = Fixture::new();
        let copy = Fixture::new();
        let source = ArtifactStore::for_project(&original.0, Uuid::new_v4().to_string()).unwrap();
        let artifact = source
            .put_bytes(
                "copy-master",
                "mp4",
                ArtifactKind::MasterVideo,
                b"copied bytes",
            )
            .unwrap();
        let destination = ArtifactStore::for_project(&copy.0, Uuid::new_v4().to_string()).unwrap();
        assert!(!destination.contains(&artifact.artifact_id));
        fs::copy(
            source.managed_path(&artifact.artifact_id).unwrap(),
            copy.0.join("media/masters").join(&artifact.artifact_id),
        )
        .unwrap();
        assert_eq!(
            destination.read_bytes(&artifact.artifact_id, 64).unwrap(),
            b"copied bytes"
        );
    }

    #[test]
    fn legacy_migration_rejects_symlinked_namespaces_and_artifacts() {
        let fixture = Fixture::new();
        let outside = Fixture::new();
        fs::create_dir_all(fixture.0.join("media")).unwrap();
        fs::create_dir_all(&outside.0).unwrap();
        let namespace = fixture.0.join("media").join(Uuid::new_v4().to_string());
        symlink(&outside.0, &namespace).unwrap();
        assert!(ArtifactStore::for_project(&fixture.0, Uuid::new_v4().to_string()).is_err());
        fs::remove_file(namespace).unwrap();
        let legacy =
            ArtifactStore::new(fixture.0.join("media"), Uuid::new_v4().to_string()).unwrap();
        let artifact = legacy
            .record_for("outside", "bin", ArtifactKind::Other, 6)
            .unwrap();
        let secret = outside.0.join("not-imported");
        fs::write(&secret, b"secret").unwrap();
        symlink(
            &secret,
            legacy
                .workspace_root
                .join("other")
                .join(&artifact.artifact_id),
        )
        .unwrap();
        assert!(ArtifactStore::for_project(&fixture.0, Uuid::new_v4().to_string()).is_err());
        assert_eq!(fs::read(secret).unwrap(), b"secret");
        assert!(!fixture
            .0
            .join("media/other")
            .join(&artifact.artifact_id)
            .exists());
    }
}
