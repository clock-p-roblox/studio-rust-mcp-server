    #[test]
    fn task_studio_control_snapshot_reports_idle_edit_without_runtime_control() {
        let mut state = empty_helper_state();
        let instance = test_instance("93795519121520", Some("task-a"), 0);
        state.instances.insert("instance-a".to_owned(), instance);

        let snapshot = task_studio_control_snapshot(&state, "task-a");

        assert_eq!(snapshot.studio_control_state, "none");
        assert_eq!(snapshot.studio_transition_phase, "idle");
        assert_eq!(snapshot.edit_runtime_state, "ready");
    }

    #[test]
    fn task_studio_control_snapshot_reports_idle_edit_without_reported_mode() {
        let mut state = empty_helper_state();
        let mut instance = test_instance("93795519121520", Some("task-a"), 0);
        instance.studio_mode = None;
        instance.studio_mode_observed_at = None;
        instance.studio_mode_source = "none".to_owned();
        state.instances.insert("instance-a".to_owned(), instance);

        let snapshot = task_studio_control_snapshot(&state, "task-a");

        assert_eq!(snapshot.studio_control_state, "none");
        assert_eq!(snapshot.studio_transition_phase, "idle");
        assert_eq!(snapshot.edit_runtime_state, "ready");
    }

    #[test]
    fn task_studio_control_snapshot_reports_none_connected_without_instances() {
        let state = empty_helper_state();
        let snapshot = task_studio_control_snapshot(&state, "task-a");

        assert_eq!(snapshot.studio_control_state, "none");
        assert_eq!(snapshot.studio_transition_phase, "idle");
        assert_eq!(snapshot.edit_runtime_state, "missing");
    }

    #[test]
    fn task_studio_control_snapshot_reports_stop_for_ready_edit_runtime() {
        let mut state = empty_helper_state();
        let mut edit_instance = test_instance("93795519121520", Some("task-a"), 0);
        edit_instance.studio_mode = None;
        edit_instance.studio_mode_source = "none".to_owned();
        edit_instance.edit_runtime_observed_at = Some(Instant::now());
        state
            .instances
            .insert("edit-instance".to_owned(), edit_instance);

        let snapshot = task_studio_control_snapshot(&state, "task-a");

        assert_eq!(snapshot.studio_control_state, "none");
        assert_eq!(snapshot.studio_transition_phase, "idle");
        assert_eq!(snapshot.edit_runtime_state, "ready");
    }

    #[test]
    fn task_studio_control_snapshot_prioritizes_stopping_over_play() {
        let mut state = empty_helper_state();
        let mut instance = test_instance("93795519121520", Some("task-a"), 0);
        instance.studio_mode = Some("start_play".to_owned());
        instance.studio_mode_source = "play_control".to_owned();
        instance.studio_control_state = "stopping".to_owned();
        instance.studio_transition_phase = "stopping_requested".to_owned();
        instance.studio_control_observed_at = Some(Instant::now());
        instance.stop_request_id = 3;
        instance.stop_request_recorded_at = Some(Instant::now());
        let mut edit_instance = test_instance("93795519121520", Some("task-a"), 0);
        edit_instance.studio_mode = Some("stop".to_owned());
        edit_instance.studio_mode_source = "edit_plugin".to_owned();
        edit_instance.edit_runtime_observed_at = Some(Instant::now());
        state.instances.insert("instance-a".to_owned(), instance);
        state
            .instances
            .insert("edit-instance".to_owned(), edit_instance);

        let snapshot = task_studio_control_snapshot(&state, "task-a");

        assert_eq!(snapshot.studio_control_state, "stopping");
        assert_eq!(snapshot.studio_transition_phase, "stopping_requested");
        assert_eq!(snapshot.active_stop_request_id, Some(3));
    }

    #[test]
    fn active_stop_request_selection_reuses_stopping_instance_without_fresh_control() {
        let mut instances = HashMap::new();
        let mut stopping_instance = test_instance("93795519121520", Some("task-a"), 0);
        stopping_instance.studio_control_state = "stopping".to_owned();
        stopping_instance.studio_transition_phase = "stopping_requested".to_owned();
        stopping_instance.studio_control_observed_at = None;
        stopping_instance.stop_request_id = 7;
        instances.insert("stopping-instance".to_owned(), stopping_instance);

        let selected =
            select_active_stop_request_for_route("93795519121520", Some("task-a"), &instances);

        assert_eq!(selected, Some(("stopping-instance".to_owned(), 7)));
        assert!(
            select_runtime_actuator_for_route("93795519121520", Some("task-a"), &instances)
                .is_none()
        );
    }

    #[test]
    fn known_runtime_session_selection_blocks_unsafe_edit_stop_fallback() {
        let mut instances = HashMap::new();
        let mut runtime_instance = test_instance("93795519121520", Some("task-a"), 0);
        runtime_instance.studio_control_state = "ready".to_owned();
        runtime_instance.studio_transition_phase = "running".to_owned();
        runtime_instance.studio_control_observed_at =
            Some(Instant::now() - STUDIO_CONTROL_HEARTBEAT_STALE_AFTER - Duration::from_millis(1));
        set_edit_runtime_unavailable(&mut runtime_instance, "runtime");
        instances.insert("runtime-instance".to_owned(), runtime_instance);

        assert!(
            select_runtime_actuator_for_route("93795519121520", Some("task-a"), &instances)
                .is_none()
        );
        assert_eq!(
            select_known_runtime_session_for_route("93795519121520", Some("task-a"), &instances),
            Some("runtime-instance".to_owned())
        );
    }

    #[test]
    fn launch_failed_session_selection_blocks_unsafe_edit_stop_fallback() {
        let mut instances = HashMap::new();
        let mut launch_failed_instance = test_instance("93795519121520", Some("task-a"), 0);
        launch_failed_instance.studio_control_state = "ready".to_owned();
        launch_failed_instance.studio_transition_phase = "idle".to_owned();
        launch_failed_instance.studio_control_observed_at =
            Some(Instant::now() - STUDIO_CONTROL_HEARTBEAT_STALE_AFTER - Duration::from_millis(1));
        set_edit_runtime_unavailable(&mut launch_failed_instance, "launch_failed");
        instances.insert("launch-failed-instance".to_owned(), launch_failed_instance);

        assert!(
            select_runtime_actuator_for_route("93795519121520", Some("task-a"), &instances)
                .is_none()
        );
        assert_eq!(
            select_known_runtime_session_for_route("93795519121520", Some("task-a"), &instances),
            Some("launch-failed-instance".to_owned())
        );
    }

    #[test]
    fn launching_session_selection_blocks_unsafe_edit_stop_fallback() {
        let mut instances = HashMap::new();
        let mut launching_instance = test_instance("93795519121520", Some("task-a"), 0);
        launching_instance.studio_control_state = "lost".to_owned();
        launching_instance.studio_transition_phase = "starting".to_owned();
        launching_instance.studio_control_observed_at = None;
        set_edit_runtime_unavailable(&mut launching_instance, "launching");
        instances.insert("launching-instance".to_owned(), launching_instance);

        assert!(
            select_runtime_actuator_for_route("93795519121520", Some("task-a"), &instances)
                .is_none()
        );
        assert_eq!(
            select_known_runtime_session_for_route("93795519121520", Some("task-a"), &instances),
            Some("launching-instance".to_owned())
        );
    }

    #[test]
    fn reported_play_mode_selection_blocks_unsafe_edit_stop_fallback() {
        let mut instances = HashMap::new();
        let mut play_instance = test_instance("93795519121520", Some("task-a"), 0);
        play_instance.studio_mode = Some("start_play".to_owned());
        play_instance.studio_mode_observed_at = Some(Instant::now());
        play_instance.studio_mode_source = "diagnostic".to_owned();
        play_instance.studio_control_state = "lost".to_owned();
        play_instance.studio_transition_phase = "idle".to_owned();
        play_instance.studio_control_observed_at = None;
        instances.insert("play-instance".to_owned(), play_instance);

        assert!(
            select_runtime_actuator_for_route("93795519121520", Some("task-a"), &instances)
                .is_none()
        );
        assert_eq!(
            select_known_runtime_session_for_route("93795519121520", Some("task-a"), &instances),
            Some("play-instance".to_owned())
        );
    }

    #[test]
    fn task_active_studio_pid_prefers_registered_plugin_pid() {
        let mut state = empty_helper_state();
        state.claimed_tasks.insert(
            "task-a".to_owned(),
            test_claimed_task("task-a", "93795519121520"),
        );
        state.launch_processes.insert(
            "task-a".to_owned(),
            LaunchProcessRecord {
                task_id: "task-a".to_owned(),
                place_id: "93795519121520".to_owned(),
                universe_id: Some("999".to_owned()),
                studio_pid: 21632,
                launched_by_helper: true,
                #[cfg(target_os = "windows")]
                kill_on_close_job: None,
                child: None,
            },
        );
        let mut instance = test_instance("93795519121520", Some("task-a"), 0);
        instance.studio_pid = Some(19180);
        state.instances.insert("instance-a".to_owned(), instance);

        assert_eq!(task_active_studio_pid(&state, "task-a"), Some(19180));
        let launch = state.launch_processes.get("task-a").unwrap();
        assert!(!launch_process_is_active_for_status(&state, launch));
    }

    #[test]
    fn task_studio_control_snapshot_reports_ready_while_control_heartbeat_is_fresh() {
        let mut state = empty_helper_state();
        let mut instance = test_instance("93795519121520", Some("task-a"), 0);
        instance.studio_mode = Some("start_play".to_owned());
        instance.studio_mode_observed_at = Some(Instant::now());
        instance.studio_control_state = "ready".to_owned();
        instance.studio_transition_phase = "running".to_owned();
        instance.studio_control_observed_at = Some(Instant::now());
        state.instances.insert("instance-a".to_owned(), instance);

        let snapshot = task_studio_control_snapshot(&state, "task-a");

        assert_eq!(snapshot.studio_control_state, "ready");
        assert_eq!(snapshot.studio_transition_phase, "running");
    }

    #[test]
    fn task_studio_control_snapshot_reports_lost_after_control_heartbeat_expires() {
        let mut state = empty_helper_state();
        let mut instance = test_instance("93795519121520", Some("task-a"), 0);
        instance.studio_mode = Some("start_play".to_owned());
        instance.studio_mode_observed_at = Some(Instant::now());
        instance.studio_control_state = "ready".to_owned();
        instance.studio_transition_phase = "running".to_owned();
        instance.studio_control_observed_at =
            Some(Instant::now() - STUDIO_CONTROL_HEARTBEAT_STALE_AFTER - Duration::from_millis(1));
        state.instances.insert("instance-a".to_owned(), instance);

        let snapshot = task_studio_control_snapshot(&state, "task-a");

        assert_eq!(snapshot.studio_control_state, "lost");
        assert_eq!(snapshot.studio_transition_phase, "running");
    }

    #[tokio::test]
    async fn edit_heartbeat_only_refreshes_edit_runtime_visibility() {
        let app = test_app_state();
        {
            let mut state = app.state.lock().await;
            let mut instance = test_instance("93795519121520", Some("task-a"), 0);
            instance.studio_mode = None;
            instance.studio_mode_source = "none".to_owned();
            instance.studio_control_state = "none".to_owned();
            instance.studio_transition_phase = "idle".to_owned();
            state.instances.insert("instance-a".to_owned(), instance);
        }

        let response = mcp_plugin_edit_heartbeat_handler(
            State(app.clone()),
            Json(PluginEditHeartbeatRequest {
                instance_id: "instance-a".to_owned(),
            }),
        )
        .await
        .unwrap();
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
        let state = app.state.lock().await;
        let instance = state.instances.get("instance-a").unwrap();
        assert!(instance.studio_mode.is_none());
        assert_eq!(instance.studio_mode_source, "none");
        assert_eq!(instance.studio_transition_phase, "idle");
        assert_eq!(instance.studio_control_state, "none");
        assert_eq!(edit_runtime_state(instance), "ready");
    }

    #[tokio::test]
    async fn edit_heartbeat_completes_pending_stop_without_reporting_stop_mode() {
        let app = test_app_state();
        {
            let mut state = app.state.lock().await;
            let mut instance = test_instance("93795519121520", Some("task-a"), 0);
            instance.studio_mode = None;
            instance.studio_mode_source = "none".to_owned();
            instance.studio_control_state = "stopping".to_owned();
            instance.studio_transition_phase = "stopping_observed".to_owned();
            instance.studio_transition_started_at = Some(Instant::now());
            instance.studio_control_observed_at = Some(
                Instant::now() - STUDIO_CONTROL_HEARTBEAT_STALE_AFTER - Duration::from_millis(1),
            );
            instance.stop_request_id = 4;
            instance.stop_result_phase = Some("observed".to_owned());
            instance.stop_result_observed_at = Some(Instant::now());
            state.instances.insert("instance-a".to_owned(), instance);
        }

        let response = mcp_plugin_edit_heartbeat_handler(
            State(app.clone()),
            Json(test_edit_heartbeat_request("instance-a")),
        )
        .await
        .unwrap();
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
        let state = app.state.lock().await;
        let instance = state.instances.get("instance-a").unwrap();

        assert!(instance.studio_mode.is_none());
        assert_eq!(instance.studio_mode_source, "none");
        assert_eq!(instance.studio_control_state, "none");
        assert_eq!(instance.studio_transition_phase, "idle");
        assert_eq!(instance.stop_result_phase.as_deref(), Some("completed"));
        assert!(instance.studio_control_observed_at.is_none());
        assert!(instance.edit_runtime_observed_at.is_some());

        let snapshot = task_studio_control_snapshot(&state, "task-a");
        assert_eq!(snapshot.studio_control_state, "none");
        assert_eq!(snapshot.studio_transition_phase, "idle");
        assert_eq!(snapshot.edit_runtime_state, "ready");
    }

    #[tokio::test]
    async fn edit_heartbeat_does_not_complete_stop_before_runtime_observes_it() {
        let app = test_app_state();
        {
            let mut state = app.state.lock().await;
            let mut instance = test_instance("93795519121520", Some("task-a"), 0);
            instance.studio_mode = None;
            instance.studio_mode_source = "none".to_owned();
            instance.studio_control_state = "stopping".to_owned();
            instance.studio_transition_phase = "stopping_requested".to_owned();
            instance.studio_transition_started_at = Some(Instant::now());
            instance.studio_control_observed_at = Some(Instant::now());
            instance.stop_request_id = 4;
            state.instances.insert("instance-a".to_owned(), instance);
        }

        let response = mcp_plugin_edit_heartbeat_handler(
            State(app.clone()),
            Json(test_edit_heartbeat_request("instance-a")),
        )
        .await
        .unwrap();
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
        let state = app.state.lock().await;
        let instance = state.instances.get("instance-a").unwrap();

        assert_eq!(instance.studio_control_state, "stopping");
        assert_eq!(instance.studio_transition_phase, "stopping_requested");
        assert_eq!(instance.stop_result_phase, None);
        assert_eq!(edit_runtime_state(instance), "stopping");
        assert_eq!(active_stop_request_id_for_instance(instance), Some(4));
    }

    #[tokio::test]
    async fn edit_heartbeat_does_not_complete_observed_stop_while_runtime_control_is_fresh() {
        let app = test_app_state();
        {
            let mut state = app.state.lock().await;
            let mut instance = test_instance("93795519121520", Some("task-a"), 0);
            instance.studio_mode = None;
            instance.studio_mode_source = "none".to_owned();
            instance.studio_control_state = "stopping".to_owned();
            instance.studio_transition_phase = "stopping_observed".to_owned();
            instance.studio_transition_started_at = Some(Instant::now());
            instance.studio_control_observed_at = Some(Instant::now());
            instance.stop_request_id = 4;
            instance.stop_result_phase = Some("observed".to_owned());
            instance.stop_result_observed_at = Some(Instant::now());
            state.instances.insert("instance-a".to_owned(), instance);
        }

        let response = mcp_plugin_edit_heartbeat_handler(
            State(app.clone()),
            Json(test_edit_heartbeat_request("instance-a")),
        )
        .await
        .unwrap();
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
        let state = app.state.lock().await;
        let instance = state.instances.get("instance-a").unwrap();

        assert_eq!(instance.studio_control_state, "stopping");
        assert_eq!(instance.studio_transition_phase, "stopping_observed");
        assert_eq!(instance.stop_result_phase.as_deref(), Some("observed"));
        assert_eq!(edit_runtime_state(instance), "stopping");
        assert_eq!(active_stop_request_id_for_instance(instance), Some(4));
    }

    #[test]
    fn runtime_actuator_heartbeat_does_not_clobber_edit_heartbeat() {
        let mut instance = test_instance("93795519121520", Some("task-a"), 0);
        instance.studio_mode = None;
        instance.studio_mode_source = "none".to_owned();
        instance.studio_control_state = "ready".to_owned();
        instance.studio_transition_phase = "running".to_owned();
        instance.studio_control_observed_at = Some(Instant::now());
        instance.edit_runtime_observed_at = Some(Instant::now());

        assert_eq!(instance.studio_control_state, "ready");
        assert_eq!(instance.studio_transition_phase, "running");
        assert_eq!(edit_runtime_state(&instance), "ready");
    }

    #[tokio::test]
    async fn control_heartbeat_rejects_invalid_or_stopped_modes_without_mutating_instance() {
        let app = test_app_state();
        {
            let mut state = app.state.lock().await;
            state.instances.insert(
                "instance-a".to_owned(),
                test_instance("93795519121520", Some("task-a"), 0),
            );
        }

        let invalid = mcp_plugin_control_heartbeat_handler(
            State(app.clone()),
            Json(PluginControlHeartbeatRequest {
                instance_id: "instance-a".to_owned(),
                mode: "bogus".to_owned(),
            }),
        )
        .await
        .unwrap();
        assert_eq!(invalid.status(), StatusCode::BAD_REQUEST);

        let stopped = mcp_plugin_control_heartbeat_handler(
            State(app.clone()),
            Json(PluginControlHeartbeatRequest {
                instance_id: "instance-a".to_owned(),
                mode: "stop".to_owned(),
            }),
        )
        .await
        .unwrap();
        assert_eq!(stopped.status(), StatusCode::BAD_REQUEST);

        let state = app.state.lock().await;
        let instance = state.instances.get("instance-a").unwrap();
        assert_eq!(instance.studio_mode.as_deref(), Some("stop"));
        assert_eq!(instance.studio_control_state, "none");
        assert_eq!(instance.studio_transition_phase, "idle");
        assert!(instance.studio_control_observed_at.is_none());
    }

    #[tokio::test]
    async fn control_heartbeat_keeps_edit_runtime_unavailable_during_runtime() {
        let app = test_app_state();
        {
            let mut state = app.state.lock().await;
            let mut instance = test_instance("93795519121520", Some("task-a"), 0);
            instance.studio_mode = None;
            instance.studio_mode_source = "none".to_owned();
            instance.edit_runtime_observed_at = Some(Instant::now());
            state.instances.insert("instance-a".to_owned(), instance);
        }

        let response = mcp_plugin_control_heartbeat_handler(
            State(app.clone()),
            Json(PluginControlHeartbeatRequest {
                instance_id: "instance-a".to_owned(),
                mode: "start_play".to_owned(),
            }),
        )
        .await
        .unwrap();
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
        {
            let state = app.state.lock().await;
            let instance = state.instances.get("instance-a").unwrap();
            assert_eq!(edit_runtime_state(instance), "runtime");
        }

        let heartbeat = mcp_plugin_edit_heartbeat_handler(
            State(app.clone()),
            Json(test_edit_heartbeat_request("instance-a")),
        )
        .await
        .unwrap();
        assert_eq!(heartbeat.status(), StatusCode::NO_CONTENT);
        let state = app.state.lock().await;
        let instance = state.instances.get("instance-a").unwrap();
        assert_eq!(edit_runtime_state(instance), "runtime");
    }

    #[tokio::test]
    async fn edit_heartbeat_recovers_runtime_latch_after_manual_stop() {
        let app = test_app_state();
        {
            let mut state = app.state.lock().await;
            let mut instance = test_instance("93795519121520", Some("task-a"), 0);
            instance.studio_control_state = "ready".to_owned();
            instance.studio_transition_phase = "running".to_owned();
            instance.studio_control_observed_at = Some(
                Instant::now() - STUDIO_CONTROL_HEARTBEAT_STALE_AFTER - Duration::from_millis(1),
            );
            set_edit_runtime_unavailable(&mut instance, "runtime");
            state.instances.insert("instance-a".to_owned(), instance);
        }

        let heartbeat = mcp_plugin_edit_heartbeat_handler(
            State(app.clone()),
            Json(test_edit_heartbeat_request("instance-a")),
        )
        .await
        .unwrap();
        assert_eq!(heartbeat.status(), StatusCode::NO_CONTENT);
        let state = app.state.lock().await;
        let instance = state.instances.get("instance-a").unwrap();
        assert_eq!(instance.studio_control_state, "none");
        assert_eq!(instance.studio_transition_phase, "idle");
        assert_eq!(edit_runtime_state(instance), "ready");
    }

    #[tokio::test]
    async fn edit_heartbeat_recovers_launch_failed_latch_after_manual_stop() {
        let app = test_app_state();
        {
            let mut state = app.state.lock().await;
            let mut instance = test_instance("93795519121520", Some("task-a"), 0);
            instance.studio_control_state = "ready".to_owned();
            instance.studio_transition_phase = "running".to_owned();
            instance.studio_control_observed_at = Some(
                Instant::now() - STUDIO_CONTROL_HEARTBEAT_STALE_AFTER - Duration::from_millis(1),
            );
            set_edit_runtime_unavailable(&mut instance, "launch_failed");
            state.instances.insert("instance-a".to_owned(), instance);
        }

        let heartbeat = mcp_plugin_edit_heartbeat_handler(
            State(app.clone()),
            Json(test_edit_heartbeat_request("instance-a")),
        )
        .await
        .unwrap();
        assert_eq!(heartbeat.status(), StatusCode::NO_CONTENT);
        let state = app.state.lock().await;
        let instance = state.instances.get("instance-a").unwrap();
        assert_eq!(instance.studio_control_state, "none");
        assert_eq!(instance.studio_transition_phase, "idle");
        assert_eq!(edit_runtime_state(instance), "ready");
    }

    #[tokio::test]
    async fn invalid_launch_mode_rejects_before_stop_request() {
        let app = test_app_state();
        {
            let mut state = app.state.lock().await;
            let mut instance = test_instance("93795519121520", Some("task-a"), 0);
            instance.studio_mode = Some("start_play".to_owned());
            instance.studio_mode_source = "play_control".to_owned();
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
                    "mode": "bogus"
                }
            }
        });

        let error = handle_remote_command(
            app.clone(),
            "93795519121520",
            Some("task-a"),
            command,
            sender,
            Arc::new(Mutex::new(HashMap::new())),
            "request-a".to_owned(),
        )
        .await
        .expect_err("invalid launch mode must reject before stopping Studio");

        assert!(error
            .to_string()
            .contains("must be start_play or run_server"));
        let state = app.state.lock().await;
        let instance = state.instances.get("instance-a").unwrap();
        assert_eq!(instance.stop_request_id, 0);
        assert_eq!(instance.studio_control_state, "ready");
        assert_eq!(instance.studio_transition_phase, "running");
    }

    #[tokio::test]
    async fn stop_request_records_poll_and_completion_diagnostics() {
        let app = test_app_state();
        {
            let mut state = app.state.lock().await;
            let mut instance = test_instance("93795519121520", Some("task-a"), 0);
            instance.studio_mode = None;
            instance.studio_mode_observed_at = None;
            instance.studio_mode_source = "none".to_owned();
            instance.studio_control_state = "ready".to_owned();
            instance.studio_transition_phase = "running".to_owned();
            instance.studio_control_observed_at = Some(Instant::now());
            state.instances.insert("instance-a".to_owned(), instance);
        }

        let accepted_stop = mcp_plugin_stop_request_handler(
            State(app.clone()),
            Json(PluginStopRequestPayload {
                instance_id: "instance-a".to_owned(),
            }),
        )
        .await
        .unwrap();
        assert_eq!(accepted_stop.status(), StatusCode::OK);
        {
            let state = app.state.lock().await;
            let instance = state.instances.get("instance-a").unwrap();
            assert_eq!(instance.stop_request_id, 1);
            assert_eq!(instance.studio_control_state, "stopping");
            assert_eq!(instance.studio_transition_phase, "stopping_requested");
            assert!(instance.stop_request_recorded_at.is_some());
            assert_eq!(active_stop_request_id_for_instance(instance), Some(1));
            assert!(instance.stop_result_phase.is_none());
        }

        let poll_stop = mcp_plugin_stop_request_poll_handler(
            State(app.clone()),
            Query(PluginStopRequestQuery {
                instance_id: "instance-a".to_owned(),
                after_id: Some(0),
            }),
        )
        .await
        .unwrap();
        assert_eq!(poll_stop.status(), StatusCode::OK);
        let body = axum::body::to_bytes(poll_stop.into_body(), usize::MAX)
            .await
            .unwrap();
        let payload: PluginStopRequestResponse = serde_json::from_slice(&body).unwrap();
        assert!(payload.stop_requested);
        assert_eq!(payload.stop_request_id, 1);
        {
            let state = app.state.lock().await;
            let instance = state.instances.get("instance-a").unwrap();
            assert!(instance.stop_request_last_polled_at.is_some());
            assert_eq!(instance.stop_request_last_poll_id, Some(1));
        }

        {
            let state = app.state.lock().await;
            let snapshot = task_studio_control_snapshot(&state, "task-a");
            assert_eq!(snapshot.studio_transition_phase, "stopping_requested");
            assert_eq!(snapshot.studio_control_state, "stopping");
            assert_eq!(snapshot.active_stop_request_id, Some(1));
            assert_eq!(snapshot.last_stop_request_id, 1);
            assert_eq!(snapshot.runtime_actuator_last_poll_id, Some(1));
        }

        let observed = mcp_plugin_stop_result_handler(
            State(app.clone()),
            Json(PluginStopResultPayload {
                instance_id: "instance-a".to_owned(),
                stop_request_id: 1,
                phase: "observed".to_owned(),
                error: None,
            }),
        )
        .await
        .unwrap();
        assert_eq!(observed.status(), StatusCode::NO_CONTENT);
        {
            let mut state = app.state.lock().await;
            let instance = state.instances.get_mut("instance-a").unwrap();
            instance.studio_control_observed_at = Some(
                Instant::now() - STUDIO_CONTROL_HEARTBEAT_STALE_AFTER - Duration::from_millis(1),
            );
        }

        let response = mcp_plugin_edit_heartbeat_handler(
            State(app.clone()),
            Json(test_edit_heartbeat_request("instance-a")),
        )
        .await
        .unwrap();
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
        let state = app.state.lock().await;
        let instance = state.instances.get("instance-a").unwrap();
        assert!(instance.studio_mode.is_none());
        assert_eq!(instance.studio_control_state, "none");
        assert_eq!(instance.studio_transition_phase, "idle");
        assert_eq!(instance.stop_result_phase.as_deref(), Some("completed"));
        assert!(instance.stop_result_error.is_none());
        assert!(instance.stop_result_observed_at.is_some());
    }

    #[tokio::test]
    async fn stop_request_rejects_stale_runtime_actuator_without_recording() {
        let app = test_app_state();
        {
            let mut state = app.state.lock().await;
            let mut instance = test_instance("93795519121520", Some("task-a"), 0);
            instance.studio_control_state = "ready".to_owned();
            instance.studio_transition_phase = "running".to_owned();
            instance.studio_control_observed_at = Some(
                Instant::now() - STUDIO_CONTROL_HEARTBEAT_STALE_AFTER - Duration::from_millis(1),
            );
            set_edit_runtime_unavailable(&mut instance, "runtime");
            state.instances.insert("instance-a".to_owned(), instance);
        }

        let rejected_stop = mcp_plugin_stop_request_handler(
            State(app.clone()),
            Json(PluginStopRequestPayload {
                instance_id: "instance-a".to_owned(),
            }),
        )
        .await
        .unwrap();
        assert_eq!(rejected_stop.status(), StatusCode::CONFLICT);
        let state = app.state.lock().await;
        let instance = state.instances.get("instance-a").unwrap();
        assert_eq!(instance.stop_request_id, 0);
        assert_eq!(instance.studio_transition_phase, "running");
        assert_eq!(edit_runtime_state(instance), "runtime");
    }

    #[tokio::test]
    async fn stop_live_query_stop_returns_already_stopped_even_if_edit_runtime_is_stale() {
        let app = test_app_state();
        {
            let mut state = app.state.lock().await;
            let mut instance = test_instance("93795519121520", Some("task-a"), 0);
            instance.edit_runtime_observed_at =
                Some(Instant::now() - EDIT_RUNTIME_STALE_AFTER - Duration::from_millis(1));
            instance.studio_control_state = "none".to_owned();
            instance.studio_transition_phase = "idle".to_owned();
            state.instances.insert("instance-a".to_owned(), instance);
        }
        let (sender, _receiver) = mpsc::unbounded_channel();
        let command = serde_json::json!({
            "id": "request-a",
            "args": {
                "StartStopPlay": {
                    "mode": "stop"
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

        respond_live_state(&app, "instance-a", "stop").await;

        let response = handle
            .await
            .expect("remote command task should join")
            .expect("stable stop should be an already_stopped success");
        assert!(response.contains("already_stopped"));
        let state = app.state.lock().await;
        let instance = state.instances.get("instance-a").unwrap();
        assert!(instance.queue.is_empty());
        assert_eq!(instance.stop_request_id, 0);
    }

    #[tokio::test]
    async fn stop_live_query_rejects_instance_id_mismatch_without_recording_stop() {
        let app = test_app_state();
        {
            let mut state = app.state.lock().await;
            let mut instance = test_instance("93795519121520", Some("task-a"), 0);
            instance.studio_control_state = "ready".to_owned();
            instance.studio_transition_phase = "running".to_owned();
            instance.studio_control_observed_at = Some(Instant::now());
            state.instances.insert("instance-a".to_owned(), instance);
        }
        let (sender, _receiver) = mpsc::unbounded_channel();
        let command = serde_json::json!({
            "id": "request-a",
            "args": {
                "StartStopPlay": {
                    "mode": "stop"
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

        let (request_id, _command) =
            take_queued_plugin_command(&app, "instance-a", "GetStudioSessionState").await;
        send_plugin_response(
            &app,
            "instance-a",
            request_id,
            true,
            serde_json::json!({
                "instance_id": "other-instance",
                "task_id": "task-a",
                "studio_session_state": "play"
            })
            .to_string(),
        )
        .await;

        let error = handle
            .await
            .expect("remote command task should join")
            .expect_err("mismatched live query instance must fail");
        assert!(error.to_string().contains("instance_id mismatch"));
        let state = app.state.lock().await;
        let instance = state.instances.get("instance-a").unwrap();
        assert_eq!(instance.stop_request_id, 0);
    }

    #[tokio::test]
    async fn plugin_response_rejects_wrong_instance_id_without_consuming_pending_request() {
        let app = test_app_state();
        {
            let mut state = app.state.lock().await;
            state.instances.insert(
                "instance-a".to_owned(),
                test_instance("93795519121520", Some("task-a"), 0),
            );
            state.instances.insert(
                "instance-b".to_owned(),
                test_instance("93795519121520", Some("task-a"), 0),
            );
            let (response_tx, _response_rx) = oneshot::channel();
            state.waiting_for_plugin.insert(
                "request-a".to_owned(),
                PendingPluginResponse {
                    instance_id: "instance-a".to_owned(),
                    tool_name: "GetStudioSessionState".to_owned(),
                    response_tx,
                },
            );
        }

        let rejected = mcp_plugin_response_handler(
            State(app.clone()),
            Json(PluginResponsePayload {
                instance_id: Some("instance-b".to_owned()),
                id: "request-a".to_owned(),
                success: true,
                response: "{}".to_owned(),
            }),
        )
        .await
        .unwrap();
        assert_eq!(rejected.status(), StatusCode::NO_CONTENT);
        let state = app.state.lock().await;
        assert!(state.waiting_for_plugin.contains_key("request-a"));
    }

    #[tokio::test]
    async fn stop_live_query_play_records_runtime_stop_request() {
        let app = test_app_state();
        {
            let mut state = app.state.lock().await;
            let mut instance = test_instance("93795519121520", Some("task-a"), 0);
            instance.studio_mode = Some("start_play".to_owned());
            instance.studio_mode_source = "play_control".to_owned();
            instance.studio_control_state = "ready".to_owned();
            instance.studio_transition_phase = "running".to_owned();
            instance.studio_control_observed_at = Some(Instant::now());
            set_edit_runtime_unavailable(&mut instance, "runtime");
            state.instances.insert("instance-a".to_owned(), instance);
        }
        let (sender, _receiver) = mpsc::unbounded_channel();
        let command = serde_json::json!({
            "id": "request-a",
            "args": {
                "StartStopPlay": {
                    "mode": "stop"
                }
            }
        });
        let app_for_command = app.clone();
        let mut handle = tokio::spawn(async move {
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
        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                let stop_request_id = {
                    let state = app.state.lock().await;
                    state
                        .instances
                        .get("instance-a")
                        .map(|instance| instance.stop_request_id)
                        .unwrap_or(0)
                };
                if stop_request_id == 1 {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .expect("stop request should be recorded after live play state");

        let observed = mcp_plugin_stop_result_handler(
            State(app.clone()),
            Json(PluginStopResultPayload {
                instance_id: "instance-a".to_owned(),
                stop_request_id: 1,
                phase: "observed".to_owned(),
                error: None,
            }),
        )
        .await
        .unwrap();
        assert_eq!(observed.status(), StatusCode::NO_CONTENT);
        tokio::time::timeout(Duration::from_millis(50), &mut handle)
            .await
            .expect_err("remote stop must keep waiting after observed until edit heartbeat completes stop");
        {
            let mut state = app.state.lock().await;
            let instance = state.instances.get_mut("instance-a").unwrap();
            instance.studio_control_observed_at = Some(
                Instant::now() - STUDIO_CONTROL_HEARTBEAT_STALE_AFTER - Duration::from_millis(1),
            );
        }
        let edit_heartbeat = mcp_plugin_edit_heartbeat_handler(
            State(app.clone()),
            Json(test_edit_heartbeat_request("instance-a")),
        )
        .await
        .unwrap();
        assert_eq!(edit_heartbeat.status(), StatusCode::NO_CONTENT);

        let response = handle
            .await
            .expect("remote command task should join")
            .expect("runtime actuator stop should succeed");
        assert_eq!(response, "Stopped");
    }

    #[test]
    fn runtime_stop_timeout_converges_to_error_and_disables_delivery() {
        let mut instance = test_instance("93795519121520", Some("task-a"), 0);
        instance.studio_control_state = "stopping".to_owned();
        instance.studio_transition_phase = "stopping_requested".to_owned();
        instance.stop_request_id = 4;

        mark_runtime_stop_timeout(&mut instance, 4, "phase=stopping_requested".to_owned());

        assert_eq!(instance.studio_control_state, "lost");
        assert_eq!(instance.studio_transition_phase, "error");
        assert_eq!(active_stop_request_id_for_instance(&instance), None);
        assert!(!is_stop_request_deliverable_phase(
            &instance.studio_transition_phase
        ));
        assert_eq!(instance.stop_result_phase.as_deref(), Some("failed"));
        assert!(instance
            .studio_control_last_error
            .as_deref()
            .unwrap_or_default()
            .contains("runtime_stop_timeout"));
    }

    #[tokio::test]
    async fn runtime_stop_result_acknowledges_and_fails_active_stop_request() {
        let app = test_app_state();
        {
            let mut state = app.state.lock().await;
            let mut instance = test_instance("93795519121520", Some("task-a"), 0);
            instance.studio_mode = Some("start_play".to_owned());
            instance.studio_mode_source = "play_control".to_owned();
            instance.studio_mode_observed_at = Some(Instant::now());
            instance.studio_control_state = "stopping".to_owned();
            instance.studio_transition_phase = "stopping_requested".to_owned();
            instance.studio_control_observed_at = Some(Instant::now());
            instance.stop_request_id = 2;
            state.instances.insert("instance-a".to_owned(), instance);
        }

        let observed = mcp_plugin_stop_result_handler(
            State(app.clone()),
            Json(PluginStopResultPayload {
                instance_id: "instance-a".to_owned(),
                stop_request_id: 2,
                phase: "observed".to_owned(),
                error: None,
            }),
        )
        .await
        .unwrap();
        assert_eq!(observed.status(), StatusCode::NO_CONTENT);
        {
            let state = app.state.lock().await;
            let instance = state.instances.get("instance-a").unwrap();
            assert_eq!(instance.studio_control_state, "stopping");
            assert_eq!(instance.studio_transition_phase, "stopping_observed");
            assert!(instance.studio_control_last_error.is_none());
        }

        let poll_after_observed = mcp_plugin_stop_request_poll_handler(
            State(app.clone()),
            Query(PluginStopRequestQuery {
                instance_id: "instance-a".to_owned(),
                after_id: Some(0),
            }),
        )
        .await
        .unwrap();
        assert_eq!(poll_after_observed.status(), StatusCode::OK);
        let body = axum::body::to_bytes(poll_after_observed.into_body(), usize::MAX)
            .await
            .unwrap();
        let payload: PluginStopRequestResponse = serde_json::from_slice(&body).unwrap();
        assert!(!payload.stop_requested);
        assert_eq!(payload.stop_request_id, 2);

        let failed = mcp_plugin_stop_result_handler(
            State(app.clone()),
            Json(PluginStopResultPayload {
                instance_id: "instance-a".to_owned(),
                stop_request_id: 2,
                phase: "failed".to_owned(),
                error: Some("EndTest: can only be called once".to_owned()),
            }),
        )
        .await
        .unwrap();
        assert_eq!(failed.status(), StatusCode::NO_CONTENT);
        {
            let state = app.state.lock().await;
            let instance = state.instances.get("instance-a").unwrap();
            assert_eq!(effective_studio_control_state(instance), "lost");
            assert_eq!(effective_studio_transition_phase(instance), "error");
            assert!(instance
                .studio_control_last_error
                .as_deref()
                .unwrap_or_default()
                .contains("EndTest: can only be called once"));
        }

        let heartbeat_after_failure = mcp_plugin_control_heartbeat_handler(
            State(app.clone()),
            Json(PluginControlHeartbeatRequest {
                instance_id: "instance-a".to_owned(),
                mode: "start_play".to_owned(),
            }),
        )
        .await
        .unwrap();
        assert_eq!(heartbeat_after_failure.status(), StatusCode::NO_CONTENT);
        let state = app.state.lock().await;
        let instance = state.instances.get("instance-a").unwrap();
        assert_eq!(effective_studio_control_state(instance), "lost");
        assert_eq!(effective_studio_transition_phase(instance), "error");
        assert_eq!(
            instance.studio_control_last_error.as_deref(),
            Some("runtime_stop_failed: play runtime failed to execute stop_request_id=2: EndTest: can only be called once")
        );
    }
