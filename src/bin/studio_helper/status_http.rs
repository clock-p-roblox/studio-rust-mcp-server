async fn helper_status(State(app): State<AppState>) -> Json<HelperStatusResponse> {
    let mut state = app.state.lock().await;
    cleanup_stale_instances(&mut state);
    reconcile_launch_processes(&mut state);
    sync_remote_connections(&app, &mut state);
    let mut active_places: Vec<_> = state
        .instances
        .values()
        .map(|instance| instance.place_id.clone())
        .chain(
            state
                .claimed_tasks
                .values()
                .map(|task| task.place_id.clone()),
        )
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    active_places.sort();
    let mut instances: Vec<_> = state
        .instances
        .iter()
        .map(|(instance_id, instance)| {
            let stop_snapshot = stop_request_snapshot_for_instance(instance);
            HelperInstanceStatus {
                instance_id: instance_id.clone(),
                place_id: instance.place_id.clone(),
                task_id: instance.task_id.clone(),
                remote_base_url: instance.remote_base_url.clone(),
                studio_pid: instance.studio_pid,
                studio_session_state: reported_session_state_for_instance(instance).to_owned(),
                last_known_session_state: last_known_session_state_for_instance(instance)
                    .map(str::to_owned),
                last_session_error_reason: instance.studio_control_last_error.clone(),
                studio_mode: instance.studio_mode.clone(),
                studio_mode_age_ms: instance
                    .studio_mode_observed_at
                    .map(|value| value.elapsed().as_millis()),
                studio_mode_source: instance.studio_mode_source.clone(),
                studio_control_state: effective_studio_control_state(instance),
                studio_transition_phase: effective_studio_transition_phase(instance),
                studio_transition_age_ms: instance
                    .studio_transition_started_at
                    .map(|value| value.elapsed().as_millis()),
                edit_runtime_state: edit_runtime_state(instance),
                edit_runtime_age_ms: instance
                    .edit_runtime_observed_at
                    .map(|value| value.elapsed().as_millis()),
                studio_control_last_error: instance.studio_control_last_error.clone(),
                active_stop_request_id: stop_snapshot.active_stop_request_id,
                last_stop_request_id: stop_snapshot.last_stop_request_id,
                stop_request_recorded_age_ms: stop_snapshot.stop_request_recorded_age_ms,
                runtime_actuator_last_poll_id: stop_snapshot.runtime_actuator_last_poll_id,
                runtime_actuator_last_poll_age_ms: stop_snapshot.runtime_actuator_last_poll_age_ms,
                stop_result_phase: stop_snapshot.stop_result_phase,
                stop_result_age_ms: stop_snapshot.stop_result_age_ms,
                stop_result_error: stop_snapshot.stop_result_error,
            }
        })
        .collect();
    instances.sort_by(|left, right| left.instance_id.cmp(&right.instance_id));
    let mut claimed_tasks: Vec<_> = state
        .claimed_tasks
        .values()
        .map(|task| {
            let connection_key = task_connection_key(&task.task_id);
            let remote_connection = state.remote_connections.get(&connection_key);
            let studio_snapshot = task_studio_mode_snapshot(&state, &task.task_id);
            let (
                official_mcp_adapter_state,
                official_mcp_adapter_age_ms,
                official_mcp_adapter_last_error,
            ) = task_official_adapter_snapshot(
                &state,
                &task.task_id,
                studio_snapshot.edit_runtime_state == "ready",
            );
            let launch_process = state.launch_processes.get(&task.task_id);
            let studio_pid = task_active_studio_pid(&state, &task.task_id);
            let active_launch_process =
                launch_process.filter(|launch| launch_process_is_active_for_status(&state, launch));
            let runtime_log_base_url = task.runtime_log_base_url.as_ref().map(|_| {
                runtime_log_forward_base_url(app.helper.port, &task.place_id, &task.task_id)
            });
            let runtime_log_upload_url = task.runtime_log_base_url.as_ref().map(|_| {
                runtime_log_forward_upload_url(app.helper.port, &task.place_id, &task.task_id)
            });
            let runtime_log_forward = runtime_log_forward_status_snapshot(&state, &task.task_id);
            ClaimedTaskStatus {
                task_id: task.task_id.clone(),
                place_id: task.place_id.clone(),
                universe_id: task.universe_id.clone(),
                mcp_base_url: task.mcp_base_url.clone(),
                rojo_base_url: task.rojo_base_url.clone(),
                runtime_log_base_url,
                runtime_log_upload_url,
                runtime_log_forward,
                studio_pid,
                launched_by_helper: active_launch_process
                    .map(|launch| launch.launched_by_helper)
                    .unwrap_or(false),
                claimed_age_ms: task.claimed_at.elapsed().as_millis(),
                remote_state: remote_connection
                    .map(|connection| match connection.connection_state {
                        RemoteConnectionState::Connecting => "connecting",
                        RemoteConnectionState::Connected => "connected",
                        RemoteConnectionState::Retrying => "retrying",
                        RemoteConnectionState::Error => "error",
                    })
                    .unwrap_or("disconnected")
                    .to_owned(),
                remote_connection_id: remote_connection
                    .and_then(|connection| connection.connection_id.clone()),
                remote_last_error: state.last_remote_errors.get(&connection_key).cloned(),
                remote_last_ready_age_ms: remote_connection.and_then(|connection| {
                    connection
                        .last_ready_at
                        .map(|value| value.elapsed().as_millis())
                }),
                remote_last_server_message_age_ms: remote_connection.and_then(|connection| {
                    connection
                        .last_server_message_at
                        .map(|value| value.elapsed().as_millis())
                }),
                studio_session_state: studio_snapshot.studio_session_state,
                last_known_session_state: studio_snapshot.last_known_session_state,
                last_session_error_reason: studio_snapshot.last_session_error_reason,
                studio_mode: studio_snapshot.studio_mode,
                studio_mode_age_ms: studio_snapshot.studio_mode_age_ms,
                studio_mode_source: studio_snapshot.studio_mode_source,
                studio_control_state: studio_snapshot.studio_control_state,
                studio_transition_phase: studio_snapshot.studio_transition_phase,
                studio_transition_age_ms: studio_snapshot.studio_transition_age_ms,
                edit_runtime_state: studio_snapshot.edit_runtime_state,
                edit_runtime_age_ms: studio_snapshot.edit_runtime_age_ms,
                studio_control_last_error: studio_snapshot.studio_control_last_error,
                active_stop_request_id: studio_snapshot.active_stop_request_id,
                last_stop_request_id: studio_snapshot.last_stop_request_id,
                stop_request_recorded_age_ms: studio_snapshot.stop_request_recorded_age_ms,
                runtime_actuator_last_poll_id: studio_snapshot.runtime_actuator_last_poll_id,
                runtime_actuator_last_poll_age_ms: studio_snapshot
                    .runtime_actuator_last_poll_age_ms,
                stop_result_phase: studio_snapshot.stop_result_phase,
                stop_result_age_ms: studio_snapshot.stop_result_age_ms,
                stop_result_error: studio_snapshot.stop_result_error,
                official_mcp_adapter_state,
                official_mcp_adapter_age_ms,
                official_mcp_adapter_last_error,
            }
        })
        .collect();
    claimed_tasks.sort_by(|left, right| left.task_id.cmp(&right.task_id));
    let mut launched_studios: Vec<_> = state
        .launch_processes
        .values()
        .filter(|launch| launch_process_is_active_for_status(&state, launch))
        .map(|launch| LaunchProcessStatus {
            task_id: launch.task_id.clone(),
            place_id: launch.place_id.clone(),
            universe_id: launch.universe_id.clone(),
            studio_pid: launch.studio_pid,
            launched_by_helper: launch.launched_by_helper,
        })
        .collect();
    launched_studios.sort_by(|left, right| left.task_id.cmp(&right.task_id));
    let mut place_ids: Vec<_> = state
        .instances
        .values()
        .map(|instance| instance.place_id.clone())
        .chain(
            state
                .claimed_tasks
                .values()
                .map(|task| task.place_id.clone()),
        )
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    place_ids.sort();
    let mut place_statuses = Vec::with_capacity(place_ids.len());
    for place_id in place_ids {
        let claimed_tasks_for_place = state
            .claimed_tasks
            .values()
            .filter(|task| task.place_id == place_id)
            .cloned()
            .collect::<Vec<_>>();
        let claimed_task = if claimed_tasks_for_place.len() == 1 {
            claimed_tasks_for_place.into_iter().next()
        } else {
            None
        };
        let mut registered_instance_ids = Vec::new();
        let mut studio_pids = Vec::new();
        for (instance_id, instance) in &state.instances {
            if instance.place_id == place_id {
                registered_instance_ids.push(instance_id.clone());
                if let Some(studio_pid) = instance.studio_pid {
                    studio_pids.push(studio_pid);
                }
            }
        }
        if let Some(claimed_task) = claimed_task.as_ref() {
            if let Some(launch) = state.launch_processes.get(&claimed_task.task_id) {
                if launch_process_is_active_for_status(&state, launch) {
                    studio_pids.push(launch.studio_pid);
                }
            }
        }
        registered_instance_ids.sort();
        studio_pids.sort();
        studio_pids.dedup();
        let connection_key = claimed_task
            .as_ref()
            .map(|task| task_connection_key(&task.task_id));
        let remote_connection = connection_key
            .as_ref()
            .and_then(|key| state.remote_connections.get(key));
        let runtime_log_base_url = claimed_task.as_ref().and_then(|task| {
            task.runtime_log_base_url.as_ref().map(|_| {
                runtime_log_forward_base_url(app.helper.port, &task.place_id, &task.task_id)
            })
        });
        let runtime_log_upload_url = claimed_task.as_ref().and_then(|task| {
            task.runtime_log_base_url.as_ref().map(|_| {
                runtime_log_forward_upload_url(app.helper.port, &task.place_id, &task.task_id)
            })
        });
        let runtime_screenshot_upload_url = claimed_task.as_ref().and_then(|task| {
            task.runtime_log_base_url.as_ref().map(|_| {
                runtime_screenshot_helper_upload_url(app.helper.port, &task.place_id, &task.task_id)
            })
        });
        let runtime_log_forward = claimed_task
            .as_ref()
            .map(|task| runtime_log_forward_status_snapshot(&state, &task.task_id));
        place_statuses.push(HelperPlaceStatus {
            remote_base_url: claimed_task
                .as_ref()
                .map(|task| task.mcp_base_url.clone())
                .unwrap_or_default(),
            place_id: place_id.clone(),
            task_id: claimed_task.as_ref().map(|task| task.task_id.clone()),
            runtime_log_base_url,
            runtime_log_upload_url,
            runtime_screenshot_upload_url,
            runtime_log_forward,
            registered_instance_count: registered_instance_ids.len(),
            registered_instance_ids,
            studio_pids,
            remote_state: remote_connection
                .map(|connection| match connection.connection_state {
                    RemoteConnectionState::Connecting => "connecting",
                    RemoteConnectionState::Connected => "connected",
                    RemoteConnectionState::Retrying => "retrying",
                    RemoteConnectionState::Error => "error",
                })
                .unwrap_or("disconnected")
                .to_owned(),
            remote_connection_id: remote_connection
                .and_then(|connection| connection.connection_id.clone()),
            remote_last_error: connection_key
                .as_ref()
                .and_then(|key| state.last_remote_errors.get(key).cloned()),
            remote_last_ready_age_ms: remote_connection.and_then(|connection| {
                connection
                    .last_ready_at
                    .map(|value| value.elapsed().as_millis())
            }),
            remote_last_server_message_age_ms: remote_connection.and_then(|connection| {
                connection
                    .last_server_message_at
                    .map(|value| value.elapsed().as_millis())
            }),
        });
    }
    Json(HelperStatusResponse {
        helper_port: app.helper.port,
        helper_id: app.helper.helper_id.clone(),
        user_name: app.helper.user_name.clone(),
        capacity: app.helper.capacity,
        hub_base_url: app.helper.hub_base_url.clone(),
        hub_last_error: state.hub_last_error.clone(),
        hub_last_claim_error: state.hub_last_claim_error.clone(),
        hub_last_ready_age_ms: state
            .hub_last_ready_at
            .map(|value| value.elapsed().as_millis()),
        active_instances: state.instances.len(),
        claimed_task_count: claimed_tasks.len(),
        active_places,
        claimed_tasks,
        launched_studios,
        instances,
        place_statuses,
        last_remote_errors: state.last_remote_errors.clone(),
    })
}

async fn helper_debug_task_handler(
    State(app): State<AppState>,
    AxumPath(task_id): AxumPath<String>,
) -> Result<Response, HelperError> {
    let mut state = app.state.lock().await;
    cleanup_stale_instances(&mut state);
    reconcile_launch_processes(&mut state);
    sync_remote_connections(&app, &mut state);
    let Some(task) = state.claimed_tasks.get(&task_id) else {
        return Ok((StatusCode::NOT_FOUND, "task not claimed by helper").into_response());
    };
    let connection_key = task_connection_key(&task.task_id);
    let remote_connection = state.remote_connections.get(&connection_key);
    let launch_process = state.launch_processes.get(&task.task_id);
    let instances = state
        .instances
        .iter()
        .filter(|(_, instance)| instance.task_id.as_deref() == Some(task_id.as_str()))
        .map(|(instance_id, instance)| {
            serde_json::json!({
                "instance_id": instance_id,
                "place_id": instance.place_id,
                "task_id": instance.task_id,
                "remote_base_url": instance.remote_base_url,
                "studio_pid": instance.studio_pid,
                "studio_mode": instance.studio_mode,
                "studio_mode_age_ms": instance.studio_mode_observed_at.map(|value| value.elapsed().as_millis()),
                "studio_mode_source": instance.studio_mode_source,
                "studio_control_state": effective_studio_control_state(instance),
                "studio_transition_phase": effective_studio_transition_phase(instance),
                "studio_transition_age_ms": instance.studio_transition_started_at.map(|value| value.elapsed().as_millis()),
                "edit_runtime_state": edit_runtime_state(instance),
                "edit_runtime_age_ms": instance.edit_runtime_observed_at.map(|value| value.elapsed().as_millis()),
                "studio_control_last_error": instance.studio_control_last_error,
            })
        })
        .collect::<Vec<_>>();
    let task_status = helper_task_status_snapshot(&state, &task.task_id);
    let runtime_log_base_url = task
        .runtime_log_base_url
        .as_ref()
        .map(|_| runtime_log_forward_base_url(app.helper.port, &task.place_id, &task.task_id));
    let runtime_log_upload_url = task
        .runtime_log_base_url
        .as_ref()
        .map(|_| runtime_log_forward_upload_url(app.helper.port, &task.place_id, &task.task_id));
    let runtime_log_forward = runtime_log_forward_status_snapshot(&state, &task.task_id);
    let payload = serde_json::json!({
        "ok": true,
        "task_id": task.task_id,
        "claimed_task": {
            "task_id": task.task_id,
            "place_id": task.place_id,
            "universe_id": task.universe_id,
            "mcp_base_url": task.mcp_base_url,
            "rojo_base_url": task.rojo_base_url,
            "runtime_log_base_url": runtime_log_base_url,
            "runtime_log_upload_url": runtime_log_upload_url,
            "claimed_age_ms": task.claimed_at.elapsed().as_millis(),
        },
        "launch_process": launch_process.map(|launch| serde_json::json!({
            "task_id": launch.task_id,
            "place_id": launch.place_id,
            "universe_id": launch.universe_id,
            "studio_pid": launch.studio_pid,
            "launched_by_helper": launch.launched_by_helper,
        })),
        "remote_connection": remote_connection.map(|connection| serde_json::json!({
            "state": remote_connection_state_name(Some(connection)),
            "connection_id": connection.connection_id,
            "last_ready_age_ms": connection.last_ready_at.map(|value| value.elapsed().as_millis()),
            "last_server_message_age_ms": connection.last_server_message_at.map(|value| value.elapsed().as_millis()),
            "last_error": state.last_remote_errors.get(&connection_key),
        })),
        "runtime_log_forward": runtime_log_forward,
        "instances": instances,
        "task_status": task_status,
    });
    Ok(Json(payload).into_response())
}
