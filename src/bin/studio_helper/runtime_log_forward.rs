use super::{
    await_observable_upstream_result, mark_runtime_log_forward_accepted,
    mark_runtime_log_forward_failed, mark_runtime_log_forward_rejected,
    mark_runtime_log_forward_succeeded, queue_task_status_updates, select_claimed_task, AppState,
    HelperError, Result,
};
use crate::text::{sanitize_identifier, sanitize_place_id, summarize_error};
use crate::urls::rojo_forward_target_path;
use axum::body::{Body, Bytes};
use axum::extract::{Path as AxumPath, State};
use axum::http::{HeaderMap, Method, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::Json;
use color_eyre::eyre::{eyre, WrapErr};
use reqwest::header::CONTENT_TYPE;
use serde::Serialize;
use tokio::sync::mpsc;

#[derive(Debug)]
pub(super) struct RuntimeLogForwardJob {
    place_id: String,
    task_id: String,
    method: Method,
    target_url: String,
    target_path: String,
    content_type: Option<String>,
    body: Bytes,
}

#[derive(Debug, Serialize)]
struct RuntimeLogForwardAcceptedResponse {
    ok: bool,
    accepted: bool,
    forward_succeeded: bool,
    task_id: String,
    target_path: String,
}

#[derive(Debug, Serialize)]
struct RuntimeLogForwardRejectedResponse {
    ok: bool,
    accepted: bool,
    code: &'static str,
    message: String,
    task_id: Option<String>,
    target_path: Option<String>,
}

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

fn is_runtime_log_upload(method: &Method, path: &str) -> bool {
    if method != Method::POST {
        return false;
    }
    matches!(
        path.trim_start_matches('/'),
        "v1/runtime-logs" | "v1/runtime-screenshots"
    )
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
    let target_path = rojo_forward_target_path(&path, uri.query());
    if !is_runtime_log_upload(&method, &target_path) {
        let payload = RuntimeLogForwardRejectedResponse {
            ok: false,
            accepted: false,
            code: "runtime_log_forward_unsupported_upload",
            message: format!(
                "runtime-log forward only accepts POST /v1/runtime-logs and POST /v1/runtime-screenshots; got {} {}",
                method.as_str(),
                target_path
            ),
            task_id: Some(task_id),
            target_path: Some(target_path),
        };
        return Ok((StatusCode::BAD_REQUEST, Json(payload)).into_response());
    }

    let target_base_url =
        resolve_runtime_log_forward_target_base_url(&app, &place_id, &task_id).await?;
    let target_url = format!("{}{}", target_base_url.trim_end_matches('/'), target_path);
    let content_type = headers
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .map(ToOwned::to_owned);
    let permit = match app.runtime_log_forward_tx.try_reserve() {
        Ok(permit) => permit,
        Err(error) => {
            let message = format!("runtime-log forward queue is full: {error}");
            {
                let mut state = app.state.lock().await;
                mark_runtime_log_forward_rejected(
                    &mut state,
                    &task_id,
                    target_path.clone(),
                    message.clone(),
                );
                queue_task_status_updates(&state, &app.helper, &task_id);
            }
            let payload = RuntimeLogForwardRejectedResponse {
                ok: false,
                accepted: false,
                code: "runtime_log_forward_queue_full",
                message,
                task_id: Some(task_id),
                target_path: Some(target_path),
            };
            return Ok((StatusCode::SERVICE_UNAVAILABLE, Json(payload)).into_response());
        }
    };

    {
        let mut state = app.state.lock().await;
        mark_runtime_log_forward_accepted(&mut state, &task_id, target_path.clone());
        queue_task_status_updates(&state, &app.helper, &task_id);
    }
    permit.send(RuntimeLogForwardJob {
        place_id,
        task_id: task_id.clone(),
        method,
        target_url,
        target_path: target_path.clone(),
        content_type,
        body,
    });
    tracing::debug!(
        task_id,
        target_path,
        "accepted runtime-log request for background helper forwarding"
    );
    let payload = RuntimeLogForwardAcceptedResponse {
        ok: true,
        accepted: true,
        forward_succeeded: false,
        task_id,
        target_path,
    };
    Response::builder()
        .status(StatusCode::ACCEPTED)
        .header(CONTENT_TYPE, "application/json")
        .body(Body::from(
            serde_json::to_vec(&payload).wrap_err("failed to encode runtime-log ack response")?,
        ))
        .wrap_err("failed to build runtime-log ack response")
        .map_err(HelperError)
}

pub(super) async fn runtime_log_forward_worker(
    app: AppState,
    mut rx: mpsc::Receiver<RuntimeLogForwardJob>,
) {
    while let Some(job) = rx.recv().await {
        forward_runtime_log_job(&app, job).await;
    }
}

async fn forward_runtime_log_job(app: &AppState, job: RuntimeLogForwardJob) {
    let bearer_token = app.helper.bearer_token.lock().await.clone();
    let reqwest_method = match reqwest::Method::from_bytes(job.method.as_str().as_bytes()) {
        Ok(method) => method,
        Err(error) => {
            record_runtime_log_forward_failure(
                app,
                &job.task_id,
                job.target_path,
                None,
                format!("unsupported runtime-log forward method: {error}"),
            )
            .await;
            return;
        }
    };
    let mut request = app
        .helper
        .client
        .request(reqwest_method, &job.target_url)
        .bearer_auth(bearer_token);
    if let Some(content_type) = job.content_type {
        request = request.header(CONTENT_TYPE, content_type);
    }
    if !job.body.is_empty() {
        request = request.body(job.body.to_vec());
    }
    let target_url = job.target_url.clone();
    let target_path = job.target_path.clone();
    let result = await_observable_upstream_result(
        "runtime_log_forward_http",
        target_path.clone(),
        async move {
            let response = request
                .send()
                .await
                .wrap_err_with(|| format!("failed to forward runtime-log request to {target_url}"))?;
            let status = response.status();
            let body = response
                .bytes()
                .await
                .wrap_err("failed to read runtime-log forward response body")?;
            Result::<(reqwest::StatusCode, Bytes)>::Ok((status, body))
        },
    )
    .await;

    match result {
        Ok((status, _body)) if status.is_success() => {
            let status = status.as_u16();
            tracing::debug!(
                place_id = job.place_id,
                task_id = job.task_id,
                target_path,
                status,
                "forwarded runtime-log request through helper"
            );
            let mut state = app.state.lock().await;
            mark_runtime_log_forward_succeeded(&mut state, &job.task_id, target_path, status);
            queue_task_status_updates(&state, &app.helper, &job.task_id);
        }
        Ok((status, body)) => {
            let status_code = status.as_u16();
            let body = String::from_utf8_lossy(&body);
            record_runtime_log_forward_failure(
                app,
                &job.task_id,
                target_path,
                Some(status_code),
                format!(
                    "runtime-log forward returned HTTP {status_code}: {}",
                    summarize_error(&body)
                ),
            )
            .await;
        }
        Err(error) => {
            record_runtime_log_forward_failure(
                app,
                &job.task_id,
                target_path,
                None,
                format!("runtime-log forward failed: {}", summarize_error(&error.to_string())),
            )
            .await;
        }
    }
}

async fn record_runtime_log_forward_failure(
    app: &AppState,
    task_id: &str,
    target_path: String,
    http_status: Option<u16>,
    error: String,
) {
    tracing::warn!(task_id, target_path, http_status, error, "runtime-log forward failed");
    let mut state = app.state.lock().await;
    mark_runtime_log_forward_failed(&mut state, task_id, target_path, http_status, error);
    queue_task_status_updates(&state, &app.helper, task_id);
}
