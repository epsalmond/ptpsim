//! #250 acceptance: real manifest EntrySteps cross the foreign async seam as
//! encoded PTP frames while Rust owns ordering, tolerance and deadlines.

use std::collections::{BTreeMap, VecDeque};
use std::future::Future;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

use camera_config::CameraManifest;
use camera_media_store::{MediaStore, ObjectQuery};
use camera_protocol_ffi::{
    parse_action_verb, run_initiator_action as execute_initiator_action,
    run_initiator_action_to_sink as execute_initiator_action_to_sink, run_mode_entry,
    run_mode_reestablishment_exit, run_selected_object_preparation, ActionArgument,
    ActionInvocationRequest, ActionRole, ActionVerb, ConfigStore, ConnectionActivityEvent,
    ConnectionActivityFailure, ConnectionActivityObserver, ConnectionActivityRetry,
    ConnectionActivityTerminalSummary, ExecutorStepFailureKind, PtpDataOutput, PtpDataOutputSink,
    PtpDataOutputSinkError, PtpExecutionOutcome, PtpExecutorError, PtpExecutorTransport,
    PtpFraming, PtpRuntimeValue, PtpSessionOpenResult, PtpTransportError, SocketRole, StepObserver,
    StepOutcome, StepReport,
};
use camera_sim::{Engine, Fault, Reply};
use futures::executor::block_on;
use ptp_core::{OperationRequest, PtpCodec, PtpIpPacket};

mod common;

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

fn xa7_mode_selection_store() -> Arc<ConfigStore> {
    let body = data("fuji/xa7/xa7.yaml")
        .replace("          - { getProp: \"0xdf21\" }\n", "")
        .replace("          - { setProp: \"0xdf21\", value: 4 }\n", "")
        .replace("          - { getProp: \"0xdf22\" }\n", "")
        .replace("          - { setProp: \"0xdf22\", value: 5 }\n", "")
        .replace("          - { getProp: \"0xdf31\" }\n", "")
        .replace("          - { setProp: \"0xdf31\", value: 2 }\n", "");
    ConfigStore::from_bundle(body, Some(data("fuji/fuji.yaml")))
        .expect("load focused X-A7 mode-selection manifest")
}

fn body_with_cold_entry_activities() -> String {
    data("fuji/gfx100ii/gfx100ii.yaml").replacen(
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
            executorSpan: { sequence: steps, startStep: 2, endStepExclusive: 7 }
        steps:"#,
        1,
    )
}

fn store_with_cold_entry_activities() -> Arc<ConfigStore> {
    store_from_body(body_with_cold_entry_activities())
}

fn store_with_cold_entry_activity_retry() -> Arc<ConfigStore> {
    let body = body_with_cold_entry_activities()
        .replacen(
            r#"          - { sendOp: "0x902b", repeat: 4 }"#,
            r#"          - retry:
              whenResponseCodes: ["0x2019"]
              maxAttempts: 2
              retryDelayMs: 0
              steps:
                - { sendOp: "0x902b", repeat: 4 }
          - { getProp: "0xdf2a" }"#,
            1,
        )
        .replacen("endStepExclusive: 7", "endStepExclusive: 8", 1);
    store_from_body(body)
}

fn store_with_tolerant_repeated_startup() -> Arc<ConfigStore> {
    let body = data("fuji/gfx100ii/gfx100ii.yaml").replacen(
        r#"- { sendOp: "0x902b", repeat: 4 }"#,
        r#"- { sendOp: "0x902b", repeat: 4, tolerant: true }
          - { getProp: "0xdf2a" }"#,
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
        r#"            - { sendOp: "0x9022" }"#,
        r#"            - tolerant: true
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

fn store_without_decode_retry() -> Arc<ConfigStore> {
    let body = data("fuji/gfx100ii/gfx100ii.yaml")
        .replace("                whenFailureClasses: [\"decode\"]\n", "");
    store_from_body(body)
}

fn store_with_uncaptured_scalar_event_predicate() -> Arc<ConfigStore> {
    let body = data("fuji/gfx100ii/gfx100ii.yaml")
        .replacen(
            r#"            - { sendOp: "0x100e", params: [0, 0] }"#,
            r#"            - { getProp: "0xdf01" }
            - { sendOp: "0x100e", params: [0, 0] }"#,
            1,
        )
        .replacen(
            "                until: { all: [] }",
            r#"                until: { prop: "0xdf01", eq: 0x16 }"#,
            1,
        );
    store_from_body(body)
}

fn store_with_composite_poll_action() -> Arc<ConfigStore> {
    let body = data("fuji/gfx100ii/gfx100ii.yaml").replacen(
        r#"            - { sendOp: "0x100e", params: [0, 0] }"#,
        r#"            - awaitUntil:
                source: { poll: "0xd212" }
                until: { prop: "0xd209", eq: 0 }
                timeoutMs: 1000
            - { sendOp: "0x100e", params: [0, 0] }"#,
        1,
    );
    store_from_body(body)
}

fn store_from_body(body: String) -> Arc<ConfigStore> {
    ConfigStore::from_manufacturer_index_with_defaults(
        common::data("fuji/index.yaml"),
        common::data("fuji/fuji.yaml"),
        common::real_fuji_bodies_with("gfx100ii", body),
    )
    .expect("GFX store loads")
}

fn action_request(
    store: &ConfigStore,
    connection: &str,
    action: ActionVerb,
    runtime_params: Vec<PtpRuntimeValue>,
) -> ActionInvocationRequest {
    let catalog = store.action_catalog();
    let action_id = catalog
        .actions
        .iter()
        .find(|entry| {
            entry.connection == connection
                && parse_action_verb(entry.action_id.clone()) == Some(action)
        })
        .expect("cataloged action")
        .action_id
        .clone();
    let mode = store
        .action(connection.into(), action)
        .expect("manifest action")
        .mode;
    ActionInvocationRequest {
        catalog_revision: catalog.revision,
        action_id,
        connection: connection.into(),
        mode,
        role: ActionRole::Initiator,
        parameters: runtime_params
            .into_iter()
            .map(|value| ActionArgument {
                name: value.key,
                value: value.value,
            })
            .collect(),
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_initiator_action(
    store: Arc<ConfigStore>,
    connection: String,
    action: ActionVerb,
    transport: Arc<dyn PtpExecutorTransport>,
    observer: Arc<dyn StepObserver>,
    activity_observer: Arc<dyn ConnectionActivityObserver>,
    runtime_params: Vec<PtpRuntimeValue>,
) -> Result<PtpExecutionOutcome, PtpExecutorError> {
    let request = action_request(&store, &connection, action, runtime_params);
    execute_initiator_action(store, request, transport, observer, activity_observer).await
}

#[allow(clippy::too_many_arguments)]
async fn run_initiator_action_to_sink(
    store: Arc<ConfigStore>,
    connection: String,
    action: ActionVerb,
    transport: Arc<dyn PtpExecutorTransport>,
    observer: Arc<dyn StepObserver>,
    activity_observer: Arc<dyn ConnectionActivityObserver>,
    sink: Arc<dyn PtpDataOutputSink>,
    runtime_params: Vec<PtpRuntimeValue>,
) -> Result<PtpExecutionOutcome, PtpExecutorError> {
    let request = action_request(&store, &connection, action, runtime_params);
    execute_initiator_action_to_sink(store, request, transport, observer, activity_observer, sink)
        .await
}

#[derive(Default)]
struct TripwireExecutorTransport(AtomicUsize);

impl TripwireExecutorTransport {
    fn touched(&self) -> PtpTransportError {
        self.0.fetch_add(1, Ordering::SeqCst);
        PtpTransportError::Failed {
            detail: "preflight reached transport".into(),
        }
    }

    fn touches(&self) -> usize {
        self.0.load(Ordering::SeqCst)
    }
}

#[async_trait::async_trait]
impl PtpExecutorTransport for TripwireExecutorTransport {
    async fn reserve_transaction_id(&self) -> Result<u32, PtpTransportError> {
        Err(self.touched())
    }

    async fn send_command_frame(&self, _frame: Vec<u8>) -> Result<(), PtpTransportError> {
        Err(self.touched())
    }

    async fn next_command_frame(&self) -> Result<Vec<u8>, PtpTransportError> {
        Err(self.touched())
    }

    async fn next_event_frame(&self, _event_code: u16) -> Result<Vec<u8>, PtpTransportError> {
        Err(self.touched())
    }

    async fn open_channel(&self, _role: SocketRole) -> Result<(), PtpTransportError> {
        Err(self.touched())
    }

    async fn close_command_channel(
        &self,
        _transport_close_frame: Option<Vec<u8>>,
    ) -> Result<(), PtpTransportError> {
        Err(self.touched())
    }

    async fn reopen_command_session(&self) -> Result<PtpSessionOpenResult, PtpTransportError> {
        Err(self.touched())
    }

    async fn sleep(&self, _ms: u32) -> Result<(), PtpTransportError> {
        Err(self.touched())
    }
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
struct RecordingSink(Mutex<Vec<PtpDataOutput>>);

#[async_trait::async_trait]
impl PtpDataOutputSink for RecordingSink {
    async fn write(&self, output: PtpDataOutput) -> Result<(), PtpDataOutputSinkError> {
        self.0.lock().expect("sink outputs").push(output);
        Ok(())
    }
}

struct FailOnceSink(AtomicBool);

#[async_trait::async_trait]
impl PtpDataOutputSink for FailOnceSink {
    async fn write(&self, _output: PtpDataOutput) -> Result<(), PtpDataOutputSinkError> {
        if !self.0.swap(true, Ordering::SeqCst) {
            Err(PtpDataOutputSinkError::Failed {
                detail: "injected sink failure".into(),
            })
        } else {
            Ok(())
        }
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
    data_writes: Vec<(u16, Vec<u32>, Vec<u8>)>,
    opened_channels: Vec<SocketRole>,
    calls: Vec<ExecutorCall>,
    operations_by_tid: BTreeMap<u32, u16>,
    next_tid: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ExecutorCall {
    Operation(u16),
    OperationCompleted(u16),
    OpenChannel(SocketRole),
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
                data_writes: Vec::new(),
                opened_channels: Vec::new(),
                calls: Vec::new(),
                operations_by_tid: BTreeMap::new(),
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

    fn opened_channels(&self) -> Vec<SocketRole> {
        self.state.lock().expect("state").opened_channels.clone()
    }

    fn calls(&self) -> Vec<ExecutorCall> {
        self.state.lock().expect("state").calls.clone()
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

    fn data_writes(&self, operation: u16, params: &[u32]) -> Vec<Vec<u8>> {
        self.state
            .lock()
            .expect("state")
            .data_writes
            .iter()
            .filter(|(candidate, candidate_params, _)| {
                *candidate == operation && candidate_params == params
            })
            .map(|(_, _, payload)| payload.clone())
            .collect()
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
                PtpFraming::Usb => {
                    state
                        .replies
                        .push_back(protocol_primitives::usb_ptp::encode_data(
                            request.code,
                            request.transaction_id,
                            &payload,
                        ))
                }
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
                state.calls.push(ExecutorCall::Operation(request.code));
                state
                    .operations_by_tid
                    .insert(request.transaction_id, request.code);
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
                    state.data_writes.push((
                        pending.request.code,
                        pending.request.params.clone(),
                        pending.payload.clone(),
                    ));
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
                state.data_writes.push((
                    pending.request.code,
                    pending.request.params.clone(),
                    pending.payload.clone(),
                ));
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
        let mut state = self.state.lock().expect("state");
        let frame = state
            .replies
            .pop_front()
            .ok_or_else(|| PtpTransportError::Failed {
                detail: "response queue empty".into(),
            })?;
        if let PtpIpPacket::OperationResponse(response) = self.decode(&frame)? {
            if let Some(operation) = state.operations_by_tid.remove(&response.transaction_id) {
                state
                    .calls
                    .push(ExecutorCall::OperationCompleted(operation));
            }
        }
        Ok(frame)
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

    async fn open_channel(&self, role: SocketRole) -> Result<(), PtpTransportError> {
        let mut state = self.state.lock().expect("state");
        state.opened_channels.push(role);
        state.calls.push(ExecutorCall::OpenChannel(role));
        Ok(())
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
    assert_eq!(
        transport.opened_channels(),
        vec![SocketRole::Event, SocketRole::LiveView]
    );
    assert!(matches!(
        transport.calls().as_slice(),
        [
            ..,
            ExecutorCall::OperationCompleted(0x902b),
            ExecutorCall::OpenChannel(SocketRole::Event),
            ExecutorCall::OpenChannel(SocketRole::LiveView),
            ExecutorCall::Operation(0x101c),
            ExecutorCall::OperationCompleted(0x101c)
        ]
    ));
    assert_eq!(outcome.steps_run, 7);
    assert_eq!(reports.0.lock().expect("reports").len(), 14);
    assert!(activities.0.lock().expect("activities").is_empty());
}

#[test]
fn xa7_neutral_mode_entries_write_exactly_one_selected_function_mode() {
    let store = xa7_mode_selection_store();
    for (target, function_mode, expected) in [
        ("photo-receiver", 4u16, 8u16),
        ("photo-receiver", 6, 8),
        ("photo-receiver", 7, 1),
        ("photo-viewer", 4, 9),
        ("photo-viewer", 6, 9),
        ("photo-viewer", 7, 2),
        ("gps-assist", 4, 10),
        ("gps-assist", 6, 10),
        ("gps-assist", 7, 17),
    ] {
        let transport = Arc::new(EngineTransport::new(
            "app",
            PtpFraming::Usb,
            PtpFraming::Usb,
        ));
        transport.override_next_data(
            ptp_core::codes::op::GET_DEVICE_PROP_VALUE,
            vec![0xdf00],
            function_mode.to_le_bytes().to_vec(),
        );
        block_on(run_mode_entry(
            Arc::clone(&store),
            "legacy-app".into(),
            None,
            target.into(),
            transport.clone(),
            Arc::new(Reports::default()),
            Arc::new(Activities::default()),
            Vec::new(),
        ))
        .unwrap_or_else(|error| panic!("{target} DF00={function_mode}: {error}"));
        assert_eq!(
            transport.data_writes(ptp_core::codes::op::SET_DEVICE_PROP_VALUE, &[0xdf01]),
            vec![expected.to_le_bytes().to_vec()],
            "{target} DF00={function_mode}"
        );
    }
}

#[test]
fn rejected_action_invocations_never_touch_the_transport_or_observers() {
    let store = store();
    let base = action_request(
        &store,
        "wireless-tether",
        ActionVerb::GetObject,
        vec![PtpRuntimeValue {
            key: "handle".into(),
            value: 1,
        }],
    );
    let cases = [
        (
            {
                let mut request = base.clone();
                request.catalog_revision = "stale".into();
                request
            },
            "staleCatalogRevision",
        ),
        (
            {
                let mut request = base.clone();
                request.connection = "app".into();
                request
            },
            "wrongMode",
        ),
        (
            {
                let mut request = base.clone();
                request.role = ActionRole::Responder;
                request
            },
            "wrongRole",
        ),
        (
            {
                let mut request = base.clone();
                request.parameters.push(ActionArgument {
                    name: "handle".into(),
                    value: 2,
                });
                request
            },
            "duplicateParameter",
        ),
        (
            {
                let mut request = base.clone();
                request.parameters.clear();
                request
            },
            "missingParameter",
        ),
        (
            {
                let mut request = base;
                request.parameters.push(ActionArgument {
                    name: "extra".into(),
                    value: 2,
                });
                request
            },
            "extraParameter",
        ),
    ];

    for (request, expected_code) in cases {
        let transport = Arc::new(TripwireExecutorTransport::default());
        let reports = Arc::new(Reports::default());
        let activities = Arc::new(Activities::default());
        let error = block_on(execute_initiator_action(
            Arc::clone(&store),
            request,
            transport.clone(),
            reports.clone(),
            activities.clone(),
        ))
        .expect_err("catalog rejection must precede execution");
        assert!(
            matches!(error, PtpExecutorError::ActionRejected { ref code, .. } if code == expected_code),
            "expected {expected_code}, got {error:?}"
        );
        assert_eq!(transport.touches(), 0, "{expected_code}");
        assert!(reports.0.lock().expect("reports").is_empty());
        assert!(activities.0.lock().expect("activities").is_empty());
    }
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

    let outcome = block_on(run_initiator_action(
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

    block_on(run_initiator_action(
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

    block_on(run_initiator_action(
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

    let error = block_on(run_initiator_action(
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
    let source = block_on(run_mode_entry(
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
    let open_capture_txid = source
        .scope
        .iter()
        .find(|value| value.key == "openCaptureTxId")
        .expect("cold entry captures open-capture transaction")
        .value;

    let outcome = block_on(run_mode_reestablishment_exit(
        store,
        "app".into(),
        Some("shooting/stills".into()),
        "image-transfer".into(),
        transport.clone(),
        Arc::new(Reports::default()),
        Arc::new(Activities::default()),
        source.scope,
    ))
    .expect("outer transition exit succeeds");
    assert!(outcome.steps_run > 0);
    assert_eq!(
        transport.request_count(0x1018, &[open_capture_txid as u32]),
        1,
        "TerminateOpenCapture consumes the captured 0x101c transaction"
    );
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

    let outcome = block_on(run_initiator_action(
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
fn ordinary_action_sink_receives_completed_outputs_in_wire_order() {
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
    let sink = Arc::new(RecordingSink::default());

    let outcome = block_on(run_initiator_action_to_sink(
        store,
        "app".into(),
        ActionVerb::ImportObjects,
        transport,
        Arc::new(Reports::default()),
        Arc::new(Activities::default()),
        sink.clone(),
        Vec::new(),
    ))
    .expect("import action succeeds");

    assert!(outcome.outputs.is_empty(), "sink owns ordinary outputs");
    let outputs = sink.0.lock().expect("sink outputs");
    assert!(!outputs.is_empty());
    assert_eq!(outputs[0].operation, 0x1015, "enumeration arrives first");
    assert!(outputs
        .iter()
        .any(|output| output.operation == 0x101b && !output.payload.is_empty()));
    assert!(outputs
        .windows(2)
        .all(|pair| pair[0].transaction_id < pair[1].transaction_id));
}

#[test]
fn ordinary_sink_failure_leaves_command_session_synchronized() {
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

    let error = block_on(run_initiator_action_to_sink(
        store.clone(),
        "app".into(),
        ActionVerb::EnumerateObjects,
        transport.clone(),
        Arc::new(Reports::default()),
        Arc::new(Activities::default()),
        Arc::new(FailOnceSink(AtomicBool::new(false))),
        Vec::new(),
    ))
    .expect_err("sink failure reaches the caller");
    let PtpExecutorError::StepFailed {
        detail, context, ..
    } = error
    else {
        panic!("expected correlated step failure")
    };
    assert!(detail.contains("injected sink failure"));
    assert!(context
        .iter()
        .any(|value| value.key == "operation" && value.value == "0x1015"));
    assert!(context
        .iter()
        .any(|value| value.key == "response" && value.value == "0x2001"));
    assert!(context.iter().any(|value| value.key == "transactionId"));

    block_on(run_initiator_action(
        store,
        "app".into(),
        ActionVerb::EnumerateObjects,
        transport,
        Arc::new(Reports::default()),
        Arc::new(Activities::default()),
        Vec::new(),
    ))
    .expect("next transaction succeeds after completed-response sink failure");
}

#[test]
fn malformed_collection_capture_fails_before_iteration() {
    let store = store_without_decode_retry();
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

    let error = block_on(run_initiator_action(
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
fn collection_capture_rejects_trailing_bytes() {
    let store = store_without_decode_retry();
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
    let mut malformed = 1_u32.to_le_bytes().to_vec();
    malformed.extend_from_slice(&7_u32.to_le_bytes());
    malformed.push(0xff);
    transport.override_next_data(0x1015, vec![0xd621], malformed);

    let error = block_on(run_initiator_action(
        store,
        "app".into(),
        ActionVerb::EnumerateObjects,
        transport,
        Arc::new(Reports::default()),
        Arc::new(Activities::default()),
        Vec::new(),
    ))
    .expect_err("trailing bytes after a u32 array fail loud");
    assert!(matches!(
        error,
        PtpExecutorError::StepFailed { ref detail, .. }
            if detail.contains("trailing bytes")
    ));
}

#[test]
fn collection_capture_rejects_over_ceiling_count_before_payload_allocation() {
    let store = store_without_decode_retry();
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
    transport.override_next_data(0x1015, vec![0xd621], (100_001_u32).to_le_bytes().to_vec());

    let error = block_on(run_initiator_action(
        store,
        "app".into(),
        ActionVerb::EnumerateObjects,
        transport,
        Arc::new(Reports::default()),
        Arc::new(Activities::default()),
        Vec::new(),
    ))
    .expect_err("an over-ceiling header fails before reading array values");
    assert!(matches!(
        error,
        PtpExecutorError::StepFailed { ref detail, .. }
            if detail.contains("exceeds 100000")
    ));
}

#[test]
fn wireless_tether_enumeration_captures_send_op_collection() {
    let store = store();
    let transport = Arc::new(EngineTransport::new(
        "wireless-tether",
        PtpFraming::Compressed,
        PtpFraming::Usb,
    ));
    transport
        .state
        .lock()
        .expect("state")
        .engine
        .configure_standard_object_queue("wireless-tether", 1)
        .expect("queue config");

    block_on(run_initiator_action(
        store.clone(),
        "wireless-tether".into(),
        ActionVerb::Shutter,
        transport.clone(),
        Arc::new(Reports::default()),
        Arc::new(Activities::default()),
        Vec::new(),
    ))
    .expect("shutter feeds the standard object queue");

    let outcome = block_on(run_initiator_action(
        store,
        "wireless-tether".into(),
        ActionVerb::EnumerateObjects,
        transport.clone(),
        Arc::new(Reports::default()),
        Arc::new(Activities::default()),
        Vec::new(),
    ))
    .expect("standard GetObjectHandles collection capture succeeds");

    assert!(outcome.collections.iter().any(|collection| {
        collection.key == "objectHandles"
            && collection.values == vec![u64::from(transport.first_handle())]
    }));
}

#[test]
fn malformed_send_op_collection_capture_fails_loud() {
    let store = store();
    let transport = Arc::new(EngineTransport::new(
        "wireless-tether",
        PtpFraming::Compressed,
        PtpFraming::Usb,
    ));
    transport
        .state
        .lock()
        .expect("state")
        .engine
        .configure_standard_object_queue("wireless-tether", 1)
        .expect("queue config");
    let mut malformed = 2_u32.to_le_bytes().to_vec();
    malformed.extend_from_slice(&transport.first_handle().to_le_bytes());
    transport.override_next_data(0x1007, vec![u32::MAX, 0], malformed);

    let error = block_on(run_initiator_action(
        store,
        "wireless-tether".into(),
        ActionVerb::EnumerateObjects,
        transport,
        Arc::new(Reports::default()),
        Arc::new(Activities::default()),
        Vec::new(),
    ))
    .expect_err("truncated sendOp u32 array fails loud");

    assert!(matches!(
        error,
        PtpExecutorError::StepFailed { ref detail, .. }
            if detail.contains("decode u32 array")
    ));
}

#[test]
fn collection_iteration_limit_fails_before_any_element_body() {
    let store = store_without_decode_retry();
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
    let error = block_on(run_initiator_action(
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
            if detail.contains("count 100001 exceeds 100000")
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
    let result = block_on(run_initiator_action(
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

    block_on(run_initiator_action(
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

fn enumerate_with_truncated_d212(
    store: Arc<ConfigStore>,
    truncated_reads: u32,
) -> (Result<(), PtpExecutorError>, usize, Arc<Reports>) {
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
    let entry_reads = transport.request_count(0x1015, &[0xd212]);
    transport.install_fault(Fault::TruncateDataParamsTimes {
        code: 0x1015,
        params: vec![0xd212],
        keep: 4,
        remaining: truncated_reads,
    });
    let reports = Arc::new(Reports::default());
    let result = block_on(run_initiator_action(
        store,
        "app".into(),
        ActionVerb::EnumerateObjects,
        transport.clone(),
        reports.clone(),
        Arc::new(Activities::default()),
        Vec::new(),
    ))
    .map(|_| ());
    let action_reads = transport.request_count(0x1015, &[0xd212]) - entry_reads;
    (result, action_reads, reports)
}

#[test]
fn selected_decode_failure_retries_the_enumeration_sequence() {
    let (result, d212_reads, reports) = enumerate_with_truncated_d212(store(), 1);
    result.expect("selected transient decode failure retries");
    // Attempt one dies on its first truncated D212 read; attempt two completes
    // both tolerant D212 reads of the priming sequence.
    assert_eq!(d212_reads, 3);
    assert!(reports.0.lock().expect("reports").iter().any(|report| {
        report.verb == "retry"
            && report.attempts == 1
            && matches!(report.outcome, StepOutcome::Succeeded)
    }));
}

#[test]
fn exhausted_decode_retry_fails_loud_with_the_decode_detail() {
    let (result, d212_reads, _) = enumerate_with_truncated_d212(store(), u32::MAX);
    assert!(matches!(
        result,
        Err(PtpExecutorError::StepFailed { ref detail, .. })
            if detail.contains("unexpected end of input")
    ));
    // Every attempt of the maxAttempts: 5 budget dies on its first D212 read.
    assert_eq!(d212_reads, 5);
}

#[test]
fn unselected_decode_failure_fails_without_retry() {
    let (result, d212_reads, _) = enumerate_with_truncated_d212(store_without_decode_retry(), 1);
    assert!(matches!(
        result,
        Err(PtpExecutorError::StepFailed { ref detail, .. })
            if detail.contains("unexpected end of input")
    ));
    assert_eq!(d212_reads, 1);
}

#[test]
fn transport_failure_is_not_selected_by_a_decode_retry() {
    let transport = Arc::new(EngineTransport::new(
        "app",
        PtpFraming::Compressed,
        PtpFraming::Usb,
    ));
    block_on(run_mode_entry(
        store().clone(),
        "app".into(),
        None,
        "image-transfer".into(),
        transport.clone(),
        Arc::new(Reports::default()),
        Arc::new(Activities::default()),
        Vec::new(),
    ))
    .expect("image-transfer entry succeeds");
    transport.install_fault(Fault::CloseOnOperation { code: 0x9050 });
    let error = block_on(run_initiator_action(
        store(),
        "app".into(),
        ActionVerb::EnumerateObjects,
        transport.clone(),
        Arc::new(Reports::default()),
        Arc::new(Activities::default()),
        Vec::new(),
    ))
    .expect_err("transport failure escapes the decode-selecting retry");
    assert!(matches!(error, PtpExecutorError::StepFailed { .. }));
    assert_eq!(transport.request_count(0x9050, &[]), 1);
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

    assert_eq!(outcome.steps_run, 7);
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
fn ptp_retry_reports_typed_empty_context_and_terminal_count() {
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
    let activities = Arc::new(Activities::default());

    block_on(run_mode_entry(
        store_with_cold_entry_activity_retry(),
        "app".into(),
        None,
        "shooting/stills".into(),
        transport,
        Arc::new(Reports::default()),
        activities.clone(),
        Vec::new(),
    ))
    .expect("the manifest-selected PTP response retries once");

    let retry = ConnectionActivityRetry {
        ordinal: 2,
        limit: 2,
        failure: ConnectionActivityFailure {
            kind: ExecutorStepFailureKind::Other,
            context: vec![],
        },
    };
    let events = activities.0.lock().expect("activities");
    assert!(events.contains(&ConnectionActivityEvent::Retrying {
        id: "camera.test.stream".into(),
        version: 1,
        retry: retry.clone(),
    }));
    assert!(events.contains(&ConnectionActivityEvent::Succeeded {
        id: "camera.test.stream".into(),
        version: 1,
        summary: ConnectionActivityTerminalSummary {
            retry_count: 1,
            last_retry: Some(retry),
        },
    }));
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
    async fn open_channel(&self, _role: SocketRole) -> Result<(), PtpTransportError> {
        Ok(())
    }
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
    async fn open_channel(&self, _role: SocketRole) -> Result<(), PtpTransportError> {
        Ok(())
    }
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
    async fn open_channel(&self, _role: SocketRole) -> Result<(), PtpTransportError> {
        Ok(())
    }
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
