use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Instant;

const MAX_EVENTS: usize = 512;
const MAX_PAYLOAD_BYTES: usize = 4 * 1024;
const MAX_TEXT_BYTES: usize = 4 * 1024;

#[derive(Debug, Clone, Default)]
pub struct TraceEndpoints {
    pub local: Option<String>,
    pub peer: Option<String>,
    pub target: Option<String>,
}

impl TraceEndpoints {
    pub fn connection(stream: &tokio::net::TcpStream) -> Self {
        Self {
            local: stream.local_addr().ok().map(|address| address.to_string()),
            peer: stream.peer_addr().ok().map(|address| address.to_string()),
            target: None,
        }
    }
}

#[derive(Debug, Clone)]
struct TraceEvent {
    sequence: u64,
    elapsed_ms: u64,
    kind: String,
    endpoints: TraceEndpoints,
    payload_hex: Option<String>,
    payload_length: Option<usize>,
    payload_truncated: bool,
    outcome: Option<String>,
    outcome_truncated: bool,
    error: Option<String>,
    error_truncated: bool,
}

#[derive(Debug, Default)]
struct TraceState {
    next_sequence: u64,
    events: VecDeque<TraceEvent>,
}

#[derive(Debug, Clone)]
pub struct TraceLog {
    started: Instant,
    state: Arc<Mutex<TraceState>>,
}

impl Default for TraceLog {
    fn default() -> Self {
        Self {
            started: Instant::now(),
            state: Arc::new(Mutex::new(TraceState {
                next_sequence: 1,
                events: VecDeque::new(),
            })),
        }
    }
}

impl TraceLog {
    pub fn record(
        &self,
        kind: impl Into<String>,
        endpoints: TraceEndpoints,
        payload: Option<&[u8]>,
        outcome: Option<String>,
        error: Option<String>,
    ) {
        let payload_length = payload.map(|bytes| bytes.len());
        let payload_truncated = payload_length.is_some_and(|length| length > MAX_PAYLOAD_BYTES);
        let payload_hex = payload.map(|bytes| hex(&bytes[..bytes.len().min(MAX_PAYLOAD_BYTES)]));
        let (outcome, outcome_truncated) = bounded_text(outcome);
        let (error, error_truncated) = bounded_text(error);
        let mut state = self.state.lock().expect("lifecycle trace lock poisoned");
        let sequence = state.next_sequence;
        state.next_sequence = state.next_sequence.saturating_add(1);
        state.events.push_back(TraceEvent {
            sequence,
            elapsed_ms: self.started.elapsed().as_millis() as u64,
            kind: kind.into(),
            endpoints,
            payload_hex,
            payload_length,
            payload_truncated,
            outcome,
            outcome_truncated,
            error,
            error_truncated,
        });
        while state.events.len() > MAX_EVENTS {
            state.events.pop_front();
        }
    }

    pub fn json(&self, instance_id: &str, after: u64) -> String {
        let state = self.state.lock().expect("lifecycle trace lock poisoned");
        let events = state
            .events
            .iter()
            .filter(|event| event.sequence > after)
            .map(|event| {
                serde_json::json!({
                    "sequence": event.sequence,
                    "elapsed_ms": event.elapsed_ms,
                    "kind": event.kind,
                    "local_endpoint": event.endpoints.local,
                    "peer_endpoint": event.endpoints.peer,
                    "target_endpoint": event.endpoints.target,
                    "payload_hex": event.payload_hex,
                    "payload_length": event.payload_length,
                    "payload_truncated": event.payload_truncated,
                    "outcome": event.outcome,
                    "outcome_truncated": event.outcome_truncated,
                    "error": event.error,
                    "error_truncated": event.error_truncated,
                })
            })
            .collect::<Vec<_>>();
        serde_json::json!({
            "instance_id": instance_id,
            "cursor": state.next_sequence.saturating_sub(1),
            "events": events,
        })
        .to_string()
    }
}

fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(DIGITS[(byte >> 4) as usize] as char);
        output.push(DIGITS[(byte & 0x0f) as usize] as char);
    }
    output
}

fn bounded_text(value: Option<String>) -> (Option<String>, bool) {
    let Some(mut value) = value else {
        return (None, false);
    };
    if value.len() <= MAX_TEXT_BYTES {
        return (Some(value), false);
    }
    let mut end = MAX_TEXT_BYTES;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    value.truncate(end);
    (Some(value), true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cursor_filters_events_and_keeps_raw_payloads() {
        let trace = TraceLog::default();
        trace.record(
            "pcss.discovery.received",
            TraceEndpoints {
                peer: Some("192.0.2.10:51562".into()),
                ..TraceEndpoints::default()
            },
            Some(&[0x44, 0x49, 0x53, 0x43]),
            Some("accepted".into()),
            None,
        );
        trace.record(
            "pcss.callback.connected",
            TraceEndpoints::default(),
            None,
            Some("connected".into()),
            None,
        );

        let all: serde_json::Value = serde_json::from_str(&trace.json("run", 0)).unwrap();
        assert_eq!(all["cursor"], 2);
        assert_eq!(all["events"][0]["payload_hex"], "44495343");

        let tail: serde_json::Value = serde_json::from_str(&trace.json("run", 1)).unwrap();
        assert_eq!(tail["events"].as_array().unwrap().len(), 1);
        assert_eq!(tail["events"][0]["sequence"], 2);
    }

    #[test]
    fn lifecycle_buffer_is_bounded() {
        let trace = TraceLog::default();
        for index in 0..(MAX_EVENTS + 5) {
            trace.record(
                "test",
                TraceEndpoints::default(),
                None,
                Some(index.to_string()),
                None,
            );
        }
        let json: serde_json::Value = serde_json::from_str(&trace.json("run", 0)).unwrap();
        let events = json["events"].as_array().unwrap();
        assert_eq!(events.len(), MAX_EVENTS);
        assert_eq!(events[0]["sequence"], 6);
    }

    #[test]
    fn oversized_payloads_are_explicitly_truncated() {
        let trace = TraceLog::default();
        let payload = vec![0xab; MAX_PAYLOAD_BYTES + 1];
        trace.record(
            "ptpip.command.accepted",
            TraceEndpoints::default(),
            Some(&payload),
            Some("accepted".into()),
            None,
        );

        let json: serde_json::Value = serde_json::from_str(&trace.json("run", 0)).unwrap();
        let event = &json["events"][0];
        assert_eq!(event["payload_length"], MAX_PAYLOAD_BYTES + 1);
        assert_eq!(event["payload_truncated"], true);
        assert_eq!(
            event["payload_hex"].as_str().unwrap().len(),
            MAX_PAYLOAD_BYTES * 2
        );
    }

    #[test]
    fn oversized_diagnostics_are_explicitly_truncated() {
        let trace = TraceLog::default();
        trace.record(
            "ptpip.first_operation.rejected",
            TraceEndpoints::default(),
            None,
            Some("x".repeat(MAX_TEXT_BYTES + 1)),
            Some("y".repeat(MAX_TEXT_BYTES * 4)),
        );

        let json: serde_json::Value = serde_json::from_str(&trace.json("run", 0)).unwrap();
        let event = &json["events"][0];
        assert_eq!(event["outcome"].as_str().unwrap().len(), MAX_TEXT_BYTES);
        assert_eq!(event["outcome_truncated"], true);
        assert_eq!(event["error"].as_str().unwrap().len(), MAX_TEXT_BYTES);
        assert_eq!(event["error_truncated"], true);
    }
}
