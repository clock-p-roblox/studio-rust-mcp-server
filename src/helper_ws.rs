use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const HELPER_WS_PATH: &str = "/ws/helper";
#[allow(dead_code)]
pub const MAX_ARTIFACT_CHUNK_MESSAGE_BYTES: usize = 500 * 1024;
#[allow(dead_code)]
pub const MAX_ARTIFACT_CHUNK_PAYLOAD_BYTES: usize = 360 * 1024;

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct HelperHello {
    pub place_id: String,
    pub helper_version: String,
    pub capabilities: Vec<String>,
    pub plugin_instance_count: usize,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ArtifactBegin {
    pub upload_id: String,
    pub request_id: String,
    pub session_id: String,
    pub runtime_id: String,
    pub place_id: String,
    pub tag: Option<String>,
    pub content_type: String,
    pub total_bytes: usize,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ArtifactFinish {
    pub upload_id: String,
    pub request_id: String,
    pub total_chunks: u32,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ArtifactChunk {
    pub upload_id: String,
    pub seq: u32,
    pub data_base64: String,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ArtifactAbort {
    pub upload_id: String,
    pub request_id: String,
    pub error: String,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ArtifactCommitted {
    pub upload_id: String,
    pub request_id: String,
    pub session_id: String,
    pub runtime_id: String,
    pub place_id: String,
    pub screenshot_path: String,
    pub screenshot_rel_path: String,
    pub artifact_dir: String,
    pub session_metadata_path: String,
    pub bytes_written: usize,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum HelperToServerMessage {
    Hello(HelperHello),
    Heartbeat {
        place_id: String,
        plugin_instance_count: usize,
    },
    ToolResult {
        request_id: String,
        response: String,
    },
    ToolError {
        request_id: String,
        error: String,
    },
    ArtifactBegin(ArtifactBegin),
    ArtifactChunk(ArtifactChunk),
    ArtifactFinish(ArtifactFinish),
    ArtifactAbort(ArtifactAbort),
}

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerToHelperMessage {
    ReadyAck {
        connection_id: String,
        place_id: String,
    },
    ToolCall {
        request_id: String,
        command: Value,
    },
    ArtifactCommitted(ArtifactCommitted),
    ArtifactFailed {
        upload_id: String,
        request_id: String,
        error: String,
    },
    CloseReason {
        reason: String,
    },
}
