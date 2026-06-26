async fn await_observable_upstream_result<T, F>(
    operation: &'static str,
    target: String,
    future: F,
) -> T
where
    F: Future<Output = T>,
{
    let started_at = Instant::now();
    let mut future = Box::pin(future);
    tokio::select! {
        result = &mut future => result,
        _ = tokio::time::sleep(OBSERVABLE_UPSTREAM_SLOW_AFTER) => {
            tracing::warn!(
                operation,
                target,
                threshold_ms = OBSERVABLE_UPSTREAM_SLOW_AFTER.as_millis(),
                elapsed_ms = started_at.elapsed().as_millis(),
                result_observable = true,
                "observable upstream operation still waiting for result"
            );
            future.await
        }
    }
}

fn maybe_sanitize_identifier(label: &str, value: Option<&str>) -> Result<Option<String>> {
    match value {
        Some(raw) => Ok(Some(sanitize_identifier(label, raw)?)),
        None => Ok(None),
    }
}

fn select_claimed_task(
    state: &HelperState,
    place_id: &str,
    task_id: Option<&str>,
) -> Result<ClaimedTask> {
    if let Some(task_id) = task_id {
        let task = state
            .claimed_tasks
            .get(task_id)
            .ok_or_else(|| eyre!("helper has no claimed task for task_id {task_id}"))?;
        if task.place_id != place_id {
            return Err(eyre!(
                "task_id {task_id} does not match place_id {place_id}"
            ));
        }
        return Ok(task.clone());
    }

    let mut matches = state
        .claimed_tasks
        .values()
        .filter(|task| task.place_id == place_id)
        .cloned()
        .collect::<Vec<_>>();
    match matches.len() {
        0 => Err(eyre!(
            "helper has no claimed task for place_id {place_id}; wait for hub claim first"
        )),
        1 => Ok(matches.remove(0)),
        _ => Err(eyre!(
            "multiple claimed tasks found for place_id {place_id}; task_id is required"
        )),
    }
}

fn route_identity_matches(instance: &PluginInstance, task_id: Option<&str>) -> bool {
    if let Some(expected_task_id) = task_id {
        if instance.task_id.as_deref() != Some(expected_task_id) {
            return false;
        }
    }
    true
}

fn select_claimed_task_for_pid(
    state: &HelperState,
    studio_pid: u32,
    place_id: &str,
) -> Option<ClaimedTask> {
    let task_id = state
        .launch_processes
        .values()
        .find(|launch| launch.studio_pid == studio_pid)
        .map(|launch| launch.task_id.clone())?;
    let task = state.claimed_tasks.get(&task_id)?;
    if task.place_id != place_id {
        return None;
    }
    Some(task.clone())
}

#[cfg(test)]
fn resolve_claimed_task_for_request(
    state: &HelperState,
    place_id: &str,
    task_id: Option<&str>,
    studio_pid: Option<u32>,
) -> Result<ClaimedTask> {
    if let Some(studio_pid) = studio_pid {
        if let Some(task) = select_claimed_task_for_pid(state, studio_pid, place_id) {
            return Ok(task);
        }
    }
    if task_id.is_none() {
        return Err(eyre!(
            "helper could not map Studio for place_id {place_id} to a claimed launch; task_id is required unless the Studio pid was launched by helper"
        ));
    }
    select_claimed_task(state, place_id, task_id)
}

fn resolve_claimed_task_for_plugin_request(
    state: &HelperState,
    place_id: &str,
    task_id: Option<&str>,
    studio_pid: Option<u32>,
) -> Result<ClaimedTask> {
    if let Some(studio_pid) = studio_pid {
        if let Some(task) = select_claimed_task_for_pid(state, studio_pid, place_id) {
            return Ok(task);
        }
    }
    select_claimed_task(state, place_id, task_id)
}

fn resolve_plugin_routing_decision(
    state: &HelperState,
    place_id: &str,
    task_id: Option<&str>,
    studio_pid: Option<u32>,
) -> Result<ClaimedTask> {
    #[cfg(target_os = "windows")]
    {
        resolve_claimed_task_for_plugin_request(state, place_id, task_id, studio_pid)
    }

    #[cfg(not(target_os = "windows"))]
    {
        resolve_claimed_task_for_plugin_request(state, place_id, task_id, studio_pid)
    }
}

fn bind_launch_process_to_pid(
    state: &mut HelperState,
    task: &ClaimedTask,
    studio_pid: u32,
) -> Result<()> {
    if let Some(existing) = state.launch_processes.get_mut(&task.task_id) {
        existing.place_id = task.place_id.clone();
        existing.universe_id = task.universe_id.clone();
        existing.studio_pid = studio_pid;
        return Ok(());
    }
    state.launch_processes.insert(
        task.task_id.clone(),
        LaunchProcessRecord {
            task_id: task.task_id.clone(),
            place_id: task.place_id.clone(),
            universe_id: task.universe_id.clone(),
            studio_pid,
            launched_by_helper: false,
            #[cfg(target_os = "windows")]
            kill_on_close_job: None,
            child: None,
        },
    );
    Ok(())
}

fn task_instance_pids(state: &HelperState, task_id: &str) -> Vec<u32> {
    let mut pids = state
        .instances
        .values()
        .filter(|instance| instance.task_id.as_deref() == Some(task_id))
        .filter_map(|instance| instance.studio_pid)
        .collect::<Vec<_>>();
    pids.sort();
    pids.dedup();
    pids
}

fn launch_process_is_active_for_status(state: &HelperState, launch: &LaunchProcessRecord) -> bool {
    let instance_pids = task_instance_pids(state, &launch.task_id);
    instance_pids.is_empty() || instance_pids.contains(&launch.studio_pid)
}

fn task_active_studio_pid(state: &HelperState, task_id: &str) -> Option<u32> {
    task_instance_pids(state, task_id)
        .into_iter()
        .next()
        .or_else(|| {
            state
                .launch_processes
                .get(task_id)
                .filter(|launch| launch_process_is_active_for_status(state, launch))
                .map(|launch| launch.studio_pid)
        })
}

fn remove_instances_for_claimed_task(state: &mut HelperState, task_id: &str) {
    state.instances.retain(|instance_id, instance| {
        let keep = !route_identity_matches(instance, Some(task_id));
        if !keep {
            tracing::info!(
                instance_id,
                task_id,
                "dropping plugin instance for released claimed task"
            );
        }
        keep
    });
}

fn release_claimed_task(
    state: &mut HelperState,
    task_id: &str,
    terminate_helper_spawned: bool,
    reason: &str,
) -> Option<PendingLaunchTermination> {
    let should_remove = state.claimed_tasks.contains_key(task_id);
    if !should_remove {
        return None;
    }

    let remove_launch_process = state.launch_processes.contains_key(task_id);
    let pending_termination = if remove_launch_process {
        state
            .launch_processes
            .remove(task_id)
            .and_then(|mut launch| {
                if terminate_helper_spawned && launch.launched_by_helper {
                    launch.child.take().map(|child| PendingLaunchTermination {
                        task_id: launch.task_id,
                        studio_pid: launch.studio_pid,
                        #[cfg(target_os = "windows")]
                        _kill_on_close_job: launch.kill_on_close_job.take(),
                        child,
                    })
                } else {
                    None
                }
            })
    } else {
        None
    };

    remove_instances_for_claimed_task(state, task_id);
    let connection_key = task_connection_key(task_id);
    if let Some(remote_connection) = state.remote_connections.remove(&connection_key) {
        let _ = remote_connection.stop_tx.send(true);
        tracing::info!(
            task_id,
            connection_key,
            "stopped remote websocket for released claimed task"
        );
    }
    state.claimed_tasks.remove(task_id);
    state.runtime_log_forward_statuses.remove(task_id);
    stop_removed_official_adapter(state, task_id);
    state.last_remote_errors.remove(&connection_key);
    tracing::info!(task_id, reason, "released claimed task from helper state");
    pending_termination
}

fn release_all_claimed_tasks(
    state: &mut HelperState,
    terminate_helper_spawned: bool,
    reason: &str,
) -> Vec<PendingLaunchTermination> {
    let mut task_ids = state.claimed_tasks.keys().cloned().collect::<Vec<_>>();
    task_ids.sort();
    let mut pending_terminations = Vec::new();
    for task_id in task_ids {
        if let Some(pending_termination) =
            release_claimed_task(state, &task_id, terminate_helper_spawned, reason)
        {
            pending_terminations.push(pending_termination);
        }
    }
    pending_terminations
}

async fn terminate_pending_launch_termination(pending: PendingLaunchTermination) {
    let PendingLaunchTermination {
        task_id,
        studio_pid,
        #[cfg(target_os = "windows")]
        _kill_on_close_job,
        mut child,
    } = pending;
    let task_id_for_join = task_id.clone();
    match tokio::task::spawn_blocking(move || {
        if let Err(error) = child.kill() {
            tracing::warn!(
                task_id,
                studio_pid,
                error = summarize_error(&error.to_string()),
                "failed to terminate helper-launched Studio during claim release"
            );
            return;
        }
        tracing::info!(
            task_id,
            studio_pid,
            "terminated helper-launched Studio during claim release"
        );
        if let Err(error) = child.wait() {
            tracing::warn!(
                task_id,
                studio_pid,
                error = summarize_error(&error.to_string()),
                "failed to reap helper-launched Studio after claim release"
            );
        }
    })
    .await
    {
        Ok(()) => {}
        Err(error) => {
            tracing::warn!(
                task_id = task_id_for_join,
                studio_pid,
                error = summarize_error(&error.to_string()),
                "helper-launched Studio termination task panicked"
            );
        }
    }
}

fn reconcile_launch_processes(state: &mut HelperState) {
    let mut exited = Vec::new();
    for (task_id, launch) in state.launch_processes.iter_mut() {
        let Some(child) = launch.child.as_mut() else {
            continue;
        };
        match child.try_wait() {
            Ok(Some(status)) => {
                exited.push((task_id.clone(), launch.studio_pid, status.to_string()))
            }
            Ok(None) => {}
            Err(error) => {
                tracing::warn!(
                    task_id,
                    studio_pid = launch.studio_pid,
                    error = summarize_error(&error.to_string()),
                    "failed to poll helper-launched Studio process"
                );
            }
        }
    }

    for (task_id, studio_pid, status) in exited {
        tracing::warn!(
            task_id,
            studio_pid,
            exit_status = status,
            "helper-launched Studio exited"
        );
        let _ = release_claimed_task(state, &task_id, false, "helper-launched Studio exited");
    }
}
