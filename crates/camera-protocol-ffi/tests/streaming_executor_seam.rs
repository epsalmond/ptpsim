use std::collections::VecDeque;
use std::future::Future;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use camera_protocol_ffi::{
    parse_action_verb, run_streaming_action as execute_streaming_action, ActionArgument,
    ActionInvocationRequest, ActionRole, ActionValue, ActionVerb, ConfigStore, PtpRuntimeValue,
    PtpStreamingError, PtpStreamingOutcome, PtpStreamingSink, PtpStreamingSinkError,
    PtpStreamingTransport, PtpTransportError,
};
use futures::executor::block_on;

mod common;

fn store() -> Arc<ConfigStore> {
    common::real_fuji_store()
}

fn string_parameter_store() -> Arc<ConfigStore> {
    let original = common::data("fuji/gfx100ii/gfx100ii.yaml");
    let body = original.replacen(
        r#"      getObject:
        mode: ""
        initiator:
          params: [handle]
          steps:
            - { sendOp: "0x1009", params: [{ runtime: handle }] }"#,
        r#"      getObject:
        mode: ""
        initiator:
          params:
            - { name: handle, kind: string }
          steps:
            - { sendOp: "0x1009", params: [{ runtime: handle }] }"#,
        1,
    );
    assert_ne!(body, original, "streaming action fixture must be replaced");
    ConfigStore::from_manufacturer_index_with_defaults(
        common::data("fuji/index.yaml"),
        common::data("fuji/fuji.yaml"),
        common::real_fuji_bodies_with("gfx100ii", body),
    )
    .expect("string-parameter streaming manifest loads")
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
                value: ActionValue::U64 { value: value.value },
            })
            .collect(),
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_streaming_action(
    store: Arc<ConfigStore>,
    connection: String,
    action: ActionVerb,
    transport: Arc<dyn PtpStreamingTransport>,
    sink: Arc<dyn PtpStreamingSink>,
    runtime_params: Vec<PtpRuntimeValue>,
    expected_payload_bytes: Option<u64>,
) -> Result<PtpStreamingOutcome, PtpStreamingError> {
    let request = action_request(&store, &connection, action, runtime_params);
    execute_streaming_action(store, request, transport, sink, expected_payload_bytes).await
}

struct Transport {
    bytes: Mutex<VecDeque<u8>>,
    sent: Mutex<Vec<Vec<u8>>>,
    requested: Mutex<Vec<u32>>,
    touches: AtomicUsize,
    invalidated: AtomicBool,
    fragment_limit: usize,
    hang_reads: bool,
    fire_deadline: bool,
}

impl Transport {
    fn new(payload: Vec<u8>, response_code: u16) -> Arc<Self> {
        Self::new_with_framing(
            payload,
            response_code,
            camera_protocol_ffi::PtpFraming::Compressed,
        )
    }

    fn new_with_framing(
        payload: Vec<u8>,
        response_code: u16,
        framing: camera_protocol_ffi::PtpFraming,
    ) -> Arc<Self> {
        let response = ptp_core::PtpIpPacket::OperationResponse(ptp_core::OperationResponse {
            code: response_code,
            transaction_id: 7,
            params: vec![11, 12],
        });
        let mut bytes = match framing {
            camera_protocol_ffi::PtpFraming::Compressed => {
                protocol_primitives::fuji_framing::encode_data(0x1009, 7, &payload)
            }
            camera_protocol_ffi::PtpFraming::Usb => {
                protocol_primitives::usb_ptp::encode_data(0x1009, 7, &payload)
            }
            camera_protocol_ffi::PtpFraming::Standard => {
                panic!("standard framing does not use this streaming seam")
            }
        };
        let response = match framing {
            camera_protocol_ffi::PtpFraming::Compressed => {
                protocol_primitives::fuji_framing::encode(&response).unwrap()
            }
            camera_protocol_ffi::PtpFraming::Usb => {
                protocol_primitives::usb_ptp::encode(&response).unwrap()
            }
            camera_protocol_ffi::PtpFraming::Standard => unreachable!(),
        };
        bytes.extend_from_slice(&response);
        Arc::new(Self {
            bytes: Mutex::new(bytes.into()),
            sent: Mutex::new(Vec::new()),
            requested: Mutex::new(Vec::new()),
            touches: AtomicUsize::new(0),
            invalidated: AtomicBool::new(false),
            fragment_limit: 37_019,
            hang_reads: false,
            fire_deadline: false,
        })
    }

    fn hanging() -> Arc<Self> {
        Arc::new(Self {
            bytes: Mutex::new(VecDeque::new()),
            sent: Mutex::new(Vec::new()),
            requested: Mutex::new(Vec::new()),
            touches: AtomicUsize::new(0),
            invalidated: AtomicBool::new(false),
            fragment_limit: 1,
            hang_reads: true,
            fire_deadline: true,
        })
    }

    fn response_only(response_code: u16) -> Arc<Self> {
        let bytes = protocol_primitives::fuji_framing::encode(
            &ptp_core::PtpIpPacket::OperationResponse(ptp_core::OperationResponse {
                code: response_code,
                transaction_id: 7,
                params: vec![11, 12],
            }),
        )
        .unwrap();
        Arc::new(Self {
            bytes: Mutex::new(bytes.into()),
            sent: Mutex::new(Vec::new()),
            requested: Mutex::new(Vec::new()),
            touches: AtomicUsize::new(0),
            invalidated: AtomicBool::new(false),
            fragment_limit: 37_019,
            hang_reads: false,
            fire_deadline: false,
        })
    }

    fn malformed_response_length() -> Arc<Self> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&13_u32.to_le_bytes());
        bytes.extend_from_slice(&3_u16.to_le_bytes());
        bytes.extend_from_slice(&0x2001_u16.to_le_bytes());
        bytes.extend_from_slice(&7_u32.to_le_bytes());
        bytes.push(0xff);
        Arc::new(Self {
            bytes: Mutex::new(bytes.into()),
            sent: Mutex::new(Vec::new()),
            requested: Mutex::new(Vec::new()),
            touches: AtomicUsize::new(0),
            invalidated: AtomicBool::new(false),
            fragment_limit: 37_019,
            hang_reads: false,
            fire_deadline: false,
        })
    }

    fn cancellable() -> Arc<Self> {
        Arc::new(Self {
            bytes: Mutex::new(VecDeque::new()),
            sent: Mutex::new(Vec::new()),
            requested: Mutex::new(Vec::new()),
            touches: AtomicUsize::new(0),
            invalidated: AtomicBool::new(false),
            fragment_limit: 1,
            hang_reads: true,
            fire_deadline: false,
        })
    }
}

#[async_trait::async_trait]
impl PtpStreamingTransport for Transport {
    async fn reserve_transaction_id(&self) -> Result<u32, PtpTransportError> {
        self.touches.fetch_add(1, Ordering::SeqCst);
        Ok(7)
    }

    async fn send_command_frame(&self, frame: Vec<u8>) -> Result<(), PtpTransportError> {
        self.touches.fetch_add(1, Ordering::SeqCst);
        self.sent.lock().unwrap().push(frame);
        Ok(())
    }

    async fn receive_command_bytes(&self, max_bytes: u32) -> Result<Vec<u8>, PtpTransportError> {
        self.touches.fetch_add(1, Ordering::SeqCst);
        self.requested.lock().unwrap().push(max_bytes);
        if self.hang_reads {
            futures::future::pending().await
        } else {
            let mut source = self.bytes.lock().unwrap();
            let count = (max_bytes as usize)
                .min(self.fragment_limit)
                .min(source.len());
            Ok(source.drain(..count).collect())
        }
    }

    async fn sleep(&self, _ms: u32) -> Result<(), PtpTransportError> {
        self.touches.fetch_add(1, Ordering::SeqCst);
        if self.fire_deadline && !self.sent.lock().unwrap().is_empty() {
            Ok(())
        } else {
            futures::future::pending().await
        }
    }

    fn invalidate_command_session(&self, _reason: String) {
        self.touches.fetch_add(1, Ordering::SeqCst);
        self.invalidated.store(true, Ordering::SeqCst);
    }
}

#[derive(Default)]
struct Sink {
    expected: Mutex<Option<u64>>,
    bytes: Mutex<Vec<u8>>,
    max_write: Mutex<usize>,
    fail: AtomicBool,
}

#[async_trait::async_trait]
impl PtpStreamingSink for Sink {
    async fn begin(&self, total_bytes: u64) -> Result<(), PtpStreamingSinkError> {
        *self.expected.lock().unwrap() = Some(total_bytes);
        Ok(())
    }

    async fn write(&self, chunk: Vec<u8>) -> Result<(), PtpStreamingSinkError> {
        if self.fail.load(Ordering::SeqCst) {
            return Err(PtpStreamingSinkError::Failed {
                detail: "disk full".into(),
            });
        }
        let mut max_write = self.max_write.lock().unwrap();
        *max_write = (*max_write).max(chunk.len());
        drop(max_write);
        self.bytes.lock().unwrap().extend_from_slice(&chunk);
        Ok(())
    }
}

fn runtime_handle() -> Vec<PtpRuntimeValue> {
    vec![PtpRuntimeValue {
        key: "handle".into(),
        value: 0x1234,
    }]
}

#[test]
fn rejected_streaming_invocations_have_zero_transport_and_sink_effects() {
    let store = store();
    let base = action_request(
        &store,
        "wireless-tether",
        ActionVerb::GetObject,
        runtime_handle(),
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
                    value: ActionValue::U64 { value: 2 },
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
                    value: ActionValue::U64 { value: 2 },
                });
                request
            },
            "extraParameter",
        ),
    ];

    for (request, expected_code) in cases {
        let transport = Transport::new(Vec::new(), 0x2001);
        let sink = Arc::new(Sink::default());
        let error = block_on(execute_streaming_action(
            Arc::clone(&store),
            request,
            transport.clone(),
            sink.clone(),
            None,
        ))
        .expect_err("catalog rejection must precede streaming I/O");
        assert!(
            matches!(error, PtpStreamingError::ActionRejected { ref code, .. } if code == expected_code),
            "expected {expected_code}, got {error:?}"
        );
        assert_eq!(
            transport.touches.load(Ordering::SeqCst),
            0,
            "{expected_code}"
        );
        assert!(transport.sent.lock().unwrap().is_empty());
        assert!(transport.requested.lock().unwrap().is_empty());
        assert!(!transport.invalidated.load(Ordering::SeqCst));
        assert_eq!(*sink.expected.lock().unwrap(), None);
        assert!(sink.bytes.lock().unwrap().is_empty());
    }
}

#[test]
fn string_streaming_parameter_returns_typed_rejection_without_io() {
    let store = string_parameter_store();
    let mut request = action_request(&store, "wireless-tether", ActionVerb::GetObject, vec![]);
    request.parameters = vec![ActionArgument {
        name: "handle".into(),
        value: ActionValue::String { value: "x".into() },
    }];
    let transport = Transport::new(Vec::new(), 0x2001);
    let sink = Arc::new(Sink::default());

    let error = block_on(execute_streaming_action(
        store,
        request,
        transport.clone(),
        sink.clone(),
        None,
    ))
    .expect_err("string streaming parameter must return an error without panicking");

    match error {
        PtpStreamingError::ActionRejected { code, detail } => {
            assert_eq!(code, "wrongParameterType");
            assert_eq!(detail, "parameter \"handle\" requires U64, got String");
        }
        other => panic!("expected typed action rejection, got {other:?}"),
    }
    assert_eq!(transport.touches.load(Ordering::SeqCst), 0);
    assert_eq!(*sink.expected.lock().unwrap(), None);
    assert!(sink.bytes.lock().unwrap().is_empty());
}

#[test]
fn whole_object_streams_in_bounded_chunks_then_validates_response() {
    let payload = (0..(2 * 1024 * 1024 + 17))
        .map(|index| (index % 251) as u8)
        .collect::<Vec<_>>();
    let transport = Transport::new(payload.clone(), 0x2001);
    let sink = Arc::new(Sink::default());
    let outcome = block_on(run_streaming_action(
        store(),
        "wireless-tether".into(),
        ActionVerb::GetObject,
        transport.clone(),
        sink.clone(),
        runtime_handle(),
        Some(payload.len() as u64),
    ))
    .expect("stream succeeds");

    assert_eq!(outcome.operation, 0x1009);
    assert_eq!(outcome.transaction_id, 7);
    assert_eq!(outcome.total_bytes, payload.len() as u64);
    assert_eq!(outcome.response_params, vec![11, 12]);
    assert_eq!(*sink.expected.lock().unwrap(), Some(payload.len() as u64));
    assert_eq!(*sink.bytes.lock().unwrap(), payload);
    assert!(*sink.max_write.lock().unwrap() <= 1024 * 1024);
    assert!(transport
        .requested
        .lock()
        .unwrap()
        .iter()
        .all(|requested| *requested <= 1024 * 1024));
    assert!(!transport.invalidated.load(Ordering::SeqCst));

    let request =
        protocol_primitives::fuji_framing::decode(&transport.sent.lock().unwrap()[0]).unwrap();
    assert!(matches!(
        request,
        ptp_core::PtpIpPacket::OperationRequest(ptp_core::OperationRequest {
            code: 0x1009,
            transaction_id: 7,
            ref params,
            ..
        }) if params == &[0x1234]
    ));
}

#[test]
fn expected_length_mismatch_invalidates_the_partial_session() {
    let transport = Transport::new(vec![1, 2, 3], 0x2001);
    let sink = Arc::new(Sink::default());
    let error = block_on(run_streaming_action(
        store(),
        "wireless-tether".into(),
        ActionVerb::GetObject,
        transport.clone(),
        sink,
        runtime_handle(),
        Some(4),
    ))
    .unwrap_err();
    assert!(matches!(error, PtpStreamingError::Framing { .. }));
    assert!(transport.invalidated.load(Ordering::SeqCst));
}

#[test]
fn sink_failure_invalidates_the_partial_session() {
    let transport = Transport::new(vec![1, 2, 3], 0x2001);
    let sink = Arc::new(Sink::default());
    sink.fail.store(true, Ordering::SeqCst);
    let error = block_on(run_streaming_action(
        store(),
        "wireless-tether".into(),
        ActionVerb::GetObject,
        transport.clone(),
        sink,
        runtime_handle(),
        Some(3),
    ))
    .unwrap_err();
    assert!(matches!(error, PtpStreamingError::Sink { .. }));
    assert!(transport.invalidated.load(Ordering::SeqCst));
}

#[test]
fn complete_non_ok_response_leaves_the_session_synchronized() {
    let transport = Transport::new(vec![1, 2, 3], 0x2019);
    let error = block_on(run_streaming_action(
        store(),
        "wireless-tether".into(),
        ActionVerb::GetObject,
        transport.clone(),
        Arc::new(Sink::default()),
        runtime_handle(),
        Some(3),
    ))
    .unwrap_err();
    assert!(matches!(
        error,
        PtpStreamingError::Response {
            response_code: 0x2019,
            transaction_id: 7,
            ..
        }
    ));
    assert!(!transport.invalidated.load(Ordering::SeqCst));
}

#[test]
fn immediate_non_ok_response_is_fully_consumed_and_leaves_session_synchronized() {
    let transport = Transport::response_only(0x2009);
    let error = block_on(run_streaming_action(
        store(),
        "wireless-tether".into(),
        ActionVerb::GetObject,
        transport.clone(),
        Arc::new(Sink::default()),
        runtime_handle(),
        None,
    ))
    .unwrap_err();
    assert!(matches!(
        error,
        PtpStreamingError::Response {
            response_code: 0x2009,
            transaction_id: 7,
            ref response_params,
        } if response_params == &[11, 12]
    ));
    assert!(transport.bytes.lock().unwrap().is_empty());
    assert!(!transport.invalidated.load(Ordering::SeqCst));
}

#[test]
fn malformed_response_parameter_length_is_rejected_and_invalidates_session() {
    let transport = Transport::malformed_response_length();
    let error = block_on(run_streaming_action(
        store(),
        "wireless-tether".into(),
        ActionVerb::GetObject,
        transport.clone(),
        Arc::new(Sink::default()),
        runtime_handle(),
        None,
    ))
    .unwrap_err();
    assert!(matches!(error, PtpStreamingError::Framing { .. }));
    assert!(transport.invalidated.load(Ordering::SeqCst));
}

#[test]
fn idle_read_deadline_invalidates_the_session() {
    let transport = Transport::hanging();
    let error = block_on(run_streaming_action(
        store(),
        "wireless-tether".into(),
        ActionVerb::GetObject,
        transport.clone(),
        Arc::new(Sink::default()),
        runtime_handle(),
        None,
    ))
    .unwrap_err();
    assert!(matches!(error, PtpStreamingError::DeadlineExceeded { .. }));
    assert!(transport.invalidated.load(Ordering::SeqCst));
}

#[test]
fn cancelling_mid_frame_invalidates_the_session() {
    let transport = Transport::cancellable();
    let mut future = Box::pin(run_streaming_action(
        store(),
        "wireless-tether".into(),
        ActionVerb::GetObject,
        transport.clone(),
        Arc::new(Sink::default()),
        runtime_handle(),
        None,
    ));
    let waker = futures::task::noop_waker();
    let mut context = std::task::Context::from_waker(&waker);
    assert!(matches!(
        future.as_mut().poll(&mut context),
        std::task::Poll::Pending
    ));
    assert!(!transport.invalidated.load(Ordering::SeqCst));
    drop(future);
    assert!(transport.invalidated.load(Ordering::SeqCst));
}
