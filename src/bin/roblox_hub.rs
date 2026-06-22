use axum::{
    extract::{Path, Request, State},
    http::{header, HeaderValue, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use clap::Parser;
use color_eyre::eyre::{eyre, Result, WrapErr};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io;
use std::net::Ipv4Addr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::sync::{Mutex, RwLock};
use tracing_subscriber::{self, EnvFilter};
use uuid::Uuid;

const DEFAULT_HUB_PORT: u16 = 44758;
const HEARTBEAT_INTERVAL_SECS: u64 = 10;

#[derive(Debug, Clone)]
struct HubConfig {
    state_file: PathBuf,
    task_heartbeat_timeout: Duration,
    helper_heartbeat_timeout: Duration,
    heartbeat_interval_sec: u64,
}

#[derive(Parser, Debug)]
#[command(
    version,
    about = "Task/helper control plane for clock-p Roblox debug clusters"
)]
struct Args {
    #[arg(long, default_value_t = DEFAULT_HUB_PORT)]
    port: u16,

    #[arg(long, default_value_t = false)]
    no_auth: bool,

    #[arg(long)]
    bearer_token: Option<String>,

    #[arg(long)]
    bearer_token_file: Option<PathBuf>,

    #[arg(long)]
    state_file: Option<PathBuf>,

    #[arg(long, default_value_t = HEARTBEAT_INTERVAL_SECS)]
    heartbeat_interval_sec: u64,

    #[arg(long, default_value_t = 30)]
    task_heartbeat_timeout_sec: u64,

    #[arg(long, default_value_t = 30)]
    helper_heartbeat_timeout_sec: u64,

    #[arg(long, default_value_t = false)]
    allow_helper_admin: bool,
}

#[derive(Clone)]
struct HttpAuthState {
    bearer_token: Option<String>,
}

#[derive(Debug)]
struct HubState {
    config: HubConfig,
    helpers: HashMap<String, HelperRecord>,
    blocked_helpers: HashMap<String, BlockedHelperRecord>,
    tasks: HashMap<String, TaskRecord>,
    state_revision: u64,
}

type SharedHubState = Arc<RwLock<HubState>>;
type SharedPersistLock = Arc<Mutex<()>>;

#[derive(Clone)]
struct AppState {
    hub: SharedHubState,
    persist_lock: SharedPersistLock,
    allow_helper_admin: bool,
}

#[derive(Debug)]
struct HelperRecord {
    helper_id: String,
    owner_user: String,
    platform: String,
    capacity: usize,
    labels: Vec<String>,
    active_task_ids: HashSet<String>,
    active_tasks: HashMap<String, HelperActiveTaskRecord>,
    registered_at_unix_ms: u64,
    last_seen_at: Instant,
}

#[derive(Debug, Clone)]
struct HelperActiveTaskRecord {
    task_id: String,
    remote_state: String,
    remote_connection_id: Option<String>,
    remote_last_error: Option<String>,
    remote_last_ready_age_ms: Option<u128>,
    remote_last_server_message_age_ms: Option<u128>,
    studio_session_state: Option<String>,
    studio_session_state_age_ms: Option<u128>,
    studio_mode: Option<String>,
    studio_mode_age_ms: Option<u128>,
    studio_mode_source: Option<String>,
    studio_control_state: Option<String>,
    studio_transition_phase: Option<String>,
    studio_transition_age_ms: Option<u128>,
    edit_runtime_state: Option<String>,
    edit_runtime_age_ms: Option<u128>,
    studio_control_last_error: Option<String>,
    last_stop_request_id: u64,
    last_stop_request_age_ms: Option<u128>,
    last_stop_request_source: Option<String>,
    last_stop_request_error: Option<String>,
    stop_intent_recorded: bool,
    stop_request_available: bool,
    stop_request_consumed: bool,
    end_test_requested: bool,
    runtime_actuator_last_poll_id: u64,
    runtime_actuator_last_poll_age_ms: Option<u128>,
    runtime_actuator_last_error: Option<String>,
    official_mcp_adapter_state: Option<String>,
    official_mcp_adapter_age_ms: Option<u128>,
    official_mcp_adapter_last_error: Option<String>,
}

#[derive(Debug, Clone)]
struct BlockedHelperRecord {
    helper_id: String,
    reason: Option<String>,
    blocked_at_unix_ms: u64,
    drain_started_at_unix_ms: u64,
    drain_deadline_at_unix_ms: u64,
    pending_task_ids: HashSet<String>,
    last_reported_task_ids: HashSet<String>,
    last_release_requested_at_unix_ms: Option<u64>,
    last_seen_at_unix_ms: Option<u64>,
    force_released_task_ids: HashSet<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
struct TaskRoutes {
    rojo_base_url: Option<String>,
    mcp_base_url: Option<String>,
    runtime_log_base_url: Option<String>,
}

#[derive(Debug)]
struct TaskRecord {
    task_id: String,
    cluster_key: String,
    task_token: String,
    recover_token: String,
    place_id: String,
    universe_id: Option<String>,
    owner_user: String,
    repo: String,
    worktree_name: String,
    service_state: String,
    accepting_launches: bool,
    services: HashMap<String, String>,
    routes: TaskRoutes,
    created_at_unix_ms: u64,
    updated_at_unix_ms: u64,
    last_task_heartbeat_at_unix_ms: u64,
    last_seen_at: Instant,
    claimed_by_helper_id: Option<String>,
    released: bool,
}

#[derive(Debug, Serialize, Deserialize)]
struct PersistedHubState {
    tasks: Vec<PersistedTaskRecord>,
    #[serde(default)]
    blocked_helpers: Vec<PersistedBlockedHelperRecord>,
}

#[derive(Debug, Serialize, Deserialize)]
struct PersistedBlockedHelperRecord {
    helper_id: String,
    #[serde(default)]
    reason: Option<String>,
    blocked_at_unix_ms: u64,
    #[serde(default)]
    drain_started_at_unix_ms: u64,
    #[serde(default)]
    drain_deadline_at_unix_ms: u64,
    #[serde(default)]
    pending_task_ids: Vec<String>,
    #[serde(default)]
    last_reported_task_ids: Vec<String>,
    #[serde(default)]
    last_release_requested_at_unix_ms: Option<u64>,
    #[serde(default)]
    last_seen_at_unix_ms: Option<u64>,
    #[serde(default)]
    force_released_task_ids: Vec<String>,
}

#[derive(Debug, Serialize, Deserialize)]
struct PersistedTaskRecord {
    task_id: String,
    cluster_key: String,
    task_token: String,
    recover_token: String,
    place_id: String,
    #[serde(default, alias = "game_id")]
    universe_id: Option<String>,
    owner_user: String,
    repo: String,
    worktree_name: String,
    service_state: String,
    accepting_launches: bool,
    services: HashMap<String, String>,
    routes: TaskRoutes,
    created_at_unix_ms: u64,
    updated_at_unix_ms: u64,
    #[serde(default)]
    last_task_heartbeat_at_unix_ms: u64,
    claimed_by_helper_id: Option<String>,
    released: bool,
}

struct PersistSnapshot {
    revision: u64,
    state_file: PathBuf,
    payload: PersistedHubState,
}

#[derive(Debug, Deserialize)]
struct CreateTaskRequest {
    cluster_key: String,
    owner_user: String,
    repo: String,
    worktree_name: String,
    place_id: String,
    #[serde(default)]
    universe_id: Option<String>,
    #[serde(default, rename = "game_id")]
    legacy_game_id: Option<String>,
}

#[derive(Debug, Serialize)]
struct CreateTaskResponse {
    task_id: String,
    task_token: String,
    recover_token: String,
    heartbeat_interval_sec: u64,
    heartbeat_timeout_sec: u64,
}

#[derive(Debug, Deserialize)]
struct RecoverTaskRequest {
    cluster_key: String,
    task_id: String,
    recover_token: String,
    #[serde(default)]
    universe_id: Option<String>,
    #[serde(default, rename = "game_id")]
    legacy_game_id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct TaskHeartbeatRequest {
    task_id: String,
    task_token: String,
    service_state: String,
    accepting_launches: bool,
    #[serde(default)]
    routes: TaskRoutes,
    #[serde(default)]
    services: HashMap<String, String>,
}

#[derive(Debug, Deserialize)]
struct ReleaseTaskRequest {
    task_id: String,
    task_token: String,
}

#[derive(Debug, Deserialize)]
struct RegisterHelperRequest {
    helper_id: String,
    owner_user: String,
    platform: String,
    capacity: usize,
    #[serde(default)]
    labels: Vec<String>,
}

#[derive(Debug, Serialize)]
struct RegisterHelperResponse {
    helper_id: String,
    heartbeat_interval_sec: u64,
    heartbeat_timeout_sec: u64,
}

#[derive(Debug, Deserialize)]
struct HelperHeartbeatRequest {
    helper_id: String,
    #[serde(default)]
    active_task_ids: Vec<String>,
    #[serde(default)]
    active_tasks: Vec<HelperActiveTaskHeartbeat>,
}

#[derive(Debug, Deserialize)]
struct HelperActiveTaskHeartbeat {
    task_id: String,
    remote_state: String,
    #[serde(default)]
    remote_connection_id: Option<String>,
    #[serde(default)]
    remote_last_error: Option<String>,
    #[serde(default)]
    remote_last_ready_age_ms: Option<u128>,
    #[serde(default)]
    remote_last_server_message_age_ms: Option<u128>,
    #[serde(default)]
    studio_session_state: Option<String>,
    #[serde(default)]
    studio_session_state_age_ms: Option<u128>,
    #[serde(default)]
    studio_mode: Option<String>,
    #[serde(default)]
    studio_mode_age_ms: Option<u128>,
    #[serde(default)]
    studio_mode_source: Option<String>,
    #[serde(default)]
    studio_control_state: Option<String>,
    #[serde(default)]
    studio_transition_phase: Option<String>,
    #[serde(default)]
    studio_transition_age_ms: Option<u128>,
    #[serde(default)]
    edit_runtime_state: Option<String>,
    #[serde(default)]
    edit_runtime_age_ms: Option<u128>,
    #[serde(default)]
    studio_control_last_error: Option<String>,
    #[serde(default)]
    last_stop_request_id: u64,
    #[serde(default)]
    last_stop_request_age_ms: Option<u128>,
    #[serde(default)]
    last_stop_request_source: Option<String>,
    #[serde(default)]
    last_stop_request_error: Option<String>,
    #[serde(default)]
    stop_intent_recorded: bool,
    #[serde(default)]
    stop_request_available: bool,
    #[serde(default)]
    stop_request_consumed: bool,
    #[serde(default)]
    end_test_requested: bool,
    #[serde(default)]
    runtime_actuator_last_poll_id: u64,
    #[serde(default)]
    runtime_actuator_last_poll_age_ms: Option<u128>,
    #[serde(default)]
    runtime_actuator_last_error: Option<String>,
    #[serde(default)]
    official_mcp_adapter_state: Option<String>,
    #[serde(default)]
    official_mcp_adapter_age_ms: Option<u128>,
    #[serde(default)]
    official_mcp_adapter_last_error: Option<String>,
}

#[derive(Debug, Deserialize)]
struct BlockHelperRequest {
    helper_id: String,
    #[serde(default)]
    reason: Option<String>,
}

#[derive(Debug, Deserialize)]
struct UnblockHelperRequest {
    helper_id: String,
}

#[derive(Debug, Serialize)]
struct HelperHeartbeatHubResponse {
    ok: bool,
    release_task_ids: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct ClaimTaskRequest {
    helper_id: String,
}

#[derive(Debug, Serialize)]
struct ClaimTaskResponse {
    claimed: bool,
    helper_id: String,
    task: Option<ClaimedTaskPayload>,
}

#[derive(Debug, Serialize)]
struct ClaimedTaskPayload {
    task_id: String,
    place_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    universe_id: Option<String>,
    owner_user: String,
    repo: String,
    worktree_name: String,
    routes: TaskRoutes,
}

#[derive(Debug, Serialize)]
struct HubStatusResponse {
    ok: bool,
    helper_count: usize,
    task_count: usize,
    helpers: Vec<HelperStatusPayload>,
    blocked_helpers: Vec<BlockedHelperPayload>,
    tasks: Vec<TaskStatusPayload>,
}

#[derive(Debug, Serialize)]
struct HelperListResponse {
    ok: bool,
    helper_count: usize,
    blocked_helper_count: usize,
    helpers: Vec<HelperStatusPayload>,
    blocked_helpers: Vec<BlockedHelperPayload>,
}

#[derive(Debug, Serialize)]
struct HelperStatusPayload {
    helper_id: String,
    owner_user: String,
    platform: String,
    capacity: usize,
    active_launch_count: usize,
    active_tasks: Vec<HelperActiveTaskPayload>,
    labels: Vec<String>,
    blocked: bool,
    registered_at_unix_ms: u64,
    last_seen_age_ms: u128,
}

#[derive(Debug, Serialize)]
struct HelperActiveTaskPayload {
    task_id: String,
    remote_state: String,
    remote_connection_id: Option<String>,
    remote_last_error: Option<String>,
    remote_last_ready_age_ms: Option<u128>,
    remote_last_server_message_age_ms: Option<u128>,
    studio_session_state: Option<String>,
    studio_session_state_age_ms: Option<u128>,
    studio_mode: Option<String>,
    studio_mode_age_ms: Option<u128>,
    studio_mode_source: Option<String>,
    studio_control_state: Option<String>,
    studio_transition_phase: Option<String>,
    studio_transition_age_ms: Option<u128>,
    edit_runtime_state: Option<String>,
    edit_runtime_age_ms: Option<u128>,
    studio_control_last_error: Option<String>,
    last_stop_request_id: u64,
    last_stop_request_age_ms: Option<u128>,
    last_stop_request_source: Option<String>,
    last_stop_request_error: Option<String>,
    stop_intent_recorded: bool,
    stop_request_available: bool,
    stop_request_consumed: bool,
    end_test_requested: bool,
    runtime_actuator_last_poll_id: u64,
    runtime_actuator_last_poll_age_ms: Option<u128>,
    runtime_actuator_last_error: Option<String>,
    official_mcp_adapter_state: Option<String>,
    official_mcp_adapter_age_ms: Option<u128>,
    official_mcp_adapter_last_error: Option<String>,
}

#[derive(Debug, Serialize)]
struct BlockedHelperPayload {
    helper_id: String,
    reason: Option<String>,
    blocked_at_unix_ms: u64,
    drain_status: String,
    drain_started_at_unix_ms: u64,
    drain_deadline_at_unix_ms: u64,
    drain_deadline_elapsed: bool,
    drain_deadline_remaining_ms: u64,
    drain_age_ms: u64,
    pending_task_ids: Vec<String>,
    last_reported_task_ids: Vec<String>,
    last_release_requested_at_unix_ms: Option<u64>,
    last_seen_age_ms: Option<u64>,
    force_released_task_ids: Vec<String>,
}

#[derive(Debug, Serialize)]
struct BlockHelperResponse {
    ok: bool,
    helper_id: String,
    blocked: bool,
}

#[derive(Debug, Serialize)]
struct TaskStatusPayload {
    task_id: String,
    cluster_key: String,
    place_id: String,
    universe_id: Option<String>,
    owner_user: String,
    repo: String,
    worktree_name: String,
    service_state: String,
    accepting_launches: bool,
    claimed_by_helper_id: Option<String>,
    released: bool,
    routes: TaskRoutes,
    services: HashMap<String, String>,
    created_at_unix_ms: u64,
    updated_at_unix_ms: u64,
    last_task_heartbeat_at_unix_ms: u64,
    last_seen_age_ms: u128,
}

fn normalize_bearer_token(token: &str) -> Option<String> {
    let trimmed = token.trim();
    if trimmed.is_empty() {
        return None;
    }
    let raw = trimmed.strip_prefix("Bearer ").unwrap_or(trimmed).trim();
    if raw.is_empty() {
        None
    } else {
        Some(raw.to_owned())
    }
}

fn default_feishu_token_paths() -> Vec<PathBuf> {
    let mut paths = Vec::new();
    if let Some(appdata) = std::env::var_os("APPDATA") {
        paths.push(
            PathBuf::from(appdata)
                .join("dev.clock-p.com")
                .join("feishu-token"),
        );
    }
    if let Some(home) = std::env::var_os("HOME") {
        paths.push(
            PathBuf::from(home)
                .join(".dev.clock-p.com")
                .join("feishu-token"),
        );
    }
    if let Some(user_profile) = std::env::var_os("USERPROFILE") {
        let path = PathBuf::from(user_profile)
            .join(".dev.clock-p.com")
            .join("feishu-token");
        if !paths.iter().any(|candidate| candidate == &path) {
            paths.push(path);
        }
    }
    paths
}

fn default_hub_state_path() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from).map(|home| {
        home.join(".dev.clock-p.com")
            .join("roblox-hub")
            .join("state.json")
    })
}

fn load_token_from_file(path: &PathBuf) -> Result<Option<String>> {
    let content = fs::read_to_string(path)
        .wrap_err_with(|| format!("Could not read token file at {}", path.display()))?;
    Ok(normalize_bearer_token(&content))
}

fn resolve_http_bearer_token(args: &Args) -> Result<Option<String>> {
    if args.no_auth {
        return Ok(None);
    }
    if let Some(token) = args
        .bearer_token
        .as_deref()
        .and_then(normalize_bearer_token)
    {
        return Ok(Some(token));
    }
    if let Some(path) = args.bearer_token_file.as_ref() {
        return load_token_from_file(path);
    }
    for path in default_feishu_token_paths() {
        if path.exists() {
            return load_token_from_file(&path);
        }
    }
    Ok(None)
}

fn resolve_hub_config(args: &Args) -> Result<HubConfig> {
    let state_file = match args.state_file.as_ref() {
        Some(path) => path.clone(),
        None => default_hub_state_path()
            .ok_or_else(|| eyre!("cannot resolve default hub state path"))?,
    };
    Ok(HubConfig {
        state_file,
        task_heartbeat_timeout: Duration::from_secs(args.task_heartbeat_timeout_sec),
        helper_heartbeat_timeout: Duration::from_secs(args.helper_heartbeat_timeout_sec),
        heartbeat_interval_sec: args.heartbeat_interval_sec,
    })
}

fn load_persisted_state(
    config: &HubConfig,
) -> Result<(
    HashMap<String, TaskRecord>,
    HashMap<String, BlockedHelperRecord>,
)> {
    if !config.state_file.exists() {
        return Ok((HashMap::new(), HashMap::new()));
    }
    let body = fs::read_to_string(&config.state_file).wrap_err_with(|| {
        format!(
            "failed to read hub state file {}",
            config.state_file.display()
        )
    })?;
    let persisted: PersistedHubState = serde_json::from_str(&body).wrap_err_with(|| {
        format!(
            "failed to parse hub state file {}",
            config.state_file.display()
        )
    })?;
    let mut tasks = HashMap::new();
    let mut blocked_helpers = HashMap::new();
    let now = now_unix_ms();
    let now_instant = Instant::now();
    for helper in persisted.blocked_helpers {
        let Ok(helper_id) = require_non_empty(&helper.helper_id, "helper_id") else {
            continue;
        };
        let drain_started_at_unix_ms = if helper.drain_started_at_unix_ms == 0 {
            helper.blocked_at_unix_ms
        } else {
            helper.drain_started_at_unix_ms
        };
        let drain_deadline_at_unix_ms = if helper.drain_deadline_at_unix_ms == 0 {
            helper
                .blocked_at_unix_ms
                .saturating_add(duration_ms(config.helper_heartbeat_timeout))
        } else {
            helper.drain_deadline_at_unix_ms
        };
        blocked_helpers.insert(
            helper_id.clone(),
            BlockedHelperRecord {
                helper_id,
                reason: normalize_optional_text(helper.reason),
                blocked_at_unix_ms: helper.blocked_at_unix_ms,
                drain_started_at_unix_ms,
                drain_deadline_at_unix_ms,
                pending_task_ids: helper.pending_task_ids.into_iter().collect(),
                last_reported_task_ids: helper.last_reported_task_ids.into_iter().collect(),
                last_release_requested_at_unix_ms: helper.last_release_requested_at_unix_ms,
                last_seen_at_unix_ms: helper.last_seen_at_unix_ms,
                force_released_task_ids: helper.force_released_task_ids.into_iter().collect(),
            },
        );
    }
    for task in persisted.tasks {
        let last_task_heartbeat_at_unix_ms = if task.last_task_heartbeat_at_unix_ms == 0 {
            task.updated_at_unix_ms
        } else {
            task.last_task_heartbeat_at_unix_ms
        };
        let elapsed_ms = now.saturating_sub(last_task_heartbeat_at_unix_ms);
        let elapsed = Duration::from_millis(elapsed_ms);
        let restored_last_seen_at = now_instant.checked_sub(elapsed).unwrap_or(now_instant);
        let claimed_by_helper_id = task.claimed_by_helper_id.as_ref().and_then(|helper_id| {
            blocked_helpers
                .get(helper_id)
                .filter(|helper| {
                    helper.pending_task_ids.contains(&task.task_id)
                        && now < helper.drain_deadline_at_unix_ms
                        && !task.released
                        && task.service_state != "expired"
                })
                .map(|_| helper_id.clone())
        });
        tasks.insert(
            task.task_id.clone(),
            TaskRecord {
                task_id: task.task_id,
                cluster_key: task.cluster_key,
                task_token: task.task_token,
                recover_token: task.recover_token,
                place_id: task.place_id,
                universe_id: task.universe_id,
                owner_user: task.owner_user,
                repo: task.repo,
                worktree_name: task.worktree_name,
                service_state: task.service_state,
                accepting_launches: task.accepting_launches,
                services: task.services,
                routes: task.routes,
                created_at_unix_ms: task.created_at_unix_ms,
                updated_at_unix_ms: task.updated_at_unix_ms,
                last_task_heartbeat_at_unix_ms,
                last_seen_at: restored_last_seen_at,
                claimed_by_helper_id,
                released: task.released,
            },
        );
    }
    for helper in blocked_helpers.values_mut() {
        let helper_id = helper.helper_id.clone();
        let pending_task_ids = helper.pending_task_ids.iter().cloned().collect::<Vec<_>>();
        for task_id in pending_task_ids {
            let keep_pending = tasks
                .get(&task_id)
                .map(|task| task.claimed_by_helper_id.as_deref() == Some(helper_id.as_str()))
                .unwrap_or(false);
            if !keep_pending {
                helper.pending_task_ids.remove(&task_id);
                helper.force_released_task_ids.insert(task_id);
            }
        }
    }
    Ok((tasks, blocked_helpers))
}

fn build_persisted_state_payload(state: &HubState) -> PersistedHubState {
    let mut blocked_helpers = state
        .blocked_helpers
        .values()
        .map(|helper| PersistedBlockedHelperRecord {
            helper_id: helper.helper_id.clone(),
            reason: helper.reason.clone(),
            blocked_at_unix_ms: helper.blocked_at_unix_ms,
            drain_started_at_unix_ms: helper.drain_started_at_unix_ms,
            drain_deadline_at_unix_ms: helper.drain_deadline_at_unix_ms,
            pending_task_ids: sorted_strings(&helper.pending_task_ids),
            last_reported_task_ids: sorted_strings(&helper.last_reported_task_ids),
            last_release_requested_at_unix_ms: helper.last_release_requested_at_unix_ms,
            last_seen_at_unix_ms: helper.last_seen_at_unix_ms,
            force_released_task_ids: sorted_strings(&helper.force_released_task_ids),
        })
        .collect::<Vec<_>>();
    blocked_helpers.sort_by(|left, right| left.helper_id.cmp(&right.helper_id));
    PersistedHubState {
        blocked_helpers,
        tasks: state
            .tasks
            .values()
            .map(|task| PersistedTaskRecord {
                task_id: task.task_id.clone(),
                cluster_key: task.cluster_key.clone(),
                task_token: task.task_token.clone(),
                recover_token: task.recover_token.clone(),
                place_id: task.place_id.clone(),
                universe_id: task.universe_id.clone(),
                owner_user: task.owner_user.clone(),
                repo: task.repo.clone(),
                worktree_name: task.worktree_name.clone(),
                service_state: task.service_state.clone(),
                accepting_launches: task.accepting_launches,
                services: task.services.clone(),
                routes: task.routes.clone(),
                created_at_unix_ms: task.created_at_unix_ms,
                updated_at_unix_ms: task.updated_at_unix_ms,
                last_task_heartbeat_at_unix_ms: task.last_task_heartbeat_at_unix_ms,
                claimed_by_helper_id: task.claimed_by_helper_id.clone(),
                released: task.released,
            })
            .collect(),
    }
}

fn build_persist_snapshot(state: &HubState) -> PersistSnapshot {
    PersistSnapshot {
        revision: state.state_revision,
        state_file: state.config.state_file.clone(),
        payload: build_persisted_state_payload(state),
    }
}

fn bump_state_revision(state: &mut HubState) -> PersistSnapshot {
    state.state_revision = state.state_revision.saturating_add(1);
    build_persist_snapshot(state)
}

fn persist_snapshot(snapshot: &PersistSnapshot) -> Result<()> {
    let parent = snapshot
        .state_file
        .parent()
        .ok_or_else(|| eyre!("hub state file has no parent directory"))?;
    fs::create_dir_all(parent)?;
    let tmp_path = snapshot.state_file.with_extension("json.tmp");
    let mut body = serde_json::to_vec(&snapshot.payload)?;
    body.push(b'\n');
    fs::write(&tmp_path, body)?;
    fs::rename(&tmp_path, &snapshot.state_file)?;
    Ok(())
}

async fn persist_snapshot_if_current(app: &AppState, snapshot: PersistSnapshot) -> Result<()> {
    let _guard = app.persist_lock.lock().await;
    let latest_revision = {
        let state = app.hub.read().await;
        state.state_revision
    };
    if snapshot.revision < latest_revision {
        return Ok(());
    }
    persist_snapshot(&snapshot)
}

async fn require_http_auth(
    State(auth): State<HttpAuthState>,
    request: Request,
    next: Next,
) -> Response {
    let Some(expected_token) = auth.bearer_token.as_ref() else {
        return next.run(request).await;
    };
    let authorized = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(normalize_bearer_token)
        .map(|provided| provided == *expected_token)
        .unwrap_or(false);
    if !authorized {
        return (StatusCode::UNAUTHORIZED, "Unauthorized").into_response();
    }
    next.run(request).await
}

fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn duration_ms(value: Duration) -> u64 {
    value.as_millis().min(u128::from(u64::MAX)) as u64
}

fn sorted_strings(values: &HashSet<String>) -> Vec<String> {
    let mut values = values.iter().cloned().collect::<Vec<_>>();
    values.sort();
    values
}

fn require_non_empty(value: &str, label: &str) -> Result<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err(eyre!("{label} must not be empty"));
    }
    Ok(trimmed.to_owned())
}

fn normalize_optional_text(value: Option<String>) -> Option<String> {
    value
        .map(|raw| raw.trim().to_owned())
        .filter(|trimmed| !trimmed.is_empty())
}

fn new_task_id() -> String {
    let short = Uuid::new_v4().simple().to_string();
    format!("t{}", &short[..10])
}

fn new_token() -> String {
    Uuid::new_v4().to_string()
}

fn clear_task_claim_state(task: &mut TaskRecord) {
    task.accepting_launches = false;
    task.claimed_by_helper_id = None;
}

fn clear_task_route_state(task: &mut TaskRecord) {
    task.routes = TaskRoutes::default();
    task.services.clear();
}

fn expire_task(task: &mut TaskRecord, now: u64) -> bool {
    if task.service_state == "expired"
        && !task.accepting_launches
        && task.claimed_by_helper_id.is_none()
        && task.routes == TaskRoutes::default()
        && task.services.is_empty()
    {
        return false;
    }
    task.service_state = "expired".to_owned();
    clear_task_claim_state(task);
    clear_task_route_state(task);
    task.updated_at_unix_ms = now;
    true
}

fn task_status_payload(task: &TaskRecord) -> TaskStatusPayload {
    TaskStatusPayload {
        task_id: task.task_id.clone(),
        cluster_key: task.cluster_key.clone(),
        place_id: task.place_id.clone(),
        universe_id: task.universe_id.clone(),
        owner_user: task.owner_user.clone(),
        repo: task.repo.clone(),
        worktree_name: task.worktree_name.clone(),
        service_state: task.service_state.clone(),
        accepting_launches: task.accepting_launches,
        claimed_by_helper_id: task.claimed_by_helper_id.clone(),
        released: task.released,
        routes: task.routes.clone(),
        services: task.services.clone(),
        created_at_unix_ms: task.created_at_unix_ms,
        updated_at_unix_ms: task.updated_at_unix_ms,
        last_task_heartbeat_at_unix_ms: task.last_task_heartbeat_at_unix_ms,
        last_seen_age_ms: task.last_seen_at.elapsed().as_millis(),
    }
}

fn helper_status_payload(helper: &HelperRecord, blocked: bool) -> HelperStatusPayload {
    let age_delta = helper.last_seen_at.elapsed().as_millis();
    let mut active_tasks = helper
        .active_task_ids
        .iter()
        .map(|task_id| {
            if let Some(task) = helper.active_tasks.get(task_id) {
                HelperActiveTaskPayload {
                    task_id: task.task_id.clone(),
                    remote_state: task.remote_state.clone(),
                    remote_connection_id: task.remote_connection_id.clone(),
                    remote_last_error: task.remote_last_error.clone(),
                    remote_last_ready_age_ms: task.remote_last_ready_age_ms,
                    remote_last_server_message_age_ms: task.remote_last_server_message_age_ms,
                    studio_session_state: task.studio_session_state.clone(),
                    studio_session_state_age_ms: task
                        .studio_session_state_age_ms
                        .map(|age| age + age_delta),
                    studio_mode: task.studio_mode.clone(),
                    studio_mode_age_ms: task.studio_mode_age_ms.map(|age| age + age_delta),
                    studio_mode_source: task.studio_mode_source.clone(),
                    studio_control_state: task.studio_control_state.clone(),
                    studio_transition_phase: task.studio_transition_phase.clone(),
                    studio_transition_age_ms: task
                        .studio_transition_age_ms
                        .map(|age| age + age_delta),
                    edit_runtime_state: task.edit_runtime_state.clone(),
                    edit_runtime_age_ms: task.edit_runtime_age_ms.map(|age| age + age_delta),
                    studio_control_last_error: task.studio_control_last_error.clone(),
                    last_stop_request_id: task.last_stop_request_id,
                    last_stop_request_age_ms: task
                        .last_stop_request_age_ms
                        .map(|age| age + age_delta),
                    last_stop_request_source: task.last_stop_request_source.clone(),
                    last_stop_request_error: task.last_stop_request_error.clone(),
                    stop_intent_recorded: task.stop_intent_recorded,
                    stop_request_available: task.stop_request_available,
                    stop_request_consumed: task.stop_request_consumed,
                    end_test_requested: task.end_test_requested,
                    runtime_actuator_last_poll_id: task.runtime_actuator_last_poll_id,
                    runtime_actuator_last_poll_age_ms: task
                        .runtime_actuator_last_poll_age_ms
                        .map(|age| age + age_delta),
                    runtime_actuator_last_error: task.runtime_actuator_last_error.clone(),
                    official_mcp_adapter_state: task.official_mcp_adapter_state.clone(),
                    official_mcp_adapter_age_ms: task
                        .official_mcp_adapter_age_ms
                        .map(|age| age + age_delta),
                    official_mcp_adapter_last_error: task.official_mcp_adapter_last_error.clone(),
                }
            } else {
                HelperActiveTaskPayload {
                    task_id: task_id.clone(),
                    remote_state: "unknown".to_owned(),
                    remote_connection_id: None,
                    remote_last_error: None,
                    remote_last_ready_age_ms: None,
                    remote_last_server_message_age_ms: None,
                    studio_session_state: None,
                    studio_session_state_age_ms: None,
                    studio_mode: None,
                    studio_mode_age_ms: None,
                    studio_mode_source: None,
                    studio_control_state: None,
                    studio_transition_phase: None,
                    studio_transition_age_ms: None,
                    edit_runtime_state: None,
                    edit_runtime_age_ms: None,
                    studio_control_last_error: None,
                    last_stop_request_id: 0,
                    last_stop_request_age_ms: None,
                    last_stop_request_source: None,
                    last_stop_request_error: None,
                    stop_intent_recorded: false,
                    stop_request_available: false,
                    stop_request_consumed: false,
                    end_test_requested: false,
                    runtime_actuator_last_poll_id: 0,
                    runtime_actuator_last_poll_age_ms: None,
                    runtime_actuator_last_error: None,
                    official_mcp_adapter_state: None,
                    official_mcp_adapter_age_ms: None,
                    official_mcp_adapter_last_error: None,
                }
            }
        })
        .collect::<Vec<_>>();
    active_tasks.sort_by(|left, right| left.task_id.cmp(&right.task_id));
    HelperStatusPayload {
        helper_id: helper.helper_id.clone(),
        owner_user: helper.owner_user.clone(),
        platform: helper.platform.clone(),
        capacity: helper.capacity,
        active_launch_count: helper.active_task_ids.len(),
        active_tasks,
        labels: helper.labels.clone(),
        blocked,
        registered_at_unix_ms: helper.registered_at_unix_ms,
        last_seen_age_ms: helper.last_seen_at.elapsed().as_millis(),
    }
}

fn blocked_helper_payload(helper: &BlockedHelperRecord) -> BlockedHelperPayload {
    let now = now_unix_ms();
    BlockedHelperPayload {
        helper_id: helper.helper_id.clone(),
        reason: helper.reason.clone(),
        blocked_at_unix_ms: helper.blocked_at_unix_ms,
        drain_status: blocked_helper_drain_status(helper).to_owned(),
        drain_started_at_unix_ms: helper.drain_started_at_unix_ms,
        drain_deadline_at_unix_ms: helper.drain_deadline_at_unix_ms,
        drain_deadline_elapsed: now >= helper.drain_deadline_at_unix_ms,
        drain_deadline_remaining_ms: helper.drain_deadline_at_unix_ms.saturating_sub(now),
        drain_age_ms: now.saturating_sub(helper.drain_started_at_unix_ms),
        pending_task_ids: sorted_strings(&helper.pending_task_ids),
        last_reported_task_ids: sorted_strings(&helper.last_reported_task_ids),
        last_release_requested_at_unix_ms: helper.last_release_requested_at_unix_ms,
        last_seen_age_ms: helper
            .last_seen_at_unix_ms
            .map(|last_seen| now.saturating_sub(last_seen)),
        force_released_task_ids: sorted_strings(&helper.force_released_task_ids),
    }
}

fn blocked_helper_drain_status(helper: &BlockedHelperRecord) -> &'static str {
    if !helper.pending_task_ids.is_empty() {
        "draining"
    } else if !helper.force_released_task_ids.is_empty() {
        "force_released"
    } else {
        "drained"
    }
}

fn request_universe_id(
    universe_id: Option<String>,
    legacy_game_id: Option<String>,
) -> Option<String> {
    universe_id
        .or(legacy_game_id)
        .filter(|value| !value.trim().is_empty())
}

fn clear_helper_claims(state: &mut HubState, helper_id: &str, now: u64) -> bool {
    let mut changed = false;
    for task in state.tasks.values_mut() {
        if task.claimed_by_helper_id.as_deref() == Some(helper_id) {
            task.claimed_by_helper_id = None;
            task.updated_at_unix_ms = now;
            changed = true;
        }
    }
    changed
}

fn clear_helper_task_claim(state: &mut HubState, helper_id: &str, task_id: &str, now: u64) -> bool {
    let mut changed = false;
    if let Some(task) = state.tasks.get_mut(task_id) {
        if task.claimed_by_helper_id.as_deref() == Some(helper_id) {
            task.claimed_by_helper_id = None;
            task.updated_at_unix_ms = now;
            changed = true;
        }
    }
    if let Some(helper) = state.helpers.get_mut(helper_id) {
        helper.active_task_ids.remove(task_id);
        helper.active_tasks.remove(task_id);
    }
    changed
}

fn force_due_blocked_helper_drains(state: &mut HubState, now: u64) -> bool {
    let helper_ids = state
        .blocked_helpers
        .iter()
        .filter(|(_, helper)| {
            !helper.pending_task_ids.is_empty() && now >= helper.drain_deadline_at_unix_ms
        })
        .map(|(helper_id, _)| helper_id.clone())
        .collect::<Vec<_>>();
    let mut changed = false;
    for helper_id in helper_ids {
        let pending_task_ids = state
            .blocked_helpers
            .get(&helper_id)
            .map(|helper| helper.pending_task_ids.iter().cloned().collect::<Vec<_>>())
            .unwrap_or_default();
        for task_id in &pending_task_ids {
            clear_helper_task_claim(state, &helper_id, task_id, now);
        }
        if let Some(helper) = state.blocked_helpers.get_mut(&helper_id) {
            for task_id in pending_task_ids {
                helper.pending_task_ids.remove(&task_id);
                helper.force_released_task_ids.insert(task_id);
            }
        }
        changed = true;
    }
    changed
}

fn task_matches_claimed_helper(task: &TaskRecord, helper_id: &str, task_id: &str) -> bool {
    !task.released
        && task.service_state != "expired"
        && task.task_id == task_id
        && task.claimed_by_helper_id.as_deref() == Some(helper_id)
}

fn cleanup_stale_state(state: &mut HubState) -> bool {
    let mut persisted_changed = false;
    let now = now_unix_ms();
    persisted_changed |= force_due_blocked_helper_drains(state, now);
    let stale_helpers: Vec<String> = state
        .helpers
        .iter()
        .filter(|(_, helper)| helper.last_seen_at.elapsed() > state.config.helper_heartbeat_timeout)
        .map(|(helper_id, _)| helper_id.clone())
        .collect();
    for helper_id in stale_helpers {
        state.helpers.remove(&helper_id);
        if !state.blocked_helpers.contains_key(&helper_id) {
            persisted_changed |= clear_helper_claims(state, &helper_id, now);
        }
    }
    for task in state.tasks.values_mut() {
        if task.released {
            continue;
        }
        if task.last_seen_at.elapsed() > state.config.task_heartbeat_timeout {
            persisted_changed |= expire_task(task, now);
        }
    }
    persisted_changed
}

async fn status_handler(State(app): State<AppState>) -> Json<HubStatusResponse> {
    let state = app.hub.read().await;
    let mut helpers = state
        .helpers
        .values()
        .map(|helper| {
            helper_status_payload(
                helper,
                state.blocked_helpers.contains_key(&helper.helper_id),
            )
        })
        .collect::<Vec<_>>();
    helpers.sort_by(|left, right| left.helper_id.cmp(&right.helper_id));
    let mut blocked_helpers = state
        .blocked_helpers
        .values()
        .map(blocked_helper_payload)
        .collect::<Vec<_>>();
    blocked_helpers.sort_by(|left, right| left.helper_id.cmp(&right.helper_id));
    let mut tasks = state
        .tasks
        .values()
        .map(task_status_payload)
        .collect::<Vec<_>>();
    tasks.sort_by(|left, right| left.task_id.cmp(&right.task_id));
    Json(HubStatusResponse {
        ok: true,
        helper_count: helpers.len(),
        task_count: tasks.len(),
        helpers,
        blocked_helpers,
        tasks,
    })
}

async fn helpers_handler(State(app): State<AppState>) -> Json<HelperListResponse> {
    let state = app.hub.read().await;
    let mut helpers = state
        .helpers
        .values()
        .map(|helper| {
            helper_status_payload(
                helper,
                state.blocked_helpers.contains_key(&helper.helper_id),
            )
        })
        .collect::<Vec<_>>();
    helpers.sort_by(|left, right| left.helper_id.cmp(&right.helper_id));
    let mut blocked_helpers = state
        .blocked_helpers
        .values()
        .map(blocked_helper_payload)
        .collect::<Vec<_>>();
    blocked_helpers.sort_by(|left, right| left.helper_id.cmp(&right.helper_id));
    Json(HelperListResponse {
        ok: true,
        helper_count: helpers.len(),
        blocked_helper_count: blocked_helpers.len(),
        helpers,
        blocked_helpers,
    })
}

async fn debug_helper_handler(
    Path(helper_id): Path<String>,
    State(app): State<AppState>,
) -> Result<Json<HelperStatusPayload>, HubError> {
    let state = app.hub.read().await;
    let helper = state
        .helpers
        .get(&helper_id)
        .ok_or_else(|| HubError(eyre!("helper not found: {helper_id}")))?;
    Ok(Json(helper_status_payload(
        helper,
        state.blocked_helpers.contains_key(&helper_id),
    )))
}

async fn task_status_handler(
    Path(task_id): Path<String>,
    State(app): State<AppState>,
) -> Result<Json<TaskStatusPayload>, HubError> {
    let state = app.hub.read().await;
    let task = state
        .tasks
        .get(&task_id)
        .ok_or_else(|| HubError(eyre!("task not found: {task_id}")))?;
    Ok(Json(task_status_payload(task)))
}

async fn create_task_handler(
    State(app): State<AppState>,
    Json(payload): Json<CreateTaskRequest>,
) -> Result<Json<CreateTaskResponse>, HubError> {
    let mut state = app.hub.write().await;
    cleanup_stale_state(&mut state);
    let cluster_key = require_non_empty(&payload.cluster_key, "cluster_key")?;
    if state.tasks.values().any(|task| {
        !task.released && task.service_state != "expired" && task.cluster_key == cluster_key
    }) {
        return Err(HubError(eyre!(
            "active task already exists for cluster_key {cluster_key}; use recover"
        )));
    }
    let now = now_unix_ms();
    let task_id = new_task_id();
    let task_token = new_token();
    let recover_token = new_token();
    state.tasks.insert(
        task_id.clone(),
        TaskRecord {
            task_id: task_id.clone(),
            cluster_key,
            task_token: task_token.clone(),
            recover_token: recover_token.clone(),
            place_id: require_non_empty(&payload.place_id, "place_id")?,
            universe_id: request_universe_id(payload.universe_id, payload.legacy_game_id),
            owner_user: require_non_empty(&payload.owner_user, "owner_user")?,
            repo: require_non_empty(&payload.repo, "repo")?,
            worktree_name: require_non_empty(&payload.worktree_name, "worktree_name")?,
            service_state: "starting".to_owned(),
            accepting_launches: false,
            services: HashMap::new(),
            routes: TaskRoutes::default(),
            created_at_unix_ms: now,
            updated_at_unix_ms: now,
            last_task_heartbeat_at_unix_ms: now,
            last_seen_at: Instant::now(),
            claimed_by_helper_id: None,
            released: false,
        },
    );
    let snapshot = bump_state_revision(&mut state);
    let heartbeat_interval_sec = state.config.heartbeat_interval_sec;
    let heartbeat_timeout_sec = state.config.task_heartbeat_timeout.as_secs();
    drop(state);
    persist_snapshot_if_current(&app, snapshot).await?;
    Ok(Json(CreateTaskResponse {
        task_id,
        task_token,
        recover_token,
        heartbeat_interval_sec,
        heartbeat_timeout_sec,
    }))
}

async fn recover_task_handler(
    State(app): State<AppState>,
    Json(payload): Json<RecoverTaskRequest>,
) -> Result<Json<CreateTaskResponse>, HubError> {
    let mut state = app.hub.write().await;
    let _cleanup_changed = cleanup_stale_state(&mut state);
    let task = state
        .tasks
        .get_mut(&payload.task_id)
        .ok_or_else(|| HubError(eyre!("task not found: {}", payload.task_id)))?;
    if task.cluster_key != payload.cluster_key {
        return Err(HubError(eyre!("cluster_key mismatch for recover")));
    }
    if task.recover_token != payload.recover_token {
        return Err(HubError(eyre!("recover_token mismatch")));
    }
    if task.released {
        return Err(HubError(eyre!(
            "released task cannot be recovered: {}",
            payload.task_id
        )));
    }
    task.task_token = new_token();
    if let Some(universe_id) = request_universe_id(payload.universe_id, payload.legacy_game_id) {
        task.universe_id = Some(universe_id);
    }
    task.service_state = "recovering".to_owned();
    clear_task_claim_state(task);
    clear_task_route_state(task);
    let now = now_unix_ms();
    task.updated_at_unix_ms = now;
    task.last_task_heartbeat_at_unix_ms = now;
    task.last_seen_at = Instant::now();
    let response_task_id = task.task_id.clone();
    let response_task_token = task.task_token.clone();
    let response_recover_token = task.recover_token.clone();
    let snapshot = bump_state_revision(&mut state);
    let heartbeat_interval_sec = state.config.heartbeat_interval_sec;
    let heartbeat_timeout_sec = state.config.task_heartbeat_timeout.as_secs();
    drop(state);
    persist_snapshot_if_current(&app, snapshot).await?;
    Ok(Json(CreateTaskResponse {
        task_id: response_task_id,
        task_token: response_task_token,
        recover_token: response_recover_token,
        heartbeat_interval_sec,
        heartbeat_timeout_sec,
    }))
}

async fn task_heartbeat_handler(
    State(app): State<AppState>,
    Json(payload): Json<TaskHeartbeatRequest>,
) -> Result<Json<serde_json::Value>, HubError> {
    let mut state = app.hub.write().await;
    let _cleanup_changed = cleanup_stale_state(&mut state);
    let task = state
        .tasks
        .get_mut(&payload.task_id)
        .ok_or_else(|| HubError(eyre!("task not found: {}", payload.task_id)))?;
    if task.task_token != payload.task_token {
        return Err(HubError(eyre!("task token mismatch")));
    }
    if task.released {
        return Err(HubError(eyre!("released task cannot accept heartbeat")));
    }
    if task.service_state == "expired" {
        return Err(HubError(eyre!(
            "expired task requires recover before heartbeat"
        )));
    }
    task.service_state = require_non_empty(&payload.service_state, "service_state")?;
    task.accepting_launches = payload.accepting_launches;
    task.routes = payload.routes;
    task.services = payload.services;
    let now = now_unix_ms();
    task.updated_at_unix_ms = now;
    task.last_task_heartbeat_at_unix_ms = now;
    task.last_seen_at = Instant::now();
    let snapshot = bump_state_revision(&mut state);
    drop(state);
    persist_snapshot_if_current(&app, snapshot).await?;
    Ok(Json(serde_json::json!({"ok": true})))
}

async fn release_task_handler(
    State(app): State<AppState>,
    Json(payload): Json<ReleaseTaskRequest>,
) -> Result<Json<serde_json::Value>, HubError> {
    let mut state = app.hub.write().await;
    let _cleanup_changed = cleanup_stale_state(&mut state);
    let helper_id = {
        let task = state
            .tasks
            .get_mut(&payload.task_id)
            .ok_or_else(|| HubError(eyre!("task not found: {}", payload.task_id)))?;
        if task.task_token != payload.task_token {
            return Err(HubError(eyre!("task token mismatch")));
        }
        let helper_id = task.claimed_by_helper_id.clone();
        task.service_state = "released".to_owned();
        clear_task_claim_state(task);
        clear_task_route_state(task);
        task.released = true;
        task.updated_at_unix_ms = now_unix_ms();
        helper_id
    };
    if let Some(helper_id) = helper_id.as_ref() {
        if let Some(helper) = state.helpers.get_mut(helper_id) {
            helper.active_task_ids.remove(&payload.task_id);
            helper.active_tasks.remove(&payload.task_id);
        }
    }
    let snapshot = bump_state_revision(&mut state);
    drop(state);
    persist_snapshot_if_current(&app, snapshot).await?;
    Ok(Json(serde_json::json!({"ok": true})))
}

async fn register_helper_handler(
    State(app): State<AppState>,
    Json(payload): Json<RegisterHelperRequest>,
) -> Result<Json<RegisterHelperResponse>, Response> {
    let mut state = app.hub.write().await;
    let cleanup_changed = cleanup_stale_state(&mut state);
    let helper_id = require_non_empty(&payload.helper_id, "helper_id")
        .map_err(HubError::from)
        .map_err(IntoResponse::into_response)?;
    if state.blocked_helpers.contains_key(&helper_id) {
        let message = format!("helper_id is blocked: {helper_id}");
        let cleanup_snapshot = cleanup_changed.then(|| bump_state_revision(&mut state));
        drop(state);
        if let Some(snapshot) = cleanup_snapshot {
            persist_snapshot_if_current(&app, snapshot)
                .await
                .map_err(HubError::from)
                .map_err(IntoResponse::into_response)?;
        }
        return Err(HttpHubError::forbidden("helper_id_blocked", message).into_response());
    }
    if let Some(existing) = state.helpers.get(&helper_id) {
        let age_ms = existing.last_seen_at.elapsed().as_millis();
        let message = format!(
            "helper_id already active: {helper_id}; existing helper is still alive (last_seen_age_ms={age_ms})"
        );
        let cleanup_snapshot = cleanup_changed.then(|| bump_state_revision(&mut state));
        drop(state);
        if let Some(snapshot) = cleanup_snapshot {
            persist_snapshot_if_current(&app, snapshot)
                .await
                .map_err(HubError::from)
                .map_err(IntoResponse::into_response)?;
        }
        return Err(HttpHubError::conflict("helper_id_conflict", message).into_response());
    }
    let now = now_unix_ms();
    state.helpers.insert(
        helper_id.clone(),
        HelperRecord {
            helper_id: helper_id.clone(),
            owner_user: require_non_empty(&payload.owner_user, "owner_user")
                .map_err(HubError::from)
                .map_err(IntoResponse::into_response)?,
            platform: require_non_empty(&payload.platform, "platform")
                .map_err(HubError::from)
                .map_err(IntoResponse::into_response)?,
            capacity: payload.capacity,
            labels: payload.labels,
            active_task_ids: HashSet::new(),
            active_tasks: HashMap::new(),
            registered_at_unix_ms: now,
            last_seen_at: Instant::now(),
        },
    );
    let cleanup_snapshot = cleanup_changed.then(|| bump_state_revision(&mut state));
    let heartbeat_interval_sec = state.config.heartbeat_interval_sec;
    let heartbeat_timeout_sec = state.config.helper_heartbeat_timeout.as_secs();
    drop(state);
    if let Some(snapshot) = cleanup_snapshot {
        persist_snapshot_if_current(&app, snapshot)
            .await
            .map_err(HubError::from)
            .map_err(IntoResponse::into_response)?;
    }
    Ok(Json(RegisterHelperResponse {
        helper_id,
        heartbeat_interval_sec,
        heartbeat_timeout_sec,
    }))
}

async fn block_helper_handler(
    State(app): State<AppState>,
    Json(payload): Json<BlockHelperRequest>,
) -> Result<Json<BlockHelperResponse>, Response> {
    if !app.allow_helper_admin {
        return Err(helper_admin_disabled_response());
    }
    let mut state = app.hub.write().await;
    cleanup_stale_state(&mut state);
    let helper_id = require_non_empty(&payload.helper_id, "helper_id")
        .map_err(HubError::from)
        .map_err(IntoResponse::into_response)?;
    let now = now_unix_ms();
    let pending_task_ids = state
        .tasks
        .values()
        .filter(|task| task.claimed_by_helper_id.as_deref() == Some(helper_id.as_str()))
        .map(|task| task.task_id.clone())
        .collect::<HashSet<_>>();
    let last_reported_task_ids = state
        .helpers
        .get(&helper_id)
        .map(|helper| helper.active_task_ids.clone())
        .unwrap_or_default();
    state.helpers.remove(&helper_id);
    let drain_timeout_ms = duration_ms(state.config.helper_heartbeat_timeout);
    let mut existing = state.blocked_helpers.remove(&helper_id);
    let drain_started_at_unix_ms = existing
        .as_ref()
        .map(|helper| helper.drain_started_at_unix_ms)
        .unwrap_or(now);
    let drain_deadline_at_unix_ms = existing
        .as_ref()
        .map(|helper| helper.drain_deadline_at_unix_ms)
        .unwrap_or_else(|| now.saturating_add(drain_timeout_ms));
    let mut pending_task_ids = existing
        .as_mut()
        .map(|helper| {
            helper
                .pending_task_ids
                .union(&pending_task_ids)
                .cloned()
                .collect::<HashSet<_>>()
        })
        .unwrap_or(pending_task_ids);
    pending_task_ids.retain(|task_id| {
        state
            .tasks
            .get(task_id)
            .map(|task| task.claimed_by_helper_id.as_deref() == Some(helper_id.as_str()))
            .unwrap_or(false)
    });
    let last_reported_task_ids = if last_reported_task_ids.is_empty() {
        existing
            .as_ref()
            .map(|helper| helper.last_reported_task_ids.clone())
            .unwrap_or_default()
    } else {
        last_reported_task_ids
    };
    let force_released_task_ids = existing
        .as_ref()
        .map(|helper| helper.force_released_task_ids.clone())
        .unwrap_or_default();
    state.blocked_helpers.insert(
        helper_id.clone(),
        BlockedHelperRecord {
            helper_id: helper_id.clone(),
            reason: normalize_optional_text(payload.reason)
                .or_else(|| existing.as_ref().and_then(|helper| helper.reason.clone())),
            blocked_at_unix_ms: now,
            drain_started_at_unix_ms,
            drain_deadline_at_unix_ms,
            pending_task_ids,
            last_reported_task_ids,
            last_release_requested_at_unix_ms: existing
                .as_ref()
                .and_then(|helper| helper.last_release_requested_at_unix_ms),
            last_seen_at_unix_ms: existing
                .as_ref()
                .and_then(|helper| helper.last_seen_at_unix_ms),
            force_released_task_ids,
        },
    );
    let snapshot = bump_state_revision(&mut state);
    drop(state);
    persist_snapshot_if_current(&app, snapshot)
        .await
        .map_err(HubError::from)
        .map_err(IntoResponse::into_response)?;
    Ok(Json(BlockHelperResponse {
        ok: true,
        helper_id,
        blocked: true,
    }))
}

async fn unblock_helper_handler(
    State(app): State<AppState>,
    Json(payload): Json<UnblockHelperRequest>,
) -> Result<Json<BlockHelperResponse>, Response> {
    if !app.allow_helper_admin {
        return Err(helper_admin_disabled_response());
    }
    let mut state = app.hub.write().await;
    let cleanup_changed = cleanup_stale_state(&mut state);
    let helper_id = require_non_empty(&payload.helper_id, "helper_id")
        .map_err(HubError::from)
        .map_err(IntoResponse::into_response)?;
    if let Some(helper) = state.blocked_helpers.get(&helper_id) {
        if !helper.pending_task_ids.is_empty() {
            let message = format!(
                "helper drain is still pending: {helper_id}; pending_task_ids={:?}",
                sorted_strings(&helper.pending_task_ids)
            );
            let cleanup_snapshot = cleanup_changed.then(|| bump_state_revision(&mut state));
            drop(state);
            if let Some(snapshot) = cleanup_snapshot {
                persist_snapshot_if_current(&app, snapshot)
                    .await
                    .map_err(HubError::from)
                    .map_err(IntoResponse::into_response)?;
            }
            return Err(HttpHubError::conflict("helper_drain_pending", message).into_response());
        }
    }
    let removed = state.blocked_helpers.remove(&helper_id).is_some();
    let snapshot = if cleanup_changed || removed {
        Some(bump_state_revision(&mut state))
    } else {
        None
    };
    drop(state);
    if let Some(snapshot) = snapshot {
        persist_snapshot_if_current(&app, snapshot)
            .await
            .map_err(HubError::from)
            .map_err(IntoResponse::into_response)?;
    }
    Ok(Json(BlockHelperResponse {
        ok: true,
        helper_id,
        blocked: false,
    }))
}

async fn helper_heartbeat_handler(
    State(app): State<AppState>,
    Json(payload): Json<HelperHeartbeatRequest>,
) -> Result<Json<HelperHeartbeatHubResponse>, HubError> {
    let mut state = app.hub.write().await;
    let cleanup_changed = cleanup_stale_state(&mut state);
    let now = now_unix_ms();
    let helper_id = payload.helper_id.clone();
    let mut active_task_ids = payload
        .active_task_ids
        .iter()
        .cloned()
        .collect::<HashSet<_>>();
    for active_task in &payload.active_tasks {
        active_task_ids.insert(active_task.task_id.clone());
    }
    if state.blocked_helpers.contains_key(&helper_id) {
        let release_task_ids = sorted_strings(&active_task_ids);
        let acked_task_ids = state
            .blocked_helpers
            .get(&helper_id)
            .map(|helper| {
                helper
                    .pending_task_ids
                    .iter()
                    .filter(|task_id| !active_task_ids.contains(*task_id))
                    .cloned()
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let mut persisted_changed = true;
        for task_id in &acked_task_ids {
            persisted_changed |= clear_helper_task_claim(&mut state, &helper_id, task_id, now);
        }
        state.helpers.remove(&helper_id);
        if let Some(helper) = state.blocked_helpers.get_mut(&helper_id) {
            for task_id in acked_task_ids {
                helper.pending_task_ids.remove(&task_id);
            }
            helper.last_seen_at_unix_ms = Some(now);
            helper.last_reported_task_ids = active_task_ids;
            if !release_task_ids.is_empty() {
                helper.last_release_requested_at_unix_ms = Some(now);
            }
        }
        persisted_changed |= force_due_blocked_helper_drains(&mut state, now);
        let snapshot = if persisted_changed {
            Some(bump_state_revision(&mut state))
        } else {
            None
        };
        drop(state);
        if let Some(snapshot) = snapshot {
            persist_snapshot_if_current(&app, snapshot).await?;
        }
        return Ok(Json(HelperHeartbeatHubResponse {
            ok: false,
            release_task_ids,
        }));
    }
    let mut release_task_ids = HashSet::new();
    let mut persisted_changed = cleanup_changed;

    for task in state.tasks.values_mut() {
        if task.claimed_by_helper_id.as_deref() != Some(helper_id.as_str()) {
            continue;
        }
        if !active_task_ids.contains(&task.task_id) {
            release_task_ids.insert(task.task_id.clone());
            task.claimed_by_helper_id = None;
            task.updated_at_unix_ms = now;
            persisted_changed = true;
        }
    }

    for task_id in &active_task_ids {
        let should_release = match state.tasks.get(task_id) {
            Some(task) => !task_matches_claimed_helper(task, &helper_id, task_id),
            None => true,
        };
        if should_release {
            release_task_ids.insert(task_id.clone());
        }
    }

    let helper = state.helpers.get_mut(&helper_id);
    let Some(helper) = helper else {
        let snapshot = if persisted_changed {
            Some(bump_state_revision(&mut state))
        } else {
            None
        };
        drop(state);
        if let Some(snapshot) = snapshot {
            persist_snapshot_if_current(&app, snapshot).await?;
        }
        return Err(HubError(eyre!("helper not found: {}", helper_id)));
    };
    helper.last_seen_at = Instant::now();
    helper.active_task_ids.clear();
    helper.active_tasks.clear();
    for task_id in active_task_ids {
        if release_task_ids.contains(&task_id) {
            continue;
        }
        helper.active_task_ids.insert(task_id);
    }
    for active_task in payload.active_tasks {
        if !helper.active_task_ids.contains(&active_task.task_id) {
            continue;
        }
        helper.active_tasks.insert(
            active_task.task_id.clone(),
            HelperActiveTaskRecord {
                task_id: active_task.task_id,
                remote_state: active_task.remote_state,
                remote_connection_id: active_task.remote_connection_id,
                remote_last_error: active_task.remote_last_error,
                remote_last_ready_age_ms: active_task.remote_last_ready_age_ms,
                remote_last_server_message_age_ms: active_task.remote_last_server_message_age_ms,
                studio_session_state: active_task.studio_session_state,
                studio_session_state_age_ms: active_task.studio_session_state_age_ms,
                studio_mode: active_task.studio_mode,
                studio_mode_age_ms: active_task.studio_mode_age_ms,
                studio_mode_source: active_task.studio_mode_source,
                studio_control_state: active_task.studio_control_state,
                studio_transition_phase: active_task.studio_transition_phase,
                studio_transition_age_ms: active_task.studio_transition_age_ms,
                edit_runtime_state: active_task.edit_runtime_state,
                edit_runtime_age_ms: active_task.edit_runtime_age_ms,
                studio_control_last_error: active_task.studio_control_last_error,
                last_stop_request_id: active_task.last_stop_request_id,
                last_stop_request_age_ms: active_task.last_stop_request_age_ms,
                last_stop_request_source: active_task.last_stop_request_source,
                last_stop_request_error: active_task.last_stop_request_error,
                stop_intent_recorded: active_task.stop_intent_recorded,
                stop_request_available: active_task.stop_request_available,
                stop_request_consumed: active_task.stop_request_consumed,
                end_test_requested: active_task.end_test_requested,
                runtime_actuator_last_poll_id: active_task.runtime_actuator_last_poll_id,
                runtime_actuator_last_poll_age_ms: active_task.runtime_actuator_last_poll_age_ms,
                runtime_actuator_last_error: active_task.runtime_actuator_last_error,
                official_mcp_adapter_state: active_task.official_mcp_adapter_state,
                official_mcp_adapter_age_ms: active_task.official_mcp_adapter_age_ms,
                official_mcp_adapter_last_error: active_task.official_mcp_adapter_last_error,
            },
        );
    }
    let snapshot = if persisted_changed {
        Some(bump_state_revision(&mut state))
    } else {
        None
    };
    let mut release_task_ids = release_task_ids.into_iter().collect::<Vec<_>>();
    release_task_ids.sort();
    drop(state);
    if let Some(snapshot) = snapshot {
        persist_snapshot_if_current(&app, snapshot).await?;
    }
    Ok(Json(HelperHeartbeatHubResponse {
        ok: true,
        release_task_ids,
    }))
}

async fn claim_task_handler(
    State(app): State<AppState>,
    Json(payload): Json<ClaimTaskRequest>,
) -> Result<Json<ClaimTaskResponse>, HubError> {
    let mut state = app.hub.write().await;
    let cleanup_changed = cleanup_stale_state(&mut state);
    if state.blocked_helpers.contains_key(&payload.helper_id) {
        let snapshot = if cleanup_changed {
            Some(bump_state_revision(&mut state))
        } else {
            None
        };
        drop(state);
        if let Some(snapshot) = snapshot {
            persist_snapshot_if_current(&app, snapshot).await?;
        }
        return Ok(Json(ClaimTaskResponse {
            claimed: false,
            helper_id: payload.helper_id,
            task: None,
        }));
    }
    let helper = state
        .helpers
        .get(&payload.helper_id)
        .ok_or_else(|| HubError(eyre!("helper not found: {}", payload.helper_id)))?;
    let helper_owner = helper.owner_user.clone();
    let helper_capacity = helper.capacity;
    let helper_launch_count = helper.active_task_ids.len();
    let helper_id = helper.helper_id.clone();
    if helper_launch_count >= helper_capacity {
        let snapshot = if cleanup_changed {
            Some(bump_state_revision(&mut state))
        } else {
            None
        };
        drop(state);
        if let Some(snapshot) = snapshot {
            persist_snapshot_if_current(&app, snapshot).await?;
        }
        return Ok(Json(ClaimTaskResponse {
            claimed: false,
            helper_id,
            task: None,
        }));
    }
    let mut candidates = state
        .tasks
        .values_mut()
        .filter(|task| {
            !task.released
                && task.service_state == "ready"
                && task.accepting_launches
                && task.claimed_by_helper_id.is_none()
                && task.owner_user == helper_owner
                && task.routes.mcp_base_url.is_some()
        })
        .collect::<Vec<_>>();
    candidates.sort_by_key(|task| task.created_at_unix_ms);
    let Some(task) = candidates.into_iter().next() else {
        let snapshot = if cleanup_changed {
            Some(bump_state_revision(&mut state))
        } else {
            None
        };
        drop(state);
        if let Some(snapshot) = snapshot {
            persist_snapshot_if_current(&app, snapshot).await?;
        }
        return Ok(Json(ClaimTaskResponse {
            claimed: false,
            helper_id,
            task: None,
        }));
    };
    task.claimed_by_helper_id = Some(helper_id.clone());
    task.updated_at_unix_ms = now_unix_ms();
    let claimed_task_id = task.task_id.clone();
    let response_task = ClaimedTaskPayload {
        task_id: claimed_task_id.clone(),
        place_id: task.place_id.clone(),
        universe_id: task.universe_id.clone(),
        owner_user: task.owner_user.clone(),
        repo: task.repo.clone(),
        worktree_name: task.worktree_name.clone(),
        routes: task.routes.clone(),
    };
    if let Some(helper) = state.helpers.get_mut(&helper_id) {
        helper.last_seen_at = Instant::now();
        helper.active_task_ids.insert(claimed_task_id);
    }
    let snapshot = bump_state_revision(&mut state);
    drop(state);
    persist_snapshot_if_current(&app, snapshot).await?;
    Ok(Json(ClaimTaskResponse {
        claimed: true,
        helper_id,
        task: Some(response_task),
    }))
}

#[derive(Debug)]
struct HubError(color_eyre::Report);

struct HttpHubError {
    status: StatusCode,
    message: String,
    code: &'static str,
}

impl From<color_eyre::Report> for HubError {
    fn from(value: color_eyre::Report) -> Self {
        Self(value)
    }
}

impl IntoResponse for HubError {
    fn into_response(self) -> Response {
        (StatusCode::BAD_REQUEST, self.0.to_string()).into_response()
    }
}

impl HttpHubError {
    fn forbidden(code: &'static str, message: String) -> Self {
        Self {
            status: StatusCode::FORBIDDEN,
            message,
            code,
        }
    }

    fn conflict(code: &'static str, message: String) -> Self {
        Self {
            status: StatusCode::CONFLICT,
            message,
            code,
        }
    }
}

fn helper_admin_disabled_response() -> Response {
    HttpHubError::forbidden(
        "helper_admin_disabled",
        "helper admin endpoints are server-admin-only and disabled for this hub process".to_owned(),
    )
    .into_response()
}

impl IntoResponse for HttpHubError {
    fn into_response(self) -> Response {
        let mut response = (self.status, self.message).into_response();
        if let Ok(value) = HeaderValue::from_str(self.code) {
            response.headers_mut().insert("x-clock-p-hub-error", value);
        }
        response
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_app() -> AppState {
        let state_file =
            std::env::temp_dir().join(format!("roblox-hub-test-{}.json", Uuid::new_v4().simple()));
        let config = HubConfig {
            state_file,
            task_heartbeat_timeout: Duration::from_secs(30),
            helper_heartbeat_timeout: Duration::from_secs(30),
            heartbeat_interval_sec: HEARTBEAT_INTERVAL_SECS,
        };
        AppState {
            hub: Arc::new(RwLock::new(HubState {
                config,
                helpers: HashMap::new(),
                blocked_helpers: HashMap::new(),
                tasks: HashMap::new(),
                state_revision: 0,
            })),
            persist_lock: Arc::new(Mutex::new(())),
            allow_helper_admin: true,
        }
    }

    fn test_task(task_id: &str, helper_id: Option<&str>) -> TaskRecord {
        let now = now_unix_ms();
        TaskRecord {
            task_id: task_id.to_owned(),
            cluster_key: "cluster".to_owned(),
            task_token: "task-token".to_owned(),
            recover_token: "recover-token".to_owned(),
            place_id: "place".to_owned(),
            universe_id: Some("universe".to_owned()),
            owner_user: "user".to_owned(),
            repo: "repo".to_owned(),
            worktree_name: "worktree".to_owned(),
            service_state: "ready".to_owned(),
            accepting_launches: true,
            services: HashMap::new(),
            routes: TaskRoutes {
                mcp_base_url: Some("http://127.0.0.1:1".to_owned()),
                ..TaskRoutes::default()
            },
            created_at_unix_ms: now,
            updated_at_unix_ms: now,
            last_task_heartbeat_at_unix_ms: now,
            last_seen_at: Instant::now(),
            claimed_by_helper_id: helper_id.map(str::to_owned),
            released: false,
        }
    }

    fn test_helper(helper_id: &str, active_task_ids: &[&str]) -> HelperRecord {
        HelperRecord {
            helper_id: helper_id.to_owned(),
            owner_user: "user".to_owned(),
            platform: "test".to_owned(),
            capacity: 4,
            labels: Vec::new(),
            active_task_ids: active_task_ids
                .iter()
                .map(|task_id| (*task_id).to_owned())
                .collect(),
            active_tasks: HashMap::new(),
            registered_at_unix_ms: now_unix_ms(),
            last_seen_at: Instant::now(),
        }
    }

    async fn seed_claimed_task(app: &AppState, helper_id: &str, task_id: &str) {
        let mut state = app.hub.write().await;
        state
            .tasks
            .insert(task_id.to_owned(), test_task(task_id, Some(helper_id)));
        state
            .helpers
            .insert(helper_id.to_owned(), test_helper(helper_id, &[task_id]));
    }

    #[tokio::test]
    async fn helper_admin_endpoints_are_disabled_by_default() {
        let mut app = test_app();
        app.allow_helper_admin = false;

        let response = block_helper_handler(
            State(app.clone()),
            Json(BlockHelperRequest {
                helper_id: "h_test".to_owned(),
                reason: None,
            }),
        )
        .await
        .err()
        .expect("block helper should be rejected");
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        assert_eq!(
            response
                .headers()
                .get("x-clock-p-hub-error")
                .and_then(|value| value.to_str().ok()),
            Some("helper_admin_disabled")
        );

        let response = unblock_helper_handler(
            State(app),
            Json(UnblockHelperRequest {
                helper_id: "h_test".to_owned(),
            }),
        )
        .await
        .err()
        .expect("unblock helper should be rejected");
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        assert_eq!(
            response
                .headers()
                .get("x-clock-p-hub-error")
                .and_then(|value| value.to_str().ok()),
            Some("helper_admin_disabled")
        );
    }

    #[tokio::test]
    async fn blocked_helper_drains_by_heartbeat_ack() {
        let app = test_app();
        seed_claimed_task(&app, "h_test", "t1").await;

        let _ = block_helper_handler(
            State(app.clone()),
            Json(BlockHelperRequest {
                helper_id: "h_test".to_owned(),
                reason: Some("test".to_owned()),
            }),
        )
        .await
        .expect("block helper should succeed");

        {
            let state = app.hub.read().await;
            assert_eq!(
                state.tasks["t1"].claimed_by_helper_id.as_deref(),
                Some("h_test")
            );
            assert!(!state.helpers.contains_key("h_test"));
            assert!(state.blocked_helpers["h_test"]
                .pending_task_ids
                .contains("t1"));
        }

        let response = helper_heartbeat_handler(
            State(app.clone()),
            Json(HelperHeartbeatRequest {
                helper_id: "h_test".to_owned(),
                active_task_ids: vec!["t1".to_owned()],
                active_tasks: Vec::new(),
            }),
        )
        .await
        .expect("blocked heartbeat should respond")
        .0;
        assert!(!response.ok);
        assert_eq!(response.release_task_ids, vec!["t1".to_owned()]);
        {
            let state = app.hub.read().await;
            assert_eq!(
                state.tasks["t1"].claimed_by_helper_id.as_deref(),
                Some("h_test")
            );
            assert!(!state.helpers.contains_key("h_test"));
        }

        let response = helper_heartbeat_handler(
            State(app.clone()),
            Json(HelperHeartbeatRequest {
                helper_id: "h_test".to_owned(),
                active_task_ids: Vec::new(),
                active_tasks: Vec::new(),
            }),
        )
        .await
        .expect("ack heartbeat should respond")
        .0;
        assert!(!response.ok);
        assert!(response.release_task_ids.is_empty());
        let state = app.hub.read().await;
        assert!(state.tasks["t1"].claimed_by_helper_id.is_none());
        assert!(state.blocked_helpers["h_test"].pending_task_ids.is_empty());
    }

    #[tokio::test]
    async fn blocked_helper_active_tasks_field_counts_as_active_report() {
        let app = test_app();
        seed_claimed_task(&app, "h_test", "t1").await;
        let _ = block_helper_handler(
            State(app.clone()),
            Json(BlockHelperRequest {
                helper_id: "h_test".to_owned(),
                reason: None,
            }),
        )
        .await
        .expect("block helper should succeed");

        let response = helper_heartbeat_handler(
            State(app.clone()),
            Json(HelperHeartbeatRequest {
                helper_id: "h_test".to_owned(),
                active_task_ids: Vec::new(),
                active_tasks: vec![HelperActiveTaskHeartbeat {
                    task_id: "t1".to_owned(),
                    remote_state: "retrying".to_owned(),
                    remote_connection_id: None,
                    remote_last_error: Some("still active".to_owned()),
                    remote_last_ready_age_ms: None,
                    remote_last_server_message_age_ms: None,
                    studio_session_state: Some("play".to_owned()),
                    studio_session_state_age_ms: Some(15),
                    studio_mode: Some("start_play".to_owned()),
                    studio_mode_age_ms: Some(15),
                    studio_mode_source: Some("play_control".to_owned()),
                    studio_control_state: Some("ready".to_owned()),
                    studio_transition_phase: Some("running".to_owned()),
                    studio_transition_age_ms: None,
                    edit_runtime_state: Some("stale".to_owned()),
                    edit_runtime_age_ms: Some(20_000),
                    studio_control_last_error: None,
                    last_stop_request_id: 0,
                    last_stop_request_age_ms: None,
                    last_stop_request_source: None,
                    last_stop_request_error: None,
                    stop_intent_recorded: false,
                    stop_request_available: false,
                    stop_request_consumed: false,
                    end_test_requested: false,
                    runtime_actuator_last_poll_id: 0,
                    runtime_actuator_last_poll_age_ms: None,
                    runtime_actuator_last_error: None,
                    official_mcp_adapter_state: Some("blocked_by_studio_mode".to_owned()),
                    official_mcp_adapter_age_ms: Some(15),
                    official_mcp_adapter_last_error: None,
                }],
            }),
        )
        .await
        .expect("blocked heartbeat should respond")
        .0;
        assert_eq!(response.release_task_ids, vec!["t1".to_owned()]);
        let state = app.hub.read().await;
        assert_eq!(
            state.tasks["t1"].claimed_by_helper_id.as_deref(),
            Some("h_test")
        );
        assert!(state.blocked_helpers["h_test"]
            .pending_task_ids
            .contains("t1"));
        assert!(state.blocked_helpers["h_test"]
            .last_reported_task_ids
            .contains("t1"));
    }

    #[tokio::test]
    async fn pending_drain_survives_hub_reload_before_deadline() {
        let app = test_app();
        seed_claimed_task(&app, "h_test", "t1").await;
        let _ = block_helper_handler(
            State(app.clone()),
            Json(BlockHelperRequest {
                helper_id: "h_test".to_owned(),
                reason: Some("reload".to_owned()),
            }),
        )
        .await
        .expect("block helper should succeed");
        let config = {
            let state = app.hub.read().await;
            state.config.clone()
        };

        let (tasks, blocked_helpers) =
            load_persisted_state(&config).expect("persisted hub state should reload");
        assert_eq!(tasks["t1"].claimed_by_helper_id.as_deref(), Some("h_test"));
        assert!(blocked_helpers["h_test"].pending_task_ids.contains("t1"));
        assert!(!blocked_helpers["h_test"]
            .force_released_task_ids
            .contains("t1"));
    }

    #[tokio::test]
    async fn blocked_helper_force_releases_after_drain_deadline() {
        let app = test_app();
        seed_claimed_task(&app, "h_test", "t1").await;
        {
            let mut state = app.hub.write().await;
            let now = now_unix_ms();
            state.blocked_helpers.insert(
                "h_test".to_owned(),
                BlockedHelperRecord {
                    helper_id: "h_test".to_owned(),
                    reason: Some("test".to_owned()),
                    blocked_at_unix_ms: now.saturating_sub(10_000),
                    drain_started_at_unix_ms: now.saturating_sub(10_000),
                    drain_deadline_at_unix_ms: now.saturating_sub(1),
                    pending_task_ids: HashSet::from(["t1".to_owned()]),
                    last_reported_task_ids: HashSet::from(["t1".to_owned()]),
                    last_release_requested_at_unix_ms: Some(now.saturating_sub(5_000)),
                    last_seen_at_unix_ms: Some(now.saturating_sub(5_000)),
                    force_released_task_ids: HashSet::new(),
                },
            );
            assert!(cleanup_stale_state(&mut state));
        }
        let state = app.hub.read().await;
        assert!(state.tasks["t1"].claimed_by_helper_id.is_none());
        assert!(state.blocked_helpers["h_test"].pending_task_ids.is_empty());
        assert!(state.blocked_helpers["h_test"]
            .force_released_task_ids
            .contains("t1"));
        let payload = blocked_helper_payload(&state.blocked_helpers["h_test"]);
        assert_eq!(payload.drain_status, "force_released");
        assert!(payload.drain_deadline_elapsed);
        drop(state);

        let response = helper_heartbeat_handler(
            State(app),
            Json(HelperHeartbeatRequest {
                helper_id: "h_test".to_owned(),
                active_task_ids: vec!["t1".to_owned()],
                active_tasks: Vec::new(),
            }),
        )
        .await
        .expect("late blocked heartbeat should still respond")
        .0;
        assert_eq!(response.release_task_ids, vec!["t1".to_owned()]);
    }

    #[tokio::test]
    async fn unblock_rejects_pending_drain() {
        let app = test_app();
        seed_claimed_task(&app, "h_test", "t1").await;
        let _ = block_helper_handler(
            State(app.clone()),
            Json(BlockHelperRequest {
                helper_id: "h_test".to_owned(),
                reason: None,
            }),
        )
        .await
        .expect("block helper should succeed");

        let response = unblock_helper_handler(
            State(app),
            Json(UnblockHelperRequest {
                helper_id: "h_test".to_owned(),
            }),
        )
        .await
        .expect_err("pending drain should reject unblock");
        assert_eq!(response.status(), StatusCode::CONFLICT);
        assert_eq!(
            response
                .headers()
                .get("x-clock-p-hub-error")
                .and_then(|value| value.to_str().ok()),
            Some("helper_drain_pending")
        );
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    color_eyre::install()?;
    let env_filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(env_filter)
        .with_writer(io::stderr)
        .with_target(false)
        .with_thread_ids(true)
        .init();

    let args = Args::parse();
    let config = resolve_hub_config(&args)?;
    let auth_state = HttpAuthState {
        bearer_token: resolve_http_bearer_token(&args)?,
    };
    let (tasks, blocked_helpers) = load_persisted_state(&config)?;
    let hub = Arc::new(RwLock::new(HubState {
        tasks,
        blocked_helpers,
        config,
        helpers: HashMap::new(),
        state_revision: 0,
    }));
    let persist_lock = Arc::new(Mutex::new(()));
    let app_state = AppState {
        hub: Arc::clone(&hub),
        persist_lock: Arc::clone(&persist_lock),
        allow_helper_admin: args.allow_helper_admin,
    };

    let cleanup_state = app_state.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(2));
        loop {
            interval.tick().await;
            let snapshot = {
                let mut state = cleanup_state.hub.write().await;
                if cleanup_stale_state(&mut state) {
                    Some(bump_state_revision(&mut state))
                } else {
                    None
                }
            };
            if let Some(snapshot) = snapshot {
                let _ = persist_snapshot_if_current(&cleanup_state, snapshot).await;
            }
        }
    });

    let app = Router::new()
        .route("/status", get(status_handler))
        .route("/v1/tasks/{task_id}", get(task_status_handler))
        .route("/v1/debug/helpers/{helper_id}", get(debug_helper_handler))
        .route("/v1/tasks/create", post(create_task_handler))
        .route("/v1/tasks/recover", post(recover_task_handler))
        .route("/v1/tasks/heartbeat", post(task_heartbeat_handler))
        .route("/v1/tasks/release", post(release_task_handler))
        .route("/v1/helpers", get(helpers_handler))
        .route("/v1/helpers/register", post(register_helper_handler))
        .route("/v1/helpers/heartbeat", post(helper_heartbeat_handler))
        .route("/v1/helpers/claim", post(claim_task_handler))
        .route("/v1/helpers/block", post(block_helper_handler))
        .route("/v1/helpers/unblock", post(unblock_helper_handler))
        .with_state(app_state)
        .layer(middleware::from_fn_with_state(
            auth_state,
            require_http_auth,
        ));

    let listener = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, args.port)).await?;
    tracing::info!(port = args.port, "roblox hub listening on 127.0.0.1");
    axum::serve(listener, app).await?;
    Ok(())
}
