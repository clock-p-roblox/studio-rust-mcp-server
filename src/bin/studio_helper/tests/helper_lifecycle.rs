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

    async fn respond_live_state(app: &AppState, instance_id: &str, studio_session_state: &str) {
        let (request_id, command) =
            take_queued_plugin_command(app, instance_id, "GetStudioSessionState").await;
        assert!(command["args"]
            .as_object()
            .and_then(|args| args.get("GetStudioSessionState"))
            .is_some());
        send_plugin_response(
            app,
            instance_id,
            request_id,
            true,
            serde_json::json!({
                "instance_id": instance_id,
                "task_id": "task-a",
                "studio_session_state": studio_session_state
            })
            .to_string(),
        )
        .await;
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

        respond_live_state(&app, "instance-a", "play").await;

        let error = handle
            .await
            .expect("remote command task should join")
            .expect_err("helper must reject relaunching an existing play session");

        assert!(error.to_string().contains("studio_already_playing"));
        let state = app.state.lock().await;
        let instance = state.instances.get("instance-a").unwrap();
        assert_eq!(instance.stop_request_id, 0);
        assert_eq!(edit_runtime_state(instance), "ready");
        assert_eq!(instance.studio_transition_phase, "running");
        assert!(instance.queue.is_empty());
        assert!(state.waiting_for_plugin.is_empty());
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

        respond_live_state(&app, "instance-a", "stop").await;
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
            Json(test_edit_heartbeat_request("instance-a")),
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
            Json(test_edit_heartbeat_request("instance-a")),
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

