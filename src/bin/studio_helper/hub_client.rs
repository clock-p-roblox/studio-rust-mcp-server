async fn hub_post_json<TRequest: Serialize, TResponse: DeserializeOwned>(
    helper: &HelperConfig,
    path: &str,
    payload: &TRequest,
) -> Result<TResponse> {
    let Some(base_url) = helper.hub_base_url.as_ref() else {
        return Err(eyre!("hub_base_url is not configured"));
    };
    let token = helper.bearer_token.lock().await.clone();
    let url = format!("{}{path}", base_url.trim_end_matches('/'));
    let request = helper.client.post(&url).bearer_auth(token).json(payload);
    let (status, body) =
        await_observable_upstream_result("hub_http", path.to_owned(), async move {
            let response = request.send().await?;
            let status = response.status();
            let body = response.text().await?;
            Result::<(StatusCode, String)>::Ok((status, body))
        })
        .await?;
    if !status.is_success() {
        return Err(eyre!("hub request {path} failed with {status}: {body}"));
    }
    Ok(serde_json::from_str(&body)?)
}

fn parse_helper_id_conflict(
    status: StatusCode,
    error_code: Option<&str>,
    body: &str,
) -> Option<String> {
    if status != StatusCode::CONFLICT {
        return None;
    }
    if error_code == Some("helper_id_conflict") {
        return Some(trim(body).to_owned());
    }
    let parsed: HubErrorResponse = serde_json::from_str(body).ok()?;
    if parsed.code.as_deref() != Some("helper_id_conflict") {
        return None;
    }
    Some(
        parsed
            .message
            .unwrap_or_else(|| "helper_id already active".to_owned()),
    )
}

async fn hub_register_helper(
    helper: &HelperConfig,
) -> std::result::Result<RegisterHelperHubResponse, RegisterHelperError> {
    let Some(base_url) = helper.hub_base_url.as_ref() else {
        return Err(RegisterHelperError::Other(eyre!(
            "hub_base_url is not configured"
        )));
    };
    let token = helper.bearer_token.lock().await.clone();
    let request = helper
        .client
        .post(format!(
            "{}/v1/helpers/register",
            base_url.trim_end_matches('/')
        ))
        .bearer_auth(token)
        .json(&RegisterHelperHubRequest {
            helper_id: helper.helper_id.clone(),
            owner_user: helper.user_name.clone(),
            platform: std::env::consts::OS.to_owned(),
            capacity: helper.capacity,
            labels: vec![std::env::consts::ARCH.to_owned()],
        });
    let (status, error_code, body) = await_observable_upstream_result(
        "hub_http",
        "/v1/helpers/register".to_owned(),
        async move {
            let response = request.send().await?;
            let status = response.status();
            let error_code = response
                .headers()
                .get("x-clock-p-hub-error")
                .and_then(|value| value.to_str().ok())
                .map(ToOwned::to_owned);
            let body = response.text().await?;
            Result::<(StatusCode, Option<String>, String)>::Ok((status, error_code, body))
        },
    )
    .await
    .map_err(RegisterHelperError::Other)?;
    if let Some(message) = parse_helper_id_conflict(status, error_code.as_deref(), &body) {
        return Err(RegisterHelperError::HelperIdConflict(message));
    }
    if !status.is_success() {
        return Err(RegisterHelperError::Other(eyre!(
            "hub request /v1/helpers/register failed with {status}: {body}"
        )));
    }
    serde_json::from_str(&body).map_err(|error| RegisterHelperError::Other(error.into()))
}

async fn hub_helper_heartbeat(
    helper: &HelperConfig,
    active_task_ids: Vec<String>,
    active_tasks: Vec<HelperHeartbeatTaskStatus>,
) -> Result<HelperHeartbeatHubResponse> {
    hub_post_json(
        helper,
        "/v1/helpers/heartbeat",
        &HelperHeartbeatHubRequest {
            helper_id: helper.helper_id.clone(),
            active_task_ids,
            active_tasks,
        },
    )
    .await
}

async fn hub_claim_task(helper: &HelperConfig) -> Result<ClaimTaskHubResponse> {
    hub_post_json(
        helper,
        "/v1/helpers/claim",
        &ClaimTaskHubRequest {
            helper_id: helper.helper_id.clone(),
        },
    )
    .await
}
