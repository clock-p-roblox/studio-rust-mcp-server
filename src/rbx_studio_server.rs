use crate::error::{Report, Result};
use crate::helper_ws::{
    ArtifactBegin, ArtifactChunk, ArtifactCommitted, ArtifactFinish, HelperToServerMessage,
    ServerToHelperMessage, MAX_ARTIFACT_CHUNK_MESSAGE_BYTES,
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
use tokio::sync::{mpsc, watch, Mutex};
use uuid::Uuid;

pub const STUDIO_PLUGIN_PORT: u16 = 44755;
const LONG_POLL_DURATION: Duration = Duration::from_secs(15);
const HELPER_REQUEST_TIMEOUT: Duration = Duration::from_secs(60);
const HELPER_HEALTH_CHECK_INTERVAL: Duration = Duration::from_secs(5);
const HELPER_HEARTBEAT_TIMEOUT: Duration = Duration::from_secs(20);

type UploadHandle = Arc<StdMutex<ArtifactUploadState>>;
type UploadRegistry = Arc<StdMutex<HashMap<Uuid, UploadHandle>>>;

fn summarize_text(value: &str) -> String {
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
    args: ToolArgumentValues,
    id: Option<Uuid>,
}

#[derive(Deserialize, Serialize, Clone, Debug)]
pub struct RunCommandResponse {
    success: bool,
    response: String,
    id: Uuid,
}

pub struct AppState {
    workspace: PathBuf,
    process_queue: VecDeque<ToolArguments>,
    output_map: HashMap<Uuid, mpsc::UnboundedSender<Result<String>>>,
    active_helper: Option<ActiveHelperConnection>,
    uploads: UploadRegistry,
    waiter: watch::Receiver<()>,
    trigger: watch::Sender<()>,
}
pub type PackedState = Arc<Mutex<AppState>>;

#[derive(Serialize)]
pub struct StatusResponse {
    service: &'static str,
    queued_requests: usize,
    pending_responses: usize,
    helper_connected: bool,
    helper_place_id: Option<String>,
}

struct ActiveHelperConnection {
    connection_id: Uuid,
    place_id: String,
    sender: mpsc::UnboundedSender<OutgoingHelperFrame>,
    last_message_at: Instant,
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

impl AppState {
    pub fn new(workspace: PathBuf) -> Self {
        let (trigger, waiter) = watch::channel(());
        Self {
            workspace,
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
    Json(StatusResponse {
        service: "rbx-studio-mcp",
        queued_requests: state.process_queue.len(),
        pending_responses: state.output_map.len(),
        helper_connected,
        helper_place_id: if helper_connected {
            state
                .active_helper
                .as_ref()
                .map(|helper| helper.place_id.clone())
        } else {
            None
        },
    })
}

impl ToolArguments {
    fn new(args: ToolArgumentValues) -> (Self, Uuid) {
        Self { args, id: None }.with_id()
    }
    fn with_id(self) -> (Self, Uuid) {
        let id = Uuid::new_v4();
        (
            Self {
                args: self.args,
                id: Some(id),
            },
            id,
        )
    }

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
                name: "Roblox_Studio".to_string(),
                version: env!("CARGO_PKG_VERSION").to_string(),
                title: Some("Roblox Studio MCP Server".to_string()),
                icons: None,
                website_url: None,
            },
            instructions: Some(
                "You must aware of current studio mode before using any tools, infer the mode from conversation context or get_studio_mode.
User run_code to query data from Roblox Studio place or to change it
After calling run_script_in_play_mode, the datamodel status will be reset to stop mode.
Prefer using start_stop_play tool instead run_script_in_play_mode, Only used run_script_in_play_mode to run one time unit test code on server datamodel.
"
                    .to_string(),
            ),
        }
    }
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema, Clone)]
struct RunCode {
    #[schemars(description = "Code to run")]
    command: String,
}
#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema, Clone)]
struct InsertModel {
    #[schemars(description = "Query to search for the model")]
    query: String,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema, Clone)]
struct GetConsoleOutput {}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema, Clone)]
struct GetStudioMode {}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema, Clone)]
struct TakeScreenshot {
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
        description = "Optional starting line (1-indexed). Negative values count from the end."
    )]
    start_line: Option<i64>,
    #[schemars(description = "Optional number of lines to return.")]
    line_count: Option<u32>,
    #[schemars(description = "Optional regex used to filter matching lines.")]
    regex: Option<String>,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema, Clone)]
struct StartStopPlay {
    #[schemars(
        description = "Mode to start or stop, must be start_play, stop, or run_server. Don't use run_server unless you are sure no client/player is needed."
    )]
    mode: String,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema, Clone)]
struct RunScriptInPlayMode {
    #[schemars(description = "Code to run")]
    code: String,
    #[schemars(description = "Timeout in seconds, defaults to 100 seconds")]
    timeout: Option<u32>,
    #[schemars(description = "Mode to run in, must be start_play or run_server")]
    mode: String,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema, Clone)]
enum ToolArgumentValues {
    RunCode(RunCode),
    InsertModel(InsertModel),
    GetConsoleOutput(GetConsoleOutput),
    StartStopPlay(StartStopPlay),
    RunScriptInPlayMode(RunScriptInPlayMode),
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
            ToolArgumentValues::RunScriptInPlayMode(_) => "run_script_in_play_mode",
            ToolArgumentValues::GetStudioMode(_) => "get_studio_mode",
            ToolArgumentValues::TakeScreenshot(_) => "take_screenshot",
            ToolArgumentValues::ReadStudioLog(_) => "read_studio_log",
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
        description = "Start or stop play mode or run the server, Don't enter run_server mode unless you are sure no client/player is needed."
    )]
    async fn start_stop_play(
        &self,
        Parameters(args): Parameters<StartStopPlay>,
    ) -> Result<CallToolResult, ErrorData> {
        self.generic_tool_run(ToolArgumentValues::StartStopPlay(args))
            .await
    }

    #[tool(
        description = "Run a script in play mode and automatically stop play after script finishes or timeout. Returns the output of the script.
        Result format: { success: boolean, value: string, error: string, logs: { level: string, message: string, ts: number }[], errors: { level: string, message: string, ts: number }[], duration: number, isTimeout: boolean }.
        - Prefer using start_stop_play tool instead run_script_in_play_mode, Only used run_script_in_play_mode to run one time unit test code on server datamodel.
        - After calling run_script_in_play_mode, the datamodel status will be reset to stop mode.
        - If It returns `StudioTestService: Previous call to start play session has not been completed`, call start_stop_play tool to stop play mode first then try it again."
    )]
    async fn run_script_in_play_mode(
        &self,
        Parameters(args): Parameters<RunScriptInPlayMode>,
    ) -> Result<CallToolResult, ErrorData> {
        self.generic_tool_run(ToolArgumentValues::RunScriptInPlayMode(args))
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

    async fn generic_tool_run(
        &self,
        args: ToolArgumentValues,
    ) -> Result<CallToolResult, ErrorData> {
        let normalized_args = {
            let workspace = { self.state.lock().await.workspace.clone() };
            normalize_tool_arguments_for_workspace(&workspace, args).map_err(|error| {
                ErrorData::internal_error(
                    format!("Unable to normalize tool arguments: {error}"),
                    None,
                )
            })?
        };
        let (command, id) = ToolArguments::new(normalized_args);
        let tool_name = command.tool_name();
        let (tx, mut rx) = mpsc::unbounded_channel::<Result<String>>();
        let (trigger, uploads, queued_requests, pending_responses) = {
            let mut state = self.state.lock().await;
            if state.active_helper.is_none() {
                return Err(ErrorData::internal_error(
                    "No active Studio helper WebSocket connection",
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
            "queued tool request"
        );
        trigger
            .send(())
            .map_err(|e| ErrorData::internal_error(format!("Unable to trigger send {e}"), None))?;
        let result = rx.recv();
        let result = match tokio::time::timeout(HELPER_REQUEST_TIMEOUT, result).await {
            Ok(Some(result)) => result,
            Ok(None) => return Err(ErrorData::internal_error("Couldn't receive response", None)),
            Err(_) => {
                let mut state = self.state.lock().await;
                remove_request_tracking(&mut state, id);
                abort_uploads_for_request(&uploads, id);
                return Err(ErrorData::internal_error(
                    format!("Timed out waiting for {tool_name} response from Studio helper"),
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
            helper.last_message_at = Instant::now();
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
        ensure_session_metadata(workspace, &session_id, &place_id)?;
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

    tracing::info!(connection_id = %connection_id, place_id = hello.place_id, capabilities = ?hello.capabilities, "helper websocket connected");
    let uploads = { Arc::clone(&state.lock().await.uploads) };
    let workspace = { state.lock().await.workspace.clone() };
    {
        let mut state = state.lock().await;
        if let Some(previous) = state.active_helper.replace(ActiveHelperConnection {
            connection_id,
            place_id: hello.place_id.clone(),
            sender: out_tx.clone(),
            last_message_at: Instant::now(),
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
                        place_id,
                        plugin_instance_count,
                    }) => {
                        tracing::debug!(connection_id = %connection_id, place_id, plugin_instance_count, "received helper heartbeat");
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
            tracing::warn!(
                %connection_id,
                place_id,
                timeout_secs = HELPER_HEARTBEAT_TIMEOUT.as_secs(),
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
            Ok((StatusCode::LOCKED, String::new()).into_response())
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
    tracing::info!(%id, tool = command.tool_name(), "proxy received tool request");
    let (tx, mut rx) = mpsc::unbounded_channel();
    let (trigger, uploads) = {
        let mut state = state.lock().await;
        state.process_queue.push_back(command);
        state.output_map.insert(id, tx);
        (state.trigger.clone(), Arc::clone(&state.uploads))
    };
    trigger.send(()).ok();
    let result = match tokio::time::timeout(HELPER_REQUEST_TIMEOUT, rx.recv()).await {
        Ok(Some(result)) => result,
        Ok(None) => return Err(eyre!("Couldn't receive response").into()),
        Err(_) => {
            let mut state = state.lock().await;
            remove_request_tracking(&mut state, id);
            abort_uploads_for_request(&uploads, id);
            tracing::warn!(%id, "proxy timed out waiting for helper response");
            return Ok(Json(RunCommandResponse {
                success: false,
                response: "Timed out waiting for Studio helper response".to_owned(),
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
                let res = res
                    .json::<RunCommandResponse>()
                    .await
                    .map(|r| r.response)
                    .map_err(Report::from);
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
