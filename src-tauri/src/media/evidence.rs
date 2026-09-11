//! Native local evidence surface: transcripts, SRT, analysis, frame samples,
//! and safe generated graphics.
//!
//! Every payload comes from a validated project/managed artifact. Provider
//! disclosure approval is deliberately not inferred here; the Pi/permissions
//! bridge must gate sending these local results to an external recipient.

use crate::editor::dispatcher::CallerContext;
use crate::editor::operations::{apply_batch, EditOp, Transcript};
use crate::error::{AppError, ErrorCode};
use crate::media::analysis::{AnalysisAction, AnalysisReply, AnalysisRuntime};
use crate::media::artifacts::{ArtifactKind, ArtifactStore};
use crate::media::graphics::{CreateGraphicRequest, GraphicReply, GraphicsRuntime};
use crate::media::jobs::{JobContext, JobPriority, JobSpec};
use crate::media::probe::resolve_packaged_binary;
use crate::media::transcribe::{
    SpeechModelInfo, TranscribeReply, TranscribeRequest, TranscribeRuntime,
};
use crate::permissions::FileGrantPurpose;
use crate::project::model::{FrameRate, TextItem, TextKind, TextStyle};
use crate::project::store::EditResult;
use crate::state::AppState;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::fs::{self, File};
use std::io::Read;
use std::path::Path;
use std::process::Command;
use std::time::Duration;
use ts_rs::TS;
use uuid::Uuid;

const MAX_SRT_BYTES: usize = 5 * 1024 * 1024;
const DEFAULT_SAMPLE_COUNT: u32 = 12;
const MAX_SAMPLE_COUNT: u32 = 32;
const MAX_FRAME_BYTES: usize = 4 * 1024 * 1024;
const MAX_FRAME_EDGE: u32 = 1_280;
const MEDIA_DEMUXERS: &str =
    "mov,matroska,avi,mpegts,mpeg,flv,ogg,mp3,wav,flac,aac,image2,png_pipe,jpeg_pipe,webp_pipe";

/// Flat native evidence actions keep the IPC contract explicit. The global
/// dispatcher may route the four method families directly to the corresponding
/// leaf request types; this aggregate is useful to the agent bridge and tests.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
#[ts(tag = "action", rename_all = "snake_case")]
pub enum EvidenceAction {
    ModelStatus {},
    Transcribe {
        #[serde(rename = "assetId")]
        #[ts(rename = "assetId")]
        asset_id: String,
        #[serde(rename = "startFrame", skip_serializing_if = "Option::is_none")]
        #[ts(rename = "startFrame", optional, type = "SafeInteger")]
        start_frame: Option<u64>,
        #[serde(rename = "endFrame", skip_serializing_if = "Option::is_none")]
        #[ts(rename = "endFrame", optional, type = "SafeInteger")]
        end_frame: Option<u64>,
        #[serde(rename = "modelConsent", default)]
        #[ts(rename = "modelConsent")]
        model_consent: bool,
    },
    Read {
        #[serde(rename = "transcriptId")]
        #[ts(rename = "transcriptId")]
        transcript_id: String,
    },
    Search {
        query: String,
        #[serde(rename = "assetId", skip_serializing_if = "Option::is_none")]
        #[ts(rename = "assetId", optional)]
        asset_id: Option<String>,
    },
    ImportSrt {
        #[serde(rename = "playheadFrame")]
        #[ts(rename = "playheadFrame", type = "SafeInteger")]
        playhead_frame: u64,
        #[serde(skip_serializing_if = "Option::is_none")]
        #[ts(optional)]
        style: Option<TextStyle>,
    },
    ExportSrt {},
    Scenes {
        #[serde(rename = "assetId")]
        #[ts(rename = "assetId")]
        asset_id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        #[ts(optional)]
        threshold: Option<f64>,
    },
    Silence {
        #[serde(rename = "assetId")]
        #[ts(rename = "assetId")]
        asset_id: String,
    },
    SampleFrames {
        #[serde(rename = "assetId")]
        #[ts(rename = "assetId")]
        asset_id: String,
        #[serde(rename = "startFrame")]
        #[ts(rename = "startFrame", type = "SafeInteger")]
        start_frame: u64,
        #[serde(rename = "endFrame")]
        #[ts(rename = "endFrame", type = "SafeInteger")]
        end_frame: u64,
        #[serde(skip_serializing_if = "Option::is_none")]
        #[ts(optional)]
        count: Option<u32>,
    },
    CreateGraphic {
        name: String,
        svg: String,
        width: u32,
        height: u32,
    },
}

/// First-class transcript action for the dispatcher/agent method family.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
#[ts(tag = "action", rename_all = "snake_case")]
pub enum TranscriptAction {
    ModelStatus {},
    Transcribe {
        #[serde(rename = "assetId")]
        #[ts(rename = "assetId")]
        asset_id: String,
        #[serde(rename = "startFrame", skip_serializing_if = "Option::is_none")]
        #[ts(rename = "startFrame", optional, type = "SafeInteger")]
        start_frame: Option<u64>,
        #[serde(rename = "endFrame", skip_serializing_if = "Option::is_none")]
        #[ts(rename = "endFrame", optional, type = "SafeInteger")]
        end_frame: Option<u64>,
        #[serde(rename = "modelConsent", default)]
        #[ts(rename = "modelConsent")]
        model_consent: bool,
    },
    Read {
        #[serde(rename = "transcriptId")]
        #[ts(rename = "transcriptId")]
        transcript_id: String,
    },
    Search {
        query: String,
        #[serde(rename = "assetId", skip_serializing_if = "Option::is_none")]
        #[ts(rename = "assetId", optional)]
        asset_id: Option<String>,
    },
    ImportSrt {
        #[serde(rename = "playheadFrame")]
        #[ts(rename = "playheadFrame", type = "SafeInteger")]
        playhead_frame: u64,
        #[serde(skip_serializing_if = "Option::is_none")]
        #[ts(optional)]
        style: Option<TextStyle>,
    },
    ExportSrt {},
}

pub type AnalyzeMediaAction = AnalysisAction;
pub type AnalyzeMediaReply = AnalysisReply;
pub type SampleFramesAction = SampleFramesRequest;
pub type CreateGraphicAction = CreateGraphicRequest;
pub type CreateGraphicReply = GraphicReply;
pub type TranscriptReply = TranscribeReply;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(tag = "kind", content = "data", rename_all = "snake_case")]
#[ts(tag = "kind", content = "data", rename_all = "snake_case")]
pub enum EvidenceReply {
    Transcript(TranscribeReply),
    TranscriptModelStatus(SpeechModelInfo),
    TranscriptRead(Transcript),
    TranscriptSearch(TranscriptSearchReply),
    SrtImported(SrtImportReply),
    SrtExported(SrtExportReply),
    Analysis(AnalysisReply),
    SampleFrames(SampleFramesReply),
    Graphic(GraphicReply),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[ts(rename_all = "camelCase")]
pub struct TranscriptSearchHit {
    pub transcript_id: String,
    pub asset_id: String,
    #[ts(type = "SafeInteger")]
    pub start_frame: u64,
    #[ts(type = "SafeInteger")]
    pub end_frame: u64,
    pub text: String,
    pub approximate: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[ts(rename_all = "camelCase")]
pub struct TranscriptSearchReply {
    pub query: String,
    pub hits: Vec<TranscriptSearchHit>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[ts(rename_all = "camelCase")]
pub struct SrtCue {
    #[ts(type = "SafeInteger")]
    pub start_time_ms: u64,
    #[ts(type = "SafeInteger")]
    pub end_time_ms: u64,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[ts(rename_all = "camelCase")]
pub struct SrtImportReply {
    pub edit: EditResult,
    pub cue_count: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[ts(rename_all = "camelCase")]
pub struct SrtExportReply {
    pub contents: String,
    pub cue_count: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[ts(rename_all = "camelCase")]
pub struct SampleFramesRequest {
    pub asset_id: String,
    #[ts(type = "SafeInteger")]
    pub start_frame: u64,
    #[ts(type = "SafeInteger")]
    pub end_frame: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub count: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[ts(rename_all = "camelCase")]
pub struct SampleFrame {
    pub asset_id: String,
    #[ts(type = "SafeInteger")]
    pub frame: u64,
    #[ts(type = "SafeInteger")]
    pub source_time_ms: u64,
    pub artifact_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[ts(rename_all = "camelCase")]
pub struct SampleFramesReply {
    pub asset_id: String,
    pub frames: Vec<SampleFrame>,
}
/// Execute bounded local evidence work through the shared job registry.
///
/// The registry owns cancellation and process-group teardown.  Supplying the
/// caller's run ID here is important: retirement can cancel queued work before
/// it starts, and workers that reach a run-fenced publisher are rejected by
/// `AppState` while holding the lifecycle lock.
pub(crate) async fn run_evidence_job<T, F>(
    state: &AppState,
    caller: &CallerContext,
    kind: &str,
    project_id: Option<String>,
    worker: F,
) -> Result<T, AppError>
where
    T: Serialize + DeserializeOwned + Send + 'static,
    F: FnOnce(JobContext) -> Result<T, AppError> + Send + 'static,
{
    state.validate_generation(caller.generation)?;
    let run_id = caller.run_id().map(str::to_owned);
    if let Some(run_id) = run_id.as_deref() {
        state.require_active_run_at(caller.generation, run_id)?;
    }
    let spec = JobSpec::new(kind, JobPriority::Background, caller.generation, project_id)?
        .with_run_id(run_id.clone());
    let registry = state.jobs().registry();
    let state_for_worker = state.clone();
    let generation = caller.generation;
    let job = registry.submit(spec, move |context| {
        context.check_cancelled()?;
        if let Some(run_id) = run_id.as_deref() {
            state_for_worker.require_active_run_at(generation, run_id)?;
        }
        let result = worker(context)?;
        serde_json::to_value(result)
            .map_err(|_| AppError::schema("The evidence job result could not be encoded"))
    })?;
    let job_id = job.job_id;
    let wait_job_id = job_id.clone();
    let wait_registry = registry.clone();
    let result = match tokio::task::spawn_blocking(move || {
        wait_registry.wait_blocking(&wait_job_id, Duration::from_secs(60 * 60))
    })
    .await
    {
        Ok(Ok(result)) => result,
        Ok(Err(error)) => {
            let _ = registry.cancel(&job_id);
            return Err(error);
        }
        Err(_) => {
            let _ = registry.cancel(&job_id);
            return Err(AppError::io("The evidence job waiter stopped unexpectedly"));
        }
    };
    state.validate_generation(generation)?;
    if let Some(run_id) = caller.run_id() {
        state.require_active_run_at(generation, run_id)?;
    }
    serde_json::from_value(result.data)
        .map_err(|_| AppError::schema("The evidence job result could not be decoded"))
}
#[derive(Debug, Clone)]
pub struct EvidenceRuntime {
    transcribe: TranscribeRuntime,
    analysis: AnalysisRuntime,
    graphics: GraphicsRuntime,
}

impl EvidenceRuntime {
    pub fn new() -> Self {
        Self {
            transcribe: TranscribeRuntime::new(),
            analysis: AnalysisRuntime::new(),
            graphics: GraphicsRuntime::new(),
        }
    }

    pub async fn handle(
        &self,
        action: EvidenceAction,
        caller: &CallerContext,
        state: &AppState,
    ) -> Result<EvidenceReply, AppError> {
        state.validate_generation(caller.generation)?;
        match action {
            EvidenceAction::ModelStatus {} => Ok(EvidenceReply::TranscriptModelStatus(
                self.transcribe.model_status(state)?,
            )),
            EvidenceAction::Transcribe {
                asset_id,
                start_frame,
                end_frame,
                model_consent,
            } => Ok(EvidenceReply::Transcript(
                self.transcribe
                    .handle(
                        TranscribeRequest {
                            asset_id,
                            start_frame,
                            end_frame,
                            model_consent,
                        },
                        caller,
                        state,
                    )
                    .await?,
            )),
            EvidenceAction::Read { transcript_id } => Ok(EvidenceReply::TranscriptRead(
                read_transcript(state, &transcript_id)?,
            )),
            EvidenceAction::Search { query, asset_id } => Ok(EvidenceReply::TranscriptSearch(
                search_transcripts(state, &query, asset_id.as_deref())?,
            )),
            EvidenceAction::ImportSrt {
                playhead_frame,
                style,
            } => Ok(EvidenceReply::SrtImported(import_srt(
                caller,
                state,
                playhead_frame,
                style.unwrap_or(TextStyle::Clean),
            )?)),
            EvidenceAction::ExportSrt {} => Ok(EvidenceReply::SrtExported(export_srt(state)?)),
            EvidenceAction::Scenes {
                asset_id,
                threshold,
            } => Ok(EvidenceReply::Analysis(
                self.analysis
                    .handle(
                        AnalysisAction::Scenes {
                            asset_id,
                            threshold,
                        },
                        caller,
                        state,
                    )
                    .await?,
            )),
            EvidenceAction::Silence { asset_id } => Ok(EvidenceReply::Analysis(
                self.analysis
                    .handle(AnalysisAction::Silence { asset_id }, caller, state)
                    .await?,
            )),
            EvidenceAction::SampleFrames {
                asset_id,
                start_frame,
                end_frame,
                count,
            } => Ok(EvidenceReply::SampleFrames(
                sample_frames(
                    caller,
                    state,
                    SampleFramesRequest {
                        asset_id,
                        start_frame,
                        end_frame,
                        count,
                    },
                )
                .await?,
            )),
            EvidenceAction::CreateGraphic {
                name,
                svg,
                width,
                height,
            } => Ok(EvidenceReply::Graphic(
                self.graphics
                    .handle(
                        CreateGraphicRequest {
                            name,
                            svg,
                            width,
                            height,
                        },
                        caller,
                        state,
                    )
                    .await?,
            )),
        }
    }

    pub async fn transcript(
        &self,
        action: TranscriptAction,
        caller: &CallerContext,
        state: &AppState,
    ) -> Result<EvidenceReply, AppError> {
        state.validate_generation(caller.generation)?;
        match action {
            TranscriptAction::ModelStatus {} => Ok(EvidenceReply::TranscriptModelStatus(
                self.transcribe.model_status(state)?,
            )),
            TranscriptAction::Transcribe {
                asset_id,
                start_frame,
                end_frame,
                model_consent,
            } => Ok(EvidenceReply::Transcript(
                self.transcribe
                    .handle(
                        TranscribeRequest {
                            asset_id,
                            start_frame,
                            end_frame,
                            model_consent,
                        },
                        caller,
                        state,
                    )
                    .await?,
            )),
            TranscriptAction::Read { transcript_id } => Ok(EvidenceReply::TranscriptRead(
                read_transcript(state, &transcript_id)?,
            )),
            TranscriptAction::Search { query, asset_id } => Ok(EvidenceReply::TranscriptSearch(
                search_transcripts(state, &query, asset_id.as_deref())?,
            )),
            TranscriptAction::ImportSrt {
                playhead_frame,
                style,
            } => Ok(EvidenceReply::SrtImported(import_srt(
                caller,
                state,
                playhead_frame,
                style.unwrap_or(TextStyle::Clean),
            )?)),
            TranscriptAction::ExportSrt {} => Ok(EvidenceReply::SrtExported(export_srt(state)?)),
        }
    }

    pub async fn analyze_media(
        &self,
        action: AnalysisAction,
        caller: &CallerContext,
        state: &AppState,
    ) -> Result<EvidenceReply, AppError> {
        state.validate_generation(caller.generation)?;
        Ok(EvidenceReply::Analysis(
            self.analysis.handle(action, caller, state).await?,
        ))
    }

    pub async fn sample_frames(
        &self,
        request: SampleFramesRequest,
        caller: &CallerContext,
        state: &AppState,
    ) -> Result<EvidenceReply, AppError> {
        state.validate_generation(caller.generation)?;
        Ok(EvidenceReply::SampleFrames(
            sample_frames(caller, state, request).await?,
        ))
    }

    pub async fn create_graphic(
        &self,
        request: CreateGraphicRequest,
        caller: &CallerContext,
        state: &AppState,
    ) -> Result<EvidenceReply, AppError> {
        state.validate_generation(caller.generation)?;
        Ok(EvidenceReply::Graphic(
            self.graphics.handle(request, caller, state).await?,
        ))
    }
}

impl Default for EvidenceRuntime {
    fn default() -> Self {
        Self::new()
    }
}

pub fn parse_srt_bytes(bytes: &[u8]) -> Result<Vec<SrtCue>, AppError> {
    if bytes.len() > MAX_SRT_BYTES {
        return Err(AppError::invalid_argument(
            "SRT input exceeds the 5 MiB limit",
        ));
    }
    let bytes = bytes.strip_prefix(&[0xef, 0xbb, 0xbf]).unwrap_or(bytes);
    let text = std::str::from_utf8(bytes)
        .map_err(|_| AppError::invalid_argument("SRT input must be UTF-8"))?
        .replace("\r\n", "\n")
        .replace('\r', "\n");
    let mut cues = Vec::new();
    let mut block = Vec::new();
    let mut previous_start = None;
    let mut previous_end = 0u64;
    for line in text.split('\n') {
        if line.trim().is_empty() {
            if !block.is_empty() {
                let cue = parse_srt_block(&block)?;
                validate_srt_order(&cue, &mut previous_start, &mut previous_end)?;
                cues.push(cue);
                block.clear();
            }
        } else {
            block.push(line.to_owned());
        }
    }
    if !block.is_empty() {
        let cue = parse_srt_block(&block)?;
        validate_srt_order(&cue, &mut previous_start, &mut previous_end)?;
        cues.push(cue);
    }
    if cues.is_empty() {
        return Err(AppError::invalid_argument("SRT input contains no cues"));
    }
    Ok(cues)
}

fn parse_srt_block(lines: &[String]) -> Result<SrtCue, AppError> {
    let (time_index, timing) = lines
        .iter()
        .enumerate()
        .find_map(|(index, line)| line.contains("-->").then_some((index, line.as_str())))
        .ok_or_else(|| AppError::invalid_argument("SRT cue has no timing line"))?;
    if time_index > 1 {
        return Err(AppError::invalid_argument("SRT cue numbering is malformed"));
    }
    if time_index == 1 && lines[0].contains("-->") {
        return Err(AppError::invalid_argument("SRT cue numbering is malformed"));
    }
    let (start, end) = timing
        .split_once("-->")
        .ok_or_else(|| AppError::invalid_argument("SRT cue timing is malformed"))?;
    let start = parse_srt_timestamp(start.trim())?;
    let end = parse_srt_timestamp(end.trim())?;
    let mut text_lines = lines[time_index + 1..].to_vec();
    if text_lines.is_empty() {
        return Err(AppError::invalid_argument("SRT cue text must not be empty"));
    }
    let text = text_lines
        .iter_mut()
        .map(|line| strip_srt_formatting(line))
        .collect::<Vec<_>>()
        .join("\n");
    if text.is_empty() {
        return Err(AppError::invalid_argument("SRT cue text must not be empty"));
    }
    Ok(SrtCue {
        start_time_ms: start,
        end_time_ms: end,
        text,
    })
}

fn validate_srt_order(
    cue: &SrtCue,
    previous_start: &mut Option<u64>,
    previous_end: &mut u64,
) -> Result<(), AppError> {
    if cue.start_time_ms >= cue.end_time_ms {
        return Err(AppError::invalid_argument(
            "SRT cue intervals must be positive",
        ));
    }
    if previous_start.is_some_and(|start| cue.start_time_ms <= start)
        || cue.start_time_ms < *previous_end
    {
        return Err(AppError::invalid_argument(
            "SRT cue timings must increase without overlap",
        ));
    }
    *previous_start = Some(cue.start_time_ms);
    *previous_end = cue.end_time_ms;
    Ok(())
}

fn parse_srt_timestamp(value: &str) -> Result<u64, AppError> {
    let (clock, millis) = value
        .rsplit_once(',')
        .ok_or_else(|| AppError::invalid_argument("SRT timestamp must use HH:MM:SS,mmm"))?;
    if millis.len() != 3 || !millis.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(AppError::invalid_argument(
            "SRT milliseconds must have three digits",
        ));
    }
    let mut fields = clock.split(':');
    let hours = parse_clock_field(fields.next())?;
    let minutes = parse_clock_field(fields.next())?;
    let seconds = parse_clock_field(fields.next())?;
    if fields.next().is_some() || minutes >= 60 || seconds >= 60 {
        return Err(AppError::invalid_argument(
            "SRT timestamp clock is malformed",
        ));
    }
    let millis = millis
        .parse::<u64>()
        .map_err(|_| AppError::invalid_argument("SRT timestamp is malformed"))?;
    hours
        .checked_mul(3_600_000)
        .and_then(|value| value.checked_add(minutes * 60_000))
        .and_then(|value| value.checked_add(seconds * 1_000))
        .and_then(|value| value.checked_add(millis))
        .ok_or_else(|| AppError::invalid_argument("SRT timestamp is too large"))
}

fn parse_clock_field(value: Option<&str>) -> Result<u64, AppError> {
    let value =
        value.ok_or_else(|| AppError::invalid_argument("SRT timestamp clock is malformed"))?;
    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(AppError::invalid_argument(
            "SRT timestamp clock is malformed",
        ));
    }
    value
        .parse::<u64>()
        .map_err(|_| AppError::invalid_argument("SRT timestamp clock is too large"))
}

fn strip_srt_formatting(value: &str) -> String {
    let mut result = String::with_capacity(value.len());
    let mut remainder = value;
    while let Some(start) = remainder.find('<') {
        result.push_str(&remainder[..start]);
        let after = &remainder[start..];
        let Some(end) = after.find('>') else {
            result.push_str(after);
            return result;
        };
        let tag = &after[..=end];
        let lower = tag.to_ascii_lowercase();
        if matches!(
            lower.as_str(),
            "<b>" | "</b>" | "<i>" | "</i>" | "<u>" | "</u>"
        ) {
            remainder = &after[end + 1..];
        } else {
            result.push_str(tag);
            remainder = &after[end + 1..];
        }
    }
    result.push_str(remainder);
    result
}

fn import_srt(
    caller: &CallerContext,
    state: &AppState,
    playhead_frame: u64,
    style: TextStyle,
) -> Result<SrtImportReply, AppError> {
    let scope = state.permissions().scope_for_state(state)?;
    let grants = state
        .permissions()
        .grant_files(caller, scope, None, FileGrantPurpose::Import)?;
    if grants.len() > 1 {
        return Err(AppError::invalid_argument(
            "Select exactly one subtitle file for SRT import",
        ));
    }
    let grant = grants
        .into_iter()
        .next()
        .ok_or_else(|| AppError::io("The selected subtitle grant was not created"))?;
    let bytes = read_granted_file(Path::new(&grant.path))?;
    let cues = parse_srt_bytes(&bytes)?;
    let snapshot = state.snapshot_at(caller.generation)?;
    let text_track_id = snapshot
        .document
        .tracks
        .iter()
        .find(|track| track.kind == crate::project::model::TrackKind::Text)
        .map(|track| track.id.clone())
        .ok_or_else(|| AppError::invalid_argument("A text track is required for SRT captions"))?;
    let fps = snapshot.document.profile.fps();
    let mut operations = Vec::with_capacity(cues.len());
    for cue in &cues {
        let start_offset = millis_to_frame_floor(cue.start_time_ms, fps)?;
        let end_offset = millis_to_frame_ceil(cue.end_time_ms, fps)?;
        let start_frame = playhead_frame
            .checked_add(start_offset)
            .ok_or_else(|| AppError::invalid_argument("SRT playhead position overflows"))?;
        let end_frame = playhead_frame
            .checked_add(end_offset)
            .ok_or_else(|| AppError::invalid_argument("SRT cue position overflows"))?;
        if end_frame <= start_frame {
            return Err(AppError::invalid_argument(
                "SRT cue is shorter than one project frame",
            ));
        }
        operations.push(EditOp::AddText {
            item: TextItem {
                id: Uuid::new_v4().to_string(),
                track_id: text_track_id.clone(),
                kind: TextKind::Caption,
                text: cue.text.clone(),
                style,
                color: crate::project::model::RgbaColor {
                    red: 255,
                    green: 255,
                    blue: 255,
                    alpha: 255,
                },
                font_size: 48,
                position_x: 5_000,
                position_y: 8_500,
                line_breaks: Vec::new(),
                start_frame: Some(start_frame),
                duration_frames: Some(end_frame - start_frame),
                owner_clip_id: None,
                source_start_frame: None,
                source_duration_frames: None,
            },
        });
    }
    let transaction_id = Uuid::new_v4().to_string();
    let label = "Import captions from SRT".to_owned();
    let payload_hash = hash_operations(&label, &operations);
    let edit = state.commit_at_with_run(
        caller.generation,
        caller.run_id(),
        transaction_id,
        snapshot.document.revision,
        label,
        payload_hash,
        move |document| apply_batch(document, &operations, &[]),
    )?;
    Ok(SrtImportReply {
        edit,
        cue_count: u32::try_from(cues.len()).unwrap_or(u32::MAX),
    })
}

fn export_srt(state: &AppState) -> Result<SrtExportReply, AppError> {
    let snapshot = state.snapshot()?;
    let mut cues = Vec::<(u64, u64, String, String)>::new();
    for item in snapshot
        .document
        .text_items
        .iter()
        .filter(|item| item.kind == TextKind::Caption)
    {
        if let Some(owner_clip_id) = item.owner_clip_id.as_deref() {
            let clip = snapshot
                .document
                .clips
                .iter()
                .find(|clip| clip.id == owner_clip_id)
                .ok_or_else(|| AppError::schema("Caption owner clip is unavailable"))?;
            if let Some(projection) = item.project_on_clip(clip)? {
                cues.push((
                    projection.timeline_start_frame,
                    projection
                        .timeline_start_frame
                        .checked_add(projection.duration_frames)
                        .ok_or_else(|| AppError::schema("Caption interval overflows"))?,
                    item.id.clone(),
                    item.text.clone(),
                ));
            }
        } else if let Some(interval) = item.timeline_interval() {
            let interval = interval?;
            cues.push((
                interval.start_frame,
                interval.end_frame(),
                item.id.clone(),
                item.text.clone(),
            ));
        }
    }
    cues.sort_by(|left, right| (left.0, left.2.as_str()).cmp(&(right.0, right.2.as_str())));
    let fps = snapshot.document.profile.fps();
    let mut contents = String::new();
    for (index, (start_frame, end_frame, _, text)) in cues.iter().enumerate() {
        let start_ms = frame_to_ms_floor(*start_frame, fps);
        let end_ms = frame_to_ms_ceil(*end_frame, fps);
        if end_ms <= start_ms {
            return Err(AppError::schema(
                "Caption interval is shorter than one millisecond",
            ));
        }
        contents.push_str(&(index + 1).to_string());
        contents.push('\n');
        contents.push_str(&format_srt_timestamp(start_ms));
        contents.push_str(" --> ");
        contents.push_str(&format_srt_timestamp(end_ms));
        contents.push('\n');
        contents.push_str(&text.replace('\r', ""));
        contents.push_str("\n\n");
    }
    Ok(SrtExportReply {
        contents,
        cue_count: u32::try_from(cues.len()).unwrap_or(u32::MAX),
    })
}

fn read_transcript(state: &AppState, transcript_id: &str) -> Result<Transcript, AppError> {
    let store = state.current_store()?;
    let mut values = store.load_transcripts(&[transcript_id.to_owned()])?;
    values.pop().ok_or_else(|| {
        AppError::new(
            ErrorCode::AssetUnavailable,
            "The requested transcript is unavailable",
        )
    })
}

fn search_transcripts(
    state: &AppState,
    query: &str,
    asset_id: Option<&str>,
) -> Result<TranscriptSearchReply, AppError> {
    let query = query.trim();
    if query.is_empty() || query.len() > 1_024 || query.contains('\0') {
        return Err(AppError::invalid_argument(
            "Transcript search text is invalid",
        ));
    }
    let store = state.current_store()?;
    let directory = store.root().join("transcripts");
    let mut ids = Vec::new();
    let entries = fs::read_dir(&directory).map_err(|_| {
        AppError::new(
            ErrorCode::AssetUnavailable,
            "The project transcript directory is unavailable",
        )
    })?;
    for entry in entries {
        let entry =
            entry.map_err(|_| AppError::io("The transcript directory could not be read"))?;
        let path = entry.path();
        let metadata = fs::symlink_metadata(&path)
            .map_err(|_| AppError::io("The transcript entry could not be inspected"))?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            continue;
        }
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        let Some(id) = name.strip_suffix(".json") else {
            continue;
        };
        if Uuid::parse_str(id).is_ok() {
            ids.push(id.to_owned());
        }
    }
    let transcripts = store.load_transcripts(&ids)?;
    let needle = query.to_lowercase();
    let mut hits = Vec::new();
    for transcript in transcripts {
        if asset_id.is_some_and(|expected| expected != transcript.asset_id) {
            continue;
        }
        for segment in transcript.segments {
            if segment.text.to_lowercase().contains(&needle) {
                hits.push(TranscriptSearchHit {
                    transcript_id: transcript.transcript_id.clone(),
                    asset_id: transcript.asset_id.clone(),
                    start_frame: segment.start_frame,
                    end_frame: segment.end_frame,
                    text: segment.text,
                    approximate: segment.approximate,
                });
            }
        }
    }
    hits.sort_by_key(|hit| {
        (
            hit.asset_id.clone(),
            hit.start_frame,
            hit.transcript_id.clone(),
        )
    });
    Ok(TranscriptSearchReply {
        query: query.to_owned(),
        hits,
    })
}

async fn sample_frames(
    caller: &CallerContext,
    state: &AppState,
    request: SampleFramesRequest,
) -> Result<SampleFramesReply, AppError> {
    state.validate_generation(caller.generation)?;
    let store = state.current_store()?;
    let snapshot = store.snapshot()?;
    let asset = snapshot
        .document
        .assets
        .iter()
        .find(|asset| asset.id == request.asset_id)
        .ok_or_else(|| {
            AppError::new(
                ErrorCode::AssetUnavailable,
                "The requested media asset is unavailable",
            )
        })?;
    let normalization = asset.normalization.as_ref().ok_or_else(|| {
        AppError::new(
            ErrorCode::AssetUnavailable,
            "The media asset has no normalized master yet",
        )
    })?;
    let video = normalization.video.as_ref().ok_or_else(|| {
        AppError::new(
            ErrorCode::MediaUnsupported,
            "Frame samples require a video or still-image asset",
        )
    })?;
    let frame_count = video.frame_count;
    if request.start_frame >= request.end_frame || request.end_frame > frame_count {
        return Err(AppError::invalid_argument(
            "The requested frame range is outside the normalized asset",
        ));
    }
    let count = request.count.unwrap_or(DEFAULT_SAMPLE_COUNT);
    if count == 0 || count > MAX_SAMPLE_COUNT {
        return Err(AppError::invalid_argument(
            "Frame sample count must be between 1 and 32",
        ));
    }
    let samples =
        count.min(u32::try_from(request.end_frame - request.start_frame).unwrap_or(u32::MAX));
    let artifacts = ArtifactStore::for_project(store.root(), store.workspace_id())?;
    let master = artifacts.managed_path(&video.master_artifact_id)?;
    let fps = snapshot.document.profile.fps();
    let (width, height) = scaled_dimensions(video.width, video.height);
    let asset_id = asset.id.clone();
    let master_artifact_id = video.master_artifact_id.clone();
    let state_for_job = state.clone();
    let master_for_job = master.clone();
    let artifacts_for_job = artifacts.clone();
    let generation = caller.generation;
    let run_id = caller.run_id().map(str::to_owned);
    run_evidence_job(
        state,
        caller,
        "sample_frames",
        Some(snapshot.project_id.clone()),
        move |context| {
            let mut frames = Vec::with_capacity(samples as usize);
            for index in 0..samples {
                context.check_cancelled()?;
                let frame = request.start_frame
                    + (u64::from(index) * (request.end_frame - request.start_frame))
                        / u64::from(samples);
                let bytes = decode_frame(
                    &context,
                    &state_for_job,
                    &master_for_job,
                    frame,
                    fps,
                    width,
                    height,
                )?;
                if bytes.len() > MAX_FRAME_BYTES {
                    return Err(AppError::new(
                        ErrorCode::MediaUnsupported,
                        "A sampled frame exceeds the 4 MiB encoded image limit",
                    ));
                }
                context.check_cancelled()?;
                state_for_job.validate_generation(generation)?;
                if let Some(run_id) = run_id.as_deref() {
                    state_for_job.require_active_run_at(generation, run_id)?;
                }
                let cache_key = format!(
                    "sample-frame:{}:{}:{}:{}x{}",
                    asset_id, master_artifact_id, frame, width, height
                );
                let artifact =
                    artifacts_for_job.put_bytes(&cache_key, "jpg", ArtifactKind::Frame, &bytes)?;
                frames.push(SampleFrame {
                    asset_id: asset_id.clone(),
                    frame,
                    source_time_ms: frame_to_ms_floor(frame, fps),
                    artifact_id: artifact.artifact_id,
                });
                context.progress(0.05 + 0.9 * f64::from(index + 1) / f64::from(samples))?;
            }
            context.check_cancelled()?;
            state_for_job.validate_generation(generation)?;
            if let Some(run_id) = run_id.as_deref() {
                state_for_job.require_active_run_at(generation, run_id)?;
            }
            Ok(SampleFramesReply { asset_id, frames })
        },
    )
    .await
}

fn decode_frame(
    context: &JobContext,
    state: &AppState,
    master: &Path,
    frame: u64,
    fps: FrameRate,
    width: u32,
    height: u32,
) -> Result<Vec<u8>, AppError> {
    let ffmpeg = resolve_packaged_binary(&state.paths().resource_dir, "ffmpeg")?;
    let timestamp = frame_to_timestamp(frame, fps);
    let mut command = Command::new(&ffmpeg);
    command
        .arg("-hide_banner")
        .arg("-nostdin")
        .arg("-protocol_whitelist")
        .arg("file,pipe")
        .arg("-format_whitelist")
        .arg(MEDIA_DEMUXERS)
        .arg("-ss")
        .arg(&timestamp)
        .arg("-i")
        .arg(master)
        .arg("-map")
        .arg("0:v:0")
        .arg("-frames:v")
        .arg("1")
        .arg("-vf")
        .arg(format!("scale={width}:{height}:flags=lanczos"))
        .args([
            "-q:v",
            "2",
            "-f",
            "image2pipe",
            "-vcodec",
            "mjpeg",
            "pipe:1",
        ]);
    let output = match context.run_command(command) {
        Ok(output) => output,
        Err(error) => {
            return Err(if error.code == ErrorCode::JobCancelled {
                error
            } else {
                AppError::new(
                    ErrorCode::MediaUnsupported,
                    "The frame sampler could not start FFmpeg",
                )
            })
        }
    };
    context.check_cancelled()?;
    if output.stdout.is_empty() {
        return Err(AppError::new(
            ErrorCode::MediaUnsupported,
            "The normalized frame could not be decoded",
        ));
    }
    Ok(output.stdout)
}

fn read_granted_file(path: &Path) -> Result<Vec<u8>, AppError> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|_| AppError::io("The selected subtitle file could not be inspected"))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(AppError::new(
            ErrorCode::PermissionDenied,
            "The selected subtitle is not a regular file",
        ));
    }
    if metadata.len() > MAX_SRT_BYTES as u64 {
        return Err(AppError::invalid_argument(
            "SRT input exceeds the 5 MiB limit",
        ));
    }
    let mut file = File::open(path).map_err(|_| {
        AppError::new(
            ErrorCode::PermissionDenied,
            "The selected subtitle could not be opened",
        )
    })?;
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.read_to_end(&mut bytes)
        .map_err(|_| AppError::io("The selected subtitle could not be read"))?;
    Ok(bytes)
}

fn hash_operations(label: &str, operations: &[EditOp]) -> String {
    let bytes = serde_json::to_vec(&(label, operations)).unwrap_or_default();
    let mut hash = 0xcbf29ce484222325u64;
    for byte in bytes {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("{hash:016x}")
}

fn frame_to_ms_floor(frame: u64, fps: FrameRate) -> u64 {
    (u128::from(frame) * 1_000 * u128::from(fps.den) / u128::from(fps.num)) as u64
}

fn frame_to_ms_ceil(frame: u64, fps: FrameRate) -> u64 {
    let denominator = u128::from(fps.num);
    let numerator = u128::from(frame) * 1_000 * u128::from(fps.den);
    ((numerator + denominator - 1) / denominator) as u64
}

fn millis_to_frame_floor(milliseconds: u64, fps: FrameRate) -> Result<u64, AppError> {
    let value = u128::from(milliseconds) * u128::from(fps.num) / (1_000 * u128::from(fps.den));
    u64::try_from(value).map_err(|_| AppError::invalid_argument("SRT frame position is too large"))
}

fn millis_to_frame_ceil(milliseconds: u64, fps: FrameRate) -> Result<u64, AppError> {
    let denominator = 1_000 * u128::from(fps.den);
    let numerator = u128::from(milliseconds) * u128::from(fps.num);
    let value = (numerator + denominator - 1) / denominator;
    u64::try_from(value).map_err(|_| AppError::invalid_argument("SRT frame position is too large"))
}

fn format_srt_timestamp(milliseconds: u64) -> String {
    let hours = milliseconds / 3_600_000;
    let minutes = (milliseconds / 60_000) % 60;
    let seconds = (milliseconds / 1_000) % 60;
    let millis = milliseconds % 1_000;
    format!("{hours:02}:{minutes:02}:{seconds:02},{millis:03}")
}

fn frame_to_timestamp(frame: u64, fps: FrameRate) -> String {
    let numerator = u128::from(frame) * u128::from(fps.den);
    let seconds = numerator / u128::from(fps.num);
    let remainder = numerator % u128::from(fps.num);
    let micros = (remainder * 1_000_000) / u128::from(fps.num);
    format!("{seconds}.{micros:06}")
}

fn scaled_dimensions(width: u32, height: u32) -> (u32, u32) {
    let long_edge = width.max(height);
    if long_edge <= MAX_FRAME_EDGE {
        return (width, height);
    }
    if width >= height {
        let height =
            ((u64::from(height) * u64::from(MAX_FRAME_EDGE)) / u64::from(width)).max(1) as u32;
        (MAX_FRAME_EDGE, height)
    } else {
        let width =
            ((u64::from(width) * u64::from(MAX_FRAME_EDGE)) / u64::from(height)).max(1) as u32;
        (width, MAX_FRAME_EDGE)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn srt_accepts_bom_unnumbered_cues_and_removes_only_simple_tags() {
        let input = b"\xEF\xBB\xBF00:00:00,000 --> 00:00:01,000\n<b>Hello</b> <font color=\"red\">world</font>\n\n2\n00:00:01,100 --> 00:00:02,000\nline one\nline two\n";
        let cues = parse_srt_bytes(input).expect("SRT parse");
        assert_eq!(cues.len(), 2);
        assert_eq!(cues[0].text, "Hello <font color=\"red\">world</font>");
        assert_eq!(cues[1].text, "line one\nline two");
    }

    #[test]
    fn srt_rejects_malformed_or_nonincreasing_cues_atomically() {
        let malformed = b"1\n00:00:02,000 --> 00:00:01,000\nbad\n";
        assert!(parse_srt_bytes(malformed).is_err());
        let overlapping = b"00:00:00,000 --> 00:00:01,000\na\n\n00:00:00,900 --> 00:00:02,000\nb\n";
        assert!(parse_srt_bytes(overlapping).is_err());
    }

    #[test]
    fn srt_caps_bytes_before_utf8_or_cue_processing() {
        let input = vec![b'a'; MAX_SRT_BYTES + 1];
        let error = parse_srt_bytes(&input).expect_err("oversized SRT");
        assert_eq!(error.code, ErrorCode::InvalidArgument);
    }

    #[test]
    fn simple_unknown_markup_remains_literal() {
        assert_eq!(
            strip_srt_formatting("<font>x</font> <i>y</i>"),
            "<font>x</font> y"
        );
    }

    #[test]
    fn sample_scaling_never_exceeds_long_edge() {
        assert_eq!(scaled_dimensions(1920, 1080), (1280, 720));
        assert_eq!(scaled_dimensions(720, 1280), (720, 1280));
    }
}
