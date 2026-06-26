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
