async fn handle_official_mcp_request(
    app: AppState,
    expected_place_id: &str,
    expected_task_id: Option<&str>,
    request: OfficialMcpRequest,
) -> Result<String> {
    let expected_task_id = expected_task_id
        .ok_or_else(|| eyre!("official MCP request requires task_id-bound remote connection"))?;
    if request.task_id != expected_task_id {
        return Err(eyre!(
            "official MCP request task_id mismatch: expected {}, got {}",
            expected_task_id,
            request.task_id
        ));
    }
    if request.place_id != expected_place_id {
        return Err(eyre!(
            "official MCP request place_id mismatch: expected {}, got {}",
            expected_place_id,
            request.place_id
        ));
    }
    let timeout = Duration::from_millis(request.timeout_ms.max(1));
    let timeout = timeout.min(OFFICIAL_MCP_REQUEST_TIMEOUT);
    let handle = ensure_official_adapter_handle(&app, &request.task_id, &request.place_id).await?;
    let (response_tx, response_rx) = oneshot::channel();
    let task_id = request.task_id.clone();
    if handle
        .sender
        .send(OfficialAdapterCommand {
            request,
            response_tx,
        })
        .is_err()
    {
        stop_and_remove_official_adapter(&app.state, &task_id).await;
        return Err(eyre!("official MCP adapter worker is not running"));
    }
    match tokio::time::timeout(timeout + Duration::from_secs(5), response_rx).await {
        Ok(Ok(result)) => result,
        Ok(Err(_)) => {
            stop_and_remove_official_adapter(&app.state, &task_id).await;
            Err(eyre!("official MCP adapter response channel closed"))
        }
        Err(_) => {
            stop_and_remove_official_adapter(&app.state, &task_id).await;
            Err(eyre!("official MCP adapter request timed out"))
        }
    }
}

async fn set_official_adapter_state(
    state: &SharedState,
    task_id: &str,
    status: &str,
    last_error: Option<String>,
) {
    let mut state = state.lock().await;
    state.official_adapter_states.insert(
        task_id.to_owned(),
        OfficialAdapterStateRecord {
            state: status.to_owned(),
            last_error,
            observed_at: Instant::now(),
        },
    );
}

fn start_official_adapter_prewarm(app: AppState, task_id: String, place_id: String) {
    tokio::spawn(async move {
        set_official_adapter_state(&app.state, &task_id, "starting", None).await;
        let handle = match ensure_official_adapter_handle(&app, &task_id, &place_id).await {
            Ok(handle) => handle,
            Err(error) => {
                set_official_adapter_state(
                    &app.state,
                    &task_id,
                    "error",
                    Some(summarize_error(&error.to_string())),
                )
                .await;
                return;
            }
        };
        let request = OfficialMcpRequest {
            request_id: Uuid::new_v4().to_string(),
            task_id: task_id.clone(),
            place_id: place_id.clone(),
            action: "ping".to_owned(),
            arguments: Value::Null,
            timeout_ms: 30_000,
        };
        let (response_tx, response_rx) = oneshot::channel();
        if handle
            .sender
            .send(OfficialAdapterCommand {
                request,
                response_tx,
            })
            .is_err()
        {
            set_official_adapter_state(
                &app.state,
                &task_id,
                "error",
                Some("official MCP adapter worker is not running".to_owned()),
            )
            .await;
            return;
        }
        match tokio::time::timeout(Duration::from_secs(35), response_rx).await {
            Ok(Ok(Ok(_))) => set_official_adapter_state(&app.state, &task_id, "ready", None).await,
            Ok(Ok(Err(error))) => {
                set_official_adapter_state(
                    &app.state,
                    &task_id,
                    "error",
                    Some(summarize_error(&error.to_string())),
                )
                .await
            }
            Ok(Err(_)) => {
                set_official_adapter_state(
                    &app.state,
                    &task_id,
                    "error",
                    Some("official MCP adapter response channel closed".to_owned()),
                )
                .await
            }
            Err(_) => {
                set_official_adapter_state(
                    &app.state,
                    &task_id,
                    "error",
                    Some("official MCP adapter prewarm timed out".to_owned()),
                )
                .await
            }
        }
    });
}

fn stop_removed_official_adapter(state: &mut HelperState, task_id: &str) -> bool {
    state.official_adapter_states.remove(task_id);
    if let Some(adapter) = state.official_adapters.remove(task_id) {
        let _ = adapter.stop_tx.send(true);
        true
    } else {
        false
    }
}

async fn stop_and_remove_official_adapter(state: &SharedState, task_id: &str) -> bool {
    let adapter = {
        let mut state = state.lock().await;
        state.official_adapter_states.remove(task_id);
        state.official_adapters.remove(task_id)
    };
    if let Some(adapter) = adapter {
        let _ = adapter.stop_tx.send(true);
        true
    } else {
        false
    }
}

async fn ensure_official_adapter_handle(
    app: &AppState,
    task_id: &str,
    place_id: &str,
) -> Result<OfficialAdapterHandle> {
    let mut state = app.state.lock().await;
    let claimed_task = state
        .claimed_tasks
        .get(task_id)
        .ok_or_else(|| eyre!("helper has not claimed task_id {task_id}"))?;
    if claimed_task.place_id != place_id {
        return Err(eyre!(
            "claimed task place_id mismatch for task_id {}: expected {}, got {}",
            task_id,
            claimed_task.place_id,
            place_id
        ));
    }
    let connection_key = task_connection_key(task_id);
    let connection = state
        .remote_connections
        .get(&connection_key)
        .ok_or_else(|| eyre!("official MCP adapter requires active remote connection"))?;
    if !matches!(
        connection.connection_state,
        RemoteConnectionState::Connected
    ) {
        return Err(eyre!(
            "official MCP adapter requires ready remote connection for task_id {}",
            task_id
        ));
    }
    if let Some(existing) = state.official_adapters.get(task_id) {
        return Ok(existing.clone());
    }

    let (sender, mut receiver) = mpsc::unbounded_channel::<OfficialAdapterCommand>();
    let (stop_tx, mut stop_rx) = watch::channel(false);
    let handle = OfficialAdapterHandle {
        sender,
        stop_tx: stop_tx.clone(),
    };
    state
        .official_adapters
        .insert(task_id.to_owned(), handle.clone());
    let worker_state = Arc::clone(&app.state);
    let task_id = task_id.to_owned();
    let place_id = place_id.to_owned();
    tokio::spawn(async move {
        official_adapter_worker(worker_state, task_id, place_id, &mut receiver, &mut stop_rx).await;
    });
    Ok(handle)
}

async fn official_adapter_worker(
    state: SharedState,
    task_id: String,
    place_id: String,
    receiver: &mut mpsc::UnboundedReceiver<OfficialAdapterCommand>,
    stop_rx: &mut watch::Receiver<bool>,
) {
    let mut process: Option<OfficialMcpProcess> = None;
    loop {
        tokio::select! {
            changed = stop_rx.changed() => {
                if changed.is_ok() && *stop_rx.borrow() {
                    break;
                }
            }
            command = receiver.recv() => {
                let Some(command) = command else {
                    break;
                };
                if let Err(error) = validate_official_adapter_request(&command.request) {
                    let _ = command.response_tx.send(Err(error));
                    continue;
                }
                set_official_adapter_state(&state, &task_id, "starting", None).await;
                let timeout = Duration::from_millis(command.request.timeout_ms.max(1))
                    .min(OFFICIAL_MCP_REQUEST_TIMEOUT);
                let result = tokio::time::timeout(
                    timeout,
                    official_adapter_execute_with_retry(
                        &task_id,
                        &place_id,
                        &command.request,
                        &mut process,
                    ),
                ).await;
                let result = match result {
                    Ok(result) => result,
                    Err(_) => {
                        if let Some(mut process) = process.take() {
                            process.kill().await;
                        }
                        Err(eyre!("official MCP adapter worker request timed out"))
                    }
                };
                let should_restart_after_error = match &result {
                    Ok(_) => false,
                    Err(error) => should_restart_official_process_after_error(error),
                };
                if should_restart_after_error {
                    if let Some(mut process) = process.take() {
                        process.kill().await;
                    }
                }
                match &result {
                    Ok(_) => set_official_adapter_state(&state, &task_id, "ready", None).await,
                    Err(error) if official_adapter_error_keeps_ready(error) => {
                        set_official_adapter_state(&state, &task_id, "ready", None).await;
                    }
                    Err(error) => {
                        set_official_adapter_state(
                            &state,
                            &task_id,
                            "error",
                            Some(summarize_error(&error.to_string())),
                        )
                        .await;
                    }
                }
                let _ = command.response_tx.send(result);
            }
        }
    }
    if let Some(mut process) = process {
        process.kill().await;
    }
    tracing::info!(task_id, place_id, "official MCP adapter worker stopped");
}

async fn official_adapter_execute_with_retry(
    task_id: &str,
    place_id: &str,
    request: &OfficialMcpRequest,
    process: &mut Option<OfficialMcpProcess>,
) -> Result<String> {
    loop {
        match official_adapter_execute(task_id, place_id, request, process).await {
            Ok(response) => return Ok(response),
            Err(error) if is_transient_official_readiness_error(&error) => {
                tracing::warn!(
                    task_id,
                    place_id,
                    action = request.action.as_str(),
                    error = summarize_error(&error.to_string()),
                    "official MCP adapter is waiting for Studio readiness"
                );
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
            Err(error) => return Err(error),
        }
    }
}

async fn official_adapter_execute(
    task_id: &str,
    place_id: &str,
    request: &OfficialMcpRequest,
    process: &mut Option<OfficialMcpProcess>,
) -> Result<String> {
    if request.action == "ping" {
        return official_adapter_ping(task_id, place_id, process).await;
    }
    let official_tool = official_tool_for_action(&request.action)?;
    ensure_official_process(process).await?;
    let process_ref = process
        .as_mut()
        .ok_or_else(|| eyre!("official MCP process was not created"))?;
    process_ref.initialize().await?;
    let studio_summary = process_ref.ensure_active_studio().await?;
    let (arguments, cleanup_guard) = prepare_official_tool_arguments(
        task_id,
        &request.request_id,
        &request.action,
        request.arguments.clone(),
    )?;
    let result = process_ref.call_tool(official_tool, arguments).await;
    drop(cleanup_guard);
    let result = result?;
    Ok(serde_json::to_string_pretty(&serde_json::json!({
        "ok": true,
        "adapter_state": "ready",
        "task_id": task_id,
        "place_id": place_id,
        "action": request.action,
        "official_tool": official_tool,
        "studio": studio_summary,
        "result": result,
    }))?)
}

fn official_tool_for_action(action: &str) -> Result<&'static str> {
    match action {
        "generate_mesh" => Ok("generate_mesh"),
        "search_creator_store" => Ok("search_creator_store"),
        "insert_from_creator_store" => Ok("insert_from_creator_store"),
        "store_image" => Ok("store_image"),
        "generate_procedural_model" => Ok("generate_procedural_model"),
        "wait_job_finished" => Ok("wait_job_finished"),
        _ => Err(eyre!("unsupported official MCP adapter action: {}", action)),
    }
}

fn validate_official_adapter_request(request: &OfficialMcpRequest) -> Result<()> {
    if request.action != "ping" {
        official_tool_for_action(&request.action)?;
    }
    Ok(())
}

fn should_restart_official_process_after_error(error: &Report) -> bool {
    !is_transient_official_readiness_error(error)
        && !official_adapter_error_keeps_ready(error)
        && !official_adapter_error_preserves_state(error)
}

fn official_adapter_error_keeps_ready(error: &Report) -> bool {
    !is_transient_official_readiness_error(error)
        && (is_official_business_tool_error(error) || is_official_request_argument_error(error))
}

fn official_adapter_error_preserves_state(error: &Report) -> bool {
    let message = error.to_string();
    message.starts_with("unsupported official MCP adapter action:")
        || message.starts_with("Invalid official MCP")
}

fn is_transient_official_readiness_error(error: &Report) -> bool {
    let message = error.to_string();
    message.contains("Not connected to the WS host")
        || message.contains("official MCP found no connected Roblox Studio instances")
        || message
            .contains("previously active Studio has disconnected or doesn't have a place opened")
}

fn is_official_business_tool_error(error: &Report) -> bool {
    let message = error.to_string();
    let Some(rest) = message.strip_prefix("official MCP tool ") else {
        return false;
    };
    let Some((tool_name, summary)) = rest.split_once(" returned error:") else {
        return false;
    };
    if is_official_channel_error_summary(summary) {
        return false;
    }
    let summary = summary.to_ascii_lowercase();
    match tool_name {
        "wait_job_finished" => is_official_wait_job_business_error_summary(&summary),
        "generate_mesh"
        | "generate_procedural_model"
        | "search_creator_store"
        | "insert_from_creator_store"
        | "store_image" => is_official_business_error_summary(&summary),
        _ => false,
    }
}

fn is_official_wait_job_business_error_summary(summary: &str) -> bool {
    if summary.contains("workflow failed") || summary.contains("no job found") {
        return true;
    }
    let squashed: String = summary
        .chars()
        .filter(|character| !matches!(character, ' ' | '_' | '-'))
        .collect();
    let references_job_id = summary.contains("job id")
        || squashed.contains("jobid")
        || squashed.contains("generationid");
    references_job_id
        && (summary.contains("invalid")
            || summary.contains("missing")
            || summary.contains("required")
            || summary.contains("malformed"))
}

fn is_official_request_argument_error(error: &Report) -> bool {
    let message = error.to_string();
    message.starts_with("store_image ")
}

fn is_official_business_error_summary(summary: &str) -> bool {
    [
        "policy",
        "moderation",
        "prompt",
        "input",
        "argument",
        "parameter",
        "invalid",
        "malformed",
        "not found",
        "no result",
        "file",
        "image",
        "mime",
        "content",
        "asset",
    ]
    .iter()
    .any(|needle| summary.contains(needle))
}

fn is_official_channel_error_summary(summary: &str) -> bool {
    let summary = summary.to_ascii_lowercase();
    [
        "not connected",
        "connection",
        "ws host",
        "websocket",
        "active studio",
        "studio unavailable",
        "studio has disconnected",
        "no connected roblox studio",
        "adapter failure",
        "internal adapter",
        "timed out",
        "timeout",
        "stdio",
        "json-rpc",
        "protocol",
        "closed stdout",
        "process closed",
    ]
    .iter()
    .any(|needle| summary.contains(needle))
}

fn prepare_official_tool_arguments(
    task_id: &str,
    request_id: &str,
    action: &str,
    arguments: Value,
) -> Result<(Value, Option<TempOfficialImage>)> {
    if action != "store_image" {
        return Ok((arguments, None));
    }
    prepare_official_store_image_arguments(task_id, request_id, arguments)
}

fn prepare_official_store_image_arguments(
    task_id: &str,
    request_id: &str,
    arguments: Value,
) -> Result<(Value, Option<TempOfficialImage>)> {
    let object = arguments
        .as_object()
        .ok_or_else(|| eyre!("store_image arguments must be a JSON object"))?;
    let file_path = string_field(object, &["filePath", "file_path"]);
    let image_base64 = string_field(object, &["imageBase64", "image_base64"]);
    match (
        file_path
            .map(|value| !trim(value).is_empty())
            .unwrap_or(false),
        image_base64
            .map(|value| !trim(value).is_empty())
            .unwrap_or(false),
    ) {
        (true, true) => {
            return Err(eyre!(
                "store_image requires exactly one of filePath or imageBase64"
            ))
        }
        (false, false) => return Err(eyre!("store_image requires filePath or imageBase64")),
        _ => {}
    }
    if let Some(path) = file_path {
        if string_field(object, &["mimeType", "mime_type"]).is_some()
            || string_field(object, &["fileName", "file_name"]).is_some()
        {
            return Err(eyre!(
                "store_image mimeType/fileName are only valid with imageBase64"
            ));
        }
        return Ok((serde_json::json!({ "filePath": trim(path) }), None));
    }

    let image_base64 = trim(image_base64.expect("image_base64 presence checked"));
    if image_base64.len() > OFFICIAL_STORE_IMAGE_MAX_BASE64_CHARS {
        return Err(eyre!(
            "store_image imageBase64 exceeds {} decoded bytes",
            OFFICIAL_STORE_IMAGE_MAX_DECODED_BYTES
        ));
    }
    let mime_type = trim(
        string_field(object, &["mimeType", "mime_type"])
            .ok_or_else(|| eyre!("store_image mimeType is required with imageBase64"))?,
    );
    let extension = official_image_extension_for_mime(&mime_type)?;
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(image_base64)
        .map_err(|error| eyre!("store_image imageBase64 decode failed: {error}"))?;
    if decoded.len() > OFFICIAL_STORE_IMAGE_MAX_DECODED_BYTES {
        return Err(eyre!(
            "store_image decoded image exceeds {} bytes",
            OFFICIAL_STORE_IMAGE_MAX_DECODED_BYTES
        ));
    }
    ensure_official_image_magic(&mime_type, &decoded)?;
    let task_id = sanitize_identifier("task_id", task_id)?;
    let request_id = sanitize_identifier("request_id", request_id)?;
    let hint = string_field(object, &["fileName", "file_name"])
        .map(sanitize_image_file_hint)
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "image".to_owned());
    let output_dir = helper_data_dir()?.join("official-images").join(&task_id);
    fs::create_dir_all(&output_dir)?;
    let output_path = output_dir.join(format!("{request_id}-{hint}.{extension}"));
    fs::write(&output_path, decoded)?;
    tracing::info!(
        path = %output_path.display(),
        task_id,
        "wrote temporary official MCP store_image input"
    );
    Ok((
        serde_json::json!({ "filePath": output_path.to_string_lossy() }),
        Some(TempOfficialImage { path: output_path }),
    ))
}

fn string_field<'a>(object: &'a serde_json::Map<String, Value>, names: &[&str]) -> Option<&'a str> {
    names
        .iter()
        .find_map(|name| object.get(*name).and_then(Value::as_str))
}

fn official_image_extension_for_mime(mime_type: &str) -> Result<&'static str> {
    match mime_type {
        "image/png" => Ok("png"),
        "image/jpeg" | "image/jpg" => Ok("jpg"),
        _ => Err(eyre!(
            "store_image mimeType must be image/png, image/jpeg, or image/jpg"
        )),
    }
}

fn ensure_official_image_magic(mime_type: &str, bytes: &[u8]) -> Result<()> {
    let valid = match mime_type {
        "image/png" => bytes.starts_with(&[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]),
        "image/jpeg" | "image/jpg" => bytes.starts_with(&[0xff, 0xd8, 0xff]),
        _ => false,
    };
    if !valid {
        return Err(eyre!(
            "store_image imageBase64 content does not match mimeType"
        ));
    }
    Ok(())
}

fn sanitize_image_file_hint(value: &str) -> String {
    let mut sanitized = trim(value)
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
                ch
            } else {
                '_'
            }
        })
        .collect::<String>();
    if sanitized.len() > 40 {
        sanitized.truncate(40);
    }
    sanitized.trim_matches('_').trim_matches('-').to_owned()
}

async fn ensure_official_process(process: &mut Option<OfficialMcpProcess>) -> Result<()> {
    if process
        .as_ref()
        .map(|item| item.initialized)
        .unwrap_or(false)
    {
        if let Some(process_ref) = process.as_mut() {
            if process_ref.child.try_wait()?.is_some() {
                *process = Some(spawn_official_mcp_process().await?);
            }
        }
    }
    if process.is_none() {
        *process = Some(spawn_official_mcp_process().await?);
    }
    Ok(())
}

async fn official_adapter_ping(
    task_id: &str,
    place_id: &str,
    process: &mut Option<OfficialMcpProcess>,
) -> Result<String> {
    ensure_official_process(process).await?;
    let process_ref = process
        .as_mut()
        .ok_or_else(|| eyre!("official MCP process was not created"))?;
    process_ref.initialize().await?;
    let studio_summary = process_ref.ensure_active_studio().await?;
    let tools = process_ref.tools_list().await?;
    let tools_payload = tools
        .get("tools")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let available_tool_names = tools_payload
        .iter()
        .filter_map(|tool| {
            tool.get("name")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned)
        })
        .collect::<HashSet<_>>();
    let missing_allowed_tools = ALLOWED_OFFICIAL_MCP_TOOLS
        .iter()
        .copied()
        .filter(|tool| !available_tool_names.contains(*tool))
        .collect::<Vec<_>>();
    Ok(serde_json::to_string_pretty(&serde_json::json!({
        "ok": true,
        "adapter_state": "ready",
        "task_id": task_id,
        "place_id": place_id,
        "studio": studio_summary,
        "official": {
            "tool_count": tools_payload.len(),
            "allowed_tool_names": ALLOWED_OFFICIAL_MCP_TOOLS,
            "missing_allowed_tools": missing_allowed_tools,
        },
    }))?)
}

impl OfficialMcpProcess {
    async fn initialize(&mut self) -> Result<()> {
        if self.initialized {
            return Ok(());
        }
        let response = self
            .request(
                "initialize",
                serde_json::json!({
                    "protocolVersion": "2025-11-25",
                    "capabilities": {},
                    "clientInfo": {
                        "name": "clockp-helper-official-adapter",
                        "version": env!("CARGO_PKG_VERSION"),
                    },
                }),
            )
            .await?;
        tracing::info!(
            server_info = ?response.get("serverInfo"),
            "initialized official StudioMCP.exe"
        );
        self.notify("notifications/initialized", serde_json::json!({}))
            .await?;
        self.initialized = true;
        Ok(())
    }

    async fn ensure_active_studio(&mut self) -> Result<Value> {
        let listed = self
            .call_tool("list_roblox_studios", serde_json::json!({}))
            .await?;
        let studio_list: OfficialStudioList = parse_official_text_json(&listed)?;
        let studio = select_official_studio(&studio_list)?;
        self.call_tool(
            "set_active_studio",
            serde_json::json!({ "studio_id": studio.id }),
        )
        .await?;
        self.studio_id = Some(studio.id.clone());
        Ok(serde_json::json!({
            "studio_id": studio.id,
            "studio_name": studio.name,
            "studio_count": studio_list.studios.len(),
            "note": studio_list.note,
            "set_active": true,
        }))
    }

    async fn tools_list(&mut self) -> Result<Value> {
        self.request("tools/list", serde_json::json!({})).await
    }

    async fn call_tool(&mut self, name: &str, arguments: Value) -> Result<Value> {
        let response = self
            .request(
                "tools/call",
                serde_json::json!({
                    "name": name,
                    "arguments": arguments,
                }),
            )
            .await?;
        let call_response: OfficialToolCallResponse = serde_json::from_value(response.clone())?;
        if call_response.is_error {
            return Err(eyre!(
                "official MCP tool {} returned error: {}",
                name,
                summarize_official_content(&call_response.content)
            ));
        }
        Ok(response)
    }

    async fn request(&mut self, method: &str, params: Value) -> Result<Value> {
        self.next_id += 1;
        let id = self.next_id;
        let payload = serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        self.write_json_line(&payload).await?;
        loop {
            let Some(line) = self.stdout_lines.next_line().await? else {
                return Err(eyre!("official MCP process closed stdout"));
            };
            let response: Value = serde_json::from_str(&line)
                .wrap_err_with(|| format!("official MCP returned non-JSON line: {line}"))?;
            if response.get("id").and_then(Value::as_u64) != Some(id) {
                continue;
            }
            if let Some(error) = response.get("error") {
                return Err(eyre!("official MCP {method} failed: {error}"));
            }
            return response
                .get("result")
                .cloned()
                .ok_or_else(|| eyre!("official MCP {method} response missing result"));
        }
    }

    async fn notify(&mut self, method: &str, params: Value) -> Result<()> {
        let payload = serde_json::json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        });
        self.write_json_line(&payload).await
    }

    async fn write_json_line(&mut self, payload: &Value) -> Result<()> {
        let encoded = serde_json::to_string(payload)?;
        self.stdin.write_all(encoded.as_bytes()).await?;
        self.stdin.write_all(b"\n").await?;
        self.stdin.flush().await?;
        Ok(())
    }

    async fn kill(&mut self) {
        let _ = self.child.start_kill();
        let _ = self.child.wait().await;
    }
}

fn parse_official_text_json<T: DeserializeOwned>(response: &Value) -> Result<T> {
    let call_response: OfficialToolCallResponse = serde_json::from_value(response.clone())?;
    let text = call_response
        .content
        .iter()
        .find_map(|item| {
            (item.item_type == "text")
                .then(|| item.text.clone())
                .flatten()
        })
        .ok_or_else(|| eyre!("official MCP response did not include text content"))?;
    Ok(serde_json::from_str(&text)?)
}

fn summarize_official_content(content: &[OfficialContentItem]) -> String {
    content
        .iter()
        .filter_map(|item| item.text.as_deref())
        .collect::<Vec<_>>()
        .join("\n")
}

fn select_official_studio(list: &OfficialStudioList) -> Result<OfficialStudioInfo> {
    if list.studios.is_empty() {
        return Err(eyre!(
            "official MCP found no connected Roblox Studio instances"
        ));
    }
    if list.studios.len() == 1 {
        return Ok(list.studios[0].clone());
    }
    Err(eyre!(
        "official MCP found multiple Studio instances and cannot bind safely in v1"
    ))
}

async fn spawn_official_mcp_process() -> Result<OfficialMcpProcess> {
    let executable = resolve_official_mcp_executable()?;
    let mut child = TokioCommand::new(&executable)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .wrap_err_with(|| format!("failed to spawn {}", executable.display()))?;
    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| eyre!("official MCP stdin was not captured"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| eyre!("official MCP stdout was not captured"))?;
    if let Some(stderr) = child.stderr.take() {
        tokio::spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                tracing::debug!(stderr = summarize_error(&line), "official MCP stderr");
            }
        });
    }
    Ok(OfficialMcpProcess {
        child,
        stdin,
        stdout_lines: BufReader::new(stdout).lines(),
        next_id: 0,
        initialized: false,
        studio_id: None,
    })
}

fn resolve_official_mcp_executable() -> Result<PathBuf> {
    #[cfg(target_os = "windows")]
    {
        let local_app_data =
            env::var_os("LOCALAPPDATA").ok_or_else(|| eyre!("LOCALAPPDATA is not set"))?;
        let roblox_dir = PathBuf::from(local_app_data).join("Roblox");
        let mcp_bat = roblox_dir.join("mcp.bat");
        if let Ok(content) = fs::read_to_string(&mcp_bat) {
            for fragment in content.split('"') {
                if fragment.ends_with("StudioMCP.exe") {
                    let path = PathBuf::from(fragment);
                    if path.is_file() {
                        return Ok(path);
                    }
                }
            }
        }
        let versions_dir = roblox_dir.join("Versions");
        if versions_dir.is_dir() {
            for entry in fs::read_dir(&versions_dir)? {
                let candidate = entry?.path().join("StudioMCP.exe");
                if candidate.is_file() {
                    return Ok(candidate);
                }
            }
        }
        Err(eyre!(
            "cannot locate StudioMCP.exe under {}",
            roblox_dir.display()
        ))
    }
    #[cfg(not(target_os = "windows"))]
    {
        Err(eyre!(
            "official StudioMCP.exe adapter is only available on Windows helper"
        ))
    }
}
