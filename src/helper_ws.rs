use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const HELPER_WS_PATH: &str = "/ws/helper";
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
    pub generation: Option<u32>,
    #[serde(rename = "launch_id", alias = "launchId")]
    pub launch_id: Option<String>,
    #[serde(rename = "helper_version", alias = "helperVersion")]
    pub helper_version: String,
    pub capabilities: Vec<String>,
    #[serde(rename = "plugin_instance_count", alias = "pluginInstanceCount")]
    pub plugin_instance_count: usize,
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
    pub generation: Option<u32>,
    #[serde(rename = "launch_id", alias = "launchId")]
    pub launch_id: Option<String>,
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
    pub generation: Option<u32>,
    #[serde(rename = "launch_id", alias = "launchId")]
    pub launch_id: Option<String>,
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
        generation: Option<u32>,
        #[serde(alias = "launchId")]
        launch_id: Option<String>,
        #[serde(alias = "pluginInstanceCount")]
        plugin_instance_count: usize,
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
        generation: Option<u32>,
        #[serde(alias = "launchId")]
        launch_id: Option<String>,
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
    #[serde(rename = "close_reason", alias = "closeReason")]
    CloseReason {
        reason: String,
    },
}

#[cfg(test)]
mod tests {
    use super::{HelperHello, HelperToServerMessage, ServerToHelperMessage};

    #[test]
    fn server_to_helper_ready_ack_accepts_camel_case_identity_fields() {
        let payload = r#"{
            "type": "readyAck",
            "connectionId": "conn_1",
            "placeId": "93795519121520",
            "taskId": "tf2a83d456a",
            "generation": 1,
            "launchId": "l_123"
        }"#;
        let decoded: ServerToHelperMessage =
            serde_json::from_str(payload).expect("camelCase ready ack should decode");
        match decoded {
            ServerToHelperMessage::ReadyAck {
                connection_id,
                place_id,
                task_id,
                generation,
                launch_id,
            } => {
                assert_eq!(connection_id, "conn_1");
                assert_eq!(place_id, "93795519121520");
                assert_eq!(task_id.as_deref(), Some("tf2a83d456a"));
                assert_eq!(generation, Some(1));
                assert_eq!(launch_id.as_deref(), Some("l_123"));
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
            "generation": 1,
            "launch_id": "l_123",
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
                generation,
                launch_id,
                helper_version,
                capabilities,
                plugin_instance_count,
            }) => {
                assert_eq!(helper_id, "h_test");
                assert_eq!(place_id, "93795519121520");
                assert_eq!(task_id.as_deref(), Some("tf2a83d456a"));
                assert_eq!(generation, Some(1));
                assert_eq!(launch_id.as_deref(), Some("l_123"));
                assert_eq!(helper_version, "0.0.0");
                assert_eq!(capabilities, vec!["ws_tool_dispatch_v1"]);
                assert_eq!(plugin_instance_count, 1);
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
            generation: Some(1),
            launch_id: Some("l_123".to_owned()),
            plugin_instance_count: 2,
        })
        .expect("heartbeat should serialize");
        assert_eq!(encoded["type"], "heartbeat");
        assert_eq!(encoded["helper_id"], "h_test");
        assert_eq!(encoded["place_id"], "93795519121520");
        assert_eq!(encoded["task_id"], "tf2a83d456a");
        assert_eq!(encoded["generation"], 1);
        assert_eq!(encoded["launch_id"], "l_123");
        assert_eq!(encoded["plugin_instance_count"], 2);
        assert!(encoded.get("helperId").is_none());
        assert!(encoded.get("taskId").is_none());
        assert!(encoded.get("launchId").is_none());
    }

    #[test]
    fn server_to_helper_ready_ack_accepts_missing_route_identity() {
        let payload = r#"{
            "type": "ready_ack",
            "connection_id": "conn_1",
            "place_id": "93795519121520"
        }"#;
        let decoded: ServerToHelperMessage =
            serde_json::from_str(payload).expect("minimal ready ack should decode");
        match decoded {
            ServerToHelperMessage::ReadyAck {
                connection_id,
                place_id,
                task_id,
                generation,
                launch_id,
            } => {
                assert_eq!(connection_id, "conn_1");
                assert_eq!(place_id, "93795519121520");
                assert_eq!(task_id, None);
                assert_eq!(generation, None);
                assert_eq!(launch_id, None);
            }
            other => panic!("expected ready ack, got {other:?}"),
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
            "generation": 1,
            "launchId": "l_123",
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
                assert_eq!(committed.generation, Some(1));
                assert_eq!(committed.launch_id.as_deref(), Some("l_123"));
                assert_eq!(committed.screenshot_path, "/tmp/shot.png");
                assert_eq!(committed.screenshot_rel_path, "client_1/shot.png");
                assert_eq!(committed.artifact_dir, "/tmp/artifacts/sess_1");
                assert_eq!(committed.session_metadata_path, "/tmp/artifacts/sess_1/session.json");
                assert_eq!(committed.bytes_written, 42);
            }
            other => panic!("expected artifact committed, got {other:?}"),
        }
    }
}
