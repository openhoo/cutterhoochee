//! Revision-pinned MP4 export with explicit IPC, execution, destination, and
//! subtitle stages.
//!
//! The public module keeps the existing editor transport contract while the
//! implementation is split by ownership: `runtime` handles IPC and jobs,
//! `encode` owns render/FFmpeg execution, `output` owns destination identity,
//! temporary files, artifact pins, and commit/rollback, and `subtitle` owns
//! caption projection and rational SRT timing.

mod encode;
mod output;
mod runtime;
mod subtitle;

use crate::media::jobs::{JobRegistry, JobSummary};
use serde::{Deserialize, Serialize};
use ts_rs::TS;

pub(super) const EXPORT_JOB_KIND: &str = "video_export";
pub(super) const MAX_EXPORT_RESOLUTION: u16 = 1080;
pub(super) const AUDIO_CHUNK_SAMPLES: u64 = 240_000;
pub(super) const AUDIO_PCM_BUFFER_BYTES: usize = 64 * 1024;
#[derive(Clone)]
pub struct ExportRuntime {
    jobs: JobRegistry,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[ts(rename_all = "camelCase")]
pub struct ExportTargetIdentity {
    pub exists: bool,
    #[ts(type = "SafeInteger")]
    pub size: u64,
    pub token: String,
}

/// The only output heights accepted by the desktop export surface.
///
/// The value is intentionally an integer in the wire contract (`720 | 1080`)
/// rather than a caller-controlled width or arbitrary scale.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
#[ts(tag = "action", rename_all = "snake_case")]
pub enum ExportAction {
    Start {
        #[ts(type = "SafeInteger")]
        revision: u64,
        #[ts(type = "720 | 1080")]
        resolution: u16,
        srt: bool,
    },
    /// Return the current state and, after completion, the finalized output.
    Status {
        #[serde(rename = "jobId")]
        #[ts(rename = "jobId")]
        job_id: String,
    },
    /// `get` is retained as a transport spelling for clients that use the
    /// generic jobs vocabulary; it has exactly the status semantics above.
    Get {
        #[serde(rename = "jobId")]
        #[ts(rename = "jobId")]
        job_id: String,
    },
    /// Progress is a status read, not a second progress source.
    Progress {
        #[serde(rename = "jobId")]
        #[ts(rename = "jobId")]
        job_id: String,
    },
    Cancel {
        #[serde(rename = "jobId")]
        #[ts(rename = "jobId")]
        job_id: String,
    },
    Play {
        #[serde(rename = "jobId")]
        #[ts(rename = "jobId")]
        job_id: String,
    },
    ShowFile {
        #[serde(rename = "jobId")]
        #[ts(rename = "jobId")]
        job_id: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[ts(rename_all = "camelCase")]
pub struct ExportResult {
    #[ts(type = "SafeInteger")]
    pub revision: u64,
    pub plan_hash: String,
    #[ts(type = "SafeInteger")]
    pub resolution: u16,
    #[ts(type = "SafeInteger")]
    pub width: u32,
    #[ts(type = "SafeInteger")]
    pub height: u32,
    #[ts(type = "SafeInteger")]
    pub fps_num: u32,
    #[ts(type = "SafeInteger")]
    pub fps_den: u32,
    #[ts(type = "SafeInteger")]
    pub duration_frames: u64,
    pub has_audio: bool,
    /// This path is produced by the native save dialog and is only exposed
    /// after FFmpeg and ffprobe have succeeded.
    pub destination: String,
    pub destination_identity: ExportTargetIdentity,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub srt_destination: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub srt_identity: Option<ExportTargetIdentity>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[ts(rename_all = "camelCase")]
pub struct ExportStatus {
    pub job: JobSummary,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub result: Option<ExportResult>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ExportOpenReply {
    pub job: JobSummary,
    pub destination: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub srt_destination: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(tag = "kind", content = "data", rename_all = "snake_case")]
#[ts(tag = "kind", content = "data", rename_all = "snake_case")]
pub enum ExportReply {
    Started(ExportStatus),
    Status(ExportStatus),
    Cancelled(ExportStatus),
    Played(ExportOpenReply),
    FileShown(ExportOpenReply),
}
