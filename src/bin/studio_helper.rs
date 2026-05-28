use axum::body::{Body, Bytes};
use axum::extract::ws::{Message as AxumWsMessage, WebSocket, WebSocketUpgrade};
use axum::extract::{ConnectInfo, Path as AxumPath, Query, State};
use axum::http::{HeaderMap, Method, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::routing::{any, get, post};
use axum::{Json, Router};
use base64::Engine as _;
use clap::Parser;
use color_eyre::eyre::{eyre, Report, Result, WrapErr};
use futures_util::{SinkExt, StreamExt};
use reqwest::header::{AUTHORIZATION, CONTENT_TYPE};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use serde_json::Value;
use std::collections::{HashMap, HashSet, VecDeque};
use std::env;
use std::fs;
use std::net::{Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::process::{Child, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::process::{
    Child as TokioChild, ChildStdin as TokioChildStdin, ChildStdout as TokioChildStdout,
    Command as TokioCommand,
};
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
    HelperTaskStatusSnapshot, HelperToServerMessage, OfficialMcpRequest, OfficialMcpResponse,
    ServerToHelperMessage, HELPER_WS_PATH, MAX_ARTIFACT_CHUNK_PAYLOAD_BYTES,
    OFFICIAL_MCP_ADAPTER_CAPABILITY,
};

#[cfg(target_os = "windows")]
use regex::Regex;
#[cfg(target_os = "windows")]
use std::mem::size_of;
#[cfg(target_os = "windows")]
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
#[cfg(target_os = "windows")]
use std::ptr::null_mut;
#[cfg(target_os = "windows")]
use std::time::{SystemTime, UNIX_EPOCH};
#[cfg(target_os = "windows")]
use windows_sys::Win32::Foundation::{HANDLE, HWND, LPARAM, RECT};
#[cfg(target_os = "windows")]
use windows_sys::Win32::NetworkManagement::IpHelper::{
    GetExtendedTcpTable, MIB_TCPROW_OWNER_PID, MIB_TCPTABLE_OWNER_PID, TCP_TABLE_OWNER_PID_ALL,
};
#[cfg(target_os = "windows")]
use windows_sys::Win32::Networking::WinSock::AF_INET;
#[cfg(target_os = "windows")]
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
    SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
};
#[cfg(target_os = "windows")]
use windows_sys::Win32::UI::WindowsAndMessaging::{
    EnumChildWindows, EnumWindows, GetClientRect, GetWindowRect, GetWindowTextLengthW,
    GetWindowTextW, GetWindowThreadProcessId, IsWindowVisible,
};

const DEFAULT_DOMAIN_SUFFIX: &str = "dev.clock-p.com";
const DEFAULT_HELPER_PORT: u16 = 44750;
const LOCAL_LONG_POLL_TIMEOUT: Duration = Duration::from_secs(15);
const PLUGIN_REQUEST_TIMEOUT: Duration = Duration::from_secs(120);
const OFFICIAL_MCP_REQUEST_TIMEOUT: Duration = Duration::from_secs(600);
const INSTANCE_STALE_AFTER: Duration = Duration::from_secs(45);
const ALLOWED_OFFICIAL_MCP_TOOLS: &[&str] = &[
    "generate_mesh",
    "search_creator_store",
    "insert_from_creator_store",
    "store_image",
    "generate_procedural_model",
    "wait_job_finished",
];

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
    #[arg(long, required = true)]
    hub_base_url: Option<String>,
    #[arg(long)]
    studio_path: Option<PathBuf>,
    #[arg(long, default_value_t = 4)]
    capacity: usize,
    #[arg(long, default_value_t = false)]
    skip_claim_studio_launch: bool,
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
    skip_claim_studio_launch: bool,
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
    remote_base_url: String,
    studio_pid: Option<u32>,
    studio_mode: Option<String>,
    studio_mode_observed_at: Option<Instant>,
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
    official_adapters: HashMap<String, OfficialAdapterHandle>,
    official_adapter_states: HashMap<String, OfficialAdapterStateRecord>,
    last_remote_errors: HashMap<String, String>,
    hub_last_error: Option<String>,
    hub_last_ready_at: Option<Instant>,
}

#[derive(Debug, Clone)]
struct OfficialAdapterStateRecord {
    state: String,
    last_error: Option<String>,
    observed_at: Instant,
}

#[derive(Clone)]
struct ClaimedTask {
    task_id: String,
    place_id: String,
    universe_id: Option<String>,
    mcp_base_url: String,
    rojo_base_url: Option<String>,
    runtime_log_base_url: Option<String>,
    claimed_at: Instant,
}

struct LaunchProcessRecord {
    task_id: String,
    place_id: String,
    universe_id: Option<String>,
    studio_pid: u32,
    launched_by_helper: bool,
    #[cfg(target_os = "windows")]
    kill_on_close_job: Option<OwnedHandle>,
    child: Option<Child>,
}

struct PendingLaunchTermination {
    task_id: String,
    studio_pid: u32,
    #[cfg(target_os = "windows")]
    _kill_on_close_job: Option<OwnedHandle>,
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

#[derive(Clone)]
struct OfficialAdapterHandle {
    sender: mpsc::UnboundedSender<OfficialAdapterCommand>,
    stop_tx: watch::Sender<bool>,
}

struct OfficialAdapterCommand {
    request: OfficialMcpRequest,
    response_tx: oneshot::Sender<Result<String>>,
}

struct OfficialMcpProcess {
    child: TokioChild,
    stdin: TokioChildStdin,
    stdout_lines: Lines<BufReader<TokioChildStdout>>,
    next_id: u64,
    initialized: bool,
    studio_id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct OfficialToolCallResponse {
    #[serde(default)]
    content: Vec<OfficialContentItem>,
    #[serde(default, rename = "isError")]
    is_error: bool,
}

#[derive(Debug, Deserialize)]
struct OfficialContentItem {
    #[serde(rename = "type")]
    item_type: String,
    text: Option<String>,
}

#[derive(Debug, Deserialize)]
struct OfficialStudioList {
    #[serde(default)]
    note: Option<String>,
    #[serde(default)]
    studios: Vec<OfficialStudioInfo>,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
struct OfficialStudioInfo {
    id: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    active: Option<bool>,
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
    #[serde(default)]
    studio_mode: Option<String>,
}

#[derive(Debug, Serialize)]
struct RegisterPluginResponse {
    instance_id: String,
    place_id: String,
    task_id: Option<String>,
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

#[derive(Debug, Deserialize)]
struct PluginStatusUpdateRequest {
    instance_id: String,
    #[serde(default)]
    studio_mode: Option<String>,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
struct PluginResponsePayload {
    instance_id: Option<String>,
    id: String,
    success: bool,
    response: String,
    #[serde(default)]
    studio_mode: Option<String>,
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
    place_id: String,
    universe_id: Option<String>,
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
    studio_mode: Option<String>,
    studio_mode_age_ms: Option<u128>,
    official_mcp_adapter_state: String,
    official_mcp_adapter_age_ms: Option<u128>,
    official_mcp_adapter_last_error: Option<String>,
}

#[derive(Debug, Serialize)]
struct LaunchProcessStatus {
    task_id: String,
    place_id: String,
    universe_id: Option<String>,
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

#[derive(Debug, Deserialize)]
struct HubErrorResponse {
    #[serde(default)]
    code: Option<String>,
    #[serde(default)]
    message: Option<String>,
}

#[derive(Debug)]
enum RegisterHelperError {
    HelperIdConflict(String),
    Other(color_eyre::Report),
}

impl std::fmt::Display for RegisterHelperError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::HelperIdConflict(message) => write!(f, "helper_id_conflict: {message}"),
            Self::Other(error) => write!(f, "{error}"),
        }
    }
}

impl std::error::Error for RegisterHelperError {}

#[derive(Debug, Serialize)]
struct HelperHeartbeatHubRequest {
    helper_id: String,
    active_task_ids: Vec<String>,
    active_tasks: Vec<HelperHeartbeatTaskStatus>,
}

#[derive(Debug, Serialize)]
struct HelperHeartbeatTaskStatus {
    task_id: String,
    remote_state: String,
    remote_connection_id: Option<String>,
    remote_last_error: Option<String>,
    remote_last_ready_age_ms: Option<u128>,
    remote_last_server_message_age_ms: Option<u128>,
    studio_mode: Option<String>,
    studio_mode_age_ms: Option<u128>,
    official_mcp_adapter_state: String,
    official_mcp_adapter_age_ms: Option<u128>,
    official_mcp_adapter_last_error: Option<String>,
}

#[derive(Debug, Deserialize)]
struct HelperHeartbeatHubResponse {
    ok: bool,
    #[serde(default)]
    release_task_ids: Vec<String>,
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
    task_id: String,
    place_id: String,
    #[serde(default)]
    universe_id: Option<String>,
    #[serde(default)]
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
    remote_base_url: String,
    studio_pid: Option<u32>,
    studio_mode: Option<String>,
    studio_mode_age_ms: Option<u128>,
}

#[derive(Debug, Serialize)]
struct HelperPlaceStatus {
    place_id: String,
    remote_base_url: String,
    task_id: Option<String>,
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
}

#[derive(Debug, Serialize)]
struct RojoConfigResponse {
    place_id: String,
    task_id: Option<String>,
    base_url: String,
    auth_header: String,
}

fn rojo_forward_base_url(helper_port: u16, place_id: &str) -> String {
    format!("http://127.0.0.1:{helper_port}/rojo-forward/{place_id}")
}

fn rojo_forward_target_path(path: &str, query: Option<&str>) -> String {
    let mut target = String::from("/");
    target.push_str(path.trim_start_matches('/'));
    if let Some(query) = query.filter(|value| !value.is_empty()) {
        target.push('?');
        target.push_str(query);
    }
    target
}

fn rojo_forward_target_ws_url(base_url: &str, path: &str, query: Option<&str>) -> Result<String> {
    let ws_base_url = if let Some(rest) = base_url.strip_prefix("https://") {
        format!("wss://{rest}")
    } else if let Some(rest) = base_url.strip_prefix("http://") {
        format!("ws://{rest}")
    } else if base_url.starts_with("wss://") || base_url.starts_with("ws://") {
        base_url.to_owned()
    } else {
        return Err(eyre!("Rojo forward base_url must use http(s) or ws(s)"));
    };
    Ok(format!(
        "{}{}",
        ws_base_url.trim_end_matches('/'),
        rojo_forward_target_path(path, query)
    ))
}

#[derive(Debug, Deserialize)]
struct ScreenshotQuery {
    #[serde(rename = "placeId")]
    place_id: Option<String>,
    #[serde(rename = "taskId")]
    task_id: Option<String>,
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

fn helper_data_dir() -> Result<PathBuf> {
    if let Some(appdata) = windows_appdata_dir() {
        return Ok(appdata.join("dev.clock-p.com").join("studio-helper"));
    }
    if let Some(home) = home_dir() {
        return Ok(home.join(".dev.clock-p.com").join("studio-helper"));
    }
    Err(eyre!("cannot resolve helper data directory"))
}

fn helper_id_from_machine_guid(machine_guid: &str) -> Result<String> {
    let normalized = trim(machine_guid).to_ascii_lowercase();
    if normalized.is_empty() {
        return Err(eyre!("MachineGuid must not be empty"));
    }
    let source = format!("windows-machine-guid:{normalized}");
    let derived = Uuid::new_v5(&Uuid::NAMESPACE_OID, source.as_bytes())
        .simple()
        .to_string();
    sanitize_identifier("helper_id", &format!("h_{}", &derived[..12]))
}

fn parse_machine_guid_reg_query_output(output: &str) -> Result<String> {
    for line in output.lines() {
        let trimmed = trim(line);
        if !trimmed.starts_with("MachineGuid") {
            continue;
        }
        let mut parts = trimmed.split_whitespace();
        let name = parts.next();
        let value_type = parts.next();
        let value = parts.collect::<Vec<_>>().join(" ");
        if name == Some("MachineGuid") && value_type == Some("REG_SZ") && !value.is_empty() {
            return Ok(trim(&value).to_owned());
        }
    }
    Err(eyre!("reg query did not return MachineGuid"))
}

#[cfg(target_os = "windows")]
fn resolve_default_helper_id() -> Result<String> {
    let output = std::process::Command::new("reg")
        .args([
            "query",
            r"HKEY_LOCAL_MACHINE\SOFTWARE\Microsoft\Cryptography",
            "/v",
            "MachineGuid",
        ])
        .output()
        .wrap_err("failed to query Windows MachineGuid with reg.exe")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(eyre!(
            "failed to query MachineGuid: status={}, stderr={}",
            output.status,
            trim(&stderr)
        ));
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let machine_guid = parse_machine_guid_reg_query_output(&stdout)?;
    helper_id_from_machine_guid(&machine_guid)
}

#[cfg(not(target_os = "windows"))]
fn resolve_default_helper_id() -> Result<String> {
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

#[cfg(target_os = "windows")]
fn create_kill_on_close_job() -> Result<OwnedHandle> {
    let job = unsafe { CreateJobObjectW(null_mut(), null_mut()) };
    if job.is_null() {
        return Err(eyre!(
            "failed to create Studio kill-on-close job object: {}",
            std::io::Error::last_os_error()
        ));
    }

    let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
    limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
    let ok = unsafe {
        SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            &limits as *const _ as *const _,
            size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        )
    };
    if ok == 0 {
        let error = std::io::Error::last_os_error();
        unsafe {
            let _ = OwnedHandle::from_raw_handle(job as _);
        }
        return Err(eyre!(
            "failed to configure Studio kill-on-close job object: {error}"
        ));
    }

    Ok(unsafe { OwnedHandle::from_raw_handle(job as _) })
}

#[cfg(target_os = "windows")]
fn assign_child_to_kill_on_close_job(job: &OwnedHandle, child: &Child) -> Result<()> {
    let process_handle = child.as_raw_handle() as HANDLE;
    let ok = unsafe { AssignProcessToJobObject(job.as_raw_handle() as HANDLE, process_handle) };
    if ok == 0 {
        return Err(eyre!(
            "failed to assign helper-launched Studio to kill-on-close job object: {}",
            std::io::Error::last_os_error()
        ));
    }
    Ok(())
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
    if let Some(universe_id) = task.universe_id.as_deref() {
        command.arg("-universeId").arg(universe_id);
    }
    if let Some(parent) = studio_path.parent() {
        command.current_dir(parent);
    }
    let mut child = command
        .spawn()
        .wrap_err_with(|| format!("failed to launch Studio at {}", studio_path.display()))?;
    let kill_on_close_job = match create_kill_on_close_job() {
        Ok(job) => job,
        Err(error) => {
            let _ = child.kill();
            let _ = child.wait();
            return Err(error).wrap_err_with(|| {
                format!(
                    "failed to arm helper-launched Studio for helper-exit cleanup at {}",
                    studio_path.display()
                )
            });
        }
    };
    if let Err(error) = assign_child_to_kill_on_close_job(&kill_on_close_job, &child) {
        let _ = child.kill();
        let _ = child.wait();
        return Err(error).wrap_err_with(|| {
            format!(
                "failed to tie helper-launched Studio to helper lifetime at {}",
                studio_path.display()
            )
        });
    }
    let studio_pid = child.id();
    tracing::info!(
        task_id = task.task_id.as_str(),
        place_id = task.place_id.as_str(),
        universe_id = task.universe_id.as_deref(),
        studio_pid,
        studio_path = %studio_path.display(),
        "launched Roblox Studio for claimed task"
    );
    Ok(LaunchProcessRecord {
        task_id: task.task_id.clone(),
        place_id: task.place_id.clone(),
        universe_id: task.universe_id.clone(),
        studio_pid,
        launched_by_helper: true,
        kill_on_close_job: Some(kill_on_close_job),
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
        return Ok(task.clone());
    }

    let mut matches = state
        .claimed_tasks
        .values()
        .filter(|task| task.place_id == place_id)
        .cloned()
        .collect::<Vec<_>>();
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

fn route_identity_matches(instance: &PluginInstance, task_id: Option<&str>) -> bool {
    if let Some(expected_task_id) = task_id {
        if instance.task_id.as_deref() != Some(expected_task_id) {
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
    studio_pid: Option<u32>,
) -> Result<ClaimedTask> {
    if let Some(studio_pid) = studio_pid {
        if let Some(task) = select_claimed_task_for_pid(state, studio_pid, place_id) {
            return Ok(task);
        }
    }
    if task_id.is_none() {
        return Err(eyre!(
            "helper could not map Studio for place_id {place_id} to a claimed launch; task_id is required unless the Studio pid was launched by helper"
        ));
    }
    select_claimed_task(state, place_id, task_id)
}

fn resolve_claimed_task_for_plugin_request(
    state: &HelperState,
    place_id: &str,
    task_id: Option<&str>,
    studio_pid: Option<u32>,
) -> Result<ClaimedTask> {
    if let Some(studio_pid) = studio_pid {
        if let Some(task) = select_claimed_task_for_pid(state, studio_pid, place_id) {
            return Ok(task);
        }
    }
    select_claimed_task(state, place_id, task_id)
}

fn resolve_plugin_routing_decision(
    state: &HelperState,
    place_id: &str,
    task_id: Option<&str>,
    studio_pid: Option<u32>,
) -> Result<ClaimedTask> {
    #[cfg(target_os = "windows")]
    {
        resolve_claimed_task_for_plugin_request(state, place_id, task_id, studio_pid)
    }

    #[cfg(not(target_os = "windows"))]
    {
        resolve_claimed_task_for_plugin_request(state, place_id, task_id, studio_pid)
    }
}

fn bind_launch_process_to_pid(
    state: &mut HelperState,
    task: &ClaimedTask,
    studio_pid: u32,
) -> Result<()> {
    if let Some(existing) = state.launch_processes.get_mut(&task.task_id) {
        existing.place_id = task.place_id.clone();
        existing.universe_id = task.universe_id.clone();
        existing.studio_pid = studio_pid;
        existing.launched_by_helper = false;
        return Ok(());
    }
    state.launch_processes.insert(
        task.task_id.clone(),
        LaunchProcessRecord {
            task_id: task.task_id.clone(),
            place_id: task.place_id.clone(),
            universe_id: task.universe_id.clone(),
            studio_pid,
            launched_by_helper: false,
            #[cfg(target_os = "windows")]
            kill_on_close_job: None,
            child: None,
        },
    );
    Ok(())
}

fn remove_instances_for_claimed_task(state: &mut HelperState, task_id: &str) {
    state.instances.retain(|instance_id, instance| {
        let keep = !route_identity_matches(instance, Some(task_id));
        if !keep {
            tracing::info!(
                instance_id,
                task_id,
                "dropping plugin instance for released claimed task"
            );
        }
        keep
    });
}

fn release_claimed_task(
    state: &mut HelperState,
    task_id: &str,
    terminate_helper_spawned: bool,
    reason: &str,
) -> Option<PendingLaunchTermination> {
    let should_remove = state.claimed_tasks.contains_key(task_id);
    if !should_remove {
        return None;
    }

    let remove_launch_process = state.launch_processes.contains_key(task_id);
    let pending_termination = if remove_launch_process {
        state
            .launch_processes
            .remove(task_id)
            .and_then(|mut launch| {
                if terminate_helper_spawned && launch.launched_by_helper {
                    launch.child.take().map(|child| PendingLaunchTermination {
                        task_id: launch.task_id,
                        studio_pid: launch.studio_pid,
                        #[cfg(target_os = "windows")]
                        _kill_on_close_job: launch.kill_on_close_job.take(),
                        child,
                    })
                } else {
                    None
                }
            })
    } else {
        None
    };

    remove_instances_for_claimed_task(state, task_id);
    state.claimed_tasks.remove(task_id);
    stop_removed_official_adapter(state, task_id);
    state
        .last_remote_errors
        .remove(&task_connection_key(task_id));
    tracing::info!(task_id, reason, "released claimed task from helper state");
    pending_termination
}

fn release_all_claimed_tasks(
    state: &mut HelperState,
    terminate_helper_spawned: bool,
    reason: &str,
) -> Vec<PendingLaunchTermination> {
    let mut task_ids = state.claimed_tasks.keys().cloned().collect::<Vec<_>>();
    task_ids.sort();
    let mut pending_terminations = Vec::new();
    for task_id in task_ids {
        if let Some(pending_termination) =
            release_claimed_task(state, &task_id, terminate_helper_spawned, reason)
        {
            pending_terminations.push(pending_termination);
        }
    }
    pending_terminations
}

async fn terminate_pending_launch_termination(pending: PendingLaunchTermination) {
    let PendingLaunchTermination {
        task_id,
        studio_pid,
        #[cfg(target_os = "windows")]
        _kill_on_close_job,
        mut child,
    } = pending;
    let task_id_for_join = task_id.clone();
    match tokio::task::spawn_blocking(move || {
        if let Err(error) = child.kill() {
            tracing::warn!(
                task_id,
                studio_pid,
                error = summarize_error(&error.to_string()),
                "failed to terminate helper-launched Studio during claim release"
            );
            return;
        }
        tracing::info!(
            task_id,
            studio_pid,
            "terminated helper-launched Studio during claim release"
        );
        if let Err(error) = child.wait() {
            tracing::warn!(
                task_id,
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
            Ok(Some(status)) => {
                exited.push((task_id.clone(), launch.studio_pid, status.to_string()))
            }
            Ok(None) => {}
            Err(error) => {
                tracing::warn!(
                    task_id,
                    studio_pid = launch.studio_pid,
                    error = summarize_error(&error.to_string()),
                    "failed to poll helper-launched Studio process"
                );
            }
        }
    }

    for (task_id, studio_pid, status) in exited {
        tracing::warn!(
            task_id,
            studio_pid,
            exit_status = status,
            "helper-launched Studio exited"
        );
        let _ = release_claimed_task(state, &task_id, false, "helper-launched Studio exited");
    }
}

fn resolve_helper_id(args: &Args) -> Result<String> {
    if let Some(value) = args.helper_id.as_deref() {
        return sanitize_identifier("helper_id", value);
    }
    resolve_default_helper_id()
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

fn parse_helper_id_conflict(
    status: StatusCode,
    error_code: Option<&str>,
    body: &str,
) -> Option<String> {
    if status != StatusCode::CONFLICT {
        return None;
    }
    if error_code == Some("helper_id_conflict") {
        return Some(trim(body).to_owned());
    }
    let parsed: HubErrorResponse = serde_json::from_str(body).ok()?;
    if parsed.code.as_deref() != Some("helper_id_conflict") {
        return None;
    }
    Some(
        parsed
            .message
            .unwrap_or_else(|| "helper_id already active".to_owned()),
    )
}

async fn hub_register_helper(
    helper: &HelperConfig,
) -> std::result::Result<RegisterHelperHubResponse, RegisterHelperError> {
    let Some(base_url) = helper.hub_base_url.as_ref() else {
        return Err(RegisterHelperError::Other(eyre!(
            "hub_base_url is not configured"
        )));
    };
    let token = helper.bearer_token.lock().await.clone();
    let response = helper
        .client
        .post(format!(
            "{}/v1/helpers/register",
            base_url.trim_end_matches('/')
        ))
        .bearer_auth(token)
        .json(&RegisterHelperHubRequest {
            helper_id: helper.helper_id.clone(),
            owner_user: helper.user_name.clone(),
            platform: std::env::consts::OS.to_owned(),
            capacity: helper.capacity,
            labels: vec![std::env::consts::ARCH.to_owned()],
        })
        .send()
        .await
        .map_err(|error| RegisterHelperError::Other(error.into()))?;
    let status = response.status();
    let error_code = response
        .headers()
        .get("x-clock-p-hub-error")
        .and_then(|value| value.to_str().ok())
        .map(ToOwned::to_owned);
    let body = response
        .text()
        .await
        .map_err(|error| RegisterHelperError::Other(error.into()))?;
    if let Some(message) = parse_helper_id_conflict(status, error_code.as_deref(), &body) {
        return Err(RegisterHelperError::HelperIdConflict(message));
    }
    if !status.is_success() {
        return Err(RegisterHelperError::Other(eyre!(
            "hub request /v1/helpers/register failed with {status}: {body}"
        )));
    }
    serde_json::from_str(&body).map_err(|error| RegisterHelperError::Other(error.into()))
}

async fn hub_helper_heartbeat(
    helper: &HelperConfig,
    active_task_ids: Vec<String>,
    active_tasks: Vec<HelperHeartbeatTaskStatus>,
) -> Result<HelperHeartbeatHubResponse> {
    hub_post_json(
        helper,
        "/v1/helpers/heartbeat",
        &HelperHeartbeatHubRequest {
            helper_id: helper.helper_id.clone(),
            active_task_ids,
            active_tasks,
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
    state: &HelperState,
    helper: &HelperConfig,
) -> Result<String> {
    let trimmed = trim(upload_url);
    if trimmed.is_empty() {
        return Err(eyre!("upload_url must not be empty"));
    }

    let _ = helper;
    let expected = select_claimed_task(state, place_id, task_id)?
        .runtime_log_base_url
        .as_deref()
        .map(runtime_screenshot_upload_url_for_base_url)
        .ok_or_else(|| eyre!("claimed task has no runtime_log_base_url"))?;
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
            remote_base_url: instance.remote_base_url.clone(),
            studio_pid: instance.studio_pid,
            studio_mode: instance.studio_mode.clone(),
            studio_mode_age_ms: instance
                .studio_mode_observed_at
                .map(|value| value.elapsed().as_millis()),
        })
        .collect();
    instances.sort_by(|left, right| left.instance_id.cmp(&right.instance_id));
    let mut claimed_tasks: Vec<_> = state
        .claimed_tasks
        .values()
        .map(|task| {
            let connection_key = task_connection_key(&task.task_id);
            let remote_connection = state.remote_connections.get(&connection_key);
            let (studio_mode, studio_mode_age_ms) =
                task_studio_mode_snapshot(&state, &task.task_id);
            let (
                official_mcp_adapter_state,
                official_mcp_adapter_age_ms,
                official_mcp_adapter_last_error,
            ) = task_official_adapter_snapshot(&state, &task.task_id, studio_mode.as_deref());
            let launch_process = state.launch_processes.get(&task.task_id);
            let studio_pid = launch_process.map(|launch| launch.studio_pid).or_else(|| {
                state
                    .instances
                    .values()
                    .find(|instance| route_identity_matches(instance, Some(task.task_id.as_str())))
                    .and_then(|instance| instance.studio_pid)
            });
            ClaimedTaskStatus {
                task_id: task.task_id.clone(),
                place_id: task.place_id.clone(),
                universe_id: task.universe_id.clone(),
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
                studio_mode,
                studio_mode_age_ms,
                official_mcp_adapter_state,
                official_mcp_adapter_age_ms,
                official_mcp_adapter_last_error,
            }
        })
        .collect();
    claimed_tasks.sort_by(|left, right| left.task_id.cmp(&right.task_id));
    let mut launched_studios: Vec<_> = state
        .launch_processes
        .values()
        .map(|launch| LaunchProcessStatus {
            task_id: launch.task_id.clone(),
            place_id: launch.place_id.clone(),
            universe_id: launch.universe_id.clone(),
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
            if let Some(launch) = state.launch_processes.get(&claimed_task.task_id) {
                studio_pids.push(launch.studio_pid);
            }
        }
        registered_instance_ids.sort();
        studio_pids.sort();
        studio_pids.dedup();
        let connection_key = claimed_task
            .as_ref()
            .map(|task| task_connection_key(&task.task_id));
        let remote_connection = connection_key
            .as_ref()
            .and_then(|key| state.remote_connections.get(key));
        place_statuses.push(HelperPlaceStatus {
            remote_base_url: claimed_task
                .as_ref()
                .map(|task| task.mcp_base_url.clone())
                .unwrap_or_default(),
            place_id: place_id.clone(),
            task_id: claimed_task.as_ref().map(|task| task.task_id.clone()),
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
            remote_last_error: connection_key
                .as_ref()
                .and_then(|key| state.last_remote_errors.get(key).cloned()),
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
    let bearer_token = app.helper.bearer_token.lock().await.clone();
    let studio_pid = resolve_peer_process_id_with_retry(peer_addr, app.helper.port).await?;
    let claimed_task = {
        let state = app.state.lock().await;
        resolve_plugin_routing_decision(&state, &place_id, explicit_task_id.as_deref(), studio_pid)
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
    let local_base_url = rojo_forward_base_url(app.helper.port, &place_id);
    tracing::info!(
        place_id,
        task_id = claimed_task.task_id,
        remote_base_url = base_url,
        base_url = local_base_url,
        "resolved rojo config from helper"
    );
    Ok(Json(RojoConfigResponse {
        place_id,
        task_id: Some(claimed_task.task_id),
        base_url: local_base_url,
        auth_header: format!("Bearer {bearer_token}"),
    }))
}

async fn resolve_rojo_forward_target_base_url(app: &AppState, place_id: &str) -> Result<String> {
    let state = app.state.lock().await;
    let claimed_task = select_claimed_task(&state, place_id, None)?;
    claimed_task
        .rojo_base_url
        .ok_or_else(|| eyre!("claimed task has no rojo_base_url"))
}

async fn rojo_forward_root_handler(
    State(app): State<AppState>,
    AxumPath(place_id): AxumPath<String>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, HelperError> {
    rojo_forward_request(app, place_id, String::new(), method, uri, headers, body).await
}

async fn rojo_forward_path_handler(
    State(app): State<AppState>,
    AxumPath((place_id, path)): AxumPath<(String, String)>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, HelperError> {
    rojo_forward_request(app, place_id, path, method, uri, headers, body).await
}

async fn rojo_forward_socket_handler(
    State(app): State<AppState>,
    AxumPath((place_id, cursor)): AxumPath<(String, String)>,
    uri: Uri,
    ws: WebSocketUpgrade,
) -> Result<Response, HelperError> {
    let place_id = sanitize_place_id(&place_id)?;
    let cursor = sanitize_identifier("cursor", &cursor)?;
    let target_base_url = resolve_rojo_forward_target_base_url(&app, &place_id).await?;
    let target_url = rojo_forward_target_ws_url(
        &target_base_url,
        &format!("api/socket/{cursor}"),
        uri.query(),
    )?;
    Ok(ws
        .on_upgrade(move |socket| rojo_forward_socket_session(app, place_id, target_url, socket))
        .into_response())
}

async fn rojo_forward_socket_session(
    app: AppState,
    place_id: String,
    target_url: String,
    client_socket: WebSocket,
) {
    if let Err(error) =
        rojo_forward_socket_session_result(app, &place_id, &target_url, client_socket).await
    {
        tracing::warn!(
            place_id,
            target_url,
            error = %error,
            "Rojo websocket forward session ended with error"
        );
    }
}

async fn rojo_forward_socket_session_result(
    app: AppState,
    place_id: &str,
    target_url: &str,
    client_socket: WebSocket,
) -> Result<()> {
    let remote_socket = connect_remote_ws(&app.helper, target_url)
        .await
        .wrap_err_with(|| format!("failed to connect Rojo websocket {target_url}"))?;
    let (mut client_sender, mut client_receiver) = client_socket.split();
    let (mut remote_sender, mut remote_receiver) = remote_socket.split();
    tracing::info!(
        place_id,
        target_url,
        "opened Rojo websocket forward session"
    );
    loop {
        tokio::select! {
            client_message = client_receiver.next() => {
                match client_message {
                    Some(Ok(message)) => {
                        let close = matches!(message, AxumWsMessage::Close(_));
                        remote_sender
                            .send(rojo_client_ws_to_remote(message))
                            .await
                            .wrap_err("failed to send Rojo websocket message to remote")?;
                        if close {
                            break;
                        }
                    }
                    Some(Err(error)) => {
                        return Err(eyre!("failed to read Rojo websocket message from client: {error}"));
                    }
                    None => break,
                }
            }
            remote_message = remote_receiver.next() => {
                match remote_message {
                    Some(Ok(message)) => {
                        let close = matches!(message, WsMessage::Close(_));
                        if let Some(client_message) = rojo_remote_ws_to_client(message) {
                            client_sender
                                .send(client_message)
                                .await
                                .wrap_err("failed to send Rojo websocket message to client")?;
                        }
                        if close {
                            break;
                        }
                    }
                    Some(Err(error)) => {
                        return Err(eyre!("failed to read Rojo websocket message from remote: {error}"));
                    }
                    None => break,
                }
            }
        }
    }
    tracing::info!(
        place_id,
        target_url,
        "closed Rojo websocket forward session"
    );
    Ok(())
}

fn rojo_client_ws_to_remote(message: AxumWsMessage) -> WsMessage {
    match message {
        AxumWsMessage::Text(text) => WsMessage::Text(text.to_string()),
        AxumWsMessage::Binary(bytes) => WsMessage::Binary(bytes.to_vec()),
        AxumWsMessage::Ping(bytes) => WsMessage::Ping(bytes.to_vec()),
        AxumWsMessage::Pong(bytes) => WsMessage::Pong(bytes.to_vec()),
        AxumWsMessage::Close(_) => WsMessage::Close(None),
    }
}

fn rojo_remote_ws_to_client(message: WsMessage) -> Option<AxumWsMessage> {
    match message {
        WsMessage::Text(text) => Some(AxumWsMessage::Text(text.into())),
        WsMessage::Binary(bytes) => Some(AxumWsMessage::Binary(bytes.into())),
        WsMessage::Ping(bytes) => Some(AxumWsMessage::Ping(bytes.into())),
        WsMessage::Pong(bytes) => Some(AxumWsMessage::Pong(bytes.into())),
        WsMessage::Close(_) => Some(AxumWsMessage::Close(None)),
        WsMessage::Frame(_) => None,
    }
}

async fn rojo_forward_request(
    app: AppState,
    place_id: String,
    path: String,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, HelperError> {
    let place_id = sanitize_place_id(&place_id)?;
    let target_base_url = resolve_rojo_forward_target_base_url(&app, &place_id).await?;
    let target_path = rojo_forward_target_path(&path, uri.query());
    let target_url = format!("{}{}", target_base_url.trim_end_matches('/'), target_path);
    let bearer_token = app.helper.bearer_token.lock().await.clone();
    let reqwest_method = reqwest::Method::from_bytes(method.as_str().as_bytes())
        .wrap_err_with(|| format!("unsupported Rojo forward method {method}"))?;
    let mut request = app
        .helper
        .client
        .request(reqwest_method, &target_url)
        .bearer_auth(bearer_token);
    if let Some(content_type) = headers
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
    {
        request = request.header(CONTENT_TYPE, content_type);
    }
    if !body.is_empty() {
        request = request.body(body.to_vec());
    }
    let response = request
        .send()
        .await
        .wrap_err_with(|| format!("failed to forward Rojo request to {target_url}"))?;
    let status =
        StatusCode::from_u16(response.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    let content_type = response
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .map(ToOwned::to_owned);
    let bytes = response
        .bytes()
        .await
        .wrap_err("failed to read Rojo forward response body")?;
    tracing::info!(
        place_id,
        method = method.as_str(),
        target_path,
        status = status.as_u16(),
        bytes = bytes.len(),
        "forwarded Rojo request through helper"
    );
    let mut builder = Response::builder().status(status);
    if let Some(content_type) = content_type {
        builder = builder.header(CONTENT_TYPE, content_type);
    }
    builder
        .body(Body::from(bytes))
        .wrap_err("failed to build Rojo forward response")
        .map_err(HelperError)
}

async fn mcp_register_handler(
    State(app): State<AppState>,
    ConnectInfo(peer_addr): ConnectInfo<SocketAddr>,
    Json(payload): Json<RegisterPluginRequest>,
) -> Result<Json<RegisterPluginResponse>, HelperError> {
    let place_id = sanitize_place_id(&payload.place_id)?;
    let instance_id = Uuid::new_v4().to_string();
    let explicit_task_id = maybe_sanitize_identifier("task_id", payload.task_id.as_deref())?;
    let studio_pid = resolve_peer_process_id_with_retry(peer_addr, app.helper.port).await?;
    let claimed_task = {
        let state = app.state.lock().await;
        resolve_plugin_routing_decision(&state, &place_id, explicit_task_id.as_deref(), studio_pid)
            .map_err(HelperError)?
    };
    let remote_base_url = claimed_task.mcp_base_url.clone();
    {
        let mut state = app.state.lock().await;
        state.instances.insert(
            instance_id.clone(),
            PluginInstance {
                place_id: place_id.clone(),
                task_id: Some(claimed_task.task_id.clone()),
                remote_base_url: remote_base_url.clone(),
                studio_pid,
                studio_mode: normalize_studio_mode(payload.studio_mode.as_deref()),
                studio_mode_observed_at: normalize_studio_mode(payload.studio_mode.as_deref())
                    .map(|_| Instant::now()),
                last_seen_at: Instant::now(),
                queue: VecDeque::new(),
                notify: Arc::new(Notify::new()),
            },
        );
        if let Some(studio_pid) = studio_pid {
            bind_launch_process_to_pid(&mut state, &claimed_task, studio_pid)
                .map_err(HelperError)?;
        }
        sync_remote_connections(&app, &mut state);
    }
    tracing::info!(
        instance_id,
        place_id,
        task_id = claimed_task.task_id,
        studio_pid,
        remote_base_url,
        "registered MCP plugin instance with helper"
    );
    Ok(Json(RegisterPluginResponse {
        instance_id,
        place_id,
        task_id: Some(claimed_task.task_id),
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

async fn mcp_plugin_status_update_handler(
    State(app): State<AppState>,
    Json(payload): Json<PluginStatusUpdateRequest>,
) -> Result<Response, HelperError> {
    let mut state = app.state.lock().await;
    let updated = update_instance_studio_mode(
        &mut state,
        &payload.instance_id,
        payload.studio_mode.as_deref(),
    );
    if !updated {
        return Ok((StatusCode::GONE, "instance expired or studio_mode invalid").into_response());
    }
    Ok(StatusCode::NO_CONTENT.into_response())
}

async fn mcp_plugin_response_handler(
    State(app): State<AppState>,
    Json(payload): Json<PluginResponsePayload>,
) -> Result<Response, HelperError> {
    let tx = {
        let mut state = app.state.lock().await;
        if let Some(instance_id) = payload.instance_id.as_deref() {
            update_instance_studio_mode(&mut state, instance_id, payload.studio_mode.as_deref());
        }
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
    let path = take_screenshot(app, place_id.as_deref(), task_id.as_deref()).await?;
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

fn task_connection_key(task_id: &str) -> String {
    format!("task:{task_id}")
}

fn normalize_studio_mode(value: Option<&str>) -> Option<String> {
    match value.map(str::trim) {
        Some("stop") => Some("stop".to_owned()),
        Some("start_play") => Some("start_play".to_owned()),
        Some("run_server") => Some("run_server".to_owned()),
        Some("unknown") => Some("unknown".to_owned()),
        _ => None,
    }
}

fn update_instance_studio_mode(
    state: &mut HelperState,
    instance_id: &str,
    studio_mode: Option<&str>,
) -> bool {
    let Some(mode) = normalize_studio_mode(studio_mode) else {
        return false;
    };
    let Some(instance) = state.instances.get_mut(instance_id) else {
        return false;
    };
    instance.studio_mode = Some(mode);
    instance.studio_mode_observed_at = Some(Instant::now());
    instance.last_seen_at = Instant::now();
    true
}

fn task_studio_mode_snapshot(state: &HelperState, task_id: &str) -> (Option<String>, Option<u128>) {
    state
        .instances
        .values()
        .filter(|instance| instance.task_id.as_deref() == Some(task_id))
        .filter_map(|instance| {
            let observed_at = instance.studio_mode_observed_at?;
            Some((
                instance.studio_mode.clone(),
                observed_at.elapsed().as_millis(),
            ))
        })
        .min_by_key(|(_, age)| *age)
        .and_then(|(mode, age)| mode.map(|mode| (Some(mode), Some(age))))
        .unwrap_or((None, None))
}

fn task_official_adapter_snapshot(
    state: &HelperState,
    task_id: &str,
    studio_mode: Option<&str>,
) -> (String, Option<u128>, Option<String>) {
    let Some(record) = state.official_adapter_states.get(task_id) else {
        return ("not_started".to_owned(), None, None);
    };
    let state_name = if record.state == "ready" {
        match studio_mode {
            Some("stop") => "ready".to_owned(),
            Some(_) => "blocked_by_studio_mode".to_owned(),
            None => "stale".to_owned(),
        }
    } else {
        record.state.clone()
    };
    (
        state_name,
        Some(record.observed_at.elapsed().as_millis()),
        record.last_error.clone(),
    )
}

fn helper_task_status_snapshot(state: &HelperState, task_id: &str) -> HelperTaskStatusSnapshot {
    let (studio_mode, studio_mode_age_ms) = task_studio_mode_snapshot(state, task_id);
    let (official_state, official_age_ms, official_last_error) =
        task_official_adapter_snapshot(state, task_id, studio_mode.as_deref());
    HelperTaskStatusSnapshot {
        task_id: task_id.to_owned(),
        studio_mode,
        studio_mode_age_ms,
        official_mcp_adapter_state: Some(official_state),
        official_mcp_adapter_age_ms: official_age_ms,
        official_mcp_adapter_last_error: official_last_error,
    }
}

fn remote_connection_state_name(
    remote_connection: Option<&RemoteConnectionHandle>,
) -> &'static str {
    remote_connection
        .map(|connection| match connection.connection_state {
            RemoteConnectionState::Connecting => "connecting",
            RemoteConnectionState::Connected => "connected",
            RemoteConnectionState::Retrying => "retrying",
        })
        .unwrap_or("disconnected")
}

fn heartbeat_task_statuses(state: &HelperState) -> Vec<HelperHeartbeatTaskStatus> {
    let mut active_tasks = state
        .claimed_tasks
        .values()
        .map(|task| {
            let connection_key = task_connection_key(&task.task_id);
            let remote_connection = state.remote_connections.get(&connection_key);
            let (studio_mode, studio_mode_age_ms) = task_studio_mode_snapshot(state, &task.task_id);
            let (
                official_mcp_adapter_state,
                official_mcp_adapter_age_ms,
                official_mcp_adapter_last_error,
            ) = task_official_adapter_snapshot(state, &task.task_id, studio_mode.as_deref());
            HelperHeartbeatTaskStatus {
                task_id: task.task_id.clone(),
                remote_state: remote_connection_state_name(remote_connection).to_owned(),
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
                studio_mode,
                studio_mode_age_ms,
                official_mcp_adapter_state,
                official_mcp_adapter_age_ms,
                official_mcp_adapter_last_error,
            }
        })
        .collect::<Vec<_>>();
    active_tasks.sort_by(|left, right| left.task_id.cmp(&right.task_id));
    active_tasks
}

fn validate_remote_ready_ack(
    expected_place_id: &str,
    expected_task_id: Option<&str>,
    ack_place_id: &str,
    ack_task_id: Option<&str>,
) -> Result<()> {
    if ack_place_id != expected_place_id {
        return Err(eyre!(
            "remote MCP ready ack place_id mismatch: expected {}, got {}",
            expected_place_id,
            ack_place_id,
        ));
    }
    if ack_task_id != expected_task_id {
        return Err(eyre!(
            "remote MCP ready ack task_id mismatch: expected {:?}, got {:?}",
            expected_task_id,
            ack_task_id,
        ));
    }
    Ok(())
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
            connection_key: task_connection_key(&claimed_task.task_id),
            place_id: claimed_task.place_id.clone(),
            task_id: Some(claimed_task.task_id.clone()),
            remote_base_url: claimed_task.mcp_base_url.clone(),
        });
    }
    let _ = app;
    targets
}

fn remote_connection_is_active(state: &HelperState, connection_key: &str) -> bool {
    state
        .claimed_tasks
        .values()
        .any(|task| task_connection_key(&task.task_id) == connection_key)
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

async fn hub_maintenance_loop(app: AppState, mut registered: bool) {
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
                Err(RegisterHelperError::HelperIdConflict(message)) => {
                    let mut state = app.state.lock().await;
                    state.hub_last_error = Some(format!("helper_id_conflict: {message}"));
                    continue;
                }
                Err(RegisterHelperError::Other(error)) => {
                    let mut state = app.state.lock().await;
                    state.hub_last_error = Some(summarize_error(&error.to_string()));
                    continue;
                }
            }
        }

        let (active_task_ids, active_tasks) = {
            let state = app.state.lock().await;
            let mut active_task_ids = state
                .claimed_tasks
                .values()
                .map(|task| task.task_id.clone())
                .collect::<Vec<_>>();
            active_task_ids.sort();
            let active_tasks = heartbeat_task_statuses(&state);
            (active_task_ids, active_tasks)
        };

        match hub_helper_heartbeat(&app.helper, active_task_ids, active_tasks).await {
            Ok(response) => {
                let pending_terminations = {
                    let mut state = app.state.lock().await;
                    let mut pending_terminations = Vec::new();
                    if response.ok {
                        state.hub_last_error = None;
                        state.hub_last_ready_at = Some(Instant::now());
                    } else {
                        state.hub_last_error = Some("hub rejected helper heartbeat".to_owned());
                        state.hub_last_ready_at = None;
                        pending_terminations.extend(release_all_claimed_tasks(
                            &mut state,
                            true,
                            "hub rejected helper heartbeat",
                        ));
                    }
                    for task_id in response.release_task_ids {
                        if let Some(pending_termination) = release_claimed_task(
                            &mut state,
                            &task_id,
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
                        place_id: task.place_id,
                        universe_id: task.universe_id.or(task.game_id),
                        mcp_base_url,
                        rojo_base_url: task.routes.rojo_base_url,
                        runtime_log_base_url: task.routes.runtime_log_base_url,
                        claimed_at: Instant::now(),
                    };
                    let launch_record: Option<LaunchProcessRecord> =
                        if app.helper.skip_claim_studio_launch {
                            tracing::warn!(
                                task_id = claimed_task.task_id.as_str(),
                                place_id = claimed_task.place_id.as_str(),
                                "skipping Studio launch for claimed task"
                            );
                            None
                        } else {
                            #[cfg(target_os = "windows")]
                            {
                                match launch_studio_for_claim(&app.helper, &claimed_task) {
                                    Ok(record) => Some(record),
                                    Err(error) => {
                                        let mut state = app.state.lock().await;
                                        state.hub_last_error =
                                            Some(summarize_error(&error.to_string()));
                                        continue;
                                    }
                                }
                            }
                            #[cfg(not(target_os = "windows"))]
                            {
                                None
                            }
                        };
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
    let captured = capture_screenshot_png(app, Some(place_id), task_id).await?;
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
                helper_version: env!("CARGO_PKG_VERSION").to_owned(),
                capabilities: vec![
                    "ws_tool_dispatch_v1".to_owned(),
                    "runtime_screenshot_stream_v1".to_owned(),
                    "read_studio_log_v1".to_owned(),
                    OFFICIAL_MCP_ADAPTER_CAPABILITY.to_owned(),
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
                task_status: {
                    let state = app.state.lock().await;
                    task_id.map(|task_id| helper_task_status_snapshot(&state, task_id))
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
    let mut ready_acknowledged = false;
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
                let task_status = {
                    let state = app.state.lock().await;
                    task_id.map(|task_id| helper_task_status_snapshot(&state, task_id))
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
                    plugin_instance_count,
                    task_status,
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
                            ServerToHelperMessage::ReadyAck { connection_id, place_id: ack_place_id, task_id: ack_task_id } => {
                                validate_remote_ready_ack(place_id, task_id, &ack_place_id, ack_task_id.as_deref())?;
                                {
                                    let mut state = app.state.lock().await;
                                    set_remote_connection_connected(&mut state, connection_key, &connection_id);
                                }
                                ready_acknowledged = true;
                                if let Some(task_id) = task_id {
                                    start_official_adapter_prewarm(app.clone(), task_id.to_owned(), place_id.to_owned());
                                }
                                tracing::info!(connection_key, place_id, connection_id, "helper websocket acknowledged by MCP server");
                            }
                            ServerToHelperMessage::ToolCall { request_id, command } => {
                                if !ready_acknowledged {
                                    return Err(eyre!("remote MCP server sent tool call before ready ack"));
                                }
                                let tool_sender = out_tx.clone();
                                let tool_waiters = Arc::clone(&upload_waiters);
                                let tool_app = app.clone();
                                let tool_place = place_id.to_owned();
                                let tool_task = task_id.map(ToOwned::to_owned);
                                tokio::spawn(async move {
                                    let result = handle_remote_command(
                                        tool_app,
                                        &tool_place,
                                        tool_task.as_deref(),
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
                            ServerToHelperMessage::OfficialMcpRequest(request) => {
                                if !ready_acknowledged {
                                    return Err(eyre!("remote MCP server sent official MCP request before ready ack"));
                                }
                                let official_sender = out_tx.clone();
                                let official_app = app.clone();
                                let expected_place_id = place_id.to_owned();
                                let expected_task_id = task_id.map(ToOwned::to_owned);
                                tokio::spawn(async move {
                                    let request_id = request.request_id.clone();
                                    let result = handle_official_mcp_request(
                                        official_app,
                                        &expected_place_id,
                                        expected_task_id.as_deref(),
                                        request,
                                    ).await;
                                    let message = match result {
                                        Ok(response) => HelperToServerMessage::OfficialMcpResponse(OfficialMcpResponse {
                                            request_id,
                                            response,
                                        }),
                                        Err(error) => HelperToServerMessage::OfficialMcpError {
                                            request_id,
                                            error: error.to_string(),
                                        },
                                    };
                                    if let Ok(encoded) = encode_remote_message(&message) {
                                        let _ = official_sender.send(RemoteOutgoingFrame::Text(encoded));
                                    }
                                });
                            }
                            ServerToHelperMessage::ArtifactCommitted(committed) => {
                                if !ready_acknowledged {
                                    return Err(eyre!("remote MCP server sent artifact committed before ready ack"));
                                }
                                if let Some(tx) = upload_waiters.lock().await.remove(&committed.upload_id) {
                                    let _ = tx.send(Ok(committed));
                                }
                            }
                            ServerToHelperMessage::ArtifactFailed { upload_id, error, .. } => {
                                if !ready_acknowledged {
                                    return Err(eyre!("remote MCP server sent artifact failed before ready ack"));
                                }
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
    if let Some(task_id) = task_id {
        if stop_and_remove_official_adapter(&app.state, task_id).await {
            tracing::info!(
                task_id,
                "stopped official MCP adapter after remote websocket ended"
            );
        }
    }
    if stopped_by_request {
        Ok(())
    } else {
        Err(eyre!("remote websocket session ended unexpectedly"))
    }
}

async fn handle_official_mcp_request(
    app: AppState,
    expected_place_id: &str,
    expected_task_id: Option<&str>,
    request: OfficialMcpRequest,
) -> Result<String> {
    let expected_task_id = expected_task_id
        .ok_or_else(|| eyre!("official MCP request requires task_id-bound remote connection"))?;
    if request.task_id != expected_task_id {
        return Err(eyre!(
            "official MCP request task_id mismatch: expected {}, got {}",
            expected_task_id,
            request.task_id
        ));
    }
    if request.place_id != expected_place_id {
        return Err(eyre!(
            "official MCP request place_id mismatch: expected {}, got {}",
            expected_place_id,
            request.place_id
        ));
    }
    let timeout = Duration::from_millis(request.timeout_ms.max(1));
    let timeout = timeout.min(OFFICIAL_MCP_REQUEST_TIMEOUT);
    let handle = ensure_official_adapter_handle(&app, &request.task_id, &request.place_id).await?;
    let (response_tx, response_rx) = oneshot::channel();
    let task_id = request.task_id.clone();
    if handle
        .sender
        .send(OfficialAdapterCommand {
            request,
            response_tx,
        })
        .is_err()
    {
        stop_and_remove_official_adapter(&app.state, &task_id).await;
        return Err(eyre!("official MCP adapter worker is not running"));
    }
    match tokio::time::timeout(timeout, response_rx).await {
        Ok(Ok(result)) => result,
        Ok(Err(_)) => {
            stop_and_remove_official_adapter(&app.state, &task_id).await;
            Err(eyre!("official MCP adapter response channel closed"))
        }
        Err(_) => {
            stop_and_remove_official_adapter(&app.state, &task_id).await;
            Err(eyre!("official MCP adapter request timed out"))
        }
    }
}

async fn set_official_adapter_state(
    state: &SharedState,
    task_id: &str,
    status: &str,
    last_error: Option<String>,
) {
    let mut state = state.lock().await;
    state.official_adapter_states.insert(
        task_id.to_owned(),
        OfficialAdapterStateRecord {
            state: status.to_owned(),
            last_error,
            observed_at: Instant::now(),
        },
    );
}

fn start_official_adapter_prewarm(app: AppState, task_id: String, place_id: String) {
    tokio::spawn(async move {
        set_official_adapter_state(&app.state, &task_id, "starting", None).await;
        let handle = match ensure_official_adapter_handle(&app, &task_id, &place_id).await {
            Ok(handle) => handle,
            Err(error) => {
                set_official_adapter_state(
                    &app.state,
                    &task_id,
                    "error",
                    Some(summarize_error(&error.to_string())),
                )
                .await;
                return;
            }
        };
        let request = OfficialMcpRequest {
            request_id: Uuid::new_v4().to_string(),
            task_id: task_id.clone(),
            place_id: place_id.clone(),
            action: "ping".to_owned(),
            arguments: Value::Null,
            timeout_ms: 30_000,
        };
        let (response_tx, response_rx) = oneshot::channel();
        if handle
            .sender
            .send(OfficialAdapterCommand {
                request,
                response_tx,
            })
            .is_err()
        {
            set_official_adapter_state(
                &app.state,
                &task_id,
                "error",
                Some("official MCP adapter worker is not running".to_owned()),
            )
            .await;
            return;
        }
        match tokio::time::timeout(Duration::from_secs(35), response_rx).await {
            Ok(Ok(Ok(_))) => set_official_adapter_state(&app.state, &task_id, "ready", None).await,
            Ok(Ok(Err(error))) => {
                set_official_adapter_state(
                    &app.state,
                    &task_id,
                    "error",
                    Some(summarize_error(&error.to_string())),
                )
                .await
            }
            Ok(Err(_)) => {
                set_official_adapter_state(
                    &app.state,
                    &task_id,
                    "error",
                    Some("official MCP adapter response channel closed".to_owned()),
                )
                .await
            }
            Err(_) => {
                set_official_adapter_state(
                    &app.state,
                    &task_id,
                    "error",
                    Some("official MCP adapter prewarm timed out".to_owned()),
                )
                .await
            }
        }
    });
}

fn stop_removed_official_adapter(state: &mut HelperState, task_id: &str) -> bool {
    state.official_adapter_states.remove(task_id);
    if let Some(adapter) = state.official_adapters.remove(task_id) {
        let _ = adapter.stop_tx.send(true);
        true
    } else {
        false
    }
}

async fn stop_and_remove_official_adapter(state: &SharedState, task_id: &str) -> bool {
    let adapter = {
        let mut state = state.lock().await;
        state.official_adapter_states.remove(task_id);
        state.official_adapters.remove(task_id)
    };
    if let Some(adapter) = adapter {
        let _ = adapter.stop_tx.send(true);
        true
    } else {
        false
    }
}

async fn ensure_official_adapter_handle(
    app: &AppState,
    task_id: &str,
    place_id: &str,
) -> Result<OfficialAdapterHandle> {
    let mut state = app.state.lock().await;
    let claimed_task = state
        .claimed_tasks
        .get(task_id)
        .ok_or_else(|| eyre!("helper has not claimed task_id {task_id}"))?;
    if claimed_task.place_id != place_id {
        return Err(eyre!(
            "claimed task place_id mismatch for task_id {}: expected {}, got {}",
            task_id,
            claimed_task.place_id,
            place_id
        ));
    }
    let connection_key = task_connection_key(task_id);
    let connection = state
        .remote_connections
        .get(&connection_key)
        .ok_or_else(|| eyre!("official MCP adapter requires active remote connection"))?;
    if !matches!(
        connection.connection_state,
        RemoteConnectionState::Connected
    ) {
        return Err(eyre!(
            "official MCP adapter requires ready remote connection for task_id {}",
            task_id
        ));
    }
    if let Some(existing) = state.official_adapters.get(task_id) {
        return Ok(existing.clone());
    }

    let (sender, mut receiver) = mpsc::unbounded_channel::<OfficialAdapterCommand>();
    let (stop_tx, mut stop_rx) = watch::channel(false);
    let handle = OfficialAdapterHandle {
        sender,
        stop_tx: stop_tx.clone(),
    };
    state
        .official_adapters
        .insert(task_id.to_owned(), handle.clone());
    let worker_state = Arc::clone(&app.state);
    let task_id = task_id.to_owned();
    let place_id = place_id.to_owned();
    tokio::spawn(async move {
        official_adapter_worker(worker_state, task_id, place_id, &mut receiver, &mut stop_rx).await;
    });
    Ok(handle)
}

async fn official_adapter_worker(
    state: SharedState,
    task_id: String,
    place_id: String,
    receiver: &mut mpsc::UnboundedReceiver<OfficialAdapterCommand>,
    stop_rx: &mut watch::Receiver<bool>,
) {
    let mut process: Option<OfficialMcpProcess> = None;
    loop {
        tokio::select! {
            changed = stop_rx.changed() => {
                if changed.is_ok() && *stop_rx.borrow() {
                    break;
                }
            }
            command = receiver.recv() => {
                let Some(command) = command else {
                    break;
                };
                set_official_adapter_state(&state, &task_id, "starting", None).await;
                let timeout = Duration::from_millis(command.request.timeout_ms.max(1))
                    .min(OFFICIAL_MCP_REQUEST_TIMEOUT);
                let result = tokio::time::timeout(
                    timeout,
                    official_adapter_execute_with_retry(
                        &task_id,
                        &place_id,
                        &command.request,
                        &mut process,
                    ),
                ).await;
                let result = match result {
                    Ok(result) => result,
                    Err(_) => {
                        if let Some(mut process) = process.take() {
                            process.kill().await;
                        }
                        Err(eyre!("official MCP adapter worker request timed out"))
                    }
                };
                let should_restart_after_error = match &result {
                    Ok(_) => false,
                    Err(error) => should_restart_official_process_after_error(error),
                };
                if should_restart_after_error {
                    if let Some(mut process) = process.take() {
                        process.kill().await;
                    }
                }
                match &result {
                    Ok(_) => set_official_adapter_state(&state, &task_id, "ready", None).await,
                    Err(error) => {
                        set_official_adapter_state(
                            &state,
                            &task_id,
                            "error",
                            Some(summarize_error(&error.to_string())),
                        )
                        .await;
                    }
                }
                let _ = command.response_tx.send(result);
            }
        }
    }
    if let Some(mut process) = process {
        process.kill().await;
    }
    tracing::info!(task_id, place_id, "official MCP adapter worker stopped");
}

async fn official_adapter_execute_with_retry(
    task_id: &str,
    place_id: &str,
    request: &OfficialMcpRequest,
    process: &mut Option<OfficialMcpProcess>,
) -> Result<String> {
    loop {
        match official_adapter_execute(task_id, place_id, request, process).await {
            Ok(response) => return Ok(response),
            Err(error) if is_transient_official_readiness_error(&error) => {
                tracing::warn!(
                    task_id,
                    place_id,
                    action = request.action.as_str(),
                    error = summarize_error(&error.to_string()),
                    "official MCP adapter is waiting for Studio readiness"
                );
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
            Err(error) => return Err(error),
        }
    }
}

async fn official_adapter_execute(
    task_id: &str,
    place_id: &str,
    request: &OfficialMcpRequest,
    process: &mut Option<OfficialMcpProcess>,
) -> Result<String> {
    if request.action == "ping" {
        return official_adapter_ping(task_id, place_id, process).await;
    }
    let official_tool = official_tool_for_action(&request.action)?;
    ensure_official_process(process).await?;
    let process_ref = process
        .as_mut()
        .ok_or_else(|| eyre!("official MCP process was not created"))?;
    process_ref.initialize().await?;
    let studio_summary = process_ref.ensure_active_studio().await?;
    let result = process_ref
        .call_tool(official_tool, request.arguments.clone())
        .await?;
    Ok(serde_json::to_string_pretty(&serde_json::json!({
        "ok": true,
        "adapter_state": "ready",
        "task_id": task_id,
        "place_id": place_id,
        "action": request.action,
        "official_tool": official_tool,
        "studio": studio_summary,
        "result": result,
    }))?)
}

fn official_tool_for_action(action: &str) -> Result<&'static str> {
    match action {
        "generate_mesh" => Ok("generate_mesh"),
        "search_creator_store" => Ok("search_creator_store"),
        "insert_from_creator_store" => Ok("insert_from_creator_store"),
        "store_image" => Ok("store_image"),
        "generate_procedural_model" => Ok("generate_procedural_model"),
        "wait_job_finished" => Ok("wait_job_finished"),
        _ => Err(eyre!("unsupported official MCP adapter action: {}", action)),
    }
}

fn should_restart_official_process_after_error(error: &Report) -> bool {
    !is_transient_official_readiness_error(error)
}

fn is_transient_official_readiness_error(error: &Report) -> bool {
    let message = error.to_string();
    message.contains("Not connected to the WS host")
        || message
            .contains("previously active Studio has disconnected or doesn't have a place opened")
}

async fn ensure_official_process(process: &mut Option<OfficialMcpProcess>) -> Result<()> {
    if process
        .as_ref()
        .map(|item| item.initialized)
        .unwrap_or(false)
    {
        if let Some(process_ref) = process.as_mut() {
            if process_ref.child.try_wait()?.is_some() {
                *process = Some(spawn_official_mcp_process().await?);
            }
        }
    }
    if process.is_none() {
        *process = Some(spawn_official_mcp_process().await?);
    }
    Ok(())
}

async fn official_adapter_ping(
    task_id: &str,
    place_id: &str,
    process: &mut Option<OfficialMcpProcess>,
) -> Result<String> {
    ensure_official_process(process).await?;
    let process_ref = process
        .as_mut()
        .ok_or_else(|| eyre!("official MCP process was not created"))?;
    process_ref.initialize().await?;
    let studio_summary = process_ref.ensure_active_studio().await?;
    let tools = process_ref.tools_list().await?;
    let tools_payload = tools
        .get("tools")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let available_tool_names = tools_payload
        .iter()
        .filter_map(|tool| {
            tool.get("name")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned)
        })
        .collect::<HashSet<_>>();
    let missing_allowed_tools = ALLOWED_OFFICIAL_MCP_TOOLS
        .iter()
        .copied()
        .filter(|tool| !available_tool_names.contains(*tool))
        .collect::<Vec<_>>();
    Ok(serde_json::to_string_pretty(&serde_json::json!({
        "ok": true,
        "adapter_state": "ready",
        "task_id": task_id,
        "place_id": place_id,
        "studio": studio_summary,
        "official": {
            "tool_count": tools_payload.len(),
            "allowed_tool_names": ALLOWED_OFFICIAL_MCP_TOOLS,
            "missing_allowed_tools": missing_allowed_tools,
        },
    }))?)
}

impl OfficialMcpProcess {
    async fn initialize(&mut self) -> Result<()> {
        if self.initialized {
            return Ok(());
        }
        let response = self
            .request(
                "initialize",
                serde_json::json!({
                    "protocolVersion": "2025-11-25",
                    "capabilities": {},
                    "clientInfo": {
                        "name": "clockp-helper-official-adapter",
                        "version": env!("CARGO_PKG_VERSION"),
                    },
                }),
            )
            .await?;
        tracing::info!(
            server_info = ?response.get("serverInfo"),
            "initialized official StudioMCP.exe"
        );
        self.notify("notifications/initialized", serde_json::json!({}))
            .await?;
        self.initialized = true;
        Ok(())
    }

    async fn ensure_active_studio(&mut self) -> Result<Value> {
        let listed = self
            .call_tool("list_roblox_studios", serde_json::json!({}))
            .await?;
        let studio_list: OfficialStudioList = parse_official_text_json(&listed)?;
        let studio = select_official_studio(&studio_list)?;
        self.call_tool(
            "set_active_studio",
            serde_json::json!({ "studio_id": studio.id }),
        )
        .await?;
        self.studio_id = Some(studio.id.clone());
        Ok(serde_json::json!({
            "studio_id": studio.id,
            "studio_name": studio.name,
            "studio_count": studio_list.studios.len(),
            "note": studio_list.note,
            "set_active": true,
        }))
    }

    async fn tools_list(&mut self) -> Result<Value> {
        self.request("tools/list", serde_json::json!({})).await
    }

    async fn call_tool(&mut self, name: &str, arguments: Value) -> Result<Value> {
        let response = self
            .request(
                "tools/call",
                serde_json::json!({
                    "name": name,
                    "arguments": arguments,
                }),
            )
            .await?;
        let call_response: OfficialToolCallResponse = serde_json::from_value(response.clone())?;
        if call_response.is_error {
            return Err(eyre!(
                "official MCP tool {} returned error: {}",
                name,
                summarize_official_content(&call_response.content)
            ));
        }
        Ok(response)
    }

    async fn request(&mut self, method: &str, params: Value) -> Result<Value> {
        self.next_id += 1;
        let id = self.next_id;
        let payload = serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        self.write_json_line(&payload).await?;
        loop {
            let Some(line) = self.stdout_lines.next_line().await? else {
                return Err(eyre!("official MCP process closed stdout"));
            };
            let response: Value = serde_json::from_str(&line)
                .wrap_err_with(|| format!("official MCP returned non-JSON line: {line}"))?;
            if response.get("id").and_then(Value::as_u64) != Some(id) {
                continue;
            }
            if let Some(error) = response.get("error") {
                return Err(eyre!("official MCP {method} failed: {error}"));
            }
            return response
                .get("result")
                .cloned()
                .ok_or_else(|| eyre!("official MCP {method} response missing result"));
        }
    }

    async fn notify(&mut self, method: &str, params: Value) -> Result<()> {
        let payload = serde_json::json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        });
        self.write_json_line(&payload).await
    }

    async fn write_json_line(&mut self, payload: &Value) -> Result<()> {
        let encoded = serde_json::to_string(payload)?;
        self.stdin.write_all(encoded.as_bytes()).await?;
        self.stdin.write_all(b"\n").await?;
        self.stdin.flush().await?;
        Ok(())
    }

    async fn kill(&mut self) {
        let _ = self.child.start_kill();
        let _ = self.child.wait().await;
    }
}

fn parse_official_text_json<T: DeserializeOwned>(response: &Value) -> Result<T> {
    let call_response: OfficialToolCallResponse = serde_json::from_value(response.clone())?;
    let text = call_response
        .content
        .iter()
        .find_map(|item| {
            (item.item_type == "text")
                .then(|| item.text.clone())
                .flatten()
        })
        .ok_or_else(|| eyre!("official MCP response did not include text content"))?;
    Ok(serde_json::from_str(&text)?)
}

fn summarize_official_content(content: &[OfficialContentItem]) -> String {
    content
        .iter()
        .filter_map(|item| item.text.as_deref())
        .collect::<Vec<_>>()
        .join("\n")
}

fn select_official_studio(list: &OfficialStudioList) -> Result<OfficialStudioInfo> {
    if list.studios.is_empty() {
        return Err(eyre!(
            "official MCP found no connected Roblox Studio instances"
        ));
    }
    if list.studios.len() == 1 {
        return Ok(list.studios[0].clone());
    }
    Err(eyre!(
        "official MCP found multiple Studio instances and cannot bind safely in v1"
    ))
}

async fn spawn_official_mcp_process() -> Result<OfficialMcpProcess> {
    let executable = resolve_official_mcp_executable()?;
    let mut child = TokioCommand::new(&executable)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .wrap_err_with(|| format!("failed to spawn {}", executable.display()))?;
    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| eyre!("official MCP stdin was not captured"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| eyre!("official MCP stdout was not captured"))?;
    if let Some(stderr) = child.stderr.take() {
        tokio::spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                tracing::debug!(stderr = summarize_error(&line), "official MCP stderr");
            }
        });
    }
    Ok(OfficialMcpProcess {
        child,
        stdin,
        stdout_lines: BufReader::new(stdout).lines(),
        next_id: 0,
        initialized: false,
        studio_id: None,
    })
}

fn resolve_official_mcp_executable() -> Result<PathBuf> {
    #[cfg(target_os = "windows")]
    {
        let local_app_data =
            env::var_os("LOCALAPPDATA").ok_or_else(|| eyre!("LOCALAPPDATA is not set"))?;
        let roblox_dir = PathBuf::from(local_app_data).join("Roblox");
        let mcp_bat = roblox_dir.join("mcp.bat");
        if let Ok(content) = fs::read_to_string(&mcp_bat) {
            for fragment in content.split('"') {
                if fragment.ends_with("StudioMCP.exe") {
                    let path = PathBuf::from(fragment);
                    if path.is_file() {
                        return Ok(path);
                    }
                }
            }
        }
        let versions_dir = roblox_dir.join("Versions");
        if versions_dir.is_dir() {
            for entry in fs::read_dir(&versions_dir)? {
                let candidate = entry?.path().join("StudioMCP.exe");
                if candidate.is_file() {
                    return Ok(candidate);
                }
            }
        }
        Err(eyre!(
            "cannot locate StudioMCP.exe under {}",
            roblox_dir.display()
        ))
    }
    #[cfg(not(target_os = "windows"))]
    {
        Err(eyre!(
            "official StudioMCP.exe adapter is only available on Windows helper"
        ))
    }
}

async fn handle_remote_command(
    app: AppState,
    place_id: &str,
    task_id: Option<&str>,
    command: Value,
    sender: mpsc::UnboundedSender<RemoteOutgoingFrame>,
    upload_waiters: Arc<Mutex<HashMap<String, oneshot::Sender<Result<ArtifactCommitted>>>>>,
    request_id: String,
) -> Result<String> {
    let (tool_name, payload) = extract_tool_name_and_args(&command)?;
    tracing::info!(
        place_id,
        task_id,
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
        _ => forward_to_plugin(app, place_id, task_id, command).await,
    }
}

async fn forward_to_plugin(
    app: AppState,
    place_id: &str,
    task_id: Option<&str>,
    command: Value,
) -> Result<String> {
    let request_id = extract_command_id(&command)?;
    let (instance_id, notify) = {
        let mut state = app.state.lock().await;
        cleanup_stale_instances(&mut state.instances);
        sync_remote_connections(&app, &mut state);
        let selected_instance_id = select_instance_for_route(place_id, task_id, &state.instances)
            .ok_or_else(|| {
            eyre!("no active Studio plugin registered for placeId {place_id}")
        })?;
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
    instances: &HashMap<String, PluginInstance>,
) -> Option<String> {
    let exact = instances
        .iter()
        .filter(|(_, instance)| {
            instance.place_id == place_id && route_identity_matches(instance, task_id)
        })
        .max_by_key(|(_, instance)| instance.last_seen_at)
        .map(|(instance_id, _)| instance_id.clone());
    if exact.is_some() {
        return exact;
    }
    if task_id.is_some() {
        return None;
    }
    select_instance_for_place(place_id, instances)
}

#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
async fn resolve_capture_pid(
    app: &AppState,
    place_id: Option<&str>,
    task_id: Option<&str>,
) -> Result<(Option<String>, u32)> {
    let state = app.state.lock().await;
    let selected = if let Some(place_id) = place_id {
        let instance_id = select_instance_for_route(place_id, task_id, &state.instances)
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
) -> Result<CapturedScreenshot> {
    #[cfg(target_os = "windows")]
    {
        let (resolved_place_id, studio_pid) = resolve_capture_pid(&app, place_id, task_id).await?;
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
        Err(eyre!(
            "take_screenshot is only available on Windows helper builds"
        ))
    }
}

async fn take_screenshot(
    app: AppState,
    place_id: Option<&str>,
    task_id: Option<&str>,
) -> Result<String> {
    #[cfg(target_os = "windows")]
    {
        let captured = capture_screenshot_png(app, place_id, task_id).await?;
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
    let captured =
        capture_screenshot_png(app.clone(), place_id.as_deref(), task_id.as_deref()).await?;
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
    if args.hub_base_url.is_none() {
        return Err(eyre!(
            "hub_base_url is required; studio_helper must route through hub and claimed task routes"
        ));
    }
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
        skip_claim_studio_launch: args.skip_claim_studio_launch,
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
        skip_claim_studio_launch = helper.skip_claim_studio_launch,
        hub_base_url = helper.hub_base_url,
        domain_suffix = helper.domain_suffix,
        token_source,
        "starting Studio helper"
    );

    let (initial_hub_registration, initial_hub_error) = match hub_register_helper(&helper).await {
        Ok(response) => {
            tracing::info!(
                helper_id = helper.helper_id,
                heartbeat_interval_sec = response.heartbeat_interval_sec,
                heartbeat_timeout_sec = response.heartbeat_timeout_sec,
                "registered helper with hub"
            );
            (Some(response), None)
        }
        Err(RegisterHelperError::HelperIdConflict(message)) => {
            eprintln!("helper_id_conflict: {message}");
            return Err(eyre!("helper_id_conflict: {message}"));
        }
        Err(RegisterHelperError::Other(error)) => {
            let startup_error = summarize_error(&error.to_string());
            tracing::warn!(
                helper_id = helper.helper_id,
                error = startup_error,
                "failed to register helper with hub during startup; continuing with background retry"
            );
            (None, Some(startup_error))
        }
    };

    let state = Arc::new(Mutex::new(HelperState {
        instances: HashMap::new(),
        claimed_tasks: HashMap::new(),
        launch_processes: HashMap::new(),
        waiting_for_plugin: HashMap::new(),
        remote_connections: HashMap::new(),
        official_adapters: HashMap::new(),
        official_adapter_states: HashMap::new(),
        last_remote_errors: HashMap::new(),
        hub_last_error: initial_hub_error,
        hub_last_ready_at: initial_hub_registration.as_ref().map(|_| Instant::now()),
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
    let hub_registered = initial_hub_registration.is_some();
    tokio::spawn(async move {
        hub_maintenance_loop(hub_app, hub_registered).await;
    });

    let app = Router::new()
        .route("/status", get(helper_status))
        .route("/v1/rojo/config", get(rojo_config_handler))
        .route("/rojo-forward/{place_id}", any(rojo_forward_root_handler))
        .route(
            "/rojo-forward/{place_id}/api/socket/{cursor}",
            get(rojo_forward_socket_handler),
        )
        .route(
            "/rojo-forward/{place_id}/{*path}",
            any(rojo_forward_path_handler),
        )
        .route("/v1/mcp/register", post(mcp_register_handler))
        .route("/v1/mcp/unregister", post(mcp_unregister_handler))
        .route("/v1/mcp/plugin/request", get(mcp_plugin_request_handler))
        .route(
            "/v1/mcp/plugin/status",
            post(mcp_plugin_status_update_handler),
        )
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

    fn test_instance(place_id: &str, task_id: Option<&str>, age_ms: u64) -> PluginInstance {
        PluginInstance {
            place_id: place_id.to_owned(),
            task_id: task_id.map(ToOwned::to_owned),
            remote_base_url: "https://example.com".to_owned(),
            studio_pid: Some(123),
            studio_mode: Some("stop".to_owned()),
            studio_mode_observed_at: Some(Instant::now()),
            last_seen_at: Instant::now() - Duration::from_millis(age_ms),
            queue: VecDeque::new(),
            notify: Arc::new(Notify::new()),
        }
    }

    fn test_claimed_task(task_id: &str, place_id: &str) -> ClaimedTask {
        ClaimedTask {
            task_id: task_id.to_owned(),
            place_id: place_id.to_owned(),
            universe_id: Some("999".to_owned()),
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
            official_adapters: HashMap::new(),
            official_adapter_states: HashMap::new(),
            last_remote_errors: HashMap::new(),
            hub_last_error: None,
            hub_last_ready_at: None,
        }
    }

    #[tokio::test]
    async fn stop_and_remove_official_adapter_drops_stale_handle() {
        let state = Arc::new(Mutex::new(empty_helper_state()));
        let (sender, _receiver) = mpsc::unbounded_channel();
        let (stop_tx, mut stop_rx) = watch::channel(false);
        state.lock().await.official_adapters.insert(
            "task-a".to_owned(),
            OfficialAdapterHandle { sender, stop_tx },
        );

        assert!(stop_and_remove_official_adapter(&state, "task-a").await);
        assert!(!state.lock().await.official_adapters.contains_key("task-a"));
        assert!(stop_rx.changed().await.is_ok());
        assert!(*stop_rx.borrow());
    }

    #[test]
    fn rojo_forward_base_url_uses_single_helper_port() {
        assert_eq!(
            rojo_forward_base_url(44750, "93795519121520"),
            "http://127.0.0.1:44750/rojo-forward/93795519121520"
        );
    }

    #[test]
    fn rojo_forward_target_path_preserves_path_and_query() {
        assert_eq!(rojo_forward_target_path("", None), "/");
        assert_eq!(rojo_forward_target_path("api/rojo", None), "/api/rojo");
        assert_eq!(
            rojo_forward_target_path("/api/read/abc", Some("x=1&y=2")),
            "/api/read/abc?x=1&y=2"
        );
    }

    #[test]
    fn rojo_forward_target_ws_url_uses_websocket_scheme() {
        assert_eq!(
            rojo_forward_target_ws_url(
                "https://93795519121520-t_example-rojo-sunjun-user.dev.clock-p.com",
                "api/socket/0",
                Some("cursor=next"),
            )
            .unwrap(),
            "wss://93795519121520-t_example-rojo-sunjun-user.dev.clock-p.com/api/socket/0?cursor=next"
        );
        assert_eq!(
            rojo_forward_target_ws_url(
                "http://93795519121520-t_example-rojo-sunjun-user.dev.clock-p.com",
                "/api/socket/1",
                None,
            )
            .unwrap(),
            "ws://93795519121520-t_example-rojo-sunjun-user.dev.clock-p.com/api/socket/1"
        );
    }

    #[test]
    fn select_instance_for_route_uses_full_identity() {
        let mut instances = HashMap::new();
        instances.insert(
            "older".to_owned(),
            test_instance("93795519121520", Some("t1"), 50),
        );
        instances.insert(
            "newer".to_owned(),
            test_instance("93795519121520", Some("t1"), 5),
        );

        let selected = select_instance_for_route("93795519121520", Some("t1"), &instances);
        assert_eq!(selected.as_deref(), Some("newer"));

        let missing = select_instance_for_route("93795519121520", Some("missing"), &instances);
        assert!(
            missing.is_none(),
            "unknown task_id should not fall back by place"
        );
    }

    #[test]
    fn official_adapter_allows_only_curated_actions() {
        assert!(ALLOWED_OFFICIAL_MCP_TOOLS.contains(&"generate_mesh"));
        assert!(ALLOWED_OFFICIAL_MCP_TOOLS.contains(&"search_creator_store"));
        assert!(!ALLOWED_OFFICIAL_MCP_TOOLS.contains(&"execute_luau"));
        assert!(!ALLOWED_OFFICIAL_MCP_TOOLS.contains(&"script_read"));
        assert_eq!(
            official_tool_for_action("generate_mesh").unwrap(),
            "generate_mesh"
        );
        assert_eq!(
            official_tool_for_action("search_creator_store").unwrap(),
            "search_creator_store"
        );
        assert!(official_tool_for_action("execute_luau").is_err());
        assert!(official_tool_for_action("script_read").is_err());
    }

    #[test]
    fn official_adapter_keeps_process_while_studio_ws_host_is_starting() {
        let transient = eyre!(
            "official MCP tool list_roblox_studios returned error: Not connected to the WS host"
        );
        assert!(!should_restart_official_process_after_error(&transient));

        let opening_place = eyre!(
            "official MCP tool search_creator_store returned error: Execution is prevented because previously active Studio has disconnected or doesn't have a place opened."
        );
        assert!(!should_restart_official_process_after_error(&opening_place));

        let fatal = eyre!("official MCP process closed stdout");
        assert!(should_restart_official_process_after_error(&fatal));
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
    fn helper_id_from_machine_guid_is_stable() {
        let machine_guid = "12345678-90AB-CDEF-1234-567890ABCDEF";
        let derived_a = helper_id_from_machine_guid(machine_guid).expect("helper id should derive");
        let derived_b = helper_id_from_machine_guid(machine_guid).expect("helper id should derive");
        assert_eq!(derived_a, derived_b);
        assert!(derived_a.starts_with("h_"));
        assert!(derived_a
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '-'));
    }

    #[test]
    fn parse_machine_guid_reg_query_output_extracts_guid() {
        let guid = parse_machine_guid_reg_query_output(
            "\nHKEY_LOCAL_MACHINE\\SOFTWARE\\Microsoft\\Cryptography\n    MachineGuid    REG_SZ    abcdef12-3456-7890-abcd-ef1234567890\n",
        )
        .expect("MachineGuid should parse");
        assert_eq!(guid, "abcdef12-3456-7890-abcd-ef1234567890");
    }

    #[test]
    fn parse_helper_id_conflict_extracts_message() {
        let message = parse_helper_id_conflict(
            StatusCode::CONFLICT,
            None,
            r#"{"code":"helper_id_conflict","message":"helper_id already active"}"#,
        )
        .expect("helper_id_conflict should parse");
        assert_eq!(message, "helper_id already active");
        let header_message = parse_helper_id_conflict(
            StatusCode::CONFLICT,
            Some("helper_id_conflict"),
            "helper_id already active",
        )
        .expect("helper_id_conflict header should parse");
        assert_eq!(header_message, "helper_id already active");
        assert!(parse_helper_id_conflict(StatusCode::BAD_REQUEST, None, "{}").is_none());
    }

    #[test]
    fn claim_task_response_ignores_internal_active_launch_id_field() {
        let payload = r#"{
            "claimed": true,
            "helper_id": "h_test",
            "task": {
                "task_id": "t_example",
                "place_id": "93795519121520",
                "universe_id": "9838206573",
                "active_launch_id": "l_internal_only",
                "routes": {
                    "rojo_base_url": "https://93795519121520-t_example-rojo-sunjun-user.dev.clock-p.com",
                    "mcp_base_url": "https://93795519121520-t_example-mcp-sunjun-user.dev.clock-p.com",
                    "runtime_log_base_url": "https://93795519121520-t_example-runtime-log-sunjun-user.dev.clock-p.com"
                }
            }
        }"#;
        let parsed: ClaimTaskHubResponse =
            serde_json::from_str(payload).expect("claim payload should decode");
        let task = parsed.task.expect("task should be present");
        assert_eq!(task.task_id, "t_example");
        assert_eq!(task.place_id, "93795519121520");
        assert_eq!(task.universe_id.as_deref(), Some("9838206573"));
    }

    #[test]
    fn claim_task_response_accepts_legacy_game_id_field() {
        let payload = r#"{
            "claimed": true,
            "helper_id": "h_test",
            "task": {
                "task_id": "t_example",
                "place_id": "93795519121520",
                "game_id": "9838206573",
                "routes": {
                    "mcp_base_url": "https://93795519121520-t_example-mcp-sunjun-user.dev.clock-p.com"
                }
            }
        }"#;
        let parsed: ClaimTaskHubResponse =
            serde_json::from_str(payload).expect("legacy claim payload should decode");
        let task = parsed.task.expect("task should be present");
        assert_eq!(task.game_id.as_deref(), Some("9838206573"));
    }

    #[test]
    fn claim_task_response_accepts_both_universe_id_and_game_id_fields() {
        let payload = r#"{
            "claimed": true,
            "helper_id": "h_test",
            "task": {
                "task_id": "t_example",
                "place_id": "93795519121520",
                "universe_id": "9838206573",
                "game_id": "9838206573",
                "routes": {
                    "mcp_base_url": "https://93795519121520-t_example-mcp-sunjun-user.dev.clock-p.com"
                }
            }
        }"#;
        let parsed: ClaimTaskHubResponse =
            serde_json::from_str(payload).expect("claim payload with both id fields should decode");
        let task = parsed.task.expect("task should be present");
        assert_eq!(task.universe_id.as_deref(), Some("9838206573"));
        assert_eq!(task.game_id.as_deref(), Some("9838206573"));
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn resolve_plugin_routing_decision_allows_explicit_task_hint() {
        let mut state = empty_helper_state();
        state.claimed_tasks.insert(
            "task_a".to_owned(),
            test_claimed_task("task_a", "93795519121520"),
        );

        let decision =
            resolve_plugin_routing_decision(&state, "93795519121520", Some("task_a"), Some(999))
                .expect("explicit task hint should resolve to managed task");
        assert_eq!(decision.task_id, "task_a");
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn resolve_plugin_routing_decision_accepts_helper_launched_pid() {
        let mut state = empty_helper_state();
        state.claimed_tasks.insert(
            "task_a".to_owned(),
            test_claimed_task("task_a", "93795519121520"),
        );
        state.launch_processes.insert(
            "task_a".to_owned(),
            LaunchProcessRecord {
                task_id: "task_a".to_owned(),
                place_id: "93795519121520".to_owned(),
                universe_id: Some("999".to_owned()),
                studio_pid: 222,
                launched_by_helper: true,
                kill_on_close_job: None,
                child: None,
            },
        );

        let decision = resolve_plugin_routing_decision(&state, "93795519121520", None, Some(222))
            .expect("helper-launched Studio should stay managed");
        assert_eq!(decision.task_id, "task_a");
    }

    #[test]
    fn resolve_claimed_task_for_request_prefers_pid_binding() {
        let mut state = empty_helper_state();
        state.claimed_tasks.insert(
            "task_a".to_owned(),
            test_claimed_task("task_a", "93795519121520"),
        );
        state.claimed_tasks.insert(
            "task_b".to_owned(),
            test_claimed_task("task_b", "93795519121520"),
        );
        state.launch_processes.insert(
            "task_a".to_owned(),
            LaunchProcessRecord {
                task_id: "task_a".to_owned(),
                place_id: "93795519121520".to_owned(),
                universe_id: Some("999".to_owned()),
                studio_pid: 111,
                launched_by_helper: true,
                #[cfg(target_os = "windows")]
                kill_on_close_job: None,
                child: None,
            },
        );
        state.launch_processes.insert(
            "task_b".to_owned(),
            LaunchProcessRecord {
                task_id: "task_b".to_owned(),
                place_id: "93795519121520".to_owned(),
                universe_id: Some("999".to_owned()),
                studio_pid: 222,
                launched_by_helper: true,
                #[cfg(target_os = "windows")]
                kill_on_close_job: None,
                child: None,
            },
        );

        let selected = resolve_claimed_task_for_request(&state, "93795519121520", None, Some(222))
            .expect("pid-bound task should resolve");
        assert_eq!(selected.task_id, "task_b");
    }

    #[test]
    fn resolve_claimed_task_for_request_requires_identity_without_pid_binding() {
        let mut state = empty_helper_state();
        state.claimed_tasks.insert(
            "task_a".to_owned(),
            test_claimed_task("task_a", "93795519121520"),
        );

        let error =
            match resolve_claimed_task_for_request(&state, "93795519121520", None, Some(999)) {
                Ok(_) => panic!("expected unbound Studio pid without explicit identity to fail"),
                Err(error) => error,
            };

        assert!(error
            .to_string()
            .contains("task_id is required unless the Studio pid was launched by helper"));
    }

    #[test]
    fn resolve_claimed_task_for_plugin_request_accepts_unique_place_binding() {
        let mut state = empty_helper_state();
        state.claimed_tasks.insert(
            "task_a".to_owned(),
            test_claimed_task("task_a", "93795519121520"),
        );

        let selected =
            resolve_claimed_task_for_plugin_request(&state, "93795519121520", None, Some(999))
                .expect("unique claimed task for place should resolve without plugin task hint");

        assert_eq!(selected.task_id, "task_a");
    }

    #[test]
    fn resolve_claimed_task_for_plugin_request_accepts_task_hint_without_extra_fields() {
        let mut state = empty_helper_state();
        state.claimed_tasks.insert(
            "task_a".to_owned(),
            test_claimed_task("task_a", "93795519121520"),
        );

        let selected = resolve_claimed_task_for_plugin_request(
            &state,
            "93795519121520",
            Some("task_a"),
            Some(999),
        )
        .expect("task hint should resolve with task_id only");

        assert_eq!(selected.task_id, "task_a");
    }

    #[test]
    fn resolve_claimed_task_for_plugin_request_rejects_ambiguous_place_binding() {
        let mut state = empty_helper_state();
        state.claimed_tasks.insert(
            "task_a".to_owned(),
            test_claimed_task("task_a", "93795519121520"),
        );
        state.claimed_tasks.insert(
            "task_b".to_owned(),
            test_claimed_task("task_b", "93795519121520"),
        );

        let error = match resolve_claimed_task_for_plugin_request(
            &state,
            "93795519121520",
            None,
            Some(999),
        ) {
            Ok(_) => panic!(
                "expected ambiguous place binding without helper pid or explicit task to fail"
            ),
            Err(error) => error,
        };

        assert!(error
            .to_string()
            .contains("multiple claimed tasks found for place_id 93795519121520"));
    }

    #[test]
    fn release_claimed_task_cleans_exact_route_state() {
        let mut state = empty_helper_state();
        state.claimed_tasks.insert(
            "task_a".to_owned(),
            test_claimed_task("task_a", "93795519121520"),
        );
        state.instances.insert(
            "instance_a".to_owned(),
            test_instance("93795519121520", Some("task_a"), 5),
        );
        state.launch_processes.insert(
            "task_a".to_owned(),
            LaunchProcessRecord {
                task_id: "task_a".to_owned(),
                place_id: "93795519121520".to_owned(),
                universe_id: Some("999".to_owned()),
                studio_pid: 111,
                launched_by_helper: false,
                #[cfg(target_os = "windows")]
                kill_on_close_job: None,
                child: None,
            },
        );
        state
            .last_remote_errors
            .insert(task_connection_key("task_a"), "boom".to_owned());

        let pending_termination = release_claimed_task(&mut state, "task_a", false, "test release");

        assert!(state.claimed_tasks.is_empty());
        assert!(state.instances.is_empty());
        assert!(state.launch_processes.is_empty());
        assert!(state.last_remote_errors.is_empty());
        assert!(pending_termination.is_none());
    }

    #[test]
    fn validate_remote_ready_ack_checks_full_route_identity() {
        validate_remote_ready_ack(
            "93795519121520",
            Some("t_example"),
            "93795519121520",
            Some("t_example"),
        )
        .expect("exact ready ack should pass");

        let missing_task =
            validate_remote_ready_ack("93795519121520", Some("t_example"), "93795519121520", None)
                .expect_err("missing task identity should fail");
        assert!(missing_task
            .to_string()
            .contains("remote MCP ready ack task_id mismatch"));

        let wrong_task = validate_remote_ready_ack(
            "93795519121520",
            Some("t_example"),
            "93795519121520",
            Some("t_other"),
        )
        .expect_err("task mismatch should fail");
        assert!(wrong_task
            .to_string()
            .contains("remote MCP ready ack task_id mismatch"));

        let wrong_place = validate_remote_ready_ack(
            "93795519121520",
            Some("t_example"),
            "111",
            Some("t_example"),
        )
        .expect_err("place mismatch should fail");
        assert!(wrong_place
            .to_string()
            .contains("remote MCP ready ack place_id mismatch"));
    }
}
