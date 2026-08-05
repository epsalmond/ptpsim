//! #342 acceptance: a `daemonAttached` (`usb-passthrough`) connection walks
//! the §11.24 entry/action grammar over typed `PtpTransactionTransport` calls
//! instead of raw frames, and a lost event on a `bestEffort` connection
//! reconciles through the declared `thenPoll` loop (§11.29). The seam is
//! backed by the scripted USB responder's transaction side
//! (`camera_sim::usb`, shared with the camera-sim acceptance tests).

use std::sync::{Arc, Mutex};

use camera_protocol_ffi::{
    run_initiator_action_txn, ActionInvocationRequest, ActionRole, ConfigStore,
    ConnectionActivityEvent, ConnectionActivityObserver, ExecutorStepFailureKind, PtpExecutorError,
    PtpTransactionError, PtpTransactionEvent, PtpTransactionResult, PtpTransactionTransport,
    StepObserver, StepReport,
};
use camera_sim::usb::{UsbEvent, UsbResponder, UsbTxnReply};
use futures::executor::block_on;

fn store(delivery: &str, steps: &str) -> Arc<ConfigStore> {
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
      autofocusLock:
        mode: ""
        initiator:
          steps:
{steps}
"#
        ),
        None,
    )
    .expect("transaction store loads")
}

fn action_request(store: &ConfigStore) -> ActionInvocationRequest {
    ActionInvocationRequest {
        catalog_revision: store.action_catalog().revision,
        action_id: "autofocusLock".into(),
        connection: "usbTether".into(),
        mode: String::new(),
        role: ActionRole::Initiator,
        parameters: Vec::new(),
    }
}

struct NullObserver;

impl StepObserver for NullObserver {
    fn on_step(&self, _report: StepReport) {}
}

struct NullActivities;

impl ConnectionActivityObserver for NullActivities {
    fn on_activity(&self, _event: ConnectionActivityEvent) {}
}

// ---------------------------------------------------------------------------
// The foreign-transport seam, backed by the shared responder. The adapter
// owns the async deadline plumbing the deterministic responder does not
// model: an event wait pends when the responder delivers nothing, and the
// wall clock pends on exact `ms` values so a test picks which deadline race
// fires.
// ---------------------------------------------------------------------------

struct ResponderTxnTransport {
    responder: Mutex<UsbResponder>,
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

#[test]
fn daemon_session_runs_typed_transactions_end_to_end() {
    let store = store(
        "reliable",
        r#"            - getProp: "0xd209"
              captures: [{ bind: focusResult, as: propValue }]
            - sendOp: "0x9026"
              params: [0x09060403]
              captures: [{ bind: afAck, as: u32Le }]"#,
    );
    let responder = UsbResponder::new()
        .reply_transaction(
            0x1015,
            &[0xd209],
            UsbTxnReply::ok(Some(3u16.to_le_bytes().to_vec())),
        )
        .reply_transaction(
            0x9026,
            &[0x09060403],
            UsbTxnReply::ok(Some(7u32.to_le_bytes().to_vec())),
        );
    // The 60-second step aggregate never fires; the steps carry the run.
    let transport = Arc::new(ResponderTxnTransport::new(responder, &[60_000]));

    let outcome = block_on(run_initiator_action_txn(
        store.clone(),
        action_request(&store),
        transport.clone(),
        Arc::new(NullObserver),
        Arc::new(NullActivities),
    ))
    .expect("typed transactions walk the initiator binding");

    assert_eq!(outcome.steps_run, 2);
    for (key, expected) in [("focusResult", 3_u64), ("afAck", 7)] {
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
                opcode: 0x1015,
                params: vec![0xd209],
                data_out: None,
                timeout_ms: 10_000,
            },
            UsbEvent::Transaction {
                opcode: 0x9026,
                params: vec![0x09060403],
                data_out: None,
                timeout_ms: 10_000,
            },
        ]
    );
}

#[test]
fn best_effort_event_miss_falls_back_to_the_then_poll_loop() {
    let store = store(
        "bestEffort",
        r#"            - awaitUntil:
                source: { event: { code: "0xc005", thenPoll: "0xd209" } }
                until: { prop: "0xd209", eq: 1 }
                timeoutMs: 30000
                intervalMs: 100"#,
    );
    // The daemon drops the push. The 10-second per-call event-wait deadline
    // fires; the 30-second aggregate budget stays pending so the fallback
    // poll loop owns the outcome.
    let responder = UsbResponder::new()
        .inject_event(0xc005, &[], true)
        .reply_transaction(
            0x1015,
            &[0xd209],
            UsbTxnReply::ok(Some(1u16.to_le_bytes().to_vec())),
        );
    let transport = Arc::new(ResponderTxnTransport::new(responder, &[30_000]));

    let outcome = block_on(run_initiator_action_txn(
        store.clone(),
        action_request(&store),
        transport.clone(),
        Arc::new(NullObserver),
        Arc::new(NullActivities),
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

#[test]
fn reliable_event_miss_still_fails_deadline_exceeded() {
    let store = store(
        "reliable",
        r#"            - awaitUntil:
                source: { event: { code: "0xc005", thenPoll: "0xd209" } }
                until: { prop: "0xd209", eq: 1 }
                timeoutMs: 30000
                intervalMs: 100"#,
    );
    // The daemon drops the push and the per-call event-wait clock stays
    // pending: the 30-second aggregate budget owns the failure, exactly as
    // on the frame path.
    let responder = UsbResponder::new().inject_event(0xc005, &[], true);
    let transport = Arc::new(ResponderTxnTransport::new(responder, &[10_000]));

    let error = block_on(run_initiator_action_txn(
        store.clone(),
        action_request(&store),
        transport.clone(),
        Arc::new(NullObserver),
        Arc::new(NullActivities),
    ))
    .expect_err("a reliable event wait keeps its deadline");
    assert!(matches!(
        error,
        PtpExecutorError::StepFailed {
            kind: ExecutorStepFailureKind::DeadlineExceeded,
            ..
        }
    ));
    assert_eq!(
        transport.log(),
        vec![UsbEvent::EventWait { event_code: 0xc005 }]
    );
}
