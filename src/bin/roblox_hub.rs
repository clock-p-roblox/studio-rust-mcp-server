use axum::{
    extract::{Request, State},
    http::{header, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use clap::Parser;
use color_eyre::eyre::{eyre, Result, WrapErr};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::io;
use std::net::Ipv4Addr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::sync::Mutex;
use tracing_subscriber::{self, EnvFilter};
use uuid::Uuid;

const DEFAULT_HUB_PORT: u16 = 44758;
const TASK_HEARTBEAT_TIMEOUT: Duration = Duration::from_secs(30);
const HELPER_HEARTBEAT_TIMEOUT: Duration = Duration::from_secs(30);
const HEARTBEAT_INTERVAL_SECS: u64 = 10;

#[derive(Debug, Clone)]
struct HubConfig {
    state_file: PathBuf,
    task_heartbeat_timeout: Duration,
    helper_heartbeat_timeout: Duration,
    heartbeat_interval_sec: u64,
}

#[derive(Parser, Debug)]
#[command(version, about = "Task/helper control plane for clock-p Roblox debug clusters")]
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
}

#[derive(Clone)]
struct HttpAuthState {
    bearer_token: Option<String>,
}

#[derive(Debug)]
struct HubState {
    config: HubConfig,
    helpers: HashMap<String, HelperRecord>,
    tasks: HashMap<String, TaskRecord>,
}

type SharedHubState = Arc<Mutex<HubState>>;

#[derive(Debug)]
struct HelperRecord {
    helper_id: String,
    owner_user: String,
    platform: String,
    capacity: usize,
    labels: Vec<String>,
    active_launches: HashMap<String, String>,
    registered_at_unix_ms: u64,
    last_seen_at: Instant,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct TaskRoutes {
    rojo_base_url: Option<String>,
    mcp_base_url: Option<String>,
    runtime_log_base_url: Option<String>,
}

#[derive(Debug)]
struct TaskRecord {
    task_id: String,
    cluster_key: String,
    generation: u32,
    task_token: String,
    recover_token: String,
    place_id: String,
    game_id: Option<String>,
    owner_user: String,
    repo: String,
    worktree_name: String,
    service_state: String,
    accepting_launches: bool,
    services: HashMap<String, String>,
    routes: TaskRoutes,
    created_at_unix_ms: u64,
    updated_at_unix_ms: u64,
    last_seen_at: Instant,
    claimed_by_helper_id: Option<String>,
    active_launch_id: Option<String>,
    released: bool,
}

#[derive(Debug, Serialize, Deserialize)]
struct PersistedHubState {
    tasks: Vec<PersistedTaskRecord>,
}

#[derive(Debug, Serialize, Deserialize)]
struct PersistedTaskRecord {
    task_id: String,
    cluster_key: String,
    generation: u32,
    task_token: String,
    recover_token: String,
    place_id: String,
    game_id: Option<String>,
    owner_user: String,
    repo: String,
    worktree_name: String,
    service_state: String,
    accepting_launches: bool,
    services: HashMap<String, String>,
    routes: TaskRoutes,
    created_at_unix_ms: u64,
    updated_at_unix_ms: u64,
    claimed_by_helper_id: Option<String>,
    active_launch_id: Option<String>,
    released: bool,
}

#[derive(Debug, Deserialize)]
struct CreateTaskRequest {
    cluster_key: String,
    owner_user: String,
    repo: String,
    worktree_name: String,
    place_id: String,
    game_id: Option<String>,
}

#[derive(Debug, Serialize)]
struct CreateTaskResponse {
    task_id: String,
    generation: u32,
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
}

#[derive(Debug, Deserialize)]
struct TaskHeartbeatRequest {
    task_id: String,
    generation: u32,
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
    generation: u32,
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
    active_launches: Vec<HelperLaunchState>,
}

#[derive(Debug, Serialize)]
struct HelperHeartbeatHubResponse {
    ok: bool,
    release_task_ids: Vec<String>,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
struct HelperLaunchState {
    launch_id: String,
    task_id: String,
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
    launch_id: String,
    task_id: String,
    generation: u32,
    place_id: String,
    game_id: Option<String>,
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
    tasks: Vec<TaskStatusPayload>,
}

#[derive(Debug, Serialize)]
struct HelperStatusPayload {
    helper_id: String,
    owner_user: String,
    platform: String,
    capacity: usize,
    active_launch_count: usize,
    labels: Vec<String>,
    registered_at_unix_ms: u64,
    last_seen_age_ms: u128,
}

#[derive(Debug, Serialize)]
struct TaskStatusPayload {
    task_id: String,
    cluster_key: String,
    generation: u32,
    place_id: String,
    game_id: Option<String>,
    owner_user: String,
    repo: String,
    worktree_name: String,
    service_state: String,
    accepting_launches: bool,
    claimed_by_helper_id: Option<String>,
    active_launch_id: Option<String>,
    released: bool,
    routes: TaskRoutes,
    services: HashMap<String, String>,
    created_at_unix_ms: u64,
    updated_at_unix_ms: u64,
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

fn default_feishu_token_path() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .map(|home| home.join(".dev.clock-p.com").join("feishu-token"))
}

fn default_hub_state_path() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .map(|home| home.join(".dev.clock-p.com").join("roblox-hub").join("state.json"))
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
    if let Some(path) = default_feishu_token_path() {
        if path.exists() {
            return load_token_from_file(&path);
        }
    }
    Ok(None)
}

fn resolve_hub_config(args: &Args) -> Result<HubConfig> {
    let state_file = match args.state_file.as_ref() {
        Some(path) => path.clone(),
        None => default_hub_state_path().ok_or_else(|| eyre!("cannot resolve default hub state path"))?,
    };
    Ok(HubConfig {
        state_file,
        task_heartbeat_timeout: Duration::from_secs(args.task_heartbeat_timeout_sec),
        helper_heartbeat_timeout: Duration::from_secs(args.helper_heartbeat_timeout_sec),
        heartbeat_interval_sec: args.heartbeat_interval_sec,
    })
}

fn load_persisted_tasks(config: &HubConfig) -> Result<HashMap<String, TaskRecord>> {
    if !config.state_file.exists() {
        return Ok(HashMap::new());
    }
    let body = fs::read_to_string(&config.state_file).wrap_err_with(|| {
        format!("failed to read hub state file {}", config.state_file.display())
    })?;
    let persisted: PersistedHubState = serde_json::from_str(&body).wrap_err_with(|| {
        format!("failed to parse hub state file {}", config.state_file.display())
    })?;
    let mut tasks = HashMap::new();
    for task in persisted.tasks {
        tasks.insert(
            task.task_id.clone(),
            TaskRecord {
                task_id: task.task_id,
                cluster_key: task.cluster_key,
                generation: task.generation,
                task_token: task.task_token,
                recover_token: task.recover_token,
                place_id: task.place_id,
                game_id: task.game_id,
                owner_user: task.owner_user,
                repo: task.repo,
                worktree_name: task.worktree_name,
                service_state: task.service_state,
                accepting_launches: task.accepting_launches,
                services: task.services,
                routes: task.routes,
                created_at_unix_ms: task.created_at_unix_ms,
                updated_at_unix_ms: task.updated_at_unix_ms,
                last_seen_at: Instant::now(),
                claimed_by_helper_id: task.claimed_by_helper_id,
                active_launch_id: task.active_launch_id,
                released: task.released,
            },
        );
    }
    Ok(tasks)
}

fn persist_state(state: &HubState) -> Result<()> {
    let parent = state
        .config
        .state_file
        .parent()
        .ok_or_else(|| eyre!("hub state file has no parent directory"))?;
    fs::create_dir_all(parent)?;
    let payload = PersistedHubState {
        tasks: state
            .tasks
            .values()
            .map(|task| PersistedTaskRecord {
                task_id: task.task_id.clone(),
                cluster_key: task.cluster_key.clone(),
                generation: task.generation,
                task_token: task.task_token.clone(),
                recover_token: task.recover_token.clone(),
                place_id: task.place_id.clone(),
                game_id: task.game_id.clone(),
                owner_user: task.owner_user.clone(),
                repo: task.repo.clone(),
                worktree_name: task.worktree_name.clone(),
                service_state: task.service_state.clone(),
                accepting_launches: task.accepting_launches,
                services: task.services.clone(),
                routes: task.routes.clone(),
                created_at_unix_ms: task.created_at_unix_ms,
                updated_at_unix_ms: task.updated_at_unix_ms,
                claimed_by_helper_id: task.claimed_by_helper_id.clone(),
                active_launch_id: task.active_launch_id.clone(),
                released: task.released,
            })
            .collect(),
    };
    let tmp_path = state.config.state_file.with_extension("json.tmp");
    fs::write(&tmp_path, format!("{}\n", serde_json::to_string_pretty(&payload)?))?;
    fs::rename(&tmp_path, &state.config.state_file)?;
    Ok(())
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

fn require_non_empty(value: &str, label: &str) -> Result<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err(eyre!("{label} must not be empty"));
    }
    Ok(trimmed.to_owned())
}

fn new_task_id() -> String {
    let short = Uuid::new_v4().simple().to_string();
    format!("t{}", &short[..10])
}

fn new_token() -> String {
    Uuid::new_v4().to_string()
}

fn cleanup_stale_state(state: &mut HubState) -> bool {
    let mut changed = false;
    let stale_helpers: Vec<String> = state
        .helpers
        .iter()
        .filter(|(_, helper)| helper.last_seen_at.elapsed() > state.config.helper_heartbeat_timeout)
        .map(|(helper_id, _)| helper_id.clone())
        .collect();
    for helper_id in stale_helpers {
        state.helpers.remove(&helper_id);
        changed = true;
        for task in state.tasks.values_mut() {
            if task.claimed_by_helper_id.as_deref() == Some(helper_id.as_str()) {
                task.claimed_by_helper_id = None;
                task.active_launch_id = None;
                task.updated_at_unix_ms = now_unix_ms();
                changed = true;
            }
        }
    }
    for task in state.tasks.values_mut() {
        if task.released {
            continue;
        }
        if task.last_seen_at.elapsed() > state.config.task_heartbeat_timeout {
            task.service_state = "expired".to_owned();
            task.accepting_launches = false;
            task.claimed_by_helper_id = None;
            task.active_launch_id = None;
            task.updated_at_unix_ms = now_unix_ms();
            changed = true;
        }
    }
    changed
}

async fn status_handler(State(state): State<SharedHubState>) -> Json<HubStatusResponse> {
    let mut state = state.lock().await;
    if cleanup_stale_state(&mut state) {
        let _ = persist_state(&state);
    }
    let mut helpers = state
        .helpers
        .values()
        .map(|helper| HelperStatusPayload {
            helper_id: helper.helper_id.clone(),
            owner_user: helper.owner_user.clone(),
            platform: helper.platform.clone(),
            capacity: helper.capacity,
            active_launch_count: helper.active_launches.len(),
            labels: helper.labels.clone(),
            registered_at_unix_ms: helper.registered_at_unix_ms,
            last_seen_age_ms: helper.last_seen_at.elapsed().as_millis(),
        })
        .collect::<Vec<_>>();
    helpers.sort_by(|left, right| left.helper_id.cmp(&right.helper_id));
    let mut tasks = state
        .tasks
        .values()
        .map(|task| TaskStatusPayload {
            task_id: task.task_id.clone(),
            cluster_key: task.cluster_key.clone(),
            generation: task.generation,
            place_id: task.place_id.clone(),
            game_id: task.game_id.clone(),
            owner_user: task.owner_user.clone(),
            repo: task.repo.clone(),
            worktree_name: task.worktree_name.clone(),
            service_state: task.service_state.clone(),
            accepting_launches: task.accepting_launches,
            claimed_by_helper_id: task.claimed_by_helper_id.clone(),
            active_launch_id: task.active_launch_id.clone(),
            released: task.released,
            routes: task.routes.clone(),
            services: task.services.clone(),
            created_at_unix_ms: task.created_at_unix_ms,
            updated_at_unix_ms: task.updated_at_unix_ms,
            last_seen_age_ms: task.last_seen_at.elapsed().as_millis(),
        })
        .collect::<Vec<_>>();
    tasks.sort_by(|left, right| left.task_id.cmp(&right.task_id));
    Json(HubStatusResponse {
        ok: true,
        helper_count: helpers.len(),
        task_count: tasks.len(),
        helpers,
        tasks,
    })
}

async fn create_task_handler(
    State(state): State<SharedHubState>,
    Json(payload): Json<CreateTaskRequest>,
) -> Result<Json<CreateTaskResponse>, HubError> {
    let mut state = state.lock().await;
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
            generation: 1,
            task_token: task_token.clone(),
            recover_token: recover_token.clone(),
            place_id: require_non_empty(&payload.place_id, "place_id")?,
            game_id: payload.game_id.filter(|value| !value.trim().is_empty()),
            owner_user: require_non_empty(&payload.owner_user, "owner_user")?,
            repo: require_non_empty(&payload.repo, "repo")?,
            worktree_name: require_non_empty(&payload.worktree_name, "worktree_name")?,
            service_state: "starting".to_owned(),
            accepting_launches: false,
            services: HashMap::new(),
            routes: TaskRoutes::default(),
            created_at_unix_ms: now,
            updated_at_unix_ms: now,
            last_seen_at: Instant::now(),
            claimed_by_helper_id: None,
            active_launch_id: None,
            released: false,
        },
    );
    persist_state(&state)?;
    Ok(Json(CreateTaskResponse {
        task_id,
        generation: 1,
        task_token,
        recover_token,
        heartbeat_interval_sec: state.config.heartbeat_interval_sec,
        heartbeat_timeout_sec: state.config.task_heartbeat_timeout.as_secs(),
    }))
}

async fn recover_task_handler(
    State(state): State<SharedHubState>,
    Json(payload): Json<RecoverTaskRequest>,
) -> Result<Json<CreateTaskResponse>, HubError> {
    let mut state = state.lock().await;
    if cleanup_stale_state(&mut state) {
        persist_state(&state)?;
    }
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
    task.generation += 1;
    task.task_token = new_token();
    task.service_state = "recovering".to_owned();
    task.accepting_launches = false;
    task.claimed_by_helper_id = None;
    task.active_launch_id = None;
    task.updated_at_unix_ms = now_unix_ms();
    task.last_seen_at = Instant::now();
    let response_task_id = task.task_id.clone();
    let response_generation = task.generation;
    let response_task_token = task.task_token.clone();
    let response_recover_token = task.recover_token.clone();
    persist_state(&state)?;
    Ok(Json(CreateTaskResponse {
        task_id: response_task_id,
        generation: response_generation,
        task_token: response_task_token,
        recover_token: response_recover_token,
        heartbeat_interval_sec: state.config.heartbeat_interval_sec,
        heartbeat_timeout_sec: state.config.task_heartbeat_timeout.as_secs(),
    }))
}

async fn task_heartbeat_handler(
    State(state): State<SharedHubState>,
    Json(payload): Json<TaskHeartbeatRequest>,
) -> Result<Json<serde_json::Value>, HubError> {
    let mut state = state.lock().await;
    if cleanup_stale_state(&mut state) {
        persist_state(&state)?;
    }
    let task = state
        .tasks
        .get_mut(&payload.task_id)
        .ok_or_else(|| HubError(eyre!("task not found: {}", payload.task_id)))?;
    if task.generation != payload.generation {
        return Err(HubError(eyre!("task generation mismatch")));
    }
    if task.task_token != payload.task_token {
        return Err(HubError(eyre!("task token mismatch")));
    }
    task.service_state = require_non_empty(&payload.service_state, "service_state")?;
    task.accepting_launches = payload.accepting_launches;
    task.routes = payload.routes;
    task.services = payload.services;
    task.updated_at_unix_ms = now_unix_ms();
    task.last_seen_at = Instant::now();
    persist_state(&state)?;
    Ok(Json(serde_json::json!({"ok": true})))
}

async fn release_task_handler(
    State(state): State<SharedHubState>,
    Json(payload): Json<ReleaseTaskRequest>,
) -> Result<Json<serde_json::Value>, HubError> {
    let mut state = state.lock().await;
    if cleanup_stale_state(&mut state) {
        persist_state(&state)?;
    }
    let helper_id = {
        let task = state
            .tasks
            .get_mut(&payload.task_id)
            .ok_or_else(|| HubError(eyre!("task not found: {}", payload.task_id)))?;
        if task.generation != payload.generation {
            return Err(HubError(eyre!("task generation mismatch")));
        }
        if task.task_token != payload.task_token {
            return Err(HubError(eyre!("task token mismatch")));
        }
        let helper_id = task.claimed_by_helper_id.clone();
        task.service_state = "released".to_owned();
        task.accepting_launches = false;
        task.claimed_by_helper_id = None;
        task.active_launch_id = None;
        task.released = true;
        task.updated_at_unix_ms = now_unix_ms();
        helper_id
    };
    if let Some(helper_id) = helper_id.as_ref() {
        if let Some(helper) = state.helpers.get_mut(helper_id) {
            helper.active_launches.retain(|_, task_id| task_id != &payload.task_id);
        }
    }
    persist_state(&state)?;
    Ok(Json(serde_json::json!({"ok": true})))
}

async fn register_helper_handler(
    State(state): State<SharedHubState>,
    Json(payload): Json<RegisterHelperRequest>,
) -> Result<Json<RegisterHelperResponse>, HubError> {
    let mut state = state.lock().await;
    if cleanup_stale_state(&mut state) {
        persist_state(&state)?;
    }
    let helper_id = require_non_empty(&payload.helper_id, "helper_id")?;
    let now = now_unix_ms();
    state.helpers.insert(
        helper_id.clone(),
        HelperRecord {
            helper_id: helper_id.clone(),
            owner_user: require_non_empty(&payload.owner_user, "owner_user")?,
            platform: require_non_empty(&payload.platform, "platform")?,
            capacity: payload.capacity,
            labels: payload.labels,
            active_launches: HashMap::new(),
            registered_at_unix_ms: now,
            last_seen_at: Instant::now(),
        },
    );
    Ok(Json(RegisterHelperResponse {
        helper_id,
        heartbeat_interval_sec: state.config.heartbeat_interval_sec,
        heartbeat_timeout_sec: state.config.helper_heartbeat_timeout.as_secs(),
    }))
}

async fn helper_heartbeat_handler(
    State(state): State<SharedHubState>,
    Json(payload): Json<HelperHeartbeatRequest>,
) -> Result<Json<HelperHeartbeatHubResponse>, HubError> {
    let mut state = state.lock().await;
    if cleanup_stale_state(&mut state) {
        persist_state(&state)?;
    }
    let now = now_unix_ms();
    let mut release_task_ids = Vec::new();
    let active_launches_by_task: HashMap<String, String> = payload
        .active_launches
        .iter()
        .map(|launch| (launch.task_id.clone(), launch.launch_id.clone()))
        .collect();
    let mut changed = false;

    for task in state.tasks.values_mut() {
        if task.claimed_by_helper_id.as_deref() != Some(payload.helper_id.as_str()) {
            continue;
        }
        match active_launches_by_task.get(&task.task_id) {
            Some(launch_id) => {
                if task.released || task.service_state == "expired" {
                    release_task_ids.push(task.task_id.clone());
                    continue;
                }
                if task.active_launch_id.as_deref() != Some(launch_id.as_str()) {
                    task.active_launch_id = Some(launch_id.clone());
                    task.updated_at_unix_ms = now;
                    changed = true;
                }
            }
            None => {
                task.claimed_by_helper_id = None;
                task.active_launch_id = None;
                task.updated_at_unix_ms = now;
                changed = true;
            }
        }
    }

    for launch in &payload.active_launches {
        let should_release = match state.tasks.get_mut(&launch.task_id) {
            Some(task) => {
                if task.released || task.service_state == "expired" {
                    true
                } else {
                    if task.claimed_by_helper_id.as_deref() != Some(payload.helper_id.as_str())
                        || task.active_launch_id.as_deref() != Some(launch.launch_id.as_str())
                    {
                        task.claimed_by_helper_id = Some(payload.helper_id.clone());
                        task.active_launch_id = Some(launch.launch_id.clone());
                        task.updated_at_unix_ms = now;
                        changed = true;
                    }
                    false
                }
            }
            None => true,
        };
        if should_release {
            if !release_task_ids.iter().any(|task_id| task_id == &launch.task_id) {
                release_task_ids.push(launch.task_id.clone());
            }
        }
    }
    let helper = state
        .helpers
        .get_mut(&payload.helper_id)
        .ok_or_else(|| HubError(eyre!("helper not found: {}", payload.helper_id)))?;
    helper.last_seen_at = Instant::now();
    helper.active_launches.clear();
    for launch in payload.active_launches {
        if release_task_ids.iter().any(|task_id| task_id == &launch.task_id) {
            continue;
        }
        helper.active_launches.insert(launch.launch_id, launch.task_id);
    }
    if changed {
        persist_state(&state)?;
    }
    Ok(Json(HelperHeartbeatHubResponse {
        ok: true,
        release_task_ids,
    }))
}

async fn claim_task_handler(
    State(state): State<SharedHubState>,
    Json(payload): Json<ClaimTaskRequest>,
) -> Result<Json<ClaimTaskResponse>, HubError> {
    let mut state = state.lock().await;
    if cleanup_stale_state(&mut state) {
        persist_state(&state)?;
    }
    let helper = state
        .helpers
        .get(&payload.helper_id)
        .ok_or_else(|| HubError(eyre!("helper not found: {}", payload.helper_id)))?;
    let helper_owner = helper.owner_user.clone();
    let helper_capacity = helper.capacity;
    let helper_launch_count = helper.active_launches.len();
    let helper_id = helper.helper_id.clone();
    if helper_launch_count >= helper_capacity {
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
        return Ok(Json(ClaimTaskResponse {
            claimed: false,
            helper_id,
            task: None,
        }));
    };
    let launch_id = format!("l_{}", &Uuid::new_v4().simple().to_string()[..10]);
    task.claimed_by_helper_id = Some(helper_id.clone());
    task.active_launch_id = Some(launch_id.clone());
    task.updated_at_unix_ms = now_unix_ms();
    let claimed_task_id = task.task_id.clone();
    let response_task = ClaimedTaskPayload {
        launch_id: launch_id.clone(),
        task_id: claimed_task_id.clone(),
        generation: task.generation,
        place_id: task.place_id.clone(),
        game_id: task.game_id.clone(),
        owner_user: task.owner_user.clone(),
        repo: task.repo.clone(),
        worktree_name: task.worktree_name.clone(),
        routes: task.routes.clone(),
    };
    if let Some(helper) = state.helpers.get_mut(&helper_id) {
        helper.last_seen_at = Instant::now();
        helper
            .active_launches
            .insert(launch_id.clone(), claimed_task_id);
    }
    persist_state(&state)?;
    Ok(Json(ClaimTaskResponse {
        claimed: true,
        helper_id,
        task: Some(response_task),
    }))
}

#[derive(Debug)]
struct HubError(color_eyre::Report);

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
    let state = Arc::new(Mutex::new(HubState {
        tasks: load_persisted_tasks(&config)?,
        config,
        helpers: HashMap::new(),
    }));

    let cleanup_state = Arc::clone(&state);
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(2));
        loop {
            interval.tick().await;
            let mut state = cleanup_state.lock().await;
            if cleanup_stale_state(&mut state) {
                let _ = persist_state(&state);
            }
        }
    });

    let app = Router::new()
        .route("/status", get(status_handler))
        .route("/v1/tasks/create", post(create_task_handler))
        .route("/v1/tasks/recover", post(recover_task_handler))
        .route("/v1/tasks/heartbeat", post(task_heartbeat_handler))
        .route("/v1/tasks/release", post(release_task_handler))
        .route("/v1/helpers/register", post(register_helper_handler))
        .route("/v1/helpers/heartbeat", post(helper_heartbeat_handler))
        .route("/v1/helpers/claim", post(claim_task_handler))
        .with_state(Arc::clone(&state))
        .layer(middleware::from_fn_with_state(auth_state, require_http_auth));

    let listener = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, args.port)).await?;
    tracing::info!(port = args.port, "roblox hub listening on 127.0.0.1");
    axum::serve(listener, app).await?;
    Ok(())
}
