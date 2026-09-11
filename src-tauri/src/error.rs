use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;
use ts_rs::TS;

/// Error codes shared by the native editor, the agent bridge, and the UI.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
#[ts(rename = "EditorErrorCode", rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ErrorCode {
    InvalidArgument,
    RevisionConflict,
    IdempotencyConflict,
    StaleSession,
    PermissionDenied,
    AssetUnavailable,
    MediaUnsupported,
    JobCancelled,
    IoError,
    AuthRequired,
    ProviderError,
    Busy,
    SchemaUnsupported,
}

/// Stable, JSON-safe error returned across every application boundary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS, Error)]
#[error("{message}")]
#[serde(rename_all = "camelCase")]
#[ts(rename = "EditorError", rename_all = "camelCase")]
pub struct AppError {
    pub code: ErrorCode,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional, type = "unknown")]
    pub details: Option<Value>,
}

impl AppError {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            details: None,
        }
    }

    pub fn with_details(mut self, details: Value) -> Self {
        self.details = Some(details);
        self
    }

    pub fn invalid_argument(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::InvalidArgument, message)
    }

    pub fn stale_session(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::StaleSession, message)
    }

    pub fn schema(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::SchemaUnsupported, message)
    }

    pub fn io(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::IoError, message)
    }

    pub fn busy(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::Busy, message)
    }
}

impl From<serde_json::Error> for AppError {
    fn from(_: serde_json::Error) -> Self {
        Self::schema("The bridge payload is not valid JSON")
    }
}

impl From<std::io::Error> for AppError {
    fn from(_: std::io::Error) -> Self {
        Self::io("The native operation could not be completed")
    }
}
