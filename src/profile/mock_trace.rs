use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MockTraceCommand {
    pub at_ms: u64,
    pub stream_id: u64,
    pub bytes: usize,
    pub fin: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MockTraceEventBatch {
    pub at_ms: u64,
    pub event_count: usize,
    pub stream_bytes: usize,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MockReplayTrace {
    pub trace_type: String,
    pub harness: String,
    pub protocol: String,
    pub server_name: String,
    pub payload_bytes: usize,
    pub requested_streams: usize,
    pub commands: Vec<MockTraceCommand>,
    pub event_batches: Vec<MockTraceEventBatch>,
}
