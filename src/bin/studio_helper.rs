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
include!("studio_helper/types.rs");
include!("studio_helper/routing_lifecycle.rs");
include!("studio_helper/hub_client.rs");
include!("studio_helper/runtime_screenshot_remote.rs");
include!("studio_helper/plugin_command_helpers.rs");
include!("studio_helper/windows_capture.rs");
include!("studio_helper/status_http.rs");
include!("studio_helper/rojo_forward.rs");
include!("studio_helper/plugin_endpoints.rs");
include!("studio_helper/session_state.rs");
include!("studio_helper/task_status.rs");
include!("studio_helper/remote_connections.rs");
include!("studio_helper/remote_artifacts.rs");
include!("studio_helper/official_adapter.rs");
include!("studio_helper/remote_commands.rs");
include!("studio_helper/local_capture_log.rs");
include!("studio_helper/error_main.rs");
include!("studio_helper/tests.rs");
