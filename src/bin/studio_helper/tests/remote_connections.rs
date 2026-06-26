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

