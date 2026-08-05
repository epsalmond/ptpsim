//! Issue #342 seam: a raw USB establishment plan (§11.29) walked by the Rust
//! executor through the foreign-transport seam (`run_usb_establishment`)
//! against a scripted in-memory USB device. The transport's call log must
//! show the claim, the bulk OUT OpenSession container, and the bulk IN and
//! interrupt captures in plan order, and the returned scope must hold the
//! captured bulk IN payload.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use camera_protocol_ffi::{
    run_usb_establishment, ConfigStore, ConnectionActivityEvent, ConnectionActivityObserver,
    ExecutorStepFailureKind, KeyValue, Step, StepObserver, StepOutcome, StepReport,
    UsbExecutorError, UsbExecutorTransport, UsbTransportError,
};
use futures::executor::block_on;

/// PTP-over-USB OpenSession command container (session id 1, transaction 0).
const OPEN_SESSION_CONTAINER: [u8; 16] = [
    0x10, 0x00, 0x00, 0x00, // length
    0x01, 0x00, // command container
    0x02, 0x10, // OpenSession
    0x00, 0x00, 0x00, 0x00, // transaction id
    0x01, 0x00, 0x00, 0x00, // session id
];

/// Canned bulk IN reply: an OK response container for the OpenSession.
const OPEN_SESSION_RESPONSE: [u8; 12] = [
    0x0c, 0x00, 0x00, 0x00, // length
    0x03, 0x00, // response container
    0x02, 0x10, // OpenSession
    0x00, 0x00, 0x00, 0x00, // transaction id
];

/// Canned interrupt IN frame: an event container.
const INTERRUPT_EVENT: [u8; 12] = [
    0x0c, 0x00, 0x00, 0x00, // length
    0x04, 0x00, // event container
    0x02, 0x40, // StateChanged
    0x00, 0x00, 0x00, 0x00, // transaction id
];

fn hex_lower(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn store() -> Arc<ConfigStore> {
    let index = r#"
manufacturer: TESTCO
families:
  test:
    usb:
      interfaces:
        stillImage: { class: 6, subclass: 1, protocol: 1 }
      establishments:
        usb-claim-open:
          mechanism: usb-claim-open
          activities:
            - id: camera.test.usb-claim-open
              version: 1
              displayRole: connecting
              defaultExpectedDurationMs: 5
              interactionRequired: false
              executorSpan: { sequence: steps, startStep: 0, endStepExclusive: 4 }
          steps:
            - usbClaim: { interface: stillImage }
            - usbBulkOut: { data: { captured: openSessionContainer } }
            - usbBulkIn: { maxLength: 512, encoding: bytes-raw, captureAs: openSessionResponse }
            - usbAwaitInterrupt: { encoding: bytes-raw, captureAs: sessionEvent }
models:
  - id: tu1
    displayName: "Test USB One"
    inherits: [test]
    manifest: tu1.yaml
"#;
    let body = r#"
schema: camera-config/v1
camera:
  manufacturer: TESTCO
  model: TU1
connections:
  usbTether:
    kind: usb
    establishment: usb-claim-open
    session: { ownership: initiatorOwned }
    events: { delivery: bestEffort }
"#;
    ConfigStore::from_manufacturer_index(
        index.into(),
        vec![KeyValue {
            key: "tu1".into(),
            value: body.into(),
        }],
    )
    .expect("USB fixture loads")
}

// ---------------------------------------------------------------------------
// The foreign-transport seam, implemented as a scripted USB device — what a
// platform app does over its USB stack, minus the bus.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
enum UsbCall {
    Claim {
        class: u8,
        subclass: u8,
        protocol: u8,
    },
    BulkOut {
        data: Vec<u8>,
    },
    BulkIn {
        max_length: u32,
    },
    AwaitInterrupt,
    ReleaseAndClose,
}

struct ScriptedUsbTransport {
    calls: Mutex<Vec<UsbCall>>,
    fail_claim: bool,
    bulk_out_results: Mutex<VecDeque<Result<(), UsbTransportError>>>,
    bulk_in_replies: Mutex<VecDeque<Vec<u8>>>,
    interrupt_replies: Mutex<VecDeque<Vec<u8>>>,
}

impl ScriptedUsbTransport {
    fn new() -> Self {
        ScriptedUsbTransport {
            calls: Mutex::new(Vec::new()),
            fail_claim: false,
            bulk_out_results: Mutex::new(VecDeque::new()),
            bulk_in_replies: Mutex::new(VecDeque::from(vec![OPEN_SESSION_RESPONSE.to_vec()])),
            interrupt_replies: Mutex::new(VecDeque::from(vec![INTERRUPT_EVENT.to_vec()])),
        }
    }

    fn calls(&self) -> Vec<UsbCall> {
        self.calls.lock().unwrap().clone()
    }
}

#[async_trait::async_trait]
impl UsbExecutorTransport for ScriptedUsbTransport {
    async fn claim_interface(
        &self,
        class: u8,
        subclass: u8,
        protocol: u8,
    ) -> Result<(), UsbTransportError> {
        self.calls.lock().unwrap().push(UsbCall::Claim {
            class,
            subclass,
            protocol,
        });
        if self.fail_claim {
            return Err(UsbTransportError::ClaimFailed {
                owner: Some("kernel.driver".into()),
            });
        }
        Ok(())
    }

    async fn bulk_out(&self, data: Vec<u8>) -> Result<(), UsbTransportError> {
        self.calls.lock().unwrap().push(UsbCall::BulkOut { data });
        self.bulk_out_results
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or(Ok(()))
    }

    async fn bulk_in(&self, max_length: u32) -> Result<Vec<u8>, UsbTransportError> {
        self.calls
            .lock()
            .unwrap()
            .push(UsbCall::BulkIn { max_length });
        Ok(self
            .bulk_in_replies
            .lock()
            .unwrap()
            .pop_front()
            .expect("scripted bulk-in reply"))
    }

    async fn next_interrupt_event(&self) -> Result<Vec<u8>, UsbTransportError> {
        self.calls.lock().unwrap().push(UsbCall::AwaitInterrupt);
        Ok(self
            .interrupt_replies
            .lock()
            .unwrap()
            .pop_front()
            .expect("scripted interrupt reply"))
    }

    async fn release_and_close(&self) -> Result<(), UsbTransportError> {
        self.calls.lock().unwrap().push(UsbCall::ReleaseAndClose);
        Ok(())
    }

    async fn sleep(&self, _ms: u32) -> Result<(), UsbTransportError> {
        Ok(())
    }
}

#[derive(Default)]
struct Recorder(Mutex<Vec<StepReport>>);
impl StepObserver for Recorder {
    fn on_step(&self, report: StepReport) {
        self.0.lock().unwrap().push(report);
    }
}

#[derive(Default)]
struct ActivityRecorder(Mutex<Vec<ConnectionActivityEvent>>);
impl ConnectionActivityObserver for ActivityRecorder {
    fn on_activity(&self, event: ConnectionActivityEvent) {
        self.0.lock().unwrap().push(event);
    }
}

fn scope_get<'a>(scope: &'a [KeyValue], key: &str) -> Option<&'a str> {
    scope
        .iter()
        .find(|kv| kv.key == key)
        .map(|kv| kv.value.as_str())
}

fn initial_scope() -> Vec<KeyValue> {
    vec![KeyValue {
        key: "openSessionContainer".into(),
        value: hex_lower(&OPEN_SESSION_CONTAINER),
    }]
}

fn initial_encodings() -> Vec<KeyValue> {
    vec![KeyValue {
        key: "openSessionContainer".into(),
        value: "bytes-raw".into(),
    }]
}

#[test]
fn usb_claim_open_plan_runs_end_to_end() {
    let transport = Arc::new(ScriptedUsbTransport::new());
    let recorder = Arc::new(Recorder::default());
    let activities = Arc::new(ActivityRecorder::default());
    let outcome = block_on(run_usb_establishment(
        store(),
        "tu1:usbTether".into(),
        transport.clone(),
        recorder.clone(),
        activities.clone(),
        initial_scope(),
        initial_encodings(),
        vec![],
    ))
    .expect("the USB establishment plan completes");

    assert_eq!(outcome.steps_run, 4);
    assert_eq!(
        scope_get(&outcome.scope, "openSessionResponse"),
        Some(hex_lower(&OPEN_SESSION_RESPONSE).as_str()),
        "the bulk IN payload binds under captureAs"
    );
    assert_eq!(
        scope_get(&outcome.scope, "sessionEvent"),
        Some(hex_lower(&INTERRUPT_EVENT).as_str()),
        "the interrupt frame binds under captureAs"
    );

    assert_eq!(
        transport.calls(),
        vec![
            UsbCall::Claim {
                class: 6,
                subclass: 1,
                protocol: 1,
            },
            UsbCall::BulkOut {
                data: OPEN_SESSION_CONTAINER.to_vec(),
            },
            UsbCall::BulkIn { max_length: 512 },
            UsbCall::AwaitInterrupt,
        ],
        "the transport saw the plan's verbs in order, claim first"
    );

    let step_outcomes: Vec<(String, StepOutcome)> = recorder
        .0
        .lock()
        .unwrap()
        .iter()
        .map(|report| (report.verb.clone(), report.outcome))
        .collect();
    let expected: Vec<(String, StepOutcome)> =
        ["usbClaim", "usbBulkOut", "usbBulkIn", "usbAwaitInterrupt"]
            .into_iter()
            .flat_map(|verb| {
                [
                    (verb.to_string(), StepOutcome::Started),
                    (verb.to_string(), StepOutcome::Succeeded),
                ]
            })
            .collect();
    assert_eq!(step_outcomes, expected);

    let events = activities.0.lock().unwrap();
    assert!(
        matches!(
            events.first(),
            Some(ConnectionActivityEvent::Started { id, .. })
                if id == "camera.test.usb-claim-open"
        ),
        "the executor span opens: {events:?}"
    );
    assert!(
        matches!(
            events.last(),
            Some(ConnectionActivityEvent::Succeeded { id, .. })
                if id == "camera.test.usb-claim-open"
        ),
        "the executor span closes: {events:?}"
    );
}

#[test]
fn claim_failure_leaves_nothing_to_release() {
    let mut transport = ScriptedUsbTransport::new();
    transport.fail_claim = true;
    let transport = Arc::new(transport);
    let error = block_on(run_usb_establishment(
        store(),
        "tu1:usbTether".into(),
        transport.clone(),
        Arc::new(Recorder::default()),
        Arc::new(ActivityRecorder::default()),
        initial_scope(),
        initial_encodings(),
        vec![],
    ))
    .expect_err("a failed claim fails the plan");

    assert!(
        matches!(
            &error,
            UsbExecutorError::StepFailed {
                kind: ExecutorStepFailureKind::Other,
                step,
                ..
            } if step == "steps[0].usbClaim"
        ),
        "the claim failure surfaces as a step failure: {error:?}"
    );
    assert_eq!(
        transport.calls(),
        vec![UsbCall::Claim {
            class: 6,
            subclass: 1,
            protocol: 1,
        }],
        "no interface was claimed, so nothing is released"
    );
}

#[test]
fn bulk_out_failure_releases_the_claimed_interface() {
    let transport = ScriptedUsbTransport::new();
    transport
        .bulk_out_results
        .lock()
        .unwrap()
        .push_back(Err(UsbTransportError::Stall {
            detail: "bulk OUT endpoint stalled".into(),
        }));
    let transport = Arc::new(transport);
    let error = block_on(run_usb_establishment(
        store(),
        "tu1:usbTether".into(),
        transport.clone(),
        Arc::new(Recorder::default()),
        Arc::new(ActivityRecorder::default()),
        initial_scope(),
        initial_encodings(),
        vec![],
    ))
    .expect_err("a stalled bulk OUT fails the plan");

    assert!(
        matches!(
            &error,
            UsbExecutorError::StepFailed {
                kind: ExecutorStepFailureKind::Other,
                step,
                ..
            } if step == "steps[1].usbBulkOut"
        ),
        "the stall surfaces as a step failure: {error:?}"
    );
    assert_eq!(
        transport.calls(),
        vec![
            UsbCall::Claim {
                class: 6,
                subclass: 1,
                protocol: 1,
            },
            UsbCall::BulkOut {
                data: OPEN_SESSION_CONTAINER.to_vec(),
            },
            UsbCall::ReleaseAndClose,
        ],
        "a failed walk releases the claimed interface"
    );
}

#[test]
fn unknown_mechanism_is_an_unknown_plan() {
    let error = block_on(run_usb_establishment(
        store(),
        "tu1:no-such-mechanism".into(),
        Arc::new(ScriptedUsbTransport::new()),
        Arc::new(Recorder::default()),
        Arc::new(ActivityRecorder::default()),
        vec![],
        vec![],
        vec![],
    ))
    .expect_err("a mechanism outside the USB registry does not resolve");

    assert!(
        matches!(&error, UsbExecutorError::UnknownPlan { .. }),
        "{error:?}"
    );
}

#[test]
fn establishment_plan_mirrors_resolved_usb_steps() {
    let plan = store()
        .establishment("tu1".into(), "usbTether".into(), vec![])
        .expect("the USB connection's establishment resolves over the FFI");
    assert_eq!(plan.plan_handle, "tu1:usbTether");
    assert!(
        matches!(
            &plan.steps[0],
            Step::UsbClaim {
                class: 6,
                subclass: 1,
                protocol: 1,
                ..
            }
        ),
        "usbClaim carries the resolved interface triple: {:?}",
        plan.steps[0]
    );
    assert!(
        matches!(&plan.steps[1], Step::UsbBulkOut { .. }),
        "{:?}",
        plan.steps[1]
    );
    assert!(
        matches!(
            &plan.steps[2],
            Step::UsbBulkIn {
                max_length: 512,
                capture_as,
                ..
            } if capture_as == "openSessionResponse"
        ),
        "{:?}",
        plan.steps[2]
    );
    assert!(
        matches!(&plan.steps[3], Step::UsbAwaitInterrupt { .. }),
        "{:?}",
        plan.steps[3]
    );
}
