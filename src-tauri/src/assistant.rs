use crate::agent_bridge::{AgentBridge, BridgeEvent, PrivateBridgeEvent};
use crate::editor::dispatcher::{CallerContext, CallerKind};
use crate::error::{AppError, ErrorCode};
use crate::permissions::PermissionScope;
use crate::state::AppState;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::Notify;
use tokio::time::timeout;
use ts_rs::TS;
use uuid::Uuid;

const MAX_PROMPT_BYTES: usize = 1024 * 1024;
const MAX_PROVIDER_ID_BYTES: usize = 128;
const MAX_MODEL_ID_BYTES: usize = 256;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(tag = "action", rename_all = "snake_case")]
#[ts(tag = "action", rename_all = "snake_case")]
pub enum AssistantAction {
    Status {},
    History {},
    Prompt { text: String },
    Stop {},
    Restart {},
    NewSession {},
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase")]
pub struct AssistantUsage {
    #[ts(type = "SafeInteger")]
    pub input: u64,
    #[ts(type = "SafeInteger")]
    pub output: u64,
    #[serde(rename = "cacheRead")]
    #[ts(rename = "cacheRead", type = "SafeInteger")]
    pub cache_read: u64,
    #[serde(rename = "cacheWrite")]
    #[ts(rename = "cacheWrite", type = "SafeInteger")]
    pub cache_write: u64,
    #[ts(type = "SafeInteger")]
    pub total: u64,
    pub cost: Option<f64>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase")]
pub struct AssistantStatus {
    pub active: bool,
    pub configured: bool,
    pub provider_id: Option<String>,
    pub model_id: Option<String>,
    pub session_id: Option<String>,
    pub usage: Option<AssistantUsage>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct SessionHistoryMessage {
    pub role: String,
    pub text: String,
    #[ts(optional, type = "SafeInteger")]
    pub timestamp: Option<u64>,
    pub tool_name: Option<String>,
    pub is_error: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(tag = "action", rename_all = "snake_case")]
#[ts(tag = "action", rename_all = "snake_case")]
pub enum AssistantReply {
    Status {
        status: AssistantStatus,
    },
    History {
        messages: Vec<SessionHistoryMessage>,
    },
    Prompt {
        accepted: bool,
    },
    Stop {
        stopped: bool,
    },
    Restart {
        restarted: bool,
    },
    NewSession {
        created: bool,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum AuthType {
    ApiKey,
    Oauth,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase")]
pub struct ProviderModel {
    pub id: String,
    pub name: String,
    pub reasoning: bool,
    pub input: Vec<String>,
    #[ts(type = "SafeInteger")]
    pub context_window: u64,
    #[ts(type = "SafeInteger")]
    pub max_tokens: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase")]
pub struct ProviderStatus {
    pub id: String,
    pub name: String,
    pub configured: bool,
    pub auth_type: Option<AuthType>,
    #[ts(type = "SafeInteger")]
    pub model_count: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase")]
pub struct SelectedModel {
    pub provider_id: String,
    pub model_id: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(tag = "action", rename_all = "snake_case")]
#[ts(tag = "action", rename_all = "snake_case")]
pub enum ProvidersAction {
    List {},
    Models {
        #[serde(rename = "providerId")]
        #[ts(rename = "providerId")]
        provider_id: String,
    },
    Login {
        #[serde(rename = "providerId")]
        #[ts(rename = "providerId")]
        provider_id: String,
        #[serde(rename = "authType")]
        #[ts(rename = "authType")]
        auth_type: AuthType,
        #[serde(rename = "sessionOnly")]
        #[ts(rename = "sessionOnly")]
        session_only: Option<bool>,
    },
    Answer {
        #[serde(rename = "promptId")]
        #[ts(rename = "promptId")]
        prompt_id: String,
        value: String,
    },
    Logout {
        #[serde(rename = "providerId")]
        #[ts(rename = "providerId")]
        provider_id: String,
    },
    Select {
        #[serde(rename = "providerId")]
        #[ts(rename = "providerId")]
        provider_id: String,
        #[serde(rename = "modelId")]
        #[ts(rename = "modelId")]
        model_id: String,
    },
    Refresh {},
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(tag = "action", rename_all = "snake_case")]
#[ts(tag = "action", rename_all = "snake_case")]
pub enum ProvidersReply {
    List {
        providers: Vec<ProviderStatus>,
        selected: Option<SelectedModel>,
    },
    Models {
        #[serde(rename = "providerId")]
        #[ts(rename = "providerId")]
        provider_id: String,
        models: Vec<ProviderModel>,
    },
    Login {
        #[serde(rename = "providerId")]
        #[ts(rename = "providerId")]
        provider_id: String,
        #[serde(rename = "type")]
        #[ts(rename = "type")]
        auth_type: AuthType,
        configured: bool,
    },
    Answer {
        #[serde(rename = "promptId")]
        #[ts(rename = "promptId")]
        prompt_id: String,
        accepted: bool,
    },
    Logout {
        #[serde(rename = "providerId")]
        #[ts(rename = "providerId")]
        provider_id: String,
        configured: bool,
    },
    Select {
        selected: SelectedModel,
    },
    Refresh {
        refreshed: bool,
    },
}

/// Provider runtime is intentionally the same native owner as assistant
/// runtime; it has no second bridge or credential path.

#[derive(Debug, Clone, PartialEq, Eq)]
struct ActiveRun {
    generation: u64,
    project_id: Option<String>,
    run_id: String,
    connection_id: String,
    provider_id: String,
    account_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ActiveLogin {
    generation: u64,
    project_id: Option<String>,
    connection_id: String,
    auth_operation_id: String,
    provider_id: String,
    auth_type: AuthType,
    prompt_ids: HashSet<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ProviderOperation {
    generation: u64,
    token: String,
    cancelled: bool,
    owner_released: bool,
    stop_acknowledged: bool,
}

#[derive(Default)]
struct AssistantInner {
    active_run: Option<ActiveRun>,
    stopping_run: Option<ActiveRun>,
    active_login: Option<ActiveLogin>,
    selected_provider_id: Option<String>,
    provider_operation: Option<ProviderOperation>,
}

/// Native supervisor for the Pi sidecar. It creates/retire run authority before
/// sending assistant work and leaves ordinary editor dispatch usable whenever
/// the sidecar is unavailable.
#[derive(Clone, Default)]
pub struct AssistantRuntime {
    inner: Arc<Mutex<AssistantInner>>,
    provider_operation_notify: Arc<Notify>,
}

impl AssistantRuntime {
    pub fn new() -> Self {
        Self::default()
    }

    pub async fn handle(
        &self,
        action: AssistantAction,
        caller: &CallerContext,
        state: &AppState,
    ) -> Result<AssistantReply, AppError> {
        require_assistant_caller(caller)?;
        if matches!(&action, AssistantAction::Stop {}) {
            self.stop(state).await?;
            return Ok(AssistantReply::Stop { stopped: true });
        }
        let bridge = state.ensure_agent_bridge().await?;
        match action {
            AssistantAction::Status {} => {
                let data = bridge
                    .private_request("assistant", json!({"action": "status"}), None)
                    .await?;
                parse_reply(data)
            }
            AssistantAction::History {} => {
                let data = bridge
                    .private_request("assistant", json!({"action": "history"}), None)
                    .await?;
                parse_reply(data)
            }
            AssistantAction::Prompt { text } => {
                validate_prompt(&text)?;
                let operation_token = self.begin_provider_operation(bridge.generation(), true)?;
                let recipient = self
                    .resolve_selected_recipient(state, &bridge, &operation_token)
                    .await;
                let (provider_id, account_id) = match recipient {
                    Ok(recipient) => recipient,
                    Err(error) => {
                        self.release_provider_operation(&operation_token);
                        return Err(error);
                    }
                };
                let scope = match state.permissions().scope_for_state(state) {
                    Ok(scope) => scope,
                    Err(error) => {
                        self.release_provider_operation(&operation_token);
                        return Err(error);
                    }
                };
                if let Err(error) =
                    state
                        .permissions()
                        .require_evidence(&scope, &provider_id, &account_id)
                {
                    let _ = state.emit_sanitized_event(
                        "evidence_requested",
                        None,
                        Some(json!({
                            "providerId": provider_id,
                            "accountId": account_id,
                            "scope": scope,
                        })),
                    );
                    self.release_provider_operation(&operation_token);
                    return Err(error);
                }
                let run_id = Uuid::new_v4().to_string();
                let active = ActiveRun {
                    generation: scope.generation,
                    project_id: scope.project_id.clone(),
                    run_id: run_id.clone(),
                    connection_id: bridge.connection_id().to_owned(),
                    provider_id,
                    account_id,
                };
                let promotion = (|| -> Result<(), AppError> {
                    let mut inner = self
                        .inner
                        .lock()
                        .map_err(|_| AppError::io("The assistant runtime lock is unavailable"))?;
                    let owns_operation =
                        inner.provider_operation.as_ref().is_some_and(|operation| {
                            operation.token == operation_token
                                && operation.generation == scope.generation
                                && !operation.cancelled
                        });
                    if !owns_operation || inner.active_run.is_some() || inner.stopping_run.is_some()
                    {
                        return Err(AppError::stale_session(
                            "The assistant prompt context is no longer active",
                        ));
                    }
                    state.permissions().register_run(&scope, &run_id)?;
                    inner.active_run = Some(active.clone());
                    inner.provider_operation = None;
                    Ok(())
                })();
                if let Err(error) = promotion {
                    self.release_provider_operation(&operation_token);
                    return Err(error);
                }
                if let Err(error) = state.permissions().require_evidence(
                    &scope,
                    &active.provider_id,
                    &active.account_id,
                ) {
                    self.finish_run(state, &active)?;
                    return Err(error);
                }
                self.active_run_recipient(active.generation, &active.run_id)?;
                let request = json!({"action": "prompt", "text": text});
                let result = bridge
                    .private_request("assistant", request, Some(run_id.clone()))
                    .await
                    .and_then(parse_reply);
                self.finish_run(state, &active)?;
                result
            }
            AssistantAction::Stop {} => {
                self.stop_active(state, &bridge).await?;
                Ok(AssistantReply::Stop { stopped: true })
            }
            AssistantAction::Restart {} => {
                self.stop_active(state, &bridge).await?;
                let data = bridge
                    .private_request("assistant", json!({"action": "restart"}), None)
                    .await?;
                parse_reply(data)
            }
            AssistantAction::NewSession {} => {
                self.stop_active(state, &bridge).await?;
                let data = bridge
                    .private_request("assistant", json!({"action": "new_session"}), None)
                    .await?;
                parse_reply(data)
            }
        }
    }

    pub async fn handle_providers(
        &self,
        action: ProvidersAction,
        caller: &CallerContext,
        state: &AppState,
    ) -> Result<ProvidersReply, AppError> {
        require_assistant_caller(caller)?;
        validate_provider_action(&action)?;
        if matches!(
            &action,
            ProvidersAction::Login { .. }
                | ProvidersAction::Answer { .. }
                | ProvidersAction::Logout { .. }
                | ProvidersAction::Select { .. }
                | ProvidersAction::Refresh {}
        ) && !matches!(&caller.kind, CallerKind::HumanWindow { .. })
        {
            return Err(AppError::new(
                ErrorCode::PermissionDenied,
                "This provider operation requires a trusted desktop window",
            ));
        }
        let bridge = state.ensure_agent_bridge().await?;
        match &action {
            ProvidersAction::Login {
                provider_id,
                auth_type,
                session_only,
            } => {
                let operation_token = self.begin_provider_operation(bridge.generation(), true)?;
                let login = match self.begin_login(state, &bridge, caller, provider_id, *auth_type)
                {
                    Ok(login) => login,
                    Err(error) => {
                        self.release_provider_operation(&operation_token);
                        return Err(error);
                    }
                };
                if matches!(caller.kind, CallerKind::HumanWindow { .. }) {
                    if let Err(error) = state
                        .credentials()
                        .set_session_mode(provider_id, session_only.unwrap_or(false), caller)
                        .await
                    {
                        self.clear_login(&login.auth_operation_id);
                        self.release_provider_operation(&operation_token);
                        return Err(error);
                    }
                }
                if let Err(error) = self.require_provider_operation(&operation_token) {
                    self.clear_login(&login.auth_operation_id);
                    self.release_provider_operation(&operation_token);
                    return Err(error);
                }
                let params = match serde_json::to_value(&action) {
                    Ok(params) => params,
                    Err(_) => {
                        self.clear_login(&login.auth_operation_id);
                        self.release_provider_operation(&operation_token);
                        return Err(AppError::schema("The provider action could not be encoded"));
                    }
                };
                let response = bridge
                    .private_request("providers", params, Some(login.auth_operation_id.clone()))
                    .await;
                self.clear_login(&login.auth_operation_id);
                self.release_provider_operation(&operation_token);
                let data = response?;
                serde_json::from_value(data)
                    .map_err(|_| AppError::schema("The provider reply has an unsupported shape"))
            }
            ProvidersAction::Answer { prompt_id, .. } => {
                self.ensure_no_active_run()?;
                let login = self.active_login_for_request(state, &bridge, caller)?;
                {
                    let mut inner = self
                        .inner
                        .lock()
                        .map_err(|_| AppError::io("The assistant runtime lock is unavailable"))?;
                    let current = inner
                        .active_login
                        .as_mut()
                        .filter(|current| current.auth_operation_id == login.auth_operation_id)
                        .ok_or_else(|| {
                            AppError::stale_session(
                                "The authentication request is no longer active",
                            )
                        })?;
                    if !current.prompt_ids.remove(prompt_id) {
                        return Err(AppError::stale_session(
                            "The authentication prompt is no longer active",
                        ));
                    }
                }
                let params = serde_json::to_value(&action)
                    .map_err(|_| AppError::schema("The provider action could not be encoded"))?;
                let response = bridge
                    .private_request("providers", params, Some(login.auth_operation_id.clone()))
                    .await;
                match response {
                    Ok(data) => serde_json::from_value(data).map_err(|_| {
                        self.clear_login(&login.auth_operation_id);
                        AppError::schema("The provider reply has an unsupported shape")
                    }),
                    Err(error) => {
                        self.clear_login(&login.auth_operation_id);
                        Err(error)
                    }
                }
            }
            ProvidersAction::Logout { provider_id } => {
                let operation_token = self.begin_provider_operation(bridge.generation(), false)?;
                self.clear_login_for_provider(provider_id);
                let params = match serde_json::to_value(&action) {
                    Ok(params) => params,
                    Err(_) => {
                        self.release_provider_operation(&operation_token);
                        return Err(AppError::schema("The provider action could not be encoded"));
                    }
                };
                let data = match bridge.private_request("providers", params, None).await {
                    Ok(data) => data,
                    Err(error) => {
                        self.release_provider_operation(&operation_token);
                        return Err(error);
                    }
                };
                if let Err(error) = self.require_provider_operation(&operation_token) {
                    self.release_provider_operation(&operation_token);
                    return Err(error);
                }
                if matches!(caller.kind, CallerKind::HumanWindow { .. }) {
                    if let Err(error) = state
                        .credentials()
                        .set_session_mode(provider_id, false, caller)
                        .await
                    {
                        self.release_provider_operation(&operation_token);
                        return Err(error);
                    }
                }
                let reply = serde_json::from_value(data)
                    .map_err(|_| AppError::schema("The provider reply has an unsupported shape"));
                let clear_result = if reply.is_ok() {
                    self.clear_selected_provider(&operation_token, provider_id)
                } else {
                    Ok(())
                };
                self.release_provider_operation(&operation_token);
                match (reply, clear_result) {
                    (Ok(reply), Ok(())) => Ok(reply),
                    (Ok(_), Err(error)) => Err(error),
                    (Err(error), _) => Err(error),
                }
            }
            ProvidersAction::Select {
                provider_id,
                model_id,
            } => {
                let operation_token = self.begin_provider_operation(bridge.generation(), false)?;
                self.clear_login_all();
                let response = self.forward_provider_request(&bridge, &action).await;
                let reply = match response {
                    Ok(ProvidersReply::Select { selected })
                        if selected.provider_id == *provider_id
                            && selected.model_id == *model_id =>
                    {
                        if let Err(error) = self.require_provider_operation(&operation_token) {
                            Err(error)
                        } else {
                            let account = state.credentials().account_identity(provider_id).await;
                            match account {
                                Ok(Some(_account_id)) => self
                                    .set_selected_provider(&operation_token, provider_id)
                                    .map(|_| ProvidersReply::Select { selected }),
                                Ok(None) => Err(AppError::new(
                                    ErrorCode::AuthRequired,
                                    "Connect the selected provider before selecting a model",
                                )),
                                Err(error) => Err(error),
                            }
                        }
                    }
                    Ok(ProvidersReply::Select { .. }) => Err(AppError::schema(
                        "The provider selection reply did not match the requested model",
                    )),
                    Ok(_) => Err(AppError::schema(
                        "The provider reply has an unsupported shape",
                    )),
                    Err(error) => Err(error),
                };
                self.release_provider_operation(&operation_token);
                reply
            }
            ProvidersAction::List {}
            | ProvidersAction::Models { .. }
            | ProvidersAction::Refresh {} => self.forward_provider_request(&bridge, &action).await,
        }
    }

    /// Retire native assistant authority before aborting sidecar work. Pending
    /// provider operations retain a cancelled marker until both stop
    /// acknowledgement and owner release have occurred.
    pub async fn stop(&self, state: &AppState) -> Result<(), AppError> {
        if let Some(bridge) = state.bridge() {
            return self.stop_active(state, &bridge).await;
        }
        let (active, provider_operation_token) = {
            let mut inner = self
                .inner
                .lock()
                .map_err(|_| AppError::io("The assistant runtime lock is unavailable"))?;
            if inner.stopping_run.is_some()
                || inner
                    .provider_operation
                    .as_ref()
                    .is_some_and(|operation| operation.cancelled)
            {
                return Err(AppError::busy("The assistant stop is already in progress"));
            }
            let active = inner.active_run.clone();
            if let Some(active) = active.as_ref() {
                inner.stopping_run = Some(active.clone());
            }
            let provider_operation_token = inner
                .provider_operation
                .as_ref()
                .map(|operation| operation.token.clone());
            if let Some(operation) = inner.provider_operation.as_mut() {
                operation.cancelled = true;
            }
            // Native prompt answers become stale immediately. The sidecar
            // cancellation below is responsible for rejecting its pending
            // provider prompt and unwinding the credential lease.
            inner.active_login = None;
            (active, provider_operation_token)
        };
        if let Some(active) = active {
            if let Err(error) = state.retire_run(active.generation, &active.run_id) {
                if let Ok(mut inner) = self.inner.lock() {
                    if inner
                        .stopping_run
                        .as_ref()
                        .is_some_and(|run| run == &active)
                    {
                        inner.stopping_run = None;
                    }
                }
                self.acknowledge_provider_operation_stop(provider_operation_token.as_deref());
                if let Some(token) = provider_operation_token.as_deref() {
                    let _ = timeout(
                        Duration::from_secs(2),
                        self.wait_provider_operation_settled(token),
                    )
                    .await;
                }
                return Err(error);
            }
            if let Ok(mut inner) = self.inner.lock() {
                if inner
                    .stopping_run
                    .as_ref()
                    .is_some_and(|run| run == &active)
                {
                    inner.stopping_run = None;
                    if inner.active_run.as_ref().is_some_and(|run| run == &active) {
                        inner.active_run = None;
                    }
                }
            }
        }
        self.acknowledge_provider_operation_stop(provider_operation_token.as_deref());
        if let Some(token) = provider_operation_token.as_deref() {
            let _ = timeout(
                Duration::from_secs(2),
                self.wait_provider_operation_settled(token),
            )
            .await;
        }
        Ok(())
    }

    pub fn active_run(&self) -> Option<(u64, String)> {
        self.inner.lock().ok().and_then(|inner| {
            inner
                .active_run
                .as_ref()
                .map(|run| (run.generation, run.run_id.clone()))
        })
    }

    /// Return the native provider/account binding captured before an assistant
    /// prompt was sent. The sidecar cannot provide or override this identity.
    pub fn active_run_recipient(
        &self,
        generation: u64,
        run_id: &str,
    ) -> Result<(String, String), AppError> {
        if run_id.is_empty() {
            return Err(AppError::invalid_argument(
                "The assistant run ID is required",
            ));
        }
        let inner = self
            .inner
            .lock()
            .map_err(|_| AppError::io("The assistant runtime lock is unavailable"))?;
        let active = inner
            .active_run
            .as_ref()
            .ok_or_else(|| AppError::stale_session("The assistant run is no longer active"))?;
        if active.generation != generation || active.run_id != run_id {
            return Err(AppError::stale_session(
                "The assistant run belongs to a different native context",
            ));
        }
        if active.provider_id.is_empty() || active.account_id.is_empty() {
            return Err(AppError::new(
                ErrorCode::ProviderError,
                "The assistant run has no native recipient binding",
            ));
        }
        Ok((active.provider_id.clone(), active.account_id.clone()))
    }

    fn begin_provider_operation(
        &self,
        generation: u64,
        reject_active_login: bool,
    ) -> Result<String, AppError> {
        let token = Uuid::new_v4().to_string();
        let mut inner = self
            .inner
            .lock()
            .map_err(|_| AppError::io("The assistant runtime lock is unavailable"))?;
        if inner.active_run.is_some()
            || inner.stopping_run.is_some()
            || inner.provider_operation.is_some()
            || (reject_active_login && inner.active_login.is_some())
        {
            return Err(AppError::busy("The assistant provider context is busy"));
        }
        inner.provider_operation = Some(ProviderOperation {
            generation,
            token: token.clone(),
            cancelled: false,
            owner_released: false,
            stop_acknowledged: false,
        });
        Ok(token)
    }

    fn require_provider_operation(&self, token: &str) -> Result<(), AppError> {
        let inner = self
            .inner
            .lock()
            .map_err(|_| AppError::io("The assistant runtime lock is unavailable"))?;
        let operation = inner
            .provider_operation
            .as_ref()
            .filter(|operation| operation.token == token)
            .ok_or_else(|| {
                AppError::stale_session("The assistant provider operation is no longer active")
            })?;
        if operation.cancelled {
            return Err(AppError::stale_session(
                "The assistant provider operation was cancelled",
            ));
        }
        Ok(())
    }

    fn release_provider_operation(&self, token: &str) {
        let settled = if let Ok(mut inner) = self.inner.lock() {
            let mut settled = false;
            if let Some(operation) = inner
                .provider_operation
                .as_mut()
                .filter(|operation| operation.token == token)
            {
                if operation.cancelled {
                    operation.owner_released = true;
                    settled = operation.stop_acknowledged;
                } else {
                    settled = true;
                }
            }
            if settled {
                inner.provider_operation = None;
            }
            settled
        } else {
            false
        };
        if settled {
            self.provider_operation_notify.notify_waiters();
        }
    }

    fn acknowledge_provider_operation_stop(&self, token: Option<&str>) {
        let Some(token) = token else {
            return;
        };
        let settled = if let Ok(mut inner) = self.inner.lock() {
            let mut settled = false;
            if let Some(operation) = inner
                .provider_operation
                .as_mut()
                .filter(|operation| operation.token == token)
            {
                operation.stop_acknowledged = true;
                settled = operation.owner_released;
            }
            if settled {
                inner.provider_operation = None;
            }
            settled
        } else {
            false
        };
        if settled {
            self.provider_operation_notify.notify_waiters();
        }
    }

    async fn wait_provider_operation_settled(&self, token: &str) -> Result<(), AppError> {
        loop {
            let notified = self.provider_operation_notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            let active = self
                .inner
                .lock()
                .map_err(|_| AppError::io("The assistant runtime lock is unavailable"))?
                .provider_operation
                .as_ref()
                .is_some_and(|operation| operation.token == token);
            if !active {
                return Ok(());
            }
            notified.await;
        }
    }

    fn set_selected_provider(&self, token: &str, provider_id: &str) -> Result<(), AppError> {
        let mut inner = self
            .inner
            .lock()
            .map_err(|_| AppError::io("The assistant runtime lock is unavailable"))?;
        let operation = inner
            .provider_operation
            .as_ref()
            .filter(|operation| operation.token == token)
            .ok_or_else(|| {
                AppError::stale_session("The assistant provider operation is no longer active")
            })?;
        if operation.cancelled {
            return Err(AppError::stale_session(
                "The assistant provider operation was cancelled",
            ));
        }
        inner.selected_provider_id = Some(provider_id.to_owned());
        Ok(())
    }
    fn clear_selected_provider(&self, token: &str, provider_id: &str) -> Result<(), AppError> {
        let mut inner = self
            .inner
            .lock()
            .map_err(|_| AppError::io("The assistant runtime lock is unavailable"))?;
        let operation = inner
            .provider_operation
            .as_ref()
            .filter(|operation| operation.token == token)
            .ok_or_else(|| {
                AppError::stale_session("The assistant provider operation is no longer active")
            })?;
        if operation.cancelled {
            return Err(AppError::stale_session(
                "The assistant provider operation was cancelled",
            ));
        }
        if inner
            .selected_provider_id
            .as_deref()
            .is_some_and(|selected| selected == provider_id)
        {
            inner.selected_provider_id = None;
        }
        Ok(())
    }

    async fn resolve_selected_recipient(
        &self,
        state: &AppState,
        bridge: &AgentBridge,
        token: &str,
    ) -> Result<(String, String), AppError> {
        self.require_provider_operation(token)?;
        let list_data = bridge
            .private_request("providers", json!({"action": "list"}), None)
            .await?;
        self.require_provider_operation(token)?;
        let list = serde_json::from_value::<ProvidersReply>(list_data)
            .map_err(|_| AppError::schema("The provider list reply has an unsupported shape"))?;
        let selected = match list {
            ProvidersReply::List {
                selected: Some(selected),
                ..
            } => selected,
            ProvidersReply::List { selected: None, .. } => {
                return Err(AppError::new(
                    ErrorCode::AuthRequired,
                    "Connect a provider and select a model before prompting",
                ))
            }
            _ => {
                return Err(AppError::schema(
                    "The provider list reply has an unsupported shape",
                ))
            }
        };
        let selected_provider_id = selected.provider_id.clone();
        validate_provider_action(&ProvidersAction::Models {
            provider_id: selected_provider_id.clone(),
        })?;
        if selected.model_id.is_empty() || selected.model_id.len() > MAX_MODEL_ID_BYTES {
            return Err(AppError::invalid_argument(
                "The selected model ID is invalid",
            ));
        }
        let models_data = bridge
            .private_request(
                "providers",
                json!({
                    "action": "models",
                    "providerId": selected_provider_id.clone(),
                }),
                None,
            )
            .await?;
        self.require_provider_operation(token)?;
        let models_reply = serde_json::from_value::<ProvidersReply>(models_data)
            .map_err(|_| AppError::schema("The provider models reply has an unsupported shape"))?;
        let available = match models_reply {
            ProvidersReply::Models {
                provider_id,
                models,
            } if provider_id == selected_provider_id => models,
            _ => {
                return Err(AppError::schema(
                    "The provider models reply has an unsupported shape",
                ))
            }
        };
        if !available.iter().any(|model| model.id == selected.model_id) {
            return Err(AppError::new(
                ErrorCode::ProviderError,
                "The persisted provider model is no longer available",
            ));
        }
        let account_id = state
            .credentials()
            .account_identity(&selected_provider_id)
            .await?
            .ok_or_else(|| {
                AppError::new(
                    ErrorCode::AuthRequired,
                    "Connect the selected provider before prompting",
                )
            })?;
        self.set_selected_provider(token, &selected_provider_id)?;
        Ok((selected_provider_id, account_id))
    }
    /// Validate a provider authentication destination before any native
    /// opener call. This general policy remains useful for diagnostics and
    /// tests; the live login path applies the provider-specific policy below.
    pub fn validate_auth_destination(url: &str) -> Result<String, AppError> {
        let parsed = reqwest::Url::parse(url).map_err(|_| {
            AppError::new(ErrorCode::ProviderError, "The provider auth URL is invalid")
        })?;
        let host = parsed.host_str().ok_or_else(|| {
            AppError::new(
                ErrorCode::ProviderError,
                "The provider auth URL has no host",
            )
        })?;
        const ALLOWED: &[&str] = &[
            "auth.openai.com",
            "accounts.google.com",
            "login.microsoftonline.com",
        ];
        if parsed.scheme() != "https"
            || !ALLOWED.iter().any(|allowed| *allowed == host)
            || !parsed.username().is_empty()
            || parsed.password().is_some()
            || parsed.port().is_some_and(|port| port != 443)
        {
            return Err(AppError::new(
                ErrorCode::PermissionDenied,
                "The provider auth destination is not trusted",
            ));
        }
        Ok(parsed.to_string())
    }

    fn validate_provider_auth_destination(
        provider_id: &str,
        auth_type: AuthType,
        url: &str,
    ) -> Result<String, AppError> {
        if provider_id != "openai-codex" || auth_type != AuthType::Oauth {
            return Err(AppError::new(
                ErrorCode::PermissionDenied,
                "This provider does not support native browser authentication",
            ));
        }
        let validated = Self::validate_auth_destination(url)?;
        let parsed = reqwest::Url::parse(&validated).map_err(|_| {
            AppError::new(ErrorCode::ProviderError, "The provider auth URL is invalid")
        })?;
        if parsed.host_str() != Some("auth.openai.com") {
            return Err(AppError::new(
                ErrorCode::PermissionDenied,
                "The provider auth destination is not trusted for OpenAI Codex",
            ));
        }
        Ok(validated)
    }

    pub fn forward_private_event(
        &self,
        state: &AppState,
        event: &PrivateBridgeEvent,
    ) -> Result<(), AppError> {
        if !matches!(
            event.event.as_str(),
            "providers_auth_event" | "providers_auth_prompt"
        ) {
            return Ok(());
        }
        let Some(mut forwarded) = event.data.as_ref().and_then(Value::as_object).cloned() else {
            return Ok(());
        };
        let data = Value::Object(forwarded.clone());
        let Some(login) = self.matching_login_context(state, event, &data) else {
            return Ok(());
        };

        // The operation id gates this event natively but never becomes part
        // of the public desktop payload. Provider identity remains visible so
        // the settings surface can reject a prompt after a local provider switch.
        forwarded.remove("authOperationId");
        if event.event == "providers_auth_prompt" {
            let Some(prompt_id) = forwarded.get("promptId").and_then(Value::as_str) else {
                self.clear_login(&login.auth_operation_id);
                return Err(AppError::schema("The authentication prompt id is missing"));
            };
            if prompt_id.is_empty() || prompt_id.len() > 256 {
                self.clear_login(&login.auth_operation_id);
                return Err(AppError::schema("The authentication prompt id is invalid"));
            }
            if forwarded.get("prompt").and_then(Value::as_object).is_none() {
                self.clear_login(&login.auth_operation_id);
                return Err(AppError::schema(
                    "The authentication prompt payload is missing",
                ));
            }
            if !self.register_prompt(&login.auth_operation_id, prompt_id)? {
                return Ok(());
            }
        } else {
            let event_type = forwarded.get("type").and_then(Value::as_str);
            match event_type {
                Some("auth_url") => {
                    let Some(url) = forwarded.get("url").and_then(Value::as_str) else {
                        self.clear_login(&login.auth_operation_id);
                        return Err(AppError::schema("The provider auth URL is missing"));
                    };
                    let validated = match Self::validate_provider_auth_destination(
                        &login.provider_id,
                        login.auth_type,
                        url,
                    ) {
                        Ok(validated) => validated,
                        Err(error) => {
                            self.clear_login(&login.auth_operation_id);
                            return Err(error);
                        }
                    };
                    if let Err(error) =
                        state.open_provider_auth_url_for_provider(&login.provider_id, &validated)
                    {
                        self.clear_login(&login.auth_operation_id);
                        return Err(error);
                    }
                    // The URL may carry OAuth state/code material. The
                    // trusted native opener consumes it; the WebView does not.
                    forwarded.remove("url");
                    forwarded.remove("verificationUri");
                }
                Some("device_code") => {
                    let Some(url) = forwarded.get("verificationUri").and_then(Value::as_str) else {
                        self.clear_login(&login.auth_operation_id);
                        return Err(AppError::schema("The provider verification URI is missing"));
                    };
                    let validated = match Self::validate_provider_auth_destination(
                        &login.provider_id,
                        login.auth_type,
                        url,
                    ) {
                        Ok(validated) => validated,
                        Err(error) => {
                            self.clear_login(&login.auth_operation_id);
                            return Err(error);
                        }
                    };
                    if let Err(error) =
                        state.open_provider_auth_url_for_provider(&login.provider_id, &validated)
                    {
                        self.clear_login(&login.auth_operation_id);
                        return Err(error);
                    }
                    forwarded.remove("verificationUri");
                    forwarded.remove("url");
                }
                Some("info") | Some("progress") => {}
                _ => {
                    self.clear_login(&login.auth_operation_id);
                    return Err(AppError::schema(
                        "The provider authentication event is unsupported",
                    ));
                }
            }
        }
        if let Err(error) =
            state.emit_sanitized_event(event.event.clone(), None, Some(Value::Object(forwarded)))
        {
            self.clear_login(&login.auth_operation_id);
            return Err(error);
        }
        Ok(())
    }

    fn begin_login(
        &self,
        state: &AppState,
        bridge: &AgentBridge,
        caller: &CallerContext,
        provider_id: &str,
        auth_type: AuthType,
    ) -> Result<ActiveLogin, AppError> {
        let generation = bridge.generation();
        state.validate_generation(generation)?;
        let project_id = state.current_project_id();
        if caller.generation != generation || caller.project_id != project_id {
            return Err(AppError::stale_session(
                "The authentication request belongs to a retired context",
            ));
        }
        let login = ActiveLogin {
            generation,
            project_id,
            connection_id: bridge.connection_id().to_owned(),
            auth_operation_id: Uuid::new_v4().to_string(),
            provider_id: provider_id.to_owned(),
            auth_type,
            prompt_ids: HashSet::new(),
        };
        let mut inner = self
            .inner
            .lock()
            .map_err(|_| AppError::io("The assistant runtime lock is unavailable"))?;
        if inner.active_run.is_some() {
            return Err(AppError::busy(
                "The assistant is already processing a prompt",
            ));
        }
        if inner.active_login.is_some() {
            return Err(AppError::new(
                ErrorCode::Busy,
                "Another provider authentication request is already active",
            ));
        }
        inner.active_login = Some(login.clone());
        Ok(login)
    }

    async fn forward_provider_request(
        &self,
        bridge: &AgentBridge,
        action: &ProvidersAction,
    ) -> Result<ProvidersReply, AppError> {
        let params = serde_json::to_value(action)
            .map_err(|_| AppError::schema("The provider action could not be encoded"))?;
        let data = bridge.private_request("providers", params, None).await?;
        serde_json::from_value(data)
            .map_err(|_| AppError::schema("The provider reply has an unsupported shape"))
    }

    fn current_login(&self) -> Result<Option<ActiveLogin>, AppError> {
        Ok(self
            .inner
            .lock()
            .map_err(|_| AppError::io("The assistant runtime lock is unavailable"))?
            .active_login
            .clone())
    }

    fn clear_login(&self, auth_operation_id: &str) {
        if let Ok(mut inner) = self.inner.lock() {
            if inner
                .active_login
                .as_ref()
                .is_some_and(|login| login.auth_operation_id == auth_operation_id)
            {
                inner.active_login = None;
            }
        }
    }

    fn clear_login_for_provider(&self, provider_id: &str) {
        if let Ok(mut inner) = self.inner.lock() {
            if inner
                .active_login
                .as_ref()
                .is_some_and(|login| login.provider_id == provider_id)
            {
                inner.active_login = None;
            }
        }
    }

    fn clear_login_all(&self) {
        if let Ok(mut inner) = self.inner.lock() {
            inner.active_login = None;
        }
    }

    fn ensure_no_active_login(&self) -> Result<(), AppError> {
        if self.current_login()?.is_some() {
            return Err(AppError::busy(
                "A provider authentication request is already active",
            ));
        }
        Ok(())
    }

    fn ensure_no_active_run(&self) -> Result<(), AppError> {
        let active = self
            .inner
            .lock()
            .map_err(|_| AppError::io("The assistant runtime lock is unavailable"))?
            .active_run
            .is_some();
        if active {
            return Err(AppError::busy(
                "The assistant is already processing a prompt",
            ));
        }
        Ok(())
    }

    fn active_login_for_request(
        &self,
        state: &AppState,
        bridge: &AgentBridge,
        caller: &CallerContext,
    ) -> Result<ActiveLogin, AppError> {
        let Some(login) = self.current_login()? else {
            return Err(AppError::stale_session(
                "There is no active provider authentication request",
            ));
        };
        let current_project = state.current_project_id();
        let valid = caller.generation == login.generation
            && caller.project_id == login.project_id
            && state.validate_generation(login.generation).is_ok()
            && current_project == login.project_id
            && bridge.generation() == login.generation
            && bridge.connection_id() == login.connection_id;
        if !valid {
            self.clear_login(&login.auth_operation_id);
            return Err(AppError::stale_session(
                "The provider authentication request belongs to a retired context",
            ));
        }
        Ok(login)
    }

    fn register_prompt(&self, auth_operation_id: &str, prompt_id: &str) -> Result<bool, AppError> {
        let mut inner = self
            .inner
            .lock()
            .map_err(|_| AppError::io("The assistant runtime lock is unavailable"))?;
        let Some(login) = inner
            .active_login
            .as_mut()
            .filter(|login| login.auth_operation_id == auth_operation_id)
        else {
            return Ok(false);
        };
        login.prompt_ids.insert(prompt_id.to_owned());
        Ok(true)
    }

    fn matching_login_context(
        &self,
        state: &AppState,
        event: &PrivateBridgeEvent,
        data: &Value,
    ) -> Option<ActiveLogin> {
        let login = self.current_login().ok().flatten()?;
        let auth_type = match data.get("authType").and_then(Value::as_str) {
            Some("api_key") => AuthType::ApiKey,
            Some("oauth") => AuthType::Oauth,
            _ => return None,
        };
        let operation_id = data.get("authOperationId").and_then(Value::as_str)?;
        let provider_id = data.get("providerId").and_then(Value::as_str)?;
        let bridge = state.bridge()?;
        if event.generation != login.generation
            || event.project_id.as_ref() != login.project_id.as_ref()
            || event.run_id.as_deref() != Some(login.auth_operation_id.as_str())
            || operation_id != login.auth_operation_id
            || provider_id != login.provider_id
            || auth_type != login.auth_type
            || state.validate_generation(login.generation).is_err()
            || state.current_project_id() != login.project_id
            || bridge.generation() != login.generation
            || bridge.connection_id() != login.connection_id
        {
            return None;
        }
        Some(login)
    }

    fn finish_run(&self, state: &AppState, active: &ActiveRun) -> Result<(), AppError> {
        let (should_finish, stopping) = {
            let inner = self
                .inner
                .lock()
                .map_err(|_| AppError::io("The assistant runtime lock is unavailable"))?;
            (
                inner.active_run.as_ref().is_some_and(|run| run == active),
                inner.stopping_run.as_ref().is_some_and(|run| run == active),
            )
        };
        if !should_finish || stopping {
            if !stopping {
                self.clear_login_all();
            }
            return Ok(());
        }
        // Retire native authority before clearing the in-memory run. A
        // failed retirement leaves the run visible so a later stop/teardown
        // can retry rather than silently abandoning it.
        if let Err(error) = state.retire_run(active.generation, &active.run_id) {
            self.clear_login_all();
            return Err(error);
        }
        if let Ok(mut inner) = self.inner.lock() {
            if inner.active_run.as_ref().is_some_and(|run| run == active) {
                inner.active_run = None;
            }
        }
        self.clear_login_all();
        Ok(())
    }

    async fn stop_active(&self, state: &AppState, bridge: &AgentBridge) -> Result<(), AppError> {
        let (active, provider_operation_token) = {
            let mut inner = self
                .inner
                .lock()
                .map_err(|_| AppError::io("The assistant runtime lock is unavailable"))?;
            if inner.stopping_run.is_some()
                || inner
                    .provider_operation
                    .as_ref()
                    .is_some_and(|operation| operation.cancelled)
            {
                return Err(AppError::busy("The assistant stop is already in progress"));
            }
            let active = inner.active_run.clone();
            if let Some(active) = active.as_ref() {
                inner.stopping_run = Some(active.clone());
            }
            let provider_operation_token = inner
                .provider_operation
                .as_ref()
                .map(|operation| operation.token.clone());
            if let Some(operation) = inner.provider_operation.as_mut() {
                operation.cancelled = true;
            }
            // Reject native prompt answers before asking the sidecar to stop.
            inner.active_login = None;
            (active, provider_operation_token)
        };
        if let Some(active) = active.as_ref() {
            // Retire first. No pending permission, credential refresh, or media
            // job can begin after this point, even if the Node abort races.
            if let Err(error) = state.retire_run(active.generation, &active.run_id) {
                if let Ok(mut inner) = self.inner.lock() {
                    if inner.stopping_run.as_ref().is_some_and(|run| run == active) {
                        inner.stopping_run = None;
                    }
                }
                self.acknowledge_provider_operation_stop(provider_operation_token.as_deref());
                if let Some(token) = provider_operation_token.as_deref() {
                    let _ = timeout(
                        Duration::from_secs(2),
                        self.wait_provider_operation_settled(token),
                    )
                    .await;
                }
                return Err(error);
            }
        }
        let stop_result = timeout(
            Duration::from_secs(2),
            bridge.private_request(
                "assistant",
                json!({"action": "stop"}),
                active.as_ref().map(|run| run.run_id.clone()),
            ),
        )
        .await;
        if !matches!(stop_result, Ok(Ok(_))) {
            bridge.force_stop();
        }
        self.acknowledge_provider_operation_stop(provider_operation_token.as_deref());
        if let Some(token) = provider_operation_token.as_deref() {
            if timeout(
                Duration::from_secs(2),
                self.wait_provider_operation_settled(token),
            )
            .await
            .is_err()
            {
                // A sidecar ACK alone is not enough to release the native
                // fence. Force the bridge so an owner still awaiting its
                // request receives an error and can release its token.
                bridge.force_stop();
                let _ = timeout(
                    Duration::from_secs(2),
                    self.wait_provider_operation_settled(token),
                )
                .await;
            }
        }
        if let Some(active) = active.as_ref() {
            if let Ok(mut inner) = self.inner.lock() {
                if inner.stopping_run.as_ref().is_some_and(|run| run == active) {
                    inner.stopping_run = None;
                    if inner.active_run.as_ref().is_some_and(|run| run == active) {
                        inner.active_run = None;
                    }
                }
            }
        }
        Ok(())
    }

    /// Retire a run synchronously before project close/switch advances the
    /// application generation. This path does not require the sidecar and is
    /// therefore safe during teardown.
    pub fn retire_generation(&self, state: &AppState, generation: u64) -> Result<(), AppError> {
        let active = self
            .inner
            .lock()
            .map_err(|_| AppError::io("The assistant runtime lock is unavailable"))?
            .active_run
            .clone()
            .filter(|run| run.generation == generation);
        let run_result = if let Some(active) = active.as_ref() {
            let result = state.retire_run(generation, &active.run_id);
            if result.is_ok() || state.generation() != generation {
                if let Ok(mut inner) = self.inner.lock() {
                    if inner.active_run.as_ref().is_some_and(|run| run == active) {
                        inner.active_run = None;
                    }
                    if inner
                        .stopping_run
                        .as_ref()
                        .is_some_and(|run| run.generation == generation)
                    {
                        inner.stopping_run = None;
                    }
                }
            }
            result
        } else {
            Ok(())
        };
        if let Ok(mut inner) = self.inner.lock() {
            if inner
                .provider_operation
                .as_ref()
                .is_some_and(|operation| operation.generation == generation)
            {
                inner.provider_operation = None;
            }
        }
        if let Ok(Some(login)) = self.current_login() {
            if login.generation == generation {
                self.clear_login(&login.auth_operation_id);
            }
        }
        let generation_result = state.permissions().retire_generation(generation);
        run_result.and(generation_result)
    }
}

fn require_assistant_caller(caller: &CallerContext) -> Result<(), AppError> {
    if matches!(
        caller.kind,
        CallerKind::HumanWindow { .. } | CallerKind::AgentSidecar { .. }
    ) {
        Ok(())
    } else {
        Err(AppError::new(
            ErrorCode::PermissionDenied,
            "The assistant caller is not trusted",
        ))
    }
}

fn validate_prompt(text: &str) -> Result<(), AppError> {
    if text.trim().is_empty() {
        return Err(AppError::invalid_argument(
            "The assistant prompt must not be empty",
        ));
    }
    if text.len() > MAX_PROMPT_BYTES {
        return Err(AppError::invalid_argument(
            "The assistant prompt exceeds 1 MiB",
        ));
    }
    Ok(())
}

fn validate_provider_action(action: &ProvidersAction) -> Result<(), AppError> {
    const PROVIDERS: &[&str] = &["anthropic", "openai", "openai-codex"];
    let provider_id = match action {
        ProvidersAction::Models { provider_id }
        | ProvidersAction::Login { provider_id, .. }
        | ProvidersAction::Logout { provider_id, .. }
        | ProvidersAction::Select { provider_id, .. } => Some(provider_id),
        _ => None,
    };
    if let Some(provider_id) = provider_id {
        if provider_id.is_empty()
            || provider_id.len() > MAX_PROVIDER_ID_BYTES
            || !PROVIDERS.contains(&provider_id.as_str())
        {
            return Err(AppError::invalid_argument("The provider is not supported"));
        }
    }
    if let ProvidersAction::Select { model_id, .. } = action {
        if model_id.is_empty() || model_id.len() > MAX_MODEL_ID_BYTES {
            return Err(AppError::invalid_argument("The model id is invalid"));
        }
    }
    if let ProvidersAction::Login {
        provider_id,
        auth_type,
        ..
    } = action
    {
        let expected = if provider_id == "openai-codex" {
            AuthType::Oauth
        } else {
            AuthType::ApiKey
        };
        if *auth_type != expected {
            return Err(AppError::invalid_argument(
                "This provider supports only its approved authentication type",
            ));
        }
    }
    Ok(())
}

fn parse_reply(data: Value) -> Result<AssistantReply, AppError> {
    serde_json::from_value(data)
        .map_err(|_| AppError::schema("The assistant reply has an unsupported shape"))
}

// Keep the private event type imported in this module's public API docs even
// when an integration build does not subscribe to the optional channel.
#[allow(dead_code)]
fn _private_event_contract(_: &PrivateBridgeEvent) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_auth_matrix_is_strict() {
        assert!(validate_provider_action(&ProvidersAction::Login {
            provider_id: "openai-codex".to_owned(),
            auth_type: AuthType::ApiKey,
            session_only: None,
        })
        .is_err());
        assert!(validate_provider_action(&ProvidersAction::Login {
            provider_id: "openai".to_owned(),
            auth_type: AuthType::ApiKey,
            session_only: Some(true),
        })
        .is_ok());
    }

    #[test]
    fn auth_opener_requires_approved_https_hosts() {
        assert!(AssistantRuntime::validate_auth_destination("http://auth.openai.com/").is_err());
        assert!(AssistantRuntime::validate_auth_destination("https://example.invalid/").is_err());
        assert!(
            AssistantRuntime::validate_auth_destination("https://auth.openai.com/callback").is_ok()
        );
    }
    fn install_provider_operation(runtime: &AssistantRuntime, token: &str) {
        let mut inner = runtime.inner.lock().expect("assistant runtime lock");
        inner.provider_operation = Some(ProviderOperation {
            generation: 1,
            token: token.to_owned(),
            cancelled: true,
            owner_released: false,
            stop_acknowledged: false,
        });
    }

    #[test]
    fn provider_operation_release_before_stop_ack_waits_for_both() {
        let runtime = AssistantRuntime::new();
        let token = "release-before-ack";
        install_provider_operation(&runtime, token);

        runtime.release_provider_operation(token);
        let operation = runtime
            .inner
            .lock()
            .expect("assistant runtime lock")
            .provider_operation
            .clone()
            .expect("cancelled operation remains fenced");
        assert!(operation.owner_released);
        assert!(!operation.stop_acknowledged);

        runtime.acknowledge_provider_operation_stop(Some(token));
        assert!(runtime
            .inner
            .lock()
            .expect("assistant runtime lock")
            .provider_operation
            .is_none());
    }

    #[test]
    fn provider_operation_stop_ack_before_release_waits_for_both() {
        let runtime = AssistantRuntime::new();
        let token = "ack-before-release";
        install_provider_operation(&runtime, token);

        runtime.acknowledge_provider_operation_stop(Some(token));
        let operation = runtime
            .inner
            .lock()
            .expect("assistant runtime lock")
            .provider_operation
            .clone()
            .expect("unreleased operation remains fenced");
        assert!(!operation.owner_released);
        assert!(operation.stop_acknowledged);

        runtime.release_provider_operation(token);
        assert!(runtime
            .inner
            .lock()
            .expect("assistant runtime lock")
            .provider_operation
            .is_none());
    }

    #[test]
    fn provider_operation_handshake_ignores_stale_tokens() {
        let runtime = AssistantRuntime::new();
        let token = "current-operation";
        install_provider_operation(&runtime, token);

        runtime.release_provider_operation("stale-operation");
        runtime.acknowledge_provider_operation_stop(Some("stale-operation"));
        let operation = runtime
            .inner
            .lock()
            .expect("assistant runtime lock")
            .provider_operation
            .clone()
            .expect("current operation remains fenced");
        assert_eq!(operation.token, token);
        assert!(!operation.owner_released);
        assert!(!operation.stop_acknowledged);
    }
}
