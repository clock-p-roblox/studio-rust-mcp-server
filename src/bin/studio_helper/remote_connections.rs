fn validate_remote_ready_ack(
    expected_place_id: &str,
    expected_task_id: Option<&str>,
    ack_place_id: &str,
    ack_task_id: Option<&str>,
) -> Result<()> {
    if ack_place_id != expected_place_id {
        return Err(eyre!(
            "remote MCP ready ack place_id mismatch: expected {}, got {}",
            expected_place_id,
            ack_place_id,
        ));
    }
    if ack_task_id != expected_task_id {
        return Err(eyre!(
            "remote MCP ready ack task_id mismatch: expected {:?}, got {:?}",
            expected_task_id,
            ack_task_id,
        ));
    }
    Ok(())
}

fn build_active_remote_targets(app: &AppState, state: &HelperState) -> Vec<RemoteTarget> {
    let mut targets = Vec::new();
    for claimed_task in state.claimed_tasks.values() {
        targets.push(RemoteTarget {
            connection_key: task_connection_key(&claimed_task.task_id),
            place_id: claimed_task.place_id.clone(),
            task_id: Some(claimed_task.task_id.clone()),
            remote_base_url: claimed_task.mcp_base_url.clone(),
        });
    }
    let _ = app;
    targets
}

fn remote_connection_is_active(state: &HelperState, connection_key: &str) -> bool {
    state
        .claimed_tasks
        .values()
        .any(|task| task_connection_key(&task.task_id) == connection_key)
}

fn remote_connection_matches_worker(
    state: &HelperState,
    connection_key: &str,
    worker_id: Uuid,
) -> bool {
    state
        .remote_connections
        .get(connection_key)
        .map(|connection| connection.worker_id == worker_id)
        .unwrap_or(false)
}

fn set_remote_connection_connecting(
    state: &mut HelperState,
    connection_key: &str,
    worker_id: Uuid,
) {
    if let Some(connection) = state.remote_connections.get_mut(connection_key) {
        if connection.worker_id != worker_id {
            return;
        }
        let now = Instant::now();
        connection.connection_state = RemoteConnectionState::Connecting;
        connection.state_changed_at = now;
        if connection.retrying_since.is_none() {
            connection.retrying_since = Some(now);
        }
        connection.connection_id = None;
        connection.last_server_message_at = None;
    }
}

fn set_remote_connection_connected(
    state: &mut HelperState,
    connection_key: &str,
    worker_id: Uuid,
    connection_id: &str,
) {
    if let Some(connection) = state.remote_connections.get_mut(connection_key) {
        let now = Instant::now();
        if connection.worker_id != worker_id {
            return;
        }
        connection.connection_state = RemoteConnectionState::Connected;
        connection.state_changed_at = now;
        connection.retrying_since = None;
        connection.consecutive_failures = 0;
        connection.connection_id = Some(connection_id.to_owned());
        connection.last_ready_at = Some(now);
        connection.last_server_message_at = Some(now);
    }
    state.last_remote_errors.remove(connection_key);
}

fn note_remote_connection_activity(state: &mut HelperState, connection_key: &str, worker_id: Uuid) {
    if let Some(connection) = state.remote_connections.get_mut(connection_key) {
        if connection.worker_id != worker_id {
            return;
        }
        connection.last_server_message_at = Some(Instant::now());
    }
}

fn set_remote_connection_retrying(
    state: &mut HelperState,
    connection_key: &str,
    worker_id: Uuid,
    error: &str,
) {
    let mut matched_worker = false;
    if let Some(connection) = state.remote_connections.get_mut(connection_key) {
        if connection.worker_id != worker_id {
            return;
        }
        matched_worker = true;
        let now = Instant::now();
        connection.connection_state = RemoteConnectionState::Retrying;
        connection.state_changed_at = now;
        if connection.retrying_since.is_none() {
            connection.retrying_since = Some(now);
        }
        connection.consecutive_failures = connection.consecutive_failures.saturating_add(1);
        connection.connection_id = None;
    }
    if matched_worker {
        state
            .last_remote_errors
            .insert(connection_key.to_owned(), summarize_error(error));
    }
}

fn set_remote_connection_error(state: &mut HelperState, connection_key: &str, error: &str) {
    if let Some(connection) = state.remote_connections.get_mut(connection_key) {
        connection.connection_state = RemoteConnectionState::Error;
        connection.state_changed_at = Instant::now();
        connection.connection_id = None;
    }
    state
        .last_remote_errors
        .insert(connection_key.to_owned(), summarize_error(error));
}

fn remote_connection_should_restart(connection: &RemoteConnectionHandle) -> bool {
    match connection.connection_state {
        RemoteConnectionState::Connecting | RemoteConnectionState::Retrying => {
            connection.state_changed_at.elapsed() >= REMOTE_WS_STALE_RESTART_AFTER
        }
        RemoteConnectionState::Connected | RemoteConnectionState::Error => false,
    }
}

fn remote_connection_should_release(connection: &RemoteConnectionHandle) -> bool {
    connection
        .retrying_since
        .map(|started_at| started_at.elapsed() >= REMOTE_WS_STALE_RELEASE_AFTER)
        .unwrap_or(false)
}

fn spawn_remote_connection(
    app: &AppState,
    state: &mut HelperState,
    target: RemoteTarget,
    retrying_since: Option<Instant>,
    consecutive_failures: u32,
) {
    let (stop_tx, stop_rx) = watch::channel(false);
    let (sender, _receiver) = mpsc::unbounded_channel::<RemoteOutgoingFrame>();
    let worker_id = Uuid::new_v4();
    state.remote_connections.insert(
        target.connection_key.clone(),
        RemoteConnectionHandle {
            worker_id,
            stop_tx,
            sender,
            remote_base_url: target.remote_base_url.clone(),
            place_id: target.place_id.clone(),
            task_id: target.task_id.clone(),
            connection_state: RemoteConnectionState::Connecting,
            state_changed_at: Instant::now(),
            retrying_since,
            consecutive_failures,
            connection_id: None,
            last_ready_at: None,
            last_server_message_at: None,
        },
    );
    let worker_app = app.clone();
    tokio::spawn(async move {
        remote_ws_loop(worker_app, target, worker_id, stop_rx).await;
    });
}

fn sync_remote_connections(app: &AppState, state: &mut HelperState) {
    let active_targets = build_active_remote_targets(app, state);
    let active_keys: HashSet<String> = active_targets
        .iter()
        .map(|target| target.connection_key.clone())
        .collect();
    let stale_places: Vec<String> = state
        .remote_connections
        .keys()
        .filter(|connection_key| !active_keys.contains(*connection_key))
        .cloned()
        .collect();
    for connection_key in stale_places {
        if let Some(handle) = state.remote_connections.remove(&connection_key) {
            let _ = handle.stop_tx.send(true);
            tracing::info!(
                connection_key,
                "stopping helper remote websocket because plugin is gone"
            );
        }
        state.last_remote_errors.remove(&connection_key);
    }
    let mut task_ids_to_release = Vec::new();
    for target in active_targets {
        if let Some(connection) = state.remote_connections.get(&target.connection_key) {
            if remote_connection_should_release(connection) {
                let message = format!(
                    "remote websocket did not recover after {}s",
                    REMOTE_WS_STALE_RELEASE_AFTER.as_secs()
                );
                tracing::warn!(
                    connection_key = target.connection_key,
                    place_id = target.place_id,
                    task_id = ?target.task_id,
                    consecutive_failures = connection.consecutive_failures,
                    "releasing claimed task after remote websocket retry timeout"
                );
                set_remote_connection_error(state, &target.connection_key, &message);
                if let Some(task_id) = target.task_id.as_ref() {
                    task_ids_to_release.push(task_id.clone());
                }
                continue;
            }
            if remote_connection_should_restart(connection) {
                let retrying_since = connection.retrying_since;
                let consecutive_failures = connection.consecutive_failures;
                let old_stop_tx = connection.stop_tx.clone();
                let _ = old_stop_tx.send(true);
                state.remote_connections.remove(&target.connection_key);
                state.last_remote_errors.insert(
                    target.connection_key.clone(),
                    "remote websocket retry watchdog restarted stale connection".to_owned(),
                );
                tracing::warn!(
                    connection_key = target.connection_key,
                    place_id = target.place_id,
                    task_id = ?target.task_id,
                    consecutive_failures,
                    "restarting stale helper remote websocket loop"
                );
                spawn_remote_connection(app, state, target, retrying_since, consecutive_failures);
            }
            continue;
        }
        spawn_remote_connection(app, state, target, None, 0);
    }
    for task_id in task_ids_to_release {
        let _ = release_claimed_task(
            state,
            &task_id,
            false,
            "remote websocket retry watchdog timed out",
        );
    }
}

async fn remote_ws_loop(
    app: AppState,
    target: RemoteTarget,
    worker_id: Uuid,
    mut stop_rx: watch::Receiver<bool>,
) {
    let ws_url = derive_remote_helper_ws_url_from_base_url(&target.remote_base_url);
    tracing::info!(
        connection_key = target.connection_key,
        place_id = target.place_id,
        ws_url,
        "starting helper remote websocket loop"
    );
    loop {
        if *stop_rx.borrow() {
            break;
        }
        {
            let mut state = app.state.lock().await;
            cleanup_stale_instances(&mut state);
            if !remote_connection_is_active(&state, &target.connection_key) {
                if remote_connection_matches_worker(&state, &target.connection_key, worker_id) {
                    state.remote_connections.remove(&target.connection_key);
                    state.last_remote_errors.remove(&target.connection_key);
                }
                break;
            }
        }

        {
            let mut state = app.state.lock().await;
            set_remote_connection_connecting(&mut state, &target.connection_key, worker_id);
        }
        let result = run_remote_ws_session(
            app.clone(),
            &target.connection_key,
            worker_id,
            &target.place_id,
            target.task_id.as_deref(),
            &ws_url,
            &mut stop_rx,
        )
        .await;
        match result {
            Ok(()) => break,
            Err(error) => {
                tracing::warn!(
                    connection_key = target.connection_key,
                    place_id = target.place_id,
                    error = summarize_error(&error.to_string()),
                    "helper remote websocket session failed"
                );
                let mut state = app.state.lock().await;
                if remote_connection_is_active(&state, &target.connection_key) {
                    set_remote_connection_retrying(
                        &mut state,
                        &target.connection_key,
                        worker_id,
                        &error.to_string(),
                    );
                }
                drop(state);
                tokio::select! {
                    _ = tokio::time::sleep(Duration::from_secs(3)) => {}
                    changed = stop_rx.changed() => {
                        if changed.is_ok() && *stop_rx.borrow() {
                            break;
                        }
                    }
                }
                continue;
            }
        }
    }
    let mut state = app.state.lock().await;
    if remote_connection_matches_worker(&state, &target.connection_key, worker_id) {
        state.remote_connections.remove(&target.connection_key);
        state.last_remote_errors.remove(&target.connection_key);
    }
    tracing::info!(
        connection_key = target.connection_key,
        place_id = target.place_id,
        "stopped helper remote websocket loop"
    );
}

fn mark_hub_reachable(state: &mut HelperState) {
    state.hub_last_error = None;
    state.hub_last_ready_at = Some(Instant::now());
}

fn mark_hub_claim_ok(state: &mut HelperState) {
    mark_hub_reachable(state);
    state.hub_last_claim_error = None;
}

fn record_hub_claim_error(state: &mut HelperState, claim_error: String) {
    state.hub_last_claim_error = Some(claim_error.clone());
    if state.claimed_tasks.is_empty() {
        state.hub_last_error = Some(claim_error);
    }
}

async fn hub_maintenance_loop(app: AppState, mut registered: bool) {
    let mut interval = tokio::time::interval(Duration::from_secs(5));
    loop {
        tokio::select! {
            _ = interval.tick() => {}
            _ = app.helper.hub_heartbeat_notify.notified() => {}
        }
        if !registered {
            match hub_register_helper(&app.helper).await {
                Ok(_) => {
                    registered = true;
                    let mut state = app.state.lock().await;
                    mark_hub_reachable(&mut state);
                }
                Err(RegisterHelperError::HelperIdConflict(message)) => {
                    let mut state = app.state.lock().await;
                    state.hub_last_error = Some(format!("helper_id_conflict: {message}"));
                    continue;
                }
                Err(RegisterHelperError::Other(error)) => {
                    let mut state = app.state.lock().await;
                    state.hub_last_error = Some(summarize_error(&error.to_string()));
                    continue;
                }
            }
        }

        let (active_task_ids, active_tasks) = {
            let state = app.state.lock().await;
            let mut active_task_ids = state
                .claimed_tasks
                .values()
                .map(|task| task.task_id.clone())
                .collect::<Vec<_>>();
            active_task_ids.sort();
            let active_tasks = heartbeat_task_statuses(&state);
            (active_task_ids, active_tasks)
        };

        match hub_helper_heartbeat(&app.helper, active_task_ids, active_tasks).await {
            Ok(response) => {
                let pending_terminations = {
                    let mut state = app.state.lock().await;
                    let mut pending_terminations = Vec::new();
                    if response.ok {
                        mark_hub_reachable(&mut state);
                    } else {
                        state.hub_last_error = Some("hub rejected helper heartbeat".to_owned());
                        state.hub_last_ready_at = None;
                        pending_terminations.extend(release_all_claimed_tasks(
                            &mut state,
                            true,
                            "hub rejected helper heartbeat",
                        ));
                    }
                    for task_id in response.release_task_ids {
                        if let Some(pending_termination) = release_claimed_task(
                            &mut state,
                            &task_id,
                            true,
                            "hub requested launch release",
                        ) {
                            pending_terminations.push(pending_termination);
                        }
                    }
                    sync_remote_connections(&app, &mut state);
                    pending_terminations
                };
                for pending_termination in pending_terminations {
                    terminate_pending_launch_termination(pending_termination).await;
                }
            }
            Err(error) => {
                let error_text = summarize_error(&error.to_string());
                let mut state = app.state.lock().await;
                state.hub_last_error = Some(error_text.clone());
                if error.to_string().contains("helper not found") {
                    registered = false;
                }
                continue;
            }
        }

        let should_claim = {
            let state = app.state.lock().await;
            state.claimed_tasks.len() < app.helper.capacity
        };
        if !should_claim {
            let mut state = app.state.lock().await;
            state.hub_last_claim_error = None;
            continue;
        }
        match hub_claim_task(&app.helper).await {
            Ok(response) => {
                {
                    let mut state = app.state.lock().await;
                    mark_hub_claim_ok(&mut state);
                }
                if let Some(task) = response.task {
                    let Some(mcp_base_url) = task.routes.mcp_base_url else {
                        let mut state = app.state.lock().await;
                        state.hub_last_error =
                            Some("hub claimed task missing mcp_base_url".to_owned());
                        continue;
                    };
                    let mut claimed_task = ClaimedTask {
                        task_id: task.task_id.clone(),
                        place_id: task.place_id,
                        universe_id: task.universe_id.or(task.game_id),
                        mcp_base_url,
                        rojo_base_url: task.routes.rojo_base_url,
                        runtime_log_base_url: task.routes.runtime_log_base_url,
                        claimed_at: Instant::now(),
                    };
                    let launch_record: Option<LaunchProcessRecord> =
                        if app.helper.skip_claim_studio_launch {
                            tracing::warn!(
                                task_id = claimed_task.task_id.as_str(),
                                place_id = claimed_task.place_id.as_str(),
                                "skipping Studio launch for claimed task"
                            );
                            None
                        } else {
                            if let Err(error) =
                                ensure_claimed_task_universe_id(&app.helper, &mut claimed_task)
                                    .await
                            {
                                let mut state = app.state.lock().await;
                                state.hub_last_error = Some(summarize_error(&error.to_string()));
                                continue;
                            }
                            #[cfg(target_os = "windows")]
                            {
                                match launch_studio_for_claim(&app.helper, &claimed_task) {
                                    Ok(record) => Some(record),
                                    Err(error) => {
                                        let mut state = app.state.lock().await;
                                        state.hub_last_error =
                                            Some(summarize_error(&error.to_string()));
                                        continue;
                                    }
                                }
                            }
                            #[cfg(not(target_os = "windows"))]
                            {
                                None
                            }
                        };
                    let mut state = app.state.lock().await;
                    state
                        .claimed_tasks
                        .insert(claimed_task.task_id.clone(), claimed_task);
                    if let Some(launch_record) = launch_record {
                        state
                            .launch_processes
                            .insert(launch_record.task_id.clone(), launch_record);
                    }
                    mark_hub_claim_ok(&mut state);
                    sync_remote_connections(&app, &mut state);
                }
            }
            Err(error) => {
                let claim_error = summarize_error(&error.to_string());
                let mut state = app.state.lock().await;
                record_hub_claim_error(&mut state, claim_error);
            }
        }
    }
}

async fn connect_remote_ws(
    helper: &HelperConfig,
    ws_url: &str,
) -> Result<
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
> {
    let current_token = helper.bearer_token.lock().await.clone();
    let current_source = helper.bearer_token_source.lock().await.clone();
    let mut attempts = vec![ResolvedToken {
        value: current_token.clone(),
        source: current_source,
    }];
    for candidate in helper.bearer_token_candidates.iter() {
        if candidate.value != current_token {
            attempts.push(candidate.clone());
        }
    }
    let mut last_error = None;
    for candidate in attempts {
        let mut request = ws_url.into_client_request()?;
        request.headers_mut().insert(
            AUTHORIZATION,
            format!("Bearer {}", candidate.value).parse()?,
        );
        match tokio::time::timeout(REMOTE_WS_CONNECT_TIMEOUT, connect_async(request)).await {
            Ok(Ok((stream, _response))) => {
                if candidate.value != current_token {
                    *helper.bearer_token.lock().await = candidate.value.clone();
                    *helper.bearer_token_source.lock().await = candidate.source.clone();
                    tracing::warn!(
                        source = candidate.source,
                        "helper switched bearer token after websocket connect retry"
                    );
                }
                return Ok(stream);
            }
            Ok(Err(error)) => {
                last_error = Some(error.to_string());
            }
            Err(_) => {
                last_error = Some(format!(
                    "websocket connect timed out after {}s",
                    REMOTE_WS_CONNECT_TIMEOUT.as_secs()
                ));
            }
        }
    }
    Err(eyre!(
        last_error.unwrap_or_else(|| "websocket connect failed".to_owned())
    ))
}
