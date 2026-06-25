use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const HELPER_WS_PATH: &str = "/ws/helper";
pub const OFFICIAL_MCP_ADAPTER_CAPABILITY: &str = "official_mcp_adapter_v1";
pub const OFFICIAL_MCP_STORE_IMAGE_BASE64_CAPABILITY: &str = "official_mcp_store_image_base64_v1";
#[allow(dead_code)]
pub const MAX_ARTIFACT_CHUNK_MESSAGE_BYTES: usize = 500 * 1024;
#[allow(dead_code)]
pub const MAX_ARTIFACT_CHUNK_PAYLOAD_BYTES: usize = 360 * 1024;

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct HelperHello {
    #[serde(rename = "helper_id", alias = "helperId")]
    pub helper_id: String,
    #[serde(rename = "place_id", alias = "placeId")]
    pub place_id: String,
    #[serde(rename = "task_id", alias = "taskId")]
    pub task_id: Option<String>,
    #[serde(rename = "helper_version", alias = "helperVersion")]
    pub helper_version: String,
    pub capabilities: Vec<String>,
    #[serde(rename = "plugin_instance_count", alias = "pluginInstanceCount")]
    pub plugin_instance_count: usize,
    #[serde(default, rename = "task_status", alias = "taskStatus")]
    pub task_status: Option<HelperTaskStatusSnapshot>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct HelperTaskStatusSnapshot {
    #[serde(rename = "task_id", alias = "taskId")]
    pub task_id: String,
    #[serde(rename = "studio_session_state", alias = "studioSessionState")]
    pub studio_session_state: Option<String>,
    #[serde(rename = "last_known_session_state", alias = "lastKnownSessionState")]
    pub last_known_session_state: Option<String>,
    #[serde(rename = "last_session_error_reason", alias = "lastSessionErrorReason")]
    pub last_session_error_reason: Option<String>,
    #[serde(rename = "studio_mode", alias = "studioMode")]
    pub studio_mode: Option<String>,
    #[serde(rename = "studio_mode_age_ms", alias = "studioModeAgeMs")]
    pub studio_mode_age_ms: Option<u64>,
    #[serde(rename = "studio_mode_source", alias = "studioModeSource")]
    pub studio_mode_source: Option<String>,
    #[serde(rename = "studio_control_state", alias = "studioControlState")]
    pub studio_control_state: Option<String>,
    #[serde(rename = "studio_transition_phase", alias = "studioTransitionPhase")]
    pub studio_transition_phase: Option<String>,
    #[serde(rename = "studio_transition_age_ms", alias = "studioTransitionAgeMs")]
    pub studio_transition_age_ms: Option<u64>,
    #[serde(rename = "edit_runtime_state", alias = "editRuntimeState")]
    pub edit_runtime_state: Option<String>,
    #[serde(rename = "edit_runtime_age_ms", alias = "editRuntimeAgeMs")]
    pub edit_runtime_age_ms: Option<u64>,
    #[serde(rename = "studio_control_last_error", alias = "studioControlLastError")]
    pub studio_control_last_error: Option<String>,
    #[serde(rename = "active_stop_request_id", alias = "activeStopRequestId")]
    pub active_stop_request_id: Option<u64>,
    #[serde(rename = "last_stop_request_id", alias = "lastStopRequestId")]
    pub last_stop_request_id: Option<u64>,
    #[serde(
        rename = "stop_request_recorded_age_ms",
        alias = "stopRequestRecordedAgeMs"
    )]
    pub stop_request_recorded_age_ms: Option<u64>,
    #[serde(
        rename = "runtime_actuator_last_poll_id",
        alias = "runtimeActuatorLastPollId"
    )]
    pub runtime_actuator_last_poll_id: Option<u64>,
    #[serde(
        rename = "runtime_actuator_last_poll_age_ms",
        alias = "runtimeActuatorLastPollAgeMs"
    )]
    pub runtime_actuator_last_poll_age_ms: Option<u64>,
    #[serde(rename = "stop_result_phase", alias = "stopResultPhase")]
    pub stop_result_phase: Option<String>,
    #[serde(rename = "stop_result_age_ms", alias = "stopResultAgeMs")]
    pub stop_result_age_ms: Option<u64>,
    #[serde(rename = "stop_result_error", alias = "stopResultError")]
    pub stop_result_error: Option<String>,
    #[serde(
        rename = "official_mcp_adapter_state",
        alias = "officialMcpAdapterState"
    )]
    pub official_mcp_adapter_state: Option<String>,
    #[serde(
        rename = "official_mcp_adapter_age_ms",
        alias = "officialMcpAdapterAgeMs"
    )]
    pub official_mcp_adapter_age_ms: Option<u64>,
    #[serde(
        rename = "official_mcp_adapter_last_error",
        alias = "officialMcpAdapterLastError"
    )]
    pub official_mcp_adapter_last_error: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ArtifactBegin {
    #[serde(rename = "upload_id", alias = "uploadId")]
    pub upload_id: String,
    #[serde(rename = "request_id", alias = "requestId")]
    pub request_id: String,
    #[serde(rename = "session_id", alias = "sessionId")]
    pub session_id: String,
    #[serde(rename = "runtime_id", alias = "runtimeId")]
    pub runtime_id: String,
    #[serde(rename = "place_id", alias = "placeId")]
    pub place_id: String,
    #[serde(rename = "task_id", alias = "taskId")]
    pub task_id: Option<String>,
    pub tag: Option<String>,
    #[serde(rename = "content_type", alias = "contentType")]
    pub content_type: String,
    #[serde(rename = "total_bytes", alias = "totalBytes")]
    pub total_bytes: usize,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ArtifactFinish {
    #[serde(rename = "upload_id", alias = "uploadId")]
    pub upload_id: String,
    #[serde(rename = "request_id", alias = "requestId")]
    pub request_id: String,
    #[serde(rename = "total_chunks", alias = "totalChunks")]
    pub total_chunks: u32,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ArtifactChunk {
    #[serde(rename = "upload_id", alias = "uploadId")]
    pub upload_id: String,
    pub seq: u32,
    #[serde(rename = "data_base64", alias = "dataBase64")]
    pub data_base64: String,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ArtifactAbort {
    #[serde(rename = "upload_id", alias = "uploadId")]
    pub upload_id: String,
    #[serde(rename = "request_id", alias = "requestId")]
    pub request_id: String,
    pub error: String,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ArtifactCommitted {
    #[serde(rename = "upload_id", alias = "uploadId")]
    pub upload_id: String,
    #[serde(rename = "request_id", alias = "requestId")]
    pub request_id: String,
    #[serde(rename = "session_id", alias = "sessionId")]
    pub session_id: String,
    #[serde(rename = "runtime_id", alias = "runtimeId")]
    pub runtime_id: String,
    #[serde(rename = "place_id", alias = "placeId")]
    pub place_id: String,
    #[serde(rename = "task_id", alias = "taskId")]
    pub task_id: Option<String>,
    #[serde(rename = "screenshot_path", alias = "screenshotPath")]
    pub screenshot_path: String,
    #[serde(rename = "screenshot_rel_path", alias = "screenshotRelPath")]
    pub screenshot_rel_path: String,
    #[serde(rename = "artifact_dir", alias = "artifactDir")]
    pub artifact_dir: String,
    #[serde(rename = "session_metadata_path", alias = "sessionMetadataPath")]
    pub session_metadata_path: String,
    #[serde(rename = "bytes_written", alias = "bytesWritten")]
    pub bytes_written: usize,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct OfficialMcpRequest {
    #[serde(rename = "request_id", alias = "requestId")]
    pub request_id: String,
    #[serde(rename = "task_id", alias = "taskId")]
    pub task_id: String,
    #[serde(rename = "place_id", alias = "placeId")]
    pub place_id: String,
    pub action: String,
    #[serde(default)]
    pub arguments: Value,
    #[serde(rename = "timeout_ms", alias = "timeoutMs")]
    pub timeout_ms: u64,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct OfficialMcpResponse {
    #[serde(rename = "request_id", alias = "requestId")]
    pub request_id: String,
    pub response: String,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(tag = "type")]
pub enum HelperToServerMessage {
    #[serde(rename = "hello", alias = "Hello")]
    Hello(HelperHello),
    #[serde(rename = "heartbeat", alias = "Heartbeat")]
    Heartbeat {
        #[serde(alias = "helperId")]
        helper_id: String,
        #[serde(alias = "placeId")]
        place_id: String,
        #[serde(alias = "taskId")]
        task_id: Option<String>,
        #[serde(alias = "pluginInstanceCount")]
        plugin_instance_count: usize,
        #[serde(default, alias = "taskStatus")]
        task_status: Option<HelperTaskStatusSnapshot>,
    },
    #[serde(rename = "tool_result", alias = "toolResult")]
    ToolResult {
        #[serde(alias = "requestId")]
        request_id: String,
        response: String,
    },
    #[serde(rename = "tool_error", alias = "toolError")]
    ToolError {
        #[serde(alias = "requestId")]
        request_id: String,
        error: String,
    },
    #[serde(rename = "artifact_begin", alias = "artifactBegin")]
    ArtifactBegin(ArtifactBegin),
    #[serde(rename = "artifact_chunk", alias = "artifactChunk")]
    ArtifactChunk(ArtifactChunk),
    #[serde(rename = "artifact_finish", alias = "artifactFinish")]
    ArtifactFinish(ArtifactFinish),
    #[serde(rename = "artifact_abort", alias = "artifactAbort")]
    ArtifactAbort(ArtifactAbort),
    #[serde(rename = "official_mcp_response", alias = "officialMcpResponse")]
    OfficialMcpResponse(OfficialMcpResponse),
    #[serde(rename = "official_mcp_error", alias = "officialMcpError")]
    OfficialMcpError {
        #[serde(alias = "requestId")]
        request_id: String,
        error: String,
    },
}

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(tag = "type")]
pub enum ServerToHelperMessage {
    #[serde(rename = "ready_ack", alias = "readyAck")]
    ReadyAck {
        #[serde(alias = "connectionId")]
        connection_id: String,
        #[serde(alias = "placeId")]
        place_id: String,
        #[serde(alias = "taskId")]
        task_id: Option<String>,
    },
    #[serde(rename = "tool_call", alias = "toolCall")]
    ToolCall {
        #[serde(alias = "requestId")]
        request_id: String,
        command: Value,
    },
    #[serde(rename = "artifact_committed", alias = "artifactCommitted")]
    ArtifactCommitted(ArtifactCommitted),
    #[serde(rename = "artifact_failed", alias = "artifactFailed")]
    ArtifactFailed {
        #[serde(alias = "uploadId")]
        upload_id: String,
        #[serde(alias = "requestId")]
        request_id: String,
        error: String,
    },
    #[serde(rename = "official_mcp_request", alias = "officialMcpRequest")]
    OfficialMcpRequest(OfficialMcpRequest),
    #[serde(rename = "close_reason", alias = "closeReason")]
    CloseReason { reason: String },
}

#[cfg(test)]
mod tests {
    use super::{
        HelperHello, HelperTaskStatusSnapshot, HelperToServerMessage, ServerToHelperMessage,
    };

    #[test]
    fn server_to_helper_ready_ack_accepts_camel_case_identity_fields() {
        let payload = r#"{
            "type": "readyAck",
            "connectionId": "conn_1",
            "placeId": "93795519121520",
            "taskId": "tf2a83d456a"
        }"#;
        let decoded: ServerToHelperMessage =
            serde_json::from_str(payload).expect("camelCase ready ack should decode");
        match decoded {
            ServerToHelperMessage::ReadyAck {
                connection_id,
                place_id,
                task_id,
            } => {
                assert_eq!(connection_id, "conn_1");
                assert_eq!(place_id, "93795519121520");
                assert_eq!(task_id.as_deref(), Some("tf2a83d456a"));
            }
            other => panic!("expected ready ack, got {other:?}"),
        }
    }

    #[test]
    fn helper_to_server_hello_accepts_snake_case_identity_fields() {
        let payload = r#"{
            "type": "hello",
            "helper_id": "h_test",
            "place_id": "93795519121520",
            "task_id": "tf2a83d456a",
            "helper_version": "0.0.0",
            "capabilities": ["ws_tool_dispatch_v1"],
            "plugin_instance_count": 1
        }"#;
        let decoded: HelperToServerMessage =
            serde_json::from_str(payload).expect("snake_case hello should decode");
        match decoded {
            HelperToServerMessage::Hello(HelperHello {
                helper_id,
                place_id,
                task_id,
                helper_version,
                capabilities,
                plugin_instance_count,
                task_status,
            }) => {
                assert_eq!(helper_id, "h_test");
                assert_eq!(place_id, "93795519121520");
                assert_eq!(task_id.as_deref(), Some("tf2a83d456a"));
                assert_eq!(helper_version, "0.0.0");
                assert_eq!(capabilities, vec!["ws_tool_dispatch_v1"]);
                assert_eq!(plugin_instance_count, 1);
                assert!(task_status.is_none());
            }
            other => panic!("expected hello, got {other:?}"),
        }
    }

    #[test]
    fn helper_to_server_heartbeat_serializes_snake_case_fields() {
        let encoded = serde_json::to_value(HelperToServerMessage::Heartbeat {
            helper_id: "h_test".to_owned(),
            place_id: "93795519121520".to_owned(),
            task_id: Some("tf2a83d456a".to_owned()),
            plugin_instance_count: 2,
            task_status: Some(HelperTaskStatusSnapshot {
                task_id: "tf2a83d456a".to_owned(),
                studio_session_state: Some("stop".to_owned()),
                last_known_session_state: Some("stop".to_owned()),
                last_session_error_reason: None,
                studio_mode: Some("stop".to_owned()),
                studio_mode_age_ms: Some(42),
                studio_mode_source: Some("edit_plugin".to_owned()),
                studio_control_state: Some("none".to_owned()),
                studio_transition_phase: Some("idle".to_owned()),
                studio_transition_age_ms: None,
                edit_runtime_state: Some("ready".to_owned()),
                edit_runtime_age_ms: Some(3),
                studio_control_last_error: None,
                active_stop_request_id: Some(12),
                last_stop_request_id: Some(12),
                stop_request_recorded_age_ms: Some(25),
                runtime_actuator_last_poll_id: Some(12),
                runtime_actuator_last_poll_age_ms: Some(11),
                stop_result_phase: Some("observed".to_owned()),
                stop_result_age_ms: Some(9),
                stop_result_error: None,
                official_mcp_adapter_state: Some("ready".to_owned()),
                official_mcp_adapter_age_ms: Some(7),
                official_mcp_adapter_last_error: None,
            }),
        })
        .expect("heartbeat should serialize");
        assert_eq!(encoded["type"], "heartbeat");
        assert_eq!(encoded["helper_id"], "h_test");
        assert_eq!(encoded["place_id"], "93795519121520");
        assert_eq!(encoded["task_id"], "tf2a83d456a");
        assert_eq!(encoded["plugin_instance_count"], 2);
        assert_eq!(encoded["task_status"]["task_id"], "tf2a83d456a");
        assert_eq!(encoded["task_status"]["studio_session_state"], "stop");
        assert_eq!(encoded["task_status"]["last_known_session_state"], "stop");
        assert_eq!(encoded["task_status"]["studio_mode"], "stop");
        assert_eq!(encoded["task_status"]["studio_mode_source"], "edit_plugin");
        assert_eq!(encoded["task_status"]["studio_control_state"], "none");
        assert_eq!(encoded["task_status"]["studio_transition_phase"], "idle");
        assert_eq!(encoded["task_status"]["edit_runtime_state"], "ready");
        assert_eq!(encoded["task_status"]["active_stop_request_id"], 12);
        assert_eq!(encoded["task_status"]["last_stop_request_id"], 12);
        assert_eq!(encoded["task_status"]["stop_result_phase"], "observed");
        assert_eq!(
            encoded["task_status"]["official_mcp_adapter_state"],
            "ready"
        );
        assert!(encoded.get("helperId").is_none());
        assert!(encoded.get("taskId").is_none());
    }

    #[test]
    fn helper_to_server_heartbeat_decodes_task_status_age_fields() {
        let payload = r#"{
            "type": "heartbeat",
            "helper_id": "h_test",
            "place_id": "93795519121520",
            "task_id": "tf2a83d456a",
            "plugin_instance_count": 1,
            "task_status": {
                "task_id": "tf2a83d456a",
                "studio_mode": "stop",
                "studio_mode_age_ms": 12,
                "studio_mode_source": "edit_plugin",
                "studio_control_state": "none",
                "studio_transition_phase": "idle",
                "studio_transition_age_ms": 4,
                "edit_runtime_state": "ready",
                "edit_runtime_age_ms": 5,
                "studio_control_last_error": null,
                "official_mcp_adapter_state": "ready",
                "official_mcp_adapter_age_ms": 34,
                "official_mcp_adapter_last_error": null
            }
        }"#;
        let decoded: HelperToServerMessage =
            serde_json::from_str(payload).expect("helper heartbeat task_status should decode");
        match decoded {
            HelperToServerMessage::Heartbeat { task_status, .. } => {
                let status = task_status.expect("task_status should be present");
                assert!(status.studio_session_state.is_none());
                assert!(status.last_known_session_state.is_none());
                assert!(status.last_session_error_reason.is_none());
                assert!(status.active_stop_request_id.is_none());
                assert!(status.last_stop_request_id.is_none());
                assert!(status.stop_request_recorded_age_ms.is_none());
                assert!(status.runtime_actuator_last_poll_id.is_none());
                assert!(status.runtime_actuator_last_poll_age_ms.is_none());
                assert!(status.stop_result_phase.is_none());
                assert!(status.stop_result_age_ms.is_none());
                assert!(status.stop_result_error.is_none());
                assert_eq!(status.studio_mode_age_ms, Some(12));
                assert_eq!(status.studio_mode_source.as_deref(), Some("edit_plugin"));
                assert_eq!(status.studio_transition_age_ms, Some(4));
                assert_eq!(status.edit_runtime_state.as_deref(), Some("ready"));
                assert_eq!(status.edit_runtime_age_ms, Some(5));
                assert_eq!(status.official_mcp_adapter_age_ms, Some(34));
            }
            other => panic!("expected heartbeat, got {other:?}"),
        }
    }

    #[test]
    fn server_to_helper_artifact_committed_accepts_camel_case_fields() {
        let payload = r#"{
            "type": "artifactCommitted",
            "uploadId": "u_1",
            "requestId": "req_1",
            "sessionId": "sess_1",
            "runtimeId": "client_1",
            "placeId": "93795519121520",
            "taskId": "tf2a83d456a",
            "screenshotPath": "/tmp/shot.png",
            "screenshotRelPath": "client_1/shot.png",
            "artifactDir": "/tmp/artifacts/sess_1",
            "sessionMetadataPath": "/tmp/artifacts/sess_1/session.json",
            "bytesWritten": 42
        }"#;
        let decoded: ServerToHelperMessage =
            serde_json::from_str(payload).expect("camelCase artifact committed should decode");
        match decoded {
            ServerToHelperMessage::ArtifactCommitted(committed) => {
                assert_eq!(committed.upload_id, "u_1");
                assert_eq!(committed.request_id, "req_1");
                assert_eq!(committed.session_id, "sess_1");
                assert_eq!(committed.runtime_id, "client_1");
                assert_eq!(committed.place_id, "93795519121520");
                assert_eq!(committed.task_id.as_deref(), Some("tf2a83d456a"));
                assert_eq!(committed.screenshot_path, "/tmp/shot.png");
                assert_eq!(committed.screenshot_rel_path, "client_1/shot.png");
                assert_eq!(committed.artifact_dir, "/tmp/artifacts/sess_1");
                assert_eq!(
                    committed.session_metadata_path,
                    "/tmp/artifacts/sess_1/session.json"
                );
                assert_eq!(committed.bytes_written, 42);
            }
            other => panic!("expected artifact committed, got {other:?}"),
        }
    }
}
