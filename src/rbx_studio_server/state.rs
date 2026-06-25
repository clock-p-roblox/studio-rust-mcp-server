use super::{ActiveHelperConnection, ArtifactUploadState, ToolArgumentValues};
use crate::error::Result;
use crate::helper_ws::RuntimeLogForwardStatusSnapshot;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;
use tokio::sync::{mpsc, watch, Mutex};
use uuid::Uuid;

pub(super) const LONG_POLL_DURATION: Duration = Duration::from_secs(15);
pub(super) const HELPER_REQUEST_TIMEOUT: Duration = Duration::from_secs(60);
pub(super) const HELPER_STUDIO_CONTROL_REQUEST_TIMEOUT: Duration = Duration::from_secs(155);
pub(super) const HELPER_HEALTH_CHECK_INTERVAL: Duration = Duration::from_secs(5);
pub(super) const HELPER_HEARTBEAT_TIMEOUT: Duration = Duration::from_secs(20);
pub(super) const HELPER_TASK_STATUS_STALE_AFTER: Duration = Duration::from_secs(10);
pub(super) const LOCAL_STUDIO_STATUS_WAIT_TIMEOUT: Duration = Duration::from_secs(2);
pub(super) const HUB_STATUS_TIMEOUT: Duration = Duration::from_secs(5);
pub(super) const REQUIRED_TASK_SERVICE_NAMES: &[&str] = &[
    "rojo",
    "mcp",
    "runtime_log",
    "rojo_public",
    "mcp_public",
    "runtime_log_public",
];
pub(super) const OFFICIAL_MCP_LONG_TIMEOUT_MS: u64 = 1_800_000;
pub(super) const OFFICIAL_MCP_STORE_IMAGE_TIMEOUT_MS: u64 = 1_800_000;
pub(super) const OFFICIAL_STORE_IMAGE_MAX_DECODED_BYTES: usize = 20 * 1024 * 1024;
pub(super) const OFFICIAL_STORE_IMAGE_MAX_BASE64_CHARS: usize =
    OFFICIAL_STORE_IMAGE_MAX_DECODED_BYTES.div_ceil(3) * 4;

pub(super) type UploadHandle = Arc<StdMutex<ArtifactUploadState>>;
pub(super) type UploadRegistry = Arc<StdMutex<HashMap<Uuid, UploadHandle>>>;

pub(super) fn summarize_text(value: &str) -> String {
    const LIMIT: usize = 180;
    let trimmed = value.trim();
    if trimmed.len() <= LIMIT {
        trimmed.to_owned()
    } else {
        format!("{}...", &trimmed[..LIMIT])
    }
}

#[derive(Deserialize, Serialize, Clone, Debug)]
pub struct ToolArguments {
    pub(super) args: ToolArgumentValues,
    pub(super) id: Option<Uuid>,
}

#[derive(Deserialize, Serialize, Clone, Debug)]
pub struct RunCommandResponse {
    pub(super) success: bool,
    pub(super) response: String,
    pub(super) id: Uuid,
}

pub struct AppState {
    pub(super) workspace: PathBuf,
    pub(super) task_id: Option<String>,
    pub(super) public_base_url: Option<String>,
    pub(super) hub_status_client: Option<HubStatusClient>,
    pub(super) process_queue: VecDeque<ToolArguments>,
    pub(super) output_map: HashMap<Uuid, mpsc::UnboundedSender<Result<String>>>,
    pub(super) active_helper: Option<ActiveHelperConnection>,
    pub(super) uploads: UploadRegistry,
    pub(super) waiter: watch::Receiver<()>,
    pub(super) trigger: watch::Sender<()>,
}
pub type PackedState = Arc<Mutex<AppState>>;

#[derive(Serialize)]
pub struct StatusResponse {
    pub(super) service: &'static str,
    pub(super) workspace: String,
    pub(super) task_id: Option<String>,
    pub(super) public_base_url: Option<String>,
    pub(super) queued_requests: usize,
    pub(super) pending_responses: usize,
    pub(super) helper_connected: bool,
    pub(super) helper_place_id: Option<String>,
    pub(super) helper_task_id: Option<String>,
    pub(super) helper_connection_id: Option<String>,
    pub(super) helper_capabilities: Option<Vec<String>>,
    pub(super) status_source: String,
    pub(super) route_status_source: String,
    pub(super) studio_status_source: String,
    pub(super) hub_base_url: Option<String>,
    pub(super) hub_snapshot_error: Option<String>,
    pub(super) task_services_ready: bool,
    pub(super) launch_ready: bool,
    pub(super) edit_ready: bool,
    pub(super) official_ready: bool,
    pub(super) studio_session_state: Option<String>,
    pub(super) last_known_session_state: Option<String>,
    pub(super) last_session_error_reason: Option<String>,
    pub(super) active_stop_request_id: Option<u64>,
    pub(super) last_stop_request_id: Option<u64>,
    pub(super) stop_request_recorded_age_ms: Option<u128>,
    pub(super) runtime_actuator_last_poll_id: Option<u64>,
    pub(super) runtime_actuator_last_poll_age_ms: Option<u128>,
    pub(super) stop_result_phase: Option<String>,
    pub(super) stop_result_age_ms: Option<u128>,
    pub(super) stop_result_error: Option<String>,
    pub(super) studio_mode: Option<String>,
    pub(super) studio_mode_age_ms: Option<u128>,
    pub(super) studio_mode_source: Option<String>,
    pub(super) studio_control_state: Option<String>,
    pub(super) studio_transition_phase: Option<String>,
    pub(super) studio_transition_age_ms: Option<u128>,
    pub(super) edit_runtime_state: Option<String>,
    pub(super) edit_runtime_age_ms: Option<u128>,
    pub(super) studio_control_last_error: Option<String>,
    pub(super) official_mcp_adapter_state: String,
    pub(super) official_mcp_adapter_age_ms: Option<u128>,
    pub(super) official_mcp_adapter_last_error: Option<String>,
    pub(super) runtime_log_forward: Option<RuntimeLogForwardStatusSnapshot>,
    pub(super) helper_last_message_age_ms: Option<u128>,
}

#[derive(Clone)]
pub struct HubStatusClient {
    pub(super) base_url: String,
    pub(super) bearer_token: Option<String>,
    pub(super) client: reqwest::Client,
}
