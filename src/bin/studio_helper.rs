use axum::extract::{ConnectInfo, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use clap::Parser;
use color_eyre::eyre::{eyre, Result, WrapErr};
use reqwest::header::{AUTHORIZATION, CONTENT_TYPE};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{HashMap, HashSet, VecDeque};
use std::env;
use std::fs;
use std::net::{Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{Mutex, Notify, oneshot};
use tracing_subscriber::EnvFilter;
use uuid::Uuid;

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
}

#[derive(Clone)]
struct HelperConfig {
    port: u16,
    user_name: String,
    bearer_token: Arc<Mutex<String>>,
    bearer_token_source: Arc<Mutex<String>>,
    bearer_token_candidates: Arc<Vec<ResolvedToken>>,
    domain_suffix: String,
    client: reqwest::Client,
}

#[derive(Clone)]
struct ResolvedToken {
    value: String,
    source: String,
}

struct PluginInstance {
    place_id: String,
    studio_pid: Option<u32>,
    last_seen_at: Instant,
    queue: VecDeque<Value>,
    notify: Arc<Notify>,
}

struct HelperState {
    instances: HashMap<String, PluginInstance>,
    waiting_for_plugin: HashMap<String, oneshot::Sender<PluginResponsePayload>>,
    active_remote_places: HashSet<String>,
    last_remote_errors: HashMap<String, String>,
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
}

#[derive(Debug, Serialize)]
struct RegisterPluginResponse {
    instance_id: String,
    place_id: String,
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
    user_name: String,
    active_instances: usize,
    active_places: Vec<String>,
    instances: Vec<HelperInstanceStatus>,
    last_remote_errors: HashMap<String, String>,
}

#[derive(Debug, Serialize)]
struct HelperInstanceStatus {
    instance_id: String,
    place_id: String,
    studio_pid: Option<u32>,
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
}

#[derive(Debug, Serialize)]
struct RojoConfigResponse {
    place_id: String,
    base_url: String,
    auth_header: String,
}

#[derive(Debug, Deserialize)]
struct ScreenshotQuery {
    #[serde(rename = "placeId")]
    place_id: Option<String>,
}

#[derive(Debug, Serialize)]
struct ScreenshotResponse {
    path: String,
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

#[cfg(target_os = "windows")]
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

fn sanitize_place_id(value: &str) -> Result<String> {
    let trimmed = trim(value);
    if trimmed.is_empty() || !trimmed.chars().all(|ch| ch.is_ascii_digit()) {
        return Err(eyre!("placeId must be digits only"));
    }
    Ok(trimmed)
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

    search.candidates.push(ChildWindowCandidate { hwnd, width, height });
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
    let rows = unsafe { std::slice::from_raw_parts(table.table.as_ptr(), table.dwNumEntries as usize) };
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

async fn resolve_peer_process_id_with_retry(peer_addr: SocketAddr, helper_port: u16) -> Result<Option<u32>> {
    for attempt in 0..10 {
        if let Some(pid) = resolve_peer_process_id(peer_addr, helper_port)? {
            return Ok(Some(pid));
        }
        if attempt < 9 {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
    Ok(None)
}

#[cfg(target_os = "windows")]
fn collect_visible_child_windows(parent_hwnd: HWND, min_width: i32, min_height: i32) -> Vec<ChildWindowCandidate> {
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
    let mut search = TopWindowSearch { candidates: Vec::new() };
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
        .find(|candidate| candidate.height > 600 && candidate.height < 750 && candidate.width > 2000)
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
    refresh_active_places(&mut state);
    let mut active_places: Vec<_> = state.active_remote_places.iter().cloned().collect();
    active_places.sort();
    let mut instances: Vec<_> = state
        .instances
        .iter()
        .map(|(instance_id, instance)| HelperInstanceStatus {
            instance_id: instance_id.clone(),
            place_id: instance.place_id.clone(),
            studio_pid: instance.studio_pid,
        })
        .collect();
    instances.sort_by(|left, right| left.instance_id.cmp(&right.instance_id));
    Json(HelperStatusResponse {
        helper_port: app.helper.port,
        user_name: app.helper.user_name.clone(),
        active_instances: state.instances.len(),
        active_places,
        instances,
        last_remote_errors: state.last_remote_errors.clone(),
    })
}

async fn rojo_config_handler(
    State(app): State<AppState>,
    Query(query): Query<PlaceQuery>,
) -> Result<Json<RojoConfigResponse>, HelperError> {
    let place_id = sanitize_place_id(&query.place_id)?;
    let base_url = derive_public_base_url("rojo", &place_id, &app.helper);
    let bearer_token = app.helper.bearer_token.lock().await.clone();
    tracing::info!(place_id, base_url, "resolved rojo config from helper");
    Ok(Json(RojoConfigResponse {
        place_id,
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
    let remote_base_url = derive_public_base_url("mcp", &place_id, &app.helper);
    probe_remote_mcp(&app.helper, &remote_base_url).await?;
    let studio_pid = resolve_peer_process_id_with_retry(peer_addr, app.helper.port).await?;
    {
        let mut state = app.state.lock().await;
        state.instances.insert(
            instance_id.clone(),
            PluginInstance {
                place_id: place_id.clone(),
                studio_pid,
                last_seen_at: Instant::now(),
                queue: VecDeque::new(),
                notify: Arc::new(Notify::new()),
            },
        );
        if state.active_remote_places.insert(place_id.clone()) {
            let worker_app = app.clone();
            let worker_place = place_id.clone();
            tokio::spawn(async move {
                remote_poll_loop(worker_app, worker_place).await;
            });
        }
    }
    tracing::info!(instance_id, place_id, studio_pid, remote_base_url, "registered MCP plugin instance with helper");
    Ok(Json(RegisterPluginResponse {
        instance_id,
        place_id,
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
    refresh_active_places(&mut state);
    tracing::info!(instance_id = payload.instance_id, "unregistered MCP plugin instance");
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
                    tracing::info!(instance_id = query.instance_id, "plugin polled helper with expired instance_id");
                    return Ok::<Option<Value>, color_eyre::Report>(None);
                };
                instance.last_seen_at = Instant::now();
                if let Some(command) = instance.queue.pop_front() {
                    tracing::info!(instance_id = query.instance_id, "helper delivered queued MCP command to plugin");
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
        tracing::info!(id = payload.id, success = payload.success, "helper received plugin tool response");
    } else {
        tracing::warn!(id = payload.id, "received late or unknown plugin tool response");
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
    let path = take_screenshot(app, place_id.as_deref()).await?;
    Ok(Json(ScreenshotResponse { path }))
}

async fn read_studio_log_debug_handler(
    Query(query): Query<ReadStudioLogArgs>,
) -> Result<Json<ReadStudioLogResponse>, HelperError> {
    Ok(Json(read_studio_log(query)?))
}

async fn remote_poll_loop(app: AppState, place_id: String) {
    let base_url = derive_public_base_url("mcp", &place_id, &app.helper);
    tracing::info!(place_id, base_url, "started helper remote poll loop");
    loop {
        {
            let mut state = app.state.lock().await;
            cleanup_stale_instances(&mut state.instances);
            refresh_active_places(&mut state);
            if !state.active_remote_places.contains(&place_id) {
                state.last_remote_errors.remove(&place_id);
                tracing::info!(place_id, "stopped helper remote poll loop because no active plugin remains");
                break;
            }
        }
        match poll_remote_command(&app.helper, &base_url).await {
            Ok(Some(command)) => {
                let response = match handle_remote_command(app.clone(), &place_id, command.clone()).await {
                    Ok(body) => PluginResponsePayload {
                        instance_id: None,
                        id: extract_command_id(&command).unwrap_or_else(|_| "unknown".to_owned()),
                        success: true,
                        response: body,
                    },
                    Err(error) => PluginResponsePayload {
                        instance_id: None,
                        id: extract_command_id(&command).unwrap_or_else(|_| "unknown".to_owned()),
                        success: false,
                        response: error.to_string(),
                    },
                };
                if let Err(error) = post_remote_response(&app.helper, &base_url, &response).await {
                    tracing::error!(place_id, error = summarize_error(&error.to_string()), "failed to post helper response to MCP server");
                    let mut state = app.state.lock().await;
                    refresh_active_places(&mut state);
                    if state.active_remote_places.contains(&place_id) {
                        state
                            .last_remote_errors
                            .insert(place_id.clone(), summarize_error(&error.to_string()));
                    }
                }
            }
            Ok(None) => {}
            Err(error) => {
                tracing::warn!(place_id, error = summarize_error(&error.to_string()), "helper MCP remote poll failed");
                let mut state = app.state.lock().await;
                refresh_active_places(&mut state);
                if state.active_remote_places.contains(&place_id) {
                    state
                        .last_remote_errors
                        .insert(place_id.clone(), summarize_error(&error.to_string()));
                }
                drop(state);
                tokio::time::sleep(Duration::from_secs(3)).await;
            }
        }
    }
}

async fn poll_remote_command(helper: &HelperConfig, base_url: &str) -> Result<Option<Value>> {
    let bearer_token = helper.bearer_token.lock().await.clone();
    let response = helper
        .client
        .get(format!("{base_url}/request"))
        .header(AUTHORIZATION, format!("Bearer {bearer_token}"))
        .send()
        .await?;
    match response.status() {
        StatusCode::OK => Ok(Some(response.json::<Value>().await?)),
        StatusCode::LOCKED => Ok(None),
        status => Err(eyre!("remote request poll returned {status}")),
    }
}

async fn probe_remote_mcp(helper: &HelperConfig, base_url: &str) -> Result<()> {
    let current_token = helper.bearer_token.lock().await.clone();
    let status = helper
        .client
        .get(format!("{base_url}/status"))
        .header(AUTHORIZATION, format!("Bearer {current_token}"))
        .send()
        .await?
        .status();
    if status.is_success() {
        return Ok(());
    }
    if status != StatusCode::UNAUTHORIZED {
        return Err(eyre!("remote MCP status probe returned {status}"));
    }

    for candidate in helper.bearer_token_candidates.iter() {
        if candidate.value == current_token {
            continue;
        }
        tracing::info!(source = candidate.source, "helper is trying alternate bearer token after unauthorized MCP probe");
        let status = helper
            .client
            .get(format!("{base_url}/status"))
            .header(AUTHORIZATION, format!("Bearer {}", candidate.value))
            .send()
            .await?
            .status();
        if status.is_success() {
            *helper.bearer_token.lock().await = candidate.value.clone();
            *helper.bearer_token_source.lock().await = candidate.source.clone();
            tracing::warn!(source = candidate.source, "helper switched to alternate bearer token after unauthorized MCP probe");
            return Ok(());
        }
    }

    Err(eyre!("remote MCP status probe returned {status}"))
}

async fn post_remote_response(
    helper: &HelperConfig,
    base_url: &str,
    response: &PluginResponsePayload,
) -> Result<()> {
    let bearer_token = helper.bearer_token.lock().await.clone();
    let status = helper
        .client
        .post(format!("{base_url}/response"))
        .header(AUTHORIZATION, format!("Bearer {bearer_token}"))
        .header(CONTENT_TYPE, "application/json")
        .json(response)
        .send()
        .await?
        .status();
    if !status.is_success() {
        return Err(eyre!("remote response post returned {status}"));
    }
    Ok(())
}

async fn handle_remote_command(app: AppState, place_id: &str, command: Value) -> Result<String> {
    let (tool_name, payload) = extract_tool_name_and_args(&command)?;
    tracing::info!(place_id, tool = tool_name, "helper received remote MCP command");
    match tool_name.as_str() {
        "TakeScreenshot" => take_screenshot(app, Some(place_id)).await,
        "ReadStudioLog" => {
            let args: ReadStudioLogArgs = serde_json::from_value(payload)?;
            Ok(serde_json::to_string(&read_studio_log(args)?)?)
        }
        _ => forward_to_plugin(app, place_id, command).await,
    }
}

async fn forward_to_plugin(app: AppState, place_id: &str, command: Value) -> Result<String> {
    let request_id = extract_command_id(&command)?;
    let (instance_id, notify) = {
        let mut state = app.state.lock().await;
        cleanup_stale_instances(&mut state.instances);
        refresh_active_places(&mut state);
        let selected_instance_id = select_instance_for_place(place_id, &state.instances)
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
    tracing::info!(instance_id, place_id, id = request_id, "forwarded MCP command from helper to local plugin");
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
            tracing::warn!(instance_id, place_id = instance.place_id, "dropping stale helper plugin instance");
        }
        keep
    });
}

fn refresh_active_places(state: &mut HelperState) {
    state.active_remote_places = state
        .instances
        .values()
        .map(|instance| instance.place_id.clone())
        .collect();
}

fn select_instance_for_place(place_id: &str, instances: &HashMap<String, PluginInstance>) -> Option<String> {
    instances
        .iter()
        .filter(|(_, instance)| instance.place_id == place_id)
        .max_by_key(|(_, instance)| instance.last_seen_at)
        .map(|(instance_id, _)| instance_id.clone())
}

#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
async fn resolve_capture_pid(app: &AppState, place_id: Option<&str>) -> Result<(Option<String>, u32)> {
    let state = app.state.lock().await;
    let selected = if let Some(place_id) = place_id {
        let instance_id = select_instance_for_place(place_id, &state.instances)
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
        return Err(eyre!("multiple Studio instances are active; pass placeId explicitly"));
    };

    let studio_pid = selected
        .1
        .ok_or_else(|| eyre!("helper could not resolve a Studio process id for the selected plugin instance"))?;
    Ok((selected.0, studio_pid))
}

async fn take_screenshot(app: AppState, place_id: Option<&str>) -> Result<String> {
    #[cfg(target_os = "windows")]
    {
        let (resolved_place_id, studio_pid) = resolve_capture_pid(&app, place_id).await?;
        let (studio_hwnd, viewport_hwnd, window_title) =
            find_studio_capture_target_for_pid(studio_pid)?;
        let output_dir = helper_data_dir()?.join("screenshots");
        fs::create_dir_all(&output_dir)?;
        let file_name = if let Some(place_id) = resolved_place_id.as_deref() {
            format!("studio-{place_id}-{}.png", now_stamp())
        } else {
            format!("studio-{}.png", now_stamp())
        };
        let output_path = output_dir.join(file_name);
        let escaped_path = output_path.display().to_string().replace('\'', "''");
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
$bitmap.Save('__OUTPUT_PATH__', [System.Drawing.Imaging.ImageFormat]::Png)
$result.graphics.Dispose()
$result.bitmap.Dispose()
$graphics.Dispose()
$bitmap.Dispose()
"#;
        let script = script_template
            .replace("__STUDIO_HWND__", &(studio_hwnd as usize).to_string())
            .replace("__VIEWPORT_HWND__", &(viewport_hwnd as usize).to_string())
            .replace("__OUTPUT_PATH__", &escaped_path);
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
        tracing::info!(path = %output_path.display(), studio_pid, window_title, "captured Studio screenshot from helper");
        return Ok(output_path.display().to_string());
    }

    #[cfg(not(target_os = "windows"))]
    {
        let _ = app;
        let _ = place_id;
        Err(eyre!("take_screenshot is only available on Windows helper builds"))
    }
}

fn read_studio_log(args: ReadStudioLogArgs) -> Result<ReadStudioLogResponse> {
    #[cfg(target_os = "windows")]
    {
        let local_app_data = env::var_os("LOCALAPPDATA")
            .map(PathBuf::from)
            .ok_or_else(|| eyre!("LOCALAPPDATA is not set"))?;
        let logs_dir = local_app_data.join("Roblox").join("logs");
        let log_path = find_latest_log_file(&logs_dir)?;
        let content = fs::read_to_string(&log_path)
            .wrap_err_with(|| format!("failed to read {}", log_path.display()))?;
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
        Err(eyre!("read_studio_log is only available on Windows helper builds"))
    }
}

#[cfg(target_os = "windows")]
fn find_latest_log_file(logs_dir: &Path) -> Result<PathBuf> {
    let mut files: Vec<_> = fs::read_dir(logs_dir)
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
    files.sort_by_key(|(_, modified)| *modified);
    files
        .pop()
        .map(|(path, _)| path)
        .ok_or_else(|| eyre!("no Studio log file found in {}", logs_dir.display()))
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
        tracing::warn!(error = summarize_error(&self.0.to_string()), "helper request failed");
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
    let helper = HelperConfig {
        port: args.port,
        user_name: resolve_user_name(&args)?,
        bearer_token: Arc::new(Mutex::new(initial_bearer_token)),
        bearer_token_source: Arc::new(Mutex::new(initial_bearer_token_source)),
        bearer_token_candidates: Arc::new(bearer_token_candidates),
        domain_suffix: args.domain_suffix.clone(),
        client: reqwest::Client::builder().timeout(Duration::from_secs(20)).build()?,
    };
    let token_source = helper.bearer_token_source.lock().await.clone();
    tracing::info!(user_name = helper.user_name, port = args.port, domain_suffix = helper.domain_suffix, token_source, "starting Studio helper");

    let state = Arc::new(Mutex::new(HelperState {
        instances: HashMap::new(),
        waiting_for_plugin: HashMap::new(),
        active_remote_places: HashSet::new(),
        last_remote_errors: HashMap::new(),
    }));

    let app_state = AppState {
        helper,
        state,
    };

    let app = Router::new()
        .route("/status", get(helper_status))
        .route("/v1/rojo/config", get(rojo_config_handler))
        .route("/v1/mcp/register", post(mcp_register_handler))
        .route("/v1/mcp/unregister", post(mcp_unregister_handler))
        .route("/v1/mcp/plugin/request", get(mcp_plugin_request_handler))
        .route("/v1/mcp/plugin/response", post(mcp_plugin_response_handler))
        .route("/v1/helper/screenshot", post(screenshot_debug_handler))
        .route("/v1/helper/studio-log", get(read_studio_log_debug_handler))
        .with_state(app_state);

    let listener = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, args.port)).await?;
    tracing::info!(port = args.port, "Studio helper listening on 127.0.0.1");
    axum::serve(listener, app.into_make_service_with_connect_info::<SocketAddr>()).await?;
    Ok(())
}
