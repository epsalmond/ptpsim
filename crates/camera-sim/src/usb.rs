//! Scripted in-memory USB responder (issue #342): the device side of the USB
//! establishment plans (§11.29), the USB counterpart to the BLE scripted
//! responder (`ble.rs`). One responder backs both USB seams: the raw side
//! (interface claim, bulk OUT/IN, interrupt IN) scripts the device a raw
//! `usb` establishment talks to, and the transaction side (typed PTP
//! transactions, code-selective events) scripts the daemon a
//! `usb-passthrough` connection attaches to.
//!
//! Like [`crate::BleResponder`], this is generic: behavior comes from
//! per-test scripting, never from vendor branches. The responder is
//! synchronous and FFI-free; each test's thin adapter maps it onto
//! `UsbExecutorTransport` or `PtpTransactionTransport` and owns the async
//! deadline plumbing the deterministic responder does not model (the BLE
//! `ResponderTransport` precedent in the executor seam tests).

use std::collections::{BTreeMap, VecDeque};

use protocol_primitives::usb_ptp;
use ptp_core::PtpIpPacket;

/// One interaction the responder observed, in arrival order. Tests assert on
/// this log to prove a plan drove the device in the expected order (the
/// [`crate::BleEvent`] precedent).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UsbEvent {
    /// The host claimed an interface (raw side).
    Claim {
        class: u8,
        subclass: u8,
        protocol: u8,
    },
    /// One bulk OUT transfer, raw bytes as written (raw side).
    BulkOut { data: Vec<u8> },
    /// One bulk IN read request (raw side).
    BulkIn { max_length: u32 },
    /// The host awaited an interrupt IN frame (raw side).
    AwaitInterrupt,
    /// The host released the interface and closed the device (raw side).
    ReleaseAndClose,
    /// One typed transaction (transaction side). `timeout_ms` is the
    /// per-call budget the executor handed the daemon to enforce.
    Transaction {
        opcode: u16,
        params: Vec<u32>,
        data_out: Option<Vec<u8>>,
        timeout_ms: u32,
    },
    /// The host awaited a typed event (transaction side).
    EventWait { event_code: u16 },
}

/// The responder failure surface, the in-memory analogue of the USB stack
/// errors a platform reports. Adapters map these onto the executor seam's
/// own error vocabulary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UsbError {
    /// A bulk transfer arrived before any claim (raw side).
    NotClaimed,
    /// The interface is already claimed.
    AlreadyClaimed,
    /// Scripted claim refusal; `owner` names the driver holding the
    /// interface when the platform reports one.
    ClaimRefused { owner: Option<String> },
    /// A bulk OUT transfer did not match its scripted expectation.
    UnexpectedBulkOut { expected: String, actual: String },
    /// A scripted command-container expectation got a transfer the `usb-ptp`
    /// codec could not decode.
    UndecodableBulkOut { data: Vec<u8> },
    /// Scripted endpoint STALL.
    Stall { detail: String },
    /// A bulk IN read arrived with no scripted reply queued.
    NoScriptedBulkIn,
}

impl std::fmt::Display for UsbError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            UsbError::NotClaimed => write!(f, "interface not claimed"),
            UsbError::AlreadyClaimed => write!(f, "interface already claimed"),
            UsbError::ClaimRefused { owner } => write!(
                f,
                "interface claim refused{}",
                owner
                    .as_ref()
                    .map(|owner| format!(" (owner: {owner})"))
                    .unwrap_or_default()
            ),
            UsbError::UnexpectedBulkOut { expected, actual } => {
                write!(f, "unexpected bulk OUT: {actual}; expected {expected}")
            }
            UsbError::UndecodableBulkOut { data } => {
                write!(f, "bulk OUT is not a decodable container: {data:02x?}")
            }
            UsbError::Stall { detail } => write!(f, "endpoint stalled: {detail}"),
            UsbError::NoScriptedBulkIn => write!(f, "no scripted bulk IN reply"),
        }
    }
}

impl std::error::Error for UsbError {}

/// The transaction-side reply to one typed PTP transaction, the daemon's
/// answer: a response code, response parameters, and optional data-in
/// payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsbTxnReply {
    pub response_code: u16,
    pub params: Vec<u32>,
    pub data_in: Option<Vec<u8>>,
}

impl UsbTxnReply {
    /// An OK (0x2001) response carrying `data_in`.
    pub fn ok(data_in: Option<Vec<u8>>) -> Self {
        UsbTxnReply {
            response_code: 0x2001,
            params: Vec::new(),
            data_in,
        }
    }

    /// A full response, for non-OK replies or response parameters.
    pub fn response(response_code: u16, params: Vec<u32>, data_in: Option<Vec<u8>>) -> Self {
        UsbTxnReply {
            response_code,
            params,
            data_in,
        }
    }
}

/// One scripted bulk OUT result. Builder call order determines whether the
/// next transfer is checked or stalled. An empty queue accepts any transfer
/// (the permissive default, the BLE scripted-GATT empty-script precedent).
#[derive(Debug, Clone, PartialEq, Eq)]
enum BulkOutScript {
    /// A command container with this code, transaction id, and parameters,
    /// compared after decoding with the `usb-ptp` codec.
    Command {
        code: u16,
        transaction_id: u32,
        params: Vec<u32>,
    },
    /// Exact bytes, for transfers that are not command containers.
    Bytes(Vec<u8>),
    /// Answer the transfer with an endpoint STALL.
    Stall(String),
}

/// A scripted interrupt IN frame (raw side). A `lost` frame is never
/// delivered, the in-memory analogue of a dropped interrupt URB.
#[derive(Debug, Clone)]
struct ScriptedInterrupt {
    payload: Vec<u8>,
    lost: bool,
}

/// A scripted typed event (transaction side). A `lost` event is never
/// delivered, the in-memory analogue of a `bestEffort` daemon dropping a
/// push.
#[derive(Debug, Clone)]
struct ScriptedTxnEvent {
    event_code: u16,
    params: Vec<u32>,
    lost: bool,
}

/// Deterministic in-memory USB device. Script the exchange with the
/// builder-style methods, then let a test adapter drive the host-call
/// surface (`claim`, `bulk_out`, `bulk_in`, `next_interrupt_event`,
/// `release_and_close` raw-side; `execute`, `poll_event` transaction-side).
/// Every host call lands in [`log`](Self::log) in arrival order.
#[derive(Default)]
pub struct UsbResponder {
    claimed: Option<(u8, u8, u8)>,
    claim_refusal: Option<Option<String>>,
    bulk_out_script: VecDeque<BulkOutScript>,
    bulk_in_replies: VecDeque<Vec<u8>>,
    interrupts: VecDeque<ScriptedInterrupt>,
    txn_replies: BTreeMap<(u16, Vec<u32>), UsbTxnReply>,
    txn_events: VecDeque<ScriptedTxnEvent>,
    log: Vec<UsbEvent>,
}

impl UsbResponder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Script the next claim refusing, like another driver holding the
    /// interface.
    pub fn with_claim_refusal(mut self, owner: Option<String>) -> Self {
        self.claim_refusal = Some(owner);
        self
    }

    /// Require the next scripted bulk OUT transfer to decode as this command
    /// container (operation code, transaction id, parameters).
    pub fn expect_bulk_out_command(
        mut self,
        code: u16,
        transaction_id: u32,
        params: &[u32],
    ) -> Self {
        self.bulk_out_script.push_back(BulkOutScript::Command {
            code,
            transaction_id,
            params: params.to_vec(),
        });
        self
    }

    /// Require the next scripted bulk OUT transfer to be exactly these bytes.
    pub fn expect_bulk_out_bytes(mut self, data: &[u8]) -> Self {
        self.bulk_out_script
            .push_back(BulkOutScript::Bytes(data.to_vec()));
        self
    }

    /// Script the next bulk OUT transfer answering STALL.
    pub fn queue_bulk_out_stall(mut self, detail: &str) -> Self {
        self.bulk_out_script
            .push_back(BulkOutScript::Stall(detail.to_string()));
        self
    }

    /// Queue one bulk IN reply, consumed in arrival order.
    pub fn queue_bulk_in(mut self, data: &[u8]) -> Self {
        self.bulk_in_replies.push_back(data.to_vec());
        self
    }

    /// Inject one interrupt IN frame. A `lost` frame is consumed by the wait
    /// but never delivered.
    pub fn inject_interrupt_frame(mut self, payload: &[u8], lost: bool) -> Self {
        self.interrupts.push_back(ScriptedInterrupt {
            payload: payload.to_vec(),
            lost,
        });
        self
    }

    /// Script the transaction reply for `(opcode, params)`. An unscripted
    /// transaction answers OK with no data, the permissive default.
    pub fn reply_transaction(mut self, opcode: u16, params: &[u32], reply: UsbTxnReply) -> Self {
        self.txn_replies.insert((opcode, params.to_vec()), reply);
        self
    }

    /// Inject one typed event. A `lost` event is consumed by the wait but
    /// never delivered (the `bestEffort` drop case).
    pub fn inject_event(mut self, event_code: u16, params: &[u32], lost: bool) -> Self {
        self.txn_events.push_back(ScriptedTxnEvent {
            event_code,
            params: params.to_vec(),
            lost,
        });
        self
    }

    /// Claim an interface (raw side).
    pub fn claim(&mut self, class: u8, subclass: u8, protocol: u8) -> Result<(), UsbError> {
        self.log.push(UsbEvent::Claim {
            class,
            subclass,
            protocol,
        });
        if self.claimed.is_some() {
            return Err(UsbError::AlreadyClaimed);
        }
        if let Some(owner) = self.claim_refusal.take() {
            return Err(UsbError::ClaimRefused { owner });
        }
        self.claimed = Some((class, subclass, protocol));
        Ok(())
    }

    /// Write one bulk OUT transfer (raw side). The next declared script item
    /// is consumed, so mixed expectations and STALLs preserve builder order.
    pub fn bulk_out(&mut self, data: &[u8]) -> Result<(), UsbError> {
        self.log.push(UsbEvent::BulkOut {
            data: data.to_vec(),
        });
        if self.claimed.is_none() {
            return Err(UsbError::NotClaimed);
        }
        match self.bulk_out_script.pop_front() {
            None => Ok(()),
            Some(BulkOutScript::Stall(detail)) => Err(UsbError::Stall { detail }),
            Some(BulkOutScript::Bytes(expected)) => {
                if expected == data {
                    Ok(())
                } else {
                    Err(UsbError::UnexpectedBulkOut {
                        expected: format!("bytes {expected:02x?}"),
                        actual: format!("bytes {data:02x?}"),
                    })
                }
            }
            Some(BulkOutScript::Command {
                code,
                transaction_id,
                params,
            }) => match usb_ptp::decode(data) {
                Ok(PtpIpPacket::OperationRequest(request))
                    if request.code == code
                        && request.transaction_id == transaction_id
                        && request.params == params =>
                {
                    Ok(())
                }
                Ok(packet) => Err(UsbError::UnexpectedBulkOut {
                    expected: format!(
                        "command {code:#06x} tid {transaction_id} params {params:02x?}"
                    ),
                    actual: format!("{packet:?}"),
                }),
                Err(_) => Err(UsbError::UndecodableBulkOut {
                    data: data.to_vec(),
                }),
            },
        }
    }

    /// Read one bulk IN transfer (raw side): the next queued reply.
    pub fn bulk_in(&mut self, max_length: u32) -> Result<Vec<u8>, UsbError> {
        self.log.push(UsbEvent::BulkIn { max_length });
        if self.claimed.is_none() {
            return Err(UsbError::NotClaimed);
        }
        self.bulk_in_replies
            .pop_front()
            .ok_or(UsbError::NoScriptedBulkIn)
    }

    /// Await one interrupt IN frame (raw side). Returns `None` when nothing
    /// is delivered (empty queue, or a `lost` frame, which is consumed): the
    /// adapter then pends so the executor's deadline owns the outcome.
    pub fn next_interrupt_event(&mut self) -> Option<Vec<u8>> {
        self.log.push(UsbEvent::AwaitInterrupt);
        match self.interrupts.pop_front() {
            Some(ScriptedInterrupt {
                payload,
                lost: false,
            }) => Some(payload),
            _ => None,
        }
    }

    /// Release the claimed interface and close the device handle (raw side).
    pub fn release_and_close(&mut self) {
        self.log.push(UsbEvent::ReleaseAndClose);
        self.claimed = None;
    }

    /// Run one typed PTP transaction (transaction side).
    pub fn execute(
        &mut self,
        opcode: u16,
        params: &[u32],
        data_out: Option<&[u8]>,
        timeout_ms: u32,
    ) -> UsbTxnReply {
        self.log.push(UsbEvent::Transaction {
            opcode,
            params: params.to_vec(),
            data_out: data_out.map(<[u8]>::to_vec),
            timeout_ms,
        });
        self.txn_replies
            .get(&(opcode, params.to_vec()))
            .cloned()
            .unwrap_or_else(|| UsbTxnReply::ok(None))
    }

    /// Await the next scripted event matching `event_code`, retaining
    /// unrelated events for their normal consumers (the code-selective
    /// contract of `PtpTransactionTransport::next_event`). Returns `None`
    /// when nothing matching is delivered (no script, or a `lost` event,
    /// which is consumed): the adapter then pends so the executor's deadline
    /// owns the outcome.
    pub fn poll_event(&mut self, event_code: u16) -> Option<(u16, Vec<u32>)> {
        self.log.push(UsbEvent::EventWait { event_code });
        let index = self
            .txn_events
            .iter()
            .position(|event| event.event_code == event_code)?;
        let event = self.txn_events.remove(index).expect("positioned above");
        (!event.lost).then_some((event.event_code, event.params))
    }

    /// Every interaction, in arrival order.
    pub fn log(&self) -> &[UsbEvent] {
        &self.log
    }
}
