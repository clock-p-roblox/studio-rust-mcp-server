use super::state::{HubStatusClient, HELPER_TASK_STATUS_STALE_AFTER, HUB_STATUS_TIMEOUT};
use crate::error::Result;
use color_eyre::eyre::eyre;
use reqwest::header::{AUTHORIZATION, CONTENT_TYPE};
use serde::Deserialize;
use std::collections::HashMap;

#[derive(Debug, Deserialize)]
pub(super) struct HubStatusPayload {
    pub(super) ok: bool,
    #[serde(default)]
    pub(super) helpers: Vec<HubHelperPayload>,
    #[serde(default)]
    pub(super) tasks: Vec<HubTaskPayload>,
}

#[derive(Debug, Deserialize)]
pub(super) struct HubTaskPayload {
    pub(super) task_id: String,
    #[serde(default)]
    pub(super) claimed_by_helper_id: Option<String>,
    #[serde(default)]
    pub(super) released: bool,
    #[serde(default)]
    pub(super) service_state: String,
    #[serde(default)]
    pub(super) accepting_launches: bool,
    #[serde(default)]
    pub(super) services: HashMap<String, String>,
}

#[derive(Debug, Deserialize)]
pub(super) struct HubHelperPayload {
    pub(super) helper_id: String,
    #[serde(default)]
    pub(super) blocked: bool,
    #[serde(default)]
    pub(super) last_seen_age_ms: Option<u128>,
    #[serde(default)]
    pub(super) active_tasks: Vec<HubHelperActiveTaskPayload>,
}

#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize)]
pub(super) struct HubHelperActiveTaskPayload {
    pub(super) task_id: String,
    #[serde(default)]
    pub(super) remote_state: String,
    #[serde(default)]
    pub(super) studio_mode: Option<String>,
    #[serde(default)]
    pub(super) studio_mode_age_ms: Option<u128>,
    #[serde(default)]
    pub(super) studio_mode_source: Option<String>,
    #[serde(default)]
    pub(super) studio_control_state: Option<String>,
    #[serde(default)]
    pub(super) studio_transition_phase: Option<String>,
    #[serde(default)]
    pub(super) studio_transition_age_ms: Option<u128>,
    #[serde(default)]
    pub(super) edit_runtime_state: Option<String>,
    #[serde(default)]
    pub(super) edit_runtime_age_ms: Option<u128>,
    #[serde(default)]
    pub(super) studio_control_last_error: Option<String>,
    #[serde(default)]
    pub(super) official_mcp_adapter_state: Option<String>,
    #[serde(default)]
    pub(super) official_mcp_adapter_age_ms: Option<u128>,
    #[serde(default)]
    pub(super) official_mcp_adapter_last_error: Option<String>,
}

#[derive(Clone, Debug)]
pub(super) struct HubTaskRouteSnapshot {
    pub(super) claimed_by_helper_id: Option<String>,
    pub(super) helper_id: Option<String>,
    pub(super) helper_blocked: bool,
    pub(super) helper_last_seen_age_ms: Option<u128>,
    pub(super) task_released: bool,
    pub(super) task_service_state: String,
    pub(super) task_accepting_launches: bool,
    pub(super) task_services_healthy: bool,
    pub(super) remote_state: Option<String>,
}

#[derive(Clone, Debug)]
pub(super) struct LocalStudioLiveSnapshot {
    pub(super) studio_mode: Option<String>,
    pub(super) studio_mode_age_ms: Option<u128>,
    pub(super) studio_mode_source: Option<String>,
    pub(super) studio_control_state: Option<String>,
    pub(super) studio_transition_phase: Option<String>,
    pub(super) studio_transition_age_ms: Option<u128>,
    pub(super) edit_runtime_state: Option<String>,
    pub(super) edit_runtime_age_ms: Option<u128>,
    pub(super) studio_control_last_error: Option<String>,
    pub(super) official_mcp_adapter_state: Option<String>,
    pub(super) official_mcp_adapter_age_ms: Option<u128>,
    pub(super) official_mcp_adapter_last_error: Option<String>,
}

impl HubTaskRouteSnapshot {
    pub(super) fn task_services_ready(&self) -> bool {
        self.task_service_state == "ready"
            && self.task_accepting_launches
            && self.task_services_healthy
            && !self.task_released
    }

    pub(super) fn helper_snapshot_fresh(&self) -> bool {
        self.helper_last_seen_age_ms
            .map(|age| age <= HELPER_TASK_STATUS_STALE_AFTER.as_millis())
            .unwrap_or(false)
    }

    pub(super) fn helper_remote_ready(&self) -> bool {
        matches!(self.remote_state.as_deref(), Some("connected" | "ready"))
    }

    pub(super) fn launch_ready(&self) -> bool {
        self.task_services_ready()
            && self.helper_snapshot_fresh()
            && self.helper_remote_ready()
            && !self.helper_blocked
    }
}

impl LocalStudioLiveSnapshot {
    pub(super) fn studio_snapshot_fresh(&self) -> bool {
        self.studio_mode_age_ms
            .map(|age| age <= HELPER_TASK_STATUS_STALE_AFTER.as_millis())
            .unwrap_or(false)
    }

    pub(super) fn edit_ready(&self) -> bool {
        self.studio_snapshot_fresh()
            && self.studio_mode.as_deref() == Some("stop")
            && self.edit_runtime_state.as_deref() == Some("ready")
            && self
                .edit_runtime_age_ms
                .map(|age| age <= HELPER_TASK_STATUS_STALE_AFTER.as_millis())
                .unwrap_or(false)
    }

    pub(super) fn official_ready(&self) -> bool {
        self.edit_ready()
            && self.official_mcp_adapter_state.as_deref() == Some("ready")
            && self
                .official_mcp_adapter_age_ms
                .map(|age| age <= HELPER_TASK_STATUS_STALE_AFTER.as_millis())
                .unwrap_or(false)
    }
}

impl HubStatusClient {
    pub(super) fn new(base_url: Option<String>, bearer_token: Option<String>) -> Option<Self> {
        let base_url = base_url
            .map(|value| value.trim().trim_end_matches('/').to_owned())
            .filter(|value| !value.is_empty())?;
        let client = reqwest::Client::builder()
            .timeout(HUB_STATUS_TIMEOUT)
            .build()
            .ok()?;
        Some(Self {
            base_url,
            bearer_token,
            client,
        })
    }

    pub(super) fn base_url(&self) -> &str {
        &self.base_url
    }

    pub(super) async fn fetch_task_route_snapshot(
        &self,
        task_id: &str,
    ) -> Result<HubTaskRouteSnapshot> {
        let url = format!("{}/status", self.base_url);
        let mut request = self
            .client
            .get(&url)
            .header(CONTENT_TYPE, "application/json");
        if let Some(token) = self.bearer_token.as_ref() {
            request = request.header(AUTHORIZATION, format!("Bearer {token}"));
        }
        let response = request
            .send()
            .await
            .map_err(|error| eyre!("failed to request hub /status: {error}"))?;
        let status = response.status();
        if !status.is_success() {
            return Err(eyre!("hub /status returned HTTP {status}").into());
        }
        let payload = response
            .json::<HubStatusPayload>()
            .await
            .map_err(|error| eyre!("failed to decode hub /status JSON: {error}"))?;
        if !payload.ok {
            return Err(eyre!("hub /status returned ok=false").into());
        }
        super::snapshot_from_hub_status_payload(payload, task_id)
    }
}
