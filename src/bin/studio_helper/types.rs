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
    #[serde(default)]
    studio_session_state: Option<String>,
    #[serde(default)]
    studio_mode: Option<String>,
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
struct PluginStudioSessionStatePayload {
    studio_session_state: String,
    #[serde(default)]
    instance_id: Option<String>,
    #[serde(default)]
    task_id: Option<String>,
}

#[derive(Debug, Clone)]
struct LiveStudioSessionState {
    instance_id: Option<String>,
    state: String,
    reason: Option<String>,
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
