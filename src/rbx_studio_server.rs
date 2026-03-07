use crate::error::{Report, Result};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::{extract::State, Json};
use color_eyre::eyre::{eyre, Error, OptionExt};
use rmcp::{
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{
        CallToolResult, Content, Implementation, ProtocolVersion, ServerCapabilities, ServerInfo,
    },
    schemars, tool, tool_handler, tool_router, ErrorData, ServerHandler,
};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use tokio::sync::oneshot::Receiver;
use tokio::sync::{mpsc, watch, Mutex};
use tokio::time::Duration;
use uuid::Uuid;

pub const STUDIO_PLUGIN_PORT: u16 = 44755;
const LONG_POLL_DURATION: Duration = Duration::from_secs(15);

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
    process_queue: VecDeque<ToolArguments>,
    output_map: HashMap<Uuid, mpsc::UnboundedSender<Result<String>>>,
    waiter: watch::Receiver<()>,
    trigger: watch::Sender<()>,
}
pub type PackedState = Arc<Mutex<AppState>>;

#[derive(Serialize)]
pub struct StatusResponse {
    service: &'static str,
    queued_requests: usize,
    pending_responses: usize,
}

impl AppState {
    pub fn new() -> Self {
        let (trigger, waiter) = watch::channel(());
        Self {
            process_queue: VecDeque::new(),
            output_map: HashMap::new(),
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
    Json(StatusResponse {
        service: "rbx-studio-mcp",
        queued_requests: state.process_queue.len(),
        pending_responses: state.output_map.len(),
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
struct TakeScreenshot {}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema, Clone)]
struct ReadStudioLog {
    #[schemars(description = "Optional starting line (1-indexed). Negative values count from the end.")]
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

    #[tool(description = "Capture a screenshot of the active Roblox Studio window through the Windows helper.")]
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
        let (command, id) = ToolArguments::new(args);
        let tool_name = command.tool_name();
        let (tx, mut rx) = mpsc::unbounded_channel::<Result<String>>();
        let (trigger, queued_requests, pending_responses) = {
            let mut state = self.state.lock().await;
            state.process_queue.push_back(command);
            state.output_map.insert(id, tx);
            (
                state.trigger.clone(),
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
        let result = rx
            .recv()
            .await
            .ok_or(ErrorData::internal_error("Couldn't receive response", None))?;
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
    {
        let mut state = state.lock().await;
        state.process_queue.push_back(command);
        state.output_map.insert(id, tx);
    }
    let result = rx.recv().await.ok_or_eyre("Couldn't receive response")?;
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
