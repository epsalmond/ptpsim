//! Bounded-memory executor for a single whole-object action.
//!
//! Compressed PTP channels carry object data as one length-prefixed frame. The
//! ordinary executor accepts complete frames, so this separate seam lets the
//! host provide bounded raw reads and a streaming sink without changing the
//! action or framing contracts.

use std::future::Future;
use std::sync::Arc;

use futures_util::future::{select, Either};
use ptp_core::codes::resp;

use crate::{
    frame_encode, ActionVerb, ConfigStore, PtpFraming, PtpRuntimeValue, PtpTransportError,
};

const HEADER_BYTES: usize = 12;
const MAX_STREAM_CHUNK: usize = 1024 * 1024;
const MAX_RESPONSE_BYTES: usize = 12 + (5 * 4);
const IO_TIMEOUT_MS: u32 = 10_000;

#[derive(Debug, thiserror::Error, uniffi::Error)]
pub enum PtpStreamingSinkError {
    #[error("stream sink failure: {detail}")]
    Failed { detail: String },
}

/// Host-owned raw I/O for one streamed command transaction.
#[uniffi::export(with_foreign)]
#[async_trait::async_trait]
pub trait PtpStreamingTransport: Send + Sync {
    async fn reserve_transaction_id(&self) -> Result<u32, PtpTransportError>;
    async fn send_command_frame(&self, frame: Vec<u8>) -> Result<(), PtpTransportError>;
    /// Return 1...`max_bytes` raw bytes from the command stream. EOF is an
    /// error while a transaction is active.
    async fn receive_command_bytes(&self, max_bytes: u32) -> Result<Vec<u8>, PtpTransportError>;
    async fn sleep(&self, ms: u32) -> Result<(), PtpTransportError>;
    /// Synchronously cancel and poison the command session. The executor calls
    /// this from its cancellation guard when a compressed frame is only partly
    /// consumed; the host must not return that session to its pool.
    fn invalidate_command_session(&self, reason: String);
}

/// Host-owned destination, normally an already-created temporary file.
#[uniffi::export(with_foreign)]
#[async_trait::async_trait]
pub trait PtpStreamingSink: Send + Sync {
    async fn begin(&self, total_bytes: u64) -> Result<(), PtpStreamingSinkError>;
    async fn write(&self, chunk: Vec<u8>) -> Result<(), PtpStreamingSinkError>;
}

#[derive(Debug, uniffi::Record)]
pub struct PtpStreamingOutcome {
    pub operation: u16,
    pub transaction_id: u32,
    pub total_bytes: u64,
    pub response_params: Vec<u32>,
}

#[derive(Debug, thiserror::Error, uniffi::Error)]
pub enum PtpStreamingError {
    #[error("unknown streaming action: {detail}")]
    UnknownPlan { detail: String },
    #[error("unsupported streaming action: {detail}")]
    UnsupportedPlan { detail: String },
    #[error("invalid runtime parameters: {detail}")]
    InvalidRuntime { detail: String },
    #[error("stream transport failure: {detail}")]
    Transport { detail: String },
    #[error("stream I/O deadline exceeded during {stage}")]
    DeadlineExceeded { stage: String },
    #[error("invalid streamed frame: {detail}")]
    Framing { detail: String },
    #[error("stream sink failure: {detail}")]
    Sink { detail: String },
    #[error("PTP response 0x{response_code:04x} for transaction {transaction_id}")]
    Response {
        response_code: u16,
        transaction_id: u32,
        response_params: Vec<u32>,
    },
}

struct SessionGuard {
    transport: Arc<dyn PtpStreamingTransport>,
    armed: bool,
    reason: String,
}

impl SessionGuard {
    fn new(transport: Arc<dyn PtpStreamingTransport>) -> Self {
        Self {
            transport,
            armed: false,
            reason: "streaming transaction did not consume a complete response".into(),
        }
    }

    fn arm(&mut self) {
        self.armed = true;
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for SessionGuard {
    fn drop(&mut self) {
        if self.armed {
            self.transport
                .invalidate_command_session(self.reason.clone());
        }
    }
}

#[uniffi::export]
#[allow(clippy::too_many_arguments)]
pub async fn run_streaming_action(
    store: Arc<ConfigStore>,
    connection: String,
    action: ActionVerb,
    transport: Arc<dyn PtpStreamingTransport>,
    sink: Arc<dyn PtpStreamingSink>,
    runtime_params: Vec<PtpRuntimeValue>,
    expected_payload_bytes: Option<u64>,
) -> Result<PtpStreamingOutcome, PtpStreamingError> {
    let connection_config = store
        .inner
        .manifest
        .connections
        .get(&connection)
        .ok_or_else(|| PtpStreamingError::UnknownPlan {
            detail: format!("unknown connection '{connection}'"),
        })?;
    if connection_config.command_framing != Some(camera_config::WireFraming::Compressed) {
        return Err(PtpStreamingError::UnsupportedPlan {
            detail: format!("connection '{connection}' is not compressed framed"),
        });
    }
    let action_verb = super::ffi_to_cc_verb(action);
    let transfer = connection_config.object_transfer.as_ref().ok_or_else(|| {
        PtpStreamingError::UnsupportedPlan {
            detail: format!("connection '{connection}' has no object-transfer contract"),
        }
    })?;
    if transfer.strategy != camera_config::ObjectTransferStrategy::WholeObject
        || transfer.read_action != action_verb
    {
        return Err(PtpStreamingError::UnsupportedPlan {
            detail: format!(
                "connection '{connection}' does not select that action for whole-object transfer"
            ),
        });
    }
    let action = connection_config.actions.get(&action_verb).ok_or_else(|| {
        PtpStreamingError::UnknownPlan {
            detail: format!("connection '{connection}' does not declare that action"),
        }
    })?;
    let (operation, params) = streaming_request(action, runtime_params)?;

    let transaction_id = with_deadline(
        Arc::clone(&transport),
        transport.reserve_transaction_id(),
        "transaction reservation",
    )
    .await?;
    let request = ptp_core::PtpIpPacket::OperationRequest(ptp_core::OperationRequest {
        data_phase_info: 1,
        code: operation,
        transaction_id,
        params,
    });
    let frame = frame_encode(PtpFraming::Compressed, &request).map_err(|error| {
        PtpStreamingError::Framing {
            detail: error.to_string(),
        }
    })?;

    let mut guard = SessionGuard::new(Arc::clone(&transport));
    guard.arm();
    with_deadline(
        Arc::clone(&transport),
        transport.send_command_frame(frame),
        "command write",
    )
    .await?;

    let header = read_exact(&transport, HEADER_BYTES, "data header").await?;
    let declared_length = u32::from_le_bytes(header[0..4].try_into().unwrap()) as usize;
    let packet_type = u16::from_le_bytes(header[4..6].try_into().unwrap());
    let echoed_operation = u16::from_le_bytes(header[6..8].try_into().unwrap());
    let echoed_transaction = u32::from_le_bytes(header[8..12].try_into().unwrap());
    if declared_length < HEADER_BYTES {
        return Err(PtpStreamingError::Framing {
            detail: format!("data frame length {declared_length} is smaller than its header"),
        });
    }
    if packet_type == 3 {
        if !valid_response_length(declared_length) {
            return Err(PtpStreamingError::Framing {
                detail: format!("invalid compressed response length {declared_length}"),
            });
        }
        let mut response_frame = header;
        response_frame.extend_from_slice(
            &read_exact(
                &transport,
                declared_length - HEADER_BYTES,
                "early response body",
            )
            .await?,
        );
        let response = decode_response(&response_frame, transaction_id)?;
        guard.disarm();
        if response.code != resp::OK {
            return Err(PtpStreamingError::Response {
                response_code: response.code,
                transaction_id,
                response_params: response.params,
            });
        }
        return Err(PtpStreamingError::Framing {
            detail: "operation returned OK without the declared data phase".into(),
        });
    }
    if packet_type != 2 || echoed_operation != operation || echoed_transaction != transaction_id {
        return Err(PtpStreamingError::Framing {
            detail: format!(
                "expected data type/op/txn 2/0x{operation:04x}/{transaction_id}, got {packet_type}/0x{echoed_operation:04x}/{echoed_transaction}"
            ),
        });
    }
    let total_bytes = (declared_length - HEADER_BYTES) as u64;
    if let Some(expected) = expected_payload_bytes {
        if expected != total_bytes {
            return Err(PtpStreamingError::Framing {
                detail: format!(
                    "streamed payload length {total_bytes} does not match expected {expected}"
                ),
            });
        }
    }
    sink.begin(total_bytes)
        .await
        .map_err(|error| PtpStreamingError::Sink {
            detail: error.to_string(),
        })?;

    let mut remaining = total_bytes;
    while remaining > 0 {
        let requested = remaining.min(MAX_STREAM_CHUNK as u64) as usize;
        let chunk = read_exact(&transport, requested, "data body").await?;
        sink.write(chunk)
            .await
            .map_err(|error| PtpStreamingError::Sink {
                detail: error.to_string(),
            })?;
        remaining -= requested as u64;
    }

    let response_length_bytes = read_exact(&transport, 4, "response header").await?;
    let response_length =
        u32::from_le_bytes(response_length_bytes[0..4].try_into().unwrap()) as usize;
    if !valid_response_length(response_length) {
        return Err(PtpStreamingError::Framing {
            detail: format!("invalid compressed response length {response_length}"),
        });
    }
    let mut response_frame = response_length_bytes;
    response_frame
        .extend_from_slice(&read_exact(&transport, response_length - 4, "response body").await?);
    let response = decode_response(&response_frame, transaction_id)?;
    guard.disarm();
    if response.code != resp::OK {
        return Err(PtpStreamingError::Response {
            response_code: response.code,
            transaction_id,
            response_params: response.params,
        });
    }
    Ok(PtpStreamingOutcome {
        operation,
        transaction_id,
        total_bytes,
        response_params: response.params,
    })
}

fn decode_response(
    frame: &[u8],
    transaction_id: u32,
) -> Result<ptp_core::OperationResponse, PtpStreamingError> {
    let response = match crate::frame_decode(PtpFraming::Compressed, frame) {
        Ok(ptp_core::PtpIpPacket::OperationResponse(response)) => response,
        Ok(other) => {
            return Err(PtpStreamingError::Framing {
                detail: format!("expected operation response, got {other:?}"),
            })
        }
        Err(error) => {
            return Err(PtpStreamingError::Framing {
                detail: error.to_string(),
            })
        }
    };
    if response.transaction_id != transaction_id {
        return Err(PtpStreamingError::Framing {
            detail: format!(
                "expected response transaction {transaction_id}, got {}",
                response.transaction_id
            ),
        });
    }
    Ok(response)
}

fn valid_response_length(length: usize) -> bool {
    (HEADER_BYTES..=MAX_RESPONSE_BYTES).contains(&length)
        && (length - HEADER_BYTES).is_multiple_of(4)
}

fn streaming_request(
    action: &camera_config::Action,
    runtime_params: Vec<PtpRuntimeValue>,
) -> Result<(u16, Vec<u32>), PtpStreamingError> {
    if action.steps.len() != 1 || action.steps[0].repeat != 1 {
        return Err(PtpStreamingError::UnsupportedPlan {
            detail: "streaming action must contain exactly one unrepeated step".into(),
        });
    }
    let step = &action.steps[0];
    let operation = step
        .send_op
        .as_deref()
        .and_then(camera_config::parse_hex_code)
        .ok_or_else(|| PtpStreamingError::UnsupportedPlan {
            detail: "streaming action step must be sendOp".into(),
        })?;
    let runtime: std::collections::BTreeMap<_, _> = runtime_params
        .into_iter()
        .map(|value| (value.key, value.value))
        .collect();
    let mut params = Vec::with_capacity(step.params.len());
    for param in &step.params {
        let value = match param {
            camera_config::StepParam::Literal(value) => *value as u64,
            camera_config::StepParam::Runtime {
                runtime: slot,
                shift,
                mask,
            } => {
                let value = runtime.get(slot).copied().ok_or_else(|| {
                    PtpStreamingError::InvalidRuntime {
                        detail: format!("runtime slot '{slot}' is unbound"),
                    }
                })?;
                value.checked_shr(*shift).unwrap_or(0) & mask.unwrap_or(u64::MAX)
            }
        };
        params.push(
            u32::try_from(value).map_err(|_| PtpStreamingError::InvalidRuntime {
                detail: format!("parameter value {value} exceeds u32"),
            })?,
        );
    }
    Ok((operation, params))
}

async fn read_exact(
    transport: &Arc<dyn PtpStreamingTransport>,
    length: usize,
    stage: &str,
) -> Result<Vec<u8>, PtpStreamingError> {
    let mut bytes = Vec::with_capacity(length);
    while bytes.len() < length {
        let remaining = length - bytes.len();
        let chunk = with_deadline(
            Arc::clone(transport),
            transport.receive_command_bytes(remaining as u32),
            stage,
        )
        .await?;
        if chunk.is_empty() {
            return Err(PtpStreamingError::Transport {
                detail: format!("command stream reached EOF during {stage}"),
            });
        }
        if chunk.len() > remaining {
            return Err(PtpStreamingError::Framing {
                detail: format!(
                    "transport returned {} bytes after a maximum request of {remaining}",
                    chunk.len()
                ),
            });
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

async fn with_deadline<T, F>(
    transport: Arc<dyn PtpStreamingTransport>,
    future: F,
    stage: &str,
) -> Result<T, PtpStreamingError>
where
    F: Future<Output = Result<T, PtpTransportError>> + Send,
{
    match select(Box::pin(future), Box::pin(transport.sleep(IO_TIMEOUT_MS))).await {
        Either::Left((result, pending_clock)) => {
            drop(pending_clock);
            result.map_err(|error| PtpStreamingError::Transport {
                detail: error.to_string(),
            })
        }
        Either::Right((clock, pending_io)) => {
            drop(pending_io);
            match clock {
                Ok(()) => Err(PtpStreamingError::DeadlineExceeded {
                    stage: stage.to_string(),
                }),
                Err(error) => Err(PtpStreamingError::Transport {
                    detail: error.to_string(),
                }),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runtime_shift_beyond_value_width_resolves_to_zero() {
        let action = camera_config::Action {
            steps: vec![camera_config::Step {
                send_op: Some("0x1009".into()),
                params: vec![camera_config::StepParam::Runtime {
                    runtime: "handle".into(),
                    shift: 64,
                    mask: None,
                }],
                repeat: 1,
                ..Default::default()
            }],
            ..Default::default()
        };
        let (_, params) = streaming_request(
            &action,
            vec![PtpRuntimeValue {
                key: "handle".into(),
                value: u64::MAX,
            }],
        )
        .expect("overshift follows the executor's zero-fill semantics");
        assert_eq!(params, vec![0]);
    }
}
