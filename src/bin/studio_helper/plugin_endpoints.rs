async fn mcp_register_handler(
    State(app): State<AppState>,
    ConnectInfo(peer_addr): ConnectInfo<SocketAddr>,
    Json(payload): Json<RegisterPluginRequest>,
) -> Result<Json<RegisterPluginResponse>, HelperError> {
    let place_id = sanitize_place_id(&payload.place_id)?;
    let instance_id = Uuid::new_v4().to_string();
    let explicit_task_id = maybe_sanitize_identifier("task_id", payload.task_id.as_deref())?;
    let studio_pid = resolve_peer_process_id_with_retry(peer_addr, app.helper.port).await?;
    let claimed_task = {
        let state = app.state.lock().await;
        resolve_plugin_routing_decision(&state, &place_id, explicit_task_id.as_deref(), studio_pid)
            .map_err(HelperError)?
    };
    let remote_base_url = claimed_task.mcp_base_url.clone();
    let runtime_log_upload_url = claimed_task
        .runtime_log_base_url
        .as_ref()
        .map(|_| runtime_log_forward_upload_url(app.helper.port, &place_id, &claimed_task.task_id));
    {
        let mut state = app.state.lock().await;
        state.instances.insert(
            instance_id.clone(),
            PluginInstance {
                place_id: place_id.clone(),
                task_id: Some(claimed_task.task_id.clone()),
                remote_base_url: remote_base_url.clone(),
                studio_pid,
                studio_mode: None,
                studio_mode_observed_at: None,
                studio_mode_source: "none".to_owned(),
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
                last_seen_at: Instant::now(),
                queue: VecDeque::new(),
                notify: Arc::new(Notify::new()),
            },
        );
        if let Some(studio_pid) = studio_pid {
            bind_launch_process_to_pid(&mut state, &claimed_task, studio_pid)
                .map_err(HelperError)?;
        }
        sync_remote_connections(&app, &mut state);
    }
    tracing::info!(
        instance_id,
        place_id,
        task_id = claimed_task.task_id,
        studio_pid,
        remote_base_url,
        "registered MCP plugin instance with helper"
    );
    Ok(Json(RegisterPluginResponse {
        instance_id,
        place_id,
        task_id: Some(claimed_task.task_id),
        remote_base_url,
        runtime_log_upload_url,
    }))
}

async fn mcp_unregister_handler(
    State(app): State<AppState>,
    Json(payload): Json<UnregisterPluginRequest>,
) -> Result<Response, HelperError> {
    let mut state = app.state.lock().await;
    let removed_place_id = state
        .instances
        .remove(&payload.instance_id)
        .map(|instance| instance.place_id);
    if let Some(place_id) = removed_place_id.as_ref() {
        let still_active = state
            .instances
            .values()
            .any(|instance| instance.place_id == *place_id);
        if !still_active {
            state.last_remote_errors.remove(place_id);
        }
    }
    sync_remote_connections(&app, &mut state);
    tracing::info!(
        instance_id = payload.instance_id,
        "unregistered MCP plugin instance"
    );
    Ok(StatusCode::NO_CONTENT.into_response())
}

async fn mcp_plugin_stop_request_handler(
    State(app): State<AppState>,
    Json(payload): Json<PluginStopRequestPayload>,
) -> Result<Response, HelperError> {
    let mut state = app.state.lock().await;
    let recorded = match record_stop_request_for_instance(&mut state, &payload.instance_id) {
        Ok(recorded) => recorded,
        Err(StopRequestRecordError::MissingInstance) => {
            return Err(HelperError(eyre!(
                "plugin instance expired: {}",
                payload.instance_id
            )));
        }
        Err(StopRequestRecordError::RuntimeActuatorUnavailable(detail)) => {
            tracing::warn!(
                instance_id = payload.instance_id,
                detail,
                "rejected MCP plugin stop request because runtime actuator is not fresh"
            );
            return Ok((
                StatusCode::CONFLICT,
                format!("runtime_stop_actuator_unavailable: {detail}"),
            )
                .into_response());
        }
    };
    if let Some(task_id) = recorded.task_id.as_deref() {
        queue_task_status_updates(&state, &app.helper, task_id);
    }
    tracing::info!(
        instance_id = payload.instance_id,
        place_id = recorded.place_id,
        task_id = ?recorded.task_id,
        stop_request_id = recorded.stop_request_id,
        "recorded MCP plugin stop request"
    );
    Ok(Json(PluginStopRequestResponse {
        stop_requested: true,
        stop_request_id: recorded.stop_request_id,
    })
    .into_response())
}

async fn mcp_plugin_stop_request_poll_handler(
    State(app): State<AppState>,
    Query(query): Query<PluginStopRequestQuery>,
) -> Result<Response, HelperError> {
    let mut state = app.state.lock().await;
    let (stop_requested, stop_request_id, task_id) = {
        let Some(instance) = state.instances.get_mut(&query.instance_id) else {
            return Ok((StatusCode::GONE, "instance expired").into_response());
        };
        let after_id = query.after_id.unwrap_or(0);
        let stop_request_id = instance.stop_request_id;
        let stop_requested = stop_request_id > after_id
            && is_stop_request_deliverable_phase(&instance.studio_transition_phase);
        instance.stop_request_last_polled_at = Some(Instant::now());
        instance.stop_request_last_poll_id = Some(if stop_requested {
            stop_request_id
        } else {
            after_id
        });
        (stop_requested, stop_request_id, instance.task_id.clone())
    };
    if let Some(task_id) = task_id.as_deref() {
        queue_task_status_updates(&state, &app.helper, task_id);
    }
    Ok(Json(PluginStopRequestResponse {
        stop_requested,
        stop_request_id,
    })
    .into_response())
}

async fn mcp_plugin_stop_result_handler(
    State(app): State<AppState>,
    Json(payload): Json<PluginStopResultPayload>,
) -> Result<Response, HelperError> {
    let mut state = app.state.lock().await;
    let (place_id, task_id, should_push_status) = {
        let Some(instance) = state.instances.get_mut(&payload.instance_id) else {
            return Ok((StatusCode::GONE, "instance expired").into_response());
        };
        if instance.stop_request_id != payload.stop_request_id {
            tracing::warn!(
                instance_id = payload.instance_id,
                stop_request_id = payload.stop_request_id,
                current_stop_request_id = instance.stop_request_id,
                phase = payload.phase,
                "ignored stale MCP plugin stop result"
            );
            return Ok(StatusCode::NO_CONTENT.into_response());
        }
        if !is_stopping_transition_phase(&instance.studio_transition_phase) {
            tracing::warn!(
                instance_id = payload.instance_id,
                stop_request_id = payload.stop_request_id,
                phase = payload.phase,
                current_phase = instance.studio_transition_phase,
                "ignored MCP plugin stop result outside stopping phase"
            );
            return Ok(StatusCode::NO_CONTENT.into_response());
        }

        let previous_phase = instance.studio_transition_phase.clone();
        match payload.phase.as_str() {
            "observed" => {
                instance.studio_control_state = "stopping".to_owned();
                set_studio_transition_phase(instance, "stopping_observed");
                instance.studio_control_last_error = None;
                instance.stop_result_phase = Some("observed".to_owned());
                instance.stop_result_error = None;
                instance.stop_result_observed_at = Some(Instant::now());
            }
            "failed" => {
                let message =
                    format!(
                    "runtime_stop_failed: play runtime failed to execute stop_request_id={}: {}",
                    payload.stop_request_id,
                    payload.error.as_deref().unwrap_or("unknown runtime stop failure")
                );
                instance.studio_control_state = "lost".to_owned();
                set_studio_transition_phase(instance, "error");
                instance.studio_control_observed_at = None;
                instance.studio_control_last_error = Some(message);
                instance.stop_result_phase = Some("failed".to_owned());
                instance.stop_result_error = instance.studio_control_last_error.clone();
                instance.stop_result_observed_at = Some(Instant::now());
            }
            _ => return Ok((StatusCode::BAD_REQUEST, "invalid stop result phase").into_response()),
        }
        instance.last_seen_at = Instant::now();
        (
            instance.place_id.clone(),
            instance.task_id.clone(),
            previous_phase != instance.studio_transition_phase,
        )
    };
    if should_push_status {
        if let Some(task_id) = task_id.as_deref() {
            queue_task_status_updates(&state, &app.helper, task_id);
        }
    }
    tracing::info!(
        instance_id = payload.instance_id,
        place_id,
        task_id = ?task_id,
        stop_request_id = payload.stop_request_id,
        phase = payload.phase,
        "accepted MCP plugin stop result"
    );
    Ok(StatusCode::NO_CONTENT.into_response())
}

async fn mcp_plugin_control_heartbeat_handler(
    State(app): State<AppState>,
    Json(payload): Json<PluginControlHeartbeatRequest>,
) -> Result<Response, HelperError> {
    let Some(mode) = normalize_studio_mode(Some(payload.mode.as_str())) else {
        return Ok((StatusCode::BAD_REQUEST, "invalid studio mode").into_response());
    };
    if !studio_mode_is_running(Some(mode.as_str())) {
        return Ok((
            StatusCode::BAD_REQUEST,
            "control heartbeat requires a running mode",
        )
            .into_response());
    }
    let mut state = app.state.lock().await;
    let (place_id, task_id, should_push_status) = {
        let Some(instance) = state.instances.get_mut(&payload.instance_id) else {
            return Ok((StatusCode::GONE, "instance expired").into_response());
        };
        let previous_control_state = effective_studio_control_state(instance);
        let previous_transition_phase = effective_studio_transition_phase(instance);
        instance.studio_control_observed_at = Some(Instant::now());
        if is_error_transition_phase(&instance.studio_transition_phase) {
            instance.studio_control_state = "lost".to_owned();
        } else if is_stopping_transition_phase(&instance.studio_transition_phase) {
            instance.studio_control_state = "stopping".to_owned();
        } else {
            instance.studio_control_state = "ready".to_owned();
            set_studio_transition_phase(instance, "running");
            instance.studio_control_last_error = None;
            set_edit_runtime_unavailable(instance, "runtime");
        }
        instance.last_seen_at = Instant::now();
        (
            instance.place_id.clone(),
            instance.task_id.clone(),
            previous_control_state != "ready" || previous_transition_phase != "running",
        )
    };
    if should_push_status {
        if let Some(task_id) = task_id.as_deref() {
            queue_task_status_updates(&state, &app.helper, task_id);
        }
    }
    tracing::debug!(
        instance_id = payload.instance_id,
        place_id,
        task_id = ?task_id,
        studio_mode = mode,
        "accepted MCP plugin control heartbeat"
    );
    Ok(StatusCode::NO_CONTENT.into_response())
}

async fn mcp_plugin_request_handler(
    State(app): State<AppState>,
    Query(query): Query<PluginRequestQuery>,
) -> Result<Response, HelperError> {
    let timeout = tokio::time::timeout(LOCAL_LONG_POLL_TIMEOUT, async {
        loop {
            let notify = {
                let mut state = app.state.lock().await;
                let Some(instance) = state.instances.get_mut(&query.instance_id) else {
                    tracing::info!(
                        instance_id = query.instance_id,
                        "plugin polled helper with expired instance_id"
                    );
                    return Ok::<Option<Value>, color_eyre::Report>(None);
                };
                instance.last_seen_at = Instant::now();
                if let Some(command) = instance.queue.pop_front() {
                    tracing::info!(
                        instance_id = query.instance_id,
                        "helper delivered queued MCP command to plugin"
                    );
                    return Ok::<Option<Value>, color_eyre::Report>(Some(command));
                }
                Arc::clone(&instance.notify)
            };
            notify.notified().await;
        }
    })
    .await;
    match timeout {
        Ok(result) => match result? {
            Some(command) => Ok(Json(command).into_response()),
            None => Ok((StatusCode::GONE, "instance expired").into_response()),
        },
        Err(_) => Ok(StatusCode::NO_CONTENT.into_response()),
    }
}

async fn mcp_plugin_edit_heartbeat_handler(
    State(app): State<AppState>,
    Json(payload): Json<PluginEditHeartbeatRequest>,
) -> Result<Response, HelperError> {
    let mut state = app.state.lock().await;
    let task_id = {
        let Some(instance) = state.instances.get_mut(&payload.instance_id) else {
            tracing::warn!(
                instance_id = payload.instance_id,
                "rejected MCP plugin edit heartbeat for missing instance"
            );
            return Ok((StatusCode::GONE, "instance expired").into_response());
        };
        if is_stopping_transition_phase(&instance.studio_transition_phase)
            && instance.stop_request_id > 0
        {
            instance.edit_runtime_observed_at = Some(Instant::now());
            if instance.stop_result_phase.as_deref() == Some("observed")
                && runtime_control_heartbeat_is_stale(instance)
            {
                instance.studio_control_state = "none".to_owned();
                set_studio_transition_phase(instance, "idle");
                instance.studio_control_observed_at = None;
                instance.studio_control_last_error = None;
                instance.stop_result_phase = Some("completed".to_owned());
                instance.stop_result_error = None;
                instance.stop_result_observed_at = Some(Instant::now());
                set_edit_runtime_available(instance);
            } else {
                set_edit_runtime_unavailable(instance, "stopping");
            }
        } else {
            match instance.edit_runtime_unavailable_reason.as_deref() {
                Some("launching") => {
                    instance.edit_runtime_observed_at = Some(Instant::now());
                }
                Some("launch_failed")
                    if runtime_control_heartbeat_expired_after_observation(instance) =>
                {
                    instance.studio_control_state = "none".to_owned();
                    set_studio_transition_phase(instance, "idle");
                    instance.studio_control_observed_at = None;
                    instance.studio_control_last_error = None;
                    set_edit_runtime_available(instance);
                }
                Some("launch_failed") => {
                    instance.edit_runtime_observed_at = Some(Instant::now());
                }
                Some("runtime")
                    if runtime_control_heartbeat_expired_after_observation(instance) =>
                {
                    instance.studio_control_state = "none".to_owned();
                    set_studio_transition_phase(instance, "idle");
                    instance.studio_control_observed_at = None;
                    instance.studio_control_last_error = None;
                    set_edit_runtime_available(instance);
                }
                Some("runtime") => {
                    instance.edit_runtime_observed_at = Some(Instant::now());
                    set_edit_runtime_unavailable(instance, "runtime");
                }
                _ => {
                    set_edit_runtime_available(instance);
                }
            }
        }
        instance.last_seen_at = Instant::now();
        instance.task_id.clone()
    };
    if let Some(task_id) = task_id {
        queue_task_status_updates(&state, &app.helper, &task_id);
    }
    if let Some(instance) = state.instances.get(&payload.instance_id) {
        tracing::debug!(
            instance_id = payload.instance_id,
            place_id = instance.place_id,
            task_id = ?instance.task_id,
            studio_pid = ?instance.studio_pid,
            edit_runtime_age_ms = instance
                .edit_runtime_observed_at
                .map(|value| value.elapsed().as_millis()),
            "accepted MCP plugin edit heartbeat"
        );
    }
    Ok(StatusCode::NO_CONTENT.into_response())
}

async fn mcp_plugin_response_handler(
    State(app): State<AppState>,
    Json(payload): Json<PluginResponsePayload>,
) -> Result<Response, HelperError> {
    let tx = {
        let mut state = app.state.lock().await;
        let Some(pending_ref) = state.waiting_for_plugin.get(&payload.id) else {
            drop(state);
            tracing::warn!(
                id = payload.id,
                "received late or unknown plugin tool response"
            );
            return Ok(StatusCode::NO_CONTENT.into_response());
        };
        let expected_instance_id = pending_ref.instance_id.clone();
        let pending_tool_name = pending_ref.tool_name.clone();
        let Some(instance_id) = payload.instance_id.as_deref() else {
            drop(state);
            tracing::warn!(
                id = payload.id,
                expected_instance_id,
                tool = pending_tool_name.as_str(),
                "rejected MCP plugin response without instance_id"
            );
            return Ok(StatusCode::NO_CONTENT.into_response());
        };
        if instance_id != expected_instance_id {
            drop(state);
            tracing::warn!(
                id = payload.id,
                instance_id,
                expected_instance_id,
                tool = pending_tool_name.as_str(),
                "rejected MCP plugin response from wrong instance"
            );
            return Ok(StatusCode::NO_CONTENT.into_response());
        }
        if let Some(instance) = state.instances.get(instance_id) {
            if let Some(task_id) = instance.task_id.clone() {
                queue_task_status_updates(&state, &app.helper, &task_id);
            }
            tracing::info!(
                instance_id,
                place_id = instance.place_id,
                task_id = ?instance.task_id,
                id = payload.id,
                tool = pending_tool_name.as_str(),
                success = payload.success,
                response_bytes = payload.response.len(),
                "helper received MCP plugin tool response"
            );
        } else {
            tracing::warn!(
                instance_id,
                id = payload.id,
                tool = pending_tool_name.as_str(),
                success = payload.success,
                response_bytes = payload.response.len(),
                "helper received MCP plugin response for missing instance"
            );
        }
        state
            .waiting_for_plugin
            .remove(&payload.id)
            .map(|pending| pending.response_tx)
    };
    if let Some(tx) = tx {
        let _ = tx.send(payload.clone());
        tracing::info!(
            id = payload.id,
            instance_id = ?payload.instance_id,
            success = payload.success,
            response_bytes = payload.response.len(),
            "helper received plugin tool response"
        );
    }
    Ok(StatusCode::NO_CONTENT.into_response())
}

async fn screenshot_debug_handler(
    State(app): State<AppState>,
    Query(query): Query<ScreenshotQuery>,
) -> Result<Json<ScreenshotResponse>, HelperError> {
    let place_id = match query.place_id.as_deref() {
        Some(value) => Some(sanitize_place_id(value)?),
        None => None,
    };
    let task_id = maybe_sanitize_identifier("task_id", query.task_id.as_deref())?;
    let path = take_screenshot(app, place_id.as_deref(), task_id.as_deref()).await?;
    Ok(Json(ScreenshotResponse { path }))
}

async fn runtime_screenshot_handler(
    State(app): State<AppState>,
    Json(payload): Json<RuntimeScreenshotRequest>,
) -> Result<Json<RuntimeScreenshotResponse>, HelperError> {
    let response = upload_runtime_screenshot(app, payload).await?;
    Ok(Json(response))
}

async fn read_studio_log_debug_handler(
    Query(query): Query<ReadStudioLogArgs>,
) -> Result<Json<ReadStudioLogResponse>, HelperError> {
    Ok(Json(read_studio_log(query)?))
}
