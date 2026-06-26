async fn handle_remote_command(
    app: AppState,
    place_id: &str,
    task_id: Option<&str>,
    command: Value,
    sender: mpsc::UnboundedSender<RemoteOutgoingFrame>,
    upload_waiters: Arc<Mutex<HashMap<String, oneshot::Sender<Result<ArtifactCommitted>>>>>,
    request_id: String,
) -> Result<String> {
    let (tool_name, payload) = extract_tool_name_and_args(&command)?;
    tracing::info!(
        place_id,
        task_id,
        tool = tool_name,
        "helper received remote MCP command"
    );
    match tool_name.as_str() {
        "TakeScreenshot" | "take_screenshot" => {
            let args: TakeScreenshotToolArgs = serde_json::from_value(payload)?;
            stream_runtime_screenshot(
                app,
                place_id,
                task_id,
                args,
                sender,
                upload_waiters,
                request_id,
            )
            .await
        }
        "ReadStudioLog" | "read_studio_log" => {
            let args: ReadStudioLogArgs = serde_json::from_value(payload)?;
            Ok(serde_json::to_string(&read_studio_log(args)?)?)
        }
        "StartStopPlay" | "start_stop_play" => {
            let args: StartStopPlayCommandArgs = serde_json::from_value(payload)?;
            if args.mode != "stop" {
                return Err(eyre!(
                    "start_stop_play is stop-only. Use launch_studio_session to enter start_play or run_server."
                ));
            }
            handle_start_stop_play_stop(app, place_id, task_id).await
        }
        "LaunchStudioSession" | "launch_studio_session" => {
            let args: LaunchStudioSessionCommandArgs = serde_json::from_value(payload)?;
            validate_launch_studio_session_mode(&args.mode)?;
            let live_state = ensure_live_state_allows_launch(app.clone(), place_id, task_id).await?;
            forward_launch_to_plugin(
                app,
                place_id,
                task_id,
                command,
                live_state.instance_id.as_deref(),
            )
            .await
        }
        _ => forward_to_plugin(app, place_id, task_id, command).await,
    }
}

async fn handle_start_stop_play_stop(
    app: AppState,
    place_id: &str,
    task_id: Option<&str>,
) -> Result<String> {
    let live_state = query_live_studio_session_state(app.clone(), place_id, task_id).await?;
    match live_state.state.as_str() {
        "stop" => {
            return Ok(serde_json::json!({
                "success": true,
                "reason": "already_stopped",
                "studio_session_state": "stop",
                "message": "Stopped"
            })
            .to_string());
        }
        "play" => {}
        "stopping" => {
            return Err(eyre!(
                "studio_stop_in_progress: Studio stop is already in progress"
            ));
        }
        "starting_play" => {
            return Err(eyre!(
                "studio_starting_play: Studio is still entering play mode"
            ));
        }
        "none_connected" => {
            return Err(eyre!(
                "{}",
                live_state
                    .reason
                    .as_deref()
                    .unwrap_or("studio_plugin_not_connected")
            ));
        }
        "none_response" => {
            return Err(eyre!(
                "{}",
                live_state
                    .reason
                    .as_deref()
                    .unwrap_or("studio_plugin_no_response")
            ));
        }
        other => {
            return Err(eyre!(
                "studio_session_state_not_stoppable: current_state={other}"
            ));
        }
    }

    let (instance_id, stop_request_id) = {
        let mut state = app.state.lock().await;
        cleanup_stale_instances(&mut state);
        sync_remote_connections(&app, &mut state);
        let instance_id = live_state
            .instance_id
            .clone()
            .or_else(|| select_instance_for_route(place_id, task_id, &state.instances))
            .ok_or_else(|| eyre!("studio_plugin_not_connected: no active Studio plugin registered for placeId {place_id}"))?;
        if !state.instances.contains_key(&instance_id) {
            return Err(eyre!(
                "studio_plugin_not_connected: live query selected plugin instance {instance_id}, but it is no longer registered"
            ));
        };
        let recorded = match record_stop_request_for_instance(&mut state, &instance_id) {
            Ok(recorded) => recorded,
            Err(StopRequestRecordError::MissingInstance) => {
                return Err(eyre!(
                    "studio_plugin_not_connected: selected plugin instance disappeared before stop request"
                ));
            }
            Err(StopRequestRecordError::RuntimeActuatorUnavailable(detail)) => {
                return Err(eyre!(
                    "runtime_stop_actuator_unavailable: instance_id={instance_id}; live_state=play; {detail}"
                ));
            }
        };
        if let Some(task_id) = recorded.task_id.as_deref() {
            queue_task_status_updates(&state, &app.helper, task_id);
        }
        (instance_id, recorded.stop_request_id)
    };

    tracing::info!(
        instance_id,
        place_id,
        task_id,
        stop_request_id,
        "recorded runtime actuator stop request after plugin live state reported play"
    );
    wait_for_runtime_actuator_stop(&app, &instance_id, stop_request_id).await?;
    Ok("Stopped".to_owned())
}

async fn ensure_live_state_allows_launch(
    app: AppState,
    place_id: &str,
    task_id: Option<&str>,
) -> Result<LiveStudioSessionState> {
    let live_state = query_live_studio_session_state(app, place_id, task_id).await?;
    match live_state.state.as_str() {
        "stop" => Ok(live_state),
        "play" => Err(eyre!(
            "studio_already_playing: Studio is already in play/run mode"
        )),
        "starting_play" => Err(eyre!(
            "launch_already_in_progress: Studio is already entering play/run mode"
        )),
        "stopping" => Err(eyre!(
            "studio_stop_in_progress: Studio stop is already in progress"
        )),
        "none_connected" => Err(eyre!(
            "{}",
            live_state
                .reason
                .as_deref()
                .unwrap_or("studio_plugin_not_connected")
        )),
        "none_response" => Err(eyre!(
            "{}",
            live_state
                .reason
                .as_deref()
                .unwrap_or("studio_plugin_no_response")
        )),
        other => Err(eyre!(
            "studio_session_state_not_launchable: current_state={other}"
        )),
    }
}

async fn query_live_studio_session_state(
    app: AppState,
    place_id: &str,
    task_id: Option<&str>,
) -> Result<LiveStudioSessionState> {
    {
        let mut state = app.state.lock().await;
        cleanup_stale_instances(&mut state);
        sync_remote_connections(&app, &mut state);
        if select_instance_for_route(place_id, task_id, &state.instances).is_none() {
            return Ok(LiveStudioSessionState {
                instance_id: None,
                state: "none_connected".to_owned(),
                reason: Some(format!(
                    "studio_plugin_not_connected: no active Studio plugin registered for placeId {place_id}"
                )),
            });
        }
    }

    let request_id = Uuid::new_v4().to_string();
    let command = serde_json::json!({
        "id": request_id,
        "args": {
            "GetStudioSessionState": {}
        }
    });
    let (instance_id, response) =
        match forward_to_plugin_raw(app, place_id, task_id, command, None, None).await {
            Ok(result) => result,
            Err(error) => {
                return Ok(LiveStudioSessionState {
                    instance_id: None,
                    state: "none_response".to_owned(),
                    reason: Some(format!("studio_plugin_no_response: {error}")),
                });
            }
        };
    if !response.success {
        return Ok(LiveStudioSessionState {
            instance_id: Some(instance_id),
            state: "none_response".to_owned(),
            reason: Some(format!("studio_plugin_no_response: {}", response.response)),
        });
    }
    let payload: PluginStudioSessionStatePayload = match serde_json::from_str(&response.response) {
        Ok(payload) => payload,
        Err(error) => {
            return Ok(LiveStudioSessionState {
                instance_id: Some(instance_id),
                state: "none_response".to_owned(),
                reason: Some(format!(
                    "studio_plugin_no_response: invalid GetStudioSessionState payload: {error}"
                )),
            });
        }
    };
    if let Some(expected_task_id) = task_id {
        match payload.task_id.as_deref() {
            Some(actual) if actual == expected_task_id => {}
            _ => {
                return Ok(LiveStudioSessionState {
                    instance_id: Some(instance_id),
                    state: "none_response".to_owned(),
                    reason: Some(format!(
                        "studio_plugin_no_response: plugin live state task_id mismatch, expected {expected_task_id}, got {:?}",
                        payload.task_id
                    )),
                });
            }
        }
    }
    match payload.instance_id.as_deref() {
        Some(reported) if reported == instance_id => {}
        _ => {
            return Ok(LiveStudioSessionState {
                instance_id: Some(instance_id.clone()),
                state: "none_response".to_owned(),
                reason: Some(format!(
                    "studio_plugin_no_response: plugin live state instance_id mismatch, expected {instance_id}, got {:?}",
                    payload.instance_id
                )),
            });
        }
    }
    let live_state = payload.studio_session_state;
    if !matches!(
        live_state.as_str(),
        "stop" | "starting_play" | "play" | "stopping"
    ) {
        return Ok(LiveStudioSessionState {
            instance_id: Some(instance_id),
            state: "none_response".to_owned(),
            reason: Some(format!(
                "studio_plugin_no_response: invalid studio_session_state {live_state}"
            )),
        });
    }
    Ok(LiveStudioSessionState {
        instance_id: Some(instance_id),
        state: live_state,
        reason: None,
    })
}

async fn wait_for_runtime_actuator_stop(
    app: &AppState,
    instance_id: &str,
    stop_request_id: u64,
) -> Result<()> {
    let deadline = Instant::now() + PLUGIN_STUDIO_CONTROL_REQUEST_TIMEOUT;
    loop {
        {
            let state = app.state.lock().await;
            let Some(instance) = state.instances.get(instance_id) else {
                return Err(eyre!(
                    "runtime_stop_instance_disappeared: instance_id={instance_id} stop_request_id={stop_request_id}"
                ));
            };
            if instance.stop_request_id > stop_request_id {
                return Err(eyre!(
                    "runtime_stop_superseded: instance_id={instance_id} stop_request_id={stop_request_id} current_stop_request_id={}",
                    instance.stop_request_id
                ));
            }
            if instance.stop_request_id == stop_request_id {
                if instance.stop_result_phase.as_deref() == Some("failed") {
                    return Err(eyre!(
                        "{}",
                        instance
                            .stop_result_error
                            .as_deref()
                            .unwrap_or("runtime_stop_failed")
                    ));
                }
                if instance.stop_result_phase.as_deref() == Some("completed")
                    && effective_studio_transition_phase(instance) == "idle"
                    && edit_runtime_state(instance) == "ready"
                {
                    return Ok(());
                }
            }
        }
        if Instant::now() >= deadline {
            let mut state = app.state.lock().await;
            let mut status_task_id: Option<String> = None;
            let detail = state
                .instances
                .get_mut(instance_id)
                .map(|instance| {
                    let detail = format!(
                        "phase={} stop_result_phase={} runtime_actuator_last_poll_id={:?} runtime_actuator_last_poll_age_ms={:?} edit_runtime_state={}",
                        effective_studio_transition_phase(instance),
                        instance.stop_result_phase.as_deref().unwrap_or("none"),
                        instance.stop_request_last_poll_id,
                        instance.stop_request_last_polled_at.map(|value| value.elapsed().as_millis()),
                        edit_runtime_state(instance)
                    );
                    if instance.stop_request_id == stop_request_id {
                        mark_runtime_stop_timeout(instance, stop_request_id, detail.clone());
                        status_task_id = instance.task_id.clone();
                    }
                    detail
                })
                .unwrap_or_else(|| "instance_missing".to_owned());
            if let Some(task_id) = status_task_id.as_deref() {
                queue_task_status_updates(&state, &app.helper, task_id);
            }
            return Err(eyre!(
                "runtime_stop_timeout: stop_request_id={stop_request_id} {detail}"
            ));
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

fn validate_launch_studio_session_mode(mode: &str) -> Result<()> {
    if mode == "start_play" || mode == "run_server" {
        return Ok(());
    }
    Err(eyre!(
        "Invalid mode in LaunchStudioSession, must be start_play or run_server"
    ))
}

async fn forward_to_plugin(
    app: AppState,
    place_id: &str,
    task_id: Option<&str>,
    command: Value,
) -> Result<String> {
    let (_, response) = forward_to_plugin_raw(app, place_id, task_id, command, None, None).await?;
    if response.success {
        Ok(response.response)
    } else {
        Err(eyre!(response.response))
    }
}

async fn forward_launch_to_plugin(
    app: AppState,
    place_id: &str,
    task_id: Option<&str>,
    command: Value,
    expected_instance_id: Option<&str>,
) -> Result<String> {
    let (instance_id, response) = forward_to_plugin_raw(
        app.clone(),
        place_id,
        task_id,
        command,
        Some("launching"),
        expected_instance_id,
    )
    .await?;
    {
        let mut state = app.state.lock().await;
        let status_task_id = if let Some(instance) = state.instances.get_mut(&instance_id) {
            if response.success {
                set_edit_runtime_unavailable(instance, "runtime");
            } else {
                set_edit_runtime_unavailable(instance, "launch_failed");
            }
            instance.task_id.clone()
        } else {
            None
        };
        if let Some(task_id) = status_task_id.as_deref() {
            queue_task_status_updates(&state, &app.helper, task_id);
        }
    }
    if response.success {
        Ok(response.response)
    } else {
        Err(eyre!(response.response))
    }
}

async fn forward_to_plugin_raw(
    app: AppState,
    place_id: &str,
    task_id: Option<&str>,
    command: Value,
    edit_unavailable_reason: Option<&str>,
    expected_instance_id: Option<&str>,
) -> Result<(String, PluginResponsePayload)> {
    let request_id = extract_command_id(&command)?;
    let (tool_name, _) = extract_tool_name_and_args(&command)?;
    let (instance_id, notify, status_task_id) = {
        let mut state = app.state.lock().await;
        cleanup_stale_instances(&mut state);
        sync_remote_connections(&app, &mut state);
        let selected_instance_id = if let Some(expected_instance_id) = expected_instance_id {
            let instance = state.instances.get(expected_instance_id).ok_or_else(|| {
                eyre!("studio_plugin_not_connected: selected plugin instance {expected_instance_id} disappeared before command dispatch")
            })?;
            if instance.place_id != place_id || !route_identity_matches(instance, task_id) {
                return Err(eyre!(
                    "studio_plugin_identity_mismatch: selected plugin instance {expected_instance_id} no longer matches place/task route"
                ));
            }
            expected_instance_id.to_owned()
        } else {
            select_instance_for_route(place_id, task_id, &state.instances)
                .ok_or_else(|| eyre!("no active Studio plugin registered for placeId {place_id}"))?
        };
        let instance = state
            .instances
            .get_mut(&selected_instance_id)
            .ok_or_else(|| eyre!("selected helper instance disappeared"))?;
        if let Some(reason) = edit_unavailable_reason {
            set_edit_runtime_unavailable(instance, reason);
        }
        let status_task_id = instance.task_id.clone();
        instance.queue.push_back(command);
        let notify = Arc::clone(&instance.notify);
        let (tx, rx) = oneshot::channel();
        state.waiting_for_plugin.insert(
            request_id.clone(),
            PendingPluginResponse {
                instance_id: selected_instance_id.clone(),
                tool_name: tool_name.clone(),
                response_tx: tx,
            },
        );
        (selected_instance_id, (notify, rx), status_task_id)
    };
    if edit_unavailable_reason.is_some() {
        if let Some(task_id) = status_task_id.as_deref() {
            let state = app.state.lock().await;
            queue_task_status_updates(&state, &app.helper, task_id);
        }
    }
    notify.0.notify_waiters();
    tracing::info!(
        instance_id,
        place_id,
        task_id,
        id = request_id,
        tool = tool_name,
        "forwarded MCP command from helper to local plugin"
    );
    let request_timeout = plugin_request_timeout_for_tool(&tool_name);
    let response = match tokio::time::timeout(request_timeout, notify.1).await {
        Ok(result) => result?,
        Err(_) => {
            let mut state = app.state.lock().await;
            state.waiting_for_plugin.remove(&request_id);
            return Err(eyre!(
                "timed out waiting {}ms for local plugin response to {tool_name}",
                request_timeout.as_millis()
            ));
        }
    };
    Ok((instance_id, response))
}

fn cleanup_stale_instances(state: &mut HelperState) {
    let waiting_instance_ids: HashSet<String> = state
        .waiting_for_plugin
        .values()
        .map(|pending| pending.instance_id.clone())
        .collect();
    state.instances.retain(|instance_id, instance| {
        let has_pending_response = waiting_instance_ids.contains(instance_id);
        let keep = has_pending_response || instance.last_seen_at.elapsed() <= INSTANCE_STALE_AFTER;
        if !keep {
            tracing::warn!(
                instance_id,
                place_id = instance.place_id,
                "dropping stale helper plugin instance"
            );
        } else if has_pending_response && instance.last_seen_at.elapsed() > INSTANCE_STALE_AFTER {
            tracing::debug!(
                instance_id,
                place_id = instance.place_id,
                "keeping stale helper plugin instance while waiting for tool response"
            );
        }
        keep
    });
}

fn select_instance_for_place(
    place_id: &str,
    instances: &HashMap<String, PluginInstance>,
) -> Option<String> {
    instances
        .iter()
        .filter(|(_, instance)| instance.place_id == place_id)
        .max_by_key(|(_, instance)| instance.last_seen_at)
        .map(|(instance_id, _)| instance_id.clone())
}

fn select_instance_for_route(
    place_id: &str,
    task_id: Option<&str>,
    instances: &HashMap<String, PluginInstance>,
) -> Option<String> {
    let exact = instances
        .iter()
        .filter(|(_, instance)| {
            instance.place_id == place_id && route_identity_matches(instance, task_id)
        })
        .max_by_key(|(_, instance)| instance.last_seen_at)
        .map(|(instance_id, _)| instance_id.clone());
    if exact.is_some() {
        return exact;
    }
    if task_id.is_some() {
        return None;
    }
    select_instance_for_place(place_id, instances)
}

#[cfg(test)]
fn select_runtime_actuator_for_route(
    place_id: &str,
    task_id: Option<&str>,
    instances: &HashMap<String, PluginInstance>,
) -> Option<String> {
    instances
        .iter()
        .filter(|(_, instance)| {
            instance.place_id == place_id
                && route_identity_matches(instance, task_id)
                && instance_has_fresh_control(instance)
        })
        .max_by_key(|(_, instance)| instance.studio_control_observed_at)
        .map(|(instance_id, _)| instance_id.clone())
}

#[cfg(test)]
fn instance_is_known_runtime_session(instance: &PluginInstance) -> bool {
    instance.edit_runtime_unavailable_reason.as_deref() == Some("runtime")
        || instance.edit_runtime_unavailable_reason.as_deref() == Some("launch_failed")
        || instance.edit_runtime_unavailable_reason.as_deref() == Some("launching")
        || matches!(
            instance.studio_mode.as_deref(),
            Some("start_play") | Some("run_server")
        )
        || instance.studio_transition_phase == "running"
        || instance_has_fresh_control(instance)
}

#[cfg(test)]
fn select_known_runtime_session_for_route(
    place_id: &str,
    task_id: Option<&str>,
    instances: &HashMap<String, PluginInstance>,
) -> Option<String> {
    instances
        .iter()
        .filter(|(_, instance)| {
            instance.place_id == place_id
                && route_identity_matches(instance, task_id)
                && instance_is_known_runtime_session(instance)
        })
        .max_by_key(|(_, instance)| instance.last_seen_at)
        .map(|(instance_id, _)| instance_id.clone())
}

#[cfg(test)]
fn select_active_stop_request_for_route(
    place_id: &str,
    task_id: Option<&str>,
    instances: &HashMap<String, PluginInstance>,
) -> Option<(String, u64)> {
    instances
        .iter()
        .filter_map(|(instance_id, instance)| {
            if instance.place_id != place_id || !route_identity_matches(instance, task_id) {
                return None;
            }
            active_stop_request_id_for_instance(instance).map(|stop_request_id| {
                (instance_id.clone(), stop_request_id, instance.last_seen_at)
            })
        })
        .max_by_key(|(_, stop_request_id, last_seen_at)| (*stop_request_id, *last_seen_at))
        .map(|(instance_id, stop_request_id, _)| (instance_id, stop_request_id))
}
