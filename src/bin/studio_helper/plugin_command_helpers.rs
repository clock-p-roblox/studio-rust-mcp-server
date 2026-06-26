fn extract_command_id(command: &Value) -> Result<String> {
    command
        .get("id")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
        .ok_or_else(|| eyre!("command payload missing string id"))
}

fn extract_tool_name_and_args(command: &Value) -> Result<(String, Value)> {
    let args = command
        .get("args")
        .and_then(Value::as_object)
        .ok_or_else(|| eyre!("command payload missing args object"))?;
    let (name, payload) = args
        .iter()
        .next()
        .ok_or_else(|| eyre!("command args object was empty"))?;
    Ok((name.clone(), payload.clone()))
}

fn is_studio_control_tool(tool_name: &str) -> bool {
    matches!(
        tool_name,
        "LaunchStudioSession" | "StartStopPlay" | "launch_studio_session" | "start_stop_play"
    )
}

fn plugin_request_timeout_for_tool(tool_name: &str) -> Duration {
    if is_studio_control_tool(tool_name) {
        PLUGIN_STUDIO_CONTROL_REQUEST_TIMEOUT
    } else {
        PLUGIN_REQUEST_TIMEOUT
    }
}
