//! #342 acceptance: a `daemonAttached` (`usb-passthrough`) connection walks
//! the §11.24 entry/action grammar over typed `PtpTransactionTransport` calls
//! instead of raw frames, and a lost event on a `bestEffort` connection
//! reconciles through the declared `thenPoll` loop (§11.29).

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use camera_protocol_ffi::{
    run_initiator_action_txn, ActionInvocationRequest, ActionRole, ConfigStore,
    ConnectionActivityEvent, ConnectionActivityObserver, ExecutorStepFailureKind, PtpExecutorError,
    PtpTransactionError, PtpTransactionEvent, PtpTransactionResult, PtpTransactionTransport,
    StepObserver, StepReport,
};
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

/// Every foreign call the executor made, in order, with the per-call budgets
/// the daemon is expected to enforce.
#[derive(Debug, Clone, PartialEq, Eq)]
enum TxnCall {
    Execute {
        opcode: u16,
        params: Vec<u32>,
        data_out: Option<Vec<u8>>,
        timeout_ms: u32,
    },
    NextEvent {
        event_code: u16,
    },
}

/// A scripted `PtpTransactionTransport`: canned transaction replies keyed by
/// (opcode, params), an optional delivered event, and a wall clock that pends
/// on exact `ms` values so a test picks which deadline race fires.
struct ScriptedTransactionTransport {
    calls: Mutex<Vec<TxnCall>>,
    replies: Mutex<BTreeMap<(u16, Vec<u32>), PtpTransactionResult>>,
    delivered_event: Option<u16>,
    pends_at: Vec<u32>,
}

impl ScriptedTransactionTransport {
    fn new(delivered_event: Option<u16>, pends_at: &[u32]) -> Self {
        Self {
            calls: Mutex::new(Vec::new()),
            replies: Mutex::new(BTreeMap::new()),
            delivered_event,
            pends_at: pends_at.to_vec(),
        }
    }

    fn reply(&self, opcode: u16, params: &[u32], result: PtpTransactionResult) {
        self.replies
            .lock()
            .expect("replies")
            .insert((opcode, params.to_vec()), result);
    }

    fn calls(&self) -> Vec<TxnCall> {
        self.calls.lock().expect("calls").clone()
    }
}

fn ok(data_in: Option<Vec<u8>>) -> PtpTransactionResult {
    PtpTransactionResult {
        response_code: 0x2001,
        params: Vec::new(),
        data_in,
    }
}

#[async_trait::async_trait]
impl PtpTransactionTransport for ScriptedTransactionTransport {
    async fn execute(
        &self,
        opcode: u16,
        params: Vec<u32>,
        data_out: Option<Vec<u8>>,
        timeout_ms: u32,
    ) -> Result<PtpTransactionResult, PtpTransactionError> {
        self.calls.lock().expect("calls").push(TxnCall::Execute {
            opcode,
            params: params.clone(),
            data_out,
            timeout_ms,
        });
        Ok(self
            .replies
            .lock()
            .expect("replies")
            .get(&(opcode, params))
            .cloned()
            .unwrap_or(PtpTransactionResult {
                response_code: 0x2001,
                params: Vec::new(),
                data_in: None,
            }))
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
        self.calls
            .lock()
            .expect("calls")
            .push(TxnCall::NextEvent { event_code });
        match self.delivered_event {
            Some(code) if code == event_code => Ok(PtpTransactionEvent {
                event_code,
                params: Vec::new(),
            }),
            _ => futures::future::pending().await,
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
    // The 60-second step aggregate never fires; the steps carry the run.
    let transport = Arc::new(ScriptedTransactionTransport::new(None, &[60_000]));
    transport.reply(0x1015, &[0xd209], ok(Some(3u16.to_le_bytes().to_vec())));
    transport.reply(0x9026, &[0x09060403], ok(Some(7u32.to_le_bytes().to_vec())));

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
        transport.calls(),
        vec![
            TxnCall::Execute {
                opcode: 0x1015,
                params: vec![0xd209],
                data_out: None,
                timeout_ms: 10_000,
            },
            TxnCall::Execute {
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
    // The event never arrives. The 10-second per-call event-wait deadline
    // fires; the 30-second aggregate budget stays pending so the fallback
    // poll loop owns the outcome.
    let transport = Arc::new(ScriptedTransactionTransport::new(None, &[30_000]));
    transport.reply(0x1015, &[0xd209], ok(Some(1u16.to_le_bytes().to_vec())));

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
        transport.calls(),
        vec![
            TxnCall::NextEvent { event_code: 0xc005 },
            TxnCall::Execute {
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
    // The event never arrives and the per-call event-wait clock stays
    // pending: the 30-second aggregate budget owns the failure, exactly as
    // on the frame path.
    let transport = Arc::new(ScriptedTransactionTransport::new(None, &[10_000]));

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
        transport.calls(),
        vec![TxnCall::NextEvent { event_code: 0xc005 }]
    );
}
