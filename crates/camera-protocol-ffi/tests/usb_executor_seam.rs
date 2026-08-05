//! Issue #342 seam: a raw USB establishment plan (§11.29) walked by the Rust
//! executor through the foreign-transport seam (`run_usb_establishment`)
//! against the scripted in-memory USB responder (`camera_sim::usb`, shared
//! with the camera-sim acceptance tests). The responder's interaction log
//! must show the claim, the bulk OUT OpenSession container, and the bulk IN
//! and interrupt captures in plan order, and the returned scope must hold the
//! captured bulk IN payload.

use std::sync::{Arc, Mutex};

use camera_protocol_ffi::{
    run_usb_establishment, ConfigStore, ConnectionActivityEvent, ConnectionActivityObserver,
    ExecutorStepFailureKind, KeyValue, Step, StepObserver, StepOutcome, StepReport,
    UsbExecutorError, UsbExecutorTransport, UsbTransportError,
};
use camera_sim::usb::{UsbError, UsbEvent, UsbResponder};
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
    # The raw kind owns the interrupt pipe, so delivery is reliable; the
    # thenPoll rule scopes to the EntryStep awaitUntil grammar (§11.29).
    events: { delivery: reliable }
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
// The foreign-transport seam, backed by the shared responder — what a
// platform app does over its USB stack, minus the bus. The adapter owns the
// async deadline plumbing the deterministic responder does not model (the
// BLE `ResponderTransport` precedent).
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

/// The default scripting for the fixture plan: the OpenSession OK response
/// and one interrupt event frame.
fn scripted_responder() -> UsbResponder {
    UsbResponder::new()
        .queue_bulk_in(&OPEN_SESSION_RESPONSE)
        .inject_interrupt_frame(&INTERRUPT_EVENT, false)
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
    let transport = Arc::new(ResponderUsbTransport::new(scripted_responder()));
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
            UsbEvent::AwaitInterrupt,
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
    let responder = scripted_responder().with_claim_refusal(Some("kernel.driver".into()));
    let transport = Arc::new(ResponderUsbTransport::new(responder));
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
        transport.log(),
        vec![UsbEvent::Claim {
            class: 6,
            subclass: 1,
            protocol: 1,
        }],
        "no interface was claimed, so nothing is released"
    );
}

#[test]
fn bulk_out_failure_releases_the_claimed_interface() {
    let responder = scripted_responder().queue_bulk_out_stall("bulk OUT endpoint stalled");
    let transport = Arc::new(ResponderUsbTransport::new(responder));
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
            UsbEvent::ReleaseAndClose,
        ],
        "a failed walk releases the claimed interface"
    );
}

#[test]
fn unknown_mechanism_is_an_unknown_plan() {
    let error = block_on(run_usb_establishment(
        store(),
        "tu1:no-such-mechanism".into(),
        Arc::new(ResponderUsbTransport::new(scripted_responder())),
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
