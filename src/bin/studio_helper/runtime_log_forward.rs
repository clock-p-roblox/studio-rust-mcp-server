use super::{await_observable_upstream_result, select_claimed_task, AppState, HelperError, Result};
use crate::text::{sanitize_identifier, sanitize_place_id};
use crate::urls::rojo_forward_target_path;
use axum::body::{Body, Bytes};
use axum::extract::{Path as AxumPath, State};
use axum::http::{HeaderMap, Method, StatusCode, Uri};
use axum::response::Response;
use color_eyre::eyre::{eyre, WrapErr};
use reqwest::header::CONTENT_TYPE;

async fn resolve_runtime_log_forward_target_base_url(
    app: &AppState,
    place_id: &str,
    task_id: &str,
) -> Result<String> {
    let state = app.state.lock().await;
    let claimed_task = select_claimed_task(&state, place_id, Some(task_id))?;
    claimed_task
        .runtime_log_base_url
        .ok_or_else(|| eyre!("claimed task has no runtime_log_base_url"))
}

pub(super) async fn runtime_log_forward_task_path_handler(
    State(app): State<AppState>,
    AxumPath((place_id, task_id, path)): AxumPath<(String, String, String)>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, HelperError> {
    runtime_log_forward_request(app, place_id, task_id, path, method, uri, headers, body).await
}

#[allow(clippy::too_many_arguments)]
async fn runtime_log_forward_request(
    app: AppState,
    place_id: String,
    task_id: String,
    path: String,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, HelperError> {
    let place_id = sanitize_place_id(&place_id)?;
    let task_id = sanitize_identifier("task_id", &task_id)?;
    let target_base_url =
        resolve_runtime_log_forward_target_base_url(&app, &place_id, &task_id).await?;
    let target_path = rojo_forward_target_path(&path, uri.query());
    let target_url = format!("{}{}", target_base_url.trim_end_matches('/'), target_path);
    let bearer_token = app.helper.bearer_token.lock().await.clone();
    let reqwest_method = reqwest::Method::from_bytes(method.as_str().as_bytes())
        .wrap_err_with(|| format!("unsupported runtime-log forward method {method}"))?;
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
    let (status, content_type, bytes) = await_observable_upstream_result(
        "runtime_log_forward_http",
        target_path.clone(),
        async move {
            let response = request.send().await.wrap_err_with(|| {
                format!("failed to forward runtime-log request to {target_url}")
            })?;
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
                .wrap_err("failed to read runtime-log forward response body")?;
            Result::<(StatusCode, Option<String>, Bytes)>::Ok((status, content_type, bytes))
        },
    )
    .await?;
    tracing::debug!(
        place_id,
        task_id,
        method = method.as_str(),
        target_path,
        status = status.as_u16(),
        bytes = bytes.len(),
        "forwarded runtime-log request through helper"
    );
    let mut builder = Response::builder().status(status);
    if let Some(content_type) = content_type {
        builder = builder.header(CONTENT_TYPE, content_type);
    }
    builder
        .body(Body::from(bytes))
        .wrap_err("failed to build runtime-log forward response")
        .map_err(HelperError)
}
