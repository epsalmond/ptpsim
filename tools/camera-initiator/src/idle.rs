//! Bounded observations of an established command socket.

use serde::Serialize;

pub const MAX_HOLD_DURATION_MS: u64 = 3_600_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum IdleEnd {
    DurationElapsed,
    PeerEof,
    PeerReset,
    PeerFrame,
    HostDeadlinePartialFrame,
    Cancelled,
    IoFailure,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct IdleObservation {
    pub end: IdleEnd,
    pub buffered_bytes: Option<usize>,
    pub peer_frame: Option<IdlePeerFrame>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct IdlePeerFrame {
    pub length: u64,
    pub sha256: String,
    pub response_code: Option<u16>,
    pub transaction_id: Option<u32>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HoldReport {
    pub phase: &'static str,
    pub requested_duration_ms: u64,
    pub started_elapsed_ns: u64,
    pub ended_elapsed_ns: u64,
    pub observation: IdleObservation,
}
