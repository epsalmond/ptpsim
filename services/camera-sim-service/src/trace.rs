use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use camera_config::PayloadMetadata;
use camera_sim::AppliedFault;
use serde::Serialize;

const MAX_EVENTS: usize = 512;
const MAX_PAYLOAD_BYTES: usize = 4 * 1024;
const MAX_TEXT_BYTES: usize = 4 * 1024;

#[derive(Debug, Clone, Default)]
pub struct TraceEndpoints {
    pub local: Option<String>,
    pub peer: Option<String>,
    pub target: Option<String>,
}

pub struct FaultTraceEvidence<'a> {
    pub endpoints: TraceEndpoints,
    pub operation: u16,
    pub transaction_id: u32,
    pub response_code: Option<&'a str>,
    pub fault: &'a AppliedFault,
    pub payload: Option<&'a PayloadMetadata>,
    pub applied: String,
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
    operation: Option<String>,
    transaction_id: Option<u32>,
    response_code: Option<String>,
    fault_id: Option<u64>,
    fault_kind: Option<String>,
    payload_summary: Option<PayloadSummary>,
    applied: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
struct PayloadSummary {
    length: u64,
    sha256: String,
    marker: String,
}

#[derive(Debug, Default)]
struct TraceState {
    next_sequence: u64,
    events: VecDeque<TraceEvent>,
    dropped_events: u64,
    truncated_payloads: u64,
    truncated_texts: u64,
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
                dropped_events: 0,
                truncated_payloads: 0,
                truncated_texts: 0,
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
            operation: None,
            transaction_id: None,
            response_code: None,
            fault_id: None,
            fault_kind: None,
            payload_summary: None,
            applied: None,
        });
        state.truncated_payloads += u64::from(payload_truncated);
        state.truncated_texts += u64::from(outcome_truncated) + u64::from(error_truncated);
        while state.events.len() > MAX_EVENTS {
            state.events.pop_front();
            state.dropped_events += 1;
        }
    }

    pub fn record_fault(&self, evidence: FaultTraceEvidence<'_>) {
        let FaultTraceEvidence {
            endpoints,
            operation,
            transaction_id,
            response_code,
            fault,
            payload,
            applied,
        } = evidence;
        let mut state = self.state.lock().expect("lifecycle trace lock poisoned");
        let sequence = state.next_sequence;
        state.next_sequence = state.next_sequence.saturating_add(1);
        state.events.push_back(TraceEvent {
            sequence,
            elapsed_ms: self.started.elapsed().as_millis() as u64,
            kind: "ptpip.fault.applied".into(),
            endpoints,
            payload_hex: None,
            payload_length: None,
            payload_truncated: false,
            outcome: None,
            outcome_truncated: false,
            error: None,
            error_truncated: false,
            operation: Some(format!("0x{operation:04x}")),
            transaction_id: Some(transaction_id),
            response_code: response_code.map(str::to_string),
            fault_id: Some(fault.id),
            fault_kind: Some(fault.kind.clone()),
            payload_summary: payload.map(|payload| PayloadSummary {
                length: payload.length,
                sha256: payload.sha256.clone(),
                marker: "faultMutation".into(),
            }),
            applied: Some(applied),
        });
        while state.events.len() > MAX_EVENTS {
            state.events.pop_front();
            state.dropped_events += 1;
        }
    }

    pub fn json(&self, instance_id: &str, after: u64) -> String {
        let state = self.state.lock().expect("lifecycle trace lock poisoned");
        let events = state
            .events
            .iter()
            .filter(|event| event.sequence > after)
            .map(|event| {
                let mut value = serde_json::json!({
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
                });
                let fields = value.as_object_mut().expect("trace event JSON object");
                if let Some(operation) = &event.operation {
                    fields.insert("operation".into(), operation.clone().into());
                }
                if let Some(transaction_id) = event.transaction_id {
                    fields.insert("transaction_id".into(), transaction_id.into());
                }
                if let Some(response_code) = &event.response_code {
                    fields.insert("response_code".into(), response_code.clone().into());
                }
                if let Some(fault_id) = event.fault_id {
                    fields.insert("fault_id".into(), fault_id.into());
                }
                if let Some(fault_kind) = &event.fault_kind {
                    fields.insert("fault_kind".into(), fault_kind.clone().into());
                }
                if let Some(payload_summary) = &event.payload_summary {
                    fields.insert(
                        "payload_summary".into(),
                        serde_json::to_value(payload_summary)
                            .expect("payload summary is JSON serializable"),
                    );
                }
                if let Some(applied) = &event.applied {
                    fields.insert("applied".into(), applied.clone().into());
                }
                value
            })
            .collect::<Vec<_>>();
        serde_json::json!({
            "instance_id": instance_id,
            "cursor": state.next_sequence.saturating_sub(1),
            "dropped_events": state.dropped_events,
            "truncated_payloads": state.truncated_payloads,
            "truncated_texts": state.truncated_texts,
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

    #[test]
    fn fault_evidence_is_structured_without_changing_lifecycle_event_keys() {
        let trace = TraceLog::default();
        trace.record(
            "ptpip.command.accepted",
            TraceEndpoints::default(),
            None,
            Some("accepted".into()),
            None,
        );
        let payload = camera_config::payload_metadata(&[0xde, 0xad, 0xbe, 0xef]);
        trace.record_fault(FaultTraceEvidence {
            endpoints: TraceEndpoints::default(),
            operation: 0x1015,
            transaction_id: 7,
            response_code: Some("0x2001"),
            fault: &AppliedFault {
                id: 3,
                kind: "replaceData".into(),
                wire: camera_sim::WirePlan::None,
            },
            payload: Some(&payload),
            applied: "replacedData".into(),
        });

        let json: serde_json::Value = serde_json::from_str(&trace.json("run", 0)).unwrap();
        assert!(json["events"][0].get("fault_id").is_none());
        let event = &json["events"][1];
        assert_eq!(event["kind"], "ptpip.fault.applied");
        assert_eq!(event["operation"], "0x1015");
        assert_eq!(event["transaction_id"], 7);
        assert_eq!(event["response_code"], "0x2001");
        assert_eq!(event["fault_id"], 3);
        assert_eq!(event["fault_kind"], "replaceData");
        assert_eq!(event["payload_summary"]["length"], 4);
        assert_eq!(event["payload_summary"]["sha256"], payload.sha256);
        assert_eq!(event["payload_summary"]["marker"], "faultMutation");
        assert_eq!(event["applied"], "replacedData");
        assert_eq!(event["payload_hex"], serde_json::Value::Null);
    }
}
