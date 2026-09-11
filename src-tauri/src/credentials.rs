use crate::editor::dispatcher::{CallerContext, CallerKind};
use crate::error::{AppError, ErrorCode};
use crate::ipc::{validate_safe_integer, MAX_SAFE_INTEGER};
use fs2::FileExt;
use keyring::v1::{Entry, Error as KeyringError};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt;
use std::fs::OpenOptions;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{Mutex as AsyncMutex, OwnedSemaphorePermit, Semaphore};
use tokio::time::{sleep, timeout};
use uuid::Uuid;

const KEYRING_SERVICE: &str = "Cutterhoochee";
const LEASE_TIMEOUT: Duration = Duration::from_secs(120);
const MAX_CREDENTIAL_BYTES: usize = 128 * 1024;
const MAX_SECRET_FIELD_BYTES: usize = 64 * 1024;
const MAX_ENV_ENTRIES: usize = 64;
const MAX_ENV_KEY_BYTES: usize = 256;
const MAX_ENV_VALUE_BYTES: usize = 16 * 1024;
const MAX_ACCOUNT_ID_BYTES: usize = 512;

const SUPPORTED_PROVIDER_IDS: [&str; 3] = ["anthropic", "openai", "openai-codex"];

const RETIRE_NONE: u8 = 0;
const RETIRE_WAIT_FOR_IO: u8 = 1;
const RETIRE_DELETE_PENDING: u8 = 2;

fn provider_mode_is_session_only(base: &StorageBackend, state: &ProviderState) -> bool {
    matches!(base, StorageBackend::SessionMemory) || state.session_only.load(Ordering::Acquire)
}

/// The only credential material accepted by the native store.  This type is
/// crate-private deliberately: credential values are private sidecar data and
/// never become part of the public editor/TS IPC schema.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub(crate) enum Credential {
    #[serde(rename = "api_key")]
    ApiKey {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        key: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        env: Option<BTreeMap<String, String>>,
    },
    #[serde(rename = "oauth")]
    OAuth {
        refresh: String,
        access: String,
        expires: f64,
        #[serde(rename = "accountId", default, skip_serializing_if = "Option::is_none")]
        account_id: Option<String>,
    },
}

impl fmt::Debug for Credential {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ApiKey { env, .. } => formatter
                .debug_struct("ApiKey")
                .field("key", &"<redacted>")
                .field(
                    "env",
                    &env.as_ref().map(|values| values.keys().collect::<Vec<_>>()),
                )
                .finish(),
            Self::OAuth { account_id, .. } => formatter
                .debug_struct("OAuth")
                .field("refresh", &"<redacted>")
                .field("access", &"<redacted>")
                .field("expires", &"<redacted>")
                .field("account_id", account_id)
                .finish(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
enum CredentialType {
    #[serde(rename = "api_key")]
    ApiKey,
    #[serde(rename = "oauth")]
    OAuth,
}

impl Credential {
    fn credential_type(&self) -> CredentialType {
        match self {
            Self::ApiKey { .. } => CredentialType::ApiKey,
            Self::OAuth { .. } => CredentialType::OAuth,
        }
    }

    /// Resolve the non-secret recipient identity for a configured credential.
    ///
    /// OAuth identities come from the provider-issued account identifier. API
    /// keys are never exposed: the identity is a domain-separated SHA-256
    /// fingerprint computed only inside the native process.
    fn account_identity(&self, provider_id: &str) -> Result<String, AppError> {
        match self {
            Self::ApiKey { key: Some(key), .. } => Ok(api_key_account_id(provider_id, key)),
            Self::OAuth {
                account_id: Some(account_id),
                ..
            } => Ok(account_id.clone()),
            _ => Err(AppError::new(
                ErrorCode::AuthRequired,
                "The configured provider account identity is unavailable",
            )),
        }
    }
}

/// Non-secret metadata returned by the private credential bridge.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CredentialInfo {
    pub(crate) provider_id: String,
    #[serde(rename = "type")]
    pub(crate) credential_type: CredentialType,
    /// Stable recipient identity. This is a provider account ID for OAuth or
    /// a native-only API-key fingerprint; it is never credential material.
    #[serde(rename = "accountId")]
    pub(crate) account_id: Option<String>,
}

/// Lease data returned by `credential_lease_acquire`.  The current value is
/// intentionally private to the sidecar response and omitted when absent.
#[derive(Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CredentialLeaseInfo {
    pub(crate) lease_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) current: Option<Credential>,
    pub(crate) auth_generation: u64,
}

impl fmt::Debug for CredentialLeaseInfo {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CredentialLeaseInfo")
            .field("lease_id", &self.lease_id)
            .field("current", &self.current)
            .field("auth_generation", &self.auth_generation)
            .finish()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CredentialStorageMode {
    Keyring,
    SessionMemory,
}

#[derive(Clone)]
enum StorageBackend {
    Keyring { lock_dir: PathBuf },
    SessionMemory,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CallerIdentity {
    connection_id: String,
    generation: u64,
    project_id: Option<String>,
    run_id: Option<String>,
}
#[derive(Debug, Clone, PartialEq, Eq)]
struct SidecarIdentity {
    connection_id: String,
    generation: u64,
}

struct ActiveLease {
    lease_id: String,
    auth_generation: u64,
    owner: CallerIdentity,
    permit: OwnedSemaphorePermit,
}

struct ProviderState {
    /// This permit is retained by ActiveLease across the Node callback.  It is
    /// not a read-then-write mutex: the callback and native commit both remain
    /// inside one ownership epoch.
    semaphore: Arc<Semaphore>,
    /// Native keyring calls are serialized separately from callback ownership.
    /// Keeping this gate separate lets logout retire a lease while a blocking
    /// keyring call finishes, after which deletion runs in order.
    io_gate: Arc<AsyncMutex<()>>,
    auth_generation: Mutex<u64>,
    lease: Mutex<Option<ActiveLease>>,

    session_only: AtomicBool,
    retire_mode: AtomicU8,
}

impl ProviderState {
    fn new() -> Self {
        Self {
            semaphore: Arc::new(Semaphore::new(1)),
            io_gate: Arc::new(AsyncMutex::new(())),
            auth_generation: Mutex::new(0),
            lease: Mutex::new(None),
            session_only: AtomicBool::new(false),
            retire_mode: AtomicU8::new(RETIRE_NONE),
        }
    }
}

struct CredentialsInner {
    backend: StorageBackend,
    session_credentials: Arc<Mutex<BTreeMap<String, Credential>>>,

    providers: Mutex<HashMap<String, Arc<ProviderState>>>,
    context: Mutex<ContextState>,
}

struct ContextState {
    active: Option<SidecarIdentity>,
    /// The last retired generation.  A new sidecar must advance it; this
    /// prevents an EOF/replay from rebinding the same generation.
    last_retired_generation: Option<u64>,
}

/// Native credential storage and per-provider lease coordinator.
#[derive(Clone)]
pub struct CredentialsRuntime {
    inner: Arc<CredentialsInner>,
}

impl CredentialsRuntime {
    /// Construct the real OS-backed store.  The app-data path is used only for
    /// empty cross-process lock files; no credential material is written there.
    /// Keyring failures are returned by operations and are never silently
    /// converted into session memory.
    pub fn new(app_data_dir: PathBuf) -> Result<Self, AppError> {
        if app_data_dir.as_os_str().is_empty() {
            return Err(AppError::invalid_argument(
                "The credential store app-data directory is required",
            ));
        }
        let lock_dir = app_data_dir.join("credential-locks");
        std::fs::create_dir_all(&lock_dir).map_err(|_| storage_error())?;
        Ok(Self::from_backend(StorageBackend::Keyring { lock_dir }))
    }

    /// Explicitly opt into process/session-only storage.  This is the only
    /// fallback for an unavailable keyring; callers must present this choice to
    /// the user rather than selecting it implicitly.
    pub fn session_memory() -> Self {
        Self::from_backend(StorageBackend::SessionMemory)
    }

    /// Descriptive alias for callers presenting the explicit fallback choice.
    pub fn new_session_memory() -> Self {
        Self::session_memory()
    }

    /// Descriptive alias useful to isolated tests and native setup code.
    pub fn in_memory() -> Self {
        Self::session_memory()
    }

    pub(crate) fn storage_mode(&self) -> CredentialStorageMode {
        match &self.inner.backend {
            StorageBackend::Keyring { .. } => CredentialStorageMode::Keyring,
            StorageBackend::SessionMemory => CredentialStorageMode::SessionMemory,
        }
    }

    /// Choose the backing mode for one provider.  This is deliberately a
    /// native operation: the Node sidecar cannot create an independent
    /// in-memory overlay that bypasses lease/generation/logout authority.
    pub async fn set_session_mode(
        &self,
        provider_id: &str,
        enabled: bool,
        caller: &CallerContext,
    ) -> Result<(), AppError> {
        validate_provider(provider_id)?;
        let trusted_window = matches!(&caller.kind, CallerKind::HumanWindow { .. });
        let identity = match &caller.kind {
            CallerKind::AgentSidecar { .. } => self.ensure_sidecar(caller)?,
            CallerKind::HumanWindow { .. } => self.ensure_human_window(caller)?,
        };
        let provider = self.provider_state(provider_id)?;
        let io = provider.io_gate.lock().await;
        self.retire_provider_for_wait(&provider)?;
        let current = if trusted_window {
            self.ensure_human_window_current(caller)
        } else {
            self.ensure_identity_current(&identity)
        };
        if let Err(error) = current {
            provider
                .retire_mode
                .store(RETIRE_WAIT_FOR_IO, Ordering::Release);
            drop(io);
            return Err(error);
        }
        provider.session_only.store(enabled, Ordering::Release);
        // A mode switch is an account/storage boundary.  Never carry a
        // previous session-only credential into a newly selected mode.
        self.inner
            .session_credentials
            .lock()
            .map_err(|_| storage_error())?
            .remove(provider_id);
        drop(io);
        Ok(())
    }

    fn from_backend(backend: StorageBackend) -> Self {
        Self {
            inner: Arc::new(CredentialsInner {
                backend,
                session_credentials: Arc::new(Mutex::new(BTreeMap::new())),

                providers: Mutex::new(HashMap::new()),
                context: Mutex::new(ContextState {
                    active: None,
                    last_retired_generation: None,
                }),
            }),
        }
    }

    /// Read a credential without exposing storage internals or accepting a UI
    /// caller.  Only a trusted sidecar context can invoke this operation.
    pub async fn read(
        &self,
        provider_id: &str,
        caller: &CallerContext,
    ) -> Result<Option<Credential>, AppError> {
        validate_provider(provider_id)?;
        let identity = self.ensure_sidecar(caller)?;
        let provider = self.provider_state(provider_id)?;
        self.await_provider_ready(&provider).await?;
        let value = self.read_backend(provider_id, &provider).await?;
        self.ensure_identity_current(&identity)?;
        Ok(value)
    }

    /// Enumerate metadata only.  This reads each supported keyring entry but
    /// never returns keys, tokens, or OAuth fields.
    pub async fn list(&self, caller: &CallerContext) -> Result<Vec<CredentialInfo>, AppError> {
        let identity = self.ensure_sidecar(caller)?;
        let mut result = Vec::new();
        for provider_id in SUPPORTED_PROVIDER_IDS {
            let provider = self.provider_state(provider_id)?;
            self.await_provider_ready(&provider).await?;
            let credential = self.read_backend(provider_id, &provider).await?;
            if let Some(credential) = credential {
                let account_id = credential.account_identity(provider_id)?;
                result.push(CredentialInfo {
                    provider_id: provider_id.to_owned(),
                    credential_type: credential.credential_type(),
                    account_id: Some(account_id),
                });
            }
            self.ensure_identity_current(&identity)?;
        }
        Ok(result)
    }

    /// Return the current non-secret recipient identity for a provider.
    ///
    /// This is intentionally metadata-only and does not accept a caller
    /// context: the native supervisor uses it while binding an active run.
    /// An absent credential is the only `None` case; malformed configured
    /// credentials fail closed instead of receiving a fabricated identity.
    pub async fn account_identity(&self, provider_id: &str) -> Result<Option<String>, AppError> {
        validate_provider(provider_id)?;
        let provider = self.provider_state(provider_id)?;
        self.await_provider_ready(&provider).await?;
        let credential = self.read_backend(provider_id, &provider).await?;
        credential
            .as_ref()
            .map(|value| value.account_identity(provider_id))
            .transpose()
    }

    /// Acquire the per-provider lease and retain its owned semaphore permit
    /// until commit/release/expiry.  The returned current value is read while
    /// the lease is owned, not by an unlocked preliminary read.
    pub async fn acquire_lease(
        &self,
        provider_id: &str,
        caller: &CallerContext,
    ) -> Result<CredentialLeaseInfo, AppError> {
        validate_provider(provider_id)?;
        let identity = self.ensure_sidecar(caller)?;
        let provider = self.provider_state(provider_id)?;
        self.await_provider_ready(&provider).await?;
        let expected_generation = self.current_auth_generation(&provider)?;
        let permit = timeout(
            LEASE_TIMEOUT,
            Arc::clone(&provider.semaphore).acquire_owned(),
        )
        .await
        .map_err(|_| busy_error("The credential lease timed out waiting for ownership"))?
        .map_err(|_| stale_error("The credential lease owner was retired"))?;

        if let Err(error) = self.ensure_identity_current(&identity) {
            drop(permit);
            return Err(error);
        }
        if provider.retire_mode.load(Ordering::Acquire) != RETIRE_NONE {
            drop(permit);
            return Err(busy_error(
                "The credential provider is changing authentication state",
            ));
        }
        if self.current_auth_generation(&provider)? != expected_generation {
            drop(permit);
            return Err(stale_error(
                "The credential auth generation changed before lease acquisition",
            ));
        }

        let lease_id = Uuid::new_v4().simple().to_string();
        {
            let mut active = provider.lease.lock().map_err(|_| storage_error())?;
            if provider.retire_mode.load(Ordering::Acquire) != RETIRE_NONE {
                drop(permit);
                return Err(busy_error(
                    "The credential provider is changing authentication state",
                ));
            }
            if active.is_some() {
                drop(permit);
                return Err(busy_error("The credential provider is already leased"));
            }
            *active = Some(ActiveLease {
                lease_id: lease_id.clone(),
                auth_generation: expected_generation,
                owner: identity.clone(),
                permit,
            });
        }

        let current = match self.read_backend(provider_id, &provider).await {
            Ok(value) => value,
            Err(error) => {
                self.release_lease_internal(&provider, &lease_id);
                return Err(error);
            }
        };
        if let Err(error) = self.ensure_lease_current(
            provider_id,
            &provider,
            &lease_id,
            &identity,
            Some(expected_generation),
        ) {
            self.release_lease_internal(&provider, &lease_id);
            return Err(error);
        }

        let runtime = self.clone();
        let provider_name = provider_id.to_owned();
        let expiration_id = lease_id.clone();
        tokio::spawn(async move {
            sleep(LEASE_TIMEOUT).await;
            runtime.expire_lease(&provider_name, &expiration_id).await;
        });
        Ok(CredentialLeaseInfo {
            lease_id,
            current,
            auth_generation: expected_generation,
        })
    }

    /// Commit the desired credential under the retained lease.  The auth
    /// generation is mandatory and checked both before and after waiting for
    /// native keyring I/O, so logout cannot be overtaken by a late refresh.
    pub async fn commit_lease(
        &self,
        provider_id: &str,
        lease_id: &str,
        auth_generation: u64,
        desired: Credential,
        caller: &CallerContext,
    ) -> Result<Credential, AppError> {
        validate_provider(provider_id)?;
        validate_lease_id(lease_id)?;
        validate_safe_integer(auth_generation, "authGeneration")?;
        validate_credential(provider_id, &desired)?;
        let identity = self.ensure_sidecar(caller)?;
        let provider = self.provider_state(provider_id)?;
        self.ensure_lease_current(
            provider_id,
            &provider,
            lease_id,
            &identity,
            Some(auth_generation),
        )?;

        let io = provider.io_gate.lock().await;
        self.ensure_lease_current(
            provider_id,
            &provider,
            lease_id,
            &identity,
            Some(auth_generation),
        )?;

        self.write_backend_locked(provider_id, &provider, &desired)
            .await?;
        // Retirement may race the blocking write.  Deletion (logout) uses the
        // same I/O gate and therefore follows this write; the stale commit is
        // never reported as success and cannot win after logout.
        let still_current = self
            .ensure_lease_current(
                provider_id,
                &provider,
                lease_id,
                &identity,
                Some(auth_generation),
            )
            .is_ok();
        self.release_lease_internal(&provider, lease_id);
        drop(io);
        if !still_current {
            return Err(stale_error(
                "The credential lease was retired during commit",
            ));
        }
        Ok(desired)
    }

    /// Commit using the lease's provider identity.  The bridge deliberately
    /// keeps providerId out of commit payloads; the native lease registry is
    /// authoritative for resolving it.
    pub async fn commit_lease_for_caller(
        &self,
        lease_id: &str,
        auth_generation: u64,
        desired: Credential,
        caller: &CallerContext,
    ) -> Result<Credential, AppError> {
        validate_lease_id(lease_id)?;
        let identity = self.ensure_sidecar(caller)?;
        let (provider_id, _) = self.find_lease(lease_id, &identity)?;
        self.commit_lease(
            provider_id.as_str(),
            lease_id,
            auth_generation,
            desired,
            caller,
        )
        .await
    }

    /// Release/cancel a lease without changing the stored credential.
    pub async fn release_lease(
        &self,
        provider_id: &str,
        lease_id: &str,
        caller: &CallerContext,
    ) -> Result<(), AppError> {
        validate_provider(provider_id)?;
        validate_lease_id(lease_id)?;
        let identity = self.ensure_sidecar(caller)?;
        let provider = self.provider_state(provider_id)?;
        self.ensure_lease_current(provider_id, &provider, lease_id, &identity, None)?;
        self.release_lease_internal(&provider, lease_id);
        Ok(())
    }

    /// Release using the lease's provider identity.  This is the wire-facing
    /// form used by cancellation paths where only leaseId is transmitted.
    pub async fn release_lease_for_caller(
        &self,
        lease_id: &str,
        caller: &CallerContext,
    ) -> Result<(), AppError> {
        validate_lease_id(lease_id)?;
        let identity = self.ensure_sidecar(caller)?;
        let (provider_id, _) = self.find_lease(lease_id, &identity)?;
        self.release_lease(provider_id.as_str(), lease_id, caller)
            .await
    }

    /// Naming alias for native callers that model cancellation explicitly.
    pub async fn cancel_lease(
        &self,
        provider_id: &str,
        lease_id: &str,
        caller: &CallerContext,
    ) -> Result<(), AppError> {
        self.release_lease(provider_id, lease_id, caller).await
    }

    /// Wire-facing cancellation alias with provider lookup.
    pub async fn cancel_lease_for_caller(
        &self,
        lease_id: &str,
        caller: &CallerContext,
    ) -> Result<(), AppError> {
        self.release_lease_for_caller(lease_id, caller).await
    }

    /// Retire the provider auth generation and delete the credential.  The
    /// generation is advanced before the delete; any callback already in Node
    /// therefore fails commit validation.  Native I/O is serialized by the
    /// provider gate, so a write already in progress is followed by deletion.
    pub async fn delete(&self, provider_id: &str, caller: &CallerContext) -> Result<(), AppError> {
        validate_provider(provider_id)?;
        let identity = self.ensure_sidecar(caller)?;
        let provider = self.provider_state(provider_id)?;
        self.retire_provider_for_delete(&provider)?;
        let io = provider.io_gate.lock().await;
        if let Err(error) = self.ensure_identity_current(&identity) {
            provider
                .retire_mode
                .store(RETIRE_WAIT_FOR_IO, Ordering::Release);
            drop(io);
            return Err(error);
        }
        let result = self.delete_backend_locked(provider_id, &provider).await;
        provider.retire_mode.store(RETIRE_NONE, Ordering::Release);
        drop(io);
        result
    }

    /// Start a new account/auth epoch without deleting anything.  Login flows
    /// use this when replacing an account so old refresh callbacks cannot write
    /// into the new account's generation.
    pub async fn retire_provider(
        &self,
        provider_id: &str,
        caller: &CallerContext,
    ) -> Result<u64, AppError> {
        validate_provider(provider_id)?;
        let identity = self.ensure_sidecar(caller)?;
        let provider = self.provider_state(provider_id)?;
        let generation = self.retire_provider_for_wait(&provider)?;
        self.ensure_identity_current(&identity)?;
        Ok(generation)
    }

    /// Return whether a provider currently uses the explicit session-only
    /// backend.  This reports native state; it does not inspect Node memory.
    pub async fn session_mode(
        &self,
        provider_id: &str,
        caller: &CallerContext,
    ) -> Result<bool, AppError> {
        validate_provider(provider_id)?;
        let identity = self.ensure_sidecar(caller)?;
        let provider = self.provider_state(provider_id)?;
        let enabled = provider_mode_is_session_only(&self.inner.backend, &provider);
        self.ensure_identity_current(&identity)?;
        Ok(enabled)
    }
    /// Return the current provider auth generation to a trusted sidecar.
    pub async fn auth_generation(
        &self,
        provider_id: &str,
        caller: &CallerContext,
    ) -> Result<u64, AppError> {
        validate_provider(provider_id)?;
        let identity = self.ensure_sidecar(caller)?;
        let provider = self.provider_state(provider_id)?;
        let generation = self.current_auth_generation(&provider)?;
        self.ensure_identity_current(&identity)?;
        Ok(generation)
    }

    /// Retire a sidecar connection after EOF, protocol failure, or replacement.
    /// A mismatched old connection cannot retire a newer sidecar.
    pub fn retire_context(&self, connection_id: &str, generation: u64) -> Result<(), AppError> {
        validate_connection_id(connection_id)?;
        validate_safe_integer(generation, "generation")?;
        let should_retire = {
            let mut context = self.inner.context.lock().map_err(|_| storage_error())?;
            let matches = context.active.as_ref().is_some_and(|active| {
                active.connection_id == connection_id && active.generation == generation
            });
            if matches {
                context.active = None;
                context.last_retired_generation =
                    Some(context.last_retired_generation.unwrap_or(0).max(generation));
            }
            matches
        };
        if should_retire {
            self.retire_matching_leases(|owner| {
                owner.connection_id == connection_id && owner.generation == generation
            })?;
            self.clear_session_credentials()?;
        }
        Ok(())
    }

    /// Retire all leases belonging to one run without touching other runs.
    pub fn retire_run(&self, run_id: &str) -> Result<(), AppError> {
        validate_run_id(run_id)?;
        self.retire_matching_leases(|owner| owner.run_id.as_deref() == Some(run_id))
    }

    /// Retire all leases for a project binding.  Project IDs are stable
    /// document identifiers and are never interpreted as paths.
    pub fn retire_project(&self, project_id: &str) -> Result<(), AppError> {
        if project_id.is_empty() || project_id.len() > 256 {
            return Err(AppError::invalid_argument(
                "The project credential binding is invalid",
            ));
        }
        self.retire_matching_leases(|owner| owner.project_id.as_deref() == Some(project_id))
    }

    fn provider_state(&self, provider_id: &str) -> Result<Arc<ProviderState>, AppError> {
        let mut providers = self.inner.providers.lock().map_err(|_| storage_error())?;
        Ok(Arc::clone(
            providers
                .entry(provider_id.to_owned())
                .or_insert_with(|| Arc::new(ProviderState::new())),
        ))
    }

    fn ensure_sidecar(&self, caller: &CallerContext) -> Result<CallerIdentity, AppError> {
        let identity = match &caller.kind {
            CallerKind::AgentSidecar { connection_id } => {
                validate_connection_id(connection_id)?;
                validate_safe_integer(caller.generation, "generation")?;
                CallerIdentity {
                    connection_id: connection_id.clone(),
                    generation: caller.generation,
                    project_id: caller.project_id.clone(),
                    run_id: caller.run_id.clone(),
                }
            }
            CallerKind::HumanWindow { .. } => {
                return Err(AppError::new(
                    ErrorCode::PermissionDenied,
                    "Credential storage is available only to the supervised assistant",
                ));
            }
        };

        let switched = {
            let mut context = self.inner.context.lock().map_err(|_| storage_error())?;
            match context.active.as_ref() {
                Some(active)
                    if active.connection_id == identity.connection_id
                        && active.generation == identity.generation =>
                {
                    false
                }
                Some(active) if identity.generation > active.generation => {
                    context.active = Some(SidecarIdentity {
                        connection_id: identity.connection_id.clone(),
                        generation: identity.generation,
                    });
                    true
                }
                Some(_) => {
                    return Err(stale_error("The sidecar credential context is retired"));
                }
                None => {
                    if context
                        .last_retired_generation
                        .is_some_and(|retired| identity.generation <= retired)
                    {
                        return Err(stale_error("The sidecar credential context is retired"));
                    }
                    context.active = Some(SidecarIdentity {
                        connection_id: identity.connection_id.clone(),
                        generation: identity.generation,
                    });
                    true
                }
            }
        };
        // On an explicit sidecar replacement, the old leases are retired
        // before this caller can acquire another one.  This call is outside the
        // context mutex, so no blocking guard is ever held over a future.
        if switched {
            self.retire_matching_leases(|owner| {
                owner.connection_id != identity.connection_id
                    || owner.generation != identity.generation
            })?;
            self.clear_session_credentials()?;
        }
        Ok(identity)
    }
    /// Provider mode selection is a trusted native UI operation, but it never
    /// accepts or returns credential material.  Validate the caller's safe
    /// generation; the application dispatcher binds it to the active window.
    fn ensure_human_window(&self, caller: &CallerContext) -> Result<CallerIdentity, AppError> {
        if !matches!(&caller.kind, CallerKind::HumanWindow { .. }) {
            return Err(AppError::new(
                ErrorCode::PermissionDenied,
                "Credential mode selection requires the trusted application window",
            ));
        }
        validate_safe_integer(caller.generation, "generation")?;
        let context = self.inner.context.lock().map_err(|_| storage_error())?;
        let connection_id = match context.active.as_ref() {
            None => "trusted-window".to_owned(),
            Some(active) if active.generation == caller.generation => active.connection_id.clone(),
            Some(_) => return Err(stale_error("The credential sidecar context is retired")),
        };
        Ok(CallerIdentity {
            connection_id,
            generation: caller.generation,
            project_id: caller.project_id.clone(),
            run_id: None,
        })
    }
    fn ensure_human_window_current(&self, caller: &CallerContext) -> Result<(), AppError> {
        if !matches!(&caller.kind, CallerKind::HumanWindow { .. }) {
            return Err(AppError::new(
                ErrorCode::PermissionDenied,
                "Credential mode selection requires the trusted application window",
            ));
        }
        validate_safe_integer(caller.generation, "generation")?;
        let context = self.inner.context.lock().map_err(|_| storage_error())?;
        match context.active.as_ref() {
            Some(active) if active.generation == caller.generation => Ok(()),
            Some(_) => Err(stale_error("The credential sidecar context is retired")),
            None if context
                .last_retired_generation
                .is_none_or(|retired| caller.generation > retired) =>
            {
                Ok(())
            }
            None => Err(stale_error("The credential sidecar context is retired")),
        }
    }

    fn clear_session_credentials(&self) -> Result<(), AppError> {
        self.inner
            .session_credentials
            .lock()
            .map_err(|_| storage_error())?
            .clear();
        Ok(())
    }
    fn ensure_identity_current(&self, identity: &CallerIdentity) -> Result<(), AppError> {
        let context = self.inner.context.lock().map_err(|_| storage_error())?;
        if context.active.as_ref().is_some_and(|active| {
            active.connection_id == identity.connection_id
                && active.generation == identity.generation
        }) {
            Ok(())
        } else {
            Err(stale_error("The sidecar credential context is retired"))
        }
    }

    fn current_auth_generation(&self, provider: &Arc<ProviderState>) -> Result<u64, AppError> {
        provider
            .auth_generation
            .lock()
            .map(|generation| *generation)
            .map_err(|_| storage_error())
    }

    fn ensure_lease_current(
        &self,
        provider_id: &str,
        provider: &Arc<ProviderState>,
        lease_id: &str,
        identity: &CallerIdentity,
        expected_generation: Option<u64>,
    ) -> Result<(), AppError> {
        validate_provider(provider_id)?;
        let (owner, lease_generation, matches_id) = {
            let active = provider.lease.lock().map_err(|_| storage_error())?;
            let lease = active
                .as_ref()
                .ok_or_else(|| stale_error("The credential lease is no longer active"))?;
            (
                lease.owner.clone(),
                lease.auth_generation,
                lease.lease_id == lease_id,
            )
        };
        if !matches_id || owner != *identity {
            return Err(stale_error(
                "The credential lease belongs to another sidecar context",
            ));
        }
        if expected_generation.is_some_and(|generation| generation != lease_generation) {
            return Err(stale_error("The credential auth generation is stale"));
        }
        let current_generation = self.current_auth_generation(provider)?;
        if current_generation != lease_generation {
            return Err(stale_error("The credential auth generation is stale"));
        }
        if provider.retire_mode.load(Ordering::Acquire) != RETIRE_NONE {
            return Err(stale_error("The credential lease is being retired"));
        }
        Ok(())
    }

    fn release_lease_internal(&self, provider: &Arc<ProviderState>, lease_id: &str) {
        let removed = provider.lease.lock().ok().and_then(|mut active| {
            if active
                .as_ref()
                .is_some_and(|lease| lease.lease_id == lease_id)
            {
                active.take()
            } else {
                None
            }
        });
        drop(removed);
    }

    fn provider_entries(&self) -> Result<Vec<(String, Arc<ProviderState>)>, AppError> {
        Ok(self
            .inner
            .providers
            .lock()
            .map_err(|_| storage_error())?
            .iter()
            .map(|(provider_id, provider)| (provider_id.clone(), Arc::clone(provider)))
            .collect())
    }

    fn find_lease(
        &self,
        lease_id: &str,
        identity: &CallerIdentity,
    ) -> Result<(String, Arc<ProviderState>), AppError> {
        for (provider_id, provider) in self.provider_entries()? {
            let found = {
                let active = provider.lease.lock().map_err(|_| storage_error())?;
                active
                    .as_ref()
                    .map(|lease| (lease.lease_id == lease_id, lease.owner == *identity))
            };
            if let Some((matches_id, matches_owner)) = found {
                if matches_id && matches_owner {
                    return Ok((provider_id, provider));
                }
                if matches_id {
                    return Err(stale_error(
                        "The credential lease belongs to another sidecar context",
                    ));
                }
            }
        }
        Err(stale_error("The credential lease is no longer active"))
    }

    async fn await_provider_ready(&self, provider: &Arc<ProviderState>) -> Result<(), AppError> {
        loop {
            match provider.retire_mode.load(Ordering::Acquire) {
                RETIRE_NONE => return Ok(()),
                RETIRE_DELETE_PENDING => {
                    return Err(busy_error("The credential provider is logging out"));
                }
                RETIRE_WAIT_FOR_IO => {
                    let io = provider.io_gate.lock().await;
                    if provider.retire_mode.load(Ordering::Acquire) == RETIRE_WAIT_FOR_IO {
                        let active = provider
                            .lease
                            .lock()
                            .map_err(|_| storage_error())?
                            .is_some();
                        if !active {
                            provider.retire_mode.store(RETIRE_NONE, Ordering::Release);
                        }
                    }
                    drop(io);
                    tokio::task::yield_now().await;
                }
                _ => return Err(storage_error()),
            }
        }
    }

    fn retire_provider_for_wait(&self, provider: &Arc<ProviderState>) -> Result<u64, AppError> {
        if provider.retire_mode.load(Ordering::Acquire) == RETIRE_DELETE_PENDING {
            return Err(busy_error("The credential provider is logging out"));
        }
        provider
            .retire_mode
            .store(RETIRE_WAIT_FOR_IO, Ordering::Release);
        let generation = match self.bump_auth_generation(provider) {
            Ok(generation) => generation,
            Err(error) => {
                provider.retire_mode.store(RETIRE_NONE, Ordering::Release);
                return Err(error);
            }
        };
        let removed = provider.lease.lock().map_err(|_| storage_error())?.take();
        drop(removed);
        Ok(generation)
    }
    fn retire_provider_for_delete(&self, provider: &Arc<ProviderState>) -> Result<u64, AppError> {
        provider
            .retire_mode
            .store(RETIRE_DELETE_PENDING, Ordering::Release);
        let generation = match self.bump_auth_generation(provider) {
            Ok(generation) => generation,
            Err(error) => {
                provider.retire_mode.store(RETIRE_NONE, Ordering::Release);
                return Err(error);
            }
        };
        let removed = provider.lease.lock().map_err(|_| storage_error())?.take();
        drop(removed);
        Ok(generation)
    }

    fn bump_auth_generation(&self, provider: &Arc<ProviderState>) -> Result<u64, AppError> {
        let mut current = provider
            .auth_generation
            .lock()
            .map_err(|_| storage_error())?;
        *current = (*current)
            .checked_add(1)
            .filter(|value| *value <= MAX_SAFE_INTEGER)
            .ok_or_else(|| storage_error())?;
        Ok(*current)
    }

    async fn expire_lease(&self, provider_id: &str, lease_id: &str) {
        let Ok(provider) = self.provider_state(provider_id) else {
            return;
        };
        // Expiry participates in the native I/O epoch.  If a commit already
        // owns the gate it completes before the expiry marker; otherwise the
        // marker wins and the waiting commit fails its second lease check.
        let io = provider.io_gate.lock().await;
        let removed = {
            let mut active = match provider.lease.lock() {
                Ok(active) => active,
                Err(_) => return,
            };
            if active
                .as_ref()
                .is_some_and(|lease| lease.lease_id == lease_id)
            {
                provider
                    .retire_mode
                    .store(RETIRE_WAIT_FOR_IO, Ordering::Release);
                active.take()
            } else {
                None
            }
        };
        if removed.is_some() {
            drop(removed);
            let _ = self.bump_auth_generation(&provider);
        }
        drop(io);
    }

    fn retire_matching_leases<F>(&self, matches: F) -> Result<(), AppError>
    where
        F: Fn(&CallerIdentity) -> bool,
    {
        let providers = self.provider_entries()?;
        for (_, provider) in providers {
            let removed = {
                let mut active = provider.lease.lock().map_err(|_| storage_error())?;
                if active.as_ref().is_some_and(|lease| matches(&lease.owner)) {
                    provider
                        .retire_mode
                        .store(RETIRE_WAIT_FOR_IO, Ordering::Release);
                    active.take()
                } else {
                    None
                }
            };
            if removed.is_some() {
                drop(removed);
                self.bump_auth_generation(&provider)?;
            }
        }
        Ok(())
    }
    fn backend_for_provider(&self, provider: &ProviderState) -> StorageBackend {
        if provider_mode_is_session_only(&self.inner.backend, provider) {
            StorageBackend::SessionMemory
        } else {
            self.inner.backend.clone()
        }
    }

    async fn read_backend(
        &self,
        provider_id: &str,
        provider: &Arc<ProviderState>,
    ) -> Result<Option<Credential>, AppError> {
        let _io = provider.io_gate.lock().await;
        let backend = self.backend_for_provider(provider);
        let provider_name = provider_id.to_owned();
        let memory = Arc::clone(&self.inner.session_credentials);
        let result = tokio::task::spawn_blocking(move || match backend {
            StorageBackend::Keyring { lock_dir } => read_keyring(&lock_dir, &provider_name),
            StorageBackend::SessionMemory => read_memory(&memory, &provider_name),
        })
        .await
        .map_err(|_| storage_error())??;
        Ok(result)
    }

    async fn write_backend_locked(
        &self,
        provider_id: &str,
        provider: &Arc<ProviderState>,
        credential: &Credential,
    ) -> Result<(), AppError> {
        let backend = self.backend_for_provider(provider);
        let provider_name = provider_id.to_owned();
        let memory = Arc::clone(&self.inner.session_credentials);
        let encoded = serde_json::to_vec(credential)
            .map_err(|_| schema_error("The credential could not be encoded"))?;
        if encoded.len() > MAX_CREDENTIAL_BYTES {
            return Err(AppError::invalid_argument("The credential is too large"));
        }
        let memory_credential = credential.clone();
        tokio::task::spawn_blocking(move || match backend {
            StorageBackend::Keyring { lock_dir } => {
                write_keyring(&lock_dir, &provider_name, &encoded)
            }
            StorageBackend::SessionMemory => {
                write_memory(&memory, &provider_name, encoded, memory_credential)
            }
        })
        .await
        .map_err(|_| storage_error())??;
        Ok(())
    }

    async fn delete_backend_locked(
        &self,
        provider_id: &str,
        provider: &Arc<ProviderState>,
    ) -> Result<(), AppError> {
        let backend = self.backend_for_provider(provider);
        let provider_name = provider_id.to_owned();
        let memory = Arc::clone(&self.inner.session_credentials);
        tokio::task::spawn_blocking(move || match backend {
            StorageBackend::Keyring { lock_dir } => delete_keyring(&lock_dir, &provider_name),
            StorageBackend::SessionMemory => delete_memory(&memory, &provider_name),
        })
        .await
        .map_err(|_| storage_error())??;
        Ok(())
    }
}

fn api_key_account_id(provider_id: &str, key: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"cutterhoochee-account-id-v1\0");
    hasher.update(provider_id.as_bytes());
    hasher.update(b"\0api_key\0");
    hasher.update(key.as_bytes());
    format!("{:x}", hasher.finalize())
}

fn validate_provider(provider_id: &str) -> Result<(), AppError> {
    if SUPPORTED_PROVIDER_IDS.contains(&provider_id) {
        Ok(())
    } else {
        Err(AppError::invalid_argument(
            "The credential provider is not supported",
        ))
    }
}

fn validate_connection_id(connection_id: &str) -> Result<(), AppError> {
    if connection_id.is_empty()
        || connection_id.len() > 256
        || connection_id.contains('\n')
        || connection_id.contains('\r')
    {
        Err(AppError::invalid_argument(
            "The sidecar connection identity is invalid",
        ))
    } else {
        Ok(())
    }
}
fn validate_lease_id(lease_id: &str) -> Result<(), AppError> {
    if lease_id.is_empty()
        || lease_id.len() > 256
        || lease_id.contains('\n')
        || lease_id.contains('\r')
    {
        Err(AppError::invalid_argument(
            "The credential lease identity is invalid",
        ))
    } else {
        Ok(())
    }
}

fn validate_run_id(run_id: &str) -> Result<(), AppError> {
    if run_id.is_empty() || run_id.len() > 256 || run_id.contains('\n') || run_id.contains('\r') {
        Err(AppError::invalid_argument(
            "The assistant run identity is invalid",
        ))
    } else {
        Ok(())
    }
}

/// Parse and validate a credential payload received from the private bridge.
/// Unknown fields are rejected before serde can ignore them, preventing a
/// future/provider-specific credential from bypassing the matrix.
pub(crate) fn parse_credential(provider_id: &str, value: Value) -> Result<Credential, AppError> {
    validate_provider(provider_id)?;
    let object = value
        .as_object()
        .ok_or_else(|| schema_error("The credential must be an object"))?;
    let credential_type = object
        .get("type")
        .and_then(Value::as_str)
        .ok_or_else(|| schema_error("The credential type is missing"))?;
    let allowed: BTreeSet<&str> = match credential_type {
        "api_key" => ["type", "key", "env"].into_iter().collect(),
        "oauth" => ["type", "refresh", "access", "expires", "accountId"]
            .into_iter()
            .collect(),
        _ => return Err(schema_error("The credential type is unsupported")),
    };
    if object.keys().any(|key| !allowed.contains(key.as_str())) {
        return Err(schema_error("The credential contains unsupported fields"));
    }
    let credential: Credential = serde_json::from_value(Value::Object(object.clone()))
        .map_err(|_| schema_error("The credential shape is invalid"))?;
    validate_credential(provider_id, &credential)?;
    Ok(credential)
}

fn validate_credential(provider_id: &str, credential: &Credential) -> Result<(), AppError> {
    let valid = match (provider_id, credential) {
        ("anthropic" | "openai", Credential::ApiKey { key, env }) => {
            let key_valid = key
                .as_ref()
                .is_some_and(|value| !value.is_empty() && value.len() <= MAX_SECRET_FIELD_BYTES);
            key_valid && validate_env(env).is_ok()
        }
        (
            "openai-codex",
            Credential::OAuth {
                refresh,
                access,
                expires,
                account_id,
            },
        ) => {
            !refresh.is_empty()
                && refresh.len() <= MAX_SECRET_FIELD_BYTES
                && !access.is_empty()
                && access.len() <= MAX_SECRET_FIELD_BYTES
                && expires.is_finite()
                && *expires >= 0.0
                && *expires <= MAX_SAFE_INTEGER as f64
                && account_id
                    .as_ref()
                    .is_some_and(|value| !value.is_empty() && value.len() <= MAX_ACCOUNT_ID_BYTES)
        }
        _ => false,
    };
    if valid {
        Ok(())
    } else {
        Err(AppError::invalid_argument(
            "The credential does not match the provider authentication policy",
        ))
    }
}

fn validate_env(env: &Option<BTreeMap<String, String>>) -> Result<(), ()> {
    let Some(values) = env else {
        return Ok(());
    };
    if values.len() > MAX_ENV_ENTRIES {
        return Err(());
    }
    if values.iter().any(|(key, value)| {
        key.is_empty()
            || key.len() > MAX_ENV_KEY_BYTES
            || value.len() > MAX_ENV_VALUE_BYTES
            || key.contains('=')
            || key.contains('\0')
            || value.contains('\0')
    }) {
        Err(())
    } else {
        Ok(())
    }
}

fn read_memory(
    memory: &Mutex<BTreeMap<String, Credential>>,
    provider_id: &str,
) -> Result<Option<Credential>, AppError> {
    let value = memory
        .lock()
        .map_err(|_| storage_error())?
        .get(provider_id)
        .cloned();
    Ok(value)
}

fn write_memory(
    memory: &Mutex<BTreeMap<String, Credential>>,
    provider_id: &str,
    encoded: Vec<u8>,
    credential: Credential,
) -> Result<(), AppError> {
    if encoded.len() > MAX_CREDENTIAL_BYTES {
        return Err(AppError::invalid_argument("The credential is too large"));
    }
    memory
        .lock()
        .map_err(|_| storage_error())?
        .insert(provider_id.to_owned(), credential);
    Ok(())
}

fn delete_memory(
    memory: &Mutex<BTreeMap<String, Credential>>,
    provider_id: &str,
) -> Result<(), AppError> {
    memory
        .lock()
        .map_err(|_| storage_error())?
        .remove(provider_id);
    Ok(())
}

fn read_keyring(lock_dir: &Path, provider_id: &str) -> Result<Option<Credential>, AppError> {
    with_keyring_lock(lock_dir, provider_id, || {
        let entry = Entry::new(KEYRING_SERVICE, provider_id).map_err(|_| storage_error())?;
        let bytes = match entry.get_secret() {
            Ok(bytes) => bytes,
            Err(KeyringError::NoEntry) => return Ok(None),
            Err(_) => return Err(storage_error()),
        };
        if bytes.len() > MAX_CREDENTIAL_BYTES {
            return Err(schema_error("The stored credential is too large"));
        }
        let value: Value = serde_json::from_slice(&bytes)
            .map_err(|_| schema_error("The stored credential data is invalid"))?;
        parse_credential(provider_id, value).map(Some)
    })
}

fn write_keyring(lock_dir: &Path, provider_id: &str, encoded: &[u8]) -> Result<(), AppError> {
    with_keyring_lock(lock_dir, provider_id, || {
        let entry = Entry::new(KEYRING_SERVICE, provider_id).map_err(|_| storage_error())?;
        entry.set_secret(encoded).map_err(|_| storage_error())
    })
}

fn delete_keyring(lock_dir: &Path, provider_id: &str) -> Result<(), AppError> {
    with_keyring_lock(lock_dir, provider_id, || {
        let entry = Entry::new(KEYRING_SERVICE, provider_id).map_err(|_| storage_error())?;
        match entry.delete_credential() {
            Ok(()) | Err(KeyringError::NoEntry) => Ok(()),
            Err(_) => Err(storage_error()),
        }
    })
}

fn with_keyring_lock<T>(
    lock_dir: &Path,
    provider_id: &str,
    operation: impl FnOnce() -> Result<T, AppError>,
) -> Result<T, AppError> {
    std::fs::create_dir_all(lock_dir).map_err(|_| storage_error())?;
    let path = lock_dir.join(format!("{provider_id}.lock"));
    let lock = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .open(path)
        .map_err(|_| storage_error())?;
    lock.lock_exclusive().map_err(|_| storage_error())?;
    let result = operation();
    let unlock = lock.unlock();
    match (result, unlock) {
        (Ok(value), Ok(())) => Ok(value),
        (Err(error), _) => Err(error),
        (Ok(_), Err(_)) => Err(storage_error()),
    }
}

fn storage_error() -> AppError {
    AppError::io("The native credential store is unavailable")
}

fn schema_error(message: &'static str) -> AppError {
    AppError::new(ErrorCode::SchemaUnsupported, message)
}

fn stale_error(message: &'static str) -> AppError {
    AppError::stale_session(message)
}

fn busy_error(message: &'static str) -> AppError {
    AppError::busy(message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::editor::dispatcher::CallerContext;
    use serde_json::json;
    use std::sync::Arc;
    use tokio::sync::Barrier;

    fn caller(connection_id: &str, generation: u64) -> CallerContext {
        CallerContext::agent_sidecar(connection_id.to_owned(), generation, None)
    }

    fn api_key(key: &str) -> Credential {
        Credential::ApiKey {
            key: Some(key.to_owned()),
            env: None,
        }
    }

    fn oauth() -> Credential {
        Credential::OAuth {
            refresh: "refresh-token".to_owned(),
            access: "access-token".to_owned(),
            expires: 4_000_000_000_000.0,
            account_id: Some("acct".to_owned()),
        }
    }

    #[test]
    fn credential_serialization_is_private_and_wire_cased() {
        let metadata = CredentialInfo {
            provider_id: "openai-codex".to_owned(),
            credential_type: CredentialType::OAuth,
            account_id: Some("acct".to_owned()),
        };
        let metadata_json = serde_json::to_value(metadata).unwrap();
        assert_eq!(
            metadata_json.get("providerId").and_then(Value::as_str),
            Some("openai-codex")
        );
        assert_eq!(
            metadata_json.get("type").and_then(Value::as_str),
            Some("oauth")
        );
        assert_eq!(
            metadata_json.get("accountId").and_then(Value::as_str),
            Some("acct")
        );
        assert!(metadata_json.get("access").is_none());
        assert!(metadata_json.get("refresh").is_none());

        let lease = CredentialLeaseInfo {
            lease_id: "lease".to_owned(),
            current: Some(oauth()),
            auth_generation: 7,
        };
        let lease_json = serde_json::to_value(lease).unwrap();
        assert_eq!(
            lease_json.get("leaseId").and_then(Value::as_str),
            Some("lease")
        );
        assert_eq!(
            lease_json.get("authGeneration").and_then(Value::as_u64),
            Some(7)
        );
        assert_eq!(
            lease_json
                .get("current")
                .and_then(|value| value.get("type"))
                .and_then(Value::as_str),
            Some("oauth")
        );
    }

    #[test]
    fn credential_parser_enforces_provider_matrix_and_unknown_fields() {
        assert_eq!(
            parse_credential("anthropic", json!({"type":"api_key", "key":"k"})).unwrap(),
            api_key("k")
        );
        assert!(parse_credential(
            "openai",
            json!({"type":"oauth", "refresh":"r", "access":"a", "expires":1})
        )
        .is_err());
        assert!(parse_credential(
            "openai-codex",
            json!({"type":"oauth", "refresh":"r", "access":"a", "expires":1})
        )
        .is_err());
        assert!(parse_credential("openai-codex", json!({"type":"oauth", "refresh":"r", "access":"a", "expires":1, "accountId":"acct", "unexpected":"x"})).is_err());
        assert!(serde_json::to_value(oauth())
            .unwrap()
            .get("access")
            .is_some());
    }

    #[tokio::test]
    async fn account_identity_is_native_stable_and_changes_on_account_replacement() {
        let runtime = CredentialsRuntime::session_memory();
        let context = caller("sidecar", 1);

        let first_lease = runtime.acquire_lease("openai", &context).await.unwrap();
        runtime
            .commit_lease(
                "openai",
                &first_lease.lease_id,
                first_lease.auth_generation,
                api_key("same-key"),
                &context,
            )
            .await
            .unwrap();
        let first = runtime.account_identity("openai").await.unwrap();
        assert!(first.is_some());

        let same_lease = runtime.acquire_lease("openai", &context).await.unwrap();
        runtime
            .commit_lease(
                "openai",
                &same_lease.lease_id,
                same_lease.auth_generation,
                api_key("same-key"),
                &context,
            )
            .await
            .unwrap();
        assert_eq!(runtime.account_identity("openai").await.unwrap(), first);

        let replacement_lease = runtime.acquire_lease("openai", &context).await.unwrap();
        runtime
            .commit_lease(
                "openai",
                &replacement_lease.lease_id,
                replacement_lease.auth_generation,
                api_key("different-key"),
                &context,
            )
            .await
            .unwrap();
        let replacement = runtime.account_identity("openai").await.unwrap();
        assert!(replacement.is_some());
        assert_ne!(replacement, first);

        let oauth_lease = runtime
            .acquire_lease("openai-codex", &context)
            .await
            .unwrap();
        runtime
            .commit_lease(
                "openai-codex",
                &oauth_lease.lease_id,
                oauth_lease.auth_generation,
                oauth(),
                &context,
            )
            .await
            .unwrap();
        assert_eq!(
            runtime.account_identity("openai-codex").await.unwrap(),
            Some("acct".to_owned())
        );
        assert_eq!(runtime.account_identity("anthropic").await.unwrap(), None);
    }

    #[tokio::test]
    async fn concurrent_lease_acquisition_is_serialized() {
        let runtime = CredentialsRuntime::session_memory();
        let first = caller("sidecar", 1);
        let second = first.clone();
        let lease = runtime.acquire_lease("openai", &first).await.unwrap();
        let runtime_for_second = runtime.clone();
        let second_task =
            tokio::spawn(async move { runtime_for_second.acquire_lease("openai", &second).await });
        assert!(timeout(Duration::from_millis(20), second_task)
            .await
            .is_err());
        runtime
            .release_lease("openai", &lease.lease_id, &first)
            .await
            .unwrap();
    }
    #[tokio::test]
    async fn explicit_session_mode_uses_native_lease_authority() {
        let runtime = CredentialsRuntime::session_memory();
        let context = caller("sidecar", 1);
        runtime
            .set_session_mode("anthropic", true, &context)
            .await
            .unwrap();
        let lease = runtime.acquire_lease("anthropic", &context).await.unwrap();
        runtime
            .commit_lease(
                "anthropic",
                &lease.lease_id,
                lease.auth_generation,
                api_key("session"),
                &context,
            )
            .await
            .unwrap();
        assert!(runtime.read("anthropic", &context).await.unwrap().is_some());
        let listed = runtime.list(&context).await.unwrap();
        assert_eq!(listed.len(), 1);
        runtime.delete("anthropic", &context).await.unwrap();
        assert!(runtime.read("anthropic", &context).await.unwrap().is_none());
    }
    #[tokio::test]
    async fn sidecar_retirement_clears_session_credentials_and_lease() {
        let runtime = CredentialsRuntime::session_memory();
        let first = caller("sidecar", 1);
        runtime
            .set_session_mode("openai", true, &first)
            .await
            .unwrap();
        let lease = runtime.acquire_lease("openai", &first).await.unwrap();
        runtime
            .commit_lease(
                "openai",
                &lease.lease_id,
                lease.auth_generation,
                api_key("transient"),
                &first,
            )
            .await
            .unwrap();
        runtime.retire_context("sidecar", 1).unwrap();
        let second = caller("replacement", 2);
        assert!(runtime.read("openai", &second).await.unwrap().is_none());
    }
    #[tokio::test]
    async fn trusted_window_can_select_session_mode_before_sidecar_registration() {
        let runtime = CredentialsRuntime::session_memory();
        let window = CallerContext::human_window("main".to_owned(), 0, None);
        runtime
            .set_session_mode("anthropic", true, &window)
            .await
            .unwrap();
        assert!(runtime
            .session_mode("anthropic", &caller("sidecar", 0))
            .await
            .is_ok());
    }
    #[tokio::test]
    async fn trusted_window_mode_rechecks_generation_after_io_wait() {
        let runtime = CredentialsRuntime::session_memory();
        let sidecar = caller("sidecar", 0);
        let _ = runtime.read("anthropic", &sidecar).await.unwrap();
        let provider = runtime.provider_state("anthropic").unwrap();
        let io = provider.io_gate.lock().await;
        let runtime_for_task = runtime.clone();
        let window = CallerContext::human_window("main".to_owned(), 0, None);
        let task = tokio::spawn(async move {
            runtime_for_task
                .set_session_mode("anthropic", true, &window)
                .await
        });
        tokio::task::yield_now().await;
        runtime.retire_context("sidecar", 0).unwrap();
        drop(io);
        assert!(task.await.unwrap().is_err());
    }

    #[tokio::test]
    async fn logout_race_retires_late_refresh_without_restoring_secret() {
        let runtime = CredentialsRuntime::session_memory();
        let context = caller("sidecar", 1);
        let seed_lease = runtime.acquire_lease("openai", &context).await.unwrap();
        runtime
            .commit_lease(
                "openai",
                &seed_lease.lease_id,
                seed_lease.auth_generation,
                api_key("before"),
                &context,
            )
            .await
            .unwrap();
        let refresh_lease = runtime.acquire_lease("openai", &context).await.unwrap();
        let barrier = Arc::new(Barrier::new(3));
        let commit_barrier = Arc::clone(&barrier);
        let commit_runtime = runtime.clone();
        let commit_context = context.clone();
        let lease_id = refresh_lease.lease_id.clone();
        let auth_generation = refresh_lease.auth_generation;
        let commit_task = tokio::spawn(async move {
            commit_barrier.wait().await;
            commit_runtime
                .commit_lease(
                    "openai",
                    &lease_id,
                    auth_generation,
                    api_key("late-refresh"),
                    &commit_context,
                )
                .await
        });
        let delete_barrier = Arc::clone(&barrier);
        let delete_runtime = runtime.clone();
        let delete_context = context.clone();
        let delete_task = tokio::spawn(async move {
            delete_barrier.wait().await;
            delete_runtime.delete("openai", &delete_context).await
        });
        barrier.wait().await;
        let _ = tokio::join!(commit_task, delete_task);
        assert!(runtime.read("openai", &context).await.unwrap().is_none());
        assert!(runtime
            .commit_lease(
                "openai",
                &refresh_lease.lease_id,
                refresh_lease.auth_generation,
                api_key("too-late"),
                &context
            )
            .await
            .is_err());
    }
}
