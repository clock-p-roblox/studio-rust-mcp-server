    #[tokio::test]
    async fn runtime_log_forward_fast_acks_slow_upstream() {
        let app = test_app_state_with_runtime_log_worker();
        let (sink_base_url, received_rx) =
            spawn_test_runtime_log_sink(204, Duration::from_millis(750)).await;
        insert_claimed_task_with_runtime_log_base(
            &app,
            "task-a",
            "134795435066737",
            sink_base_url.clone(),
        )
        .await;

        let started = Instant::now();
        let response = call_runtime_log_forward(app.clone(), "v1/runtime-logs")
            .await
            .expect("runtime-log upload should be accepted");

        assert_eq!(response.status(), StatusCode::ACCEPTED);
        assert!(
            started.elapsed() < Duration::from_millis(250),
            "runtime-log forward waited for slow upstream instead of acking quickly"
        );
        let request = received_rx
            .await
            .expect("sink should receive background request");
        assert!(request.starts_with("POST /v1/runtime-logs "));
        tokio::time::sleep(Duration::from_millis(900)).await;
        let state = app.state.lock().await;
        let status = runtime_log_forward_status_snapshot(&state, "task-a");
        assert_eq!(status.accepted_count, 1);
        assert_eq!(status.forwarded_count, 1);
        assert_eq!(status.failed_count, 0);
        assert_eq!(status.state, "ready");
    }

    #[tokio::test]
    async fn runtime_log_forward_allows_upload_query_and_forwards_it() {
        let app = test_app_state_with_runtime_log_worker();
        let (sink_base_url, received_rx) =
            spawn_test_runtime_log_sink(204, Duration::from_millis(0)).await;
        insert_claimed_task_with_runtime_log_base(
            &app,
            "task-a",
            "134795435066737",
            sink_base_url.clone(),
        )
        .await;

        let response = call_runtime_log_forward_with_uri(
            app,
            "v1/runtime-logs",
            Uri::from_static("/v1/runtime-logs?cursor=abc"),
        )
        .await
        .expect("runtime-log upload with query should be accepted");

        assert_eq!(response.status(), StatusCode::ACCEPTED);
        let request = received_rx
            .await
            .expect("sink should receive background request");
        assert!(request.starts_with("POST /v1/runtime-logs?cursor=abc "));
    }

    #[tokio::test]
    async fn runtime_log_forward_completion_after_release_does_not_recreate_status() {
        let app = test_app_state_with_runtime_log_worker();
        let (sink_base_url, _received_rx) =
            spawn_test_runtime_log_sink(204, Duration::from_millis(750)).await;
        insert_claimed_task_with_runtime_log_base(
            &app,
            "task-a",
            "134795435066737",
            sink_base_url.clone(),
        )
        .await;

        let response = call_runtime_log_forward(app.clone(), "v1/runtime-logs")
            .await
            .expect("runtime-log upload should be accepted before release");
        assert_eq!(response.status(), StatusCode::ACCEPTED);
        {
            let mut state = app.state.lock().await;
            assert!(state.runtime_log_forward_statuses.contains_key("task-a"));
            let _ = release_claimed_task(&mut state, "task-a", false, "test release");
            assert!(!state.runtime_log_forward_statuses.contains_key("task-a"));
        }

        tokio::time::sleep(Duration::from_millis(900)).await;
        let state = app.state.lock().await;
        assert!(
            !state.runtime_log_forward_statuses.contains_key("task-a"),
            "released task must not be recreated by a late runtime-log forward completion"
        );
    }

    #[tokio::test]
    async fn runtime_log_forward_records_background_http_failure() {
        let app = test_app_state_with_runtime_log_worker();
        let (sink_base_url, _received_rx) =
            spawn_test_runtime_log_sink(503, Duration::from_millis(0)).await;
        insert_claimed_task_with_runtime_log_base(
            &app,
            "task-a",
            "134795435066737",
            sink_base_url.clone(),
        )
        .await;

        let response = call_runtime_log_forward(app.clone(), "v1/runtime-logs")
            .await
            .expect("runtime-log upload should be accepted before background failure");

        assert_eq!(response.status(), StatusCode::ACCEPTED);
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let state = app.state.lock().await;
                let status = runtime_log_forward_status_snapshot(&state, "task-a");
                if status.failed_count == 1 {
                    assert_eq!(status.state, "error");
                    assert_eq!(status.last_http_status, Some(503));
                    let last_error = status.last_error.as_deref().unwrap_or_default();
                    assert!(last_error.contains("HTTP 503"));
                    assert!(
                        !last_error.contains(&sink_base_url),
                        "runtime-log status must not expose upstream URL"
                    );
                    break;
                }
                drop(state);
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        })
        .await
        .expect("runtime-log background failure should update status");
    }

    #[tokio::test]
    async fn runtime_log_forward_records_background_connection_failure() {
        let app = test_app_state_with_runtime_log_worker();
        let sink_base_url = spawn_disconnect_runtime_log_sink().await;
        insert_claimed_task_with_runtime_log_base(
            &app,
            "task-a",
            "134795435066737",
            sink_base_url.clone(),
        )
        .await;

        let response = call_runtime_log_forward(app.clone(), "v1/runtime-logs")
            .await
            .expect("runtime-log upload should be accepted before background connection failure");

        assert_eq!(response.status(), StatusCode::ACCEPTED);
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let state = app.state.lock().await;
                let status = runtime_log_forward_status_snapshot(&state, "task-a");
                if status.failed_count == 1 {
                    assert_eq!(status.state, "error");
                    assert_eq!(status.last_http_status, None);
                    let last_error = status.last_error.as_deref().unwrap_or_default();
                    assert!(last_error.contains("runtime-log forward failed"));
                    assert!(
                        !last_error.contains(&sink_base_url),
                        "runtime-log status must not expose upstream URL"
                    );
                    break;
                }
                drop(state);
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        })
        .await
        .expect("runtime-log background connection failure should update status");
    }

    #[tokio::test]
    async fn runtime_log_forward_queue_full_does_not_decrement_existing_queue() {
        let (app, _runtime_log_forward_rx) = test_app_state_with_runtime_log_queue_only();
        insert_claimed_task_with_runtime_log_base(
            &app,
            "task-a",
            "134795435066737",
            "http://127.0.0.1:9".to_owned(),
        )
        .await;

        for _ in 0..RUNTIME_LOG_FORWARD_QUEUE_CAPACITY {
            let response = call_runtime_log_forward(app.clone(), "v1/runtime-logs")
                .await
                .expect("runtime-log upload should be accepted while queue has capacity");
            assert_eq!(response.status(), StatusCode::ACCEPTED);
        }
        let response = call_runtime_log_forward(app.clone(), "v1/runtime-logs")
            .await
            .expect("full runtime-log queue should return a response");

        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        let state = app.state.lock().await;
        let status = runtime_log_forward_status_snapshot(&state, "task-a");
        assert_eq!(
            status.queued_count, RUNTIME_LOG_FORWARD_QUEUE_CAPACITY as u64,
            "queue_full must not decrement jobs that were already queued"
        );
        assert_eq!(
            status.accepted_count,
            RUNTIME_LOG_FORWARD_QUEUE_CAPACITY as u64
        );
        assert_eq!(status.failed_count, 1);
        assert!(status
            .last_error
            .as_deref()
            .unwrap_or_default()
            .contains("queue is full"));
    }

    #[tokio::test]
    async fn runtime_log_forward_rejects_non_upload_proxy_paths() {
        let app = test_app_state_with_runtime_log_worker();
        insert_claimed_task_with_runtime_log_base(
            &app,
            "task-a",
            "134795435066737",
            "http://127.0.0.1:1".to_owned(),
        )
        .await;

        let response = call_runtime_log_forward(app.clone(), "v1/not-runtime-log")
            .await
            .expect("unsupported runtime-log forward path should return a response");

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let state = app.state.lock().await;
        let status = runtime_log_forward_status_snapshot(&state, "task-a");
        assert_eq!(status.accepted_count, 0);
        assert_eq!(status.failed_count, 0);
        assert_eq!(status.state, "idle");
    }

