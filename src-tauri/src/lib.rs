pub mod activity;
pub mod agent_bridge;
pub mod assistant;
pub mod credentials;
pub mod editor;
pub mod error;
mod gtk_runtime;
pub mod ipc;
pub mod media;
pub mod permissions;
pub mod project;
pub mod state;

use crate::editor::dispatcher::{dispatch, CallerContext};
use crate::error::AppError;
use crate::ipc::{EditorReply, EditorRequest};
use crate::media::render::SoftwarePreviewPacket;
use crate::media::MediaAction;
use crate::permissions::FileGrantPurpose;
use crate::state::AppState;
use serde_json::json;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tauri::http::{header, HeaderValue, Method, Request, Response, StatusCode};
use tauri::ipc::{Channel, InvokeResponseBody, Response as IpcResponse};
use tauri::Manager;
use tauri::{DragDropEvent, WindowEvent};
const MAX_RANGE_BYTES: u64 = 1024 * 1024;
const MAX_FULL_RESPONSE_BYTES: u64 = 8 * 1024 * 1024;
const MAX_SOFTWARE_HEADER_BYTES: usize = 4096;
const MAX_SOFTWARE_PAYLOAD_BYTES: usize = 4 * 1024 * 1024;
pub fn run() {
    // This must happen before Builder::build initializes GTK or any image-loading
    // thread can cause Glycin to cache the host configuration.
    let gtk_runtime =
        gtk_runtime::prepare().expect("error while preparing the packaged GTK/Glycin runtime");
    let gtk_runtime = Arc::new(Mutex::new(Some(gtk_runtime)));
    let setup_runtime = Arc::clone(&gtk_runtime);
    let app_result = tauri::Builder::default()
        .plugin(
            tauri_plugin_opener::Builder::new()
                .open_js_links_on_click(false)
                .build(),
        )
        .on_window_event(|window, event| {
            if let WindowEvent::DragDrop(DragDropEvent::Drop { paths, .. }) = event {
                let paths = paths.clone();
                let label = window.label().to_owned();
                let state = window
                    .app_handle()
                    .try_state::<AppState>()
                    .map(|state| state.inner().clone());
                if let Some(state) = state {
                    tauri::async_runtime::spawn(handle_native_drop(label, paths, state));
                }
            }
        })
        .register_uri_scheme_protocol("artifact", |_ctx, request| {
            artifact_protocol_response(_ctx, request)
        })
        .setup(move |app| {
            let result = setup_app(app);
            if result.is_err() {
                cleanup_gtk_runtime(&setup_runtime);
            }
            result
        })
        .invoke_handler(tauri::generate_handler![
            editor::dispatcher::editor_call,
            artifact_url,
            artifact_read,
            preview_software_subscribe,
            preview_software_ack,
            preview_software_cancel,
            agent_activity_snapshot,
            preview_transport_complete,
        ])
        .build(tauri::generate_context!());
    let app = app_result.unwrap_or_else(|error| {
        cleanup_gtk_runtime(&gtk_runtime);
        panic!("error while building Cutterhoochee: {error}");
    });

    app.run(move |_app_handle, event| {
        // ExitRequested is cancellable and can be followed by more GTK work.
        // Cleanup belongs to the final, non-cancellable Exit event.
        if matches!(event, tauri::RunEvent::Exit) {
            cleanup_gtk_runtime(&gtk_runtime);
        }
    });
}

fn setup_app(app: &mut tauri::App) -> Result<(), Box<dyn std::error::Error>> {
    let paths = agent_bridge::AgentPaths::from_app(app.handle())?;
    let state = AppState::new_with_handle(paths, Some(app.handle().clone()))?;
    app.manage(state.clone());

    // Sidecar startup is supervised and never blocks the UI. A missing
    // packaged resource leaves manual editing available and is exposed
    // as a restartable assistant error instead of a fake success.
    let bridge_state = state.clone();
    tauri::async_runtime::spawn(async move {
        if let Err(error) = bridge_state.ensure_agent_bridge().await {
            eprintln!("[cutterhoochee-agent] {}", error.message);
        }
    });
    Ok(())
}

fn cleanup_gtk_runtime(runtime: &Arc<Mutex<Option<gtk_runtime::RuntimeGuard>>>) {
    if let Ok(mut runtime) = runtime.lock() {
        runtime.take();
    }
}

#[tauri::command]
fn agent_activity_snapshot(
    window: tauri::WebviewWindow,
    state: tauri::State<'_, AppState>,
    generation: u64,
    project_id: Option<String>,
) -> Result<Vec<activity::AgentActivity>, AppError> {
    if window.label() != "main" {
        return Err(AppError::invalid_argument(
            "Activity is only available in the editor window",
        ));
    }
    state.activity_snapshot_at(generation, project_id.as_deref())
}

#[tauri::command]
fn preview_transport_complete(
    window: tauri::WebviewWindow,
    state: tauri::State<'_, AppState>,
    completion: media::render::PreviewTransportCompletion,
) -> Result<(), AppError> {
    if window.label() != "main" {
        return Err(AppError::invalid_argument(
            "Only the editor window can acknowledge playback",
        ));
    }
    state.render().complete_transport(completion, &state)
}

async fn handle_native_drop(label: String, paths: Vec<PathBuf>, state: AppState) {
    if paths.is_empty() {
        return;
    }
    let arrival_generation = state.generation();
    let arrival_project = state.current_project_id();
    let caller =
        CallerContext::human_window(label.clone(), arrival_generation, arrival_project.clone());
    let (generation, project_id, caller) = if arrival_project.is_some() {
        (arrival_generation, arrival_project, caller)
    } else {
        let reply = dispatch(
            EditorRequest::ProjectCreate {
                name: "Untitled".to_owned(),
                aspect: None,
                fps_num: None,
                fps_den: None,
            },
            caller,
            &state,
        )
        .await;
        let status = match reply {
            Ok(EditorReply::ProjectStatus(status))
                if status.open && status.project_id.is_some() =>
            {
                status
            }
            Ok(_) => return,
            Err(error) => {
                let _ = state.emit_sanitized_event(
                    "media_drop_failed",
                    None,
                    Some(json!({"code": error.code, "message": error.message})),
                );
                return;
            }
        };
        let project_id = status.project_id.clone();
        let generation = status.generation;
        if state.validate_generation(generation).is_err()
            || state.current_project_id() != project_id
        {
            let _ = state.emit_sanitized_event(
                "media_drop_failed",
                None,
                Some(json!({"code": "STALE_SESSION", "message": "The project changed before the dropped files could be approved"})),
            );
            return;
        }
        (
            generation,
            project_id.clone(),
            CallerContext::human_window(label.clone(), generation, project_id),
        )
    };
    if state.validate_generation(generation).is_err() || state.current_project_id() != project_id {
        let _ = state.emit_sanitized_event(
            "media_drop_failed",
            None,
            Some(json!({"code": "STALE_SESSION", "message": "The project changed before the dropped files could be approved"})),
        );
        return;
    }
    let scope = match state.permissions().scope_for_state(&state) {
        Ok(scope) if scope.generation == generation && scope.project_id == project_id => scope,
        Ok(_) => {
            let _ = state.emit_sanitized_event(
                "media_drop_failed",
                None,
                Some(json!({"code": "STALE_SESSION", "message": "The project approval scope changed"})),
            );
            return;
        }
        Err(error) => {
            let _ = state.emit_sanitized_event(
                "media_drop_failed",
                None,
                Some(json!({"code": error.code, "message": error.message})),
            );
            return;
        }
    };
    let grants = match state.permissions().grant_native_drop_files(
        &caller,
        scope,
        paths,
        FileGrantPurpose::Import,
    ) {
        Ok(grants) => grants,
        Err(error) => {
            let _ = state.emit_sanitized_event(
                "media_drop_failed",
                None,
                Some(json!({"code": error.code, "message": error.message})),
            );
            return;
        }
    };
    if state.validate_generation(generation).is_err() || state.current_project_id() != project_id {
        let _ = state.emit_sanitized_event(
            "media_drop_failed",
            None,
            Some(json!({"code": "STALE_SESSION", "message": "The project changed after dropped-file approval"})),
        );
        return;
    }
    let paths = grants.into_iter().map(|grant| grant.path).collect();
    let request = EditorRequest::Media(MediaAction::Import { paths: Some(paths) });
    if let Err(error) = dispatch(request, caller, &state).await {
        let _ = state.emit_sanitized_event(
            "media_drop_failed",
            None,
            Some(json!({"code": error.code, "message": error.message})),
        );
    }
}

/// Return a revocable opaque URL for a currently managed artifact. The URL
/// contains no filesystem path; the protocol handler revalidates its identity
/// against the current project and generation for every request.
#[tauri::command]
fn artifact_url(
    state: tauri::State<'_, AppState>,
    artifact_id: String,
    generation: u64,
) -> Result<String, AppError> {
    state.artifact_url_at(generation, &artifact_id)
}

/// Read a bounded artifact range for transports that cannot use a media URL.
/// The native store validates containment and the current generation before
/// opening any file.
#[tauri::command]
fn artifact_read(
    state: tauri::State<'_, AppState>,
    artifact_id: String,
    generation: u64,
    offset: Option<u64>,
    length: Option<u64>,
) -> Result<tauri::ipc::Response, AppError> {
    let workspace_id = state
        .current_workspace_id()
        .ok_or_else(|| AppError::invalid_argument("No project is open"))?;
    let (bytes, _, _) =
        state.read_artifact_at(generation, &workspace_id, &artifact_id, offset, length)?;
    Ok(tauri::ipc::Response::new(bytes))
}

/// Subscribe one WebView to a bounded software-preview stream. FFmpeg stays in
/// the native worker; only validated binary JPEG packets cross this channel.
#[tauri::command]
async fn preview_software_subscribe(
    state: tauri::State<'_, AppState>,
    channel: Channel<IpcResponse>,
    project_id: String,
    generation: u64,
    revision: u64,
    plan_hash: String,
    start_frame: u64,
) -> Result<(), AppError> {
    state.validate_generation(generation)?;
    let snapshot = state.snapshot_at(generation)?;
    if snapshot.document.project_id != project_id {
        return Err(AppError::stale_session(
            "The software preview belongs to a different project",
        ));
    }
    let capture = state.render().capture(&state, generation, revision)?;
    if capture.plan.project_id != project_id || capture.plan.plan_hash != plan_hash {
        return Err(AppError::stale_session(
            "The software preview plan is no longer current",
        ));
    }
    let receiver = state
        .render()
        .start_software_preview(capture.plan, generation, start_frame)?;
    let render = state.render().clone();
    tauri::async_runtime::spawn_blocking(move || {
        loop {
            match receiver.recv_timeout(Duration::from_millis(100)) {
                Ok(packet) => {
                    let Ok(payload) = encode_software_packet(packet) else {
                        break;
                    };
                    if channel
                        .send(IpcResponse::new(InvokeResponseBody::Raw(payload)))
                        .is_err()
                    {
                        break;
                    }
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
        let _ = render.cancel_software_preview(generation, &project_id, revision, &plan_hash);
        Ok::<(), AppError>(())
    })
    .await
    .map_err(|_| AppError::io("The software preview channel worker stopped unexpectedly"))??;
    Ok(())
}

#[tauri::command]
fn preview_software_ack(
    state: tauri::State<'_, AppState>,
    project_id: String,
    generation: u64,
    revision: u64,
    plan_hash: String,
    sequence: u64,
) -> Result<(), AppError> {
    state
        .render()
        .ack_software_preview(generation, &project_id, revision, &plan_hash, sequence)
}

#[tauri::command]
fn preview_software_cancel(
    state: tauri::State<'_, AppState>,
    project_id: String,
    generation: u64,
    revision: u64,
    plan_hash: String,
) -> Result<(), AppError> {
    state
        .render()
        .cancel_software_preview(generation, &project_id, revision, &plan_hash)
}

fn encode_software_packet(packet: SoftwarePreviewPacket) -> Result<Vec<u8>, AppError> {
    if packet.data.len() > MAX_SOFTWARE_PAYLOAD_BYTES {
        return Err(AppError::invalid_argument(
            "The software preview payload exceeds the supported limit",
        ));
    }
    let metadata = serde_json::to_vec(&serde_json::json!({
        "generation": packet.generation,
        "projectId": packet.project_id,
        "revision": packet.revision,
        "planHash": packet.plan_hash,
        "frame": packet.frame,
        "sequence": packet.sequence,
        "width": packet.width,
        "height": packet.height,
        "contentType": packet.content_type,
    }))
    .map_err(|_| AppError::schema("The software preview metadata could not be encoded"))?;
    if metadata.is_empty() || metadata.len() > MAX_SOFTWARE_HEADER_BYTES {
        return Err(AppError::schema(
            "The software preview metadata exceeds the supported limit",
        ));
    }
    let header_len = u32::try_from(metadata.len())
        .map_err(|_| AppError::schema("The software preview metadata length is invalid"))?;
    let mut encoded = Vec::with_capacity(4 + metadata.len() + packet.data.len());
    encoded.extend_from_slice(&header_len.to_le_bytes());
    encoded.extend_from_slice(&metadata);
    encoded.extend_from_slice(&packet.data);
    Ok(encoded)
}
fn artifact_protocol_response<R: tauri::Runtime>(
    context: tauri::UriSchemeContext<'_, R>,
    request: Request<Vec<u8>>,
) -> Response<Vec<u8>> {
    if request.method() == Method::OPTIONS {
        return response(StatusCode::NO_CONTENT, Vec::new(), None, None, None);
    }
    if request.method() != Method::GET && request.method() != Method::HEAD {
        return response(StatusCode::METHOD_NOT_ALLOWED, Vec::new(), None, None, None);
    }
    let Some((workspace_id, generation, artifact_id)) = parse_artifact_path(request.uri().path())
    else {
        return response(StatusCode::NOT_FOUND, Vec::new(), None, None, None);
    };
    let Some(state) = context.app_handle().try_state::<AppState>() else {
        return response(StatusCode::NOT_FOUND, Vec::new(), None, None, None);
    };
    let Ok((total, extension)) = state.artifact_metadata_at(generation, workspace_id, artifact_id)
    else {
        return response(StatusCode::NOT_FOUND, Vec::new(), None, None, None);
    };
    let content_type = content_type_for_extension(extension.as_str());
    let range_header = request
        .headers()
        .get(header::RANGE)
        .and_then(|value| value.to_str().ok());
    let range = match range_header {
        None => None,
        Some(value) => match parse_range(value, total) {
            Ok(range) => Some(range),
            Err(()) => {
                return response(
                    StatusCode::RANGE_NOT_SATISFIABLE,
                    Vec::new(),
                    Some(content_type),
                    Some(total),
                    Some(format!("bytes */{total}")),
                )
            }
        },
    };
    let (start, length, status) = match range {
        Some((start, length)) => (start, length, StatusCode::PARTIAL_CONTENT),
        None => {
            if request.method() == Method::GET && total > MAX_FULL_RESPONSE_BYTES {
                return response(
                    StatusCode::RANGE_NOT_SATISFIABLE,
                    Vec::new(),
                    Some(content_type),
                    Some(total),
                    Some(format!("bytes */{total}")),
                );
            }
            (0, total, StatusCode::OK)
        }
    };
    if request.method() == Method::HEAD {
        return response(
            status,
            Vec::new(),
            Some(content_type),
            Some(length),
            range.map(|(offset, count)| {
                format!(
                    "bytes {}-{}/{}",
                    offset,
                    offset.saturating_add(count).saturating_sub(1),
                    total
                )
            }),
        );
    }
    let Ok((bytes, _, _)) = state.read_artifact_at(
        generation,
        workspace_id,
        artifact_id,
        Some(start),
        Some(length),
    ) else {
        return response(StatusCode::NOT_FOUND, Vec::new(), None, None, None);
    };
    response(
        status,
        bytes,
        Some(content_type),
        Some(length),
        range.map(|(offset, count)| {
            format!(
                "bytes {}-{}/{}",
                offset,
                offset.saturating_add(count).saturating_sub(1),
                total
            )
        }),
    )
}

fn parse_artifact_path(path: &str) -> Option<(&str, u64, &str)> {
    let mut parts = path.split('/');
    let empty = parts.next()?;
    let workspace = parts.next()?;
    let generation = parts.next()?.parse::<u64>().ok()?;
    let artifact = parts.next()?;
    if empty != ""
        || workspace.is_empty()
        || artifact.is_empty()
        || parts.next().is_some()
        || workspace.contains('%')
        || artifact.contains('%')
        || workspace.contains('.')
        || artifact.contains("..")
    {
        return None;
    }
    Some((workspace, generation, artifact))
}

fn parse_range(value: &str, total: u64) -> Result<(u64, u64), ()> {
    let value = value.strip_prefix("bytes=").ok_or(())?;
    if value.contains(',') {
        return Err(());
    }
    let (start, end) = value.split_once('-').ok_or(())?;
    if start.is_empty() {
        let length = end.parse::<u64>().map_err(|_| ())?;
        if length == 0 {
            return Err(());
        }
        let length = length.min(total).min(MAX_RANGE_BYTES);
        return Ok((total.saturating_sub(length), length));
    }
    let start = start.parse::<u64>().map_err(|_| ())?;
    if start >= total {
        return Err(());
    }
    let requested_end = if end.is_empty() {
        total.saturating_sub(1)
    } else {
        end.parse::<u64>()
            .map_err(|_| ())?
            .min(total.saturating_sub(1))
    };
    let end = requested_end.min(start.saturating_add(MAX_RANGE_BYTES.saturating_sub(1)));
    if end < start {
        return Err(());
    }
    let length = end.saturating_sub(start).saturating_add(1);
    Ok((start, length))
}

fn content_type_for_extension(extension: &str) -> &'static str {
    match extension {
        "mp4" | "m4v" => "video/mp4",
        "webm" => "video/webm",
        "jpg" | "jpeg" => "image/jpeg",
        "png" => "image/png",
        "webp" => "image/webp",
        "wav" => "audio/wav",
        "flac" => "audio/flac",
        "mp3" => "audio/mpeg",
        "json" => "application/json",
        _ => "application/octet-stream",
    }
}

fn response(
    status: StatusCode,
    body: Vec<u8>,
    content_type: Option<&'static str>,
    content_length: Option<u64>,
    content_range: Option<String>,
) -> Response<Vec<u8>> {
    let mut response = Response::new(body);
    *response.status_mut() = status;
    let headers = response.headers_mut();
    headers.insert(
        header::ACCESS_CONTROL_ALLOW_ORIGIN,
        HeaderValue::from_static("*"),
    );
    headers.insert(
        header::ACCESS_CONTROL_ALLOW_METHODS,
        HeaderValue::from_static("GET, HEAD, OPTIONS"),
    );
    headers.insert(
        header::ACCESS_CONTROL_ALLOW_HEADERS,
        HeaderValue::from_static("Range, Content-Type"),
    );
    headers.insert(
        header::ACCESS_CONTROL_EXPOSE_HEADERS,
        HeaderValue::from_static("Accept-Ranges, Content-Length, Content-Range, Content-Type"),
    );
    headers.insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    if let Some(content_type) = content_type {
        headers.insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
    }
    if let Some(content_length) = content_length {
        if let Ok(value) = HeaderValue::try_from(content_length.to_string()) {
            headers.insert(header::CONTENT_LENGTH, value);
        }
    }
    if let Some(content_range) = content_range {
        if let Ok(value) = HeaderValue::try_from(content_range) {
            headers.insert(header::CONTENT_RANGE, value);
        }
    }
    response
}
