use crate::error::{Report, Result};
use crate::helper_ws::{
    ArtifactBegin, ArtifactChunk, ArtifactCommitted, ArtifactFinish, HelperTaskStatusSnapshot,
    HelperToServerMessage, OfficialMcpRequest, ServerToHelperMessage,
    MAX_ARTIFACT_CHUNK_MESSAGE_BYTES, OFFICIAL_MCP_ADAPTER_CAPABILITY,
    OFFICIAL_MCP_STORE_IMAGE_BASE64_CAPABILITY,
};
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::{extract::State, Json};
use base64::Engine as _;
use color_eyre::eyre::{eyre, Error, OptionExt};
use futures_util::{SinkExt, StreamExt};
use rmcp::{
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{
        CallToolResult, Content, Implementation, ProtocolVersion, ServerCapabilities, ServerInfo,
    },
    schemars, tool, tool_handler, tool_router, ErrorData, ServerHandler,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{HashMap, VecDeque};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::sync::oneshot::Receiver;
use tokio::sync::{mpsc, watch};
use uuid::Uuid;

pub const STUDIO_PLUGIN_PORT: u16 = 44755;
#[path = "rbx_studio_server/hub_status.rs"]
mod hub_status;
#[path = "rbx_studio_server/state.rs"]
mod state;
use hub_status::*;
pub use state::*;

struct ActiveHelperConnection {
    connection_id: Uuid,
    helper_id: String,
    place_id: String,
    task_id: Option<String>,
    capabilities: Vec<String>,
    sender: mpsc::UnboundedSender<OutgoingHelperFrame>,
    last_message_at: Instant,
    task_status: Option<HelperTaskStatusSnapshot>,
}

fn ensure_reported_age_fresh(
    action: &str,
    label: &str,
    age_ms: Option<u128>,
) -> Result<(), ErrorData> {
    let Some(age_ms) = age_ms else {
        return Err(ErrorData::internal_error(
            format!("{action} failed fast: {label}_snapshot_age_unavailable"),
            None,
        ));
    };
    if age_ms > HELPER_TASK_STATUS_STALE_AFTER.as_millis() {
        return Err(ErrorData::internal_error(
            format!("{action} failed fast: {label}_snapshot_stale age_ms={age_ms}"),
            None,
        ));
    }
    Ok(())
}

fn snapshot_from_hub_status_payload(
    payload: HubStatusPayload,
    task_id: &str,
) -> Result<HubTaskRouteSnapshot> {
    let task = payload
        .tasks
        .into_iter()
        .find(|task| task.task_id == task_id)
        .ok_or_else(|| eyre!("task not found in hub status: {task_id}"))?;
    let claimed_by_helper_id = task.claimed_by_helper_id.clone();
    let mut helper_match = None;
    let mut active_task_match = None;
    if let Some(claimed_helper_id) = claimed_by_helper_id.as_deref() {
        helper_match = payload
            .helpers
            .into_iter()
            .find(|helper| helper.helper_id == claimed_helper_id);
        if let Some(helper) = helper_match.as_ref() {
            active_task_match = helper
                .active_tasks
                .iter()
                .find(|active_task| active_task.task_id == task_id)
                .cloned();
        }
    }
    Ok(HubTaskRouteSnapshot {
        claimed_by_helper_id,
        helper_id: helper_match.as_ref().map(|helper| helper.helper_id.clone()),
        helper_blocked: helper_match
            .as_ref()
            .map(|helper| helper.blocked)
            .unwrap_or(false),
        helper_last_seen_age_ms: helper_match
            .as_ref()
            .and_then(|helper| helper.last_seen_age_ms),
        task_released: task.released,
        task_service_state: task.service_state,
        task_accepting_launches: task.accepting_launches,
        task_services_healthy: task_reports_all_services_healthy(&task.services),
        remote_state: active_task_match
            .as_ref()
            .map(|active_task| active_task.remote_state.clone()),
    })
}

fn task_reports_all_services_healthy(services: &HashMap<String, String>) -> bool {
    REQUIRED_TASK_SERVICE_NAMES
        .iter()
        .all(|name| services.get(*name).map(String::as_str) == Some("healthy"))
}

async fn require_hub_route_ready_snapshot(
    hub_client: Option<HubStatusClient>,
    task_id: &str,
    action: &str,
) -> Result<HubTaskRouteSnapshot, ErrorData> {
    let Some(hub_client) = hub_client else {
        return Err(ErrorData::internal_error(
            format!("{action} failed fast: hub_snapshot_not_configured"),
            None,
        ));
    };
    let snapshot = hub_client
        .fetch_task_route_snapshot(task_id)
        .await
        .map_err(|error| {
            ErrorData::internal_error(
                format!("{action} failed fast: hub_snapshot_unavailable error={error}"),
                None,
            )
        })?;
    if snapshot.task_released || snapshot.task_service_state == "expired" {
        return Err(ErrorData::internal_error(
            format!(
                "{action} failed fast: hub_task_not_active state={} released={}",
                snapshot.task_service_state, snapshot.task_released
            ),
            None,
        ));
    }
    if !snapshot.task_services_ready() {
        return Err(ErrorData::internal_error(
            format!(
                "{action} failed fast: hub_task_services_not_ready state={} accepting_launches={} services_healthy={} released={}",
                snapshot.task_service_state,
                snapshot.task_accepting_launches,
                snapshot.task_services_healthy,
                snapshot.task_released
            ),
            None,
        ));
    }
    let Some(claimed_helper_id) = snapshot.claimed_by_helper_id.as_deref() else {
        return Err(ErrorData::internal_error(
            format!("{action} failed fast: hub_task_unclaimed"),
            None,
        ));
    };
    if snapshot.helper_id.as_deref() != Some(claimed_helper_id) {
        return Err(ErrorData::internal_error(
            format!(
                "{action} failed fast: hub_claimed_helper_not_online claimed_helper_id={claimed_helper_id}"
            ),
            None,
        ));
    }
    if snapshot.helper_blocked {
        return Err(ErrorData::internal_error(
            format!("{action} failed fast: hub_helper_blocked helper_id={claimed_helper_id}"),
            None,
        ));
    }
    if snapshot.helper_last_seen_age_ms.unwrap_or(u128::MAX)
        > HELPER_TASK_STATUS_STALE_AFTER.as_millis()
    {
        return Err(ErrorData::internal_error(
            format!(
                "{action} failed fast: hub_helper_snapshot_stale age_ms={}",
                snapshot.helper_last_seen_age_ms.unwrap_or(u128::MAX)
            ),
            None,
        ));
    }
    if !matches!(
        snapshot.remote_state.as_deref(),
        Some("connected" | "ready")
    ) {
        return Err(ErrorData::internal_error(
            format!(
                "{action} failed fast: hub_helper_remote_not_ready state={}",
                snapshot.remote_state.as_deref().unwrap_or("unknown")
            ),
            None,
        ));
    }
    Ok(snapshot)
}

fn studio_mode_is_running(mode: Option<&str>) -> bool {
    matches!(mode, Some("start_play") | Some("run_server"))
}

fn studio_transition_is_stopping(phase: Option<&str>) -> bool {
    matches!(phase, Some("stopping_requested" | "stopping_observed"))
}

#[derive(Clone, Copy)]
enum LocalStudioGateKind {
    Edit,
    Launch,
    StopControl,
    Official,
}

fn local_gate_requires_hub_route_ready(kind: LocalStudioGateKind) -> bool {
    !matches!(kind, LocalStudioGateKind::StopControl)
}

fn opt_age_to_u128(value: Option<u64>) -> Option<u128> {
    value.map(u128::from)
}

fn local_studio_live_snapshot_locked(
    state: &AppState,
    requested_task_id: &str,
    action: &str,
) -> Result<LocalStudioLiveSnapshot, ErrorData> {
    let Some(helper) = state.active_helper.as_ref() else {
        return Err(ErrorData::internal_error(
            format!("{action} failed fast: local_helper_missing"),
            None,
        ));
    };
    let Some(helper_task_id) = helper.task_id.as_deref() else {
        return Err(ErrorData::internal_error(
            format!("{action} failed fast: local_helper_task_id_missing"),
            None,
        ));
    };
    if helper_task_id != requested_task_id {
        return Err(ErrorData::internal_error(
            format!(
                "{action} failed fast: local_helper_task_id_mismatch active_helper_task_id={helper_task_id}, requested_task_id={requested_task_id}"
            ),
            None,
        ));
    }
    let helper_last_message_age_ms = helper.last_message_at.elapsed().as_millis();
    if helper_last_message_age_ms > HELPER_HEARTBEAT_TIMEOUT.as_millis() {
        return Err(ErrorData::internal_error(
            format!("{action} failed fast: local_helper_stale age_ms={helper_last_message_age_ms}"),
            None,
        ));
    }
    let Some(status) = helper.task_status.as_ref() else {
        return Err(ErrorData::internal_error(
            format!("{action} failed fast: local_task_status_missing"),
            None,
        ));
    };
    if status.task_id != requested_task_id {
        return Err(ErrorData::internal_error(
            format!(
                "{action} failed fast: local_task_status_task_id_mismatch status_task_id={}, requested_task_id={requested_task_id}",
                status.task_id
            ),
            None,
        ));
    }
    Ok(LocalStudioLiveSnapshot {
        studio_mode: status.studio_mode.clone(),
        studio_mode_age_ms: opt_age_to_u128(status.studio_mode_age_ms),
        studio_mode_source: status.studio_mode_source.clone(),
        studio_control_state: status.studio_control_state.clone(),
        studio_transition_phase: status.studio_transition_phase.clone(),
        studio_transition_age_ms: opt_age_to_u128(status.studio_transition_age_ms),
        edit_runtime_state: status.edit_runtime_state.clone(),
        edit_runtime_age_ms: opt_age_to_u128(status.edit_runtime_age_ms),
        studio_control_last_error: status.studio_control_last_error.clone(),
        official_mcp_adapter_state: status.official_mcp_adapter_state.clone(),
        official_mcp_adapter_age_ms: opt_age_to_u128(status.official_mcp_adapter_age_ms),
        official_mcp_adapter_last_error: status.official_mcp_adapter_last_error.clone(),
    })
}

fn local_studio_gate_wait_reason(
    state: &AppState,
    requested_task_id: &str,
    kind: LocalStudioGateKind,
) -> Option<&'static str> {
    let helper = state.active_helper.as_ref()?;
    if helper.task_id.as_deref() != Some(requested_task_id)
        || helper.last_message_at.elapsed() > HELPER_HEARTBEAT_TIMEOUT
    {
        return None;
    }
    let Some(status) = helper.task_status.as_ref() else {
        return Some("local_task_status_missing");
    };
    if status.task_id != requested_task_id {
        return None;
    }
    if status.studio_mode.is_none() || status.studio_mode_age_ms.is_none() {
        return Some("studio_mode_unavailable");
    }
    let studio_mode = status.studio_mode.as_deref();
    match kind {
        LocalStudioGateKind::Edit => {
            if studio_mode != Some("stop") {
                return None;
            }
            if status.edit_runtime_state.is_none() || status.edit_runtime_age_ms.is_none() {
                Some("edit_runtime_unavailable")
            } else {
                None
            }
        }
        LocalStudioGateKind::Launch => match studio_mode {
            Some("stop") => {
                if status.edit_runtime_state.is_none() || status.edit_runtime_age_ms.is_none() {
                    Some("edit_runtime_unavailable")
                } else {
                    None
                }
            }
            Some("start_play" | "run_server") => {
                if status.studio_control_state.is_none() || status.studio_transition_phase.is_none()
                {
                    Some("studio_control_unavailable")
                } else {
                    None
                }
            }
            _ => None,
        },
        LocalStudioGateKind::StopControl => match studio_mode {
            Some("start_play" | "run_server") => {
                if status.studio_control_state.is_none() || status.studio_transition_phase.is_none()
                {
                    Some("studio_control_unavailable")
                } else {
                    None
                }
            }
            _ => None,
        },
        LocalStudioGateKind::Official => {
            if studio_mode != Some("stop") {
                return None;
            }
            if status.edit_runtime_state.is_none() || status.edit_runtime_age_ms.is_none() {
                return Some("edit_runtime_unavailable");
            }
            if status.official_mcp_adapter_state.is_none()
                || status.official_mcp_adapter_age_ms.is_none()
            {
                Some("official_mcp_adapter_unavailable")
            } else {
                None
            }
        }
    }
}

async fn wait_for_local_studio_status_if_needed(
    state: PackedState,
    requested_task_id: &str,
    action: &str,
    kind: LocalStudioGateKind,
) -> Result<(), ErrorData> {
    let deadline = Instant::now() + LOCAL_STUDIO_STATUS_WAIT_TIMEOUT;
    loop {
        let wait_reason = {
            let state = state.lock().await;
            local_studio_gate_wait_reason(&state, requested_task_id, kind)
        };
        let Some(wait_reason) = wait_reason else {
            return Ok(());
        };
        if Instant::now() >= deadline {
            return Err(ErrorData::internal_error(
                format!(
                    "{action} failed fast: local_studio_status_unavailable reason={wait_reason}"
                ),
                None,
            ));
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn require_local_edit_ready_locked(
    state: &AppState,
    requested_task_id: &str,
    action: &str,
) -> Result<LocalStudioLiveSnapshot, ErrorData> {
    let snapshot = local_studio_live_snapshot_locked(state, requested_task_id, action)?;
    ensure_reported_age_fresh(action, "studio_mode", snapshot.studio_mode_age_ms)?;
    match snapshot.studio_mode.as_deref() {
        Some("stop") => {
            ensure_reported_age_fresh(action, "edit_runtime", snapshot.edit_runtime_age_ms)?;
            if snapshot.edit_runtime_state.as_deref() != Some("ready") {
                return Err(ErrorData::internal_error(
                    format!(
                        "{action} failed fast: edit_runtime_not_ready state={}",
                        snapshot.edit_runtime_state.as_deref().unwrap_or("unknown")
                    ),
                    None,
                ));
            }
            Ok(snapshot)
        }
        Some(mode) => Err(ErrorData::internal_error(
            format!("{action} failed fast: studio_mode_not_stop current_mode={mode}"),
            None,
        )),
        None => Err(ErrorData::internal_error(
            format!("{action} failed fast: studio_mode_unknown"),
            None,
        )),
    }
}

fn require_local_launch_ready_locked(
    state: &AppState,
    requested_task_id: &str,
    action: &str,
) -> Result<(), ErrorData> {
    let snapshot = local_studio_live_snapshot_locked(state, requested_task_id, action)?;
    ensure_reported_age_fresh(action, "studio_mode", snapshot.studio_mode_age_ms)?;
    let Some(mode) = snapshot.studio_mode.as_deref() else {
        return Err(ErrorData::internal_error(
            format!("{action} failed fast: studio_mode_unknown"),
            None,
        ));
    };
    if studio_mode_is_running(Some(mode)) {
        return require_local_stop_control_ready_locked(state, requested_task_id, action);
    }
    if mode != "stop" {
        return Err(ErrorData::internal_error(
            format!("{action} failed fast: studio_mode_unknown current_mode={mode}"),
            None,
        ));
    }
    require_local_edit_ready_locked(state, requested_task_id, action)?;
    Ok(())
}

fn require_local_stop_control_ready_locked(
    state: &AppState,
    requested_task_id: &str,
    action: &str,
) -> Result<(), ErrorData> {
    let snapshot = local_studio_live_snapshot_locked(state, requested_task_id, action)?;
    ensure_reported_age_fresh(action, "studio_mode", snapshot.studio_mode_age_ms)?;
    let Some(mode) = snapshot.studio_mode.as_deref() else {
        return Err(ErrorData::internal_error(
            format!("{action} failed fast: studio_mode_unknown"),
            None,
        ));
    };
    if mode == "stop" {
        return Ok(());
    }
    if !studio_mode_is_running(Some(mode)) {
        return Err(ErrorData::internal_error(
            format!("{action} failed fast: studio_mode_unknown current_mode={mode}"),
            None,
        ));
    }
    let transition_phase = snapshot
        .studio_transition_phase
        .as_deref()
        .unwrap_or("unknown");
    if studio_transition_is_stopping(Some(transition_phase)) {
        return Err(ErrorData::internal_error(
            format!(
                "{action} failed fast: studio_stop_in_progress current_mode={mode} transition_phase={transition_phase}"
            ),
            None,
        ));
    }
    if snapshot.studio_control_state.as_deref() != Some("ready") {
        return Err(ErrorData::internal_error(
            format!(
                "{action} failed fast: uncontrolled_play_session current_mode={mode} studio_control_state={} transition_phase={transition_phase}",
                snapshot
                    .studio_control_state
                    .as_deref()
                    .unwrap_or("unknown")
            ),
            None,
        ));
    }

    Ok(())
}

fn require_local_official_ready_locked(
    state: &AppState,
    requested_task_id: &str,
    action: &str,
) -> Result<(), ErrorData> {
    let snapshot = require_local_edit_ready_locked(state, requested_task_id, action)?;
    ensure_reported_age_fresh(
        action,
        "official_mcp_adapter",
        snapshot.official_mcp_adapter_age_ms,
    )?;
    match snapshot.official_mcp_adapter_state.as_deref() {
        Some("ready") => Ok(()),
        Some(state) => Err(ErrorData::internal_error(
            format!("{action} failed fast: official_mcp_adapter_not_ready state={state}"),
            None,
        )),
        None => Err(ErrorData::internal_error(
            format!("{action} failed fast: official_mcp_adapter_snapshot_unavailable"),
            None,
        )),
    }
}

fn require_active_helper_matches_hub_route_locked(
    helper: &ActiveHelperConnection,
    snapshot: &HubTaskRouteSnapshot,
    action: &str,
) -> Result<(), ErrorData> {
    let Some(route_helper_id) = snapshot.helper_id.as_deref() else {
        return Err(ErrorData::internal_error(
            format!("{action} failed fast: hub_claimed_helper_not_online"),
            None,
        ));
    };
    if helper.helper_id != route_helper_id {
        return Err(ErrorData::internal_error(
            format!(
                "{action} failed fast: hub_route_helper_mismatch active_helper_id={} hub_helper_id={route_helper_id}",
                helper.helper_id
            ),
            None,
        ));
    }
    Ok(())
}

fn local_studio_live_snapshot_for_status_locked(
    state: &AppState,
    task_id: Option<&str>,
) -> (String, Option<LocalStudioLiveSnapshot>) {
    let Some(task_id) = task_id else {
        return ("task_id_unconfigured".to_owned(), None);
    };
    let Some(helper) = state.active_helper.as_ref() else {
        return ("local_helper_missing".to_owned(), None);
    };
    let Some(helper_task_id) = helper.task_id.as_deref() else {
        return ("local_helper_task_id_missing".to_owned(), None);
    };
    if helper_task_id != task_id {
        return ("task_id_mismatch".to_owned(), None);
    }
    if helper.last_message_at.elapsed() > HELPER_HEARTBEAT_TIMEOUT {
        return ("local_helper_stale".to_owned(), None);
    }
    if helper.task_status.is_none() {
        return ("local_task_status_missing".to_owned(), None);
    }
    match local_studio_live_snapshot_locked(state, task_id, "status") {
        Ok(snapshot) => ("local_helper_live".to_owned(), Some(snapshot)),
        Err(error) => (format!("source_error: {error}"), None),
    }
}

#[derive(Clone)]
enum OutgoingHelperFrame {
    Text(String),
}

struct ArtifactUploadState {
    request_id: Uuid,
    session_id: String,
    runtime_id: String,
    place_id: String,
    task_id: Option<String>,
    tag: Option<String>,
    temp_path: PathBuf,
    artifact_dir: PathBuf,
    screenshot_dir: PathBuf,
    session_metadata_path: PathBuf,
    total_bytes: usize,
    bytes_written: usize,
    expected_seq: u32,
}

struct PreparedArtifactUpload {
    upload_id: Uuid,
    upload: ArtifactUploadState,
}

struct QueuedToolRequest {
    id: Uuid,
    tool_name: &'static str,
    helper_request_timeout: Duration,
    rx: mpsc::UnboundedReceiver<Result<String>>,
    uploads: UploadRegistry,
}

impl AppState {
    pub fn new(
        workspace: PathBuf,
        task_id: Option<String>,
        public_base_url: Option<String>,
        hub_base_url: Option<String>,
        hub_bearer_token: Option<String>,
    ) -> Self {
        let (trigger, waiter) = watch::channel(());
        Self {
            workspace,
            task_id,
            public_base_url,
            hub_status_client: HubStatusClient::new(hub_base_url, hub_bearer_token),
            process_queue: VecDeque::new(),
            output_map: HashMap::new(),
            active_helper: None,
            uploads: Arc::new(StdMutex::new(HashMap::new())),
            waiter,
            trigger,
        }
    }
}

pub async fn status_handler(State(state): State<PackedState>) -> Json<StatusResponse> {
    let (
        workspace,
        task_id,
        public_base_url,
        queued_requests,
        pending_responses,
        helper_connected,
        helper_place_id,
        helper_task_id,
        helper_connection_id,
        helper_capabilities,
        helper_last_message_age_ms,
        hub_status_client,
    ) = {
        let state = state.lock().await;
        tracing::info!(
            queued_requests = state.process_queue.len(),
            pending_responses = state.output_map.len(),
            "status requested"
        );
        let helper_connected = state
            .active_helper
            .as_ref()
            .map(|helper| helper.last_message_at.elapsed() <= HELPER_HEARTBEAT_TIMEOUT)
            .unwrap_or(false);
        let helper_last_message_age_ms = state
            .active_helper
            .as_ref()
            .map(|helper| helper.last_message_at.elapsed().as_millis());
        let helper_capabilities = if helper_connected {
            state
                .active_helper
                .as_ref()
                .map(|helper| helper.capabilities.clone())
        } else {
            None
        };
        (
            state.workspace.to_string_lossy().into_owned(),
            state.task_id.clone(),
            state.public_base_url.clone(),
            state.process_queue.len(),
            state.output_map.len(),
            helper_connected,
            if helper_connected {
                state
                    .active_helper
                    .as_ref()
                    .map(|helper| helper.place_id.clone())
            } else {
                None
            },
            if helper_connected {
                state
                    .active_helper
                    .as_ref()
                    .and_then(|helper| helper.task_id.clone())
            } else {
                None
            },
            state
                .active_helper
                .as_ref()
                .map(|helper| helper.connection_id.to_string()),
            helper_capabilities,
            helper_last_message_age_ms,
            state.hub_status_client.clone(),
        )
    };
    let mut status_source = "hub_unconfigured".to_owned();
    let mut route_status_source = "hub_unconfigured".to_owned();
    let mut hub_snapshot_error = None;
    let mut studio_mode = None;
    let mut studio_mode_age_ms = None;
    let mut studio_mode_source = None;
    let mut studio_control_state = None;
    let mut studio_transition_phase = None;
    let mut studio_transition_age_ms = None;
    let mut edit_runtime_state = None;
    let mut edit_runtime_age_ms = None;
    let mut studio_control_last_error = None;
    let mut official_mcp_adapter_state = "hub_unconfigured".to_owned();
    let mut official_mcp_adapter_age_ms = None;
    let mut official_mcp_adapter_last_error = None;
    let mut task_services_ready = false;
    let mut route_launch_ready = false;
    let status_task_id = task_id.clone().or_else(|| helper_task_id.clone());
    let hub_base_url = hub_status_client
        .as_ref()
        .map(|client| client.base_url().to_owned());
    if let (Some(client), Some(task_id)) = (hub_status_client.as_ref(), status_task_id.as_deref()) {
        match client.fetch_task_route_snapshot(task_id).await {
            Ok(snapshot) => {
                status_source = "hub".to_owned();
                route_status_source = "hub".to_owned();
                task_services_ready = snapshot.task_services_ready();
                route_launch_ready = snapshot.launch_ready();
            }
            Err(error) => {
                status_source = "hub_error".to_owned();
                route_status_source = "hub_error".to_owned();
                hub_snapshot_error = Some(error.to_string());
                official_mcp_adapter_state = "hub_error".to_owned();
            }
        }
    } else if status_task_id.is_none() {
        status_source = "task_id_unconfigured".to_owned();
        route_status_source = "task_id_unconfigured".to_owned();
        official_mcp_adapter_state = "task_id_unconfigured".to_owned();
    }
    let (studio_status_source, local_snapshot) = {
        let state = state.lock().await;
        local_studio_live_snapshot_for_status_locked(&state, status_task_id.as_deref())
    };
    if let Some(snapshot) = local_snapshot.as_ref() {
        studio_mode = snapshot.studio_mode.clone();
        studio_mode_age_ms = snapshot.studio_mode_age_ms;
        studio_mode_source = snapshot.studio_mode_source.clone();
        studio_control_state = snapshot.studio_control_state.clone();
        studio_transition_phase = snapshot.studio_transition_phase.clone();
        studio_transition_age_ms = snapshot.studio_transition_age_ms;
        edit_runtime_state = snapshot.edit_runtime_state.clone();
        edit_runtime_age_ms = snapshot.edit_runtime_age_ms;
        studio_control_last_error = snapshot.studio_control_last_error.clone();
        official_mcp_adapter_state = snapshot
            .official_mcp_adapter_state
            .clone()
            .unwrap_or_else(|| "not_started".to_owned());
        official_mcp_adapter_age_ms = snapshot.official_mcp_adapter_age_ms;
        official_mcp_adapter_last_error = snapshot.official_mcp_adapter_last_error.clone();
    } else if official_mcp_adapter_state == "hub_unconfigured"
        || official_mcp_adapter_state == "task_id_unconfigured"
    {
        official_mcp_adapter_state = studio_status_source.clone();
    }
    let local_helper_ready_for_launch = helper_connected
        && status_task_id
            .as_deref()
            .is_some_and(|task_id| helper_task_id.as_deref() == Some(task_id));
    let launch_ready = route_launch_ready && local_helper_ready_for_launch;
    let edit_ready = launch_ready
        && local_snapshot
            .as_ref()
            .map(LocalStudioLiveSnapshot::edit_ready)
            .unwrap_or(false);
    let official_ready = launch_ready
        && local_snapshot
            .as_ref()
            .map(LocalStudioLiveSnapshot::official_ready)
            .unwrap_or(false);
    Json(StatusResponse {
        service: "rbx-studio-mcp",
        workspace,
        task_id: status_task_id,
        public_base_url,
        queued_requests,
        pending_responses,
        helper_connected,
        helper_place_id,
        helper_task_id,
        helper_connection_id,
        helper_capabilities,
        status_source,
        route_status_source,
        studio_status_source,
        hub_base_url,
        hub_snapshot_error,
        task_services_ready,
        launch_ready,
        edit_ready,
        official_ready,
        studio_mode,
        studio_mode_age_ms,
        studio_mode_source,
        studio_control_state,
        studio_transition_phase,
        studio_transition_age_ms,
        edit_runtime_state,
        edit_runtime_age_ms,
        studio_control_last_error,
        official_mcp_adapter_state,
        official_mcp_adapter_age_ms,
        official_mcp_adapter_last_error,
        helper_last_message_age_ms,
    })
}

pub async fn debug_state_handler(State(state): State<PackedState>) -> Json<serde_json::Value> {
    let state = state.lock().await;
    let active_helper = state.active_helper.as_ref().map(|helper| {
        serde_json::json!({
            "connection_id": helper.connection_id.to_string(),
            "place_id": helper.place_id,
            "task_id": helper.task_id,
            "capabilities": helper.capabilities,
            "last_message_age_ms": helper.last_message_at.elapsed().as_millis(),
            "task_status": helper.task_status,
        })
    });
    let queued_requests = state
        .process_queue
        .iter()
        .map(|request| {
            serde_json::json!({
                "id": request.id.map(|id| id.to_string()),
                "tool": request.tool_name(),
                "task_id": request.args.task_id(),
            })
        })
        .collect::<Vec<_>>();
    let mut pending_request_ids = state
        .output_map
        .keys()
        .map(Uuid::to_string)
        .collect::<Vec<_>>();
    pending_request_ids.sort();
    Json(serde_json::json!({
        "ok": true,
        "service": "rbx-studio-mcp",
        "workspace": state.workspace.to_string_lossy(),
        "task_id": state.task_id,
        "public_base_url": state.public_base_url,
        "hub_base_url": state.hub_status_client.as_ref().map(|client| client.base_url().to_owned()),
        "queued_request_count": state.process_queue.len(),
        "pending_response_count": state.output_map.len(),
        "queued_requests": queued_requests,
        "pending_request_ids": pending_request_ids,
        "active_helper": active_helper,
    }))
}

impl ToolArguments {
    fn tool_name(&self) -> &'static str {
        self.args.tool_name()
    }
}
#[derive(Clone)]
pub struct RBXStudioServer {
    state: PackedState,
    tool_router: ToolRouter<Self>,
}

#[tool_handler]
impl ServerHandler for RBXStudioServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo {
            protocol_version: ProtocolVersion::LATEST,
            capabilities: ServerCapabilities::builder().enable_tools().build(),
            server_info: Implementation {
                name: "clockp_mcp".to_string(),
                version: env!("CARGO_PKG_VERSION").to_string(),
                title: Some("clockp MCP".to_string()),
                icons: None,
                website_url: None,
            },
            instructions: Some(
                "Use launch_studio_session for high-level launches into start_play or run_server.
get_studio_mode is for diagnostics and stop-path checks, not the normal launch entrypoint.
Use run_code to query or edit the Roblox Studio place while Studio is in stop/edit mode.
If Studio is already in start_play or run_server, launch_studio_session may stop/relaunch only when the live helper status proves fresh runtime control; otherwise it must fail fast as uncontrolled_play_session.
start_stop_play is stop-only and is idempotent when Studio is already stopped.
Do not recover by sending another play+stop loop.
If status reports studio_transition_phase=stopping_requested or stopping_observed, wait for stop/idle instead of issuing another play or stop command.
If status reports an uncontrolled play session, do not issue start_stop_play(stop); restore fresh runtime control or rebuild the session before launching again.
"
                    .to_string(),
            ),
        }
    }
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema, Clone)]
struct RunCode {
    #[schemars(
        description = "Required clock-p task_id used to route this call to the matching Studio plugin instance."
    )]
    task_id: String,
    #[schemars(description = "Code to run")]
    command: String,
    #[schemars(
        description = "Set true only for read-only diagnostics that are safe during start_play/run_server. Omit or false for any edit-capable code."
    )]
    diagnostic: Option<bool>,
}
#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema, Clone)]
struct InsertModel {
    #[schemars(
        description = "Required clock-p task_id used to route this call to the matching Studio plugin instance."
    )]
    task_id: String,
    #[schemars(description = "Query to search for the model")]
    query: String,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema, Clone)]
struct GetConsoleOutput {
    #[schemars(
        description = "Required clock-p task_id used to route this call to the matching Studio plugin instance."
    )]
    task_id: String,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema, Clone)]
struct GetStudioMode {
    #[schemars(
        description = "Required clock-p task_id used to route this call to the matching Studio plugin instance."
    )]
    task_id: String,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema, Clone)]
struct TakeScreenshot {
    #[schemars(
        description = "Required clock-p task_id used to route this call to the matching Studio plugin instance."
    )]
    task_id: String,
    #[schemars(
        description = "Optional session_id. When omitted, the MCP server will try to read .clock-p/current_session.json from the workspace."
    )]
    session_id: Option<String>,
    #[schemars(description = "Optional runtime_id. Defaults to server.")]
    runtime_id: Option<String>,
    #[schemars(description = "Optional screenshot tag used in the final file name.")]
    tag: Option<String>,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema, Clone)]
struct ReadStudioLog {
    #[schemars(
        description = "Required clock-p task_id used to route this call to the matching Studio plugin instance."
    )]
    task_id: String,
    #[schemars(
        description = "Optional starting line (1-indexed). Negative values count from the end."
    )]
    start_line: Option<i64>,
    #[schemars(description = "Optional number of lines to return.")]
    line_count: Option<u32>,
    #[schemars(description = "Optional regex used to filter matching lines.")]
    regex: Option<String>,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema, Clone)]
struct OfficialMcpPing {
    #[schemars(
        description = "Required clock-p task_id used to bind the official Studio MCP adapter to the matching remote task."
    )]
    task_id: String,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema, Clone)]
struct OfficialMcpVector3 {
    x: f64,
    y: f64,
    z: f64,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema, Clone)]
struct OfficialMcpGenerateMesh {
    #[schemars(
        description = "Required clock-p task_id used to bind the official Studio MCP adapter to the matching remote task."
    )]
    task_id: String,
    #[schemars(description = "Text prompt describing the mesh to generate.")]
    text_prompt: String,
    #[schemars(description = "Optional bounding box size for the generated mesh.")]
    size: Option<OfficialMcpVector3>,
    #[schemars(description = "Optional triangle limit, between 12 and 20000.")]
    max_triangles: Option<f64>,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema, Clone)]
struct OfficialMcpSearchCreatorStore {
    #[schemars(
        description = "Required clock-p task_id used to bind the official Studio MCP adapter to the matching remote task."
    )]
    task_id: String,
    #[schemars(description = "Creator Store search query.")]
    query: String,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema, Clone)]
struct OfficialMcpInsertFromCreatorStore {
    #[schemars(
        description = "Required clock-p task_id used to bind the official Studio MCP adapter to the matching remote task."
    )]
    task_id: String,
    #[schemars(description = "Search id returned by official_mcp_search_creator_store.")]
    search_id: String,
    #[schemars(description = "Optional objectTypes returned by the search result.")]
    object_types: Option<Vec<String>>,
    #[schemars(description = "Optional display name for the inserted asset.")]
    asset_name: Option<String>,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema, Clone)]
struct OfficialMcpStoreImage {
    #[schemars(
        description = "Required clock-p task_id used to bind the official Studio MCP adapter to the matching remote task."
    )]
    task_id: String,
    #[schemars(
        description = "Optional absolute png/jpg/jpeg path on the Windows helper machine. Mutually exclusive with image_base64."
    )]
    file_path: Option<String>,
    #[schemars(
        description = "Optional base64-encoded png/jpg/jpeg bytes. The Windows helper writes it to a task-scoped local temp file before calling official MCP store_image. Mutually exclusive with file_path."
    )]
    image_base64: Option<String>,
    #[schemars(
        description = "Required with image_base64. Allowed values: image/png, image/jpeg, image/jpg."
    )]
    mime_type: Option<String>,
    #[schemars(
        description = "Optional display hint only. The helper does not trust it as a path."
    )]
    file_name: Option<String>,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema, Clone)]
struct OfficialMcpGenerateProceduralModel {
    #[schemars(
        description = "Required clock-p task_id used to bind the official Studio MCP adapter to the matching remote task."
    )]
    task_id: String,
    #[schemars(description = "User's exact prompt for the procedural model.")]
    prompt: String,
    #[schemars(description = "Optional IMAGEID returned by official_mcp_store_image.")]
    attached_image_uri: Option<String>,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema, Clone)]
struct OfficialMcpWaitJobFinished {
    #[schemars(
        description = "Required clock-p task_id used to bind the official Studio MCP adapter to the matching remote task."
    )]
    task_id: String,
    #[schemars(description = "Generation id returned by official_mcp_generate_procedural_model.")]
    generation_id: String,
    #[schemars(description = "Optional wait timeout in seconds.")]
    timeout: Option<f64>,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema, Clone)]
struct StartStopPlay {
    #[schemars(
        description = "Required clock-p task_id used to route this call to the matching Studio plugin instance."
    )]
    task_id: String,
    #[schemars(
        description = "Only stop is allowed. Use launch_studio_session to enter start_play or run_server."
    )]
    mode: String,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema, Clone)]
struct LaunchStudioSession {
    #[schemars(
        description = "Required clock-p task_id used to route this call to the matching Studio plugin instance."
    )]
    task_id: String,
    #[schemars(
        description = "Target mode to launch into, must be start_play or run_server. A controlled existing play/run session may be stopped and relaunched; uncontrolled play/run must fail fast."
    )]
    mode: String,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema, Clone)]
enum ToolArgumentValues {
    RunCode(RunCode),
    InsertModel(InsertModel),
    GetConsoleOutput(GetConsoleOutput),
    StartStopPlay(StartStopPlay),
    LaunchStudioSession(LaunchStudioSession),
    GetStudioMode(GetStudioMode),
    TakeScreenshot(TakeScreenshot),
    ReadStudioLog(ReadStudioLog),
}

impl ToolArgumentValues {
    fn tool_name(&self) -> &'static str {
        match self {
            ToolArgumentValues::RunCode(_) => "run_code",
            ToolArgumentValues::InsertModel(_) => "insert_model",
            ToolArgumentValues::GetConsoleOutput(_) => "get_console_output",
            ToolArgumentValues::StartStopPlay(_) => "start_stop_play",
            ToolArgumentValues::LaunchStudioSession(_) => "launch_studio_session",
            ToolArgumentValues::GetStudioMode(_) => "get_studio_mode",
            ToolArgumentValues::TakeScreenshot(_) => "take_screenshot",
            ToolArgumentValues::ReadStudioLog(_) => "read_studio_log",
        }
    }

    fn task_id(&self) -> &str {
        match self {
            ToolArgumentValues::RunCode(args) => &args.task_id,
            ToolArgumentValues::InsertModel(args) => &args.task_id,
            ToolArgumentValues::GetConsoleOutput(args) => &args.task_id,
            ToolArgumentValues::StartStopPlay(args) => &args.task_id,
            ToolArgumentValues::LaunchStudioSession(args) => &args.task_id,
            ToolArgumentValues::GetStudioMode(args) => &args.task_id,
            ToolArgumentValues::TakeScreenshot(args) => &args.task_id,
            ToolArgumentValues::ReadStudioLog(args) => &args.task_id,
        }
    }

    fn validate(&self) -> Result<(), ErrorData> {
        match self {
            ToolArgumentValues::StartStopPlay(args) if args.mode != "stop" => {
                Err(ErrorData::invalid_params(
                    "start_stop_play is stop-only. Use launch_studio_session to enter start_play or run_server.",
                    None,
                ))
            }
            _ => Ok(()),
        }
    }

    fn local_studio_gate_kind(&self) -> Option<LocalStudioGateKind> {
        match self {
            ToolArgumentValues::RunCode(args) if args.diagnostic != Some(true) => {
                Some(LocalStudioGateKind::Edit)
            }
            ToolArgumentValues::InsertModel(_) => Some(LocalStudioGateKind::Edit),
            ToolArgumentValues::LaunchStudioSession(_) => Some(LocalStudioGateKind::Launch),
            ToolArgumentValues::StartStopPlay(_) => Some(LocalStudioGateKind::StopControl),
            _ => None,
        }
    }

    fn helper_request_timeout(&self) -> Duration {
        match self {
            ToolArgumentValues::StartStopPlay(_) | ToolArgumentValues::LaunchStudioSession(_) => {
                HELPER_STUDIO_CONTROL_REQUEST_TIMEOUT
            }
            _ => HELPER_REQUEST_TIMEOUT,
        }
    }
}
#[tool_router]
impl RBXStudioServer {
    pub fn new(state: PackedState) -> Self {
        Self {
            state,
            tool_router: Self::tool_router(),
        }
    }

    #[tool(
        description = "Runs a command in Roblox Studio and returns the printed output. Can be used to both make changes and retrieve information"
    )]
    async fn run_code(
        &self,
        Parameters(args): Parameters<RunCode>,
    ) -> Result<CallToolResult, ErrorData> {
        self.generic_tool_run(ToolArgumentValues::RunCode(args))
            .await
    }

    #[tool(
        description = "Inserts a model from the Roblox marketplace into the workspace. Returns the inserted model name."
    )]
    async fn insert_model(
        &self,
        Parameters(args): Parameters<InsertModel>,
    ) -> Result<CallToolResult, ErrorData> {
        self.generic_tool_run(ToolArgumentValues::InsertModel(args))
            .await
    }

    #[tool(description = "Get the console output from Roblox Studio.")]
    async fn get_console_output(
        &self,
        Parameters(args): Parameters<GetConsoleOutput>,
    ) -> Result<CallToolResult, ErrorData> {
        self.generic_tool_run(ToolArgumentValues::GetConsoleOutput(args))
            .await
    }

    #[tool(
        description = "Stop the current Roblox Studio play/run session. This tool never starts Studio."
    )]
    async fn start_stop_play(
        &self,
        Parameters(args): Parameters<StartStopPlay>,
    ) -> Result<CallToolResult, ErrorData> {
        self.generic_tool_run(ToolArgumentValues::StartStopPlay(args))
            .await
    }

    #[tool(
        description = "Launch Roblox Studio into start_play or run_server. If Studio is already in play/run, the launch may stop/relaunch only when the live helper status proves fresh runtime control; uncontrolled play/run fails fast. Returns the plugin launch JSON with final_mode and message."
    )]
    async fn launch_studio_session(
        &self,
        Parameters(args): Parameters<LaunchStudioSession>,
    ) -> Result<CallToolResult, ErrorData> {
        self.generic_tool_run(ToolArgumentValues::LaunchStudioSession(args))
            .await
    }

    #[tool(
        description = "Get the current studio mode. Returns the studio mode. The result will be one of start_play, run_server, or stop."
    )]
    async fn get_studio_mode(
        &self,
        Parameters(args): Parameters<GetStudioMode>,
    ) -> Result<CallToolResult, ErrorData> {
        self.generic_tool_run(ToolArgumentValues::GetStudioMode(args))
            .await
    }

    #[tool(
        description = "Capture a screenshot of the active Roblox Studio window through the Windows helper, store it under the current workspace artifacts directory, and return the final file path."
    )]
    async fn take_screenshot(
        &self,
        Parameters(args): Parameters<TakeScreenshot>,
    ) -> Result<CallToolResult, ErrorData> {
        self.generic_tool_run(ToolArgumentValues::TakeScreenshot(args))
            .await
    }

    #[tool(description = "Read the latest Roblox Studio desktop log through the Windows helper.")]
    async fn read_studio_log(
        &self,
        Parameters(args): Parameters<ReadStudioLog>,
    ) -> Result<CallToolResult, ErrorData> {
        self.generic_tool_run(ToolArgumentValues::ReadStudioLog(args))
            .await
    }

    #[tool(
        description = "Ping the hidden official Roblox Studio MCP adapter through the Windows helper. This only checks lifecycle and returns a summarized tools/list result; it does not expose official tools directly."
    )]
    async fn official_mcp_ping(
        &self,
        Parameters(args): Parameters<OfficialMcpPing>,
    ) -> Result<CallToolResult, ErrorData> {
        self.dispatch_official_mcp("ping", serde_json::json!({}), 30_000, args.task_id)
            .await
    }

    #[tool(
        description = "Generate a textured mesh through the hidden official Roblox Studio MCP adapter. Requires an explicit clock-p task_id and fails if it does not match the active helper connection."
    )]
    async fn official_mcp_generate_mesh(
        &self,
        Parameters(args): Parameters<OfficialMcpGenerateMesh>,
    ) -> Result<CallToolResult, ErrorData> {
        let mut arguments = serde_json::Map::new();
        arguments.insert("textPrompt".to_owned(), Value::String(args.text_prompt));
        if let Some(size) = args.size {
            arguments.insert(
                "size".to_owned(),
                serde_json::json!({
                    "x": size.x,
                    "y": size.y,
                    "z": size.z,
                }),
            );
        }
        if let Some(max_triangles) = args.max_triangles {
            arguments.insert("maxTriangles".to_owned(), serde_json::json!(max_triangles));
        }
        self.dispatch_official_mcp(
            "generate_mesh",
            Value::Object(arguments),
            OFFICIAL_MCP_LONG_TIMEOUT_MS,
            args.task_id,
        )
        .await
    }

    #[tool(
        description = "Search the Roblox Creator Store through the hidden official Roblox Studio MCP adapter. Use the returned searchId with official_mcp_insert_from_creator_store."
    )]
    async fn official_mcp_search_creator_store(
        &self,
        Parameters(args): Parameters<OfficialMcpSearchCreatorStore>,
    ) -> Result<CallToolResult, ErrorData> {
        self.dispatch_official_mcp(
            "search_creator_store",
            serde_json::json!({ "query": args.query }),
            60_000,
            args.task_id,
        )
        .await
    }

    #[tool(
        description = "Insert a Creator Store asset selected from official_mcp_search_creator_store through the hidden official Roblox Studio MCP adapter."
    )]
    async fn official_mcp_insert_from_creator_store(
        &self,
        Parameters(args): Parameters<OfficialMcpInsertFromCreatorStore>,
    ) -> Result<CallToolResult, ErrorData> {
        let mut arguments = serde_json::Map::new();
        arguments.insert("searchId".to_owned(), Value::String(args.search_id));
        if let Some(object_types) = args.object_types {
            arguments.insert("objectTypes".to_owned(), serde_json::json!(object_types));
        }
        if let Some(asset_name) = args.asset_name {
            arguments.insert("assetName".to_owned(), Value::String(asset_name));
        }
        self.dispatch_official_mcp(
            "insert_from_creator_store",
            Value::Object(arguments),
            120_000,
            args.task_id,
        )
        .await
    }

    #[tool(
        description = "Convert a local Windows image file into an official MCP IMAGEID token that can be passed to official_mcp_generate_procedural_model."
    )]
    async fn official_mcp_store_image(
        &self,
        Parameters(args): Parameters<OfficialMcpStoreImage>,
    ) -> Result<CallToolResult, ErrorData> {
        let task_id = args.task_id.clone();
        let arguments = official_store_image_arguments(args)?;
        self.dispatch_official_mcp(
            "store_image",
            arguments,
            OFFICIAL_MCP_STORE_IMAGE_TIMEOUT_MS,
            task_id,
        )
        .await
    }

    #[tool(
        description = "Generate a procedural primitive model through the hidden official Roblox Studio MCP adapter. Use only when the user explicitly asks for primitive shapes, blocks, or geometric parts."
    )]
    async fn official_mcp_generate_procedural_model(
        &self,
        Parameters(args): Parameters<OfficialMcpGenerateProceduralModel>,
    ) -> Result<CallToolResult, ErrorData> {
        let mut arguments = serde_json::Map::new();
        arguments.insert("prompt".to_owned(), Value::String(args.prompt));
        if let Some(attached_image_uri) = args.attached_image_uri {
            arguments.insert(
                "attachedImageUri".to_owned(),
                Value::String(attached_image_uri),
            );
        }
        self.dispatch_official_mcp(
            "generate_procedural_model",
            Value::Object(arguments),
            OFFICIAL_MCP_LONG_TIMEOUT_MS,
            args.task_id,
        )
        .await
    }

    #[tool(
        description = "Wait for an explicit official procedural generation job to finish. Do not call automatically after official_mcp_generate_procedural_model unless the user asks to wait."
    )]
    async fn official_mcp_wait_job_finished(
        &self,
        Parameters(args): Parameters<OfficialMcpWaitJobFinished>,
    ) -> Result<CallToolResult, ErrorData> {
        let mut arguments = serde_json::Map::new();
        arguments.insert("generationId".to_owned(), Value::String(args.generation_id));
        if let Some(timeout) = args.timeout {
            arguments.insert("timeout".to_owned(), serde_json::json!(timeout));
        }
        self.dispatch_official_mcp(
            "wait_job_finished",
            Value::Object(arguments),
            OFFICIAL_MCP_LONG_TIMEOUT_MS,
            args.task_id,
        )
        .await
    }

    async fn dispatch_official_mcp(
        &self,
        action: &str,
        arguments: Value,
        timeout_ms: u64,
        task_id: String,
    ) -> Result<CallToolResult, ErrorData> {
        let request_id = Uuid::new_v4();
        let (tx, mut rx) = mpsc::unbounded_channel::<Result<String>>();
        let requires_store_image_base64_capability =
            action == "store_image" && arguments.get("imageBase64").is_some();
        let task_id = sanitize_identifier("task_id", &task_id).map_err(|error| {
            ErrorData::internal_error(
                format!("Invalid official MCP task_id argument: {error}"),
                None,
            )
        })?;
        let hub_status_client = { self.state.lock().await.hub_status_client.clone() };
        let mut route_snapshot =
            require_hub_route_ready_snapshot(hub_status_client.clone(), &task_id, action).await?;
        if action != "ping" {
            wait_for_local_studio_status_if_needed(
                Arc::clone(&self.state),
                &task_id,
                action,
                LocalStudioGateKind::Official,
            )
            .await?;
            route_snapshot =
                require_hub_route_ready_snapshot(hub_status_client, &task_id, action).await?;
        }
        let sender = {
            let mut state = self.state.lock().await;
            let Some(helper) = state.active_helper.as_ref() else {
                return Err(ErrorData::internal_error(
                    "No active Studio helper WebSocket connection",
                    None,
                ));
            };
            if helper.last_message_at.elapsed() > HELPER_HEARTBEAT_TIMEOUT {
                return Err(ErrorData::internal_error(
                    "Studio helper WebSocket connection is stale",
                    None,
                ));
            }
            if !helper
                .capabilities
                .iter()
                .any(|capability| capability == OFFICIAL_MCP_ADAPTER_CAPABILITY)
            {
                return Err(ErrorData::internal_error(
                    "Studio helper does not declare official_mcp_adapter_v1",
                    None,
                ));
            }
            if requires_store_image_base64_capability
                && !helper
                    .capabilities
                    .iter()
                    .any(|capability| capability == OFFICIAL_MCP_STORE_IMAGE_BASE64_CAPABILITY)
            {
                return Err(ErrorData::internal_error(
                    "Studio helper does not declare official_mcp_store_image_base64_v1; deploy the updated Windows helper before using image_base64",
                    None,
                ));
            }
            let Some(helper_task_id) = helper.task_id.as_deref() else {
                return Err(ErrorData::internal_error(
                    "Official MCP request requires a task_id-bound helper connection",
                    None,
                ));
            };
            if helper_task_id != task_id {
                return Err(ErrorData::internal_error(
                    format!(
                        "Official MCP task_id mismatch: active_helper_task_id={helper_task_id}, requested_task_id={task_id}"
                    ),
                    None,
                ));
            }
            require_active_helper_matches_hub_route_locked(helper, &route_snapshot, action)?;
            if action != "ping" {
                require_local_official_ready_locked(&state, &task_id, action)?;
            }
            let request = OfficialMcpRequest {
                request_id: request_id.to_string(),
                task_id,
                place_id: helper.place_id.clone(),
                action: action.to_owned(),
                arguments,
                timeout_ms,
            };
            let message =
                serde_json::to_string(&ServerToHelperMessage::OfficialMcpRequest(request))
                    .map_err(|error| {
                        ErrorData::internal_error(
                            format!("failed to encode official MCP request: {error}"),
                            None,
                        )
                    })?;
            let sender = helper.sender.clone();
            state.output_map.insert(request_id, tx);
            (sender, message)
        };
        if sender.0.send(OutgoingHelperFrame::Text(sender.1)).is_err() {
            let mut state = self.state.lock().await;
            state.output_map.remove(&request_id);
            return Err(ErrorData::internal_error(
                "helper WebSocket sender dropped before official MCP dispatch",
                None,
            ));
        }
        let wait_timeout = Duration::from_millis(timeout_ms.max(1)) + Duration::from_secs(5);
        let result = match tokio::time::timeout(wait_timeout, rx.recv()).await {
            Ok(Some(result)) => result,
            Ok(None) => {
                return Err(ErrorData::internal_error(
                    "Couldn't receive official MCP response",
                    None,
                ))
            }
            Err(_) => {
                let mut state = self.state.lock().await;
                remove_request_tracking(&mut state, request_id);
                return Err(ErrorData::internal_error(
                    "Timed out waiting for official MCP response from Studio helper",
                    None,
                ));
            }
        };
        {
            let mut state = self.state.lock().await;
            state.output_map.remove(&request_id);
        }
        match result {
            Ok(result) => Ok(CallToolResult::success(vec![Content::text(result)])),
            Err(error) => Ok(CallToolResult::error(vec![Content::text(
                error.to_string(),
            )])),
        }
    }

    async fn generic_tool_run(
        &self,
        args: ToolArgumentValues,
    ) -> Result<CallToolResult, ErrorData> {
        let QueuedToolRequest {
            id,
            tool_name,
            helper_request_timeout,
            mut rx,
            uploads,
        } = queue_tool_request(Arc::clone(&self.state), args, None, "mcp").await?;
        let result = rx.recv();
        let result = match tokio::time::timeout(helper_request_timeout, result).await {
            Ok(Some(result)) => result,
            Ok(None) => return Err(ErrorData::internal_error("Couldn't receive response", None)),
            Err(_) => {
                let mut state = self.state.lock().await;
                remove_request_tracking(&mut state, id);
                abort_uploads_for_request(&uploads, id);
                return Err(ErrorData::internal_error(
                    format!(
                        "Timed out waiting {}ms for {tool_name} response from Studio helper",
                        helper_request_timeout.as_millis()
                    ),
                    None,
                ));
            }
        };
        {
            let mut state = self.state.lock().await;
            state.output_map.remove_entry(&id);
        }
        match result {
            Ok(result) => {
                tracing::info!(
                    %id,
                    tool = tool_name,
                    result = summarize_text(&result),
                    "tool request succeeded"
                );
                Ok(CallToolResult::success(vec![Content::text(result)]))
            }
            Err(err) => {
                tracing::warn!(
                    %id,
                    tool = tool_name,
                    error = summarize_text(&err.to_string()),
                    "tool request failed"
                );
                Ok(CallToolResult::error(vec![Content::text(err.to_string())]))
            }
        }
    }
}

async fn queue_tool_request(
    state: PackedState,
    args: ToolArgumentValues,
    request_id: Option<Uuid>,
    source: &'static str,
) -> Result<QueuedToolRequest, ErrorData> {
    let normalized_args = {
        let workspace = { state.lock().await.workspace.clone() };
        normalize_tool_arguments_for_workspace(&workspace, args).map_err(|error| {
            ErrorData::internal_error(format!("Unable to normalize tool arguments: {error}"), None)
        })?
    };
    normalized_args.validate()?;
    let requested_task_id =
        sanitize_identifier("task_id", normalized_args.task_id()).map_err(|error| {
            ErrorData::internal_error(format!("Invalid MCP tool task_id argument: {error}"), None)
        })?;
    let id = request_id.unwrap_or_else(Uuid::new_v4);
    let command = ToolArguments {
        args: normalized_args,
        id: Some(id),
    };
    let tool_name = command.tool_name();
    let helper_request_timeout = command.args.helper_request_timeout();
    let gate_kind = command.args.local_studio_gate_kind();
    let mut route_snapshot = None;
    if let Some(gate_kind) = gate_kind {
        let hub_status_client = if local_gate_requires_hub_route_ready(gate_kind) {
            let hub_status_client = { state.lock().await.hub_status_client.clone() };
            require_hub_route_ready_snapshot(
                hub_status_client.clone(),
                &requested_task_id,
                tool_name,
            )
            .await?;
            Some(hub_status_client)
        } else {
            None
        };
        wait_for_local_studio_status_if_needed(
            Arc::clone(&state),
            &requested_task_id,
            tool_name,
            gate_kind,
        )
        .await?;
        if let Some(hub_status_client) = hub_status_client {
            route_snapshot = Some(
                require_hub_route_ready_snapshot(hub_status_client, &requested_task_id, tool_name)
                    .await?,
            );
        }
    }
    let (tx, rx) = mpsc::unbounded_channel::<Result<String>>();
    let (trigger, uploads, queued_requests, pending_responses) = {
        let mut state = state.lock().await;
        let Some(helper) = state.active_helper.as_ref() else {
            return Err(ErrorData::internal_error(
                "No active Studio helper WebSocket connection",
                None,
            ));
        };
        let Some(helper_task_id) = helper.task_id.as_deref() else {
            return Err(ErrorData::internal_error(
                "MCP tool request requires a task_id-bound helper connection",
                None,
            ));
        };
        if helper_task_id != requested_task_id {
            return Err(ErrorData::internal_error(
                format!(
                    "MCP tool task_id mismatch: active_helper_task_id={helper_task_id}, requested_task_id={requested_task_id}"
                ),
                None,
            ));
        }
        if let Some(snapshot) = route_snapshot.as_ref() {
            require_active_helper_matches_hub_route_locked(helper, snapshot, tool_name)?;
        }
        match gate_kind {
            Some(LocalStudioGateKind::Edit) => {
                require_local_edit_ready_locked(&state, &requested_task_id, tool_name)?;
            }
            Some(LocalStudioGateKind::Launch) => {
                require_local_launch_ready_locked(&state, &requested_task_id, tool_name)?;
            }
            Some(LocalStudioGateKind::StopControl) => {
                require_local_stop_control_ready_locked(&state, &requested_task_id, tool_name)?;
            }
            Some(LocalStudioGateKind::Official) | None => {}
        }
        if state.output_map.contains_key(&id)
            || state
                .process_queue
                .iter()
                .any(|queued| queued.id == Some(id))
        {
            return Err(ErrorData::internal_error(
                format!("MCP tool request id collision: {id}"),
                None,
            ));
        }
        state.process_queue.push_back(command);
        state.output_map.insert(id, tx);
        (
            state.trigger.clone(),
            Arc::clone(&state.uploads),
            state.process_queue.len(),
            state.output_map.len(),
        )
    };
    tracing::info!(
        %id,
        tool = tool_name,
        queued_requests,
        pending_responses,
        source,
        "queued tool request"
    );
    trigger
        .send(())
        .map_err(|e| ErrorData::internal_error(format!("Unable to trigger send {e}"), None))?;
    Ok(QueuedToolRequest {
        id,
        tool_name,
        helper_request_timeout,
        rx,
        uploads,
    })
}

fn official_store_image_argument_error(message: impl Into<String>) -> ErrorData {
    ErrorData::internal_error(
        format!(
            "Invalid official_mcp_store_image arguments: {}",
            message.into()
        ),
        None,
    )
}

fn official_store_image_arguments(args: OfficialMcpStoreImage) -> Result<Value, ErrorData> {
    let has_file_path = args
        .file_path
        .as_deref()
        .map(|value| !value.trim().is_empty())
        .unwrap_or(false);
    let has_image_base64 = args
        .image_base64
        .as_deref()
        .map(|value| !value.trim().is_empty())
        .unwrap_or(false);
    match (has_file_path, has_image_base64) {
        (true, true) => {
            return Err(official_store_image_argument_error(
                "file_path and image_base64 are mutually exclusive",
            ))
        }
        (false, false) => {
            return Err(official_store_image_argument_error(
                "one of file_path or image_base64 is required",
            ))
        }
        _ => {}
    }

    if has_file_path {
        if args.mime_type.is_some() || args.file_name.is_some() {
            return Err(official_store_image_argument_error(
                "mime_type and file_name are only valid with image_base64",
            ));
        }
        let file_path = args.file_path.unwrap_or_default().trim().to_owned();
        return Ok(serde_json::json!({ "filePath": file_path }));
    }

    let image_base64 = args.image_base64.unwrap_or_default();
    let normalized_base64 = image_base64.trim();
    if normalized_base64.len() > OFFICIAL_STORE_IMAGE_MAX_BASE64_CHARS {
        return Err(official_store_image_argument_error(format!(
            "image_base64 exceeds {} decoded bytes",
            OFFICIAL_STORE_IMAGE_MAX_DECODED_BYTES
        )));
    }
    let mime_type = args
        .mime_type
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            official_store_image_argument_error("mime_type is required with image_base64")
        })?;
    if !matches!(mime_type, "image/png" | "image/jpeg" | "image/jpg") {
        return Err(official_store_image_argument_error(
            "mime_type must be image/png, image/jpeg, or image/jpg",
        ));
    }
    let mut arguments = serde_json::Map::new();
    arguments.insert(
        "imageBase64".to_owned(),
        Value::String(normalized_base64.to_owned()),
    );
    arguments.insert("mimeType".to_owned(), Value::String(mime_type.to_owned()));
    if let Some(file_name) = args.file_name {
        arguments.insert("fileName".to_owned(), Value::String(file_name));
    }
    Ok(Value::Object(arguments))
}

fn sanitize_identifier(label: &str, value: &str) -> Result<String> {
    let trimmed = value.trim();
    if trimmed.is_empty()
        || !trimmed
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '-')
    {
        return Err(eyre!("{label} must use [A-Za-z0-9_-] only").into());
    }
    Ok(trimmed.to_owned())
}

fn read_current_session_id(workspace: &Path) -> Result<String> {
    let path = workspace.join(".clock-p").join("current_session.json");
    let value = fs::read_to_string(&path)
        .map_err(|error| eyre!("failed to read {}: {error}", path.display()))?;
    let payload: Value = serde_json::from_str(&value)
        .map_err(|error| eyre!("invalid JSON in {}: {error}", path.display()))?;
    let session_id = payload
        .get("session_id")
        .and_then(Value::as_str)
        .ok_or_else(|| eyre!("current_session.json missing session_id"))?;
    sanitize_identifier("session_id", session_id)
}

fn normalize_tool_arguments_for_workspace(
    workspace: &Path,
    args: ToolArgumentValues,
) -> Result<ToolArgumentValues> {
    match args {
        ToolArgumentValues::TakeScreenshot(mut payload) => {
            let session_id = match payload.session_id.as_deref() {
                Some(value) => sanitize_identifier("session_id", value)?,
                None => read_current_session_id(workspace)?,
            };
            let runtime_id = match payload.runtime_id.as_deref() {
                Some(value) => sanitize_identifier("runtime_id", value)?,
                None => "server".to_owned(),
            };
            let tag = payload
                .tag
                .take()
                .map(|value| sanitize_identifier("tag", &value))
                .transpose()?;
            payload.session_id = Some(session_id);
            payload.runtime_id = Some(runtime_id);
            payload.tag = tag;
            Ok(ToolArgumentValues::TakeScreenshot(payload))
        }
        other => Ok(other),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::Mutex;

    #[test]
    fn stop_mode_gate_rejects_stale_reported_studio_mode_age() {
        let error = ensure_reported_age_fresh(
            "insert_model",
            "studio_mode",
            Some(HELPER_TASK_STATUS_STALE_AFTER.as_millis() + 1),
        )
        .unwrap_err();
        assert!(error.to_string().contains("studio_mode_snapshot_stale"));
    }

    #[test]
    fn hub_snapshot_parser_reads_claimed_helper_route_state() {
        let payload = HubStatusPayload {
            ok: true,
            tasks: vec![HubTaskPayload {
                task_id: "t_test".to_owned(),
                claimed_by_helper_id: Some("h_test".to_owned()),
                released: false,
                service_state: "ready".to_owned(),
                accepting_launches: true,
                services: HashMap::from([
                    ("rojo".to_owned(), "healthy".to_owned()),
                    ("mcp".to_owned(), "healthy".to_owned()),
                    ("runtime_log".to_owned(), "healthy".to_owned()),
                    ("rojo_public".to_owned(), "healthy".to_owned()),
                    ("mcp_public".to_owned(), "healthy".to_owned()),
                    ("runtime_log_public".to_owned(), "healthy".to_owned()),
                ]),
            }],
            helpers: vec![HubHelperPayload {
                helper_id: "h_test".to_owned(),
                blocked: false,
                last_seen_age_ms: Some(3),
                active_tasks: vec![HubHelperActiveTaskPayload {
                    task_id: "t_test".to_owned(),
                    remote_state: "connected".to_owned(),
                    studio_mode: Some("stop".to_owned()),
                    studio_mode_age_ms: Some(4),
                    studio_mode_source: Some("edit_plugin".to_owned()),
                    studio_control_state: Some("none".to_owned()),
                    studio_transition_phase: Some("idle".to_owned()),
                    studio_transition_age_ms: None,
                    edit_runtime_state: Some("ready".to_owned()),
                    edit_runtime_age_ms: Some(2),
                    studio_control_last_error: None,
                    official_mcp_adapter_state: Some("ready".to_owned()),
                    official_mcp_adapter_age_ms: Some(5),
                    official_mcp_adapter_last_error: None,
                }],
            }],
        };
        let snapshot = snapshot_from_hub_status_payload(payload, "t_test").unwrap();
        assert_eq!(snapshot.helper_id.as_deref(), Some("h_test"));
        assert_eq!(snapshot.remote_state.as_deref(), Some("connected"));
        assert!(snapshot.task_services_ready());
        assert!(snapshot.launch_ready());
    }

    #[test]
    fn hub_snapshot_requires_all_task_services_healthy_for_task_services_ready() {
        let payload = HubStatusPayload {
            ok: true,
            tasks: vec![HubTaskPayload {
                task_id: "t_test".to_owned(),
                claimed_by_helper_id: Some("h_test".to_owned()),
                released: false,
                service_state: "ready".to_owned(),
                accepting_launches: true,
                services: HashMap::from([
                    ("rojo".to_owned(), "healthy".to_owned()),
                    ("mcp".to_owned(), "healthy".to_owned()),
                    ("runtime_log".to_owned(), "healthy".to_owned()),
                    ("rojo_public".to_owned(), "healthy".to_owned()),
                    ("mcp_public".to_owned(), "healthy".to_owned()),
                    ("runtime_log_public".to_owned(), "error".to_owned()),
                ]),
            }],
            helpers: vec![HubHelperPayload {
                helper_id: "h_test".to_owned(),
                blocked: false,
                last_seen_age_ms: Some(3),
                active_tasks: vec![HubHelperActiveTaskPayload {
                    task_id: "t_test".to_owned(),
                    remote_state: "connected".to_owned(),
                    studio_mode: Some("stop".to_owned()),
                    studio_mode_age_ms: Some(4),
                    studio_mode_source: Some("edit_plugin".to_owned()),
                    studio_control_state: Some("none".to_owned()),
                    studio_transition_phase: Some("idle".to_owned()),
                    studio_transition_age_ms: None,
                    edit_runtime_state: Some("ready".to_owned()),
                    edit_runtime_age_ms: Some(2),
                    studio_control_last_error: None,
                    official_mcp_adapter_state: Some("ready".to_owned()),
                    official_mcp_adapter_age_ms: Some(5),
                    official_mcp_adapter_last_error: None,
                }],
            }],
        };
        let snapshot = snapshot_from_hub_status_payload(payload, "t_test").unwrap();
        assert!(!snapshot.task_services_ready());
        assert!(!snapshot.launch_ready());
    }

    #[test]
    fn hub_snapshot_ignores_legacy_studio_mirror_for_readiness() {
        let payload = HubStatusPayload {
            ok: true,
            tasks: vec![HubTaskPayload {
                task_id: "t_test".to_owned(),
                claimed_by_helper_id: Some("h_test".to_owned()),
                released: false,
                service_state: "ready".to_owned(),
                accepting_launches: true,
                services: HashMap::from([
                    ("rojo".to_owned(), "healthy".to_owned()),
                    ("mcp".to_owned(), "healthy".to_owned()),
                    ("runtime_log".to_owned(), "healthy".to_owned()),
                    ("rojo_public".to_owned(), "healthy".to_owned()),
                    ("mcp_public".to_owned(), "healthy".to_owned()),
                    ("runtime_log_public".to_owned(), "healthy".to_owned()),
                ]),
            }],
            helpers: vec![HubHelperPayload {
                helper_id: "h_test".to_owned(),
                blocked: false,
                last_seen_age_ms: Some(3),
                active_tasks: vec![HubHelperActiveTaskPayload {
                    task_id: "t_test".to_owned(),
                    remote_state: "connected".to_owned(),
                    studio_mode: Some("start_play".to_owned()),
                    studio_mode_age_ms: Some(4),
                    studio_mode_source: Some("play_control".to_owned()),
                    studio_control_state: Some("ready".to_owned()),
                    studio_transition_phase: Some("running".to_owned()),
                    studio_transition_age_ms: None,
                    edit_runtime_state: Some("stale".to_owned()),
                    edit_runtime_age_ms: Some(20_000),
                    studio_control_last_error: None,
                    official_mcp_adapter_state: Some("blocked_by_studio_mode".to_owned()),
                    official_mcp_adapter_age_ms: Some(5),
                    official_mcp_adapter_last_error: None,
                }],
            }],
        };
        let snapshot = snapshot_from_hub_status_payload(payload, "t_test").unwrap();

        assert!(snapshot.task_services_ready());
        assert!(snapshot.launch_ready());
    }

    fn local_task_status(
        studio_mode: &str,
        control_state: &str,
        transition_phase: &str,
    ) -> HelperTaskStatusSnapshot {
        HelperTaskStatusSnapshot {
            task_id: "t_test".to_owned(),
            studio_mode: Some(studio_mode.to_owned()),
            studio_mode_age_ms: Some(4),
            studio_mode_source: Some("play_control".to_owned()),
            studio_control_state: Some(control_state.to_owned()),
            studio_transition_phase: Some(transition_phase.to_owned()),
            studio_transition_age_ms: None,
            edit_runtime_state: Some("stale".to_owned()),
            edit_runtime_age_ms: Some(20_000),
            studio_control_last_error: None,
            official_mcp_adapter_state: Some("blocked_by_studio_mode".to_owned()),
            official_mcp_adapter_age_ms: Some(5),
            official_mcp_adapter_last_error: None,
        }
    }

    fn state_with_local_task_status(status: HelperTaskStatusSnapshot) -> AppState {
        let workspace = std::env::temp_dir().join(format!(
            "rbx-studio-mcp-local-status-test-{}",
            Uuid::new_v4()
        ));
        let (sender, _receiver) = mpsc::unbounded_channel();
        let mut state = AppState::new(
            workspace,
            Some("t_test".to_owned()),
            None,
            Some("http://127.0.0.1:1".to_owned()),
            None,
        );
        state.active_helper = Some(ActiveHelperConnection {
            connection_id: Uuid::new_v4(),
            helper_id: "h_test".to_owned(),
            place_id: "93795519121520".to_owned(),
            task_id: Some("t_test".to_owned()),
            capabilities: vec![],
            sender,
            last_message_at: Instant::now(),
            task_status: Some(status),
        });
        state
    }

    #[test]
    fn launch_preflight_allows_controlled_running_session() {
        let state =
            state_with_local_task_status(local_task_status("start_play", "ready", "running"));
        require_local_launch_ready_locked(&state, "t_test", "launch_studio_session").expect(
            "controlled running session can be stopped/relaunched by the high-level launch",
        );
    }

    #[test]
    fn launch_wait_does_not_require_edit_runtime_fields_when_already_running() {
        let mut state =
            state_with_local_task_status(local_task_status("start_play", "ready", "running"));
        let status = state
            .active_helper
            .as_mut()
            .and_then(|helper| helper.task_status.as_mut())
            .expect("test state should have helper task status");
        status.edit_runtime_state = None;
        status.edit_runtime_age_ms = None;

        assert_eq!(
            local_studio_gate_wait_reason(&state, "t_test", LocalStudioGateKind::Launch),
            None
        );
        require_local_launch_ready_locked(&state, "t_test", "launch_studio_session")
            .expect("controlled running launch should not require edit runtime fields");
    }

    #[test]
    fn launch_wait_requires_control_fields_when_already_running() {
        let mut state =
            state_with_local_task_status(local_task_status("start_play", "ready", "running"));
        let status = state
            .active_helper
            .as_mut()
            .and_then(|helper| helper.task_status.as_mut())
            .expect("test state should have helper task status");
        status.studio_control_state = None;

        assert_eq!(
            local_studio_gate_wait_reason(&state, "t_test", LocalStudioGateKind::Launch),
            Some("studio_control_unavailable")
        );
    }

    #[test]
    fn launch_preflight_rejects_uncontrolled_running_session() {
        let state = state_with_local_task_status(local_task_status("start_play", "lost", "error"));
        let error = require_local_launch_ready_locked(&state, "t_test", "launch_studio_session")
            .expect_err("uncontrolled running session cannot be relaunched safely");

        assert!(error.to_string().contains("uncontrolled_play_session"));
        assert!(error.to_string().contains("studio_control_state=lost"));
    }

    #[test]
    fn stop_preflight_allows_controlled_running_session() {
        let state =
            state_with_local_task_status(local_task_status("start_play", "ready", "running"));
        require_local_stop_control_ready_locked(&state, "t_test", "start_stop_play")
            .expect("controlled running session should be stoppable");
    }

    #[test]
    fn launch_preflight_allows_stable_edit_session() {
        let mut status = local_task_status("stop", "none", "idle");
        status.edit_runtime_state = Some("ready".to_owned());
        status.edit_runtime_age_ms = Some(4);
        let state = state_with_local_task_status(status);
        require_local_launch_ready_locked(&state, "t_test", "launch_studio_session")
            .expect("stable edit session should be launchable");
    }

    #[test]
    fn stop_preflight_rejects_uncontrolled_running_session() {
        let state = state_with_local_task_status(local_task_status("start_play", "lost", "error"));
        let error = require_local_stop_control_ready_locked(&state, "t_test", "start_stop_play")
            .expect_err("uncontrolled running session cannot be stopped safely");

        assert!(error.to_string().contains("uncontrolled_play_session"));
        assert!(error.to_string().contains("studio_control_state=lost"));
    }

    #[test]
    fn stop_preflight_rejects_stop_in_progress() {
        let state = state_with_local_task_status(local_task_status(
            "start_play",
            "stopping",
            "stopping_requested",
        ));
        let error = require_local_stop_control_ready_locked(&state, "t_test", "start_stop_play")
            .expect_err("stopping session must wait for stop/idle");

        assert!(error.to_string().contains("studio_stop_in_progress"));
        assert!(error.to_string().contains("stopping_requested"));

        let observed_state = state_with_local_task_status(local_task_status(
            "start_play",
            "stopping",
            "stopping_observed",
        ));
        let observed_error =
            require_local_stop_control_ready_locked(&observed_state, "t_test", "start_stop_play")
                .expect_err("observed stopping session must wait for stop/idle");

        assert!(observed_error
            .to_string()
            .contains("studio_stop_in_progress"));
        assert!(observed_error.to_string().contains("stopping_observed"));
    }

    #[test]
    fn active_helper_route_mismatch_is_rejected_before_dispatch() {
        let state =
            state_with_local_task_status(local_task_status("start_play", "ready", "running"));
        let helper = state
            .active_helper
            .as_ref()
            .expect("test state should have an active helper");
        let snapshot = HubTaskRouteSnapshot {
            claimed_by_helper_id: Some("h_other".to_owned()),
            helper_id: Some("h_other".to_owned()),
            helper_blocked: false,
            helper_last_seen_age_ms: Some(3),
            task_released: false,
            task_service_state: "ready".to_owned(),
            task_accepting_launches: true,
            task_services_healthy: true,
            remote_state: Some("connected".to_owned()),
        };
        let error =
            require_active_helper_matches_hub_route_locked(helper, &snapshot, "start_stop_play")
                .expect_err("route/helper mismatch must fail closed");

        assert!(error.to_string().contains("hub_route_helper_mismatch"));
        assert!(error.to_string().contains("active_helper_id=h_test"));
        assert!(error.to_string().contains("hub_helper_id=h_other"));
    }

    #[test]
    fn hub_snapshot_stale_helper_blocks_all_ready_states() {
        let payload = HubStatusPayload {
            ok: true,
            tasks: vec![HubTaskPayload {
                task_id: "t_test".to_owned(),
                claimed_by_helper_id: Some("h_test".to_owned()),
                released: false,
                service_state: "ready".to_owned(),
                accepting_launches: true,
                services: HashMap::from([
                    ("rojo".to_owned(), "healthy".to_owned()),
                    ("mcp".to_owned(), "healthy".to_owned()),
                    ("runtime_log".to_owned(), "healthy".to_owned()),
                    ("rojo_public".to_owned(), "healthy".to_owned()),
                    ("mcp_public".to_owned(), "healthy".to_owned()),
                    ("runtime_log_public".to_owned(), "healthy".to_owned()),
                ]),
            }],
            helpers: vec![HubHelperPayload {
                helper_id: "h_test".to_owned(),
                blocked: false,
                last_seen_age_ms: Some(HELPER_TASK_STATUS_STALE_AFTER.as_millis() + 1),
                active_tasks: vec![HubHelperActiveTaskPayload {
                    task_id: "t_test".to_owned(),
                    remote_state: "connected".to_owned(),
                    studio_mode: Some("stop".to_owned()),
                    studio_mode_age_ms: Some(4),
                    studio_mode_source: Some("edit_plugin".to_owned()),
                    studio_control_state: Some("none".to_owned()),
                    studio_transition_phase: Some("idle".to_owned()),
                    studio_transition_age_ms: None,
                    edit_runtime_state: Some("ready".to_owned()),
                    edit_runtime_age_ms: Some(2),
                    studio_control_last_error: None,
                    official_mcp_adapter_state: Some("ready".to_owned()),
                    official_mcp_adapter_age_ms: Some(5),
                    official_mcp_adapter_last_error: None,
                }],
            }],
        };
        let snapshot = snapshot_from_hub_status_payload(payload, "t_test").unwrap();

        assert!(snapshot.task_services_ready());
        assert!(!snapshot.launch_ready());
    }

    #[test]
    fn run_code_requires_edit_gate_unless_marked_diagnostic() {
        assert!(ToolArgumentValues::RunCode(RunCode {
            task_id: "t_test".to_owned(),
            command: "print('ok')".to_owned(),
            diagnostic: Some(true),
        })
        .local_studio_gate_kind()
        .is_none());
        assert!(matches!(
            ToolArgumentValues::RunCode(RunCode {
                task_id: "t_test".to_owned(),
                command: "workspace.Part:Destroy()".to_owned(),
                diagnostic: None,
            })
            .local_studio_gate_kind(),
            Some(LocalStudioGateKind::Edit)
        ));
    }

    #[test]
    fn studio_tools_use_distinct_local_gate_kinds() {
        assert!(matches!(
            ToolArgumentValues::StartStopPlay(StartStopPlay {
                task_id: "t_test".to_owned(),
                mode: "stop".to_owned(),
            })
            .local_studio_gate_kind(),
            Some(LocalStudioGateKind::StopControl)
        ));
        assert!(matches!(
            ToolArgumentValues::LaunchStudioSession(LaunchStudioSession {
                task_id: "t_test".to_owned(),
                mode: "start_play".to_owned(),
            })
            .local_studio_gate_kind(),
            Some(LocalStudioGateKind::Launch)
        ));
        assert!(matches!(
            ToolArgumentValues::InsertModel(InsertModel {
                task_id: "t_test".to_owned(),
                query: "tree".to_owned(),
            })
            .local_studio_gate_kind(),
            Some(LocalStudioGateKind::Edit)
        ));
    }

    #[test]
    fn only_stop_control_skips_hub_route_readiness_gate() {
        assert!(!local_gate_requires_hub_route_ready(
            LocalStudioGateKind::StopControl
        ));
        assert!(local_gate_requires_hub_route_ready(
            LocalStudioGateKind::Launch
        ));
        assert!(local_gate_requires_hub_route_ready(
            LocalStudioGateKind::Edit
        ));
        assert!(local_gate_requires_hub_route_ready(
            LocalStudioGateKind::Official
        ));
    }

    #[test]
    fn edit_gate_requires_edit_runtime_ready() {
        let mut status = local_task_status("stop", "none", "idle");
        status.edit_runtime_state = Some("missing".to_owned());
        status.edit_runtime_age_ms = Some(4);
        let state = state_with_local_task_status(status);
        let error = require_local_edit_ready_locked(&state, "t_test", "insert_model")
            .expect_err("edit tools require edit runtime readiness");

        assert!(error.to_string().contains("edit_runtime_not_ready"));
    }

    #[test]
    fn diagnostic_run_code_has_no_local_gate() {
        assert!(ToolArgumentValues::RunCode(RunCode {
            task_id: "t_test".to_owned(),
            command: "print('ok')".to_owned(),
            diagnostic: Some(true),
        })
        .local_studio_gate_kind()
        .is_none());
    }

    #[test]
    fn studio_control_tools_use_extended_helper_timeout() {
        let run_code_timeout = ToolArgumentValues::RunCode(RunCode {
            task_id: "t_test".to_owned(),
            command: "print('ok')".to_owned(),
            diagnostic: Some(true),
        })
        .helper_request_timeout();
        let launch_timeout = ToolArgumentValues::LaunchStudioSession(LaunchStudioSession {
            task_id: "t_test".to_owned(),
            mode: "start_play".to_owned(),
        })
        .helper_request_timeout();
        let stop_timeout = ToolArgumentValues::StartStopPlay(StartStopPlay {
            task_id: "t_test".to_owned(),
            mode: "stop".to_owned(),
        })
        .helper_request_timeout();
        assert_eq!(run_code_timeout, HELPER_REQUEST_TIMEOUT);
        assert_eq!(launch_timeout, HELPER_STUDIO_CONTROL_REQUEST_TIMEOUT);
        assert_eq!(stop_timeout, HELPER_STUDIO_CONTROL_REQUEST_TIMEOUT);
        assert!(launch_timeout > run_code_timeout);
    }

    #[test]
    fn start_stop_play_validates_as_stop_only() {
        ToolArgumentValues::StartStopPlay(StartStopPlay {
            task_id: "t_test".to_owned(),
            mode: "stop".to_owned(),
        })
        .validate()
        .expect("stop remains valid");

        let error = ToolArgumentValues::StartStopPlay(StartStopPlay {
            task_id: "t_test".to_owned(),
            mode: "start_play".to_owned(),
        })
        .validate()
        .expect_err("start_stop_play must not start Studio");

        assert!(error.to_string().contains("stop-only"));
    }

    fn test_state(workspace_name: &str) -> PackedState {
        let workspace = std::env::temp_dir().join(format!(
            "rbx-studio-mcp-test-{workspace_name}-{}",
            Uuid::new_v4()
        ));
        fs::create_dir_all(&workspace).expect("test workspace should be created");
        Arc::new(Mutex::new(AppState::new(workspace, None, None, None, None)))
    }

    #[tokio::test]
    async fn proxy_queue_reuses_start_stop_play_validation() {
        let state = test_state("proxy-validation");
        let result = queue_tool_request(
            Arc::clone(&state),
            ToolArgumentValues::StartStopPlay(StartStopPlay {
                task_id: "t_test".to_owned(),
                mode: "start_play".to_owned(),
            }),
            Some(Uuid::new_v4()),
            "proxy",
        )
        .await;
        let error = match result {
            Ok(_) => panic!("proxy must not bypass start_stop_play validation"),
            Err(error) => error,
        };

        assert!(error.to_string().contains("stop-only"));
        assert_eq!(state.lock().await.process_queue.len(), 0);
    }

    #[tokio::test]
    async fn proxy_queue_reuses_active_helper_task_id_gate() {
        let state = test_state("proxy-task-id");
        let (sender, _receiver) = mpsc::unbounded_channel();
        {
            let mut state = state.lock().await;
            state.active_helper = Some(ActiveHelperConnection {
                connection_id: Uuid::new_v4(),
                helper_id: "h_test".to_owned(),
                place_id: "93795519121520".to_owned(),
                task_id: Some("t_other".to_owned()),
                capabilities: vec![],
                sender,
                last_message_at: Instant::now(),
                task_status: None,
            });
        }

        let result = queue_tool_request(
            Arc::clone(&state),
            ToolArgumentValues::TakeScreenshot(TakeScreenshot {
                task_id: "t_test".to_owned(),
                session_id: Some("sess_test".to_owned()),
                runtime_id: Some("server".to_owned()),
                tag: None,
            }),
            Some(Uuid::new_v4()),
            "proxy",
        )
        .await;
        let error = match result {
            Ok(_) => panic!("proxy must not bypass active helper task_id gate"),
            Err(error) => error,
        };

        assert!(error.to_string().contains("task_id mismatch"));
        assert_eq!(state.lock().await.process_queue.len(), 0);
    }

    #[tokio::test]
    async fn proxy_queue_rejects_duplicate_request_id_without_overwrite() {
        let state = test_state("proxy-duplicate-id");
        let (helper_sender, _helper_receiver) = mpsc::unbounded_channel();
        let (pending_sender, _pending_receiver) = mpsc::unbounded_channel();
        let request_id = Uuid::new_v4();
        {
            let mut state = state.lock().await;
            state.active_helper = Some(ActiveHelperConnection {
                connection_id: Uuid::new_v4(),
                helper_id: "h_test".to_owned(),
                place_id: "93795519121520".to_owned(),
                task_id: Some("t_test".to_owned()),
                capabilities: vec![],
                sender: helper_sender,
                last_message_at: Instant::now(),
                task_status: None,
            });
            state.output_map.insert(request_id, pending_sender);
        }

        let result = queue_tool_request(
            Arc::clone(&state),
            ToolArgumentValues::TakeScreenshot(TakeScreenshot {
                task_id: "t_test".to_owned(),
                session_id: Some("sess_test".to_owned()),
                runtime_id: Some("server".to_owned()),
                tag: None,
            }),
            Some(request_id),
            "proxy",
        )
        .await;
        let error = match result {
            Ok(_) => panic!("proxy must reject duplicate request ids"),
            Err(error) => error,
        };

        let state = state.lock().await;
        assert!(error.to_string().contains("request id collision"));
        assert_eq!(state.output_map.len(), 1);
        assert_eq!(state.process_queue.len(), 0);
    }

    #[test]
    fn proxy_response_success_false_is_error_result() {
        let result = proxy_response_to_result(RunCommandResponse {
            success: false,
            response: "gate rejected request".to_owned(),
            id: Uuid::new_v4(),
        });

        let error = result.expect_err("proxy success=false must propagate as an error");
        assert!(error.to_string().contains("gate rejected request"));
    }

    #[test]
    fn store_image_accepts_base64_source_shape() {
        let arguments = official_store_image_arguments(OfficialMcpStoreImage {
            task_id: "t_test".to_owned(),
            file_path: None,
            image_base64: Some("abcd".to_owned()),
            mime_type: Some("image/png".to_owned()),
            file_name: Some("../probe.png".to_owned()),
        })
        .expect("base64 store_image arguments should be accepted by server preflight");

        assert_eq!(arguments["imageBase64"], "abcd");
        assert_eq!(arguments["mimeType"], "image/png");
        assert_eq!(arguments["fileName"], "../probe.png");
    }

    #[test]
    fn store_image_rejects_missing_or_duplicate_sources() {
        let missing = official_store_image_arguments(OfficialMcpStoreImage {
            task_id: "t_test".to_owned(),
            file_path: None,
            image_base64: None,
            mime_type: None,
            file_name: None,
        })
        .unwrap_err();
        assert!(missing
            .to_string()
            .contains("one of file_path or image_base64"));

        let duplicate = official_store_image_arguments(OfficialMcpStoreImage {
            task_id: "t_test".to_owned(),
            file_path: Some("C:\\tmp\\x.png".to_owned()),
            image_base64: Some("abcd".to_owned()),
            mime_type: Some("image/png".to_owned()),
            file_name: None,
        })
        .unwrap_err();
        assert!(duplicate.to_string().contains("mutually exclusive"));
    }
}

fn now_unix_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

fn sanitize_file_component(value: &str, fallback: &str) -> String {
    let mut result = String::new();
    for ch in value.chars() {
        if ch.is_ascii_alphanumeric() || ch == '_' || ch == '-' {
            result.push(ch);
        } else if !result.ends_with('-') {
            result.push('-');
        }
    }
    let trimmed = result.trim_matches('-');
    if trimmed.is_empty() {
        fallback.to_owned()
    } else {
        trimmed.to_owned()
    }
}

fn workspace_relative_path(workspace: &Path, path: &Path) -> String {
    path.strip_prefix(workspace)
        .map(|value| value.to_string_lossy().replace('\\', "/"))
        .unwrap_or_else(|_| path.to_string_lossy().into_owned())
}

fn ensure_session_metadata(
    workspace: &Path,
    session_id: &str,
    place_id: &str,
    task_id: Option<&str>,
) -> Result<(PathBuf, PathBuf, PathBuf, PathBuf)> {
    let artifact_dir = workspace
        .join(".clock-p")
        .join("artifacts")
        .join(session_id);
    let log_dir = artifact_dir.join("logs");
    let screenshot_dir = artifact_dir.join("screenshots");
    fs::create_dir_all(&log_dir)?;
    fs::create_dir_all(&screenshot_dir)?;
    let metadata_path = artifact_dir.join("session.json");
    if !metadata_path.exists() {
        let payload = serde_json::json!({
            "session_id": session_id,
            "place_id": place_id,
            "task_id": task_id,
            "workspace": workspace.to_string_lossy(),
            "created_at_unix_ms": now_unix_ms(),
            "artifact_dir": artifact_dir.to_string_lossy(),
            "log_dir": log_dir.to_string_lossy(),
            "screenshot_dir": screenshot_dir.to_string_lossy(),
        });
        fs::write(
            &metadata_path,
            format!("{}\n", serde_json::to_string_pretty(&payload)?),
        )?;
    }
    Ok((artifact_dir, log_dir, screenshot_dir, metadata_path))
}

fn remove_request_tracking(state: &mut AppState, request_id: Uuid) {
    state.output_map.remove(&request_id);
    state
        .process_queue
        .retain(|command| command.id != Some(request_id));
}

fn fail_request(state: &mut AppState, request_id: Uuid, message: &str) {
    let tx = state.output_map.remove(&request_id);
    state
        .process_queue
        .retain(|command| command.id != Some(request_id));
    if let Some(tx) = tx {
        let _ = tx.send(Err(Report::from(eyre!(message.to_owned()))));
    }
}

fn fail_all_pending(state: &mut AppState, message: &str) {
    for (_, tx) in state.output_map.drain() {
        let _ = tx.send(Err(Report::from(eyre!(message.to_owned()))));
    }
    state.process_queue.clear();
}

fn abort_all_uploads(uploads: &UploadRegistry) {
    let drained: Vec<UploadHandle> = {
        let mut uploads = uploads.lock().unwrap();
        uploads.drain().map(|(_, upload)| upload).collect()
    };
    for upload in drained {
        let temp_path = upload.lock().unwrap().temp_path.clone();
        let _ = fs::remove_file(temp_path);
    }
}

fn abort_uploads_for_request(uploads: &UploadRegistry, request_id: Uuid) {
    let removed: Vec<UploadHandle> = {
        let mut uploads = uploads.lock().unwrap();
        let upload_ids: Vec<Uuid> = uploads
            .iter()
            .filter_map(|(upload_id, upload)| {
                (upload.lock().unwrap().request_id == request_id).then_some(*upload_id)
            })
            .collect();
        upload_ids
            .into_iter()
            .filter_map(|upload_id| uploads.remove(&upload_id))
            .collect()
    };
    for upload in removed {
        let temp_path = upload.lock().unwrap().temp_path.clone();
        let _ = fs::remove_file(temp_path);
    }
}

fn touch_active_helper(state: &mut AppState, connection_id: Uuid) -> bool {
    match state.active_helper.as_mut() {
        Some(helper) if helper.connection_id == connection_id => {
            let idle_for = helper.last_message_at.elapsed();
            helper.last_message_at = Instant::now();
            if idle_for >= Duration::from_secs(10) {
                tracing::info!(
                    %connection_id,
                    place_id = helper.place_id,
                    idle_for_ms = idle_for.as_millis(),
                    "helper websocket traffic resumed after idle gap"
                );
            }
            true
        }
        _ => false,
    }
}

async fn helper_queue_loop(
    state: PackedState,
    connection_id: Uuid,
    sender: mpsc::UnboundedSender<OutgoingHelperFrame>,
) {
    let mut waiter = { state.lock().await.waiter.clone() };
    loop {
        let task = {
            let mut state = state.lock().await;
            match state.active_helper.as_ref() {
                Some(helper) if helper.connection_id == connection_id => {
                    state.process_queue.pop_front()
                }
                _ => return,
            }
        };

        if let Some(task) = task {
            let Some(id) = task.id else {
                continue;
            };
            let payload = match serde_json::to_value(&task) {
                Ok(payload) => payload,
                Err(error) => {
                    let mut state = state.lock().await;
                    fail_request(
                        &mut state,
                        id,
                        &format!("failed to encode tool call: {error}"),
                    );
                    continue;
                }
            };
            let message = ServerToHelperMessage::ToolCall {
                request_id: id.to_string(),
                command: payload,
            };
            let encoded = match serde_json::to_string(&message) {
                Ok(encoded) => encoded,
                Err(error) => {
                    let mut state = state.lock().await;
                    fail_request(
                        &mut state,
                        id,
                        &format!("failed to encode helper message: {error}"),
                    );
                    continue;
                }
            };
            if sender.send(OutgoingHelperFrame::Text(encoded)).is_err() {
                let mut state = state.lock().await;
                fail_request(
                    &mut state,
                    id,
                    "helper WebSocket sender dropped before tool dispatch",
                );
                return;
            }
            continue;
        }

        if waiter.changed().await.is_err() {
            return;
        }
    }
}

fn parse_request_uuid(value: &str) -> Result<Uuid> {
    Ok(Uuid::parse_str(value).map_err(|error| eyre!("invalid request_id {value}: {error}"))?)
}

fn prepare_artifact_upload(
    workspace: &Path,
    begin: ArtifactBegin,
) -> Result<PreparedArtifactUpload> {
    let upload_id = Uuid::parse_str(&begin.upload_id)
        .map_err(|error| eyre!("invalid upload_id {}: {error}", begin.upload_id))?;
    let request_id = parse_request_uuid(&begin.request_id)?;
    let session_id = sanitize_identifier("session_id", &begin.session_id)?;
    let runtime_id = sanitize_identifier("runtime_id", &begin.runtime_id)?;
    let place_id = sanitize_identifier("place_id", &begin.place_id)?;
    if begin.content_type != "image/png" {
        return Err(eyre!("artifact upload only supports image/png").into());
    }
    let (artifact_dir, _log_dir, screenshot_root, session_metadata_path) =
        ensure_session_metadata(workspace, &session_id, &place_id, begin.task_id.as_deref())?;
    let screenshot_dir = screenshot_root.join(&runtime_id);
    fs::create_dir_all(&screenshot_dir)?;
    let temp_path = screenshot_dir.join(format!(".upload-{upload_id}.part"));
    if temp_path.exists() {
        fs::remove_file(&temp_path)?;
    }
    Ok(PreparedArtifactUpload {
        upload_id,
        upload: ArtifactUploadState {
            request_id,
            session_id,
            runtime_id,
            place_id,
            task_id: begin.task_id,
            tag: begin.tag,
            temp_path,
            artifact_dir,
            screenshot_dir,
            session_metadata_path,
            total_bytes: begin.total_bytes,
            bytes_written: 0,
            expected_seq: 0,
        },
    })
}

fn register_artifact_upload(state: &AppState, prepared: PreparedArtifactUpload) -> Result<()> {
    if !state.output_map.contains_key(&prepared.upload.request_id) {
        let _ = fs::remove_file(&prepared.upload.temp_path);
        return Err(eyre!(
            "artifact upload request is not pending: {}",
            prepared.upload.request_id
        )
        .into());
    }
    state
        .uploads
        .lock()
        .unwrap()
        .insert(prepared.upload_id, Arc::new(StdMutex::new(prepared.upload)));
    Ok(())
}

fn handle_artifact_chunk(uploads: &UploadRegistry, payload: ArtifactChunk) -> Result<()> {
    if payload.data_base64.len() > MAX_ARTIFACT_CHUNK_MESSAGE_BYTES {
        return Err(eyre!(
            "artifact chunk exceeds {} encoded bytes",
            MAX_ARTIFACT_CHUNK_MESSAGE_BYTES
        )
        .into());
    }
    let upload_id = Uuid::parse_str(&payload.upload_id)
        .map_err(|error| eyre!("invalid upload_id {}: {error}", payload.upload_id))?;
    let seq = payload.seq;
    let chunk = base64::engine::general_purpose::STANDARD
        .decode(payload.data_base64)
        .map_err(|error| eyre!("artifact chunk base64 decode failed: {error}"))?;
    let upload = uploads
        .lock()
        .unwrap()
        .get(&upload_id)
        .cloned()
        .ok_or_else(|| eyre!("artifact chunk references unknown upload {upload_id}"))?;
    let mut upload = upload.lock().unwrap();
    if seq != upload.expected_seq {
        return Err(eyre!(
            "artifact chunk sequence mismatch for {upload_id}: expected {}, got {seq}",
            upload.expected_seq
        )
        .into());
    }
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&upload.temp_path)?;
    file.write_all(&chunk)?;
    upload.bytes_written += chunk.len();
    upload.expected_seq += 1;
    Ok(())
}

fn finalize_artifact_upload(
    workspace: &Path,
    uploads: &UploadRegistry,
    finish: ArtifactFinish,
) -> Result<ArtifactCommitted> {
    let upload_id = Uuid::parse_str(&finish.upload_id)
        .map_err(|error| eyre!("invalid upload_id {}: {error}", finish.upload_id))?;
    let upload = uploads.lock().unwrap().remove(&upload_id).ok_or_else(|| {
        eyre!(
            "artifact finish references unknown upload {}",
            finish.upload_id
        )
    })?;
    let upload = upload.lock().unwrap();
    let cleanup_temp = || {
        let _ = fs::remove_file(&upload.temp_path);
    };
    let finish_request_id = match parse_request_uuid(&finish.request_id) {
        Ok(request_id) => request_id,
        Err(error) => {
            cleanup_temp();
            return Err(error);
        }
    };
    if upload.request_id != finish_request_id {
        cleanup_temp();
        return Err(eyre!("artifact finish request_id mismatch").into());
    }
    if upload.expected_seq != finish.total_chunks {
        cleanup_temp();
        return Err(eyre!(
            "artifact finish chunk mismatch: expected {}, got {}",
            upload.expected_seq,
            finish.total_chunks
        )
        .into());
    }
    if upload.bytes_written != upload.total_bytes {
        cleanup_temp();
        return Err(eyre!(
            "artifact byte mismatch: expected {}, got {}",
            upload.total_bytes,
            upload.bytes_written
        )
        .into());
    }
    let mut file_name = format!("{}-{}", now_unix_ms(), &upload_id.to_string()[..8]);
    if let Some(tag) = upload.tag.as_deref() {
        file_name.push('-');
        file_name.push_str(&sanitize_file_component(tag, "shot"));
    }
    file_name.push_str(".png");
    let final_path = upload.screenshot_dir.join(file_name);
    if let Err(error) = fs::rename(&upload.temp_path, &final_path) {
        cleanup_temp();
        return Err(error.into());
    }
    Ok(ArtifactCommitted {
        upload_id: finish.upload_id,
        request_id: finish.request_id,
        session_id: upload.session_id.clone(),
        runtime_id: upload.runtime_id.clone(),
        place_id: upload.place_id.clone(),
        task_id: upload.task_id.clone(),
        screenshot_path: final_path.to_string_lossy().into_owned(),
        screenshot_rel_path: workspace_relative_path(workspace, &final_path),
        artifact_dir: upload.artifact_dir.to_string_lossy().into_owned(),
        session_metadata_path: upload.session_metadata_path.to_string_lossy().into_owned(),
        bytes_written: upload.bytes_written,
    })
}

fn committed_response_body(committed: &ArtifactCommitted) -> Result<String> {
    Ok(serde_json::to_string(&serde_json::json!({
        "session_id": committed.session_id,
        "runtime_id": committed.runtime_id,
        "place_id": committed.place_id,
        "task_id": committed.task_id,
        "screenshot_path": committed.screenshot_path,
        "screenshot_rel_path": committed.screenshot_rel_path,
        "artifact_dir": committed.artifact_dir,
        "session_metadata_path": committed.session_metadata_path,
        "bytes_written": committed.bytes_written,
    }))?)
}

fn remove_upload_by_id(uploads: &UploadRegistry, upload_id: &str) {
    if let Ok(parsed) = Uuid::parse_str(upload_id) {
        if let Some(upload) = uploads.lock().unwrap().remove(&parsed) {
            let temp_path = upload.lock().unwrap().temp_path.clone();
            let _ = fs::remove_file(temp_path);
        }
    }
}

pub async fn helper_ws_handler(
    ws: WebSocketUpgrade,
    State(state): State<PackedState>,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| helper_ws_session(socket, state))
}

async fn helper_ws_session(socket: WebSocket, state: PackedState) {
    let connection_id = Uuid::new_v4();
    let (mut writer, mut reader) = socket.split();
    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<OutgoingHelperFrame>();
    let writer_task = tokio::spawn(async move {
        while let Some(frame) = out_rx.recv().await {
            let message = match frame {
                OutgoingHelperFrame::Text(text) => Message::Text(text.into()),
            };
            if writer.send(message).await.is_err() {
                break;
            }
        }
    });

    let first_message = reader.next().await;
    let hello = match first_message {
        Some(Ok(Message::Text(text))) => match serde_json::from_str::<HelperToServerMessage>(&text)
        {
            Ok(HelperToServerMessage::Hello(hello)) => hello,
            Ok(_) => {
                let _ = out_tx.send(OutgoingHelperFrame::Text(
                    serde_json::to_string(&ServerToHelperMessage::CloseReason {
                        reason: "expected hello as first helper message".to_owned(),
                    })
                    .unwrap(),
                ));
                writer_task.abort();
                return;
            }
            Err(error) => {
                tracing::warn!(
                    error = summarize_text(&error.to_string()),
                    "failed to decode helper hello"
                );
                writer_task.abort();
                return;
            }
        },
        _ => {
            writer_task.abort();
            return;
        }
    };

    tracing::info!(connection_id = %connection_id, place_id = hello.place_id, task_id = ?hello.task_id, capabilities = ?hello.capabilities, "helper websocket connected");
    let uploads = { Arc::clone(&state.lock().await.uploads) };
    let workspace = { state.lock().await.workspace.clone() };
    {
        let mut state = state.lock().await;
        if let Some(previous) = state.active_helper.replace(ActiveHelperConnection {
            connection_id,
            helper_id: hello.helper_id.clone(),
            place_id: hello.place_id.clone(),
            task_id: hello.task_id.clone(),
            capabilities: hello.capabilities.clone(),
            sender: out_tx.clone(),
            last_message_at: Instant::now(),
            task_status: hello.task_status.clone(),
        }) {
            abort_all_uploads(&uploads);
            fail_all_pending(
                &mut state,
                "Studio helper connection was replaced by a newer session",
            );
            let _ = previous.sender.send(OutgoingHelperFrame::Text(
                serde_json::to_string(&ServerToHelperMessage::CloseReason {
                    reason: "replaced by newer helper connection".to_owned(),
                })
                .unwrap(),
            ));
        }
    }

    let _ = out_tx.send(OutgoingHelperFrame::Text(
        serde_json::to_string(&ServerToHelperMessage::ReadyAck {
            connection_id: connection_id.to_string(),
            place_id: hello.place_id.clone(),
            task_id: hello.task_id.clone(),
        })
        .unwrap(),
    ));
    let queue_task = tokio::spawn(helper_queue_loop(
        Arc::clone(&state),
        connection_id,
        out_tx.clone(),
    ));

    while let Some(message) = reader.next().await {
        match message {
            Ok(Message::Text(text)) => {
                let mut active_state = state.lock().await;
                if !touch_active_helper(&mut active_state, connection_id) {
                    break;
                }
                drop(active_state);
                let parsed = serde_json::from_str::<HelperToServerMessage>(&text);
                match parsed {
                    Ok(HelperToServerMessage::Heartbeat {
                        helper_id,
                        place_id,
                        task_id,
                        plugin_instance_count,
                        task_status,
                    }) => {
                        let studio_mode = task_status
                            .as_ref()
                            .and_then(|status| status.studio_mode.clone());
                        let studio_mode_age_ms = task_status
                            .as_ref()
                            .and_then(|status| status.studio_mode_age_ms);
                        let official_mcp_adapter_state = task_status
                            .as_ref()
                            .and_then(|status| status.official_mcp_adapter_state.clone());
                        let official_mcp_adapter_age_ms = task_status
                            .as_ref()
                            .and_then(|status| status.official_mcp_adapter_age_ms);
                        let mut state = state.lock().await;
                        if let Some(helper) = state.active_helper.as_mut() {
                            if helper.connection_id == connection_id {
                                helper.task_status = task_status;
                            }
                        }
                        tracing::debug!(
                            connection_id = %connection_id,
                            helper_id,
                            place_id,
                            task_id = ?task_id,
                            plugin_instance_count,
                            studio_mode = ?studio_mode,
                            studio_mode_age_ms = ?studio_mode_age_ms,
                            official_mcp_adapter_state = ?official_mcp_adapter_state,
                            official_mcp_adapter_age_ms = ?official_mcp_adapter_age_ms,
                            "received helper heartbeat"
                        );
                    }
                    Ok(HelperToServerMessage::ToolResult {
                        request_id,
                        response,
                    }) => match parse_request_uuid(&request_id) {
                        Ok(parsed_id) => {
                            let mut state = state.lock().await;
                            if let Some(tx) = state.output_map.remove(&parsed_id) {
                                let _ = tx.send(Ok(response));
                            }
                        }
                        Err(error) => tracing::warn!(
                            error = summarize_text(&error.to_string()),
                            "invalid helper tool result request_id"
                        ),
                    },
                    Ok(HelperToServerMessage::ToolError { request_id, error }) => {
                        match parse_request_uuid(&request_id) {
                            Ok(parsed_id) => {
                                let mut state = state.lock().await;
                                if let Some(tx) = state.output_map.remove(&parsed_id) {
                                    let _ = tx.send(Err(Report::from(eyre!(error))));
                                }
                            }
                            Err(error) => tracing::warn!(
                                error = summarize_text(&error.to_string()),
                                "invalid helper tool error request_id"
                            ),
                        }
                    }
                    Ok(HelperToServerMessage::ArtifactBegin(begin)) => {
                        let prepared = prepare_artifact_upload(&workspace, begin.clone());
                        let result = match prepared {
                            Ok(prepared) => {
                                let state = state.lock().await;
                                register_artifact_upload(&state, prepared)
                            }
                            Err(error) => Err(error),
                        };
                        if let Err(error) = result {
                            remove_upload_by_id(&uploads, &begin.upload_id);
                            let _ = out_tx.send(OutgoingHelperFrame::Text(
                                serde_json::to_string(&ServerToHelperMessage::ArtifactFailed {
                                    upload_id: begin.upload_id,
                                    request_id: begin.request_id,
                                    error: error.to_string(),
                                })
                                .unwrap(),
                            ));
                        }
                    }
                    Ok(HelperToServerMessage::ArtifactChunk(chunk)) => {
                        if let Err(error) = handle_artifact_chunk(&uploads, chunk.clone()) {
                            if let Ok(upload_id) = Uuid::parse_str(&chunk.upload_id) {
                                if let Some(upload) = uploads.lock().unwrap().remove(&upload_id) {
                                    let upload = upload.lock().unwrap();
                                    let request_id = upload.request_id;
                                    let temp_path = upload.temp_path.clone();
                                    let _ = fs::remove_file(temp_path);
                                    let _ = out_tx.send(OutgoingHelperFrame::Text(
                                        serde_json::to_string(
                                            &ServerToHelperMessage::ArtifactFailed {
                                                upload_id: upload_id.to_string(),
                                                request_id: request_id.to_string(),
                                                error: error.to_string(),
                                            },
                                        )
                                        .unwrap(),
                                    ));
                                }
                            }
                            tracing::warn!(
                                error = summarize_text(&error.to_string()),
                                "failed to process artifact chunk"
                            );
                        }
                    }
                    Ok(HelperToServerMessage::ArtifactFinish(finish)) => {
                        let result = finalize_artifact_upload(&workspace, &uploads, finish.clone());
                        match result {
                            Ok(committed) => {
                                let response_body = committed_response_body(&committed);
                                let request_id = parse_request_uuid(&committed.request_id);
                                match (request_id, response_body) {
                                    (Ok(request_id), Ok(response_body)) => {
                                        let pending_tx = {
                                            let mut state = state.lock().await;
                                            state.output_map.remove(&request_id)
                                        };
                                        if let Some(tx) = pending_tx {
                                            let _ = tx.send(Ok(response_body));
                                            let _ = out_tx.send(OutgoingHelperFrame::Text(
                                                serde_json::to_string(
                                                    &ServerToHelperMessage::ArtifactCommitted(
                                                        committed,
                                                    ),
                                                )
                                                .unwrap(),
                                            ));
                                        } else {
                                            let _ = fs::remove_file(&committed.screenshot_path);
                                            let _ = out_tx.send(OutgoingHelperFrame::Text(
                                                serde_json::to_string(
                                                    &ServerToHelperMessage::ArtifactFailed {
                                                        upload_id: finish.upload_id,
                                                        request_id: finish.request_id,
                                                        error:
                                                            "artifact request is no longer pending"
                                                                .to_owned(),
                                                    },
                                                )
                                                .unwrap(),
                                            ));
                                        }
                                    }
                                    (Err(error), _) | (_, Err(error)) => {
                                        let _ = fs::remove_file(&committed.screenshot_path);
                                        let _ = out_tx.send(OutgoingHelperFrame::Text(
                                            serde_json::to_string(
                                                &ServerToHelperMessage::ArtifactFailed {
                                                    upload_id: finish.upload_id,
                                                    request_id: finish.request_id,
                                                    error: error.to_string(),
                                                },
                                            )
                                            .unwrap(),
                                        ));
                                    }
                                }
                            }
                            Err(error) => {
                                remove_upload_by_id(&uploads, &finish.upload_id);
                                let _ = out_tx.send(OutgoingHelperFrame::Text(
                                    serde_json::to_string(&ServerToHelperMessage::ArtifactFailed {
                                        upload_id: finish.upload_id,
                                        request_id: finish.request_id,
                                        error: error.to_string(),
                                    })
                                    .unwrap(),
                                ));
                            }
                        }
                    }
                    Ok(HelperToServerMessage::ArtifactAbort(abort)) => {
                        remove_upload_by_id(&uploads, &abort.upload_id);
                        let mut state = state.lock().await;
                        if let Ok(request_id) = parse_request_uuid(&abort.request_id) {
                            fail_request(&mut state, request_id, &abort.error);
                        }
                    }
                    Ok(HelperToServerMessage::OfficialMcpResponse(response)) => {
                        match parse_request_uuid(&response.request_id) {
                            Ok(parsed_id) => {
                                let mut state = state.lock().await;
                                if let Some(tx) = state.output_map.remove(&parsed_id) {
                                    let _ = tx.send(Ok(response.response));
                                }
                            }
                            Err(error) => tracing::warn!(
                                error = summarize_text(&error.to_string()),
                                "invalid helper official MCP response request_id"
                            ),
                        }
                    }
                    Ok(HelperToServerMessage::OfficialMcpError { request_id, error }) => {
                        match parse_request_uuid(&request_id) {
                            Ok(parsed_id) => {
                                let mut state = state.lock().await;
                                if let Some(tx) = state.output_map.remove(&parsed_id) {
                                    let _ = tx.send(Err(Report::from(eyre!(error))));
                                }
                            }
                            Err(error) => tracing::warn!(
                                error = summarize_text(&error.to_string()),
                                "invalid helper official MCP error request_id"
                            ),
                        }
                    }
                    Ok(HelperToServerMessage::Hello(_)) => {}
                    Err(error) => {
                        tracing::warn!(
                            error = summarize_text(&error.to_string()),
                            "failed to decode helper ws message"
                        );
                    }
                }
            }
            Ok(Message::Binary(_)) => {}
            Ok(Message::Close(_)) => break,
            Ok(Message::Ping(_)) | Ok(Message::Pong(_)) => {
                let mut state = state.lock().await;
                if !touch_active_helper(&mut state, connection_id) {
                    break;
                }
            }
            Err(error) => {
                tracing::warn!(connection_id = %connection_id, error = summarize_text(&error.to_string()), "helper websocket errored");
                break;
            }
        }
    }

    queue_task.abort();
    writer_task.abort();
    let mut state = state.lock().await;
    if state
        .active_helper
        .as_ref()
        .map(|helper| helper.connection_id == connection_id)
        .unwrap_or(false)
    {
        state.active_helper = None;
        abort_all_uploads(&uploads);
        fail_all_pending(&mut state, "Studio helper WebSocket disconnected");
    }
    tracing::info!(connection_id = %connection_id, "helper websocket disconnected");
}

pub async fn helper_health_loop(state: PackedState) {
    let mut interval = tokio::time::interval(HELPER_HEALTH_CHECK_INTERVAL);
    loop {
        interval.tick().await;
        let stale_helper = {
            let mut state = state.lock().await;
            let Some(helper) = state.active_helper.as_ref() else {
                continue;
            };
            if helper.last_message_at.elapsed() <= HELPER_HEARTBEAT_TIMEOUT {
                continue;
            }
            let sender = helper.sender.clone();
            let uploads = Arc::clone(&state.uploads);
            let connection_id = helper.connection_id;
            let place_id = helper.place_id.clone();
            state.active_helper = None;
            fail_all_pending(&mut state, "Studio helper heartbeat timed out");
            Some((sender, uploads, connection_id, place_id))
        };
        if let Some((sender, uploads, connection_id, place_id)) = stale_helper {
            abort_all_uploads(&uploads);
            let _ = sender.send(OutgoingHelperFrame::Text(
                serde_json::to_string(&ServerToHelperMessage::CloseReason {
                    reason: "helper heartbeat timed out".to_owned(),
                })
                .unwrap(),
            ));
            let (queued_requests, pending_responses) = {
                let state = state.lock().await;
                (state.process_queue.len(), state.output_map.len())
            };
            tracing::warn!(
                %connection_id,
                place_id,
                timeout_secs = HELPER_HEARTBEAT_TIMEOUT.as_secs(),
                queued_requests,
                pending_responses,
                "helper websocket heartbeat timed out"
            );
        }
    }
}

pub async fn request_handler(State(state): State<PackedState>) -> Result<impl IntoResponse> {
    let timeout = tokio::time::timeout(LONG_POLL_DURATION, async {
        let mut waiter = { state.lock().await.waiter.clone() };
        loop {
            {
                let mut state = state.lock().await;
                if let Some(task) = state.process_queue.pop_front() {
                    tracing::info!(
                        id = ?task.id,
                        tool = task.tool_name(),
                        queued_requests = state.process_queue.len(),
                        pending_responses = state.output_map.len(),
                        "plugin long poll received queued tool"
                    );
                    return Ok::<ToolArguments, Error>(task);
                }
            }
            waiter.changed().await?
        }
    })
    .await;
    match timeout {
        Ok(result) => Ok(Json(result?).into_response()),
        _ => {
            tracing::debug!("plugin long poll timed out with no queued tool");
            Ok(StatusCode::NO_CONTENT.into_response())
        }
    }
}

pub async fn response_handler(
    State(state): State<PackedState>,
    Json(payload): Json<RunCommandResponse>,
) -> Result<impl IntoResponse> {
    tracing::info!(
        id = %payload.id,
        success = payload.success,
        response = summarize_text(&payload.response),
        "received plugin response"
    );
    let mut state = state.lock().await;
    let tx = state
        .output_map
        .remove(&payload.id)
        .ok_or_eyre("Unknown ID")?;
    let result: Result<String, Report> = if payload.success {
        Ok(payload.response)
    } else {
        Err(Report::from(eyre!(payload.response)))
    };
    Ok(tx.send(result)?)
}

pub async fn proxy_handler(
    State(state): State<PackedState>,
    Json(command): Json<ToolArguments>,
) -> Result<impl IntoResponse> {
    let id = command.id.ok_or_eyre("Got proxy command with no id")?;
    let args = command.args;
    let tool_name = args.tool_name();
    tracing::info!(%id, tool = tool_name, "proxy received tool request");
    let queued = match queue_tool_request(Arc::clone(&state), args, Some(id), "proxy").await {
        Ok(queued) => queued,
        Err(error) => {
            tracing::warn!(
                %id,
                error = summarize_text(&error.to_string()),
                "proxy rejected tool request before queue"
            );
            return Ok(Json(RunCommandResponse {
                success: false,
                response: error.to_string(),
                id,
            }));
        }
    };
    let tool_name = queued.tool_name;
    let helper_request_timeout = queued.helper_request_timeout;
    let uploads = queued.uploads;
    let mut rx = queued.rx;
    let result = match tokio::time::timeout(helper_request_timeout, rx.recv()).await {
        Ok(Some(result)) => result,
        Ok(None) => return Err(eyre!("Couldn't receive response").into()),
        Err(_) => {
            let mut state = state.lock().await;
            remove_request_tracking(&mut state, id);
            abort_uploads_for_request(&uploads, id);
            tracing::warn!(
                %id,
                tool = tool_name,
                timeout_ms = helper_request_timeout.as_millis(),
                "proxy timed out waiting for helper response"
            );
            return Ok(Json(RunCommandResponse {
                success: false,
                response: format!(
                    "Timed out waiting {}ms for {tool_name} response from Studio helper",
                    helper_request_timeout.as_millis()
                ),
                id,
            }));
        }
    };
    {
        let mut state = state.lock().await;
        state.output_map.remove_entry(&id);
    }
    let (success, response) = match result {
        Ok(s) => (true, s),
        Err(e) => (false, e.to_string()),
    };
    tracing::info!(
        %id,
        success,
        response = summarize_text(&response),
        "proxy returning tool response"
    );
    Ok(Json(RunCommandResponse {
        success,
        response,
        id,
    }))
}

fn proxy_response_to_result(response: RunCommandResponse) -> Result<String> {
    if response.success {
        Ok(response.response)
    } else {
        Err(eyre!(response.response).into())
    }
}

pub async fn dud_proxy_loop(state: PackedState, exit: Receiver<()>, plugin_port: u16) {
    let client = reqwest::Client::new();

    let mut waiter = { state.lock().await.waiter.clone() };
    while exit.is_empty() {
        let entry = { state.lock().await.process_queue.pop_front() };
        if let Some(entry) = entry {
            let id = entry.id.unwrap();
            let tool_name = entry.tool_name();
            tracing::info!(%id, tool = tool_name, plugin_port, "proxy forwarding tool to busy plugin port");
            let res = client
                .post(format!("http://127.0.0.1:{plugin_port}/proxy"))
                .json(&entry)
                .send()
                .await;
            if let Ok(res) = res {
                let tx = { state.lock().await.output_map.remove(&id).unwrap() };
                let res = match res.json::<RunCommandResponse>().await {
                    Ok(response) => proxy_response_to_result(response),
                    Err(error) => Err(Report::from(error)),
                };
                match &res {
                    Ok(body) => tracing::info!(
                        %id,
                        tool = tool_name,
                        response = summarize_text(body),
                        "proxy received plugin response"
                    ),
                    Err(err) => tracing::warn!(
                        %id,
                        tool = tool_name,
                        error = summarize_text(&err.to_string()),
                        "proxy failed to decode plugin response"
                    ),
                }
                tx.send(res).unwrap();
            } else {
                tracing::error!("Failed to proxy: {res:?}");
            };
        } else {
            waiter.changed().await.unwrap();
        }
    }
}
