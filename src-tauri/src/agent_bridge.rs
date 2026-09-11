use crate::editor::dispatcher::{dispatch, CallerContext};
use crate::error::{AppError, ErrorCode};
use crate::ipc::{EditorReply, EditorRequest, MAX_NDJSON_LINE_BYTES, PROTOCOL_VERSION};
use crate::media::artifacts::ArtifactStore;
use crate::media::probe::resolve_packaged_binary;
use crate::state::AppState;
use base64::Engine as _;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::io;
use std::path::PathBuf;
use std::pin::Pin;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tauri::Manager;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command};
use tokio::sync::{broadcast, oneshot, Mutex as AsyncMutex};
use tokio::time::{sleep, timeout};
use uuid::Uuid;

const MAX_DIAGNOSTIC_BYTES: usize = 8 * 1024;
const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;

/// App-owned paths used by the Node sidecar. The sidecar never inherits the
/// user's HOME, config, cache, temporary directory, or environment secrets.
#[derive(Debug, Clone)]
pub struct AgentPaths {
    pub resource_dir: PathBuf,
    pub node_path: PathBuf,
    pub agent_entrypoint: PathBuf,
    pub app_data_dir: PathBuf,
    pub app_cache_dir: PathBuf,
    pub agent_home: PathBuf,
    pub agent_config_dir: PathBuf,
    pub agent_cache_dir: PathBuf,
    pub session_dir: PathBuf,
    pub temp_dir: PathBuf,
    pub artifact_dir: PathBuf,
}

impl AgentPaths {
    pub fn from_app(app: &tauri::AppHandle) -> Result<Self, AppError> {
        let resource_dir = app
            .path()
            .resource_dir()
            .map_err(|_| AppError::io("The bundled resource directory is unavailable"))?;
        let app_data_root = app
            .path()
            .app_data_dir()
            .map_err(|_| AppError::io("The application data directory is unavailable"))?;
        let app_cache_root = app
            .path()
            .app_cache_dir()
            .map_err(|_| AppError::io("The application cache directory is unavailable"))?;
        let app_data_dir = app_data_root.join("cutterhoochee");
        let app_cache_dir = app_cache_root.join("cutterhoochee");
        let target = env!("CUTTERHOOCHEE_TARGET_TRIPLE");
        let node_name = format!("node-{target}{}", if cfg!(windows) { ".exe" } else { "" });
        Ok(Self {
            resource_dir: resource_dir.clone(),
            node_path: resource_dir.join("binaries").join(node_name),
            agent_entrypoint: resource_dir.join("resources/agent/agent/dist/main.js"),
            agent_home: app_data_dir.join("agent-home"),
            agent_config_dir: app_data_dir.join("agent-config"),
            agent_cache_dir: app_cache_dir.join("agent-cache"),
            session_dir: app_data_dir.join("sessions"),
            temp_dir: app_cache_dir.join("tmp"),
            artifact_dir: app_cache_dir.join("artifacts"),
            app_data_dir,
            app_cache_dir,
        })
    }

    pub fn ensure_owned_directories(&self) -> Result<(), AppError> {
        if !self.node_path.is_absolute() || !self.agent_entrypoint.is_absolute() {
            return Err(AppError::io("The bundled Node path must be absolute"));
        }
        for directory in [
            &self.app_data_dir,
            &self.app_cache_dir,
            &self.agent_home,
            &self.agent_config_dir,
            &self.agent_cache_dir,
            &self.session_dir,
            &self.temp_dir,
            &self.artifact_dir,
        ] {
            std::fs::create_dir_all(directory)
                .map_err(|_| AppError::io("An app-owned sidecar directory could not be created"))?;
        }
        Ok(())
    }

    fn node_bin_dir(&self) -> Result<PathBuf, AppError> {
        self.node_path
            .parent()
            .map(PathBuf::from)
            .ok_or_else(|| AppError::io("The bundled Node path has no parent directory"))
    }

    fn sanitized_path(&self) -> Result<std::ffi::OsString, AppError> {
        let node_bin = self.node_bin_dir()?;
        let bundled_bins = self.resource_dir.join("binaries");
        std::env::join_paths([node_bin, bundled_bins])
            .map_err(|_| AppError::io("The bundled sidecar PATH could not be constructed"))
    }
}

#[derive(Debug, Clone)]
pub struct BridgeEvent {
    pub id: String,
    pub project_id: Option<String>,
    pub generation: u64,
    pub run_id: Option<String>,
    pub event: String,
    pub data: Option<Value>,
}

/// Private events are carried on a separate Rust channel even though their
/// wire envelope uses the same `kind: event` shape. Credential/auth payloads
/// therefore cannot accidentally enter the desktop event stream.
#[derive(Debug, Clone)]
pub struct PrivateBridgeEvent {
    pub id: String,
    pub project_id: Option<String>,
    pub generation: u64,
    pub run_id: Option<String>,
    pub event: String,
    pub data: Option<Value>,
}

#[derive(Debug, Clone)]
pub struct PrivateRequestContext {
    pub id: String,
    pub project_id: Option<String>,
    pub generation: u64,
    pub run_id: Option<String>,
    pub caller: CallerContext,
}

pub type PrivateRequestFuture = Pin<Box<dyn Future<Output = Result<Value, AppError>> + Send>>;
pub type PrivateRequestHandler =
    Arc<dyn Fn(String, Value, PrivateRequestContext) -> PrivateRequestFuture + Send + Sync>;

#[derive(Debug, Clone)]
pub enum BridgeLifecycle {
    Terminated {
        connection_id: String,
        generation: u64,
    },
}

/// One wire representation is used for editor and private messages. The
/// editor adapter validates response data as `EditorReply`; private responses
/// remain generic values for the credential/auth handlers.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct WireEnvelope {
    v: u8,
    id: String,
    #[serde(rename = "projectId")]
    project_id: Option<String>,
    generation: u64,
    #[serde(rename = "runId", skip_serializing_if = "Option::is_none")]
    run_id: Option<String>,
    #[serde(flatten)]
    body: WireMessage,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum WireMessage {
    Request {
        method: String,
        params: Value,
    },
    Response {
        ok: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        data: Option<Value>,
        #[serde(skip_serializing_if = "Option::is_none")]
        error: Option<AppError>,
    },
    Event {
        event: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        data: Option<Value>,
    },
}

enum PromptWriteError {
    Guard(AppError),
    Write(AppError),
}

struct BridgeInner {
    state: AppState,
    connection_id: String,
    generation: u64,
    stdin: AsyncMutex<ChildStdin>,
    child: Arc<AsyncMutex<Child>>,
    pending: Mutex<HashMap<String, oneshot::Sender<Result<Value, AppError>>>>,
    active_ids: Mutex<HashSet<String>>,
    sequence: AtomicU64,
    terminated: AtomicBool,
    events: broadcast::Sender<BridgeEvent>,
    private_events: broadcast::Sender<PrivateBridgeEvent>,
    private_handler: Mutex<Option<PrivateRequestHandler>>,
    lifecycle_hook: Mutex<Option<Arc<dyn Fn(BridgeLifecycle) + Send + Sync>>>,
}

/// Supervised, multiplexed transport to the bundled Node/Pi process.
#[derive(Clone)]
pub struct AgentBridge {
    inner: Arc<BridgeInner>,
}

impl AgentBridge {
    pub fn spawn(state: AppState, paths: AgentPaths) -> Result<Self, AppError> {
        paths.ensure_owned_directories()?;
        if !paths.node_path.is_absolute() {
            return Err(AppError::io("The bundled Node path must be absolute"));
        }
        let generation = state.generation();
        let project_id = state.current_project_id();
        let workspace_id = state.current_workspace_id();
        let connection_id = Uuid::new_v4().simple().to_string();
        let mut command = Command::new(&paths.node_path);
        command
            .arg(&paths.agent_entrypoint)
            .current_dir(&paths.agent_home)
            .env_clear()
            .env("HOME", &paths.agent_home)
            .env("XDG_CONFIG_HOME", &paths.agent_config_dir)
            .env("XDG_CACHE_HOME", &paths.agent_cache_dir)
            .env("TMPDIR", &paths.temp_dir)
            .env("CUTTERHOOCHEE_AGENT_DIR", &paths.agent_home)
            .env("CUTTERHOOCHEE_SESSION_DIR", &paths.session_dir)
            .env("CUTTERHOOCHEE_BRIDGE", "1")
            .env(
                "CUTTERHOOCHEE_PROTOCOL_VERSION",
                PROTOCOL_VERSION.to_string(),
            )
            .env("CUTTERHOOCHEE_GENERATION", generation.to_string())
            .env(
                "CUTTERHOOCHEE_PROJECT_ID",
                project_id.as_deref().unwrap_or_default(),
            )
            .env(
                "CUTTERHOOCHEE_WORKSPACE_ID",
                workspace_id.as_deref().unwrap_or_default(),
            )
            .env("LANG", "C.UTF-8")
            .env("LC_ALL", "C.UTF-8")
            .env("NODE_ENV", "production")
            .env("PATH", paths.sanitized_path()?);
        #[cfg(unix)]
        command.process_group(0);
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let mut child = command
            .spawn()
            .map_err(|_| AppError::io("The bundled Node agent could not be started"))?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| AppError::io("The bundled Node agent stdin was not available"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| AppError::io("The Node agent stdout was not available"))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| AppError::io("The Node agent stderr was not available"))?;
        let (events, _) = broadcast::channel(128);
        let (private_events, _) = broadcast::channel(128);
        let bridge = Self {
            inner: Arc::new(BridgeInner {
                state,
                connection_id,
                generation,
                stdin: AsyncMutex::new(stdin),
                child: Arc::new(AsyncMutex::new(child)),
                pending: Mutex::new(HashMap::new()),
                active_ids: Mutex::new(HashSet::new()),
                sequence: AtomicU64::new(0),
                terminated: AtomicBool::new(false),
                events,
                private_events,
                private_handler: Mutex::new(None),
                lifecycle_hook: Mutex::new(None),
            }),
        };
        let stdout_bridge = bridge.clone();
        tokio::spawn(async move { stdout_loop(stdout, stdout_bridge).await });
        let stderr_bridge = bridge.clone();
        tokio::spawn(async move { stderr_loop(stderr, stderr_bridge).await });
        let wait_bridge = bridge.clone();
        tokio::spawn(async move { wait_loop(wait_bridge).await });
        Ok(bridge)
    }

    pub fn connection_id(&self) -> &str {
        &self.inner.connection_id
    }

    pub fn generation(&self) -> u64 {
        self.inner.generation
    }

    pub fn is_terminated(&self) -> bool {
        self.inner.terminated.load(Ordering::Acquire)
    }

    pub fn subscribe_events(&self) -> broadcast::Receiver<BridgeEvent> {
        self.inner.events.subscribe()
    }

    pub fn subscribe_private_events(&self) -> broadcast::Receiver<PrivateBridgeEvent> {
        self.inner.private_events.subscribe()
    }

    pub fn register_private_handler(&self, handler: PrivateRequestHandler) -> Result<(), AppError> {
        let mut current = self
            .inner
            .private_handler
            .lock()
            .map_err(|_| AppError::io("The bridge private handler lock is unavailable"))?;
        *current = Some(handler);
        Ok(())
    }

    pub fn register_lifecycle_hook(
        &self,
        hook: Arc<dyn Fn(BridgeLifecycle) + Send + Sync>,
    ) -> Result<(), AppError> {
        let mut current = self
            .inner
            .lifecycle_hook
            .lock()
            .map_err(|_| AppError::io("The bridge lifecycle hook lock is unavailable"))?;
        *current = Some(hook);
        Ok(())
    }

    /// Send a typed editor request. The wire response is generic at this
    /// transport layer and parsed only after the pending request is identified.
    pub async fn request(
        &self,
        request: EditorRequest,
        run_id: Option<String>,
    ) -> Result<EditorReply, AppError> {
        let value = self
            .send_request(request.method().to_owned(), request.params(), run_id)
            .await?;
        serde_json::from_value(value)
            .map_err(|_| AppError::schema("The editor bridge response has an invalid shape"))
    }

    /// Send an assistant/provider/credential private method over the same
    /// envelope. Private methods have a strict direction and are never routed
    /// through `EditorRequest::from_wire`.
    pub async fn private_request(
        &self,
        method: &str,
        params: Value,
        run_id: Option<String>,
    ) -> Result<Value, AppError> {
        if !is_private_method(method) {
            return Err(AppError::schema(
                "The bridge private method is not allowlisted",
            ));
        }
        self.send_request(method.to_owned(), params, run_id).await
    }

    pub fn stop(&self) {
        self.fail(AppError::stale_session("The agent bridge was stopped"));
    }

    /// Retire and immediately kill the owned process group after a graceful
    /// stop deadline has elapsed. This is only used by the assistant
    /// supervisor; ordinary bridge errors still get the normal grace period.
    pub fn force_stop(&self) {
        self.fail(AppError::stale_session(
            "The agent bridge was force-stopped",
        ));
        let child = self.inner.child.clone();
        if let Ok(mut child_guard) = child.try_lock() {
            #[cfg(unix)]
            if let Some(pid) = child_guard.id() {
                unsafe {
                    libc::kill(-(pid as i32), libc::SIGKILL);
                }
            }
            let _ = child_guard.start_kill();
        };
    }

    fn ensure_live(&self) -> Result<(), AppError> {
        if self.is_terminated() {
            Err(AppError::stale_session(
                "The agent bridge is no longer active",
            ))
        } else {
            Ok(())
        }
    }
    async fn send_request(
        &self,
        method: String,
        params: Value,
        run_id: Option<String>,
    ) -> Result<Value, AppError> {
        self.ensure_live()?;
        self.inner
            .state
            .validate_generation(self.inner.generation)?;
        let prompt_run_id = if method == "assistant"
            && params.get("action").and_then(Value::as_str) == Some("prompt")
        {
            run_id.clone()
        } else {
            None
        };
        let id = format!(
            "{}-{}",
            self.inner.connection_id,
            self.inner.sequence.fetch_add(1, Ordering::Relaxed)
        );
        let envelope = WireEnvelope {
            v: PROTOCOL_VERSION,
            id: id.clone(),
            project_id: self.inner.state.current_project_id(),
            generation: self.inner.generation,
            run_id,
            body: WireMessage::Request { method, params },
        };
        let encoded = encode_line(&envelope)?;
        let (sender, receiver) = oneshot::channel();
        {
            let mut active = self
                .inner
                .active_ids
                .lock()
                .map_err(|_| AppError::io("The bridge request registry is unavailable"))?;
            if !active.insert(id.clone()) {
                return Err(AppError::schema("A bridge message id is already in flight"));
            }
        }
        if let Err(error) = self
            .inner
            .pending
            .lock()
            .map_err(|_| AppError::io("The bridge request registry is unavailable"))
            .map(|mut pending| {
                pending.insert(id.clone(), sender);
            })
        {
            if let Ok(mut active) = self.inner.active_ids.lock() {
                active.remove(&id);
            }
            return Err(error);
        }
        if let Some(prompt_run_id) = prompt_run_id.as_deref() {
            match self.write_prompt_encoded(&encoded, prompt_run_id).await {
                Ok(()) => {}
                Err(PromptWriteError::Guard(error)) => {
                    self.remove_pending(&id);
                    return Err(error);
                }
                Err(PromptWriteError::Write(error)) => {
                    self.remove_pending(&id);
                    self.fail(error.clone());
                    return Err(error);
                }
            }
        } else if let Err(error) = self.write_encoded(&encoded).await {
            self.remove_pending(&id);
            self.fail(error.clone());
            return Err(error);
        }
        receiver.await.unwrap_or_else(|_| {
            Err(AppError::stale_session(
                "The agent bridge ended before replying",
            ))
        })
    }

    async fn write_prompt_encoded(
        &self,
        encoded: &[u8],
        run_id: &str,
    ) -> Result<(), PromptWriteError> {
        self.ensure_live().map_err(PromptWriteError::Write)?;
        let mut stdin = self.inner.stdin.lock().await;
        self.inner
            .state
            .assistant()
            .active_run_recipient(self.inner.generation, run_id)
            .map_err(PromptWriteError::Guard)?;
        stdin.write_all(encoded).await.map_err(|_| {
            PromptWriteError::Write(AppError::io("The agent bridge could not write a message"))
        })?;
        stdin.flush().await.map_err(|_| {
            PromptWriteError::Write(AppError::io("The agent bridge could not flush a message"))
        })?;
        Ok(())
    }

    async fn write_encoded(&self, encoded: &[u8]) -> Result<(), AppError> {
        self.ensure_live()?;
        let mut stdin = self.inner.stdin.lock().await;
        stdin
            .write_all(encoded)
            .await
            .map_err(|_| AppError::io("The agent bridge could not write a message"))?;
        stdin
            .flush()
            .await
            .map_err(|_| AppError::io("The agent bridge could not flush a message"))?;
        Ok(())
    }

    fn remove_pending(&self, id: &str) {
        if let Ok(mut pending) = self.inner.pending.lock() {
            pending.remove(id);
        }
        if let Ok(mut active) = self.inner.active_ids.lock() {
            active.remove(id);
        }
    }

    fn fail(&self, error: AppError) {
        if self.inner.terminated.swap(true, Ordering::AcqRel) {
            return;
        }
        if let Ok(mut pending) = self.inner.pending.lock() {
            let waiting = std::mem::take(&mut *pending);
            for (_, sender) in waiting {
                let _ = sender.send(Err(error.clone()));
            }
        }
        if let Ok(mut active) = self.inner.active_ids.lock() {
            active.clear();
        }
        self.inner
            .state
            .clear_agent_bridge(&self.inner.connection_id);
        let event = BridgeEvent {
            id: format!("bridge-{}", self.inner.connection_id),
            project_id: self.inner.state.current_project_id(),
            generation: self.inner.generation,
            run_id: None,
            event: "assistant_unavailable".to_owned(),
            data: Some(serde_json::json!({"message": error.message})),
        };
        let _ = self.inner.events.send(event.clone());
        let _ =
            self.inner
                .state
                .emit_sanitized_event(event.event.clone(), None, event.data.clone());
        if let Ok(hook) = self.inner.lifecycle_hook.lock() {
            if let Some(hook) = hook.clone() {
                hook(BridgeLifecycle::Terminated {
                    connection_id: self.inner.connection_id.clone(),
                    generation: self.inner.generation,
                });
            }
        }
        self.terminate_process();
    }

    fn terminate_process(&self) {
        let child = self.inner.child.clone();
        #[cfg(unix)]
        {
            if let Ok(mut child_guard) = child.try_lock() {
                if let Some(pid) = child_guard.id() {
                    unsafe {
                        libc::kill(-(pid as i32), libc::SIGTERM);
                    }
                }
                let _ = child_guard.start_kill();
            }
        }
        #[cfg(not(unix))]
        {
            if let Ok(mut child_guard) = child.try_lock() {
                let _ = child_guard.start_kill();
            }
        }
        let bridge = self.clone();
        tokio::spawn(async move {
            sleep(Duration::from_secs(2)).await;
            if bridge.is_terminated() {
                let child = bridge.inner.child.clone();
                if let Ok(mut child_guard) = child.try_lock() {
                    #[cfg(unix)]
                    if let Some(pid) = child_guard.id() {
                        unsafe {
                            libc::kill(-(pid as i32), libc::SIGKILL);
                        }
                    }
                    let _ = child_guard.start_kill();
                };
            }
        });
    }

    async fn handle_message(&self, envelope: WireEnvelope) -> Result<(), AppError> {
        self.validate_session(&envelope)?;
        match envelope.body.clone() {
            WireMessage::Response { ok, data, error } => {
                let sender = {
                    let mut pending =
                        self.inner.pending.lock().map_err(|_| {
                            AppError::io("The bridge response registry is unavailable")
                        })?;
                    pending.remove(&envelope.id)
                }
                .ok_or_else(|| AppError::schema("The bridge response id is not in flight"))?;
                if let Ok(mut active) = self.inner.active_ids.lock() {
                    active.remove(&envelope.id);
                }
                let result = if ok {
                    Ok(data.ok_or_else(|| AppError::schema("Bridge response data is missing"))?)
                } else {
                    Err(error
                        .ok_or_else(|| AppError::schema("Bridge response error is missing"))?)
                };
                let _ = sender.send(result);
                Ok(())
            }
            WireMessage::Event { event, data } => {
                if is_private_event(&event) {
                    let _ = self.inner.private_events.send(PrivateBridgeEvent {
                        id: envelope.id,
                        project_id: envelope.project_id,
                        generation: envelope.generation,
                        run_id: envelope.run_id,
                        event,
                        data,
                    });
                } else {
                    let bridge_event = BridgeEvent {
                        id: envelope.id,
                        project_id: envelope.project_id,
                        generation: envelope.generation,
                        run_id: envelope.run_id,
                        event,
                        data,
                    };
                    let _ = self.inner.events.send(bridge_event.clone());
                    let _ = self.inner.state.emit_bridge_event(&bridge_event);
                }
                Ok(())
            }
            WireMessage::Request { method, params } => {
                let inserted = {
                    let mut active =
                        self.inner.active_ids.lock().map_err(|_| {
                            AppError::io("The bridge request registry is unavailable")
                        })?;
                    active.insert(envelope.id.clone())
                };
                if !inserted {
                    return Err(AppError::schema("A bridge message id is already in flight"));
                }
                let bridge = self.clone();
                if is_private_method(&method) && !is_editor_method(&method) {
                    let handler = bridge
                        .inner
                        .private_handler
                        .lock()
                        .map_err(|_| {
                            AppError::io("The bridge private handler lock is unavailable")
                        })?
                        .clone();
                    let context = PrivateRequestContext {
                        id: envelope.id.clone(),
                        project_id: envelope.project_id.clone(),
                        generation: envelope.generation,
                        run_id: envelope.run_id.clone(),
                        caller: CallerContext::agent_sidecar(
                            bridge.inner.connection_id.clone(),
                            envelope.generation,
                            envelope.project_id.clone(),
                        )
                        .with_run_id(envelope.run_id.clone()),
                    };
                    tokio::spawn(async move {
                        let result = match handler {
                            Some(handler) => handler(method, params, context).await,
                            None => {
                                Err(AppError::schema("No private bridge handler is configured"))
                            }
                        };
                        bridge.respond(&envelope, result).await;
                    });
                    return Ok(());
                }
                if !is_editor_method(&method) {
                    self.remove_pending(&envelope.id);
                    let response = response_envelope(
                        &envelope,
                        Err(AppError::schema("Unknown bridge method")),
                    );
                    self.write_encoded(&encode_line(&response)?).await?;
                    return Ok(());
                }
                let request = match EditorRequest::from_wire(&method, &params) {
                    Ok(request) => request,
                    Err(error) => {
                        self.remove_pending(&envelope.id);
                        let response = response_envelope(&envelope, Err(error));
                        self.write_encoded(&encode_line(&response)?).await?;
                        return Ok(());
                    }
                };
                let bridge = self.clone();
                tokio::spawn(async move {
                    let result = if bridge
                        .inner
                        .state
                        .validate_generation(envelope.generation)
                        .is_err()
                    {
                        Err(AppError::stale_session(
                            "The editor request belongs to a retired generation",
                        ))
                    } else {
                        let caller = CallerContext::agent_sidecar(
                            bridge.inner.connection_id.clone(),
                            envelope.generation,
                            envelope.project_id.clone(),
                        )
                        .with_run_id(envelope.run_id.clone());
                        dispatch(request, caller, &bridge.inner.state).await
                    };
                    bridge
                        .respond(
                            &envelope,
                            result.map(|reply| serde_json::to_value(reply).unwrap_or(Value::Null)),
                        )
                        .await;
                });
                Ok(())
            }
        }
    }

    async fn respond(&self, request: &WireEnvelope, result: Result<Value, AppError>) {
        let response = response_envelope(request, result);
        match encode_line(&response) {
            Ok(encoded) => {
                if let Err(error) = self.write_encoded(&encoded).await {
                    self.fail(error);
                }
            }
            Err(error) => self.fail(error),
        }
        self.remove_pending(&request.id);
    }

    fn validate_session(&self, envelope: &WireEnvelope) -> Result<(), AppError> {
        validate_wire_envelope(envelope)?;
        if envelope.generation != self.inner.generation {
            return Err(AppError::stale_session(
                "The bridge message belongs to a retired sidecar generation",
            ));
        }
        self.inner.state.validate_generation(envelope.generation)?;
        if envelope.project_id != self.inner.state.current_project_id() {
            return Err(AppError::stale_session(
                "The bridge message belongs to a different project",
            ));
        }
        Ok(())
    }
}
const MAX_EVIDENCE_IMAGE_BYTES: u64 = 3 * 1024 * 1024;
const MAX_EVIDENCE_IMAGE_EDGE: u32 = 1_280;
const MAX_EVIDENCE_SOURCE_BYTES: u64 = 64 * 1024 * 1024;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct EvidenceImageReadParams {
    artifact_id: String,
    #[serde(default)]
    offset: Option<u64>,
    #[serde(default)]
    length: Option<u64>,
    #[serde(default)]
    max_edge: Option<u32>,
    #[serde(default)]
    max_bytes: Option<u64>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct EvidenceImageReadReply {
    artifact_id: String,
    mime_type: String,
    base64: String,
    byte_size: u64,
    offset: u64,
}

/// Decode and scale one managed image through the packaged FFmpeg binary.
///
/// The source is already restricted to the managed frame/thumbnail directory.
/// FFmpeg receives bytes through stdin and emits one PNG through stdout; no
/// path, filter graph, or network input is accepted from the sidecar.
async fn render_evidence_derivative(
    state: &AppState,
    source: &[u8],
    decoder: &str,
    max_edge: u32,
    max_bytes: u64,
) -> Result<Vec<u8>, AppError> {
    let ffmpeg = resolve_packaged_binary(&state.paths().resource_dir, "ffmpeg")?;
    let max_bytes = usize::try_from(max_bytes)
        .map_err(|_| AppError::invalid_argument("The evidence image byte limit is out of range"))?;
    let mut edge = max_edge;
    for _ in 0..24 {
        let filter = format!("scale={edge}:{edge}:force_original_aspect_ratio=decrease");
        let mut command = Command::new(&ffmpeg);
        command
            .kill_on_drop(true)
            .args([
                "-hide_banner",
                "-loglevel",
                "error",
                "-nostdin",
                "-protocol_whitelist",
                "file,pipe",
                "-f",
                "image2pipe",
                "-vcodec",
                decoder,
                "-i",
                "pipe:0",
                "-frames:v",
                "1",
                "-vf",
                &filter,
                "-f",
                "image2pipe",
                "-vcodec",
                "png",
                "pipe:1",
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command.spawn().map_err(|_| {
            AppError::new(
                ErrorCode::MediaUnsupported,
                "The packaged FFmpeg could not start",
            )
        })?;
        if let Some(mut stdin) = child.stdin.take() {
            stdin
                .write_all(source)
                .await
                .map_err(|_| AppError::io("The evidence image could not be sent to FFmpeg"))?;
        }
        let output = timeout(Duration::from_secs(10), child.wait_with_output())
            .await
            .map_err(|_| {
                AppError::new(
                    ErrorCode::MediaUnsupported,
                    "Evidence image rendering timed out",
                )
            })?
            .map_err(|_| {
                AppError::new(
                    ErrorCode::MediaUnsupported,
                    "Evidence image rendering failed",
                )
            })?;
        if !output.status.success() || output.stdout.is_empty() {
            return Err(AppError::new(
                ErrorCode::MediaUnsupported,
                "The managed evidence image could not be decoded",
            ));
        }
        if output.stdout.len() <= max_bytes {
            return Ok(output.stdout);
        }
        let next_edge = ((u64::from(edge) * 3) / 4).max(1) as u32;
        if next_edge >= edge {
            break;
        }
        edge = next_edge;
    }
    Err(AppError::new(
        ErrorCode::MediaUnsupported,
        "The evidence image could not fit the approved derivative byte limit",
    ))
}

/// Read one bounded managed evidence image for the live assistant run.
///
/// This is deliberately separate from the renderer artifact command: only
/// frame/thumbnail PNG or JPEG artifacts can cross this supervised bridge,
/// and the native active-run recipient owns the evidence-consent lookup.
pub async fn read_evidence_image(
    state: &AppState,
    context: &PrivateRequestContext,
    params: Value,
) -> Result<Value, AppError> {
    state.validate_generation(context.generation)?;
    if context.project_id != state.current_project_id() {
        return Err(AppError::stale_session(
            "The evidence request belongs to another project",
        ));
    }
    let run_id = context
        .run_id
        .as_deref()
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            AppError::new(
                ErrorCode::PermissionDenied,
                "Evidence bytes require a live assistant run",
            )
        })?;
    if context.caller.run_id() != Some(run_id) {
        return Err(AppError::stale_session(
            "The evidence request run does not match its native caller",
        ));
    }
    let request: EvidenceImageReadParams = serde_json::from_value(params)
        .map_err(|_| AppError::invalid_argument("The evidence image request is invalid"))?;
    if request.artifact_id.is_empty() || request.artifact_id.len() > 256 {
        return Err(AppError::invalid_argument(
            "The evidence artifact ID is invalid",
        ));
    }
    if request.offset.is_some() || request.length.is_some() {
        return Err(AppError::invalid_argument(
            "Image derivative sizing cannot be combined with an artifact range",
        ));
    }
    let max_edge = request.max_edge.unwrap_or(MAX_EVIDENCE_IMAGE_EDGE);
    let max_bytes = request.max_bytes.unwrap_or(MAX_EVIDENCE_IMAGE_BYTES);
    if max_edge == 0 || max_edge > MAX_EVIDENCE_IMAGE_EDGE {
        return Err(AppError::invalid_argument(
            "The evidence image edge limit is invalid",
        ));
    }
    if max_bytes == 0 || max_bytes > MAX_EVIDENCE_IMAGE_BYTES {
        return Err(AppError::invalid_argument(
            "The evidence image byte limit is invalid",
        ));
    }

    let scope = state.permissions().scope_for_state(state)?;
    state.permissions().require_active_run(
        context.generation,
        context.project_id.as_deref(),
        run_id,
    )?;
    let (provider_id, account_id) = state
        .assistant()
        .active_run_recipient(context.generation, run_id)?;
    state
        .permissions()
        .require_evidence(&scope, &provider_id, &account_id)?;

    let workspace_id = state
        .current_workspace_id()
        .ok_or_else(|| AppError::stale_session("The evidence workspace is no longer open"))?;
    let store = state.current_store()?;
    if store.workspace_id() != workspace_id {
        return Err(AppError::stale_session(
            "The evidence workspace binding is retired",
        ));
    }
    let artifacts = ArtifactStore::for_project(store.root(), workspace_id.clone())?;
    let path = artifacts.managed_path(&request.artifact_id)?;
    let kind_directory = path
        .parent()
        .and_then(|parent| parent.file_name())
        .and_then(|name| name.to_str());
    if !matches!(kind_directory, Some("frames") | Some("thumbnails")) {
        return Err(AppError::new(
            ErrorCode::PermissionDenied,
            "Only managed frame or thumbnail artifacts may be shared as evidence",
        ));
    }
    let extension = path
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    let decoder = match extension.as_str() {
        "png" => "png",
        "jpg" | "jpeg" => "mjpeg",
        _ => {
            return Err(AppError::new(
                ErrorCode::PermissionDenied,
                "Only managed PNG or JPEG artifacts may be shared as evidence",
            ))
        }
    };
    let (source_size, _) =
        state.artifact_metadata_at(context.generation, &workspace_id, &request.artifact_id)?;
    if source_size == 0 || source_size > MAX_EVIDENCE_SOURCE_BYTES {
        return Err(AppError::new(
            ErrorCode::MediaUnsupported,
            "The managed evidence image is outside the supported source size",
        ));
    }
    let source = tokio::fs::read(&path).await.map_err(|_| {
        AppError::new(
            ErrorCode::AssetUnavailable,
            "The managed evidence image is unavailable",
        )
    })?;
    if source.len() as u64 != source_size {
        return Err(AppError::stale_session(
            "The managed evidence image changed while it was being read",
        ));
    }
    let bytes = render_evidence_derivative(state, &source, decoder, max_edge, max_bytes).await?;
    state.validate_generation(context.generation)?;
    if context.project_id != state.current_project_id()
        || state.current_workspace_id().as_deref() != Some(workspace_id.as_str())
    {
        return Err(AppError::stale_session(
            "The evidence context changed while the image was being read",
        ));
    }
    state.permissions().require_active_run(
        context.generation,
        context.project_id.as_deref(),
        run_id,
    )?;
    let (post_provider_id, post_account_id) = state
        .assistant()
        .active_run_recipient(context.generation, run_id)?;
    if post_provider_id != provider_id || post_account_id != account_id {
        return Err(AppError::stale_session(
            "The assistant recipient changed while the image was being read",
        ));
    }
    state
        .permissions()
        .require_evidence(&scope, &post_provider_id, &post_account_id)?;
    if bytes.len() as u64 > max_bytes || bytes.is_empty() {
        return Err(AppError::new(
            ErrorCode::MediaUnsupported,
            "The evidence image derivative is outside the approved byte limit",
        ));
    }
    serde_json::to_value(EvidenceImageReadReply {
        artifact_id: request.artifact_id,
        mime_type: "image/png".to_owned(),
        base64: base64::engine::general_purpose::STANDARD.encode(&bytes),
        byte_size: bytes.len() as u64,
        offset: 0,
    })
    .map_err(|_| AppError::schema("The evidence image response could not be encoded"))
}

fn is_editor_method(method: &str) -> bool {
    matches!(
        method,
        "project_status"
            | "project_create"
            | "project_open"
            | "project_save"
            | "project_close"
            | "project_snapshot"
            | "timeline_snapshot"
            | "timeline_selection"
            | "project_history"
            | "edit_project"
            | "media"
            | "jobs"
            | "preview"
            | "export_video"
            | "evidence"
            | "transcript"
            | "analyze_media"
            | "sample_frames"
            | "create_graphic"
            | "permissions"
    )
}

fn is_private_method(method: &str) -> bool {
    matches!(
        method,
        "assistant"
            | "providers"
            | "credential_read"
            | "credential_list"
            | "credential_lease_acquire"
            | "credential_lease_commit"
            | "credential_lease_release"
            | "credential_delete"
            | "evidence_image_read"
            | "auth_open"
            | "auth_prompt"
            | "auth_event"
    )
}

fn is_private_event(event: &str) -> bool {
    matches!(
        event,
        "providers_auth_event" | "providers_auth_prompt" | "credential_auth_event" | "auth_event"
    )
}

fn response_envelope(request: &WireEnvelope, result: Result<Value, AppError>) -> WireEnvelope {
    let body = match result {
        Ok(data) => WireMessage::Response {
            ok: true,
            data: Some(data),
            error: None,
        },
        Err(error) => WireMessage::Response {
            ok: false,
            data: None,
            error: Some(error),
        },
    };
    WireEnvelope {
        v: PROTOCOL_VERSION,
        id: request.id.clone(),
        project_id: request.project_id.clone(),
        generation: request.generation,
        run_id: request.run_id.clone(),
        body,
    }
}

async fn stdout_loop(stdout: ChildStdout, bridge: AgentBridge) {
    let mut reader = BufReader::new(stdout);
    loop {
        match read_limited_line(&mut reader).await {
            Ok(None) => {
                bridge.fail(AppError::io("The agent bridge reached EOF"));
                break;
            }
            Ok(Some(line)) => {
                if line.is_empty() {
                    bridge.fail(AppError::schema("The agent bridge emitted an empty line"));
                    break;
                }
                let envelope = match serde_json::from_slice::<WireEnvelope>(&line) {
                    Ok(envelope) => envelope,
                    Err(_) => {
                        bridge.fail(AppError::schema("The agent bridge emitted invalid JSON"));
                        break;
                    }
                };
                if let Err(error) = bridge.handle_message(envelope).await {
                    bridge.fail(error);
                    break;
                }
            }
            Err(error) => {
                let message = if error.kind() == io::ErrorKind::InvalidData {
                    AppError::schema("The agent bridge emitted a line larger than 16 MiB")
                } else {
                    AppError::io("The agent bridge stdout could not be read")
                };
                bridge.fail(message);
                break;
            }
        }
    }
}

async fn stderr_loop(stderr: ChildStderr, _bridge: AgentBridge) {
    let mut reader = BufReader::new(stderr);
    loop {
        match read_limited_line(&mut reader).await {
            Ok(None) => break,
            Ok(Some(line)) => {
                let text = String::from_utf8_lossy(&line);
                eprintln!("[cutterhoochee-agent] {}", redact_diagnostics(&text));
            }
            Err(_) => break,
        }
    }
}

async fn wait_loop(bridge: AgentBridge) {
    loop {
        let status = {
            let mut child = bridge.inner.child.lock().await;
            match child.try_wait() {
                Ok(status) => status,
                Err(_) => {
                    bridge.fail(AppError::io("The agent process status could not be read"));
                    return;
                }
            }
        };
        if status.is_some() {
            if !bridge.is_terminated() {
                bridge.fail(AppError::io("The agent process exited unexpectedly"));
            }
            return;
        }
        sleep(Duration::from_millis(50)).await;
    }
}

async fn read_limited_line<R>(reader: &mut R) -> io::Result<Option<Vec<u8>>>
where
    R: AsyncBufRead + Unpin,
{
    let mut line = Vec::with_capacity(4096);
    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            if line.is_empty() {
                return Ok(None);
            }
            break;
        }
        let take = available
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(available.len(), |position| position + 1);
        if line.len().saturating_add(take) > MAX_NDJSON_LINE_BYTES + 1 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "NDJSON line exceeds the protocol limit",
            ));
        }
        line.extend_from_slice(&available[..take]);
        reader.consume(take);
        if line.last() == Some(&b'\n') {
            break;
        }
    }
    if line.last() == Some(&b'\n') {
        line.pop();
    }
    if line.last() == Some(&b'\r') {
        line.pop();
    }
    if line.len() > MAX_NDJSON_LINE_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "NDJSON line exceeds the protocol limit",
        ));
    }
    Ok(Some(line))
}

fn encode_line(envelope: &WireEnvelope) -> Result<Vec<u8>, AppError> {
    validate_wire_envelope(envelope)?;
    let mut encoded = serde_json::to_vec(envelope)
        .map_err(|_| AppError::schema("The bridge message could not be encoded"))?;
    if encoded.len() > MAX_NDJSON_LINE_BYTES {
        return Err(AppError::schema(
            "The bridge message exceeds the 16 MiB limit",
        ));
    }
    encoded.push(b'\n');
    Ok(encoded)
}

fn validate_wire_envelope(envelope: &WireEnvelope) -> Result<(), AppError> {
    if envelope.v != PROTOCOL_VERSION {
        return Err(AppError::schema(format!(
            "Unsupported bridge protocol version: {}",
            envelope.v
        )));
    }
    if envelope.id.is_empty()
        || envelope.id.len() > 256
        || envelope.id.contains('\r')
        || envelope.id.contains('\n')
    {
        return Err(AppError::invalid_argument("Bridge message id is invalid"));
    }
    if envelope.run_id.as_deref().is_some_and(|run_id| {
        run_id.is_empty() || run_id.len() > 256 || run_id.contains('\r') || run_id.contains('\n')
    }) {
        return Err(AppError::invalid_argument("Bridge run id is invalid"));
    }
    if envelope.generation > MAX_SAFE_INTEGER {
        return Err(AppError::schema(
            "generation exceeds the safe integer range",
        ));
    }
    match &envelope.body {
        WireMessage::Request { method, params } => {
            if method.is_empty() || method.len() > 128 || !params.is_object() {
                return Err(AppError::invalid_argument("Bridge request is invalid"));
            }
        }
        WireMessage::Response { ok, data, error } => {
            if *ok == data.is_none() || (!*ok && error.is_none()) || (*ok && error.is_some()) {
                return Err(AppError::schema(
                    "Bridge response has inconsistent result fields",
                ));
            }
        }
        WireMessage::Event { event, .. } => {
            if event.is_empty()
                || event.len() > 128
                || !event.chars().all(|value| {
                    value.is_ascii_lowercase()
                        || value.is_ascii_digit()
                        || value == '_'
                        || value == '.'
                        || value == '-'
                })
            {
                return Err(AppError::invalid_argument("Bridge event name is invalid"));
            }
        }
    }
    Ok(())
}

fn redact_diagnostics(input: &str) -> String {
    let mut output = String::with_capacity(input.len().min(MAX_DIAGNOSTIC_BYTES));
    let mut redact_next = 0usize;
    for token in input.split_whitespace() {
        let lower = token.to_ascii_lowercase();
        let header = lower == "authorization:"
            || lower.starts_with("authorization:")
            || lower == "api_key:"
            || lower == "apikey:";
        let bearer = lower == "bearer";
        let sensitive = redact_next > 0
            || header
            || bearer
            || lower.starts_with("sk-")
            || lower.starts_with("sk-ant-")
            || lower.starts_with("xoxb-")
            || lower.starts_with("ghp_")
            || lower.starts_with("github_pat_")
            || lower.starts_with("bearer=")
            || lower.starts_with("bearer:")
            || lower.contains("api_key=")
            || lower.contains("apikey=")
            || lower.contains("access_token=")
            || lower.contains("refresh_token=")
            || lower.contains("password=")
            || lower.contains("secret=");
        if sensitive {
            output.push_str("[REDACTED]");
            if redact_next > 0 {
                redact_next -= 1;
            } else if header {
                redact_next = 2;
            } else if bearer {
                redact_next = 1;
            }
        } else {
            output.push_str(token);
            redact_next = 0;
        }
        output.push(' ');
        if output.len() >= MAX_DIAGNOSTIC_BYTES {
            let mut limit = MAX_DIAGNOSTIC_BYTES;
            while limit > 0 && !output.is_char_boundary(limit) {
                limit -= 1;
            }
            output.truncate(limit);
            output.push('…');
            break;
        }
    }
    output.trim_end().to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn private_methods_are_strictly_allowlisted() {
        assert!(is_private_method("credential_read"));
        assert!(is_private_method("assistant"));
        assert!(!is_private_method("system_execute"));
        assert!(!is_private_method("credential_read_extra"));
    }

    #[test]
    fn private_events_do_not_match_public_editor_events() {
        assert!(is_private_event("providers_auth_event"));
        assert!(!is_private_event("assistant_text_delta"));
    }

    #[test]
    fn diagnostics_redact_credentials() {
        let redacted = redact_diagnostics("authorization: Bearer sk-ant-secret");
        assert!(!redacted.contains("sk-ant-secret"));
    }
    #[cfg(unix)]
    fn bridge_fixture_paths(root: &std::path::Path) -> AgentPaths {
        let resource_dir = root.join("resources");
        AgentPaths {
            resource_dir: resource_dir.clone(),
            node_path: std::path::PathBuf::from("/bin/sh"),
            agent_entrypoint: resource_dir.join("agent.sh"),
            app_data_dir: root.join("app-data"),
            app_cache_dir: root.join("app-cache"),
            agent_home: root.join("app-data/agent-home"),
            agent_config_dir: root.join("app-data/agent-config"),
            agent_cache_dir: root.join("app-cache/agent-cache"),
            session_dir: root.join("app-data/sessions"),
            temp_dir: root.join("app-cache/tmp"),
            artifact_dir: root.join("app-cache/artifacts"),
        }
    }

    #[cfg(unix)]
    fn shell_quote(value: &str) -> String {
        format!("'{}'", value.replace('\'', "'\\''"))
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn child_eof_rejects_pending_request_revokes_permission_and_prevents_late_write() {
        use crate::editor::dispatcher::CallerContext;
        use crate::error::ErrorCode;
        use crate::ipc::EditorRequest;
        use crate::permissions::{PermissionEvent, PermissionsAction, SystemWriteRequest};
        use std::fs;

        let root =
            std::env::temp_dir().join(format!("cutterhoochee-agent-bridge-{}", Uuid::new_v4()));
        fs::create_dir_all(root.join("resources")).expect("bridge fixture resources");
        let target = root.join("target.txt");
        let original = b"original bytes".to_vec();
        fs::write(&target, &original).expect("bridge fixture target");

        let paths = bridge_fixture_paths(&root);
        let state = AppState::new(paths).expect("bridge fixture state");
        state
            .create_project(
                &root.join("fixture.cutproj"),
                "Bridge fixture".to_owned(),
                Some("16:9".to_owned()),
                Some(30),
                Some(1),
            )
            .expect("bridge fixture project");
        let scope = state
            .permissions()
            .scope_for_state(&state)
            .expect("bridge fixture permission scope");
        let run_id = format!("bridge-run-{}", Uuid::new_v4());
        state
            .permissions()
            .register_run(&scope, &run_id)
            .expect("bridge fixture run authority");
        let mut permission_events = state.permissions().subscribe();

        let tool_request =
            EditorRequest::Permissions(PermissionsAction::SystemWrite(SystemWriteRequest {
                path: target.to_string_lossy().into_owned(),
                data: b"must not be written".to_vec(),
                overwrite: true,
                operation_id: None,
            }));
        let tool_envelope = WireEnvelope {
            v: PROTOCOL_VERSION,
            id: "tool-request".to_owned(),
            project_id: state.current_project_id(),
            generation: state.generation(),
            run_id: Some(run_id.clone()),
            body: WireMessage::Request {
                method: tool_request.method().to_owned(),
                params: tool_request.params(),
            },
        };
        let tool_line = serde_json::to_string(&tool_envelope).expect("encode tool request");
        let script = root.join("resources/agent.sh");
        let script_body = format!(
            "printf '%s\\n' {}\nIFS= read -r _ || true\nexit 0\n",
            shell_quote(&tool_line)
        );
        fs::write(&script, script_body).expect("write bridge fixture agent");

        let bridge = state
            .ensure_agent_bridge()
            .await
            .expect("start bridge fixture agent");
        let permission = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                if let Some(permission) = state
                    .permissions()
                    .pending(&scope)
                    .expect("list bridge fixture permissions")
                    .into_iter()
                    .next()
                {
                    break permission;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("tool permission should become pending");

        let caller_error = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            bridge.request(EditorRequest::ProjectStatus {}, None),
        )
        .await
        .expect("pending bridge request should be rejected after child EOF")
        .expect_err("child EOF must reject the pending bridge caller");
        assert_eq!(caller_error.code, ErrorCode::IoError);

        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                match permission_events.recv().await {
                    Ok(PermissionEvent::Revoked { operation_id })
                        if operation_id == permission.operation_id =>
                    {
                        break operation_id;
                    }
                    Ok(_) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                        panic!("permission event stream closed before revocation")
                    }
                }
            }
        })
        .await
        .expect("pending tool permission should be revoked");

        let ui = CallerContext::human_window(
            "main".to_owned(),
            scope.generation,
            scope.project_id.clone(),
        );
        let late_error = state
            .permissions()
            .answer(&ui, &permission.operation_id, true)
            .expect_err("late permission reply must be rejected");
        assert_eq!(late_error.code, ErrorCode::PermissionDenied);
        assert_eq!(
            fs::read(&target).expect("read bridge fixture target"),
            original
        );

        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                if bridge.is_terminated() && state.bridge().is_none() {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("bridge lifecycle teardown should complete");

        let child_status = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                let status = {
                    let mut child = bridge.inner.child.lock().await;
                    child.try_wait().expect("read bridge fixture child status")
                };
                if let Some(status) = status {
                    break status;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("bridge fixture child should be reaped");
        assert!(child_status.success());

        drop(bridge);
        drop(state);
        fs::remove_dir_all(&root).expect("remove bridge fixture");
        assert!(!root.exists());
    }
}
