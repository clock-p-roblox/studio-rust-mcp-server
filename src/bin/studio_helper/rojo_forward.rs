async fn rojo_config_handler(
    State(app): State<AppState>,
    ConnectInfo(peer_addr): ConnectInfo<SocketAddr>,
    Query(query): Query<PlaceQuery>,
) -> Result<Json<RojoConfigResponse>, HelperError> {
    let place_id = sanitize_place_id(&query.place_id)?;
    let explicit_task_id = maybe_sanitize_identifier("task_id", query.task_id.as_deref())?;
    let bearer_token = app.helper.bearer_token.lock().await.clone();
    let studio_pid = resolve_peer_process_id_with_retry(peer_addr, app.helper.port).await?;
    let claimed_task = {
        let state = app.state.lock().await;
        resolve_plugin_routing_decision(&state, &place_id, explicit_task_id.as_deref(), studio_pid)
            .map_err(HelperError)?
    };
    if let Some(studio_pid) = studio_pid {
        let mut state = app.state.lock().await;
        bind_launch_process_to_pid(&mut state, &claimed_task, studio_pid).map_err(HelperError)?;
    }
    let base_url = claimed_task
        .rojo_base_url
        .clone()
        .ok_or_else(|| HelperError(eyre!("claimed task has no rojo_base_url")))?;
    let local_base_url = rojo_forward_base_url(app.helper.port, &place_id, &claimed_task.task_id);
    tracing::info!(
        place_id,
        task_id = claimed_task.task_id,
        remote_base_url = base_url,
        base_url = local_base_url,
        "resolved rojo config from helper"
    );
    Ok(Json(RojoConfigResponse {
        place_id,
        task_id: Some(claimed_task.task_id),
        base_url: local_base_url,
        auth_header: format!("Bearer {bearer_token}"),
    }))
}

async fn resolve_rojo_forward_target_base_url(
    app: &AppState,
    place_id: &str,
    task_id: Option<&str>,
) -> Result<String> {
    let state = app.state.lock().await;
    let claimed_task = select_claimed_task(&state, place_id, task_id)?;
    claimed_task
        .rojo_base_url
        .ok_or_else(|| eyre!("claimed task has no rojo_base_url"))
}

async fn rojo_forward_root_handler(
    State(app): State<AppState>,
    AxumPath(place_id): AxumPath<String>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, HelperError> {
    rojo_forward_request(
        app,
        place_id,
        None,
        String::new(),
        method,
        uri,
        headers,
        body,
    )
    .await
}

async fn rojo_forward_path_handler(
    State(app): State<AppState>,
    AxumPath((place_id, path)): AxumPath<(String, String)>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, HelperError> {
    rojo_forward_request(app, place_id, None, path, method, uri, headers, body).await
}

async fn rojo_forward_task_root_handler(
    State(app): State<AppState>,
    AxumPath((place_id, task_id)): AxumPath<(String, String)>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, HelperError> {
    rojo_forward_request(
        app,
        place_id,
        Some(task_id),
        String::new(),
        method,
        uri,
        headers,
        body,
    )
    .await
}

async fn rojo_forward_task_path_handler(
    State(app): State<AppState>,
    AxumPath((place_id, task_id, path)): AxumPath<(String, String, String)>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, HelperError> {
    rojo_forward_request(
        app,
        place_id,
        Some(task_id),
        path,
        method,
        uri,
        headers,
        body,
    )
    .await
}

async fn rojo_forward_socket_handler(
    State(app): State<AppState>,
    AxumPath((place_id, cursor)): AxumPath<(String, String)>,
    uri: Uri,
    ws: WebSocketUpgrade,
) -> Result<Response, HelperError> {
    let place_id = sanitize_place_id(&place_id)?;
    let cursor = sanitize_identifier("cursor", &cursor)?;
    let target_base_url = resolve_rojo_forward_target_base_url(&app, &place_id, None).await?;
    let target_url = rojo_forward_target_ws_url(
        &target_base_url,
        &format!("api/socket/{cursor}"),
        uri.query(),
    )?;
    Ok(ws
        .on_upgrade(move |socket| {
            rojo_forward_socket_session(app, place_id, None, target_url, socket)
        })
        .into_response())
}

async fn rojo_forward_task_socket_handler(
    State(app): State<AppState>,
    AxumPath((place_id, task_id, cursor)): AxumPath<(String, String, String)>,
    uri: Uri,
    ws: WebSocketUpgrade,
) -> Result<Response, HelperError> {
    let place_id = sanitize_place_id(&place_id)?;
    let task_id = sanitize_identifier("task_id", &task_id)?;
    let cursor = sanitize_identifier("cursor", &cursor)?;
    let target_base_url =
        resolve_rojo_forward_target_base_url(&app, &place_id, Some(&task_id)).await?;
    let target_url = rojo_forward_target_ws_url(
        &target_base_url,
        &format!("api/socket/{cursor}"),
        uri.query(),
    )?;
    Ok(ws
        .on_upgrade(move |socket| {
            rojo_forward_socket_session(app, place_id, Some(task_id), target_url, socket)
        })
        .into_response())
}

async fn rojo_forward_socket_session(
    app: AppState,
    place_id: String,
    task_id: Option<String>,
    target_url: String,
    client_socket: WebSocket,
) {
    if let Err(error) = rojo_forward_socket_session_result(
        app,
        &place_id,
        task_id.as_deref(),
        &target_url,
        client_socket,
    )
    .await
    {
        tracing::warn!(
            place_id,
            task_id,
            target_url,
            error = %error,
            "Rojo websocket forward session ended with error"
        );
    }
}

async fn rojo_forward_socket_session_result(
    app: AppState,
    place_id: &str,
    task_id: Option<&str>,
    target_url: &str,
    client_socket: WebSocket,
) -> Result<()> {
    let remote_socket = connect_remote_ws(&app.helper, target_url)
        .await
        .wrap_err_with(|| format!("failed to connect Rojo websocket {target_url}"))?;
    let (mut client_sender, mut client_receiver) = client_socket.split();
    let (mut remote_sender, mut remote_receiver) = remote_socket.split();
    tracing::debug!(
        place_id,
        task_id,
        target_url,
        "opened Rojo websocket forward session"
    );
    loop {
        tokio::select! {
            client_message = client_receiver.next() => {
                match client_message {
                    Some(Ok(message)) => {
                        let close = matches!(message, AxumWsMessage::Close(_));
                        remote_sender
                            .send(rojo_client_ws_to_remote(message))
                            .await
                            .wrap_err("failed to send Rojo websocket message to remote")?;
                        if close {
                            break;
                        }
                    }
                    Some(Err(error)) => {
                        return Err(eyre!("failed to read Rojo websocket message from client: {error}"));
                    }
                    None => break,
                }
            }
            remote_message = remote_receiver.next() => {
                match remote_message {
                    Some(Ok(message)) => {
                        let close = matches!(message, WsMessage::Close(_));
                        if let Some(client_message) = rojo_remote_ws_to_client(message) {
                            client_sender
                                .send(client_message)
                                .await
                                .wrap_err("failed to send Rojo websocket message to client")?;
                        }
                        if close {
                            break;
                        }
                    }
                    Some(Err(error)) => {
                        return Err(eyre!("failed to read Rojo websocket message from remote: {error}"));
                    }
                    None => break,
                }
            }
        }
    }
    tracing::debug!(
        place_id,
        task_id,
        target_url,
        "closed Rojo websocket forward session"
    );
    Ok(())
}

fn rojo_client_ws_to_remote(message: AxumWsMessage) -> WsMessage {
    match message {
        AxumWsMessage::Text(text) => WsMessage::Text(text.to_string()),
        AxumWsMessage::Binary(bytes) => WsMessage::Binary(bytes.to_vec()),
        AxumWsMessage::Ping(bytes) => WsMessage::Ping(bytes.to_vec()),
        AxumWsMessage::Pong(bytes) => WsMessage::Pong(bytes.to_vec()),
        AxumWsMessage::Close(_) => WsMessage::Close(None),
    }
}

fn rojo_remote_ws_to_client(message: WsMessage) -> Option<AxumWsMessage> {
    match message {
        WsMessage::Text(text) => Some(AxumWsMessage::Text(text.into())),
        WsMessage::Binary(bytes) => Some(AxumWsMessage::Binary(bytes.into())),
        WsMessage::Ping(bytes) => Some(AxumWsMessage::Ping(bytes.into())),
        WsMessage::Pong(bytes) => Some(AxumWsMessage::Pong(bytes.into())),
        WsMessage::Close(_) => Some(AxumWsMessage::Close(None)),
        WsMessage::Frame(_) => None,
    }
}

#[allow(clippy::too_many_arguments)]
async fn rojo_forward_request(
    app: AppState,
    place_id: String,
    task_id: Option<String>,
    path: String,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, HelperError> {
    let place_id = sanitize_place_id(&place_id)?;
    let task_id = maybe_sanitize_identifier("task_id", task_id.as_deref())?;
    let target_base_url =
        resolve_rojo_forward_target_base_url(&app, &place_id, task_id.as_deref()).await?;
    let target_path = rojo_forward_target_path(&path, uri.query());
    let target_url = format!("{}{}", target_base_url.trim_end_matches('/'), target_path);
    let bearer_token = app.helper.bearer_token.lock().await.clone();
    let reqwest_method = reqwest::Method::from_bytes(method.as_str().as_bytes())
        .wrap_err_with(|| format!("unsupported Rojo forward method {method}"))?;
    let mut request = app
        .helper
        .client
        .request(reqwest_method, &target_url)
        .bearer_auth(bearer_token);
    if let Some(content_type) = headers
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
    {
        request = request.header(CONTENT_TYPE, content_type);
    }
    if !body.is_empty() {
        request = request.body(body.to_vec());
    }
    let (status, content_type, bytes) =
        await_observable_upstream_result("rojo_forward_http", target_path.clone(), async move {
            let response = request
                .send()
                .await
                .wrap_err_with(|| format!("failed to forward Rojo request to {target_url}"))?;
            let status =
                StatusCode::from_u16(response.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
            let content_type = response
                .headers()
                .get(CONTENT_TYPE)
                .and_then(|value| value.to_str().ok())
                .map(ToOwned::to_owned);
            let bytes = response
                .bytes()
                .await
                .wrap_err("failed to read Rojo forward response body")?;
            Result::<(StatusCode, Option<String>, Bytes)>::Ok((status, content_type, bytes))
        })
        .await?;
    tracing::debug!(
        place_id,
        task_id,
        method = method.as_str(),
        target_path,
        status = status.as_u16(),
        bytes = bytes.len(),
        "forwarded Rojo request through helper"
    );
    let mut builder = Response::builder().status(status);
    if let Some(content_type) = content_type {
        builder = builder.header(CONTENT_TYPE, content_type);
    }
    builder
        .body(Body::from(bytes))
        .wrap_err("failed to build Rojo forward response")
        .map_err(HelperError)
}
