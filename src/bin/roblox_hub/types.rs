use clap::Parser;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{Mutex, RwLock};

pub(super) const DEFAULT_HUB_PORT: u16 = 44758;
pub(super) const HEARTBEAT_INTERVAL_SECS: u64 = 10;

#[derive(Debug, Clone)]
pub(super) struct HubConfig {
    pub(super) state_file: PathBuf,
    pub(super) task_heartbeat_timeout: Duration,
    pub(super) helper_heartbeat_timeout: Duration,
    pub(super) heartbeat_interval_sec: u64,
}

#[derive(Parser, Debug)]
#[command(
    version,
    about = "Task/helper control plane for clock-p Roblox debug clusters"
)]
pub(super) struct Args {
    #[arg(long, default_value_t = DEFAULT_HUB_PORT)]
    pub(super) port: u16,

    #[arg(long, default_value_t = false)]
    pub(super) no_auth: bool,

    #[arg(long)]
    pub(super) bearer_token: Option<String>,

    #[arg(long)]
    pub(super) bearer_token_file: Option<PathBuf>,

    #[arg(long)]
    pub(super) state_file: Option<PathBuf>,

    #[arg(long, default_value_t = HEARTBEAT_INTERVAL_SECS)]
    pub(super) heartbeat_interval_sec: u64,

    #[arg(long, default_value_t = 30)]
    pub(super) task_heartbeat_timeout_sec: u64,

    #[arg(long, default_value_t = 30)]
    pub(super) helper_heartbeat_timeout_sec: u64,

    #[arg(long, default_value_t = false)]
    pub(super) allow_helper_admin: bool,
}

#[derive(Clone)]
pub(super) struct HttpAuthState {
    pub(super) bearer_token: Option<String>,
}

#[derive(Debug)]
pub(super) struct HubState {
    pub(super) config: HubConfig,
    pub(super) helpers: HashMap<String, HelperRecord>,
    pub(super) blocked_helpers: HashMap<String, BlockedHelperRecord>,
    pub(super) tasks: HashMap<String, TaskRecord>,
    pub(super) state_revision: u64,
}

pub(super) type SharedHubState = Arc<RwLock<HubState>>;
pub(super) type SharedPersistLock = Arc<Mutex<()>>;

#[derive(Clone)]
pub(super) struct AppState {
    pub(super) hub: SharedHubState,
    pub(super) persist_lock: SharedPersistLock,
    pub(super) allow_helper_admin: bool,
}

#[derive(Debug)]
pub(super) struct HelperRecord {
    pub(super) helper_id: String,
    pub(super) owner_user: String,
    pub(super) platform: String,
    pub(super) capacity: usize,
    pub(super) labels: Vec<String>,
    pub(super) active_task_ids: HashSet<String>,
    pub(super) active_tasks: HashMap<String, HelperActiveTaskRecord>,
    pub(super) registered_at_unix_ms: u64,
    pub(super) last_seen_at: Instant,
}

#[derive(Debug, Clone)]
pub(super) struct HelperActiveTaskRecord {
    pub(super) task_id: String,
    pub(super) remote_state: String,
    pub(super) remote_connection_id: Option<String>,
    pub(super) remote_last_error: Option<String>,
    pub(super) remote_last_ready_age_ms: Option<u128>,
    pub(super) remote_last_server_message_age_ms: Option<u128>,
    pub(super) studio_mode: Option<String>,
    pub(super) studio_mode_age_ms: Option<u128>,
    pub(super) studio_mode_source: Option<String>,
    pub(super) studio_control_state: Option<String>,
    pub(super) studio_transition_phase: Option<String>,
    pub(super) studio_transition_age_ms: Option<u128>,
    pub(super) edit_runtime_state: Option<String>,
    pub(super) edit_runtime_age_ms: Option<u128>,
    pub(super) studio_control_last_error: Option<String>,
    pub(super) official_mcp_adapter_state: Option<String>,
    pub(super) official_mcp_adapter_age_ms: Option<u128>,
    pub(super) official_mcp_adapter_last_error: Option<String>,
}

#[derive(Debug, Clone)]
pub(super) struct BlockedHelperRecord {
    pub(super) helper_id: String,
    pub(super) reason: Option<String>,
    pub(super) blocked_at_unix_ms: u64,
    pub(super) drain_started_at_unix_ms: u64,
    pub(super) drain_deadline_at_unix_ms: u64,
    pub(super) pending_task_ids: HashSet<String>,
    pub(super) last_reported_task_ids: HashSet<String>,
    pub(super) last_release_requested_at_unix_ms: Option<u64>,
    pub(super) last_seen_at_unix_ms: Option<u64>,
    pub(super) force_released_task_ids: HashSet<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub(super) struct TaskRoutes {
    pub(super) rojo_base_url: Option<String>,
    pub(super) mcp_base_url: Option<String>,
    pub(super) runtime_log_base_url: Option<String>,
}

#[derive(Debug)]
pub(super) struct TaskRecord {
    pub(super) task_id: String,
    pub(super) cluster_key: String,
    pub(super) task_token: String,
    pub(super) recover_token: String,
    pub(super) place_id: String,
    pub(super) universe_id: Option<String>,
    pub(super) owner_user: String,
    pub(super) repo: String,
    pub(super) worktree_name: String,
    pub(super) service_state: String,
    pub(super) accepting_launches: bool,
    pub(super) services: HashMap<String, String>,
    pub(super) routes: TaskRoutes,
    pub(super) created_at_unix_ms: u64,
    pub(super) updated_at_unix_ms: u64,
    pub(super) last_task_heartbeat_at_unix_ms: u64,
    pub(super) last_seen_at: Instant,
    pub(super) claimed_by_helper_id: Option<String>,
    pub(super) released: bool,
}

#[derive(Debug, Serialize, Deserialize)]
pub(super) struct PersistedHubState {
    pub(super) tasks: Vec<PersistedTaskRecord>,
    #[serde(default)]
    pub(super) blocked_helpers: Vec<PersistedBlockedHelperRecord>,
}

#[derive(Debug, Serialize, Deserialize)]
pub(super) struct PersistedBlockedHelperRecord {
    pub(super) helper_id: String,
    #[serde(default)]
    pub(super) reason: Option<String>,
    pub(super) blocked_at_unix_ms: u64,
    #[serde(default)]
    pub(super) drain_started_at_unix_ms: u64,
    #[serde(default)]
    pub(super) drain_deadline_at_unix_ms: u64,
    #[serde(default)]
    pub(super) pending_task_ids: Vec<String>,
    #[serde(default)]
    pub(super) last_reported_task_ids: Vec<String>,
    #[serde(default)]
    pub(super) last_release_requested_at_unix_ms: Option<u64>,
    #[serde(default)]
    pub(super) last_seen_at_unix_ms: Option<u64>,
    #[serde(default)]
    pub(super) force_released_task_ids: Vec<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub(super) struct PersistedTaskRecord {
    pub(super) task_id: String,
    pub(super) cluster_key: String,
    pub(super) task_token: String,
    pub(super) recover_token: String,
    pub(super) place_id: String,
    #[serde(default, alias = "game_id")]
    pub(super) universe_id: Option<String>,
    pub(super) owner_user: String,
    pub(super) repo: String,
    pub(super) worktree_name: String,
    pub(super) service_state: String,
    pub(super) accepting_launches: bool,
    pub(super) services: HashMap<String, String>,
    pub(super) routes: TaskRoutes,
    pub(super) created_at_unix_ms: u64,
    pub(super) updated_at_unix_ms: u64,
    #[serde(default)]
    pub(super) last_task_heartbeat_at_unix_ms: u64,
    pub(super) claimed_by_helper_id: Option<String>,
    pub(super) released: bool,
}

pub(super) struct PersistSnapshot {
    pub(super) revision: u64,
    pub(super) state_file: PathBuf,
    pub(super) payload: PersistedHubState,
}

#[derive(Debug, Deserialize)]
pub(super) struct CreateTaskRequest {
    pub(super) cluster_key: String,
    pub(super) owner_user: String,
    pub(super) repo: String,
    pub(super) worktree_name: String,
    pub(super) place_id: String,
    #[serde(default)]
    pub(super) universe_id: Option<String>,
    #[serde(default, rename = "game_id")]
    pub(super) legacy_game_id: Option<String>,
}

#[derive(Debug, Serialize)]
pub(super) struct CreateTaskResponse {
    pub(super) task_id: String,
    pub(super) task_token: String,
    pub(super) recover_token: String,
    pub(super) heartbeat_interval_sec: u64,
    pub(super) heartbeat_timeout_sec: u64,
}

#[derive(Debug, Deserialize)]
pub(super) struct RecoverTaskRequest {
    pub(super) cluster_key: String,
    pub(super) task_id: String,
    pub(super) recover_token: String,
    #[serde(default)]
    pub(super) universe_id: Option<String>,
    #[serde(default, rename = "game_id")]
    pub(super) legacy_game_id: Option<String>,
}

#[derive(Debug, Deserialize)]
pub(super) struct TaskHeartbeatRequest {
    pub(super) task_id: String,
    pub(super) task_token: String,
    pub(super) service_state: String,
    pub(super) accepting_launches: bool,
    #[serde(default)]
    pub(super) routes: TaskRoutes,
    #[serde(default)]
    pub(super) services: HashMap<String, String>,
}

#[derive(Debug, Deserialize)]
pub(super) struct ReleaseTaskRequest {
    pub(super) task_id: String,
    pub(super) task_token: String,
}

#[derive(Debug, Deserialize)]
pub(super) struct RegisterHelperRequest {
    pub(super) helper_id: String,
    pub(super) owner_user: String,
    pub(super) platform: String,
    pub(super) capacity: usize,
    #[serde(default)]
    pub(super) labels: Vec<String>,
}

#[derive(Debug, Serialize)]
pub(super) struct RegisterHelperResponse {
    pub(super) helper_id: String,
    pub(super) heartbeat_interval_sec: u64,
    pub(super) heartbeat_timeout_sec: u64,
}

#[derive(Debug, Deserialize)]
pub(super) struct HelperHeartbeatRequest {
    pub(super) helper_id: String,
    #[serde(default)]
    pub(super) active_task_ids: Vec<String>,
    #[serde(default)]
    pub(super) active_tasks: Vec<HelperActiveTaskHeartbeat>,
}

#[derive(Debug, Deserialize)]
pub(super) struct HelperActiveTaskHeartbeat {
    pub(super) task_id: String,
    pub(super) remote_state: String,
    #[serde(default)]
    pub(super) remote_connection_id: Option<String>,
    #[serde(default)]
    pub(super) remote_last_error: Option<String>,
    #[serde(default)]
    pub(super) remote_last_ready_age_ms: Option<u128>,
    #[serde(default)]
    pub(super) remote_last_server_message_age_ms: Option<u128>,
    #[serde(default)]
    pub(super) studio_mode: Option<String>,
    #[serde(default)]
    pub(super) studio_mode_age_ms: Option<u128>,
    #[serde(default)]
    pub(super) studio_mode_source: Option<String>,
    #[serde(default)]
    pub(super) studio_control_state: Option<String>,
    #[serde(default)]
    pub(super) studio_transition_phase: Option<String>,
    #[serde(default)]
    pub(super) studio_transition_age_ms: Option<u128>,
    #[serde(default)]
    pub(super) edit_runtime_state: Option<String>,
    #[serde(default)]
    pub(super) edit_runtime_age_ms: Option<u128>,
    #[serde(default)]
    pub(super) studio_control_last_error: Option<String>,
    #[serde(default)]
    pub(super) official_mcp_adapter_state: Option<String>,
    #[serde(default)]
    pub(super) official_mcp_adapter_age_ms: Option<u128>,
    #[serde(default)]
    pub(super) official_mcp_adapter_last_error: Option<String>,
}

#[derive(Debug, Deserialize)]
pub(super) struct BlockHelperRequest {
    pub(super) helper_id: String,
    #[serde(default)]
    pub(super) reason: Option<String>,
}

#[derive(Debug, Deserialize)]
pub(super) struct UnblockHelperRequest {
    pub(super) helper_id: String,
}

#[derive(Debug, Serialize)]
pub(super) struct HelperHeartbeatHubResponse {
    pub(super) ok: bool,
    pub(super) release_task_ids: Vec<String>,
}

#[derive(Debug, Deserialize)]
pub(super) struct ClaimTaskRequest {
    pub(super) helper_id: String,
}

#[derive(Debug, Serialize)]
pub(super) struct ClaimTaskResponse {
    pub(super) claimed: bool,
    pub(super) helper_id: String,
    pub(super) task: Option<ClaimedTaskPayload>,
}

#[derive(Debug, Serialize)]
pub(super) struct ClaimedTaskPayload {
    pub(super) task_id: String,
    pub(super) place_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) universe_id: Option<String>,
    pub(super) owner_user: String,
    pub(super) repo: String,
    pub(super) worktree_name: String,
    pub(super) routes: TaskRoutes,
}

#[derive(Debug, Serialize)]
pub(super) struct HubStatusResponse {
    pub(super) ok: bool,
    pub(super) helper_count: usize,
    pub(super) task_count: usize,
    pub(super) helpers: Vec<HelperStatusPayload>,
    pub(super) blocked_helpers: Vec<BlockedHelperPayload>,
    pub(super) tasks: Vec<TaskStatusPayload>,
}

#[derive(Debug, Serialize)]
pub(super) struct HelperListResponse {
    pub(super) ok: bool,
    pub(super) helper_count: usize,
    pub(super) blocked_helper_count: usize,
    pub(super) helpers: Vec<HelperStatusPayload>,
    pub(super) blocked_helpers: Vec<BlockedHelperPayload>,
}

#[derive(Debug, Serialize)]
pub(super) struct HelperStatusPayload {
    pub(super) helper_id: String,
    pub(super) owner_user: String,
    pub(super) platform: String,
    pub(super) capacity: usize,
    pub(super) active_launch_count: usize,
    pub(super) active_tasks: Vec<HelperActiveTaskPayload>,
    pub(super) labels: Vec<String>,
    pub(super) blocked: bool,
    pub(super) registered_at_unix_ms: u64,
    pub(super) last_seen_age_ms: u128,
}

#[derive(Debug, Serialize)]
pub(super) struct HelperActiveTaskPayload {
    pub(super) task_id: String,
    pub(super) remote_state: String,
    pub(super) remote_connection_id: Option<String>,
    pub(super) remote_last_error: Option<String>,
    pub(super) remote_last_ready_age_ms: Option<u128>,
    pub(super) remote_last_server_message_age_ms: Option<u128>,
    pub(super) studio_mode: Option<String>,
    pub(super) studio_mode_age_ms: Option<u128>,
    pub(super) studio_mode_source: Option<String>,
    pub(super) studio_control_state: Option<String>,
    pub(super) studio_transition_phase: Option<String>,
    pub(super) studio_transition_age_ms: Option<u128>,
    pub(super) edit_runtime_state: Option<String>,
    pub(super) edit_runtime_age_ms: Option<u128>,
    pub(super) studio_control_last_error: Option<String>,
    pub(super) official_mcp_adapter_state: Option<String>,
    pub(super) official_mcp_adapter_age_ms: Option<u128>,
    pub(super) official_mcp_adapter_last_error: Option<String>,
}

#[derive(Debug, Serialize)]
pub(super) struct BlockedHelperPayload {
    pub(super) helper_id: String,
    pub(super) reason: Option<String>,
    pub(super) blocked_at_unix_ms: u64,
    pub(super) drain_status: String,
    pub(super) drain_started_at_unix_ms: u64,
    pub(super) drain_deadline_at_unix_ms: u64,
    pub(super) drain_deadline_elapsed: bool,
    pub(super) drain_deadline_remaining_ms: u64,
    pub(super) drain_age_ms: u64,
    pub(super) pending_task_ids: Vec<String>,
    pub(super) last_reported_task_ids: Vec<String>,
    pub(super) last_release_requested_at_unix_ms: Option<u64>,
    pub(super) last_seen_age_ms: Option<u64>,
    pub(super) force_released_task_ids: Vec<String>,
}

#[derive(Debug, Serialize)]
pub(super) struct BlockHelperResponse {
    pub(super) ok: bool,
    pub(super) helper_id: String,
    pub(super) blocked: bool,
}

#[derive(Debug, Serialize)]
pub(super) struct TaskStatusPayload {
    pub(super) task_id: String,
    pub(super) cluster_key: String,
    pub(super) place_id: String,
    pub(super) universe_id: Option<String>,
    pub(super) owner_user: String,
    pub(super) repo: String,
    pub(super) worktree_name: String,
    pub(super) service_state: String,
    pub(super) accepting_launches: bool,
    pub(super) claimed_by_helper_id: Option<String>,
    pub(super) released: bool,
    pub(super) routes: TaskRoutes,
    pub(super) services: HashMap<String, String>,
    pub(super) created_at_unix_ms: u64,
    pub(super) updated_at_unix_ms: u64,
    pub(super) last_task_heartbeat_at_unix_ms: u64,
    pub(super) last_seen_age_ms: u128,
}
