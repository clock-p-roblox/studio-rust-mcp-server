use axum::extract::{ConnectInfo, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use base64::Engine as _;
use clap::Parser;
use color_eyre::eyre::{eyre, Result, WrapErr};
use futures_util::{SinkExt, StreamExt};
use reqwest::header::{AUTHORIZATION, CONTENT_TYPE};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use serde_json::Value;
use std::collections::{HashMap, HashSet, VecDeque};
use std::env;
use std::fs;
use std::net::{Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::process::Child;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, oneshot, watch, Mutex, Notify};
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::protocol::Message as WsMessage;
use tracing_subscriber::EnvFilter;
use uuid::Uuid;

#[path = "../helper_ws.rs"]
mod helper_ws;

use helper_ws::{
    ArtifactAbort, ArtifactBegin, ArtifactChunk, ArtifactCommitted, ArtifactFinish, HelperHello,
    HelperToServerMessage, ServerToHelperMessage, HELPER_WS_PATH, MAX_ARTIFACT_CHUNK_PAYLOAD_BYTES,
};

#[cfg(target_os = "windows")]
use regex::Regex;
#[cfg(target_os = "windows")]
use std::mem::size_of;
#[cfg(target_os = "windows")]
use std::ptr::null_mut;
#[cfg(target_os = "windows")]
use std::time::{SystemTime, UNIX_EPOCH};
#[cfg(target_os = "windows")]
use windows_sys::Win32::Foundation::{HWND, LPARAM, RECT};
#[cfg(target_os = "windows")]
use windows_sys::Win32::NetworkManagement::IpHelper::{
    GetExtendedTcpTable, MIB_TCPROW_OWNER_PID, MIB_TCPTABLE_OWNER_PID, TCP_TABLE_OWNER_PID_ALL,
};
#[cfg(target_os = "windows")]
use windows_sys::Win32::Networking::WinSock::AF_INET;
#[cfg(target_os = "windows")]
use windows_sys::Win32::UI::WindowsAndMessaging::{
    EnumChildWindows, EnumWindows, GetClientRect, GetWindowRect, GetWindowTextLengthW,
    GetWindowTextW, GetWindowThreadProcessId, IsWindowVisible,
};

const DEFAULT_DOMAIN_SUFFIX: &str = "dev.clock-p.com";
const DEFAULT_HELPER_PORT: u16 = 44750;
const LOCAL_LONG_POLL_TIMEOUT: Duration = Duration::from_secs(15);
const PLUGIN_REQUEST_TIMEOUT: Duration = Duration::from_secs(120);
const INSTANCE_STALE_AFTER: Duration = Duration::from_secs(45);

#[derive(Parser, Debug, Clone)]
struct Args {
    #[arg(long, default_value_t = DEFAULT_HELPER_PORT)]
    port: u16,
    #[arg(long)]
    user_name: Option<String>,
    #[arg(long)]
    bearer_token: Option<String>,
    #[arg(long)]
    bearer_token_file: Option<PathBuf>,
    #[arg(long, default_value = DEFAULT_DOMAIN_SUFFIX)]
    domain_suffix: String,
    #[arg(long)]
    helper_id: Option<String>,
    #[arg(long)]
    hub_base_url: Option<String>,
    #[arg(long)]
    studio_path: Option<PathBuf>,
    #[arg(long, default_value_t = 4)]
    capacity: usize,
}

#[derive(Clone)]
struct HelperConfig {
    port: u16,
    helper_id: String,
    capacity: usize,
    user_name: String,
    bearer_token: Arc<Mutex<String>>,
    bearer_token_source: Arc<Mutex<String>>,
    bearer_token_candidates: Arc<Vec<ResolvedToken>>,
    domain_suffix: String,
    hub_base_url: Option<String>,
    studio_path: Option<PathBuf>,
    client: reqwest::Client,
}

#[derive(Clone)]
struct ResolvedToken {
    value: String,
    source: String,
}

struct PluginInstance {
    place_id: String,
    task_id: Option<String>,
    generation: Option<u32>,
    launch_id: Option<String>,
    remote_base_url: String,
    studio_pid: Option<u32>,
    last_seen_at: Instant,
    queue: VecDeque<Value>,
    notify: Arc<Notify>,
}

struct HelperState {
    instances: HashMap<String, PluginInstance>,
    claimed_tasks: HashMap<String, ClaimedTask>,
    launch_processes: HashMap<String, LaunchProcessRecord>,
    waiting_for_plugin: HashMap<String, oneshot::Sender<PluginResponsePayload>>,
    remote_connections: HashMap<String, RemoteConnectionHandle>,
    last_remote_errors: HashMap<String, String>,
    hub_last_error: Option<String>,
    hub_last_ready_at: Option<Instant>,
}

#[derive(Clone)]
struct ClaimedTask {
    task_id: String,
    launch_id: String,
    generation: u32,
    place_id: String,
    game_id: Option<String>,
    mcp_base_url: String,
    rojo_base_url: Option<String>,
    runtime_log_base_url: Option<String>,
    claimed_at: Instant,
}

struct LaunchProcessRecord {
    task_id: String,
    launch_id: String,
    generation: u32,
    place_id: String,
    game_id: Option<String>,
    studio_pid: u32,
    launched_by_helper: bool,
    child: Option<Child>,
}

struct PendingLaunchTermination {
    task_id: String,
    launch_id: String,
    generation: u32,
    studio_pid: u32,
    child: Child,
}

fn runtime_screenshot_upload_url_for_base_url(base_url: &str) -> String {
    format!("{}/v1/runtime-screenshots", base_url.trim_end_matches('/'))
}

#[derive(Clone)]
struct RemoteTarget {
    connection_key: String,
    place_id: String,
    task_id: Option<String>,
    generation: Option<u32>,
    launch_id: Option<String>,
    remote_base_url: String,
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
enum RemoteConnectionState {
    Connecting,
    Connected,
    Retrying,
}

struct RemoteConnectionHandle {
    stop_tx: watch::Sender<bool>,
    sender: mpsc::UnboundedSender<RemoteOutgoingFrame>,
    remote_base_url: String,
    place_id: String,
    task_id: Option<String>,
    generation: Option<u32>,
    launch_id: Option<String>,
    connection_state: RemoteConnectionState,
    connection_id: Option<String>,
    last_ready_at: Option<Instant>,
    last_server_message_at: Option<Instant>,
}

#[derive(Clone)]
enum RemoteOutgoingFrame {
    Text(String),
    Pong(Vec<u8>),
}

type SharedState = Arc<Mutex<HelperState>>;

#[derive(Clone)]
struct AppState {
    helper: HelperConfig,
    state: SharedState,
}

#[derive(Debug, Deserialize)]
struct RegisterPluginRequest {
    place_id: String,
    task_id: Option<String>,
    launch_id: Option<String>,
}

#[derive(Debug, Serialize)]
struct RegisterPluginResponse {
    instance_id: String,
    place_id: String,
    task_id: Option<String>,
    launch_id: Option<String>,
    remote_base_url: String,
}

#[derive(Debug, Deserialize)]
struct UnregisterPluginRequest {
    instance_id: String,
}

#[derive(Debug, Deserialize)]
struct PluginRequestQuery {
    instance_id: String,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
struct PluginResponsePayload {
    instance_id: Option<String>,
    id: String,
    success: bool,
    response: String,
}

#[derive(Debug, Serialize)]
struct HelperStatusResponse {
    helper_port: u16,
    helper_id: String,
    user_name: String,
    capacity: usize,
    hub_base_url: Option<String>,
    hub_last_error: Option<String>,
    hub_last_ready_age_ms: Option<u128>,
    active_instances: usize,
    claimed_task_count: usize,
    active_places: Vec<String>,
    claimed_tasks: Vec<ClaimedTaskStatus>,
    launched_studios: Vec<LaunchProcessStatus>,
    instances: Vec<HelperInstanceStatus>,
    place_statuses: Vec<HelperPlaceStatus>,
    last_remote_errors: HashMap<String, String>,
}

#[derive(Debug, Serialize)]
struct ClaimedTaskStatus {
    task_id: String,
    launch_id: String,
    generation: u32,
    place_id: String,
    game_id: Option<String>,
    mcp_base_url: String,
    rojo_base_url: Option<String>,
    runtime_log_base_url: Option<String>,
    studio_pid: Option<u32>,
    launched_by_helper: bool,
    claimed_age_ms: u128,
    remote_state: String,
    remote_connection_id: Option<String>,
    remote_last_error: Option<String>,
    remote_last_ready_age_ms: Option<u128>,
    remote_last_server_message_age_ms: Option<u128>,
}

#[derive(Debug, Serialize)]
struct LaunchProcessStatus {
    task_id: String,
    launch_id: String,
    generation: u32,
    place_id: String,
    game_id: Option<String>,
    studio_pid: u32,
    launched_by_helper: bool,
}

#[derive(Debug, Serialize)]
struct RegisterHelperHubRequest {
    helper_id: String,
    owner_user: String,
    platform: String,
    capacity: usize,
    labels: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct RegisterHelperHubResponse {
    helper_id: String,
    heartbeat_interval_sec: u64,
    heartbeat_timeout_sec: u64,
}

#[derive(Debug, Serialize)]
struct HelperHeartbeatHubRequest {
    helper_id: String,
    active_launches: Vec<HelperLaunchHubPayload>,
}

#[derive(Debug, Serialize, Deserialize)]
struct HelperLaunchHubPayload {
    launch_id: String,
    task_id: String,
    generation: u32,
}

#[derive(Debug, Deserialize)]
struct HelperHeartbeatHubResponse {
    ok: bool,
    #[serde(default)]
    release_launches: Vec<HelperLaunchHubPayload>,
}

#[derive(Debug, Serialize)]
struct ClaimTaskHubRequest {
    helper_id: String,
}

#[derive(Debug, Deserialize)]
struct ClaimTaskHubResponse {
    claimed: bool,
    helper_id: String,
    task: Option<ClaimedTaskHubPayload>,
}

#[derive(Debug, Deserialize)]
struct ClaimedTaskHubPayload {
    launch_id: String,
    task_id: String,
    generation: u32,
    place_id: String,
    game_id: Option<String>,
    routes: ClaimedTaskRoutes,
}

#[derive(Debug, Deserialize)]
struct ClaimedTaskRoutes {
    rojo_base_url: Option<String>,
    mcp_base_url: Option<String>,
    runtime_log_base_url: Option<String>,
}

#[derive(Debug, Serialize)]
struct HelperInstanceStatus {
    instance_id: String,
    place_id: String,
    task_id: Option<String>,
    generation: Option<u32>,
    launch_id: Option<String>,
    remote_base_url: String,
    studio_pid: Option<u32>,
}

#[derive(Debug, Serialize)]
struct HelperPlaceStatus {
    place_id: String,
    remote_base_url: String,
    task_id: Option<String>,
    generation: Option<u32>,
    launch_id: Option<String>,
    registered_instance_count: usize,
    registered_instance_ids: Vec<String>,
    studio_pids: Vec<u32>,
    remote_state: String,
    remote_connection_id: Option<String>,
    remote_last_error: Option<String>,
    remote_last_ready_age_ms: Option<u128>,
    remote_last_server_message_age_ms: Option<u128>,
}

#[cfg(target_os = "windows")]
#[derive(Clone)]
struct WindowCandidate {
    hwnd: HWND,
    process_id: u32,
    title: String,
    width: i32,
    height: i32,
}

#[cfg(target_os = "windows")]
struct TopWindowSearch {
    candidates: Vec<WindowCandidate>,
}

#[cfg(target_os = "windows")]
struct ChildWindowCandidate {
    hwnd: HWND,
    width: i32,
    height: i32,
}

#[cfg(target_os = "windows")]
struct ChildWindowSearch {
    min_width: i32,
    min_height: i32,
    candidates: Vec<ChildWindowCandidate>,
}

#[derive(Debug, Deserialize)]
struct PlaceQuery {
    #[serde(rename = "placeId")]
    place_id: String,
    #[serde(rename = "taskId")]
    task_id: Option<String>,
    #[serde(rename = "launchId")]
    launch_id: Option<String>,
}

#[derive(Debug, Serialize)]
struct RojoConfigResponse {
    place_id: String,
    task_id: Option<String>,
    launch_id: Option<String>,
    base_url: String,
    auth_header: String,
}

#[derive(Debug, Deserialize)]
struct ScreenshotQuery {
    #[serde(rename = "placeId")]
    place_id: Option<String>,
    #[serde(rename = "taskId")]
    task_id: Option<String>,
    generation: Option<u32>,
    #[serde(rename = "launchId")]
    launch_id: Option<String>,
}

#[derive(Debug, Serialize)]
struct ScreenshotResponse {
    path: String,
}

#[derive(Debug, Deserialize)]
struct RuntimeScreenshotRequest {
    #[serde(rename = "placeId", alias = "place_id")]
    place_id: Option<String>,
    #[serde(rename = "taskId", alias = "task_id")]
    task_id: Option<String>,
    generation: Option<u32>,
    #[serde(rename = "launchId", alias = "launch_id")]
    launch_id: Option<String>,
    session_id: String,
    runtime_id: String,
    tag: Option<String>,
    upload_url: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct RuntimeScreenshotSinkResponse {
    ok: bool,
    session_id: String,
    runtime_id: String,
    artifact_dir: String,
    screenshot_path: String,
    session_metadata_path: String,
    bytes_written: usize,
}

#[derive(Debug, Serialize)]
struct RuntimeScreenshotResponse {
    session_id: String,
    runtime_id: String,
    place_id: String,
    screenshot_path: String,
    screenshot_rel_path: Option<String>,
    artifact_dir: String,
    session_metadata_path: String,
    bytes_written: usize,
}

#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
struct CapturedScreenshot {
    resolved_place_id: Option<String>,
    png_bytes: Vec<u8>,
    window_title: String,
}

#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
#[derive(Debug, Deserialize, Clone)]
struct ReadStudioLogArgs {
    #[serde(default)]
    start_line: Option<i64>,
    #[serde(default)]
    line_count: Option<usize>,
    #[serde(default)]
    regex: Option<String>,
}

#[derive(Debug, Serialize)]
struct ReadStudioLogResponse {
    path: String,
    total_lines: usize,
    range: [usize; 2],
    lines: Vec<String>,
}

fn trim(value: &str) -> String {
    value.trim().to_owned()
}

fn normalize_bearer_token(value: &str) -> Option<String> {
    let trimmed = value.trim().trim_start_matches('\u{feff}').trim();
    if trimmed.is_empty() {
        return None;
    }

    let raw = trimmed.strip_prefix("Bearer ").unwrap_or(trimmed).trim();
    if raw.is_empty() {
        return None;
    }

    Some(raw.to_owned())
}

fn home_dir() -> Option<PathBuf> {
    env::var_os("HOME")
        .or_else(|| env::var_os("USERPROFILE"))
        .map(PathBuf::from)
}

fn windows_appdata_dir() -> Option<PathBuf> {
    env::var_os("APPDATA").map(PathBuf::from)
}

fn user_name_candidates() -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    if let Some(appdata) = windows_appdata_dir() {
        candidates.push(appdata.join("dev.clock-p.com").join("feishu-user_name"));
    }
    if let Some(home) = home_dir() {
        candidates.push(home.join(".dev.clock-p.com").join("feishu-user_name"));
    }
    candidates
}

fn token_candidates() -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    if let Some(appdata) = windows_appdata_dir() {
        candidates.push(appdata.join("dev.clock-p.com").join("feishu-token"));
    }
    if let Some(home) = home_dir() {
        candidates.push(home.join(".dev.clock-p.com").join("feishu-token"));
    }
    candidates
}

fn read_trimmed_file(path: &Path) -> Result<String> {
    Ok(trim(
        &fs::read_to_string(path)
            .wrap_err_with(|| format!("failed to read {}", path.display()))?
            .replace('\r', "")
            .replace('\n', ""),
    ))
}

fn resolve_user_name(args: &Args) -> Result<String> {
    if let Some(value) = args.user_name.as_ref() {
        let trimmed = trim(value).to_lowercase();
        if !trimmed.is_empty() {
            return Ok(trimmed);
        }
    }

    for candidate in user_name_candidates() {
        if candidate.is_file() {
            let value = read_trimmed_file(&candidate)?.to_lowercase();
            if !value.is_empty() {
                return Ok(value);
            }
        }
    }

    Err(eyre!("cannot resolve feishu-user_name for helper"))
}

fn resolve_bearer_token(args: &Args) -> Result<String> {
    Ok(resolve_bearer_token_candidates(args)?
        .into_iter()
        .next()
        .ok_or_else(|| eyre!("cannot resolve feishu-token for helper"))?
        .value)
}

fn resolve_bearer_token_candidates(args: &Args) -> Result<Vec<ResolvedToken>> {
    let mut resolved = Vec::new();

    if let Some(value) = args.bearer_token.as_ref() {
        if let Some(normalized) = normalize_bearer_token(value) {
            resolved.push(ResolvedToken {
                value: normalized,
                source: "cli --bearer-token".to_owned(),
            });
        }
    }

    if let Some(path) = args.bearer_token_file.as_ref() {
        if let Some(normalized) = normalize_bearer_token(&read_trimmed_file(path)?) {
            resolved.push(ResolvedToken {
                value: normalized,
                source: path.display().to_string(),
            });
        }
    }

    for candidate in token_candidates() {
        if candidate.is_file() {
            if let Some(normalized) = normalize_bearer_token(&read_trimmed_file(&candidate)?) {
                resolved.push(ResolvedToken {
                    value: normalized,
                    source: candidate.display().to_string(),
                });
            }
        }
    }

    let mut unique = Vec::new();
    let mut seen = HashSet::new();
    for item in resolved {
        if seen.insert(item.value.clone()) {
            unique.push(item);
        }
    }

    if unique.is_empty() {
        return Err(eyre!("cannot resolve feishu-token for helper"));
    }

    Ok(unique)
}

fn derive_public_base_url(kind: &str, place_id: &str, helper: &HelperConfig) -> String {
    format!(
        "https://{place_id}-{kind}-{}-user.{}",
        helper.user_name, helper.domain_suffix
    )
}

fn derive_runtime_screenshot_upload_url(place_id: &str, helper: &HelperConfig) -> String {
    format!(
        "{}/v1/runtime-screenshots",
        derive_public_base_url("runtime-log", place_id, helper)
    )
}

fn helper_data_dir() -> Result<PathBuf> {
    if let Some(appdata) = windows_appdata_dir() {
        return Ok(appdata.join("dev.clock-p.com").join("studio-helper"));
    }
    if let Some(home) = home_dir() {
        return Ok(home.join(".dev.clock-p.com").join("studio-helper"));
    }
    Err(eyre!("cannot resolve helper data directory"))
}

#[cfg(target_os = "windows")]
fn now_stamp() -> String {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    millis.to_string()
}

#[cfg(target_os = "windows")]
fn resolve_studio_path(helper: &HelperConfig) -> Result<PathBuf> {
    if let Some(path) = helper.studio_path.as_ref() {
        if path.is_file() {
            return Ok(path.clone());
        }
        return Err(eyre!(
            "configured studio_path does not exist: {}",
            path.display()
        ));
    }
    if let Some(path) = env::var_os("CLOCK_P_STUDIO_PATH").map(PathBuf::from) {
        if path.is_file() {
            return Ok(path);
        }
        return Err(eyre!(
            "CLOCK_P_STUDIO_PATH does not exist: {}",
            path.display()
        ));
    }
    let versions_root = env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .map(|path| path.join("Roblox").join("Versions"))
        .ok_or_else(|| eyre!("cannot resolve LOCALAPPDATA for Studio discovery"))?;
    let entries = fs::read_dir(&versions_root).wrap_err_with(|| {
        format!(
            "cannot read Roblox Studio versions directory: {}",
            versions_root.display()
        )
    })?;
    let mut candidates = Vec::new();
    for entry in entries {
        let entry = entry?;
        let candidate = entry.path().join("RobloxStudioBeta.exe");
        if !candidate.is_file() {
            continue;
        }
        let modified = candidate
            .metadata()
            .and_then(|metadata| metadata.modified())
            .unwrap_or(UNIX_EPOCH);
        candidates.push((modified, candidate));
    }
    candidates.sort_by_key(|(modified, _)| *modified);
    candidates.pop().map(|(_, path)| path).ok_or_else(|| {
        eyre!(
            "could not locate RobloxStudioBeta.exe under {}",
            versions_root.display()
        )
    })
}

#[cfg(target_os = "windows")]
fn launch_studio_for_claim(
    helper: &HelperConfig,
    task: &ClaimedTask,
) -> Result<LaunchProcessRecord> {
    let studio_path = resolve_studio_path(helper)?;
    let mut command = std::process::Command::new(&studio_path);
    command.arg("-task").arg("EditPlace");
    command.arg("-placeId").arg(&task.place_id);
    if let Some(game_id) = task.game_id.as_deref() {
        command.arg("-universeId").arg(game_id);
    }
    if let Some(parent) = studio_path.parent() {
        command.current_dir(parent);
    }
    let child = command
        .spawn()
        .wrap_err_with(|| format!("failed to launch Studio at {}", studio_path.display()))?;
    let studio_pid = child.id();
    tracing::info!(
        task_id = task.task_id.as_str(),
        generation = task.generation,
        launch_id = task.launch_id.as_str(),
        place_id = task.place_id.as_str(),
        game_id = task.game_id.as_deref(),
        studio_pid,
        studio_path = %studio_path.display(),
        "launched Roblox Studio for claimed task"
    );
    Ok(LaunchProcessRecord {
        task_id: task.task_id.clone(),
        launch_id: task.launch_id.clone(),
        generation: task.generation,
        place_id: task.place_id.clone(),
        game_id: task.game_id.clone(),
        studio_pid,
        launched_by_helper: true,
        child: Some(child),
    })
}

fn sanitize_place_id(value: &str) -> Result<String> {
    let trimmed = trim(value);
    if trimmed.is_empty() || !trimmed.chars().all(|ch| ch.is_ascii_digit()) {
        return Err(eyre!("placeId must be digits only"));
    }
    Ok(trimmed)
}

fn sanitize_identifier(label: &str, value: &str) -> Result<String> {
    let trimmed = trim(value);
    if trimmed.is_empty()
        || !trimmed
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '-')
    {
        return Err(eyre!("{label} must use [A-Za-z0-9_-] only"));
    }
    Ok(trimmed)
}

fn maybe_sanitize_identifier(label: &str, value: Option<&str>) -> Result<Option<String>> {
    match value {
        Some(raw) => Ok(Some(sanitize_identifier(label, raw)?)),
        None => Ok(None),
    }
}

fn select_claimed_task(
    state: &HelperState,
    place_id: &str,
    task_id: Option<&str>,
    generation: Option<u32>,
    launch_id: Option<&str>,
) -> Result<ClaimedTask> {
    if let Some(task_id) = task_id {
        let task = state
            .claimed_tasks
            .get(task_id)
            .ok_or_else(|| eyre!("helper has no claimed task for task_id {task_id}"))?;
        if task.place_id != place_id {
            return Err(eyre!(
                "task_id {task_id} does not match place_id {place_id}"
            ));
        }
        if let Some(expected_generation) = generation {
            if task.generation != expected_generation {
                return Err(eyre!(
                    "task_id {task_id} generation mismatch: expected {expected_generation}, got {}",
                    task.generation
                ));
            }
        }
        if let Some(expected_launch_id) = launch_id {
            if task.launch_id != expected_launch_id {
                return Err(eyre!(
                    "task_id {task_id} launch_id mismatch: expected {expected_launch_id}, got {}",
                    task.launch_id
                ));
            }
        }
        return Ok(task.clone());
    }

    let mut matches = state
        .claimed_tasks
        .values()
        .filter(|task| task.place_id == place_id)
        .cloned()
        .collect::<Vec<_>>();
    if let Some(expected_generation) = generation {
        matches.retain(|task| task.generation == expected_generation);
    }
    if let Some(expected_launch_id) = launch_id {
        matches.retain(|task| task.launch_id == expected_launch_id);
    }
    match matches.len() {
        0 => Err(eyre!(
            "helper has no claimed task for place_id {place_id}; wait for hub claim first"
        )),
        1 => Ok(matches.remove(0)),
        _ => Err(eyre!(
            "multiple claimed tasks found for place_id {place_id}; task_id is required"
        )),
    }
}

fn route_identity_matches(
    instance: &PluginInstance,
    task_id: Option<&str>,
    generation: Option<u32>,
    launch_id: Option<&str>,
) -> bool {
    if let Some(expected_task_id) = task_id {
        if instance.task_id.as_deref() != Some(expected_task_id) {
            return false;
        }
    }
    if let Some(expected_generation) = generation {
        if instance.generation != Some(expected_generation) {
            return false;
        }
    }
    if let Some(expected_launch_id) = launch_id {
        if instance.launch_id.as_deref() != Some(expected_launch_id) {
            return false;
        }
    }
    true
}

fn select_claimed_task_for_pid(
    state: &HelperState,
    studio_pid: u32,
    place_id: &str,
) -> Option<ClaimedTask> {
    let task_id = state
        .launch_processes
        .values()
        .find(|launch| launch.studio_pid == studio_pid)
        .map(|launch| launch.task_id.clone())?;
    let task = state.claimed_tasks.get(&task_id)?;
    if task.place_id != place_id {
        return None;
    }
    Some(task.clone())
}

#[cfg(test)]
fn resolve_claimed_task_for_request(
    state: &HelperState,
    place_id: &str,
    task_id: Option<&str>,
    generation: Option<u32>,
    launch_id: Option<&str>,
    studio_pid: Option<u32>,
) -> Result<ClaimedTask> {
    if let Some(studio_pid) = studio_pid {
        if let Some(task) = select_claimed_task_for_pid(state, studio_pid, place_id) {
            return Ok(task);
        }
    }
    if task_id.is_none() && generation.is_none() && launch_id.is_none() {
        return Err(eyre!(
            "helper could not map Studio for place_id {place_id} to a claimed launch; task_id, generation, and launch_id are required unless the Studio pid was launched by helper"
        ));
    }
    select_claimed_task(state, place_id, task_id, generation, launch_id)
}

fn resolve_claimed_task_for_plugin_request(
    state: &HelperState,
    place_id: &str,
    task_id: Option<&str>,
    launch_id: Option<&str>,
    studio_pid: Option<u32>,
) -> Result<ClaimedTask> {
    if let Some(studio_pid) = studio_pid {
        if let Some(task) = select_claimed_task_for_pid(state, studio_pid, place_id) {
            return Ok(task);
        }
    }
    if task_id.is_none() && launch_id.is_none() {
        return Err(eyre!(
            "helper could not map Studio for place_id {place_id} to a claimed launch; task_id or launch_id is required unless the Studio pid was launched by helper"
        ));
    }
    select_claimed_task(state, place_id, task_id, None, launch_id)
}

fn bind_launch_process_to_pid(
    state: &mut HelperState,
    task: &ClaimedTask,
    studio_pid: u32,
) -> Result<()> {
    if let Some(existing) = state.launch_processes.get_mut(&task.task_id) {
        if existing.generation != task.generation || existing.launch_id != task.launch_id {
            return Err(eyre!(
                "launch process identity mismatch for task {}: expected gen {} launch {}, got gen {} launch {}",
                task.task_id,
                task.generation,
                task.launch_id,
                existing.generation,
                existing.launch_id
            ));
        }
        if existing.studio_pid != studio_pid {
            if existing.launched_by_helper {
                return Err(eyre!(
                    "helper-launched Studio pid mismatch for task {}: expected {}, got {}",
                    task.task_id,
                    existing.studio_pid,
                    studio_pid
                ));
            }
            existing.studio_pid = studio_pid;
        }
        return Ok(());
    }
    state.launch_processes.insert(
        task.task_id.clone(),
        LaunchProcessRecord {
            task_id: task.task_id.clone(),
            launch_id: task.launch_id.clone(),
            generation: task.generation,
            place_id: task.place_id.clone(),
            game_id: task.game_id.clone(),
            studio_pid,
            launched_by_helper: false,
            child: None,
        },
    );
    Ok(())
}

fn remove_instances_for_claimed_task(
    state: &mut HelperState,
    task_id: &str,
    generation: u32,
    launch_id: &str,
) {
    state.instances.retain(|instance_id, instance| {
        let keep =
            !route_identity_matches(instance, Some(task_id), Some(generation), Some(launch_id));
        if !keep {
            tracing::info!(
                instance_id,
                task_id,
                generation,
                launch_id,
                "dropping plugin instance for released claimed task"
            );
        }
        keep
    });
}

fn release_claimed_task(
    state: &mut HelperState,
    task_id: &str,
    generation: u32,
    launch_id: &str,
    terminate_helper_spawned: bool,
    reason: &str,
) -> Option<PendingLaunchTermination> {
    let should_remove = state
        .claimed_tasks
        .get(task_id)
        .map(|task| task.generation == generation && task.launch_id == launch_id)
        .unwrap_or(false);
    if !should_remove {
        return None;
    }

    let remove_launch_process = state
        .launch_processes
        .get(task_id)
        .map(|launch| launch.generation == generation && launch.launch_id == launch_id)
        .unwrap_or(false);
    let pending_termination = if remove_launch_process {
        state
            .launch_processes
            .remove(task_id)
            .and_then(|mut launch| {
                if terminate_helper_spawned && launch.launched_by_helper {
                    launch.child.take().map(|child| PendingLaunchTermination {
                        task_id: launch.task_id,
                        launch_id: launch.launch_id,
                        generation: launch.generation,
                        studio_pid: launch.studio_pid,
                        child,
                    })
                } else {
                    None
                }
            })
    } else {
        None
    };

    remove_instances_for_claimed_task(state, task_id, generation, launch_id);
    state.claimed_tasks.remove(task_id);
    state
        .last_remote_errors
        .remove(&task_connection_key(task_id, generation, launch_id));
    tracing::info!(
        task_id,
        generation,
        launch_id,
        reason,
        "released claimed task from helper state"
    );
    pending_termination
}

async fn terminate_pending_launch_termination(pending: PendingLaunchTermination) {
    let PendingLaunchTermination {
        task_id,
        launch_id,
        generation,
        studio_pid,
        mut child,
    } = pending;
    let task_id_for_join = task_id.clone();
    let launch_id_for_join = launch_id.clone();
    match tokio::task::spawn_blocking(move || {
        if let Err(error) = child.kill() {
            tracing::warn!(
                task_id,
                generation,
                launch_id,
                studio_pid,
                error = summarize_error(&error.to_string()),
                "failed to terminate helper-launched Studio during claim release"
            );
            return;
        }
        tracing::info!(
            task_id,
            generation,
            launch_id,
            studio_pid,
            "terminated helper-launched Studio during claim release"
        );
        if let Err(error) = child.wait() {
            tracing::warn!(
                task_id,
                generation,
                launch_id,
                studio_pid,
                error = summarize_error(&error.to_string()),
                "failed to reap helper-launched Studio after claim release"
            );
        }
    })
    .await
    {
        Ok(()) => {}
        Err(error) => {
            tracing::warn!(
                task_id = task_id_for_join,
                generation,
                launch_id = launch_id_for_join,
                studio_pid,
                error = summarize_error(&error.to_string()),
                "helper-launched Studio termination task panicked"
            );
        }
    }
}

fn reconcile_launch_processes(state: &mut HelperState) {
    let mut exited = Vec::new();
    for (task_id, launch) in state.launch_processes.iter_mut() {
        let Some(child) = launch.child.as_mut() else {
            continue;
        };
        match child.try_wait() {
            Ok(Some(status)) => exited.push((
                task_id.clone(),
                launch.generation,
                launch.launch_id.clone(),
                launch.studio_pid,
                status.to_string(),
            )),
            Ok(None) => {}
            Err(error) => {
                tracing::warn!(
                    task_id,
                    generation = launch.generation,
                    launch_id = launch.launch_id,
                    studio_pid = launch.studio_pid,
                    error = summarize_error(&error.to_string()),
                    "failed to poll helper-launched Studio process"
                );
            }
        }
    }

    for (task_id, generation, launch_id, studio_pid, status) in exited {
        tracing::warn!(
            task_id,
            generation,
            launch_id,
            studio_pid,
            exit_status = status,
            "helper-launched Studio exited"
        );
        let _ = release_claimed_task(
            state,
            &task_id,
            generation,
            &launch_id,
            false,
            "helper-launched Studio exited",
        );
    }
}

fn resolve_helper_id(args: &Args) -> Result<String> {
    if let Some(value) = args.helper_id.as_deref() {
        return sanitize_identifier("helper_id", value);
    }
    let data_dir = helper_data_dir()?;
    fs::create_dir_all(&data_dir)?;
    let path = data_dir.join("helper-id");
    if path.exists() {
        let existing = trim(&fs::read_to_string(&path)?);
        return sanitize_identifier("helper_id", &existing);
    }
    let generated = format!("h_{}", &Uuid::new_v4().simple().to_string()[..10]);
    fs::write(&path, format!("{generated}\n"))?;
    Ok(generated)
}

async fn hub_post_json<TRequest: Serialize, TResponse: DeserializeOwned>(
    helper: &HelperConfig,
    path: &str,
    payload: &TRequest,
) -> Result<TResponse> {
    let Some(base_url) = helper.hub_base_url.as_ref() else {
        return Err(eyre!("hub_base_url is not configured"));
    };
    let token = helper.bearer_token.lock().await.clone();
    let response = helper
        .client
        .post(format!("{}{path}", base_url.trim_end_matches('/')))
        .bearer_auth(token)
        .json(payload)
        .send()
        .await?;
    let status = response.status();
    let body = response.text().await?;
    if !status.is_success() {
        return Err(eyre!("hub request {path} failed with {status}: {body}"));
    }
    Ok(serde_json::from_str(&body)?)
}

async fn hub_register_helper(helper: &HelperConfig) -> Result<RegisterHelperHubResponse> {
    hub_post_json(
        helper,
        "/v1/helpers/register",
        &RegisterHelperHubRequest {
            helper_id: helper.helper_id.clone(),
            owner_user: helper.user_name.clone(),
            platform: std::env::consts::OS.to_owned(),
            capacity: helper.capacity,
            labels: vec![std::env::consts::ARCH.to_owned()],
        },
    )
    .await
}

async fn hub_helper_heartbeat(
    helper: &HelperConfig,
    active_launches: Vec<HelperLaunchHubPayload>,
) -> Result<HelperHeartbeatHubResponse> {
    hub_post_json(
        helper,
        "/v1/helpers/heartbeat",
        &HelperHeartbeatHubRequest {
            helper_id: helper.helper_id.clone(),
            active_launches,
        },
    )
    .await
}

async fn hub_claim_task(helper: &HelperConfig) -> Result<ClaimTaskHubResponse> {
    hub_post_json(
        helper,
        "/v1/helpers/claim",
        &ClaimTaskHubRequest {
            helper_id: helper.helper_id.clone(),
        },
    )
    .await
}

fn validate_runtime_screenshot_upload_url(
    upload_url: &str,
    place_id: &str,
    task_id: Option<&str>,
    generation: Option<u32>,
    state: &HelperState,
    helper: &HelperConfig,
) -> Result<String> {
    let trimmed = trim(upload_url);
    if trimmed.is_empty() {
        return Err(eyre!("upload_url must not be empty"));
    }

    let expected = match select_claimed_task(state, place_id, task_id, generation, None) {
        Ok(task) => task
            .runtime_log_base_url
            .as_deref()
            .map(runtime_screenshot_upload_url_for_base_url)
            .ok_or_else(|| eyre!("claimed task has no runtime_log_base_url"))?,
        Err(_) => derive_runtime_screenshot_upload_url(place_id, helper),
    };
    if trimmed != expected {
        return Err(eyre!(
            "upload_url must match runtime screenshot sink for placeId {place_id}: {expected}"
        ));
    }

    Ok(expected)
}

async fn post_runtime_screenshot(
    helper: &HelperConfig,
    upload_url: &str,
    session_id: &str,
    runtime_id: &str,
    place_id: &str,
    tag: Option<&str>,
    png_bytes: &[u8],
) -> Result<RuntimeScreenshotSinkResponse> {
    let current_token = helper.bearer_token.lock().await.clone();
    let current_source = helper.bearer_token_source.lock().await.clone();
    let mut attempts = vec![ResolvedToken {
        value: current_token.clone(),
        source: current_source,
    }];
    for candidate in helper.bearer_token_candidates.iter() {
        if candidate.value != current_token {
            attempts.push(candidate.clone());
        }
    }

    let normalized_tag = tag.map(trim).filter(|value| !value.is_empty());
    let mut saw_unauthorized = false;
    for candidate in attempts {
        let mut request = helper
            .client
            .post(upload_url)
            .header(AUTHORIZATION, format!("Bearer {}", candidate.value))
            .header(CONTENT_TYPE, "image/png")
            .header("X-Session-Id", session_id)
            .header("X-Runtime-Id", runtime_id)
            .header("X-Place-Id", place_id);
        if let Some(tag) = normalized_tag.as_deref() {
            request = request.header("X-Screenshot-Tag", tag);
        }

        let response = request.body(png_bytes.to_vec()).send().await?;
        let status = response.status();
        let response_body = response.text().await?;
        if status == StatusCode::UNAUTHORIZED {
            saw_unauthorized = true;
            tracing::warn!(
                source = candidate.source,
                "runtime screenshot upload was unauthorized; trying next token candidate"
            );
            continue;
        }
        if !status.is_success() {
            return Err(eyre!(
                "runtime screenshot upload returned {status}: {response_body}"
            ));
        }

        let sink_response: RuntimeScreenshotSinkResponse = serde_json::from_str(&response_body)
            .wrap_err("runtime screenshot upload returned invalid JSON")?;
        if !sink_response.ok {
            return Err(eyre!(
                "runtime screenshot upload returned ok=false: {response_body}"
            ));
        }
        if sink_response.session_id != session_id {
            return Err(eyre!(
                "runtime screenshot upload returned mismatched session_id: expected {session_id}, got {}",
                sink_response.session_id
            ));
        }
        if sink_response.runtime_id != runtime_id {
            return Err(eyre!(
                "runtime screenshot upload returned mismatched runtime_id: expected {runtime_id}, got {}",
                sink_response.runtime_id
            ));
        }

        if candidate.value != current_token {
            *helper.bearer_token.lock().await = candidate.value.clone();
            *helper.bearer_token_source.lock().await = candidate.source.clone();
            tracing::warn!(
                source = candidate.source,
                "helper switched bearer token after runtime screenshot upload unauthorized"
            );
        }
        return Ok(sink_response);
    }

    if saw_unauthorized {
        return Err(eyre!(
            "runtime screenshot upload returned unauthorized for all token candidates"
        ));
    }
    Err(eyre!("runtime screenshot upload could not complete"))
}

fn extract_command_id(command: &Value) -> Result<String> {
    command
        .get("id")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
        .ok_or_else(|| eyre!("command payload missing string id"))
}

fn extract_tool_name_and_args(command: &Value) -> Result<(String, Value)> {
    let args = command
        .get("args")
        .and_then(Value::as_object)
        .ok_or_else(|| eyre!("command payload missing args object"))?;
    let (name, payload) = args
        .iter()
        .next()
        .ok_or_else(|| eyre!("command args object was empty"))?;
    Ok((name.clone(), payload.clone()))
}

fn summarize_error(value: &str) -> String {
    const LIMIT: usize = 180;
    let trimmed = value.trim();
    if trimmed.len() <= LIMIT {
        trimmed.to_owned()
    } else {
        format!("{}...", &trimmed[..LIMIT])
    }
}

#[cfg(target_os = "windows")]
fn decode_ipv4_addr(value: u32) -> Ipv4Addr {
    Ipv4Addr::from(u32::from_be(value))
}

#[cfg(target_os = "windows")]
fn decode_ipv4_port(value: u32) -> u16 {
    u16::from_be(value as u16)
}

#[cfg(target_os = "windows")]
unsafe extern "system" fn enum_top_windows_callback(hwnd: HWND, lparam: LPARAM) -> i32 {
    let search = &mut *(lparam as *mut TopWindowSearch);
    if IsWindowVisible(hwnd) == 0 {
        return 1;
    }

    let mut window_pid = 0u32;
    GetWindowThreadProcessId(hwnd, &mut window_pid);

    let mut rect = RECT::default();
    let used_window_rect = if GetWindowRect(hwnd, &mut rect) != 0 {
        true
    } else {
        GetClientRect(hwnd, &mut rect) != 0
    };
    if !used_window_rect {
        rect = RECT::default();
    }
    let width = rect.right - rect.left;
    let height = rect.bottom - rect.top;

    let title_length = GetWindowTextLengthW(hwnd);
    let mut title_buffer = vec![0u16; title_length as usize + 1];
    let copied = GetWindowTextW(hwnd, title_buffer.as_mut_ptr(), title_buffer.len() as i32);
    let title = String::from_utf16_lossy(&title_buffer[..copied.max(0) as usize]);

    search.candidates.push(WindowCandidate {
        hwnd,
        process_id: window_pid,
        title,
        width: width.max(0),
        height: height.max(0),
    });
    1
}

#[cfg(target_os = "windows")]
unsafe extern "system" fn enum_child_windows_callback(hwnd: HWND, lparam: LPARAM) -> i32 {
    let search = &mut *(lparam as *mut ChildWindowSearch);
    if IsWindowVisible(hwnd) == 0 {
        return 1;
    }

    let mut rect = RECT::default();
    if GetClientRect(hwnd, &mut rect) == 0 {
        return 1;
    }
    let width = rect.right - rect.left;
    let height = rect.bottom - rect.top;
    if width < search.min_width || height < search.min_height {
        return 1;
    }

    search.candidates.push(ChildWindowCandidate {
        hwnd,
        width,
        height,
    });
    1
}

#[cfg(target_os = "windows")]
fn resolve_peer_process_id(peer_addr: SocketAddr, helper_port: u16) -> Result<Option<u32>> {
    let SocketAddr::V4(peer_v4) = peer_addr else {
        return Ok(None);
    };

    let mut buffer_size = 0u32;
    unsafe {
        GetExtendedTcpTable(
            null_mut(),
            &mut buffer_size,
            0,
            AF_INET as u32,
            TCP_TABLE_OWNER_PID_ALL,
            0,
        );
    }
    if buffer_size == 0 {
        return Err(eyre!("GetExtendedTcpTable did not report a buffer size"));
    }

    let mut buffer = vec![0u8; buffer_size as usize + size_of::<MIB_TCPROW_OWNER_PID>()];
    let result = unsafe {
        GetExtendedTcpTable(
            buffer.as_mut_ptr().cast(),
            &mut buffer_size,
            0,
            AF_INET as u32,
            TCP_TABLE_OWNER_PID_ALL,
            0,
        )
    };
    if result != 0 {
        return Err(eyre!("GetExtendedTcpTable failed with code {result}"));
    }

    let table = unsafe { &*(buffer.as_ptr().cast::<MIB_TCPTABLE_OWNER_PID>()) };
    let rows =
        unsafe { std::slice::from_raw_parts(table.table.as_ptr(), table.dwNumEntries as usize) };
    let peer_ip = *peer_v4.ip();
    let peer_port = peer_v4.port();
    let helper_ip = Ipv4Addr::LOCALHOST;
    for row in rows {
        let local_ip = decode_ipv4_addr(row.dwLocalAddr);
        let remote_ip = decode_ipv4_addr(row.dwRemoteAddr);
        let local_port = decode_ipv4_port(row.dwLocalPort);
        let remote_port = decode_ipv4_port(row.dwRemotePort);
        if local_ip == peer_ip
            && local_port == peer_port
            && remote_ip == helper_ip
            && remote_port == helper_port
        {
            return Ok(Some(row.dwOwningPid));
        }
    }

    Ok(None)
}

#[cfg(not(target_os = "windows"))]
fn resolve_peer_process_id(_peer_addr: SocketAddr, _helper_port: u16) -> Result<Option<u32>> {
    Ok(None)
}

async fn resolve_peer_process_id_with_retry(
    peer_addr: SocketAddr,
    helper_port: u16,
) -> Result<Option<u32>> {
    for attempt in 0..10 {
        let peer_addr_for_attempt = peer_addr;
        let pid = tokio::task::spawn_blocking(move || {
            resolve_peer_process_id(peer_addr_for_attempt, helper_port)
        })
        .await
        .map_err(|error| eyre!("peer pid lookup task failed: {error}"))??;
        if let Some(pid) = pid {
            return Ok(Some(pid));
        }
        if attempt < 9 {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
    Ok(None)
}

#[cfg(target_os = "windows")]
fn collect_visible_child_windows(
    parent_hwnd: HWND,
    min_width: i32,
    min_height: i32,
) -> Vec<ChildWindowCandidate> {
    let mut search = ChildWindowSearch {
        min_width,
        min_height,
        candidates: Vec::new(),
    };
    unsafe {
        EnumChildWindows(
            parent_hwnd,
            Some(enum_child_windows_callback),
            (&mut search as *mut ChildWindowSearch) as LPARAM,
        );
    }
    search.candidates
}

#[cfg(target_os = "windows")]
fn find_studio_capture_target_for_pid(studio_pid: u32) -> Result<(HWND, HWND, String)> {
    let mut search = TopWindowSearch {
        candidates: Vec::new(),
    };
    unsafe {
        EnumWindows(
            Some(enum_top_windows_callback),
            (&mut search as *mut TopWindowSearch) as LPARAM,
        );
    }
    let all_candidates = search.candidates;
    let exact_candidates: Vec<_> = all_candidates
        .iter()
        .filter(|candidate| candidate.process_id == studio_pid)
        .cloned()
        .collect();
    let studio_window = exact_candidates
        .into_iter()
        .max_by_key(|candidate| {
            let title_score = if candidate.title.contains("Roblox Studio") { 1 } else { 0 };
            (title_score, i64::from(candidate.width) * i64::from(candidate.height))
        })
        .or_else(|| {
            let studio_named: Vec<_> = all_candidates
                .iter()
                .filter(|candidate| candidate.title.contains("Roblox Studio"))
                .cloned()
                .collect();
            if studio_named.len() == 1 {
                let candidate = studio_named.into_iter().next().unwrap();
                tracing::warn!(
                    requested_pid = studio_pid,
                    fallback_pid = candidate.process_id,
                    title = candidate.title,
                    "did not find an exact Studio window pid match; falling back to the only visible Roblox Studio window"
                );
                Some(candidate)
            } else {
                None
            }
        })
        .ok_or_else(|| {
            let visible_studio_windows: Vec<_> = all_candidates
                .iter()
                .filter(|candidate| candidate.title.contains("Roblox Studio"))
                .map(|candidate| format!("pid={} title={}", candidate.process_id, candidate.title))
                .collect();
            if visible_studio_windows.is_empty() {
                eyre!("could not find any visible Roblox Studio window while resolving pid {studio_pid}")
            } else {
                eyre!(
                    "could not find an exact visible Roblox Studio window for pid {studio_pid}; visible Studio windows: {}",
                    visible_studio_windows.join(" | ")
                )
            }
        })?;

    let descendants = collect_visible_child_windows(studio_window.hwnd, 500, 400);
    let content_area = descendants
        .iter()
        .find(|candidate| {
            candidate.height > 600 && candidate.height < 750 && candidate.width > 2000
        })
        .map(|candidate| candidate.hwnd)
        .or_else(|| {
            descendants
                .iter()
                .filter(|candidate| candidate.width > candidate.height)
                .max_by_key(|candidate| i64::from(candidate.width) * i64::from(candidate.height))
                .map(|candidate| candidate.hwnd)
        })
        .unwrap_or(studio_window.hwnd);

    let viewport_candidates = collect_visible_child_windows(content_area, 500, 400);
    let viewport = viewport_candidates
        .iter()
        .find(|candidate| {
            candidate.width < 1660
                && candidate.height < 680
                && candidate.width > 1000
                && candidate.height > 500
        })
        .map(|candidate| candidate.hwnd)
        .or_else(|| {
            viewport_candidates
                .iter()
                .find(|candidate| candidate.width > 1000 && candidate.height > 500)
                .map(|candidate| candidate.hwnd)
        })
        .unwrap_or(studio_window.hwnd);

    Ok((studio_window.hwnd, viewport, studio_window.title))
}

async fn helper_status(State(app): State<AppState>) -> Json<HelperStatusResponse> {
    let mut state = app.state.lock().await;
    cleanup_stale_instances(&mut state.instances);
    reconcile_launch_processes(&mut state);
    sync_remote_connections(&app, &mut state);
    let mut active_places: Vec<_> = state
        .instances
        .values()
        .map(|instance| instance.place_id.clone())
        .chain(
            state
                .claimed_tasks
                .values()
                .map(|task| task.place_id.clone()),
        )
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    active_places.sort();
    let mut instances: Vec<_> = state
        .instances
        .iter()
        .map(|(instance_id, instance)| HelperInstanceStatus {
            instance_id: instance_id.clone(),
            place_id: instance.place_id.clone(),
            task_id: instance.task_id.clone(),
            generation: instance.generation,
            launch_id: instance.launch_id.clone(),
            remote_base_url: instance.remote_base_url.clone(),
            studio_pid: instance.studio_pid,
        })
        .collect();
    instances.sort_by(|left, right| left.instance_id.cmp(&right.instance_id));
    let mut claimed_tasks: Vec<_> = state
        .claimed_tasks
        .values()
        .map(|task| {
            let connection_key =
                task_connection_key(&task.task_id, task.generation, &task.launch_id);
            let remote_connection = state.remote_connections.get(&connection_key);
            let launch_process = state.launch_processes.get(&task.task_id).filter(|launch| {
                launch.generation == task.generation && launch.launch_id == task.launch_id
            });
            let studio_pid = launch_process.map(|launch| launch.studio_pid).or_else(|| {
                state
                    .instances
                    .values()
                    .find(|instance| {
                        route_identity_matches(
                            instance,
                            Some(task.task_id.as_str()),
                            Some(task.generation),
                            Some(task.launch_id.as_str()),
                        )
                    })
                    .and_then(|instance| instance.studio_pid)
            });
            ClaimedTaskStatus {
                task_id: task.task_id.clone(),
                launch_id: task.launch_id.clone(),
                generation: task.generation,
                place_id: task.place_id.clone(),
                game_id: task.game_id.clone(),
                mcp_base_url: task.mcp_base_url.clone(),
                rojo_base_url: task.rojo_base_url.clone(),
                runtime_log_base_url: task.runtime_log_base_url.clone(),
                studio_pid,
                launched_by_helper: launch_process
                    .map(|launch| launch.launched_by_helper)
                    .unwrap_or(false),
                claimed_age_ms: task.claimed_at.elapsed().as_millis(),
                remote_state: remote_connection
                    .map(|connection| match connection.connection_state {
                        RemoteConnectionState::Connecting => "connecting",
                        RemoteConnectionState::Connected => "connected",
                        RemoteConnectionState::Retrying => "retrying",
                    })
                    .unwrap_or("disconnected")
                    .to_owned(),
                remote_connection_id: remote_connection
                    .and_then(|connection| connection.connection_id.clone()),
                remote_last_error: state.last_remote_errors.get(&connection_key).cloned(),
                remote_last_ready_age_ms: remote_connection.and_then(|connection| {
                    connection
                        .last_ready_at
                        .map(|value| value.elapsed().as_millis())
                }),
                remote_last_server_message_age_ms: remote_connection.and_then(|connection| {
                    connection
                        .last_server_message_at
                        .map(|value| value.elapsed().as_millis())
                }),
            }
        })
        .collect();
    claimed_tasks.sort_by(|left, right| left.task_id.cmp(&right.task_id));
    let mut launched_studios: Vec<_> = state
        .launch_processes
        .values()
        .map(|launch| LaunchProcessStatus {
            task_id: launch.task_id.clone(),
            launch_id: launch.launch_id.clone(),
            generation: launch.generation,
            place_id: launch.place_id.clone(),
            game_id: launch.game_id.clone(),
            studio_pid: launch.studio_pid,
            launched_by_helper: launch.launched_by_helper,
        })
        .collect();
    launched_studios.sort_by(|left, right| left.task_id.cmp(&right.task_id));
    let mut place_ids: Vec<_> = state
        .instances
        .values()
        .map(|instance| instance.place_id.clone())
        .chain(
            state
                .claimed_tasks
                .values()
                .map(|task| task.place_id.clone()),
        )
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    place_ids.sort();
    let mut place_statuses = Vec::with_capacity(place_ids.len());
    for place_id in place_ids {
        let claimed_tasks_for_place = state
            .claimed_tasks
            .values()
            .filter(|task| task.place_id == place_id)
            .cloned()
            .collect::<Vec<_>>();
        let claimed_task = if claimed_tasks_for_place.len() == 1 {
            claimed_tasks_for_place.into_iter().next()
        } else {
            None
        };
        let mut registered_instance_ids = Vec::new();
        let mut studio_pids = Vec::new();
        for (instance_id, instance) in &state.instances {
            if instance.place_id == place_id {
                registered_instance_ids.push(instance_id.clone());
                if let Some(studio_pid) = instance.studio_pid {
                    studio_pids.push(studio_pid);
                }
            }
        }
        if let Some(claimed_task) = claimed_task.as_ref() {
            if let Some(launch) =
                state
                    .launch_processes
                    .get(&claimed_task.task_id)
                    .filter(|launch| {
                        launch.generation == claimed_task.generation
                            && launch.launch_id == claimed_task.launch_id
                    })
            {
                studio_pids.push(launch.studio_pid);
            }
        }
        registered_instance_ids.sort();
        studio_pids.sort();
        studio_pids.dedup();
        let connection_key = claimed_task
            .as_ref()
            .map(|task| task_connection_key(&task.task_id, task.generation, &task.launch_id))
            .unwrap_or_else(|| place_connection_key(&place_id));
        let remote_connection = state.remote_connections.get(&connection_key);
        place_statuses.push(HelperPlaceStatus {
            remote_base_url: claimed_task
                .as_ref()
                .map(|task| task.mcp_base_url.clone())
                .unwrap_or_else(|| derive_public_base_url("mcp", &place_id, &app.helper)),
            place_id: place_id.clone(),
            task_id: claimed_task.as_ref().map(|task| task.task_id.clone()),
            generation: claimed_task.as_ref().map(|task| task.generation),
            launch_id: claimed_task.as_ref().map(|task| task.launch_id.clone()),
            registered_instance_count: registered_instance_ids.len(),
            registered_instance_ids,
            studio_pids,
            remote_state: remote_connection
                .map(|connection| match connection.connection_state {
                    RemoteConnectionState::Connecting => "connecting",
                    RemoteConnectionState::Connected => "connected",
                    RemoteConnectionState::Retrying => "retrying",
                })
                .unwrap_or("disconnected")
                .to_owned(),
            remote_connection_id: remote_connection
                .and_then(|connection| connection.connection_id.clone()),
            remote_last_error: state.last_remote_errors.get(&connection_key).cloned(),
            remote_last_ready_age_ms: remote_connection.and_then(|connection| {
                connection
                    .last_ready_at
                    .map(|value| value.elapsed().as_millis())
            }),
            remote_last_server_message_age_ms: remote_connection.and_then(|connection| {
                connection
                    .last_server_message_at
                    .map(|value| value.elapsed().as_millis())
            }),
        });
    }
    Json(HelperStatusResponse {
        helper_port: app.helper.port,
        helper_id: app.helper.helper_id.clone(),
        user_name: app.helper.user_name.clone(),
        capacity: app.helper.capacity,
        hub_base_url: app.helper.hub_base_url.clone(),
        hub_last_error: state.hub_last_error.clone(),
        hub_last_ready_age_ms: state
            .hub_last_ready_at
            .map(|value| value.elapsed().as_millis()),
        active_instances: state.instances.len(),
        claimed_task_count: claimed_tasks.len(),
        active_places,
        claimed_tasks,
        launched_studios,
        instances,
        place_statuses,
        last_remote_errors: state.last_remote_errors.clone(),
    })
}

async fn rojo_config_handler(
    State(app): State<AppState>,
    ConnectInfo(peer_addr): ConnectInfo<SocketAddr>,
    Query(query): Query<PlaceQuery>,
) -> Result<Json<RojoConfigResponse>, HelperError> {
    let place_id = sanitize_place_id(&query.place_id)?;
    let explicit_task_id = maybe_sanitize_identifier("task_id", query.task_id.as_deref())?;
    let explicit_launch_id = maybe_sanitize_identifier("launch_id", query.launch_id.as_deref())?;
    let bearer_token = app.helper.bearer_token.lock().await.clone();
    if app.helper.hub_base_url.is_none() {
        let base_url = derive_public_base_url("rojo", &place_id, &app.helper);
        tracing::info!(
            place_id,
            base_url,
            "resolved legacy rojo config from helper"
        );
        return Ok(Json(RojoConfigResponse {
            place_id,
            task_id: None,
            launch_id: None,
            base_url,
            auth_header: format!("Bearer {bearer_token}"),
        }));
    }
    let studio_pid = resolve_peer_process_id_with_retry(peer_addr, app.helper.port).await?;
    let claimed_task = {
        let state = app.state.lock().await;
        resolve_claimed_task_for_plugin_request(
            &state,
            &place_id,
            explicit_task_id.as_deref(),
            explicit_launch_id.as_deref(),
            studio_pid,
        )
        .map_err(HelperError)?
    };
    if let Some(studio_pid) = studio_pid {
        let mut state = app.state.lock().await;
        bind_launch_process_to_pid(&mut state, &claimed_task, studio_pid).map_err(HelperError)?;
    }
    let base_url = claimed_task
        .rojo_base_url
        .clone()
        .ok_or_else(|| HelperError(eyre!("claimed task has no rojo_base_url")))?;
    tracing::info!(
        place_id,
        task_id = claimed_task.task_id,
        launch_id = claimed_task.launch_id,
        base_url,
        "resolved rojo config from helper"
    );
    Ok(Json(RojoConfigResponse {
        place_id,
        task_id: Some(claimed_task.task_id),
        launch_id: Some(claimed_task.launch_id),
        base_url,
        auth_header: format!("Bearer {bearer_token}"),
    }))
}

async fn mcp_register_handler(
    State(app): State<AppState>,
    ConnectInfo(peer_addr): ConnectInfo<SocketAddr>,
    Json(payload): Json<RegisterPluginRequest>,
) -> Result<Json<RegisterPluginResponse>, HelperError> {
    let place_id = sanitize_place_id(&payload.place_id)?;
    let instance_id = Uuid::new_v4().to_string();
    let explicit_task_id = maybe_sanitize_identifier("task_id", payload.task_id.as_deref())?;
    let explicit_launch_id = maybe_sanitize_identifier("launch_id", payload.launch_id.as_deref())?;
    let studio_pid = resolve_peer_process_id_with_retry(peer_addr, app.helper.port).await?;
    let claimed_task = if app.helper.hub_base_url.is_some() {
        let state = app.state.lock().await;
        Some(
            resolve_claimed_task_for_plugin_request(
                &state,
                &place_id,
                explicit_task_id.as_deref(),
                explicit_launch_id.as_deref(),
                studio_pid,
            )
            .map_err(HelperError)?,
        )
    } else {
        None
    };
    let remote_base_url = claimed_task
        .as_ref()
        .map(|task| task.mcp_base_url.clone())
        .unwrap_or_else(|| derive_public_base_url("mcp", &place_id, &app.helper));
    {
        let mut state = app.state.lock().await;
        state.instances.insert(
            instance_id.clone(),
            PluginInstance {
                place_id: place_id.clone(),
                task_id: claimed_task.as_ref().map(|task| task.task_id.clone()),
                generation: claimed_task.as_ref().map(|task| task.generation),
                launch_id: claimed_task.as_ref().map(|task| task.launch_id.clone()),
                remote_base_url: remote_base_url.clone(),
                studio_pid,
                last_seen_at: Instant::now(),
                queue: VecDeque::new(),
                notify: Arc::new(Notify::new()),
            },
        );
        if let (Some(claimed_task), Some(studio_pid)) = (claimed_task.as_ref(), studio_pid) {
            bind_launch_process_to_pid(&mut state, claimed_task, studio_pid)
                .map_err(HelperError)?;
        }
        sync_remote_connections(&app, &mut state);
    }
    tracing::info!(
        instance_id,
        place_id,
        task_id = claimed_task.as_ref().map(|task| task.task_id.clone()),
        launch_id = claimed_task.as_ref().map(|task| task.launch_id.clone()),
        studio_pid,
        remote_base_url,
        "registered MCP plugin instance with helper"
    );
    Ok(Json(RegisterPluginResponse {
        instance_id,
        place_id,
        task_id: claimed_task.as_ref().map(|task| task.task_id.clone()),
        launch_id: claimed_task.as_ref().map(|task| task.launch_id.clone()),
        remote_base_url,
    }))
}

async fn mcp_unregister_handler(
    State(app): State<AppState>,
    Json(payload): Json<UnregisterPluginRequest>,
) -> Result<Response, HelperError> {
    let mut state = app.state.lock().await;
    let removed_place_id = state
        .instances
        .remove(&payload.instance_id)
        .map(|instance| instance.place_id);
    if let Some(place_id) = removed_place_id.as_ref() {
        let still_active = state
            .instances
            .values()
            .any(|instance| instance.place_id == *place_id);
        if !still_active {
            state.last_remote_errors.remove(place_id);
        }
    }
    sync_remote_connections(&app, &mut state);
    tracing::info!(
        instance_id = payload.instance_id,
        "unregistered MCP plugin instance"
    );
    Ok(StatusCode::NO_CONTENT.into_response())
}

async fn mcp_plugin_request_handler(
    State(app): State<AppState>,
    Query(query): Query<PluginRequestQuery>,
) -> Result<Response, HelperError> {
    let timeout = tokio::time::timeout(LOCAL_LONG_POLL_TIMEOUT, async {
        loop {
            let notify = {
                let mut state = app.state.lock().await;
                let Some(instance) = state.instances.get_mut(&query.instance_id) else {
                    tracing::info!(
                        instance_id = query.instance_id,
                        "plugin polled helper with expired instance_id"
                    );
                    return Ok::<Option<Value>, color_eyre::Report>(None);
                };
                instance.last_seen_at = Instant::now();
                if let Some(command) = instance.queue.pop_front() {
                    tracing::info!(
                        instance_id = query.instance_id,
                        "helper delivered queued MCP command to plugin"
                    );
                    return Ok::<Option<Value>, color_eyre::Report>(Some(command));
                }
                Arc::clone(&instance.notify)
            };
            notify.notified().await;
        }
    })
    .await;
    match timeout {
        Ok(result) => match result? {
            Some(command) => Ok(Json(command).into_response()),
            None => Ok((StatusCode::GONE, "instance expired").into_response()),
        },
        Err(_) => Ok((StatusCode::LOCKED, String::new()).into_response()),
    }
}

async fn mcp_plugin_response_handler(
    State(app): State<AppState>,
    Json(payload): Json<PluginResponsePayload>,
) -> Result<Response, HelperError> {
    let tx = {
        let mut state = app.state.lock().await;
        state.waiting_for_plugin.remove(&payload.id)
    };
    if let Some(tx) = tx {
        let _ = tx.send(payload.clone());
        tracing::info!(
            id = payload.id,
            success = payload.success,
            "helper received plugin tool response"
        );
    } else {
        tracing::warn!(
            id = payload.id,
            "received late or unknown plugin tool response"
        );
    }
    Ok(StatusCode::NO_CONTENT.into_response())
}

async fn screenshot_debug_handler(
    State(app): State<AppState>,
    Query(query): Query<ScreenshotQuery>,
) -> Result<Json<ScreenshotResponse>, HelperError> {
    let place_id = match query.place_id.as_deref() {
        Some(value) => Some(sanitize_place_id(value)?),
        None => None,
    };
    let task_id = maybe_sanitize_identifier("task_id", query.task_id.as_deref())?;
    let launch_id = maybe_sanitize_identifier("launch_id", query.launch_id.as_deref())?;
    let path = take_screenshot(
        app,
        place_id.as_deref(),
        task_id.as_deref(),
        query.generation,
        launch_id.as_deref(),
    )
    .await?;
    Ok(Json(ScreenshotResponse { path }))
}

async fn runtime_screenshot_handler(
    State(app): State<AppState>,
    Json(payload): Json<RuntimeScreenshotRequest>,
) -> Result<Json<RuntimeScreenshotResponse>, HelperError> {
    let response = upload_runtime_screenshot(app, payload).await?;
    Ok(Json(response))
}

async fn read_studio_log_debug_handler(
    Query(query): Query<ReadStudioLogArgs>,
) -> Result<Json<ReadStudioLogResponse>, HelperError> {
    Ok(Json(read_studio_log(query)?))
}

fn place_connection_key(place_id: &str) -> String {
    format!("place:{place_id}")
}

fn task_connection_key(task_id: &str, generation: u32, launch_id: &str) -> String {
    format!("task:{task_id}:{generation}:{launch_id}")
}

fn derive_remote_helper_ws_url_from_base_url(remote_base_url: &str) -> String {
    let base = if remote_base_url.starts_with("https://") {
        remote_base_url.replacen("https://", "wss://", 1)
    } else if remote_base_url.starts_with("http://") {
        remote_base_url.replacen("http://", "ws://", 1)
    } else {
        remote_base_url.to_owned()
    };
    format!("{}{}", base.trim_end_matches('/'), HELPER_WS_PATH)
}

fn build_active_remote_targets(app: &AppState, state: &HelperState) -> Vec<RemoteTarget> {
    let mut targets = Vec::new();
    for claimed_task in state.claimed_tasks.values() {
        targets.push(RemoteTarget {
            connection_key: task_connection_key(
                &claimed_task.task_id,
                claimed_task.generation,
                &claimed_task.launch_id,
            ),
            place_id: claimed_task.place_id.clone(),
            task_id: Some(claimed_task.task_id.clone()),
            generation: Some(claimed_task.generation),
            launch_id: Some(claimed_task.launch_id.clone()),
            remote_base_url: claimed_task.mcp_base_url.clone(),
        });
    }
    if app.helper.hub_base_url.is_some() {
        return targets;
    }
    let mut seen_places = HashSet::new();
    for instance in state.instances.values() {
        if !seen_places.insert(instance.place_id.clone()) {
            continue;
        }
        targets.push(RemoteTarget {
            connection_key: place_connection_key(&instance.place_id),
            place_id: instance.place_id.clone(),
            task_id: instance.task_id.clone(),
            generation: instance.generation,
            launch_id: instance.launch_id.clone(),
            remote_base_url: derive_public_base_url("mcp", &instance.place_id, &app.helper),
        });
    }
    targets
}

fn remote_connection_is_active(state: &HelperState, connection_key: &str) -> bool {
    state.claimed_tasks.values().any(|task| {
        task_connection_key(&task.task_id, task.generation, &task.launch_id) == connection_key
    }) || state
        .instances
        .values()
        .any(|instance| place_connection_key(&instance.place_id) == connection_key)
}

fn set_remote_connection_connecting(state: &mut HelperState, connection_key: &str) {
    if let Some(connection) = state.remote_connections.get_mut(connection_key) {
        connection.connection_state = RemoteConnectionState::Connecting;
        connection.connection_id = None;
        connection.last_server_message_at = None;
    }
}

fn set_remote_connection_connected(
    state: &mut HelperState,
    connection_key: &str,
    connection_id: &str,
) {
    if let Some(connection) = state.remote_connections.get_mut(connection_key) {
        let now = Instant::now();
        connection.connection_state = RemoteConnectionState::Connected;
        connection.connection_id = Some(connection_id.to_owned());
        connection.last_ready_at = Some(now);
        connection.last_server_message_at = Some(now);
    }
    state.last_remote_errors.remove(connection_key);
}

fn note_remote_connection_activity(state: &mut HelperState, connection_key: &str) {
    if let Some(connection) = state.remote_connections.get_mut(connection_key) {
        connection.last_server_message_at = Some(Instant::now());
    }
}

fn set_remote_connection_retrying(state: &mut HelperState, connection_key: &str, error: &str) {
    if let Some(connection) = state.remote_connections.get_mut(connection_key) {
        connection.connection_state = RemoteConnectionState::Retrying;
        connection.connection_id = None;
    }
    state
        .last_remote_errors
        .insert(connection_key.to_owned(), summarize_error(error));
}

fn sync_remote_connections(app: &AppState, state: &mut HelperState) {
    let active_targets = build_active_remote_targets(app, state);
    let active_keys: HashSet<String> = active_targets
        .iter()
        .map(|target| target.connection_key.clone())
        .collect();
    let stale_places: Vec<String> = state
        .remote_connections
        .keys()
        .filter(|connection_key| !active_keys.contains(*connection_key))
        .cloned()
        .collect();
    for connection_key in stale_places {
        if let Some(handle) = state.remote_connections.remove(&connection_key) {
            let _ = handle.stop_tx.send(true);
            tracing::info!(
                connection_key,
                "stopping helper remote websocket because plugin is gone"
            );
        }
        state.last_remote_errors.remove(&connection_key);
    }
    for target in active_targets {
        if state
            .remote_connections
            .contains_key(&target.connection_key)
        {
            continue;
        }
        let (stop_tx, stop_rx) = watch::channel(false);
        let (sender, _receiver) = mpsc::unbounded_channel::<RemoteOutgoingFrame>();
        state.remote_connections.insert(
            target.connection_key.clone(),
            RemoteConnectionHandle {
                stop_tx,
                sender,
                remote_base_url: target.remote_base_url.clone(),
                place_id: target.place_id.clone(),
                task_id: target.task_id.clone(),
                generation: target.generation,
                launch_id: target.launch_id.clone(),
                connection_state: RemoteConnectionState::Connecting,
                connection_id: None,
                last_ready_at: None,
                last_server_message_at: None,
            },
        );
        let worker_app = app.clone();
        let worker_target = target.clone();
        tokio::spawn(async move {
            remote_ws_loop(worker_app, worker_target, stop_rx).await;
        });
    }
}

async fn remote_ws_loop(app: AppState, target: RemoteTarget, mut stop_rx: watch::Receiver<bool>) {
    let ws_url = derive_remote_helper_ws_url_from_base_url(&target.remote_base_url);
    tracing::info!(
        connection_key = target.connection_key,
        place_id = target.place_id,
        ws_url,
        "starting helper remote websocket loop"
    );
    loop {
        if *stop_rx.borrow() {
            break;
        }
        {
            let mut state = app.state.lock().await;
            cleanup_stale_instances(&mut state.instances);
            if !remote_connection_is_active(&state, &target.connection_key) {
                state.remote_connections.remove(&target.connection_key);
                state.last_remote_errors.remove(&target.connection_key);
                break;
            }
        }

        {
            let mut state = app.state.lock().await;
            set_remote_connection_connecting(&mut state, &target.connection_key);
        }
        let result = run_remote_ws_session(
            app.clone(),
            &target.connection_key,
            &target.place_id,
            target.task_id.as_deref(),
            target.generation,
            target.launch_id.as_deref(),
            &ws_url,
            &mut stop_rx,
        )
        .await;
        match result {
            Ok(()) => break,
            Err(error) => {
                tracing::warn!(
                    connection_key = target.connection_key,
                    place_id = target.place_id,
                    error = summarize_error(&error.to_string()),
                    "helper remote websocket session failed"
                );
                let mut state = app.state.lock().await;
                if remote_connection_is_active(&state, &target.connection_key) {
                    set_remote_connection_retrying(
                        &mut state,
                        &target.connection_key,
                        &error.to_string(),
                    );
                }
                drop(state);
                tokio::select! {
                    _ = tokio::time::sleep(Duration::from_secs(3)) => {}
                    changed = stop_rx.changed() => {
                        if changed.is_ok() && *stop_rx.borrow() {
                            break;
                        }
                    }
                }
                continue;
            }
        }
    }
    let mut state = app.state.lock().await;
    state.remote_connections.remove(&target.connection_key);
    state.last_remote_errors.remove(&target.connection_key);
    tracing::info!(
        connection_key = target.connection_key,
        place_id = target.place_id,
        "stopped helper remote websocket loop"
    );
}

async fn hub_maintenance_loop(app: AppState) {
    if app.helper.hub_base_url.is_none() {
        return;
    }
    let mut registered = false;
    let mut interval = tokio::time::interval(Duration::from_secs(5));
    loop {
        interval.tick().await;
        if !registered {
            match hub_register_helper(&app.helper).await {
                Ok(_) => {
                    registered = true;
                    let mut state = app.state.lock().await;
                    state.hub_last_error = None;
                    state.hub_last_ready_at = Some(Instant::now());
                }
                Err(error) => {
                    let mut state = app.state.lock().await;
                    state.hub_last_error = Some(summarize_error(&error.to_string()));
                    continue;
                }
            }
        }

        let active_launches = {
            let state = app.state.lock().await;
            state
                .claimed_tasks
                .values()
                .map(|task| HelperLaunchHubPayload {
                    launch_id: task.launch_id.clone(),
                    task_id: task.task_id.clone(),
                    generation: task.generation,
                })
                .collect::<Vec<_>>()
        };

        match hub_helper_heartbeat(&app.helper, active_launches).await {
            Ok(response) => {
                let pending_terminations = {
                    let mut state = app.state.lock().await;
                    state.hub_last_error = None;
                    state.hub_last_ready_at = Some(Instant::now());
                    let mut pending_terminations = Vec::new();
                    for release in response.release_launches {
                        if let Some(pending_termination) = release_claimed_task(
                            &mut state,
                            &release.task_id,
                            release.generation,
                            &release.launch_id,
                            true,
                            "hub requested launch release",
                        ) {
                            pending_terminations.push(pending_termination);
                        }
                    }
                    sync_remote_connections(&app, &mut state);
                    pending_terminations
                };
                for pending_termination in pending_terminations {
                    terminate_pending_launch_termination(pending_termination).await;
                }
            }
            Err(error) => {
                let error_text = summarize_error(&error.to_string());
                let mut state = app.state.lock().await;
                state.hub_last_error = Some(error_text.clone());
                if error.to_string().contains("helper not found") {
                    registered = false;
                }
                continue;
            }
        }

        let should_claim = {
            let state = app.state.lock().await;
            state.claimed_tasks.len() < app.helper.capacity
        };
        if !should_claim {
            continue;
        }
        match hub_claim_task(&app.helper).await {
            Ok(response) => {
                if let Some(task) = response.task {
                    let Some(mcp_base_url) = task.routes.mcp_base_url else {
                        let mut state = app.state.lock().await;
                        state.hub_last_error =
                            Some("hub claimed task missing mcp_base_url".to_owned());
                        continue;
                    };
                    let claimed_task = ClaimedTask {
                        task_id: task.task_id.clone(),
                        launch_id: task.launch_id,
                        generation: task.generation,
                        place_id: task.place_id,
                        game_id: task.game_id,
                        mcp_base_url,
                        rojo_base_url: task.routes.rojo_base_url,
                        runtime_log_base_url: task.routes.runtime_log_base_url,
                        claimed_at: Instant::now(),
                    };
                    #[cfg(target_os = "windows")]
                    let launch_record = match launch_studio_for_claim(&app.helper, &claimed_task) {
                        Ok(record) => Some(record),
                        Err(error) => {
                            let mut state = app.state.lock().await;
                            state.hub_last_error = Some(summarize_error(&error.to_string()));
                            continue;
                        }
                    };
                    #[cfg(not(target_os = "windows"))]
                    let launch_record: Option<LaunchProcessRecord> = None;
                    let mut state = app.state.lock().await;
                    state
                        .claimed_tasks
                        .insert(claimed_task.task_id.clone(), claimed_task);
                    if let Some(launch_record) = launch_record {
                        state
                            .launch_processes
                            .insert(launch_record.task_id.clone(), launch_record);
                    }
                    state.hub_last_error = None;
                    state.hub_last_ready_at = Some(Instant::now());
                    sync_remote_connections(&app, &mut state);
                }
            }
            Err(error) => {
                let mut state = app.state.lock().await;
                state.hub_last_error = Some(summarize_error(&error.to_string()));
            }
        }
    }
}

async fn connect_remote_ws(
    helper: &HelperConfig,
    ws_url: &str,
) -> Result<
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
> {
    let current_token = helper.bearer_token.lock().await.clone();
    let current_source = helper.bearer_token_source.lock().await.clone();
    let mut attempts = vec![ResolvedToken {
        value: current_token.clone(),
        source: current_source,
    }];
    for candidate in helper.bearer_token_candidates.iter() {
        if candidate.value != current_token {
            attempts.push(candidate.clone());
        }
    }
    let mut last_error = None;
    for candidate in attempts {
        let mut request = ws_url.into_client_request()?;
        request.headers_mut().insert(
            AUTHORIZATION,
            format!("Bearer {}", candidate.value).parse()?,
        );
        match connect_async(request).await {
            Ok((stream, _response)) => {
                if candidate.value != current_token {
                    *helper.bearer_token.lock().await = candidate.value.clone();
                    *helper.bearer_token_source.lock().await = candidate.source.clone();
                    tracing::warn!(
                        source = candidate.source,
                        "helper switched bearer token after websocket connect retry"
                    );
                }
                return Ok(stream);
            }
            Err(error) => {
                last_error = Some(error.to_string());
            }
        }
    }
    Err(eyre!(
        last_error.unwrap_or_else(|| "websocket connect failed".to_owned())
    ))
}

fn encode_remote_message(message: &HelperToServerMessage) -> Result<String> {
    Ok(serde_json::to_string(message)?)
}

fn build_artifact_chunk_message(upload_id: Uuid, seq: u32, chunk: &[u8]) -> Result<String> {
    encode_remote_message(&HelperToServerMessage::ArtifactChunk(ArtifactChunk {
        upload_id: upload_id.to_string(),
        seq,
        data_base64: base64::engine::general_purpose::STANDARD.encode(chunk),
    }))
}

#[derive(Debug, Deserialize)]
struct TakeScreenshotToolArgs {
    session_id: Option<String>,
    runtime_id: Option<String>,
    tag: Option<String>,
}

fn send_artifact_abort_message(
    sender: &mpsc::UnboundedSender<RemoteOutgoingFrame>,
    upload_id: Uuid,
    request_id: &str,
    error: &str,
) {
    if let Ok(encoded) =
        encode_remote_message(&HelperToServerMessage::ArtifactAbort(ArtifactAbort {
            upload_id: upload_id.to_string(),
            request_id: request_id.to_owned(),
            error: error.to_owned(),
        }))
    {
        let _ = sender.send(RemoteOutgoingFrame::Text(encoded));
    }
}

async fn stream_runtime_screenshot(
    app: AppState,
    place_id: &str,
    task_id: Option<&str>,
    generation: Option<u32>,
    launch_id: Option<&str>,
    args: TakeScreenshotToolArgs,
    sender: mpsc::UnboundedSender<RemoteOutgoingFrame>,
    upload_waiters: Arc<Mutex<HashMap<String, oneshot::Sender<Result<ArtifactCommitted>>>>>,
    request_id: String,
) -> Result<String> {
    let session_id = sanitize_identifier(
        "session_id",
        args.session_id
            .as_deref()
            .ok_or_else(|| eyre!("take_screenshot requires session_id"))?,
    )?;
    let runtime_id =
        sanitize_identifier("runtime_id", args.runtime_id.as_deref().unwrap_or("server"))?;
    let tag = args
        .tag
        .as_deref()
        .map(|value| sanitize_identifier("tag", value))
        .transpose()?;
    let captured =
        capture_screenshot_png(app, Some(place_id), task_id, generation, launch_id).await?;
    let upload_id = Uuid::new_v4();
    let (tx, rx) = oneshot::channel();
    upload_waiters
        .lock()
        .await
        .insert(upload_id.to_string(), tx);

    if sender
        .send(RemoteOutgoingFrame::Text(encode_remote_message(
            &HelperToServerMessage::ArtifactBegin(ArtifactBegin {
                upload_id: upload_id.to_string(),
                request_id: request_id.clone(),
                session_id: session_id.clone(),
                runtime_id: runtime_id.clone(),
                place_id: place_id.to_owned(),
                task_id: task_id.map(ToOwned::to_owned),
                generation,
                launch_id: launch_id.map(ToOwned::to_owned),
                tag: tag.clone(),
                content_type: "image/png".to_owned(),
                total_bytes: captured.png_bytes.len(),
            }),
        )?))
        .is_err()
    {
        upload_waiters.lock().await.remove(&upload_id.to_string());
        return Err(eyre!(
            "remote websocket sender dropped before artifact begin"
        ));
    }

    for (seq, chunk) in captured
        .png_bytes
        .chunks(MAX_ARTIFACT_CHUNK_PAYLOAD_BYTES)
        .enumerate()
    {
        if sender
            .send(RemoteOutgoingFrame::Text(build_artifact_chunk_message(
                upload_id, seq as u32, chunk,
            )?))
            .is_err()
        {
            upload_waiters.lock().await.remove(&upload_id.to_string());
            send_artifact_abort_message(
                &sender,
                upload_id,
                &request_id,
                "remote websocket sender dropped during artifact upload",
            );
            return Err(eyre!(
                "remote websocket sender dropped during artifact upload"
            ));
        }
    }

    if sender
        .send(RemoteOutgoingFrame::Text(encode_remote_message(
            &HelperToServerMessage::ArtifactFinish(ArtifactFinish {
                upload_id: upload_id.to_string(),
                request_id: request_id.clone(),
                total_chunks: captured
                    .png_bytes
                    .chunks(MAX_ARTIFACT_CHUNK_PAYLOAD_BYTES)
                    .len() as u32,
            }),
        )?))
        .is_err()
    {
        upload_waiters.lock().await.remove(&upload_id.to_string());
        send_artifact_abort_message(
            &sender,
            upload_id,
            &request_id,
            "remote websocket sender dropped before artifact finish",
        );
        return Err(eyre!(
            "remote websocket sender dropped before artifact finish"
        ));
    }

    let committed = match tokio::time::timeout(Duration::from_secs(30), rx).await {
        Ok(result) => result??,
        Err(_) => {
            upload_waiters.lock().await.remove(&upload_id.to_string());
            send_artifact_abort_message(
                &sender,
                upload_id,
                &request_id,
                "timed out waiting for artifact commit acknowledgement",
            );
            return Err(eyre!(
                "timed out waiting for artifact commit acknowledgement"
            ));
        }
    };

    let response = RuntimeScreenshotResponse {
        session_id: committed.session_id,
        runtime_id: committed.runtime_id,
        place_id: committed.place_id,
        screenshot_path: committed.screenshot_path,
        screenshot_rel_path: Some(committed.screenshot_rel_path),
        artifact_dir: committed.artifact_dir,
        session_metadata_path: committed.session_metadata_path,
        bytes_written: committed.bytes_written,
    };
    Ok(serde_json::to_string(&response)?)
}

async fn run_remote_ws_session(
    app: AppState,
    connection_key: &str,
    place_id: &str,
    task_id: Option<&str>,
    generation: Option<u32>,
    launch_id: Option<&str>,
    ws_url: &str,
    stop_rx: &mut watch::Receiver<bool>,
) -> Result<()> {
    let stream = connect_remote_ws(&app.helper, ws_url).await?;
    let (mut writer, mut reader) = stream.split();
    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<RemoteOutgoingFrame>();
    let mut last_server_message_at = Instant::now();
    let mut last_heartbeat_sent_at = Instant::now();
    let mut heartbeat_count: u64 = 0;
    let mut stopped_by_request = false;
    {
        let mut state = app.state.lock().await;
        if let Some(connection) = state.remote_connections.get_mut(connection_key) {
            connection.sender = out_tx.clone();
        }
        set_remote_connection_connecting(&mut state, connection_key);
        state.last_remote_errors.remove(connection_key);
    }
    let writer_task = tokio::spawn(async move {
        while let Some(frame) = out_rx.recv().await {
            let message = match frame {
                RemoteOutgoingFrame::Text(text) => WsMessage::Text(text.into()),
                RemoteOutgoingFrame::Pong(payload) => WsMessage::Pong(payload.into()),
            };
            if let Err(error) = writer.send(message).await {
                tracing::warn!(
                    error = summarize_error(&error.to_string()),
                    "helper remote websocket writer failed"
                );
                break;
            }
        }
    });
    if out_tx
        .send(RemoteOutgoingFrame::Text(encode_remote_message(
            &HelperToServerMessage::Hello(HelperHello {
                helper_id: app.helper.helper_id.clone(),
                place_id: place_id.to_owned(),
                task_id: task_id.map(ToOwned::to_owned),
                generation,
                launch_id: launch_id.map(ToOwned::to_owned),
                helper_version: env!("CARGO_PKG_VERSION").to_owned(),
                capabilities: vec![
                    "ws_tool_dispatch_v1".to_owned(),
                    "runtime_screenshot_stream_v1".to_owned(),
                    "read_studio_log_v1".to_owned(),
                ],
                plugin_instance_count: {
                    let state = app.state.lock().await;
                    state
                        .instances
                        .values()
                        .filter(|instance| {
                            if let Some(expected_task_id) = task_id {
                                instance.task_id.as_deref() == Some(expected_task_id)
                            } else {
                                instance.place_id == place_id
                            }
                        })
                        .count()
                },
            }),
        )?))
        .is_err()
    {
        writer_task.abort();
        return Err(eyre!("remote websocket sender dropped before hello"));
    }

    let upload_waiters: Arc<Mutex<HashMap<String, oneshot::Sender<Result<ArtifactCommitted>>>>> =
        Arc::new(Mutex::new(HashMap::new()));
    let mut heartbeat = tokio::time::interval(Duration::from_secs(5));
    loop {
        tokio::select! {
            changed = stop_rx.changed() => {
                if changed.is_ok() && *stop_rx.borrow() {
                    stopped_by_request = true;
                    break;
                }
            }
            _ = heartbeat.tick() => {
                let plugin_instance_count = {
                    let state = app.state.lock().await;
                    state.instances.values().filter(|instance| {
                        if let Some(expected_task_id) = task_id {
                            instance.task_id.as_deref() == Some(expected_task_id)
                        } else {
                            instance.place_id == place_id
                        }
                    }).count()
                };
                heartbeat_count += 1;
                last_heartbeat_sent_at = Instant::now();
                if heartbeat_count == 1 || heartbeat_count % 12 == 0 {
                    tracing::info!(
                        place_id,
                        heartbeat_count,
                        plugin_instance_count,
                        idle_from_server_ms = last_server_message_at.elapsed().as_millis(),
                        "sending helper remote websocket heartbeat"
                    );
                }
                if out_tx.send(RemoteOutgoingFrame::Text(encode_remote_message(&HelperToServerMessage::Heartbeat {
                    helper_id: app.helper.helper_id.clone(),
                    place_id: place_id.to_owned(),
                    task_id: task_id.map(ToOwned::to_owned),
                    generation,
                    launch_id: launch_id.map(ToOwned::to_owned),
                    plugin_instance_count,
                })?)).is_err() {
                    tracing::warn!(place_id, heartbeat_count, "failed to queue helper remote websocket heartbeat");
                    break;
                }
            }
            message = reader.next() => {
                let Some(message) = message else {
                    break;
                };
                match message? {
                    WsMessage::Text(text) => {
                        last_server_message_at = Instant::now();
                        {
                            let mut state = app.state.lock().await;
                            note_remote_connection_activity(&mut state, connection_key);
                        }
                        match serde_json::from_str::<ServerToHelperMessage>(&text)? {
                            ServerToHelperMessage::ReadyAck { connection_id, task_id: ack_task_id, generation: ack_generation, launch_id: ack_launch_id, .. } => {
                                if ack_task_id != task_id.map(ToOwned::to_owned)
                                    || ack_generation != generation
                                    || ack_launch_id != launch_id.map(ToOwned::to_owned)
                                {
                                    return Err(eyre!(
                                        "remote MCP ready ack identity mismatch: expected task={:?} generation={:?} launch={:?}, got task={:?} generation={:?} launch={:?}",
                                        task_id,
                                        generation,
                                        launch_id,
                                        ack_task_id,
                                        ack_generation,
                                        ack_launch_id,
                                    ));
                                }
                                {
                                    let mut state = app.state.lock().await;
                                    set_remote_connection_connected(&mut state, connection_key, &connection_id);
                                }
                                tracing::info!(connection_key, place_id, connection_id, "helper websocket acknowledged by MCP server");
                            }
                            ServerToHelperMessage::ToolCall { request_id, command } => {
                                let tool_sender = out_tx.clone();
                                let tool_waiters = Arc::clone(&upload_waiters);
                                let tool_app = app.clone();
                                let tool_place = place_id.to_owned();
                                let tool_task = task_id.map(ToOwned::to_owned);
                                let tool_generation = generation;
                                let tool_launch = launch_id.map(ToOwned::to_owned);
                                tokio::spawn(async move {
                                    let result = handle_remote_command(
                                        tool_app,
                                        &tool_place,
                                        tool_task.as_deref(),
                                        tool_generation,
                                        tool_launch.as_deref(),
                                        command,
                                        tool_sender.clone(),
                                        tool_waiters,
                                        request_id.clone(),
                                    ).await;
                                    let message = match result {
                                        Ok(body) => HelperToServerMessage::ToolResult { request_id, response: body },
                                        Err(error) => HelperToServerMessage::ToolError { request_id, error: error.to_string() },
                                    };
                                    if let Ok(encoded) = encode_remote_message(&message) {
                                        let _ = tool_sender.send(RemoteOutgoingFrame::Text(encoded));
                                    }
                                });
                            }
                            ServerToHelperMessage::ArtifactCommitted(committed) => {
                                if let Some(tx) = upload_waiters.lock().await.remove(&committed.upload_id) {
                                    let _ = tx.send(Ok(committed));
                                }
                            }
                            ServerToHelperMessage::ArtifactFailed { upload_id, error, .. } => {
                                if let Some(tx) = upload_waiters.lock().await.remove(&upload_id) {
                                    let _ = tx.send(Err(eyre!(error)));
                                }
                            }
                            ServerToHelperMessage::CloseReason { reason } => {
                                return Err(eyre!("remote MCP server closed helper websocket: {reason}"));
                            }
                        }
                    }
                    WsMessage::Ping(payload) => {
                        last_server_message_at = Instant::now();
                        {
                            let mut state = app.state.lock().await;
                            note_remote_connection_activity(&mut state, connection_key);
                        }
                        let _ = out_tx.send(RemoteOutgoingFrame::Pong(payload));
                    }
                    WsMessage::Close(_) => break,
                    WsMessage::Binary(_) | WsMessage::Pong(_) | WsMessage::Frame(_) => {
                        last_server_message_at = Instant::now();
                        {
                            let mut state = app.state.lock().await;
                            note_remote_connection_activity(&mut state, connection_key);
                        }
                    }
                }
            }
        }
    }
    writer_task.abort();
    let mut pending = upload_waiters.lock().await;
    for (_, tx) in pending.drain() {
        let _ = tx.send(Err(eyre!(
            "remote websocket session closed before artifact commit"
        )));
    }
    tracing::warn!(
        connection_key,
        place_id,
        heartbeat_count,
        last_server_message_age_ms = last_server_message_at.elapsed().as_millis(),
        last_heartbeat_sent_age_ms = last_heartbeat_sent_at.elapsed().as_millis(),
        pending_upload_waiters = pending.len(),
        "helper remote websocket session ended"
    );
    if stopped_by_request {
        Ok(())
    } else {
        Err(eyre!("remote websocket session ended unexpectedly"))
    }
}

async fn handle_remote_command(
    app: AppState,
    place_id: &str,
    task_id: Option<&str>,
    generation: Option<u32>,
    launch_id: Option<&str>,
    command: Value,
    sender: mpsc::UnboundedSender<RemoteOutgoingFrame>,
    upload_waiters: Arc<Mutex<HashMap<String, oneshot::Sender<Result<ArtifactCommitted>>>>>,
    request_id: String,
) -> Result<String> {
    let (tool_name, payload) = extract_tool_name_and_args(&command)?;
    tracing::info!(
        place_id,
        task_id,
        generation,
        launch_id,
        tool = tool_name,
        "helper received remote MCP command"
    );
    match tool_name.as_str() {
        "TakeScreenshot" | "take_screenshot" => {
            let args: TakeScreenshotToolArgs = serde_json::from_value(payload)?;
            stream_runtime_screenshot(
                app,
                place_id,
                task_id,
                generation,
                launch_id,
                args,
                sender,
                upload_waiters,
                request_id,
            )
            .await
        }
        "ReadStudioLog" | "read_studio_log" => {
            let args: ReadStudioLogArgs = serde_json::from_value(payload)?;
            Ok(serde_json::to_string(&read_studio_log(args)?)?)
        }
        _ => forward_to_plugin(app, place_id, task_id, generation, launch_id, command).await,
    }
}

async fn forward_to_plugin(
    app: AppState,
    place_id: &str,
    task_id: Option<&str>,
    generation: Option<u32>,
    launch_id: Option<&str>,
    command: Value,
) -> Result<String> {
    let request_id = extract_command_id(&command)?;
    let (instance_id, notify) = {
        let mut state = app.state.lock().await;
        cleanup_stale_instances(&mut state.instances);
        sync_remote_connections(&app, &mut state);
        let selected_instance_id =
            select_instance_for_route(place_id, task_id, generation, launch_id, &state.instances)
                .ok_or_else(|| eyre!("no active Studio plugin registered for placeId {place_id}"))?;
        let instance = state
            .instances
            .get_mut(&selected_instance_id)
            .ok_or_else(|| eyre!("selected helper instance disappeared"))?;
        instance.queue.push_back(command);
        let notify = Arc::clone(&instance.notify);
        let (tx, rx) = oneshot::channel();
        state.waiting_for_plugin.insert(request_id.clone(), tx);
        (selected_instance_id, (notify, rx))
    };
    notify.0.notify_waiters();
    tracing::info!(
        instance_id,
        place_id,
        task_id,
        generation,
        launch_id,
        id = request_id,
        "forwarded MCP command from helper to local plugin"
    );
    let response = match tokio::time::timeout(PLUGIN_REQUEST_TIMEOUT, notify.1).await {
        Ok(result) => result?,
        Err(_) => {
            let mut state = app.state.lock().await;
            state.waiting_for_plugin.remove(&request_id);
            return Err(eyre!("timed out waiting for local plugin response"));
        }
    };
    if response.success {
        Ok(response.response)
    } else {
        Err(eyre!(response.response))
    }
}

fn cleanup_stale_instances(instances: &mut HashMap<String, PluginInstance>) {
    instances.retain(|instance_id, instance| {
        let keep = instance.last_seen_at.elapsed() <= INSTANCE_STALE_AFTER;
        if !keep {
            tracing::warn!(
                instance_id,
                place_id = instance.place_id,
                "dropping stale helper plugin instance"
            );
        }
        keep
    });
}

fn select_instance_for_place(
    place_id: &str,
    instances: &HashMap<String, PluginInstance>,
) -> Option<String> {
    instances
        .iter()
        .filter(|(_, instance)| instance.place_id == place_id)
        .max_by_key(|(_, instance)| instance.last_seen_at)
        .map(|(instance_id, _)| instance_id.clone())
}

fn select_instance_for_route(
    place_id: &str,
    task_id: Option<&str>,
    generation: Option<u32>,
    launch_id: Option<&str>,
    instances: &HashMap<String, PluginInstance>,
) -> Option<String> {
    let exact = instances
        .iter()
        .filter(|(_, instance)| {
            instance.place_id == place_id
                && route_identity_matches(instance, task_id, generation, launch_id)
        })
        .max_by_key(|(_, instance)| instance.last_seen_at)
        .map(|(instance_id, _)| instance_id.clone());
    if exact.is_some() {
        return exact;
    }
    if task_id.is_some() || generation.is_some() || launch_id.is_some() {
        return None;
    }
    select_instance_for_place(place_id, instances)
}

#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
async fn resolve_capture_pid(
    app: &AppState,
    place_id: Option<&str>,
    task_id: Option<&str>,
    generation: Option<u32>,
    launch_id: Option<&str>,
) -> Result<(Option<String>, u32)> {
    let state = app.state.lock().await;
    let selected = if let Some(place_id) = place_id {
        let instance_id =
            select_instance_for_route(place_id, task_id, generation, launch_id, &state.instances)
                .ok_or_else(|| eyre!("no active Studio plugin registered for placeId {place_id}"))?;
        let instance = state
            .instances
            .get(&instance_id)
            .ok_or_else(|| eyre!("selected helper instance disappeared"))?;
        (Some(place_id.to_owned()), instance.studio_pid)
    } else if state.instances.len() == 1 {
        let instance = state
            .instances
            .values()
            .next()
            .ok_or_else(|| eyre!("no active Studio plugin registered"))?;
        (Some(instance.place_id.clone()), instance.studio_pid)
    } else if state.instances.is_empty() {
        return Err(eyre!("no active Studio plugin registered"));
    } else {
        return Err(eyre!(
            "multiple Studio instances are active; pass placeId explicitly"
        ));
    };

    let studio_pid = selected.1.ok_or_else(|| {
        eyre!("helper could not resolve a Studio process id for the selected plugin instance")
    })?;
    Ok((selected.0, studio_pid))
}

async fn capture_screenshot_png(
    app: AppState,
    place_id: Option<&str>,
    task_id: Option<&str>,
    generation: Option<u32>,
    launch_id: Option<&str>,
) -> Result<CapturedScreenshot> {
    #[cfg(target_os = "windows")]
    {
        let (resolved_place_id, studio_pid) =
            resolve_capture_pid(&app, place_id, task_id, generation, launch_id).await?;
        let (studio_hwnd, viewport_hwnd, window_title) =
            find_studio_capture_target_for_pid(studio_pid)?;
        let script_template = r#"
Add-Type -AssemblyName System.Drawing
Add-Type @"
using System;
using System.Runtime.InteropServices;
public static class Win32 {
    [DllImport("user32.dll")]
    public static extern bool GetClientRect(IntPtr hWnd, out RECT rect);

    [DllImport("user32.dll")]
    public static extern bool PrintWindow(IntPtr hwnd, IntPtr hdcBlt, int nFlags);

    [DllImport("user32.dll")]
    public static extern bool IsIconic(IntPtr hWnd);

    [DllImport("user32.dll")]
    public static extern bool ShowWindow(IntPtr hWnd, int nCmdShow);

    [DllImport("user32.dll")]
    public static extern bool SetForegroundWindow(IntPtr hWnd);

    public struct RECT {
        public int Left;
        public int Top;
        public int Right;
        public int Bottom;
    }
}
"@

$studioHwnd = [IntPtr]::new(__STUDIO_HWND__)
$viewportHwnd = [IntPtr]::new(__VIEWPORT_HWND__)

$rect = New-Object Win32+RECT
[Win32]::GetClientRect($viewportHwnd, [ref]$rect) | Out-Null
$width = $rect.Right - $rect.Left
$height = $rect.Bottom - $rect.Top
if ($width -le 0 -or $height -le 0) {
    throw 'Selected Studio viewport has invalid bounds'
}

$capture = {
    param($windowHandle, $w, $h)

    $bitmap = New-Object System.Drawing.Bitmap $w, $h
    $graphics = [System.Drawing.Graphics]::FromImage($bitmap)
    $hdc = $graphics.GetHdc()
    $ok = [Win32]::PrintWindow($windowHandle, $hdc, 2)
    $graphics.ReleaseHdc($hdc)

    return @{
        ok = $ok
        bitmap = $bitmap
        graphics = $graphics
    }
}

$result = & $capture $viewportHwnd $width $height
$sampleX = [Math]::Min(10, [Math]::Max($width - 1, 0))
$sampleY = [Math]::Min(10, [Math]::Max($height - 1, 0))
if (-not $result.ok -or $result.bitmap.GetPixel($sampleX, $sampleY).ToArgb() -eq [System.Drawing.Color]::Black.ToArgb()) {
    if ([Win32]::IsIconic($studioHwnd)) {
        [Win32]::ShowWindow($studioHwnd, 9) | Out-Null
    }
    [Win32]::SetForegroundWindow($studioHwnd) | Out-Null
    Start-Sleep -Milliseconds 300
    $result.graphics.Dispose()
    $result.bitmap.Dispose()
    $result = & $capture $viewportHwnd $width $height
}

if (-not $result.ok) {
    throw 'PrintWindow failed for the selected Studio viewport'
}

$bitmap = New-Object System.Drawing.Bitmap $width, $height
$graphics = [System.Drawing.Graphics]::FromImage($bitmap)
$graphics.DrawImage($result.bitmap, 0, 0)
$memory = New-Object System.IO.MemoryStream
$bitmap.Save($memory, [System.Drawing.Imaging.ImageFormat]::Png)
$bytes = $memory.ToArray()
[Console]::OpenStandardOutput().Write($bytes, 0, $bytes.Length)
$result.graphics.Dispose()
$result.bitmap.Dispose()
$graphics.Dispose()
$bitmap.Dispose()
$memory.Dispose()
"#;
        let script = script_template
            .replace("__STUDIO_HWND__", &(studio_hwnd as usize).to_string())
            .replace("__VIEWPORT_HWND__", &(viewport_hwnd as usize).to_string());
        let output = std::process::Command::new("powershell.exe")
            .args(["-NoProfile", "-NonInteractive", "-Command", &script])
            .output()
            .wrap_err("failed to launch powershell screenshot helper")?;
        if !output.status.success() {
            return Err(eyre!(
                "screenshot command failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }
        tracing::info!(
            studio_pid,
            window_title,
            byte_len = output.stdout.len(),
            "captured Studio screenshot bytes from helper"
        );
        return Ok(CapturedScreenshot {
            resolved_place_id,
            png_bytes: output.stdout,
            window_title,
        });
    }

    #[cfg(not(target_os = "windows"))]
    {
        let _ = app;
        let _ = place_id;
        let _ = task_id;
        let _ = generation;
        let _ = launch_id;
        Err(eyre!(
            "take_screenshot is only available on Windows helper builds"
        ))
    }
}

async fn take_screenshot(
    app: AppState,
    place_id: Option<&str>,
    task_id: Option<&str>,
    generation: Option<u32>,
    launch_id: Option<&str>,
) -> Result<String> {
    #[cfg(target_os = "windows")]
    {
        let captured =
            capture_screenshot_png(app, place_id, task_id, generation, launch_id).await?;
        let output_dir = helper_data_dir()?.join("screenshots");
        fs::create_dir_all(&output_dir)?;
        let file_name = if let Some(place_id) = captured.resolved_place_id.as_deref() {
            format!("studio-{place_id}-{}.png", now_stamp())
        } else {
            format!("studio-{}.png", now_stamp())
        };
        let output_path = output_dir.join(file_name);
        fs::write(&output_path, captured.png_bytes)?;
        tracing::info!(path = %output_path.display(), window_title = captured.window_title, "saved Studio screenshot debug file from helper");
        return Ok(output_path.display().to_string());
    }

    #[cfg(not(target_os = "windows"))]
    {
        let _ = app;
        let _ = place_id;
        let _ = task_id;
        let _ = generation;
        let _ = launch_id;
        Err(eyre!(
            "take_screenshot is only available on Windows helper builds"
        ))
    }
}

async fn upload_runtime_screenshot(
    app: AppState,
    payload: RuntimeScreenshotRequest,
) -> Result<RuntimeScreenshotResponse> {
    let place_id = match payload.place_id.as_deref() {
        Some(value) => Some(sanitize_place_id(value)?),
        None => None,
    };
    let session_id = sanitize_identifier("session_id", &payload.session_id)?;
    let runtime_id = sanitize_identifier("runtime_id", &payload.runtime_id)?;
    let task_id = maybe_sanitize_identifier("task_id", payload.task_id.as_deref())?;

    let launch_id = maybe_sanitize_identifier("launch_id", payload.launch_id.as_deref())?;
    let captured = capture_screenshot_png(
        app.clone(),
        place_id.as_deref(),
        task_id.as_deref(),
        payload.generation,
        launch_id.as_deref(),
    )
    .await?;
    let final_place_id = captured
        .resolved_place_id
        .clone()
        .or(place_id)
        .ok_or_else(|| eyre!("helper could not resolve placeId for runtime screenshot upload"))?;
    let upload_url = {
        let state = app.state.lock().await;
        validate_runtime_screenshot_upload_url(
            &payload.upload_url,
            &final_place_id,
            task_id.as_deref(),
            payload.generation,
            &state,
            &app.helper,
        )?
    };
    let sink_response = post_runtime_screenshot(
        &app.helper,
        &upload_url,
        &session_id,
        &runtime_id,
        &final_place_id,
        payload.tag.as_deref(),
        &captured.png_bytes,
    )
    .await?;
    tracing::info!(
        session_id,
        runtime_id,
        place_id = final_place_id,
        screenshot_path = sink_response.screenshot_path,
        window_title = captured.window_title,
        "uploaded runtime screenshot through helper"
    );
    Ok(RuntimeScreenshotResponse {
        session_id: sink_response.session_id,
        runtime_id: sink_response.runtime_id,
        place_id: final_place_id,
        screenshot_path: sink_response.screenshot_path,
        screenshot_rel_path: None,
        artifact_dir: sink_response.artifact_dir,
        session_metadata_path: sink_response.session_metadata_path,
        bytes_written: sink_response.bytes_written,
    })
}

fn read_studio_log(args: ReadStudioLogArgs) -> Result<ReadStudioLogResponse> {
    #[cfg(target_os = "windows")]
    {
        let local_app_data = env::var_os("LOCALAPPDATA")
            .map(PathBuf::from)
            .ok_or_else(|| eyre!("LOCALAPPDATA is not set"))?;
        let logs_dir = local_app_data.join("Roblox").join("logs");
        let (log_path, content) = read_latest_studio_log(&logs_dir)?;
        let mut all_lines: Vec<String> = content.lines().map(ToOwned::to_owned).collect();
        if let Some(pattern) = args.regex.as_ref() {
            let regex = Regex::new(pattern)?;
            all_lines.retain(|line| regex.is_match(line));
        }
        let total_lines = all_lines.len();
        let count = args.line_count.unwrap_or(total_lines.max(1)).max(1);
        let mut start_line = args.start_line.unwrap_or(1);
        if start_line < 0 {
            start_line = (total_lines as i64 + start_line + 1).max(1);
        }
        let start = start_line.clamp(1, total_lines.max(1) as i64) as usize;
        let end = if total_lines == 0 {
            0
        } else {
            (start + count - 1).min(total_lines)
        };
        let lines = if total_lines == 0 || end == 0 {
            Vec::new()
        } else {
            all_lines[(start - 1)..end].to_vec()
        };
        tracing::info!(path = %log_path.display(), total_lines, returned_lines = lines.len(), "read Studio log from helper");
        return Ok(ReadStudioLogResponse {
            path: log_path.display().to_string(),
            total_lines,
            range: [if total_lines == 0 { 0 } else { start }, end],
            lines,
        });
    }

    #[cfg(not(target_os = "windows"))]
    {
        let _ = args;
        Err(eyre!(
            "read_studio_log is only available on Windows helper builds"
        ))
    }
}

#[cfg(target_os = "windows")]
fn list_candidate_log_files(logs_dir: &Path) -> Result<Vec<PathBuf>> {
    let mut studio_files = Vec::new();
    let mut other_files = Vec::new();
    let mut entries: Vec<_> = fs::read_dir(logs_dir)
        .wrap_err_with(|| format!("failed to read {}", logs_dir.display()))?
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            if path.extension().and_then(|value| value.to_str()) == Some("log") {
                let modified = entry.metadata().ok()?.modified().ok()?;
                Some((path, modified))
            } else {
                None
            }
        })
        .collect();
    entries.sort_by_key(|(_, modified)| *modified);
    entries.reverse();
    for (path, _) in entries {
        let file_name = path
            .file_name()
            .and_then(|value| value.to_str())
            .unwrap_or_default();
        if file_name.contains("_Studio_") {
            studio_files.push(path);
        } else {
            other_files.push(path);
        }
    }
    if !studio_files.is_empty() {
        return Ok(studio_files);
    }
    if !other_files.is_empty() {
        tracing::warn!(
            path = %logs_dir.display(),
            "no explicit Studio log file matched '_Studio_', falling back to latest .log files"
        );
        return Ok(other_files);
    }
    Err(eyre!("no Studio log file found in {}", logs_dir.display()))
}

#[cfg(target_os = "windows")]
fn read_latest_studio_log(logs_dir: &Path) -> Result<(PathBuf, String)> {
    let candidates = list_candidate_log_files(logs_dir)?;
    let mut last_error = None;
    for path in candidates {
        match fs::read(&path) {
            Ok(bytes) => {
                let content = String::from_utf8_lossy(&bytes).into_owned();
                return Ok((path, content));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                tracing::warn!(path = %path.display(), "Studio log candidate disappeared before read; trying next file");
                last_error = Some(format!("{}: {}", path.display(), error));
            }
            Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {
                tracing::warn!(path = %path.display(), "Studio log candidate was not readable; trying next file");
                last_error = Some(format!("{}: {}", path.display(), error));
            }
            Err(error) => {
                last_error = Some(format!("{}: {}", path.display(), error));
            }
        }
    }
    Err(eyre!(
        "failed to read any Studio log file in {}{}",
        logs_dir.display(),
        last_error
            .as_deref()
            .map(|value| format!("; last error: {value}"))
            .unwrap_or_default()
    ))
}

#[derive(Debug)]
struct HelperError(color_eyre::Report);

impl<E> From<E> for HelperError
where
    E: Into<color_eyre::Report>,
{
    fn from(value: E) -> Self {
        Self(value.into())
    }
}

impl IntoResponse for HelperError {
    fn into_response(self) -> Response {
        tracing::warn!(
            error = summarize_error(&self.0.to_string()),
            "helper request failed"
        );
        (StatusCode::BAD_REQUEST, self.0.to_string()).into_response()
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    color_eyre::install()?;
    let env_filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(env_filter)
        .with_target(false)
        .with_thread_ids(true)
        .init();

    let args = Args::parse();
    let bearer_token_candidates = resolve_bearer_token_candidates(&args)?;
    let initial_bearer_token = resolve_bearer_token(&args)?;
    let initial_bearer_token_source = bearer_token_candidates
        .iter()
        .find(|candidate| candidate.value == initial_bearer_token)
        .map(|candidate| candidate.source.clone())
        .unwrap_or_else(|| "unknown".to_owned());
    let helper_id = resolve_helper_id(&args)?;
    let helper = HelperConfig {
        port: args.port,
        helper_id: helper_id.clone(),
        capacity: args.capacity,
        user_name: resolve_user_name(&args)?,
        bearer_token: Arc::new(Mutex::new(initial_bearer_token)),
        bearer_token_source: Arc::new(Mutex::new(initial_bearer_token_source)),
        bearer_token_candidates: Arc::new(bearer_token_candidates),
        domain_suffix: args.domain_suffix.clone(),
        hub_base_url: args.hub_base_url.clone(),
        studio_path: args.studio_path.clone(),
        client: reqwest::Client::builder()
            .timeout(Duration::from_secs(20))
            .build()?,
    };
    let token_source = helper.bearer_token_source.lock().await.clone();
    tracing::info!(
        helper_id = helper.helper_id,
        user_name = helper.user_name,
        port = args.port,
        capacity = helper.capacity,
        hub_base_url = helper.hub_base_url,
        domain_suffix = helper.domain_suffix,
        token_source,
        "starting Studio helper"
    );

    let state = Arc::new(Mutex::new(HelperState {
        instances: HashMap::new(),
        claimed_tasks: HashMap::new(),
        launch_processes: HashMap::new(),
        waiting_for_plugin: HashMap::new(),
        remote_connections: HashMap::new(),
        last_remote_errors: HashMap::new(),
        hub_last_error: None,
        hub_last_ready_at: None,
    }));

    let app_state = AppState { helper, state };

    let maintenance_app = app_state.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(2));
        loop {
            interval.tick().await;
            let mut state = maintenance_app.state.lock().await;
            cleanup_stale_instances(&mut state.instances);
            reconcile_launch_processes(&mut state);
            sync_remote_connections(&maintenance_app, &mut state);
        }
    });

    let hub_app = app_state.clone();
    tokio::spawn(async move {
        hub_maintenance_loop(hub_app).await;
    });

    let app = Router::new()
        .route("/status", get(helper_status))
        .route("/v1/rojo/config", get(rojo_config_handler))
        .route("/v1/mcp/register", post(mcp_register_handler))
        .route("/v1/mcp/unregister", post(mcp_unregister_handler))
        .route("/v1/mcp/plugin/request", get(mcp_plugin_request_handler))
        .route("/v1/mcp/plugin/response", post(mcp_plugin_response_handler))
        .route("/v1/helper/screenshot", post(screenshot_debug_handler))
        .route(
            "/v1/helper/runtime-screenshot",
            post(runtime_screenshot_handler),
        )
        .route("/v1/helper/studio-log", get(read_studio_log_debug_handler))
        .with_state(app_state);

    let listener = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, args.port)).await?;
    tracing::info!(port = args.port, "Studio helper listening on 127.0.0.1");
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_instance(
        place_id: &str,
        task_id: Option<&str>,
        generation: Option<u32>,
        launch_id: Option<&str>,
        age_ms: u64,
    ) -> PluginInstance {
        PluginInstance {
            place_id: place_id.to_owned(),
            task_id: task_id.map(ToOwned::to_owned),
            generation,
            launch_id: launch_id.map(ToOwned::to_owned),
            remote_base_url: "https://example.com".to_owned(),
            studio_pid: Some(123),
            last_seen_at: Instant::now() - Duration::from_millis(age_ms),
            queue: VecDeque::new(),
            notify: Arc::new(Notify::new()),
        }
    }

    fn test_claimed_task(
        task_id: &str,
        generation: u32,
        launch_id: &str,
        place_id: &str,
    ) -> ClaimedTask {
        ClaimedTask {
            task_id: task_id.to_owned(),
            launch_id: launch_id.to_owned(),
            generation,
            place_id: place_id.to_owned(),
            game_id: Some("999".to_owned()),
            mcp_base_url: "https://example.com".to_owned(),
            rojo_base_url: Some("https://example.com".to_owned()),
            runtime_log_base_url: Some("https://example.com".to_owned()),
            claimed_at: Instant::now(),
        }
    }

    fn empty_helper_state() -> HelperState {
        HelperState {
            instances: HashMap::new(),
            claimed_tasks: HashMap::new(),
            launch_processes: HashMap::new(),
            waiting_for_plugin: HashMap::new(),
            remote_connections: HashMap::new(),
            last_remote_errors: HashMap::new(),
            hub_last_error: None,
            hub_last_ready_at: None,
        }
    }

    #[test]
    fn select_instance_for_route_uses_full_identity() {
        let mut instances = HashMap::new();
        instances.insert(
            "older".to_owned(),
            test_instance("93795519121520", Some("t1"), Some(1), Some("l_old"), 50),
        );
        instances.insert(
            "newer".to_owned(),
            test_instance("93795519121520", Some("t1"), Some(2), Some("l_new"), 5),
        );

        let selected = select_instance_for_route(
            "93795519121520",
            Some("t1"),
            Some(2),
            Some("l_new"),
            &instances,
        );
        assert_eq!(selected.as_deref(), Some("newer"));

        let missing = select_instance_for_route(
            "93795519121520",
            Some("t1"),
            Some(3),
            Some("l_missing"),
            &instances,
        );
        assert!(missing.is_none());
    }

    #[test]
    fn derive_remote_helper_ws_url_supports_http_and_https() {
        assert_eq!(
            derive_remote_helper_ws_url_from_base_url("https://example.com"),
            "wss://example.com/ws/helper"
        );
        assert_eq!(
            derive_remote_helper_ws_url_from_base_url("http://127.0.0.1:44756"),
            "ws://127.0.0.1:44756/ws/helper"
        );
        assert_eq!(
            derive_remote_helper_ws_url_from_base_url("ws://127.0.0.1:44756/"),
            "ws://127.0.0.1:44756/ws/helper"
        );
    }

    #[test]
    fn resolve_claimed_task_for_request_prefers_pid_binding() {
        let mut state = empty_helper_state();
        state.claimed_tasks.insert(
            "task_a".to_owned(),
            test_claimed_task("task_a", 1, "l_a", "93795519121520"),
        );
        state.claimed_tasks.insert(
            "task_b".to_owned(),
            test_claimed_task("task_b", 1, "l_b", "93795519121520"),
        );
        state.launch_processes.insert(
            "task_a".to_owned(),
            LaunchProcessRecord {
                task_id: "task_a".to_owned(),
                launch_id: "l_a".to_owned(),
                generation: 1,
                place_id: "93795519121520".to_owned(),
                game_id: Some("999".to_owned()),
                studio_pid: 111,
                launched_by_helper: true,
                child: None,
            },
        );
        state.launch_processes.insert(
            "task_b".to_owned(),
            LaunchProcessRecord {
                task_id: "task_b".to_owned(),
                launch_id: "l_b".to_owned(),
                generation: 1,
                place_id: "93795519121520".to_owned(),
                game_id: Some("999".to_owned()),
                studio_pid: 222,
                launched_by_helper: true,
                child: None,
            },
        );

        let selected =
            resolve_claimed_task_for_request(&state, "93795519121520", None, None, None, Some(222))
                .expect("pid-bound task should resolve");
        assert_eq!(selected.task_id, "task_b");
        assert_eq!(selected.launch_id, "l_b");
    }

    #[test]
    fn resolve_claimed_task_for_request_requires_identity_without_pid_binding() {
        let mut state = empty_helper_state();
        state.claimed_tasks.insert(
            "task_a".to_owned(),
            test_claimed_task("task_a", 1, "l_a", "93795519121520"),
        );

        let error = match resolve_claimed_task_for_request(
            &state,
            "93795519121520",
            None,
            None,
            None,
            Some(999),
        ) {
            Ok(_) => panic!("expected unbound Studio pid without explicit identity to fail"),
            Err(error) => error,
        };

        assert!(error
            .to_string()
            .contains("task_id, generation, and launch_id are required"));
    }

    #[test]
    fn resolve_claimed_task_for_plugin_request_bans_place_only_fallback() {
        let mut state = empty_helper_state();
        state.claimed_tasks.insert(
            "task_a".to_owned(),
            test_claimed_task("task_a", 1, "l_a", "93795519121520"),
        );

        let error = match resolve_claimed_task_for_plugin_request(
            &state,
            "93795519121520",
            None,
            None,
            Some(999),
        ) {
            Ok(_) => panic!("expected place-only plugin request without pid binding to fail"),
            Err(error) => error,
        };

        assert!(error
            .to_string()
            .contains("task_id or launch_id is required"));
    }

    #[test]
    fn resolve_claimed_task_for_plugin_request_accepts_task_hint_without_generation() {
        let mut state = empty_helper_state();
        state.claimed_tasks.insert(
            "task_a".to_owned(),
            test_claimed_task("task_a", 7, "l_a", "93795519121520"),
        );

        let selected = resolve_claimed_task_for_plugin_request(
            &state,
            "93795519121520",
            Some("task_a"),
            Some("l_a"),
            Some(999),
        )
        .expect("task hint should resolve without plugin-supplied generation");

        assert_eq!(selected.task_id, "task_a");
        assert_eq!(selected.generation, 7);
        assert_eq!(selected.launch_id, "l_a");
    }

    #[test]
    fn release_claimed_task_cleans_exact_route_state() {
        let mut state = empty_helper_state();
        state.claimed_tasks.insert(
            "task_a".to_owned(),
            test_claimed_task("task_a", 1, "l_a", "93795519121520"),
        );
        state.instances.insert(
            "instance_a".to_owned(),
            test_instance("93795519121520", Some("task_a"), Some(1), Some("l_a"), 5),
        );
        state.launch_processes.insert(
            "task_a".to_owned(),
            LaunchProcessRecord {
                task_id: "task_a".to_owned(),
                launch_id: "l_a".to_owned(),
                generation: 1,
                place_id: "93795519121520".to_owned(),
                game_id: Some("999".to_owned()),
                studio_pid: 111,
                launched_by_helper: false,
                child: None,
            },
        );
        state
            .last_remote_errors
            .insert(task_connection_key("task_a", 1, "l_a"), "boom".to_owned());

        let pending_termination =
            release_claimed_task(&mut state, "task_a", 1, "l_a", false, "test release");

        assert!(state.claimed_tasks.is_empty());
        assert!(state.instances.is_empty());
        assert!(state.launch_processes.is_empty());
        assert!(state.last_remote_errors.is_empty());
        assert!(pending_termination.is_none());
    }
}
