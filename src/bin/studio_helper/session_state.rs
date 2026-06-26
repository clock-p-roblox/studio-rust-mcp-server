fn task_connection_key(task_id: &str) -> String {
    format!("task:{task_id}")
}

fn normalize_studio_mode(value: Option<&str>) -> Option<String> {
    match value.map(str::trim) {
        Some("stop") => Some("stop".to_owned()),
        Some("start_play") => Some("start_play".to_owned()),
        Some("run_server") => Some("run_server".to_owned()),
        Some("unknown") => Some("unknown".to_owned()),
        _ => None,
    }
}

fn studio_mode_is_running(mode: Option<&str>) -> bool {
    matches!(mode, Some("start_play") | Some("run_server"))
}

#[derive(Clone, Debug)]
struct StudioTaskStatusSnapshot {
    studio_session_state: String,
    last_known_session_state: Option<String>,
    last_session_error_reason: Option<String>,
    studio_mode: Option<String>,
    studio_mode_age_ms: Option<u128>,
    studio_mode_source: String,
    studio_control_state: String,
    studio_transition_phase: String,
    studio_transition_age_ms: Option<u128>,
    edit_runtime_state: String,
    edit_runtime_age_ms: Option<u128>,
    studio_control_last_error: Option<String>,
    active_stop_request_id: Option<u64>,
    last_stop_request_id: u64,
    stop_request_recorded_age_ms: Option<u128>,
    runtime_actuator_last_poll_id: Option<u64>,
    runtime_actuator_last_poll_age_ms: Option<u128>,
    stop_result_phase: Option<String>,
    stop_result_age_ms: Option<u128>,
    stop_result_error: Option<String>,
}

#[derive(Clone, Debug)]
struct StudioStopRequestSnapshot {
    active_stop_request_id: Option<u64>,
    last_stop_request_id: u64,
    stop_request_recorded_age_ms: Option<u128>,
    runtime_actuator_last_poll_id: Option<u64>,
    runtime_actuator_last_poll_age_ms: Option<u128>,
    stop_result_phase: Option<String>,
    stop_result_age_ms: Option<u128>,
    stop_result_error: Option<String>,
}

struct RecordedStopRequest {
    place_id: String,
    task_id: Option<String>,
    stop_request_id: u64,
}

enum StopRequestRecordError {
    MissingInstance,
    RuntimeActuatorUnavailable(String),
}

fn is_stopping_transition_phase(phase: &str) -> bool {
    phase == "stopping_requested" || phase == "stopping_observed"
}

fn active_stop_request_id_for_instance(instance: &PluginInstance) -> Option<u64> {
    if instance.stop_request_id > 0
        && is_stopping_transition_phase(&instance.studio_transition_phase)
    {
        Some(instance.stop_request_id)
    } else {
        None
    }
}

fn stop_request_snapshot_for_instance(instance: &PluginInstance) -> StudioStopRequestSnapshot {
    StudioStopRequestSnapshot {
        active_stop_request_id: active_stop_request_id_for_instance(instance),
        last_stop_request_id: instance.stop_request_id,
        stop_request_recorded_age_ms: instance
            .stop_request_recorded_at
            .map(|value| value.elapsed().as_millis()),
        runtime_actuator_last_poll_id: instance.stop_request_last_poll_id,
        runtime_actuator_last_poll_age_ms: instance
            .stop_request_last_polled_at
            .map(|value| value.elapsed().as_millis()),
        stop_result_phase: instance.stop_result_phase.clone(),
        stop_result_age_ms: instance
            .stop_result_observed_at
            .map(|value| value.elapsed().as_millis()),
        stop_result_error: instance.stop_result_error.clone(),
    }
}

fn empty_stop_request_snapshot() -> StudioStopRequestSnapshot {
    StudioStopRequestSnapshot {
        active_stop_request_id: None,
        last_stop_request_id: 0,
        stop_request_recorded_age_ms: None,
        runtime_actuator_last_poll_id: None,
        runtime_actuator_last_poll_age_ms: None,
        stop_result_phase: None,
        stop_result_age_ms: None,
        stop_result_error: None,
    }
}

fn is_stop_request_deliverable_phase(phase: &str) -> bool {
    phase == "stopping_requested"
}

fn is_error_transition_phase(phase: &str) -> bool {
    phase == "error"
}

fn set_edit_runtime_available(instance: &mut PluginInstance) {
    instance.edit_runtime_observed_at = Some(Instant::now());
    instance.edit_runtime_unavailable_reason = None;
}

fn set_edit_runtime_unavailable(instance: &mut PluginInstance, reason: &str) {
    instance.edit_runtime_unavailable_reason = Some(reason.to_owned());
}

fn edit_unavailable_reason_priority(reason: &str) -> u8 {
    match reason {
        "stopping" => 4,
        "launching" => 3,
        "runtime" => 2,
        "launch_failed" => 1,
        _ => 0,
    }
}

fn current_session_state_for_instance(instance: &PluginInstance) -> Option<&'static str> {
    let transition_phase = effective_studio_transition_phase(instance);
    if is_stopping_transition_phase(&transition_phase) {
        return Some("stopping");
    }
    if instance.edit_runtime_unavailable_reason.as_deref() == Some("launching") {
        return Some("starting_play");
    }
    if instance_has_fresh_control(instance) {
        return Some("play");
    }
    if instance.edit_runtime_unavailable_reason.as_deref() == Some("launch_failed") {
        return None;
    }
    None
}

fn last_known_session_state_for_instance(instance: &PluginInstance) -> Option<&'static str> {
    let transition_phase = effective_studio_transition_phase(instance);
    if is_stopping_transition_phase(&transition_phase) {
        return Some("stopping");
    }
    if instance.edit_runtime_unavailable_reason.as_deref() == Some("launching") {
        return Some("starting_play");
    }
    if instance_has_fresh_control(instance) {
        return Some("play");
    }
    if instance.edit_runtime_unavailable_reason.as_deref() == Some("launch_failed") {
        return None;
    }
    None
}

fn reported_session_state_for_instance(instance: &PluginInstance) -> &'static str {
    current_session_state_for_instance(instance).unwrap_or_else(|| {
        match edit_runtime_state(instance).as_str() {
            "ready" => "stop",
            _ => "none_response",
        }
    })
}

fn task_studio_session_state_snapshot(
    state: &HelperState,
    task_id: &str,
) -> (String, Option<String>, Option<String>) {
    let mut has_instance = false;
    let mut has_stopping = false;
    let mut has_launching = false;
    let mut has_launch_failed = false;
    let mut has_runtime = false;
    let mut has_play = false;
    let mut has_stop = false;
    let mut newest_known: Option<(&'static str, u128)> = None;
    let mut newest_error: Option<(String, u128)> = None;

    for instance in state
        .instances
        .values()
        .filter(|instance| instance.task_id.as_deref() == Some(task_id))
    {
        has_instance = true;
        match current_session_state_for_instance(instance) {
            Some("stopping") => has_stopping = true,
            Some("starting_play") => has_launching = true,
            Some("launch_failed") => has_launch_failed = true,
            Some("play") => has_play = true,
            Some("stop") => has_stop = true,
            _ => {}
        }
        match edit_runtime_state(instance).as_str() {
            "ready" => has_stop = true,
            "runtime" => has_runtime = true,
            _ => {}
        }
        if let Some(known) = last_known_session_state_for_instance(instance) {
            let age = instance
                .studio_mode_observed_at
                .or(instance.studio_transition_started_at)
                .map(|observed_at| observed_at.elapsed().as_millis())
                .unwrap_or(u128::MAX);
            if newest_known
                .as_ref()
                .map(|(_, current_age)| age < *current_age)
                .unwrap_or(true)
            {
                newest_known = Some((known, age));
            }
        }
        if let Some(error) = instance.studio_control_last_error.as_ref() {
            let age = instance
                .studio_transition_started_at
                .or(instance.studio_control_observed_at)
                .or(instance.studio_mode_observed_at)
                .map(|observed_at| observed_at.elapsed().as_millis())
                .unwrap_or(u128::MAX);
            if newest_error
                .as_ref()
                .map(|(_, current_age)| age < *current_age)
                .unwrap_or(true)
            {
                newest_error = Some((error.clone(), age));
            }
        }
    }

    let studio_session_state = if !has_instance {
        "none_connected"
    } else if has_stopping {
        "stopping"
    } else if has_launching {
        "starting_play"
    } else if has_play {
        "play"
    } else if has_launch_failed {
        "none_response"
    } else if has_runtime {
        "none_response"
    } else if has_stop {
        "stop"
    } else {
        "none_response"
    }
    .to_owned();

    let last_known_session_state = if matches!(
        studio_session_state.as_str(),
        "stop" | "starting_play" | "play" | "stopping"
    ) {
        Some(studio_session_state.clone())
    } else {
        newest_known.map(|(state, _)| state.to_owned())
    };

    let last_session_error_reason =
        newest_error
            .map(|(error, _)| error)
            .or_else(|| match studio_session_state.as_str() {
                "none_connected" => Some("studio_plugin_not_connected".to_owned()),
                "none_response" => Some("studio_plugin_no_response".to_owned()),
                _ => None,
            });

    (
        studio_session_state,
        last_known_session_state,
        last_session_error_reason,
    )
}

fn set_studio_transition_phase(instance: &mut PluginInstance, phase: &str) {
    if instance.studio_transition_phase != phase {
        instance.studio_transition_started_at = if phase == "idle" || phase == "running" {
            None
        } else {
            Some(Instant::now())
        };
    }
    instance.studio_transition_phase = phase.to_owned();
}

fn instance_has_fresh_control(instance: &PluginInstance) -> bool {
    instance.studio_control_state == "ready"
        && instance
            .studio_control_observed_at
            .map(|observed_at| observed_at.elapsed() <= STUDIO_CONTROL_HEARTBEAT_STALE_AFTER)
            .unwrap_or(false)
}

fn runtime_control_heartbeat_is_stale(instance: &PluginInstance) -> bool {
    instance
        .studio_control_observed_at
        .map(|observed_at| observed_at.elapsed() > STUDIO_CONTROL_HEARTBEAT_STALE_AFTER)
        .unwrap_or(true)
}

fn runtime_control_heartbeat_expired_after_observation(instance: &PluginInstance) -> bool {
    instance
        .studio_control_observed_at
        .map(|observed_at| observed_at.elapsed() > STUDIO_CONTROL_HEARTBEAT_STALE_AFTER)
        .unwrap_or(false)
}

fn effective_studio_control_state(instance: &PluginInstance) -> String {
    let transition_phase = effective_studio_transition_phase(instance);
    if is_error_transition_phase(&transition_phase) {
        return "lost".to_owned();
    }
    if is_stopping_transition_phase(&transition_phase) {
        return "stopping".to_owned();
    }
    if instance_has_fresh_control(instance) {
        "ready".to_owned()
    } else if instance.studio_control_state == "none" && transition_phase == "idle" {
        "none".to_owned()
    } else {
        "lost".to_owned()
    }
}

fn effective_studio_transition_phase(instance: &PluginInstance) -> String {
    if is_error_transition_phase(&instance.studio_transition_phase)
        || is_stopping_transition_phase(&instance.studio_transition_phase)
    {
        return instance.studio_transition_phase.clone();
    }
    if instance_has_fresh_control(instance) {
        "running".to_owned()
    } else {
        instance.studio_transition_phase.clone()
    }
}

fn record_stop_request_for_instance(
    state: &mut HelperState,
    instance_id: &str,
) -> Result<RecordedStopRequest, StopRequestRecordError> {
    let Some(instance) = state.instances.get_mut(instance_id) else {
        return Err(StopRequestRecordError::MissingInstance);
    };
    if !instance_has_fresh_control(instance) {
        return Err(StopRequestRecordError::RuntimeActuatorUnavailable(
            runtime_actuator_unavailable_detail(instance),
        ));
    }
    instance.stop_request_id = instance.stop_request_id.saturating_add(1);
    instance.studio_control_state = "stopping".to_owned();
    set_studio_transition_phase(instance, "stopping_requested");
    instance.studio_control_last_error = None;
    instance.stop_request_recorded_at = Some(Instant::now());
    instance.stop_request_last_polled_at = None;
    instance.stop_request_last_poll_id = None;
    instance.stop_result_phase = None;
    instance.stop_result_error = None;
    instance.stop_result_observed_at = None;
    set_edit_runtime_unavailable(instance, "stopping");
    instance.last_seen_at = Instant::now();
    Ok(RecordedStopRequest {
        place_id: instance.place_id.clone(),
        task_id: instance.task_id.clone(),
        stop_request_id: instance.stop_request_id,
    })
}

fn runtime_actuator_unavailable_detail(instance: &PluginInstance) -> String {
    format!(
        "studio_control_state={} studio_transition_phase={} control_age_ms={:?} edit_runtime_state={} unavailable_reason={:?}",
        instance.studio_control_state,
        instance.studio_transition_phase,
        instance
            .studio_control_observed_at
            .map(|observed_at| observed_at.elapsed().as_millis()),
        edit_runtime_state(instance),
        instance.edit_runtime_unavailable_reason
    )
}

fn mark_runtime_stop_timeout(instance: &mut PluginInstance, stop_request_id: u64, detail: String) {
    instance.studio_control_state = "lost".to_owned();
    set_studio_transition_phase(instance, "error");
    instance.studio_control_last_error = Some(format!(
        "runtime_stop_timeout: stop_request_id={stop_request_id} {detail}"
    ));
    instance.stop_result_phase = Some("failed".to_owned());
    instance.stop_result_error = instance.studio_control_last_error.clone();
    instance.stop_result_observed_at = Some(Instant::now());
    set_edit_runtime_unavailable(instance, "runtime");
    instance.last_seen_at = Instant::now();
}

#[cfg(test)]
fn initial_control_state_for_mode(mode: Option<&str>) -> (String, String, Option<Instant>) {
    match mode {
        None | Some("stop") => ("none".to_owned(), "idle".to_owned(), None),
        Some("start_play") | Some("run_server") => ("lost".to_owned(), "starting".to_owned(), None),
        _ => ("lost".to_owned(), "error".to_owned(), None),
    }
}

fn edit_runtime_state(instance: &PluginInstance) -> String {
    if let Some(reason) = instance.edit_runtime_unavailable_reason.as_deref() {
        return reason.to_owned();
    }
    let Some(observed_at) = instance.edit_runtime_observed_at else {
        return "missing".to_owned();
    };
    if observed_at.elapsed() <= EDIT_RUNTIME_STALE_AFTER {
        "ready".to_owned()
    } else {
        "stale".to_owned()
    }
}

fn task_edit_runtime_snapshot<'a>(
    instances: impl Iterator<Item = &'a PluginInstance>,
) -> (String, Option<u128>) {
    let mut has_instance = false;
    let mut unavailable_reason: Option<String> = None;
    let mut newest_edit_age_ms: Option<u128> = None;

    for instance in instances {
        has_instance = true;
        if let Some(reason) = instance.edit_runtime_unavailable_reason.as_ref() {
            if unavailable_reason
                .as_deref()
                .map(|current| {
                    edit_unavailable_reason_priority(reason)
                        > edit_unavailable_reason_priority(current)
                })
                .unwrap_or(true)
            {
                unavailable_reason = Some(reason.clone());
            }
        }
        if let Some(observed_at) = instance.edit_runtime_observed_at {
            let age_ms = observed_at.elapsed().as_millis();
            if newest_edit_age_ms
                .map(|current| age_ms < current)
                .unwrap_or(true)
            {
                newest_edit_age_ms = Some(age_ms);
            }
        }
    }

    let state = if !has_instance {
        "missing"
    } else if let Some(reason) = unavailable_reason.as_deref() {
        reason
    } else if newest_edit_age_ms
        .map(|age_ms| age_ms <= EDIT_RUNTIME_STALE_AFTER.as_millis())
        .unwrap_or(false)
    {
        "ready"
    } else if newest_edit_age_ms.is_some() {
        "stale"
    } else {
        "missing"
    };

    (state.to_owned(), newest_edit_age_ms)
}

fn task_studio_mode_snapshot(state: &HelperState, task_id: &str) -> StudioTaskStatusSnapshot {
    let (studio_session_state, last_known_session_state, last_session_error_reason) =
        task_studio_session_state_snapshot(state, task_id);
    let stop_snapshot = state
        .instances
        .values()
        .filter(|instance| instance.task_id.as_deref() == Some(task_id))
        .max_by_key(|instance| {
            (
                active_stop_request_id_for_instance(instance).is_some(),
                instance.stop_request_id,
                instance.last_seen_at,
            )
        })
        .map(stop_request_snapshot_for_instance)
        .unwrap_or_else(empty_stop_request_snapshot);
    let (task_edit_runtime_state, task_edit_runtime_age_ms) = task_edit_runtime_snapshot(
        state
            .instances
            .values()
            .filter(|instance| instance.task_id.as_deref() == Some(task_id)),
    );
    state
        .instances
        .values()
        .filter(|instance| instance.task_id.as_deref() == Some(task_id))
        .max_by_key(|instance| instance.last_seen_at)
        .map(|instance| {
            (
                instance.studio_mode.clone(),
                instance
                    .studio_mode_observed_at
                    .map(|observed_at| observed_at.elapsed().as_millis()),
                instance.studio_mode_source.clone(),
                effective_studio_control_state(instance),
                effective_studio_transition_phase(instance),
                instance
                    .studio_transition_started_at
                    .map(|value| value.elapsed().as_millis()),
                instance.studio_control_last_error.clone(),
            )
        })
        .map(
            |(
                mode,
                age,
                mode_source,
                control_state,
                transition_phase,
                transition_age_ms,
                control_last_error,
            )| StudioTaskStatusSnapshot {
                studio_session_state: studio_session_state.clone(),
                last_known_session_state: last_known_session_state.clone(),
                last_session_error_reason: last_session_error_reason.clone(),
                studio_mode: mode,
                studio_mode_age_ms: age,
                studio_mode_source: mode_source,
                studio_control_state: control_state,
                studio_transition_phase: transition_phase,
                studio_transition_age_ms: transition_age_ms,
                edit_runtime_state: task_edit_runtime_state.clone(),
                edit_runtime_age_ms: task_edit_runtime_age_ms,
                studio_control_last_error: control_last_error,
                active_stop_request_id: stop_snapshot.active_stop_request_id,
                last_stop_request_id: stop_snapshot.last_stop_request_id,
                stop_request_recorded_age_ms: stop_snapshot.stop_request_recorded_age_ms,
                runtime_actuator_last_poll_id: stop_snapshot.runtime_actuator_last_poll_id,
                runtime_actuator_last_poll_age_ms: stop_snapshot.runtime_actuator_last_poll_age_ms,
                stop_result_phase: stop_snapshot.stop_result_phase.clone(),
                stop_result_age_ms: stop_snapshot.stop_result_age_ms,
                stop_result_error: stop_snapshot.stop_result_error.clone(),
            },
        )
        .unwrap_or(StudioTaskStatusSnapshot {
            studio_session_state,
            last_known_session_state,
            last_session_error_reason,
            studio_mode: None,
            studio_mode_age_ms: None,
            studio_mode_source: "none".to_owned(),
            studio_control_state: "none".to_owned(),
            studio_transition_phase: "idle".to_owned(),
            studio_transition_age_ms: None,
            edit_runtime_state: task_edit_runtime_state,
            edit_runtime_age_ms: task_edit_runtime_age_ms,
            studio_control_last_error: None,
            active_stop_request_id: stop_snapshot.active_stop_request_id,
            last_stop_request_id: stop_snapshot.last_stop_request_id,
            stop_request_recorded_age_ms: stop_snapshot.stop_request_recorded_age_ms,
            runtime_actuator_last_poll_id: stop_snapshot.runtime_actuator_last_poll_id,
            runtime_actuator_last_poll_age_ms: stop_snapshot.runtime_actuator_last_poll_age_ms,
            stop_result_phase: stop_snapshot.stop_result_phase,
            stop_result_age_ms: stop_snapshot.stop_result_age_ms,
            stop_result_error: stop_snapshot.stop_result_error,
        })
}

fn task_official_adapter_snapshot(
    state: &HelperState,
    task_id: &str,
    edit_runtime_ready: bool,
) -> (String, Option<u128>, Option<String>) {
    let Some(record) = state.official_adapter_states.get(task_id) else {
        return ("not_started".to_owned(), None, None);
    };
    let state_name = if record.state == "ready" {
        if edit_runtime_ready {
            "ready".to_owned()
        } else {
            "stale".to_owned()
        }
    } else {
        record.state.clone()
    };
    (
        state_name,
        Some(record.observed_at.elapsed().as_millis()),
        record.last_error.clone(),
    )
}
