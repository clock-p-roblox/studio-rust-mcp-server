fn validate_runtime_screenshot_upload_url(
    upload_url: &str,
    place_id: &str,
    task_id: Option<&str>,
    state: &HelperState,
    helper: &HelperConfig,
) -> Result<String> {
    let trimmed = trim(upload_url);
    if trimmed.is_empty() {
        return Err(eyre!("upload_url must not be empty"));
    }

    let claimed_task = select_claimed_task(state, place_id, task_id)?;
    let expected_remote = claimed_task
        .runtime_log_base_url
        .as_deref()
        .map(runtime_screenshot_upload_url_for_base_url)
        .ok_or_else(|| eyre!("claimed task has no runtime_log_base_url"))?;

    let expected_helper =
        runtime_screenshot_helper_upload_url(helper.port, place_id, &claimed_task.task_id);
    if trimmed != expected_helper {
        return Err(eyre!(
            "upload_url must use local helper runtime screenshot forward for placeId {place_id}: {expected_helper}"
        ));
    }

    Ok(expected_remote)
}

async fn post_runtime_screenshot(
    helper: &HelperConfig,
    upload_url: &str,
    session_id: &str,
    runtime_id: &str,
    place_id: &str,
    tag: Option<&str>,
    png_bytes: &[u8],
) -> Result<RuntimeScreenshotSinkResponse> {
    let current_token = helper.bearer_token.lock().await.clone();
    let current_source = helper.bearer_token_source.lock().await.clone();
    let mut attempts = vec![ResolvedToken {
        value: current_token.clone(),
        source: current_source,
    }];
    for candidate in helper.bearer_token_candidates.iter() {
        if candidate.value != current_token {
            attempts.push(candidate.clone());
        }
    }

    let normalized_tag = tag.map(trim).filter(|value| !value.is_empty());
    let mut saw_unauthorized = false;
    for candidate in attempts {
        let mut request = helper
            .client
            .post(upload_url)
            .header(AUTHORIZATION, format!("Bearer {}", candidate.value))
            .header(CONTENT_TYPE, "image/png")
            .header("X-Session-Id", session_id)
            .header("X-Runtime-Id", runtime_id)
            .header("X-Place-Id", place_id);
        if let Some(tag) = normalized_tag.as_deref() {
            request = request.header("X-Screenshot-Tag", tag);
        }

        let upload_target = format!("runtime-screenshot-upstream source={}", candidate.source);
        let (status, response_body) = await_observable_upstream_result(
            "runtime_screenshot_upload",
            upload_target,
            async move {
                let response = request
                    .body(png_bytes.to_vec())
                    .send()
                    .await
                    .map_err(|_| eyre!("runtime screenshot upstream request failed"))?;
                let status = response.status();
                let response_body = response
                    .text()
                    .await
                    .map_err(|_| eyre!("runtime screenshot upstream response read failed"))?;
                Result::<(StatusCode, String)>::Ok((status, response_body))
            },
        )
        .await?;
        if status == StatusCode::UNAUTHORIZED {
            saw_unauthorized = true;
            tracing::warn!(
                source = candidate.source,
                "runtime screenshot upload was unauthorized; trying next token candidate"
            );
            continue;
        }
        if !status.is_success() {
            return Err(eyre!("runtime screenshot upload returned {status}"));
        }

        let sink_response: RuntimeScreenshotSinkResponse = serde_json::from_str(&response_body)
            .wrap_err("runtime screenshot upload returned invalid JSON")?;
        if !sink_response.ok {
            return Err(eyre!("runtime screenshot upload returned ok=false"));
        }
        if sink_response.session_id != session_id {
            return Err(eyre!(
                "runtime screenshot upload returned mismatched session_id: expected {session_id}, got {}",
                sink_response.session_id
            ));
        }
        if sink_response.runtime_id != runtime_id {
            return Err(eyre!(
                "runtime screenshot upload returned mismatched runtime_id: expected {runtime_id}, got {}",
                sink_response.runtime_id
            ));
        }

        if candidate.value != current_token {
            *helper.bearer_token.lock().await = candidate.value.clone();
            *helper.bearer_token_source.lock().await = candidate.source.clone();
            tracing::warn!(
                source = candidate.source,
                "helper switched bearer token after runtime screenshot upload unauthorized"
            );
        }
        return Ok(sink_response);
    }

    if saw_unauthorized {
        return Err(eyre!(
            "runtime screenshot upload returned unauthorized for all token candidates"
        ));
    }
    Err(eyre!("runtime screenshot upload could not complete"))
}

