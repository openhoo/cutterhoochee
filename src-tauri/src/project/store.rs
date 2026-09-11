pub use crate::editor::history::EditResult;
use crate::editor::history::{HistoryEntry, Receipt};
use crate::editor::operations::Transcript;
use crate::error::{AppError, ErrorCode};
use crate::ipc::{validate_safe_integer, MAX_SAFE_INTEGER};
use crate::project::model::{
    AspectRatio, FrameRate, ProjectDocument, ProjectEnvelope, PROJECT_SCHEMA_VERSION,
};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use uuid::Uuid;

#[cfg(unix)]
use std::ffi::CString;
#[cfg(unix)]
use std::os::fd::{AsRawFd, FromRawFd};
#[cfg(unix)]
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};

pub const PROJECT_FILE_NAME: &str = "project.json";
const LOCK_FILE_NAME: &str = ".project.lock";
const WORKSPACE_DIR_NAME: &str = "workspaces";
const MAX_TOKEN_BYTES: usize = 256;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HistoryAction {
    Undo,
    Redo,
}

impl HistoryAction {
    pub fn parse(value: &str) -> Result<Self, AppError> {
        match value {
            "undo" => Ok(Self::Undo),
            "redo" => Ok(Self::Redo),
            _ => Err(AppError::invalid_argument(
                "History action must be undo or redo",
            )),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct StoreSnapshot {
    pub document: ProjectDocument,
    pub project_id: String,
    pub workspace_id: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FileIdentity {
    device: u64,
    inode: u64,
}

struct StoreState {
    envelope: ProjectEnvelope,
    project_identity: FileIdentity,
    poisoned: bool,
}

struct StoreInner {
    root: PathBuf,
    project_file: PathBuf,
    workspace_id: String,
    workspace_binding_file: PathBuf,
    root_identity: FileIdentity,
    lock_identity: FileIdentity,
    stable_project_id: String,
    lock: ProjectLock,
    root_dir: File,
    state: Mutex<StoreState>,
}

/// A durable project writer. Cloning a store shares the active document and
/// lock; opening the same root again in another process is rejected by the OS
/// lock rather than relying on an in-memory registry.
#[derive(Clone)]
pub struct ProjectStore {
    inner: Arc<StoreInner>,
}

impl std::fmt::Debug for ProjectStore {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ProjectStore")
            .field("root", &self.inner.root)
            .field("workspace_id", &self.inner.workspace_id)
            .finish_non_exhaustive()
    }
}

impl ProjectStore {
    pub fn create(
        root: &Path,
        app_data: &Path,
        name: &str,
        aspect: Option<&str>,
        fps_num: u64,
        fps_den: u64,
    ) -> Result<Self, AppError> {
        let root = prepare_root(root, true)?;
        let project_file = root.join(PROJECT_FILE_NAME);
        let root_dir = open_root_directory(&root)?;
        let root_identity = file_identity(&root_dir)?;
        verify_root_path(&root, root_identity)?;
        ensure_layout_at(&root_dir)?;
        match entry_at(&root_dir, PROJECT_FILE_NAME) {
            Ok(_) => {
                return Err(AppError::busy(
                    "A project already exists at the selected location",
                ))
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(_) => return Err(AppError::io("The project file could not be inspected")),
        }
        let lock = ProjectLock::acquire_at(&root_dir)?;
        let lock_identity = lock.identity;
        let aspect = parse_aspect(aspect)?;
        let fps_num = u32::try_from(fps_num)
            .map_err(|_| AppError::invalid_argument("Frame rate numerator is too large"))?;
        let fps_den = u32::try_from(fps_den)
            .map_err(|_| AppError::invalid_argument("Frame rate denominator is too large"))?;
        let document =
            ProjectDocument::new(name.to_owned(), aspect, FrameRate::new(fps_num, fps_den)?)?;
        let envelope = ProjectEnvelope::new(document)?;
        let project_identity = match persist_project_file(
            &root,
            &root_dir,
            &lock,
            root_identity,
            lock_identity,
            None,
            &envelope,
            None,
        ) {
            Ok(identity) => identity,
            Err(PersistFailure::Safe(error) | PersistFailure::Indeterminate(error)) => {
                return Err(error)
            }
        };
        let binding = workspace_binding(
            &root,
            &project_file,
            &envelope.document.project_id,
            project_identity,
            app_data,
        )?;
        Ok(Self {
            inner: Arc::new(StoreInner {
                root,
                project_file,
                workspace_id: binding.workspace_id,
                workspace_binding_file: binding.file,
                root_identity,
                lock_identity,
                stable_project_id: envelope.document.project_id.clone(),
                lock,
                root_dir,
                state: Mutex::new(StoreState {
                    envelope,
                    project_identity,
                    poisoned: false,
                }),
            }),
        })
    }

    pub fn open(root: &Path, app_data: &Path) -> Result<Self, AppError> {
        let root = prepare_root(root, false)?;
        let project_file = root.join(PROJECT_FILE_NAME);
        let root_dir = open_root_directory(&root)?;
        let root_identity = file_identity(&root_dir)?;
        verify_root_path(&root, root_identity)?;
        ensure_layout_at(&root_dir)?;
        let lock = ProjectLock::acquire_at(&root_dir)?;
        let lock_identity = lock.identity;
        let project_entry = entry_at(&root_dir, PROJECT_FILE_NAME)
            .map_err(|_| AppError::schema("The project.json file is missing or unreadable"))?;
        if project_entry.kind != EntryKind::Regular {
            return Err(AppError::schema(
                "The project.json file is missing or unreadable",
            ));
        }
        let project_file_handle = openat_readonly(&root_dir, PROJECT_FILE_NAME)
            .map_err(|_| AppError::schema("The project.json file is missing or unreadable"))?;
        let project_identity = file_identity(&project_file_handle)?;
        if project_identity != project_entry.identity {
            return Err(AppError::io("The project file was replaced"));
        }
        let envelope = read_envelope_file(project_file_handle)?;
        let binding = workspace_binding(
            &root,
            &project_file,
            &envelope.document.project_id,
            project_identity,
            app_data,
        )?;
        Ok(Self {
            inner: Arc::new(StoreInner {
                root,
                project_file,
                workspace_id: binding.workspace_id,
                workspace_binding_file: binding.file,
                root_identity,
                lock_identity,
                stable_project_id: envelope.document.project_id.clone(),
                lock,
                root_dir,
                state: Mutex::new(StoreState {
                    envelope,
                    project_identity,
                    poisoned: false,
                }),
            }),
        })
    }

    pub fn snapshot(&self) -> Result<StoreSnapshot, AppError> {
        let state = self
            .inner
            .state
            .lock()
            .map_err(|_| AppError::io("The project writer lock is unavailable"))?;
        ensure_healthy(&state)?;
        verify_store_identity(&self.inner, state.project_identity)?;
        Ok(StoreSnapshot {
            document: state.envelope.document.clone(),
            project_id: self.inner.stable_project_id.clone(),
            workspace_id: self.inner.workspace_id.clone(),
        })
    }
    /// Read the status fields without cloning the active document.
    pub(crate) fn metadata(&self) -> Result<(String, String, String, u64), AppError> {
        let state = self
            .inner
            .state
            .lock()
            .map_err(|_| AppError::io("The project writer lock is unavailable"))?;
        ensure_healthy(&state)?;
        verify_store_identity(&self.inner, state.project_identity)?;
        Ok((
            self.inner.stable_project_id.clone(),
            self.inner.workspace_id.clone(),
            state.envelope.document.name.clone(),
            state.envelope.document.revision,
        ))
    }

    /// The project UUID is immutable and remains available for recovery/error
    /// reporting even when the current store has been poisoned.
    pub fn project_id(&self) -> String {
        self.inner.stable_project_id.clone()
    }

    pub fn is_poisoned(&self) -> bool {
        self.inner
            .state
            .lock()
            .map(|state| state.poisoned)
            .unwrap_or(true)
    }

    pub fn envelope(&self) -> Result<ProjectEnvelope, AppError> {
        let state = self
            .inner
            .state
            .lock()
            .map_err(|_| AppError::io("The project writer lock is unavailable"))?;
        ensure_healthy(&state)?;
        verify_store_identity(&self.inner, state.project_identity)?;
        Ok(state.envelope.clone())
    }
    /// Return the exact committed transaction delta when its history entry is
    /// still retained. This avoids attributing a later concurrent snapshot to
    /// an earlier activity receipt.
    pub fn transaction_delta(
        &self,
        transaction_id: &str,
    ) -> Result<Option<crate::editor::history::TransactionDelta>, AppError> {
        let state = self
            .inner
            .state
            .lock()
            .map_err(|_| AppError::io("The project writer lock is unavailable"))?;
        ensure_healthy(&state)?;
        verify_store_identity(&self.inner, state.project_identity)?;
        Ok(state
            .envelope
            .history
            .undo
            .iter()
            .chain(state.envelope.history.redo.iter())
            .find(|entry| entry.transaction_id == transaction_id)
            .map(|entry| entry.delta.clone()))
    }

    pub fn load_transcripts(&self, ids: &[String]) -> Result<Vec<Transcript>, AppError> {
        let state = self
            .inner
            .state
            .lock()
            .map_err(|_| AppError::io("The project writer lock is unavailable"))?;
        ensure_healthy(&state)?;
        verify_store_identity(&self.inner, state.project_identity)?;
        self.load_transcripts_for_document(&state.envelope.document, ids)
    }

    /// Load transcript evidence without reacquiring the project writer lock.
    ///
    /// The caller must already hold `inner.state`; the borrowed document ties
    /// this helper to that guarded state.  This is used by an edit callback
    /// running inside `commit`/`dry_run`, where calling `load_transcripts`
    /// would recursively lock the same non-reentrant mutex.
    pub(crate) fn load_transcripts_for_document(
        &self,
        _document: &ProjectDocument,
        ids: &[String],
    ) -> Result<Vec<Transcript>, AppError> {
        let mut seen = std::collections::HashSet::with_capacity(ids.len());
        let mut result = Vec::with_capacity(ids.len());
        for id in ids {
            let parsed = Uuid::parse_str(id)
                .map_err(|_| AppError::invalid_argument("transcriptId must be a UUID"))?;
            if parsed.is_nil() {
                return Err(AppError::invalid_argument(
                    "transcriptId must not be the nil UUID",
                ));
            }
            if !seen.insert(id.as_str()) {
                continue;
            }
            let bytes = read_transcript_bytes(&self.inner, id).map_err(|_| {
                AppError::new(
                    ErrorCode::AssetUnavailable,
                    "The requested local transcript is unavailable",
                )
            })?;
            let transcript: Transcript = serde_json::from_slice(&bytes)
                .map_err(|_| AppError::schema("The stored transcript is malformed"))?;
            if transcript.transcript_id != *id {
                return Err(AppError::schema(
                    "The stored transcript ID does not match its filename",
                ));
            }
            transcript.validate()?;
            result.push(transcript);
        }
        Ok(result)
    }
    /// Validate an edit against the current revision without recording a
    /// receipt, history entry, or project-file write.
    pub fn dry_run<F>(
        &self,
        transaction_id: String,
        expected_revision: u64,
        label: String,
        payload_hash: String,
        apply: F,
    ) -> Result<EditResult, AppError>
    where
        F: FnOnce(&mut ProjectDocument) -> Result<(), AppError>,
    {
        validate_token(&transaction_id, "transactionId")?;
        validate_token(&label, "label")?;
        validate_token(&payload_hash, "payloadHash")?;
        validate_safe_integer(expected_revision, "expectedRevision")?;
        let state = self
            .inner
            .state
            .lock()
            .map_err(|_| AppError::io("The project writer lock is unavailable"))?;
        ensure_healthy(&state)?;
        verify_store_identity(&self.inner, state.project_identity)?;
        if state.envelope.document.revision != expected_revision {
            return Err(AppError::new(
                ErrorCode::RevisionConflict,
                "The project revision is no longer current",
            ));
        }
        let before = state.envelope.document.clone();
        let mut candidate = before.clone();
        apply(&mut candidate)?;
        if candidate.project_id != before.project_id {
            return Err(AppError::invalid_argument(
                "An edit cannot change the project identity",
            ));
        }
        candidate.revision = before.revision;
        candidate.validate()?;
        let delta = before.diff_validated(&candidate)?;
        let result = EditResult {
            transaction_id,
            label,
            revision: before.revision,
            changed: !delta.is_empty(),
            affected_entities: delta.affected_entities(),
        };
        result.validate()?;
        Ok(result)
    }

    pub fn workspace_id(&self) -> &str {
        &self.inner.workspace_id
    }

    pub fn root(&self) -> &Path {
        &self.inner.root
    }

    /// Persist one validated local transcript atomically under this project.
    ///
    /// Transcript files are intentionally outside `project.json`: they are
    /// source-coordinate evidence and can be regenerated without changing the
    /// document revision.  The source asset/hash/frame bounds are checked
    /// against the active document before any bytes are written.
    pub fn write_transcript(&self, transcript: &Transcript) -> Result<(), AppError> {
        transcript.validate()?;
        let mut state = self
            .inner
            .state
            .lock()
            .map_err(|_| AppError::io("The project writer lock is unavailable"))?;
        ensure_healthy(&state)?;
        verify_store_identity(&self.inner, state.project_identity)?;
        let asset = state
            .envelope
            .document
            .assets
            .iter()
            .find(|asset| asset.id == transcript.asset_id)
            .ok_or_else(|| {
                AppError::new(
                    ErrorCode::AssetUnavailable,
                    "The transcript asset is not present in the project",
                )
            })?;
        if asset.content_hash != transcript.source_hash {
            return Err(AppError::new(
                ErrorCode::AssetUnavailable,
                "The transcript source hash does not match the project asset",
            ));
        }
        let frame_count = asset.frame_count().ok_or_else(|| {
            AppError::new(
                ErrorCode::AssetUnavailable,
                "The transcript asset has no normalized frame bounds",
            )
        })?;
        if transcript
            .segments
            .iter()
            .any(|segment| segment.end_frame > frame_count)
        {
            return Err(AppError::invalid_argument(
                "The transcript source interval exceeds the normalized asset",
            ));
        }
        let bytes = serde_json::to_vec_pretty(transcript)?;
        write_transcript_file(&self.inner, &transcript.transcript_id, &bytes)
    }

    /// Apply one coherent edit under the project writer. The receipt lookup is
    /// deliberately first: a retried transaction returns its original result
    /// even when the caller's expected revision is now stale.
    pub fn commit<F>(
        &self,
        transaction_id: String,
        expected_revision: u64,
        label: String,
        payload_hash: String,
        apply: F,
    ) -> Result<EditResult, AppError>
    where
        F: FnOnce(&mut ProjectDocument) -> Result<(), AppError>,
    {
        validate_token(&transaction_id, "transactionId")?;
        validate_token(&label, "label")?;
        validate_token(&payload_hash, "payloadHash")?;
        validate_safe_integer(expected_revision, "expectedRevision")?;

        let mut state = self
            .inner
            .state
            .lock()
            .map_err(|_| AppError::io("The project writer lock is unavailable"))?;
        ensure_healthy(&state)?;
        verify_store_identity(&self.inner, state.project_identity)?;
        if let Some(receipt) = state
            .envelope
            .receipts
            .iter()
            .find(|receipt| receipt.transaction_id == transaction_id)
        {
            if receipt.payload_hash == payload_hash {
                return Ok(receipt.result.clone());
            }
            return Err(AppError::new(
                ErrorCode::IdempotencyConflict,
                "The transaction ID was already used with a different payload",
            ));
        }

        let current_revision = state.envelope.document.revision;
        if expected_revision != current_revision {
            return Err(AppError::new(
                ErrorCode::RevisionConflict,
                "The project revision is no longer current",
            ));
        }

        let before = state.envelope.document.clone();
        let mut candidate = before.clone();
        apply(&mut candidate)?;
        if candidate.project_id != before.project_id {
            return Err(AppError::invalid_argument(
                "An edit cannot change the project identity",
            ));
        }
        candidate.revision = current_revision;
        candidate.validate()?;
        let delta = before.diff_validated(&candidate)?;
        let changed = !delta.is_empty();
        let next_revision = if changed {
            current_revision
                .checked_add(1)
                .filter(|revision| *revision <= MAX_SAFE_INTEGER)
                .ok_or_else(|| AppError::io("The project revision exhausted its safe range"))?
        } else {
            current_revision
        };
        candidate.revision = next_revision;
        candidate.validate()?;

        let result = EditResult {
            transaction_id: transaction_id.clone(),
            label: label.clone(),
            revision: next_revision,
            changed,
            affected_entities: delta.affected_entities(),
        };
        result.validate()?;

        let mut envelope = state.envelope.clone();
        envelope.document = candidate;
        if changed {
            let entry = HistoryEntry {
                transaction_id: transaction_id.clone(),
                label,
                expected_revision,
                revision: next_revision,
                payload_hash: payload_hash.clone(),
                delta,
                result: result.clone(),
            };
            envelope.history.push_undo(entry)?;
            envelope.history.clear_redo();
        }
        envelope.receipts.push(Receipt {
            transaction_id,
            payload_hash,
            result: result.clone(),
        });
        envelope.trim_receipts();
        envelope.validate()?;
        let project_identity = match persist_project_file(
            &self.inner.root,
            &self.inner.root_dir,
            &self.inner.lock,
            self.inner.root_identity,
            self.inner.lock_identity,
            Some(state.project_identity),
            &envelope,
            Some(&self.inner),
        ) {
            Ok(identity) => identity,
            Err(PersistFailure::Safe(error)) => return Err(error),
            Err(PersistFailure::Indeterminate(error)) => {
                state.poisoned = true;
                return Err(error);
            }
        };
        if let Err(error) = update_workspace_binding(&self.inner, project_identity, None) {
            state.poisoned = true;
            return Err(committed_state_unknown(error));
        }
        state.envelope = envelope;
        state.project_identity = project_identity;
        Ok(result)
    }

    /// Undo/redo is one global chronological stack. When supplied, the expected
    /// transaction ID is checked against the top entry before writing any state.
    pub fn history(
        &self,
        action: HistoryAction,
        expected_revision: u64,
        expected_transaction_id: Option<String>,
    ) -> Result<EditResult, AppError> {
        validate_safe_integer(expected_revision, "expectedRevision")?;
        let mut state = self
            .inner
            .state
            .lock()
            .map_err(|_| AppError::io("The project writer lock is unavailable"))?;
        ensure_healthy(&state)?;
        verify_store_identity(&self.inner, state.project_identity)?;
        if state.envelope.document.revision != expected_revision {
            return Err(AppError::new(
                ErrorCode::RevisionConflict,
                "The project revision is no longer current",
            ));
        }
        let entry = match action {
            HistoryAction::Undo => state.envelope.history.peek_undo(),
            HistoryAction::Redo => state.envelope.history.peek_redo(),
        }
        .ok_or_else(|| AppError::invalid_argument("There is no history entry to apply"))?;
        if expected_transaction_id
            .as_deref()
            .is_some_and(|expected_id| entry.transaction_id != expected_id)
        {
            return Err(AppError::new(
                ErrorCode::RevisionConflict,
                "The requested history entry is no longer at the top of the stack",
            ));
        }

        let mut candidate = state.envelope.document.clone();
        match action {
            HistoryAction::Undo => entry.delta.apply_validated(&mut candidate, false)?,
            HistoryAction::Redo => entry.delta.apply_validated(&mut candidate, true)?,
        }
        let next_revision = candidate
            .revision
            .checked_add(1)
            .filter(|revision| *revision <= MAX_SAFE_INTEGER)
            .ok_or_else(|| AppError::io("The project revision exhausted its safe range"))?;
        candidate.revision = next_revision;
        candidate.validate()?;

        let result = EditResult {
            transaction_id: entry.transaction_id.clone(),
            label: match action {
                HistoryAction::Undo => format!("Undo {}", entry.label),
                HistoryAction::Redo => format!("Redo {}", entry.label),
            },
            revision: next_revision,
            changed: true,
            affected_entities: entry.delta.affected_entities(),
        };
        result.validate()?;
        let mut envelope = state.envelope.clone();
        envelope.document = candidate;
        match action {
            HistoryAction::Undo => {
                let moved = envelope.history.pop_undo().ok_or_else(|| {
                    AppError::invalid_argument("There is no history entry to apply")
                })?;
                envelope.history.push_redo(moved)?;
            }
            HistoryAction::Redo => {
                let moved = envelope.history.pop_redo().ok_or_else(|| {
                    AppError::invalid_argument("There is no history entry to apply")
                })?;
                envelope.history.push_undo(moved)?;
            }
        }
        envelope.validate()?;
        let project_identity = match persist_project_file(
            &self.inner.root,
            &self.inner.root_dir,
            &self.inner.lock,
            self.inner.root_identity,
            self.inner.lock_identity,
            Some(state.project_identity),
            &envelope,
            Some(&self.inner),
        ) {
            Ok(identity) => identity,
            Err(PersistFailure::Safe(error)) => return Err(error),
            Err(PersistFailure::Indeterminate(error)) => {
                state.poisoned = true;
                return Err(error);
            }
        };
        if let Err(error) = update_workspace_binding(&self.inner, project_identity, None) {
            state.poisoned = true;
            return Err(committed_state_unknown(error));
        }
        state.envelope = envelope;
        state.project_identity = project_identity;
        Ok(result)
    }

    pub fn flush(&self) -> Result<(), AppError> {
        let state = self
            .inner
            .state
            .lock()
            .map_err(|_| AppError::io("The project writer lock is unavailable"))?;
        ensure_healthy(&state)?;
        verify_store_identity(&self.inner, state.project_identity)?;
        let file = openat_readonly(&self.inner.root_dir, PROJECT_FILE_NAME).map_err(|_| {
            AppError::io("The project file could not be opened for synchronization")
        })?;
        file.sync_all()
            .map_err(|_| AppError::io("The project file could not be synchronized"))?;
        sync_directory(&self.inner.root_dir)
    }
}

fn parse_aspect(value: Option<&str>) -> Result<AspectRatio, AppError> {
    match value.unwrap_or("16:9") {
        "16:9" => Ok(AspectRatio::Landscape),
        "9:16" => Ok(AspectRatio::Portrait),
        "1:1" => Ok(AspectRatio::Square),
        _ => Err(AppError::invalid_argument(
            "Aspect must be 16:9, 9:16, or 1:1",
        )),
    }
}

fn validate_token(value: &str, field: &str) -> Result<(), AppError> {
    if value.is_empty() || value.len() > MAX_TOKEN_BYTES || value.chars().any(char::is_control) {
        return Err(AppError::invalid_argument(format!("{field} is invalid")));
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EntryKind {
    Regular,
    Directory,
    Symlink,
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct DirectoryEntry {
    identity: FileIdentity,
    kind: EntryKind,
}

#[derive(Debug)]
enum PersistFailure {
    Safe(AppError),
    Indeterminate(AppError),
}

struct WorkspaceBinding {
    workspace_id: String,
    file: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct WorkspaceRecord {
    workspace_id: String,
    root_identity: String,
    #[serde(default)]
    project_id: Option<String>,
    #[serde(default)]
    project_identity: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pending_project_identity: Option<String>,
}

fn committed_state_unknown(_cause: AppError) -> AppError {
    AppError::io("The project save outcome is unknown; reopen the project before continuing")
        .with_details(serde_json::json!({
            "reason": "committed-state-unknown",
            "recovery": "reopen",
        }))
}

fn ensure_healthy(state: &StoreState) -> Result<(), AppError> {
    if state.poisoned {
        return Err(committed_state_unknown(AppError::io("poisoned store")));
    }
    Ok(())
}

fn prepare_root(root: &Path, create: bool) -> Result<PathBuf, AppError> {
    if create {
        match fs::symlink_metadata(root) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(AppError::invalid_argument(
                    "The project root must not be a symlink",
                ))
            }
            Ok(metadata) if !metadata.is_dir() => {
                return Err(AppError::invalid_argument(
                    "The project root must be a directory",
                ))
            }
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                fs::create_dir_all(root)
                    .map_err(|_| AppError::io("The project directory could not be created"))?;
            }
            Err(_) => return Err(AppError::io("The project directory could not be inspected")),
        }
    }
    let canonical = fs::canonicalize(root)
        .map_err(|_| AppError::io("The project directory could not be resolved"))?;
    if !canonical.is_dir() {
        return Err(AppError::invalid_argument(
            "The project root must be a directory",
        ));
    }
    Ok(canonical)
}

fn verify_root_path(root: &Path, expected: FileIdentity) -> Result<(), AppError> {
    let metadata = fs::symlink_metadata(root)
        .map_err(|_| AppError::io("The project root could not be inspected"))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(AppError::io("The project root was replaced"));
    }
    if metadata_identity(&metadata)? != expected {
        return Err(AppError::io("The project root was replaced"));
    }
    Ok(())
}

fn open_root_directory(path: &Path) -> Result<File, AppError> {
    #[cfg(unix)]
    {
        return OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(path)
            .map_err(|_| {
                AppError::io(
                    "The project directory could not be opened with replacement protection",
                )
            });
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        Err(AppError::io(
            "Safe descriptor-relative project storage is unavailable on this platform",
        ))
    }
}

fn metadata_identity(metadata: &fs::Metadata) -> Result<FileIdentity, AppError> {
    #[cfg(unix)]
    {
        return Ok(FileIdentity {
            device: metadata.dev(),
            inode: metadata.ino(),
        });
    }
    #[cfg(not(unix))]
    {
        let _ = metadata;
        Err(AppError::io(
            "Stable project identity is unavailable on this platform",
        ))
    }
}

fn file_identity(file: &File) -> Result<FileIdentity, AppError> {
    metadata_identity(
        &file
            .metadata()
            .map_err(|_| AppError::io("The project file identity could not be read"))?,
    )
}

fn ensure_layout(root: &Path) -> Result<(), AppError> {
    for child in ["media", "generated", "transcripts"] {
        let path = root.join(child);
        match fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(AppError::io("The project support directory is a symlink"))
            }
            Ok(metadata) if !metadata.is_dir() => {
                return Err(AppError::io("The project support path is not a directory"))
            }
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                fs::create_dir(&path).map_err(|_| {
                    AppError::io("The project support directories could not be created")
                })?;
            }
            Err(_) => {
                return Err(AppError::io(
                    "The project support directory could not be inspected",
                ))
            }
        }
    }
    Ok(())
}

fn ensure_layout_at(root: &File) -> Result<(), AppError> {
    for child in ["media", "generated", "transcripts"] {
        #[cfg(unix)]
        {
            let name = CString::new(child).expect("static child name");
            let result = unsafe { libc::mkdirat(root.as_raw_fd(), name.as_ptr(), 0o700) };
            if result < 0 {
                let error = io::Error::last_os_error();
                if error.kind() != io::ErrorKind::AlreadyExists {
                    return Err(AppError::io(
                        "The project support directories could not be created",
                    ));
                }
            }
            openat_directory(root, child)
                .map_err(|_| AppError::io("The project support directory is missing or unsafe"))?;
        }
        #[cfg(not(unix))]
        {
            let _ = child;
            let _ = root;
            return Err(AppError::io(
                "Safe descriptor-relative project storage is unavailable on this platform",
            ));
        }
    }
    Ok(())
}

fn openat_directory(root: &File, name: &str) -> io::Result<File> {
    #[cfg(unix)]
    {
        return openat_file(
            root,
            name,
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            0,
        );
    }
    #[cfg(not(unix))]
    {
        let _ = (root, name);
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "descriptor-relative storage is unavailable",
        ))
    }
}

fn openat_readonly(root: &File, name: &str) -> io::Result<File> {
    #[cfg(unix)]
    {
        return openat_file(
            root,
            name,
            libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            0,
        );
    }
    #[cfg(not(unix))]
    {
        let _ = (root, name);
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "descriptor-relative storage is unavailable",
        ))
    }
}

fn openat_create_new(root: &File, name: &str) -> io::Result<File> {
    #[cfg(unix)]
    {
        return openat_file(
            root,
            name,
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            0o600,
        );
    }
    #[cfg(not(unix))]
    {
        let _ = (root, name);
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "descriptor-relative storage is unavailable",
        ))
    }
}

fn openat_file(root: &File, name: &str, flags: i32, mode: u32) -> io::Result<File> {
    #[cfg(unix)]
    {
        let name = CString::new(name)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "invalid filename"))?;
        let fd = unsafe { libc::openat(root.as_raw_fd(), name.as_ptr(), flags, mode) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        return Ok(unsafe { File::from_raw_fd(fd) });
    }
    #[cfg(not(unix))]
    {
        let _ = (root, name, flags, mode);
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "descriptor-relative storage is unavailable",
        ))
    }
}

fn entry_at(root: &File, name: &str) -> io::Result<DirectoryEntry> {
    #[cfg(unix)]
    {
        let name = CString::new(name)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "invalid filename"))?;
        let mut stat = std::mem::MaybeUninit::<libc::stat>::zeroed();
        let result = unsafe {
            libc::fstatat(
                root.as_raw_fd(),
                name.as_ptr(),
                stat.as_mut_ptr(),
                libc::AT_SYMLINK_NOFOLLOW,
            )
        };
        if result < 0 {
            return Err(io::Error::last_os_error());
        }
        let stat = unsafe { stat.assume_init() };
        let kind = match (stat.st_mode as libc::mode_t) & libc::S_IFMT {
            mode if mode == libc::S_IFREG => EntryKind::Regular,
            mode if mode == libc::S_IFDIR => EntryKind::Directory,
            mode if mode == libc::S_IFLNK => EntryKind::Symlink,
            _ => EntryKind::Other,
        };
        return Ok(DirectoryEntry {
            identity: FileIdentity {
                device: stat.st_dev as u64,
                inode: stat.st_ino as u64,
            },
            kind,
        });
    }
    #[cfg(not(unix))]
    {
        let _ = (root, name);
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "stable project identity is unavailable",
        ))
    }
}

fn rename_at(root: &File, from: &str, to: &str) -> io::Result<()> {
    #[cfg(unix)]
    {
        let from = CString::new(from)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "invalid filename"))?;
        let to = CString::new(to)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "invalid filename"))?;
        let result = unsafe {
            libc::renameat(
                root.as_raw_fd(),
                from.as_ptr(),
                root.as_raw_fd(),
                to.as_ptr(),
            )
        };
        if result < 0 {
            return Err(io::Error::last_os_error());
        }
        return Ok(());
    }
    #[cfg(not(unix))]
    {
        let _ = (root, from, to);
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "descriptor-relative storage is unavailable",
        ))
    }
}

#[cfg(target_os = "linux")]
fn exchange_at(root: &File, left: &str, right: &str) -> io::Result<()> {
    let left = CString::new(left)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "invalid filename"))?;
    let right = CString::new(right)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "invalid filename"))?;
    let result = unsafe {
        libc::syscall(
            libc::SYS_renameat2,
            root.as_raw_fd(),
            left.as_ptr(),
            root.as_raw_fd(),
            right.as_ptr(),
            libc::RENAME_EXCHANGE,
        )
    };
    if result < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn unlink_at(root: &File, name: &str) -> io::Result<()> {
    #[cfg(unix)]
    {
        let name = CString::new(name)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "invalid filename"))?;
        let result = unsafe { libc::unlinkat(root.as_raw_fd(), name.as_ptr(), 0) };
        if result < 0 {
            return Err(io::Error::last_os_error());
        }
        return Ok(());
    }
    #[cfg(not(unix))]
    {
        let _ = (root, name);
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "descriptor-relative storage is unavailable",
        ))
    }
}

fn sync_directory(root: &File) -> Result<(), AppError> {
    root.sync_all()
        .map_err(|_| AppError::io("The project parent directory could not be synchronized"))
}

fn read_envelope_file(mut file: File) -> Result<ProjectEnvelope, AppError> {
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)
        .map_err(|_| AppError::io("The project.json file could not be read"))?;
    let value: serde_json::Value = serde_json::from_slice(&bytes)?;
    validate_strict_schema(&value)?;
    let envelope: ProjectEnvelope = serde_json::from_value(value)
        .map_err(|_| AppError::schema("The project.json file is malformed"))?;
    if envelope.schema_version != PROJECT_SCHEMA_VERSION {
        return Err(AppError::schema(
            "The project schema version is unsupported",
        ));
    }
    envelope.validate_open()?;
    Ok(envelope)
}

fn strict_fields<'a>(
    value: &'a serde_json::Value,
    path: &str,
    allowed: &[&str],
) -> Result<&'a serde_json::Map<String, serde_json::Value>, AppError> {
    let object = value
        .as_object()
        .ok_or_else(|| AppError::schema(format!("{path} must be an object")))?;
    if let Some(unknown) = object.keys().find(|key| !allowed.contains(&key.as_str())) {
        return Err(AppError::schema(format!(
            "{path} contains unsupported field {unknown}"
        )));
    }
    Ok(object)
}

fn strict_array<'a>(
    value: &'a serde_json::Value,
    path: &str,
) -> Result<&'a Vec<serde_json::Value>, AppError> {
    value
        .as_array()
        .ok_or_else(|| AppError::schema(format!("{path} must be an array")))
}

fn validate_strict_schema(value: &serde_json::Value) -> Result<(), AppError> {
    let envelope = strict_fields(
        value,
        "envelope",
        &["schemaVersion", "document", "history", "receipts"],
    )?;
    let document = envelope
        .get("document")
        .ok_or_else(|| AppError::schema("envelope.document is required"))?;
    let document = strict_fields(
        document,
        "document",
        &[
            "projectId",
            "name",
            "revision",
            "profile",
            "assets",
            "tracks",
            "clips",
            "textItems",
            "transitions",
        ],
    )?;
    let profile = strict_fields(
        document
            .get("profile")
            .ok_or_else(|| AppError::schema("document.profile is required"))?,
        "document.profile",
        &["width", "height", "fpsNum", "fpsDen", "background"],
    )?;
    strict_fields(
        profile
            .get("background")
            .ok_or_else(|| AppError::schema("document.profile.background is required"))?,
        "document.profile.background",
        &["red", "green", "blue", "alpha"],
    )?;

    for (field, path) in [
        ("assets", "document.assets"),
        ("tracks", "document.tracks"),
        ("clips", "document.clips"),
        ("textItems", "document.textItems"),
        ("transitions", "document.transitions"),
    ] {
        let values = strict_array(
            document
                .get(field)
                .ok_or_else(|| AppError::schema(format!("{path} is required")))?,
            path,
        )?;
        for (index, value) in values.iter().enumerate() {
            match field {
                "assets" => validate_asset_schema(value, &format!("{path}[{index}]"))?,
                "tracks" => {
                    strict_fields(
                        value,
                        &format!("{path}[{index}]"),
                        &["id", "kind", "name", "muted", "locked"],
                    )?;
                }
                "clips" => {
                    strict_fields(
                        value,
                        &format!("{path}[{index}]"),
                        &[
                            "id",
                            "trackId",
                            "assetId",
                            "startFrame",
                            "inFrame",
                            "durationFrames",
                            "fit",
                            "centerX",
                            "centerY",
                            "scale",
                            "opacity",
                            "gainDb",
                            "audioEnabled",
                            "fadeInFrames",
                            "fadeOutFrames",
                        ],
                    )?;
                }
                "textItems" => {
                    strict_fields(
                        value,
                        &format!("{path}[{index}]"),
                        &[
                            "id",
                            "trackId",
                            "kind",
                            "text",
                            "style",
                            "color",
                            "fontSize",
                            "positionX",
                            "positionY",
                            "lineBreaks",
                            "startFrame",
                            "durationFrames",
                            "ownerClipId",
                            "sourceStartFrame",
                            "sourceDurationFrames",
                        ],
                    )?;
                    let text = value.as_object().expect("strict_fields checked object");
                    strict_fields(
                        text.get("color")
                            .ok_or_else(|| AppError::schema("text.color is required"))?,
                        &format!("{path}[{index}].color"),
                        &["red", "green", "blue", "alpha"],
                    )?;
                }
                "transitions" => {
                    strict_fields(
                        value,
                        &format!("{path}[{index}]"),
                        &["id", "leftClipId", "rightClipId", "durationFrames"],
                    )?;
                }
                _ => unreachable!(),
            }
        }
    }

    let history = strict_fields(
        envelope
            .get("history")
            .ok_or_else(|| AppError::schema("envelope.history is required"))?,
        "history",
        &["undo", "redo"],
    )?;
    for field in ["undo", "redo"] {
        let entries = strict_array(
            history
                .get(field)
                .ok_or_else(|| AppError::schema(format!("history.{field} is required")))?,
            &format!("history.{field}"),
        )?;
        for (index, entry) in entries.iter().enumerate() {
            validate_history_entry_schema(entry, &format!("history.{field}[{index}]"))?;
        }
    }
    let receipts = strict_array(
        envelope
            .get("receipts")
            .ok_or_else(|| AppError::schema("envelope.receipts is required"))?,
        "receipts",
    )?;
    for (index, receipt) in receipts.iter().enumerate() {
        let receipt = strict_fields(
            receipt,
            &format!("receipts[{index}]"),
            &["transactionId", "payloadHash", "result"],
        )?;
        validate_edit_result_schema(
            receipt
                .get("result")
                .ok_or_else(|| AppError::schema("receipt.result is required"))?,
            &format!("receipts[{index}].result"),
        )?;
    }
    Ok(())
}

fn validate_asset_schema(value: &serde_json::Value, path: &str) -> Result<(), AppError> {
    let asset = strict_fields(
        value,
        path,
        &["id", "kind", "contentHash", "original", "normalization"],
    )?;
    let original = strict_fields(
        asset
            .get("original")
            .ok_or_else(|| AppError::schema(format!("{path}.original is required")))?,
        &format!("{path}.original"),
        &[
            "fileName",
            "location",
            "byteSize",
            "modifiedTimeMs",
            "streams",
        ],
    )?;
    let streams = strict_array(
        original
            .get("streams")
            .ok_or_else(|| AppError::schema(format!("{path}.original.streams is required")))?,
        &format!("{path}.original.streams"),
    )?;
    for (index, stream) in streams.iter().enumerate() {
        strict_fields(
            stream,
            &format!("{path}.original.streams[{index}]"),
            &[
                "kind",
                "codec",
                "startTimeMs",
                "durationMs",
                "width",
                "height",
                "sampleRate",
                "channels",
                "rotationDegrees",
                "sampleAspectNum",
                "sampleAspectDen",
                "colorSpace",
                "colorTransfer",
                "colorPrimaries",
                "colorRange",
            ],
        )?;
    }
    if let Some(normalization) = asset.get("normalization") {
        let normalization = strict_fields(
            normalization,
            &format!("{path}.normalization"),
            &["rendererVersion", "epochMs", "video", "audio"],
        )?;
        if let Some(video) = normalization.get("video") {
            strict_fields(
                video,
                &format!("{path}.normalization.video"),
                &[
                    "masterArtifactId",
                    "proxyArtifactId",
                    "frameCount",
                    "width",
                    "height",
                    "fpsNum",
                    "fpsDen",
                    "activeStartFrame",
                    "activeEndFrame",
                    "sourceStartMs",
                    "sourceEndMs",
                    "proxyFrameCount",
                ],
            )?;
        }
        if let Some(audio) = normalization.get("audio") {
            strict_fields(
                audio,
                &format!("{path}.normalization.audio"),
                &[
                    "pcmArtifactId",
                    "sampleCount",
                    "sampleRate",
                    "channels",
                    "durationFrames",
                    "activeStartSample",
                    "activeEndSample",
                    "sourceStartMs",
                    "sourceEndMs",
                ],
            )?;
        }
    }
    Ok(())
}

fn validate_history_entry_schema(value: &serde_json::Value, path: &str) -> Result<(), AppError> {
    let entry = strict_fields(
        value,
        path,
        &[
            "transactionId",
            "label",
            "expectedRevision",
            "revision",
            "payloadHash",
            "delta",
            "result",
        ],
    )?;
    validate_delta_schema(
        entry
            .get("delta")
            .ok_or_else(|| AppError::schema(format!("{path}.delta is required")))?,
        &format!("{path}.delta"),
    )?;
    validate_edit_result_schema(
        entry
            .get("result")
            .ok_or_else(|| AppError::schema(format!("{path}.result is required")))?,
        &format!("{path}.result"),
    )
}

fn validate_delta_schema(value: &serde_json::Value, path: &str) -> Result<(), AppError> {
    let delta = strict_fields(value, path, &["changes"])?;
    let changes = strict_array(
        delta
            .get("changes")
            .ok_or_else(|| AppError::schema(format!("{path}.changes is required")))?,
        &format!("{path}.changes"),
    )?;
    for (index, change) in changes.iter().enumerate() {
        let change_path = format!("{path}.changes[{index}]");
        let change = strict_fields(
            change,
            &change_path,
            &["entity", "before", "after", "beforeIndex", "afterIndex"],
        )?;
        strict_fields(
            change
                .get("entity")
                .ok_or_else(|| AppError::schema(format!("{change_path}.entity is required")))?,
            &format!("{change_path}.entity"),
            &["kind", "id"],
        )?;
        for field in ["before", "after"] {
            if let Some(state) = change.get(field) {
                let state_path = format!("{change_path}.{field}");
                let state = strict_fields(state, &state_path, &["kind", "value"])?;
                let kind = state
                    .get("kind")
                    .and_then(serde_json::Value::as_str)
                    .ok_or_else(|| AppError::schema(format!("{state_path}.kind is required")))?;
                let value = state
                    .get("value")
                    .ok_or_else(|| AppError::schema(format!("{state_path}.value is required")))?;
                match kind {
                    "document" => {
                        strict_fields(value, &format!("{state_path}.value"), &["name", "profile"])?;
                    }
                    "asset" => {
                        strict_fields(
                            value,
                            &format!("{state_path}.value"),
                            &["id", "kind", "contentHash", "original", "normalization"],
                        )?;
                    }
                    "track" => {
                        strict_fields(
                            value,
                            &format!("{state_path}.value"),
                            &["id", "kind", "name", "muted", "locked"],
                        )?;
                    }
                    "clip" => {
                        strict_fields(
                            value,
                            &format!("{state_path}.value"),
                            &[
                                "id",
                                "trackId",
                                "assetId",
                                "startFrame",
                                "inFrame",
                                "durationFrames",
                                "fit",
                                "centerX",
                                "centerY",
                                "scale",
                                "opacity",
                                "gainDb",
                                "audioEnabled",
                                "fadeInFrames",
                                "fadeOutFrames",
                            ],
                        )?;
                    }
                    "textItem" => {
                        strict_fields(
                            value,
                            &format!("{state_path}.value"),
                            &[
                                "id",
                                "trackId",
                                "kind",
                                "text",
                                "style",
                                "color",
                                "fontSize",
                                "positionX",
                                "positionY",
                                "lineBreaks",
                                "startFrame",
                                "durationFrames",
                                "ownerClipId",
                                "sourceStartFrame",
                                "sourceDurationFrames",
                            ],
                        )?;
                    }
                    "transition" => {
                        strict_fields(
                            value,
                            &format!("{state_path}.value"),
                            &["id", "leftClipId", "rightClipId", "durationFrames"],
                        )?;
                    }
                    _ => {
                        return Err(AppError::schema(format!(
                            "{state_path}.kind is unsupported"
                        )))
                    }
                }
            }
        }
    }
    Ok(())
}

fn validate_edit_result_schema(value: &serde_json::Value, path: &str) -> Result<(), AppError> {
    let result = strict_fields(
        value,
        path,
        &[
            "transactionId",
            "label",
            "revision",
            "changed",
            "affectedEntities",
        ],
    )?;
    let entities = strict_array(
        result
            .get("affectedEntities")
            .ok_or_else(|| AppError::schema(format!("{path}.affectedEntities is required")))?,
        &format!("{path}.affectedEntities"),
    )?;
    for (index, entity) in entities.iter().enumerate() {
        strict_fields(
            entity,
            &format!("{path}.affectedEntities[{index}]"),
            &["kind", "id"],
        )?;
    }
    Ok(())
}

fn workspace_binding(
    root: &Path,
    _project_file: &Path,
    project_id: &str,
    project_identity: FileIdentity,
    app_data: &Path,
) -> Result<WorkspaceBinding, AppError> {
    let canonical_root = root.to_string_lossy().into_owned();
    let root_identity = format!("{canonical_root}\0{}", filesystem_identity(root)?);
    let project_identity = format_file_identity(project_identity);
    let key = stable_identity_key(root_identity.as_bytes());
    let dir = app_data.join(WORKSPACE_DIR_NAME);
    fs::create_dir_all(&dir)
        .map_err(|_| AppError::io("The workspace binding directory could not be created"))?;
    let file = dir.join(format!("{key}.json"));
    if file.exists() {
        let bytes =
            fs::read(&file).map_err(|_| AppError::io("The workspace binding could not be read"))?;
        let mut record: WorkspaceRecord = serde_json::from_slice(&bytes)
            .map_err(|_| AppError::schema("The workspace binding is malformed"))?;
        if record.workspace_id.is_empty() {
            return Err(AppError::schema("The workspace binding is malformed"));
        }
        if record.root_identity == root_identity
            && record.project_id.as_deref() == Some(project_id)
            && (record.project_identity.as_deref() == Some(project_identity.as_str())
                || record.pending_project_identity.as_deref() == Some(project_identity.as_str()))
        {
            if record.pending_project_identity.is_some() {
                record.project_identity = Some(project_identity);
                record.pending_project_identity = None;
                atomic_write_bytes(&file, &serde_json::to_vec(&record)?)?;
            }
            return Ok(WorkspaceBinding {
                workspace_id: record.workspace_id,
                file,
            });
        }
    }
    let record = WorkspaceRecord {
        workspace_id: Uuid::new_v4().to_string(),
        root_identity,
        project_id: Some(project_id.to_owned()),
        project_identity: Some(project_identity),
        pending_project_identity: None,
    };
    atomic_write_bytes(&file, &serde_json::to_vec(&record)?)?;
    Ok(WorkspaceBinding {
        workspace_id: record.workspace_id,
        file,
    })
}

fn filesystem_identity(root: &Path) -> Result<String, AppError> {
    let metadata = fs::symlink_metadata(root)
        .map_err(|_| AppError::io("The project root identity could not be read"))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(AppError::io("The project root identity could not be read"));
    }
    Ok(format_file_identity(metadata_identity(&metadata)?))
}

fn format_file_identity(identity: FileIdentity) -> String {
    format!("unix:{}:{}", identity.device, identity.inode)
}

fn stable_identity_key(value: &[u8]) -> String {
    // Stable, non-secret filename key. The random workspace UUID remains the
    // authority; this key only locates the binding by canonical path and file
    // identity.
    let mut hash = 0xcbf29ce484222325u64;
    for byte in value {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("{hash:016x}")
}

fn update_workspace_binding(
    inner: &StoreInner,
    project_identity: FileIdentity,
    pending_project_identity: Option<FileIdentity>,
) -> Result<(), AppError> {
    let root_identity = format!(
        "{}\0{}",
        inner.root.to_string_lossy(),
        format_file_identity(inner.root_identity)
    );
    let record = WorkspaceRecord {
        workspace_id: inner.workspace_id.clone(),
        root_identity,
        project_id: Some(inner.stable_project_id.clone()),
        project_identity: Some(format_file_identity(project_identity)),
        pending_project_identity: pending_project_identity.map(format_file_identity),
    };
    atomic_write_bytes(&inner.workspace_binding_file, &serde_json::to_vec(&record)?)
}

fn verify_store_root_lock(
    root: &Path,
    root_dir: &File,
    lock: &ProjectLock,
    expected_root: FileIdentity,
    expected_lock: FileIdentity,
) -> Result<(), AppError> {
    verify_root_path(root, expected_root)?;
    if file_identity(root_dir)? != expected_root || file_identity(&lock.file)? != expected_lock {
        return Err(AppError::io("The project root or lock was replaced"));
    }
    let lock_entry = entry_at(root_dir, LOCK_FILE_NAME)
        .map_err(|_| AppError::io("The project lock file could not be inspected"))?;
    if lock_entry.kind != EntryKind::Regular || lock_entry.identity != expected_lock {
        return Err(AppError::io("The project lock was replaced"));
    }
    Ok(())
}

fn verify_store_identity(
    inner: &StoreInner,
    expected_project: FileIdentity,
) -> Result<(), AppError> {
    verify_store_root_lock(
        &inner.root,
        &inner.root_dir,
        &inner.lock,
        inner.root_identity,
        inner.lock_identity,
    )?;
    let project_entry = entry_at(&inner.root_dir, PROJECT_FILE_NAME)
        .map_err(|_| AppError::io("The project file was replaced"))?;
    if project_entry.kind != EntryKind::Regular || project_entry.identity != expected_project {
        return Err(AppError::io("The project file was replaced"));
    }
    Ok(())
}

fn persist_project_file(
    root: &Path,
    root_dir: &File,
    lock: &ProjectLock,
    root_identity: FileIdentity,
    lock_identity: FileIdentity,
    expected_project: Option<FileIdentity>,
    envelope: &ProjectEnvelope,
    workspace: Option<&StoreInner>,
) -> Result<FileIdentity, PersistFailure> {
    envelope.validate().map_err(PersistFailure::Safe)?;
    let bytes =
        serde_json::to_vec_pretty(envelope).map_err(|error| PersistFailure::Safe(error.into()))?;

    #[cfg(not(target_os = "linux"))]
    {
        let _ = (
            root,
            root_dir,
            lock,
            root_identity,
            lock_identity,
            expected_project,
            workspace,
            bytes,
        );
        return Err(PersistFailure::Safe(AppError::io(
            "Safe descriptor-relative project persistence is unavailable on this platform",
        )));
    }

    #[cfg(target_os = "linux")]
    {
        verify_store_root_lock(root, root_dir, lock, root_identity, lock_identity)
            .map_err(PersistFailure::Safe)?;
        match expected_project {
            Some(expected) => {
                let entry = entry_at(root_dir, PROJECT_FILE_NAME).map_err(|_| {
                    PersistFailure::Safe(AppError::io("The project file was replaced"))
                })?;
                if entry.kind != EntryKind::Regular || entry.identity != expected {
                    return Err(PersistFailure::Safe(AppError::io(
                        "The project file was replaced",
                    )));
                }
            }
            None => match entry_at(root_dir, PROJECT_FILE_NAME) {
                Ok(_) => {
                    return Err(PersistFailure::Safe(AppError::busy(
                        "A project already exists at the selected location",
                    )))
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(_) => {
                    return Err(PersistFailure::Safe(AppError::io(
                        "The project file could not be inspected",
                    )))
                }
            },
        }

        let temp_name = format!(
            ".{}.{}.{}.tmp",
            PROJECT_FILE_NAME,
            std::process::id(),
            Uuid::new_v4().simple()
        );
        let mut temp = openat_create_new(root_dir, &temp_name).map_err(|_| {
            PersistFailure::Safe(AppError::io("The project file could not be created"))
        })?;
        let temp_identity = file_identity(&temp).map_err(PersistFailure::Safe)?;
        let write_result = temp.write_all(&bytes).and_then(|()| temp.sync_all());
        if write_result.is_err() {
            let _ = unlink_at(root_dir, &temp_name);
            return Err(PersistFailure::Safe(AppError::io(
                "The project file could not be atomically saved",
            )));
        }

        if let Err(error) =
            verify_store_root_lock(root, root_dir, lock, root_identity, lock_identity)
        {
            let _ = unlink_at(root_dir, &temp_name);
            return Err(PersistFailure::Safe(error));
        }
        match expected_project {
            Some(expected) => {
                let entry = entry_at(root_dir, PROJECT_FILE_NAME).map_err(|_| {
                    PersistFailure::Safe(AppError::io("The project file was replaced"))
                })?;
                if entry.kind != EntryKind::Regular || entry.identity != expected {
                    let _ = unlink_at(root_dir, &temp_name);
                    return Err(PersistFailure::Safe(AppError::io(
                        "The project file was replaced",
                    )));
                }
            }
            None => {
                if entry_at(root_dir, PROJECT_FILE_NAME).is_ok() {
                    let _ = unlink_at(root_dir, &temp_name);
                    return Err(PersistFailure::Safe(AppError::busy(
                        "A project already exists at the selected location",
                    )));
                }
            }
        }

        if let Some(expected) = expected_project {
            // Persist both exact identities before exchanging project.json.
            // A process death on either side of the exchange must retain the
            // workspace, without accepting an unrelated replacement inode.
            if let Some(inner) = workspace {
                if let Err(error) = update_workspace_binding(inner, expected, Some(temp_identity)) {
                    let _ = unlink_at(root_dir, &temp_name);
                    return Err(PersistFailure::Safe(error));
                }
            }
            if let Err(_) = exchange_at(root_dir, &temp_name, PROJECT_FILE_NAME) {
                let _ = unlink_at(root_dir, &temp_name);
                return Err(PersistFailure::Safe(AppError::io(
                    "The project file could not be atomically saved",
                )));
            }
            let moved = entry_at(root_dir, &temp_name);
            let target = entry_at(root_dir, PROJECT_FILE_NAME);
            let moved_is_expected = moved
                .as_ref()
                .ok()
                .map(|entry| entry.kind == EntryKind::Regular && entry.identity == expected)
                .unwrap_or(false);
            if !moved_is_expected {
                let target_is_new = target
                    .as_ref()
                    .ok()
                    .map(|entry| {
                        entry.kind == EntryKind::Regular && entry.identity == temp_identity
                    })
                    .unwrap_or(false);
                if target_is_new
                    && moved.is_ok()
                    && exchange_at(root_dir, &temp_name, PROJECT_FILE_NAME).is_ok()
                {
                    let _ = unlink_at(root_dir, &temp_name);
                    if sync_directory(root_dir).is_ok() {
                        return Err(PersistFailure::Safe(AppError::io(
                            "The project file was replaced",
                        )));
                    }
                    return Err(PersistFailure::Indeterminate(committed_state_unknown(
                        AppError::io("The project identity rollback could not be synchronized"),
                    )));
                }
                return Err(PersistFailure::Indeterminate(committed_state_unknown(
                    AppError::io("The project file replacement could not be verified"),
                )));
            }
            let target_is_new = target
                .as_ref()
                .ok()
                .map(|entry| entry.kind == EntryKind::Regular && entry.identity == temp_identity)
                .unwrap_or(false);
            if !target_is_new {
                let _ = unlink_at(root_dir, &temp_name);
                return Err(PersistFailure::Indeterminate(committed_state_unknown(
                    AppError::io("The project file changed during save"),
                )));
            }
        } else if let Err(_) = rename_at(root_dir, &temp_name, PROJECT_FILE_NAME) {
            let _ = unlink_at(root_dir, &temp_name);
            return Err(PersistFailure::Safe(AppError::io(
                "The project file could not be atomically saved",
            )));
        }

        if sync_directory(root_dir).is_err() {
            if let Some(expected) = expected_project {
                let _ = rollback_exchange(root_dir, temp_identity, expected, &temp_name);
            }
            return Err(PersistFailure::Indeterminate(committed_state_unknown(
                AppError::io("The project directory synchronization outcome is unknown"),
            )));
        }
        let post_project = entry_at(root_dir, PROJECT_FILE_NAME);
        let target_is_new = post_project
            .as_ref()
            .ok()
            .map(|entry| entry.kind == EntryKind::Regular && entry.identity == temp_identity)
            .unwrap_or(false);
        if verify_store_root_lock(root, root_dir, lock, root_identity, lock_identity).is_err()
            || !target_is_new
        {
            let _ = unlink_at(root_dir, &temp_name);
            return Err(PersistFailure::Indeterminate(committed_state_unknown(
                AppError::io("The project identity changed during save"),
            )));
        }
        let _ = unlink_at(root_dir, &temp_name);
        Ok(temp_identity)
    }
}

#[cfg(target_os = "linux")]
fn rollback_exchange(
    root: &File,
    new_identity: FileIdentity,
    old_identity: FileIdentity,
    temp_name: &str,
) -> bool {
    let target = entry_at(root, PROJECT_FILE_NAME);
    let backup = entry_at(root, temp_name);
    if target
        .as_ref()
        .ok()
        .is_some_and(|entry| entry.kind == EntryKind::Regular && entry.identity == new_identity)
        && backup
            .as_ref()
            .ok()
            .is_some_and(|entry| entry.kind == EntryKind::Regular && entry.identity == old_identity)
        && exchange_at(root, temp_name, PROJECT_FILE_NAME).is_ok()
    {
        let _ = unlink_at(root, temp_name);
        return true;
    }
    false
}

fn read_transcript_bytes(inner: &StoreInner, id: &str) -> io::Result<Vec<u8>> {
    let directory = openat_directory(&inner.root_dir, "transcripts")?;
    let mut file = openat_readonly(&directory, &format!("{id}.json"))?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    Ok(bytes)
}

fn write_transcript_file(inner: &StoreInner, id: &str, bytes: &[u8]) -> Result<(), AppError> {
    let directory = openat_directory(&inner.root_dir, "transcripts")
        .map_err(|_| AppError::io("The transcript directory is unavailable"))?;
    atomic_write_at(&directory, &format!("{id}.json"), bytes)
}

fn atomic_write_at(directory: &File, target: &str, bytes: &[u8]) -> Result<(), AppError> {
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (directory, target, bytes);
        return Err(AppError::io(
            "Safe descriptor-relative project persistence is unavailable on this platform",
        ));
    }
    #[cfg(target_os = "linux")]
    {
        let temp_name = format!(
            ".{}.{}.{}.tmp",
            target,
            std::process::id(),
            Uuid::new_v4().simple()
        );
        let mut file = openat_create_new(directory, &temp_name)
            .map_err(|_| AppError::io("The transcript could not be created"))?;
        if file
            .write_all(bytes)
            .and_then(|()| file.sync_all())
            .is_err()
        {
            let _ = unlink_at(directory, &temp_name);
            return Err(AppError::io("The transcript could not be atomically saved"));
        }
        if let Ok(entry) = entry_at(directory, target) {
            if entry.kind == EntryKind::Symlink || entry.kind == EntryKind::Directory {
                let _ = unlink_at(directory, &temp_name);
                return Err(AppError::io("The transcript target is not a regular file"));
            }
        }
        if rename_at(directory, &temp_name, target).is_err() {
            let _ = unlink_at(directory, &temp_name);
            return Err(AppError::io("The transcript could not be atomically saved"));
        }
        sync_directory(directory)?;
        Ok(())
    }
}

fn atomic_write_bytes(path: &Path, bytes: &[u8]) -> Result<(), AppError> {
    let parent = path
        .parent()
        .ok_or_else(|| AppError::io("The project file has no parent directory"))?;
    fs::create_dir_all(parent)
        .map_err(|_| AppError::io("The project file parent directory could not be created"))?;
    let temp = parent.join(format!(
        ".{}.{}.{}.tmp",
        path.file_name().unwrap_or_default().to_string_lossy(),
        std::process::id(),
        Uuid::new_v4().simple()
    ));
    let write_result = (|| -> io::Result<()> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        fs::rename(&temp, path)?;
        sync_parent(Some(parent)).map_err(|error| io::Error::other(error.message))?;
        Ok(())
    })();
    if write_result.is_err() {
        let _ = fs::remove_file(&temp);
        return Err(AppError::io(
            "The project file could not be atomically saved",
        ));
    }
    Ok(())
}

fn sync_parent(parent: Option<&Path>) -> Result<(), AppError> {
    let parent = parent.ok_or_else(|| AppError::io("The project file parent is unavailable"))?;
    let directory = File::open(parent)
        .map_err(|_| AppError::io("The project parent directory could not be opened"))?;
    directory
        .sync_all()
        .map_err(|_| AppError::io("The project parent directory could not be synchronized"))
}

fn open_lock_at(root: &File) -> io::Result<File> {
    #[cfg(unix)]
    {
        return openat_file(
            root,
            LOCK_FILE_NAME,
            libc::O_RDWR | libc::O_CREAT | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            0o600,
        );
    }
    #[cfg(not(unix))]
    {
        let _ = root;
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "descriptor-relative storage is unavailable",
        ))
    }
}

struct ProjectLock {
    file: File,
    identity: FileIdentity,
    #[allow(dead_code)]
    path: PathBuf,
}

impl ProjectLock {
    fn acquire_at(root: &File) -> Result<Self, AppError> {
        let file = open_lock_at(root)
            .map_err(|_| AppError::io("The project lock file could not be opened"))?;
        let identity = file_identity(&file)?;
        file.try_lock_exclusive().map_err(|error| {
            if error.kind() == io::ErrorKind::WouldBlock {
                AppError::busy("The project is already open by another writer")
            } else {
                AppError::io("The project lock could not be acquired")
            }
        })?;
        Ok(Self {
            file,
            identity,
            path: PathBuf::from(LOCK_FILE_NAME),
        })
    }
}

impl Drop for ProjectLock {
    fn drop(&mut self) {
        let _ = self.file.unlock();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Barrier};
    use std::time::{SystemTime, UNIX_EPOCH};
    fn temp_root(label: &str) -> PathBuf {
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        std::env::temp_dir().join(format!("cutterhoochee-{label}-{suffix}.cutproj"))
    }

    fn store(label: &str) -> ProjectStore {
        let root = temp_root(label);
        ProjectStore::create(
            &root,
            &std::env::temp_dir().join("cutterhoochee-test-app-data"),
            "Test",
            Some("16:9"),
            30,
            1,
        )
        .expect("create store")
    }

    #[test]
    fn invalid_closure_rolls_back_without_revision_or_file_change() {
        let store = store("invalid");
        let before = store.snapshot().expect("snapshot");
        let bytes_before = fs::read(&store.inner.project_file).expect("project bytes");
        let result = store.commit(
            "tx-invalid".to_owned(),
            0,
            "invalid".to_owned(),
            "payload-invalid".to_owned(),
            |document| {
                document.name = "".to_owned();
                Ok(())
            },
        );
        assert!(result.is_err());
        assert_eq!(store.snapshot().expect("snapshot"), before);
        assert_eq!(
            fs::read(&store.inner.project_file).expect("project bytes"),
            bytes_before
        );
    }

    #[test]
    fn receipt_is_checked_before_revision_and_payload_conflicts() {
        let store = store("idempotency");
        let first = store
            .commit(
                "tx-one".to_owned(),
                0,
                "name".to_owned(),
                "payload-one".to_owned(),
                |document| {
                    document.name = "Changed".to_owned();
                    Ok(())
                },
            )
            .expect("first commit");
        let replay = store
            .commit(
                "tx-one".to_owned(),
                0,
                "name".to_owned(),
                "payload-one".to_owned(),
                |_| Err(AppError::invalid_argument("must not run")),
            )
            .expect("replay");
        assert_eq!(first, replay);
        let conflict = store.commit(
            "tx-one".to_owned(),
            1,
            "name".to_owned(),
            "payload-two".to_owned(),
            |_| Ok(()),
        );
        assert_eq!(
            conflict.expect_err("payload conflict").code,
            ErrorCode::IdempotencyConflict
        );
    }

    #[test]
    fn fresh_edit_clears_redo_after_revision_guarded_undo() {
        let store = store("history");
        store
            .commit(
                "tx-one".to_owned(),
                0,
                "one".to_owned(),
                "payload-one".to_owned(),
                |document| {
                    document.name = "One".to_owned();
                    Ok(())
                },
            )
            .expect("first");
        assert_eq!(
            store
                .history(HistoryAction::Undo, 0, None)
                .unwrap_err()
                .code,
            ErrorCode::RevisionConflict
        );
        assert_eq!(
            store
                .history(HistoryAction::Undo, 1, Some("wrong-top".to_owned()))
                .unwrap_err()
                .code,
            ErrorCode::RevisionConflict
        );
        store.history(HistoryAction::Undo, 1, None).expect("undo");
        store
            .commit(
                "tx-two".to_owned(),
                2,
                "two".to_owned(),
                "payload-two".to_owned(),
                |document| {
                    document.name = "Two".to_owned();
                    Ok(())
                },
            )
            .expect("second");
        let redo = store.history(HistoryAction::Redo, 3, Some("tx-one".to_owned()));
        assert!(redo.is_err());
    }

    #[test]
    fn second_open_is_busy_and_reopen_reads_complete_envelope() {
        let root = temp_root("lock");
        let app_data = std::env::temp_dir().join("cutterhoochee-test-app-data");
        let store =
            ProjectStore::create(&root, &app_data, "Test", Some("16:9"), 30, 1).expect("create");
        let busy = ProjectStore::open(&root, &app_data).expect_err("lock must be held");
        assert_eq!(busy.code, ErrorCode::Busy);
        drop(store);
        let reopened = ProjectStore::open(&root, &app_data).expect("reopen");
        assert_eq!(reopened.snapshot().expect("snapshot").document.revision, 0);
    }

    #[test]
    fn stale_revision_race_allows_exactly_one_commit() {
        let store = Arc::new(store("race"));
        let barrier = Arc::new(Barrier::new(3));
        let left_store = Arc::clone(&store);
        let left_barrier = Arc::clone(&barrier);
        let left = std::thread::spawn(move || {
            left_barrier.wait();
            left_store.commit(
                "tx-first".to_owned(),
                0,
                "first".to_owned(),
                "payload-first".to_owned(),
                |document| {
                    document.name = "First".to_owned();
                    Ok(())
                },
            )
        });
        let right_store = Arc::clone(&store);
        let right_barrier = Arc::clone(&barrier);
        let right = std::thread::spawn(move || {
            right_barrier.wait();
            right_store.commit(
                "tx-second".to_owned(),
                0,
                "second".to_owned(),
                "payload-second".to_owned(),
                |document| {
                    document.name = "Second".to_owned();
                    Ok(())
                },
            )
        });
        barrier.wait();
        let left = left.join().expect("left writer");
        let right = right.join().expect("right writer");
        let outcomes = [left, right];
        assert_eq!(
            outcomes.iter().filter(|result| result.is_ok()).count(),
            1,
            "exactly one concurrent writer must commit",
        );
        assert_eq!(
            outcomes
                .iter()
                .filter(|result| result
                    .as_ref()
                    .err()
                    .is_some_and(|error| error.code == ErrorCode::RevisionConflict))
                .count(),
            1,
            "the losing writer must receive a revision conflict",
        );
        assert_eq!(store.snapshot().expect("snapshot").document.revision, 1);
    }

    #[test]
    fn renamed_open_root_cannot_commit_into_replacement_root() {
        let app_data = temp_root("replacement-app-data");
        let root = temp_root("replacement-root");
        let store = ProjectStore::create(&root, &app_data, "Original", Some("16:9"), 30, 1)
            .expect("create original");
        let moved = temp_root("replacement-moved");
        fs::rename(&root, &moved).expect("move original root");

        let replacement =
            ProjectStore::create(&root, &app_data, "Replacement", Some("16:9"), 30, 1)
                .expect("create replacement");
        let replacement_file = root.join(PROJECT_FILE_NAME);
        let bytes_before = fs::read(&replacement_file).expect("replacement bytes");
        let envelope_before = replacement.envelope().expect("replacement envelope");

        let result = store.commit(
            "tx-old-root".to_owned(),
            0,
            "must fail".to_owned(),
            "payload-old-root".to_owned(),
            |document| {
                document.name = "Must not reach replacement".to_owned();
                Ok(())
            },
        );
        assert_eq!(
            result.expect_err("renamed root must fail").code,
            ErrorCode::IoError
        );
        assert_eq!(
            fs::read(&replacement_file).expect("replacement bytes"),
            bytes_before
        );
        assert_eq!(
            replacement.envelope().expect("replacement envelope"),
            envelope_before
        );
    }

    #[test]
    fn replaced_project_file_is_rejected_without_overwriting_foreign_bytes() {
        let store = store("replacement-file");
        let project_file = store.inner.project_file.clone();
        let foreign = project_file.with_extension("foreign");
        fs::copy(&project_file, &foreign).expect("copy foreign envelope");
        fs::rename(&foreign, &project_file).expect("replace project file");
        let bytes_before = fs::read(&project_file).expect("foreign bytes");

        let result = store.commit(
            "tx-replaced-file".to_owned(),
            0,
            "must fail".to_owned(),
            "payload-replaced-file".to_owned(),
            |document| {
                document.name = "Must not overwrite foreign bytes".to_owned();
                Ok(())
            },
        );
        assert_eq!(
            result.expect_err("replaced file must fail").code,
            ErrorCode::IoError
        );
        assert_eq!(
            fs::read(&project_file).expect("foreign bytes"),
            bytes_before
        );
        assert!(
            store.snapshot().is_err(),
            "replaced store must not present stale state"
        );
    }

    #[test]
    fn prepared_workspace_binding_recovers_both_sides_of_project_exchange() {
        for exchanged in [false, true] {
            let root = temp_root("prepared-binding");
            let app_data = temp_root("prepared-binding-data");
            let store = ProjectStore::create(&root, &app_data, "Initial", Some("16:9"), 30, 1)
                .expect("create");
            let workspace_id = store.workspace_id().to_owned();
            let binding_file = store.inner.workspace_binding_file.clone();
            let current_identity = entry_at(&store.inner.root_dir, PROJECT_FILE_NAME)
                .unwrap()
                .identity;
            if exchanged {
                let mut envelope = store.envelope().unwrap();
                envelope.document.name = "Prepared".to_owned();
                persist_project_file(
                    &store.inner.root,
                    &store.inner.root_dir,
                    &store.inner.lock,
                    store.inner.root_identity,
                    store.inner.lock_identity,
                    Some(current_identity),
                    &envelope,
                    Some(&store.inner),
                )
                .expect("exchange without finalizing the workspace binding");
            } else {
                let candidate = openat_create_new(&store.inner.root_dir, ".prepared-binding.tmp")
                    .expect("candidate");
                update_workspace_binding(
                    &store.inner,
                    current_identity,
                    Some(file_identity(&candidate).unwrap()),
                )
                .expect("prepare without exchanging");
            }
            drop(store);
            let reopened = ProjectStore::open(&root, &app_data).expect("recover");
            assert_eq!(reopened.workspace_id(), workspace_id);
            assert_eq!(
                reopened.snapshot().unwrap().document.name,
                if exchanged { "Prepared" } else { "Initial" }
            );
            let record: WorkspaceRecord =
                serde_json::from_slice(&fs::read(binding_file).unwrap()).unwrap();
            assert!(record.pending_project_identity.is_none());
            drop(reopened);
            fs::remove_dir_all(root).unwrap();
            fs::remove_dir_all(app_data).unwrap();
        }
    }

    #[test]
    fn own_consecutive_saves_reopen_with_the_same_workspace_binding() {
        let root = temp_root("consecutive");
        let app_data = temp_root("consecutive-app-data");
        let store =
            ProjectStore::create(&root, &app_data, "Initial", Some("16:9"), 30, 1).expect("create");
        let workspace_id = store.workspace_id().to_owned();
        store
            .commit(
                "tx-consecutive-one".to_owned(),
                0,
                "one".to_owned(),
                "payload-consecutive-one".to_owned(),
                |document| {
                    document.name = "One".to_owned();
                    Ok(())
                },
            )
            .expect("first save");
        store
            .commit(
                "tx-consecutive-two".to_owned(),
                1,
                "two".to_owned(),
                "payload-consecutive-two".to_owned(),
                |document| {
                    document.name = "Two".to_owned();
                    Ok(())
                },
            )
            .expect("second save");
        drop(store);

        let reopened = ProjectStore::open(&root, &app_data).expect("reopen");
        assert_eq!(reopened.workspace_id(), workspace_id);
        let envelope = reopened.envelope().expect("envelope");
        assert_eq!(envelope.document.name, "Two");
        assert_eq!(envelope.document.revision, 2);
        assert_eq!(envelope.history.undo.len(), 2);
        assert_eq!(envelope.receipts.len(), 2);
    }

    #[test]
    fn swapped_persisted_history_is_rejected_when_opening() {
        let root = temp_root("history-order");
        let app_data = temp_root("history-order-app-data");
        let store =
            ProjectStore::create(&root, &app_data, "Initial", Some("16:9"), 30, 1).expect("create");
        store
            .commit(
                "tx-history-one".to_owned(),
                0,
                "one".to_owned(),
                "payload-history-one".to_owned(),
                |document| {
                    document.name = "One".to_owned();
                    Ok(())
                },
            )
            .expect("first save");
        store
            .commit(
                "tx-history-two".to_owned(),
                1,
                "two".to_owned(),
                "payload-history-two".to_owned(),
                |document| {
                    document.name = "Two".to_owned();
                    Ok(())
                },
            )
            .expect("second save");
        drop(store);

        let project_file = root.join(PROJECT_FILE_NAME);
        let bytes = fs::read(&project_file).expect("project bytes");
        let mut value: serde_json::Value = serde_json::from_slice(&bytes).expect("project JSON");
        let undo = value
            .get_mut("history")
            .and_then(|history| history.get_mut("undo"))
            .and_then(serde_json::Value::as_array_mut)
            .expect("undo history");
        undo.swap(0, 1);
        fs::write(
            &project_file,
            serde_json::to_vec_pretty(&value).expect("serialized project"),
        )
        .expect("corrupt history order");

        assert!(ProjectStore::open(&root, &app_data).is_err());
    }

    #[test]
    fn replacing_or_copying_a_project_root_gets_a_new_workspace_binding() {
        let app_data = temp_root("workspace-app-data");
        let root = temp_root("workspace");
        let store =
            ProjectStore::create(&root, &app_data, "Test", Some("16:9"), 30, 1).expect("create");
        let original_workspace = store.workspace_id().to_owned();
        drop(store);

        let moved = temp_root("workspace-moved");
        fs::rename(&root, &moved).expect("move original root");
        fs::create_dir_all(&root).expect("replacement root");
        fs::copy(moved.join(PROJECT_FILE_NAME), root.join(PROJECT_FILE_NAME))
            .expect("copy project envelope");
        ensure_layout(&root).expect("replacement layout");
        let replaced = ProjectStore::open(&root, &app_data).expect("open replacement");
        assert_ne!(replaced.workspace_id(), original_workspace);
        drop(replaced);

        let copied = temp_root("workspace-copy");
        fs::create_dir_all(&copied).expect("copied root");
        fs::copy(
            moved.join(PROJECT_FILE_NAME),
            copied.join(PROJECT_FILE_NAME),
        )
        .expect("copy project envelope");
        ensure_layout(&copied).expect("copied layout");
        let copied_store = ProjectStore::open(&copied, &app_data).expect("open copied root");
        assert_ne!(copied_store.workspace_id(), original_workspace);
    }

    #[test]
    fn no_change_records_receipt_without_history() {
        let store = store("no-change");
        let result = store
            .commit(
                "tx-no-change".to_owned(),
                0,
                "noop".to_owned(),
                "payload-no-change".to_owned(),
                |_| Ok(()),
            )
            .expect("no-change commit");
        assert!(!result.changed);
        assert_eq!(result.revision, 0);
        let envelope = store.envelope().expect("envelope");
        assert!(envelope.history.undo.is_empty());
        assert!(envelope.history.redo.is_empty());
        assert_eq!(envelope.receipts.len(), 1);
    }

    #[test]
    fn complete_envelope_reopens_with_document_history_and_receipt() {
        let root = temp_root("complete");
        let app_data = std::env::temp_dir().join("cutterhoochee-test-app-data");
        let store =
            ProjectStore::create(&root, &app_data, "Test", Some("16:9"), 30, 1).expect("create");
        store
            .commit(
                "tx-persisted".to_owned(),
                0,
                "rename".to_owned(),
                "payload-persisted".to_owned(),
                |document| {
                    document.name = "Persisted".to_owned();
                    Ok(())
                },
            )
            .expect("commit");
        let project_id = store.project_id();
        drop(store);

        let reopened = ProjectStore::open(&root, &app_data).expect("reopen");
        let envelope = reopened.envelope().expect("complete envelope");
        assert_eq!(envelope.document.project_id, project_id);
        assert_eq!(envelope.document.name, "Persisted");
        assert_eq!(envelope.document.revision, 1);
        assert_eq!(envelope.history.undo.len(), 1);
        assert_eq!(envelope.receipts.len(), 1);
        assert_eq!(envelope.receipts[0].transaction_id, "tx-persisted");
    }

    #[test]
    fn failed_atomic_save_keeps_previous_active_document() {
        let store = store("save-failure");
        let before = store.snapshot().expect("snapshot");
        let project_file = store.inner.project_file.clone();
        let backup = project_file.with_extension("json.previous");
        fs::rename(&project_file, &backup).expect("move project file");
        fs::create_dir(&project_file).expect("block replacement path");

        let result = store.commit(
            "tx-failing-save".to_owned(),
            0,
            "rename".to_owned(),
            "payload-failing-save".to_owned(),
            |document| {
                document.name = "Must not become active".to_owned();
                Ok(())
            },
        );
        assert_eq!(result.expect_err("save must fail").code, ErrorCode::IoError);
        let state = store.inner.state.lock().expect("state");
        assert_eq!(state.envelope.document, before.document);
        assert!(!state.poisoned);
        drop(state);

        fs::remove_dir(&project_file).expect("remove blocker");
        fs::rename(backup, project_file).expect("restore project file");
        assert_eq!(store.snapshot().expect("snapshot"), before);
    }
}
