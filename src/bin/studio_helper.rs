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
use std::future::Future;
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
    RuntimeLogForwardStatusSnapshot, ServerToHelperMessage, HELPER_WS_PATH,
    MAX_ARTIFACT_CHUNK_PAYLOAD_BYTES, OFFICIAL_MCP_ADAPTER_CAPABILITY,
    OFFICIAL_MCP_STORE_IMAGE_BASE64_CAPABILITY,
};

#[path = "studio_helper/config.rs"]
mod config;
#[path = "studio_helper/runtime_log_forward.rs"]
mod runtime_log_forward;
#[path = "studio_helper/studio_process.rs"]
mod studio_process;
#[path = "studio_helper/text.rs"]
mod text;
#[path = "studio_helper/urls.rs"]
mod urls;

use config::{
    build_http_client, helper_data_dir, resolve_bearer_token, resolve_bearer_token_candidates,
    resolve_helper_id, resolve_user_name, Args, HelperConfig, ResolvedToken,
};
#[cfg(test)]
use config::{helper_id_from_machine_guid, parse_machine_guid_reg_query_output};
use runtime_log_forward::runtime_log_forward_task_path_handler;
use runtime_log_forward::{runtime_log_forward_worker, RuntimeLogForwardJob};
#[cfg(target_os = "windows")]
use studio_process::launch_studio_for_claim;
#[cfg(test)]
use studio_process::studio_launch_args_for_claim;
use studio_process::{ensure_claimed_task_universe_id, now_stamp};
use text::{sanitize_identifier, sanitize_place_id, summarize_error, trim};
use urls::{
    derive_remote_helper_ws_url_from_base_url, rojo_forward_base_url, rojo_forward_target_path,
    rojo_forward_target_ws_url, runtime_log_forward_base_url, runtime_log_forward_upload_url,
    runtime_screenshot_helper_upload_url, runtime_screenshot_upload_url_for_base_url,
};

#[cfg(target_os = "windows")]
use regex::Regex;
#[cfg(target_os = "windows")]
use std::mem::size_of;
#[cfg(target_os = "windows")]
use std::os::windows::io::OwnedHandle;
#[cfg(target_os = "windows")]
use std::ptr::null_mut;
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
const PLUGIN_REQUEST_TIMEOUT: Duration = Duration::from_secs(60);
const PLUGIN_STUDIO_CONTROL_REQUEST_TIMEOUT: Duration = Duration::from_secs(150);
const STUDIO_CONTROL_HEARTBEAT_STALE_AFTER: Duration = Duration::from_secs(2);
const EDIT_RUNTIME_STALE_AFTER: Duration = Duration::from_secs(5);
const ROBLOX_PLACE_UNIVERSE_LOOKUP_TIMEOUT: Duration = Duration::from_secs(10);
const OFFICIAL_MCP_REQUEST_TIMEOUT: Duration = Duration::from_secs(1800);
const OBSERVABLE_UPSTREAM_SLOW_AFTER: Duration = Duration::from_secs(5);
const OFFICIAL_STORE_IMAGE_MAX_DECODED_BYTES: usize = 20 * 1024 * 1024;
const OFFICIAL_STORE_IMAGE_MAX_BASE64_CHARS: usize =
    OFFICIAL_STORE_IMAGE_MAX_DECODED_BYTES.div_ceil(3) * 4;
const RUNTIME_LOG_FORWARD_QUEUE_CAPACITY: usize = 64;
const INSTANCE_STALE_AFTER: Duration = Duration::from_secs(45);
const REMOTE_WS_CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
const REMOTE_WS_STALE_RESTART_AFTER: Duration = Duration::from_secs(30);
const REMOTE_WS_STALE_RELEASE_AFTER: Duration = Duration::from_secs(180);
const REMOTE_WS_PING_INTERVAL: Duration = Duration::from_secs(15);
const REMOTE_WS_PONG_TIMEOUT: Duration = Duration::from_secs(45);
const ALLOWED_OFFICIAL_MCP_TOOLS: &[&str] = &[
    "generate_mesh",
    "search_creator_store",
    "insert_from_creator_store",
    "store_image",
    "generate_procedural_model",
    "wait_job_finished",
];

struct PluginInstance {
    place_id: String,
    task_id: Option<String>,
    remote_base_url: String,
    studio_pid: Option<u32>,
    studio_mode: Option<String>,
    studio_mode_observed_at: Option<Instant>,
    studio_mode_source: String,
    studio_control_state: String,
    studio_transition_phase: String,
    studio_transition_started_at: Option<Instant>,
    studio_control_observed_at: Option<Instant>,
    edit_runtime_observed_at: Option<Instant>,
    edit_runtime_unavailable_reason: Option<String>,
    studio_control_last_error: Option<String>,
    stop_request_id: u64,
    stop_request_recorded_at: Option<Instant>,
    stop_request_last_polled_at: Option<Instant>,
    stop_request_last_poll_id: Option<u64>,
    stop_result_phase: Option<String>,
    stop_result_error: Option<String>,
    stop_result_observed_at: Option<Instant>,
    last_seen_at: Instant,
    queue: VecDeque<Value>,
    notify: Arc<Notify>,
}

struct PendingPluginResponse {
    instance_id: String,
    tool_name: String,
    response_tx: oneshot::Sender<PluginResponsePayload>,
}

struct HelperState {
    instances: HashMap<String, PluginInstance>,
    claimed_tasks: HashMap<String, ClaimedTask>,
    launch_processes: HashMap<String, LaunchProcessRecord>,
    waiting_for_plugin: HashMap<String, PendingPluginResponse>,
    remote_connections: HashMap<String, RemoteConnectionHandle>,
    official_adapters: HashMap<String, OfficialAdapterHandle>,
    official_adapter_states: HashMap<String, OfficialAdapterStateRecord>,
    runtime_log_forward_statuses: HashMap<String, RuntimeLogForwardStatusRecord>,
    last_remote_errors: HashMap<String, String>,
    hub_last_error: Option<String>,
    hub_last_claim_error: Option<String>,
    hub_last_ready_at: Option<Instant>,
}

#[derive(Debug, Clone, Default)]
struct RuntimeLogForwardStatusRecord {
    queued_count: u64,
    accepted_count: u64,
    forwarded_count: u64,
    failed_count: u64,
    last_accepted_at: Option<Instant>,
    last_forwarded_at: Option<Instant>,
    last_attempt_at: Option<Instant>,
    last_target_path: Option<String>,
    last_http_status: Option<u16>,
    last_error: Option<String>,
    last_error_at: Option<Instant>,
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
    Error,
}

struct RemoteConnectionHandle {
    worker_id: Uuid,
    stop_tx: watch::Sender<bool>,
    sender: mpsc::UnboundedSender<RemoteOutgoingFrame>,
    #[allow(dead_code)]
    remote_base_url: String,
    #[allow(dead_code)]
    place_id: String,
    #[allow(dead_code)]
    task_id: Option<String>,
    connection_state: RemoteConnectionState,
    state_changed_at: Instant,
    retrying_since: Option<Instant>,
    consecutive_failures: u32,
    connection_id: Option<String>,
    last_ready_at: Option<Instant>,
    last_server_message_at: Option<Instant>,
}

#[derive(Clone)]
enum RemoteOutgoingFrame {
    Text(String),
    Ping(Vec<u8>),
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

#[derive(Debug)]
struct TempOfficialImage {
    path: PathBuf,
}

impl Drop for TempOfficialImage {
    fn drop(&mut self) {
        if let Err(error) = fs::remove_file(&self.path) {
            tracing::warn!(
                path = %self.path.display(),
                error = %error,
                "failed to remove temporary official MCP image file"
            );
        }
    }
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
    runtime_log_forward_tx: mpsc::Sender<RuntimeLogForwardJob>,
}

#[derive(Debug, Deserialize)]
struct RegisterPluginRequest {
    place_id: String,
    task_id: Option<String>,
}

#[derive(Debug, Serialize)]
struct RegisterPluginResponse {
    instance_id: String,
    place_id: String,
    task_id: Option<String>,
    remote_base_url: String,
    runtime_log_base_url: Option<String>,
    runtime_log_remote_base_url: Option<String>,
    runtime_log_upload_url: Option<String>,
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
struct PluginEditHeartbeatRequest {
    instance_id: String,
}

#[derive(Debug, Deserialize)]
struct PluginControlHeartbeatRequest {
    instance_id: String,
    mode: String,
}

#[derive(Debug, Deserialize)]
struct PluginStopRequestPayload {
    instance_id: String,
}

#[derive(Debug, Deserialize)]
struct PluginStopRequestQuery {
    instance_id: String,
    #[serde(default)]
    after_id: Option<u64>,
}

#[derive(Debug, Deserialize, Serialize)]
struct PluginStopRequestResponse {
    stop_requested: bool,
    stop_request_id: u64,
}

#[derive(Debug, Deserialize)]
struct PluginStopResultPayload {
    instance_id: String,
    stop_request_id: u64,
    phase: String,
    #[serde(default)]
    error: Option<String>,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
struct PluginResponsePayload {
    instance_id: Option<String>,
    id: String,
    success: bool,
    response: String,
}

#[derive(Debug, Deserialize)]
struct StartStopPlayCommandArgs {
    mode: String,
}

#[derive(Debug, Deserialize)]
struct LaunchStudioSessionCommandArgs {
    mode: String,
}

#[derive(Debug, Serialize)]
struct HelperStatusResponse {
    helper_port: u16,
    helper_id: String,
    user_name: String,
    capacity: usize,
    hub_base_url: Option<String>,
    hub_last_error: Option<String>,
    hub_last_claim_error: Option<String>,
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
    runtime_log_remote_base_url: Option<String>,
    runtime_log_upload_url: Option<String>,
    runtime_log_forward: RuntimeLogForwardStatusSnapshot,
    studio_pid: Option<u32>,
    launched_by_helper: bool,
    claimed_age_ms: u128,
    remote_state: String,
    remote_connection_id: Option<String>,
    remote_last_error: Option<String>,
    remote_last_ready_age_ms: Option<u128>,
    remote_last_server_message_age_ms: Option<u128>,
    studio_session_state: String,
    last_known_session_state: Option<String>,
    last_session_error_reason: Option<String>,
    studio_mode: Option<String>,
    studio_mode_age_ms: Option<u128>,
    studio_mode_source: String,
    studio_control_state: String,
    studio_transition_phase: String,
    studio_transition_age_ms: Option<u128>,
    edit_runtime_state: String,
    edit_runtime_age_ms: Option<u128>,
    studio_control_last_error: Option<String>,
    active_stop_request_id: Option<u64>,
    last_stop_request_id: u64,
    stop_request_recorded_age_ms: Option<u128>,
    runtime_actuator_last_poll_id: Option<u64>,
    runtime_actuator_last_poll_age_ms: Option<u128>,
    stop_result_phase: Option<String>,
    stop_result_age_ms: Option<u128>,
    stop_result_error: Option<String>,
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
    #[allow(dead_code)]
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
    studio_session_state: String,
    last_known_session_state: Option<String>,
    last_session_error_reason: Option<String>,
    studio_mode: Option<String>,
    studio_mode_age_ms: Option<u128>,
    studio_mode_source: String,
    studio_control_state: String,
    studio_transition_phase: String,
    studio_transition_age_ms: Option<u128>,
    edit_runtime_state: String,
    edit_runtime_age_ms: Option<u128>,
    studio_control_last_error: Option<String>,
    active_stop_request_id: Option<u64>,
    last_stop_request_id: u64,
    stop_request_recorded_age_ms: Option<u128>,
    runtime_actuator_last_poll_id: Option<u64>,
    runtime_actuator_last_poll_age_ms: Option<u128>,
    stop_result_phase: Option<String>,
    stop_result_age_ms: Option<u128>,
    stop_result_error: Option<String>,
    official_mcp_adapter_state: String,
    official_mcp_adapter_age_ms: Option<u128>,
    official_mcp_adapter_last_error: Option<String>,
    runtime_log_forward: RuntimeLogForwardStatusSnapshot,
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
    #[allow(dead_code)]
    claimed: bool,
    #[allow(dead_code)]
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
    studio_session_state: String,
    last_known_session_state: Option<String>,
    last_session_error_reason: Option<String>,
    studio_mode: Option<String>,
    studio_mode_age_ms: Option<u128>,
    studio_mode_source: String,
    studio_control_state: String,
    studio_transition_phase: String,
    studio_transition_age_ms: Option<u128>,
    edit_runtime_state: String,
    edit_runtime_age_ms: Option<u128>,
    studio_control_last_error: Option<String>,
    active_stop_request_id: Option<u64>,
    last_stop_request_id: u64,
    stop_request_recorded_age_ms: Option<u128>,
    runtime_actuator_last_poll_id: Option<u64>,
    runtime_actuator_last_poll_age_ms: Option<u128>,
    stop_result_phase: Option<String>,
    stop_result_age_ms: Option<u128>,
    stop_result_error: Option<String>,
}

#[derive(Debug, Serialize)]
struct HelperPlaceStatus {
    place_id: String,
    remote_base_url: String,
    task_id: Option<String>,
    runtime_log_base_url: Option<String>,
    runtime_log_remote_base_url: Option<String>,
    runtime_log_upload_url: Option<String>,
    runtime_screenshot_upload_url: Option<String>,
    runtime_log_forward: Option<RuntimeLogForwardStatusSnapshot>,
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

async fn await_observable_upstream_result<T, F>(
    operation: &'static str,
    target: String,
    future: F,
) -> T
where
    F: Future<Output = T>,
{
    let started_at = Instant::now();
    let mut future = Box::pin(future);
    tokio::select! {
        result = &mut future => result,
        _ = tokio::time::sleep(OBSERVABLE_UPSTREAM_SLOW_AFTER) => {
            tracing::warn!(
                operation,
                target,
                threshold_ms = OBSERVABLE_UPSTREAM_SLOW_AFTER.as_millis(),
                elapsed_ms = started_at.elapsed().as_millis(),
                result_observable = true,
                "observable upstream operation still waiting for result"
            );
            future.await
        }
    }
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

fn task_instance_pids(state: &HelperState, task_id: &str) -> Vec<u32> {
    let mut pids = state
        .instances
        .values()
        .filter(|instance| instance.task_id.as_deref() == Some(task_id))
        .filter_map(|instance| instance.studio_pid)
        .collect::<Vec<_>>();
    pids.sort();
    pids.dedup();
    pids
}

fn launch_process_is_active_for_status(state: &HelperState, launch: &LaunchProcessRecord) -> bool {
    let instance_pids = task_instance_pids(state, &launch.task_id);
    instance_pids.is_empty() || instance_pids.contains(&launch.studio_pid)
}

fn task_active_studio_pid(state: &HelperState, task_id: &str) -> Option<u32> {
    task_instance_pids(state, task_id)
        .into_iter()
        .next()
        .or_else(|| {
            state
                .launch_processes
                .get(task_id)
                .filter(|launch| launch_process_is_active_for_status(state, launch))
                .map(|launch| launch.studio_pid)
        })
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
    let connection_key = task_connection_key(task_id);
    if let Some(remote_connection) = state.remote_connections.remove(&connection_key) {
        let _ = remote_connection.stop_tx.send(true);
        tracing::info!(
            task_id,
            connection_key,
            "stopped remote websocket for released claimed task"
        );
    }
    state.claimed_tasks.remove(task_id);
    state.runtime_log_forward_statuses.remove(task_id);
    stop_removed_official_adapter(state, task_id);
    state.last_remote_errors.remove(&connection_key);
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

async fn hub_post_json<TRequest: Serialize, TResponse: DeserializeOwned>(
    helper: &HelperConfig,
    path: &str,
    payload: &TRequest,
) -> Result<TResponse> {
    let Some(base_url) = helper.hub_base_url.as_ref() else {
        return Err(eyre!("hub_base_url is not configured"));
    };
    let token = helper.bearer_token.lock().await.clone();
    let url = format!("{}{path}", base_url.trim_end_matches('/'));
    let request = helper.client.post(&url).bearer_auth(token).json(payload);
    let (status, body) =
        await_observable_upstream_result("hub_http", path.to_owned(), async move {
            let response = request.send().await?;
            let status = response.status();
            let body = response.text().await?;
            Result::<(StatusCode, String)>::Ok((status, body))
        })
        .await?;
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
    let request = helper
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
        });
    let (status, error_code, body) = await_observable_upstream_result(
        "hub_http",
        "/v1/helpers/register".to_owned(),
        async move {
            let response = request.send().await?;
            let status = response.status();
            let error_code = response
                .headers()
                .get("x-clock-p-hub-error")
                .and_then(|value| value.to_str().ok())
                .map(ToOwned::to_owned);
            let body = response.text().await?;
            Result::<(StatusCode, Option<String>, String)>::Ok((status, error_code, body))
        },
    )
    .await
    .map_err(RegisterHelperError::Other)?;
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

    let claimed_task = select_claimed_task(state, place_id, task_id)?;
    let expected_remote = claimed_task
        .runtime_log_base_url
        .as_deref()
        .map(runtime_screenshot_upload_url_for_base_url)
        .ok_or_else(|| eyre!("claimed task has no runtime_log_base_url"))?;
    if trimmed == expected_remote {
        return Ok(expected_remote);
    }

    let expected_helper =
        runtime_screenshot_helper_upload_url(helper.port, place_id, &claimed_task.task_id);
    if trimmed != expected_helper {
        return Err(eyre!(
            "upload_url must match runtime screenshot sink for placeId {place_id}: {expected_remote} or {expected_helper}"
        ));
    }

    Ok(expected_remote)
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

        let upload_target = format!("{upload_url} source={}", candidate.source);
        let (status, response_body) = await_observable_upstream_result(
            "runtime_screenshot_upload",
            upload_target,
            async move {
                let response = request.body(png_bytes.to_vec()).send().await?;
                let status = response.status();
                let response_body = response.text().await?;
                Result::<(StatusCode, String)>::Ok((status, response_body))
            },
        )
        .await?;
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

fn is_studio_control_tool(tool_name: &str) -> bool {
    matches!(
        tool_name,
        "LaunchStudioSession" | "StartStopPlay" | "launch_studio_session" | "start_stop_play"
    )
}

fn plugin_request_timeout_for_tool(tool_name: &str) -> Duration {
    if is_studio_control_tool(tool_name) {
        PLUGIN_STUDIO_CONTROL_REQUEST_TIMEOUT
    } else {
        PLUGIN_REQUEST_TIMEOUT
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
    cleanup_stale_instances(&mut state);
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
        .map(|(instance_id, instance)| {
            let stop_snapshot = stop_request_snapshot_for_instance(instance);
            HelperInstanceStatus {
                instance_id: instance_id.clone(),
                place_id: instance.place_id.clone(),
                task_id: instance.task_id.clone(),
                remote_base_url: instance.remote_base_url.clone(),
                studio_pid: instance.studio_pid,
                studio_session_state: reported_session_state_for_instance(instance).to_owned(),
                last_known_session_state: last_known_session_state_for_instance(instance)
                    .map(str::to_owned),
                last_session_error_reason: instance.studio_control_last_error.clone(),
                studio_mode: instance.studio_mode.clone(),
                studio_mode_age_ms: instance
                    .studio_mode_observed_at
                    .map(|value| value.elapsed().as_millis()),
                studio_mode_source: instance.studio_mode_source.clone(),
                studio_control_state: effective_studio_control_state(instance),
                studio_transition_phase: effective_studio_transition_phase(instance),
                studio_transition_age_ms: instance
                    .studio_transition_started_at
                    .map(|value| value.elapsed().as_millis()),
                edit_runtime_state: edit_runtime_state(instance),
                edit_runtime_age_ms: instance
                    .edit_runtime_observed_at
                    .map(|value| value.elapsed().as_millis()),
                studio_control_last_error: instance.studio_control_last_error.clone(),
                active_stop_request_id: stop_snapshot.active_stop_request_id,
                last_stop_request_id: stop_snapshot.last_stop_request_id,
                stop_request_recorded_age_ms: stop_snapshot.stop_request_recorded_age_ms,
                runtime_actuator_last_poll_id: stop_snapshot.runtime_actuator_last_poll_id,
                runtime_actuator_last_poll_age_ms: stop_snapshot.runtime_actuator_last_poll_age_ms,
                stop_result_phase: stop_snapshot.stop_result_phase,
                stop_result_age_ms: stop_snapshot.stop_result_age_ms,
                stop_result_error: stop_snapshot.stop_result_error,
            }
        })
        .collect();
    instances.sort_by(|left, right| left.instance_id.cmp(&right.instance_id));
    let mut claimed_tasks: Vec<_> = state
        .claimed_tasks
        .values()
        .map(|task| {
            let connection_key = task_connection_key(&task.task_id);
            let remote_connection = state.remote_connections.get(&connection_key);
            let studio_snapshot = task_studio_mode_snapshot(&state, &task.task_id);
            let (
                official_mcp_adapter_state,
                official_mcp_adapter_age_ms,
                official_mcp_adapter_last_error,
            ) = task_official_adapter_snapshot(
                &state,
                &task.task_id,
                studio_snapshot.edit_runtime_state == "ready",
            );
            let launch_process = state.launch_processes.get(&task.task_id);
            let studio_pid = task_active_studio_pid(&state, &task.task_id);
            let active_launch_process =
                launch_process.filter(|launch| launch_process_is_active_for_status(&state, launch));
            let runtime_log_base_url = task.runtime_log_base_url.as_ref().map(|_| {
                runtime_log_forward_base_url(app.helper.port, &task.place_id, &task.task_id)
            });
            let runtime_log_upload_url = task.runtime_log_base_url.as_ref().map(|_| {
                runtime_log_forward_upload_url(app.helper.port, &task.place_id, &task.task_id)
            });
            let runtime_log_forward = runtime_log_forward_status_snapshot(&state, &task.task_id);
            ClaimedTaskStatus {
                task_id: task.task_id.clone(),
                place_id: task.place_id.clone(),
                universe_id: task.universe_id.clone(),
                mcp_base_url: task.mcp_base_url.clone(),
                rojo_base_url: task.rojo_base_url.clone(),
                runtime_log_base_url,
                runtime_log_remote_base_url: task.runtime_log_base_url.clone(),
                runtime_log_upload_url,
                runtime_log_forward,
                studio_pid,
                launched_by_helper: active_launch_process
                    .map(|launch| launch.launched_by_helper)
                    .unwrap_or(false),
                claimed_age_ms: task.claimed_at.elapsed().as_millis(),
                remote_state: remote_connection
                    .map(|connection| match connection.connection_state {
                        RemoteConnectionState::Connecting => "connecting",
                        RemoteConnectionState::Connected => "connected",
                        RemoteConnectionState::Retrying => "retrying",
                        RemoteConnectionState::Error => "error",
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
                studio_session_state: studio_snapshot.studio_session_state,
                last_known_session_state: studio_snapshot.last_known_session_state,
                last_session_error_reason: studio_snapshot.last_session_error_reason,
                studio_mode: studio_snapshot.studio_mode,
                studio_mode_age_ms: studio_snapshot.studio_mode_age_ms,
                studio_mode_source: studio_snapshot.studio_mode_source,
                studio_control_state: studio_snapshot.studio_control_state,
                studio_transition_phase: studio_snapshot.studio_transition_phase,
                studio_transition_age_ms: studio_snapshot.studio_transition_age_ms,
                edit_runtime_state: studio_snapshot.edit_runtime_state,
                edit_runtime_age_ms: studio_snapshot.edit_runtime_age_ms,
                studio_control_last_error: studio_snapshot.studio_control_last_error,
                active_stop_request_id: studio_snapshot.active_stop_request_id,
                last_stop_request_id: studio_snapshot.last_stop_request_id,
                stop_request_recorded_age_ms: studio_snapshot.stop_request_recorded_age_ms,
                runtime_actuator_last_poll_id: studio_snapshot.runtime_actuator_last_poll_id,
                runtime_actuator_last_poll_age_ms: studio_snapshot
                    .runtime_actuator_last_poll_age_ms,
                stop_result_phase: studio_snapshot.stop_result_phase,
                stop_result_age_ms: studio_snapshot.stop_result_age_ms,
                stop_result_error: studio_snapshot.stop_result_error,
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
        .filter(|launch| launch_process_is_active_for_status(&state, launch))
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
                if launch_process_is_active_for_status(&state, launch) {
                    studio_pids.push(launch.studio_pid);
                }
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
        let runtime_log_base_url = claimed_task.as_ref().and_then(|task| {
            task.runtime_log_base_url.as_ref().map(|_| {
                runtime_log_forward_base_url(app.helper.port, &task.place_id, &task.task_id)
            })
        });
        let runtime_log_upload_url = claimed_task.as_ref().and_then(|task| {
            task.runtime_log_base_url.as_ref().map(|_| {
                runtime_log_forward_upload_url(app.helper.port, &task.place_id, &task.task_id)
            })
        });
        let runtime_screenshot_upload_url = claimed_task.as_ref().and_then(|task| {
            task.runtime_log_base_url.as_ref().map(|_| {
                runtime_screenshot_helper_upload_url(app.helper.port, &task.place_id, &task.task_id)
            })
        });
        let runtime_log_forward = claimed_task
            .as_ref()
            .map(|task| runtime_log_forward_status_snapshot(&state, &task.task_id));
        place_statuses.push(HelperPlaceStatus {
            remote_base_url: claimed_task
                .as_ref()
                .map(|task| task.mcp_base_url.clone())
                .unwrap_or_default(),
            place_id: place_id.clone(),
            task_id: claimed_task.as_ref().map(|task| task.task_id.clone()),
            runtime_log_base_url,
            runtime_log_remote_base_url: claimed_task
                .as_ref()
                .and_then(|task| task.runtime_log_base_url.clone()),
            runtime_log_upload_url,
            runtime_screenshot_upload_url,
            runtime_log_forward,
            registered_instance_count: registered_instance_ids.len(),
            registered_instance_ids,
            studio_pids,
            remote_state: remote_connection
                .map(|connection| match connection.connection_state {
                    RemoteConnectionState::Connecting => "connecting",
                    RemoteConnectionState::Connected => "connected",
                    RemoteConnectionState::Retrying => "retrying",
                    RemoteConnectionState::Error => "error",
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
        hub_last_claim_error: state.hub_last_claim_error.clone(),
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

async fn helper_debug_task_handler(
    State(app): State<AppState>,
    AxumPath(task_id): AxumPath<String>,
) -> Result<Response, HelperError> {
    let mut state = app.state.lock().await;
    cleanup_stale_instances(&mut state);
    reconcile_launch_processes(&mut state);
    sync_remote_connections(&app, &mut state);
    let Some(task) = state.claimed_tasks.get(&task_id) else {
        return Ok((StatusCode::NOT_FOUND, "task not claimed by helper").into_response());
    };
    let connection_key = task_connection_key(&task.task_id);
    let remote_connection = state.remote_connections.get(&connection_key);
    let launch_process = state.launch_processes.get(&task.task_id);
    let instances = state
        .instances
        .iter()
        .filter(|(_, instance)| instance.task_id.as_deref() == Some(task_id.as_str()))
        .map(|(instance_id, instance)| {
            serde_json::json!({
                "instance_id": instance_id,
                "place_id": instance.place_id,
                "task_id": instance.task_id,
                "remote_base_url": instance.remote_base_url,
                "studio_pid": instance.studio_pid,
                "studio_mode": instance.studio_mode,
                "studio_mode_age_ms": instance.studio_mode_observed_at.map(|value| value.elapsed().as_millis()),
                "studio_mode_source": instance.studio_mode_source,
                "studio_control_state": effective_studio_control_state(instance),
                "studio_transition_phase": effective_studio_transition_phase(instance),
                "studio_transition_age_ms": instance.studio_transition_started_at.map(|value| value.elapsed().as_millis()),
                "edit_runtime_state": edit_runtime_state(instance),
                "edit_runtime_age_ms": instance.edit_runtime_observed_at.map(|value| value.elapsed().as_millis()),
                "studio_control_last_error": instance.studio_control_last_error,
            })
        })
        .collect::<Vec<_>>();
    let task_status = helper_task_status_snapshot(&state, &task.task_id);
    let runtime_log_base_url = task
        .runtime_log_base_url
        .as_ref()
        .map(|_| runtime_log_forward_base_url(app.helper.port, &task.place_id, &task.task_id));
    let runtime_log_upload_url = task
        .runtime_log_base_url
        .as_ref()
        .map(|_| runtime_log_forward_upload_url(app.helper.port, &task.place_id, &task.task_id));
    let runtime_log_forward = runtime_log_forward_status_snapshot(&state, &task.task_id);
    let payload = serde_json::json!({
        "ok": true,
        "task_id": task.task_id,
        "claimed_task": {
            "task_id": task.task_id,
            "place_id": task.place_id,
            "universe_id": task.universe_id,
            "mcp_base_url": task.mcp_base_url,
            "rojo_base_url": task.rojo_base_url,
            "runtime_log_base_url": runtime_log_base_url,
            "runtime_log_remote_base_url": task.runtime_log_base_url,
            "runtime_log_upload_url": runtime_log_upload_url,
            "claimed_age_ms": task.claimed_at.elapsed().as_millis(),
        },
        "launch_process": launch_process.map(|launch| serde_json::json!({
            "task_id": launch.task_id,
            "place_id": launch.place_id,
            "universe_id": launch.universe_id,
            "studio_pid": launch.studio_pid,
            "launched_by_helper": launch.launched_by_helper,
        })),
        "remote_connection": remote_connection.map(|connection| serde_json::json!({
            "state": remote_connection_state_name(Some(connection)),
            "connection_id": connection.connection_id,
            "last_ready_age_ms": connection.last_ready_at.map(|value| value.elapsed().as_millis()),
            "last_server_message_age_ms": connection.last_server_message_at.map(|value| value.elapsed().as_millis()),
            "last_error": state.last_remote_errors.get(&connection_key),
        })),
        "runtime_log_forward": runtime_log_forward,
        "instances": instances,
        "task_status": task_status,
    });
    Ok(Json(payload).into_response())
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
    let local_base_url = rojo_forward_base_url(app.helper.port, &place_id, &claimed_task.task_id);
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

async fn resolve_rojo_forward_target_base_url(
    app: &AppState,
    place_id: &str,
    task_id: Option<&str>,
) -> Result<String> {
    let state = app.state.lock().await;
    let claimed_task = select_claimed_task(&state, place_id, task_id)?;
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
    rojo_forward_request(
        app,
        place_id,
        None,
        String::new(),
        method,
        uri,
        headers,
        body,
    )
    .await
}

async fn rojo_forward_path_handler(
    State(app): State<AppState>,
    AxumPath((place_id, path)): AxumPath<(String, String)>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, HelperError> {
    rojo_forward_request(app, place_id, None, path, method, uri, headers, body).await
}

async fn rojo_forward_task_root_handler(
    State(app): State<AppState>,
    AxumPath((place_id, task_id)): AxumPath<(String, String)>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, HelperError> {
    rojo_forward_request(
        app,
        place_id,
        Some(task_id),
        String::new(),
        method,
        uri,
        headers,
        body,
    )
    .await
}

async fn rojo_forward_task_path_handler(
    State(app): State<AppState>,
    AxumPath((place_id, task_id, path)): AxumPath<(String, String, String)>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, HelperError> {
    rojo_forward_request(
        app,
        place_id,
        Some(task_id),
        path,
        method,
        uri,
        headers,
        body,
    )
    .await
}

async fn rojo_forward_socket_handler(
    State(app): State<AppState>,
    AxumPath((place_id, cursor)): AxumPath<(String, String)>,
    uri: Uri,
    ws: WebSocketUpgrade,
) -> Result<Response, HelperError> {
    let place_id = sanitize_place_id(&place_id)?;
    let cursor = sanitize_identifier("cursor", &cursor)?;
    let target_base_url = resolve_rojo_forward_target_base_url(&app, &place_id, None).await?;
    let target_url = rojo_forward_target_ws_url(
        &target_base_url,
        &format!("api/socket/{cursor}"),
        uri.query(),
    )?;
    Ok(ws
        .on_upgrade(move |socket| {
            rojo_forward_socket_session(app, place_id, None, target_url, socket)
        })
        .into_response())
}

async fn rojo_forward_task_socket_handler(
    State(app): State<AppState>,
    AxumPath((place_id, task_id, cursor)): AxumPath<(String, String, String)>,
    uri: Uri,
    ws: WebSocketUpgrade,
) -> Result<Response, HelperError> {
    let place_id = sanitize_place_id(&place_id)?;
    let task_id = sanitize_identifier("task_id", &task_id)?;
    let cursor = sanitize_identifier("cursor", &cursor)?;
    let target_base_url =
        resolve_rojo_forward_target_base_url(&app, &place_id, Some(&task_id)).await?;
    let target_url = rojo_forward_target_ws_url(
        &target_base_url,
        &format!("api/socket/{cursor}"),
        uri.query(),
    )?;
    Ok(ws
        .on_upgrade(move |socket| {
            rojo_forward_socket_session(app, place_id, Some(task_id), target_url, socket)
        })
        .into_response())
}

async fn rojo_forward_socket_session(
    app: AppState,
    place_id: String,
    task_id: Option<String>,
    target_url: String,
    client_socket: WebSocket,
) {
    if let Err(error) = rojo_forward_socket_session_result(
        app,
        &place_id,
        task_id.as_deref(),
        &target_url,
        client_socket,
    )
    .await
    {
        tracing::warn!(
            place_id,
            task_id,
            target_url,
            error = %error,
            "Rojo websocket forward session ended with error"
        );
    }
}

async fn rojo_forward_socket_session_result(
    app: AppState,
    place_id: &str,
    task_id: Option<&str>,
    target_url: &str,
    client_socket: WebSocket,
) -> Result<()> {
    let remote_socket = connect_remote_ws(&app.helper, target_url)
        .await
        .wrap_err_with(|| format!("failed to connect Rojo websocket {target_url}"))?;
    let (mut client_sender, mut client_receiver) = client_socket.split();
    let (mut remote_sender, mut remote_receiver) = remote_socket.split();
    tracing::debug!(
        place_id,
        task_id,
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
    tracing::debug!(
        place_id,
        task_id,
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

#[allow(clippy::too_many_arguments)]
async fn rojo_forward_request(
    app: AppState,
    place_id: String,
    task_id: Option<String>,
    path: String,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, HelperError> {
    let place_id = sanitize_place_id(&place_id)?;
    let task_id = maybe_sanitize_identifier("task_id", task_id.as_deref())?;
    let target_base_url =
        resolve_rojo_forward_target_base_url(&app, &place_id, task_id.as_deref()).await?;
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
    let (status, content_type, bytes) =
        await_observable_upstream_result("rojo_forward_http", target_path.clone(), async move {
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
            Result::<(StatusCode, Option<String>, Bytes)>::Ok((status, content_type, bytes))
        })
        .await?;
    tracing::debug!(
        place_id,
        task_id,
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
    let runtime_log_remote_base_url = claimed_task.runtime_log_base_url.clone();
    let runtime_log_base_url = runtime_log_remote_base_url
        .as_ref()
        .map(|_| runtime_log_forward_base_url(app.helper.port, &place_id, &claimed_task.task_id));
    let runtime_log_upload_url = runtime_log_remote_base_url
        .as_ref()
        .map(|_| runtime_log_forward_upload_url(app.helper.port, &place_id, &claimed_task.task_id));
    {
        let mut state = app.state.lock().await;
        state.instances.insert(
            instance_id.clone(),
            PluginInstance {
                place_id: place_id.clone(),
                task_id: Some(claimed_task.task_id.clone()),
                remote_base_url: remote_base_url.clone(),
                studio_pid,
                studio_mode: None,
                studio_mode_observed_at: None,
                studio_mode_source: "none".to_owned(),
                studio_control_state: "none".to_owned(),
                studio_transition_phase: "idle".to_owned(),
                studio_transition_started_at: None,
                studio_control_observed_at: None,
                edit_runtime_observed_at: Some(Instant::now()),
                edit_runtime_unavailable_reason: None,
                studio_control_last_error: None,
                stop_request_id: 0,
                stop_request_recorded_at: None,
                stop_request_last_polled_at: None,
                stop_request_last_poll_id: None,
                stop_result_phase: None,
                stop_result_error: None,
                stop_result_observed_at: None,
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
        runtime_log_base_url,
        runtime_log_remote_base_url,
        runtime_log_upload_url,
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

async fn mcp_plugin_stop_request_handler(
    State(app): State<AppState>,
    Json(payload): Json<PluginStopRequestPayload>,
) -> Result<Response, HelperError> {
    let mut state = app.state.lock().await;
    let recorded = match record_stop_request_for_instance(&mut state, &payload.instance_id) {
        Ok(recorded) => recorded,
        Err(StopRequestRecordError::MissingInstance) => {
            return Err(HelperError(eyre!(
                "plugin instance expired: {}",
                payload.instance_id
            )));
        }
        Err(StopRequestRecordError::RuntimeActuatorUnavailable(detail)) => {
            tracing::warn!(
                instance_id = payload.instance_id,
                detail,
                "rejected MCP plugin stop request because runtime actuator is not fresh"
            );
            return Ok((
                StatusCode::CONFLICT,
                format!("runtime_stop_actuator_unavailable: {detail}"),
            )
                .into_response());
        }
    };
    if let Some(task_id) = recorded.task_id.as_deref() {
        queue_task_status_updates(&state, &app.helper, task_id);
    }
    tracing::info!(
        instance_id = payload.instance_id,
        place_id = recorded.place_id,
        task_id = ?recorded.task_id,
        stop_request_id = recorded.stop_request_id,
        "recorded MCP plugin stop request"
    );
    Ok(Json(PluginStopRequestResponse {
        stop_requested: true,
        stop_request_id: recorded.stop_request_id,
    })
    .into_response())
}

async fn mcp_plugin_stop_request_poll_handler(
    State(app): State<AppState>,
    Query(query): Query<PluginStopRequestQuery>,
) -> Result<Response, HelperError> {
    let mut state = app.state.lock().await;
    let (stop_requested, stop_request_id, task_id) = {
        let Some(instance) = state.instances.get_mut(&query.instance_id) else {
            return Ok((StatusCode::GONE, "instance expired").into_response());
        };
        let after_id = query.after_id.unwrap_or(0);
        let stop_request_id = instance.stop_request_id;
        let stop_requested = stop_request_id > after_id
            && is_stop_request_deliverable_phase(&instance.studio_transition_phase);
        instance.stop_request_last_polled_at = Some(Instant::now());
        instance.stop_request_last_poll_id = Some(if stop_requested {
            stop_request_id
        } else {
            after_id
        });
        (stop_requested, stop_request_id, instance.task_id.clone())
    };
    if let Some(task_id) = task_id.as_deref() {
        queue_task_status_updates(&state, &app.helper, task_id);
    }
    Ok(Json(PluginStopRequestResponse {
        stop_requested,
        stop_request_id,
    })
    .into_response())
}

async fn mcp_plugin_stop_result_handler(
    State(app): State<AppState>,
    Json(payload): Json<PluginStopResultPayload>,
) -> Result<Response, HelperError> {
    let mut state = app.state.lock().await;
    let (place_id, task_id, should_push_status) = {
        let Some(instance) = state.instances.get_mut(&payload.instance_id) else {
            return Ok((StatusCode::GONE, "instance expired").into_response());
        };
        if instance.stop_request_id != payload.stop_request_id {
            tracing::warn!(
                instance_id = payload.instance_id,
                stop_request_id = payload.stop_request_id,
                current_stop_request_id = instance.stop_request_id,
                phase = payload.phase,
                "ignored stale MCP plugin stop result"
            );
            return Ok(StatusCode::NO_CONTENT.into_response());
        }
        if !is_stopping_transition_phase(&instance.studio_transition_phase) {
            tracing::warn!(
                instance_id = payload.instance_id,
                stop_request_id = payload.stop_request_id,
                phase = payload.phase,
                current_phase = instance.studio_transition_phase,
                "ignored MCP plugin stop result outside stopping phase"
            );
            return Ok(StatusCode::NO_CONTENT.into_response());
        }

        let previous_phase = instance.studio_transition_phase.clone();
        match payload.phase.as_str() {
            "observed" => {
                instance.studio_control_state = "stopping".to_owned();
                set_studio_transition_phase(instance, "stopping_observed");
                instance.studio_control_last_error = None;
                instance.stop_result_phase = Some("observed".to_owned());
                instance.stop_result_error = None;
                instance.stop_result_observed_at = Some(Instant::now());
            }
            "failed" => {
                let message =
                    format!(
                    "runtime_stop_failed: play runtime failed to execute stop_request_id={}: {}",
                    payload.stop_request_id,
                    payload.error.as_deref().unwrap_or("unknown runtime stop failure")
                );
                instance.studio_control_state = "lost".to_owned();
                set_studio_transition_phase(instance, "error");
                instance.studio_control_observed_at = None;
                instance.studio_control_last_error = Some(message);
                instance.stop_result_phase = Some("failed".to_owned());
                instance.stop_result_error = instance.studio_control_last_error.clone();
                instance.stop_result_observed_at = Some(Instant::now());
            }
            _ => return Ok((StatusCode::BAD_REQUEST, "invalid stop result phase").into_response()),
        }
        instance.last_seen_at = Instant::now();
        (
            instance.place_id.clone(),
            instance.task_id.clone(),
            previous_phase != instance.studio_transition_phase,
        )
    };
    if should_push_status {
        if let Some(task_id) = task_id.as_deref() {
            queue_task_status_updates(&state, &app.helper, task_id);
        }
    }
    tracing::info!(
        instance_id = payload.instance_id,
        place_id,
        task_id = ?task_id,
        stop_request_id = payload.stop_request_id,
        phase = payload.phase,
        "accepted MCP plugin stop result"
    );
    Ok(StatusCode::NO_CONTENT.into_response())
}

async fn mcp_plugin_control_heartbeat_handler(
    State(app): State<AppState>,
    Json(payload): Json<PluginControlHeartbeatRequest>,
) -> Result<Response, HelperError> {
    let Some(mode) = normalize_studio_mode(Some(payload.mode.as_str())) else {
        return Ok((StatusCode::BAD_REQUEST, "invalid studio mode").into_response());
    };
    if !studio_mode_is_running(Some(mode.as_str())) {
        return Ok((
            StatusCode::BAD_REQUEST,
            "control heartbeat requires a running mode",
        )
            .into_response());
    }
    let mut state = app.state.lock().await;
    let (place_id, task_id, should_push_status) = {
        let Some(instance) = state.instances.get_mut(&payload.instance_id) else {
            return Ok((StatusCode::GONE, "instance expired").into_response());
        };
        let previous_control_state = effective_studio_control_state(instance);
        let previous_transition_phase = effective_studio_transition_phase(instance);
        instance.studio_control_observed_at = Some(Instant::now());
        if is_error_transition_phase(&instance.studio_transition_phase) {
            instance.studio_control_state = "lost".to_owned();
        } else if is_stopping_transition_phase(&instance.studio_transition_phase) {
            instance.studio_control_state = "stopping".to_owned();
        } else {
            instance.studio_control_state = "ready".to_owned();
            set_studio_transition_phase(instance, "running");
            instance.studio_control_last_error = None;
            set_edit_runtime_unavailable(instance, "runtime");
        }
        instance.last_seen_at = Instant::now();
        (
            instance.place_id.clone(),
            instance.task_id.clone(),
            previous_control_state != "ready" || previous_transition_phase != "running",
        )
    };
    if should_push_status {
        if let Some(task_id) = task_id.as_deref() {
            queue_task_status_updates(&state, &app.helper, task_id);
        }
    }
    tracing::debug!(
        instance_id = payload.instance_id,
        place_id,
        task_id = ?task_id,
        studio_mode = mode,
        "accepted MCP plugin control heartbeat"
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
        Err(_) => Ok(StatusCode::NO_CONTENT.into_response()),
    }
}

async fn mcp_plugin_edit_heartbeat_handler(
    State(app): State<AppState>,
    Json(payload): Json<PluginEditHeartbeatRequest>,
) -> Result<Response, HelperError> {
    let mut state = app.state.lock().await;
    let task_id = {
        let Some(instance) = state.instances.get_mut(&payload.instance_id) else {
            tracing::warn!(
                instance_id = payload.instance_id,
                "rejected MCP plugin edit heartbeat for missing instance"
            );
            return Ok((StatusCode::GONE, "instance expired").into_response());
        };
        if is_stopping_transition_phase(&instance.studio_transition_phase)
            && instance.stop_request_id > 0
        {
            instance.edit_runtime_observed_at = Some(Instant::now());
            if instance.stop_result_phase.as_deref() == Some("observed")
                && runtime_control_heartbeat_is_stale(instance)
            {
                instance.studio_control_state = "none".to_owned();
                set_studio_transition_phase(instance, "idle");
                instance.studio_control_observed_at = None;
                instance.studio_control_last_error = None;
                instance.stop_result_phase = Some("completed".to_owned());
                instance.stop_result_error = None;
                instance.stop_result_observed_at = Some(Instant::now());
                set_edit_runtime_available(instance);
            } else {
                set_edit_runtime_unavailable(instance, "stopping");
            }
        } else {
            match instance.edit_runtime_unavailable_reason.as_deref() {
                Some("launching") => {
                    instance.edit_runtime_observed_at = Some(Instant::now());
                }
                Some("launch_failed")
                    if runtime_control_heartbeat_expired_after_observation(instance) =>
                {
                    instance.studio_control_state = "none".to_owned();
                    set_studio_transition_phase(instance, "idle");
                    instance.studio_control_observed_at = None;
                    instance.studio_control_last_error = None;
                    set_edit_runtime_available(instance);
                }
                Some("launch_failed") => {
                    instance.edit_runtime_observed_at = Some(Instant::now());
                }
                Some("runtime")
                    if runtime_control_heartbeat_expired_after_observation(instance) =>
                {
                    instance.studio_control_state = "none".to_owned();
                    set_studio_transition_phase(instance, "idle");
                    instance.studio_control_observed_at = None;
                    instance.studio_control_last_error = None;
                    set_edit_runtime_available(instance);
                }
                Some("runtime") => {
                    instance.edit_runtime_observed_at = Some(Instant::now());
                    set_edit_runtime_unavailable(instance, "runtime");
                }
                _ => {
                    set_edit_runtime_available(instance);
                }
            }
        }
        instance.last_seen_at = Instant::now();
        instance.task_id.clone()
    };
    if let Some(task_id) = task_id {
        queue_task_status_updates(&state, &app.helper, &task_id);
    }
    if let Some(instance) = state.instances.get(&payload.instance_id) {
        tracing::debug!(
            instance_id = payload.instance_id,
            place_id = instance.place_id,
            task_id = ?instance.task_id,
            studio_pid = ?instance.studio_pid,
            edit_runtime_age_ms = instance
                .edit_runtime_observed_at
                .map(|value| value.elapsed().as_millis()),
            "accepted MCP plugin edit heartbeat"
        );
    }
    Ok(StatusCode::NO_CONTENT.into_response())
}

async fn mcp_plugin_response_handler(
    State(app): State<AppState>,
    Json(payload): Json<PluginResponsePayload>,
) -> Result<Response, HelperError> {
    let tx = {
        let mut state = app.state.lock().await;
        let pending = state.waiting_for_plugin.remove(&payload.id);
        if pending.is_some() {
            let pending_tool_name = pending
                .as_ref()
                .map(|pending| pending.tool_name.clone())
                .unwrap_or_else(|| "unknown".to_owned());
            if let Some(instance_id) = payload.instance_id.as_deref() {
                if let Some(instance) = state.instances.get(instance_id) {
                    if let Some(task_id) = instance.task_id.clone() {
                        queue_task_status_updates(&state, &app.helper, &task_id);
                    }
                    tracing::info!(
                        instance_id,
                        place_id = instance.place_id,
                        task_id = ?instance.task_id,
                        id = payload.id,
                        tool = pending_tool_name.as_str(),
                        success = payload.success,
                        response_bytes = payload.response.len(),
                        "helper received MCP plugin tool response"
                    );
                } else {
                    tracing::warn!(
                        instance_id,
                        id = payload.id,
                        tool = pending_tool_name.as_str(),
                        success = payload.success,
                        response_bytes = payload.response.len(),
                        "helper received MCP plugin response for missing instance"
                    );
                }
            }
        }
        pending.map(|pending| pending.response_tx)
    };
    if let Some(tx) = tx {
        let _ = tx.send(payload.clone());
        tracing::info!(
            id = payload.id,
            instance_id = ?payload.instance_id,
            success = payload.success,
            response_bytes = payload.response.len(),
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

fn studio_mode_is_running(mode: Option<&str>) -> bool {
    matches!(mode, Some("start_play") | Some("run_server"))
}

#[derive(Clone, Debug)]
struct StudioTaskStatusSnapshot {
    studio_session_state: String,
    last_known_session_state: Option<String>,
    last_session_error_reason: Option<String>,
    studio_mode: Option<String>,
    studio_mode_age_ms: Option<u128>,
    studio_mode_source: String,
    studio_control_state: String,
    studio_transition_phase: String,
    studio_transition_age_ms: Option<u128>,
    edit_runtime_state: String,
    edit_runtime_age_ms: Option<u128>,
    studio_control_last_error: Option<String>,
    active_stop_request_id: Option<u64>,
    last_stop_request_id: u64,
    stop_request_recorded_age_ms: Option<u128>,
    runtime_actuator_last_poll_id: Option<u64>,
    runtime_actuator_last_poll_age_ms: Option<u128>,
    stop_result_phase: Option<String>,
    stop_result_age_ms: Option<u128>,
    stop_result_error: Option<String>,
}

#[derive(Clone, Debug)]
struct StudioStopRequestSnapshot {
    active_stop_request_id: Option<u64>,
    last_stop_request_id: u64,
    stop_request_recorded_age_ms: Option<u128>,
    runtime_actuator_last_poll_id: Option<u64>,
    runtime_actuator_last_poll_age_ms: Option<u128>,
    stop_result_phase: Option<String>,
    stop_result_age_ms: Option<u128>,
    stop_result_error: Option<String>,
}

struct RecordedStopRequest {
    place_id: String,
    task_id: Option<String>,
    stop_request_id: u64,
}

enum StopRequestRecordError {
    MissingInstance,
    RuntimeActuatorUnavailable(String),
}

fn is_stopping_transition_phase(phase: &str) -> bool {
    phase == "stopping_requested" || phase == "stopping_observed"
}

fn active_stop_request_id_for_instance(instance: &PluginInstance) -> Option<u64> {
    if instance.stop_request_id > 0
        && is_stopping_transition_phase(&instance.studio_transition_phase)
    {
        Some(instance.stop_request_id)
    } else {
        None
    }
}

fn stop_request_snapshot_for_instance(instance: &PluginInstance) -> StudioStopRequestSnapshot {
    StudioStopRequestSnapshot {
        active_stop_request_id: active_stop_request_id_for_instance(instance),
        last_stop_request_id: instance.stop_request_id,
        stop_request_recorded_age_ms: instance
            .stop_request_recorded_at
            .map(|value| value.elapsed().as_millis()),
        runtime_actuator_last_poll_id: instance.stop_request_last_poll_id,
        runtime_actuator_last_poll_age_ms: instance
            .stop_request_last_polled_at
            .map(|value| value.elapsed().as_millis()),
        stop_result_phase: instance.stop_result_phase.clone(),
        stop_result_age_ms: instance
            .stop_result_observed_at
            .map(|value| value.elapsed().as_millis()),
        stop_result_error: instance.stop_result_error.clone(),
    }
}

fn empty_stop_request_snapshot() -> StudioStopRequestSnapshot {
    StudioStopRequestSnapshot {
        active_stop_request_id: None,
        last_stop_request_id: 0,
        stop_request_recorded_age_ms: None,
        runtime_actuator_last_poll_id: None,
        runtime_actuator_last_poll_age_ms: None,
        stop_result_phase: None,
        stop_result_age_ms: None,
        stop_result_error: None,
    }
}

fn is_stop_request_deliverable_phase(phase: &str) -> bool {
    phase == "stopping_requested"
}

fn is_error_transition_phase(phase: &str) -> bool {
    phase == "error"
}

fn set_edit_runtime_available(instance: &mut PluginInstance) {
    instance.edit_runtime_observed_at = Some(Instant::now());
    instance.edit_runtime_unavailable_reason = None;
}

fn set_edit_runtime_unavailable(instance: &mut PluginInstance, reason: &str) {
    instance.edit_runtime_unavailable_reason = Some(reason.to_owned());
}

fn edit_unavailable_reason_priority(reason: &str) -> u8 {
    match reason {
        "stopping" => 4,
        "launching" => 3,
        "runtime" => 2,
        "launch_failed" => 1,
        _ => 0,
    }
}

fn current_session_state_for_instance(instance: &PluginInstance) -> Option<&'static str> {
    let transition_phase = effective_studio_transition_phase(instance);
    if is_stopping_transition_phase(&transition_phase) {
        return Some("stopping");
    }
    if instance.edit_runtime_unavailable_reason.as_deref() == Some("launching") {
        return Some("launching");
    }
    if instance.edit_runtime_unavailable_reason.as_deref() == Some("launch_failed") {
        return Some("launch_failed");
    }
    None
}

fn last_known_session_state_for_instance(instance: &PluginInstance) -> Option<&'static str> {
    let transition_phase = effective_studio_transition_phase(instance);
    if is_stopping_transition_phase(&transition_phase) {
        return Some("stopping");
    }
    if instance.edit_runtime_unavailable_reason.as_deref() == Some("launching") {
        return Some("launching");
    }
    if instance.edit_runtime_unavailable_reason.as_deref() == Some("launch_failed") {
        return Some("launch_failed");
    }
    None
}

fn reported_session_state_for_instance(instance: &PluginInstance) -> &'static str {
    current_session_state_for_instance(instance).unwrap_or_else(|| {
        match edit_runtime_state(instance).as_str() {
            "ready" => "edit_connected",
            "runtime" => "edit_suspended_by_runtime",
            "launch_failed" => "launch_failed",
            _ => "none_response",
        }
    })
}

fn task_studio_session_state_snapshot(
    state: &HelperState,
    task_id: &str,
) -> (String, Option<String>, Option<String>) {
    let mut has_instance = false;
    let mut has_stopping = false;
    let mut has_launching = false;
    let mut has_launch_failed = false;
    let mut has_runtime = false;
    let mut has_play = false;
    let mut has_starting = false;
    let mut has_stop = false;
    let mut has_edit_connected = false;
    let mut newest_known: Option<(&'static str, u128)> = None;
    let mut newest_error: Option<(String, u128)> = None;

    for instance in state
        .instances
        .values()
        .filter(|instance| instance.task_id.as_deref() == Some(task_id))
    {
        has_instance = true;
        match current_session_state_for_instance(instance) {
            Some("stopping") => has_stopping = true,
            Some("launching") => has_launching = true,
            Some("launch_failed") => has_launch_failed = true,
            Some("play") => has_play = true,
            Some("starting_play") => has_starting = true,
            Some("stop") => has_stop = true,
            _ => {}
        }
        match edit_runtime_state(instance).as_str() {
            "ready" => has_edit_connected = true,
            "runtime" => has_runtime = true,
            _ => {}
        }
        if let Some(known) = last_known_session_state_for_instance(instance) {
            let age = instance
                .studio_mode_observed_at
                .or(instance.studio_transition_started_at)
                .map(|observed_at| observed_at.elapsed().as_millis())
                .unwrap_or(u128::MAX);
            if newest_known
                .as_ref()
                .map(|(_, current_age)| age < *current_age)
                .unwrap_or(true)
            {
                newest_known = Some((known, age));
            }
        }
        if let Some(error) = instance.studio_control_last_error.as_ref() {
            let age = instance
                .studio_transition_started_at
                .or(instance.studio_control_observed_at)
                .or(instance.studio_mode_observed_at)
                .map(|observed_at| observed_at.elapsed().as_millis())
                .unwrap_or(u128::MAX);
            if newest_error
                .as_ref()
                .map(|(_, current_age)| age < *current_age)
                .unwrap_or(true)
            {
                newest_error = Some((error.clone(), age));
            }
        }
    }

    let studio_session_state = if !has_instance {
        "none_connected"
    } else if has_stopping {
        "stopping"
    } else if has_launching {
        "launching"
    } else if has_launch_failed {
        "launch_failed"
    } else if has_runtime {
        "edit_suspended_by_runtime"
    } else if has_play {
        "play"
    } else if has_starting {
        "starting_play"
    } else if has_stop {
        "stop"
    } else if has_edit_connected {
        "edit_connected"
    } else {
        "none_response"
    }
    .to_owned();

    let last_known_session_state = if matches!(
        studio_session_state.as_str(),
        "stop"
            | "starting_play"
            | "play"
            | "stopping"
            | "launching"
            | "launch_failed"
            | "edit_suspended_by_runtime"
            | "edit_connected"
    ) {
        Some(studio_session_state.clone())
    } else {
        newest_known.map(|(state, _)| state.to_owned())
    };

    let last_session_error_reason =
        newest_error
            .map(|(error, _)| error)
            .or_else(|| match studio_session_state.as_str() {
                "none_connected" => Some("studio_plugin_not_connected".to_owned()),
                "none_response" => Some("studio_plugin_no_response".to_owned()),
                _ => None,
            });

    (
        studio_session_state,
        last_known_session_state,
        last_session_error_reason,
    )
}

fn set_studio_transition_phase(instance: &mut PluginInstance, phase: &str) {
    if instance.studio_transition_phase != phase {
        instance.studio_transition_started_at = if phase == "idle" || phase == "running" {
            None
        } else {
            Some(Instant::now())
        };
    }
    instance.studio_transition_phase = phase.to_owned();
}

fn instance_has_fresh_control(instance: &PluginInstance) -> bool {
    instance.studio_control_state == "ready"
        && instance
            .studio_control_observed_at
            .map(|observed_at| observed_at.elapsed() <= STUDIO_CONTROL_HEARTBEAT_STALE_AFTER)
            .unwrap_or(false)
}

fn runtime_control_heartbeat_is_stale(instance: &PluginInstance) -> bool {
    instance
        .studio_control_observed_at
        .map(|observed_at| observed_at.elapsed() > STUDIO_CONTROL_HEARTBEAT_STALE_AFTER)
        .unwrap_or(true)
}

fn runtime_control_heartbeat_expired_after_observation(instance: &PluginInstance) -> bool {
    instance
        .studio_control_observed_at
        .map(|observed_at| observed_at.elapsed() > STUDIO_CONTROL_HEARTBEAT_STALE_AFTER)
        .unwrap_or(false)
}

fn effective_studio_control_state(instance: &PluginInstance) -> String {
    let transition_phase = effective_studio_transition_phase(instance);
    if is_error_transition_phase(&transition_phase) {
        return "lost".to_owned();
    }
    if is_stopping_transition_phase(&transition_phase) {
        return "stopping".to_owned();
    }
    if instance_has_fresh_control(instance) {
        "ready".to_owned()
    } else if instance.studio_control_state == "none" && transition_phase == "idle" {
        "none".to_owned()
    } else {
        "lost".to_owned()
    }
}

fn effective_studio_transition_phase(instance: &PluginInstance) -> String {
    if is_error_transition_phase(&instance.studio_transition_phase)
        || is_stopping_transition_phase(&instance.studio_transition_phase)
    {
        return instance.studio_transition_phase.clone();
    }
    if instance_has_fresh_control(instance) {
        "running".to_owned()
    } else {
        instance.studio_transition_phase.clone()
    }
}

fn record_stop_request_for_instance(
    state: &mut HelperState,
    instance_id: &str,
) -> Result<RecordedStopRequest, StopRequestRecordError> {
    let Some(instance) = state.instances.get_mut(instance_id) else {
        return Err(StopRequestRecordError::MissingInstance);
    };
    if !instance_has_fresh_control(instance) {
        return Err(StopRequestRecordError::RuntimeActuatorUnavailable(
            runtime_actuator_unavailable_detail(instance),
        ));
    }
    instance.stop_request_id = instance.stop_request_id.saturating_add(1);
    instance.studio_control_state = "stopping".to_owned();
    set_studio_transition_phase(instance, "stopping_requested");
    instance.studio_control_last_error = None;
    instance.stop_request_recorded_at = Some(Instant::now());
    instance.stop_request_last_polled_at = None;
    instance.stop_request_last_poll_id = None;
    instance.stop_result_phase = None;
    instance.stop_result_error = None;
    instance.stop_result_observed_at = None;
    set_edit_runtime_unavailable(instance, "stopping");
    instance.last_seen_at = Instant::now();
    Ok(RecordedStopRequest {
        place_id: instance.place_id.clone(),
        task_id: instance.task_id.clone(),
        stop_request_id: instance.stop_request_id,
    })
}

fn runtime_actuator_unavailable_detail(instance: &PluginInstance) -> String {
    format!(
        "studio_control_state={} studio_transition_phase={} control_age_ms={:?} edit_runtime_state={} unavailable_reason={:?}",
        instance.studio_control_state,
        instance.studio_transition_phase,
        instance
            .studio_control_observed_at
            .map(|observed_at| observed_at.elapsed().as_millis()),
        edit_runtime_state(instance),
        instance.edit_runtime_unavailable_reason
    )
}

fn mark_runtime_stop_timeout(instance: &mut PluginInstance, stop_request_id: u64, detail: String) {
    instance.studio_control_state = "lost".to_owned();
    set_studio_transition_phase(instance, "error");
    instance.studio_control_last_error = Some(format!(
        "runtime_stop_timeout: stop_request_id={stop_request_id} {detail}"
    ));
    instance.stop_result_phase = Some("failed".to_owned());
    instance.stop_result_error = instance.studio_control_last_error.clone();
    instance.stop_result_observed_at = Some(Instant::now());
    set_edit_runtime_unavailable(instance, "runtime");
    instance.last_seen_at = Instant::now();
}

#[cfg(test)]
fn initial_control_state_for_mode(mode: Option<&str>) -> (String, String, Option<Instant>) {
    match mode {
        None | Some("stop") => ("none".to_owned(), "idle".to_owned(), None),
        Some("start_play") | Some("run_server") => ("lost".to_owned(), "starting".to_owned(), None),
        _ => ("lost".to_owned(), "error".to_owned(), None),
    }
}

fn edit_runtime_state(instance: &PluginInstance) -> String {
    if let Some(reason) = instance.edit_runtime_unavailable_reason.as_deref() {
        return reason.to_owned();
    }
    let Some(observed_at) = instance.edit_runtime_observed_at else {
        return "missing".to_owned();
    };
    if observed_at.elapsed() <= EDIT_RUNTIME_STALE_AFTER {
        "ready".to_owned()
    } else {
        "stale".to_owned()
    }
}

fn task_edit_runtime_snapshot<'a>(
    instances: impl Iterator<Item = &'a PluginInstance>,
) -> (String, Option<u128>) {
    let mut has_instance = false;
    let mut unavailable_reason: Option<String> = None;
    let mut newest_edit_age_ms: Option<u128> = None;

    for instance in instances {
        has_instance = true;
        if let Some(reason) = instance.edit_runtime_unavailable_reason.as_ref() {
            if unavailable_reason
                .as_deref()
                .map(|current| {
                    edit_unavailable_reason_priority(reason)
                        > edit_unavailable_reason_priority(current)
                })
                .unwrap_or(true)
            {
                unavailable_reason = Some(reason.clone());
            }
        }
        if let Some(observed_at) = instance.edit_runtime_observed_at {
            let age_ms = observed_at.elapsed().as_millis();
            if newest_edit_age_ms
                .map(|current| age_ms < current)
                .unwrap_or(true)
            {
                newest_edit_age_ms = Some(age_ms);
            }
        }
    }

    let state = if !has_instance {
        "missing"
    } else if let Some(reason) = unavailable_reason.as_deref() {
        reason
    } else if newest_edit_age_ms
        .map(|age_ms| age_ms <= EDIT_RUNTIME_STALE_AFTER.as_millis())
        .unwrap_or(false)
    {
        "ready"
    } else if newest_edit_age_ms.is_some() {
        "stale"
    } else {
        "missing"
    };

    (state.to_owned(), newest_edit_age_ms)
}

fn task_studio_mode_snapshot(state: &HelperState, task_id: &str) -> StudioTaskStatusSnapshot {
    let (studio_session_state, last_known_session_state, last_session_error_reason) =
        task_studio_session_state_snapshot(state, task_id);
    let stop_snapshot = state
        .instances
        .values()
        .filter(|instance| instance.task_id.as_deref() == Some(task_id))
        .max_by_key(|instance| {
            (
                active_stop_request_id_for_instance(instance).is_some(),
                instance.stop_request_id,
                instance.last_seen_at,
            )
        })
        .map(stop_request_snapshot_for_instance)
        .unwrap_or_else(empty_stop_request_snapshot);
    let (task_edit_runtime_state, task_edit_runtime_age_ms) = task_edit_runtime_snapshot(
        state
            .instances
            .values()
            .filter(|instance| instance.task_id.as_deref() == Some(task_id)),
    );
    state
        .instances
        .values()
        .filter(|instance| instance.task_id.as_deref() == Some(task_id))
        .max_by_key(|instance| instance.last_seen_at)
        .map(|instance| {
            (
                instance.studio_mode.clone(),
                instance
                    .studio_mode_observed_at
                    .map(|observed_at| observed_at.elapsed().as_millis()),
                instance.studio_mode_source.clone(),
                effective_studio_control_state(instance),
                effective_studio_transition_phase(instance),
                instance
                    .studio_transition_started_at
                    .map(|value| value.elapsed().as_millis()),
                instance.studio_control_last_error.clone(),
            )
        })
        .map(
            |(
                mode,
                age,
                mode_source,
                control_state,
                transition_phase,
                transition_age_ms,
                control_last_error,
            )| StudioTaskStatusSnapshot {
                studio_session_state: studio_session_state.clone(),
                last_known_session_state: last_known_session_state.clone(),
                last_session_error_reason: last_session_error_reason.clone(),
                studio_mode: mode,
                studio_mode_age_ms: age,
                studio_mode_source: mode_source,
                studio_control_state: control_state,
                studio_transition_phase: transition_phase,
                studio_transition_age_ms: transition_age_ms,
                edit_runtime_state: task_edit_runtime_state.clone(),
                edit_runtime_age_ms: task_edit_runtime_age_ms,
                studio_control_last_error: control_last_error,
                active_stop_request_id: stop_snapshot.active_stop_request_id,
                last_stop_request_id: stop_snapshot.last_stop_request_id,
                stop_request_recorded_age_ms: stop_snapshot.stop_request_recorded_age_ms,
                runtime_actuator_last_poll_id: stop_snapshot.runtime_actuator_last_poll_id,
                runtime_actuator_last_poll_age_ms: stop_snapshot.runtime_actuator_last_poll_age_ms,
                stop_result_phase: stop_snapshot.stop_result_phase.clone(),
                stop_result_age_ms: stop_snapshot.stop_result_age_ms,
                stop_result_error: stop_snapshot.stop_result_error.clone(),
            },
        )
        .unwrap_or(StudioTaskStatusSnapshot {
            studio_session_state,
            last_known_session_state,
            last_session_error_reason,
            studio_mode: None,
            studio_mode_age_ms: None,
            studio_mode_source: "none".to_owned(),
            studio_control_state: "none".to_owned(),
            studio_transition_phase: "idle".to_owned(),
            studio_transition_age_ms: None,
            edit_runtime_state: task_edit_runtime_state,
            edit_runtime_age_ms: task_edit_runtime_age_ms,
            studio_control_last_error: None,
            active_stop_request_id: stop_snapshot.active_stop_request_id,
            last_stop_request_id: stop_snapshot.last_stop_request_id,
            stop_request_recorded_age_ms: stop_snapshot.stop_request_recorded_age_ms,
            runtime_actuator_last_poll_id: stop_snapshot.runtime_actuator_last_poll_id,
            runtime_actuator_last_poll_age_ms: stop_snapshot.runtime_actuator_last_poll_age_ms,
            stop_result_phase: stop_snapshot.stop_result_phase,
            stop_result_age_ms: stop_snapshot.stop_result_age_ms,
            stop_result_error: stop_snapshot.stop_result_error,
        })
}

fn task_official_adapter_snapshot(
    state: &HelperState,
    task_id: &str,
    edit_runtime_ready: bool,
) -> (String, Option<u128>, Option<String>) {
    let Some(record) = state.official_adapter_states.get(task_id) else {
        return ("not_started".to_owned(), None, None);
    };
    let state_name = if record.state == "ready" {
        if edit_runtime_ready {
            "ready".to_owned()
        } else {
            "stale".to_owned()
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

fn ws_age_ms(value: Option<u128>) -> Option<u64> {
    value.map(|age| age.min(u64::MAX as u128) as u64)
}

fn runtime_log_forward_status_snapshot(
    state: &HelperState,
    task_id: &str,
) -> RuntimeLogForwardStatusSnapshot {
    let record = state
        .runtime_log_forward_statuses
        .get(task_id)
        .cloned()
        .unwrap_or_default();
    let state_name = if record.queued_count > 0 {
        "forwarding"
    } else if record.last_error.is_some() {
        "error"
    } else if record.forwarded_count > 0 {
        "ready"
    } else {
        "idle"
    };
    RuntimeLogForwardStatusSnapshot {
        state: state_name.to_owned(),
        queued_count: record.queued_count,
        accepted_count: record.accepted_count,
        forwarded_count: record.forwarded_count,
        failed_count: record.failed_count,
        last_accepted_age_ms: ws_age_ms(
            record
                .last_accepted_at
                .map(|value| value.elapsed().as_millis()),
        ),
        last_forwarded_age_ms: ws_age_ms(
            record
                .last_forwarded_at
                .map(|value| value.elapsed().as_millis()),
        ),
        last_attempt_age_ms: ws_age_ms(
            record
                .last_attempt_at
                .map(|value| value.elapsed().as_millis()),
        ),
        last_target_path: record.last_target_path,
        last_http_status: record.last_http_status,
        last_error: record.last_error,
        last_error_age_ms: ws_age_ms(
            record
                .last_error_at
                .map(|value| value.elapsed().as_millis()),
        ),
    }
}

fn mark_runtime_log_forward_accepted(state: &mut HelperState, task_id: &str, target_path: String) {
    let record = state
        .runtime_log_forward_statuses
        .entry(task_id.to_owned())
        .or_default();
    record.queued_count = record.queued_count.saturating_add(1);
    record.accepted_count = record.accepted_count.saturating_add(1);
    record.last_accepted_at = Some(Instant::now());
    record.last_target_path = Some(target_path);
    record.last_http_status = None;
}

fn mark_runtime_log_forward_succeeded(
    state: &mut HelperState,
    task_id: &str,
    target_path: String,
    http_status: u16,
) {
    let record = state
        .runtime_log_forward_statuses
        .entry(task_id.to_owned())
        .or_default();
    record.queued_count = record.queued_count.saturating_sub(1);
    record.forwarded_count = record.forwarded_count.saturating_add(1);
    record.last_forwarded_at = Some(Instant::now());
    record.last_attempt_at = record.last_forwarded_at;
    record.last_target_path = Some(target_path);
    record.last_http_status = Some(http_status);
    record.last_error = None;
    record.last_error_at = None;
}

fn mark_runtime_log_forward_failed(
    state: &mut HelperState,
    task_id: &str,
    target_path: String,
    http_status: Option<u16>,
    error: String,
) {
    let record = state
        .runtime_log_forward_statuses
        .entry(task_id.to_owned())
        .or_default();
    record.queued_count = record.queued_count.saturating_sub(1);
    record.failed_count = record.failed_count.saturating_add(1);
    record.last_attempt_at = Some(Instant::now());
    record.last_target_path = Some(target_path);
    record.last_http_status = http_status;
    record.last_error = Some(error);
    record.last_error_at = record.last_attempt_at;
}

fn runtime_log_forward_task_is_current(
    state: &HelperState,
    task_id: &str,
    place_id: &str,
    claimed_at: Instant,
) -> bool {
    state
        .claimed_tasks
        .get(task_id)
        .map(|task| task.place_id == place_id && task.claimed_at == claimed_at)
        .unwrap_or(false)
}

fn mark_runtime_log_forward_succeeded_if_current(
    state: &mut HelperState,
    task_id: &str,
    place_id: &str,
    claimed_at: Instant,
    target_path: String,
    http_status: u16,
) -> bool {
    if !runtime_log_forward_task_is_current(state, task_id, place_id, claimed_at) {
        return false;
    }
    mark_runtime_log_forward_succeeded(state, task_id, target_path, http_status);
    true
}

fn mark_runtime_log_forward_failed_if_current(
    state: &mut HelperState,
    task_id: &str,
    place_id: &str,
    claimed_at: Instant,
    target_path: String,
    http_status: Option<u16>,
    error: String,
) -> bool {
    if !runtime_log_forward_task_is_current(state, task_id, place_id, claimed_at) {
        return false;
    }
    mark_runtime_log_forward_failed(state, task_id, target_path, http_status, error);
    true
}

fn mark_runtime_log_forward_rejected(
    state: &mut HelperState,
    task_id: &str,
    target_path: String,
    error: String,
) {
    let record = state
        .runtime_log_forward_statuses
        .entry(task_id.to_owned())
        .or_default();
    record.failed_count = record.failed_count.saturating_add(1);
    record.last_attempt_at = Some(Instant::now());
    record.last_target_path = Some(target_path);
    record.last_http_status = None;
    record.last_error = Some(error);
    record.last_error_at = record.last_attempt_at;
}

fn helper_task_status_snapshot(state: &HelperState, task_id: &str) -> HelperTaskStatusSnapshot {
    let studio_snapshot = task_studio_mode_snapshot(state, task_id);
    let (official_state, official_age_ms, official_last_error) = task_official_adapter_snapshot(
        state,
        task_id,
        studio_snapshot.edit_runtime_state == "ready",
    );
    HelperTaskStatusSnapshot {
        task_id: task_id.to_owned(),
        studio_session_state: Some(studio_snapshot.studio_session_state),
        last_known_session_state: studio_snapshot.last_known_session_state,
        last_session_error_reason: studio_snapshot.last_session_error_reason,
        studio_mode: studio_snapshot.studio_mode,
        studio_mode_age_ms: ws_age_ms(studio_snapshot.studio_mode_age_ms),
        studio_mode_source: Some(studio_snapshot.studio_mode_source),
        studio_control_state: Some(studio_snapshot.studio_control_state),
        studio_transition_phase: Some(studio_snapshot.studio_transition_phase),
        studio_transition_age_ms: ws_age_ms(studio_snapshot.studio_transition_age_ms),
        edit_runtime_state: Some(studio_snapshot.edit_runtime_state),
        edit_runtime_age_ms: ws_age_ms(studio_snapshot.edit_runtime_age_ms),
        studio_control_last_error: studio_snapshot.studio_control_last_error,
        active_stop_request_id: studio_snapshot.active_stop_request_id,
        last_stop_request_id: Some(studio_snapshot.last_stop_request_id),
        stop_request_recorded_age_ms: ws_age_ms(studio_snapshot.stop_request_recorded_age_ms),
        runtime_actuator_last_poll_id: studio_snapshot.runtime_actuator_last_poll_id,
        runtime_actuator_last_poll_age_ms: ws_age_ms(
            studio_snapshot.runtime_actuator_last_poll_age_ms,
        ),
        stop_result_phase: studio_snapshot.stop_result_phase,
        stop_result_age_ms: ws_age_ms(studio_snapshot.stop_result_age_ms),
        stop_result_error: studio_snapshot.stop_result_error,
        official_mcp_adapter_state: Some(official_state),
        official_mcp_adapter_age_ms: ws_age_ms(official_age_ms),
        official_mcp_adapter_last_error: official_last_error,
        runtime_log_forward: Some(runtime_log_forward_status_snapshot(state, task_id)),
    }
}

fn queue_remote_task_status_heartbeat(
    state: &HelperState,
    helper: &HelperConfig,
    task_id: &str,
) -> bool {
    let Some(task) = state.claimed_tasks.get(task_id) else {
        return false;
    };
    let connection_key = task_connection_key(&task.task_id);
    let Some(connection) = state.remote_connections.get(&connection_key) else {
        return false;
    };
    let plugin_instance_count = state
        .instances
        .values()
        .filter(|instance| instance.task_id.as_deref() == Some(task_id))
        .count();
    let task_status = Some(helper_task_status_snapshot(state, task_id));
    let message = HelperToServerMessage::Heartbeat {
        helper_id: helper.helper_id.clone(),
        place_id: task.place_id.clone(),
        task_id: Some(task.task_id.clone()),
        plugin_instance_count,
        task_status,
    };
    let Ok(encoded) = encode_remote_message(&message) else {
        return false;
    };
    connection
        .sender
        .send(RemoteOutgoingFrame::Text(encoded))
        .is_ok()
}

fn queue_task_status_updates(state: &HelperState, helper: &HelperConfig, task_id: &str) -> bool {
    let queued_remote = queue_remote_task_status_heartbeat(state, helper, task_id);
    helper.hub_heartbeat_notify.notify_one();
    queued_remote
}

fn remote_connection_state_name(
    remote_connection: Option<&RemoteConnectionHandle>,
) -> &'static str {
    remote_connection
        .map(|connection| match connection.connection_state {
            RemoteConnectionState::Connecting => "connecting",
            RemoteConnectionState::Connected => "connected",
            RemoteConnectionState::Retrying => "retrying",
            RemoteConnectionState::Error => "error",
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
            let studio_snapshot = task_studio_mode_snapshot(state, &task.task_id);
            let (
                official_mcp_adapter_state,
                official_mcp_adapter_age_ms,
                official_mcp_adapter_last_error,
            ) = task_official_adapter_snapshot(
                state,
                &task.task_id,
                studio_snapshot.edit_runtime_state == "ready",
            );
            let runtime_log_forward = runtime_log_forward_status_snapshot(state, &task.task_id);
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
                studio_session_state: studio_snapshot.studio_session_state,
                last_known_session_state: studio_snapshot.last_known_session_state,
                last_session_error_reason: studio_snapshot.last_session_error_reason,
                studio_mode: studio_snapshot.studio_mode,
                studio_mode_age_ms: studio_snapshot.studio_mode_age_ms,
                studio_mode_source: studio_snapshot.studio_mode_source,
                studio_control_state: studio_snapshot.studio_control_state,
                studio_transition_phase: studio_snapshot.studio_transition_phase,
                studio_transition_age_ms: studio_snapshot.studio_transition_age_ms,
                edit_runtime_state: studio_snapshot.edit_runtime_state,
                edit_runtime_age_ms: studio_snapshot.edit_runtime_age_ms,
                studio_control_last_error: studio_snapshot.studio_control_last_error,
                active_stop_request_id: studio_snapshot.active_stop_request_id,
                last_stop_request_id: studio_snapshot.last_stop_request_id,
                stop_request_recorded_age_ms: studio_snapshot.stop_request_recorded_age_ms,
                runtime_actuator_last_poll_id: studio_snapshot.runtime_actuator_last_poll_id,
                runtime_actuator_last_poll_age_ms: studio_snapshot
                    .runtime_actuator_last_poll_age_ms,
                stop_result_phase: studio_snapshot.stop_result_phase,
                stop_result_age_ms: studio_snapshot.stop_result_age_ms,
                stop_result_error: studio_snapshot.stop_result_error,
                official_mcp_adapter_state,
                official_mcp_adapter_age_ms,
                official_mcp_adapter_last_error,
                runtime_log_forward,
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

fn remote_connection_matches_worker(
    state: &HelperState,
    connection_key: &str,
    worker_id: Uuid,
) -> bool {
    state
        .remote_connections
        .get(connection_key)
        .map(|connection| connection.worker_id == worker_id)
        .unwrap_or(false)
}

fn set_remote_connection_connecting(
    state: &mut HelperState,
    connection_key: &str,
    worker_id: Uuid,
) {
    if let Some(connection) = state.remote_connections.get_mut(connection_key) {
        if connection.worker_id != worker_id {
            return;
        }
        let now = Instant::now();
        connection.connection_state = RemoteConnectionState::Connecting;
        connection.state_changed_at = now;
        if connection.retrying_since.is_none() {
            connection.retrying_since = Some(now);
        }
        connection.connection_id = None;
        connection.last_server_message_at = None;
    }
}

fn set_remote_connection_connected(
    state: &mut HelperState,
    connection_key: &str,
    worker_id: Uuid,
    connection_id: &str,
) {
    if let Some(connection) = state.remote_connections.get_mut(connection_key) {
        let now = Instant::now();
        if connection.worker_id != worker_id {
            return;
        }
        connection.connection_state = RemoteConnectionState::Connected;
        connection.state_changed_at = now;
        connection.retrying_since = None;
        connection.consecutive_failures = 0;
        connection.connection_id = Some(connection_id.to_owned());
        connection.last_ready_at = Some(now);
        connection.last_server_message_at = Some(now);
    }
    state.last_remote_errors.remove(connection_key);
}

fn note_remote_connection_activity(state: &mut HelperState, connection_key: &str, worker_id: Uuid) {
    if let Some(connection) = state.remote_connections.get_mut(connection_key) {
        if connection.worker_id != worker_id {
            return;
        }
        connection.last_server_message_at = Some(Instant::now());
    }
}

fn set_remote_connection_retrying(
    state: &mut HelperState,
    connection_key: &str,
    worker_id: Uuid,
    error: &str,
) {
    let mut matched_worker = false;
    if let Some(connection) = state.remote_connections.get_mut(connection_key) {
        if connection.worker_id != worker_id {
            return;
        }
        matched_worker = true;
        let now = Instant::now();
        connection.connection_state = RemoteConnectionState::Retrying;
        connection.state_changed_at = now;
        if connection.retrying_since.is_none() {
            connection.retrying_since = Some(now);
        }
        connection.consecutive_failures = connection.consecutive_failures.saturating_add(1);
        connection.connection_id = None;
    }
    if matched_worker {
        state
            .last_remote_errors
            .insert(connection_key.to_owned(), summarize_error(error));
    }
}

fn set_remote_connection_error(state: &mut HelperState, connection_key: &str, error: &str) {
    if let Some(connection) = state.remote_connections.get_mut(connection_key) {
        connection.connection_state = RemoteConnectionState::Error;
        connection.state_changed_at = Instant::now();
        connection.connection_id = None;
    }
    state
        .last_remote_errors
        .insert(connection_key.to_owned(), summarize_error(error));
}

fn remote_connection_should_restart(connection: &RemoteConnectionHandle) -> bool {
    match connection.connection_state {
        RemoteConnectionState::Connecting | RemoteConnectionState::Retrying => {
            connection.state_changed_at.elapsed() >= REMOTE_WS_STALE_RESTART_AFTER
        }
        RemoteConnectionState::Connected | RemoteConnectionState::Error => false,
    }
}

fn remote_connection_should_release(connection: &RemoteConnectionHandle) -> bool {
    connection
        .retrying_since
        .map(|started_at| started_at.elapsed() >= REMOTE_WS_STALE_RELEASE_AFTER)
        .unwrap_or(false)
}

fn spawn_remote_connection(
    app: &AppState,
    state: &mut HelperState,
    target: RemoteTarget,
    retrying_since: Option<Instant>,
    consecutive_failures: u32,
) {
    let (stop_tx, stop_rx) = watch::channel(false);
    let (sender, _receiver) = mpsc::unbounded_channel::<RemoteOutgoingFrame>();
    let worker_id = Uuid::new_v4();
    state.remote_connections.insert(
        target.connection_key.clone(),
        RemoteConnectionHandle {
            worker_id,
            stop_tx,
            sender,
            remote_base_url: target.remote_base_url.clone(),
            place_id: target.place_id.clone(),
            task_id: target.task_id.clone(),
            connection_state: RemoteConnectionState::Connecting,
            state_changed_at: Instant::now(),
            retrying_since,
            consecutive_failures,
            connection_id: None,
            last_ready_at: None,
            last_server_message_at: None,
        },
    );
    let worker_app = app.clone();
    tokio::spawn(async move {
        remote_ws_loop(worker_app, target, worker_id, stop_rx).await;
    });
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
    let mut task_ids_to_release = Vec::new();
    for target in active_targets {
        if let Some(connection) = state.remote_connections.get(&target.connection_key) {
            if remote_connection_should_release(connection) {
                let message = format!(
                    "remote websocket did not recover after {}s",
                    REMOTE_WS_STALE_RELEASE_AFTER.as_secs()
                );
                tracing::warn!(
                    connection_key = target.connection_key,
                    place_id = target.place_id,
                    task_id = ?target.task_id,
                    consecutive_failures = connection.consecutive_failures,
                    "releasing claimed task after remote websocket retry timeout"
                );
                set_remote_connection_error(state, &target.connection_key, &message);
                if let Some(task_id) = target.task_id.as_ref() {
                    task_ids_to_release.push(task_id.clone());
                }
                continue;
            }
            if remote_connection_should_restart(connection) {
                let retrying_since = connection.retrying_since;
                let consecutive_failures = connection.consecutive_failures;
                let old_stop_tx = connection.stop_tx.clone();
                let _ = old_stop_tx.send(true);
                state.remote_connections.remove(&target.connection_key);
                state.last_remote_errors.insert(
                    target.connection_key.clone(),
                    "remote websocket retry watchdog restarted stale connection".to_owned(),
                );
                tracing::warn!(
                    connection_key = target.connection_key,
                    place_id = target.place_id,
                    task_id = ?target.task_id,
                    consecutive_failures,
                    "restarting stale helper remote websocket loop"
                );
                spawn_remote_connection(app, state, target, retrying_since, consecutive_failures);
            }
            continue;
        }
        spawn_remote_connection(app, state, target, None, 0);
    }
    for task_id in task_ids_to_release {
        let _ = release_claimed_task(
            state,
            &task_id,
            false,
            "remote websocket retry watchdog timed out",
        );
    }
}

async fn remote_ws_loop(
    app: AppState,
    target: RemoteTarget,
    worker_id: Uuid,
    mut stop_rx: watch::Receiver<bool>,
) {
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
            cleanup_stale_instances(&mut state);
            if !remote_connection_is_active(&state, &target.connection_key) {
                if remote_connection_matches_worker(&state, &target.connection_key, worker_id) {
                    state.remote_connections.remove(&target.connection_key);
                    state.last_remote_errors.remove(&target.connection_key);
                }
                break;
            }
        }

        {
            let mut state = app.state.lock().await;
            set_remote_connection_connecting(&mut state, &target.connection_key, worker_id);
        }
        let result = run_remote_ws_session(
            app.clone(),
            &target.connection_key,
            worker_id,
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
                        worker_id,
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
    if remote_connection_matches_worker(&state, &target.connection_key, worker_id) {
        state.remote_connections.remove(&target.connection_key);
        state.last_remote_errors.remove(&target.connection_key);
    }
    tracing::info!(
        connection_key = target.connection_key,
        place_id = target.place_id,
        "stopped helper remote websocket loop"
    );
}

fn mark_hub_reachable(state: &mut HelperState) {
    state.hub_last_error = None;
    state.hub_last_ready_at = Some(Instant::now());
}

fn mark_hub_claim_ok(state: &mut HelperState) {
    mark_hub_reachable(state);
    state.hub_last_claim_error = None;
}

fn record_hub_claim_error(state: &mut HelperState, claim_error: String) {
    state.hub_last_claim_error = Some(claim_error.clone());
    if state.claimed_tasks.is_empty() {
        state.hub_last_error = Some(claim_error);
    }
}

async fn hub_maintenance_loop(app: AppState, mut registered: bool) {
    let mut interval = tokio::time::interval(Duration::from_secs(5));
    loop {
        tokio::select! {
            _ = interval.tick() => {}
            _ = app.helper.hub_heartbeat_notify.notified() => {}
        }
        if !registered {
            match hub_register_helper(&app.helper).await {
                Ok(_) => {
                    registered = true;
                    let mut state = app.state.lock().await;
                    mark_hub_reachable(&mut state);
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
                        mark_hub_reachable(&mut state);
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
            let mut state = app.state.lock().await;
            state.hub_last_claim_error = None;
            continue;
        }
        match hub_claim_task(&app.helper).await {
            Ok(response) => {
                {
                    let mut state = app.state.lock().await;
                    mark_hub_claim_ok(&mut state);
                }
                if let Some(task) = response.task {
                    let Some(mcp_base_url) = task.routes.mcp_base_url else {
                        let mut state = app.state.lock().await;
                        state.hub_last_error =
                            Some("hub claimed task missing mcp_base_url".to_owned());
                        continue;
                    };
                    let mut claimed_task = ClaimedTask {
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
                            if let Err(error) =
                                ensure_claimed_task_universe_id(&app.helper, &mut claimed_task)
                                    .await
                            {
                                let mut state = app.state.lock().await;
                                state.hub_last_error = Some(summarize_error(&error.to_string()));
                                continue;
                            }
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
                    mark_hub_claim_ok(&mut state);
                    sync_remote_connections(&app, &mut state);
                }
            }
            Err(error) => {
                let claim_error = summarize_error(&error.to_string());
                let mut state = app.state.lock().await;
                record_hub_claim_error(&mut state, claim_error);
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
        match tokio::time::timeout(REMOTE_WS_CONNECT_TIMEOUT, connect_async(request)).await {
            Ok(Ok((stream, _response))) => {
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
            Ok(Err(error)) => {
                last_error = Some(error.to_string());
            }
            Err(_) => {
                last_error = Some(format!(
                    "websocket connect timed out after {}s",
                    REMOTE_WS_CONNECT_TIMEOUT.as_secs()
                ));
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
    let upload_target = format!(
        "request_id={request_id} upload_id={upload_id} place_id={place_id} task_id={}",
        task_id.unwrap_or("")
    );

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

    let committed =
        await_observable_upstream_result("remote_artifact_upload", upload_target, async {
            match tokio::time::timeout(Duration::from_secs(30), rx).await {
                Ok(result) => result?,
                Err(_) => {
                    upload_waiters.lock().await.remove(&upload_id.to_string());
                    send_artifact_abort_message(
                        &sender,
                        upload_id,
                        &request_id,
                        "timed out waiting for artifact commit acknowledgement",
                    );
                    Err(eyre!(
                        "timed out waiting for artifact commit acknowledgement"
                    ))
                }
            }
        })
        .await?;

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
    worker_id: Uuid,
    place_id: &str,
    task_id: Option<&str>,
    ws_url: &str,
    stop_rx: &mut watch::Receiver<bool>,
) -> Result<()> {
    let stream = connect_remote_ws(&app.helper, ws_url).await?;
    {
        let state = app.state.lock().await;
        if *stop_rx.borrow() || !remote_connection_matches_worker(&state, connection_key, worker_id)
        {
            return Ok(());
        }
    }
    let (mut writer, mut reader) = stream.split();
    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<RemoteOutgoingFrame>();
    let mut last_server_message_at = Instant::now();
    let mut last_heartbeat_sent_at = Instant::now();
    let mut heartbeat_count: u64 = 0;
    let mut stopped_by_request = false;
    {
        let mut state = app.state.lock().await;
        if let Some(connection) = state.remote_connections.get_mut(connection_key) {
            if connection.worker_id == worker_id {
                connection.sender = out_tx.clone();
            }
        }
        set_remote_connection_connecting(&mut state, connection_key, worker_id);
        state.last_remote_errors.remove(connection_key);
    }
    let writer_task = tokio::spawn(async move {
        while let Some(frame) = out_rx.recv().await {
            let message = match frame {
                RemoteOutgoingFrame::Text(text) => WsMessage::Text(text),
                RemoteOutgoingFrame::Ping(payload) => WsMessage::Ping(payload),
                RemoteOutgoingFrame::Pong(payload) => WsMessage::Pong(payload),
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
    let hello_sent_at = Instant::now();
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
                    OFFICIAL_MCP_STORE_IMAGE_BASE64_CAPABILITY.to_owned(),
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
    let mut ping = tokio::time::interval(REMOTE_WS_PING_INTERVAL);
    let mut last_pong_at = Instant::now();
    let mut ready_acknowledged = false;
    let mut hello_slow_logged = false;
    let mut hello_slow_timer = Box::pin(tokio::time::sleep(OBSERVABLE_UPSTREAM_SLOW_AFTER));
    let mut session_error: Option<Report> = None;
    loop {
        tokio::select! {
            _ = &mut hello_slow_timer, if !ready_acknowledged && !hello_slow_logged => {
                hello_slow_logged = true;
                tracing::warn!(
                    connection_key,
                    place_id,
                    task_id = ?task_id,
                    threshold_ms = OBSERVABLE_UPSTREAM_SLOW_AFTER.as_millis(),
                    elapsed_ms = hello_sent_at.elapsed().as_millis(),
                    result_observable = true,
                    "helper remote websocket hello still waiting for ready_ack"
                );
            }
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
                tracing::debug!(
                    place_id,
                    task_id = ?task_id,
                    heartbeat_count,
                    plugin_instance_count,
                    studio_mode = ?task_status.as_ref().and_then(|status| status.studio_mode.clone()),
                    studio_mode_age_ms = ?task_status.as_ref().and_then(|status| status.studio_mode_age_ms),
                    studio_control_state = ?task_status.as_ref().and_then(|status| status.studio_control_state.clone()),
                    studio_transition_phase = ?task_status.as_ref().and_then(|status| status.studio_transition_phase.clone()),
                    official_mcp_adapter_state = ?task_status.as_ref().and_then(|status| status.official_mcp_adapter_state.clone()),
                    official_mcp_adapter_age_ms = ?task_status.as_ref().and_then(|status| status.official_mcp_adapter_age_ms),
                    idle_from_server_ms = last_server_message_at.elapsed().as_millis(),
                    "sending helper remote websocket heartbeat"
                );
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
            _ = ping.tick() => {
                if last_pong_at.elapsed() > REMOTE_WS_PONG_TIMEOUT {
                    session_error = Some(eyre!(
                        "remote websocket pong timed out after {}ms",
                        last_pong_at.elapsed().as_millis()
                    ));
                    break;
                }
                let payload = Uuid::new_v4().as_bytes().to_vec();
                if out_tx.send(RemoteOutgoingFrame::Ping(payload)).is_err() {
                    tracing::warn!(place_id, "failed to queue helper remote websocket ping");
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
                            note_remote_connection_activity(&mut state, connection_key, worker_id);
                        }
                        match serde_json::from_str::<ServerToHelperMessage>(&text)? {
                            ServerToHelperMessage::ReadyAck { connection_id, place_id: ack_place_id, task_id: ack_task_id } => {
                                validate_remote_ready_ack(place_id, task_id, &ack_place_id, ack_task_id.as_deref())?;
                                {
                                    let mut state = app.state.lock().await;
                                    set_remote_connection_connected(&mut state, connection_key, worker_id, &connection_id);
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
                            note_remote_connection_activity(&mut state, connection_key, worker_id);
                        }
                        let _ = out_tx.send(RemoteOutgoingFrame::Pong(payload));
                    }
                    WsMessage::Close(_) => break,
                    WsMessage::Pong(_) => {
                        last_pong_at = Instant::now();
                        last_server_message_at = Instant::now();
                        {
                            let mut state = app.state.lock().await;
                            note_remote_connection_activity(&mut state, connection_key, worker_id);
                        }
                    }
                    WsMessage::Binary(_) | WsMessage::Frame(_) => {
                        last_server_message_at = Instant::now();
                        {
                            let mut state = app.state.lock().await;
                            note_remote_connection_activity(&mut state, connection_key, worker_id);
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
    } else if let Some(error) = session_error {
        Err(error)
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
    match tokio::time::timeout(timeout + Duration::from_secs(5), response_rx).await {
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
                if let Err(error) = validate_official_adapter_request(&command.request) {
                    let _ = command.response_tx.send(Err(error));
                    continue;
                }
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
                    Err(error) if official_adapter_error_keeps_ready(error) => {
                        set_official_adapter_state(&state, &task_id, "ready", None).await;
                    }
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
    let (arguments, cleanup_guard) = prepare_official_tool_arguments(
        task_id,
        &request.request_id,
        &request.action,
        request.arguments.clone(),
    )?;
    let result = process_ref.call_tool(official_tool, arguments).await;
    drop(cleanup_guard);
    let result = result?;
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

fn validate_official_adapter_request(request: &OfficialMcpRequest) -> Result<()> {
    if request.action != "ping" {
        official_tool_for_action(&request.action)?;
    }
    Ok(())
}

fn should_restart_official_process_after_error(error: &Report) -> bool {
    !is_transient_official_readiness_error(error)
        && !official_adapter_error_keeps_ready(error)
        && !official_adapter_error_preserves_state(error)
}

fn official_adapter_error_keeps_ready(error: &Report) -> bool {
    !is_transient_official_readiness_error(error)
        && (is_official_business_tool_error(error) || is_official_request_argument_error(error))
}

fn official_adapter_error_preserves_state(error: &Report) -> bool {
    let message = error.to_string();
    message.starts_with("unsupported official MCP adapter action:")
        || message.starts_with("Invalid official MCP")
}

fn is_transient_official_readiness_error(error: &Report) -> bool {
    let message = error.to_string();
    message.contains("Not connected to the WS host")
        || message.contains("official MCP found no connected Roblox Studio instances")
        || message
            .contains("previously active Studio has disconnected or doesn't have a place opened")
}

fn is_official_business_tool_error(error: &Report) -> bool {
    let message = error.to_string();
    let Some(rest) = message.strip_prefix("official MCP tool ") else {
        return false;
    };
    let Some((tool_name, summary)) = rest.split_once(" returned error:") else {
        return false;
    };
    if is_official_channel_error_summary(summary) {
        return false;
    }
    let summary = summary.to_ascii_lowercase();
    match tool_name {
        "wait_job_finished" => is_official_wait_job_business_error_summary(&summary),
        "generate_mesh"
        | "generate_procedural_model"
        | "search_creator_store"
        | "insert_from_creator_store"
        | "store_image" => is_official_business_error_summary(&summary),
        _ => false,
    }
}

fn is_official_wait_job_business_error_summary(summary: &str) -> bool {
    if summary.contains("workflow failed") || summary.contains("no job found") {
        return true;
    }
    let squashed: String = summary
        .chars()
        .filter(|character| !matches!(character, ' ' | '_' | '-'))
        .collect();
    let references_job_id = summary.contains("job id")
        || squashed.contains("jobid")
        || squashed.contains("generationid");
    references_job_id
        && (summary.contains("invalid")
            || summary.contains("missing")
            || summary.contains("required")
            || summary.contains("malformed"))
}

fn is_official_request_argument_error(error: &Report) -> bool {
    let message = error.to_string();
    message.starts_with("store_image ")
}

fn is_official_business_error_summary(summary: &str) -> bool {
    [
        "policy",
        "moderation",
        "prompt",
        "input",
        "argument",
        "parameter",
        "invalid",
        "malformed",
        "not found",
        "no result",
        "file",
        "image",
        "mime",
        "content",
        "asset",
    ]
    .iter()
    .any(|needle| summary.contains(needle))
}

fn is_official_channel_error_summary(summary: &str) -> bool {
    let summary = summary.to_ascii_lowercase();
    [
        "not connected",
        "connection",
        "ws host",
        "websocket",
        "active studio",
        "studio unavailable",
        "studio has disconnected",
        "no connected roblox studio",
        "adapter failure",
        "internal adapter",
        "timed out",
        "timeout",
        "stdio",
        "json-rpc",
        "protocol",
        "closed stdout",
        "process closed",
    ]
    .iter()
    .any(|needle| summary.contains(needle))
}

fn prepare_official_tool_arguments(
    task_id: &str,
    request_id: &str,
    action: &str,
    arguments: Value,
) -> Result<(Value, Option<TempOfficialImage>)> {
    if action != "store_image" {
        return Ok((arguments, None));
    }
    prepare_official_store_image_arguments(task_id, request_id, arguments)
}

fn prepare_official_store_image_arguments(
    task_id: &str,
    request_id: &str,
    arguments: Value,
) -> Result<(Value, Option<TempOfficialImage>)> {
    let object = arguments
        .as_object()
        .ok_or_else(|| eyre!("store_image arguments must be a JSON object"))?;
    let file_path = string_field(object, &["filePath", "file_path"]);
    let image_base64 = string_field(object, &["imageBase64", "image_base64"]);
    match (
        file_path
            .map(|value| !trim(value).is_empty())
            .unwrap_or(false),
        image_base64
            .map(|value| !trim(value).is_empty())
            .unwrap_or(false),
    ) {
        (true, true) => {
            return Err(eyre!(
                "store_image requires exactly one of filePath or imageBase64"
            ))
        }
        (false, false) => return Err(eyre!("store_image requires filePath or imageBase64")),
        _ => {}
    }
    if let Some(path) = file_path {
        if string_field(object, &["mimeType", "mime_type"]).is_some()
            || string_field(object, &["fileName", "file_name"]).is_some()
        {
            return Err(eyre!(
                "store_image mimeType/fileName are only valid with imageBase64"
            ));
        }
        return Ok((serde_json::json!({ "filePath": trim(path) }), None));
    }

    let image_base64 = trim(image_base64.expect("image_base64 presence checked"));
    if image_base64.len() > OFFICIAL_STORE_IMAGE_MAX_BASE64_CHARS {
        return Err(eyre!(
            "store_image imageBase64 exceeds {} decoded bytes",
            OFFICIAL_STORE_IMAGE_MAX_DECODED_BYTES
        ));
    }
    let mime_type = trim(
        string_field(object, &["mimeType", "mime_type"])
            .ok_or_else(|| eyre!("store_image mimeType is required with imageBase64"))?,
    );
    let extension = official_image_extension_for_mime(&mime_type)?;
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(image_base64)
        .map_err(|error| eyre!("store_image imageBase64 decode failed: {error}"))?;
    if decoded.len() > OFFICIAL_STORE_IMAGE_MAX_DECODED_BYTES {
        return Err(eyre!(
            "store_image decoded image exceeds {} bytes",
            OFFICIAL_STORE_IMAGE_MAX_DECODED_BYTES
        ));
    }
    ensure_official_image_magic(&mime_type, &decoded)?;
    let task_id = sanitize_identifier("task_id", task_id)?;
    let request_id = sanitize_identifier("request_id", request_id)?;
    let hint = string_field(object, &["fileName", "file_name"])
        .map(sanitize_image_file_hint)
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "image".to_owned());
    let output_dir = helper_data_dir()?.join("official-images").join(&task_id);
    fs::create_dir_all(&output_dir)?;
    let output_path = output_dir.join(format!("{request_id}-{hint}.{extension}"));
    fs::write(&output_path, decoded)?;
    tracing::info!(
        path = %output_path.display(),
        task_id,
        "wrote temporary official MCP store_image input"
    );
    Ok((
        serde_json::json!({ "filePath": output_path.to_string_lossy() }),
        Some(TempOfficialImage { path: output_path }),
    ))
}

fn string_field<'a>(object: &'a serde_json::Map<String, Value>, names: &[&str]) -> Option<&'a str> {
    names
        .iter()
        .find_map(|name| object.get(*name).and_then(Value::as_str))
}

fn official_image_extension_for_mime(mime_type: &str) -> Result<&'static str> {
    match mime_type {
        "image/png" => Ok("png"),
        "image/jpeg" | "image/jpg" => Ok("jpg"),
        _ => Err(eyre!(
            "store_image mimeType must be image/png, image/jpeg, or image/jpg"
        )),
    }
}

fn ensure_official_image_magic(mime_type: &str, bytes: &[u8]) -> Result<()> {
    let valid = match mime_type {
        "image/png" => bytes.starts_with(&[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]),
        "image/jpeg" | "image/jpg" => bytes.starts_with(&[0xff, 0xd8, 0xff]),
        _ => false,
    };
    if !valid {
        return Err(eyre!(
            "store_image imageBase64 content does not match mimeType"
        ));
    }
    Ok(())
}

fn sanitize_image_file_hint(value: &str) -> String {
    let mut sanitized = trim(value)
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
                ch
            } else {
                '_'
            }
        })
        .collect::<String>();
    if sanitized.len() > 40 {
        sanitized.truncate(40);
    }
    sanitized.trim_matches('_').trim_matches('-').to_owned()
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
        "StartStopPlay" | "start_stop_play" => {
            let args: StartStopPlayCommandArgs = serde_json::from_value(payload)?;
            if args.mode != "stop" {
                return Err(eyre!(
                    "start_stop_play is stop-only. Use launch_studio_session to enter start_play or run_server."
                ));
            }
            if let Some(result) =
                stop_via_runtime_actuator_if_available(app.clone(), place_id, task_id).await?
            {
                return Ok(result);
            }
            ensure_edit_runtime_ready_for_stop_fallback(&app, place_id, task_id).await?;
            forward_to_plugin(app, place_id, task_id, command).await
        }
        "LaunchStudioSession" | "launch_studio_session" => {
            let args: LaunchStudioSessionCommandArgs = serde_json::from_value(payload)?;
            validate_launch_studio_session_mode(&args.mode)?;
            forward_launch_to_plugin(app, place_id, task_id, command).await
        }
        _ => forward_to_plugin(app, place_id, task_id, command).await,
    }
}

async fn stop_via_runtime_actuator_if_available(
    app: AppState,
    place_id: &str,
    task_id: Option<&str>,
) -> Result<Option<String>> {
    let (instance_id, stop_request_id, reused_existing) = {
        let mut state = app.state.lock().await;
        cleanup_stale_instances(&mut state);
        sync_remote_connections(&app, &mut state);
        if let Some((instance_id, stop_request_id)) =
            select_active_stop_request_for_route(place_id, task_id, &state.instances)
        {
            return Ok(Some({
                let status_task_id = if let Some(instance) = state.instances.get_mut(&instance_id) {
                    set_edit_runtime_unavailable(instance, "stopping");
                    instance.task_id.clone()
                } else {
                    None
                };
                if let Some(task_id) = status_task_id.as_deref() {
                    queue_task_status_updates(&state, &app.helper, task_id);
                }
                drop(state);
                tracing::info!(
                    instance_id,
                    place_id,
                    task_id,
                    stop_request_id,
                    "reusing active runtime actuator stop request for remote stop command"
                );
                wait_for_runtime_actuator_stop(&app, &instance_id, stop_request_id).await?;
                "Stopped".to_owned()
            }));
        }
        let Some(instance_id) =
            select_runtime_actuator_for_route(place_id, task_id, &state.instances)
        else {
            if let Some(instance_id) =
                select_known_runtime_session_for_route(place_id, task_id, &state.instances)
            {
                return Err(eyre!(
                    "runtime_stop_actuator_unavailable: instance_id={instance_id}; known runtime session has no fresh runtime actuator, refusing unsafe edit-stop fallback"
                ));
            }
            return Ok(None);
        };
        let recorded = match record_stop_request_for_instance(&mut state, &instance_id) {
            Ok(recorded) => recorded,
            Err(StopRequestRecordError::MissingInstance) => return Ok(None),
            Err(StopRequestRecordError::RuntimeActuatorUnavailable(detail)) => {
                return Err(eyre!(
                    "runtime_stop_actuator_unavailable: instance_id={instance_id}; {detail}; refusing unsafe edit-stop fallback"
                ));
            }
        };
        if let Some(task_id) = recorded.task_id.as_deref() {
            queue_task_status_updates(&state, &app.helper, task_id);
        }
        (instance_id, recorded.stop_request_id, false)
    };

    tracing::info!(
        instance_id,
        place_id,
        task_id,
        stop_request_id,
        reused_existing,
        "recorded runtime actuator stop request for remote stop command"
    );
    wait_for_runtime_actuator_stop(&app, &instance_id, stop_request_id).await?;
    Ok(Some("Stopped".to_owned()))
}

async fn ensure_edit_runtime_ready_for_stop_fallback(
    app: &AppState,
    place_id: &str,
    task_id: Option<&str>,
) -> Result<()> {
    let state = app.state.lock().await;
    let Some(instance_id) = select_instance_for_route(place_id, task_id, &state.instances) else {
        return Err(eyre!(
            "no active Studio plugin registered for placeId {place_id}"
        ));
    };
    let instance = state
        .instances
        .get(&instance_id)
        .ok_or_else(|| eyre!("selected helper instance disappeared"))?;
    let edit_state = edit_runtime_state(instance);
    if edit_state == "ready" && effective_studio_transition_phase(instance) == "idle" {
        return Ok(());
    }
    Err(eyre!(
        "runtime_stop_actuator_unavailable: instance_id={instance_id}; edit runtime is not ready for safe stop fallback; edit_runtime_state={edit_state}; studio_transition_phase={}; studio_control_state={}; refusing unsafe edit-stop fallback",
        effective_studio_transition_phase(instance),
        effective_studio_control_state(instance)
    ))
}

async fn wait_for_runtime_actuator_stop(
    app: &AppState,
    instance_id: &str,
    stop_request_id: u64,
) -> Result<()> {
    let deadline = Instant::now() + PLUGIN_STUDIO_CONTROL_REQUEST_TIMEOUT;
    loop {
        {
            let state = app.state.lock().await;
            let Some(instance) = state.instances.get(instance_id) else {
                return Err(eyre!(
                    "runtime_stop_instance_disappeared: instance_id={instance_id} stop_request_id={stop_request_id}"
                ));
            };
            if instance.stop_request_id > stop_request_id {
                return Err(eyre!(
                    "runtime_stop_superseded: instance_id={instance_id} stop_request_id={stop_request_id} current_stop_request_id={}",
                    instance.stop_request_id
                ));
            }
            if instance.stop_request_id == stop_request_id {
                if instance.stop_result_phase.as_deref() == Some("failed") {
                    return Err(eyre!(
                        "{}",
                        instance
                            .stop_result_error
                            .as_deref()
                            .unwrap_or("runtime_stop_failed")
                    ));
                }
                if instance.stop_result_phase.as_deref() == Some("completed")
                    && effective_studio_transition_phase(instance) == "idle"
                    && edit_runtime_state(instance) == "ready"
                {
                    return Ok(());
                }
            }
        }
        if Instant::now() >= deadline {
            let mut state = app.state.lock().await;
            let mut status_task_id: Option<String> = None;
            let detail = state
                .instances
                .get_mut(instance_id)
                .map(|instance| {
                    let detail = format!(
                        "phase={} stop_result_phase={} runtime_actuator_last_poll_id={:?} runtime_actuator_last_poll_age_ms={:?} edit_runtime_state={}",
                        effective_studio_transition_phase(instance),
                        instance.stop_result_phase.as_deref().unwrap_or("none"),
                        instance.stop_request_last_poll_id,
                        instance.stop_request_last_polled_at.map(|value| value.elapsed().as_millis()),
                        edit_runtime_state(instance)
                    );
                    if instance.stop_request_id == stop_request_id {
                        mark_runtime_stop_timeout(instance, stop_request_id, detail.clone());
                        status_task_id = instance.task_id.clone();
                    }
                    detail
                })
                .unwrap_or_else(|| "instance_missing".to_owned());
            if let Some(task_id) = status_task_id.as_deref() {
                queue_task_status_updates(&state, &app.helper, task_id);
            }
            return Err(eyre!(
                "runtime_stop_timeout: stop_request_id={stop_request_id} {detail}"
            ));
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

fn validate_launch_studio_session_mode(mode: &str) -> Result<()> {
    if mode == "start_play" || mode == "run_server" {
        return Ok(());
    }
    Err(eyre!(
        "Invalid mode in LaunchStudioSession, must be start_play or run_server"
    ))
}

async fn forward_to_plugin(
    app: AppState,
    place_id: &str,
    task_id: Option<&str>,
    command: Value,
) -> Result<String> {
    let (_, response) = forward_to_plugin_raw(app, place_id, task_id, command, None).await?;
    if response.success {
        Ok(response.response)
    } else {
        Err(eyre!(response.response))
    }
}

async fn forward_launch_to_plugin(
    app: AppState,
    place_id: &str,
    task_id: Option<&str>,
    command: Value,
) -> Result<String> {
    let (instance_id, response) =
        forward_to_plugin_raw(app.clone(), place_id, task_id, command, Some("launching")).await?;
    {
        let mut state = app.state.lock().await;
        let status_task_id = if let Some(instance) = state.instances.get_mut(&instance_id) {
            if response.success {
                set_edit_runtime_unavailable(instance, "runtime");
            } else {
                set_edit_runtime_unavailable(instance, "launch_failed");
            }
            instance.task_id.clone()
        } else {
            None
        };
        if let Some(task_id) = status_task_id.as_deref() {
            queue_task_status_updates(&state, &app.helper, task_id);
        }
    }
    if response.success {
        Ok(response.response)
    } else {
        Err(eyre!(response.response))
    }
}

async fn forward_to_plugin_raw(
    app: AppState,
    place_id: &str,
    task_id: Option<&str>,
    command: Value,
    edit_unavailable_reason: Option<&str>,
) -> Result<(String, PluginResponsePayload)> {
    let request_id = extract_command_id(&command)?;
    let (tool_name, _) = extract_tool_name_and_args(&command)?;
    let (instance_id, notify, status_task_id) = {
        let mut state = app.state.lock().await;
        cleanup_stale_instances(&mut state);
        sync_remote_connections(&app, &mut state);
        let selected_instance_id = select_instance_for_route(place_id, task_id, &state.instances)
            .ok_or_else(|| {
            eyre!("no active Studio plugin registered for placeId {place_id}")
        })?;
        let instance = state
            .instances
            .get_mut(&selected_instance_id)
            .ok_or_else(|| eyre!("selected helper instance disappeared"))?;
        if let Some(reason) = edit_unavailable_reason {
            set_edit_runtime_unavailable(instance, reason);
        }
        let status_task_id = instance.task_id.clone();
        instance.queue.push_back(command);
        let notify = Arc::clone(&instance.notify);
        let (tx, rx) = oneshot::channel();
        state.waiting_for_plugin.insert(
            request_id.clone(),
            PendingPluginResponse {
                instance_id: selected_instance_id.clone(),
                tool_name: tool_name.clone(),
                response_tx: tx,
            },
        );
        (selected_instance_id, (notify, rx), status_task_id)
    };
    if edit_unavailable_reason.is_some() {
        if let Some(task_id) = status_task_id.as_deref() {
            let state = app.state.lock().await;
            queue_task_status_updates(&state, &app.helper, task_id);
        }
    }
    notify.0.notify_waiters();
    tracing::info!(
        instance_id,
        place_id,
        task_id,
        id = request_id,
        tool = tool_name,
        "forwarded MCP command from helper to local plugin"
    );
    let request_timeout = plugin_request_timeout_for_tool(&tool_name);
    let response = match tokio::time::timeout(request_timeout, notify.1).await {
        Ok(result) => result?,
        Err(_) => {
            let mut state = app.state.lock().await;
            state.waiting_for_plugin.remove(&request_id);
            return Err(eyre!(
                "timed out waiting {}ms for local plugin response to {tool_name}",
                request_timeout.as_millis()
            ));
        }
    };
    Ok((instance_id, response))
}

fn cleanup_stale_instances(state: &mut HelperState) {
    let waiting_instance_ids: HashSet<String> = state
        .waiting_for_plugin
        .values()
        .map(|pending| pending.instance_id.clone())
        .collect();
    state.instances.retain(|instance_id, instance| {
        let has_pending_response = waiting_instance_ids.contains(instance_id);
        let keep = has_pending_response || instance.last_seen_at.elapsed() <= INSTANCE_STALE_AFTER;
        if !keep {
            tracing::warn!(
                instance_id,
                place_id = instance.place_id,
                "dropping stale helper plugin instance"
            );
        } else if has_pending_response && instance.last_seen_at.elapsed() > INSTANCE_STALE_AFTER {
            tracing::debug!(
                instance_id,
                place_id = instance.place_id,
                "keeping stale helper plugin instance while waiting for tool response"
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

fn select_runtime_actuator_for_route(
    place_id: &str,
    task_id: Option<&str>,
    instances: &HashMap<String, PluginInstance>,
) -> Option<String> {
    instances
        .iter()
        .filter(|(_, instance)| {
            instance.place_id == place_id
                && route_identity_matches(instance, task_id)
                && instance_has_fresh_control(instance)
        })
        .max_by_key(|(_, instance)| instance.studio_control_observed_at)
        .map(|(instance_id, _)| instance_id.clone())
}

fn instance_is_known_runtime_session(instance: &PluginInstance) -> bool {
    instance.edit_runtime_unavailable_reason.as_deref() == Some("runtime")
        || instance.edit_runtime_unavailable_reason.as_deref() == Some("launch_failed")
        || instance.edit_runtime_unavailable_reason.as_deref() == Some("launching")
        || matches!(
            instance.studio_mode.as_deref(),
            Some("start_play") | Some("run_server")
        )
        || instance.studio_transition_phase == "running"
        || instance_has_fresh_control(instance)
}

fn select_known_runtime_session_for_route(
    place_id: &str,
    task_id: Option<&str>,
    instances: &HashMap<String, PluginInstance>,
) -> Option<String> {
    instances
        .iter()
        .filter(|(_, instance)| {
            instance.place_id == place_id
                && route_identity_matches(instance, task_id)
                && instance_is_known_runtime_session(instance)
        })
        .max_by_key(|(_, instance)| instance.last_seen_at)
        .map(|(instance_id, _)| instance_id.clone())
}

fn select_active_stop_request_for_route(
    place_id: &str,
    task_id: Option<&str>,
    instances: &HashMap<String, PluginInstance>,
) -> Option<(String, u64)> {
    instances
        .iter()
        .filter_map(|(instance_id, instance)| {
            if instance.place_id != place_id || !route_identity_matches(instance, task_id) {
                return None;
            }
            active_stop_request_id_for_instance(instance).map(|stop_request_id| {
                (instance_id.clone(), stop_request_id, instance.last_seen_at)
            })
        })
        .max_by_key(|(_, stop_request_id, last_seen_at)| (*stop_request_id, *last_seen_at))
        .map(|(instance_id, stop_request_id, _)| (instance_id, stop_request_id))
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
        Ok(CapturedScreenshot {
            resolved_place_id,
            png_bytes: output.stdout,
            window_title,
        })
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
        Ok(output_path.display().to_string())
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
        tracing::debug!(path = %log_path.display(), total_lines, returned_lines = lines.len(), "read Studio log from helper");
        Ok(ReadStudioLogResponse {
            path: log_path.display().to_string(),
            total_lines,
            range: [if total_lines == 0 { 0 } else { start }, end],
            lines,
        })
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
        client: build_http_client()?,
        hub_heartbeat_notify: Arc::new(Notify::new()),
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
        runtime_log_forward_statuses: HashMap::new(),
        last_remote_errors: HashMap::new(),
        hub_last_error: initial_hub_error,
        hub_last_claim_error: None,
        hub_last_ready_at: initial_hub_registration.as_ref().map(|_| Instant::now()),
    }));

    let (runtime_log_forward_tx, runtime_log_forward_rx) =
        mpsc::channel(RUNTIME_LOG_FORWARD_QUEUE_CAPACITY);
    let app_state = AppState {
        helper,
        state,
        runtime_log_forward_tx,
    };

    let runtime_log_forward_app = app_state.clone();
    tokio::spawn(async move {
        runtime_log_forward_worker(runtime_log_forward_app, runtime_log_forward_rx).await;
    });

    let maintenance_app = app_state.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(2));
        loop {
            interval.tick().await;
            let mut state = maintenance_app.state.lock().await;
            cleanup_stale_instances(&mut state);
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
        .route("/v1/debug/tasks/{task_id}", get(helper_debug_task_handler))
        .route("/v1/rojo/config", get(rojo_config_handler))
        .route(
            "/rojo-forward/{place_id}/task/{task_id}",
            any(rojo_forward_task_root_handler),
        )
        .route(
            "/rojo-forward/{place_id}/task/{task_id}/api/socket/{cursor}",
            get(rojo_forward_task_socket_handler),
        )
        .route(
            "/rojo-forward/{place_id}/task/{task_id}/{*path}",
            any(rojo_forward_task_path_handler),
        )
        .route("/rojo-forward/{place_id}", any(rojo_forward_root_handler))
        .route(
            "/rojo-forward/{place_id}/api/socket/{cursor}",
            get(rojo_forward_socket_handler),
        )
        .route(
            "/rojo-forward/{place_id}/{*path}",
            any(rojo_forward_path_handler),
        )
        .route(
            "/runtime-log-forward/{place_id}/task/{task_id}/{*path}",
            any(runtime_log_forward_task_path_handler),
        )
        .route("/v1/mcp/register", post(mcp_register_handler))
        .route("/v1/mcp/unregister", post(mcp_unregister_handler))
        .route("/v1/mcp/plugin/request", get(mcp_plugin_request_handler))
        .route(
            "/v1/mcp/plugin/stop-request",
            get(mcp_plugin_stop_request_poll_handler).post(mcp_plugin_stop_request_handler),
        )
        .route(
            "/v1/mcp/plugin/stop-result",
            post(mcp_plugin_stop_result_handler),
        )
        .route(
            "/v1/mcp/plugin/edit-heartbeat",
            post(mcp_plugin_edit_heartbeat_handler),
        )
        .route(
            "/v1/mcp/plugin/control-heartbeat",
            post(mcp_plugin_control_heartbeat_handler),
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
            studio_mode_source: "edit_plugin".to_owned(),
            studio_control_state: "none".to_owned(),
            studio_transition_phase: "idle".to_owned(),
            studio_transition_started_at: None,
            studio_control_observed_at: None,
            edit_runtime_observed_at: Some(Instant::now()),
            edit_runtime_unavailable_reason: None,
            studio_control_last_error: None,
            stop_request_id: 0,
            stop_request_recorded_at: None,
            stop_request_last_polled_at: None,
            stop_request_last_poll_id: None,
            stop_result_phase: None,
            stop_result_error: None,
            stop_result_observed_at: None,
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

    async fn insert_claimed_task_with_runtime_log_base(
        app: &AppState,
        task_id: &str,
        place_id: &str,
        runtime_log_base_url: String,
    ) {
        let mut task = test_claimed_task(task_id, place_id);
        task.runtime_log_base_url = Some(runtime_log_base_url);
        let mut state = app.state.lock().await;
        state.claimed_tasks.insert(task_id.to_owned(), task);
    }

    async fn call_runtime_log_forward(
        app: AppState,
        path: &str,
    ) -> std::result::Result<Response, HelperError> {
        call_runtime_log_forward_with_uri(app, path, Uri::from_static("/v1/runtime-logs")).await
    }

    async fn call_runtime_log_forward_with_uri(
        app: AppState,
        path: &str,
        uri: Uri,
    ) -> std::result::Result<Response, HelperError> {
        runtime_log_forward_task_path_handler(
            State(app),
            AxumPath((
                "134795435066737".to_owned(),
                "task-a".to_owned(),
                path.to_owned(),
            )),
            Method::POST,
            uri,
            HeaderMap::new(),
            Bytes::from_static(b"{\"ok\":true}"),
        )
        .await
    }

    #[tokio::test]
    async fn runtime_log_forward_fast_acks_slow_upstream() {
        let app = test_app_state_with_runtime_log_worker();
        let (sink_base_url, received_rx) =
            spawn_test_runtime_log_sink(204, Duration::from_millis(750)).await;
        insert_claimed_task_with_runtime_log_base(&app, "task-a", "134795435066737", sink_base_url)
            .await;

        let started = Instant::now();
        let response = call_runtime_log_forward(app.clone(), "v1/runtime-logs")
            .await
            .expect("runtime-log upload should be accepted");

        assert_eq!(response.status(), StatusCode::ACCEPTED);
        assert!(
            started.elapsed() < Duration::from_millis(250),
            "runtime-log forward waited for slow upstream instead of acking quickly"
        );
        let request = received_rx
            .await
            .expect("sink should receive background request");
        assert!(request.starts_with("POST /v1/runtime-logs "));
        tokio::time::sleep(Duration::from_millis(900)).await;
        let state = app.state.lock().await;
        let status = runtime_log_forward_status_snapshot(&state, "task-a");
        assert_eq!(status.accepted_count, 1);
        assert_eq!(status.forwarded_count, 1);
        assert_eq!(status.failed_count, 0);
        assert_eq!(status.state, "ready");
    }

    #[tokio::test]
    async fn runtime_log_forward_allows_upload_query_and_forwards_it() {
        let app = test_app_state_with_runtime_log_worker();
        let (sink_base_url, received_rx) =
            spawn_test_runtime_log_sink(204, Duration::from_millis(0)).await;
        insert_claimed_task_with_runtime_log_base(&app, "task-a", "134795435066737", sink_base_url)
            .await;

        let response = call_runtime_log_forward_with_uri(
            app,
            "v1/runtime-logs",
            Uri::from_static("/v1/runtime-logs?cursor=abc"),
        )
        .await
        .expect("runtime-log upload with query should be accepted");

        assert_eq!(response.status(), StatusCode::ACCEPTED);
        let request = received_rx
            .await
            .expect("sink should receive background request");
        assert!(request.starts_with("POST /v1/runtime-logs?cursor=abc "));
    }

    #[tokio::test]
    async fn runtime_log_forward_completion_after_release_does_not_recreate_status() {
        let app = test_app_state_with_runtime_log_worker();
        let (sink_base_url, _received_rx) =
            spawn_test_runtime_log_sink(204, Duration::from_millis(750)).await;
        insert_claimed_task_with_runtime_log_base(&app, "task-a", "134795435066737", sink_base_url)
            .await;

        let response = call_runtime_log_forward(app.clone(), "v1/runtime-logs")
            .await
            .expect("runtime-log upload should be accepted before release");
        assert_eq!(response.status(), StatusCode::ACCEPTED);
        {
            let mut state = app.state.lock().await;
            assert!(state.runtime_log_forward_statuses.contains_key("task-a"));
            let _ = release_claimed_task(&mut state, "task-a", false, "test release");
            assert!(!state.runtime_log_forward_statuses.contains_key("task-a"));
        }

        tokio::time::sleep(Duration::from_millis(900)).await;
        let state = app.state.lock().await;
        assert!(
            !state.runtime_log_forward_statuses.contains_key("task-a"),
            "released task must not be recreated by a late runtime-log forward completion"
        );
    }

    #[tokio::test]
    async fn runtime_log_forward_records_background_http_failure() {
        let app = test_app_state_with_runtime_log_worker();
        let (sink_base_url, _received_rx) =
            spawn_test_runtime_log_sink(503, Duration::from_millis(0)).await;
        insert_claimed_task_with_runtime_log_base(&app, "task-a", "134795435066737", sink_base_url)
            .await;

        let response = call_runtime_log_forward(app.clone(), "v1/runtime-logs")
            .await
            .expect("runtime-log upload should be accepted before background failure");

        assert_eq!(response.status(), StatusCode::ACCEPTED);
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let state = app.state.lock().await;
                let status = runtime_log_forward_status_snapshot(&state, "task-a");
                if status.failed_count == 1 {
                    assert_eq!(status.state, "error");
                    assert_eq!(status.last_http_status, Some(503));
                    assert!(status
                        .last_error
                        .as_deref()
                        .unwrap_or_default()
                        .contains("HTTP 503"));
                    break;
                }
                drop(state);
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        })
        .await
        .expect("runtime-log background failure should update status");
    }

    #[tokio::test]
    async fn runtime_log_forward_records_background_connection_failure() {
        let app = test_app_state_with_runtime_log_worker();
        let sink_base_url = spawn_disconnect_runtime_log_sink().await;
        insert_claimed_task_with_runtime_log_base(&app, "task-a", "134795435066737", sink_base_url)
            .await;

        let response = call_runtime_log_forward(app.clone(), "v1/runtime-logs")
            .await
            .expect("runtime-log upload should be accepted before background connection failure");

        assert_eq!(response.status(), StatusCode::ACCEPTED);
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let state = app.state.lock().await;
                let status = runtime_log_forward_status_snapshot(&state, "task-a");
                if status.failed_count == 1 {
                    assert_eq!(status.state, "error");
                    assert_eq!(status.last_http_status, None);
                    assert!(status
                        .last_error
                        .as_deref()
                        .unwrap_or_default()
                        .contains("runtime-log forward failed"));
                    break;
                }
                drop(state);
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        })
        .await
        .expect("runtime-log background connection failure should update status");
    }

    #[tokio::test]
    async fn runtime_log_forward_queue_full_does_not_decrement_existing_queue() {
        let (app, _runtime_log_forward_rx) = test_app_state_with_runtime_log_queue_only();
        insert_claimed_task_with_runtime_log_base(
            &app,
            "task-a",
            "134795435066737",
            "http://127.0.0.1:9".to_owned(),
        )
        .await;

        for _ in 0..RUNTIME_LOG_FORWARD_QUEUE_CAPACITY {
            let response = call_runtime_log_forward(app.clone(), "v1/runtime-logs")
                .await
                .expect("runtime-log upload should be accepted while queue has capacity");
            assert_eq!(response.status(), StatusCode::ACCEPTED);
        }
        let response = call_runtime_log_forward(app.clone(), "v1/runtime-logs")
            .await
            .expect("full runtime-log queue should return a response");

        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        let state = app.state.lock().await;
        let status = runtime_log_forward_status_snapshot(&state, "task-a");
        assert_eq!(
            status.queued_count, RUNTIME_LOG_FORWARD_QUEUE_CAPACITY as u64,
            "queue_full must not decrement jobs that were already queued"
        );
        assert_eq!(
            status.accepted_count,
            RUNTIME_LOG_FORWARD_QUEUE_CAPACITY as u64
        );
        assert_eq!(status.failed_count, 1);
        assert!(status
            .last_error
            .as_deref()
            .unwrap_or_default()
            .contains("queue is full"));
    }

    #[tokio::test]
    async fn runtime_log_forward_rejects_non_upload_proxy_paths() {
        let app = test_app_state_with_runtime_log_worker();
        insert_claimed_task_with_runtime_log_base(
            &app,
            "task-a",
            "134795435066737",
            "http://127.0.0.1:1".to_owned(),
        )
        .await;

        let response = call_runtime_log_forward(app.clone(), "v1/not-runtime-log")
            .await
            .expect("unsupported runtime-log forward path should return a response");

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let state = app.state.lock().await;
        let status = runtime_log_forward_status_snapshot(&state, "task-a");
        assert_eq!(status.accepted_count, 0);
        assert_eq!(status.failed_count, 0);
        assert_eq!(status.state, "idle");
    }

    #[test]
    fn studio_launch_args_include_place_and_universe_ids() {
        let task = test_claimed_task("task-a", "134795435066737");

        let args = studio_launch_args_for_claim(&task).unwrap();

        assert_eq!(
            args,
            vec![
                "-task",
                "EditPlace",
                "-placeId",
                "134795435066737",
                "-universeId",
                "999"
            ]
        );
    }

    #[test]
    fn studio_launch_args_reject_missing_universe_id() {
        let mut task = test_claimed_task("task-a", "134795435066737");
        task.universe_id = None;

        let error = studio_launch_args_for_claim(&task).unwrap_err().to_string();

        assert!(error.contains("without universeId"));
    }

    #[test]
    fn hub_reachable_keeps_claim_error_until_claim_result() {
        let mut state = empty_helper_state();
        state.hub_last_error = Some("heartbeat failed".to_owned());
        state.hub_last_claim_error = Some("claim failed".to_owned());
        state.hub_last_ready_at = None;

        mark_hub_reachable(&mut state);

        assert!(state.hub_last_error.is_none());
        assert_eq!(state.hub_last_claim_error.as_deref(), Some("claim failed"));
        assert!(state.hub_last_ready_at.is_some());

        mark_hub_claim_ok(&mut state);

        assert!(state.hub_last_claim_error.is_none());
        assert!(state.hub_last_ready_at.is_some());
    }

    #[test]
    fn record_hub_claim_error_keeps_active_task_health_separate() {
        let mut idle_state = empty_helper_state();
        record_hub_claim_error(&mut idle_state, "claim failed".to_owned());

        assert_eq!(idle_state.hub_last_error.as_deref(), Some("claim failed"));
        assert_eq!(
            idle_state.hub_last_claim_error.as_deref(),
            Some("claim failed")
        );

        let mut active_state = empty_helper_state();
        active_state.claimed_tasks.insert(
            "task-a".to_owned(),
            test_claimed_task("task-a", "134795435066737"),
        );
        record_hub_claim_error(&mut active_state, "claim failed".to_owned());

        assert!(active_state.hub_last_error.is_none());
        assert_eq!(
            active_state.hub_last_claim_error.as_deref(),
            Some("claim failed")
        );
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
            runtime_log_forward_statuses: HashMap::new(),
            last_remote_errors: HashMap::new(),
            hub_last_error: None,
            hub_last_claim_error: None,
            hub_last_ready_at: None,
        }
    }

    fn test_app_state() -> AppState {
        let (runtime_log_forward_tx, _runtime_log_forward_rx) =
            mpsc::channel(RUNTIME_LOG_FORWARD_QUEUE_CAPACITY);
        AppState {
            helper: HelperConfig {
                port: DEFAULT_HELPER_PORT,
                helper_id: "h_test".to_owned(),
                capacity: 1,
                user_name: "local-test".to_owned(),
                bearer_token: Arc::new(Mutex::new("test-token".to_owned())),
                bearer_token_source: Arc::new(Mutex::new("test".to_owned())),
                bearer_token_candidates: Arc::new(Vec::new()),
                domain_suffix: DEFAULT_DOMAIN_SUFFIX.to_owned(),
                hub_base_url: Some("http://127.0.0.1:1".to_owned()),
                studio_path: None,
                skip_claim_studio_launch: true,
                client: reqwest::Client::new(),
                hub_heartbeat_notify: Arc::new(Notify::new()),
            },
            state: Arc::new(Mutex::new(empty_helper_state())),
            runtime_log_forward_tx,
        }
    }

    async fn take_queued_plugin_command(
        app: &AppState,
        instance_id: &str,
        tool_name: &str,
    ) -> (String, Value) {
        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                let maybe_command = {
                    let mut state = app.state.lock().await;
                    let instance = state
                        .instances
                        .get_mut(instance_id)
                        .expect("test plugin instance should exist");
                    let position = instance.queue.iter().position(|command| {
                        command
                            .get("args")
                            .and_then(Value::as_object)
                            .and_then(|args| args.keys().next())
                            .map(|name| name == tool_name)
                            .unwrap_or(false)
                    });
                    position.and_then(|index| instance.queue.remove(index))
                };
                if let Some(command) = maybe_command {
                    let id = command
                        .get("id")
                        .and_then(Value::as_str)
                        .expect("test command should have id")
                        .to_owned();
                    return (id, command);
                }
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .expect("plugin command should be queued")
    }

    async fn send_plugin_response(
        app: &AppState,
        instance_id: &str,
        request_id: String,
        success: bool,
        response: String,
    ) {
        mcp_plugin_response_handler(
            State(app.clone()),
            Json(PluginResponsePayload {
                instance_id: Some(instance_id.to_owned()),
                id: request_id,
                success,
                response,
            }),
        )
        .await
        .expect("plugin response should be accepted");
    }

    fn test_app_state_with_runtime_log_worker() -> AppState {
        let (runtime_log_forward_tx, runtime_log_forward_rx) =
            mpsc::channel(RUNTIME_LOG_FORWARD_QUEUE_CAPACITY);
        let client = reqwest::Client::builder()
            .no_proxy()
            .build()
            .expect("test client should build");
        let app = AppState {
            helper: HelperConfig {
                port: DEFAULT_HELPER_PORT,
                helper_id: "h_test".to_owned(),
                capacity: 1,
                user_name: "local-test".to_owned(),
                bearer_token: Arc::new(Mutex::new("test-token".to_owned())),
                bearer_token_source: Arc::new(Mutex::new("test".to_owned())),
                bearer_token_candidates: Arc::new(Vec::new()),
                domain_suffix: DEFAULT_DOMAIN_SUFFIX.to_owned(),
                hub_base_url: Some("http://127.0.0.1:1".to_owned()),
                studio_path: None,
                skip_claim_studio_launch: true,
                client,
                hub_heartbeat_notify: Arc::new(Notify::new()),
            },
            state: Arc::new(Mutex::new(empty_helper_state())),
            runtime_log_forward_tx,
        };
        let worker_app = app.clone();
        tokio::spawn(async move {
            runtime_log_forward_worker(worker_app, runtime_log_forward_rx).await;
        });
        app
    }

    fn test_app_state_with_runtime_log_queue_only(
    ) -> (AppState, mpsc::Receiver<RuntimeLogForwardJob>) {
        let (runtime_log_forward_tx, runtime_log_forward_rx) =
            mpsc::channel(RUNTIME_LOG_FORWARD_QUEUE_CAPACITY);
        let client = reqwest::Client::builder()
            .no_proxy()
            .build()
            .expect("test client should build");
        let app = AppState {
            helper: HelperConfig {
                port: DEFAULT_HELPER_PORT,
                helper_id: "h_test".to_owned(),
                capacity: 1,
                user_name: "local-test".to_owned(),
                bearer_token: Arc::new(Mutex::new("test-token".to_owned())),
                bearer_token_source: Arc::new(Mutex::new("test".to_owned())),
                bearer_token_candidates: Arc::new(Vec::new()),
                domain_suffix: DEFAULT_DOMAIN_SUFFIX.to_owned(),
                hub_base_url: Some("http://127.0.0.1:1".to_owned()),
                studio_path: None,
                skip_claim_studio_launch: true,
                client,
                hub_heartbeat_notify: Arc::new(Notify::new()),
            },
            state: Arc::new(Mutex::new(empty_helper_state())),
            runtime_log_forward_tx,
        };
        (app, runtime_log_forward_rx)
    }

    async fn spawn_test_runtime_log_sink(
        status: u16,
        delay: Duration,
    ) -> (String, oneshot::Receiver<String>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("test sink should bind");
        let addr = listener.local_addr().expect("test sink should have addr");
        let (tx, rx) = oneshot::channel();
        tokio::spawn(async move {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let mut buffer = vec![0; 4096];
            let Ok(read) = socket.read(&mut buffer).await else {
                return;
            };
            let request = String::from_utf8_lossy(&buffer[..read]).to_string();
            let _ = tx.send(request);
            tokio::time::sleep(delay).await;
            let reason = if (200..300).contains(&status) {
                "OK"
            } else {
                "ERROR"
            };
            let body = if (200..300).contains(&status) {
                ""
            } else {
                "sink failure"
            };
            let response = format!(
                "HTTP/1.1 {status} {reason}\r\nContent-Length: {}\r\n\r\n{body}",
                body.len()
            );
            let _ = socket.write_all(response.as_bytes()).await;
        });
        (format!("http://{addr}"), rx)
    }

    async fn spawn_disconnect_runtime_log_sink() -> String {
        use tokio::io::AsyncWriteExt;

        let listener = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("test sink should bind");
        let addr = listener.local_addr().expect("test sink should have addr");
        tokio::spawn(async move {
            if let Ok((mut socket, _)) = listener.accept().await {
                let _ = socket.write_all(b"not an http response\r\n\r\n").await;
                drop(socket);
            }
        });
        format!("http://{addr}")
    }

    #[test]
    fn cleanup_stale_instances_keeps_instance_waiting_for_plugin_response() {
        let mut state = empty_helper_state();
        state.instances.insert(
            "instance-a".to_owned(),
            test_instance(
                "93795519121520",
                Some("task-a"),
                INSTANCE_STALE_AFTER.as_millis() as u64 + 1_000,
            ),
        );
        let (response_tx, _response_rx) = oneshot::channel();
        state.waiting_for_plugin.insert(
            "request-a".to_owned(),
            PendingPluginResponse {
                instance_id: "instance-a".to_owned(),
                tool_name: "RunCode".to_owned(),
                response_tx,
            },
        );

        cleanup_stale_instances(&mut state);

        assert!(state.instances.contains_key("instance-a"));
    }

    #[test]
    fn cleanup_stale_instances_drops_stale_instance_without_pending_response() {
        let mut state = empty_helper_state();
        state.instances.insert(
            "instance-a".to_owned(),
            test_instance(
                "93795519121520",
                Some("task-a"),
                INSTANCE_STALE_AFTER.as_millis() as u64 + 1_000,
            ),
        );

        cleanup_stale_instances(&mut state);

        assert!(!state.instances.contains_key("instance-a"));
    }

    #[tokio::test]
    async fn helper_launch_rejects_play_state_without_recording_stop_request() {
        let app = test_app_state();
        {
            let mut state = app.state.lock().await;
            let mut instance = test_instance("93795519121520", Some("task-a"), 0);
            instance.studio_mode = Some("start_play".to_owned());
            instance.studio_mode_source = "play_control".to_owned();
            instance.studio_mode_observed_at = Some(Instant::now());
            instance.studio_control_state = "ready".to_owned();
            instance.studio_transition_phase = "running".to_owned();
            instance.studio_control_observed_at = Some(Instant::now());
            state.instances.insert("instance-a".to_owned(), instance);
        }
        let (sender, _receiver) = mpsc::unbounded_channel();
        let command = serde_json::json!({
            "id": "request-a",
            "args": {
                "LaunchStudioSession": {
                    "mode": "start_play"
                }
            }
        });

        let app_for_command = app.clone();
        let handle = tokio::spawn(async move {
            handle_remote_command(
                app_for_command,
                "93795519121520",
                Some("task-a"),
                command,
                sender,
                Arc::new(Mutex::new(HashMap::new())),
                "request-a".to_owned(),
            )
            .await
        });

        let (launch_id, _launch) =
            take_queued_plugin_command(&app, "instance-a", "LaunchStudioSession").await;
        send_plugin_response(
            &app,
            "instance-a",
            launch_id,
            false,
            "studio_already_playing: Studio is already in play/run mode".to_owned(),
        )
        .await;

        let error = handle
            .await
            .expect("remote command task should join")
            .expect_err("plugin must reject relaunching an existing play session");

        assert!(error.to_string().contains("studio_already_playing"));
        let state = app.state.lock().await;
        let instance = state.instances.get("instance-a").unwrap();
        assert_eq!(instance.stop_request_id, 0);
        assert_eq!(edit_runtime_state(instance), "launch_failed");
        assert_eq!(instance.studio_transition_phase, "running");
        assert!(instance.queue.is_empty());
        assert!(state.waiting_for_plugin.is_empty());
        drop(state);

        let heartbeat_after_launch_failed = mcp_plugin_edit_heartbeat_handler(
            State(app.clone()),
            Json(PluginEditHeartbeatRequest {
                instance_id: "instance-a".to_owned(),
            }),
        )
        .await
        .unwrap();
        assert_eq!(
            heartbeat_after_launch_failed.status(),
            StatusCode::NO_CONTENT
        );
        let state = app.state.lock().await;
        let instance = state.instances.get("instance-a").unwrap();
        assert_eq!(edit_runtime_state(instance), "launch_failed");
    }

    #[tokio::test]
    async fn helper_launch_forwards_from_stop_without_rewriting_response() {
        let app = test_app_state();
        {
            let mut state = app.state.lock().await;
            state.instances.insert(
                "instance-a".to_owned(),
                test_instance("93795519121520", Some("task-a"), 0),
            );
        }
        let (sender, _receiver) = mpsc::unbounded_channel();
        let command = serde_json::json!({
            "id": "request-a",
            "args": {
                "LaunchStudioSession": {
                    "mode": "start_play"
                }
            }
        });
        let expected_response = serde_json::json!({
            "message": "Launched Studio session in start_play.",
            "actions": ["start_play"],
            "requested_mode": "start_play",
            "restart_applied": false,
            "previous_mode": "stop",
            "final_mode": "start_play"
        })
        .to_string();
        let app_for_command = app.clone();
        let handle = tokio::spawn(async move {
            handle_remote_command(
                app_for_command,
                "93795519121520",
                Some("task-a"),
                command,
                sender,
                Arc::new(Mutex::new(HashMap::new())),
                "request-a".to_owned(),
            )
            .await
        });

        let (launch_id, launch_command) =
            take_queued_plugin_command(&app, "instance-a", "LaunchStudioSession").await;
        {
            let state = app.state.lock().await;
            let instance = state.instances.get("instance-a").unwrap();
            assert_eq!(instance.stop_request_id, 0);
            assert_eq!(edit_runtime_state(instance), "launching");
            assert!(instance.queue.is_empty());
        }
        let heartbeat_while_launching = mcp_plugin_edit_heartbeat_handler(
            State(app.clone()),
            Json(PluginEditHeartbeatRequest {
                instance_id: "instance-a".to_owned(),
            }),
        )
        .await
        .unwrap();
        assert_eq!(heartbeat_while_launching.status(), StatusCode::NO_CONTENT);
        {
            let state = app.state.lock().await;
            let instance = state.instances.get("instance-a").unwrap();
            assert_eq!(edit_runtime_state(instance), "launching");
        }
        assert_eq!(
            launch_command["args"]["LaunchStudioSession"]["mode"],
            Value::String("start_play".to_owned())
        );
        send_plugin_response(
            &app,
            "instance-a",
            launch_id,
            true,
            expected_response.clone(),
        )
        .await;

        let response = handle
            .await
            .expect("remote command task should join")
            .expect("launch response should succeed");
        assert_eq!(response, expected_response);
        let state = app.state.lock().await;
        let instance = state.instances.get("instance-a").unwrap();
        assert_eq!(instance.stop_request_id, 0);
        assert_eq!(edit_runtime_state(instance), "runtime");
        drop(state);
        let heartbeat_after_runtime = mcp_plugin_edit_heartbeat_handler(
            State(app.clone()),
            Json(PluginEditHeartbeatRequest {
                instance_id: "instance-a".to_owned(),
            }),
        )
        .await
        .unwrap();
        assert_eq!(heartbeat_after_runtime.status(), StatusCode::NO_CONTENT);
        let state = app.state.lock().await;
        let instance = state.instances.get("instance-a").unwrap();
        assert_eq!(edit_runtime_state(instance), "runtime");
    }

    #[test]
    fn studio_control_tools_use_extended_plugin_timeout() {
        assert_eq!(
            plugin_request_timeout_for_tool("RunCode"),
            PLUGIN_REQUEST_TIMEOUT
        );
        assert_eq!(
            plugin_request_timeout_for_tool("LaunchStudioSession"),
            PLUGIN_STUDIO_CONTROL_REQUEST_TIMEOUT
        );
        assert_eq!(
            plugin_request_timeout_for_tool("StartStopPlay"),
            PLUGIN_STUDIO_CONTROL_REQUEST_TIMEOUT
        );
        assert!(PLUGIN_STUDIO_CONTROL_REQUEST_TIMEOUT > PLUGIN_REQUEST_TIMEOUT);
    }

    #[test]
    fn initial_control_state_is_idle_without_reported_mode() {
        let (control_state, transition_phase, observed_at) = initial_control_state_for_mode(None);

        assert_eq!(control_state, "none");
        assert_eq!(transition_phase, "idle");
        assert!(observed_at.is_none());
    }

    #[test]
    fn task_studio_mode_snapshot_reports_none_for_idle_stop_without_runtime_control() {
        let mut state = empty_helper_state();
        let instance = test_instance("93795519121520", Some("task-a"), 0);
        state.instances.insert("instance-a".to_owned(), instance);

        let snapshot = task_studio_mode_snapshot(&state, "task-a");

        assert_eq!(snapshot.studio_mode.as_deref(), Some("stop"));
        assert_eq!(snapshot.studio_mode_source, "edit_plugin");
        assert_eq!(snapshot.studio_control_state, "none");
        assert_eq!(snapshot.studio_transition_phase, "idle");
        assert_eq!(snapshot.edit_runtime_state, "ready");
    }

    #[test]
    fn task_studio_mode_snapshot_reports_none_for_idle_edit_without_reported_mode() {
        let mut state = empty_helper_state();
        let mut instance = test_instance("93795519121520", Some("task-a"), 0);
        instance.studio_mode = None;
        instance.studio_mode_observed_at = None;
        instance.studio_mode_source = "none".to_owned();
        state.instances.insert("instance-a".to_owned(), instance);

        let snapshot = task_studio_mode_snapshot(&state, "task-a");

        assert!(snapshot.studio_mode.is_none());
        assert_eq!(snapshot.studio_mode_source, "none");
        assert_eq!(snapshot.studio_control_state, "none");
        assert_eq!(snapshot.studio_transition_phase, "idle");
        assert_eq!(snapshot.edit_runtime_state, "ready");
    }

    #[test]
    fn task_studio_session_state_reports_none_connected_without_instances() {
        let state = empty_helper_state();
        let snapshot = task_studio_mode_snapshot(&state, "task-a");

        assert_eq!(snapshot.studio_session_state, "none_connected");
        assert!(snapshot.last_known_session_state.is_none());
        assert_eq!(
            snapshot.last_session_error_reason.as_deref(),
            Some("studio_plugin_not_connected")
        );
    }

    #[test]
    fn task_studio_session_state_reports_edit_connected_for_ready_edit_runtime() {
        let mut state = empty_helper_state();
        let mut edit_instance = test_instance("93795519121520", Some("task-a"), 0);
        edit_instance.studio_mode = None;
        edit_instance.studio_mode_source = "none".to_owned();
        edit_instance.edit_runtime_observed_at = Some(Instant::now());
        state
            .instances
            .insert("edit-instance".to_owned(), edit_instance);

        let snapshot = task_studio_mode_snapshot(&state, "task-a");

        assert_eq!(snapshot.studio_session_state, "edit_connected");
        assert_eq!(
            snapshot.last_known_session_state.as_deref(),
            Some("edit_connected")
        );
        assert!(snapshot.last_session_error_reason.is_none());
        assert_eq!(snapshot.edit_runtime_state, "ready");
    }

    #[test]
    fn task_studio_session_state_prioritizes_stopping_over_play() {
        let mut state = empty_helper_state();
        let mut instance = test_instance("93795519121520", Some("task-a"), 0);
        instance.studio_mode = Some("start_play".to_owned());
        instance.studio_mode_source = "play_control".to_owned();
        instance.studio_control_state = "stopping".to_owned();
        instance.studio_transition_phase = "stopping_requested".to_owned();
        instance.studio_control_observed_at = Some(Instant::now());
        instance.stop_request_id = 3;
        instance.stop_request_recorded_at = Some(Instant::now());
        let mut edit_instance = test_instance("93795519121520", Some("task-a"), 0);
        edit_instance.studio_mode = Some("stop".to_owned());
        edit_instance.studio_mode_source = "edit_plugin".to_owned();
        edit_instance.edit_runtime_observed_at = Some(Instant::now());
        state.instances.insert("instance-a".to_owned(), instance);
        state
            .instances
            .insert("edit-instance".to_owned(), edit_instance);

        let snapshot = task_studio_mode_snapshot(&state, "task-a");

        assert_eq!(snapshot.studio_session_state, "stopping");
        assert_eq!(snapshot.active_stop_request_id, Some(3));
        assert_eq!(
            snapshot.last_known_session_state.as_deref(),
            Some("stopping")
        );
    }

    #[test]
    fn active_stop_request_selection_reuses_stopping_instance_without_fresh_control() {
        let mut instances = HashMap::new();
        let mut stopping_instance = test_instance("93795519121520", Some("task-a"), 0);
        stopping_instance.studio_control_state = "stopping".to_owned();
        stopping_instance.studio_transition_phase = "stopping_requested".to_owned();
        stopping_instance.studio_control_observed_at = None;
        stopping_instance.stop_request_id = 7;
        instances.insert("stopping-instance".to_owned(), stopping_instance);

        let selected =
            select_active_stop_request_for_route("93795519121520", Some("task-a"), &instances);

        assert_eq!(selected, Some(("stopping-instance".to_owned(), 7)));
        assert!(
            select_runtime_actuator_for_route("93795519121520", Some("task-a"), &instances)
                .is_none()
        );
    }

    #[test]
    fn known_runtime_session_selection_blocks_unsafe_edit_stop_fallback() {
        let mut instances = HashMap::new();
        let mut runtime_instance = test_instance("93795519121520", Some("task-a"), 0);
        runtime_instance.studio_control_state = "ready".to_owned();
        runtime_instance.studio_transition_phase = "running".to_owned();
        runtime_instance.studio_control_observed_at =
            Some(Instant::now() - STUDIO_CONTROL_HEARTBEAT_STALE_AFTER - Duration::from_millis(1));
        set_edit_runtime_unavailable(&mut runtime_instance, "runtime");
        instances.insert("runtime-instance".to_owned(), runtime_instance);

        assert!(
            select_runtime_actuator_for_route("93795519121520", Some("task-a"), &instances)
                .is_none()
        );
        assert_eq!(
            select_known_runtime_session_for_route("93795519121520", Some("task-a"), &instances),
            Some("runtime-instance".to_owned())
        );
    }

    #[test]
    fn launch_failed_session_selection_blocks_unsafe_edit_stop_fallback() {
        let mut instances = HashMap::new();
        let mut launch_failed_instance = test_instance("93795519121520", Some("task-a"), 0);
        launch_failed_instance.studio_control_state = "ready".to_owned();
        launch_failed_instance.studio_transition_phase = "idle".to_owned();
        launch_failed_instance.studio_control_observed_at =
            Some(Instant::now() - STUDIO_CONTROL_HEARTBEAT_STALE_AFTER - Duration::from_millis(1));
        set_edit_runtime_unavailable(&mut launch_failed_instance, "launch_failed");
        instances.insert("launch-failed-instance".to_owned(), launch_failed_instance);

        assert!(
            select_runtime_actuator_for_route("93795519121520", Some("task-a"), &instances)
                .is_none()
        );
        assert_eq!(
            select_known_runtime_session_for_route("93795519121520", Some("task-a"), &instances),
            Some("launch-failed-instance".to_owned())
        );
    }

    #[test]
    fn launching_session_selection_blocks_unsafe_edit_stop_fallback() {
        let mut instances = HashMap::new();
        let mut launching_instance = test_instance("93795519121520", Some("task-a"), 0);
        launching_instance.studio_control_state = "lost".to_owned();
        launching_instance.studio_transition_phase = "starting".to_owned();
        launching_instance.studio_control_observed_at = None;
        set_edit_runtime_unavailable(&mut launching_instance, "launching");
        instances.insert("launching-instance".to_owned(), launching_instance);

        assert!(
            select_runtime_actuator_for_route("93795519121520", Some("task-a"), &instances)
                .is_none()
        );
        assert_eq!(
            select_known_runtime_session_for_route("93795519121520", Some("task-a"), &instances),
            Some("launching-instance".to_owned())
        );
    }

    #[test]
    fn reported_play_mode_selection_blocks_unsafe_edit_stop_fallback() {
        let mut instances = HashMap::new();
        let mut play_instance = test_instance("93795519121520", Some("task-a"), 0);
        play_instance.studio_mode = Some("start_play".to_owned());
        play_instance.studio_mode_observed_at = Some(Instant::now());
        play_instance.studio_mode_source = "diagnostic".to_owned();
        play_instance.studio_control_state = "lost".to_owned();
        play_instance.studio_transition_phase = "idle".to_owned();
        play_instance.studio_control_observed_at = None;
        instances.insert("play-instance".to_owned(), play_instance);

        assert!(
            select_runtime_actuator_for_route("93795519121520", Some("task-a"), &instances)
                .is_none()
        );
        assert_eq!(
            select_known_runtime_session_for_route("93795519121520", Some("task-a"), &instances),
            Some("play-instance".to_owned())
        );
    }

    #[test]
    fn task_active_studio_pid_prefers_registered_plugin_pid() {
        let mut state = empty_helper_state();
        state.claimed_tasks.insert(
            "task-a".to_owned(),
            test_claimed_task("task-a", "93795519121520"),
        );
        state.launch_processes.insert(
            "task-a".to_owned(),
            LaunchProcessRecord {
                task_id: "task-a".to_owned(),
                place_id: "93795519121520".to_owned(),
                universe_id: Some("999".to_owned()),
                studio_pid: 21632,
                launched_by_helper: true,
                #[cfg(target_os = "windows")]
                kill_on_close_job: None,
                child: None,
            },
        );
        let mut instance = test_instance("93795519121520", Some("task-a"), 0);
        instance.studio_pid = Some(19180);
        state.instances.insert("instance-a".to_owned(), instance);

        assert_eq!(task_active_studio_pid(&state, "task-a"), Some(19180));
        let launch = state.launch_processes.get("task-a").unwrap();
        assert!(!launch_process_is_active_for_status(&state, launch));
    }

    #[test]
    fn task_studio_mode_snapshot_reports_ready_while_control_heartbeat_is_fresh() {
        let mut state = empty_helper_state();
        let mut instance = test_instance("93795519121520", Some("task-a"), 0);
        instance.studio_mode = Some("start_play".to_owned());
        instance.studio_mode_observed_at = Some(Instant::now());
        instance.studio_control_state = "ready".to_owned();
        instance.studio_transition_phase = "running".to_owned();
        instance.studio_control_observed_at = Some(Instant::now());
        state.instances.insert("instance-a".to_owned(), instance);

        let snapshot = task_studio_mode_snapshot(&state, "task-a");

        assert_eq!(snapshot.studio_mode.as_deref(), Some("start_play"));
        assert_eq!(snapshot.studio_control_state, "ready");
        assert_eq!(snapshot.studio_transition_phase, "running");
    }

    #[test]
    fn task_studio_mode_snapshot_reports_lost_after_control_heartbeat_expires() {
        let mut state = empty_helper_state();
        let mut instance = test_instance("93795519121520", Some("task-a"), 0);
        instance.studio_mode = Some("start_play".to_owned());
        instance.studio_mode_observed_at = Some(Instant::now());
        instance.studio_control_state = "ready".to_owned();
        instance.studio_transition_phase = "running".to_owned();
        instance.studio_control_observed_at =
            Some(Instant::now() - STUDIO_CONTROL_HEARTBEAT_STALE_AFTER - Duration::from_millis(1));
        state.instances.insert("instance-a".to_owned(), instance);

        let snapshot = task_studio_mode_snapshot(&state, "task-a");

        assert_eq!(snapshot.studio_mode.as_deref(), Some("start_play"));
        assert_eq!(snapshot.studio_control_state, "lost");
        assert_eq!(snapshot.studio_transition_phase, "running");
    }

    #[tokio::test]
    async fn edit_heartbeat_completes_pending_stop_without_reporting_stop_mode() {
        let app = test_app_state();
        {
            let mut state = app.state.lock().await;
            let mut instance = test_instance("93795519121520", Some("task-a"), 0);
            instance.studio_mode = None;
            instance.studio_mode_source = "none".to_owned();
            instance.studio_control_state = "stopping".to_owned();
            instance.studio_transition_phase = "stopping_observed".to_owned();
            instance.studio_transition_started_at = Some(Instant::now());
            instance.studio_control_observed_at = Some(
                Instant::now() - STUDIO_CONTROL_HEARTBEAT_STALE_AFTER - Duration::from_millis(1),
            );
            instance.stop_request_id = 4;
            instance.stop_result_phase = Some("observed".to_owned());
            instance.stop_result_observed_at = Some(Instant::now());
            state.instances.insert("instance-a".to_owned(), instance);
        }

        let response = mcp_plugin_edit_heartbeat_handler(
            State(app.clone()),
            Json(PluginEditHeartbeatRequest {
                instance_id: "instance-a".to_owned(),
            }),
        )
        .await
        .unwrap();
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
        let state = app.state.lock().await;
        let instance = state.instances.get("instance-a").unwrap();

        assert!(instance.studio_mode.is_none());
        assert_eq!(instance.studio_mode_source, "none");
        assert_eq!(instance.studio_control_state, "none");
        assert_eq!(instance.studio_transition_phase, "idle");
        assert_eq!(instance.stop_result_phase.as_deref(), Some("completed"));
        assert!(instance.studio_control_observed_at.is_none());
        assert!(instance.edit_runtime_observed_at.is_some());

        let snapshot = task_studio_mode_snapshot(&state, "task-a");
        assert_eq!(snapshot.studio_control_state, "none");
        assert_eq!(snapshot.studio_transition_phase, "idle");
        assert_eq!(snapshot.edit_runtime_state, "ready");
    }

    #[tokio::test]
    async fn edit_heartbeat_does_not_complete_stop_before_runtime_observes_it() {
        let app = test_app_state();
        {
            let mut state = app.state.lock().await;
            let mut instance = test_instance("93795519121520", Some("task-a"), 0);
            instance.studio_mode = None;
            instance.studio_mode_source = "none".to_owned();
            instance.studio_control_state = "stopping".to_owned();
            instance.studio_transition_phase = "stopping_requested".to_owned();
            instance.studio_transition_started_at = Some(Instant::now());
            instance.studio_control_observed_at = Some(Instant::now());
            instance.stop_request_id = 4;
            state.instances.insert("instance-a".to_owned(), instance);
        }

        let response = mcp_plugin_edit_heartbeat_handler(
            State(app.clone()),
            Json(PluginEditHeartbeatRequest {
                instance_id: "instance-a".to_owned(),
            }),
        )
        .await
        .unwrap();
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
        let state = app.state.lock().await;
        let instance = state.instances.get("instance-a").unwrap();

        assert_eq!(instance.studio_control_state, "stopping");
        assert_eq!(instance.studio_transition_phase, "stopping_requested");
        assert_eq!(instance.stop_result_phase, None);
        assert_eq!(edit_runtime_state(instance), "stopping");
        assert_eq!(active_stop_request_id_for_instance(instance), Some(4));
    }

    #[tokio::test]
    async fn edit_heartbeat_does_not_complete_observed_stop_while_runtime_control_is_fresh() {
        let app = test_app_state();
        {
            let mut state = app.state.lock().await;
            let mut instance = test_instance("93795519121520", Some("task-a"), 0);
            instance.studio_mode = None;
            instance.studio_mode_source = "none".to_owned();
            instance.studio_control_state = "stopping".to_owned();
            instance.studio_transition_phase = "stopping_observed".to_owned();
            instance.studio_transition_started_at = Some(Instant::now());
            instance.studio_control_observed_at = Some(Instant::now());
            instance.stop_request_id = 4;
            instance.stop_result_phase = Some("observed".to_owned());
            instance.stop_result_observed_at = Some(Instant::now());
            state.instances.insert("instance-a".to_owned(), instance);
        }

        let response = mcp_plugin_edit_heartbeat_handler(
            State(app.clone()),
            Json(PluginEditHeartbeatRequest {
                instance_id: "instance-a".to_owned(),
            }),
        )
        .await
        .unwrap();
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
        let state = app.state.lock().await;
        let instance = state.instances.get("instance-a").unwrap();

        assert_eq!(instance.studio_control_state, "stopping");
        assert_eq!(instance.studio_transition_phase, "stopping_observed");
        assert_eq!(instance.stop_result_phase.as_deref(), Some("observed"));
        assert_eq!(edit_runtime_state(instance), "stopping");
        assert_eq!(active_stop_request_id_for_instance(instance), Some(4));
    }

    #[test]
    fn runtime_actuator_heartbeat_does_not_clobber_edit_heartbeat() {
        let mut instance = test_instance("93795519121520", Some("task-a"), 0);
        instance.studio_mode = None;
        instance.studio_mode_source = "none".to_owned();
        instance.studio_control_state = "ready".to_owned();
        instance.studio_transition_phase = "running".to_owned();
        instance.studio_control_observed_at = Some(Instant::now());
        instance.edit_runtime_observed_at = Some(Instant::now());

        assert_eq!(instance.studio_control_state, "ready");
        assert_eq!(instance.studio_transition_phase, "running");
        assert_eq!(edit_runtime_state(&instance), "ready");
    }

    #[tokio::test]
    async fn control_heartbeat_rejects_invalid_or_stopped_modes_without_mutating_instance() {
        let app = test_app_state();
        {
            let mut state = app.state.lock().await;
            state.instances.insert(
                "instance-a".to_owned(),
                test_instance("93795519121520", Some("task-a"), 0),
            );
        }

        let invalid = mcp_plugin_control_heartbeat_handler(
            State(app.clone()),
            Json(PluginControlHeartbeatRequest {
                instance_id: "instance-a".to_owned(),
                mode: "bogus".to_owned(),
            }),
        )
        .await
        .unwrap();
        assert_eq!(invalid.status(), StatusCode::BAD_REQUEST);

        let stopped = mcp_plugin_control_heartbeat_handler(
            State(app.clone()),
            Json(PluginControlHeartbeatRequest {
                instance_id: "instance-a".to_owned(),
                mode: "stop".to_owned(),
            }),
        )
        .await
        .unwrap();
        assert_eq!(stopped.status(), StatusCode::BAD_REQUEST);

        let state = app.state.lock().await;
        let instance = state.instances.get("instance-a").unwrap();
        assert_eq!(instance.studio_mode.as_deref(), Some("stop"));
        assert_eq!(instance.studio_control_state, "none");
        assert_eq!(instance.studio_transition_phase, "idle");
        assert!(instance.studio_control_observed_at.is_none());
    }

    #[tokio::test]
    async fn control_heartbeat_keeps_edit_runtime_unavailable_during_runtime() {
        let app = test_app_state();
        {
            let mut state = app.state.lock().await;
            let mut instance = test_instance("93795519121520", Some("task-a"), 0);
            instance.studio_mode = None;
            instance.studio_mode_source = "none".to_owned();
            instance.edit_runtime_observed_at = Some(Instant::now());
            state.instances.insert("instance-a".to_owned(), instance);
        }

        let response = mcp_plugin_control_heartbeat_handler(
            State(app.clone()),
            Json(PluginControlHeartbeatRequest {
                instance_id: "instance-a".to_owned(),
                mode: "start_play".to_owned(),
            }),
        )
        .await
        .unwrap();
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
        {
            let state = app.state.lock().await;
            let instance = state.instances.get("instance-a").unwrap();
            assert_eq!(edit_runtime_state(instance), "runtime");
        }

        let heartbeat = mcp_plugin_edit_heartbeat_handler(
            State(app.clone()),
            Json(PluginEditHeartbeatRequest {
                instance_id: "instance-a".to_owned(),
            }),
        )
        .await
        .unwrap();
        assert_eq!(heartbeat.status(), StatusCode::NO_CONTENT);
        let state = app.state.lock().await;
        let instance = state.instances.get("instance-a").unwrap();
        assert_eq!(edit_runtime_state(instance), "runtime");
    }

    #[tokio::test]
    async fn edit_heartbeat_recovers_runtime_latch_after_manual_stop() {
        let app = test_app_state();
        {
            let mut state = app.state.lock().await;
            let mut instance = test_instance("93795519121520", Some("task-a"), 0);
            instance.studio_control_state = "ready".to_owned();
            instance.studio_transition_phase = "running".to_owned();
            instance.studio_control_observed_at = Some(
                Instant::now() - STUDIO_CONTROL_HEARTBEAT_STALE_AFTER - Duration::from_millis(1),
            );
            set_edit_runtime_unavailable(&mut instance, "runtime");
            state.instances.insert("instance-a".to_owned(), instance);
        }

        let heartbeat = mcp_plugin_edit_heartbeat_handler(
            State(app.clone()),
            Json(PluginEditHeartbeatRequest {
                instance_id: "instance-a".to_owned(),
            }),
        )
        .await
        .unwrap();
        assert_eq!(heartbeat.status(), StatusCode::NO_CONTENT);
        let state = app.state.lock().await;
        let instance = state.instances.get("instance-a").unwrap();
        assert_eq!(instance.studio_control_state, "none");
        assert_eq!(instance.studio_transition_phase, "idle");
        assert_eq!(edit_runtime_state(instance), "ready");
    }

    #[tokio::test]
    async fn edit_heartbeat_recovers_launch_failed_latch_after_manual_stop() {
        let app = test_app_state();
        {
            let mut state = app.state.lock().await;
            let mut instance = test_instance("93795519121520", Some("task-a"), 0);
            instance.studio_control_state = "ready".to_owned();
            instance.studio_transition_phase = "running".to_owned();
            instance.studio_control_observed_at = Some(
                Instant::now() - STUDIO_CONTROL_HEARTBEAT_STALE_AFTER - Duration::from_millis(1),
            );
            set_edit_runtime_unavailable(&mut instance, "launch_failed");
            state.instances.insert("instance-a".to_owned(), instance);
        }

        let heartbeat = mcp_plugin_edit_heartbeat_handler(
            State(app.clone()),
            Json(PluginEditHeartbeatRequest {
                instance_id: "instance-a".to_owned(),
            }),
        )
        .await
        .unwrap();
        assert_eq!(heartbeat.status(), StatusCode::NO_CONTENT);
        let state = app.state.lock().await;
        let instance = state.instances.get("instance-a").unwrap();
        assert_eq!(instance.studio_control_state, "none");
        assert_eq!(instance.studio_transition_phase, "idle");
        assert_eq!(edit_runtime_state(instance), "ready");
    }

    #[tokio::test]
    async fn invalid_launch_mode_rejects_before_stop_request() {
        let app = test_app_state();
        {
            let mut state = app.state.lock().await;
            let mut instance = test_instance("93795519121520", Some("task-a"), 0);
            instance.studio_mode = Some("start_play".to_owned());
            instance.studio_mode_source = "play_control".to_owned();
            instance.studio_control_state = "ready".to_owned();
            instance.studio_transition_phase = "running".to_owned();
            instance.studio_control_observed_at = Some(Instant::now());
            state.instances.insert("instance-a".to_owned(), instance);
        }
        let (sender, _receiver) = mpsc::unbounded_channel();
        let command = serde_json::json!({
            "id": "request-a",
            "args": {
                "LaunchStudioSession": {
                    "mode": "bogus"
                }
            }
        });

        let error = handle_remote_command(
            app.clone(),
            "93795519121520",
            Some("task-a"),
            command,
            sender,
            Arc::new(Mutex::new(HashMap::new())),
            "request-a".to_owned(),
        )
        .await
        .expect_err("invalid launch mode must reject before stopping Studio");

        assert!(error
            .to_string()
            .contains("must be start_play or run_server"));
        let state = app.state.lock().await;
        let instance = state.instances.get("instance-a").unwrap();
        assert_eq!(instance.stop_request_id, 0);
        assert_eq!(instance.studio_control_state, "ready");
        assert_eq!(instance.studio_transition_phase, "running");
    }

    #[tokio::test]
    async fn stop_request_records_poll_and_completion_diagnostics() {
        let app = test_app_state();
        {
            let mut state = app.state.lock().await;
            let mut instance = test_instance("93795519121520", Some("task-a"), 0);
            instance.studio_mode = None;
            instance.studio_mode_observed_at = None;
            instance.studio_mode_source = "none".to_owned();
            instance.studio_control_state = "ready".to_owned();
            instance.studio_transition_phase = "running".to_owned();
            instance.studio_control_observed_at = Some(Instant::now());
            state.instances.insert("instance-a".to_owned(), instance);
        }

        let accepted_stop = mcp_plugin_stop_request_handler(
            State(app.clone()),
            Json(PluginStopRequestPayload {
                instance_id: "instance-a".to_owned(),
            }),
        )
        .await
        .unwrap();
        assert_eq!(accepted_stop.status(), StatusCode::OK);
        {
            let state = app.state.lock().await;
            let instance = state.instances.get("instance-a").unwrap();
            assert_eq!(instance.stop_request_id, 1);
            assert_eq!(instance.studio_control_state, "stopping");
            assert_eq!(instance.studio_transition_phase, "stopping_requested");
            assert!(instance.stop_request_recorded_at.is_some());
            assert_eq!(active_stop_request_id_for_instance(instance), Some(1));
            assert!(instance.stop_result_phase.is_none());
        }

        let poll_stop = mcp_plugin_stop_request_poll_handler(
            State(app.clone()),
            Query(PluginStopRequestQuery {
                instance_id: "instance-a".to_owned(),
                after_id: Some(0),
            }),
        )
        .await
        .unwrap();
        assert_eq!(poll_stop.status(), StatusCode::OK);
        let body = axum::body::to_bytes(poll_stop.into_body(), usize::MAX)
            .await
            .unwrap();
        let payload: PluginStopRequestResponse = serde_json::from_slice(&body).unwrap();
        assert!(payload.stop_requested);
        assert_eq!(payload.stop_request_id, 1);
        {
            let state = app.state.lock().await;
            let instance = state.instances.get("instance-a").unwrap();
            assert!(instance.stop_request_last_polled_at.is_some());
            assert_eq!(instance.stop_request_last_poll_id, Some(1));
        }

        {
            let state = app.state.lock().await;
            let snapshot = task_studio_mode_snapshot(&state, "task-a");
            assert_eq!(snapshot.studio_transition_phase, "stopping_requested");
            assert_eq!(snapshot.studio_control_state, "stopping");
            assert_eq!(snapshot.active_stop_request_id, Some(1));
            assert_eq!(snapshot.last_stop_request_id, 1);
            assert_eq!(snapshot.runtime_actuator_last_poll_id, Some(1));
        }

        let observed = mcp_plugin_stop_result_handler(
            State(app.clone()),
            Json(PluginStopResultPayload {
                instance_id: "instance-a".to_owned(),
                stop_request_id: 1,
                phase: "observed".to_owned(),
                error: None,
            }),
        )
        .await
        .unwrap();
        assert_eq!(observed.status(), StatusCode::NO_CONTENT);
        {
            let mut state = app.state.lock().await;
            let instance = state.instances.get_mut("instance-a").unwrap();
            instance.studio_control_observed_at = Some(
                Instant::now() - STUDIO_CONTROL_HEARTBEAT_STALE_AFTER - Duration::from_millis(1),
            );
        }

        let response = mcp_plugin_edit_heartbeat_handler(
            State(app.clone()),
            Json(PluginEditHeartbeatRequest {
                instance_id: "instance-a".to_owned(),
            }),
        )
        .await
        .unwrap();
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
        let state = app.state.lock().await;
        let instance = state.instances.get("instance-a").unwrap();
        assert!(instance.studio_mode.is_none());
        assert_eq!(instance.studio_control_state, "none");
        assert_eq!(instance.studio_transition_phase, "idle");
        assert_eq!(instance.stop_result_phase.as_deref(), Some("completed"));
        assert!(instance.stop_result_error.is_none());
        assert!(instance.stop_result_observed_at.is_some());
    }

    #[tokio::test]
    async fn stop_request_rejects_stale_runtime_actuator_without_recording() {
        let app = test_app_state();
        {
            let mut state = app.state.lock().await;
            let mut instance = test_instance("93795519121520", Some("task-a"), 0);
            instance.studio_control_state = "ready".to_owned();
            instance.studio_transition_phase = "running".to_owned();
            instance.studio_control_observed_at = Some(
                Instant::now() - STUDIO_CONTROL_HEARTBEAT_STALE_AFTER - Duration::from_millis(1),
            );
            set_edit_runtime_unavailable(&mut instance, "runtime");
            state.instances.insert("instance-a".to_owned(), instance);
        }

        let rejected_stop = mcp_plugin_stop_request_handler(
            State(app.clone()),
            Json(PluginStopRequestPayload {
                instance_id: "instance-a".to_owned(),
            }),
        )
        .await
        .unwrap();
        assert_eq!(rejected_stop.status(), StatusCode::CONFLICT);
        let state = app.state.lock().await;
        let instance = state.instances.get("instance-a").unwrap();
        assert_eq!(instance.stop_request_id, 0);
        assert_eq!(instance.studio_transition_phase, "running");
        assert_eq!(edit_runtime_state(instance), "runtime");
    }

    #[tokio::test]
    async fn stop_fallback_rejects_when_edit_runtime_is_not_ready() {
        let app = test_app_state();
        {
            let mut state = app.state.lock().await;
            let mut instance = test_instance("93795519121520", Some("task-a"), 0);
            instance.edit_runtime_observed_at =
                Some(Instant::now() - EDIT_RUNTIME_STALE_AFTER - Duration::from_millis(1));
            instance.studio_control_state = "none".to_owned();
            instance.studio_transition_phase = "idle".to_owned();
            state.instances.insert("instance-a".to_owned(), instance);
        }
        let (sender, _receiver) = mpsc::unbounded_channel();
        let command = serde_json::json!({
            "id": "request-a",
            "args": {
                "StartStopPlay": {
                    "mode": "stop"
                }
            }
        });

        let error = handle_remote_command(
            app.clone(),
            "93795519121520",
            Some("task-a"),
            command,
            sender,
            Arc::new(Mutex::new(HashMap::new())),
            "request-a".to_owned(),
        )
        .await
        .expect_err("stale edit runtime must reject unsafe stop fallback");

        assert!(error
            .to_string()
            .contains("edit runtime is not ready for safe stop fallback"));
        let state = app.state.lock().await;
        let instance = state.instances.get("instance-a").unwrap();
        assert!(instance.queue.is_empty());
        assert_eq!(instance.stop_request_id, 0);
    }

    #[tokio::test]
    async fn stop_fallback_allows_ready_edit_runtime_noop() {
        let app = test_app_state();
        {
            let mut state = app.state.lock().await;
            let mut instance = test_instance("93795519121520", Some("task-a"), 0);
            instance.edit_runtime_observed_at = Some(Instant::now());
            instance.studio_control_state = "none".to_owned();
            instance.studio_transition_phase = "idle".to_owned();
            state.instances.insert("instance-a".to_owned(), instance);
        }
        let (sender, _receiver) = mpsc::unbounded_channel();
        let command = serde_json::json!({
            "id": "request-a",
            "args": {
                "StartStopPlay": {
                    "mode": "stop"
                }
            }
        });
        let app_for_command = app.clone();
        let handle = tokio::spawn(async move {
            handle_remote_command(
                app_for_command,
                "93795519121520",
                Some("task-a"),
                command,
                sender,
                Arc::new(Mutex::new(HashMap::new())),
                "request-a".to_owned(),
            )
            .await
        });

        let (request_id, queued_command) =
            take_queued_plugin_command(&app, "instance-a", "StartStopPlay").await;
        assert_eq!(
            queued_command["args"]["StartStopPlay"]["mode"],
            Value::String("stop".to_owned())
        );
        send_plugin_response(&app, "instance-a", request_id, true, "Stopped".to_owned()).await;
        let response = handle
            .await
            .expect("remote command task should join")
            .expect("ready edit runtime fallback should succeed");
        assert_eq!(response, "Stopped");
    }

    #[test]
    fn runtime_stop_timeout_converges_to_error_and_disables_delivery() {
        let mut instance = test_instance("93795519121520", Some("task-a"), 0);
        instance.studio_control_state = "stopping".to_owned();
        instance.studio_transition_phase = "stopping_requested".to_owned();
        instance.stop_request_id = 4;

        mark_runtime_stop_timeout(&mut instance, 4, "phase=stopping_requested".to_owned());

        assert_eq!(instance.studio_control_state, "lost");
        assert_eq!(instance.studio_transition_phase, "error");
        assert_eq!(active_stop_request_id_for_instance(&instance), None);
        assert!(!is_stop_request_deliverable_phase(
            &instance.studio_transition_phase
        ));
        assert_eq!(instance.stop_result_phase.as_deref(), Some("failed"));
        assert!(instance
            .studio_control_last_error
            .as_deref()
            .unwrap_or_default()
            .contains("runtime_stop_timeout"));
    }

    #[tokio::test]
    async fn runtime_stop_result_acknowledges_and_fails_active_stop_request() {
        let app = test_app_state();
        {
            let mut state = app.state.lock().await;
            let mut instance = test_instance("93795519121520", Some("task-a"), 0);
            instance.studio_mode = Some("start_play".to_owned());
            instance.studio_mode_source = "play_control".to_owned();
            instance.studio_mode_observed_at = Some(Instant::now());
            instance.studio_control_state = "stopping".to_owned();
            instance.studio_transition_phase = "stopping_requested".to_owned();
            instance.studio_control_observed_at = Some(Instant::now());
            instance.stop_request_id = 2;
            state.instances.insert("instance-a".to_owned(), instance);
        }

        let observed = mcp_plugin_stop_result_handler(
            State(app.clone()),
            Json(PluginStopResultPayload {
                instance_id: "instance-a".to_owned(),
                stop_request_id: 2,
                phase: "observed".to_owned(),
                error: None,
            }),
        )
        .await
        .unwrap();
        assert_eq!(observed.status(), StatusCode::NO_CONTENT);
        {
            let state = app.state.lock().await;
            let instance = state.instances.get("instance-a").unwrap();
            assert_eq!(instance.studio_control_state, "stopping");
            assert_eq!(instance.studio_transition_phase, "stopping_observed");
            assert!(instance.studio_control_last_error.is_none());
        }

        let poll_after_observed = mcp_plugin_stop_request_poll_handler(
            State(app.clone()),
            Query(PluginStopRequestQuery {
                instance_id: "instance-a".to_owned(),
                after_id: Some(0),
            }),
        )
        .await
        .unwrap();
        assert_eq!(poll_after_observed.status(), StatusCode::OK);
        let body = axum::body::to_bytes(poll_after_observed.into_body(), usize::MAX)
            .await
            .unwrap();
        let payload: PluginStopRequestResponse = serde_json::from_slice(&body).unwrap();
        assert!(!payload.stop_requested);
        assert_eq!(payload.stop_request_id, 2);

        let failed = mcp_plugin_stop_result_handler(
            State(app.clone()),
            Json(PluginStopResultPayload {
                instance_id: "instance-a".to_owned(),
                stop_request_id: 2,
                phase: "failed".to_owned(),
                error: Some("EndTest: can only be called once".to_owned()),
            }),
        )
        .await
        .unwrap();
        assert_eq!(failed.status(), StatusCode::NO_CONTENT);
        {
            let state = app.state.lock().await;
            let instance = state.instances.get("instance-a").unwrap();
            assert_eq!(effective_studio_control_state(instance), "lost");
            assert_eq!(effective_studio_transition_phase(instance), "error");
            assert!(instance
                .studio_control_last_error
                .as_deref()
                .unwrap_or_default()
                .contains("EndTest: can only be called once"));
        }

        let heartbeat_after_failure = mcp_plugin_control_heartbeat_handler(
            State(app.clone()),
            Json(PluginControlHeartbeatRequest {
                instance_id: "instance-a".to_owned(),
                mode: "start_play".to_owned(),
            }),
        )
        .await
        .unwrap();
        assert_eq!(heartbeat_after_failure.status(), StatusCode::NO_CONTENT);
        let state = app.state.lock().await;
        let instance = state.instances.get("instance-a").unwrap();
        assert_eq!(effective_studio_control_state(instance), "lost");
        assert_eq!(effective_studio_transition_phase(instance), "error");
        assert_eq!(
            instance.studio_control_last_error.as_deref(),
            Some("runtime_stop_failed: play runtime failed to execute stop_request_id=2: EndTest: can only be called once")
        );
    }

    #[tokio::test]
    async fn immediate_task_status_update_queues_remote_snapshot_and_hub_notify() {
        let mut state = empty_helper_state();
        state.claimed_tasks.insert(
            "task-a".to_owned(),
            test_claimed_task("task-a", "93795519121520"),
        );
        let mut instance = test_instance("93795519121520", Some("task-a"), 0);
        instance.studio_mode = Some("start_play".to_owned());
        instance.studio_mode_source = "play_control".to_owned();
        instance.studio_control_state = "ready".to_owned();
        instance.studio_transition_phase = "running".to_owned();
        instance.studio_control_observed_at = Some(Instant::now());
        state.instances.insert("instance-a".to_owned(), instance);
        let (sender, mut receiver) = mpsc::unbounded_channel();
        let (stop_tx, _stop_rx) = watch::channel(false);
        state.remote_connections.insert(
            task_connection_key("task-a").to_owned(),
            RemoteConnectionHandle {
                worker_id: Uuid::new_v4(),
                stop_tx,
                sender,
                remote_base_url: "http://127.0.0.1:44880".to_owned(),
                place_id: "93795519121520".to_owned(),
                task_id: Some("task-a".to_owned()),
                connection_state: RemoteConnectionState::Connected,
                state_changed_at: Instant::now(),
                retrying_since: None,
                consecutive_failures: 0,
                connection_id: Some("connection-a".to_owned()),
                last_ready_at: Some(Instant::now()),
                last_server_message_at: Some(Instant::now()),
            },
        );
        let app = test_app_state();

        assert!(queue_task_status_updates(&state, &app.helper, "task-a"));
        let frame = receiver
            .try_recv()
            .expect("heartbeat frame should be queued");
        let RemoteOutgoingFrame::Text(encoded) = frame else {
            panic!("expected text heartbeat frame");
        };
        let payload: serde_json::Value = serde_json::from_str(&encoded).unwrap();

        assert_eq!(payload["type"], "heartbeat");
        assert_eq!(payload["task_id"], "task-a");
        assert_eq!(payload["plugin_instance_count"], 1);
        assert_eq!(payload["task_status"]["studio_mode"], "start_play");
        assert_eq!(payload["task_status"]["studio_mode_source"], "play_control");
        assert_eq!(payload["task_status"]["studio_control_state"], "ready");
        assert_eq!(payload["task_status"]["studio_transition_phase"], "running");
        assert_eq!(payload["task_status"]["edit_runtime_state"], "ready");
        tokio::time::timeout(
            Duration::from_millis(50),
            app.helper.hub_heartbeat_notify.notified(),
        )
        .await
        .expect("hub heartbeat notify should be queued");
    }

    #[tokio::test]
    async fn task_status_update_notifies_hub_without_remote_connection() {
        let mut state = empty_helper_state();
        state.claimed_tasks.insert(
            "task-a".to_owned(),
            test_claimed_task("task-a", "93795519121520"),
        );
        let app = test_app_state();

        assert!(!queue_task_status_updates(&state, &app.helper, "task-a"));
        tokio::time::timeout(
            Duration::from_millis(50),
            app.helper.hub_heartbeat_notify.notified(),
        )
        .await
        .expect("hub heartbeat notify should not depend on server websocket");
    }

    fn test_remote_connection_handle(
        state: RemoteConnectionState,
        state_age: Duration,
        retrying_age: Option<Duration>,
    ) -> RemoteConnectionHandle {
        let (stop_tx, _stop_rx) = watch::channel(false);
        let (sender, _receiver) = mpsc::unbounded_channel();
        RemoteConnectionHandle {
            worker_id: Uuid::new_v4(),
            stop_tx,
            sender,
            remote_base_url: "https://example.com".to_owned(),
            place_id: "93795519121520".to_owned(),
            task_id: Some("task-a".to_owned()),
            connection_state: state,
            state_changed_at: Instant::now() - state_age,
            retrying_since: retrying_age.map(|age| Instant::now() - age),
            consecutive_failures: 1,
            connection_id: None,
            last_ready_at: None,
            last_server_message_at: None,
        }
    }

    #[test]
    fn remote_connection_watchdog_restarts_stale_retrying_connection() {
        let fresh_retrying = test_remote_connection_handle(
            RemoteConnectionState::Retrying,
            REMOTE_WS_STALE_RESTART_AFTER - Duration::from_secs(1),
            Some(Duration::from_secs(1)),
        );
        assert!(!remote_connection_should_restart(&fresh_retrying));
        assert!(!remote_connection_should_release(&fresh_retrying));

        let stale_retrying = test_remote_connection_handle(
            RemoteConnectionState::Retrying,
            REMOTE_WS_STALE_RESTART_AFTER + Duration::from_secs(1),
            Some(Duration::from_secs(10)),
        );
        assert!(remote_connection_should_restart(&stale_retrying));
        assert!(!remote_connection_should_release(&stale_retrying));
    }

    #[test]
    fn remote_connection_watchdog_restarts_stale_connecting_connection() {
        let stale_connecting = test_remote_connection_handle(
            RemoteConnectionState::Connecting,
            REMOTE_WS_STALE_RESTART_AFTER + Duration::from_secs(1),
            Some(Duration::from_secs(10)),
        );
        assert!(remote_connection_should_restart(&stale_connecting));
        assert!(!remote_connection_should_release(&stale_connecting));
    }

    #[test]
    fn remote_connection_watchdog_keeps_connected_connection_without_server_chatter() {
        let mut connected =
            test_remote_connection_handle(RemoteConnectionState::Connected, Duration::ZERO, None);
        connected.last_server_message_at = Some(Instant::now() - Duration::from_secs(60));
        assert!(!remote_connection_should_restart(&connected));
        assert!(!remote_connection_should_release(&connected));
    }

    #[test]
    fn remote_ws_pong_timeout_exceeds_ping_interval() {
        assert!(REMOTE_WS_PONG_TIMEOUT > REMOTE_WS_PING_INTERVAL);
        assert!(REMOTE_WS_PONG_TIMEOUT > REMOTE_WS_CONNECT_TIMEOUT);
    }

    #[test]
    fn remote_connection_watchdog_releases_long_unrecovered_retrying_connection() {
        let stale_retrying = test_remote_connection_handle(
            RemoteConnectionState::Retrying,
            REMOTE_WS_STALE_RESTART_AFTER + Duration::from_secs(1),
            Some(REMOTE_WS_STALE_RELEASE_AFTER + Duration::from_secs(1)),
        );
        assert!(remote_connection_should_restart(&stale_retrying));
        assert!(remote_connection_should_release(&stale_retrying));
    }

    #[test]
    fn release_claimed_task_stops_remote_connection_worker() {
        let mut state = empty_helper_state();
        state.claimed_tasks.insert(
            "task-a".to_owned(),
            test_claimed_task("task-a", "93795519121520"),
        );
        let (stop_tx, mut stop_rx) = watch::channel(false);
        let (sender, _receiver) = mpsc::unbounded_channel();
        state.remote_connections.insert(
            task_connection_key("task-a"),
            RemoteConnectionHandle {
                worker_id: Uuid::new_v4(),
                stop_tx,
                sender,
                remote_base_url: "https://example.com".to_owned(),
                place_id: "93795519121520".to_owned(),
                task_id: Some("task-a".to_owned()),
                connection_state: RemoteConnectionState::Retrying,
                state_changed_at: Instant::now(),
                retrying_since: Some(Instant::now()),
                consecutive_failures: 1,
                connection_id: None,
                last_ready_at: None,
                last_server_message_at: None,
            },
        );

        let pending = release_claimed_task(&mut state, "task-a", false, "test release");
        assert!(pending.is_none());
        assert!(!state.claimed_tasks.contains_key("task-a"));
        assert!(!state
            .remote_connections
            .contains_key(&task_connection_key("task-a")));
        assert!(*stop_rx.borrow_and_update());
    }

    #[test]
    fn remote_connection_state_name_reports_error_state() {
        let error_connection =
            test_remote_connection_handle(RemoteConnectionState::Error, Duration::ZERO, None);
        assert_eq!(
            remote_connection_state_name(Some(&error_connection)),
            "error"
        );
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
    fn rojo_forward_base_url_uses_single_helper_port_and_task_path() {
        assert_eq!(
            rojo_forward_base_url(44750, "93795519121520", "tff2f06bc6a"),
            "http://127.0.0.1:44750/rojo-forward/93795519121520/task/tff2f06bc6a"
        );
    }

    #[test]
    fn runtime_log_forward_upload_url_uses_single_helper_port_and_task_path() {
        assert_eq!(
            runtime_log_forward_upload_url(44750, "93795519121520", "tff2f06bc6a"),
            "http://127.0.0.1:44750/runtime-log-forward/93795519121520/task/tff2f06bc6a/v1/runtime-logs"
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
        assert!(!official_adapter_error_keeps_ready(&transient));

        let no_connected_studio = eyre!("official MCP found no connected Roblox Studio instances");
        assert!(!should_restart_official_process_after_error(
            &no_connected_studio
        ));
        assert!(!official_adapter_error_keeps_ready(&no_connected_studio));

        let opening_place = eyre!(
            "official MCP tool search_creator_store returned error: Execution is prevented because previously active Studio has disconnected or doesn't have a place opened."
        );
        assert!(!should_restart_official_process_after_error(&opening_place));
        assert!(!official_adapter_error_keeps_ready(&opening_place));

        let fatal = eyre!("official MCP process closed stdout");
        assert!(should_restart_official_process_after_error(&fatal));
        assert!(!official_adapter_error_keeps_ready(&fatal));
    }

    #[test]
    fn official_adapter_keeps_ready_for_tool_and_request_failures() {
        let workflow_failed =
            eyre!("official MCP tool wait_job_finished returned error: Workflow failed");
        assert!(!should_restart_official_process_after_error(
            &workflow_failed
        ));
        assert!(official_adapter_error_keeps_ready(&workflow_failed));

        let missing_generation = eyre!(
            "official MCP tool wait_job_finished returned error: No job found with generation ID: generation-id-does-not-exist"
        );
        assert!(!should_restart_official_process_after_error(
            &missing_generation
        ));
        assert!(official_adapter_error_keeps_ready(&missing_generation));

        let required_generation_id =
            eyre!("official MCP tool wait_job_finished returned error: generationId required");
        assert!(!should_restart_official_process_after_error(
            &required_generation_id
        ));
        assert!(official_adapter_error_keeps_ready(&required_generation_id));

        let missing_generation_id =
            eyre!("official MCP tool wait_job_finished returned error: generation_id missing");
        assert!(!should_restart_official_process_after_error(
            &missing_generation_id
        ));
        assert!(official_adapter_error_keeps_ready(&missing_generation_id));

        let required_job_id =
            eyre!("official MCP tool wait_job_finished returned error: job id required");
        assert!(!should_restart_official_process_after_error(
            &required_job_id
        ));
        assert!(official_adapter_error_keeps_ready(&required_job_id));

        let poll_generation_failed = eyre!(
            "official MCP tool wait_job_finished returned error: failed to poll generation ID result"
        );
        assert!(should_restart_official_process_after_error(
            &poll_generation_failed
        ));
        assert!(!official_adapter_error_keeps_ready(&poll_generation_failed));

        let generate_failed =
            eyre!("official MCP tool generate_mesh returned error: policy rejected input");
        assert!(!should_restart_official_process_after_error(
            &generate_failed
        ));
        assert!(official_adapter_error_keeps_ready(&generate_failed));

        let generate_channel_failed =
            eyre!("official MCP tool generate_mesh returned error: Studio connection lost");
        assert!(should_restart_official_process_after_error(
            &generate_channel_failed
        ));
        assert!(!official_adapter_error_keeps_ready(
            &generate_channel_failed
        ));

        let search_channel_failed =
            eyre!("official MCP tool search_creator_store returned error: Studio connection lost");
        assert!(should_restart_official_process_after_error(
            &search_channel_failed
        ));
        assert!(!official_adapter_error_keeps_ready(&search_channel_failed));

        let insert_channel_failed = eyre!(
            "official MCP tool insert_from_creator_store returned error: Roblox Studio unavailable"
        );
        assert!(should_restart_official_process_after_error(
            &insert_channel_failed
        ));
        assert!(!official_adapter_error_keeps_ready(&insert_channel_failed));

        let store_image_channel_failed =
            eyre!("official MCP tool store_image returned error: internal adapter failure");
        assert!(should_restart_official_process_after_error(
            &store_image_channel_failed
        ));
        assert!(!official_adapter_error_keeps_ready(
            &store_image_channel_failed
        ));

        let store_image_file_failed =
            eyre!("official MCP tool store_image returned error: file not found");
        assert!(!should_restart_official_process_after_error(
            &store_image_file_failed
        ));
        assert!(official_adapter_error_keeps_ready(&store_image_file_failed));

        let internal_studio_tool_failed =
            eyre!("official MCP tool list_roblox_studios returned error: unexpected failure");
        assert!(should_restart_official_process_after_error(
            &internal_studio_tool_failed
        ));
        assert!(!official_adapter_error_keeps_ready(
            &internal_studio_tool_failed
        ));

        let bad_image = eyre!("store_image imageBase64 content does not match mimeType");
        assert!(!should_restart_official_process_after_error(&bad_image));
        assert!(official_adapter_error_keeps_ready(&bad_image));

        let unsupported_action = eyre!("unsupported official MCP adapter action: unknown_action");
        assert!(!should_restart_official_process_after_error(
            &unsupported_action
        ));
        assert!(!official_adapter_error_keeps_ready(&unsupported_action));
        assert!(official_adapter_error_preserves_state(&unsupported_action));
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
    fn task_scoped_rojo_forward_resolves_ambiguous_place_binding() {
        let mut state = empty_helper_state();
        state.claimed_tasks.insert(
            "task_a".to_owned(),
            test_claimed_task("task_a", "93795519121520"),
        );
        state.claimed_tasks.insert(
            "task_b".to_owned(),
            test_claimed_task("task_b", "93795519121520"),
        );

        let selected = select_claimed_task(&state, "93795519121520", Some("task_b"))
            .expect("task-scoped Rojo forward should not depend on place uniqueness");

        assert_eq!(selected.task_id, "task_b");
    }

    #[test]
    fn bind_launch_process_preserves_helper_launch_ownership() {
        let mut state = empty_helper_state();
        let claimed_task = test_claimed_task("task_a", "93795519121520");
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

        bind_launch_process_to_pid(&mut state, &claimed_task, 222)
            .expect("plugin registration should update pid binding");

        let launch = state
            .launch_processes
            .get("task_a")
            .expect("launch process should stay tracked");
        assert_eq!(launch.studio_pid, 222);
        assert!(launch.launched_by_helper);
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

    #[test]
    fn store_image_legacy_file_path_passes_through() {
        let (arguments, cleanup) = prepare_official_store_image_arguments(
            "t_example",
            "req_example",
            serde_json::json!({ "filePath": "C:\\tmp\\probe.png" }),
        )
        .expect("legacy Windows filePath should pass through");

        assert!(cleanup.is_none());
        assert_eq!(arguments["filePath"], "C:\\tmp\\probe.png");
    }

    #[test]
    fn store_image_base64_writes_task_scoped_temp_file() {
        let png_base64 =
            "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+/p9sAAAAASUVORK5CYII=";
        let (arguments, cleanup) = prepare_official_store_image_arguments(
            "t_example",
            "req_example",
            serde_json::json!({
                "imageBase64": png_base64,
                "mimeType": "image/png",
                "fileName": "..\\bad name.png",
            }),
        )
        .expect("base64 image should be written to helper temp storage");
        let cleanup = cleanup.expect("base64 mode should create a temp cleanup guard");
        let path = cleanup.path.clone();

        assert_eq!(
            arguments["filePath"].as_str(),
            Some(path.to_string_lossy().as_ref())
        );
        assert!(path.exists());
        assert!(path
            .components()
            .any(|component| component.as_os_str() == "official-images"));
        assert!(path
            .file_name()
            .and_then(|value| value.to_str())
            .unwrap_or_default()
            .ends_with(".png"));

        drop(cleanup);
        assert!(!path.exists());
    }

    #[test]
    fn store_image_base64_rejects_mime_mismatch() {
        let jpeg_header_as_base64 = "/9j/AA==";
        let error = prepare_official_store_image_arguments(
            "t_example",
            "req_example",
            serde_json::json!({
                "imageBase64": jpeg_header_as_base64,
                "mimeType": "image/png",
            }),
        )
        .unwrap_err();

        assert!(error.to_string().contains("does not match mimeType"));
    }
}
