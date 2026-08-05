//! Issue #342 acceptance: the scripted USB responder
//! (`camera_sim::usb::UsbResponder`) backing both USB seams end to end. The
//! real `families.fuji.usb.establishments.usb-claim-open` plan from
//! `packages/camera-config-data/fuji/index.yaml` walks over the raw seam
//! (`run_usb_establishment`), and inline `usb-passthrough` plans walk the
//! transaction seam (`run_initiator_action_txn`), including a lost
//! `bestEffort` event reconciling through its declared `thenPoll` (§11.29).

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use camera_protocol_ffi::{
    run_initiator_action_txn, run_usb_establishment, ActionInvocationRequest, ActionRole,
    ConfigStore, ConnectionActivityEvent, ConnectionActivityObserver, KeyValue,
    PtpTransactionError, PtpTransactionEvent, PtpTransactionResult, PtpTransactionTransport,
    StepObserver, StepOutcome, StepReport, UsbExecutorTransport, UsbTransportError,
};
use camera_sim::usb::{UsbError, UsbEvent, UsbResponder, UsbTxnReply};
use futures::executor::block_on;
use protocol_primitives::usb_ptp;
use ptp_core::{OperationResponse, PtpIpPacket, Writer};

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

fn hex_lower(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

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

fn data(rel: &str) -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../packages/camera-config-data")
        .join(rel);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

/// The real Fuji index + bodies, as a vendored consumer constructs it. The
/// body list mirrors `common::real_fuji_bodies` in the camera-protocol-ffi
/// tests: the loader requires a body per declared model.
fn real_fuji_store() -> Arc<ConfigStore> {
    let bodies: Vec<KeyValue> = ["gfx100ii", "xa7", "fuji-generic"]
        .into_iter()
        .map(|model| KeyValue {
            key: model.to_string(),
            value: data(&format!("fuji/{model}/{model}.yaml")),
        })
        .collect();
    ConfigStore::from_manufacturer_index_with_defaults(
        data("fuji/index.yaml"),
        data("fuji/fuji.yaml"),
        bodies,
    )
    .expect("manufacturer index loads")
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

fn scope_get<'a>(scope: &'a [KeyValue], key: &str) -> Option<&'a str> {
    scope
        .iter()
        .find(|kv| kv.key == key)
        .map(|kv| kv.value.as_str())
}

// ---------------------------------------------------------------------------
// The foreign-transport seams, backed by the shared responder: what a
// platform app does over its USB stack or daemon session, minus the bus. The
// adapters own the async deadline plumbing the deterministic responder does
// not model (the BLE `ResponderTransport` precedent).
// ---------------------------------------------------------------------------

struct ResponderUsbTransport {
    responder: Mutex<UsbResponder>,
}

impl ResponderUsbTransport {
    fn new(responder: UsbResponder) -> Self {
        ResponderUsbTransport {
            responder: Mutex::new(responder),
        }
    }

    fn log(&self) -> Vec<UsbEvent> {
        self.responder.lock().expect("responder").log().to_vec()
    }
}

fn usb_err(error: UsbError) -> UsbTransportError {
    match error {
        UsbError::ClaimRefused { owner } => UsbTransportError::ClaimFailed { owner },
        UsbError::Stall { detail } => UsbTransportError::Stall { detail },
        other => UsbTransportError::Failed {
            detail: other.to_string(),
        },
    }
}

#[async_trait::async_trait]
impl UsbExecutorTransport for ResponderUsbTransport {
    async fn claim_interface(
        &self,
        class: u8,
        subclass: u8,
        protocol: u8,
    ) -> Result<(), UsbTransportError> {
        self.responder
            .lock()
            .expect("responder")
            .claim(class, subclass, protocol)
            .map_err(usb_err)
    }

    async fn bulk_out(&self, data: Vec<u8>) -> Result<(), UsbTransportError> {
        self.responder
            .lock()
            .expect("responder")
            .bulk_out(&data)
            .map_err(usb_err)
    }

    async fn bulk_in(&self, max_length: u32) -> Result<Vec<u8>, UsbTransportError> {
        self.responder
            .lock()
            .expect("responder")
            .bulk_in(max_length)
            .map_err(usb_err)
    }

    async fn next_interrupt_event(&self) -> Result<Vec<u8>, UsbTransportError> {
        let frame = self
            .responder
            .lock()
            .expect("responder")
            .next_interrupt_event();
        match frame {
            Some(frame) => Ok(frame),
            // A lost or unscripted frame never arrives; the executor's
            // deadline owns the outcome.
            None => futures::future::pending().await,
        }
    }

    async fn release_and_close(&self) -> Result<(), UsbTransportError> {
        self.responder
            .lock()
            .expect("responder")
            .release_and_close();
        Ok(())
    }

    async fn sleep(&self, _ms: u32) -> Result<(), UsbTransportError> {
        Ok(())
    }
}

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

    async fn close(&self) -> Result<(), PtpTransactionError> {
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
// Raw seam: the real family plan over the responder.
// ---------------------------------------------------------------------------

#[test]
fn real_usb_claim_open_plan_runs_end_to_end_over_the_responder() {
    // Canned camera replies, encoded with the same codec the responder uses
    // to check the plan's request containers.
    let open_session_response =
        usb_ptp::encode(&PtpIpPacket::OperationResponse(OperationResponse {
            code: 0x2001, // OK
            transaction_id: 0,
            params: vec![],
        }))
        .expect("response container encodes");
    let device_info_container = usb_ptp::encode_data(0x1001, 1, &device_info_dataset());

    let responder = UsbResponder::new()
        // OpenSession(1), tid 0.
        .expect_bulk_out_command(0x1002, 0, &[1])
        .queue_bulk_in(&open_session_response)
        // GetDeviceInfo, tid 1.
        .expect_bulk_out_command(0x1001, 1, &[])
        .queue_bulk_in(&device_info_container);
    let transport = Arc::new(ResponderUsbTransport::new(responder));
    let steps = Arc::new(StepRecorder::default());
    let activities = Arc::new(ActivityRecorder::default());

    let outcome = block_on(run_usb_establishment(
        real_fuji_store(),
        "gfx100ii:usb".into(),
        transport.clone(),
        steps.clone(),
        activities.clone(),
        vec![],
        vec![],
        vec![],
    ))
    .expect("the real usb-claim-open plan completes against the responder");

    assert_eq!(outcome.steps_run, 5);
    assert_eq!(
        scope_get(&outcome.scope, "openSessionResponse"),
        Some(hex_lower(&open_session_response).as_str()),
        "the OpenSession response container binds under captureAs"
    );
    assert_eq!(
        scope_get(&outcome.scope, "deviceInfo"),
        Some(hex_lower(&device_info_container).as_str()),
        "the GetDeviceInfo data container binds under captureAs"
    );
    assert_eq!(
        transport.log(),
        vec![
            UsbEvent::Claim {
                class: 6,
                subclass: 1,
                protocol: 1,
            },
            UsbEvent::BulkOut {
                data: OPEN_SESSION_CONTAINER.to_vec(),
            },
            UsbEvent::BulkIn { max_length: 512 },
            UsbEvent::BulkOut {
                data: GET_DEVICE_INFO_CONTAINER.to_vec(),
            },
            UsbEvent::BulkIn { max_length: 65536 },
        ],
        "the responder saw the plan's verbs in order, claim first"
    );

    let step_outcomes: Vec<(String, StepOutcome)> = steps
        .0
        .lock()
        .expect("steps")
        .iter()
        .map(|report| (report.verb.clone(), report.outcome))
        .collect();
    let expected: Vec<(String, StepOutcome)> = [
        "usbClaim",
        "usbBulkOut",
        "usbBulkIn",
        "usbBulkOut",
        "usbBulkIn",
    ]
    .into_iter()
    .flat_map(|verb| {
        [
            (verb.to_string(), StepOutcome::Started),
            (verb.to_string(), StepOutcome::Succeeded),
        ]
    })
    .collect();
    assert_eq!(step_outcomes, expected);

    let events = activities.0.lock().expect("activities");
    assert!(
        matches!(
            events.first(),
            Some(ConnectionActivityEvent::Started { id, .. })
                if id == "camera.session.open.usb"
        ),
        "the executor span opens: {events:?}"
    );
    assert!(
        matches!(
            events.last(),
            Some(ConnectionActivityEvent::Succeeded { id, .. })
                if id == "camera.session.open.usb"
        ),
        "the executor span closes: {events:?}"
    );
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
