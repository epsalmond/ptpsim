use std::collections::BTreeMap;
use std::io::{self, Write};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::Instant;

use camera_config::{
    ActionOutcome, ActionRole, DataDirection, ExecutionContext, LifecycleMarker, ObservationLine,
    ObservationRecorder, PayloadMetadataBuilder, PtpDataPhase, PtpEventRecord, PtpRequest,
    PtpResponse, PtpTransactionRecord, PtpTransport, TransactionOutcome,
};
use camera_protocol_ffi::{
    CameraEvent, ConnectionActivityEvent, PtpFraming, StepOutcome, StepReport,
};
use ptp_core::{PtpCodec, PtpIpPacket};
use serde_json::{json, Value};

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum TraceFormat {
    Jsonl,
    Text,
}

pub struct TraceWriter {
    started: Instant,
    format: TraceFormat,
    output: Mutex<Box<dyn Write + Send>>,
    next_frame_id: AtomicU64,
    failure: Mutex<Option<String>>,
    observations: Option<ObservationRecorder>,
    connection: String,
    mode: String,
    ptp: Mutex<PtpObservationState>,
}

struct PtpObservationState {
    pending: BTreeMap<u32, PendingTransaction>,
    recorded: BTreeMap<u32, String>,
    connection_instance: String,
    session: String,
}

impl Default for PtpObservationState {
    fn default() -> Self {
        Self {
            pending: BTreeMap::new(),
            recorded: BTreeMap::new(),
            connection_instance: "initiator-connection-0000000000000000".into(),
            session: "ptp-session-0000000000000000".into(),
        }
    }
}

struct PendingTransaction {
    request: PtpRequest,
    request_payload: Option<PayloadMetadataBuilder>,
    response_payload: Option<PayloadMetadataBuilder>,
    request_expected: Option<u64>,
    response_expected: Option<u64>,
}

impl TraceWriter {
    pub fn new(format: TraceFormat, output: Box<dyn Write + Send>) -> Self {
        Self {
            started: Instant::now(),
            format,
            output: Mutex::new(output),
            next_frame_id: AtomicU64::new(1),
            failure: Mutex::new(None),
            observations: None,
            connection: "unspecified".into(),
            mode: "unspecified".into(),
            ptp: Mutex::new(PtpObservationState::default()),
        }
    }

    pub fn with_observations(
        format: TraceFormat,
        output: Box<dyn Write + Send>,
        observations: ObservationRecorder,
        connection: String,
        mode: String,
    ) -> Self {
        let mut writer = Self::new(format, output);
        writer.observations = Some(observations);
        writer.connection = connection;
        writer.mode = mode;
        writer
    }

    /// Start a fresh physical PTP session before any transaction frame is
    /// observed. The persisted lifecycle ordinal is the identity, so reconnects
    /// and process restarts cannot reuse a correlation key even when earlier
    /// connection attempts produced no transactions.
    pub fn begin_ptp_session(&self) -> io::Result<()> {
        let Some(observations) = &self.observations else {
            return Ok(());
        };
        let mut state = self
            .ptp
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if !state.pending.is_empty() {
            return Err(io::Error::other(
                "cannot replace a PTP observation session with pending transactions",
            ));
        }
        let sequence = observations
            .record_lifecycle(
                self.context("connectionOpened"),
                LifecycleMarker::ConnectionOpened,
                None,
                None,
                BTreeMap::new(),
            )
            .map_err(|error| io::Error::other(error.to_string()))?;
        state.connection_instance = format!("initiator-connection-{sequence:016x}");
        state.session = format!("ptp-session-{sequence:016x}");
        state.recorded.clear();
        Ok(())
    }

    /// Start a fresh logical PTP session on the current physical connection.
    /// The persisted retry ordinal keeps correlation identities unique when a
    /// protocol recovery resets transaction IDs without replacing the socket.
    pub fn retry_logical_ptp_session(&self) -> io::Result<()> {
        let Some(observations) = &self.observations else {
            return Ok(());
        };
        let mut state = self
            .ptp
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if !state.pending.is_empty() {
            return Err(io::Error::other(
                "cannot replace a logical PTP observation session with pending transactions",
            ));
        }
        let sequence = observations
            .record_lifecycle(
                self.context("logicalSessionRetry"),
                LifecycleMarker::Retry,
                None,
                Some(1),
                BTreeMap::from([("reason".into(), "sessionAlreadyOpen".into())]),
            )
            .map_err(|error| io::Error::other(error.to_string()))?;
        state.session = format!("ptp-session-{sequence:016x}");
        state.recorded.clear();
        Ok(())
    }

    pub fn wire(&self, direction: &str, channel: &str, bytes: &[u8]) -> io::Result<u64> {
        let frame_id = self.next_frame_id.fetch_add(1, Ordering::Relaxed);
        self.emit(json!({
            "kind": "wire",
            "elapsedMs": self.elapsed_ms(),
            "direction": direction,
            "channel": channel,
            "frameId": frame_id,
            "offset": 0,
            "length": bytes.len(),
            "hex": encode_hex(bytes),
        }))?;
        Ok(frame_id)
    }

    /// Feed one complete command-channel frame into the canonical transaction
    /// assembler after the corresponding transport I/O succeeds.
    pub fn ptp_frame(&self, direction: &str, framing: PtpFraming, bytes: &[u8]) -> io::Result<()> {
        if self.observations.is_none() {
            return Ok(());
        }
        let packet = decode_frame(framing, bytes)?;
        let mut state = self
            .ptp
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        match packet {
            PtpIpPacket::OperationRequest(request) if direction == "tx" => {
                if state.pending.contains_key(&request.transaction_id) {
                    return Err(io::Error::other(format!(
                        "duplicate pending PTP transaction {}",
                        request.transaction_id
                    )));
                }
                state.pending.insert(
                    request.transaction_id,
                    PendingTransaction {
                        request: PtpRequest {
                            framing: framing_name(framing).into(),
                            operation: format!("0x{:04x}", request.code),
                            parameters: request.params,
                            data: None,
                        },
                        request_payload: None,
                        response_payload: None,
                        request_expected: None,
                        response_expected: None,
                    },
                );
            }
            PtpIpPacket::StartData(start) => {
                let pending = state
                    .pending
                    .get_mut(&start.transaction_id)
                    .ok_or_else(|| {
                        io::Error::other(format!(
                            "data start references unknown PTP transaction {}",
                            start.transaction_id
                        ))
                    })?;
                let (payload, expected) = payload_side(pending, direction)?;
                payload.get_or_insert_with(PayloadMetadataBuilder::new);
                *expected = Some(start.total_length);
            }
            PtpIpPacket::Data(data) | PtpIpPacket::EndData(data) => {
                let pending = state.pending.get_mut(&data.transaction_id).ok_or_else(|| {
                    io::Error::other(format!(
                        "data frame references unknown PTP transaction {}",
                        data.transaction_id
                    ))
                })?;
                let (payload, _) = payload_side(pending, direction)?;
                payload
                    .get_or_insert_with(PayloadMetadataBuilder::new)
                    .update(&data.payload);
            }
            PtpIpPacket::OperationResponse(response) if direction == "rx" => {
                self.finish_ptp_transaction(
                    &mut state,
                    response.transaction_id,
                    response.code,
                    response.params,
                    None,
                )?;
            }
            _ => {}
        }
        Ok(())
    }

    /// Complete a bounded raw streaming action using metadata accumulated by
    /// the sink; the raw body never needs to be reassembled by the recorder.
    pub fn complete_streaming_transaction(
        &self,
        transaction_id: u32,
        response_params: Vec<u32>,
        payload: camera_config::PayloadMetadata,
    ) -> io::Result<()> {
        self.complete_streaming_response(transaction_id, 0x2001, response_params, Some(payload))
    }

    /// Complete a raw streaming transaction that returned a response before
    /// or after its optional data body. This keeps retryable and terminal
    /// non-OK responses in the canonical observation bundle too.
    pub fn complete_streaming_response(
        &self,
        transaction_id: u32,
        response_code: u16,
        response_params: Vec<u32>,
        payload: Option<camera_config::PayloadMetadata>,
    ) -> io::Result<()> {
        let mut state = self
            .ptp
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        self.finish_ptp_transaction(
            &mut state,
            transaction_id,
            response_code,
            response_params,
            payload,
        )
    }

    pub fn ptp_event(&self, event: &CameraEvent) -> io::Result<()> {
        let Some(observations) = &self.observations else {
            return Ok(());
        };
        let mut state = self
            .ptp
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let transaction_record_id = state.recorded.get(&event.txn).cloned();
        let connection_instance = state.connection_instance.clone();
        let session = state.session.clone();
        observations
            .append(self.context("event"), |common| {
                ObservationLine::PtpEvent(PtpEventRecord {
                    common,
                    connection_instance,
                    session,
                    endpoint_set: "event".into(),
                    transaction_id: event.txn,
                    transaction_record_id,
                    event: format!("0x{:04x}", event.code),
                    parameters: event.params.clone(),
                    payload: None,
                })
            })
            .map(|_| ())
            .map_err(|error| io::Error::other(error.to_string()))?;
        if state.recorded.len() > 4096 {
            if let Some(oldest) = state.recorded.keys().next().copied() {
                state.recorded.remove(&oldest);
            }
        }
        Ok(())
    }

    fn finish_ptp_transaction(
        &self,
        state: &mut PtpObservationState,
        transaction_id: u32,
        response_code: u16,
        response_params: Vec<u32>,
        streamed_payload: Option<camera_config::PayloadMetadata>,
    ) -> io::Result<()> {
        let Some(observations) = &self.observations else {
            return Ok(());
        };
        let mut pending = state.pending.remove(&transaction_id).ok_or_else(|| {
            io::Error::other(format!(
                "response references unknown PTP transaction {transaction_id}"
            ))
        })?;
        if let Some(payload) = pending.request_payload.take() {
            pending.request.data = Some(PtpDataPhase {
                direction: DataDirection::HostToCamera,
                payload: payload.metadata(),
            });
        }
        let response_payload = streamed_payload.or_else(|| {
            pending
                .response_payload
                .take()
                .map(|payload| payload.metadata())
        });
        let request_length = pending
            .request
            .data
            .as_ref()
            .map(|data| data.payload.length);
        let response_length = response_payload.as_ref().map(|payload| payload.length);
        let incomplete = pending
            .request_expected
            .zip(request_length)
            .is_some_and(|(expected, actual)| expected != actual)
            || pending
                .response_expected
                .zip(response_length)
                .is_some_and(|(expected, actual)| expected != actual);
        let outcome = if incomplete {
            TransactionOutcome::Incomplete
        } else if response_code == 0x2001 {
            TransactionOutcome::Ok
        } else {
            TransactionOutcome::NonOk
        };
        let connection_instance = state.connection_instance.clone();
        let session = state.session.clone();
        let ordinal = observations
            .append(self.context("command"), |common| {
                ObservationLine::PtpTransaction(Box::new(PtpTransactionRecord {
                    common,
                    transport: PtpTransport::PtpIp,
                    connection_instance,
                    session,
                    endpoint_set: "command".into(),
                    transaction_id,
                    request: pending.request,
                    response: Some(PtpResponse {
                        code: format!("0x{response_code:04x}"),
                        parameters: response_params,
                        data: response_payload.map(|payload| PtpDataPhase {
                            direction: DataDirection::CameraToHost,
                            payload,
                        }),
                    }),
                    outcome,
                    evidence_basis: None,
                    observed_effect: None,
                    readback: None,
                }))
            })
            .map_err(|error| io::Error::other(error.to_string()))?;
        let record_id = format!("record-{ordinal:016x}");
        state.recorded.insert(transaction_id, record_id);
        Ok(())
    }

    fn fail_ptp_transaction(&self, report: &StepReport, operation: u16) -> io::Result<()> {
        let Some(observations) = &self.observations else {
            return Ok(());
        };
        let transaction_id = report.transaction_id.unwrap_or(0);
        let mut state = self
            .ptp
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut pending =
            state
                .pending
                .remove(&transaction_id)
                .unwrap_or_else(|| PendingTransaction {
                    request: PtpRequest {
                        framing: "manifest-selected".into(),
                        operation: format!("0x{operation:04x}"),
                        parameters: Vec::new(),
                        data: None,
                    },
                    request_payload: None,
                    response_payload: None,
                    request_expected: None,
                    response_expected: None,
                });
        if let Some(payload) = pending.request_payload.take() {
            pending.request.data = Some(PtpDataPhase {
                direction: DataDirection::HostToCamera,
                payload: payload.metadata(),
            });
        }
        let outcome = if report
            .error
            .as_deref()
            .is_some_and(|error| error.contains("timed out") || error.contains("deadline"))
        {
            TransactionOutcome::Timeout
        } else {
            TransactionOutcome::TransportAbort
        };
        let connection_instance = state.connection_instance.clone();
        let session = state.session.clone();
        let ordinal = observations
            .append(self.context("command"), |common| {
                ObservationLine::PtpTransaction(Box::new(PtpTransactionRecord {
                    common,
                    transport: PtpTransport::PtpIp,
                    connection_instance,
                    session,
                    endpoint_set: "command".into(),
                    transaction_id,
                    request: pending.request,
                    response: None,
                    outcome,
                    evidence_basis: None,
                    observed_effect: None,
                    readback: None,
                }))
            })
            .map_err(|error| io::Error::other(error.to_string()))?;
        let record_id = format!("record-{ordinal:016x}");
        state.recorded.insert(transaction_id, record_id);
        Ok(())
    }

    pub fn begin_wire_frame(&self, direction: &str, channel: &str, length: u64) -> io::Result<u64> {
        let frame_id = self.next_frame_id.fetch_add(1, Ordering::Relaxed);
        self.emit(json!({
            "kind": "wireStart",
            "elapsedMs": self.elapsed_ms(),
            "direction": direction,
            "channel": channel,
            "frameId": frame_id,
            "length": length,
        }))?;
        Ok(frame_id)
    }

    pub fn wire_chunk(
        &self,
        direction: &str,
        channel: &str,
        frame_id: u64,
        offset: u64,
        bytes: &[u8],
    ) -> io::Result<()> {
        self.emit(json!({
            "kind": "wireChunk",
            "elapsedMs": self.elapsed_ms(),
            "direction": direction,
            "channel": channel,
            "frameId": frame_id,
            "offset": offset,
            "length": bytes.len(),
            "hex": encode_hex(bytes),
        }))
    }

    pub fn live_view(&self, length: usize) -> io::Result<()> {
        self.emit(json!({
            "kind": "liveView",
            "elapsedMs": self.elapsed_ms(),
            "length": length,
            "ready": true,
        }))
    }

    pub fn step(&self, report: &StepReport) -> io::Result<()> {
        self.emit(json!({
            "kind": "step",
            "elapsedMs": self.elapsed_ms(),
            "path": report.step_path,
            "verb": report.verb,
            "outcome": format!("{:?}", report.outcome),
            "operation": report.operation,
            "property": report.property,
            "response": report.response_code,
            "transactionId": report.transaction_id,
            "tolerant": report.tolerant,
            "attempts": report.attempts,
            "error": report.error,
            "activityId": report.activity_id,
            "activityVersion": report.activity_version,
        }))?;
        if report.outcome == StepOutcome::Started {
            return Ok(());
        }
        let Some(operation) = report.operation else {
            return Ok(());
        };
        if report.outcome == StepOutcome::Failed && report.response_code.is_none() {
            self.fail_ptp_transaction(report, operation)?;
        }
        Ok(())
    }

    pub fn activity(&self, event: &ConnectionActivityEvent) -> io::Result<()> {
        self.emit(json!({
            "kind": "activity",
            "elapsedMs": self.elapsed_ms(),
            "event": format!("{event:?}"),
        }))
    }

    pub fn session(&self, state: &str, detail: Value) -> io::Result<()> {
        self.emit(json!({
            "kind": "session",
            "elapsedMs": self.elapsed_ms(),
            "state": state,
            "detail": detail,
        }))?;
        let marker = match state {
            "pcssDiscoverySent" => Some(LifecycleMarker::Discovery),
            "pcssCallbackAccepted" => Some(LifecycleMarker::Association),
            "eventConnected" | "liveViewConnected" => Some(LifecycleMarker::ConnectionOpened),
            "opened" => Some(LifecycleMarker::SessionOpened),
            "initRetry" | "pcssRediscovery" => Some(LifecycleMarker::Retry),
            "closed" => Some(LifecycleMarker::SessionClosed),
            "poisoned" => Some(LifecycleMarker::Teardown),
            _ => None,
        };
        if let (Some(observations), Some(marker)) = (&self.observations, marker) {
            let attempt = detail
                .get("retry")
                .or_else(|| detail.get("attempt"))
                .and_then(Value::as_u64)
                .and_then(|value| u32::try_from(value).ok());
            let detail = detail
                .as_object()
                .into_iter()
                .flatten()
                .filter(|(key, _)| {
                    matches!(
                        key.as_str(),
                        "reason"
                            | "retry"
                            | "limit"
                            | "attempt"
                            | "mode"
                            | "response"
                            | "transactionId"
                            | "connection"
                            | "port"
                    )
                })
                .map(|(key, value)| {
                    let value = value
                        .as_str()
                        .map(str::to_owned)
                        .unwrap_or_else(|| value.to_string());
                    (key.clone(), value)
                })
                .collect();
            observations
                .record_lifecycle(self.context(state), marker, None, attempt, detail)
                .map_err(|error| io::Error::other(error.to_string()))?;
        }
        Ok(())
    }

    pub fn checkpoint(&self, name: &str, detail: Value) -> io::Result<()> {
        self.emit(json!({
            "kind": "checkpoint",
            "elapsedMs": self.elapsed_ms(),
            "name": name,
            "detail": detail,
        }))
    }

    pub fn outcome(&self, status: &str, detail: Value) -> io::Result<()> {
        self.emit(json!({
            "kind": "outcome",
            "elapsedMs": self.elapsed_ms(),
            "status": status,
            "detail": detail,
        }))
    }

    pub fn action(
        &self,
        catalog_revision: String,
        action_id: String,
        parameters: BTreeMap<String, Value>,
        outcome: ActionOutcome,
    ) -> io::Result<()> {
        let Some(observations) = &self.observations else {
            return Ok(());
        };
        observations
            .record_action(
                self.context("action"),
                catalog_revision,
                action_id,
                ActionRole::Initiator,
                parameters,
                outcome,
            )
            .map(|_| ())
            .map_err(|error| io::Error::other(error.to_string()))
    }

    fn context(&self, state: &str) -> ExecutionContext {
        ExecutionContext {
            connection: self.connection.clone(),
            mode: self.mode.clone(),
            state: state.into(),
        }
    }

    fn elapsed_ms(&self) -> u128 {
        self.started.elapsed().as_millis()
    }

    fn emit(&self, value: Value) -> io::Result<()> {
        if let Some(detail) = self
            .failure
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
        {
            return Err(io::Error::new(io::ErrorKind::BrokenPipe, detail));
        }
        let mut output = self
            .output
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let result = (|| {
            match self.format {
                TraceFormat::Jsonl => serde_json::to_writer(&mut *output, &value)?,
                TraceFormat::Text => write_text(&mut *output, &value)?,
            }
            output.write_all(b"\n")?;
            output.flush()
        })();
        drop(output);
        if let Err(error) = &result {
            *self
                .failure
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(error.to_string());
        }
        result
    }
}

fn payload_side<'a>(
    pending: &'a mut PendingTransaction,
    direction: &str,
) -> io::Result<(&'a mut Option<PayloadMetadataBuilder>, &'a mut Option<u64>)> {
    match direction {
        "tx" => Ok((&mut pending.request_payload, &mut pending.request_expected)),
        "rx" => Ok((
            &mut pending.response_payload,
            &mut pending.response_expected,
        )),
        _ => Err(io::Error::other(format!(
            "unknown PTP frame direction {direction:?}"
        ))),
    }
}

fn framing_name(framing: PtpFraming) -> &'static str {
    match framing {
        PtpFraming::Standard => "standard",
        PtpFraming::Compressed => "compressed",
        PtpFraming::Usb => "usb",
    }
}

fn decode_frame(framing: PtpFraming, bytes: &[u8]) -> io::Result<PtpIpPacket> {
    let result = match framing {
        PtpFraming::Standard => PtpIpPacket::decode(bytes),
        PtpFraming::Compressed => protocol_primitives::fuji_framing::decode(bytes),
        PtpFraming::Usb => protocol_primitives::usb_ptp::decode(bytes),
    };
    result.map_err(|error| io::Error::other(format!("decode observed PTP frame: {error:?}")))
}

fn write_text(mut output: impl Write, value: &Value) -> io::Result<()> {
    let elapsed = value.get("elapsedMs").and_then(Value::as_u64).unwrap_or(0);
    let kind = value
        .get("kind")
        .and_then(Value::as_str)
        .unwrap_or("record");
    write!(output, "{elapsed:>8}ms {kind}")?;
    if let Some(direction) = value.get("direction").and_then(Value::as_str) {
        write!(output, " {direction}")?;
    }
    if let Some(channel) = value.get("channel").and_then(Value::as_str) {
        write!(output, " {channel}")?;
    }
    if let Some(path) = value.get("path").and_then(Value::as_str) {
        write!(output, " {path}")?;
    }
    if let Some(hex) = value.get("hex").and_then(Value::as_str) {
        write!(output, " {hex}")?;
    } else {
        write!(output, " {value}")?;
    }
    Ok(())
}

fn encode_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        encoded.push(HEX[(byte >> 4) as usize] as char);
        encoded.push(HEX[(byte & 0x0f) as usize] as char);
    }
    encoded
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    use camera_config::{
        direct_epistemic, no_loss, BundleHeader, CameraContext, CaptureClock, CaptureContext,
        CaptureInterface, CaptureInterfaceType, ClientContext, ClockType, ClockUnit,
    };

    struct FailingWriter;

    impl Write for FailingWriter {
        fn write(&mut self, _buf: &[u8]) -> io::Result<usize> {
            Err(io::Error::new(io::ErrorKind::BrokenPipe, "injected"))
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    struct TempObservationDir(PathBuf);

    impl TempObservationDir {
        fn new() -> Self {
            let nonce = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("system clock after Unix epoch")
                .as_nanos();
            let path = std::env::temp_dir().join(format!(
                "ptpsim-initiator-trace-{}-{nonce}",
                std::process::id()
            ));
            std::fs::create_dir_all(&path).expect("create temporary observation directory");
            Self(path)
        }

        fn observation_path(&self) -> PathBuf {
            self.0.join("observations.jsonl")
        }
    }

    impl Drop for TempObservationDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn observation_header() -> BundleHeader {
        BundleHeader {
            schema: camera_config::OBSERVATION_SCHEMA_VERSION.into(),
            run_id: "trace-run".into(),
            record_id: "header".into(),
            ordinal: 0,
            camera: CameraContext {
                manufacturer: "EXAMPLE".into(),
                model: "TEST".into(),
                body_id: "body".into(),
                firmware: "1".into(),
            },
            client: ClientContext {
                artifact: "camera-initiator-test".into(),
                version: "1".into(),
                platform: "test".into(),
            },
            capture: CaptureContext {
                interfaces: vec![CaptureInterface {
                    id: "command".into(),
                    interface_type: CaptureInterfaceType::Tcp,
                    role: "initiator".into(),
                }],
                clocks: vec![CaptureClock {
                    id: "process-monotonic".into(),
                    clock_type: ClockType::Monotonic,
                    unit: ClockUnit::Milliseconds,
                }],
                clock_mappings: Vec::new(),
                loss: no_loss(),
                redactions: Vec::new(),
                tool_versions: BTreeMap::new(),
                artifacts: Vec::new(),
            },
            epistemic: direct_epistemic(),
        }
    }

    fn observed_trace(recorder: ObservationRecorder) -> TraceWriter {
        TraceWriter::with_observations(
            TraceFormat::Jsonl,
            Box::new(Vec::<u8>::new()),
            recorder,
            "ptpip".into(),
            "shooting/stills".into(),
        )
    }

    fn record_transaction(trace: &TraceWriter, transaction_id: u32) {
        let request =
            ptp_core::encode(&PtpIpPacket::OperationRequest(ptp_core::OperationRequest {
                data_phase_info: 1,
                code: 0x1001,
                transaction_id,
                params: Vec::new(),
            }))
            .unwrap();
        trace
            .ptp_frame("tx", PtpFraming::Standard, &request)
            .unwrap();
        let response = ptp_core::encode(&PtpIpPacket::OperationResponse(
            ptp_core::OperationResponse {
                code: 0x2001,
                transaction_id,
                params: Vec::new(),
            },
        ))
        .unwrap();
        trace
            .ptp_frame("rx", PtpFraming::Standard, &response)
            .unwrap();
    }

    #[test]
    fn hex_is_lowercase_and_complete() {
        assert_eq!(encode_hex(&[0x00, 0xab, 0xff]), "00abff");
    }

    #[test]
    fn trace_failures_are_sticky() {
        let trace = TraceWriter::new(TraceFormat::Jsonl, Box::new(FailingWriter));
        assert!(trace.session("first", json!({})).is_err());
        let second = trace.session("second", json!({})).unwrap_err();
        assert_eq!(second.kind(), io::ErrorKind::BrokenPipe);
        assert!(second.to_string().contains("injected"));
    }

    #[test]
    fn event_links_are_exact_and_do_not_cross_reconnects() {
        let recorder = ObservationRecorder::open(None, observation_header()).unwrap();
        let trace = observed_trace(recorder.clone());
        trace.begin_ptp_session().unwrap();
        record_transaction(&trace, 7);
        trace
            .ptp_event(&CameraEvent {
                code: 0x4002,
                txn: 7,
                params: Vec::new(),
            })
            .unwrap();
        trace
            .ptp_event(&CameraEvent {
                code: 0x4002,
                txn: 8,
                params: Vec::new(),
            })
            .unwrap();
        trace.begin_ptp_session().unwrap();
        trace
            .ptp_event(&CameraEvent {
                code: 0x4002,
                txn: 7,
                params: Vec::new(),
            })
            .unwrap();

        let export: Value = serde_json::from_str(&recorder.export_json(0).unwrap()).unwrap();
        let records = export["records"].as_array().unwrap();
        let transaction = records
            .iter()
            .find(|record| record["kind"] == "ptpTransaction")
            .unwrap();
        let events = records
            .iter()
            .filter(|record| record["kind"] == "ptpEvent")
            .collect::<Vec<_>>();
        assert_eq!(events.len(), 3);
        assert_eq!(events[0]["transactionId"], 7);
        assert_eq!(events[0]["transactionRecordId"], transaction["recordId"]);
        assert!(events[1].get("transactionRecordId").is_none());
        assert!(events[2].get("transactionRecordId").is_none());
        assert_ne!(events[0]["session"], events[2]["session"]);
    }

    #[test]
    fn connection_identities_survive_empty_reconnects_and_restart() {
        let directory = TempObservationDir::new();
        let path = directory.observation_path();
        let recorder = ObservationRecorder::open(Some(path.clone()), observation_header()).unwrap();
        let trace = observed_trace(recorder);
        trace.begin_ptp_session().unwrap();
        let first = trace.ptp.lock().unwrap().connection_instance.clone();
        trace.begin_ptp_session().unwrap();
        let second = trace.ptp.lock().unwrap().connection_instance.clone();
        assert_ne!(first, second);
        drop(trace);

        let reopened = ObservationRecorder::open(Some(path), observation_header()).unwrap();
        let trace = observed_trace(reopened);
        trace.begin_ptp_session().unwrap();
        let after_restart = trace.ptp.lock().unwrap().connection_instance.clone();
        assert_ne!(after_restart, first);
        assert_ne!(after_restart, second);
        assert_eq!(after_restart, "initiator-connection-0000000000000003");
    }
}
