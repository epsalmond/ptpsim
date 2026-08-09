//! Issue #342 acceptance: the scripted USB responder
//! (`camera_sim::usb::UsbResponder`) backs transaction-seam tests and enforces
//! its command script directly. Inline `usb-passthrough` plans exercise
//! `run_initiator_action_txn`, including a lost `bestEffort` event reconciling
//! through its declared `thenPoll` (§11.29).

use std::sync::{Arc, Mutex};

use camera_protocol_ffi::{
    run_initiator_action_txn, ActionInvocationRequest, ActionRole, ConfigStore,
    ConnectionActivityEvent, ConnectionActivityObserver, PtpTransactionError, PtpTransactionEvent,
    PtpTransactionResult, PtpTransactionTransport, StepObserver, StepReport,
};
use camera_sim::usb::{UsbError, UsbEvent, UsbResponder, UsbTxnReply};
use futures::executor::block_on;
use ptp_core::Writer;

/// PTP-over-USB OpenSession command container (session id 1, transaction 0).
const OPEN_SESSION_CONTAINER: [u8; 16] = [
    0x10, 0x00, 0x00, 0x00, // length
    0x01, 0x00, // command container
    0x02, 0x10, // OpenSession
    0x00, 0x00, 0x00, 0x00, // transaction id
    0x01, 0x00, 0x00, 0x00, // session id
];

/// PTP-over-USB GetDeviceInfo command container (transaction 1).
const GET_DEVICE_INFO_CONTAINER: [u8; 12] = [
    0x0c, 0x00, 0x00, 0x00, // length
    0x01, 0x00, // command container
    0x01, 0x10, // GetDeviceInfo
    0x01, 0x00, 0x00, 0x00, // transaction id
];

/// A minimal PIMA DeviceInfo dataset: standard version 100, no vendor
/// extension, the three session operations in the operations array, empty
/// strings elsewhere. Enough for the conformance layer to verify a capture
/// decodes, without modelling a body.
fn device_info_dataset() -> Vec<u8> {
    fn empty_ptp_string(w: &mut Writer) {
        w.u8(0);
    }
    fn u16_array(w: &mut Writer, values: &[u16]) {
        w.u32(values.len() as u32);
        for value in values {
            w.u16(*value);
        }
    }
    let mut w = Writer::new();
    w.u16(100); // standard version
    w.u32(0); // vendor extension id
    w.u16(0); // vendor extension version
    empty_ptp_string(&mut w); // vendor extension description
    w.u16(0); // functional mode
    u16_array(&mut w, &[0x1001, 0x1002, 0x1003]); // operations supported
    u16_array(&mut w, &[]); // events supported
    u16_array(&mut w, &[]); // device properties supported
    u16_array(&mut w, &[]); // capture formats
    u16_array(&mut w, &[]); // image formats
    empty_ptp_string(&mut w); // manufacturer
    empty_ptp_string(&mut w); // model
    empty_ptp_string(&mut w); // device version
    empty_ptp_string(&mut w); // serial number
    w.into_vec()
}

/// An inline `usb-passthrough` connection carrying one action with the given
/// id, delivery trait, and initiator steps.
fn passthrough_store(delivery: &str, action_id: &str, steps: &str) -> Arc<ConfigStore> {
    ConfigStore::from_bundle(
        format!(
            r#"schema: camera-config/v1
camera: {{ manufacturer: Test, model: Txn, firmware: "1" }}
properties:
  "0xd209": {{ name: autofocusResult, type: u16, access: readWrite }}
connections:
  usbTether:
    kind: usb-passthrough
    session: {{ ownership: daemonAttached }}
    events: {{ delivery: {delivery} }}
    actions:
      {action_id}:
        mode: ""
        initiator:
          steps:
{steps}
"#
        ),
        None,
    )
    .expect("passthrough store loads")
}

fn action_request(store: &ConfigStore, action_id: &str) -> ActionInvocationRequest {
    ActionInvocationRequest {
        catalog_revision: store.action_catalog().revision,
        action_id: action_id.into(),
        connection: "usbTether".into(),
        mode: String::new(),
        role: ActionRole::Initiator,
        parameters: Vec::new(),
    }
}

#[derive(Default)]
struct StepRecorder(Mutex<Vec<StepReport>>);

impl StepObserver for StepRecorder {
    fn on_step(&self, report: StepReport) {
        self.0.lock().expect("steps").push(report);
    }
}

#[derive(Default)]
struct ActivityRecorder(Mutex<Vec<ConnectionActivityEvent>>);

impl ConnectionActivityObserver for ActivityRecorder {
    fn on_activity(&self, event: ConnectionActivityEvent) {
        self.0.lock().expect("activities").push(event);
    }
}

// Transaction seam backed by the shared deterministic responder.

struct ResponderTxnTransport {
    responder: Mutex<UsbResponder>,
    /// Wall clock that pends on exact `ms` values so a test picks which
    /// executor deadline race fires.
    pends_at: Vec<u32>,
}

impl ResponderTxnTransport {
    fn new(responder: UsbResponder, pends_at: &[u32]) -> Self {
        ResponderTxnTransport {
            responder: Mutex::new(responder),
            pends_at: pends_at.to_vec(),
        }
    }

    fn log(&self) -> Vec<UsbEvent> {
        self.responder.lock().expect("responder").log().to_vec()
    }
}

#[async_trait::async_trait]
impl PtpTransactionTransport for ResponderTxnTransport {
    async fn execute(
        &self,
        opcode: u16,
        params: Vec<u32>,
        data_out: Option<Vec<u8>>,
        timeout_ms: u32,
    ) -> Result<PtpTransactionResult, PtpTransactionError> {
        let reply = self.responder.lock().expect("responder").execute(
            opcode,
            &params,
            data_out.as_deref(),
            timeout_ms,
        );
        Ok(PtpTransactionResult {
            response_code: reply.response_code,
            params: reply.params,
            data_in: reply.data_in,
        })
    }

    async fn read_partial_object(
        &self,
        _handle: u32,
        _offset: u64,
        _length: u32,
        _timeout_ms: u32,
    ) -> Result<Vec<u8>, PtpTransactionError> {
        Err(PtpTransactionError::Failed {
            detail: "readPartialObject is not scripted in this test".into(),
        })
    }

    async fn next_event(
        &self,
        event_code: u16,
    ) -> Result<PtpTransactionEvent, PtpTransactionError> {
        let event = self
            .responder
            .lock()
            .expect("responder")
            .poll_event(event_code);
        match event {
            Some((event_code, params)) => Ok(PtpTransactionEvent { event_code, params }),
            // A lost or unscripted event never arrives; the executor's
            // deadline owns the outcome.
            None => futures::future::pending().await,
        }
    }

    async fn shutdown(&self) -> Result<(), PtpTransactionError> {
        Ok(())
    }

    async fn sleep(&self, ms: u32) -> Result<(), PtpTransactionError> {
        if self.pends_at.contains(&ms) {
            futures::future::pending().await
        } else {
            Ok(())
        }
    }
}

// ---------------------------------------------------------------------------
// Transaction seam: inline usb-passthrough plans over the responder.
// ---------------------------------------------------------------------------

#[test]
fn passthrough_attach_runs_typed_transactions_over_the_responder() {
    // Attach: GetDeviceInfo, then a property readback capture.
    let store = passthrough_store(
        "bestEffort",
        "readDeviceInfo",
        r#"            - sendOp: "0x1001"
              captures: [{ bind: deviceInfoHead, as: u32Le }]
            - getProp: "0xd209"
              captures: [{ bind: focusResult, as: propValue }]"#,
    );
    let responder = UsbResponder::new()
        .reply_transaction(0x1001, &[], UsbTxnReply::ok(Some(device_info_dataset())))
        .reply_transaction(
            0x1015,
            &[0xd209],
            UsbTxnReply::ok(Some(3u16.to_le_bytes().to_vec())),
        );
    // The 60-second step aggregate never fires; the steps carry the run.
    let transport = Arc::new(ResponderTxnTransport::new(responder, &[60_000]));

    let outcome = block_on(run_initiator_action_txn(
        store.clone(),
        action_request(&store, "readDeviceInfo"),
        transport.clone(),
        Arc::new(StepRecorder::default()),
        Arc::new(ActivityRecorder::default()),
    ))
    .expect("the attach plan walks typed transactions");

    assert_eq!(outcome.steps_run, 2);
    for (key, expected) in [("deviceInfoHead", 100_u64), ("focusResult", 3)] {
        let value = outcome
            .scope
            .iter()
            .find(|value| value.key == key)
            .unwrap_or_else(|| panic!("{key} is captured"));
        assert_eq!(value.value.as_u64().expect("numeric capture"), expected);
    }
    assert_eq!(
        transport.log(),
        vec![
            UsbEvent::Transaction {
                opcode: 0x1001,
                params: vec![],
                data_out: None,
                timeout_ms: 10_000,
            },
            UsbEvent::Transaction {
                opcode: 0x1015,
                params: vec![0xd209],
                data_out: None,
                timeout_ms: 10_000,
            },
        ]
    );
}

#[test]
fn best_effort_event_loss_falls_back_to_then_poll_over_the_responder() {
    let store = passthrough_store(
        "bestEffort",
        "autofocusLock",
        r#"            - awaitUntil:
                source: { event: { code: "0xc005", thenPoll: "0xd209" } }
                until: { prop: "0xd209", eq: 1 }
                timeoutMs: 30000
                intervalMs: 100"#,
    );
    let responder = UsbResponder::new()
        // The daemon drops the push; the per-call event-wait deadline fires.
        .inject_event(0xc005, &[], true)
        .reply_transaction(
            0x1015,
            &[0xd209],
            UsbTxnReply::ok(Some(1u16.to_le_bytes().to_vec())),
        );
    // The 30-second aggregate budget stays pending so the fallback poll loop
    // owns the outcome.
    let transport = Arc::new(ResponderTxnTransport::new(responder, &[30_000]));

    let outcome = block_on(run_initiator_action_txn(
        store.clone(),
        action_request(&store, "autofocusLock"),
        transport.clone(),
        Arc::new(StepRecorder::default()),
        Arc::new(ActivityRecorder::default()),
    ))
    .expect("a lost best-effort event reconciles through thenPoll");

    assert_eq!(outcome.steps_run, 1);
    assert_eq!(
        transport.log(),
        vec![
            UsbEvent::EventWait { event_code: 0xc005 },
            UsbEvent::Transaction {
                opcode: 0x1015,
                params: vec![0xd209],
                data_out: None,
                timeout_ms: 10_000,
            },
        ]
    );
}

// ---------------------------------------------------------------------------
// The responder's own script contract, exercised directly.
// ---------------------------------------------------------------------------

#[test]
fn responder_enforces_its_script() {
    // Bulk I/O requires a claim first.
    let mut responder = UsbResponder::new();
    assert_eq!(
        responder.bulk_out(&OPEN_SESSION_CONTAINER),
        Err(UsbError::NotClaimed)
    );

    // A scripted command expectation decodes the transfer and rejects a
    // different container.
    let mut responder = UsbResponder::new().expect_bulk_out_command(0x1002, 0, &[1]);
    responder.claim(6, 1, 1).expect("claim");
    let error = responder
        .bulk_out(&GET_DEVICE_INFO_CONTAINER)
        .expect_err("a different command container is rejected");
    assert!(
        matches!(error, UsbError::UnexpectedBulkOut { .. }),
        "{error:?}"
    );

    // A transfer the codec cannot decode is reported, not panicked on.
    let mut responder = UsbResponder::new().expect_bulk_out_command(0x1002, 0, &[1]);
    responder.claim(6, 1, 1).expect("claim");
    let error = responder
        .bulk_out(&[0xde, 0xad])
        .expect_err("an undecodable transfer is rejected");
    assert!(
        matches!(error, UsbError::UndecodableBulkOut { .. }),
        "{error:?}"
    );

    // A lost interrupt frame is consumed but never delivered.
    let mut responder = UsbResponder::new().inject_interrupt_frame(&[0x0c], true);
    assert_eq!(responder.next_interrupt_event(), None);
    assert_eq!(
        responder.next_interrupt_event(),
        None,
        "the lost frame was consumed, the queue stays empty"
    );

    // A lost typed event is consumed but never delivered; unrelated events
    // stay queued for their own consumers (code-selective).
    let mut responder = UsbResponder::new()
        .inject_event(0x4002, &[9], false)
        .inject_event(0xc005, &[], true);
    assert_eq!(responder.poll_event(0xc005), None);
    assert_eq!(responder.poll_event(0x4002), Some((0x4002, vec![9])));
}
