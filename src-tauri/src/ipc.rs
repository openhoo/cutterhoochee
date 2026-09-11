use crate::assistant::{AssistantAction, AssistantReply, ProvidersAction, ProvidersReply};
use crate::editor::operations::EditOp;
use crate::error::{AppError, ErrorCode};
use crate::media::evidence::{
    AnalyzeMediaAction, CreateGraphicAction, SampleFramesAction, TranscriptAction,
};
use crate::media::export::{ExportAction, ExportReply};
use crate::media::render::{PreviewAction, PreviewReply};
use crate::media::{EvidenceAction, EvidenceReply, JobsAction, JobsReply, MediaAction, MediaReply};
use crate::permissions::{PermissionsAction, PermissionsReply};
use crate::project::model::ProjectDocument;
use crate::project::store::EditResult;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use ts_rs::TS;

pub const PROTOCOL_VERSION: u8 = 1;
pub const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;
pub const MAX_NDJSON_LINE_BYTES: usize = 16 * 1024 * 1024;
pub const MAX_BRIDGE_ID_BYTES: usize = 256;

/// A sanitized event emitted to the trusted desktop window. Event payloads are
/// produced by native runtimes and never accepted as authority from the UI.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase")]
pub struct EditorEvent {
    pub kind: String,
    pub project_id: Option<String>,
    #[ts(type = "SafeInteger")]
    pub generation: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub run_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional, type = "unknown")]
    pub data: Option<Value>,
}

/// Typed requests accepted by both the trusted Tauri window and the
/// supervised agent. Open/create never carry a filesystem path: the native
/// handler owns the dialog and chooses the resulting canonical root.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(tag = "method", content = "params", rename_all = "snake_case")]
#[ts(tag = "method", content = "params", rename_all = "snake_case")]
pub enum EditorRequest {
    ProjectStatus {},
    ProjectCreate {
        name: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        #[ts(optional)]
        aspect: Option<String>,
        #[serde(rename = "fpsNum", skip_serializing_if = "Option::is_none")]
        #[ts(rename = "fpsNum", optional, type = "SafeInteger")]
        fps_num: Option<u64>,
        #[serde(rename = "fpsDen", skip_serializing_if = "Option::is_none")]
        #[ts(rename = "fpsDen", optional, type = "SafeInteger")]
        fps_den: Option<u64>,
    },
    ProjectOpen {},
    ProjectSave {},
    ProjectClose {},
    ProjectSnapshot {},
    TimelineSnapshot {},
    TimelineSelection {
        selection: TimelineSelection,
    },
    ProjectHistory {
        action: String,
        #[serde(rename = "expectedRevision")]
        #[ts(rename = "expectedRevision", type = "SafeInteger")]
        expected_revision: u64,
        #[serde(
            rename = "expectedTransactionId",
            skip_serializing_if = "Option::is_none"
        )]
        #[ts(rename = "expectedTransactionId", optional)]
        expected_transaction_id: Option<String>,
    },
    EditProject {
        #[serde(rename = "transactionId")]
        #[ts(rename = "transactionId")]
        transaction_id: String,
        #[serde(rename = "expectedRevision")]
        #[ts(rename = "expectedRevision", type = "SafeInteger")]
        expected_revision: u64,
        label: String,
        operations: Vec<EditOp>,
        #[serde(rename = "dryRun", skip_serializing_if = "Option::is_none")]
        #[ts(rename = "dryRun", optional)]
        dry_run: Option<bool>,
    },
    Media(MediaAction),
    Jobs(JobsAction),
    Preview(PreviewAction),
    ExportVideo(ExportAction),
    Evidence(EvidenceAction),
    Transcript(TranscriptAction),
    AnalyzeMedia(AnalyzeMediaAction),
    SampleFrames(SampleFramesAction),
    CreateGraphic(CreateGraphicAction),
    Assistant(AssistantAction),
    Providers(ProvidersAction),
    Permissions(PermissionsAction),
}

impl EditorRequest {
    pub fn method(&self) -> &'static str {
        match self {
            Self::ProjectStatus {} => "project_status",
            Self::ProjectCreate { .. } => "project_create",
            Self::ProjectOpen {} => "project_open",
            Self::ProjectSave {} => "project_save",
            Self::ProjectClose {} => "project_close",
            Self::ProjectSnapshot {} => "project_snapshot",
            Self::TimelineSnapshot {} => "timeline_snapshot",
            Self::TimelineSelection { .. } => "timeline_selection",
            Self::ProjectHistory { .. } => "project_history",
            Self::EditProject { .. } => "edit_project",
            Self::Media(..) => "media",
            Self::Jobs(..) => "jobs",
            Self::Preview(..) => "preview",
            Self::ExportVideo(..) => "export_video",
            Self::Evidence(..) => "evidence",
            Self::Transcript(..) => "transcript",
            Self::AnalyzeMedia(..) => "analyze_media",
            Self::SampleFrames(..) => "sample_frames",
            Self::CreateGraphic(..) => "create_graphic",
            Self::Assistant(..) => "assistant",
            Self::Providers(..) => "providers",
            Self::Permissions(..) => "permissions",
        }
    }

    pub fn params(&self) -> Value {
        serde_json::to_value(self)
            .ok()
            .and_then(|value| value.get("params").cloned())
            .unwrap_or_else(|| Value::Object(Map::new()))
    }

    pub fn from_wire(method: &str, params: &Value) -> Result<Self, AppError> {
        if !params.is_object() {
            return Err(AppError::invalid_argument(
                "Editor request params must be an object",
            ));
        }
        let fields = params.as_object().expect("object checked");
        let unknown = |allowed: &[&str]| {
            fields
                .keys()
                .find(|key| !allowed.contains(&key.as_str()))
                .cloned()
        };
        match method {
            "project_status" => {
                if let Some(field) = unknown(&[]) {
                    return Err(AppError::invalid_argument(format!(
                        "project_status does not accept field {field}"
                    )));
                }
                Ok(Self::ProjectStatus {})
            }
            "project_create" => {
                if let Some(field) = unknown(&["name", "aspect", "fpsNum", "fpsDen"]) {
                    return Err(AppError::invalid_argument(format!(
                        "project_create has an unknown field {field}"
                    )));
                }
                let name = fields
                    .get("name")
                    .and_then(Value::as_str)
                    .filter(|value| !value.trim().is_empty())
                    .ok_or_else(|| AppError::invalid_argument("project_create.name is required"))?
                    .to_owned();
                let aspect = optional_string(fields, "aspect")?;
                let fps_num = optional_safe_integer(fields, "fpsNum")?;
                let fps_den = optional_safe_integer(fields, "fpsDen")?;
                Ok(Self::ProjectCreate {
                    name,
                    aspect,
                    fps_num,
                    fps_den,
                })
            }
            "project_open" => {
                if let Some(field) = unknown(&[]) {
                    return Err(AppError::invalid_argument(format!(
                        "project_open does not accept field {field}"
                    )));
                }
                Ok(Self::ProjectOpen {})
            }
            "project_save" => {
                if let Some(field) = unknown(&[]) {
                    return Err(AppError::invalid_argument(format!(
                        "project_save does not accept field {field}"
                    )));
                }
                Ok(Self::ProjectSave {})
            }
            "project_close" => {
                if let Some(field) = unknown(&[]) {
                    return Err(AppError::invalid_argument(format!(
                        "project_close does not accept field {field}"
                    )));
                }
                Ok(Self::ProjectClose {})
            }
            "project_snapshot" => {
                if let Some(field) = unknown(&[]) {
                    return Err(AppError::invalid_argument(format!(
                        "project_snapshot does not accept field {field}"
                    )));
                }
                Ok(Self::ProjectSnapshot {})
            }
            "timeline_snapshot" => {
                if let Some(field) = unknown(&[]) {
                    return Err(AppError::invalid_argument(format!(
                        "timeline_snapshot does not accept field {field}"
                    )));
                }
                Ok(Self::TimelineSnapshot {})
            }
            "timeline_selection" => {
                if let Some(field) = unknown(&["selection"]) {
                    return Err(AppError::invalid_argument(format!(
                        "timeline_selection has an unknown field {field}"
                    )));
                }
                let selection =
                    serde_json::from_value(fields.get("selection").cloned().ok_or_else(|| {
                        AppError::invalid_argument("timeline_selection.selection is required")
                    })?)
                    .map_err(|_| {
                        AppError::invalid_argument("timeline_selection.selection is invalid")
                    })?;
                Ok(Self::TimelineSelection { selection })
            }
            "project_history" => {
                if let Some(field) =
                    unknown(&["action", "expectedRevision", "expectedTransactionId"])
                {
                    return Err(AppError::invalid_argument(format!(
                        "project_history has an unknown field {field}"
                    )));
                }
                let action = fields
                    .get("action")
                    .and_then(Value::as_str)
                    .ok_or_else(|| {
                        AppError::invalid_argument("project_history.action is required")
                    })?
                    .to_owned();
                if action != "undo" && action != "redo" {
                    return Err(AppError::invalid_argument(
                        "project_history.action must be undo or redo",
                    ));
                }
                let expected_revision = required_safe_integer(fields, "expectedRevision")?;
                let expected_transaction_id = optional_string(fields, "expectedTransactionId")?;
                Ok(Self::ProjectHistory {
                    action,
                    expected_revision,
                    expected_transaction_id,
                })
            }
            "edit_project" => {
                if let Some(field) = unknown(&[
                    "transactionId",
                    "expectedRevision",
                    "label",
                    "operations",
                    "dryRun",
                ]) {
                    return Err(AppError::invalid_argument(format!(
                        "edit_project has an unknown field {field}"
                    )));
                }
                let transaction_id = required_string(fields, "transactionId")?;
                let expected_revision = required_safe_integer(fields, "expectedRevision")?;
                let label = required_string(fields, "label")?;
                let operations_value = fields.get("operations").ok_or_else(|| {
                    AppError::invalid_argument("edit_project.operations is required")
                })?;
                let operation_values = operations_value.as_array().ok_or_else(|| {
                    AppError::invalid_argument("edit_project.operations must be an array")
                })?;
                for operation in operation_values {
                    validate_edit_operation_wire(operation)?;
                }
                let operations = serde_json::from_value::<Vec<EditOp>>(operations_value.clone())
                    .map_err(|_| {
                        AppError::invalid_argument(
                            "edit_project.operations contains an invalid operation",
                        )
                    })?;
                let dry_run = optional_bool(fields, "dryRun")?;
                Ok(Self::EditProject {
                    transaction_id,
                    expected_revision,
                    label,
                    operations,
                    dry_run,
                })
            }
            "media" | "jobs" | "preview" | "export_video" | "evidence" | "transcript"
            | "analyze_media" | "sample_frames" | "create_graphic" | "assistant" | "providers"
            | "permissions" => parse_category_request(method, params),
            _ => Err(AppError::schema(format!("Unknown editor method: {method}"))),
        }
    }
}

fn parse_category_request(method: &str, params: &Value) -> Result<EditorRequest, AppError> {
    serde_json::from_value(serde_json::json!({
        "method": method,
        "params": params,
    }))
    .map_err(|_| AppError::invalid_argument(format!("{method} params are invalid")))
}

fn required_string(fields: &Map<String, Value>, field: &str) -> Result<String, AppError> {
    fields
        .get(field)
        .and_then(Value::as_str)
        .ok_or_else(|| AppError::invalid_argument(format!("{field} must be a string")))
        .map(ToOwned::to_owned)
}

fn optional_bool(fields: &Map<String, Value>, field: &str) -> Result<Option<bool>, AppError> {
    match fields.get(field) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => value
            .as_bool()
            .map(Some)
            .ok_or_else(|| AppError::invalid_argument(format!("{field} must be a boolean"))),
    }
}

fn validate_edit_operation_wire(value: &Value) -> Result<(), AppError> {
    let fields = value
        .as_object()
        .ok_or_else(|| AppError::invalid_argument("Each edit operation must be an object"))?;
    let operation = fields
        .get("op")
        .and_then(Value::as_str)
        .ok_or_else(|| AppError::invalid_argument("Each edit operation needs an op"))?;
    let allowed: &[&str] = match operation {
        "set_project" => &["op", "name", "aspect"],
        "add_track" => &["op", "id", "kind", "name", "index"],
        "update_track" => &["op", "trackId", "name", "muted", "locked"],
        "remove_track" => &["op", "trackId", "deleteItems"],
        "insert_clip" => &["op", "clip"],
        "move_clip" => &["op", "clipId", "trackId", "startFrame"],
        "trim_clip" => &["op", "clipId", "inFrame", "startFrame", "durationFrames"],
        "split_clip" => &["op", "clipId", "frame", "rightClipId"],
        "update_clip" => &["op", "clipId", "patch"],
        "remove_clips" => &["op", "clipIds"],
        "remove_range" => &["op", "startFrame", "endFrame", "ripple"],
        "add_text" => &["op", "item"],
        "update_text" => &["op", "textId", "patch"],
        "remove_text" => &["op", "textId"],
        "add_transition" => &["op", "leftClipId", "rightClipId", "durationFrames"],
        "remove_transition" => &["op", "transitionId"],
        "replace_captions" => &["op", "clipId", "transcriptId", "style"],
        _ => {
            return Err(AppError::schema(format!(
                "Unknown edit operation: {operation}"
            )))
        }
    };
    if let Some(field) = fields.keys().find(|key| !allowed.contains(&key.as_str())) {
        return Err(AppError::invalid_argument(format!(
            "{operation} has an unknown field {field}"
        )));
    }
    Ok(())
}

fn optional_string(fields: &Map<String, Value>, field: &str) -> Result<Option<String>, AppError> {
    match fields.get(field) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => value
            .as_str()
            .map(|s| Some(s.to_owned()))
            .ok_or_else(|| AppError::invalid_argument(format!("{field} must be a string"))),
    }
}

fn required_safe_integer(fields: &Map<String, Value>, field: &str) -> Result<u64, AppError> {
    fields
        .get(field)
        .and_then(Value::as_u64)
        .ok_or_else(|| AppError::invalid_argument(format!("{field} must be a safe integer")))
        .and_then(|value| {
            validate_safe_integer(value, field)?;
            Ok(value)
        })
}

fn optional_safe_integer(
    fields: &Map<String, Value>,
    field: &str,
) -> Result<Option<u64>, AppError> {
    match fields.get(field) {
        None | Some(Value::Null) => Ok(None),
        Some(_) => required_safe_integer(fields, field).map(Some),
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase")]
pub struct TimelineRange {
    #[ts(type = "SafeInteger")]
    pub start_frame: u64,
    #[ts(type = "SafeInteger")]
    pub end_frame: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase")]
pub struct TimelineSelection {
    pub clip_ids: Vec<String>,
    pub text_ids: Vec<String>,
    #[ts(type = "SafeInteger")]
    pub playhead_frame: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub range: Option<TimelineRange>,
}
impl TimelineSelection {
    pub fn empty() -> Self {
        Self {
            clip_ids: Vec::new(),
            text_ids: Vec::new(),
            playhead_frame: 0,
            range: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase")]
pub struct TimelineSnapshot {
    #[ts(type = "SafeInteger")]
    pub revision: u64,
    #[ts(type = "SafeInteger")]
    pub duration_frames: u64,
    pub selection: TimelineSelection,
    pub document: ProjectDocument,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase")]
pub struct ProjectStatus {
    #[ts(type = "SafeInteger")]
    pub generation: u64,
    pub open: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub project_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub workspace_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional, type = "SafeInteger")]
    pub revision: Option<u64>,
}

impl ProjectStatus {
    pub fn closed(generation: u64) -> Self {
        Self {
            generation,
            open: false,
            project_id: None,
            workspace_id: None,
            name: None,
            revision: None,
        }
    }

    pub fn open(
        generation: u64,
        project_id: String,
        workspace_id: String,
        name: String,
        revision: u64,
    ) -> Self {
        Self {
            generation,
            open: true,
            project_id: Some(project_id),
            workspace_id: Some(workspace_id),
            name: Some(name),
            revision: Some(revision),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase")]
pub struct ProjectSnapshot {
    pub document: ProjectDocument,
    pub workspace_id: String,
}

/// Replies are tagged independently of the outer NDJSON response so the
/// dispatcher can grow without changing the transport envelope.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(tag = "kind", content = "data", rename_all = "snake_case")]
#[ts(tag = "kind", content = "data", rename_all = "snake_case")]
pub enum EditorReply {
    ProjectStatus(ProjectStatus),
    ProjectSnapshot(ProjectSnapshot),
    TimelineSnapshot(TimelineSnapshot),
    ProjectEdit(EditResult),
    ProjectHistory(EditResult),
    Media(MediaReply),
    Jobs(JobsReply),
    Preview(PreviewReply),
    ExportVideo(ExportReply),
    Evidence(EvidenceReply),
    Transcript(EvidenceReply),
    AnalyzeMedia(EvidenceReply),
    SampleFrames(EvidenceReply),
    CreateGraphic(EvidenceReply),
    Assistant(AssistantReply),
    Providers(ProvidersReply),
    Permissions(PermissionsReply),
}

/// The body of a v1 bridge envelope. Requests arriving from the supervised
/// Node process are routed through the same Rust dispatcher as Tauri calls.
#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[ts(tag = "kind", rename_all = "snake_case")]
pub enum BridgeMessage {
    Request {
        method: String,
        #[ts(type = "unknown")]
        params: Value,
    },
    Response {
        ok: bool,
        /// The transport envelope intentionally carries generic JSON so
        /// private sidecar/auth responses do not become public editor types.
        /// `EditorReply` is decoded and guarded only by the editor adapter.
        #[serde(skip_serializing_if = "Option::is_none")]
        #[ts(optional, type = "unknown")]
        data: Option<Value>,
        #[serde(skip_serializing_if = "Option::is_none")]
        #[ts(optional)]
        error: Option<AppError>,
    },
    Event {
        event: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        #[ts(optional, type = "unknown")]
        data: Option<Value>,
    },
}
#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase")]
pub struct BridgeEnvelope {
    #[ts(type = "number")]
    pub v: u8,
    pub id: String,
    pub project_id: Option<String>,
    #[ts(type = "SafeInteger")]
    pub generation: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub run_id: Option<String>,
    #[serde(flatten)]
    #[ts(flatten)]
    pub body: BridgeMessage,
}

impl BridgeEnvelope {
    pub fn request(
        id: String,
        project_id: Option<String>,
        generation: u64,
        run_id: Option<String>,
        request: &EditorRequest,
    ) -> Self {
        Self {
            v: PROTOCOL_VERSION,
            id,
            project_id,
            generation,
            run_id,
            body: BridgeMessage::Request {
                method: request.method().to_owned(),
                params: request.params(),
            },
        }
    }

    pub fn response(request: &Self, result: Result<EditorReply, AppError>) -> Self {
        let body = match result {
            Ok(data) => match serde_json::to_value(data) {
                Ok(data) => BridgeMessage::Response {
                    ok: true,
                    data: Some(data),
                    error: None,
                },
                Err(_) => BridgeMessage::Response {
                    ok: false,
                    data: None,
                    error: Some(AppError::schema("The bridge response could not be encoded")),
                },
            },
            Err(error) => BridgeMessage::Response {
                ok: false,
                data: None,
                error: Some(error),
            },
        };

        Self {
            v: PROTOCOL_VERSION,
            id: request.id.clone(),
            project_id: request.project_id.clone(),
            generation: request.generation,
            run_id: request.run_id.clone(),
            body,
        }
    }

    pub fn event(
        id: String,
        project_id: Option<String>,
        generation: u64,
        run_id: Option<String>,
        event: String,
        data: Option<Value>,
    ) -> Self {
        Self {
            v: PROTOCOL_VERSION,
            id,
            project_id,
            generation,
            run_id,
            body: BridgeMessage::Event { event, data },
        }
    }
}

pub fn validate_safe_integer(value: u64, field: &str) -> Result<(), AppError> {
    if value > MAX_SAFE_INTEGER {
        return Err(AppError::schema(format!(
            "{field} exceeds the safe integer range"
        )));
    }
    Ok(())
}

pub fn validate_envelope(envelope: &BridgeEnvelope) -> Result<(), AppError> {
    if envelope.v != PROTOCOL_VERSION {
        return Err(AppError::schema(format!(
            "Unsupported bridge protocol version: {}",
            envelope.v
        )));
    }
    if envelope.id.is_empty()
        || envelope.id.len() > MAX_BRIDGE_ID_BYTES
        || envelope.id.contains('\r')
        || envelope.id.contains('\n')
    {
        return Err(AppError::invalid_argument("Bridge message id is invalid"));
    }
    if envelope.run_id.as_deref().is_some_and(|run_id| {
        run_id.is_empty()
            || run_id.len() > MAX_BRIDGE_ID_BYTES
            || run_id.contains('\r')
            || run_id.contains('\n')
    }) {
        return Err(AppError::invalid_argument("Bridge run id is invalid"));
    }
    validate_safe_integer(envelope.generation, "generation")?;

    match &envelope.body {
        BridgeMessage::Request { method, params } => {
            if method.is_empty() || method.len() > 128 {
                return Err(AppError::invalid_argument("Bridge method is invalid"));
            }
            if !params.is_object() {
                return Err(AppError::invalid_argument(
                    "Bridge request params must be an object",
                ));
            }
        }
        BridgeMessage::Response { ok, data, error } => {
            if *ok == data.is_none() || (!*ok && error.is_none()) || (*ok && error.is_some()) {
                return Err(AppError::schema(
                    "Bridge response has inconsistent result fields",
                ));
            }
            if let Some(data) = data.as_ref() {
                if let Ok(EditorReply::ProjectStatus(status)) =
                    serde_json::from_value::<EditorReply>(data.clone())
                {
                    validate_safe_integer(status.generation, "generation")?;
                    if let Some(revision) = status.revision {
                        validate_safe_integer(revision, "revision")?;
                    }
                }
            }
        }
        BridgeMessage::Event { event, .. } => {
            if event.is_empty() || event.len() > 128 {
                return Err(AppError::invalid_argument("Bridge event name is invalid"));
            }
        }
    }

    Ok(())
}

#[allow(dead_code)]
fn _keep_error_code_exported(_: ErrorCode) {}
