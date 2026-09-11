use crate::error::{AppError, ErrorCode};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::io::{Read, Write};
use std::process::{ChildStdin, Command, Output, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use ts_rs::TS;
use uuid::Uuid;

const MAX_JOB_OUTPUT_BYTES: usize = 16 * 1024 * 1024;
const MAX_RETAINED_JOBS: usize = 256;

fn invalid(message: impl Into<String>) -> AppError {
    AppError::invalid_argument(message)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
#[ts(rename_all = "snake_case")]
pub enum JobPriority {
    Foreground,
    Background,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
#[ts(rename_all = "snake_case")]
pub enum JobState {
    Queued,
    Running,
    Completed,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase")]
pub struct JobSummary {
    pub job_id: String,
    pub kind: String,
    pub priority: JobPriority,
    pub state: JobState,
    #[ts(type = "number")]
    pub progress: f64,
    #[ts(type = "SafeInteger")]
    pub generation: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub project_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub run_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub error: Option<AppError>,
    #[ts(type = "SafeInteger")]
    pub created_at_ms: u64,
}

impl JobSummary {
    pub fn validate(&self) -> Result<(), AppError> {
        if Uuid::parse_str(&self.job_id).is_err() {
            return Err(invalid("Job ID must be a UUID"));
        }
        if !self.progress.is_finite() || !(0.0..=1.0).contains(&self.progress) {
            return Err(invalid("Job progress must be between zero and one"));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase")]
pub struct JobResult {
    pub job_id: String,
    #[ts(type = "unknown")]
    pub data: Value,
}
#[derive(Clone)]
pub struct JobSpec {
    pub kind: String,
    pub priority: JobPriority,
    pub generation: u64,
    pub project_id: Option<String>,
    pub run_id: Option<String>,
    activity_id: Option<String>,
}

/// Called after a job's externally visible summary changes. Hooks run off the
/// registry lock so native event publishers may safely query state again.
pub type JobNotificationHook = Arc<dyn Fn(JobSummary, Option<Value>) + Send + Sync>;

#[derive(Clone)]
pub(crate) struct ActivityJobSnapshot {
    pub summary: JobSummary,
    pub activity_id: Option<String>,
    pub cancellation_requested: bool,
    pub committed_asset: Option<(String, u64)>,
}

/// Called for a worker's domain-specific metadata event (for example probe
/// metadata). The callback is native-owned; callers never provide event data.
pub type JobEventHook = Arc<dyn Fn(String, Value) + Send + Sync>;

impl JobSpec {
    pub fn new(
        kind: impl Into<String>,
        priority: JobPriority,
        generation: u64,
        project_id: Option<String>,
    ) -> Result<Self, AppError> {
        let kind = kind.into();
        if kind.trim().is_empty() || kind.len() > 128 || kind.contains('\0') || kind.contains('\n')
        {
            return Err(invalid("Job kind is invalid"));
        }
        Ok(Self {
            kind,
            priority,
            generation,
            project_id,
            run_id: None,
            activity_id: None,
        })
    }

    pub fn with_run_id(mut self, run_id: Option<String>) -> Self {
        self.run_id = run_id;
        self
    }

    pub(crate) fn with_activity_id(mut self, activity_id: Option<String>) -> Self {
        self.activity_id = activity_id;
        self
    }
}

struct JobEntry {
    summary: JobSummary,
    activity_id: Option<String>,
    cancel: Arc<AtomicBool>,
    process: Arc<Mutex<Option<u32>>>,
    result: Option<JobResult>,
    committed_asset: Option<(String, u64)>,
}

struct RegistryState {
    entries: HashMap<String, JobEntry>,
    active_background: usize,
    active_foreground: usize,
}

struct RegistryInner {
    state: Mutex<RegistryState>,
    capacity: Condvar,
}

/// Shared job ownership for ingest, render, evidence and export. Background
/// work is capped at two concurrent jobs; foreground work bypasses that cap so
/// seeking/audio stills cannot be starved by a long import.
#[derive(Clone)]
pub struct JobRegistry {
    inner: Arc<RegistryInner>,
}

impl Default for JobRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl JobRegistry {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(RegistryInner {
                state: Mutex::new(RegistryState {
                    entries: HashMap::new(),
                    active_background: 0,
                    active_foreground: 0,
                }),
                capacity: Condvar::new(),
            }),
        }
    }

    pub fn submit<F>(&self, spec: JobSpec, worker: F) -> Result<JobSummary, AppError>
    where
        F: FnOnce(JobContext) -> Result<Value, AppError> + Send + 'static,
    {
        self.submit_with_hooks(spec, None, None, worker)
    }

    pub fn submit_with_hooks<F>(
        &self,
        spec: JobSpec,
        notification_hook: Option<JobNotificationHook>,
        event_hook: Option<JobEventHook>,
        worker: F,
    ) -> Result<JobSummary, AppError>
    where
        F: FnOnce(JobContext) -> Result<Value, AppError> + Send + 'static,
    {
        let job_id = Uuid::new_v4().to_string();
        let created_at_ms = now_ms();
        let summary = JobSummary {
            job_id: job_id.clone(),
            kind: spec.kind,
            priority: spec.priority,
            state: JobState::Queued,
            progress: 0.0,
            generation: spec.generation,
            project_id: spec.project_id,
            run_id: spec.run_id,
            error: None,
            created_at_ms,
        };
        summary.validate()?;
        let cancel = Arc::new(AtomicBool::new(false));
        let process = Arc::new(Mutex::new(None));
        {
            let mut state = self
                .inner
                .state
                .lock()
                .map_err(|_| AppError::io("The job registry lock is unavailable"))?;
            self.prune_locked(&mut state);
            state.entries.insert(
                job_id.clone(),
                JobEntry {
                    summary: summary.clone(),
                    activity_id: spec.activity_id,
                    cancel: cancel.clone(),
                    process: process.clone(),
                    result: None,
                    committed_asset: None,
                },
            );
        }
        let registry = self.clone();
        let priority = summary.priority;
        thread::Builder::new()
            .name(format!("cutterhoochee-job-{job_id}"))
            .spawn(move || {
                registry.run(
                    job_id,
                    priority,
                    cancel,
                    process,
                    notification_hook,
                    event_hook,
                    worker,
                );
            })
            .map_err(|_| AppError::io("The media job could not be started"))?;
        Ok(summary)
    }

    pub fn list(&self) -> Vec<JobSummary> {
        let Ok(state) = self.inner.state.lock() else {
            return Vec::new();
        };
        let mut jobs: Vec<_> = state
            .entries
            .values()
            .map(|entry| entry.summary.clone())
            .collect();
        jobs.sort_by_key(|job| job.created_at_ms);
        jobs
    }

    pub(crate) fn activity_snapshot(
        &self,
        generation: u64,
        project_id: Option<&str>,
    ) -> Result<Vec<ActivityJobSnapshot>, AppError> {
        let state = self
            .inner
            .state
            .lock()
            .map_err(|_| AppError::io("The job registry lock is unavailable"))?;
        Ok(state
            .entries
            .values()
            .filter(|entry| {
                entry.summary.generation == generation
                    && entry.summary.project_id.as_deref() == project_id
            })
            .map(|entry| ActivityJobSnapshot {
                summary: entry.summary.clone(),
                activity_id: entry.activity_id.clone(),
                cancellation_requested: entry.cancel.load(Ordering::Acquire),
                committed_asset: entry.committed_asset.clone(),
            })
            .collect())
    }

    pub fn get(&self, job_id: &str) -> Result<JobSummary, AppError> {
        let state = self
            .inner
            .state
            .lock()
            .map_err(|_| AppError::io("The job registry lock is unavailable"))?;
        state
            .entries
            .get(job_id)
            .map(|entry| entry.summary.clone())
            .ok_or_else(|| AppError::invalid_argument("Unknown job ID"))
    }

    pub fn result(&self, job_id: &str) -> Result<Option<JobResult>, AppError> {
        let state = self
            .inner
            .state
            .lock()
            .map_err(|_| AppError::io("The job registry lock is unavailable"))?;
        Ok(state
            .entries
            .get(job_id)
            .and_then(|entry| entry.result.clone()))
    }

    pub fn cancel(&self, job_id: &str) -> Result<JobSummary, AppError> {
        let (cancel, process, state) = {
            let state = self
                .inner
                .state
                .lock()
                .map_err(|_| AppError::io("The job registry lock is unavailable"))?;
            let entry = state
                .entries
                .get(job_id)
                .ok_or_else(|| AppError::invalid_argument("Unknown job ID"))?;
            (
                entry.cancel.clone(),
                entry.process.clone(),
                entry.summary.state,
            )
        };
        if matches!(
            state,
            JobState::Completed | JobState::Failed | JobState::Cancelled
        ) {
            return self.get(job_id);
        }
        cancel.store(true, Ordering::Release);
        if let Ok(pid) = process.lock().map(|value| *value) {
            if let Some(pid) = pid {
                terminate_process_group(pid);
            }
        }
        self.get(job_id)
    }

    pub fn cancel_run(&self, generation: u64, run_id: &str) -> Result<(), AppError> {
        if run_id.is_empty() {
            return Err(invalid("The assistant run id is invalid"));
        }
        let jobs: Vec<String> = {
            let state = self
                .inner
                .state
                .lock()
                .map_err(|_| AppError::io("The job registry lock is unavailable"))?;
            state
                .entries
                .values()
                .filter(|entry| {
                    entry.summary.generation == generation
                        && entry.summary.run_id.as_deref() == Some(run_id)
                        && matches!(entry.summary.state, JobState::Queued | JobState::Running)
                })
                .map(|entry| entry.summary.job_id.clone())
                .collect()
        };
        for job_id in jobs {
            let _ = self.cancel(&job_id)?;
        }
        Ok(())
    }

    pub fn wait_blocking(&self, job_id: &str, timeout: Duration) -> Result<JobResult, AppError> {
        let start = SystemTime::now();
        loop {
            let summary = self.get(job_id)?;
            match summary.state {
                JobState::Completed => {
                    return self
                        .result(job_id)?
                        .ok_or_else(|| AppError::io("Completed job has no result"))
                }
                JobState::Cancelled => {
                    return Err(AppError::new(
                        ErrorCode::JobCancelled,
                        "The media job was cancelled",
                    ))
                }
                JobState::Failed => {
                    return Err(summary
                        .error
                        .unwrap_or_else(|| AppError::io("The media job failed")))
                }
                JobState::Queued | JobState::Running => {}
            }
            if start.elapsed().unwrap_or_default() >= timeout {
                return Err(AppError::busy("Timed out waiting for the media job"));
            }
            thread::sleep(Duration::from_millis(25));
        }
    }

    pub fn update_progress(&self, job_id: &str, progress: f64) -> Result<(), AppError> {
        let progress = progress.clamp(0.0, 1.0);
        let mut state = self
            .inner
            .state
            .lock()
            .map_err(|_| AppError::io("The job registry lock is unavailable"))?;
        let entry = state
            .entries
            .get_mut(job_id)
            .ok_or_else(|| AppError::invalid_argument("Unknown job ID"))?;
        entry.summary.progress = progress;
        Ok(())
    }

    fn update_progress_with_hook(
        &self,
        job_id: &str,
        progress: f64,
        hook: Option<&JobNotificationHook>,
    ) -> Result<(), AppError> {
        let progress = progress.clamp(0.0, 1.0);
        let summary = {
            let mut state = self
                .inner
                .state
                .lock()
                .map_err(|_| AppError::io("The job registry lock is unavailable"))?;
            let entry = state
                .entries
                .get_mut(job_id)
                .ok_or_else(|| AppError::invalid_argument("Unknown job ID"))?;
            entry.summary.progress = progress;
            entry.summary.clone()
        };
        if let Some(hook) = hook {
            hook(summary, None);
        }
        Ok(())
    }

    fn run<F>(
        &self,
        job_id: String,
        priority: JobPriority,
        cancel: Arc<AtomicBool>,
        process: Arc<Mutex<Option<u32>>>,
        notification_hook: Option<JobNotificationHook>,
        event_hook: Option<JobEventHook>,
        worker: F,
    ) where
        F: FnOnce(JobContext) -> Result<Value, AppError> + Send + 'static,
    {
        if self.wait_for_capacity(priority, &cancel).is_err() {
            self.finish(
                job_id,
                priority,
                &cancel,
                Err(AppError::new(
                    ErrorCode::JobCancelled,
                    "The media job was cancelled",
                )),
                None,
                notification_hook,
            );
            return;
        }
        let running = self.mark_running(&job_id);
        if let (Some(hook), Some(summary)) = (notification_hook.as_ref(), running) {
            hook(summary, None);
        }
        let context = JobContext {
            job_id: job_id.clone(),
            cancel: cancel.clone(),
            process: process.clone(),
            registry: self.clone(),
            notification_hook: notification_hook.clone(),
            event_hook,
        };
        let result = if cancel.load(Ordering::Acquire) {
            Err(AppError::new(
                ErrorCode::JobCancelled,
                "The media job was cancelled",
            ))
        } else {
            worker(context)
        };
        self.finish(
            job_id,
            priority,
            &cancel,
            result,
            Some(process),
            notification_hook,
        );
    }

    fn wait_for_capacity(&self, priority: JobPriority, cancel: &AtomicBool) -> Result<(), ()> {
        let mut state = self.inner.state.lock().map_err(|_| ())?;
        loop {
            if cancel.load(Ordering::Acquire) {
                return Err(());
            }
            if priority == JobPriority::Foreground || state.active_background < 2 {
                match priority {
                    JobPriority::Foreground => state.active_foreground += 1,
                    JobPriority::Background => state.active_background += 1,
                }
                return Ok(());
            }
            state = self
                .inner
                .capacity
                .wait_timeout(state, Duration::from_millis(50))
                .map_err(|_| ())?
                .0;
        }
    }

    fn mark_running(&self, job_id: &str) -> Option<JobSummary> {
        let Ok(mut state) = self.inner.state.lock() else {
            return None;
        };
        let entry = state.entries.get_mut(job_id)?;
        entry.summary.state = JobState::Running;
        Some(entry.summary.clone())
    }

    fn finish(
        &self,
        job_id: String,
        priority: JobPriority,
        cancel: &AtomicBool,
        result: Result<Value, AppError>,
        process: Option<Arc<Mutex<Option<u32>>>>,
        notification_hook: Option<JobNotificationHook>,
    ) {
        if let Some(process) = process {
            if let Ok(mut slot) = process.lock() {
                *slot = None;
            }
        }
        let mut callback: Option<(JobSummary, Option<Value>)> = None;
        if let Ok(mut state) = self.inner.state.lock() {
            match priority {
                JobPriority::Foreground => {
                    state.active_foreground = state.active_foreground.saturating_sub(1)
                }
                JobPriority::Background => {
                    state.active_background = state.active_background.saturating_sub(1)
                }
            }
            if let Some(entry) = state.entries.get_mut(&job_id) {
                match result {
                    Ok(data) if !cancel.load(Ordering::Acquire) => {
                        entry.summary.state = JobState::Completed;
                        entry.summary.progress = 1.0;
                        entry.result = Some(JobResult {
                            job_id: job_id.clone(),
                            data: data.clone(),
                        });
                        callback = Some((entry.summary.clone(), Some(data)));
                    }
                    Err(error)
                        if error.code == ErrorCode::JobCancelled
                            || cancel.load(Ordering::Acquire) =>
                    {
                        entry.summary.state = JobState::Cancelled;
                        entry.summary.error = Some(AppError::new(
                            ErrorCode::JobCancelled,
                            "The media job was cancelled",
                        ));
                        callback = Some((entry.summary.clone(), None));
                    }
                    Err(error) => {
                        entry.summary.state = JobState::Failed;
                        entry.summary.error = Some(error);
                        callback = Some((entry.summary.clone(), None));
                    }
                    Ok(_) => {
                        entry.summary.state = JobState::Cancelled;
                        entry.summary.error = Some(AppError::new(
                            ErrorCode::JobCancelled,
                            "The media job was cancelled",
                        ));
                        callback = Some((entry.summary.clone(), None));
                    }
                }
            }
            self.prune_locked(&mut state);
        }
        if let (Some(hook), Some((summary, data))) = (notification_hook, callback) {
            hook(summary, data);
        }
        self.inner.capacity.notify_all();
    }

    fn prune_locked(&self, state: &mut RegistryState) {
        if state.entries.len() <= MAX_RETAINED_JOBS {
            return;
        }
        let mut finished: Vec<(String, u64)> = state
            .entries
            .iter()
            .filter(|(_, entry)| {
                matches!(
                    entry.summary.state,
                    JobState::Completed | JobState::Failed | JobState::Cancelled
                )
            })
            .map(|(id, entry)| (id.clone(), entry.summary.created_at_ms))
            .collect();
        finished.sort_by_key(|(_, timestamp)| *timestamp);
        while state.entries.len() > MAX_RETAINED_JOBS {
            let Some((id, _)) = finished.first().cloned() else {
                break;
            };
            finished.remove(0);
            state.entries.remove(&id);
        }
    }
}

pub struct JobContext {
    job_id: String,
    cancel: Arc<AtomicBool>,
    process: Arc<Mutex<Option<u32>>>,
    registry: JobRegistry,
    notification_hook: Option<JobNotificationHook>,
    event_hook: Option<JobEventHook>,
}

impl JobContext {
    pub fn job_id(&self) -> &str {
        &self.job_id
    }

    pub(crate) fn record_asset_commit(
        &self,
        asset_id: String,
        revision: u64,
    ) -> Result<(), AppError> {
        let summary = {
            let mut state = self
                .registry
                .inner
                .state
                .lock()
                .map_err(|_| AppError::io("The job registry lock is unavailable"))?;
            let entry = state
                .entries
                .get_mut(&self.job_id)
                .ok_or_else(|| invalid("Unknown job ID"))?;
            entry.committed_asset = Some((asset_id, revision));
            entry.summary.clone()
        };
        if let Some(hook) = self.notification_hook.as_ref() {
            hook(summary, None);
        }
        Ok(())
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancel.load(Ordering::Acquire)
    }

    pub fn check_cancelled(&self) -> Result<(), AppError> {
        if self.is_cancelled() {
            Err(AppError::new(
                ErrorCode::JobCancelled,
                "The media job was cancelled",
            ))
        } else {
            Ok(())
        }
    }

    pub fn progress(&self, value: f64) -> Result<(), AppError> {
        self.registry.update_progress_with_hook(
            &self.job_id,
            value,
            self.notification_hook.as_ref(),
        )
    }

    pub fn event(&self, kind: impl Into<String>, data: Value) {
        if let Some(hook) = self.event_hook.as_ref() {
            hook(kind.into(), data);
        }
    }
    /// Run a native process in its own process group. stdout/stderr are read on
    /// bounded worker threads, and cancellation terminates the entire group.
    pub fn run_command(&self, command: Command) -> Result<Output, AppError> {
        self.run_command_inner(command, None, None)
    }

    pub fn run_command_with_progress(
        &self,
        command: Command,
        expected_duration_ms: Option<u64>,
    ) -> Result<Output, AppError> {
        self.run_command_inner(command, expected_duration_ms, None)
    }

    /// Run a native process while streaming a bounded producer into stdin.
    ///
    /// The producer owns the pipe until it returns, then stdin is closed so
    /// consumers such as ffmpeg can finalize their output. The child remains
    /// in the job's process group and is cancelled exactly like other media
    /// commands.
    pub fn run_command_with_stdin_progress<F>(
        &self,
        command: Command,
        expected_duration_ms: Option<u64>,
        producer: F,
    ) -> Result<Output, AppError>
    where
        F: FnOnce(&mut ChildStdin) -> Result<(), AppError> + Send + 'static,
    {
        self.run_command_inner(command, expected_duration_ms, Some(Box::new(producer)))
    }

    fn run_command_inner(
        &self,
        mut command: Command,
        expected_duration_ms: Option<u64>,
        producer: Option<Box<dyn FnOnce(&mut ChildStdin) -> Result<(), AppError> + Send>>,
    ) -> Result<Output, AppError> {
        self.check_cancelled()?;
        command
            .stdin(if producer.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            unsafe {
                command.pre_exec(|| {
                    libc::setpgid(0, 0);
                    Ok(())
                });
            }
        }
        let mut child = command
            .spawn()
            .map_err(|_| AppError::io("The media process could not be started"))?;
        let pid = child.id();
        if let Ok(mut slot) = self.process.lock() {
            *slot = Some(pid);
        }
        let input_writer = producer.map(|producer| {
            let stdin = child
                .stdin
                .take()
                .ok_or_else(|| AppError::io("The media process stdin was not available"));
            thread::spawn(move || match stdin {
                Ok(mut stdin) => producer(&mut stdin),
                Err(error) => Err(error),
            })
        });
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let registry = self.registry.clone();
        let job_id = self.job_id.clone();
        let out_reader = thread::spawn(move || {
            read_bounded_progress(stdout, registry, job_id, expected_duration_ms)
        });
        let err_reader = thread::spawn(move || read_bounded(stderr));
        loop {
            if self.is_cancelled() {
                terminate_process_group(pid);
            }
            match child.try_wait() {
                Ok(Some(status)) => {
                    let input_error = input_writer
                        .map(|writer| {
                            writer
                                .join()
                                .unwrap_or_else(|_| Err(AppError::io("Media stdin writer failed")))
                        })
                        .and_then(Result::err);
                    let stdout = out_reader
                        .join()
                        .unwrap_or_else(|_| Err(AppError::io("Media stdout reader failed")))?;
                    let stderr = err_reader
                        .join()
                        .unwrap_or_else(|_| Err(AppError::io("Media stderr reader failed")))?;
                    if !status.success() {
                        let detail = String::from_utf8_lossy(&stderr);
                        let detail = detail.lines().last().unwrap_or("media process failed");
                        return Err(AppError::new(
                            ErrorCode::MediaUnsupported,
                            format!("Media process failed: {detail}"),
                        ));
                    }
                    if let Some(error) = input_error {
                        return Err(error);
                    }
                    return Ok(Output {
                        status,
                        stdout,
                        stderr,
                    });
                }
                Ok(None) => thread::sleep(Duration::from_millis(25)),
                Err(_) => {
                    terminate_process_group(pid);
                    return Err(AppError::io("The media process status could not be read"));
                }
            }
        }
    }
}

fn read_bounded(mut reader: Option<impl Read>) -> Result<Vec<u8>, AppError> {
    let Some(mut reader) = reader else {
        return Ok(Vec::new());
    };
    let mut bytes = Vec::new();
    let mut buffer = [0u8; 64 * 1024];
    let mut overflow = false;
    loop {
        let count = reader.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        if !overflow && bytes.len().saturating_add(count) <= MAX_JOB_OUTPUT_BYTES {
            bytes.extend_from_slice(&buffer[..count]);
        } else {
            // Keep draining after the cap so the child never blocks on a
            // full pipe; report the bounded-output error only at EOF.
            overflow = true;
        }
    }
    if overflow {
        return Err(AppError::io(
            "Media process output exceeded the supported limit",
        ));
    }
    Ok(bytes)
}

fn read_bounded_progress(
    mut reader: Option<impl Read>,
    registry: JobRegistry,
    job_id: String,
    expected_duration_ms: Option<u64>,
) -> Result<Vec<u8>, AppError> {
    let Some(mut reader) = reader else {
        return Ok(Vec::new());
    };
    let mut bytes = Vec::new();
    let mut buffer = [0u8; 64 * 1024];
    let mut pending = String::new();
    let mut overflow = false;
    loop {
        let count = reader.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        if !overflow && bytes.len().saturating_add(count) <= MAX_JOB_OUTPUT_BYTES {
            bytes.extend_from_slice(&buffer[..count]);
        } else {
            overflow = true;
        }
        pending.push_str(&String::from_utf8_lossy(&buffer[..count]));
        while let Some(index) = pending.find('\n') {
            let line = pending[..index].trim().to_owned();
            pending.drain(..=index);
            let Some(value) = line
                .strip_prefix("out_time_ms=")
                .and_then(|value| value.parse::<f64>().ok())
            else {
                continue;
            };
            let Some(duration_ms) = expected_duration_ms.filter(|duration| *duration > 0) else {
                continue;
            };
            let progress = (value / (duration_ms as f64 * 1000.0)).clamp(0.0, 0.98);
            let _ = registry.update_progress(&job_id, progress);
        }
    }
    if overflow {
        return Err(AppError::io(
            "Media process output exceeded the supported limit",
        ));
    }
    Ok(bytes)
}

fn terminate_process_group(pid: u32) {
    #[cfg(unix)]
    {
        let pid = pid as libc::pid_t;
        unsafe {
            libc::kill(-pid, libc::SIGTERM);
        }
        thread::sleep(Duration::from_millis(100));
        unsafe {
            libc::kill(-pid, libc::SIGKILL);
        }
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}
