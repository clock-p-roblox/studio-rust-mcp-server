fn ws_age_ms(value: Option<u128>) -> Option<u64> {
    value.map(|age| age.min(u64::MAX as u128) as u64)
}

fn runtime_log_forward_status_snapshot(
    state: &HelperState,
    task_id: &str,
) -> RuntimeLogForwardStatusSnapshot {
    let record = state
        .runtime_log_forward_statuses
        .get(task_id)
        .cloned()
        .unwrap_or_default();
    let state_name = if record.queued_count > 0 {
        "forwarding"
    } else if record.last_error.is_some() {
        "error"
    } else if record.forwarded_count > 0 {
        "ready"
    } else {
        "idle"
    };
    RuntimeLogForwardStatusSnapshot {
        state: state_name.to_owned(),
        queued_count: record.queued_count,
        accepted_count: record.accepted_count,
        forwarded_count: record.forwarded_count,
        failed_count: record.failed_count,
        last_accepted_age_ms: ws_age_ms(
            record
                .last_accepted_at
                .map(|value| value.elapsed().as_millis()),
        ),
        last_forwarded_age_ms: ws_age_ms(
            record
                .last_forwarded_at
                .map(|value| value.elapsed().as_millis()),
        ),
        last_attempt_age_ms: ws_age_ms(
            record
                .last_attempt_at
                .map(|value| value.elapsed().as_millis()),
        ),
        last_target_path: record.last_target_path,
        last_http_status: record.last_http_status,
        last_error: record.last_error,
        last_error_age_ms: ws_age_ms(
            record
                .last_error_at
                .map(|value| value.elapsed().as_millis()),
        ),
    }
}

fn mark_runtime_log_forward_accepted(state: &mut HelperState, task_id: &str, target_path: String) {
    let record = state
        .runtime_log_forward_statuses
        .entry(task_id.to_owned())
        .or_default();
    record.queued_count = record.queued_count.saturating_add(1);
    record.accepted_count = record.accepted_count.saturating_add(1);
    record.last_accepted_at = Some(Instant::now());
    record.last_target_path = Some(target_path);
    record.last_http_status = None;
}

fn mark_runtime_log_forward_succeeded(
    state: &mut HelperState,
    task_id: &str,
    target_path: String,
    http_status: u16,
) {
    let record = state
        .runtime_log_forward_statuses
        .entry(task_id.to_owned())
        .or_default();
    record.queued_count = record.queued_count.saturating_sub(1);
    record.forwarded_count = record.forwarded_count.saturating_add(1);
    record.last_forwarded_at = Some(Instant::now());
    record.last_attempt_at = record.last_forwarded_at;
    record.last_target_path = Some(target_path);
    record.last_http_status = Some(http_status);
    record.last_error = None;
    record.last_error_at = None;
}

fn mark_runtime_log_forward_failed(
    state: &mut HelperState,
    task_id: &str,
    target_path: String,
    http_status: Option<u16>,
    error: String,
) {
    let record = state
        .runtime_log_forward_statuses
        .entry(task_id.to_owned())
        .or_default();
    record.queued_count = record.queued_count.saturating_sub(1);
    record.failed_count = record.failed_count.saturating_add(1);
    record.last_attempt_at = Some(Instant::now());
    record.last_target_path = Some(target_path);
    record.last_http_status = http_status;
    record.last_error = Some(error);
    record.last_error_at = record.last_attempt_at;
}

fn runtime_log_forward_task_is_current(
    state: &HelperState,
    task_id: &str,
    place_id: &str,
    claimed_at: Instant,
) -> bool {
    state
        .claimed_tasks
        .get(task_id)
        .map(|task| task.place_id == place_id && task.claimed_at == claimed_at)
        .unwrap_or(false)
}

fn mark_runtime_log_forward_succeeded_if_current(
    state: &mut HelperState,
    task_id: &str,
    place_id: &str,
    claimed_at: Instant,
    target_path: String,
    http_status: u16,
) -> bool {
    if !runtime_log_forward_task_is_current(state, task_id, place_id, claimed_at) {
        return false;
    }
    mark_runtime_log_forward_succeeded(state, task_id, target_path, http_status);
    true
}

fn mark_runtime_log_forward_failed_if_current(
    state: &mut HelperState,
    task_id: &str,
    place_id: &str,
    claimed_at: Instant,
    target_path: String,
    http_status: Option<u16>,
    error: String,
) -> bool {
    if !runtime_log_forward_task_is_current(state, task_id, place_id, claimed_at) {
        return false;
    }
    mark_runtime_log_forward_failed(state, task_id, target_path, http_status, error);
    true
}

fn mark_runtime_log_forward_rejected(
    state: &mut HelperState,
    task_id: &str,
    target_path: String,
    error: String,
) {
    let record = state
        .runtime_log_forward_statuses
        .entry(task_id.to_owned())
        .or_default();
    record.failed_count = record.failed_count.saturating_add(1);
    record.last_attempt_at = Some(Instant::now());
    record.last_target_path = Some(target_path);
    record.last_http_status = None;
    record.last_error = Some(error);
    record.last_error_at = record.last_attempt_at;
}

fn helper_task_status_snapshot(state: &HelperState, task_id: &str) -> HelperTaskStatusSnapshot {
    let studio_snapshot = task_studio_mode_snapshot(state, task_id);
    let (official_state, official_age_ms, official_last_error) = task_official_adapter_snapshot(
        state,
        task_id,
        studio_snapshot.edit_runtime_state == "ready",
    );
    HelperTaskStatusSnapshot {
        task_id: task_id.to_owned(),
        studio_session_state: Some(studio_snapshot.studio_session_state),
        last_known_session_state: studio_snapshot.last_known_session_state,
        last_session_error_reason: studio_snapshot.last_session_error_reason,
        studio_mode: studio_snapshot.studio_mode,
        studio_mode_age_ms: ws_age_ms(studio_snapshot.studio_mode_age_ms),
        studio_mode_source: Some(studio_snapshot.studio_mode_source),
        studio_control_state: Some(studio_snapshot.studio_control_state),
        studio_transition_phase: Some(studio_snapshot.studio_transition_phase),
        studio_transition_age_ms: ws_age_ms(studio_snapshot.studio_transition_age_ms),
        edit_runtime_state: Some(studio_snapshot.edit_runtime_state),
        edit_runtime_age_ms: ws_age_ms(studio_snapshot.edit_runtime_age_ms),
        studio_control_last_error: studio_snapshot.studio_control_last_error,
        active_stop_request_id: studio_snapshot.active_stop_request_id,
        last_stop_request_id: Some(studio_snapshot.last_stop_request_id),
        stop_request_recorded_age_ms: ws_age_ms(studio_snapshot.stop_request_recorded_age_ms),
        runtime_actuator_last_poll_id: studio_snapshot.runtime_actuator_last_poll_id,
        runtime_actuator_last_poll_age_ms: ws_age_ms(
            studio_snapshot.runtime_actuator_last_poll_age_ms,
        ),
        stop_result_phase: studio_snapshot.stop_result_phase,
        stop_result_age_ms: ws_age_ms(studio_snapshot.stop_result_age_ms),
        stop_result_error: studio_snapshot.stop_result_error,
        official_mcp_adapter_state: Some(official_state),
        official_mcp_adapter_age_ms: ws_age_ms(official_age_ms),
        official_mcp_adapter_last_error: official_last_error,
        runtime_log_forward: Some(runtime_log_forward_status_snapshot(state, task_id)),
    }
}

fn queue_remote_task_status_heartbeat(
    state: &HelperState,
    helper: &HelperConfig,
    task_id: &str,
) -> bool {
    let Some(task) = state.claimed_tasks.get(task_id) else {
        return false;
    };
    let connection_key = task_connection_key(&task.task_id);
    let Some(connection) = state.remote_connections.get(&connection_key) else {
        return false;
    };
    let plugin_instance_count = state
        .instances
        .values()
        .filter(|instance| instance.task_id.as_deref() == Some(task_id))
        .count();
    let task_status = Some(helper_task_status_snapshot(state, task_id));
    let message = HelperToServerMessage::Heartbeat {
        helper_id: helper.helper_id.clone(),
        place_id: task.place_id.clone(),
        task_id: Some(task.task_id.clone()),
        plugin_instance_count,
        task_status,
    };
    let Ok(encoded) = encode_remote_message(&message) else {
        return false;
    };
    connection
        .sender
        .send(RemoteOutgoingFrame::Text(encoded))
        .is_ok()
}

fn queue_task_status_updates(state: &HelperState, helper: &HelperConfig, task_id: &str) -> bool {
    let queued_remote = queue_remote_task_status_heartbeat(state, helper, task_id);
    helper.hub_heartbeat_notify.notify_one();
    queued_remote
}

fn remote_connection_state_name(
    remote_connection: Option<&RemoteConnectionHandle>,
) -> &'static str {
    remote_connection
        .map(|connection| match connection.connection_state {
            RemoteConnectionState::Connecting => "connecting",
            RemoteConnectionState::Connected => "connected",
            RemoteConnectionState::Retrying => "retrying",
            RemoteConnectionState::Error => "error",
        })
        .unwrap_or("disconnected")
}

fn heartbeat_task_statuses(state: &HelperState) -> Vec<HelperHeartbeatTaskStatus> {
    let mut active_tasks = state
        .claimed_tasks
        .values()
        .map(|task| {
            let connection_key = task_connection_key(&task.task_id);
            let remote_connection = state.remote_connections.get(&connection_key);
            let studio_snapshot = task_studio_mode_snapshot(state, &task.task_id);
            let (
                official_mcp_adapter_state,
                official_mcp_adapter_age_ms,
                official_mcp_adapter_last_error,
            ) = task_official_adapter_snapshot(
                state,
                &task.task_id,
                studio_snapshot.edit_runtime_state == "ready",
            );
            let runtime_log_forward = runtime_log_forward_status_snapshot(state, &task.task_id);
            HelperHeartbeatTaskStatus {
                task_id: task.task_id.clone(),
                remote_state: remote_connection_state_name(remote_connection).to_owned(),
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
                runtime_log_forward,
            }
        })
        .collect::<Vec<_>>();
    active_tasks.sort_by(|left, right| left.task_id.cmp(&right.task_id));
    active_tasks
}
