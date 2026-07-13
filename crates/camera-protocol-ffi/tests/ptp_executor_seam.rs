//! #250 acceptance: real manifest EntrySteps cross the foreign async seam as
//! encoded PTP frames while Rust owns ordering, tolerance and deadlines.

use std::collections::VecDeque;
use std::future::Future;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

use camera_config::CameraManifest;
use camera_media_store::{MediaStore, ObjectQuery};
use camera_protocol_ffi::{
    run_action, run_mode_entry, run_mode_reestablishment_exit, run_selected_object_preparation,
    ActionVerb, ConfigStore, ConnectionActivityEvent, ConnectionActivityObserver, KeyValue,
    PtpExecutorError, PtpExecutorTransport, PtpFraming, PtpRuntimeValue, PtpSessionOpenResult,
    PtpTransportError, StepObserver, StepOutcome, StepReport,
};
use camera_sim::{Engine, Fault, Reply};
use futures::executor::block_on;
use ptp_core::{OperationRequest, PtpCodec, PtpIpPacket};

fn data(rel: &str) -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../packages/camera-config-data")
        .join(rel);
    std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("read {}: {error}", path.display()))
}

fn store() -> Arc<ConfigStore> {
    store_from_body(data("fuji/gfx100ii/gfx100ii.yaml"))
}

fn store_with_standard_app_framing() -> Arc<ConfigStore> {
    let body = data("fuji/gfx100ii/gfx100ii.yaml").replacen(
        "commandFraming: compressed",
        "commandFraming: standard",
        1,
    );
    store_from_body(body)
}

fn store_with_cold_entry_activities() -> Arc<ConfigStore> {
    let body = data("fuji/gfx100ii/gfx100ii.yaml").replacen(
        "      - to: shooting/stills          # enter live-view from a cold App connection\n        steps:",
        r#"      - to: shooting/stills          # enter live-view from a cold App connection
        activities:
          - id: camera.test.bootstrap
            version: 1
            displayRole: preparingConnection
            defaultExpectedDurationMs: 10
            interactionRequired: false
            executorSpan: { sequence: steps, startStep: 0, endStepExclusive: 2 }
          - id: camera.test.stream
            version: 1
            displayRole: openingSession
            defaultExpectedDurationMs: 10
            interactionRequired: false
            executorSpan: { sequence: steps, startStep: 2, endStepExclusive: 5 }
        steps:"#,
        1,
    );
    store_from_body(body)
}

fn store_with_tolerant_repeated_startup() -> Arc<ConfigStore> {
    let body = data("fuji/gfx100ii/gfx100ii.yaml").replacen(
        r#"- { sendOp: "0x902b", repeat: 4 }"#,
        r#"- { sendOp: "0x902b", repeat: 4, tolerant: true }"#,
        1,
    );
    store_from_body(body)
}

fn store_with_tolerant_reopen() -> Arc<ConfigStore> {
    let body = data("fuji/gfx100ii/gfx100ii.yaml").replacen(
        "- { reopenSession: {} }",
        "- { reopenSession: {}, tolerant: true }",
        1,
    );
    store_from_body(body)
}

fn store_with_tolerant_prime_retry() -> Arc<ConfigStore> {
    let body = data("fuji/gfx100ii/gfx100ii.yaml").replacen(
        r#"          - { sendOp: "0x9022" }"#,
        r#"          - tolerant: true
            retry:
              whenResponseCodes: ["0x2019"]
              maxAttempts: 3
              retryDelayMs: 0
              steps:
                - { sendOp: "0x9022" }"#,
        1,
    );
    store_from_body(body)
}

fn store_with_uncaptured_scalar_event_predicate() -> Arc<ConfigStore> {
    let body = data("fuji/gfx100ii/gfx100ii.yaml")
        .replacen(
            r#"          - { sendOp: "0x100e", params: [0, 0] }"#,
            r#"          - { getProp: "0xdf01" }
          - { sendOp: "0x100e", params: [0, 0] }"#,
            1,
        )
        .replacen(
            "              until: { all: [] }",
            r#"              until: { prop: "0xdf01", eq: 0x16 }"#,
            1,
        );
    store_from_body(body)
}

fn store_with_composite_poll_action() -> Arc<ConfigStore> {
    let body = data("fuji/gfx100ii/gfx100ii.yaml").replacen(
        r#"          - { sendOp: "0x100e", params: [0, 0] }"#,
        r#"          - awaitUntil:
              source: { poll: "0xd212" }
              until: { prop: "0xd209", eq: 0 }
              timeoutMs: 1000
          - { sendOp: "0x100e", params: [0, 0] }"#,
        1,
    );
    store_from_body(body)
}

fn store_from_body(body: String) -> Arc<ConfigStore> {
    ConfigStore::from_manufacturer_index(
        data("fuji/index.yaml"),
        vec![KeyValue {
            key: "gfx100ii".into(),
            value: body,
        }],
    )
    .expect("GFX store loads")
}

fn engine(connection: &str) -> (Engine, u32) {
    let manifest = CameraManifest::from_yaml(&data("fuji/gfx100ii/gfx100ii.consolidated.yaml"))
        .expect("consolidated manifest loads");
    let root = std::env::temp_dir().join(format!(
        "ptpsim-ffi-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos()
    ));
    let dcim = root.join("DCIM/100_FUJI");
    std::fs::create_dir_all(&dcim).expect("temp media root");
    std::fs::write(dcim.join("DSCF0001.JPG"), b"\xff\xd8ffi-test\xff\xd9").expect("test jpeg");
    let mut media = MediaStore::open(root).expect("media store");
    media.scan().expect("scan media");
    let handle = media
        .handles(ObjectQuery {
            format: Some(ptp_core::codes::format::EXIF_JPEG),
            ..Default::default()
        })
        .into_iter()
        .next()
        .expect("jpeg handle");
    let mut engine = Engine::new(manifest, media);
    engine.bind_connection(connection);
    let reply = engine.on_operation(
        &OperationRequest {
            data_phase_info: 1,
            code: ptp_core::codes::op::OPEN_SESSION,
            transaction_id: 1,
            params: vec![1],
        },
        None,
    );
    assert!(matches!(reply, Reply::Response(response) if response.code == 0x2001));
    (engine, handle)
}

#[derive(Default)]
struct Reports(Mutex<Vec<StepReport>>);

impl StepObserver for Reports {
    fn on_step(&self, report: StepReport) {
        self.0.lock().expect("reports").push(report);
    }
}

#[derive(Default)]
struct Activities(Mutex<Vec<ConnectionActivityEvent>>);

impl ConnectionActivityObserver for Activities {
    fn on_activity(&self, event: ConnectionActivityEvent) {
        self.0.lock().expect("activities").push(event);
    }
}

struct EngineState {
    engine: Engine,
    pending_data_out: Option<PendingDataOut>,
    replies: VecDeque<Vec<u8>>,
    operations: Vec<u16>,
    requests: Vec<(u16, Vec<u32>)>,
    next_tid: u32,
}

struct PendingDataOut {
    request: OperationRequest,
    declared_length: Option<u64>,
    payload: Vec<u8>,
}

type DataOverride = (u16, Vec<u32>, Vec<u8>);

struct EngineTransport {
    framing: PtpFraming,
    event_framing: PtpFraming,
    state: Mutex<EngineState>,
    first_handle: u32,
    suppress_events: AtomicBool,
    fire_deadlines: AtomicBool,
    truncate_standard_data: AtomicBool,
    data_override: Mutex<Option<DataOverride>>,
    close_calls: AtomicUsize,
    reopen_calls: AtomicUsize,
}

impl EngineTransport {
    fn new(connection: &str, framing: PtpFraming, event_framing: PtpFraming) -> Self {
        let (engine, first_handle) = engine(connection);
        Self {
            framing,
            event_framing,
            state: Mutex::new(EngineState {
                engine,
                pending_data_out: None,
                replies: VecDeque::new(),
                operations: Vec::new(),
                requests: Vec::new(),
                next_tid: 2,
            }),
            first_handle,
            suppress_events: AtomicBool::new(false),
            fire_deadlines: AtomicBool::new(false),
            truncate_standard_data: AtomicBool::new(false),
            data_override: Mutex::new(None),
            close_calls: AtomicUsize::new(0),
            reopen_calls: AtomicUsize::new(0),
        }
    }

    fn operations(&self) -> Vec<u16> {
        self.state.lock().expect("state").operations.clone()
    }

    fn request_count(&self, operation: u16, params: &[u32]) -> usize {
        self.state
            .lock()
            .expect("state")
            .requests
            .iter()
            .filter(|(candidate, candidate_params)| {
                *candidate == operation && candidate_params == params
            })
            .count()
    }

    fn first_handle(&self) -> u32 {
        self.first_handle
    }

    fn install_fault(&self, fault: Fault) {
        self.state
            .lock()
            .expect("state")
            .engine
            .install_fault(fault);
    }

    fn force_event_deadline(&self) {
        self.suppress_events.store(true, Ordering::SeqCst);
        self.fire_deadlines.store(true, Ordering::SeqCst);
    }

    fn close_calls(&self) -> usize {
        self.close_calls.load(Ordering::SeqCst)
    }

    fn reopen_calls(&self) -> usize {
        self.reopen_calls.load(Ordering::SeqCst)
    }

    fn truncate_next_standard_data(&self) {
        self.truncate_standard_data.store(true, Ordering::SeqCst);
    }

    fn take_queued_event(&self, code: u16) -> bool {
        self.state.lock().expect("state").engine.take_event(code)
    }

    fn override_next_data(&self, operation: u16, params: Vec<u32>, payload: Vec<u8>) {
        *self.data_override.lock().expect("data override") = Some((operation, params, payload));
    }

    fn decode(&self, frame: &[u8]) -> Result<PtpIpPacket, PtpTransportError> {
        match self.framing {
            PtpFraming::Standard => PtpIpPacket::decode(frame),
            PtpFraming::Compressed => protocol_primitives::fuji_framing::decode(frame),
            PtpFraming::Usb => protocol_primitives::usb_ptp::decode(frame),
        }
        .map_err(|error| PtpTransportError::Failed {
            detail: error.to_string(),
        })
    }

    fn encode(&self, packet: &PtpIpPacket) -> Result<Vec<u8>, PtpTransportError> {
        match self.framing {
            PtpFraming::Standard => ptp_core::encode(packet).map_err(|error| error.to_string()),
            PtpFraming::Compressed => {
                protocol_primitives::fuji_framing::encode(packet).map_err(|error| error.to_string())
            }
            PtpFraming::Usb => {
                protocol_primitives::usb_ptp::encode(packet).map_err(|error| error.to_string())
            }
        }
        .map_err(|detail| PtpTransportError::Failed { detail })
    }

    fn queue_reply(
        &self,
        state: &mut EngineState,
        request: &OperationRequest,
        reply: Reply,
    ) -> Result<(), PtpTransportError> {
        let mut queue_data = |mut payload: Vec<u8>| -> Result<(), PtpTransportError> {
            let mut data_override = self.data_override.lock().expect("data override");
            if data_override
                .as_ref()
                .is_some_and(|(operation, params, _)| {
                    *operation == request.code && *params == request.params
                })
            {
                payload = data_override.take().expect("matched above").2;
            }
            drop(data_override);
            match self.framing {
                PtpFraming::Compressed => {
                    state
                        .replies
                        .push_back(protocol_primitives::fuji_framing::encode_data(
                            request.code,
                            request.transaction_id,
                            &payload,
                        ))
                }
                PtpFraming::Standard => {
                    state.replies.push_back(self.encode(&PtpIpPacket::StartData(
                        ptp_core::StartData {
                            transaction_id: request.transaction_id,
                            total_length: payload.len() as u64,
                        },
                    ))?);
                    if !self.truncate_standard_data.swap(false, Ordering::SeqCst) {
                        state.replies.push_back(self.encode(&PtpIpPacket::EndData(
                            ptp_core::DataBlock {
                                transaction_id: request.transaction_id,
                                payload,
                            },
                        ))?);
                    }
                }
                PtpFraming::Usb => state.replies.push_back(self.encode(&PtpIpPacket::Data(
                    ptp_core::DataBlock {
                        transaction_id: request.transaction_id,
                        payload,
                    },
                ))?),
            }
            Ok(())
        };
        match reply {
            Reply::Response(response) => state
                .replies
                .push_back(self.encode(&PtpIpPacket::OperationResponse(response))?),
            Reply::Data { data, response } => {
                queue_data(data)?;
                state
                    .replies
                    .push_back(self.encode(&PtpIpPacket::OperationResponse(response))?);
            }
            Reply::DataStream {
                source,
                response,
                completion,
            } => {
                let data = source
                    .read_chunk(0, source.len() as usize)
                    .map_err(|error| PtpTransportError::Failed {
                        detail: error.to_string(),
                    })?;
                queue_data(data)?;
                state
                    .replies
                    .push_back(self.encode(&PtpIpPacket::OperationResponse(response))?);
                if let Some(completion) = completion {
                    state.engine.complete_stream(completion);
                }
            }
            Reply::NoResponse => {}
            Reply::Close => {
                return Err(PtpTransportError::NotConnected);
            }
        }
        Ok(())
    }
}

#[async_trait::async_trait]
impl PtpExecutorTransport for EngineTransport {
    async fn reserve_transaction_id(&self) -> Result<u32, PtpTransportError> {
        let mut state = self.state.lock().expect("state");
        let tid = state.next_tid;
        state.next_tid += 1;
        Ok(tid)
    }

    async fn send_command_frame(&self, frame: Vec<u8>) -> Result<(), PtpTransportError> {
        let packet = self.decode(&frame)?;
        let mut state = self.state.lock().expect("state");
        match packet {
            PtpIpPacket::OperationRequest(request) => {
                state.operations.push(request.code);
                state.requests.push((request.code, request.params.clone()));
                if request.data_phase_info == 2
                    || request.code == ptp_core::codes::op::SET_DEVICE_PROP_VALUE
                {
                    state.pending_data_out = Some(PendingDataOut {
                        request,
                        declared_length: None,
                        payload: Vec::new(),
                    });
                } else {
                    let reply = state.engine.on_operation(&request, None);
                    self.queue_reply(&mut state, &request, reply)?;
                }
            }
            PtpIpPacket::StartData(start) => {
                let pending =
                    state
                        .pending_data_out
                        .as_mut()
                        .ok_or_else(|| PtpTransportError::Failed {
                            detail: "data start without request".into(),
                        })?;
                if pending.request.transaction_id != start.transaction_id
                    || pending.declared_length.is_some()
                {
                    return Err(PtpTransportError::Failed {
                        detail: "mismatched or duplicate data start".into(),
                    });
                }
                pending.declared_length = Some(start.total_length);
            }
            PtpIpPacket::Data(data) => {
                let pending =
                    state
                        .pending_data_out
                        .as_mut()
                        .ok_or_else(|| PtpTransportError::Failed {
                            detail: "data phase without request".into(),
                        })?;
                if pending.request.transaction_id != data.transaction_id {
                    return Err(PtpTransportError::Failed {
                        detail: "mismatched data transaction".into(),
                    });
                }
                pending.payload.extend_from_slice(&data.payload);
                if !matches!(self.framing, PtpFraming::Standard) {
                    let pending = state.pending_data_out.take().expect("checked above");
                    let reply = state
                        .engine
                        .on_operation(&pending.request, Some(&pending.payload));
                    self.queue_reply(&mut state, &pending.request, reply)?;
                }
            }
            PtpIpPacket::EndData(data) => {
                let mut pending =
                    state
                        .pending_data_out
                        .take()
                        .ok_or_else(|| PtpTransportError::Failed {
                            detail: "data end without request".into(),
                        })?;
                if pending.request.transaction_id != data.transaction_id {
                    return Err(PtpTransportError::Failed {
                        detail: "mismatched data transaction".into(),
                    });
                }
                pending.payload.extend_from_slice(&data.payload);
                if pending.declared_length != Some(pending.payload.len() as u64) {
                    return Err(PtpTransportError::Failed {
                        detail: "standard data-out length mismatch".into(),
                    });
                }
                let reply = state
                    .engine
                    .on_operation(&pending.request, Some(&pending.payload));
                self.queue_reply(&mut state, &pending.request, reply)?;
            }
            other => {
                return Err(PtpTransportError::Failed {
                    detail: format!("unexpected client packet {other:?}"),
                });
            }
        }
        Ok(())
    }

    async fn next_command_frame(&self) -> Result<Vec<u8>, PtpTransportError> {
        self.state
            .lock()
            .expect("state")
            .replies
            .pop_front()
            .ok_or_else(|| PtpTransportError::Failed {
                detail: "response queue empty".into(),
            })
    }

    async fn next_event_frame(&self, event_code: u16) -> Result<Vec<u8>, PtpTransportError> {
        if self.suppress_events.load(Ordering::SeqCst) {
            return futures::future::pending().await;
        }
        let mut state = self.state.lock().expect("state");
        if !state.engine.take_event(event_code) {
            return Err(PtpTransportError::Failed {
                detail: format!("event {event_code:#06x} is not queued"),
            });
        }
        let event = PtpIpPacket::Event(ptp_core::EventPacket {
            code: event_code,
            transaction_id: 0,
            params: Vec::new(),
        });
        match self.event_framing {
            PtpFraming::Standard => ptp_core::encode(&event).map_err(|error| error.to_string()),
            PtpFraming::Compressed => {
                protocol_primitives::fuji_framing::encode(&event).map_err(|error| error.to_string())
            }
            PtpFraming::Usb => {
                protocol_primitives::usb_ptp::encode(&event).map_err(|error| error.to_string())
            }
        }
        .map_err(|detail| PtpTransportError::Failed { detail })
    }

    async fn close_command_channel(
        &self,
        _transport_close_frame: Option<Vec<u8>>,
    ) -> Result<(), PtpTransportError> {
        self.close_calls.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    async fn reopen_command_session(&self) -> Result<PtpSessionOpenResult, PtpTransportError> {
        self.reopen_calls.fetch_add(1, Ordering::SeqCst);
        let mut state = self.state.lock().expect("state");
        state.next_tid = 2;
        let request = OperationRequest {
            data_phase_info: 1,
            code: ptp_core::codes::op::OPEN_SESSION,
            transaction_id: 1,
            params: vec![1],
        };
        match state.engine.on_operation(&request, None) {
            Reply::Response(response) => Ok(PtpSessionOpenResult {
                transaction_id: response.transaction_id,
                response_code: response.code,
                response_params: response.params,
            }),
            other => Err(PtpTransportError::Failed {
                detail: format!("unexpected reopen reply {other:?}"),
            }),
        }
    }

    async fn sleep(&self, ms: u32) -> Result<(), PtpTransportError> {
        if ms == 5_000 && self.fire_deadlines.load(Ordering::SeqCst) {
            Ok(())
        } else if ms >= 5_000 {
            futures::future::pending().await
        } else {
            Ok(())
        }
    }
}

#[test]
fn real_gfx_cold_entry_runs_in_manifest_wire_order() {
    let transport = Arc::new(EngineTransport::new(
        "app",
        PtpFraming::Compressed,
        PtpFraming::Usb,
    ));
    let reports = Arc::new(Reports::default());
    let activities = Arc::new(Activities::default());

    let outcome = block_on(run_mode_entry(
        store(),
        "app".into(),
        None,
        "shooting/stills".into(),
        transport.clone(),
        reports.clone(),
        activities.clone(),
        Vec::<PtpRuntimeValue>::new(),
    ))
    .expect("cold entry succeeds");

    assert_eq!(
        transport.operations(),
        vec![0x1016, 0x1016, 0x1015, 0x1016, 0x902b, 0x902b, 0x902b, 0x902b, 0x101c]
    );
    assert_eq!(outcome.steps_run, 5);
    assert_eq!(reports.0.lock().expect("reports").len(), 10);
    assert!(activities.0.lock().expect("activities").is_empty());
}

#[test]
fn real_gfx_autofocus_runs_event_then_poll_in_rust() {
    let transport = Arc::new(EngineTransport::new(
        "app",
        PtpFraming::Compressed,
        PtpFraming::Usb,
    ));
    block_on(run_mode_entry(
        store(),
        "app".into(),
        None,
        "shooting/stills".into(),
        transport.clone(),
        Arc::new(Reports::default()),
        Arc::new(Activities::default()),
        Vec::new(),
    ))
    .expect("cold entry succeeds");
    let before = transport.operations().len();

    let outcome = block_on(run_action(
        store(),
        "app".into(),
        ActionVerb::AutofocusLock,
        transport.clone(),
        Arc::new(Reports::default()),
        Arc::new(Activities::default()),
        vec![PtpRuntimeValue {
            key: "afArea".into(),
            value: 0x0906_0403,
        }],
    ))
    .expect("autofocus action succeeds");

    let operations = transport.operations();
    assert_eq!(operations[before], 0x9026);
    assert!(operations[before + 1..]
        .iter()
        .all(|operation| *operation == 0x1015));
    assert!(
        operations.len() >= before + 3,
        "AF polls until the value settles"
    );
    assert!(outcome.steps_run >= 2);
}

#[test]
fn uncaptured_scalar_get_seeds_a_later_event_predicate() {
    let store = store_with_uncaptured_scalar_event_predicate();
    let transport = Arc::new(EngineTransport::new(
        "app",
        PtpFraming::Compressed,
        PtpFraming::Usb,
    ));
    block_on(run_mode_entry(
        store.clone(),
        "app".into(),
        None,
        "shooting/stills".into(),
        transport.clone(),
        Arc::new(Reports::default()),
        Arc::new(Activities::default()),
        Vec::new(),
    ))
    .expect("cold entry succeeds");

    block_on(run_action(
        store,
        "app".into(),
        ActionVerb::Shutter,
        transport,
        Arc::new(Reports::default()),
        Arc::new(Activities::default()),
        Vec::new(),
    ))
    .expect("event predicate sees the prior uncaptured property read");
}

#[test]
fn composite_poll_populates_manifest_declared_member_scope() {
    let store = store_with_composite_poll_action();
    let transport = Arc::new(EngineTransport::new(
        "app",
        PtpFraming::Compressed,
        PtpFraming::Usb,
    ));
    block_on(run_mode_entry(
        store.clone(),
        "app".into(),
        None,
        "shooting/stills".into(),
        transport.clone(),
        Arc::new(Reports::default()),
        Arc::new(Activities::default()),
        Vec::new(),
    ))
    .expect("cold entry succeeds");

    block_on(run_action(
        store,
        "app".into(),
        ActionVerb::Shutter,
        transport,
        Arc::new(Reports::default()),
        Arc::new(Activities::default()),
        Vec::new(),
    ))
    .expect("record-stream polling stays inside the Rust executor");
}

#[test]
fn await_deadline_covers_the_pending_event_pull() {
    let transport = Arc::new(EngineTransport::new(
        "app",
        PtpFraming::Compressed,
        PtpFraming::Usb,
    ));
    block_on(run_mode_entry(
        store(),
        "app".into(),
        None,
        "shooting/stills".into(),
        transport.clone(),
        Arc::new(Reports::default()),
        Arc::new(Activities::default()),
        Vec::new(),
    ))
    .expect("cold entry succeeds");
    transport.force_event_deadline();

    let error = block_on(run_action(
        store(),
        "app".into(),
        ActionVerb::AutofocusLock,
        transport,
        Arc::new(Reports::default()),
        Arc::new(Activities::default()),
        vec![PtpRuntimeValue {
            key: "afArea".into(),
            value: 0x0906_0403,
        }],
    ))
    .expect_err("event wait reaches the step budget");
    assert!(matches!(
        error,
        PtpExecutorError::StepFailed {
            kind: camera_protocol_ffi::ExecutorStepFailureKind::DeadlineExceeded,
            ..
        }
    ));
}

#[test]
fn match_aware_event_delivery_preserves_unrelated_events() {
    let transport = Arc::new(EngineTransport::new(
        "app",
        PtpFraming::Compressed,
        PtpFraming::Usb,
    ));
    block_on(run_mode_entry(
        store(),
        "app".into(),
        None,
        "shooting/stills".into(),
        transport.clone(),
        Arc::new(Reports::default()),
        Arc::new(Activities::default()),
        Vec::new(),
    ))
    .expect("cold entry succeeds");

    block_on(async {
        for request in [
            OperationRequest {
                data_phase_info: 1,
                code: 0x100e,
                transaction_id: 100,
                params: vec![0, 0],
            },
            OperationRequest {
                data_phase_info: 1,
                code: 0x9026,
                transaction_id: 101,
                params: vec![0x0906_0403],
            },
        ] {
            let frame = transport
                .encode(&PtpIpPacket::OperationRequest(request))
                .expect("encode request");
            transport
                .send_command_frame(frame)
                .await
                .expect("send request");
        }
        transport
            .next_event_frame(0xc005)
            .await
            .expect("select autofocus event");
    });

    assert!(transport.take_queued_event(0xc001));
}

#[test]
fn real_in_place_reopen_transition_runs_through_the_transport_seam() {
    let transport = Arc::new(EngineTransport::new(
        "app",
        PtpFraming::Compressed,
        PtpFraming::Usb,
    ));
    let store = store();
    block_on(run_mode_entry(
        store.clone(),
        "app".into(),
        None,
        "image-transfer".into(),
        transport.clone(),
        Arc::new(Reports::default()),
        Arc::new(Activities::default()),
        Vec::new(),
    ))
    .expect("image-transfer entry succeeds");

    let outcome = block_on(run_mode_entry(
        store,
        "app".into(),
        Some("image-transfer".into()),
        "shooting/stills".into(),
        transport,
        Arc::new(Reports::default()),
        Arc::new(Activities::default()),
        vec![PtpRuntimeValue {
            key: "openCaptureTxId".into(),
            value: 10,
        }],
    ))
    .expect("reopen transition succeeds");
    assert!(outcome.steps_run > 1);
}

#[test]
fn tolerated_close_response_does_not_skip_reopen_lifecycle() {
    let transport = Arc::new(EngineTransport::new(
        "app",
        PtpFraming::Compressed,
        PtpFraming::Usb,
    ));
    let store = store_with_tolerant_reopen();
    block_on(run_mode_entry(
        store.clone(),
        "app".into(),
        None,
        "image-transfer".into(),
        transport.clone(),
        Arc::new(Reports::default()),
        Arc::new(Activities::default()),
        Vec::new(),
    ))
    .expect("image-transfer entry succeeds");
    transport.install_fault(Fault::FailOperationTimes {
        code: ptp_core::codes::op::CLOSE_SESSION,
        response: 0x2019,
        remaining: 1,
    });
    let reports = Arc::new(Reports::default());

    block_on(run_mode_entry(
        store,
        "app".into(),
        Some("image-transfer".into()),
        "shooting/stills".into(),
        transport.clone(),
        reports.clone(),
        Arc::new(Activities::default()),
        Vec::new(),
    ))
    .expect("tolerant session responses preserve lifecycle");
    assert_eq!(transport.close_calls(), 1);
    assert_eq!(transport.reopen_calls(), 1);
    assert!(reports.0.lock().expect("reports").iter().any(|report| {
        report.verb == "reopenSession"
            && matches!(report.outcome, StepOutcome::Tolerated)
            && report.response_code != Some(0x2001)
    }));
}

#[test]
fn outer_reestablishment_entry_runs_only_its_exit_steps() {
    let transport = Arc::new(EngineTransport::new(
        "app",
        PtpFraming::Compressed,
        PtpFraming::Usb,
    ));
    let store = store();
    block_on(run_mode_entry(
        store.clone(),
        "app".into(),
        None,
        "shooting/stills".into(),
        transport.clone(),
        Arc::new(Reports::default()),
        Arc::new(Activities::default()),
        Vec::new(),
    ))
    .expect("shooting entry succeeds");

    let outcome = block_on(run_mode_reestablishment_exit(
        store,
        "app".into(),
        Some("shooting/stills".into()),
        "image-transfer".into(),
        transport,
        Arc::new(Reports::default()),
        Arc::new(Activities::default()),
        vec![PtpRuntimeValue {
            key: "openCaptureTxId".into(),
            value: 10,
        }],
    ))
    .expect("outer transition exit succeeds");
    assert!(outcome.steps_run > 0);
}

#[test]
fn wrong_entrypoint_and_unknown_plan_fail_typed_before_io() {
    let unsupported = block_on(run_mode_entry(
        store(),
        "usb".into(),
        None,
        "raw-conv-backup-restore".into(),
        Arc::new(PendingTransport),
        Arc::new(Reports::default()),
        Arc::new(Activities::default()),
        Vec::new(),
    ))
    .expect_err("manual entry is host-owned");
    assert!(matches!(
        unsupported,
        PtpExecutorError::UnsupportedPlan { .. }
    ));

    let unknown = block_on(run_mode_entry(
        store(),
        "app".into(),
        None,
        "missing".into(),
        Arc::new(PendingTransport),
        Arc::new(Reports::default()),
        Arc::new(Activities::default()),
        Vec::new(),
    ))
    .expect_err("unknown entry fails");
    assert!(matches!(unknown, PtpExecutorError::UnknownPlan { .. }));
}

#[test]
fn selected_object_preparation_returns_transfer_bindings() {
    let store = store();
    let projection = store
        .selected_object_transfer("app".into())
        .expect("projection query")
        .expect("app projection");
    let handle_slot = projection.params[0].clone();
    let transport = Arc::new(EngineTransport::new(
        "app",
        PtpFraming::Compressed,
        PtpFraming::Usb,
    ));
    block_on(run_mode_entry(
        store.clone(),
        "app".into(),
        None,
        "image-transfer".into(),
        transport.clone(),
        Arc::new(Reports::default()),
        Arc::new(Activities::default()),
        Vec::new(),
    ))
    .expect("image-transfer entry succeeds");

    let handle = transport.first_handle();
    let outcome = block_on(run_selected_object_preparation(
        store,
        "app".into(),
        transport,
        Arc::new(Reports::default()),
        Arc::new(Activities::default()),
        vec![PtpRuntimeValue {
            key: handle_slot,
            value: handle as u64,
        }],
    ))
    .expect("selected-object preparation succeeds");

    assert!(
        outcome
            .scope
            .iter()
            .any(|value| value.key == projection.transfer_size_slot && value.value > 0),
        "scope: {:?}",
        outcome
            .scope
            .iter()
            .map(|value| (&value.key, value.value))
            .collect::<Vec<_>>()
    );
    assert!(outcome
        .scope
        .iter()
        .any(|value| value.key == projection.chunk_size_slot && value.value > 0));
    assert!(!outcome.outputs.is_empty());
}

#[test]
fn real_import_action_captures_collection_then_chunks_each_object() {
    let store = store();
    let transport = Arc::new(EngineTransport::new(
        "app",
        PtpFraming::Compressed,
        PtpFraming::Usb,
    ));
    block_on(run_mode_entry(
        store.clone(),
        "app".into(),
        None,
        "image-transfer".into(),
        transport.clone(),
        Arc::new(Reports::default()),
        Arc::new(Activities::default()),
        Vec::new(),
    ))
    .expect("image-transfer entry succeeds");

    let outcome = block_on(run_action(
        store,
        "app".into(),
        ActionVerb::ImportObjects,
        transport,
        Arc::new(Reports::default()),
        Arc::new(Activities::default()),
        Vec::new(),
    ))
    .expect("import action succeeds");

    assert!(outcome
        .collections
        .iter()
        .any(|collection| !collection.values.is_empty()));
    assert!(outcome
        .outputs
        .iter()
        .any(|output| output.operation == 0x101b && !output.payload.is_empty()));
}

#[test]
fn malformed_collection_capture_fails_before_iteration() {
    let store = store();
    let transport = Arc::new(EngineTransport::new(
        "app",
        PtpFraming::Compressed,
        PtpFraming::Usb,
    ));
    block_on(run_mode_entry(
        store.clone(),
        "app".into(),
        None,
        "image-transfer".into(),
        transport.clone(),
        Arc::new(Reports::default()),
        Arc::new(Activities::default()),
        Vec::new(),
    ))
    .expect("image-transfer entry succeeds");
    let mut malformed = 2_u32.to_le_bytes().to_vec();
    malformed.extend_from_slice(&7_u32.to_le_bytes());
    transport.override_next_data(0x1015, vec![0xd621], malformed);

    let error = block_on(run_action(
        store,
        "app".into(),
        ActionVerb::EnumerateObjects,
        transport,
        Arc::new(Reports::default()),
        Arc::new(Activities::default()),
        Vec::new(),
    ))
    .expect_err("truncated u32 array fails loud");
    assert!(matches!(
        error,
        PtpExecutorError::StepFailed { ref detail, .. }
            if detail.contains("decode u32 array")
    ));
}

#[test]
fn collection_iteration_limit_fails_before_any_element_body() {
    let store = store();
    let transport = Arc::new(EngineTransport::new(
        "app",
        PtpFraming::Compressed,
        PtpFraming::Usb,
    ));
    block_on(run_mode_entry(
        store.clone(),
        "app".into(),
        None,
        "image-transfer".into(),
        transport.clone(),
        Arc::new(Reports::default()),
        Arc::new(Activities::default()),
        Vec::new(),
    ))
    .expect("image-transfer entry succeeds");
    let mut oversized = 100_001_u32.to_le_bytes().to_vec();
    for handle in 0..100_001_u32 {
        oversized.extend_from_slice(&handle.to_le_bytes());
    }
    transport.override_next_data(0x1015, vec![0xd621], oversized);

    let before = transport.operations().len();
    let error = block_on(run_action(
        store,
        "app".into(),
        ActionVerb::ImportObjects,
        transport.clone(),
        Arc::new(Reports::default()),
        Arc::new(Activities::default()),
        Vec::new(),
    ))
    .expect_err("collection cap fails loud");
    assert!(matches!(
        error,
        PtpExecutorError::StepFailed { ref detail, .. }
            if detail.contains("collection exceeds 100000 elements")
    ));
    assert!(!transport.operations()[before..].contains(&0x1008));
}

fn import_with_enumeration_fault(response: u16) -> (Result<(), PtpExecutorError>, usize) {
    let store = store();
    let transport = Arc::new(EngineTransport::new(
        "app",
        PtpFraming::Compressed,
        PtpFraming::Usb,
    ));
    block_on(run_mode_entry(
        store.clone(),
        "app".into(),
        None,
        "image-transfer".into(),
        transport.clone(),
        Arc::new(Reports::default()),
        Arc::new(Activities::default()),
        Vec::new(),
    ))
    .expect("image-transfer entry succeeds");
    transport.install_fault(Fault::FailOperationParamsTimes {
        code: 0x1015,
        params: vec![0xd621],
        response,
        remaining: 1,
    });
    let result = block_on(run_action(
        store,
        "app".into(),
        ActionVerb::ImportObjects,
        transport.clone(),
        Arc::new(Reports::default()),
        Arc::new(Activities::default()),
        Vec::new(),
    ))
    .map(|_| ());
    let attempts = transport.request_count(0x1015, &[0xd621]);
    (result, attempts)
}

#[test]
fn manifest_selected_response_retries_the_enumeration_sequence() {
    let (result, attempts) = import_with_enumeration_fault(0x2019);
    result.expect("selected transient response retries");
    assert_eq!(attempts, 2);
}

#[test]
fn exhausted_retry_can_be_tolerated_only_by_its_outer_step() {
    let store = store_with_tolerant_prime_retry();
    let transport = Arc::new(EngineTransport::new(
        "app",
        PtpFraming::Compressed,
        PtpFraming::Usb,
    ));
    block_on(run_mode_entry(
        store.clone(),
        "app".into(),
        None,
        "shooting/stills".into(),
        transport.clone(),
        Arc::new(Reports::default()),
        Arc::new(Activities::default()),
        Vec::new(),
    ))
    .expect("cold entry succeeds");
    transport.install_fault(Fault::FailOperationTimes {
        code: 0x9022,
        response: 0x2019,
        remaining: 3,
    });
    let reports = Arc::new(Reports::default());

    block_on(run_action(
        store,
        "app".into(),
        ActionVerb::Shutter,
        transport.clone(),
        reports.clone(),
        Arc::new(Activities::default()),
        Vec::new(),
    ))
    .expect("outer retry tolerance accepts the exhausted selected response");
    assert_eq!(transport.request_count(0x9022, &[]), 3);
    assert!(reports.0.lock().expect("reports").iter().any(|report| {
        report.verb == "retry" && matches!(report.outcome, StepOutcome::Tolerated)
    }));
}

#[test]
fn unselected_response_fails_without_retry() {
    let (result, attempts) = import_with_enumeration_fault(0x2005);
    assert!(matches!(result, Err(PtpExecutorError::StepFailed { .. })));
    assert_eq!(attempts, 1);
}

#[test]
fn standard_framing_runs_the_same_real_plan() {
    let transport = Arc::new(EngineTransport::new(
        "app",
        PtpFraming::Standard,
        PtpFraming::Usb,
    ));
    let outcome = block_on(run_mode_entry(
        store_with_standard_app_framing(),
        "app".into(),
        None,
        "shooting/stills".into(),
        transport.clone(),
        Arc::new(Reports::default()),
        Arc::new(Activities::default()),
        Vec::new(),
    ))
    .expect("standard-framed cold entry succeeds");

    assert_eq!(outcome.steps_run, 5);
    assert_eq!(transport.operations().len(), 9);
}

#[test]
fn standard_framing_rejects_a_truncated_data_phase() {
    let transport = Arc::new(EngineTransport::new(
        "app",
        PtpFraming::Standard,
        PtpFraming::Usb,
    ));
    transport.truncate_next_standard_data();
    let error = block_on(run_mode_entry(
        store_with_standard_app_framing(),
        "app".into(),
        None,
        "shooting/stills".into(),
        transport,
        Arc::new(Reports::default()),
        Arc::new(Activities::default()),
        Vec::new(),
    ))
    .expect_err("missing EndData fails loud");

    assert!(matches!(
        error,
        PtpExecutorError::StepFailed { ref detail, .. }
            if detail.contains("incomplete standard data phase")
    ));
}

#[test]
fn tolerant_repeated_send_still_issues_every_repeat() {
    let transport = Arc::new(EngineTransport::new(
        "app",
        PtpFraming::Compressed,
        PtpFraming::Usb,
    ));
    transport.install_fault(Fault::FailOperationTimes {
        code: 0x902b,
        response: 0x2019,
        remaining: 1,
    });
    let reports = Arc::new(Reports::default());
    block_on(run_mode_entry(
        store_with_tolerant_repeated_startup(),
        "app".into(),
        None,
        "shooting/stills".into(),
        transport.clone(),
        reports.clone(),
        Arc::new(Activities::default()),
        Vec::new(),
    ))
    .expect("tolerated repeat completes");

    assert_eq!(transport.request_count(0x902b, &[]), 4);
    assert!(reports.0.lock().expect("reports").iter().any(|report| {
        report.verb == "sendOp"
            && matches!(report.outcome, StepOutcome::Tolerated)
            && report.operation == Some(0x902b)
            && report.response_code == Some(0x2019)
            && report.transaction_id.is_some()
    }));
}

#[test]
fn ptp_spans_emit_the_shared_activity_stream() {
    let transport = Arc::new(EngineTransport::new(
        "app",
        PtpFraming::Compressed,
        PtpFraming::Usb,
    ));
    let reports = Arc::new(Reports::default());
    let activities = Arc::new(Activities::default());
    block_on(run_mode_entry(
        store_with_cold_entry_activities(),
        "app".into(),
        None,
        "shooting/stills".into(),
        transport,
        reports.clone(),
        activities.clone(),
        Vec::new(),
    ))
    .expect("activity-authored cold entry succeeds");

    let events = activities.0.lock().expect("activities");
    assert!(matches!(
        events.as_slice(),
        [
            ConnectionActivityEvent::Started { id: first, .. },
            ConnectionActivityEvent::Succeeded { .. },
            ConnectionActivityEvent::Started { id: second, .. },
            ConnectionActivityEvent::Succeeded { .. },
        ] if first == "camera.test.bootstrap" && second == "camera.test.stream"
    ));
    let reports = reports.0.lock().expect("reports");
    assert_eq!(
        reports[0].activity_id.as_deref(),
        Some("camera.test.bootstrap")
    );
    assert_eq!(
        reports[4].activity_id.as_deref(),
        Some("camera.test.stream")
    );
}

#[test]
fn dropping_a_pending_walk_cancels_the_active_activity_once() {
    let activities = Arc::new(Activities::default());
    let mut future = Box::pin(run_mode_entry(
        store_with_cold_entry_activities(),
        "app".into(),
        None,
        "shooting/stills".into(),
        Arc::new(PendingTransport),
        Arc::new(Reports::default()),
        activities.clone(),
        Vec::new(),
    ));
    let waker = futures::task::noop_waker();
    let mut context = Context::from_waker(&waker);
    assert!(matches!(future.as_mut().poll(&mut context), Poll::Pending));
    drop(future);

    let events = activities.0.lock().expect("activities");
    assert!(matches!(
        events.as_slice(),
        [
            ConnectionActivityEvent::Started { .. },
            ConnectionActivityEvent::Cancelled { .. }
        ]
    ));
}

enum FailureMode {
    Transport,
    Deadline,
}

struct FailingTransport(FailureMode);

struct PendingTransport;

struct WholeStepDeadlineTransport {
    pending_read_dropped: Arc<AtomicBool>,
}

struct DropSignal(Arc<AtomicBool>);

impl Drop for DropSignal {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

#[async_trait::async_trait]
impl PtpExecutorTransport for WholeStepDeadlineTransport {
    async fn reserve_transaction_id(&self) -> Result<u32, PtpTransportError> {
        Ok(2)
    }
    async fn send_command_frame(&self, _frame: Vec<u8>) -> Result<(), PtpTransportError> {
        Ok(())
    }
    async fn next_command_frame(&self) -> Result<Vec<u8>, PtpTransportError> {
        let _signal = DropSignal(self.pending_read_dropped.clone());
        futures::future::pending().await
    }
    async fn next_event_frame(&self, _event_code: u16) -> Result<Vec<u8>, PtpTransportError> {
        unreachable!()
    }
    async fn close_command_channel(
        &self,
        _transport_close_frame: Option<Vec<u8>>,
    ) -> Result<(), PtpTransportError> {
        unreachable!()
    }
    async fn reopen_command_session(&self) -> Result<PtpSessionOpenResult, PtpTransportError> {
        unreachable!()
    }
    async fn sleep(&self, ms: u32) -> Result<(), PtpTransportError> {
        if ms == 60_000 {
            Ok(())
        } else {
            futures::future::pending().await
        }
    }
}

#[async_trait::async_trait]
impl PtpExecutorTransport for PendingTransport {
    async fn reserve_transaction_id(&self) -> Result<u32, PtpTransportError> {
        futures::future::pending().await
    }
    async fn send_command_frame(&self, _frame: Vec<u8>) -> Result<(), PtpTransportError> {
        futures::future::pending().await
    }
    async fn next_command_frame(&self) -> Result<Vec<u8>, PtpTransportError> {
        futures::future::pending().await
    }
    async fn next_event_frame(&self, _event_code: u16) -> Result<Vec<u8>, PtpTransportError> {
        futures::future::pending().await
    }
    async fn close_command_channel(
        &self,
        _transport_close_frame: Option<Vec<u8>>,
    ) -> Result<(), PtpTransportError> {
        futures::future::pending().await
    }
    async fn reopen_command_session(&self) -> Result<PtpSessionOpenResult, PtpTransportError> {
        futures::future::pending().await
    }
    async fn sleep(&self, _ms: u32) -> Result<(), PtpTransportError> {
        futures::future::pending().await
    }
}

#[async_trait::async_trait]
impl PtpExecutorTransport for FailingTransport {
    async fn reserve_transaction_id(&self) -> Result<u32, PtpTransportError> {
        match self.0 {
            FailureMode::Transport => Err(PtpTransportError::Failed {
                detail: "injected".into(),
            }),
            FailureMode::Deadline => futures::future::pending().await,
        }
    }
    async fn send_command_frame(&self, _frame: Vec<u8>) -> Result<(), PtpTransportError> {
        unreachable!()
    }
    async fn next_command_frame(&self) -> Result<Vec<u8>, PtpTransportError> {
        unreachable!()
    }
    async fn next_event_frame(&self, _event_code: u16) -> Result<Vec<u8>, PtpTransportError> {
        unreachable!()
    }
    async fn close_command_channel(
        &self,
        _transport_close_frame: Option<Vec<u8>>,
    ) -> Result<(), PtpTransportError> {
        unreachable!()
    }
    async fn reopen_command_session(&self) -> Result<PtpSessionOpenResult, PtpTransportError> {
        unreachable!()
    }
    async fn sleep(&self, _ms: u32) -> Result<(), PtpTransportError> {
        match self.0 {
            FailureMode::Transport => futures::future::pending().await,
            FailureMode::Deadline => Ok(()),
        }
    }
}

fn first_step_error(mode: FailureMode) -> PtpExecutorError {
    block_on(run_mode_entry(
        store(),
        "app".into(),
        None,
        "shooting/stills".into(),
        Arc::new(FailingTransport(mode)),
        Arc::new(Reports::default()),
        Arc::new(Activities::default()),
        Vec::new(),
    ))
    .expect_err("first step fails")
}

#[test]
fn transport_failure_is_not_swallowed_by_a_tolerant_step() {
    assert!(matches!(
        first_step_error(FailureMode::Transport),
        PtpExecutorError::StepFailed {
            kind: camera_protocol_ffi::ExecutorStepFailureKind::Other,
            ..
        }
    ));
}

#[test]
fn per_verb_deadline_is_typed() {
    assert!(matches!(
        first_step_error(FailureMode::Deadline),
        PtpExecutorError::StepFailed {
            kind: camera_protocol_ffi::ExecutorStepFailureKind::DeadlineExceeded,
            ..
        }
    ));
}

#[test]
fn aggregate_step_deadline_cancels_the_pending_foreign_read() {
    let dropped = Arc::new(AtomicBool::new(false));
    let error = block_on(run_mode_entry(
        store(),
        "app".into(),
        None,
        "shooting/stills".into(),
        Arc::new(WholeStepDeadlineTransport {
            pending_read_dropped: dropped.clone(),
        }),
        Arc::new(Reports::default()),
        Arc::new(Activities::default()),
        Vec::new(),
    ))
    .expect_err("whole-step clock wins over a fresh per-call backstop");

    assert!(matches!(
        error,
        PtpExecutorError::StepFailed {
            kind: camera_protocol_ffi::ExecutorStepFailureKind::DeadlineExceeded,
            ref detail,
            ..
        } if detail.contains("step deadline")
    ));
    assert!(dropped.load(Ordering::SeqCst));
}
