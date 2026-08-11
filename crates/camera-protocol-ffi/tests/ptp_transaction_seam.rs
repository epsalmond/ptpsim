//! #342 acceptance: a `daemonAttached` (`usb-passthrough`) connection walks
//! the §11.24 entry/action grammar over typed `PtpTransactionTransport` calls
//! instead of raw frames, and a lost event on a `bestEffort` connection
//! reconciles through the declared `thenPoll` loop (§11.29). The seam is
//! backed by the scripted USB responder's transaction side
//! (`camera_sim::usb`, shared with the camera-sim acceptance tests).

use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

use camera_protocol_ffi::{
    run_initiator_action_txn, run_mode_entry, run_mode_entry_txn,
    run_selected_object_preparation_txn, ActionInvocationRequest, ActionRole, ConfigStore,
    ConnectionActivityEvent, ConnectionActivityObserver, ExecutorStepFailureKind, PtpExecutorError,
    PtpExecutorTransport, PtpRuntimeValue, PtpSessionOpenResult, PtpTransactionError,
    PtpTransactionEvent, PtpTransactionResult, PtpTransactionTransport, PtpTransportError,
    SocketRole, StepObserver, StepReport,
};
use camera_sim::usb::{UsbEvent, UsbResponder, UsbTxnReply};
use futures::executor::block_on;
use ptp_core::{ObjectInfo, Writer};

mod common;

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

fn selected_object_store(preparation_prefix: &str) -> Arc<ConfigStore> {
    ConfigStore::from_bundle(
        format!(
            r#"schema: camera-config/v1
camera: {{ manufacturer: Test, model: Txn, firmware: "1" }}
media:
  formats:
    "0x3801": {{ name: exifJpeg, vendor: standard, isPhotosCompatible: true }}
properties:
  "0xd209": {{ name: autofocusResult, type: u16, access: readWrite }}
  "0xd226": {{ name: imageImportFilter, type: u16, access: readWrite }}
  "0xd235": {{ name: chunkSize, type: u32, access: readOnly }}
  "0xd621": {{ name: objectHandles, access: readOnly }}
connections:
  usbTether:
    kind: usb-passthrough
    session: {{ ownership: daemonAttached }}
    events: {{ delivery: bestEffort }}
    modes: [image-transfer]
    objectTransfer:
      strategy: chunked
      resumePolicy: byteOffset
      readAction: getObject
      formats: {{ "0x3801": confirmed }}
    actions:
      importObjects:
        mode: image-transfer
        initiator:
          steps:
            - getProp: "0xd621"
              captures: [{{ bind: objectHandles, as: ptpU32Array }}]
            - loop:
                forEach:
                  in: objectHandles
                  bind: handle
                  body:
{preparation_prefix}
                    - getProp: "0xd235"
                      captures:
                        - bind: chunkSize
                          as: propValue
                          fallback:
                            value: 0x00200000
                            whenResponseCodes: ["0x200a"]
                    - loop:
                        chunk:
                          total: objectTransferSize
                          size: {{ runtime: chunkSize }}
                          offsetBind: offset
                          lengthBind: length
                          body:
                            - sendOp: "0x101b"
                              params:
                                - {{ runtime: handle }}
                                - {{ runtime: offset, mask: 0xffffffff }}
                                - {{ runtime: length }}
                                - {{ runtime: offset, shift: 32 }}
      getObject:
        mode: image-transfer
        initiator:
          params: [handle, offset, length]
          steps:
            - sendOp: "0x101b"
              params:
                - {{ runtime: handle }}
                - {{ runtime: offset, mask: 0xffffffff }}
                - {{ runtime: length }}
                - {{ runtime: offset, shift: 32 }}
"#
        ),
        None,
    )
    .expect("selected-object transaction store loads")
}

fn object_info_payload(size: u32) -> Vec<u8> {
    let mut writer = Writer::new();
    ObjectInfo {
        storage_id: 0x0001_0001,
        object_format: 0xb103,
        object_compressed_size: size,
        parent_object: 0x20,
        association_type: 1,
        association_desc: 0x30,
        filename: "DSCF0001.RAF".into(),
        ..Default::default()
    }
    .encode(&mut writer)
    .expect("ObjectInfo encodes");
    writer.into_vec()
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
    pending_execute: Option<(u16, Arc<AtomicBool>)>,
    failed_execute: Option<u16>,
}

impl ResponderTxnTransport {
    fn new(responder: UsbResponder, pends_at: &[u32]) -> Self {
        ResponderTxnTransport {
            responder: Mutex::new(responder),
            pends_at: pends_at.to_vec(),
            pending_execute: None,
            failed_execute: None,
        }
    }

    fn with_pending_execute(mut self, opcode: u16, cancelled: Arc<AtomicBool>) -> Self {
        self.pending_execute = Some((opcode, cancelled));
        self
    }

    fn with_failed_execute(mut self, opcode: u16) -> Self {
        self.failed_execute = Some(opcode);
        self
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
        if self.failed_execute == Some(opcode) {
            return Err(PtpTransactionError::Failed {
                detail: "scripted transport failure".into(),
            });
        }
        let reply = self.responder.lock().expect("responder").execute(
            opcode,
            &params,
            data_out.as_deref(),
            timeout_ms,
        );
        if let Some((pending_opcode, cancelled)) = &self.pending_execute {
            if *pending_opcode == opcode {
                struct CancellationSignal(Arc<AtomicBool>);
                impl Drop for CancellationSignal {
                    fn drop(&mut self) {
                        self.0.store(true, Ordering::SeqCst);
                    }
                }
                let _signal = CancellationSignal(Arc::clone(cancelled));
                futures::future::pending::<()>().await;
            }
        }
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
fn real_pass_through_device_info_action_uses_the_transaction_seam() {
    let store = common::real_fuji_store();
    let request = ActionInvocationRequest {
        catalog_revision: store.action_catalog().revision,
        action_id: "readDeviceInfo".into(),
        connection: "usb-passthrough".into(),
        mode: String::new(),
        role: ActionRole::Initiator,
        parameters: Vec::new(),
    };
    let responder = UsbResponder::new().reply_transaction(
        0x1001,
        &[],
        UsbTxnReply::ok(Some(vec![0x01, 0x02, 0x03])),
    );
    let transport = Arc::new(ResponderTxnTransport::new(responder, &[60_000]));

    let outcome = block_on(run_initiator_action_txn(
        store,
        request,
        transport.clone(),
        Arc::new(NullObserver),
        Arc::new(NullActivities),
    ))
    .expect("the manifest readDeviceInfo action runs on the daemon session");

    assert_eq!(outcome.steps_run, 1);
    assert_eq!(
        transport.log(),
        vec![UsbEvent::Transaction {
            opcode: 0x1001,
            params: Vec::new(),
            data_out: None,
            timeout_ms: 10_000,
        }]
    );
}

#[test]
fn real_pass_through_image_transfer_entry_runs_no_transactions() {
    let store = common::real_fuji_store();
    let transport = Arc::new(ResponderTxnTransport::new(UsbResponder::new(), &[60_000]));

    let outcome = block_on(run_mode_entry_txn(
        store,
        "usb-passthrough".into(),
        None,
        "image-transfer".into(),
        transport.clone(),
        Arc::new(NullObserver),
        Arc::new(NullActivities),
        Vec::new(),
    ))
    .expect("the daemon-owned image-transfer entry needs no preparation transaction");

    assert_eq!(outcome.steps_run, 0);
    assert_eq!(transport.log(), Vec::<UsbEvent>::new());
}

#[test]
fn selected_object_preparation_runs_over_typed_transactions() {
    let store = selected_object_store(
        r#"                    - { getProp: "0xd209", tolerant: true }
                    - { setProp: "0xd226", value: 1, tolerant: true }
                    - sendOp: "0x1008"
                      params: [{ runtime: handle }]
                      captures:
                        - { bind: objectReportedSize, as: objectInfoCompressedSize }
                        - { bind: objectTransferSize, as: objectInfoCompressedSize }
                    - if:
                        slot: objectReportedSize
                        equals: 0xffffffff
                        then:
                          - sendOp: "0x9803"
                            params: [{ runtime: handle }, 0xdc04]
                            captures: [{ bind: objectTransferSize, as: u64Le }]"#,
    );
    let handle = 0x41;
    let responder = UsbResponder::new()
        .reply_transaction(
            0x1015,
            &[0xd209],
            UsbTxnReply::response(0x200a, vec![], None),
        )
        .reply_transaction(
            0x1016,
            &[0xd226],
            UsbTxnReply::response(0x201d, vec![], None),
        )
        .reply_transaction(
            0x1008,
            &[handle],
            UsbTxnReply::ok(Some(object_info_payload(0x1234_5678))),
        )
        .reply_transaction(
            0x1015,
            &[0xd235],
            UsbTxnReply::ok(Some(0x0020_0000_u32.to_le_bytes().to_vec())),
        );
    let transport = Arc::new(ResponderTxnTransport::new(responder, &[60_000]));

    let outcome = block_on(run_selected_object_preparation_txn(
        store,
        "usbTether".into(),
        transport.clone(),
        Arc::new(NullObserver),
        Arc::new(NullActivities),
        vec![PtpRuntimeValue {
            key: "handle".into(),
            value: handle as u64,
        }],
    ))
    .expect("selected-object preparation uses the transaction seam");

    assert_eq!(outcome.steps_run, 5);
    for (key, expected) in [
        ("objectReportedSize", 0x1234_5678_u64),
        ("objectTransferSize", 0x1234_5678),
        ("chunkSize", 0x0020_0000),
    ] {
        assert_eq!(
            outcome
                .scope
                .iter()
                .find(|value| value.key == key)
                .unwrap_or_else(|| panic!("{key} is captured"))
                .value
                .as_u64(),
            Some(expected),
        );
    }
    assert_eq!(
        outcome
            .outputs
            .iter()
            .map(|output| output.operation)
            .collect::<Vec<_>>(),
        vec![0x1008, 0x1015],
        "ObjectInfo remains the first preparation output",
    );
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
                opcode: 0x1016,
                params: vec![0xd226],
                data_out: Some(vec![1, 0]),
                timeout_ms: 10_000,
            },
            UsbEvent::Transaction {
                opcode: 0x1008,
                params: vec![handle],
                data_out: None,
                timeout_ms: 10_000,
            },
            UsbEvent::Transaction {
                opcode: 0x1015,
                params: vec![0xd235],
                data_out: None,
                timeout_ms: 10_000,
            },
        ]
    );
}

#[test]
fn selected_object_preparation_binds_selected_prop_fallback() {
    let preparation = r#"                    - sendOp: "0x1008"
                      params: [{ runtime: handle }]
                      captures:
                        - { bind: objectReportedSize, as: objectInfoCompressedSize }
                        - { bind: objectTransferSize, as: objectInfoCompressedSize }
                    - if:
                        slot: objectReportedSize
                        equals: 0xffffffff
                        then:
                          - sendOp: "0x9803"
                            params: [{ runtime: handle }, 0xdc04]
                            captures: [{ bind: objectTransferSize, as: u64Le }]"#;
    let handle = 0x44;
    let responder = UsbResponder::new()
        .reply_transaction(
            0x1008,
            &[handle],
            UsbTxnReply::ok(Some(object_info_payload(0x0760_0000))),
        )
        .reply_transaction(
            0x1015,
            &[0xd235],
            UsbTxnReply::response(0x200a, vec![], None),
        );
    let transport = Arc::new(ResponderTxnTransport::new(responder, &[60_000]));

    let outcome = block_on(run_selected_object_preparation_txn(
        selected_object_store(preparation),
        "usbTether".into(),
        transport,
        Arc::new(NullObserver),
        Arc::new(NullActivities),
        vec![PtpRuntimeValue {
            key: "handle".into(),
            value: handle as u64,
        }],
    ))
    .expect("selected response binds the authored fallback");

    let chunk_size = outcome
        .scope
        .iter()
        .find(|value| value.key == "chunkSize")
        .expect("fallback binds chunkSize");
    assert_eq!(chunk_size.value.as_u64(), Some(0x0020_0000));
    assert_eq!(outcome.steps_run, 3);
}

#[test]
fn selected_object_preparation_prop_fallback_does_not_select_other_failures() {
    let preparation = r#"                    - sendOp: "0x1008"
                      params: [{ runtime: handle }]
                      captures:
                        - { bind: objectReportedSize, as: objectInfoCompressedSize }
                        - { bind: objectTransferSize, as: objectInfoCompressedSize }
                    - if:
                        slot: objectReportedSize
                        equals: 0xffffffff
                        then:
                          - sendOp: "0x9803"
                            params: [{ runtime: handle }, 0xdc04]
                            captures: [{ bind: objectTransferSize, as: u64Le }]"#;
    let handle = 0x45;
    let response_transport = Arc::new(ResponderTxnTransport::new(
        UsbResponder::new()
            .reply_transaction(
                0x1008,
                &[handle],
                UsbTxnReply::ok(Some(object_info_payload(0x0760_0000))),
            )
            .reply_transaction(
                0x1015,
                &[0xd235],
                UsbTxnReply::response(0x2002, vec![], None),
            ),
        &[60_000],
    ));
    let response_error = block_on(run_selected_object_preparation_txn(
        selected_object_store(preparation),
        "usbTether".into(),
        response_transport,
        Arc::new(NullObserver),
        Arc::new(NullActivities),
        vec![PtpRuntimeValue {
            key: "handle".into(),
            value: handle as u64,
        }],
    ))
    .expect_err("an unselected response remains terminal");
    assert!(response_error.to_string().contains("0x2002"));

    let transport_error = block_on(run_selected_object_preparation_txn(
        selected_object_store(preparation),
        "usbTether".into(),
        Arc::new(
            ResponderTxnTransport::new(
                UsbResponder::new().reply_transaction(
                    0x1008,
                    &[handle],
                    UsbTxnReply::ok(Some(object_info_payload(0x0760_0000))),
                ),
                &[60_000],
            )
            .with_failed_execute(0x1015),
        ),
        Arc::new(NullObserver),
        Arc::new(NullActivities),
        vec![PtpRuntimeValue {
            key: "handle".into(),
            value: handle as u64,
        }],
    ))
    .expect_err("a transport error remains terminal");
    assert!(transport_error
        .to_string()
        .contains("scripted transport failure"));
}

#[test]
fn selected_object_preparation_propagates_strict_get_prop_response_error() {
    let store = selected_object_store(
        r#"                    - { getProp: "0xd209" }
                    - sendOp: "0x1008"
                      params: [{ runtime: handle }]
                      captures:
                        - { bind: objectReportedSize, as: objectInfoCompressedSize }
                        - { bind: objectTransferSize, as: objectInfoCompressedSize }
                    - if:
                        slot: objectReportedSize
                        equals: 0xffffffff
                        then:
                          - sendOp: "0x9803"
                            params: [{ runtime: handle }, 0xdc04]
                            captures: [{ bind: objectTransferSize, as: u64Le }]"#,
    );
    let handle = 0x42;
    let responder = UsbResponder::new().reply_transaction(
        0x1015,
        &[0xd209],
        UsbTxnReply::response(0x200a, vec![], None),
    );
    let transport = Arc::new(ResponderTxnTransport::new(responder, &[60_000]));

    let error = block_on(run_selected_object_preparation_txn(
        store,
        "usbTether".into(),
        transport.clone(),
        Arc::new(NullObserver),
        Arc::new(NullActivities),
        vec![PtpRuntimeValue {
            key: "handle".into(),
            value: handle as u64,
        }],
    ))
    .expect_err("strict property rejection remains terminal");

    assert!(error.to_string().contains("0x200a"), "{error}");
    assert_eq!(
        transport.log(),
        vec![UsbEvent::Transaction {
            opcode: 0x1015,
            params: vec![0xd209],
            data_out: None,
            timeout_ms: 10_000,
        }]
    );
}

#[test]
fn selected_object_preparation_propagates_non_ok_response() {
    let store = selected_object_store(
        r#"                    - sendOp: "0x1008"
                      params: [{ runtime: handle }]
                      captures:
                        - { bind: objectReportedSize, as: objectInfoCompressedSize }
                        - { bind: objectTransferSize, as: objectInfoCompressedSize }
                    - if:
                        slot: objectReportedSize
                        equals: 0xffffffff
                        then:
                          - sendOp: "0x9803"
                            params: [{ runtime: handle }, 0xdc04]
                            captures: [{ bind: objectTransferSize, as: u64Le }]"#,
    );
    let handle = 0x42;
    let responder = UsbResponder::new().reply_transaction(
        0x1008,
        &[handle],
        UsbTxnReply::response(0x2002, vec![], None),
    );
    let transport = Arc::new(ResponderTxnTransport::new(responder, &[60_000]));

    let error = block_on(run_selected_object_preparation_txn(
        store,
        "usbTether".into(),
        transport.clone(),
        Arc::new(NullObserver),
        Arc::new(NullActivities),
        vec![PtpRuntimeValue {
            key: "handle".into(),
            value: handle as u64,
        }],
    ))
    .expect_err("ObjectInfo rejection remains terminal");

    assert!(error.to_string().contains("0x2002"), "{error}");
    assert_eq!(
        transport.log(),
        vec![UsbEvent::Transaction {
            opcode: 0x1008,
            params: vec![handle],
            data_out: None,
            timeout_ms: 10_000,
        }]
    );
}

#[test]
fn cancelling_selected_object_preparation_drops_the_pending_transaction() {
    let store = selected_object_store(
        r#"                    - sendOp: "0x1008"
                      params: [{ runtime: handle }]
                      captures:
                        - { bind: objectReportedSize, as: objectInfoCompressedSize }
                        - { bind: objectTransferSize, as: objectInfoCompressedSize }
                    - if:
                        slot: objectReportedSize
                        equals: 0xffffffff
                        then:
                          - sendOp: "0x9803"
                            params: [{ runtime: handle }, 0xdc04]
                            captures: [{ bind: objectTransferSize, as: u64Le }]"#,
    );
    let handle = 0x43;
    let cancelled = Arc::new(AtomicBool::new(false));
    let transport = Arc::new(
        ResponderTxnTransport::new(UsbResponder::new(), &[10_000, 60_000])
            .with_pending_execute(0x1008, Arc::clone(&cancelled)),
    );
    let mut run = Box::pin(run_selected_object_preparation_txn(
        store,
        "usbTether".into(),
        transport.clone(),
        Arc::new(NullObserver),
        Arc::new(NullActivities),
        vec![PtpRuntimeValue {
            key: "handle".into(),
            value: handle as u64,
        }],
    ));
    let waker = futures::task::noop_waker();
    let mut context = Context::from_waker(&waker);

    match run.as_mut().poll(&mut context) {
        Poll::Pending => {}
        Poll::Ready(result) => {
            panic!("selected-object run completed before cancellation: {result:?}")
        }
    }
    assert!(!cancelled.load(Ordering::SeqCst));
    drop(run);

    assert!(
        cancelled.load(Ordering::SeqCst),
        "dropping the run cancels the pending transport future",
    );
    assert_eq!(
        transport.log(),
        vec![UsbEvent::Transaction {
            opcode: 0x1008,
            params: vec![handle],
            data_out: None,
            timeout_ms: 10_000,
        }]
    );
}

#[test]
fn selected_preparation_continues_after_best_effort_event_timeout() {
    let store = selected_object_store(
        r#"                    - sendOp: "0x1008"
                      params: [{ runtime: handle }]
                      captures:
                        - { bind: objectReportedSize, as: objectInfoCompressedSize }
                        - { bind: objectTransferSize, as: objectInfoCompressedSize }
                    - if:
                        slot: objectReportedSize
                        equals: 0xffffffff
                        then:
                          - sendOp: "0x9803"
                            params: [{ runtime: handle }, 0xdc04]
                            captures: [{ bind: objectTransferSize, as: u64Le }]
                    - awaitUntil:
                        source: { event: { code: "0xc005", thenPoll: "0xd209" } }
                        until: { prop: "0xd209", eq: 1 }
                        timeoutMs: 30000
                        intervalMs: 100"#,
    );
    let handle = 0x44;
    let responder = UsbResponder::new()
        .reply_transaction(
            0x1008,
            &[handle],
            UsbTxnReply::ok(Some(object_info_payload(4096))),
        )
        .inject_event(0xc005, &[], true)
        .reply_transaction(
            0x1015,
            &[0xd209],
            UsbTxnReply::ok(Some(1_u16.to_le_bytes().to_vec())),
        )
        .reply_transaction(
            0x1015,
            &[0xd235],
            UsbTxnReply::ok(Some(1024_u32.to_le_bytes().to_vec())),
        );
    let transport = Arc::new(ResponderTxnTransport::new(responder, &[30_000, 60_000]));

    let outcome = block_on(run_selected_object_preparation_txn(
        store,
        "usbTether".into(),
        transport.clone(),
        Arc::new(NullObserver),
        Arc::new(NullActivities),
        vec![PtpRuntimeValue {
            key: "handle".into(),
            value: handle as u64,
        }],
    ))
    .expect("a lost best-effort event does not skip later preparation");

    assert_eq!(outcome.steps_run, 4);
    assert_eq!(
        transport.log(),
        vec![
            UsbEvent::Transaction {
                opcode: 0x1008,
                params: vec![handle],
                data_out: None,
                timeout_ms: 10_000,
            },
            UsbEvent::EventWait { event_code: 0xc005 },
            UsbEvent::Transaction {
                opcode: 0x1015,
                params: vec![0xd209],
                data_out: None,
                timeout_ms: 10_000,
            },
            UsbEvent::Transaction {
                opcode: 0x1015,
                params: vec![0xd235],
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

/// A store whose single `usbTether` connection block is spliced in verbatim,
/// so a test picks the trait fields and plan shapes it exercises.
fn connection_store(connection: &str) -> Arc<ConfigStore> {
    ConfigStore::from_bundle(
        format!(
            r#"schema: camera-config/v1
camera: {{ manufacturer: Test, model: Txn, firmware: "1" }}
properties:
  "0xd209": {{ name: autofocusResult, type: u16, access: readWrite }}
connections:
{connection}
"#
        ),
        None,
    )
    .expect("connection store loads")
}

/// A frame-seam transport that records every call. The ownership guard must
/// fail the run before any I/O, so the recording stays empty.
#[derive(Default)]
struct RecordingFrameTransport {
    calls: Mutex<Vec<&'static str>>,
}

#[async_trait::async_trait]
impl PtpExecutorTransport for RecordingFrameTransport {
    async fn reserve_transaction_id(&self) -> Result<u32, PtpTransportError> {
        self.calls.lock().unwrap().push("reserveTransactionId");
        Ok(0)
    }

    async fn send_command_frame(&self, _frame: Vec<u8>) -> Result<(), PtpTransportError> {
        self.calls.lock().unwrap().push("sendCommandFrame");
        Ok(())
    }

    async fn next_command_frame(&self) -> Result<Vec<u8>, PtpTransportError> {
        self.calls.lock().unwrap().push("nextCommandFrame");
        futures::future::pending().await
    }

    async fn next_event_frame(&self, _event_code: u16) -> Result<Vec<u8>, PtpTransportError> {
        self.calls.lock().unwrap().push("nextEventFrame");
        futures::future::pending().await
    }

    async fn open_channel(&self, _role: SocketRole) -> Result<(), PtpTransportError> {
        self.calls.lock().unwrap().push("openChannel");
        Ok(())
    }

    async fn close_command_channel(
        &self,
        _transport_close_frame: Option<Vec<u8>>,
    ) -> Result<(), PtpTransportError> {
        self.calls.lock().unwrap().push("closeCommandChannel");
        Ok(())
    }

    async fn reopen_command_session(&self) -> Result<PtpSessionOpenResult, PtpTransportError> {
        self.calls.lock().unwrap().push("reopenCommandSession");
        Ok(PtpSessionOpenResult {
            transaction_id: 0,
            response_code: 0x2001,
            response_params: vec![],
        })
    }

    async fn sleep(&self, _ms: u32) -> Result<(), PtpTransportError> {
        Ok(())
    }
}

#[test]
fn daemon_attached_connection_cannot_enter_a_frame_entry_point() {
    let store = connection_store(
        r#"  usbTether:
    kind: usb-passthrough
    session: { ownership: daemonAttached }
    events: { delivery: bestEffort }
    entries:
      - to: shooting
        steps:
          - { closeSession: {} }"#,
    );
    let transport = Arc::new(RecordingFrameTransport::default());

    let error = block_on(run_mode_entry(
        store,
        "usbTether".into(),
        None,
        "shooting".into(),
        transport.clone(),
        Arc::new(NullObserver),
        Arc::new(NullActivities),
        vec![],
    ))
    .expect_err("a daemonAttached connection cannot walk the frame seam");
    let message = error.to_string();
    assert!(
        message.contains("usbTether"),
        "names the connection, got: {message}",
    );
    assert!(
        message.contains("daemonAttached"),
        "names the declared ownership, got: {message}",
    );
    assert!(
        transport.calls.lock().unwrap().is_empty(),
        "no OpenSession/CloseSession or channel call reached the transport",
    );
}

#[test]
fn initiator_owned_connection_cannot_enter_a_transaction_entry_point() {
    let store = connection_store(
        r#"  usbTether:
    kind: usb
    session: { ownership: initiatorOwned }
    events: { delivery: reliable }
    actions:
      autofocusLock:
        mode: ""
        initiator:
          steps:
            - getProp: "0xd209""#,
    );
    let transport = Arc::new(ResponderTxnTransport::new(UsbResponder::new(), &[]));

    let error = block_on(run_initiator_action_txn(
        store.clone(),
        action_request(&store),
        transport.clone(),
        Arc::new(NullObserver),
        Arc::new(NullActivities),
    ))
    .expect_err("an initiatorOwned connection cannot walk the transaction seam");
    let message = error.to_string();
    assert!(
        message.contains("usbTether"),
        "names the connection, got: {message}",
    );
    assert!(
        message.contains("initiatorOwned"),
        "names the declared ownership, got: {message}",
    );
    assert!(
        transport.log().is_empty(),
        "no transaction reached the daemon seam",
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
