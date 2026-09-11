use crate::editor::dispatcher::{CallerContext, CallerKind};
use crate::error::{AppError, ErrorCode};
use crate::state::AppState;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap};
use std::fs::{self, File, OpenOptions};
use std::future::Future;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::io::AsyncReadExt;
use tokio::process::{Child, Command};
use tokio::sync::Notify;
use tokio::time::{timeout, Duration as TokioDuration};
use ts_rs::TS;
use uuid::Uuid;

const PERMISSION_TTL: Duration = Duration::from_secs(120);
const MAX_READ_BYTES: u64 = 1024 * 1024;
const MAX_WRITE_BYTES: usize = 1024 * 1024;
const MAX_COMMAND_OUTPUT_BYTES: usize = 1024 * 1024;
const MAX_HTTP_BODY_BYTES: usize = 1024 * 1024;
const DEFAULT_COMMAND_TIMEOUT_MS: u64 = 60_000;
const MAX_COMMAND_TIMEOUT_MS: u64 = 600_000;
const PERMISSIONS_FILE: &str = "permissions.json";
const EVIDENCE_FILE: &str = "evidence-grants.json";
const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;

/// The non-portable authority scope. Workspace identity is intentionally
/// independent from the project UUID in project.json; copied projects receive
/// a different binding and therefore cannot inherit these records.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase")]
pub struct PermissionScope {
    pub workspace_id: String,
    pub project_id: Option<String>,
    #[ts(type = "SafeInteger")]
    pub generation: u64,
}

impl PermissionScope {
    pub fn validate(&self) -> Result<(), AppError> {
        if self.workspace_id.trim().is_empty() || self.workspace_id.len() > 256 {
            return Err(AppError::invalid_argument("workspaceId is required"));
        }
        if self
            .project_id
            .as_deref()
            .is_some_and(|id| id.trim().is_empty())
        {
            return Err(AppError::invalid_argument("projectId must not be empty"));
        }
        if self.generation > MAX_SAFE_INTEGER {
            return Err(AppError::schema(
                "generation exceeds the safe integer range",
            ));
        }
        Ok(())
    }
}

/// Identity captured for a selected file or approved overwrite target.
/// Paths are canonicalized, and the token contains stable filesystem metadata;
/// it is never accepted from a caller on its own.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase")]
pub struct FileIdentity {
    pub canonical_path: String,
    pub token: String,
    #[ts(type = "SafeInteger")]
    pub size: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum FileGrantPurpose {
    Import,
    Relink,
    Read,
    Evidence,
    Upload,
}

/// A read/use grant persisted outside the portable project envelope.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase")]
pub struct FileGrant {
    pub grant_id: String,
    pub scope: PermissionScope,
    pub path: String,
    pub identity: FileIdentity,
    pub purpose: FileGrantPurpose,
    pub created_at_ms: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum PermissionOperation {
    FileGrant,
    SystemRead,
    SystemWrite,
    SystemExecute,
    SystemHttp,
    Overwrite,
    Upload,
}

/// The UI-facing operation summary. It contains no request body or credential
/// material; body payloads are represented by an exact SHA-256 and size.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase")]
pub struct PermissionDetails {
    pub operation: PermissionOperation,
    pub path: Option<String>,
    pub paths: Vec<String>,
    pub canonical_executable: Option<String>,
    pub arguments: Vec<String>,
    pub cwd: Option<String>,
    pub url: Option<String>,
    pub method: Option<String>,
    pub body_sha256: Option<String>,
    #[ts(type = "SafeInteger")]
    pub body_bytes: u64,
    pub target_identity: Option<FileIdentity>,
    #[ts(type = "SafeInteger")]
    pub offset: Option<u64>,
    #[ts(type = "SafeInteger")]
    pub length: Option<u64>,
    #[ts(type = "SafeInteger")]
    pub timeout_ms: Option<u64>,
    pub overwrite: bool,
}

/// Authoritative pending record emitted to trusted UI. The corresponding
/// exact operation is retained privately in the runtime and must match at
/// consume time; callers cannot alter it by replaying the displayed object.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase")]
pub struct PermissionRequest {
    pub operation_id: String,
    pub scope: PermissionScope,
    pub run_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub tool_call_id: Option<String>,
    pub details: PermissionDetails,
    #[ts(type = "SafeInteger")]
    pub expires_at_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase")]
pub struct PermissionDecision {
    pub operation_id: String,
    pub allow: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase")]
pub struct EvidenceGrant {
    pub scope: PermissionScope,
    pub provider_id: String,
    pub account_id: String,
    pub granted_at_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase")]
pub struct EvidenceGrantResult {
    pub provider_id: String,
    pub account_id: String,
    pub allowed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase")]
pub struct SystemReadRequest {
    pub path: String,
    #[ts(type = "SafeInteger")]
    pub offset: u64,
    #[ts(type = "SafeInteger")]
    pub length: u64,
    pub operation_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase")]
pub struct SystemReadReply {
    pub path: String,
    #[ts(type = "SafeInteger")]
    pub offset: u64,
    pub data: Vec<u8>,
    pub eof: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase")]
pub struct SystemWriteRequest {
    pub path: String,
    pub data: Vec<u8>,
    pub overwrite: bool,
    pub operation_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase")]
pub struct SystemWriteReply {
    pub path: String,
    #[ts(type = "SafeInteger")]
    pub bytes_written: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase")]
pub struct SystemExecuteRequest {
    pub executable: String,
    pub arguments: Vec<String>,
    pub cwd: String,
    pub environment: BTreeMap<String, String>,
    #[ts(type = "SafeInteger")]
    pub timeout_ms: Option<u64>,
    pub operation_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase")]
pub struct SystemExecuteReply {
    pub status: Option<i32>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub timed_out: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase")]
pub struct SystemHttpRequest {
    pub url: String,
    pub method: String,
    pub headers: BTreeMap<String, String>,
    pub body: Vec<u8>,
    #[ts(type = "SafeInteger")]
    pub timeout_ms: Option<u64>,
    pub operation_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase")]
pub struct SystemHttpReply {
    pub status: u16,
    pub content_type: Option<String>,
    pub body: Vec<u8>,
    pub redirect_url: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(tag = "action", content = "params", rename_all = "snake_case")]
#[ts(tag = "action", content = "params", rename_all = "snake_case")]
pub enum PermissionsAction {
    Pending {},
    Answer {
        #[serde(rename = "operationId")]
        #[ts(rename = "operationId")]
        operation_id: String,
        allow: bool,
    },
    Evidence {
        #[serde(rename = "providerId")]
        #[ts(rename = "providerId")]
        provider_id: String,
        #[serde(rename = "accountId")]
        #[ts(rename = "accountId")]
        account_id: String,
        allow: bool,
    },
    Revoke {
        #[serde(rename = "providerId")]
        #[ts(rename = "providerId")]
        provider_id: Option<String>,
        #[serde(rename = "accountId")]
        #[ts(rename = "accountId")]
        account_id: Option<String>,
    },
    GrantFiles {
        paths: Option<Vec<String>>,
        purpose: FileGrantPurpose,
    },
    SystemRead(SystemReadRequest),
    SystemWrite(SystemWriteRequest),
    SystemExecute(SystemExecuteRequest),
    SystemHttp(SystemHttpRequest),
}
impl PermissionsAction {
    pub fn is_ui_safe(&self) -> bool {
        matches!(
            self,
            Self::Pending {}
                | Self::Answer { .. }
                | Self::Evidence { .. }
                | Self::Revoke { .. }
                | Self::GrantFiles { .. }
        )
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(tag = "kind", content = "data", rename_all = "snake_case")]
#[ts(tag = "kind", content = "data", rename_all = "snake_case")]
pub enum PermissionsReply {
    Pending(Vec<PermissionRequest>),
    Answer(PermissionDecision),
    Evidence(EvidenceGrantResult),
    Files(Vec<FileGrant>),
    SystemRead(SystemReadReply),
    SystemWrite(SystemWriteReply),
    SystemExecute(SystemExecuteReply),
    SystemHttp(SystemHttpReply),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PermissionFlow<T> {
    Pending(PermissionRequest),
    Ready(T),
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum PermissionEvent {
    Requested(PermissionRequest),
    Decided(PermissionDecision),
    Revoked { operation_id: String },
    EvidenceChanged(EvidenceGrantResult),
}

/// A consumed, exact destination approval. The grant is deliberately
/// non-serializable and carries the canonical parent identity so installation
/// can reopen that directory by descriptor rather than trusting a pathname.
#[derive(Debug, Clone)]
pub struct ExportDestinationGrant {
    destination: PathBuf,
    target: FileIdentity,
    parent: FileIdentity,
    overwrite: bool,
}

#[derive(Debug, Clone)]
pub struct BackupCapability {
    destination: PathBuf,
    backup_name: Option<String>,
    parent: FileIdentity,
    original: FileIdentity,
}

#[derive(Debug, Clone)]
pub struct InstallOutcome {
    destination: PathBuf,
    installed: FileIdentity,
    original: FileIdentity,
    replaced: bool,
}

impl InstallOutcome {
    pub fn destination(&self) -> &Path {
        &self.destination
    }

    pub fn installed_identity(&self) -> &FileIdentity {
        &self.installed
    }

    pub fn original_identity(&self) -> &FileIdentity {
        &self.original
    }

    pub fn replaced(&self) -> bool {
        self.replaced
    }
}

impl BackupCapability {
    /// Remove only this grant's app-owned backup. The destination itself is
    /// never touched.
    pub fn discard(self) -> Result<(), AppError> {
        if let Some(backup_name) = self.backup_name.as_deref() {
            remove_backup_file(&self.parent, backup_name, &self.original)?;
        }
        Ok(())
    }

    pub fn restore_if_unchanged(self, outcome: &InstallOutcome) -> Result<(), AppError> {
        if self.destination != outcome.destination || self.original != outcome.original {
            return Err(AppError::new(
                ErrorCode::PermissionDenied,
                "The export backup does not match the install outcome",
            ));
        }
        restore_export_backup(&self, outcome)
    }
}

impl ExportDestinationGrant {
    pub fn destination(&self) -> &Path {
        Path::new(&self.destination)
    }

    pub fn expected_identity(&self) -> &FileIdentity {
        &self.target
    }

    pub fn overwrite(&self) -> bool {
        self.overwrite
    }

    pub fn backup_existing(&self) -> Result<BackupCapability, AppError> {
        backup_export_target(
            self.destination(),
            &self.target,
            &self.parent,
            self.overwrite,
        )
    }

    /// Install an already-rendered temporary file and return the identity
    /// observed after installation for conditional rollback.
    pub fn install_with_state(&self, temporary: &Path) -> Result<InstallOutcome, AppError> {
        install_approved_file(
            temporary,
            self.destination(),
            &self.target,
            &self.parent,
            self.overwrite,
        )
    }

    /// Compatibility convenience for single-output callers.
    pub fn install(&self, temporary: &Path, destination: &Path) -> Result<(), AppError> {
        if destination != self.destination() {
            return Err(AppError::new(
                ErrorCode::PermissionDenied,
                "The export destination no longer matches its approval",
            ));
        }
        self.install_with_state(temporary).map(|_| ())
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct EvidenceKey {
    workspace_id: String,
    project_id: Option<String>,
    provider_id: String,
    account_id: String,
}

#[derive(Debug, Clone)]
struct ActiveScope {
    scope: PermissionScope,
}

#[derive(Debug, Clone)]
struct PendingRecord {
    request: PermissionRequest,
    exact: ExactOperation,
    decision: Option<bool>,
    consumed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ExactOperation {
    FileGrant {
        paths: Vec<FileIdentity>,
        purpose: FileGrantPurpose,
    },
    SystemRead {
        identity: FileIdentity,
        offset: u64,
        length: u64,
    },
    SystemWrite {
        target: FileIdentity,
        data_sha256: String,
        data_len: usize,
        overwrite: bool,
    },
    ExportDestination {
        target: FileIdentity,
        parent: FileIdentity,
        overwrite: bool,
    },
    SystemExecute {
        executable: FileIdentity,
        arguments: Vec<String>,
        cwd: FileIdentity,
        environment: BTreeMap<String, String>,
        timeout_ms: u64,
    },
    SystemHttp {
        url: String,
        method: String,
        headers: BTreeMap<String, String>,
        body_sha256: String,
        body_len: usize,
        timeout_ms: u64,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct StoredFileGrant {
    grant: FileGrant,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct StoredEvidenceGrant {
    grant: EvidenceGrant,
}

struct PermissionsInner {
    app_data_dir: PathBuf,
    grants: HashMap<String, FileGrant>,
    evidence: HashMap<EvidenceKey, EvidenceGrant>,
    active_scopes: HashMap<u64, ActiveScope>,
    active_runs: HashMap<(u64, String), ()>,
    retired_runs: HashMap<(u64, String), ()>,
    running_commands: HashMap<(u64, String), Vec<Arc<CommandCancellation>>>,
    pending: HashMap<String, PendingRecord>,
    waiters: HashMap<String, Arc<Notify>>,
    events: tokio::sync::broadcast::Sender<PermissionEvent>,
}

/// Backend-enforced file, evidence, and external-operation permissions.
/// Records are scoped to an app-owned workspace binding and never enter the
/// portable project JSON.
#[derive(Clone)]
pub struct PermissionsRuntime {
    inner: Arc<Mutex<PermissionsInner>>,
}
struct CommandCancellation {
    cancelled: AtomicBool,
    notify: Notify,
    pid: Mutex<Option<u32>>,
}

impl CommandCancellation {
    fn new() -> Self {
        Self {
            cancelled: AtomicBool::new(false),
            notify: Notify::new(),
            pid: Mutex::new(None),
        }
    }

    fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }

    fn pid(&self) -> Option<u32> {
        self.pid.lock().ok().and_then(|value| *value)
    }

    fn spawn(&self, mut command: Command) -> Result<Child, AppError> {
        let mut pid = self
            .pid
            .lock()
            .map_err(|_| AppError::io("The approved command cancellation state is unavailable"))?;
        if self.is_cancelled() {
            return Err(AppError::stale_session(
                "The approved command was cancelled",
            ));
        }
        let child = command
            .spawn()
            .map_err(|_| AppError::io("The approved command could not be started"))?;
        *pid = child.id();
        Ok(child)
    }

    fn clear_pid(&self) {
        if let Ok(mut pid) = self.pid.lock() {
            *pid = None;
        }
    }

    fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
        self.notify.notify_waiters();
        #[cfg(unix)]
        if let Some(pid) = self.pid() {
            signal_process_group(pid, libc::SIGTERM);
            signal_process_group(pid, libc::SIGKILL);
        }
    }

    async fn wait_cancelled(&self) {
        loop {
            let notified = self.notify.notified();
            if self.is_cancelled() {
                return;
            }
            notified.await;
        }
    }
}

struct RunningCommandLease {
    permissions: PermissionsRuntime,
    key: (u64, String),
    cancellation: Arc<CommandCancellation>,
}

impl RunningCommandLease {
    fn cancellation(&self) -> &CommandCancellation {
        &self.cancellation
    }
}

impl Drop for RunningCommandLease {
    fn drop(&mut self) {
        self.cancellation.cancel();
        self.permissions
            .finish_running_command(&self.key, &self.cancellation);
    }
}

impl PermissionsRuntime {
    pub fn new(app_data_dir: PathBuf) -> Result<Self, AppError> {
        fs::create_dir_all(&app_data_dir)
            .map_err(|_| AppError::io("The permission data directory could not be created"))?;
        let grants = load_json::<Vec<StoredFileGrant>>(&app_data_dir.join(PERMISSIONS_FILE))?
            .unwrap_or_default()
            .into_iter()
            .map(|item| (item.grant.grant_id.clone(), item.grant))
            .collect();
        let evidence = load_json::<Vec<StoredEvidenceGrant>>(&app_data_dir.join(EVIDENCE_FILE))?
            .unwrap_or_default()
            .into_iter()
            .map(|item| {
                let key = EvidenceKey {
                    workspace_id: item.grant.scope.workspace_id.clone(),
                    project_id: item.grant.scope.project_id.clone(),
                    provider_id: item.grant.provider_id.clone(),
                    account_id: item.grant.account_id.clone(),
                };
                (key, item.grant)
            })
            .collect();
        let (events, _) = tokio::sync::broadcast::channel(128);
        Ok(Self {
            inner: Arc::new(Mutex::new(PermissionsInner {
                app_data_dir,
                grants,
                evidence,
                active_scopes: HashMap::new(),
                active_runs: HashMap::new(),
                retired_runs: HashMap::new(),
                running_commands: HashMap::new(),
                pending: HashMap::new(),
                waiters: HashMap::new(),
                events,
            })),
        })
    }

    /// Build the only scope accepted for the currently open project. Native
    /// bridge callers must obtain scopes this way rather than forwarding
    /// workspace/project fields supplied by WebView JSON.
    pub fn scope_for_state(&self, state: &AppState) -> Result<PermissionScope, AppError> {
        let scope = PermissionScope {
            workspace_id: state
                .current_workspace_id()
                .ok_or_else(|| AppError::invalid_argument("No project is open"))?,
            project_id: state.current_project_id(),
            generation: state.generation(),
        };
        self.register_scope(scope.clone())?;
        Ok(scope)
    }

    pub(crate) fn register_scope(&self, scope: PermissionScope) -> Result<(), AppError> {
        scope.validate()?;
        let stale_commands = {
            let mut inner = self.lock()?;
            let stale_keys: Vec<(u64, String)> = inner
                .running_commands
                .keys()
                .filter(|(generation, _)| *generation != scope.generation)
                .cloned()
                .collect();
            let mut stale_commands = Vec::new();
            for key in stale_keys {
                if let Some(mut commands) = inner.running_commands.remove(&key) {
                    stale_commands.append(&mut commands);
                }
            }
            inner.active_scopes.clear();
            inner.active_scopes.insert(
                scope.generation,
                ActiveScope {
                    scope: scope.clone(),
                },
            );
            inner
                .active_runs
                .retain(|(generation, _), _| *generation == scope.generation);
            inner
                .retired_runs
                .retain(|(generation, _), _| *generation == scope.generation);
            stale_commands
        };
        for command in stale_commands {
            command.cancel();
        }
        Ok(())
    }

    pub(crate) fn register_run(
        &self,
        scope: &PermissionScope,
        run_id: &str,
    ) -> Result<(), AppError> {
        self.require_active_scope(scope)?;
        if run_id.is_empty() || run_id.len() > 256 {
            return Err(AppError::invalid_argument("runId is invalid"));
        }
        let mut inner = self.lock()?;
        if inner
            .retired_runs
            .contains_key(&(scope.generation, run_id.to_owned()))
        {
            return Err(AppError::stale_session("The assistant run is retired"));
        }
        inner
            .active_runs
            .insert((scope.generation, run_id.to_owned()), ());
        Ok(())
    }
    fn begin_running_command(
        &self,
        scope: &PermissionScope,
        run_id: &str,
    ) -> Result<RunningCommandLease, AppError> {
        scope.validate()?;
        let mut inner = self.lock()?;
        require_active_scope_locked(&inner, scope)?;
        require_active_run_locked(&inner, scope, Some(run_id))?;
        let key = (scope.generation, run_id.to_owned());
        let cancellation = Arc::new(CommandCancellation::new());
        inner
            .running_commands
            .entry(key.clone())
            .or_default()
            .push(cancellation.clone());
        Ok(RunningCommandLease {
            permissions: self.clone(),
            key,
            cancellation,
        })
    }

    fn finish_running_command(&self, key: &(u64, String), cancellation: &Arc<CommandCancellation>) {
        let Ok(mut inner) = self.inner.lock() else {
            return;
        };
        let empty = if let Some(commands) = inner.running_commands.get_mut(key) {
            commands.retain(|current| !Arc::ptr_eq(current, cancellation));
            commands.is_empty()
        } else {
            false
        };
        if empty {
            inner.running_commands.remove(key);
        }
    }

    /// Check a run without consulting `AppState`. Native state writers call
    /// this while their lifecycle lock is held, so this deliberately inspects
    /// only the permission runtime's own generation/project/run registry.
    pub(crate) fn require_active_run(
        &self,
        generation: u64,
        project_id: Option<&str>,
        run_id: &str,
    ) -> Result<(), AppError> {
        if run_id.is_empty() || run_id.len() > 256 {
            return Err(AppError::stale_session("The assistant run is invalid"));
        }
        let inner = self.lock()?;
        let active = inner
            .active_scopes
            .get(&generation)
            .ok_or_else(|| AppError::stale_session("The application generation is retired"))?;
        if active.scope.project_id.as_deref() != project_id {
            return Err(AppError::stale_session(
                "The assistant run belongs to another project",
            ));
        }
        if inner
            .retired_runs
            .contains_key(&(generation, run_id.to_owned()))
            || !inner
                .active_runs
                .contains_key(&(generation, run_id.to_owned()))
        {
            return Err(AppError::stale_session("The assistant run is retired"));
        }
        Ok(())
    }

    pub(crate) fn validate_run(
        &self,
        scope: &PermissionScope,
        run_id: Option<&str>,
    ) -> Result<(), AppError> {
        scope.validate()?;
        let inner = self.lock()?;
        require_active_scope_locked(&inner, scope)?;
        require_active_run_locked(&inner, scope, run_id)
    }

    fn require_active_scope(&self, scope: &PermissionScope) -> Result<(), AppError> {
        scope.validate()?;
        let inner = self.lock()?;
        require_active_scope_locked(&inner, scope)
    }

    fn require_caller_scope(
        &self,
        caller: &CallerContext,
        scope: &PermissionScope,
    ) -> Result<(), AppError> {
        scope.validate()?;
        let inner = self.lock()?;
        require_caller_scope_locked(&inner, caller, scope)
    }

    pub fn subscribe(&self) -> tokio::sync::broadcast::Receiver<PermissionEvent> {
        self.inner
            .lock()
            .map(|inner| inner.events.subscribe())
            .unwrap_or_else(|_| {
                let (_, receiver) = tokio::sync::broadcast::channel(1);
                receiver
            })
    }

    pub async fn handle(
        &self,
        action: &PermissionsAction,
        caller: &CallerContext,
        state: &AppState,
    ) -> Result<PermissionsReply, AppError> {
        let scope = self.scope_for_state(state)?;
        if action.is_ui_safe() && !matches!(caller.kind, CallerKind::HumanWindow { .. }) {
            return Err(AppError::new(
                ErrorCode::PermissionDenied,
                "Only a trusted application window may perform this permission action",
            ));
        }
        match action {
            PermissionsAction::Pending {} => Ok(PermissionsReply::Pending(self.pending(&scope)?)),
            PermissionsAction::Answer {
                operation_id,
                allow,
            } => Ok(PermissionsReply::Answer(self.answer(
                caller,
                operation_id,
                *allow,
            )?)),
            PermissionsAction::Evidence {
                provider_id,
                account_id,
                allow,
            } => Ok(PermissionsReply::Evidence(self.evidence_grant(
                caller,
                scope,
                provider_id.clone(),
                account_id.clone(),
                *allow,
            )?)),
            PermissionsAction::Revoke {
                provider_id,
                account_id,
            } => {
                self.revoke_evidence(
                    caller,
                    &scope,
                    provider_id.as_deref(),
                    account_id.as_deref(),
                )?;
                Ok(PermissionsReply::Evidence(EvidenceGrantResult {
                    provider_id: provider_id.clone().unwrap_or_default(),
                    account_id: account_id.clone().unwrap_or_default(),
                    allowed: false,
                }))
            }
            PermissionsAction::GrantFiles { paths, purpose } => {
                let paths = paths
                    .as_ref()
                    .map(|items| items.iter().map(PathBuf::from).collect());
                Ok(PermissionsReply::Files(
                    self.grant_files(caller, scope, paths, *purpose)?,
                ))
            }
            PermissionsAction::SystemRead(request) => {
                let flow = self.consume_read(caller, &scope, caller.run_id.as_deref(), request)?;
                let reply = self
                    .await_and_consume(flow, |operation_id| {
                        let mut request = request.clone();
                        request.operation_id = Some(operation_id);
                        std::future::ready(self.consume_read(
                            caller,
                            &scope,
                            caller.run_id.as_deref(),
                            &request,
                        ))
                    })
                    .await?;
                Ok(PermissionsReply::SystemRead(reply))
            }
            PermissionsAction::SystemWrite(request) => {
                let flow = self.consume_write(caller, &scope, caller.run_id.as_deref(), request)?;
                let reply = self
                    .await_and_consume(flow, |operation_id| {
                        let mut request = request.clone();
                        request.operation_id = Some(operation_id);
                        std::future::ready(self.consume_write(
                            caller,
                            &scope,
                            caller.run_id.as_deref(),
                            &request,
                        ))
                    })
                    .await?;
                Ok(PermissionsReply::SystemWrite(reply))
            }
            PermissionsAction::SystemExecute(request) => {
                let flow = self
                    .consume_execute(caller, &scope, caller.run_id.as_deref(), request)
                    .await?;
                let reply = self
                    .await_and_consume(flow, |operation_id| async {
                        let mut request = request.clone();
                        request.operation_id = Some(operation_id);
                        self.consume_execute(caller, &scope, caller.run_id.as_deref(), &request)
                            .await
                    })
                    .await?;
                Ok(PermissionsReply::SystemExecute(reply))
            }
            PermissionsAction::SystemHttp(request) => {
                let flow = self
                    .consume_http(caller, &scope, caller.run_id.as_deref(), request)
                    .await?;
                let reply = self
                    .await_and_consume(flow, |operation_id| async {
                        let mut request = request.clone();
                        request.operation_id = Some(operation_id);
                        self.consume_http(caller, &scope, caller.run_id.as_deref(), &request)
                            .await
                    })
                    .await?;
                Ok(PermissionsReply::SystemHttp(reply))
            }
        }
    }

    async fn await_and_consume<T, F, Fut>(
        &self,
        flow: PermissionFlow<T>,
        consume: F,
    ) -> Result<T, AppError>
    where
        F: FnOnce(String) -> Fut,
        Fut: Future<Output = Result<PermissionFlow<T>, AppError>>,
    {
        match flow {
            PermissionFlow::Ready(reply) => Ok(reply),
            PermissionFlow::Pending(request) => {
                let operation_id = request.operation_id;
                if !self.await_decision(&operation_id).await? {
                    let _ = self.revoke_operation(&operation_id);
                    return Err(AppError::new(
                        ErrorCode::PermissionDenied,
                        "The native permission request was denied",
                    ));
                }
                match consume(operation_id).await? {
                    PermissionFlow::Ready(reply) => Ok(reply),
                    PermissionFlow::Pending(_) => Err(AppError::new(
                        ErrorCode::PermissionDenied,
                        "The native permission request was not consumed",
                    )),
                }
            }
        }
    }

    pub fn pending(&self, scope: &PermissionScope) -> Result<Vec<PermissionRequest>, AppError> {
        self.require_active_scope(scope)?;
        let now = now_ms();
        let mut inner = self.lock()?;
        inner
            .pending
            .retain(|_, record| record.request.expires_at_ms > now && !record.consumed);
        Ok(inner
            .pending
            .values()
            .filter(|record| {
                record.request.scope == *scope && record.decision.is_none() && !record.consumed
            })
            .map(|record| record.request.clone())
            .collect())
    }

    /// Grant files selected through the native chooser. A chooser-only
    /// request (`paths == None`) is allowed for the trusted application
    /// window or a live assistant sidecar run; caller-supplied paths are
    /// always rejected. Sidecars can request the chooser but cannot mint
    /// grants from JSON.
    pub fn grant_files(
        &self,
        caller: &CallerContext,
        scope: PermissionScope,
        paths: Option<Vec<PathBuf>>,
        purpose: FileGrantPurpose,
    ) -> Result<Vec<FileGrant>, AppError> {
        let chosen = match paths {
            Some(_) => {
                return Err(AppError::new(
                    ErrorCode::PermissionDenied,
                    "Caller-supplied file paths require a native approval workflow",
                ))
            }
            None => {
                self.require_caller_scope(caller, &scope)?;
                match &caller.kind {
                    CallerKind::HumanWindow { .. } => require_human(caller)?,
                    CallerKind::AgentSidecar { .. } => {
                        require_agent(caller)?;
                        self.validate_run(&scope, caller.run_id.as_deref())?;
                    }
                }
                rfd::FileDialog::new()
                    .set_title("Choose files for Cutterhoochee")
                    .pick_files()
                    .ok_or_else(|| AppError::io("File selection was cancelled"))?
            }
        };
        let mut identities = Vec::with_capacity(chosen.len());
        for path in &chosen {
            identities.push(canonical_file_identity(path)?);
        }
        let now = now_ms();
        let mut grants = Vec::with_capacity(identities.len());
        let mut inner = self.lock()?;
        validate_grant_publish_locked(&inner, caller, &scope)?;
        for identity in identities {
            let grant = FileGrant {
                grant_id: Uuid::new_v4().to_string(),
                scope: scope.clone(),
                path: identity.canonical_path.clone(),
                identity,
                purpose,
                created_at_ms: now,
            };
            inner.grants.insert(grant.grant_id.clone(), grant.clone());
            grants.push(grant);
        }
        persist_grants(&inner)?;
        Ok(grants)
    }

    /// Mint exact-file grants for paths delivered by Tauri's native
    /// `DragDropEvent::Drop` handler. This remains crate-visible:
    /// serialized editor JSON cannot call it, and the handler constructs the
    /// `HumanWindow` caller from the trusted native window label.
    pub(crate) fn grant_native_drop_files(
        &self,
        caller: &CallerContext,
        scope: PermissionScope,
        paths: Vec<PathBuf>,
        purpose: FileGrantPurpose,
    ) -> Result<Vec<FileGrant>, AppError> {
        require_human(caller)?;
        self.require_caller_scope(caller, &scope)?;
        if paths.is_empty() {
            return Err(AppError::invalid_argument(
                "At least one dropped file is required",
            ));
        }
        let identities = paths
            .iter()
            .map(|path| canonical_file_identity(path))
            .collect::<Result<Vec<_>, _>>()?;
        let now = now_ms();
        let grants = identities
            .into_iter()
            .map(|identity| FileGrant {
                grant_id: Uuid::new_v4().to_string(),
                scope: scope.clone(),
                path: identity.canonical_path.clone(),
                identity,
                purpose,
                created_at_ms: now,
            })
            .collect::<Vec<_>>();
        let mut inner = self.lock()?;
        for grant in &grants {
            inner.grants.insert(grant.grant_id.clone(), grant.clone());
        }
        persist_grants(&inner)?;
        Ok(grants)
    }

    /// Find and revalidate a previously approved exact-file grant. The
    /// caller-supplied path is only a lookup key; it can never mint authority.
    pub(crate) fn find_valid_grant(
        &self,
        scope: &PermissionScope,
        path: &Path,
        purpose: FileGrantPurpose,
    ) -> Result<FileGrant, AppError> {
        self.require_active_scope(scope)?;
        let identity = canonical_file_identity(path)?;
        let grant_id = {
            let inner = self.lock()?;
            inner
                .grants
                .values()
                .find(|grant| {
                    grant.scope == *scope && grant.purpose == purpose && grant.identity == identity
                })
                .map(|grant| grant.grant_id.clone())
        }
        .ok_or_else(|| {
            AppError::new(
                ErrorCode::PermissionDenied,
                "The file has not been approved by a native file selection",
            )
        })?;
        self.validate_grant(scope, &grant_id, Some(purpose))
    }

    /// Start a native approval for paths supplied by an agent. Metadata
    /// identities are captured for the authoritative prompt, but no content
    /// is read and no persistent grant is minted until the trusted UI answer
    /// is consumed.
    pub fn request_file_grant(
        &self,
        caller: &CallerContext,
        scope: PermissionScope,
        run_id: Option<String>,
        paths: Vec<PathBuf>,
        purpose: FileGrantPurpose,
    ) -> Result<PermissionRequest, AppError> {
        require_agent(caller)?;
        self.require_caller_scope(caller, &scope)?;
        self.validate_run(&scope, run_id.as_deref())?;
        if paths.is_empty() {
            return Err(AppError::invalid_argument("At least one file is required"));
        }
        let mut identities = Vec::with_capacity(paths.len());
        for path in &paths {
            identities.push(canonical_file_identity(path)?);
        }
        let details = PermissionDetails {
            operation: PermissionOperation::FileGrant,
            path: None,
            paths: identities
                .iter()
                .map(|identity| identity.canonical_path.clone())
                .collect(),
            canonical_executable: None,
            arguments: Vec::new(),
            cwd: None,
            url: None,
            method: None,
            body_sha256: None,
            body_bytes: 0,
            target_identity: None,
            offset: None,
            length: None,
            timeout_ms: None,
            overwrite: false,
        };
        self.insert_pending(
            scope,
            run_id,
            caller.tool_call_id.clone(),
            details,
            ExactOperation::FileGrant {
                paths: identities,
                purpose,
            },
        )
    }

    /// Consume a decided file approval exactly once, revalidate every
    /// identity, and persist all resulting grants atomically as one batch.
    pub fn consume_file_grant(
        &self,
        caller: &CallerContext,
        scope: &PermissionScope,
        run_id: Option<&str>,
        operation_id: &str,
    ) -> Result<Vec<FileGrant>, AppError> {
        require_agent(caller)?;
        self.require_caller_scope(caller, scope)?;
        self.validate_run(scope, run_id)?;
        let exact = self.consume(operation_id, scope, run_id)?;
        let (identities, purpose) = match exact {
            ExactOperation::FileGrant { paths, purpose } => (paths, purpose),
            _ => {
                return Err(AppError::new(
                    ErrorCode::PermissionDenied,
                    "The approval is not a file grant",
                ))
            }
        };
        for identity in &identities {
            let current = canonical_file_identity(Path::new(&identity.canonical_path))?;
            if current != *identity {
                return Err(AppError::new(
                    ErrorCode::AssetUnavailable,
                    "A selected file changed before approval was consumed",
                ));
            }
        }
        let now = now_ms();
        let grants = identities
            .into_iter()
            .map(|identity| FileGrant {
                grant_id: Uuid::new_v4().to_string(),
                scope: scope.clone(),
                path: identity.canonical_path.clone(),
                identity,
                purpose,
                created_at_ms: now,
            })
            .collect::<Vec<_>>();
        let mut inner = self.lock()?;
        for grant in &grants {
            inner.grants.insert(grant.grant_id.clone(), grant.clone());
        }
        persist_grants(&inner)?;
        Ok(grants)
    }

    pub fn validate_grant(
        &self,
        scope: &PermissionScope,
        grant_id: &str,
        expected_purpose: Option<FileGrantPurpose>,
    ) -> Result<FileGrant, AppError> {
        self.require_active_scope(scope)?;
        let grant = {
            let inner = self.lock()?;
            inner.grants.get(grant_id).cloned().ok_or_else(|| {
                AppError::new(ErrorCode::PermissionDenied, "The file is not granted")
            })?
        };
        if grant.scope != *scope || expected_purpose.is_some_and(|purpose| grant.purpose != purpose)
        {
            return Err(AppError::new(
                ErrorCode::PermissionDenied,
                "The file grant belongs to another workspace or purpose",
            ));
        }
        let current = canonical_file_identity(Path::new(&grant.path))?;
        if current != grant.identity {
            return Err(AppError::new(
                ErrorCode::AssetUnavailable,
                "The granted file changed after approval; relink or grant it again",
            ));
        }
        Ok(grant)
    }

    pub fn revoke_file_grants(&self, scope: &PermissionScope) -> Result<(), AppError> {
        self.require_active_scope(scope)?;
        let mut inner = self.lock()?;
        inner.grants.retain(|_, grant| grant.scope != *scope);
        persist_grants(&inner)
    }

    pub fn evidence_grant(
        &self,
        caller: &CallerContext,
        scope: PermissionScope,
        provider_id: String,
        account_id: String,
        allow: bool,
    ) -> Result<EvidenceGrantResult, AppError> {
        require_human(caller)?;
        self.require_caller_scope(caller, &scope)?;
        validate_provider_account(&provider_id, &account_id)?;
        let key = EvidenceKey {
            workspace_id: scope.workspace_id.clone(),
            project_id: scope.project_id.clone(),
            provider_id: provider_id.clone(),
            account_id: account_id.clone(),
        };
        let mut inner = self.lock()?;
        if allow {
            inner.evidence.insert(
                key,
                EvidenceGrant {
                    scope,
                    provider_id: provider_id.clone(),
                    account_id: account_id.clone(),
                    granted_at_ms: now_ms(),
                },
            );
        } else {
            inner.evidence.remove(&key);
        }
        persist_evidence(&inner)?;
        let result = EvidenceGrantResult {
            provider_id,
            account_id,
            allowed: allow,
        };
        let _ = inner
            .events
            .send(PermissionEvent::EvidenceChanged(result.clone()));
        Ok(result)
    }

    pub fn evidence_allowed(
        &self,
        scope: &PermissionScope,
        provider_id: &str,
        account_id: &str,
    ) -> bool {
        if self.require_active_scope(scope).is_err()
            || provider_id.is_empty()
            || account_id.is_empty()
        {
            return false;
        }
        self.inner
            .lock()
            .ok()
            .and_then(|inner| {
                inner
                    .evidence
                    .get(&EvidenceKey {
                        workspace_id: scope.workspace_id.clone(),
                        project_id: scope.project_id.clone(),
                        provider_id: provider_id.to_owned(),
                        account_id: account_id.to_owned(),
                    })
                    .map(|grant| grant.scope.generation == scope.generation)
            })
            .unwrap_or(false)
    }

    pub fn require_evidence(
        &self,
        scope: &PermissionScope,
        provider_id: &str,
        account_id: &str,
    ) -> Result<(), AppError> {
        if self.evidence_allowed(scope, provider_id, account_id) {
            Ok(())
        } else {
            Err(AppError::new(
                ErrorCode::PermissionDenied,
                "Evidence sharing is not approved for this provider, account, and workspace",
            ))
        }
    }

    pub fn revoke_evidence(
        &self,
        caller: &CallerContext,
        scope: &PermissionScope,
        provider_id: Option<&str>,
        account_id: Option<&str>,
    ) -> Result<(), AppError> {
        require_human(caller)?;
        self.require_caller_scope(caller, scope)?;
        let mut inner = self.lock()?;
        inner.evidence.retain(|key, _| {
            if key.workspace_id != scope.workspace_id || key.project_id != scope.project_id {
                return true;
            }
            if provider_id.is_some_and(|value| key.provider_id != value) {
                return true;
            }
            if account_id.is_some_and(|value| key.account_id != value) {
                return true;
            }
            false
        });
        persist_evidence(&inner)
    }

    /// Start an approval. Only a supervised sidecar can ask for system
    /// authority. The operation is exact and receives a fresh UUID and a
    /// 120-second expiry; no caller-selected operation id is accepted.
    pub fn request_system_read(
        &self,
        caller: &CallerContext,
        scope: PermissionScope,
        run_id: Option<String>,
        request: &SystemReadRequest,
    ) -> Result<PermissionRequest, AppError> {
        require_agent(caller)?;
        self.require_caller_scope(caller, &scope)?;
        self.validate_run(&scope, run_id.as_deref())?;
        let identity = canonical_file_identity(Path::new(&request.path))?;
        let length = checked_length(request.length)?;
        let exact = ExactOperation::SystemRead {
            identity: identity.clone(),
            offset: request.offset,
            length,
        };
        let details = PermissionDetails {
            operation: PermissionOperation::SystemRead,
            path: Some(identity.canonical_path.clone()),
            paths: Vec::new(),
            canonical_executable: None,
            arguments: Vec::new(),
            cwd: None,
            url: None,
            method: None,
            body_sha256: None,
            body_bytes: length,
            target_identity: Some(identity),
            offset: Some(request.offset),
            length: Some(length),
            timeout_ms: None,
            overwrite: false,
        };
        self.insert_pending(scope, run_id, caller.tool_call_id.clone(), details, exact)
    }

    /// Request approval for a native export destination. The destination is
    /// canonicalized now, and its parent identity is retained privately for
    /// descriptor-relative installation. Human UI exports may omit `run_id`;
    /// agent exports must provide a live run id.
    pub fn request_export_destination(
        &self,
        caller: &CallerContext,
        scope: PermissionScope,
        run_id: Option<String>,
        path: PathBuf,
        overwrite: bool,
    ) -> Result<PermissionRequest, AppError> {
        self.require_caller_scope(caller, &scope)?;
        self.validate_export_caller(caller, &scope, run_id.as_deref())?;
        let target = canonical_target_identity(&path, overwrite)?;
        let parent_path = Path::new(&target.canonical_path)
            .parent()
            .ok_or_else(|| AppError::invalid_argument("The export destination has no parent"))?;
        let parent = canonical_directory(parent_path)?;
        let details = PermissionDetails {
            operation: if overwrite {
                PermissionOperation::Overwrite
            } else {
                PermissionOperation::SystemWrite
            },
            path: Some(target.canonical_path.clone()),
            paths: Vec::new(),
            canonical_executable: None,
            arguments: Vec::new(),
            cwd: None,
            url: None,
            method: None,
            body_sha256: None,
            body_bytes: 0,
            target_identity: Some(target.clone()),
            offset: None,
            length: None,
            timeout_ms: None,
            overwrite,
        };
        self.insert_pending(
            scope,
            run_id,
            caller.tool_call_id.clone(),
            details,
            ExactOperation::ExportDestination {
                target,
                parent,
                overwrite,
            },
        )
    }

    /// Consume an approved destination exactly once and return a non-
    /// serializable install capability. `expected_identity`, when supplied,
    /// must match the identity shown in the approval record.
    pub fn consume_export_destination(
        &self,
        caller: &CallerContext,
        scope: &PermissionScope,
        run_id: Option<&str>,
        operation_id: &str,
        path: &Path,
        expected_identity: Option<&FileIdentity>,
    ) -> Result<ExportDestinationGrant, AppError> {
        self.require_caller_scope(caller, scope)?;
        self.validate_export_caller(caller, scope, run_id)?;
        let exact = self.consume(operation_id, scope, run_id)?;
        let (target, parent, overwrite) = match exact {
            ExactOperation::ExportDestination {
                target,
                parent,
                overwrite,
            } => (target, parent, overwrite),
            _ => {
                return Err(AppError::new(
                    ErrorCode::PermissionDenied,
                    "The approval is not an export destination",
                ))
            }
        };
        let canonical_path = canonical_target_identity(path, overwrite)?;
        if canonical_path.canonical_path != target.canonical_path
            || expected_identity.is_some_and(|expected| expected != &target)
            || canonical_path != target
        {
            return Err(AppError::new(
                ErrorCode::PermissionDenied,
                "The export destination changed before installation",
            ));
        }
        Ok(ExportDestinationGrant {
            destination: PathBuf::from(target.canonical_path.clone()),
            target,
            parent,
            overwrite,
        })
    }

    fn validate_export_caller(
        &self,
        caller: &CallerContext,
        scope: &PermissionScope,
        run_id: Option<&str>,
    ) -> Result<(), AppError> {
        match caller.kind {
            CallerKind::HumanWindow { .. } => {
                if run_id.is_some() {
                    return Err(AppError::invalid_argument(
                        "Human export approvals cannot carry an assistant run id",
                    ));
                }
                Ok(())
            }
            CallerKind::AgentSidecar { .. } => {
                require_agent(caller)?;
                self.validate_run(scope, run_id)
            }
            _ => Err(AppError::new(
                ErrorCode::PermissionDenied,
                "The export caller is not trusted",
            )),
        }
    }

    pub fn request_system_write(
        &self,
        caller: &CallerContext,
        scope: PermissionScope,
        run_id: Option<String>,
        request: &SystemWriteRequest,
    ) -> Result<PermissionRequest, AppError> {
        require_agent(caller)?;
        self.require_caller_scope(caller, &scope)?;
        self.validate_run(&scope, run_id.as_deref())?;
        if request.data.len() > MAX_WRITE_BYTES {
            return Err(AppError::invalid_argument(
                "system_write data exceeds 1 MiB",
            ));
        }
        let target = canonical_target_identity(Path::new(&request.path), request.overwrite)?;
        let hash = sha256_hex(&request.data);
        let exact = ExactOperation::SystemWrite {
            target: target.clone(),
            data_sha256: hash.clone(),
            data_len: request.data.len(),
            overwrite: request.overwrite,
        };
        let details = PermissionDetails {
            operation: if request.overwrite {
                PermissionOperation::Overwrite
            } else {
                PermissionOperation::SystemWrite
            },
            path: Some(target.canonical_path.clone()),
            paths: Vec::new(),
            canonical_executable: None,
            arguments: Vec::new(),
            cwd: None,
            url: None,
            method: None,
            body_sha256: Some(hash),
            body_bytes: request.data.len() as u64,
            target_identity: Some(target),
            offset: None,
            length: None,
            timeout_ms: None,
            overwrite: request.overwrite,
        };
        self.insert_pending(scope, run_id, caller.tool_call_id.clone(), details, exact)
    }

    pub fn request_system_execute(
        &self,
        caller: &CallerContext,
        scope: PermissionScope,
        run_id: Option<String>,
        request: &SystemExecuteRequest,
    ) -> Result<PermissionRequest, AppError> {
        require_agent(caller)?;
        self.require_caller_scope(caller, &scope)?;
        self.validate_run(&scope, run_id.as_deref())?;
        let executable = canonical_executable(Path::new(&request.executable))?;
        let cwd = canonical_directory(Path::new(&request.cwd))?;
        let environment = sanitize_environment(&request.environment)?;
        let timeout_ms = normalize_timeout(request.timeout_ms)?;
        let details = PermissionDetails {
            operation: PermissionOperation::SystemExecute,
            path: None,
            paths: Vec::new(),
            canonical_executable: Some(executable.canonical_path.clone()),
            arguments: request
                .arguments
                .iter()
                .map(|arg| redact_display(arg))
                .collect(),
            cwd: Some(cwd.canonical_path.clone()),
            url: None,
            method: None,
            body_sha256: None,
            body_bytes: 0,
            target_identity: Some(executable.clone()),
            offset: None,
            length: None,
            timeout_ms: Some(timeout_ms),
            overwrite: false,
        };
        self.insert_pending(
            scope,
            run_id,
            caller.tool_call_id.clone(),
            details,
            ExactOperation::SystemExecute {
                executable,
                arguments: request.arguments.clone(),
                cwd,
                environment,
                timeout_ms,
            },
        )
    }

    pub fn request_system_http(
        &self,
        caller: &CallerContext,
        scope: PermissionScope,
        run_id: Option<String>,
        request: &SystemHttpRequest,
    ) -> Result<PermissionRequest, AppError> {
        require_agent(caller)?;
        self.require_caller_scope(caller, &scope)?;
        self.validate_run(&scope, run_id.as_deref())?;
        let url = validate_http_url(&request.url)?;
        let method = validate_http_method(&request.method)?;
        if request.body.len() > MAX_HTTP_BODY_BYTES {
            return Err(AppError::invalid_argument("system_http body exceeds 1 MiB"));
        }
        let headers = sanitize_headers(&request.headers)?;
        let timeout_ms = normalize_timeout(request.timeout_ms)?;
        let hash = sha256_hex(&request.body);
        let details = PermissionDetails {
            operation: PermissionOperation::SystemHttp,
            path: None,
            paths: Vec::new(),
            canonical_executable: None,
            arguments: Vec::new(),
            cwd: None,
            url: Some(url.clone()),
            method: Some(method.clone()),
            body_sha256: Some(hash.clone()),
            body_bytes: request.body.len() as u64,
            target_identity: None,
            offset: None,
            length: None,
            timeout_ms: Some(timeout_ms),
            overwrite: false,
        };
        self.insert_pending(
            scope,
            run_id,
            caller.tool_call_id.clone(),
            details,
            ExactOperation::SystemHttp {
                url,
                method,
                headers,
                body_sha256: hash,
                body_len: request.body.len(),
                timeout_ms,
            },
        )
    }

    pub async fn await_decision(&self, operation_id: &str) -> Result<bool, AppError> {
        loop {
            let (notified, remaining_ms) = {
                let mut inner = self.lock()?;
                let expires_at_ms = inner
                    .pending
                    .get(operation_id)
                    .map(|record| record.request.expires_at_ms)
                    .ok_or_else(|| {
                        AppError::new(
                            ErrorCode::PermissionDenied,
                            "The permission request is no longer pending",
                        )
                    })?;
                let notify = inner
                    .waiters
                    .entry(operation_id.to_owned())
                    .or_insert_with(|| Arc::new(Notify::new()))
                    .clone();
                let notified = notify.notified_owned();
                let now = now_ms();
                if expires_at_ms <= now {
                    let notify = inner.waiters.remove(operation_id);
                    inner.pending.remove(operation_id);
                    if let Some(notify) = notify {
                        notify.notify_waiters();
                    }
                    return Err(AppError::new(
                        ErrorCode::PermissionDenied,
                        "The permission request expired",
                    ));
                }
                let decision = inner
                    .pending
                    .get(operation_id)
                    .and_then(|record| record.decision);
                if let Some(decision) = decision {
                    return Ok(decision);
                }
                (notified, expires_at_ms.saturating_sub(now))
            };
            if timeout(TokioDuration::from_millis(remaining_ms), notified)
                .await
                .is_err()
            {
                self.revoke_operation(operation_id)?;
                return Err(AppError::new(
                    ErrorCode::PermissionDenied,
                    "The permission request expired",
                ));
            }
        }
    }

    /// Only a trusted human window may answer. The operation remains pending
    /// until consumed, so an answer cannot itself execute an effect.
    pub fn answer(
        &self,
        caller: &CallerContext,
        operation_id: &str,
        allow: bool,
    ) -> Result<PermissionDecision, AppError> {
        require_human(caller)?;
        let (decision, notify) = {
            let mut inner = self.lock()?;
            let scope = inner
                .pending
                .get(operation_id)
                .map(|record| record.request.scope.clone())
                .ok_or_else(|| {
                    AppError::new(
                        ErrorCode::PermissionDenied,
                        "The permission request is no longer pending",
                    )
                })?;
            if inner
                .active_scopes
                .get(&scope.generation)
                .map(|active| &active.scope)
                != Some(&scope)
                || caller.generation != scope.generation
                || caller.project_id != scope.project_id
            {
                return Err(AppError::stale_session(
                    "The permission belongs to a retired generation or workspace",
                ));
            }
            let expired = inner
                .pending
                .get(operation_id)
                .map(|record| record.request.expires_at_ms <= now_ms() || record.consumed)
                .unwrap_or(true);
            if expired {
                let notify = inner.waiters.remove(operation_id);
                inner.pending.remove(operation_id);
                if let Some(notify) = notify {
                    notify.notify_waiters();
                }
                return Err(AppError::new(
                    ErrorCode::PermissionDenied,
                    "The permission request expired",
                ));
            }
            if inner
                .pending
                .get(operation_id)
                .and_then(|record| record.decision)
                .is_some()
            {
                return Err(AppError::new(
                    ErrorCode::PermissionDenied,
                    "The permission request was already decided",
                ));
            }
            let record = inner.pending.get_mut(operation_id).ok_or_else(|| {
                AppError::new(
                    ErrorCode::PermissionDenied,
                    "The permission request is no longer pending",
                )
            })?;
            record.decision = Some(allow);
            let decision = PermissionDecision {
                operation_id: operation_id.to_owned(),
                allow,
            };
            let notify = inner.waiters.get(operation_id).cloned();
            (decision, notify)
        };
        if let Some(notify) = notify {
            notify.notify_waiters();
        }
        if let Ok(inner) = self.inner.lock() {
            let _ = inner
                .events
                .send(PermissionEvent::Decided(decision.clone()));
        }
        Ok(decision)
    }

    pub fn revoke_operation(&self, operation_id: &str) -> Result<(), AppError> {
        let (record, notify) = {
            let mut inner = self.lock()?;
            (
                inner.pending.remove(operation_id),
                inner.waiters.remove(operation_id),
            )
        };
        if let Some(notify) = notify {
            notify.notify_waiters();
        }
        if let Some(record) = record {
            if let Ok(inner) = self.inner.lock() {
                let _ = inner.events.send(PermissionEvent::Revoked {
                    operation_id: record.request.operation_id,
                });
            }
        }
        Ok(())
    }

    /// Retire before aborting the sidecar/job. This removes every queued
    /// approval for the run, and adds a tombstone so the same run id can never
    /// be registered again in this generation.
    pub fn revoke_run(&self, generation: u64, run_id: &str) -> Result<(), AppError> {
        if run_id.is_empty() {
            return Err(AppError::invalid_argument("runId is required"));
        }
        let (revoked, notifies, commands) = {
            let mut inner = self.lock()?;
            let key = (generation, run_id.to_owned());
            inner.active_runs.remove(&key);
            inner.retired_runs.insert(key.clone(), ());
            let commands = inner.running_commands.remove(&key).unwrap_or_default();
            let revoked: Vec<String> = inner
                .pending
                .iter()
                .filter_map(|(id, record)| {
                    (record.request.scope.generation == generation
                        && record.request.run_id.as_deref() == Some(run_id))
                    .then_some(id.clone())
                })
                .collect();
            let mut notifies = Vec::new();
            for operation_id in &revoked {
                inner.pending.remove(operation_id);
                if let Some(notify) = inner.waiters.remove(operation_id) {
                    notifies.push(notify);
                }
            }
            (revoked, notifies, commands)
        };
        for command in commands {
            command.cancel();
        }
        for notify in notifies {
            notify.notify_waiters();
        }
        if let Ok(inner) = self.inner.lock() {
            for operation_id in revoked {
                let _ = inner.events.send(PermissionEvent::Revoked { operation_id });
            }
        }
        Ok(())
    }

    pub fn revoke_generation(&self, generation: u64) -> Result<(), AppError> {
        let (notifies, revoked, commands) = {
            let mut inner = self.lock()?;
            inner.active_scopes.remove(&generation);
            let active_runs: Vec<(u64, String)> = inner
                .active_runs
                .keys()
                .filter(|(value, _)| *value == generation)
                .cloned()
                .collect();
            inner
                .active_runs
                .retain(|(value, _), _| *value != generation);
            for key in active_runs {
                inner.retired_runs.insert(key, ());
            }
            let command_keys: Vec<(u64, String)> = inner
                .running_commands
                .keys()
                .filter(|(value, _)| *value == generation)
                .cloned()
                .collect();
            let mut commands = Vec::new();
            for key in command_keys {
                if let Some(mut values) = inner.running_commands.remove(&key) {
                    commands.append(&mut values);
                }
            }
            let revoked: Vec<String> = inner
                .pending
                .iter()
                .filter_map(|(id, record)| {
                    (record.request.scope.generation == generation).then_some(id.clone())
                })
                .collect();
            let mut notifies = Vec::new();
            for operation_id in &revoked {
                inner.pending.remove(operation_id);
                if let Some(notify) = inner.waiters.remove(operation_id) {
                    notifies.push(notify);
                }
            }
            inner
                .evidence
                .retain(|_, grant| grant.scope.generation != generation);
            (notifies, revoked, commands)
        };
        for command in commands {
            command.cancel();
        }
        for notify in notifies {
            notify.notify_waiters();
        }
        if let Ok(inner) = self.inner.lock() {
            for operation_id in revoked {
                let _ = inner.events.send(PermissionEvent::Revoked { operation_id });
            }
            persist_evidence(&inner)?;
        }
        Ok(())
    }
    /// Lifecycle-facing name used by state/assistant teardown. It preserves
    /// retired run tombstones until the next scope registration.
    pub(crate) fn retire_generation(&self, generation: u64) -> Result<(), AppError> {
        self.revoke_generation(generation)
    }

    pub fn consume_read(
        &self,
        caller: &CallerContext,
        scope: &PermissionScope,
        run_id: Option<&str>,
        request: &SystemReadRequest,
    ) -> Result<PermissionFlow<SystemReadReply>, AppError> {
        require_agent(caller)?;
        self.require_caller_scope(caller, scope)?;
        self.validate_run(scope, run_id)?;
        let operation_id = match request.operation_id.as_deref() {
            Some(id) => id,
            None => {
                return Ok(PermissionFlow::Pending(self.request_system_read(
                    caller,
                    scope.clone(),
                    run_id.map(str::to_owned),
                    request,
                )?));
            }
        };
        let exact = self.consume(operation_id, scope, run_id)?;
        let (identity, offset, length) = match exact {
            ExactOperation::SystemRead {
                identity,
                offset,
                length,
            } => (identity, offset, length),
            _ => {
                return Err(AppError::new(
                    ErrorCode::PermissionDenied,
                    "The approval does not match a read",
                ))
            }
        };
        let (data, eof) = read_bounded_range(&identity, offset, length)?;
        Ok(PermissionFlow::Ready(SystemReadReply {
            path: identity.canonical_path,
            offset,
            data,
            eof,
        }))
    }

    pub fn consume_write(
        &self,
        caller: &CallerContext,
        scope: &PermissionScope,
        run_id: Option<&str>,
        request: &SystemWriteRequest,
    ) -> Result<PermissionFlow<SystemWriteReply>, AppError> {
        require_agent(caller)?;
        self.require_caller_scope(caller, scope)?;
        self.validate_run(scope, run_id)?;
        if request.data.len() > MAX_WRITE_BYTES {
            return Err(AppError::invalid_argument(
                "system_write data exceeds 1 MiB",
            ));
        }
        let operation_id = match request.operation_id.as_deref() {
            Some(id) => id,
            None => {
                return Ok(PermissionFlow::Pending(self.request_system_write(
                    caller,
                    scope.clone(),
                    run_id.map(str::to_owned),
                    request,
                )?))
            }
        };
        let exact = self.consume(operation_id, scope, run_id)?;
        let (target, hash, data_len, overwrite) = match exact {
            ExactOperation::SystemWrite {
                target,
                data_sha256,
                data_len,
                overwrite,
            } => (target, data_sha256, data_len, overwrite),
            _ => {
                return Err(AppError::new(
                    ErrorCode::PermissionDenied,
                    "The approval does not match a write",
                ))
            }
        };
        if data_len != request.data.len()
            || hash != sha256_hex(&request.data)
            || overwrite != request.overwrite
        {
            return Err(AppError::new(
                ErrorCode::PermissionDenied,
                "Write arguments changed after approval",
            ));
        }
        atomic_write_approved(
            Path::new(&target.canonical_path),
            &request.data,
            &target,
            overwrite,
        )?;
        Ok(PermissionFlow::Ready(SystemWriteReply {
            path: target.canonical_path,
            bytes_written: data_len as u64,
        }))
    }

    pub async fn consume_execute(
        &self,
        caller: &CallerContext,
        scope: &PermissionScope,
        run_id: Option<&str>,
        request: &SystemExecuteRequest,
    ) -> Result<PermissionFlow<SystemExecuteReply>, AppError> {
        require_agent(caller)?;
        self.require_caller_scope(caller, scope)?;
        self.validate_run(scope, run_id)?;
        let operation_id = match request.operation_id.as_deref() {
            Some(id) => id,
            None => {
                return Ok(PermissionFlow::Pending(self.request_system_execute(
                    caller,
                    scope.clone(),
                    run_id.map(str::to_owned),
                    request,
                )?))
            }
        };
        let exact = self.consume(operation_id, scope, run_id)?;
        let (executable, arguments, cwd, environment, timeout_ms) = match exact {
            ExactOperation::SystemExecute {
                executable,
                arguments,
                cwd,
                environment,
                timeout_ms,
            } => (executable, arguments, cwd, environment, timeout_ms),
            _ => {
                return Err(AppError::new(
                    ErrorCode::PermissionDenied,
                    "The approval does not match execution",
                ))
            }
        };
        if request.arguments != arguments
            || sanitize_environment(&request.environment)? != environment
            || request.timeout_ms.unwrap_or(DEFAULT_COMMAND_TIMEOUT_MS) != timeout_ms
            || request.executable != executable.canonical_path
            || request.cwd != cwd.canonical_path
        {
            return Err(AppError::new(
                ErrorCode::PermissionDenied,
                "Command arguments changed after approval",
            ));
        }
        let current_executable = canonical_executable(Path::new(&executable.canonical_path))?;
        let current_cwd = canonical_directory(Path::new(&cwd.canonical_path))?;
        if current_executable != executable || current_cwd != cwd {
            return Err(AppError::new(
                ErrorCode::PermissionDenied,
                "The approved command target changed",
            ));
        }
        let Some(run_id) = run_id else {
            return Err(AppError::stale_session("A live assistant run is required"));
        };
        let running = self.begin_running_command(scope, run_id)?;
        let result = run_approved_command(
            &executable.canonical_path,
            &arguments,
            &cwd.canonical_path,
            &environment,
            timeout_ms,
            running.cancellation(),
        )
        .await?;
        drop(running);
        Ok(PermissionFlow::Ready(result))
    }

    pub async fn consume_http(
        &self,
        caller: &CallerContext,
        scope: &PermissionScope,
        run_id: Option<&str>,
        request: &SystemHttpRequest,
    ) -> Result<PermissionFlow<SystemHttpReply>, AppError> {
        require_agent(caller)?;
        self.require_caller_scope(caller, scope)?;
        self.validate_run(scope, run_id)?;
        if request.body.len() > MAX_HTTP_BODY_BYTES {
            return Err(AppError::invalid_argument("system_http body exceeds 1 MiB"));
        }
        let operation_id = match request.operation_id.as_deref() {
            Some(id) => id,
            None => {
                return Ok(PermissionFlow::Pending(self.request_system_http(
                    caller,
                    scope.clone(),
                    run_id.map(str::to_owned),
                    request,
                )?))
            }
        };
        let exact = self.consume(operation_id, scope, run_id)?;
        let (url, method, headers, hash, body_len, timeout_ms) = match exact {
            ExactOperation::SystemHttp {
                url,
                method,
                headers,
                body_sha256,
                body_len,
                timeout_ms,
            } => (url, method, headers, body_sha256, body_len, timeout_ms),
            _ => {
                return Err(AppError::new(
                    ErrorCode::PermissionDenied,
                    "The approval does not match HTTP",
                ))
            }
        };
        if validate_http_url(&request.url)? != url
            || validate_http_method(&request.method)? != method
            || sanitize_headers(&request.headers)? != headers
            || body_len != request.body.len()
            || hash != sha256_hex(&request.body)
            || request.timeout_ms.unwrap_or(DEFAULT_COMMAND_TIMEOUT_MS) != timeout_ms
        {
            return Err(AppError::new(
                ErrorCode::PermissionDenied,
                "HTTP arguments changed after approval",
            ));
        }
        let response =
            execute_approved_http(&url, &method, &headers, &request.body, timeout_ms).await?;
        Ok(PermissionFlow::Ready(response))
    }

    fn insert_pending(
        &self,
        scope: PermissionScope,
        run_id: Option<String>,
        tool_call_id: Option<String>,
        details: PermissionDetails,
        exact: ExactOperation,
    ) -> Result<PermissionRequest, AppError> {
        let now = now_ms();
        let request = PermissionRequest {
            operation_id: Uuid::new_v4().to_string(),
            scope,
            run_id,
            tool_call_id,
            details,
            expires_at_ms: now.saturating_add(PERMISSION_TTL.as_millis() as u64),
        };
        let mut inner = self.lock()?;
        require_active_scope_locked(&inner, &request.scope)?;
        if request.run_id.is_some() {
            require_active_run_locked(&inner, &request.scope, request.run_id.as_deref())?;
        }
        inner
            .pending
            .retain(|_, record| record.request.expires_at_ms > now && !record.consumed);
        inner
            .waiters
            .insert(request.operation_id.clone(), Arc::new(Notify::new()));
        inner.pending.insert(
            request.operation_id.clone(),
            PendingRecord {
                request: request.clone(),
                exact,
                decision: None,
                consumed: false,
            },
        );
        let _ = inner
            .events
            .send(PermissionEvent::Requested(request.clone()));
        Ok(request)
    }

    fn consume(
        &self,
        operation_id: &str,
        scope: &PermissionScope,
        run_id: Option<&str>,
    ) -> Result<ExactOperation, AppError> {
        scope.validate()?;
        let mut inner = self.lock()?;
        let record = inner.pending.get(operation_id).ok_or_else(|| {
            AppError::new(
                ErrorCode::PermissionDenied,
                "The permission request is no longer pending",
            )
        })?;
        if record.request.scope != *scope || record.request.run_id.as_deref() != run_id {
            return Err(AppError::new(
                ErrorCode::PermissionDenied,
                "The permission scope no longer matches",
            ));
        }
        if record.request.expires_at_ms <= now_ms() {
            inner.pending.remove(operation_id);
            return Err(AppError::new(
                ErrorCode::PermissionDenied,
                "The permission request expired",
            ));
        }
        if record.consumed {
            return Err(AppError::new(
                ErrorCode::PermissionDenied,
                "The permission request was already consumed",
            ));
        }
        if record.decision != Some(true) {
            return Err(AppError::new(
                ErrorCode::PermissionDenied,
                "The permission request was not approved",
            ));
        }
        let record = inner.pending.remove(operation_id).ok_or_else(|| {
            AppError::new(
                ErrorCode::PermissionDenied,
                "The permission request is no longer pending",
            )
        })?;
        inner.waiters.remove(operation_id);
        Ok(record.exact)
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, PermissionsInner>, AppError> {
        self.inner
            .lock()
            .map_err(|_| AppError::io("The permission runtime lock is unavailable"))
    }
}

fn require_active_scope_locked(
    inner: &PermissionsInner,
    scope: &PermissionScope,
) -> Result<(), AppError> {
    match inner.active_scopes.get(&scope.generation) {
        Some(active) if active.scope == *scope => Ok(()),
        _ => Err(AppError::new(
            ErrorCode::PermissionDenied,
            "The permission scope is not the active native workspace",
        )),
    }
}

fn require_caller_scope_locked(
    inner: &PermissionsInner,
    caller: &CallerContext,
    scope: &PermissionScope,
) -> Result<(), AppError> {
    require_active_scope_locked(inner, scope)?;
    if caller.generation != scope.generation || caller.project_id != scope.project_id {
        return Err(AppError::stale_session(
            "The permission scope belongs to another generation",
        ));
    }
    Ok(())
}

fn require_active_run_locked(
    inner: &PermissionsInner,
    scope: &PermissionScope,
    run_id: Option<&str>,
) -> Result<(), AppError> {
    let run_id =
        run_id.ok_or_else(|| AppError::stale_session("A live assistant run is required"))?;
    if run_id.is_empty() || run_id.len() > 256 {
        return Err(AppError::stale_session("The assistant run is invalid"));
    }
    let key = (scope.generation, run_id.to_owned());
    if inner.retired_runs.contains_key(&key) || !inner.active_runs.contains_key(&key) {
        return Err(AppError::stale_session("The assistant run is retired"));
    }
    Ok(())
}

fn validate_grant_publish_locked(
    inner: &PermissionsInner,
    caller: &CallerContext,
    scope: &PermissionScope,
) -> Result<(), AppError> {
    scope.validate()?;
    require_caller_scope_locked(inner, caller, scope)?;
    if matches!(caller.kind, CallerKind::AgentSidecar { .. }) {
        require_active_run_locked(inner, scope, caller.run_id.as_deref())?;
    }
    Ok(())
}

fn require_human(caller: &CallerContext) -> Result<(), AppError> {
    if matches!(caller.kind, CallerKind::HumanWindow { .. }) {
        Ok(())
    } else {
        Err(AppError::new(
            ErrorCode::PermissionDenied,
            "Only a trusted application window may answer permissions",
        ))
    }
}

fn require_agent(caller: &CallerContext) -> Result<(), AppError> {
    if matches!(caller.kind, CallerKind::AgentSidecar { .. }) {
        Ok(())
    } else {
        Err(AppError::new(
            ErrorCode::PermissionDenied,
            "External operations must originate from the supervised assistant",
        ))
    }
}

fn validate_run_scope(scope: &PermissionScope, run_id: Option<&str>) -> Result<(), AppError> {
    if run_id.is_some_and(|id| id.is_empty() || id.len() > 256) {
        return Err(AppError::invalid_argument("runId is invalid"));
    }
    if scope.project_id.is_none() && run_id.is_some() {
        return Err(AppError::stale_session(
            "An assistant run requires an open project",
        ));
    }
    Ok(())
}

fn validate_provider_account(provider_id: &str, account_id: &str) -> Result<(), AppError> {
    if provider_id.is_empty()
        || provider_id.len() > 128
        || account_id.is_empty()
        || account_id.len() > 256
    {
        return Err(AppError::invalid_argument(
            "providerId and accountId are required",
        ));
    }
    Ok(())
}

fn checked_length(length: u64) -> Result<u64, AppError> {
    if length == 0 || length > MAX_READ_BYTES {
        Err(AppError::invalid_argument(
            "Read length must be between 1 byte and 1 MiB",
        ))
    } else {
        Ok(length)
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u64::MAX as u128) as u64
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let digest = hasher.finalize();
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn canonical_file_identity(path: &Path) -> Result<FileIdentity, AppError> {
    if !path.is_absolute() {
        return Err(AppError::invalid_argument("File paths must be absolute"));
    }
    #[cfg(not(unix))]
    {
        return Err(AppError::new(
            ErrorCode::MediaUnsupported,
            "Stable file identity checks are unavailable on this platform",
        ));
    }
    let link_metadata = fs::symlink_metadata(path).map_err(|_| {
        AppError::new(
            ErrorCode::AssetUnavailable,
            "The selected file is unavailable",
        )
    })?;
    if link_metadata.file_type().is_symlink() {
        return Err(AppError::new(
            ErrorCode::PermissionDenied,
            "Symlinked files require selecting the target directly",
        ));
    }
    if !link_metadata.is_file() {
        return Err(AppError::invalid_argument(
            "The selected path must be a regular file",
        ));
    }

    let canonical = fs::canonicalize(path).map_err(|_| {
        AppError::new(
            ErrorCode::AssetUnavailable,
            "The selected file could not be resolved",
        )
    })?;
    let metadata = fs::metadata(&canonical).map_err(|_| {
        AppError::new(
            ErrorCode::AssetUnavailable,
            "The selected file metadata is unavailable",
        )
    })?;
    let token = metadata_token(&metadata);
    Ok(FileIdentity {
        canonical_path: canonical.to_string_lossy().into_owned(),
        token,
        size: metadata.len(),
    })
}

fn directory_identity_matches(current: &FileIdentity, expected: &FileIdentity) -> bool {
    if current.canonical_path != expected.canonical_path {
        return false;
    }
    #[cfg(unix)]
    {
        let current_parts: Vec<&str> = current.token.split(':').collect();
        let expected_parts: Vec<&str> = expected.token.split(':').collect();
        if current_parts.first() == Some(&"unix")
            && expected_parts.first() == Some(&"unix")
            && current_parts.len() >= 3
            && expected_parts.len() >= 3
        {
            return current_parts[1] == expected_parts[1] && current_parts[2] == expected_parts[2];
        }
    }
    current.token == expected.token
}

#[cfg(target_os = "linux")]
fn export_parent_fd(expected_parent: &FileIdentity) -> Result<File, AppError> {
    use std::ffi::CString;
    use std::os::unix::io::FromRawFd;
    let path = CString::new(expected_parent.canonical_path.as_bytes())
        .map_err(|_| AppError::invalid_argument("The export parent path is invalid"))?;
    let fd = unsafe {
        libc::open(
            path.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW,
        )
    };
    if fd < 0 {
        return Err(AppError::new(
            ErrorCode::PermissionDenied,
            "The export destination directory could not be opened safely",
        ));
    }
    let directory = unsafe { File::from_raw_fd(fd) };
    let metadata = directory
        .metadata()
        .map_err(|_| AppError::io("The export destination directory metadata is unavailable"))?;
    let current = FileIdentity {
        canonical_path: expected_parent.canonical_path.clone(),
        token: directory_token(&metadata),
        size: metadata.len(),
    };
    if !directory_identity_matches(&current, expected_parent) {
        return Err(AppError::new(
            ErrorCode::PermissionDenied,
            "The export destination directory changed",
        ));
    }
    Ok(directory)
}

#[cfg(target_os = "linux")]
fn export_name(path: &Path, message: &'static str) -> Result<std::ffi::CString, AppError> {
    use std::ffi::CString;
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| AppError::invalid_argument(message))?;
    CString::new(name).map_err(|_| AppError::invalid_argument(message))
}

#[cfg(target_os = "linux")]
fn target_identity_at(
    directory_fd: std::os::unix::io::RawFd,
    name: &std::ffi::CStr,
    canonical_path: &str,
) -> Result<Option<FileIdentity>, AppError> {
    use std::os::unix::io::FromRawFd;
    let fd = unsafe {
        libc::openat(
            directory_fd,
            name.as_ptr(),
            libc::O_RDONLY | libc::O_CLOEXEC | libc::O_NOFOLLOW,
        )
    };
    if fd < 0 {
        if std::io::Error::last_os_error().raw_os_error() == Some(libc::ENOENT) {
            return Ok(None);
        }
        return Err(AppError::new(
            ErrorCode::PermissionDenied,
            "The export destination could not be opened safely",
        ));
    }
    let file = unsafe { File::from_raw_fd(fd) };
    let metadata = file
        .metadata()
        .map_err(|_| AppError::io("The export destination metadata is unavailable"))?;
    if !metadata.is_file() {
        return Err(AppError::new(
            ErrorCode::PermissionDenied,
            "The export destination must be a regular file",
        ));
    }
    Ok(Some(FileIdentity {
        canonical_path: canonical_path.to_owned(),
        token: metadata_token(&metadata),
        size: metadata.len(),
    }))
}

#[cfg(target_os = "linux")]
fn backup_export_target(
    destination: &Path,
    expected: &FileIdentity,
    expected_parent: &FileIdentity,
    overwrite: bool,
) -> Result<BackupCapability, AppError> {
    if !overwrite || expected.token.starts_with("missing:") {
        return Ok(BackupCapability {
            destination: destination.to_path_buf(),
            backup_name: None,
            parent: expected_parent.clone(),
            original: expected.clone(),
        });
    }
    let destination_name = export_name(destination, "The export destination filename is invalid")?;
    let directory = export_parent_fd(expected_parent)?;
    let directory_fd = std::os::unix::io::AsRawFd::as_raw_fd(&directory);
    let current = target_identity_at(directory_fd, &destination_name, &expected.canonical_path)?
        .ok_or_else(|| {
            AppError::new(
                ErrorCode::PermissionDenied,
                "The approved export target disappeared",
            )
        })?;
    if current != *expected {
        return Err(AppError::new(
            ErrorCode::PermissionDenied,
            "The approved export target changed before backup",
        ));
    }
    let backup_name = format!(
        ".cutterhoochee-export-backup-{}.bak",
        Uuid::new_v4().simple()
    );
    let backup_c = std::ffi::CString::new(backup_name.as_bytes())
        .map_err(|_| AppError::io("The export backup filename is invalid"))?;
    let result = unsafe {
        libc::linkat(
            directory_fd,
            destination_name.as_ptr(),
            directory_fd,
            backup_c.as_ptr(),
            0,
        )
    };
    if result != 0 {
        return Err(AppError::io(
            "The existing export target could not be backed up safely",
        ));
    }
    Ok(BackupCapability {
        destination: destination.to_path_buf(),
        backup_name: Some(backup_name),
        parent: expected_parent.clone(),
        original: expected.clone(),
    })
}

#[cfg(all(unix, not(target_os = "linux")))]
fn backup_export_target(
    _destination: &Path,
    _expected: &FileIdentity,
    _expected_parent: &FileIdentity,
    _overwrite: bool,
) -> Result<BackupCapability, AppError> {
    Err(AppError::new(
        ErrorCode::MediaUnsupported,
        "Descriptor-relative export backups are unavailable on this platform",
    ))
}

#[cfg(not(unix))]
fn backup_export_target(
    _destination: &Path,
    _expected: &FileIdentity,
    _expected_parent: &FileIdentity,
    _overwrite: bool,
) -> Result<BackupCapability, AppError> {
    Err(AppError::new(
        ErrorCode::MediaUnsupported,
        "Descriptor-relative export backups are unavailable on this platform",
    ))
}

#[cfg(target_os = "linux")]
fn install_approved_file(
    temporary: &Path,
    destination: &Path,
    expected: &FileIdentity,
    expected_parent: &FileIdentity,
    overwrite: bool,
) -> Result<InstallOutcome, AppError> {
    use std::os::unix::io::AsRawFd;
    let directory = export_parent_fd(expected_parent)?;
    let directory_fd = directory.as_raw_fd();
    let temporary_name = export_name(temporary, "The export temporary filename is invalid")?;
    let destination_name = export_name(destination, "The export destination filename is invalid")?;
    if temporary_name.as_c_str() == destination_name.as_c_str() {
        return Err(AppError::invalid_argument(
            "The export temporary file must differ from its destination",
        ));
    }
    let temporary_parent = temporary
        .parent()
        .ok_or_else(|| AppError::invalid_argument("The export temporary file has no parent"))?;
    let current_temporary_parent = canonical_directory(temporary_parent)?;
    if !directory_identity_matches(&current_temporary_parent, expected_parent) {
        return Err(AppError::new(
            ErrorCode::PermissionDenied,
            "The export temporary file is outside the approved directory",
        ));
    }
    let temporary_path = temporary.to_string_lossy().into_owned();
    let _temporary_identity = target_identity_at(directory_fd, &temporary_name, &temporary_path)?
        .ok_or_else(|| {
        AppError::new(
            ErrorCode::AssetUnavailable,
            "The export temporary file is unavailable",
        )
    })?;
    let current = target_identity_at(directory_fd, &destination_name, &expected.canonical_path)?;
    match current {
        Some(current) if overwrite && current == *expected => {}
        None if expected.token.starts_with("missing:") && !overwrite => {}
        _ => {
            return Err(AppError::new(
                ErrorCode::PermissionDenied,
                "The export destination changed before installation",
            ))
        }
    }
    let latest = target_identity_at(directory_fd, &destination_name, &expected.canonical_path)?;
    match latest {
        Some(latest) if overwrite && latest == *expected => {}
        None if expected.token.starts_with("missing:") && !overwrite => {}
        _ => {
            return Err(AppError::new(
                ErrorCode::PermissionDenied,
                "The export destination changed during installation",
            ))
        }
    }
    let flags = if overwrite { 0 } else { libc::RENAME_NOREPLACE };
    let result = unsafe {
        libc::syscall(
            libc::SYS_renameat2,
            directory_fd,
            temporary_name.as_ptr(),
            directory_fd,
            destination_name.as_ptr(),
            flags,
        )
    };
    if result != 0 {
        return Err(
            if !overwrite && std::io::Error::last_os_error().raw_os_error() == Some(libc::EEXIST) {
                AppError::new(
                    ErrorCode::PermissionDenied,
                    "The export destination appeared after approval",
                )
            } else {
                AppError::io("The completed export could not be installed")
            },
        );
    }
    let installed = target_identity_at(directory_fd, &destination_name, &expected.canonical_path)?
        .ok_or_else(|| AppError::io("The installed export could not be verified"))?;
    directory
        .sync_all()
        .map_err(|_| AppError::io("The export destination directory could not be flushed"))?;
    Ok(InstallOutcome {
        destination: destination.to_path_buf(),
        installed,
        original: expected.clone(),
        replaced: overwrite && !expected.token.starts_with("missing:"),
    })
}

#[cfg(all(unix, not(target_os = "linux")))]
fn install_approved_file(
    _temporary: &Path,
    _destination: &Path,
    _expected: &FileIdentity,
    _expected_parent: &FileIdentity,
    _overwrite: bool,
) -> Result<InstallOutcome, AppError> {
    Err(AppError::new(
        ErrorCode::MediaUnsupported,
        "Descriptor-relative export installation is unavailable on this platform",
    ))
}

#[cfg(not(unix))]
fn install_approved_file(
    _temporary: &Path,
    _destination: &Path,
    _expected: &FileIdentity,
    _expected_parent: &FileIdentity,
    _overwrite: bool,
) -> Result<InstallOutcome, AppError> {
    Err(AppError::new(
        ErrorCode::MediaUnsupported,
        "Descriptor-relative export installation is unavailable on this platform",
    ))
}

#[cfg(target_os = "linux")]
fn remove_backup_file(
    parent: &FileIdentity,
    backup_name: &str,
    expected: &FileIdentity,
) -> Result<(), AppError> {
    use std::ffi::CString;
    use std::os::unix::io::AsRawFd;
    let directory = export_parent_fd(parent)?;
    let name = CString::new(backup_name)
        .map_err(|_| AppError::invalid_argument("The export backup filename is invalid"))?;
    let backup_path = format!("{}/{}", parent.canonical_path, backup_name);
    let current = target_identity_at(directory.as_raw_fd(), &name, &backup_path)?
        .ok_or_else(|| AppError::io("The export backup disappeared"))?;
    if current.token != expected.token || current.size != expected.size {
        return Err(AppError::new(
            ErrorCode::PermissionDenied,
            "The export backup changed before cleanup",
        ));
    }
    let result = unsafe { libc::unlinkat(directory.as_raw_fd(), name.as_ptr(), 0) };
    if result != 0 {
        return Err(AppError::io("The export backup could not be removed"));
    }
    directory
        .sync_all()
        .map_err(|_| AppError::io("The export destination directory could not be flushed"))?;
    Ok(())
}

#[cfg(all(unix, not(target_os = "linux")))]
fn remove_backup_file(
    _parent: &FileIdentity,
    _backup_name: &str,
    _expected: &FileIdentity,
) -> Result<(), AppError> {
    Err(AppError::new(
        ErrorCode::MediaUnsupported,
        "Descriptor-relative export backups are unavailable on this platform",
    ))
}

#[cfg(not(unix))]
fn remove_backup_file(
    _parent: &FileIdentity,
    _backup_name: &str,
    _expected: &FileIdentity,
) -> Result<(), AppError> {
    Err(AppError::new(
        ErrorCode::MediaUnsupported,
        "Descriptor-relative export backups are unavailable on this platform",
    ))
}

#[cfg(target_os = "linux")]
fn restore_export_backup(
    backup: &BackupCapability,
    outcome: &InstallOutcome,
) -> Result<(), AppError> {
    use std::ffi::CString;
    use std::os::unix::io::AsRawFd;
    let directory = export_parent_fd(&backup.parent)?;
    let directory_fd = directory.as_raw_fd();
    let destination_name = export_name(
        &backup.destination,
        "The export destination filename is invalid",
    )?;
    let destination_path = outcome.destination.to_string_lossy().into_owned();
    let current = target_identity_at(directory_fd, &destination_name, &destination_path)?
        .ok_or_else(|| {
            AppError::new(
                ErrorCode::PermissionDenied,
                "The installed export disappeared",
            )
        })?;
    if current != outcome.installed {
        return Err(AppError::new(
            ErrorCode::PermissionDenied,
            "The export destination was replaced after installation",
        ));
    }

    let Some(backup_name) = backup.backup_name.as_deref() else {
        if !backup.original.token.starts_with("missing:") {
            return Err(AppError::io("The export backup state is incomplete"));
        }
        let unlink = unsafe { libc::unlinkat(directory_fd, destination_name.as_ptr(), 0) };
        if unlink != 0 {
            return Err(AppError::io(
                "The newly installed export could not be removed",
            ));
        }
        directory
            .sync_all()
            .map_err(|_| AppError::io("The export destination directory could not be flushed"))?;
        return Ok(());
    };

    let backup_c = CString::new(backup_name.as_bytes())
        .map_err(|_| AppError::invalid_argument("The export backup filename is invalid"))?;
    let backup_path = format!("{}/{}", backup.parent.canonical_path, backup_name);
    let backup_identity = target_identity_at(directory_fd, &backup_c, &backup_path)?
        .ok_or_else(|| AppError::io("The export backup disappeared"))?;
    if backup_identity.token != backup.original.token
        || backup_identity.size != backup.original.size
    {
        return Err(AppError::new(
            ErrorCode::PermissionDenied,
            "The export backup changed before rollback",
        ));
    }
    let result = unsafe {
        libc::syscall(
            libc::SYS_renameat2,
            directory_fd,
            backup_c.as_ptr(),
            directory_fd,
            destination_name.as_ptr(),
            libc::RENAME_EXCHANGE,
        )
    };
    if result != 0 {
        return Err(AppError::io("The export destination could not be restored"));
    }
    let installed_path = outcome.installed.canonical_path.clone();
    let installed_backup = target_identity_at(directory_fd, &backup_c, &installed_path)?
        .ok_or_else(|| AppError::io("The replaced export could not be verified"))?;
    if installed_backup != outcome.installed {
        return Err(AppError::new(
            ErrorCode::PermissionDenied,
            "The export rollback identity could not be verified",
        ));
    }
    let unlink = unsafe { libc::unlinkat(directory_fd, backup_c.as_ptr(), 0) };
    if unlink != 0 {
        return Err(AppError::io(
            "The temporary installed export could not be removed",
        ));
    }
    directory
        .sync_all()
        .map_err(|_| AppError::io("The export destination directory could not be flushed"))?;
    Ok(())
}

#[cfg(all(unix, not(target_os = "linux")))]
fn restore_export_backup(
    _backup: &BackupCapability,
    _outcome: &InstallOutcome,
) -> Result<(), AppError> {
    Err(AppError::new(
        ErrorCode::MediaUnsupported,
        "Descriptor-relative export rollback is unavailable on this platform",
    ))
}

#[cfg(not(unix))]
fn restore_export_backup(
    _backup: &BackupCapability,
    _outcome: &InstallOutcome,
) -> Result<(), AppError> {
    Err(AppError::new(
        ErrorCode::MediaUnsupported,
        "Descriptor-relative export rollback is unavailable on this platform",
    ))
}

fn canonical_target_identity(path: &Path, overwrite: bool) -> Result<FileIdentity, AppError> {
    if !path.is_absolute() {
        return Err(AppError::invalid_argument("Write paths must be absolute"));
    }
    if let Ok(link_metadata) = fs::symlink_metadata(path) {
        if link_metadata.file_type().is_symlink() {
            return Err(AppError::new(
                ErrorCode::PermissionDenied,
                "Symlinked targets cannot be overwritten",
            ));
        }
        if link_metadata.is_dir() {
            return Err(AppError::invalid_argument(
                "The write target must be a file",
            ));
        }
        if !overwrite {
            return Err(AppError::new(
                ErrorCode::PermissionDenied,
                "The destination exists; explicit overwrite approval is required",
            ));
        }
        return canonical_file_identity(path);
    }
    let parent = path
        .parent()
        .ok_or_else(|| AppError::invalid_argument("The write target has no parent"))?;
    let canonical_parent = canonical_directory(parent)?;
    let name = path
        .file_name()
        .ok_or_else(|| AppError::invalid_argument("The write target has no filename"))?;
    let canonical_path = Path::new(&canonical_parent.canonical_path).join(name);
    Ok(FileIdentity {
        canonical_path: canonical_path.to_string_lossy().into_owned(),
        token: format!("missing:{}", canonical_parent.token),
        size: 0,
    })
}

fn canonical_executable(path: &Path) -> Result<FileIdentity, AppError> {
    let identity = canonical_file_identity(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(&identity.canonical_path)
            .map_err(|_| AppError::io("The approved executable metadata is unavailable"))?
            .permissions()
            .mode();
        if mode & 0o111 == 0 {
            return Err(AppError::invalid_argument(
                "The executable is not executable",
            ));
        }
    }
    #[cfg(not(unix))]
    {
        return Err(AppError::new(
            ErrorCode::MediaUnsupported,
            "Stable directory identity checks are unavailable on this platform",
        ));
    }
    Ok(identity)
}

fn canonical_directory(path: &Path) -> Result<FileIdentity, AppError> {
    if !path.is_absolute() {
        return Err(AppError::invalid_argument(
            "Working directories must be absolute",
        ));
    }
    #[cfg(not(unix))]
    {
        return Err(AppError::new(
            ErrorCode::MediaUnsupported,
            "Stable directory identity checks are unavailable on this platform",
        ));
    }
    let link_metadata = fs::symlink_metadata(path).map_err(|_| {
        AppError::new(
            ErrorCode::PermissionDenied,
            "The working directory is unavailable",
        )
    })?;
    if link_metadata.file_type().is_symlink() || !link_metadata.is_dir() {
        return Err(AppError::new(
            ErrorCode::PermissionDenied,
            "The working directory must be a real directory",
        ));
    }
    let canonical = fs::canonicalize(path).map_err(|_| {
        AppError::new(
            ErrorCode::PermissionDenied,
            "The working directory could not be resolved",
        )
    })?;
    let metadata = fs::metadata(&canonical).map_err(|_| {
        AppError::new(
            ErrorCode::PermissionDenied,
            "The working directory metadata is unavailable",
        )
    })?;
    Ok(FileIdentity {
        canonical_path: canonical.to_string_lossy().into_owned(),
        token: directory_token(&metadata),
        size: 0,
    })
}

fn metadata_token(metadata: &fs::Metadata) -> String {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
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
            .and_then(|value| value.duration_since(UNIX_EPOCH).ok())
            .map(|value| value.as_nanos().to_string())
            .unwrap_or_default();
        format!(
            "file:{}:{}:{}",
            metadata.len(),
            modified,
            metadata.file_type().is_file()
        )
    }
}

fn directory_token(metadata: &fs::Metadata) -> String {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        // Directory contents change independently of the approved directory.
        // Bind its object and authority, not its mutable size or timestamps.
        format!(
            "unix-directory:{}:{}:{}:{}:{}",
            metadata.dev(),
            metadata.ino(),
            metadata.mode(),
            metadata.uid(),
            metadata.gid(),
        )
    }
    #[cfg(not(unix))]
    {
        metadata_token(metadata)
    }
}

fn read_bounded_range(
    expected: &FileIdentity,
    offset: u64,
    length: u64,
) -> Result<(Vec<u8>, bool), AppError> {
    let length = checked_length(length)?;
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    }
    let mut file = options.open(&expected.canonical_path).map_err(|_| {
        AppError::new(
            ErrorCode::AssetUnavailable,
            "The approved file could not be opened safely",
        )
    })?;
    let metadata = file
        .metadata()
        .map_err(|_| AppError::io("The approved file metadata could not be read"))?;
    if metadata_token(&metadata) != expected.token || metadata.len() != expected.size {
        return Err(AppError::new(
            ErrorCode::AssetUnavailable,
            "The approved file changed before reading",
        ));
    }
    let size = metadata.len();
    if offset > size {
        return Err(AppError::invalid_argument(
            "Read offset is outside the file",
        ));
    }
    file.seek(SeekFrom::Start(offset))
        .map_err(|_| AppError::io("The approved file could not be positioned"))?;
    let to_read = (size - offset).min(length) as usize;
    let mut data = vec![0u8; to_read];
    file.read_exact(&mut data)
        .map_err(|_| AppError::io("The approved file could not be read"))?;
    Ok((data, offset.saturating_add(to_read as u64) >= size))
}

#[cfg(unix)]
fn atomic_write_approved(
    path: &Path,
    data: &[u8],
    expected: &FileIdentity,
    overwrite: bool,
) -> Result<(), AppError> {
    use std::ffi::CString;
    use std::os::unix::io::{AsRawFd, FromRawFd};

    let parent_path = path
        .parent()
        .ok_or_else(|| AppError::invalid_argument("The write target has no parent"))?;
    let parent = canonical_directory(parent_path)?;
    let target_name = path
        .file_name()
        .ok_or_else(|| AppError::invalid_argument("The write target has no filename"))?
        .to_str()
        .ok_or_else(|| AppError::invalid_argument("The write target filename is not UTF-8"))?;
    let target_c = CString::new(target_name)
        .map_err(|_| AppError::invalid_argument("The write target filename is invalid"))?;
    let temporary_name = format!(".cutterhoochee-write-{}.tmp", Uuid::new_v4().simple());
    let temporary_c = CString::new(temporary_name.clone())
        .map_err(|_| AppError::io("The temporary write path is invalid"))?;
    let parent_c = CString::new(parent.canonical_path.as_bytes())
        .map_err(|_| AppError::io("The parent path is invalid"))?;
    let dir_fd = unsafe {
        libc::open(
            parent_c.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW,
        )
    };
    if dir_fd < 0 {
        return Err(AppError::io(
            "The destination directory could not be opened safely",
        ));
    }
    let directory = unsafe { File::from_raw_fd(dir_fd) };
    let directory_fd = directory.as_raw_fd();

    let mut target_present = false;
    let target_fd = unsafe {
        libc::openat(
            directory_fd,
            target_c.as_ptr(),
            libc::O_RDONLY | libc::O_CLOEXEC | libc::O_NOFOLLOW,
        )
    };
    if target_fd >= 0 {
        target_present = true;
        let target_file = unsafe { File::from_raw_fd(target_fd) };
        let metadata = target_file
            .metadata()
            .map_err(|_| AppError::io("The approved destination metadata is unavailable"))?;
        let current = FileIdentity {
            canonical_path: expected.canonical_path.clone(),
            token: metadata_token(&metadata),
            size: metadata.len(),
        };
        if !overwrite || current != *expected {
            return Err(AppError::new(
                ErrorCode::PermissionDenied,
                "The approved destination changed before writing",
            ));
        }
    } else if overwrite || std::io::Error::last_os_error().raw_os_error() != Some(libc::ENOENT) {
        return Err(AppError::new(
            ErrorCode::PermissionDenied,
            "The approved destination is no longer available",
        ));
    } else if expected.token != format!("missing:{}", parent.token) {
        return Err(AppError::new(
            ErrorCode::PermissionDenied,
            "The approved destination parent changed",
        ));
    }

    let temporary_fd = unsafe {
        libc::openat(
            directory_fd,
            temporary_c.as_ptr(),
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_CLOEXEC | libc::O_NOFOLLOW,
            0o600,
        )
    };
    if temporary_fd < 0 {
        return Err(AppError::io(
            "The temporary destination could not be created",
        ));
    }
    let mut temporary_file = unsafe { File::from_raw_fd(temporary_fd) };
    let result = (|| {
        temporary_file
            .write_all(data)
            .map_err(|_| AppError::io("The destination could not be written"))?;
        temporary_file
            .sync_all()
            .map_err(|_| AppError::io("The destination could not be flushed"))?;

        let latest_fd = unsafe {
            libc::openat(
                directory_fd,
                target_c.as_ptr(),
                libc::O_RDONLY | libc::O_CLOEXEC | libc::O_NOFOLLOW,
            )
        };
        if latest_fd >= 0 {
            let latest_file = unsafe { File::from_raw_fd(latest_fd) };
            let metadata = latest_file
                .metadata()
                .map_err(|_| AppError::io("The destination metadata could not be read"))?;
            let latest = FileIdentity {
                canonical_path: expected.canonical_path.clone(),
                token: metadata_token(&metadata),
                size: metadata.len(),
            };
            if !overwrite || latest != *expected {
                return Err(AppError::new(
                    ErrorCode::PermissionDenied,
                    "The approved destination changed during writing",
                ));
            }
        } else if target_present
            || std::io::Error::last_os_error().raw_os_error() != Some(libc::ENOENT)
        {
            return Err(AppError::new(
                ErrorCode::PermissionDenied,
                "The approved destination changed during writing",
            ));
        }

        #[cfg(target_os = "linux")]
        let rename_result = unsafe {
            libc::syscall(
                libc::SYS_renameat2,
                directory_fd,
                temporary_c.as_ptr(),
                directory_fd,
                target_c.as_ptr(),
                if overwrite { 0 } else { libc::RENAME_NOREPLACE },
            )
        };
        #[cfg(not(target_os = "linux"))]
        let rename_result = unsafe {
            libc::renameat(
                directory_fd,
                temporary_c.as_ptr(),
                directory_fd,
                target_c.as_ptr(),
            ) as libc::c_long
        };
        if rename_result != 0 {
            return Err(if !overwrite {
                AppError::new(
                    ErrorCode::PermissionDenied,
                    "The destination appeared after approval",
                )
            } else {
                AppError::io("The completed destination could not be installed")
            });
        }
        directory
            .sync_all()
            .map_err(|_| AppError::io("The destination directory could not be flushed"))?;
        Ok(())
    })();
    if result.is_err() {
        unsafe {
            libc::unlinkat(directory_fd, temporary_c.as_ptr(), 0);
        }
    }
    result
}

#[cfg(not(unix))]
fn atomic_write_approved(
    _path: &Path,
    _data: &[u8],
    _expected: &FileIdentity,
    _overwrite: bool,
) -> Result<(), AppError> {
    Err(AppError::new(
        ErrorCode::MediaUnsupported,
        "Atomic identity-safe writes are unavailable on this platform",
    ))
}

fn sanitize_environment(
    environment: &BTreeMap<String, String>,
) -> Result<BTreeMap<String, String>, AppError> {
    const ALLOWED: &[&str] = &[
        "LANG",
        "LC_ALL",
        "PATH",
        "HOME",
        "TMPDIR",
        "XDG_CONFIG_HOME",
        "XDG_CACHE_HOME",
    ];
    let mut result = BTreeMap::new();
    for (key, value) in environment {
        if !ALLOWED.contains(&key.as_str()) {
            return Err(AppError::new(
                ErrorCode::PermissionDenied,
                "Only the documented locale and app-directory environment variables may be set",
            ));
        }
        if key.contains('=') || value.contains('\0') || value.len() > 4096 {
            return Err(AppError::invalid_argument(
                "The command environment contains an invalid value",
            ));
        }
        result.insert(key.clone(), value.clone());
    }
    Ok(result)
}

fn normalize_timeout(timeout_ms: Option<u64>) -> Result<u64, AppError> {
    let value = timeout_ms.unwrap_or(DEFAULT_COMMAND_TIMEOUT_MS);
    if value == 0 || value > MAX_COMMAND_TIMEOUT_MS {
        return Err(AppError::invalid_argument(
            "Timeout must be between 1 ms and 10 minutes",
        ));
    }
    Ok(value)
}

fn validate_http_url(input: &str) -> Result<String, AppError> {
    let parsed = reqwest::Url::parse(input)
        .map_err(|_| AppError::invalid_argument("HTTP URL is invalid"))?;
    if !matches!(parsed.scheme(), "http" | "https") || parsed.host_str().is_none() {
        return Err(AppError::invalid_argument(
            "Only HTTP(S) URLs are supported",
        ));
    }
    if parsed.username() != "" || parsed.password().is_some() {
        return Err(AppError::new(
            ErrorCode::PermissionDenied,
            "URLs containing credentials are not allowed",
        ));
    }
    Ok(parsed.to_string())
}

fn validate_http_method(input: &str) -> Result<String, AppError> {
    let method = reqwest::Method::from_bytes(input.as_bytes())
        .map_err(|_| AppError::invalid_argument("HTTP method is invalid"))?;
    if !method.is_safe()
        && method != reqwest::Method::POST
        && method != reqwest::Method::PUT
        && method != reqwest::Method::PATCH
        && method != reqwest::Method::DELETE
    {
        return Err(AppError::new(
            ErrorCode::PermissionDenied,
            "The HTTP method is not approved for this operation",
        ));
    }
    Ok(method.as_str().to_ascii_uppercase())
}

fn sanitize_headers(
    headers: &BTreeMap<String, String>,
) -> Result<BTreeMap<String, String>, AppError> {
    let mut result = BTreeMap::new();
    for (key, value) in headers {
        let lower = key.to_ascii_lowercase();
        if !lower
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '-')
            || matches!(
                lower.as_str(),
                "authorization" | "proxy-authorization" | "cookie" | "set-cookie"
            )
            || value.contains('\r')
            || value.contains('\n')
            || value.len() > 8192
        {
            return Err(AppError::new(
                ErrorCode::PermissionDenied,
                "Sensitive or invalid HTTP headers are not accepted",
            ));
        }
        result.insert(lower, value.clone());
    }
    Ok(result)
}

async fn run_approved_command(
    executable: &str,
    arguments: &[String],
    cwd: &str,
    environment: &BTreeMap<String, String>,
    timeout_ms: u64,
    cancellation: &CommandCancellation,
) -> Result<SystemExecuteReply, AppError> {
    let mut command = Command::new(executable);
    command
        .args(arguments)
        .current_dir(cwd)
        .env_clear()
        .envs(environment)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    #[cfg(unix)]
    command.process_group(0);
    let mut child = cancellation.spawn(command)?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| AppError::io("The approved command stdout was unavailable"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| AppError::io("The approved command stderr was unavailable"))?;
    let stdout_task = tokio::spawn(read_child_output(stdout));
    let stderr_task = tokio::spawn(read_child_output(stderr));
    let wait_result = tokio::select! {
        _ = cancellation.wait_cancelled() => {
            terminate_child(&mut child).await;
            cancellation.clear_pid();
            stdout_task.abort();
            stderr_task.abort();
            return Err(AppError::stale_session("The approved command was cancelled"));
        }
        result = timeout(TokioDuration::from_millis(timeout_ms), child.wait()) => result,
    };
    let status = match wait_result {
        Ok(Ok(status)) => status,
        Ok(Err(_)) => {
            terminate_child(&mut child).await;
            let _ = stdout_task.await;
            let _ = stderr_task.await;
            cancellation.clear_pid();
            return Err(AppError::io(
                "The approved command status could not be read",
            ));
        }
        Err(_) => {
            terminate_child(&mut child).await;
            let _ = stdout_task.await;
            let _ = stderr_task.await;
            cancellation.clear_pid();
            return Ok(SystemExecuteReply {
                status: None,
                stdout: Vec::new(),
                stderr: Vec::new(),
                timed_out: true,
            });
        }
    };
    let stdout = stdout_task
        .await
        .map_err(|_| AppError::io("The approved command stdout task failed"))??;
    let stderr = stderr_task
        .await
        .map_err(|_| AppError::io("The approved command stderr task failed"))??;
    cancellation.clear_pid();
    Ok(SystemExecuteReply {
        status: status.code(),
        stdout,
        stderr,
        timed_out: false,
    })
}

async fn read_child_output<R>(mut reader: R) -> Result<Vec<u8>, AppError>
where
    R: tokio::io::AsyncRead + Unpin,
{
    let mut result = Vec::new();
    let mut chunk = [0u8; 16 * 1024];
    loop {
        let read = reader
            .read(&mut chunk)
            .await
            .map_err(|_| AppError::io("The approved command output could not be read"))?;
        if read == 0 {
            return Ok(result);
        }
        if result.len().saturating_add(read) > MAX_COMMAND_OUTPUT_BYTES {
            return Err(AppError::new(
                ErrorCode::ProviderError,
                "The approved command output exceeded the 1 MiB limit",
            ));
        }
        result.extend_from_slice(&chunk[..read]);
    }
}

#[cfg(unix)]
fn signal_process_group(pid: u32, signal: i32) {
    if pid == 0 {
        return;
    }
    unsafe {
        libc::kill(-(pid as i32), signal);
    }
}

async fn terminate_child(child: &mut Child) {
    #[cfg(unix)]
    let pid = child.id();
    #[cfg(unix)]
    if let Some(pid) = pid {
        signal_process_group(pid, libc::SIGTERM);
    }
    let _ = child.kill().await;
    #[cfg(unix)]
    if let Some(pid) = pid {
        signal_process_group(pid, libc::SIGKILL);
    }
}

async fn execute_approved_http(
    url: &str,
    method: &str,
    headers: &BTreeMap<String, String>,
    body: &[u8],
    timeout_ms: u64,
) -> Result<SystemHttpReply, AppError> {
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(TokioDuration::from_millis(timeout_ms))
        .build()
        .map_err(|_| AppError::io("The approved HTTP client could not be created"))?;
    let method = reqwest::Method::from_bytes(method.as_bytes())
        .map_err(|_| AppError::invalid_argument("HTTP method is invalid"))?;
    let mut request = client.request(method, url).body(body.to_vec());
    for (key, value) in headers {
        request = request.header(key, value);
    }
    let response = request
        .send()
        .await
        .map_err(|_| AppError::new(ErrorCode::ProviderError, "The approved HTTP request failed"))?;
    let status = response.status().as_u16();
    let redirect_url = if response.status().is_redirection() {
        response
            .headers()
            .get(reqwest::header::LOCATION)
            .and_then(|value| value.to_str().ok())
            .map(ToOwned::to_owned)
    } else {
        None
    };
    if redirect_url.is_some() {
        return Ok(SystemHttpReply {
            status,
            content_type: None,
            body: Vec::new(),
            redirect_url,
        });
    }
    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .map(|value| value.chars().take(256).collect());
    let mut stream = response;
    let mut body_result = Vec::new();
    while let Some(chunk) = stream.chunk().await.map_err(|_| {
        AppError::new(
            ErrorCode::ProviderError,
            "The HTTP response could not be read",
        )
    })? {
        if body_result.len().saturating_add(chunk.len()) > MAX_HTTP_BODY_BYTES {
            return Err(AppError::new(
                ErrorCode::ProviderError,
                "The HTTP response exceeded the 1 MiB limit",
            ));
        }
        body_result.extend_from_slice(&chunk);
    }
    Ok(SystemHttpReply {
        status,
        content_type,
        body: body_result,
        redirect_url: None,
    })
}

fn redact_display(input: &str) -> String {
    let lower = input.to_ascii_lowercase();
    if lower.contains("token")
        || lower.contains("secret")
        || lower.contains("password")
        || lower.contains("api_key")
        || lower.starts_with("sk-")
        || lower.starts_with("bearer ")
    {
        "[REDACTED]".to_owned()
    } else {
        input.chars().take(4096).collect()
    }
}

fn load_json<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<Option<T>, AppError> {
    if !path.exists() {
        return Ok(None);
    }
    let bytes =
        fs::read(path).map_err(|_| AppError::io("Permission metadata could not be read"))?;
    serde_json::from_slice(&bytes)
        .map(Some)
        .map_err(|_| AppError::schema("Permission metadata is malformed"))
}

fn persist_grants(inner: &PermissionsInner) -> Result<(), AppError> {
    let values: Vec<_> = inner
        .grants
        .values()
        .cloned()
        .map(|grant| StoredFileGrant { grant })
        .collect();
    atomic_json_write(&inner.app_data_dir.join(PERMISSIONS_FILE), &values)
}

fn persist_evidence(inner: &PermissionsInner) -> Result<(), AppError> {
    let values: Vec<_> = inner
        .evidence
        .values()
        .cloned()
        .map(|grant| StoredEvidenceGrant { grant })
        .collect();
    atomic_json_write(&inner.app_data_dir.join(EVIDENCE_FILE), &values)
}

fn atomic_json_write<T: Serialize>(path: &Path, value: &T) -> Result<(), AppError> {
    let bytes = serde_json::to_vec(value)
        .map_err(|_| AppError::schema("Permission metadata could not be encoded"))?;
    let parent = path
        .parent()
        .ok_or_else(|| AppError::io("Permission metadata has no parent"))?;
    fs::create_dir_all(parent)
        .map_err(|_| AppError::io("Permission metadata directory could not be created"))?;
    let temporary = parent.join(format!(
        ".{}.{}.tmp",
        path.file_name().unwrap_or_default().to_string_lossy(),
        Uuid::new_v4().simple()
    ));
    {
        let mut file = File::create(&temporary)
            .map_err(|_| AppError::io("Permission metadata temporary file could not be created"))?;
        file.write_all(&bytes)
            .map_err(|_| AppError::io("Permission metadata could not be written"))?;
        file.sync_all()
            .map_err(|_| AppError::io("Permission metadata could not be flushed"))?;
    }
    fs::rename(&temporary, path).map_err(|_| {
        let _ = fs::remove_file(&temporary);
        AppError::io("Permission metadata could not be installed")
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn directory_identity_survives_content_changes_but_rejects_replacement() {
        let root = std::env::temp_dir().join(format!("cutterhoochee-directory-{}", Uuid::new_v4()));
        let cwd = root.join("cwd");
        fs::create_dir_all(&cwd).unwrap();
        let original = canonical_directory(&cwd).unwrap();
        let missing_target = canonical_target_identity(&cwd.join("output.txt"), false).unwrap();
        fs::write(cwd.join("unrelated.txt"), b"unrelated contents").unwrap();
        fs::File::open(&cwd)
            .unwrap()
            .set_times(fs::FileTimes::new().set_modified(UNIX_EPOCH + Duration::from_secs(1)))
            .unwrap();
        assert_eq!(canonical_directory(&cwd).unwrap(), original);
        assert_eq!(
            canonical_target_identity(&cwd.join("output.txt"), false).unwrap(),
            missing_target
        );
        fs::rename(&cwd, root.join("original")).unwrap();
        fs::create_dir(&cwd).unwrap();
        assert_ne!(canonical_directory(&cwd).unwrap(), original);
        assert_ne!(
            canonical_target_identity(&cwd.join("output.txt"), false).unwrap(),
            missing_target
        );
        fs::remove_dir(&cwd).unwrap();
        std::os::unix::fs::symlink(root.join("original"), &cwd).unwrap();
        assert!(canonical_directory(&cwd).is_err());
        fs::remove_dir_all(root).unwrap();
    }
    #[cfg(target_os = "linux")]
    #[test]
    fn export_install_allows_sibling_creation_but_rejects_directory_replacement() {
        let root =
            std::env::temp_dir().join(format!("cutterhoochee-export-directory-{}", Uuid::new_v4()));
        let destination_dir = root.join("destination");
        fs::create_dir_all(&destination_dir).unwrap();
        let destination = destination_dir.join("portrait.mp4");
        let parent = canonical_directory(&destination_dir).unwrap();
        let target = canonical_target_identity(&destination, false).unwrap();

        // Rendering creates hidden temporary/sibling files in this directory.
        fs::write(
            destination_dir.join(".portrait.mp4.cutterhoochee-temp"),
            b"temporary",
        )
        .unwrap();
        export_parent_fd(&parent).unwrap();

        let temporary = destination_dir.join(".portrait.mp4.cutterhoochee-ready");
        fs::write(&temporary, b"encoded portrait").unwrap();
        install_approved_file(&temporary, &destination, &target, &parent, false).unwrap();
        assert_eq!(fs::read(&destination).unwrap(), b"encoded portrait");

        fs::rename(&destination_dir, root.join("original")).unwrap();
        fs::create_dir(&destination_dir).unwrap();
        assert!(export_parent_fd(&parent).is_err());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn display_redacts_secret_like_arguments() {
        assert_eq!(redact_display("--api_key=sk-test"), "[REDACTED]");
        assert_eq!(redact_display("--format=json"), "--format=json");
    }

    #[test]
    fn http_urls_reject_embedded_credentials() {
        assert!(validate_http_url("https://user:password@example.test/a").is_err());
        assert_eq!(
            validate_http_url("https://example.test/a").unwrap(),
            "https://example.test/a"
        );
    }

    #[test]
    fn timeouts_have_a_hard_ten_minute_cap() {
        assert_eq!(normalize_timeout(None).unwrap(), DEFAULT_COMMAND_TIMEOUT_MS);
        assert!(normalize_timeout(Some(MAX_COMMAND_TIMEOUT_MS + 1)).is_err());
    }
    // `kill(-pgid, 0)` reports zombie members as present. On Linux inspect
    // process state so this regression does not depend on PID 1 reaping an
    // orphaned descendant before it can observe that no process is alive.
    #[cfg(all(unix, target_os = "linux"))]
    fn process_group_has_running_members(pgid: u32) -> bool {
        let Ok(entries) = fs::read_dir("/proc") else {
            return true;
        };
        for entry in entries.flatten() {
            let Some(name) = entry.file_name().to_str().map(ToOwned::to_owned) else {
                continue;
            };
            if name.parse::<u32>().is_err() {
                continue;
            }
            let Ok(stat) = fs::read_to_string(entry.path().join("stat")) else {
                continue;
            };
            let Some((_, fields)) = stat.rsplit_once(") ") else {
                continue;
            };
            let mut fields = fields.split_whitespace();
            let Some(state) = fields.next() else {
                continue;
            };
            let Some(_parent) = fields.next() else {
                continue;
            };
            let Some(group) = fields.next().and_then(|value| value.parse::<u32>().ok()) else {
                continue;
            };
            if group == pgid && !matches!(state, "Z" | "X") {
                return true;
            }
        }
        false
    }

    #[cfg(all(unix, not(target_os = "linux")))]
    fn process_group_has_running_members(pgid: u32) -> bool {
        unsafe { libc::kill(-(pgid as libc::pid_t), 0) == 0 }
    }

    #[cfg(unix)]
    async fn wait_for_process_group_to_stop(pgid: u32) {
        timeout(Duration::from_secs(2), async {
            loop {
                if !process_group_has_running_members(pgid) {
                    return;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("approved command process group should stop");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn revoking_run_terminates_an_approved_command_process_group() {
        let root = std::env::temp_dir().join(format!("cutterhoochee-command-{}", Uuid::new_v4()));
        fs::create_dir_all(&root).expect("command fixture directory");
        let permissions =
            PermissionsRuntime::new(root.join("permissions")).expect("permission runtime");
        let scope = PermissionScope {
            workspace_id: "command-workspace".to_owned(),
            project_id: Some("command-project".to_owned()),
            generation: 1,
        };
        permissions
            .register_scope(scope.clone())
            .expect("command scope");
        let run_id = "command-run".to_owned();
        permissions
            .register_run(&scope, &run_id)
            .expect("command run");
        let lease = permissions
            .begin_running_command(&scope, &run_id)
            .expect("command lease");
        let observed = lease.cancellation.clone();
        let task_cancellation = lease.cancellation.clone();
        let child_marker = root.join(".command-child.pid");
        let cwd = root.to_string_lossy().into_owned();
        let mut task = tokio::spawn(async move {
            let _lease = lease;
            let arguments = vec![
                "-c".to_owned(),
                r#"/usr/bin/sleep 30 & child=$!; while ! kill -0 "$child" 2>/dev/null; do :; done; printf '%s\n' "$child" > .command-child.pid; wait "$child""#.to_owned(),
            ];
            run_approved_command(
                "/bin/sh",
                &arguments,
                &cwd,
                &std::collections::BTreeMap::new(),
                120_000,
                &task_cancellation,
            )
            .await
        });
        let pid = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if let Some(pid) = observed.pid() {
                    break pid;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("approved command should spawn");
        let child_pid = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let child_pid = fs::read_to_string(&child_marker)
                    .ok()
                    .and_then(|value| value.trim().parse::<u32>().ok());
                if let Some(child_pid) = child_pid {
                    if child_pid != 0
                        && child_pid != pid
                        && unsafe { libc::getpgid(child_pid as libc::pid_t) } == pid as libc::pid_t
                    {
                        break child_pid;
                    }
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("approved command descendant should join process group");
        assert_ne!(child_pid, pid);
        assert!(process_group_has_running_members(pid));
        permissions
            .revoke_run(scope.generation, &run_id)
            .expect("revoke command run");
        let result = tokio::time::timeout(Duration::from_secs(2), &mut task).await;
        let result = match result {
            Ok(result) => result.expect("approved command task"),
            Err(_) => {
                task.abort();
                let _ = task.await;
                panic!("revoked approved command did not terminate");
            }
        };
        assert_eq!(
            result
                .expect_err("revoked approved command should fail")
                .code,
            ErrorCode::StaleSession
        );
        assert_eq!(unsafe { libc::kill(pid as libc::pid_t, 0) }, -1);
        wait_for_process_group_to_stop(pid).await;
        fs::remove_dir_all(root).expect("remove command fixture");
    }

    fn permission_test_state(root: &std::path::Path) -> AppState {
        let resource_dir = root.join("resources");
        let paths = crate::agent_bridge::AgentPaths {
            resource_dir: resource_dir.clone(),
            node_path: resource_dir.join("node"),
            agent_entrypoint: resource_dir.join("agent.js"),
            app_data_dir: root.join("app-data"),
            app_cache_dir: root.join("app-cache"),
            agent_home: root.join("app-data/agent-home"),
            agent_config_dir: root.join("app-data/agent-config"),
            agent_cache_dir: root.join("app-cache/agent-cache"),
            session_dir: root.join("app-data/sessions"),
            temp_dir: root.join("app-cache/tmp"),
            artifact_dir: root.join("app-cache/artifacts"),
        };
        let state = AppState::new(paths).expect("permission test state");
        state
            .create_project(
                &root.join("fixture.cutproj"),
                "Permission fixture".to_owned(),
                Some("16:9".to_owned()),
                Some(30),
                Some(1),
            )
            .expect("permission test project");
        state
    }

    fn read_action(path: &std::path::Path) -> PermissionsAction {
        PermissionsAction::SystemRead(SystemReadRequest {
            path: path.to_string_lossy().into_owned(),
            offset: 0,
            length: 64,
            operation_id: None,
        })
    }

    fn write_request(path: &std::path::Path, data: &[u8]) -> SystemWriteRequest {
        SystemWriteRequest {
            path: path.to_string_lossy().into_owned(),
            data: data.to_vec(),
            overwrite: true,
            operation_id: None,
        }
    }

    fn expire_pending_permission(permissions: &PermissionsRuntime, operation_id: &str) {
        let mut inner = permissions.inner.lock().expect("permission test lock");
        inner
            .pending
            .get_mut(operation_id)
            .expect("permission test pending operation")
            .request
            .expires_at_ms = now_ms().saturating_sub(1);
    }

    async fn pending_request(state: &AppState, scope: &PermissionScope) -> PermissionRequest {
        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if let Some(request) = state
                    .permissions()
                    .pending(scope)
                    .expect("pending permissions")
                    .into_iter()
                    .next()
                {
                    break request;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("permission request should become visible")
    }

    #[test]
    fn granted_system_write_permission_is_single_use() {
        let root =
            std::env::temp_dir().join(format!("cutterhoochee-permission-write-{}", Uuid::new_v4()));
        fs::create_dir_all(&root).expect("permission fixture directory");
        let fixture = root.join("fixture.txt");
        fs::write(&fixture, b"original").expect("permission fixture");
        let state = permission_test_state(&root);
        let permissions = state.permissions().clone();
        let scope = permissions
            .scope_for_state(&state)
            .expect("permission scope");
        let run_id = format!("write-{}", Uuid::new_v4());
        permissions
            .register_run(&scope, &run_id)
            .expect("write run");
        let caller = CallerContext::agent_sidecar(
            "permission-test".to_owned(),
            scope.generation,
            scope.project_id.clone(),
        )
        .with_run_id(Some(run_id.clone()));
        let ui = CallerContext::human_window(
            "main".to_owned(),
            scope.generation,
            scope.project_id.clone(),
        );
        let mut request = write_request(&fixture, b"approved once");
        let pending = permissions
            .request_system_write(&caller, scope.clone(), Some(run_id.clone()), &request)
            .expect("write permission request");
        permissions
            .answer(&ui, &pending.operation_id, true)
            .expect("write permission decision");
        request.operation_id = Some(pending.operation_id.clone());
        let first = permissions
            .consume_write(&caller, &scope, Some(&run_id), &request)
            .expect("first approved write");
        match first {
            PermissionFlow::Ready(reply) => {
                assert_eq!(reply.bytes_written, b"approved once".len() as u64)
            }
            PermissionFlow::Pending(_) => panic!("approved write should execute"),
        }
        assert_eq!(
            fs::read(&fixture).expect("read first write"),
            b"approved once"
        );
        let second_error = permissions
            .consume_write(&caller, &scope, Some(&run_id), &request)
            .expect_err("a granted write must be single-use");
        assert_eq!(second_error.code, ErrorCode::PermissionDenied);
        assert_eq!(
            fs::read(&fixture).expect("read after replay"),
            b"approved once"
        );
        drop(state);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn expired_pending_permission_cannot_be_answered_or_read() {
        let root = std::env::temp_dir().join(format!(
            "cutterhoochee-permission-expired-pending-{}",
            Uuid::new_v4()
        ));
        fs::create_dir_all(&root).expect("permission fixture directory");
        let fixture = root.join("fixture.txt");
        let original = b"original".to_vec();
        fs::write(&fixture, &original).expect("permission fixture");
        let state = permission_test_state(&root);
        let permissions = state.permissions().clone();
        let scope = permissions
            .scope_for_state(&state)
            .expect("permission scope");
        let run_id = format!("read-{}", Uuid::new_v4());
        permissions.register_run(&scope, &run_id).expect("read run");
        let caller = CallerContext::agent_sidecar(
            "permission-test".to_owned(),
            scope.generation,
            scope.project_id.clone(),
        )
        .with_run_id(Some(run_id.clone()));
        let ui = CallerContext::human_window(
            "main".to_owned(),
            scope.generation,
            scope.project_id.clone(),
        );
        let request = SystemReadRequest {
            path: fixture.to_string_lossy().into_owned(),
            offset: 0,
            length: 64,
            operation_id: None,
        };
        let pending_consume = permissions
            .request_system_read(&caller, scope.clone(), Some(run_id.clone()), &request)
            .expect("read permission request for consume");
        let pending_answer = permissions
            .request_system_read(&caller, scope.clone(), Some(run_id.clone()), &request)
            .expect("read permission request for answer");
        expire_pending_permission(&permissions, &pending_consume.operation_id);
        expire_pending_permission(&permissions, &pending_answer.operation_id);
        let replaced = root.join("original-fixture.txt");
        fs::rename(&fixture, &replaced).expect("replace expired read target");
        fs::write(&fixture, &original).expect("restore expired read target");
        let mut expired_request = request.clone();
        expired_request.operation_id = Some(pending_consume.operation_id);
        let read_error = permissions
            .consume_read(&caller, &scope, Some(&run_id), &expired_request)
            .expect_err("an expired read must not execute");
        assert_eq!(read_error.code, ErrorCode::PermissionDenied);
        assert_eq!(
            fs::read(&fixture).expect("read expired read target"),
            original
        );

        let answer_error = permissions
            .answer(&ui, &pending_answer.operation_id, true)
            .expect_err("an expired permission must not be answerable");
        assert_eq!(answer_error.code, ErrorCode::PermissionDenied);
        drop(state);
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn expired_granted_permission_cannot_authorize_or_write() {
        let root = std::env::temp_dir().join(format!(
            "cutterhoochee-permission-expired-granted-{}",
            Uuid::new_v4()
        ));
        fs::create_dir_all(&root).expect("permission fixture directory");
        let fixture = root.join("fixture.txt");
        fs::write(&fixture, b"original").expect("permission fixture");
        let state = permission_test_state(&root);
        let permissions = state.permissions().clone();
        let scope = permissions
            .scope_for_state(&state)
            .expect("permission scope");
        let run_id = format!("write-{}", Uuid::new_v4());
        permissions
            .register_run(&scope, &run_id)
            .expect("write run");
        let caller = CallerContext::agent_sidecar(
            "permission-test".to_owned(),
            scope.generation,
            scope.project_id.clone(),
        )
        .with_run_id(Some(run_id.clone()));
        let ui = CallerContext::human_window(
            "main".to_owned(),
            scope.generation,
            scope.project_id.clone(),
        );
        let mut request = write_request(&fixture, b"must not be written");
        let pending = permissions
            .request_system_write(&caller, scope.clone(), Some(run_id.clone()), &request)
            .expect("write permission request");
        permissions
            .answer(&ui, &pending.operation_id, true)
            .expect("write permission decision");
        expire_pending_permission(&permissions, &pending.operation_id);
        let decision_error = permissions
            .await_decision(&pending.operation_id)
            .await
            .expect_err("an expired grant must not authorize");
        assert_eq!(decision_error.code, ErrorCode::PermissionDenied);
        let pending_direct = permissions
            .request_system_write(&caller, scope.clone(), Some(run_id.clone()), &request)
            .expect("second write permission request");
        permissions
            .answer(&ui, &pending_direct.operation_id, true)
            .expect("second write permission decision");
        expire_pending_permission(&permissions, &pending_direct.operation_id);
        request.operation_id = Some(pending_direct.operation_id);
        let write_error = permissions
            .consume_write(&caller, &scope, Some(&run_id), &request)
            .expect_err("an expired write must not execute");
        assert_eq!(write_error.code, ErrorCode::PermissionDenied);
        assert_eq!(
            fs::read(&fixture).expect("read expired write target"),
            b"original"
        );
        drop(state);
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn system_permission_handle_waits_for_decision_and_respects_revocation() {
        let root =
            std::env::temp_dir().join(format!("cutterhoochee-permission-{}", Uuid::new_v4()));
        fs::create_dir_all(&root).expect("permission fixture directory");
        let fixture = root.join("fixture.txt");
        fs::write(&fixture, b"approved once").expect("permission fixture");
        let state = permission_test_state(&root);
        let scope = state
            .permissions()
            .scope_for_state(&state)
            .expect("permission scope");
        let ui = CallerContext::human_window(
            "main".to_owned(),
            scope.generation,
            scope.project_id.clone(),
        );

        let allow_run = format!("allow-{}", Uuid::new_v4());
        state
            .permissions()
            .register_run(&scope, &allow_run)
            .expect("allow run");
        let allow_caller = CallerContext::agent_sidecar(
            "permission-test".to_owned(),
            scope.generation,
            scope.project_id.clone(),
        )
        .with_run_id(Some(allow_run));
        let allow_state = state.clone();
        let allow_permissions = state.permissions().clone();
        let allow_action = read_action(&fixture);
        let allow_task = tokio::spawn(async move {
            allow_permissions
                .handle(&allow_action, &allow_caller, &allow_state)
                .await
        });
        let allow_request = pending_request(&state, &scope).await;
        state
            .permissions()
            .answer(&ui, &allow_request.operation_id, true)
            .expect("allow decision");
        match allow_task
            .await
            .expect("allow handle task")
            .expect("allowed read")
        {
            PermissionsReply::SystemRead(reply) => assert_eq!(reply.data, b"approved once"),
            other => panic!("unexpected allowed reply: {other:?}"),
        }
        assert!(state
            .permissions()
            .pending(&scope)
            .expect("allow pending")
            .is_empty());

        let deny_run = format!("deny-{}", Uuid::new_v4());
        state
            .permissions()
            .register_run(&scope, &deny_run)
            .expect("deny run");
        let deny_caller = CallerContext::agent_sidecar(
            "permission-test".to_owned(),
            scope.generation,
            scope.project_id.clone(),
        )
        .with_run_id(Some(deny_run));
        let deny_state = state.clone();
        let deny_permissions = state.permissions().clone();
        let deny_action = read_action(&fixture);
        let deny_task = tokio::spawn(async move {
            deny_permissions
                .handle(&deny_action, &deny_caller, &deny_state)
                .await
        });
        let deny_request = pending_request(&state, &scope).await;
        state
            .permissions()
            .answer(&ui, &deny_request.operation_id, false)
            .expect("deny decision");
        let deny_error = deny_task
            .await
            .expect("deny handle task")
            .expect_err("denied read must fail");
        assert_eq!(deny_error.code, ErrorCode::PermissionDenied);
        assert!(state
            .permissions()
            .pending(&scope)
            .expect("deny pending")
            .is_empty());

        let revoke_run = format!("revoke-{}", Uuid::new_v4());
        state
            .permissions()
            .register_run(&scope, &revoke_run)
            .expect("revoke run");
        let revoke_caller = CallerContext::agent_sidecar(
            "permission-test".to_owned(),
            scope.generation,
            scope.project_id.clone(),
        )
        .with_run_id(Some(revoke_run.clone()));
        let revoke_state = state.clone();
        let revoke_permissions = state.permissions().clone();
        let revoke_action = read_action(&fixture);
        let revoke_task = tokio::spawn(async move {
            revoke_permissions
                .handle(&revoke_action, &revoke_caller, &revoke_state)
                .await
        });
        let _ = pending_request(&state, &scope).await;
        state
            .permissions()
            .revoke_run(scope.generation, &revoke_run)
            .expect("revoke run");
        assert!(revoke_task.await.expect("revoke handle task").is_err());
        assert!(state
            .permissions()
            .pending(&scope)
            .expect("revoke pending")
            .is_empty());

        drop(state);
        let _ = fs::remove_dir_all(root);
    }
}
