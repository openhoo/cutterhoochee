use crate::editor::dispatcher::{CallerContext, CallerKind};
use crate::editor::history::{EntityDelta, EntityKind, EntityState, TransactionDelta};
use crate::editor::operations::EditOp;
use crate::error::{AppError, ErrorCode};
use crate::ipc::{EditorReply, EditorRequest};
use crate::media::jobs::{JobState, JobSummary};
use crate::media::render::PreviewAction;
use crate::media::{EvidenceAction, JobsAction, MediaAction};
use crate::project::model::{MediaClip, ProjectDocument, TextItem};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex};
use ts_rs::TS;
use uuid::Uuid;

pub const MAX_ACTIVITY_REGISTRY: usize = 128;
const MAX_ACTIVITY_CHANGES: usize = 32;
const MAX_ACTIVITY_TARGETS: usize = 32;
const MAX_ACTIVITY_STRING: usize = 512;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "lowercase")]
#[ts(rename_all = "lowercase")]
pub enum ActivityOrigin {
    Agent,
    User,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
#[ts(rename_all = "snake_case")]
pub enum ActivityPhase {
    Running,
    AwaitingApproval,
    Queued,
    Cancelling,
    Completed,
    Cancelled,
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, TS)]
#[serde(rename_all = "lowercase")]
#[ts(rename_all = "lowercase")]
pub enum ActivityTargetKind {
    Project,
    Asset,
    Track,
    Clip,
    Text,
    Transition,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "lowercase")]
#[ts(rename_all = "lowercase")]
pub enum ActivityTargetSpace {
    Timeline,
    Source,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase")]
pub struct ActivityTarget {
    pub kind: ActivityTargetKind,
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub track_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub space: Option<ActivityTargetSpace>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional, type = "SafeInteger")]
    pub start_frame: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional, type = "SafeInteger")]
    pub end_frame: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase")]
pub struct ActivityGeometry {
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub track_id: Option<String>,
    #[ts(type = "SafeInteger")]
    pub start_frame: u64,
    #[ts(type = "SafeInteger")]
    pub duration_frames: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase")]
pub struct ActivityFieldChange {
    pub field: String,
    pub before: String,
    pub after: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase")]
pub struct ActivityChange {
    pub target: ActivityTarget,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub before: Option<ActivityGeometry>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub after: Option<ActivityGeometry>,
    pub fields: Vec<ActivityFieldChange>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase")]
pub struct AgentActivity {
    pub id: String,
    #[ts(type = "SafeInteger")]
    pub sequence: u64,
    pub origin: ActivityOrigin,
    #[ts(type = "string | null")]
    pub project_id: Option<String>,
    #[ts(type = "SafeInteger")]
    pub generation: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub run_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub tool_call_id: Option<String>,
    pub tool: String,
    pub action: String,
    pub label: String,
    pub phase: ActivityPhase,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional, type = "SafeInteger")]
    pub revision: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub transaction_id: Option<String>,
    pub changed: bool,
    pub dry_run: bool,
    pub targets: Vec<ActivityTarget>,
    #[ts(type = "SafeInteger")]
    pub total_targets: u64,
    pub changes: Vec<ActivityChange>,
    pub job_ids: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional, type = "number")]
    pub progress: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub error: Option<AppError>,
}

#[derive(Debug, Clone)]
pub(crate) struct ActivityHandle {
    pub(crate) id: String,
    pub(crate) generation: u64,
    pub(crate) project_id: Option<String>,
}

#[derive(Debug, Clone)]
pub(crate) struct ActivityStart {
    pub(crate) generation: u64,
    pub(crate) project_id: Option<String>,
    pub(crate) origin: ActivityOrigin,
    pub(crate) run_id: Option<String>,
    pub(crate) tool_call_id: Option<String>,
    pub(crate) tool: String,
    pub(crate) action: String,
    pub(crate) label: String,
    pub(crate) dry_run: bool,
    pub(crate) targets: Vec<ActivityTarget>,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct ActivityPatch {
    pub(crate) phase: Option<ActivityPhase>,
    pub(crate) revision: Option<u64>,
    pub(crate) transaction_id: Option<String>,
    pub(crate) changed: Option<bool>,
    pub(crate) targets: Option<Vec<ActivityTarget>>,
    pub(crate) total_targets: Option<u64>,
    pub(crate) changes: Option<Vec<ActivityChange>>,
    pub(crate) job_ids: Option<Vec<String>>,
    pub(crate) progress: Option<Option<f64>>,
    pub(crate) message: Option<Option<String>>,
    pub(crate) error: Option<Option<AppError>>,
}

#[derive(Default)]
struct RegistryState {
    next_sequence: u64,
    activities: VecDeque<AgentActivity>,
    approvals: HashMap<String, Vec<String>>,
}

#[derive(Clone, Default)]
pub struct ActivityRegistry {
    inner: Arc<Mutex<RegistryState>>,
}

impl ActivityRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub(crate) fn start(
        &self,
        start: ActivityStart,
    ) -> Result<(ActivityHandle, AgentActivity), AppError> {
        validate_activity_string(&start.tool, "activity.tool", 128)?;
        validate_activity_string(&start.action, "activity.action", 128)?;
        validate_activity_string(&start.label, "activity.label", MAX_ACTIVITY_STRING)?;
        let mut state = self
            .inner
            .lock()
            .map_err(|_| AppError::io("The activity registry lock is unavailable"))?;
        let sequence = next_sequence(&mut state)?;
        let activity = AgentActivity {
            id: format!("activity-{}", Uuid::new_v4()),
            sequence,
            origin: start.origin,
            project_id: start.project_id.clone(),
            generation: start.generation,
            run_id: start.run_id,
            tool_call_id: start.tool_call_id,
            tool: start.tool,
            action: start.action,
            label: start.label,
            phase: ActivityPhase::Running,
            revision: None,
            transaction_id: None,
            changed: false,
            dry_run: start.dry_run,
            targets: bounded_targets(start.targets),
            total_targets: 0,
            changes: Vec::new(),
            job_ids: Vec::new(),
            progress: None,
            message: None,
            error: None,
        };
        let mut activity = activity;
        activity.total_targets = activity.targets.len() as u64;
        state.activities.push_back(activity.clone());
        prune_locked(&mut state);
        Ok((
            ActivityHandle {
                id: activity.id.clone(),
                generation: activity.generation,
                project_id: activity.project_id.clone(),
            },
            activity,
        ))
    }

    pub(crate) fn patch(
        &self,
        handle: &ActivityHandle,
        patch: ActivityPatch,
    ) -> Result<Option<AgentActivity>, AppError> {
        self.patch_id(
            &handle.id,
            handle.generation,
            handle.project_id.as_deref(),
            patch,
        )
    }

    pub(crate) fn patch_id(
        &self,
        id: &str,
        generation: u64,
        project_id: Option<&str>,
        patch: ActivityPatch,
    ) -> Result<Option<AgentActivity>, AppError> {
        let mut state = self
            .inner
            .lock()
            .map_err(|_| AppError::io("The activity registry lock is unavailable"))?;
        let Some(index) = state.activities.iter().position(|activity| {
            activity.id == id
                && activity.generation == generation
                && activity.project_id.as_deref() == project_id
        }) else {
            return Ok(None);
        };
        let before = state.activities[index].clone();
        apply_patch(&mut state.activities[index], patch);
        normalize_activity(&mut state.activities[index]);
        if activity_without_sequence(&before) == activity_without_sequence(&state.activities[index])
        {
            return Ok(None);
        }
        let sequence = next_sequence(&mut state)?;
        state.activities[index].sequence = sequence;
        Ok(Some(state.activities[index].clone()))
    }

    pub(crate) fn get(&self, handle: &ActivityHandle) -> Result<Option<AgentActivity>, AppError> {
        let state = self
            .inner
            .lock()
            .map_err(|_| AppError::io("The activity registry lock is unavailable"))?;
        Ok(state
            .activities
            .iter()
            .find(|activity| {
                activity.id == handle.id
                    && activity.generation == handle.generation
                    && activity.project_id == handle.project_id
            })
            .cloned())
    }

    pub(crate) fn snapshot(
        &self,
        generation: u64,
        project_id: Option<&str>,
    ) -> Result<Vec<AgentActivity>, AppError> {
        let state = self
            .inner
            .lock()
            .map_err(|_| AppError::io("The activity registry lock is unavailable"))?;
        Ok(state
            .activities
            .iter()
            .filter(|activity| {
                activity.generation == generation && activity.project_id.as_deref() == project_id
            })
            .cloned()
            .collect())
    }
    pub(crate) fn reconcile_jobs(
        &self,
        generation: u64,
        project_id: Option<&str>,
        jobs: &[crate::media::jobs::ActivityJobSnapshot],
    ) -> Result<Vec<AgentActivity>, AppError> {
        let mut changed = Vec::new();
        let mut state = self
            .inner
            .lock()
            .map_err(|_| AppError::io("The activity registry lock is unavailable"))?;
        for index in 0..state.activities.len() {
            let activity = &state.activities[index];
            if activity.generation != generation || activity.project_id.as_deref() != project_id {
                continue;
            }
            let mut matching: Vec<_> = jobs
                .iter()
                .filter(|job| {
                    job.summary.generation == generation
                        && job.summary.project_id.as_deref() == project_id
                        && (job.activity_id.as_deref() == Some(activity.id.as_str())
                            || activity.job_ids.contains(&job.summary.job_id))
                })
                .collect();
            if matching.is_empty() || matching.len() < activity.job_ids.len() {
                continue;
            }
            matching.sort_by(|left, right| left.summary.job_id.cmp(&right.summary.job_id));
            let active = matching
                .iter()
                .any(|job| matches!(job.summary.state, JobState::Queued | JobState::Running));
            let failed = matching
                .iter()
                .find(|job| job.summary.state == JobState::Failed);
            let cancelled = matching
                .iter()
                .find(|job| job.summary.state == JobState::Cancelled);
            let phase = if active {
                if matching.iter().any(|job| {
                    job.cancellation_requested
                        && matches!(job.summary.state, JobState::Queued | JobState::Running)
                }) {
                    ActivityPhase::Cancelling
                } else if activity.phase == ActivityPhase::AwaitingApproval {
                    ActivityPhase::AwaitingApproval
                } else if matching
                    .iter()
                    .any(|job| job.summary.state == JobState::Running)
                {
                    ActivityPhase::Running
                } else {
                    ActivityPhase::Queued
                }
            } else if failed.is_some() {
                ActivityPhase::Failed
            } else if cancelled.is_some() {
                ActivityPhase::Cancelled
            } else {
                ActivityPhase::Completed
            };
            let before = activity.clone();
            let activity = &mut state.activities[index];
            activity.job_ids = bounded_job_ids(
                matching
                    .iter()
                    .map(|job| job.summary.job_id.clone())
                    .collect(),
            );
            for job in &matching {
                if let Some((asset_id, revision)) = &job.committed_asset {
                    add_target(
                        &mut activity.targets,
                        asset_target(asset_id, ActivityTargetSpace::Source),
                    );
                    activity.total_targets =
                        activity.total_targets.max(activity.targets.len() as u64);
                    activity.changed = true;
                    activity.revision = Some(activity.revision.unwrap_or(0).max(*revision));
                }
            }
            activity.phase = phase;
            activity.progress = Some(
                matching.iter().map(|job| job.summary.progress).sum::<f64>()
                    / matching.len() as f64,
            );
            activity.error = failed
                .or(cancelled)
                .and_then(|job| job.summary.error.clone());
            activity.message = if phase == ActivityPhase::Completed {
                None
            } else {
                Some(format_activity_phase(phase))
            };
            normalize_activity(activity);
            if activity_without_sequence(&before) != activity_without_sequence(activity) {
                let sequence = next_sequence(&mut state)?;
                state.activities[index].sequence = sequence;
                changed.push(state.activities[index].clone());
            }
        }
        Ok(changed)
    }

    pub(crate) fn approval_requested(
        &self,
        generation: u64,
        project_id: Option<&str>,
        run_id: Option<&str>,
        tool_call_id: Option<&str>,
        operation_id: &str,
    ) -> Result<Vec<AgentActivity>, AppError> {
        let mut state = self
            .inner
            .lock()
            .map_err(|_| AppError::io("The activity registry lock is unavailable"))?;
        if state.approvals.contains_key(operation_id) {
            return Ok(Vec::new());
        }
        let indices: Vec<usize> = state
            .activities
            .iter()
            .enumerate()
            .filter_map(|(index, activity)| {
                (activity.generation == generation
                    && activity.project_id.as_deref() == project_id
                    && activity.phase == ActivityPhase::Running
                    && activity.run_id.as_deref() == run_id
                    && activity.tool_call_id.as_deref() == tool_call_id)
                    .then_some(index)
            })
            .collect();
        let mut ids = Vec::with_capacity(indices.len());
        let mut changed = Vec::with_capacity(indices.len());
        for index in indices {
            state.activities[index].phase = ActivityPhase::AwaitingApproval;
            state.activities[index].message = Some("Waiting for native approval".to_owned());
            let sequence = next_sequence(&mut state)?;
            state.activities[index].sequence = sequence;
            ids.push(state.activities[index].id.clone());
            changed.push(state.activities[index].clone());
        }
        state.approvals.insert(operation_id.to_owned(), ids);
        Ok(changed)
    }

    pub(crate) fn approval_decided(
        &self,
        operation_id: &str,
        allowed: bool,
    ) -> Result<Vec<AgentActivity>, AppError> {
        let mut state = self
            .inner
            .lock()
            .map_err(|_| AppError::io("The activity registry lock is unavailable"))?;
        let ids = state.approvals.remove(operation_id).unwrap_or_default();
        let mut changed = Vec::new();
        for id in ids {
            let Some(index) = state.activities.iter().position(|item| item.id == id) else {
                continue;
            };
            if state.activities[index].phase != ActivityPhase::AwaitingApproval {
                continue;
            }
            if allowed {
                state.activities[index].phase = ActivityPhase::Running;
                state.activities[index].message = None;
            } else {
                state.activities[index].phase = ActivityPhase::Failed;
                state.activities[index].error = Some(AppError::new(
                    ErrorCode::PermissionDenied,
                    "The native permission request was denied",
                ));
                state.activities[index].message = Some("Native approval denied".to_owned());
            }
            let sequence = next_sequence(&mut state)?;
            state.activities[index].sequence = sequence;
            changed.push(state.activities[index].clone());
        }
        Ok(changed)
    }

    pub(crate) fn approval_revoked(
        &self,
        operation_id: &str,
    ) -> Result<Vec<AgentActivity>, AppError> {
        self.approval_decided(operation_id, false)
    }
}

fn next_sequence(state: &mut RegistryState) -> Result<u64, AppError> {
    let sequence = state
        .next_sequence
        .checked_add(1)
        .ok_or_else(|| AppError::io("The activity sequence exhausted its safe range"))?;
    if sequence > 9_007_199_254_740_991 {
        return Err(AppError::io(
            "The activity sequence exhausted its safe range",
        ));
    }
    state.next_sequence = sequence;
    Ok(sequence)
}

fn prune_locked(state: &mut RegistryState) {
    while state.activities.len() > MAX_ACTIVITY_REGISTRY {
        let index = state
            .activities
            .iter()
            .position(|activity| is_terminal(activity.phase))
            .unwrap_or(0);
        state.activities.remove(index);
    }
    let present: HashSet<String> = state
        .activities
        .iter()
        .map(|item| item.id.clone())
        .collect();
    state.approvals.retain(|_, ids| {
        ids.retain(|id| present.contains(id));
        !ids.is_empty()
    });
}

fn is_terminal(phase: ActivityPhase) -> bool {
    matches!(
        phase,
        ActivityPhase::Completed | ActivityPhase::Cancelled | ActivityPhase::Failed
    )
}

fn apply_patch(activity: &mut AgentActivity, patch: ActivityPatch) {
    if let Some(value) = patch.phase {
        activity.phase = value;
    }
    if let Some(value) = patch.revision {
        activity.revision = Some(value);
    }
    if let Some(value) = patch.transaction_id {
        activity.transaction_id = Some(value);
    }
    if let Some(value) = patch.changed {
        activity.changed = value && !activity.dry_run;
    }
    if let Some(value) = patch.targets {
        activity.targets = bounded_targets(value);
    }
    if let Some(value) = patch.total_targets {
        activity.total_targets = value;
    }
    if let Some(value) = patch.changes {
        activity.changes = value.into_iter().take(MAX_ACTIVITY_CHANGES).collect();
    }
    if let Some(value) = patch.job_ids {
        activity.job_ids = bounded_job_ids(value);
    }
    if let Some(value) = patch.progress {
        activity.progress = value.map(|item| item.clamp(0.0, 1.0));
    }
    if let Some(value) = patch.message {
        activity.message = value.map(|item| truncate(&item));
    }
    if let Some(value) = patch.error {
        activity.error = value;
    }
}

fn normalize_activity(activity: &mut AgentActivity) {
    activity.targets = bounded_targets(std::mem::take(&mut activity.targets));
    activity.changes = activity
        .changes
        .drain(..)
        .take(MAX_ACTIVITY_CHANGES)
        .collect();
    activity.job_ids = bounded_job_ids(std::mem::take(&mut activity.job_ids));
    activity.total_targets = activity.total_targets.max(activity.targets.len() as u64);
    if activity.dry_run {
        activity.changed = false;
    }
    if is_terminal(activity.phase)
        && activity.error.is_none()
        && activity.phase == ActivityPhase::Completed
    {
        activity.progress = activity.progress.map(|_| 1.0);
    }
}

fn activity_without_sequence(activity: &AgentActivity) -> AgentActivity {
    let mut copy = activity.clone();
    copy.sequence = 0;
    copy
}

fn validate_activity_string(value: &str, field: &str, max: usize) -> Result<(), AppError> {
    if value.trim().is_empty()
        || value.len() > max
        || value.contains('\0')
        || value.contains('\n')
        || value.contains('\r')
    {
        return Err(AppError::invalid_argument(format!("{field} is invalid")));
    }
    Ok(())
}

fn truncate(value: &str) -> String {
    value.chars().take(MAX_ACTIVITY_STRING).collect()
}

fn bounded_job_ids(mut ids: Vec<String>) -> Vec<String> {
    ids.retain(|id| uuid::Uuid::parse_str(id).is_ok());
    ids.sort();
    ids.dedup();
    ids.truncate(64);
    ids
}

fn bounded_targets(targets: Vec<ActivityTarget>) -> Vec<ActivityTarget> {
    let mut seen = HashSet::new();
    targets
        .into_iter()
        .filter(|target| {
            !target.id.is_empty()
                && target.id.len() <= 256
                && !target.id.contains('\n')
                && seen.insert((
                    target.kind,
                    target.id.clone(),
                    target.track_id.clone(),
                    target.start_frame,
                    target.end_frame,
                ))
        })
        .take(MAX_ACTIVITY_TARGETS)
        .collect()
}

fn add_target(targets: &mut Vec<ActivityTarget>, target: ActivityTarget) {
    if targets.iter().any(|item| item == &target) {
        return;
    }
    if targets.len() < MAX_ACTIVITY_TARGETS {
        targets.push(target);
    }
}

fn activity_target(kind: ActivityTargetKind, id: &str) -> ActivityTarget {
    ActivityTarget {
        kind,
        id: id.to_owned(),
        track_id: None,
        space: None,
        start_frame: None,
        end_frame: None,
    }
}

fn asset_target(id: &str, space: ActivityTargetSpace) -> ActivityTarget {
    ActivityTarget {
        kind: ActivityTargetKind::Asset,
        id: id.to_owned(),
        track_id: None,
        space: Some(space),
        start_frame: None,
        end_frame: None,
    }
}

fn clip_target(clip: &MediaClip, space: ActivityTargetSpace) -> ActivityTarget {
    ActivityTarget {
        kind: ActivityTargetKind::Clip,
        id: clip.id.clone(),
        track_id: Some(clip.track_id.clone()),
        space: Some(space),
        start_frame: Some(clip.start_frame),
        end_frame: clip.end_frame().ok(),
    }
}

fn text_target(item: &TextItem, space: ActivityTargetSpace) -> ActivityTarget {
    let end = item
        .timeline_interval()
        .and_then(Result::ok)
        .map(|interval| interval.end_frame());
    ActivityTarget {
        kind: ActivityTargetKind::Text,
        id: item.id.clone(),
        track_id: Some(item.track_id.clone()),
        space: Some(space),
        start_frame: item.start_frame,
        end_frame: end,
    }
}

fn target_from_entity(
    entity: &crate::editor::history::EntityRef,
    state: Option<&EntityState>,
    space: ActivityTargetSpace,
) -> ActivityTarget {
    let kind = match entity.kind {
        EntityKind::Document => ActivityTargetKind::Project,
        EntityKind::Asset => ActivityTargetKind::Asset,
        EntityKind::Track => ActivityTargetKind::Track,
        EntityKind::Clip => ActivityTargetKind::Clip,
        EntityKind::TextItem => ActivityTargetKind::Text,
        EntityKind::Transition => ActivityTargetKind::Transition,
    };
    let mut target = activity_target(kind, &entity.id);
    target.space = Some(space);
    match state {
        Some(EntityState::Clip(clip)) => {
            target.track_id = Some(clip.track_id.clone());
            target.start_frame = Some(clip.start_frame);
            target.end_frame = clip.end_frame().ok();
        }
        Some(EntityState::TextItem(item)) => {
            target.track_id = Some(item.track_id.clone());
            target.start_frame = item.start_frame;
            target.end_frame = item
                .timeline_interval()
                .and_then(Result::ok)
                .map(|interval| interval.end_frame());
        }
        _ => {}
    }
    target
}

fn geometry(state: Option<&EntityState>) -> Option<ActivityGeometry> {
    match state {
        Some(EntityState::Clip(clip)) => Some(ActivityGeometry {
            track_id: Some(clip.track_id.clone()),
            start_frame: clip.start_frame,
            duration_frames: clip.duration_frames,
        }),
        Some(EntityState::TextItem(item)) => {
            item.timeline_interval()
                .and_then(Result::ok)
                .map(|interval| ActivityGeometry {
                    track_id: Some(item.track_id.clone()),
                    start_frame: interval.start_frame,
                    duration_frames: interval.duration_frames,
                })
        }
        _ => None,
    }
}

fn push_field(
    fields: &mut Vec<ActivityFieldChange>,
    field: &str,
    before: Option<String>,
    after: Option<String>,
) {
    if before == after {
        return;
    }
    fields.push(ActivityFieldChange {
        field: field.to_owned(),
        before: before.map(|value| truncate(&value)).unwrap_or_default(),
        after: after.map(|value| truncate(&value)).unwrap_or_default(),
    });
}

fn json_string<T: Serialize>(value: &T) -> Option<String> {
    serde_json::to_string(value)
        .ok()
        .map(|value| truncate(&value))
}

fn entity_fields(
    before: Option<&EntityState>,
    after: Option<&EntityState>,
) -> Vec<ActivityFieldChange> {
    let mut fields = Vec::new();
    let (before, after) = match (before, after) {
        (Some(before), Some(after)) => (Some(before), Some(after)),
        (before, after) => (before, after),
    };
    let field = |state: Option<&EntityState>, name: &str| -> Option<String> {
        match (state, name) {
            (Some(EntityState::Document { name, .. }), "name") => Some(name.clone()),
            (Some(EntityState::Document { profile, .. }), "profile") => json_string(profile),
            (Some(EntityState::Asset(asset)), "kind") => json_string(&asset.kind),
            (Some(EntityState::Asset(asset)), "fileName") => Some(asset.original.file_name.clone()),
            (Some(EntityState::Asset(asset)), "contentHash") => Some(asset.content_hash.clone()),
            (Some(EntityState::Asset(asset)), "frameCount") => {
                asset.frame_count().map(|value| value.to_string())
            }
            (Some(EntityState::Track(track)), "name") => Some(track.name.clone()),
            (Some(EntityState::Track(track)), "muted") => Some(track.muted.to_string()),
            (Some(EntityState::Track(track)), "locked") => Some(track.locked.to_string()),
            (Some(EntityState::Clip(clip)), "trackId") => Some(clip.track_id.clone()),
            (Some(EntityState::Clip(clip)), "startFrame") => Some(clip.start_frame.to_string()),
            (Some(EntityState::Clip(clip)), "inFrame") => Some(clip.in_frame.to_string()),
            (Some(EntityState::Clip(clip)), "durationFrames") => {
                Some(clip.duration_frames.to_string())
            }
            (Some(EntityState::Clip(clip)), "fit") => json_string(&clip.fit),
            (Some(EntityState::Clip(clip)), "scale") => Some(clip.scale.to_string()),
            (Some(EntityState::Clip(clip)), "opacity") => Some(clip.opacity.to_string()),
            (Some(EntityState::Clip(clip)), "gainDb") => Some(clip.gain_db.to_string()),
            (Some(EntityState::Clip(clip)), "audioEnabled") => Some(clip.audio_enabled.to_string()),
            (Some(EntityState::Clip(clip)), "fadeInFrames") => {
                Some(clip.fade_in_frames.to_string())
            }
            (Some(EntityState::Clip(clip)), "fadeOutFrames") => {
                Some(clip.fade_out_frames.to_string())
            }
            (Some(EntityState::TextItem(item)), "trackId") => Some(item.track_id.clone()),
            (Some(EntityState::TextItem(item)), "text") => Some(item.text.clone()),
            (Some(EntityState::TextItem(item)), "kind") => json_string(&item.kind),
            (Some(EntityState::TextItem(item)), "style") => json_string(&item.style),
            (Some(EntityState::TextItem(item)), "startFrame") => {
                item.start_frame.map(|value| value.to_string())
            }
            (Some(EntityState::TextItem(item)), "durationFrames") => {
                item.duration_frames.map(|value| value.to_string())
            }
            (Some(EntityState::TextItem(item)), "ownerClipId") => item.owner_clip_id.clone(),
            (Some(EntityState::Transition(item)), "leftClipId") => Some(item.left_clip_id.clone()),
            (Some(EntityState::Transition(item)), "rightClipId") => {
                Some(item.right_clip_id.clone())
            }
            (Some(EntityState::Transition(item)), "durationFrames") => {
                Some(item.duration_frames.to_string())
            }
            _ => None,
        }
    };
    let names: &[&str] = match (before.or(after)) {
        Some(EntityState::Document { .. }) => &["name", "profile"],
        Some(EntityState::Asset(_)) => &["kind", "fileName", "contentHash", "frameCount"],
        Some(EntityState::Track(_)) => &["name", "muted", "locked"],
        Some(EntityState::Clip(_)) => &[
            "trackId",
            "startFrame",
            "inFrame",
            "durationFrames",
            "fit",
            "scale",
            "opacity",
            "gainDb",
            "audioEnabled",
            "fadeInFrames",
            "fadeOutFrames",
        ],
        Some(EntityState::TextItem(_)) => &[
            "trackId",
            "text",
            "kind",
            "style",
            "startFrame",
            "durationFrames",
            "ownerClipId",
        ],
        Some(EntityState::Transition(_)) => &["leftClipId", "rightClipId", "durationFrames"],
        None => &[],
    };
    for name in names {
        push_field(&mut fields, name, field(before, name), field(after, name));
    }
    fields
}

pub(crate) fn changes_from_delta(delta: &TransactionDelta) -> (Vec<ActivityChange>, u64) {
    let total = delta.changes.len() as u64;
    let changes = delta
        .changes
        .iter()
        .take(MAX_ACTIVITY_CHANGES)
        .map(|change| {
            let target = target_from_entity(
                &change.entity,
                change.after.as_ref().or(change.before.as_ref()),
                ActivityTargetSpace::Timeline,
            );
            ActivityChange {
                target,
                before: geometry(change.before.as_ref()),
                after: geometry(change.after.as_ref()),
                fields: entity_fields(change.before.as_ref(), change.after.as_ref()),
            }
        })
        .collect();
    (changes, total)
}

pub(crate) fn changes_between(
    before: &ProjectDocument,
    after: &ProjectDocument,
) -> Result<(Vec<ActivityChange>, u64), AppError> {
    let delta = TransactionDelta::between(before, after)?;
    Ok(changes_from_delta(&delta))
}

pub(crate) fn extract_job_ids(reply: &EditorReply) -> Vec<String> {
    let value = serde_json::to_value(reply).unwrap_or(Value::Null);
    let mut ids = Vec::new();
    collect_job_ids(&value, &mut ids);
    bounded_job_ids(ids)
}

fn collect_job_ids(value: &Value, ids: &mut Vec<String>) {
    match value {
        Value::Object(map) => {
            for (key, value) in map {
                if key.eq_ignore_ascii_case("jobId") {
                    if let Some(id) = value.as_str() {
                        ids.push(id.to_owned());
                    }
                } else if key.eq_ignore_ascii_case("jobIds") {
                    if let Some(values) = value.as_array() {
                        for value in values {
                            if let Some(id) = value.as_str() {
                                ids.push(id.to_owned());
                            }
                        }
                    }
                }
                collect_job_ids(value, ids);
            }
        }
        Value::Array(values) => {
            for value in values {
                collect_job_ids(value, ids);
            }
        }
        _ => {}
    }
}

fn format_activity_phase(phase: ActivityPhase) -> String {
    match phase {
        ActivityPhase::Running => "Native operation running",
        ActivityPhase::AwaitingApproval => "Waiting for native approval",
        ActivityPhase::Queued => "Native job queued",
        ActivityPhase::Cancelling => "Cancelling native job",
        ActivityPhase::Completed => "Native operation completed",
        ActivityPhase::Cancelled => "Native operation cancelled",
        ActivityPhase::Failed => "Native operation failed",
    }
    .to_owned()
}

fn active_targets_at(document: &ProjectDocument, frame: u64) -> Vec<ActivityTarget> {
    let mut targets = Vec::new();
    for clip in &document.clips {
        if clip.start_frame <= frame && clip.end_frame().is_ok_and(|end| frame < end) {
            add_target(
                &mut targets,
                clip_target(clip, ActivityTargetSpace::Timeline),
            );
            add_target(
                &mut targets,
                activity_target(ActivityTargetKind::Track, &clip.track_id),
            );
        }
    }
    for text in &document.text_items {
        if text
            .timeline_interval()
            .and_then(Result::ok)
            .is_some_and(|interval| interval.start_frame <= frame && frame < interval.end_frame())
        {
            add_target(
                &mut targets,
                text_target(text, ActivityTargetSpace::Timeline),
            );
        }
    }
    targets
}

fn basename(path: &str) -> Option<String> {
    let name = path.rsplit(['/', '\\']).next()?.trim();
    if name.is_empty() {
        None
    } else {
        Some(truncate(name))
    }
}

fn label_with_filename(label: &str, paths: Option<&Vec<String>>) -> String {
    let Some(name) = paths
        .and_then(|paths| paths.first())
        .and_then(|path| basename(path))
    else {
        return label.to_owned();
    };
    format!("{label}: {name}")
}

pub(crate) fn describe_request(
    request: &EditorRequest,
    caller: &CallerContext,
    document: Option<&ProjectDocument>,
) -> Option<ActivityStart> {
    if matches!(caller.kind, CallerKind::HumanWindow { .. })
        && matches!(
            request,
            EditorRequest::Media(
                MediaAction::List {}
                    | MediaAction::Inspect { .. }
                    | MediaAction::Thumbnail { .. }
                    | MediaAction::Waveform { .. }
            ) | EditorRequest::Preview(
                PreviewAction::Plan { .. }
                    | PreviewAction::RenderFrame { .. }
                    | PreviewAction::RenderAudioWindow { .. }
                    | PreviewAction::Inspect { .. }
            )
        )
    {
        return None;
    }
    let origin = match caller.kind {
        CallerKind::AgentSidecar { .. } => ActivityOrigin::Agent,
        CallerKind::HumanWindow { .. } => ActivityOrigin::User,
    };
    let agent = matches!(origin, ActivityOrigin::Agent);
    let mut targets = Vec::new();
    let import_label = match request {
        EditorRequest::Media(MediaAction::Import { paths }) => {
            label_with_filename("Import media", paths.as_ref())
        }
        _ => String::new(),
    };
    let (tool, action, label, dry_run, track) = match request {
        EditorRequest::ProjectStatus {} => {
            ("project", "status", "Inspect project status", false, false)
        }
        EditorRequest::ProjectCreate { .. } => {
            ("project", "create", "Create project", false, false)
        }
        EditorRequest::ProjectOpen {} => ("project", "open", "Open project", false, false),
        EditorRequest::ProjectClose {} => ("project", "close", "Close project", false, false),
        EditorRequest::ProjectSave {} => ("project", "save", "Save project", false, true),
        EditorRequest::ProjectSnapshot {} => {
            ("project", "snapshot", "Read project snapshot", false, false)
        }
        EditorRequest::TimelineSnapshot {} => (
            "timeline",
            "snapshot",
            "Read timeline snapshot",
            false,
            false,
        ),
        EditorRequest::TimelineSelection { .. } => (
            "timeline",
            "selection",
            "Update timeline selection",
            false,
            true,
        ),
        EditorRequest::ProjectHistory { action, .. } => (
            "history",
            action.as_str(),
            "Apply project history",
            false,
            true,
        ),
        EditorRequest::EditProject { label, dry_run, .. } => (
            "edit_project",
            "edit",
            label.as_str(),
            dry_run.unwrap_or(false),
            true,
        ),
        EditorRequest::Media(action) => match action {
            MediaAction::List {} => ("media", "list", "Read media library", false, false),
            MediaAction::Inspect { asset_id } => {
                targets.push(asset_target(asset_id, ActivityTargetSpace::Source));
                ("media", "inspect", "Inspect media asset", false, true)
            }
            MediaAction::Import { .. } => ("media", "import", import_label.as_str(), false, true),
            MediaAction::Relink { asset_id } => {
                targets.push(asset_target(asset_id, ActivityTargetSpace::Source));
                ("media", "relink", "Relink media asset", false, true)
            }
            MediaAction::Remove { asset_id } => {
                targets.push(asset_target(asset_id, ActivityTargetSpace::Source));
                ("media", "remove", "Remove media asset", false, true)
            }
            MediaAction::Thumbnail { asset_id, .. } => {
                targets.push(asset_target(asset_id, ActivityTargetSpace::Source));
                ("media", "thumbnail", "Render media thumbnail", false, true)
            }
            MediaAction::Waveform { asset_id } => {
                targets.push(asset_target(asset_id, ActivityTargetSpace::Source));
                ("media", "waveform", "Build media waveform", false, true)
            }
        },
        EditorRequest::Jobs(action) => match action {
            JobsAction::List {} => ("jobs", "list", "Read native jobs", false, false),
            JobsAction::Get { .. } => ("jobs", "get", "Inspect native job", false, false),
            JobsAction::Cancel { .. } => ("jobs", "cancel", "Cancel native job", false, true),
        },
        EditorRequest::Preview(action) => match action {
            PreviewAction::Plan { .. } => ("preview", "plan", "Inspect preview plan", false, true),
            PreviewAction::RenderFrame { frame, .. } => {
                if let Some(document) = document {
                    targets.extend(active_targets_at(document, *frame));
                }
                (
                    "preview",
                    "render_frame",
                    "Render preview frame",
                    false,
                    true,
                )
            }
            PreviewAction::RenderAudioWindow { .. } => (
                "preview",
                "render_audio",
                "Render preview audio",
                false,
                false,
            ),
            PreviewAction::Seek { .. } => ("preview", "seek", "Seek preview", false, true),
            PreviewAction::Play {} => ("preview", "play", "Play preview", false, true),
            PreviewAction::Pause {} => ("preview", "pause", "Pause preview", false, true),
            PreviewAction::Inspect { frames, .. } => {
                if let Some(document) = document {
                    for frame in frames.iter().copied().take(32) {
                        targets.extend(active_targets_at(document, frame));
                    }
                }
                ("preview", "inspect", "Inspect preview frames", false, true)
            }
        },
        EditorRequest::ExportVideo(action) => match action {
            crate::media::export::ExportAction::Start { .. } => {
                ("export", "start", "Export video", false, true)
            }
            crate::media::export::ExportAction::Status { .. } => {
                ("export", "status", "Read export status", false, false)
            }
            crate::media::export::ExportAction::Get { .. } => {
                ("export", "get", "Read export result", false, false)
            }
            crate::media::export::ExportAction::Progress { .. } => {
                ("export", "progress", "Read export progress", false, false)
            }
            crate::media::export::ExportAction::Cancel { .. } => {
                ("export", "cancel", "Cancel export", false, true)
            }
            crate::media::export::ExportAction::Play { .. } => {
                ("export", "play", "Play exported video", false, false)
            }
            crate::media::export::ExportAction::ShowFile { .. } => {
                ("export", "show_file", "Show exported video", false, false)
            }
        },
        EditorRequest::Evidence(action) => evidence_description(action, &mut targets),
        EditorRequest::Transcript(action) => transcript_description(action, &mut targets),
        EditorRequest::AnalyzeMedia(action) => match action {
            crate::media::analysis::AnalysisAction::Scenes { asset_id, .. } => {
                targets.push(asset_target(asset_id, ActivityTargetSpace::Source));
                ("analysis", "scenes", "Analyze scenes", false, true)
            }
            crate::media::analysis::AnalysisAction::Silence { asset_id } => {
                targets.push(asset_target(asset_id, ActivityTargetSpace::Source));
                ("analysis", "silence", "Analyze silence", false, true)
            }
        },
        EditorRequest::SampleFrames(action) => {
            targets.push(ActivityTarget {
                kind: ActivityTargetKind::Asset,
                id: action.asset_id.clone(),
                track_id: None,
                space: Some(ActivityTargetSpace::Source),
                start_frame: Some(action.start_frame),
                end_frame: Some(action.end_frame),
            });
            (
                "evidence",
                "sample_frames",
                "Sample source frames",
                false,
                true,
            )
        }
        EditorRequest::CreateGraphic(_) => {
            ("graphics", "create", "Create graphic asset", false, true)
        }
        EditorRequest::Assistant(action) => {
            if !agent {
                return None;
            }
            let name = match action {
                crate::assistant::AssistantAction::Prompt { .. } => "prompt",
                crate::assistant::AssistantAction::Stop {} => "stop",
                crate::assistant::AssistantAction::Restart {} => "restart",
                crate::assistant::AssistantAction::NewSession {} => "new_session",
                _ => "status",
            };
            ("assistant", name, "Assistant operation", false, false)
        }
        EditorRequest::Providers(action) => {
            if !agent {
                return None;
            }
            let name = match action {
                crate::assistant::ProvidersAction::Login { .. } => "login",
                crate::assistant::ProvidersAction::Answer { .. } => "answer",
                crate::assistant::ProvidersAction::Logout { .. } => "logout",
                crate::assistant::ProvidersAction::Select { .. } => "select",
                crate::assistant::ProvidersAction::Refresh {} => "refresh",
                _ => "status",
            };
            ("providers", name, "Provider operation", false, false)
        }
        EditorRequest::Permissions(action) => {
            if !agent {
                return None;
            }
            let name = match action {
                crate::permissions::PermissionsAction::Pending {} => "pending",
                crate::permissions::PermissionsAction::Answer { .. } => "answer",
                crate::permissions::PermissionsAction::Evidence { .. } => "evidence",
                crate::permissions::PermissionsAction::Revoke { .. } => "revoke",
                crate::permissions::PermissionsAction::GrantFiles { .. } => "grant_files",
                crate::permissions::PermissionsAction::SystemRead(_) => "system_read",
                crate::permissions::PermissionsAction::SystemWrite(_) => "system_write",
                crate::permissions::PermissionsAction::SystemExecute(_) => "system_execute",
                crate::permissions::PermissionsAction::SystemHttp(_) => "system_http",
                _ => "permission",
            };
            let label = match action {
                crate::permissions::PermissionsAction::SystemRead(_) => "Read external file",
                crate::permissions::PermissionsAction::SystemWrite(_) => "Write external file",
                crate::permissions::PermissionsAction::SystemExecute(_) => "Run external command",
                crate::permissions::PermissionsAction::SystemHttp(_) => {
                    "Send external HTTP request"
                }
                _ => "Permission operation",
            };
            ("permissions", name, label, false, false)
        }
    };
    if !track && !agent {
        return None;
    }
    if targets.is_empty()
        && track
        && document.is_some()
        && !matches!(
            request,
            EditorRequest::ProjectSave { .. } | EditorRequest::TimelineSelection { .. }
        )
    {
        targets.push(activity_target(
            ActivityTargetKind::Project,
            &document.unwrap().project_id,
        ));
    }
    Some(ActivityStart {
        generation: caller.generation,
        project_id: caller.project_id.clone(),
        origin,
        run_id: caller.run_id.clone(),
        tool_call_id: caller.tool_call_id.clone(),
        tool: tool.to_owned(),
        action: action.to_owned(),
        label: truncate(label),
        dry_run,
        targets: bounded_targets(targets),
    })
}

fn evidence_description(
    action: &EvidenceAction,
    targets: &mut Vec<ActivityTarget>,
) -> (&'static str, &'static str, &'static str, bool, bool) {
    match action {
        EvidenceAction::ModelStatus {} => (
            "evidence",
            "model_status",
            "Read speech model status",
            false,
            false,
        ),
        EvidenceAction::Transcribe {
            asset_id,
            start_frame,
            end_frame,
            ..
        } => {
            targets.push(ActivityTarget {
                kind: ActivityTargetKind::Asset,
                id: asset_id.clone(),
                track_id: None,
                space: Some(ActivityTargetSpace::Source),
                start_frame: *start_frame,
                end_frame: *end_frame,
            });
            ("evidence", "transcribe", "Transcribe media", false, true)
        }
        EvidenceAction::Read { transcript_id } => {
            targets.push(asset_target(transcript_id, ActivityTargetSpace::Source));
            ("evidence", "read", "Read transcript", false, true)
        }
        EvidenceAction::Search { asset_id, .. } => {
            if let Some(asset_id) = asset_id {
                targets.push(asset_target(asset_id, ActivityTargetSpace::Source));
            }
            ("evidence", "search", "Search transcript", false, true)
        }
        EvidenceAction::ImportSrt { .. } => {
            ("evidence", "import_srt", "Import subtitles", false, true)
        }
        EvidenceAction::ExportSrt {} => ("evidence", "export_srt", "Export subtitles", false, true),
        EvidenceAction::Scenes { asset_id, .. } => {
            targets.push(asset_target(asset_id, ActivityTargetSpace::Source));
            ("evidence", "scenes", "Analyze scenes", false, true)
        }
        EvidenceAction::Silence { asset_id } => {
            targets.push(asset_target(asset_id, ActivityTargetSpace::Source));
            ("evidence", "silence", "Analyze silence", false, true)
        }
        EvidenceAction::SampleFrames {
            asset_id,
            start_frame,
            end_frame,
            ..
        } => {
            targets.push(ActivityTarget {
                kind: ActivityTargetKind::Asset,
                id: asset_id.clone(),
                track_id: None,
                space: Some(ActivityTargetSpace::Source),
                start_frame: Some(*start_frame),
                end_frame: Some(*end_frame),
            });
            (
                "evidence",
                "sample_frames",
                "Sample source frames",
                false,
                true,
            )
        }
        EvidenceAction::CreateGraphic { .. } => {
            ("graphics", "create", "Create graphic asset", false, true)
        }
    }
}

fn transcript_description(
    action: &crate::media::evidence::TranscriptAction,
    targets: &mut Vec<ActivityTarget>,
) -> (&'static str, &'static str, &'static str, bool, bool) {
    match action {
        crate::media::evidence::TranscriptAction::ModelStatus {} => (
            "transcript",
            "model_status",
            "Read speech model status",
            false,
            false,
        ),
        crate::media::evidence::TranscriptAction::Transcribe {
            asset_id,
            start_frame,
            end_frame,
            ..
        } => {
            targets.push(ActivityTarget {
                kind: ActivityTargetKind::Asset,
                id: asset_id.clone(),
                track_id: None,
                space: Some(ActivityTargetSpace::Source),
                start_frame: *start_frame,
                end_frame: *end_frame,
            });
            ("transcript", "transcribe", "Transcribe media", false, true)
        }
        crate::media::evidence::TranscriptAction::Read { transcript_id } => {
            targets.push(asset_target(transcript_id, ActivityTargetSpace::Source));
            ("transcript", "read", "Read transcript", false, true)
        }
        crate::media::evidence::TranscriptAction::Search { asset_id, .. } => {
            if let Some(asset_id) = asset_id {
                targets.push(asset_target(asset_id, ActivityTargetSpace::Source));
            }
            ("transcript", "search", "Search transcript", false, true)
        }
        crate::media::evidence::TranscriptAction::ImportSrt { .. } => {
            ("transcript", "import_srt", "Import subtitles", false, true)
        }
        crate::media::evidence::TranscriptAction::ExportSrt {} => {
            ("transcript", "export_srt", "Export subtitles", false, true)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scope() -> ActivityStart {
        ActivityStart {
            generation: 7,
            project_id: Some("project".to_owned()),
            origin: ActivityOrigin::Agent,
            run_id: Some("run".to_owned()),
            tool_call_id: Some("tool".to_owned()),
            tool: "edit_project".to_owned(),
            action: "edit".to_owned(),
            label: "Edit".to_owned(),
            dry_run: false,
            targets: vec![],
        }
    }

    fn job_snapshot(summary: JobSummary) -> crate::media::jobs::ActivityJobSnapshot {
        crate::media::jobs::ActivityJobSnapshot {
            summary,
            activity_id: None,
            cancellation_requested: false,
            committed_asset: None,
        }
    }

    #[test]
    fn registry_is_bounded_and_sequences_only_change_on_mutation() {
        let registry = ActivityRegistry::new();
        let (handle, first) = registry.start(scope()).expect("start");
        assert_eq!(first.sequence, 1);
        assert!(registry
            .patch(&handle, ActivityPatch::default())
            .expect("noop")
            .is_none());
        let changed = registry
            .patch(
                &handle,
                ActivityPatch {
                    phase: Some(ActivityPhase::Completed),
                    ..Default::default()
                },
            )
            .expect("patch")
            .expect("changed");
        assert!(changed.sequence > first.sequence);
        for _ in 0..(MAX_ACTIVITY_REGISTRY + 8) {
            registry.start(scope()).expect("bounded start");
        }
        assert!(
            registry
                .snapshot(7, Some("project"))
                .expect("snapshot")
                .len()
                <= MAX_ACTIVITY_REGISTRY
        );
    }

    #[test]
    fn dry_run_never_reports_changed() {
        let mut start = scope();
        start.dry_run = true;
        let registry = ActivityRegistry::new();
        let (handle, _) = registry.start(start).expect("start");
        let activity = registry
            .patch(
                &handle,
                ActivityPatch {
                    changed: Some(true),
                    ..Default::default()
                },
            )
            .expect("patch");
        assert!(activity.is_none());
        assert!(!registry.snapshot(7, Some("project")).unwrap()[0].changed);
    }

    #[test]
    fn job_reconciliation_keeps_queued_running_and_terminal_truthful() {
        let registry = ActivityRegistry::new();
        let (handle, _) = registry.start(scope()).expect("start");
        let job = JobSummary {
            job_id: Uuid::new_v4().to_string(),
            kind: "media_import".to_owned(),
            priority: crate::media::jobs::JobPriority::Background,
            state: JobState::Queued,
            progress: 0.0,
            generation: 7,
            project_id: Some("project".to_owned()),
            run_id: Some("run".to_owned()),
            error: None,
            created_at_ms: 0,
        };
        let _ = registry
            .patch(
                &handle,
                ActivityPatch {
                    job_ids: Some(vec![job.job_id.clone()]),
                    phase: Some(ActivityPhase::Queued),
                    ..Default::default()
                },
            )
            .expect("attach");
        let queued = registry
            .reconcile_jobs(7, Some("project"), &[job_snapshot(job.clone())])
            .expect("queued");
        assert_eq!(
            queued.last().expect("queued update").phase,
            ActivityPhase::Queued
        );
        let mut running = job.clone();
        running.state = JobState::Running;
        running.progress = 0.42;
        let running = registry
            .reconcile_jobs(7, Some("project"), &[job_snapshot(running)])
            .expect("running");
        assert_eq!(
            running.last().expect("running update").phase,
            ActivityPhase::Running
        );
        let mut completed = job;
        completed.state = JobState::Completed;
        completed.progress = 1.0;
        let completed = registry
            .reconcile_jobs(7, Some("project"), &[job_snapshot(completed)])
            .expect("completed");
        assert_eq!(
            completed.last().expect("completed update").phase,
            ActivityPhase::Completed
        );
        assert!(!completed.last().unwrap().changed);
    }

    #[test]
    fn job_groups_wait_for_all_workers_and_preserve_commits_after_cancel() {
        let registry = ActivityRegistry::new();
        let (handle, _) = registry.start(scope()).unwrap();
        let mut first = job_snapshot(JobSummary {
            job_id: Uuid::new_v4().to_string(),
            kind: "media_import".to_owned(),
            priority: crate::media::jobs::JobPriority::Background,
            state: JobState::Failed,
            progress: 0.2,
            generation: 7,
            project_id: Some("project".to_owned()),
            run_id: Some("run".to_owned()),
            error: Some(AppError::io("failed")),
            created_at_ms: 0,
        });
        let mut second = first.clone();
        second.summary.job_id = Uuid::new_v4().to_string();
        second.summary.state = JobState::Running;
        second.summary.error = None;
        second.cancellation_requested = true;
        let asset_id = Uuid::new_v4().to_string();
        second.committed_asset = Some((asset_id.clone(), 12));
        registry
            .patch(
                &handle,
                ActivityPatch {
                    job_ids: Some(vec![
                        first.summary.job_id.clone(),
                        second.summary.job_id.clone(),
                    ]),
                    ..Default::default()
                },
            )
            .unwrap();
        let updates = registry
            .reconcile_jobs(7, Some("project"), &[first.clone(), second.clone()])
            .unwrap();
        let activity = updates.last().unwrap();
        assert_eq!(activity.phase, ActivityPhase::Cancelling);
        assert!(activity.changed);
        assert_eq!(activity.revision, Some(12));
        assert!(activity.targets.iter().any(|target| target.id == asset_id));
        assert!(registry
            .reconcile_jobs(7, Some("project"), &[second.clone(), first.clone()])
            .unwrap()
            .is_empty());
        second.summary.state = JobState::Cancelled;
        let updates = registry
            .reconcile_jobs(7, Some("project"), &[second.clone(), first.clone()])
            .unwrap();
        assert_eq!(updates.last().unwrap().phase, ActivityPhase::Failed);
        assert!(updates.last().unwrap().changed);
        first.summary.state = JobState::Completed;
        first.summary.error = None;
        let updates = registry
            .reconcile_jobs(7, Some("project"), &[first, second])
            .unwrap();
        assert_eq!(updates.last().unwrap().phase, ActivityPhase::Cancelled);
        assert!(updates.last().unwrap().changed);
    }

    #[test]
    fn approval_waits_correlate_to_the_exact_tool_and_do_not_replay() {
        let registry = ActivityRegistry::new();
        registry.start(scope()).unwrap();
        let mut other = scope();
        other.tool_call_id = Some("other".to_owned());
        registry.start(other).unwrap();
        let updates = registry
            .approval_requested(7, Some("project"), Some("run"), Some("tool"), "approval")
            .unwrap();
        assert_eq!(updates.len(), 1);
        assert_eq!(updates[0].tool_call_id.as_deref(), Some("tool"));
        assert!(registry
            .approval_requested(7, Some("project"), Some("run"), Some("tool"), "approval")
            .unwrap()
            .is_empty());
        let updates = registry.approval_decided("approval", true).unwrap();
        assert_eq!(updates.len(), 1);
        assert_eq!(updates[0].phase, ActivityPhase::Running);
    }
}
