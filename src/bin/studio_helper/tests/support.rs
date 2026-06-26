
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

    fn test_edit_heartbeat_request(instance_id: &str) -> PluginEditHeartbeatRequest {
        PluginEditHeartbeatRequest {
            instance_id: instance_id.to_owned(),
            studio_session_state: None,
            studio_mode: None,
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
