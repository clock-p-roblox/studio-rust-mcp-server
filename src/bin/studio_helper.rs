use axum::extract::{Query, State};
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
use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{Mutex, Notify, oneshot};
use tracing_subscriber::EnvFilter;
use uuid::Uuid;

#[cfg(target_os = "windows")]
use regex::Regex;
#[cfg(target_os = "windows")]
use std::time::{SystemTime, UNIX_EPOCH};

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
    bearer_token: String,
    domain_suffix: String,
    client: reqwest::Client,
}

struct PluginInstance {
    place_id: String,
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
    last_remote_errors: HashMap<String, String>,
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
    if let Some(value) = args.bearer_token.as_ref() {
        if let Some(normalized) = normalize_bearer_token(value) {
            return Ok(normalized);
        }
    }

    if let Some(path) = args.bearer_token_file.as_ref() {
        if let Some(normalized) = normalize_bearer_token(&read_trimmed_file(path)?) {
            return Ok(normalized);
        }
    }

    for candidate in token_candidates() {
        if candidate.is_file() {
            if let Some(normalized) = normalize_bearer_token(&read_trimmed_file(&candidate)?) {
                return Ok(normalized);
            }
        }
    }

    Err(eyre!("cannot resolve feishu-token for helper"))
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

async fn helper_status(State(app): State<AppState>) -> Json<HelperStatusResponse> {
    let mut state = app.state.lock().await;
    cleanup_stale_instances(&mut state.instances);
    refresh_active_places(&mut state);
    let mut active_places: Vec<_> = state.active_remote_places.iter().cloned().collect();
    active_places.sort();
    Json(HelperStatusResponse {
        helper_port: app.helper.port,
        user_name: app.helper.user_name.clone(),
        active_instances: state.instances.len(),
        active_places,
        last_remote_errors: state.last_remote_errors.clone(),
    })
}

async fn rojo_config_handler(
    State(app): State<AppState>,
    Query(query): Query<PlaceQuery>,
) -> Result<Json<RojoConfigResponse>, HelperError> {
    let place_id = sanitize_place_id(&query.place_id)?;
    let base_url = derive_public_base_url("rojo", &place_id, &app.helper);
    tracing::info!(place_id, base_url, "resolved rojo config from helper");
    Ok(Json(RojoConfigResponse {
        place_id,
        base_url,
        auth_header: format!("Bearer {}", app.helper.bearer_token),
    }))
}

async fn mcp_register_handler(
    State(app): State<AppState>,
    Json(payload): Json<RegisterPluginRequest>,
) -> Result<Json<RegisterPluginResponse>, HelperError> {
    let place_id = sanitize_place_id(&payload.place_id)?;
    let instance_id = Uuid::new_v4().to_string();
    let remote_base_url = derive_public_base_url("mcp", &place_id, &app.helper);
    probe_remote_mcp(&app.helper, &remote_base_url).await?;
    {
        let mut state = app.state.lock().await;
        state.instances.insert(
            instance_id.clone(),
            PluginInstance {
                place_id: place_id.clone(),
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
    tracing::info!(instance_id, place_id, remote_base_url, "registered MCP plugin instance with helper");
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
    Query(query): Query<ScreenshotQuery>,
) -> Result<Json<ScreenshotResponse>, HelperError> {
    let place_id = match query.place_id.as_deref() {
        Some(value) => Some(sanitize_place_id(value)?),
        None => None,
    };
    let path = take_screenshot(place_id.as_deref()).await?;
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
    let response = helper
        .client
        .get(format!("{base_url}/request"))
        .header(AUTHORIZATION, format!("Bearer {}", helper.bearer_token))
        .send()
        .await?;
    match response.status() {
        StatusCode::OK => Ok(Some(response.json::<Value>().await?)),
        StatusCode::LOCKED => Ok(None),
        status => Err(eyre!("remote request poll returned {status}")),
    }
}

async fn probe_remote_mcp(helper: &HelperConfig, base_url: &str) -> Result<()> {
    let response = helper
        .client
        .get(format!("{base_url}/status"))
        .header(AUTHORIZATION, format!("Bearer {}", helper.bearer_token))
        .send()
        .await?;
    let status = response.status();
    if !status.is_success() {
        return Err(eyre!("remote MCP status probe returned {status}"));
    }
    Ok(())
}

async fn post_remote_response(
    helper: &HelperConfig,
    base_url: &str,
    response: &PluginResponsePayload,
) -> Result<()> {
    let status = helper
        .client
        .post(format!("{base_url}/response"))
        .header(AUTHORIZATION, format!("Bearer {}", helper.bearer_token))
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
        "TakeScreenshot" => take_screenshot(Some(place_id)).await,
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

async fn take_screenshot(place_id: Option<&str>) -> Result<String> {
    #[cfg(target_os = "windows")]
    {
        let output_dir = helper_data_dir()?.join("screenshots");
        fs::create_dir_all(&output_dir)?;
        let file_name = if let Some(place_id) = place_id {
            format!("studio-{place_id}-{}.png", now_stamp())
        } else {
            format!("studio-{}.png", now_stamp())
        };
        let output_path = output_dir.join(file_name);
        let escaped_path = output_path.display().to_string().replace('\'', "''");
        let script = format!(
            concat!(
                "Add-Type -AssemblyName System.Drawing;",
                "Add-Type @\"using System; using System.Runtime.InteropServices; public static class Win32 {{",
                "[DllImport(\"user32.dll\")] public static extern IntPtr GetForegroundWindow();",
                "[DllImport(\"user32.dll\")] public static extern bool GetWindowRect(IntPtr hWnd, out RECT rect);",
                "public struct RECT {{ public int Left; public int Top; public int Right; public int Bottom; }} }}\"@;",
                "$hwnd=[Win32]::GetForegroundWindow();",
                "if ($hwnd -eq [IntPtr]::Zero) {{ throw 'No foreground window available'; }};",
                "$rect=New-Object Win32+RECT; [Win32]::GetWindowRect($hwnd, [ref]$rect) | Out-Null;",
                "$width=$rect.Right-$rect.Left; $height=$rect.Bottom-$rect.Top;",
                "if ($width -le 0 -or $height -le 0) {{ throw 'Foreground window has invalid bounds'; }};",
                "$bitmap=New-Object System.Drawing.Bitmap $width, $height;",
                "$graphics=[System.Drawing.Graphics]::FromImage($bitmap);",
                "$graphics.CopyFromScreen($rect.Left, $rect.Top, 0, 0, $bitmap.Size);",
                "$bitmap.Save('{}', [System.Drawing.Imaging.ImageFormat]::Png);",
                "$graphics.Dispose(); $bitmap.Dispose();"
            ),
            escaped_path,
        );
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
        tracing::info!(path = %output_path.display(), "captured Studio screenshot from helper");
        return Ok(output_path.display().to_string());
    }

    #[cfg(not(target_os = "windows"))]
    {
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
    let helper = HelperConfig {
        port: args.port,
        user_name: resolve_user_name(&args)?,
        bearer_token: resolve_bearer_token(&args)?,
        domain_suffix: args.domain_suffix.clone(),
        client: reqwest::Client::builder().timeout(Duration::from_secs(20)).build()?,
    };
    tracing::info!(user_name = helper.user_name, port = args.port, domain_suffix = helper.domain_suffix, "starting Studio helper");

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
    axum::serve(listener, app).await?;
    Ok(())
}
