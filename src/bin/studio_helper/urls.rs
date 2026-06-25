use super::HELPER_WS_PATH;
use color_eyre::eyre::{eyre, Result};

pub(super) fn runtime_screenshot_upload_url_for_base_url(base_url: &str) -> String {
    format!("{}/v1/runtime-screenshots", base_url.trim_end_matches('/'))
}

pub(super) fn runtime_screenshot_helper_upload_url(
    helper_port: u16,
    place_id: &str,
    task_id: &str,
) -> String {
    format!(
        "{}/v1/runtime-screenshots",
        runtime_log_forward_base_url(helper_port, place_id, task_id)
    )
}

pub(super) fn rojo_forward_base_url(helper_port: u16, place_id: &str, task_id: &str) -> String {
    format!("http://127.0.0.1:{helper_port}/rojo-forward/{place_id}/task/{task_id}")
}

pub(super) fn runtime_log_forward_base_url(
    helper_port: u16,
    place_id: &str,
    task_id: &str,
) -> String {
    format!("http://127.0.0.1:{helper_port}/runtime-log-forward/{place_id}/task/{task_id}")
}

pub(super) fn runtime_log_forward_upload_url(
    helper_port: u16,
    place_id: &str,
    task_id: &str,
) -> String {
    format!(
        "{}/v1/runtime-logs",
        runtime_log_forward_base_url(helper_port, place_id, task_id)
    )
}

pub(super) fn rojo_forward_target_path(path: &str, query: Option<&str>) -> String {
    let mut target = String::from("/");
    target.push_str(path.trim_start_matches('/'));
    if let Some(query) = query.filter(|value| !value.is_empty()) {
        target.push('?');
        target.push_str(query);
    }
    target
}

pub(super) fn rojo_forward_target_ws_url(
    base_url: &str,
    path: &str,
    query: Option<&str>,
) -> Result<String> {
    let ws_base_url = if let Some(rest) = base_url.strip_prefix("https://") {
        format!("wss://{rest}")
    } else if let Some(rest) = base_url.strip_prefix("http://") {
        format!("ws://{rest}")
    } else if base_url.starts_with("wss://") || base_url.starts_with("ws://") {
        base_url.to_owned()
    } else {
        return Err(eyre!("Rojo forward base_url must use http(s) or ws(s)"));
    };
    Ok(format!(
        "{}{}",
        ws_base_url.trim_end_matches('/'),
        rojo_forward_target_path(path, query)
    ))
}

pub(super) fn derive_remote_helper_ws_url_from_base_url(remote_base_url: &str) -> String {
    let base = if remote_base_url.starts_with("https://") {
        remote_base_url.replacen("https://", "wss://", 1)
    } else if remote_base_url.starts_with("http://") {
        remote_base_url.replacen("http://", "ws://", 1)
    } else {
        remote_base_url.to_owned()
    };
    format!("{}{}", base.trim_end_matches('/'), HELPER_WS_PATH)
}
