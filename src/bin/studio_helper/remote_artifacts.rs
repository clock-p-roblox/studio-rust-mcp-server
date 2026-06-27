fn encode_remote_message(message: &HelperToServerMessage) -> Result<String> {
    Ok(serde_json::to_string(message)?)
}

fn build_artifact_chunk_message(upload_id: Uuid, seq: u32, chunk: &[u8]) -> Result<String> {
    encode_remote_message(&HelperToServerMessage::ArtifactChunk(ArtifactChunk {
        upload_id: upload_id.to_string(),
        seq,
        data_base64: base64::engine::general_purpose::STANDARD.encode(chunk),
    }))
}

#[derive(Debug, Deserialize)]
struct TakeScreenshotToolArgs {
    session_id: Option<String>,
    runtime_id: Option<String>,
    tag: Option<String>,
}

fn send_artifact_abort_message(
    sender: &mpsc::UnboundedSender<RemoteOutgoingFrame>,
    upload_id: Uuid,
    request_id: &str,
    error: &str,
) {
    if let Ok(encoded) =
        encode_remote_message(&HelperToServerMessage::ArtifactAbort(ArtifactAbort {
            upload_id: upload_id.to_string(),
            request_id: request_id.to_owned(),
            error: error.to_owned(),
        }))
    {
        let _ = sender.send(RemoteOutgoingFrame::Text(encoded));
    }
}

async fn stream_runtime_screenshot(
    app: AppState,
    place_id: &str,
    task_id: Option<&str>,
    args: TakeScreenshotToolArgs,
    sender: mpsc::UnboundedSender<RemoteOutgoingFrame>,
    upload_waiters: Arc<Mutex<HashMap<String, oneshot::Sender<Result<ArtifactCommitted>>>>>,
    request_id: String,
) -> Result<String> {
    let session_id = sanitize_identifier(
        "session_id",
        args.session_id
            .as_deref()
            .ok_or_else(|| eyre!("take_screenshot requires session_id"))?,
    )?;
    let runtime_id =
        sanitize_identifier("runtime_id", args.runtime_id.as_deref().unwrap_or("server"))?;
    let tag = args
        .tag
        .as_deref()
        .map(|value| sanitize_identifier("tag", value))
        .transpose()?;
    let captured = capture_screenshot_png(app, Some(place_id), task_id).await?;
    let upload_id = Uuid::new_v4();
    let (tx, rx) = oneshot::channel();
    upload_waiters
        .lock()
        .await
        .insert(upload_id.to_string(), tx);
    let upload_target = format!(
        "request_id={request_id} upload_id={upload_id} place_id={place_id} task_id={}",
        task_id.unwrap_or("")
    );

    if sender
        .send(RemoteOutgoingFrame::Text(encode_remote_message(
            &HelperToServerMessage::ArtifactBegin(ArtifactBegin {
                upload_id: upload_id.to_string(),
                request_id: request_id.clone(),
                session_id: session_id.clone(),
                runtime_id: runtime_id.clone(),
                place_id: place_id.to_owned(),
                task_id: task_id.map(ToOwned::to_owned),
                tag: tag.clone(),
                content_type: "image/png".to_owned(),
                total_bytes: captured.png_bytes.len(),
            }),
        )?))
        .is_err()
    {
        upload_waiters.lock().await.remove(&upload_id.to_string());
        return Err(eyre!(
            "remote websocket sender dropped before artifact begin"
        ));
    }

    for (seq, chunk) in captured
        .png_bytes
        .chunks(MAX_ARTIFACT_CHUNK_PAYLOAD_BYTES)
        .enumerate()
    {
        if sender
            .send(RemoteOutgoingFrame::Text(build_artifact_chunk_message(
                upload_id, seq as u32, chunk,
            )?))
            .is_err()
        {
            upload_waiters.lock().await.remove(&upload_id.to_string());
            send_artifact_abort_message(
                &sender,
                upload_id,
                &request_id,
                "remote websocket sender dropped during artifact upload",
            );
            return Err(eyre!(
                "remote websocket sender dropped during artifact upload"
            ));
        }
    }

    if sender
        .send(RemoteOutgoingFrame::Text(encode_remote_message(
            &HelperToServerMessage::ArtifactFinish(ArtifactFinish {
                upload_id: upload_id.to_string(),
                request_id: request_id.clone(),
                total_chunks: captured
                    .png_bytes
                    .chunks(MAX_ARTIFACT_CHUNK_PAYLOAD_BYTES)
                    .len() as u32,
            }),
        )?))
        .is_err()
    {
        upload_waiters.lock().await.remove(&upload_id.to_string());
        send_artifact_abort_message(
            &sender,
            upload_id,
            &request_id,
            "remote websocket sender dropped before artifact finish",
        );
        return Err(eyre!(
            "remote websocket sender dropped before artifact finish"
        ));
    }

    let committed =
        await_observable_upstream_result("remote_artifact_upload", upload_target, async {
            match tokio::time::timeout(Duration::from_secs(30), rx).await {
                Ok(result) => result?,
                Err(_) => {
                    upload_waiters.lock().await.remove(&upload_id.to_string());
                    send_artifact_abort_message(
                        &sender,
                        upload_id,
                        &request_id,
                        "timed out waiting for artifact commit acknowledgement",
                    );
                    Err(eyre!(
                        "timed out waiting for artifact commit acknowledgement"
                    ))
                }
            }
        })
        .await?;

    let response = RuntimeScreenshotResponse {
        session_id: committed.session_id,
        runtime_id: committed.runtime_id,
        place_id: committed.place_id,
        screenshot_path: committed.screenshot_path,
        screenshot_rel_path: Some(committed.screenshot_rel_path),
        artifact_dir: committed.artifact_dir,
        session_metadata_path: committed.session_metadata_path,
        bytes_written: committed.bytes_written,
    };
    Ok(serde_json::to_string(&response)?)
}

async fn run_remote_ws_session(
    app: AppState,
    connection_key: &str,
    worker_id: Uuid,
    place_id: &str,
    task_id: Option<&str>,
    ws_url: &str,
    stop_rx: &mut watch::Receiver<bool>,
) -> Result<()> {
    let stream = connect_remote_ws(&app.helper, ws_url).await?;
    {
        let state = app.state.lock().await;
        if *stop_rx.borrow() || !remote_connection_matches_worker(&state, connection_key, worker_id)
        {
            return Ok(());
        }
    }
    let (mut writer, mut reader) = stream.split();
    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<RemoteOutgoingFrame>();
    let mut last_server_message_at = Instant::now();
    let mut last_heartbeat_sent_at = Instant::now();
    let mut heartbeat_count: u64 = 0;
    let mut stopped_by_request = false;
    {
        let mut state = app.state.lock().await;
        if let Some(connection) = state.remote_connections.get_mut(connection_key) {
            if connection.worker_id == worker_id {
                connection.sender = out_tx.clone();
            }
        }
        set_remote_connection_connecting(&mut state, connection_key, worker_id);
        state.last_remote_errors.remove(connection_key);
    }
    let writer_task = tokio::spawn(async move {
        while let Some(frame) = out_rx.recv().await {
            let message = match frame {
                RemoteOutgoingFrame::Text(text) => WsMessage::Text(text),
                RemoteOutgoingFrame::Ping(payload) => WsMessage::Ping(payload),
                RemoteOutgoingFrame::Pong(payload) => WsMessage::Pong(payload),
            };
            if let Err(error) = writer.send(message).await {
                tracing::warn!(
                    error = summarize_error(&error.to_string()),
                    "helper remote websocket writer failed"
                );
                break;
            }
        }
    });
    let hello_sent_at = Instant::now();
    if out_tx
        .send(RemoteOutgoingFrame::Text(encode_remote_message(
            &HelperToServerMessage::Hello(HelperHello {
                helper_id: app.helper.helper_id.clone(),
                place_id: place_id.to_owned(),
                task_id: task_id.map(ToOwned::to_owned),
                helper_version: env!("CARGO_PKG_VERSION").to_owned(),
                capabilities: vec![
                    "ws_tool_dispatch_v1".to_owned(),
                    "runtime_screenshot_stream_v1".to_owned(),
                    "read_studio_log_v1".to_owned(),
                    OFFICIAL_MCP_ADAPTER_CAPABILITY.to_owned(),
                    OFFICIAL_MCP_STORE_IMAGE_BASE64_CAPABILITY.to_owned(),
                ],
                plugin_instance_count: {
                    let state = app.state.lock().await;
                    state
                        .instances
                        .values()
                        .filter(|instance| {
                            if let Some(expected_task_id) = task_id {
                                instance.task_id.as_deref() == Some(expected_task_id)
                            } else {
                                instance.place_id == place_id
                            }
                        })
                        .count()
                },
                task_status: {
                    let state = app.state.lock().await;
                    task_id.map(|task_id| helper_task_status_snapshot(&state, task_id))
                },
            }),
        )?))
        .is_err()
    {
        writer_task.abort();
        return Err(eyre!("remote websocket sender dropped before hello"));
    }

    let upload_waiters: Arc<Mutex<HashMap<String, oneshot::Sender<Result<ArtifactCommitted>>>>> =
        Arc::new(Mutex::new(HashMap::new()));
    let mut heartbeat = tokio::time::interval(Duration::from_secs(5));
    let mut ping = tokio::time::interval(REMOTE_WS_PING_INTERVAL);
    let mut last_pong_at = Instant::now();
    let mut ready_acknowledged = false;
    let mut hello_slow_logged = false;
    let mut hello_slow_timer = Box::pin(tokio::time::sleep(OBSERVABLE_UPSTREAM_SLOW_AFTER));
    let mut session_error: Option<Report> = None;
    loop {
        tokio::select! {
            _ = &mut hello_slow_timer, if !ready_acknowledged && !hello_slow_logged => {
                hello_slow_logged = true;
                tracing::warn!(
                    connection_key,
                    place_id,
                    task_id = ?task_id,
                    threshold_ms = OBSERVABLE_UPSTREAM_SLOW_AFTER.as_millis(),
                    elapsed_ms = hello_sent_at.elapsed().as_millis(),
                    result_observable = true,
                    "helper remote websocket hello still waiting for ready_ack"
                );
            }
            changed = stop_rx.changed() => {
                if changed.is_ok() && *stop_rx.borrow() {
                    stopped_by_request = true;
                    break;
                }
            }
            _ = heartbeat.tick() => {
                let plugin_instance_count = {
                    let state = app.state.lock().await;
                    state.instances.values().filter(|instance| {
                        if let Some(expected_task_id) = task_id {
                            instance.task_id.as_deref() == Some(expected_task_id)
                        } else {
                            instance.place_id == place_id
                        }
                    }).count()
                };
                let task_status = {
                    let state = app.state.lock().await;
                    task_id.map(|task_id| helper_task_status_snapshot(&state, task_id))
                };
                heartbeat_count += 1;
                last_heartbeat_sent_at = Instant::now();
                tracing::debug!(
                    place_id,
                    task_id = ?task_id,
                    heartbeat_count,
                    plugin_instance_count,
                    studio_control_state = ?task_status.as_ref().and_then(|status| status.studio_control_state.clone()),
                    studio_transition_phase = ?task_status.as_ref().and_then(|status| status.studio_transition_phase.clone()),
                    official_mcp_adapter_state = ?task_status.as_ref().and_then(|status| status.official_mcp_adapter_state.clone()),
                    official_mcp_adapter_age_ms = ?task_status.as_ref().and_then(|status| status.official_mcp_adapter_age_ms),
                    idle_from_server_ms = last_server_message_at.elapsed().as_millis(),
                    "sending helper remote websocket heartbeat"
                );
                if out_tx.send(RemoteOutgoingFrame::Text(encode_remote_message(&HelperToServerMessage::Heartbeat {
                    helper_id: app.helper.helper_id.clone(),
                    place_id: place_id.to_owned(),
                    task_id: task_id.map(ToOwned::to_owned),
                    plugin_instance_count,
                    task_status,
                })?)).is_err() {
                    tracing::warn!(place_id, heartbeat_count, "failed to queue helper remote websocket heartbeat");
                    break;
                }
            }
            _ = ping.tick() => {
                if last_pong_at.elapsed() > REMOTE_WS_PONG_TIMEOUT {
                    session_error = Some(eyre!(
                        "remote websocket pong timed out after {}ms",
                        last_pong_at.elapsed().as_millis()
                    ));
                    break;
                }
                let payload = Uuid::new_v4().as_bytes().to_vec();
                if out_tx.send(RemoteOutgoingFrame::Ping(payload)).is_err() {
                    tracing::warn!(place_id, "failed to queue helper remote websocket ping");
                    break;
                }
            }
            message = reader.next() => {
                let Some(message) = message else {
                    break;
                };
                match message? {
                    WsMessage::Text(text) => {
                        last_server_message_at = Instant::now();
                        {
                            let mut state = app.state.lock().await;
                            note_remote_connection_activity(&mut state, connection_key, worker_id);
                        }
                        match serde_json::from_str::<ServerToHelperMessage>(&text)? {
                            ServerToHelperMessage::ReadyAck { connection_id, place_id: ack_place_id, task_id: ack_task_id } => {
                                validate_remote_ready_ack(place_id, task_id, &ack_place_id, ack_task_id.as_deref())?;
                                {
                                    let mut state = app.state.lock().await;
                                    set_remote_connection_connected(&mut state, connection_key, worker_id, &connection_id);
                                }
                                ready_acknowledged = true;
                                if let Some(task_id) = task_id {
                                    start_official_adapter_prewarm(app.clone(), task_id.to_owned(), place_id.to_owned());
                                }
                                tracing::info!(connection_key, place_id, connection_id, "helper websocket acknowledged by MCP server");
                            }
                            ServerToHelperMessage::ToolCall { request_id, command } => {
                                if !ready_acknowledged {
                                    return Err(eyre!("remote MCP server sent tool call before ready ack"));
                                }
                                let tool_sender = out_tx.clone();
                                let tool_waiters = Arc::clone(&upload_waiters);
                                let tool_app = app.clone();
                                let tool_place = place_id.to_owned();
                                let tool_task = task_id.map(ToOwned::to_owned);
                                tokio::spawn(async move {
                                    let result = handle_remote_command(
                                        tool_app,
                                        &tool_place,
                                        tool_task.as_deref(),
                                        command,
                                        tool_sender.clone(),
                                        tool_waiters,
                                        request_id.clone(),
                                    ).await;
                                    let message = match result {
                                        Ok(body) => HelperToServerMessage::ToolResult { request_id, response: body },
                                        Err(error) => HelperToServerMessage::ToolError { request_id, error: error.to_string() },
                                    };
                                    if let Ok(encoded) = encode_remote_message(&message) {
                                        let _ = tool_sender.send(RemoteOutgoingFrame::Text(encoded));
                                    }
                                });
                            }
                            ServerToHelperMessage::OfficialMcpRequest(request) => {
                                if !ready_acknowledged {
                                    return Err(eyre!("remote MCP server sent official MCP request before ready ack"));
                                }
                                let official_sender = out_tx.clone();
                                let official_app = app.clone();
                                let expected_place_id = place_id.to_owned();
                                let expected_task_id = task_id.map(ToOwned::to_owned);
                                tokio::spawn(async move {
                                    let request_id = request.request_id.clone();
                                    let result = handle_official_mcp_request(
                                        official_app,
                                        &expected_place_id,
                                        expected_task_id.as_deref(),
                                        request,
                                    ).await;
                                    let message = match result {
                                        Ok(response) => HelperToServerMessage::OfficialMcpResponse(OfficialMcpResponse {
                                            request_id,
                                            response,
                                        }),
                                        Err(error) => HelperToServerMessage::OfficialMcpError {
                                            request_id,
                                            error: error.to_string(),
                                        },
                                    };
                                    if let Ok(encoded) = encode_remote_message(&message) {
                                        let _ = official_sender.send(RemoteOutgoingFrame::Text(encoded));
                                    }
                                });
                            }
                            ServerToHelperMessage::ArtifactCommitted(committed) => {
                                if !ready_acknowledged {
                                    return Err(eyre!("remote MCP server sent artifact committed before ready ack"));
                                }
                                if let Some(tx) = upload_waiters.lock().await.remove(&committed.upload_id) {
                                    let _ = tx.send(Ok(committed));
                                }
                            }
                            ServerToHelperMessage::ArtifactFailed { upload_id, error, .. } => {
                                if !ready_acknowledged {
                                    return Err(eyre!("remote MCP server sent artifact failed before ready ack"));
                                }
                                if let Some(tx) = upload_waiters.lock().await.remove(&upload_id) {
                                    let _ = tx.send(Err(eyre!(error)));
                                }
                            }
                            ServerToHelperMessage::CloseReason { reason } => {
                                return Err(eyre!("remote MCP server closed helper websocket: {reason}"));
                            }
                        }
                    }
                    WsMessage::Ping(payload) => {
                        last_server_message_at = Instant::now();
                        {
                            let mut state = app.state.lock().await;
                            note_remote_connection_activity(&mut state, connection_key, worker_id);
                        }
                        let _ = out_tx.send(RemoteOutgoingFrame::Pong(payload));
                    }
                    WsMessage::Close(_) => break,
                    WsMessage::Pong(_) => {
                        last_pong_at = Instant::now();
                        last_server_message_at = Instant::now();
                        {
                            let mut state = app.state.lock().await;
                            note_remote_connection_activity(&mut state, connection_key, worker_id);
                        }
                    }
                    WsMessage::Binary(_) | WsMessage::Frame(_) => {
                        last_server_message_at = Instant::now();
                        {
                            let mut state = app.state.lock().await;
                            note_remote_connection_activity(&mut state, connection_key, worker_id);
                        }
                    }
                }
            }
        }
    }
    writer_task.abort();
    let mut pending = upload_waiters.lock().await;
    for (_, tx) in pending.drain() {
        let _ = tx.send(Err(eyre!(
            "remote websocket session closed before artifact commit"
        )));
    }
    tracing::warn!(
        connection_key,
        place_id,
        heartbeat_count,
        last_server_message_age_ms = last_server_message_at.elapsed().as_millis(),
        last_heartbeat_sent_age_ms = last_heartbeat_sent_at.elapsed().as_millis(),
        pending_upload_waiters = pending.len(),
        "helper remote websocket session ended"
    );
    if let Some(task_id) = task_id {
        if stop_and_remove_official_adapter(&app.state, task_id).await {
            tracing::info!(
                task_id,
                "stopped official MCP adapter after remote websocket ended"
            );
        }
    }
    if stopped_by_request {
        Ok(())
    } else if let Some(error) = session_error {
        Err(error)
    } else {
        Err(eyre!("remote websocket session ended unexpectedly"))
    }
}
