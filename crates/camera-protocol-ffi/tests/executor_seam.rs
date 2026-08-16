//! Issue #246 acceptance: the REAL Fuji establishment plans from
//! `packages/camera-config-data/fuji/index.yaml`, walked by the Rust executor
//! through the foreign-transport seam (`run_establishment`) against the same
//! in-memory responder the reference walker's tests use. The responder's
//! interaction log must match the reference app wire order the `camera-sim`
//! `ble_pair_roundtrip` / `ble_establish_wifi_ap` tests assert — proving the
//! executor and the reference walker agree on per-verb semantics while the
//! executor adds the retry/deadline/telemetry layer.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use camera_config::index::{CccdMode, FamilyBleBlock, ModelView, ResolvedManufacturerIndex};
use camera_protocol_ffi::{
    run_establishment, run_post_exit_readiness, BleManufacturerData, ConfigStore,
    ConnectionActivityEvent, ConnectionActivityFailure, ConnectionActivityObserver,
    ConnectionActivityRetry, ConnectionActivityTerminalSummary, EstablishmentConfirmOutcome,
    EstablishmentRefinement, ExecutorError, ExecutorStepFailureKind, KeyValue, Recognition,
    ReconnectDecision, ScanObservation, StepObserver, StepOutcome, StepReport, TransportError,
};
use camera_sim::{BleEvent, BleResponder};
use futures::executor::block_on;
use protocol_primitives::{NikonLssAuthenticationSelection, NikonLssClient, NikonLssServer};

mod common;

fn data(rel: &str) -> String {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../packages/camera-config-data")
        .join(rel);
    std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("read {}: {e}", p.display()))
}

fn store() -> Arc<ConfigStore> {
    common::real_fuji_store()
}

fn poll_timeout_store() -> Arc<ConfigStore> {
    let index = r#"
manufacturer: TESTCO
families:
  test:
    ble:
      gatt: { state: "00002A25-0000-1000-8000-00805F9B34FB" }
      advert: {}
      establishments:
        poll:
          mechanism: poll
          activities:
            - id: camera.test.poll
              version: 1
              displayRole: waitingForCamera
              defaultExpectedDurationMs: 5
              interactionRequired: false
              executorSpan: { sequence: postExitReadiness, startStep: 0, endStepExclusive: 3 }
          postExitReadiness:
            - bleConnect: {}
            - bleDiscoverServices: {}
            - bleAwaitUntil:
                source: { read: state }
                capture: { at: 0, length: 1, encoding: u8, name: state }
                until: { state: { eq: "1" } }
                intervalMs: 1
                timeoutMs: 5
models:
  - id: tm1
    displayName: "Test One"
    inherits: [test]
    manifest: tm1.yaml
"#;
    let body = r#"
schema: camera-config/v1
camera:
  manufacturer: TESTCO
  model: TM1
connections:
  ble:
    kind: ble
    establishment: poll
"#;
    ConfigStore::from_manufacturer_index(
        index.into(),
        vec![KeyValue {
            key: "tm1".into(),
            value: body.into(),
        }],
    )
    .expect("poll-timeout fixture loads")
}

fn nikon_lss_store() -> Arc<ConfigStore> {
    let index = r#"
manufacturer: NIKON
families:
  lss:
    ble:
      gatt:
        auth: "00002000-3DD4-4255-8D62-6DC7B9BD5561"
        config: "00002004-3DD4-4255-8D62-6DC7B9BD5561"
      establishments:
        pair:
          mechanism: pair
          params: [clientDeviceId, clientNonce]
          activities:
            - { id: camera.test.lss, version: 1, displayRole: connecting, defaultExpectedDurationMs: 1, interactionRequired: false, executorSpan: { sequence: steps, startStep: 0, endStepExclusive: 4 } }
          steps:
            - bleConnect: {}
            - bleDiscoverServices: {}
            - nikonLssAuthenticate:
                gatt: auth
                clientDeviceId: { runtime: clientDeviceId, encoding: bytes-raw }
                nonce: { runtime: clientNonce, encoding: bytes-raw }
                timeoutMs: 1000
            - nikonLssReadConnectionConfiguration:
                gatt: config
                flagsCaptureAs: flags
                ssidCaptureAs: ssid
                passwordCaptureAs: password
                securityModeCaptureAs: security
                sppMaxLengthCaptureAs: sppMaximumLength
models:
  - id: d850
    displayName: "D850"
    inherits: [lss]
    manifest: d850.yaml
"#;
    let body = r#"
schema: camera-config/v1
camera:
  manufacturer: NIKON
  model: D850
connections:
  ble:
    kind: ble
    establishment: pair
"#;
    ConfigStore::from_manufacturer_index(
        index.into(),
        vec![KeyValue {
            key: "d850".into(),
            value: body.into(),
        }],
    )
    .expect("Nikon LSS fixture loads")
}

struct NikonLssScript {
    stage1: Vec<u8>,
    stage2: Vec<u8>,
    stage3: Vec<u8>,
    stage4: Vec<u8>,
    configuration: Vec<u8>,
}

fn nikon_lss_script() -> NikonLssScript {
    let device_id = [0x10; 8];
    let client_nonce = [0x20; 8];
    let mut client = NikonLssClient::new(device_id, client_nonce);
    let stage1 = client.stage1_record().unwrap();
    let selection = NikonLssAuthenticationSelection::new(3).unwrap();
    let mut server = NikonLssServer::new(selection, [0x30; 8], [0x40; 8]);
    let stage2 = server.handle_stage1(&stage1).unwrap();
    let stage3 = client.handle_stage2(&stage2).unwrap();
    let (stage4, session) = server.finish_stage3(&stage3).unwrap();

    let mut ssid = b"D850_TEST_AP".to_vec();
    ssid.resize(32, 0);
    let mut password = b"snapbridge-password".to_vec();
    password.resize(64, 0);
    let mut config = vec![0x03];
    config.extend(session.encrypt(&ssid).unwrap());
    config.extend(session.encrypt(&password).unwrap());
    config.push(1); // wpa2
    config.extend(512_u32.to_le_bytes());
    NikonLssScript {
        stage1: stage1.to_vec(),
        stage2: stage2.to_vec(),
        stage3: stage3.to_vec(),
        stage4: stage4.to_vec(),
        configuration: config,
    }
}

fn gfx100ii() -> ModelView {
    ResolvedManufacturerIndex::from_yaml(&data("fuji/index.yaml"))
        .expect("fuji/index.yaml loads")
        .models
        .into_iter()
        .find(|m| m.id == "gfx100ii")
        .expect("gfx100ii present")
}

fn uuid(ble: &FamilyBleBlock, name: &str) -> String {
    ble.gatt
        .get(name)
        .unwrap_or_else(|| panic!("gatt name {name} in catalog"))
        .clone()
}

// ---------------------------------------------------------------------------
// The foreign-transport seam, implemented over the in-memory responder — what
// a platform app does over CoreBluetooth/Android BLE, minus the radio.
// ---------------------------------------------------------------------------

struct ResponderTransport {
    responder: Mutex<BleResponder>,
    sleep_log: Arc<Mutex<Vec<u32>>>,
}

impl ResponderTransport {
    fn new(responder: BleResponder) -> Self {
        ResponderTransport {
            responder: Mutex::new(responder),
            sleep_log: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn into_log(self) -> Vec<BleEvent> {
        self.responder.into_inner().unwrap().log().to_vec()
    }
}

fn transport_err(e: camera_sim::ble::BleError) -> TransportError {
    TransportError::Failed {
        detail: e.to_string(),
    }
}

#[async_trait::async_trait]
impl camera_protocol_ffi::BleExecutorTransport for ResponderTransport {
    async fn connect(&self) -> Result<(), TransportError> {
        self.responder.lock().unwrap().connect();
        Ok(())
    }
    async fn await_disconnect(&self) -> Result<(), TransportError> {
        self.responder
            .lock()
            .unwrap()
            .await_disconnect()
            .map_err(transport_err)
    }
    async fn request_mtu(&self, mtu: u16) -> Result<u16, TransportError> {
        self.responder
            .lock()
            .unwrap()
            .request_mtu(mtu)
            .map_err(transport_err)
    }
    async fn ensure_services_discovered(&self) -> Result<(), TransportError> {
        self.responder
            .lock()
            .unwrap()
            .discover_services()
            .map_err(transport_err)
    }
    async fn read(&self, characteristic: String) -> Result<Vec<u8>, TransportError> {
        self.responder
            .lock()
            .unwrap()
            .read(&characteristic)
            .map_err(transport_err)
    }
    async fn peripheral_name(&self) -> Result<String, TransportError> {
        self.responder
            .lock()
            .unwrap()
            .peripheral_name()
            .map_err(transport_err)
    }
    async fn write(&self, characteristic: String, value: Vec<u8>) -> Result<(), TransportError> {
        self.responder
            .lock()
            .unwrap()
            .write(&characteristic, &value)
            .map_err(transport_err)
    }
    async fn write_with_notification_fence(
        &self,
        characteristic: String,
        value: Vec<u8>,
        notification_characteristic: String,
    ) -> Result<(), TransportError> {
        self.responder
            .lock()
            .unwrap()
            .write_with_notification_fence(&characteristic, &value, &notification_characteristic)
            .map_err(transport_err)
    }
    async fn subscribe(
        &self,
        characteristic: String,
        mode: camera_protocol_ffi::CccdMode,
    ) -> Result<(), TransportError> {
        let mode = match mode {
            camera_protocol_ffi::CccdMode::Notify => CccdMode::Notify,
            camera_protocol_ffi::CccdMode::Indicate => CccdMode::Indicate,
        };
        self.responder
            .lock()
            .unwrap()
            .subscribe(&characteristic, mode)
            .map_err(transport_err)
    }
    async fn next_notification(&self, characteristic: String) -> Result<Vec<u8>, TransportError> {
        let payload = self
            .responder
            .lock()
            .unwrap()
            .take_notification(&characteristic);
        match payload {
            Some(p) => Ok(p),
            // Queue exhausted: park forever, like a camera that never
            // notifies — the executor's budget decides.
            None => std::future::pending().await,
        }
    }
    async fn sleep(&self, ms: u32) -> Result<(), TransportError> {
        self.sleep_log.lock().unwrap().push(ms);
        Ok(())
    }
}

/// Clock seam transport: optionally stalls connect, serves one read, then
/// parks. Its clock can either elapse normally or fail as a foreign transport.
struct ClockTestTransport {
    first_read: Mutex<Option<Vec<u8>>>,
    clock_fails: bool,
    stall_connect: bool,
}

#[async_trait::async_trait]
impl camera_protocol_ffi::BleExecutorTransport for ClockTestTransport {
    async fn connect(&self) -> Result<(), TransportError> {
        if self.stall_connect {
            std::future::pending().await
        } else {
            Ok(())
        }
    }
    async fn await_disconnect(&self) -> Result<(), TransportError> {
        std::future::pending().await
    }
    async fn request_mtu(&self, mtu: u16) -> Result<u16, TransportError> {
        Ok(mtu)
    }
    async fn ensure_services_discovered(&self) -> Result<(), TransportError> {
        Ok(())
    }
    async fn read(&self, _characteristic: String) -> Result<Vec<u8>, TransportError> {
        let first = self.first_read.lock().unwrap().take();
        match first {
            Some(value) => Ok(value),
            None => std::future::pending().await,
        }
    }
    async fn peripheral_name(&self) -> Result<String, TransportError> {
        Ok("CLOCK-TEST".to_string())
    }
    async fn write(&self, _characteristic: String, _value: Vec<u8>) -> Result<(), TransportError> {
        Ok(())
    }
    async fn write_with_notification_fence(
        &self,
        _characteristic: String,
        _value: Vec<u8>,
        _notification_characteristic: String,
    ) -> Result<(), TransportError> {
        Ok(())
    }
    async fn subscribe(
        &self,
        _characteristic: String,
        _mode: camera_protocol_ffi::CccdMode,
    ) -> Result<(), TransportError> {
        Ok(())
    }
    async fn next_notification(&self, _characteristic: String) -> Result<Vec<u8>, TransportError> {
        std::future::pending().await
    }
    async fn sleep(&self, _ms: u32) -> Result<(), TransportError> {
        if self.clock_fails {
            Err(TransportError::Failed {
                detail: "clock unavailable".into(),
            })
        } else {
            Ok(())
        }
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

fn no_retry_summary() -> ConnectionActivityTerminalSummary {
    ConnectionActivityTerminalSummary {
        retry_count: 0,
        last_retry: None,
    }
}

fn launch_refusal_failure() -> ConnectionActivityFailure {
    ConnectionActivityFailure {
        kind: ExecutorStepFailureKind::ConditionRejected,
        context: vec![
            KeyValue {
                key: "apState".into(),
                value: "32768".into(),
            },
            KeyValue {
                key: "stateErrorDetails".into(),
                value: "2".into(),
            },
        ],
    }
}

fn launch_refusal_retry() -> ConnectionActivityRetry {
    ConnectionActivityRetry {
        ordinal: 2,
        limit: 2,
        failure: launch_refusal_failure(),
    }
}

/// Recognize through the FFI exactly as the app does, returning the plan
/// handle + the runtime scope and its capture encodings — the triple the app
/// threads verbatim into `run_establishment` (#43).
fn recognize(
    store: &Arc<ConfigStore>,
    advert: ScanObservation,
) -> (String, Vec<KeyValue>, Vec<KeyValue>) {
    match store.recognize(advert) {
        Recognition::Candidate {
            model,
            connection,
            runtime_scope,
            runtime_scope_encodings,
            ..
        } => (
            format!("{model}:{connection}"),
            runtime_scope,
            runtime_scope_encodings,
        ),
        other => panic!("expected Candidate, got {other:?}"),
    }
}

fn legacy_advert(ble: &FamilyBleBlock) -> ScanObservation {
    ScanObservation::BleAdvert {
        service_uuids: vec![ble.advert.service_uuids["fileTransfer"].clone()],
        manufacturer_data: Some(BleManufacturerData {
            company_id: ble.advert.manufacturer_company_id.expect("company id"),
            payload: vec![0x02, 0x44, 0x73, 0x2a, 0x80],
        }),
        service_data: vec![],
        local_name: None,
        tx_power: None,
        ad_records: vec![],
    }
}

fn red_advert(ble: &FamilyBleBlock) -> ScanObservation {
    // No service UUIDs: a fresh RED pairing advert matches `bleRedAdvert` on
    // manufacturer data alone. The RED service UUIDs now select the
    // reconnect-only startup/awake signatures (discoverable: false), which
    // never surface through `recognize`.
    ScanObservation::BleAdvert {
        service_uuids: vec![],
        manufacturer_data: Some(BleManufacturerData {
            company_id: ble.advert.manufacturer_company_id.expect("company id"),
            payload: vec![0x01, b'A', b'B', b'C', b'D', b'E'],
        }),
        service_data: vec![],
        local_name: None,
        tx_power: None,
        ad_records: vec![],
    }
}

fn legacy_startup_advert(ble: &FamilyBleBlock) -> ScanObservation {
    ScanObservation::BleAdvert {
        service_uuids: vec![ble.advert.service_uuids["cameraStartupInformation"].clone()],
        manufacturer_data: Some(BleManufacturerData {
            company_id: ble.advert.manufacturer_company_id.expect("company id"),
            payload: vec![0x02, 0x44, 0x73, 0x2a, 0x80, 0x00],
        }),
        service_data: vec![],
        local_name: Some("0C3EGFX100II-0C3E".into()),
        tx_power: None,
        ad_records: vec![],
    }
}

fn legacy_awake_advert(ble: &FamilyBleBlock) -> ScanObservation {
    ScanObservation::BleAdvert {
        service_uuids: vec![ble.advert.service_uuids["fileTransfer"].clone()],
        manufacturer_data: None,
        service_data: vec![],
        local_name: Some("0C3EGFX100II-0C3E".into()),
        tx_power: None,
        ad_records: vec![],
    }
}

fn persisted_legacy_scope() -> Vec<KeyValue> {
    vec![
        KeyValue {
            key: "pairingKeyBytes".into(),
            value: "44732a80".into(),
        },
        KeyValue {
            key: "style".into(),
            value: "legacy".into(),
        },
        KeyValue {
            key: "shortSerial".into(),
            value: "0C3E".into(),
        },
    ]
}

fn persisted_legacy_encodings() -> Vec<KeyValue> {
    vec![KeyValue {
        key: "pairingKeyBytes".into(),
        value: "bytes-le".into(),
    }]
}

fn responder_for(ble: &FamilyBleBlock) -> BleResponder {
    BleResponder::new(ble.gatt.values().cloned())
        .serve_read(&uuid(ble, "protectedSerialString"), b"FF123456")
        .serve_read(&uuid(ble, "transferState"), &[0x00])
}

fn runtime_params() -> Vec<KeyValue> {
    vec![KeyValue {
        key: "terminalName".to_string(),
        value: "iphone".to_string(),
    }]
}

/// The CCCD rounds in reference app order, by symbolic name (mirrors the reference
/// walker's acceptance test).
const FIRST_ROUND: [&str; 4] = [
    "apState",
    "transferState",
    "dateSyncState",
    "locationSyncState",
];
const SECOND_ROUND: [&str; 9] = [
    "remoteBootSetting",
    "cameraSSIDNameString",
    "loggingSetting",
    "cameraVitalState",
    "locationSyncSetting",
    "imageResizeRate",
    "imageResizeSetting",
    "iptcSetting",
    "locationSyncCycle",
];

fn assert_app_order(ble: &FamilyBleBlock, log: &[BleEvent], red_exchange: bool) {
    let mut expected: Vec<BleEvent> = vec![
        BleEvent::Connect,
        BleEvent::DiscoverServices,
        BleEvent::Read {
            uuid: uuid(ble, "protectedSerialString"),
        },
        BleEvent::Write {
            uuid: uuid(ble, "pairingKey"),
            value: if red_exchange {
                b"ABCDE".to_vec()
            } else {
                vec![0x44, 0x73, 0x2a, 0x80]
            },
        },
        BleEvent::Write {
            uuid: uuid(ble, "deviceNameString"),
            value: b"iphone".to_vec(),
        },
    ];
    if red_exchange {
        expected.push(BleEvent::Read {
            uuid: uuid(ble, "deviceIdentificationNumber"),
        });
        expected.push(BleEvent::Write {
            uuid: uuid(ble, "deviceIdentificationNumber"),
            value: 0x3234_5678u32.to_le_bytes().to_vec(),
        });
    }
    for name in FIRST_ROUND {
        expected.push(BleEvent::Subscribe {
            uuid: uuid(ble, name),
            mode: CccdMode::Notify,
        });
    }
    expected.push(BleEvent::Read {
        uuid: uuid(ble, "transferState"),
    });
    for name in SECOND_ROUND {
        expected.push(BleEvent::Subscribe {
            uuid: uuid(ble, name),
            mode: CccdMode::Notify,
        });
    }
    assert_eq!(
        log, expected,
        "responder log must match the reference app wire order"
    );
}

fn scope_get<'a>(scope: &'a [KeyValue], key: &str) -> Option<&'a str> {
    scope
        .iter()
        .find(|kv| kv.key == key)
        .map(|kv| kv.value.as_str())
}

// ---------------------------------------------------------------------------
// Saved reconnect handles (mechanism-backed plan selectors)
// ---------------------------------------------------------------------------

#[test]
fn wake_decision_handle_runs_and_refines() {
    let store = store();
    let view = gfx100ii();
    let ble = view.ble.as_ref().unwrap();
    let persisted = persisted_legacy_scope();
    let ReconnectDecision::Wake { plan, .. } = store.reconnect_decision(
        "gfx100ii".into(),
        legacy_startup_advert(ble),
        persisted.clone(),
    ) else {
        panic!("startup advert must select Wake");
    };
    assert_eq!(plan.plan_handle, "gfx100ii:ble-wake");

    let responder = BleResponder::new(ble.gatt.values().cloned()).queue_peer_disconnect();
    let transport = Arc::new(ResponderTransport::new(responder));
    let outcome = block_on(run_establishment(
        store.clone(),
        plan.plan_handle.clone(),
        transport.clone(),
        Arc::new(Recorder::default()),
        Arc::new(ActivityRecorder::default()),
        persisted.clone(),
        persisted_legacy_encodings(),
        vec![],
    ))
    .expect("the mechanism-backed wake handle resolves");
    assert_eq!(outcome.steps_run, 2);
    assert_eq!(
        outcome.summary.confirm_outcome,
        EstablishmentConfirmOutcome::NotDeclared
    );

    let transport = Arc::try_unwrap(transport).unwrap_or_else(|_| panic!("sole owner"));
    assert_eq!(
        transport.into_log(),
        vec![BleEvent::Connect, BleEvent::PeerDisconnect]
    );
    assert!(matches!(
        store.refine_establishment(plan.plan_handle, "2.30".into(), persisted, 2),
        Ok(EstablishmentRefinement::NoChange)
    ));
}

#[test]
fn ready_decision_handle_runs_and_refines() {
    let store = store();
    let view = gfx100ii();
    let ble = view.ble.as_ref().unwrap();
    let persisted = persisted_legacy_scope();
    let ReconnectDecision::Ready { plan, .. } = store.reconnect_decision(
        "gfx100ii".into(),
        legacy_awake_advert(ble),
        persisted.clone(),
    ) else {
        panic!("awake advert must select Ready");
    };
    assert_eq!(plan.plan_handle, "gfx100ii:ble-reconnect");

    let transport = Arc::new(ResponderTransport::new(responder_for(ble)));
    let outcome = block_on(run_establishment(
        store.clone(),
        plan.plan_handle.clone(),
        transport.clone(),
        Arc::new(Recorder::default()),
        Arc::new(ActivityRecorder::default()),
        persisted.clone(),
        persisted_legacy_encodings(),
        runtime_params(),
    ))
    .expect("the mechanism-backed reconnect handle resolves");
    assert!(outcome.steps_run > 0);

    let transport = Arc::try_unwrap(transport).unwrap_or_else(|_| panic!("sole owner"));
    let log = transport.into_log();
    assert!(matches!(log.first(), Some(BleEvent::Connect)));
    assert!(!log.iter().any(|event| {
        matches!(event, BleEvent::Read { uuid: read } if read == &uuid(ble, "protectedSerialString"))
    }));
    assert!(matches!(
        store.refine_establishment(plan.plan_handle, "2.30".into(), persisted, 0),
        Ok(EstablishmentRefinement::NoChange)
    ));
}

#[test]
fn unknown_plan_handles_fail_before_io() {
    let store = store();
    let view = gfx100ii();
    let ble = view.ble.as_ref().unwrap();

    for handle in ["gfx100ii:missing", "gfx100ii:usb"] {
        let transport = Arc::new(ResponderTransport::new(responder_for(ble)));
        let error = block_on(run_establishment(
            store.clone(),
            handle.into(),
            transport.clone(),
            Arc::new(Recorder::default()),
            Arc::new(ActivityRecorder::default()),
            vec![],
            vec![],
            vec![],
        ))
        .expect_err("unknown handles fail loud");
        assert!(matches!(error, ExecutorError::UnknownPlan { .. }));
        let transport = Arc::try_unwrap(transport).unwrap_or_else(|_| panic!("sole owner"));
        assert!(transport.into_log().is_empty(), "resolver performs no I/O");
    }
}

// ---------------------------------------------------------------------------
// ble-pair
// ---------------------------------------------------------------------------

#[test]
fn legacy_pair_plan_round_trips_through_the_executor() {
    let store = store();
    let view = gfx100ii();
    let ble = view.ble.as_ref().unwrap();
    let (handle, scope, encodings) = recognize(&store, legacy_advert(ble));
    assert_eq!(handle, "gfx100ii:ble");
    assert_eq!(scope_get(&scope, "style"), Some("legacy"));
    assert_eq!(scope_get(&encodings, "pairingKeyBytes"), Some("bytes-le"));

    let transport = Arc::new(ResponderTransport::new(responder_for(ble)));
    let recorder = Arc::new(Recorder::default());
    let activities = Arc::new(ActivityRecorder::default());
    let outcome = block_on(run_establishment(
        store,
        handle,
        transport.clone(),
        recorder.clone(),
        activities.clone(),
        scope,
        encodings,
        runtime_params(),
    ))
    .expect("every step of the legacy plan completes");

    let reports = recorder.0.lock().unwrap();
    assert!(
        reports.iter().all(|r| r.outcome != StepOutcome::Failed),
        "no step failed"
    );
    let started = reports
        .iter()
        .filter(|r| r.outcome == StepOutcome::Started)
        .count();
    assert_eq!(
        started as u32, outcome.steps_run,
        "one Started per dispatched step"
    );
    assert!(reports
        .iter()
        .all(|report| { report.activity_id.is_some() && report.activity_version == Some(1) }));
    drop(reports);

    assert_eq!(
        *activities.0.lock().unwrap(),
        vec![
            ConnectionActivityEvent::Started {
                id: "camera.link.connect".into(),
                version: 1,
            },
            ConnectionActivityEvent::Succeeded {
                id: "camera.link.connect".into(),
                version: 1,
                summary: no_retry_summary(),
            },
            ConnectionActivityEvent::Started {
                id: "camera.pair.confirm".into(),
                version: 1,
            },
            ConnectionActivityEvent::Succeeded {
                id: "camera.pair.confirm".into(),
                version: 1,
                summary: no_retry_summary(),
            },
            ConnectionActivityEvent::Started {
                id: "camera.pair.configure".into(),
                version: 1,
            },
            ConnectionActivityEvent::Succeeded {
                id: "camera.pair.configure".into(),
                version: 1,
                summary: no_retry_summary(),
            },
        ]
    );

    assert_eq!(
        scope_get(&outcome.scope, "cameraSerial"),
        Some("4646313233343536"), // hex of b"FF123456" (encoding: bytes)
    );
    assert_eq!(scope_get(&outcome.scope, "transferState"), Some("00"));
    assert!(scope_get(&outcome.scope, "idNumber").is_none());
    assert_eq!(
        outcome.summary.confirm_outcome,
        EstablishmentConfirmOutcome::Satisfied
    );
    assert_eq!(outcome.summary.tolerated_step_count, 0);
    assert!(outcome.summary.tolerated_step_paths.is_empty());

    let transport = Arc::try_unwrap(transport).unwrap_or_else(|_| panic!("sole owner"));
    assert_app_order(ble, &transport.into_log(), false);
}

#[test]
fn withheld_registration_anchor_completes_with_unsatisfied_ffi_summary() {
    let store = store();
    let view = gfx100ii();
    let ble = view.ble.as_ref().unwrap();
    let (handle, scope, encodings) = recognize(&store, legacy_advert(ble));
    let transfer_state = uuid(ble, "transferState");
    let catalog = ble
        .gatt
        .values()
        .filter(|candidate| candidate.as_str() != transfer_state)
        .cloned();
    let responder =
        BleResponder::new(catalog).serve_read(&uuid(ble, "protectedSerialString"), b"FF123456");
    let transport = Arc::new(ResponderTransport::new(responder));
    let recorder = Arc::new(Recorder::default());

    let outcome = block_on(run_establishment(
        store,
        handle,
        transport,
        recorder.clone(),
        Arc::new(ActivityRecorder::default()),
        scope,
        encodings,
        runtime_params(),
    ))
    .expect("the tolerant anchor failure does not abort establishment");

    assert_eq!(
        outcome.summary.confirm_outcome,
        EstablishmentConfirmOutcome::Unsatisfied
    );
    assert_eq!(outcome.summary.tolerated_step_count, 2);
    assert_eq!(
        outcome.summary.tolerated_step_paths,
        vec!["steps[7].bleSubscribe", "steps[10].bleRead"]
    );
    let tolerated_paths: Vec<String> = recorder
        .0
        .lock()
        .unwrap()
        .iter()
        .filter(|report| report.outcome == StepOutcome::Tolerated)
        .map(|report| report.step_path.clone())
        .collect();
    assert_eq!(outcome.summary.tolerated_step_paths, tolerated_paths);
}

#[test]
fn red_pair_plan_round_trips_with_id_number_echo() {
    let store = store();
    let view = gfx100ii();
    let ble = view.ble.as_ref().unwrap();
    let (handle, scope, encodings) = recognize(&store, red_advert(ble));
    assert_eq!(scope_get(&scope, "style"), Some("red"));
    assert_eq!(scope_get(&scope, "shortSerial"), Some("ABCDE"));
    assert_eq!(scope_get(&encodings, "pairingKeyBytes"), Some("ascii"));

    let responder = responder_for(ble).serve_read(
        &uuid(ble, "deviceIdentificationNumber"),
        &0x1234_5678u32.to_le_bytes(),
    );
    let transport = Arc::new(ResponderTransport::new(responder));
    let recorder = Arc::new(Recorder::default());
    let activities = Arc::new(ActivityRecorder::default());
    let outcome = block_on(run_establishment(
        store,
        handle,
        transport.clone(),
        recorder.clone(),
        activities.clone(),
        scope,
        encodings,
        runtime_params(),
    ))
    .expect("every step of the RED plan completes");

    // The idNumber echo (read u32, | 0x20000000, write back LE) proves the
    // executor rebuilt the advert-capture encodings itself: `shortSerial`
    // (ascii) wrote back as its bytes, `idNumber` (u32) re-encoded LE.
    assert_eq!(scope_get(&outcome.scope, "idNumber"), Some("305419896"));
    assert!(
        recorder.0.lock().unwrap().iter().any(|report| {
            report.step_path.contains(".if.then[")
                && report.activity_id.as_deref() == Some("camera.pair.configure")
        }),
        "nested steps inherit their top-level activity"
    );

    let transport = Arc::try_unwrap(transport).unwrap_or_else(|_| panic!("sole owner"));
    assert_app_order(ble, &transport.into_log(), true);
}

#[test]
fn missing_protected_serial_read_is_tolerated_not_fatal() {
    let store = store();
    let view = gfx100ii();
    let ble = view.ble.as_ref().unwrap();
    let (handle, scope, encodings) = recognize(&store, legacy_advert(ble));

    // Catalogued but unserved: the bond-trigger read fails like a body that
    // doesn't expose it, and the plan's `tolerant: true` swallows it.
    let responder = BleResponder::new(ble.gatt.values().cloned())
        .serve_read(&uuid(ble, "transferState"), &[0x00]);
    let transport = Arc::new(ResponderTransport::new(responder));
    let recorder = Arc::new(Recorder::default());
    let activities = Arc::new(ActivityRecorder::default());
    let outcome = block_on(run_establishment(
        store,
        handle,
        transport,
        recorder.clone(),
        activities.clone(),
        scope,
        encodings,
        runtime_params(),
    ))
    .expect("the tolerated read does not abort the walk");

    assert!(scope_get(&outcome.scope, "cameraSerial").is_none());
    let reports = recorder.0.lock().unwrap();
    let tolerated: Vec<&StepReport> = reports
        .iter()
        .filter(|r| r.outcome == StepOutcome::Tolerated)
        .collect();
    assert_eq!(
        tolerated.len(),
        1,
        "exactly the bond-trigger read tolerated"
    );
    assert_eq!(tolerated[0].verb, "bleRead");
    assert!(tolerated[0].error.is_some());
    let activity_events = activities.0.lock().unwrap();
    assert!(activity_events.iter().all(|event| !matches!(
        event,
        ConnectionActivityEvent::Failed { .. } | ConnectionActivityEvent::Cancelled { .. }
    )));
    assert!(
        activity_events.iter().any(|event| matches!(
            event,
            ConnectionActivityEvent::Succeeded { id, .. } if id == "camera.pair.confirm"
        )),
        "a tolerated raw failure keeps the activity alive through success"
    );
}

// ---------------------------------------------------------------------------
// ble-establish-wifi-ap (preflight read + notification-only await)
// ---------------------------------------------------------------------------

#[test]
fn wifi_ap_plan_awaits_launch_and_binds_credentials() {
    let store = store();
    let view = gfx100ii();
    let ble = view.ble.as_ref().unwrap();

    let launch = uuid(ble, "functionLaunchRequest");
    let ap_state = uuid(ble, "apState");
    let details = uuid(ble, "stateErrorDetails");
    let ssid_uuid = uuid(ble, "cameraSSIDNameString");
    let pass_uuid = uuid(ble, "cameraWiFiPassphraseString");

    let responder = BleResponder::new([
        launch.clone(),
        ap_state.clone(),
        details.clone(),
        ssid_uuid.clone(),
        pass_uuid.clone(),
    ])
    // The pre-transition NotLaunched baseline is read before the command.
    // Rejection-shaped and Launching intermediate states remain eligible to
    // transition to terminal Launched within the same await budget.
    .serve_read(&ap_state, &[0x00, 0x80])
    .serve_read(&details, &[0x00, 0x00])
    .queue_notification_after_fenced_write(&ap_state, &launch, 1, &[0x00, 0x80])
    .queue_notification_after_fenced_write(&ap_state, &launch, 1, &[0x02, 0x80])
    .queue_notification_after_fenced_write(&ap_state, &launch, 1, &[0x01, 0x80])
    .serve_read(&ssid_uuid, b"GFX100II-1234")
    .serve_read(&pass_uuid, b"12345678");

    let transport = Arc::new(ResponderTransport::new(responder));
    let recorder = Arc::new(Recorder::default());
    let activities = Arc::new(ActivityRecorder::default());
    let outcome = block_on(run_establishment(
        store,
        "gfx100ii:app".to_string(),
        transport.clone(),
        recorder,
        activities.clone(),
        vec![],
        vec![],
        // 0x0004 RemoteShooting, bound as the u16-le launch value.
        vec![KeyValue {
            key: "launchMode".to_string(),
            value: "4".to_string(),
        }],
    ))
    .expect("the establish-wifi-ap plan walks to completion");

    assert_eq!(scope_get(&outcome.scope, "ssid"), Some("GFX100II-1234"));
    assert_eq!(scope_get(&outcome.scope, "passphrase"), Some("12345678"));

    let transport = Arc::try_unwrap(transport).unwrap_or_else(|_| panic!("sole owner"));
    let log = transport.into_log();
    let launch_writes: Vec<&BleEvent> = log
        .iter()
        .filter(|e| matches!(e, BleEvent::Write { uuid, .. } if *uuid == launch))
        .collect();
    assert_eq!(
        launch_writes.len(),
        1,
        "launch request written exactly once"
    );
    let preflight_read = log
        .iter()
        .position(|event| matches!(event, BleEvent::Read { uuid } if *uuid == ap_state))
        .expect("pre-command AP-state read");
    let subscription = log
        .iter()
        .position(|event| matches!(event, BleEvent::Subscribe { uuid, .. } if *uuid == ap_state))
        .expect("AP-state subscription");
    let launch_write = log
        .iter()
        .position(|event| matches!(event, BleEvent::Write { uuid, .. } if *uuid == launch))
        .expect("function-launch write");
    assert!(preflight_read < subscription && subscription < launch_write);
    assert!(activities.0.lock().unwrap().iter().all(|event| !matches!(
        event,
        ConnectionActivityEvent::Retrying { id, .. } if id == "camera.ap.launch"
    )));
}

#[test]
fn wifi_ap_fence_discards_stale_notification_before_first_attempt() {
    let store = store();
    let view = gfx100ii();
    let ble = view.ble.as_ref().unwrap();
    let launch = uuid(ble, "functionLaunchRequest");
    let image_setting = uuid(ble, "imageTransferSetting");
    let ap_state = uuid(ble, "apState");
    let ssid = uuid(ble, "cameraSSIDNameString");
    let passphrase = uuid(ble, "cameraWiFiPassphraseString");
    let responder = BleResponder::new([
        launch.clone(),
        image_setting,
        ap_state.clone(),
        ssid.clone(),
        passphrase.clone(),
    ])
    .serve_read(&ap_state, &[0x00, 0x80])
    .queue_notification(&ap_state, &[0x00, 0x80])
    .queue_notification_after_fenced_write(&ap_state, &launch, 1, &[0x01, 0x80])
    .serve_read(&ssid, b"GFX100II-1234")
    .serve_read(&passphrase, b"12345678");
    let transport = Arc::new(ResponderTransport::new(responder));

    let outcome = block_on(run_establishment(
        store,
        "gfx100ii:app".into(),
        transport.clone(),
        Arc::new(Recorder::default()),
        Arc::new(ActivityRecorder::default()),
        vec![],
        vec![],
        vec![KeyValue {
            key: "launchMode".into(),
            value: "4".into(),
        }],
    ))
    .expect("the stale pre-command refusal is fenced before the causal success");
    assert_eq!(scope_get(&outcome.scope, "apState"), Some("32769"));

    let transport = Arc::try_unwrap(transport).unwrap_or_else(|_| panic!("sole owner"));
    let log = transport.into_log();
    let fence = log
        .iter()
        .position(
            |event| matches!(event, BleEvent::NotificationFence { uuid } if uuid == &ap_state),
        )
        .expect("AP-state fence");
    assert!(matches!(
        log.get(fence + 1),
        Some(BleEvent::Write { uuid, .. }) if uuid == &launch
    ));
}

#[test]
fn wifi_ap_malformed_notification_cannot_reuse_the_preflight_state() {
    let store = store();
    let view = gfx100ii();
    let ble = view.ble.as_ref().unwrap();
    let launch = uuid(ble, "functionLaunchRequest");
    let ap_state = uuid(ble, "apState");
    let responder = BleResponder::new([launch.clone(), ap_state.clone()])
        .serve_read(&ap_state, &[0x00, 0x80])
        .queue_notification_after_fenced_write(&ap_state, &launch, 1, &[0x00]);

    let error = block_on(run_establishment(
        store,
        "gfx100ii:app".into(),
        Arc::new(ResponderTransport::new(responder)),
        Arc::new(Recorder::default()),
        Arc::new(ActivityRecorder::default()),
        vec![],
        vec![],
        vec![KeyValue {
            key: "launchMode".into(),
            value: "4".into(),
        }],
    ))
    .expect_err("a malformed notification cannot satisfy or reject from stale baseline scope");

    assert!(matches!(
        error,
        ExecutorError::StepFailed {
            kind: ExecutorStepFailureKind::DeadlineExceeded,
            ..
        }
    ));
}

#[test]
fn wifi_ap_malformed_retry_notification_cannot_reuse_the_refusal() {
    let store = store();
    let view = gfx100ii();
    let ble = view.ble.as_ref().unwrap();
    let launch = uuid(ble, "functionLaunchRequest");
    let image_setting = uuid(ble, "imageTransferSetting");
    let ap_state = uuid(ble, "apState");
    let details = uuid(ble, "stateErrorDetails");
    let responder = BleResponder::new([
        launch.clone(),
        image_setting,
        ap_state.clone(),
        details.clone(),
    ])
    .serve_read(&ap_state, &[0x00, 0x80])
    .queue_notification_after_fenced_write(&ap_state, &launch, 1, &[0x00, 0x80])
    .queue_notification_after_fenced_write(&ap_state, &launch, 2, &[0x00])
    .serve_read(&details, &[0x02, 0x00]);

    let error = block_on(run_establishment(
        store,
        "gfx100ii:app".into(),
        Arc::new(ResponderTransport::new(responder)),
        Arc::new(Recorder::default()),
        Arc::new(ActivityRecorder::default()),
        vec![],
        vec![],
        vec![KeyValue {
            key: "launchMode".into(),
            value: "4".into(),
        }],
    ))
    .expect_err("a malformed retry notification cannot reuse the prior refusal");

    assert!(matches!(
        error,
        ExecutorError::StepFailed {
            kind: ExecutorStepFailureKind::DeadlineExceeded,
            ..
        }
    ));
}

#[test]
fn wifi_ap_retry_fence_discards_notification_left_by_prior_attempt() {
    let store = store();
    let view = gfx100ii();
    let ble = view.ble.as_ref().unwrap();
    let launch = uuid(ble, "functionLaunchRequest");
    let image_setting = uuid(ble, "imageTransferSetting");
    let ap_state = uuid(ble, "apState");
    let details = uuid(ble, "stateErrorDetails");
    let responder = BleResponder::new([
        launch.clone(),
        image_setting,
        ap_state.clone(),
        details.clone(),
    ])
    .serve_read(&ap_state, &[0x00, 0x80])
    .queue_notification_after_fenced_write(&ap_state, &launch, 1, &[0x00, 0x80])
    .queue_notification_after_fenced_write(&ap_state, &launch, 1, &[0x01, 0x80])
    .queue_notification_after_fenced_write(&ap_state, &launch, 2, &[0x00, 0x80])
    .serve_read(&details, &[0x02, 0x00]);
    let transport = Arc::new(ResponderTransport::new(responder));

    let error = block_on(run_establishment(
        store,
        "gfx100ii:app".into(),
        transport.clone(),
        Arc::new(Recorder::default()),
        Arc::new(ActivityRecorder::default()),
        vec![],
        vec![],
        vec![KeyValue {
            key: "launchMode".into(),
            value: "4".into(),
        }],
    ))
    .expect_err("attempt-1 launched residue cannot satisfy attempt 2");
    assert!(matches!(
        error,
        ExecutorError::StepFailed {
            kind: ExecutorStepFailureKind::ConditionRejected,
            ..
        }
    ));

    let transport = Arc::try_unwrap(transport).unwrap_or_else(|_| panic!("sole owner"));
    let log = transport.into_log();
    assert_eq!(
        log.iter()
            .filter(
                |event| matches!(event, BleEvent::NotificationFence { uuid } if uuid == &ap_state)
            )
            .count(),
        2
    );
    assert_eq!(
        log.iter()
            .filter(|event| matches!(event, BleEvent::Write { uuid, .. } if uuid == &launch))
            .count(),
        2
    );
}

#[test]
fn wifi_ap_retry_success_preserves_the_rejected_snapshot() {
    let store = store();
    let view = gfx100ii();
    let ble = view.ble.as_ref().unwrap();
    let launch = uuid(ble, "functionLaunchRequest");
    let image_setting = uuid(ble, "imageTransferSetting");
    let ap_state = uuid(ble, "apState");
    let details = uuid(ble, "stateErrorDetails");
    let ssid = uuid(ble, "cameraSSIDNameString");
    let passphrase = uuid(ble, "cameraWiFiPassphraseString");
    let responder = BleResponder::new([
        launch.clone(),
        image_setting,
        ap_state.clone(),
        details.clone(),
        ssid.clone(),
        passphrase.clone(),
    ])
    .serve_read_sequence(&ap_state, vec![vec![0x00, 0x80]])
    .queue_notification_after_fenced_write(&ap_state, &launch, 1, &[0x00, 0x80])
    .queue_notification_after_fenced_write(&ap_state, &launch, 2, &[0x01, 0x80])
    .serve_read(&details, &[0x02, 0x00])
    .serve_read(&ssid, b"GFX100II-1234")
    .serve_read(&passphrase, b"12345678");
    let activities = Arc::new(ActivityRecorder::default());

    let outcome = block_on(run_establishment(
        store,
        "gfx100ii:app".into(),
        Arc::new(ResponderTransport::new(responder)),
        Arc::new(Recorder::default()),
        activities.clone(),
        vec![],
        vec![],
        vec![KeyValue {
            key: "launchMode".into(),
            value: "4".into(),
        }],
    ))
    .expect("the second launch attempt succeeds");

    assert_eq!(scope_get(&outcome.scope, "apState"), Some("32769"));
    assert_eq!(scope_get(&outcome.scope, "stateErrorDetails"), Some("2"));
    let events = activities.0.lock().unwrap();
    assert!(events.contains(&ConnectionActivityEvent::Retrying {
        id: "camera.ap.launch".into(),
        version: 2,
        retry: launch_refusal_retry(),
    }));
    assert!(events.contains(&ConnectionActivityEvent::Succeeded {
        id: "camera.ap.launch".into(),
        version: 2,
        summary: ConnectionActivityTerminalSummary {
            retry_count: 1,
            last_retry: Some(launch_refusal_retry()),
        },
    }));
}

#[test]
fn recovered_launch_snapshot_survives_a_later_activity_failure() {
    let store = store();
    let view = gfx100ii();
    let ble = view.ble.as_ref().unwrap();
    let launch = uuid(ble, "functionLaunchRequest");
    let image_setting = uuid(ble, "imageTransferSetting");
    let ap_state = uuid(ble, "apState");
    let details = uuid(ble, "stateErrorDetails");
    let responder = BleResponder::new([
        launch.clone(),
        image_setting,
        ap_state.clone(),
        details.clone(),
    ])
    .serve_read_sequence(&ap_state, vec![vec![0x00, 0x80]])
    .queue_notification_after_fenced_write(&ap_state, &launch, 1, &[0x00, 0x80])
    .queue_notification_after_fenced_write(&ap_state, &launch, 2, &[0x01, 0x80])
    .serve_read(&details, &[0x02, 0x00]);
    let activities = Arc::new(ActivityRecorder::default());

    let error = block_on(run_establishment(
        store,
        "gfx100ii:app".into(),
        Arc::new(ResponderTransport::new(responder)),
        Arc::new(Recorder::default()),
        activities.clone(),
        vec![],
        vec![],
        vec![KeyValue {
            key: "launchMode".into(),
            value: "4".into(),
        }],
    ))
    .expect_err("the required credential read is not exposed");
    assert!(matches!(
        error,
        ExecutorError::StepFailed {
            kind: ExecutorStepFailureKind::Other,
            ..
        }
    ));

    let events = activities.0.lock().unwrap();
    assert!(events.contains(&ConnectionActivityEvent::Succeeded {
        id: "camera.ap.launch".into(),
        version: 2,
        summary: ConnectionActivityTerminalSummary {
            retry_count: 1,
            last_retry: Some(launch_refusal_retry()),
        },
    }));
    assert!(events.contains(&ConnectionActivityEvent::Failed {
        id: "camera.ap.credentials".into(),
        version: 1,
        summary: no_retry_summary(),
        failure: ConnectionActivityFailure {
            kind: ExecutorStepFailureKind::Other,
            context: vec![],
        },
    }));
}

#[test]
fn wifi_ap_retry_exhaustion_crosses_ffi_with_typed_context() {
    let store = store();
    let view = gfx100ii();
    let ble = view.ble.as_ref().unwrap();
    let launch = uuid(ble, "functionLaunchRequest");
    let image_setting = uuid(ble, "imageTransferSetting");
    let ap_state = uuid(ble, "apState");
    let details = uuid(ble, "stateErrorDetails");
    let responder = BleResponder::new([
        launch.clone(),
        image_setting.clone(),
        ap_state.clone(),
        details.clone(),
    ])
    .serve_read_sequence(&ap_state, vec![vec![0x00, 0x80]])
    .queue_notification_after_fenced_write(&ap_state, &launch, 1, &[0x00, 0x80])
    .queue_notification_after_fenced_write(&ap_state, &launch, 2, &[0x00, 0x80])
    .serve_read(&details, &[0x02, 0x00]);
    let transport = Arc::new(ResponderTransport::new(responder));
    let sleep_log = transport.sleep_log.clone();
    let activities = Arc::new(ActivityRecorder::default());
    let error = block_on(run_establishment(
        store,
        "gfx100ii:app".to_string(),
        transport.clone(),
        Arc::new(Recorder::default()),
        activities.clone(),
        vec![],
        vec![],
        vec![KeyValue {
            key: "launchMode".to_string(),
            value: "4".to_string(),
        }],
    ))
    .expect_err("the second NotLaunched refusal exhausts the bounded retry");
    match error {
        ExecutorError::StepFailed {
            kind,
            context,
            detail,
            ..
        } => {
            assert_eq!(kind, ExecutorStepFailureKind::ConditionRejected);
            assert!(detail.contains("failWhen"));
            assert_eq!(scope_get(&context, "apState"), Some("32768"));
            assert_eq!(scope_get(&context, "stateErrorDetails"), Some("2"));
            assert_eq!(
                context.len(),
                2,
                "only manifest-selected context crosses FFI"
            );
        }
        other => panic!("expected typed StepFailed, got {other:?}"),
    }
    assert_eq!(
        sleep_log
            .lock()
            .unwrap()
            .iter()
            .filter(|ms| **ms == 200)
            .count(),
        1,
    );
    assert_eq!(
        *activities.0.lock().unwrap(),
        vec![
            ConnectionActivityEvent::Started {
                id: "camera.ap.prepare".into(),
                version: 2,
            },
            ConnectionActivityEvent::Succeeded {
                id: "camera.ap.prepare".into(),
                version: 2,
                summary: no_retry_summary(),
            },
            ConnectionActivityEvent::Started {
                id: "camera.ap.launch".into(),
                version: 2,
            },
            ConnectionActivityEvent::Retrying {
                id: "camera.ap.launch".into(),
                version: 2,
                retry: launch_refusal_retry(),
            },
            ConnectionActivityEvent::Failed {
                id: "camera.ap.launch".into(),
                version: 2,
                summary: ConnectionActivityTerminalSummary {
                    retry_count: 1,
                    last_retry: Some(launch_refusal_retry()),
                },
                failure: launch_refusal_failure(),
            },
        ]
    );
    drop(sleep_log);
    let transport = Arc::try_unwrap(transport).unwrap_or_else(|_| panic!("sole owner"));
    let log = transport.into_log();
    assert_eq!(
        log.iter()
            .filter(|event| matches!(event, BleEvent::Write { uuid, .. } if uuid == &launch))
            .count(),
        2,
    );
    assert_eq!(
        log.iter()
            .filter(|event| matches!(event, BleEvent::Write { uuid, .. } if uuid == &image_setting))
            .count(),
        1,
    );
    assert_eq!(
        log.iter()
            .filter(|event| matches!(event, BleEvent::Subscribe { uuid, .. } if uuid == &ap_state))
            .count(),
        1,
    );
}

// ---------------------------------------------------------------------------
// postExitReadiness (run_post_exit_readiness — the pre-replay gate)
// ---------------------------------------------------------------------------

#[test]
fn post_exit_readiness_gate_awaits_the_not_launched_baseline() {
    let store = store();
    let view = gfx100ii();
    let ble = view.ble.as_ref().unwrap();
    let ap_state = uuid(ble, "apState");

    // Seeded-notify source: the seed read still sees Launching (0x8002); the
    // terminal NotLaunched (0x8000 = 32768) baseline arrives by notification.
    let responder = BleResponder::new([ap_state.clone()])
        .serve_read(&ap_state, &[0x02, 0x80])
        .queue_notification(&ap_state, &[0x00, 0x80]);

    let transport = Arc::new(ResponderTransport::new(responder));
    let recorder = Arc::new(Recorder::default());
    let activities = Arc::new(ActivityRecorder::default());
    let outcome = block_on(run_post_exit_readiness(
        store,
        "gfx100ii:app".to_string(),
        transport.clone(),
        recorder.clone(),
        activities.clone(),
        vec![],
        vec![],
        vec![],
    ))
    .expect("the post-exit readiness gate walks to completion");

    assert_eq!(
        outcome.steps_run, 3,
        "bleConnect + bleDiscoverServices + bleAwaitUntil"
    );
    assert_eq!(scope_get(&outcome.scope, "apState"), Some("32768"));
    assert_eq!(
        *activities.0.lock().unwrap(),
        vec![
            ConnectionActivityEvent::Started {
                id: "camera.ap.reset".into(),
                version: 1,
            },
            ConnectionActivityEvent::Succeeded {
                id: "camera.ap.reset".into(),
                version: 1,
                summary: no_retry_summary(),
            },
        ],
        "the Rust gate emits only its executor span, never host checkpoints"
    );

    let transport = Arc::try_unwrap(transport).unwrap_or_else(|_| panic!("sole owner"));
    let log = transport.into_log();
    let expected = vec![
        BleEvent::Connect,
        BleEvent::DiscoverServices,
        BleEvent::Subscribe {
            uuid: ap_state.clone(),
            mode: CccdMode::Notify,
        },
        BleEvent::Read {
            uuid: ap_state.clone(),
        },
    ];
    assert_eq!(log, expected, "connect, arm notifications, one seed read");

    let reports = recorder.0.lock().unwrap();
    let terminal: Vec<&StepReport> = reports
        .iter()
        .filter(|r| r.outcome != StepOutcome::Started)
        .collect();
    assert_eq!(terminal.len(), 3);
    assert!(terminal.iter().all(|r| r.outcome == StepOutcome::Succeeded));
}

#[test]
fn post_exit_notification_timeout_crosses_ffi_with_deadline_kind() {
    let store = store();
    let view = gfx100ii();
    let ble = view.ble.as_ref().unwrap();
    let ap_state = uuid(ble, "apState");

    // The seed read is still Launching and no terminal notification follows.
    let responder = BleResponder::new([ap_state.clone()]).serve_read(&ap_state, &[0x02, 0x80]);
    let error = block_on(run_post_exit_readiness(
        store,
        "gfx100ii:app".into(),
        Arc::new(ResponderTransport::new(responder)),
        Arc::new(Recorder::default()),
        Arc::new(ActivityRecorder::default()),
        vec![],
        vec![],
        vec![],
    ))
    .expect_err("the notification budget lapses");

    match error {
        ExecutorError::StepFailed {
            step, kind, detail, ..
        } => {
            assert_eq!(step, "steps[2].bleAwaitUntil");
            assert_eq!(kind, ExecutorStepFailureKind::DeadlineExceeded);
            assert!(detail.contains("within 20000ms"));
        }
        other => panic!("expected typed step failure, got {other:?}"),
    }
}

#[test]
fn post_exit_poll_timeout_crosses_ffi_with_deadline_kind() {
    let error = block_on(run_post_exit_readiness(
        poll_timeout_store(),
        "tm1:ble".into(),
        Arc::new(ClockTestTransport {
            first_read: Mutex::new(Some(vec![0x00])),
            clock_fails: false,
            stall_connect: false,
        }),
        Arc::new(Recorder::default()),
        Arc::new(ActivityRecorder::default()),
        vec![],
        vec![],
        vec![],
    ))
    .expect_err("the poll budget lapses");

    match error {
        ExecutorError::StepFailed {
            step, kind, detail, ..
        } => {
            assert_eq!(step, "steps[2].bleAwaitUntil");
            assert_eq!(kind, ExecutorStepFailureKind::DeadlineExceeded);
            assert!(detail.contains("within 5ms"));
        }
        other => panic!("expected typed step failure, got {other:?}"),
    }
}

#[test]
fn per_verb_clock_failure_crosses_ffi_as_other() {
    let error = block_on(run_post_exit_readiness(
        poll_timeout_store(),
        "tm1:ble".into(),
        Arc::new(ClockTestTransport {
            first_read: Mutex::new(None),
            clock_fails: true,
            stall_connect: true,
        }),
        Arc::new(Recorder::default()),
        Arc::new(ActivityRecorder::default()),
        vec![],
        vec![],
        vec![],
    ))
    .expect_err("the foreign deadline clock fails");

    match error {
        ExecutorError::StepFailed {
            step, kind, detail, ..
        } => {
            assert_eq!(step, "steps[0].bleConnect");
            assert_eq!(kind, ExecutorStepFailureKind::Other);
            assert!(detail.contains("clock unavailable"));
        }
        other => panic!("expected typed step failure, got {other:?}"),
    }
}

#[test]
fn notification_budget_clock_failure_crosses_ffi_as_other() {
    let error = block_on(run_post_exit_readiness(
        store(),
        "gfx100ii:app".into(),
        Arc::new(ClockTestTransport {
            first_read: Mutex::new(Some(vec![0x02, 0x80])),
            clock_fails: true,
            stall_connect: false,
        }),
        Arc::new(Recorder::default()),
        Arc::new(ActivityRecorder::default()),
        vec![],
        vec![],
        vec![],
    ))
    .expect_err("the notification budget clock fails");

    match error {
        ExecutorError::StepFailed {
            step, kind, detail, ..
        } => {
            assert_eq!(step, "steps[2].bleAwaitUntil");
            assert_eq!(kind, ExecutorStepFailureKind::Other);
            assert!(detail.contains("clock unavailable"));
        }
        other => panic!("expected typed step failure, got {other:?}"),
    }
}

#[test]
fn poll_budget_clock_failure_crosses_ffi_as_other() {
    let error = block_on(run_post_exit_readiness(
        poll_timeout_store(),
        "tm1:ble".into(),
        Arc::new(ClockTestTransport {
            first_read: Mutex::new(Some(vec![0x00])),
            clock_fails: true,
            stall_connect: false,
        }),
        Arc::new(Recorder::default()),
        Arc::new(ActivityRecorder::default()),
        vec![],
        vec![],
        vec![],
    ))
    .expect_err("the poll budget clock fails");

    match error {
        ExecutorError::StepFailed {
            step, kind, detail, ..
        } => {
            assert_eq!(step, "steps[2].bleAwaitUntil");
            assert_eq!(kind, ExecutorStepFailureKind::Other);
            assert!(detail.contains("clock unavailable"));
        }
        other => panic!("expected typed step failure, got {other:?}"),
    }
}

#[test]
fn connection_without_a_gate_resolves_immediately_with_no_io() {
    let store = store();
    let view = gfx100ii();
    let ble = view.ble.as_ref().unwrap();

    // gfx100ii:ble (ble-pair) declares no postExitReadiness.
    let transport = Arc::new(ResponderTransport::new(responder_for(ble)));
    let recorder = Arc::new(Recorder::default());
    let outcome = block_on(run_post_exit_readiness(
        store,
        "gfx100ii:ble".to_string(),
        transport.clone(),
        recorder.clone(),
        Arc::new(ActivityRecorder::default()),
        vec![],
        vec![],
        vec![],
    ))
    .expect("an empty gate is Ok");

    assert_eq!(outcome.steps_run, 0);
    let transport = Arc::try_unwrap(transport).unwrap_or_else(|_| panic!("sole owner"));
    assert!(transport.into_log().is_empty(), "no I/O for an empty gate");
    assert!(recorder.0.lock().unwrap().is_empty(), "no step reports");
}

#[test]
fn nikon_lss_executor_authenticates_and_binds_only_decoded_configuration() {
    let auth = "00002000-3DD4-4255-8D62-6DC7B9BD5561";
    let config_gatt = "00002004-3DD4-4255-8D62-6DC7B9BD5561";
    let script = nikon_lss_script();
    let responder = BleResponder::new([auth.into(), config_gatt.into()])
        .expect_exact_write(auth, &script.stage1)
        .queue_ordered_indication(auth, &script.stage2)
        .expect_exact_write(auth, &script.stage3)
        .queue_ordered_indication(auth, &script.stage4)
        .serve_read(config_gatt, &script.configuration);
    let transport = Arc::new(ResponderTransport::new(responder));
    let outcome = block_on(run_establishment(
        nikon_lss_store(),
        "d850:ble".into(),
        transport.clone(),
        Arc::new(Recorder::default()),
        Arc::new(ActivityRecorder::default()),
        vec![],
        vec![],
        vec![
            KeyValue {
                key: "clientDeviceId".into(),
                value: "1010101010101010".into(),
            },
            KeyValue {
                key: "clientNonce".into(),
                value: "2020202020202020".into(),
            },
        ],
    ))
    .expect("LSS executor completes");

    assert_eq!(scope_get(&outcome.scope, "flags"), Some("3"));
    assert_eq!(scope_get(&outcome.scope, "ssid"), Some("D850_TEST_AP"));
    assert_eq!(
        scope_get(&outcome.scope, "password"),
        Some("snapbridge-password")
    );
    assert_eq!(scope_get(&outcome.scope, "security"), Some("wpa2"));
    assert_eq!(scope_get(&outcome.scope, "sppMaximumLength"), Some("512"));
    assert_eq!(
        outcome.scope.len(),
        5,
        "no cipher/key/runtime material in scope"
    );

    let transport = Arc::try_unwrap(transport).unwrap_or_else(|_| panic!("sole owner"));
    let log = transport.into_log();
    assert!(matches!(
        &log[2],
        BleEvent::Subscribe {
            mode: CccdMode::Indicate,
            ..
        }
    ));
    assert_eq!(
        log.iter()
            .filter(|event| matches!(event, BleEvent::Write { uuid, .. } if uuid == auth))
            .count(),
        2
    );
}

#[test]
fn nikon_lss_executor_fails_when_fresh_nonce_is_missing() {
    let error = block_on(run_establishment(
        nikon_lss_store(),
        "d850:ble".into(),
        Arc::new(ResponderTransport::new(BleResponder::new([
            "00002000-3DD4-4255-8D62-6DC7B9BD5561".into(),
            "00002004-3DD4-4255-8D62-6DC7B9BD5561".into(),
        ]))),
        Arc::new(Recorder::default()),
        Arc::new(ActivityRecorder::default()),
        vec![],
        vec![],
        vec![KeyValue {
            key: "clientDeviceId".into(),
            value: "1010101010101010".into(),
        }],
    ))
    .expect_err("fresh runtime entropy is mandatory");
    assert!(matches!(
        error,
        ExecutorError::StepFailed { detail, .. }
            if detail.contains("runtime slot 'clientNonce' unbound")
    ));
}

#[test]
fn nikon_lss_executor_clears_optional_configuration_slots_when_flags_are_absent() {
    let auth = "00002000-3DD4-4255-8D62-6DC7B9BD5561";
    let config_gatt = "00002004-3DD4-4255-8D62-6DC7B9BD5561";
    let script = nikon_lss_script();
    let responder = BleResponder::new([auth.into(), config_gatt.into()])
        .expect_exact_write(auth, &script.stage1)
        .queue_ordered_indication(auth, &script.stage2)
        .expect_exact_write(auth, &script.stage3)
        .queue_ordered_indication(auth, &script.stage4)
        .serve_read(config_gatt, &[0]);
    let outcome = block_on(run_establishment(
        nikon_lss_store(),
        "d850:ble".into(),
        Arc::new(ResponderTransport::new(responder)),
        Arc::new(Recorder::default()),
        Arc::new(ActivityRecorder::default()),
        vec![
            KeyValue {
                key: "ssid".into(),
                value: "stale-ap".into(),
            },
            KeyValue {
                key: "password".into(),
                value: "stale-password".into(),
            },
            KeyValue {
                key: "security".into(),
                value: "wpa2".into(),
            },
            KeyValue {
                key: "sppMaximumLength".into(),
                value: "512".into(),
            },
        ],
        vec![
            KeyValue {
                key: "ssid".into(),
                value: "utf8".into(),
            },
            KeyValue {
                key: "password".into(),
                value: "utf8".into(),
            },
            KeyValue {
                key: "security".into(),
                value: "utf8".into(),
            },
            KeyValue {
                key: "sppMaximumLength".into(),
                value: "u32-le".into(),
            },
        ],
        vec![
            KeyValue {
                key: "clientDeviceId".into(),
                value: "1010101010101010".into(),
            },
            KeyValue {
                key: "clientNonce".into(),
                value: "2020202020202020".into(),
            },
        ],
    ))
    .expect("configuration without optional blocks completes");

    assert_eq!(scope_get(&outcome.scope, "flags"), Some("0"));
    for name in ["ssid", "password", "security", "sppMaximumLength"] {
        assert_eq!(scope_get(&outcome.scope, name), None, "stale {name}");
    }
}

#[test]
fn nikon_lss_executor_rejects_malformed_configuration_lengths() {
    let auth = "00002000-3DD4-4255-8D62-6DC7B9BD5561";
    let config_gatt = "00002004-3DD4-4255-8D62-6DC7B9BD5561";
    for malformed in [vec![0x00, 0xff], vec![0x02, 0x00, 0x02, 0x00]] {
        let script = nikon_lss_script();
        let responder = BleResponder::new([auth.into(), config_gatt.into()])
            .expect_exact_write(auth, &script.stage1)
            .queue_ordered_indication(auth, &script.stage2)
            .expect_exact_write(auth, &script.stage3)
            .queue_ordered_indication(auth, &script.stage4)
            .serve_read(config_gatt, &malformed);
        let error = block_on(run_establishment(
            nikon_lss_store(),
            "d850:ble".into(),
            Arc::new(ResponderTransport::new(responder)),
            Arc::new(Recorder::default()),
            Arc::new(ActivityRecorder::default()),
            vec![],
            vec![],
            vec![
                KeyValue {
                    key: "clientDeviceId".into(),
                    value: "1010101010101010".into(),
                },
                KeyValue {
                    key: "clientNonce".into(),
                    value: "2020202020202020".into(),
                },
            ],
        ))
        .expect_err("malformed configuration length must fail");
        assert!(matches!(
            error,
            ExecutorError::StepFailed { detail, .. }
                if detail.contains("connection configuration")
        ));
    }
}

#[test]
fn legacy_app_establish_awaits_indications_and_completes_on_state_1_through_ffi() {
    // Synthetic xa7 legacy-app-establish-wifi-ap via the FFI seam.
    let index = r#"
manufacturer: FUJIFILM
families:
  fuji:
    ble:
      gatt:
        apState: "A68E3F66-0FCC-4395-8D4C-AA980B5877FA"
        functionLaunchRequest: "600655E6-3637-42F1-8FB2-44EFC5C63B13"
        cameraSSIDNameString: "BF6DC9CF-3606-4EC9-A4C8-D77576E93EA4"
        transferState: "BD17BA04-B76B-4892-A545-B73BA1F74DAE"
      advert: {}
      establishments:
        legacy-app-pair:
          mechanism: legacy-app-pair
          activities:
            - { id: camera.remote.registration, version: 1, displayRole: confirmingPairing, defaultExpectedDurationMs: 1, interactionRequired: false, executorSpan: { sequence: steps, startStep: 0, endStepExclusive: 2 } }
          steps:
            - bleConnect: {}
            - bleDiscoverServices: {}
        legacy-app-establish-wifi-ap:
          mechanism: legacy-app-establish-wifi-ap
          prerequisite: legacy-app-pair
          onDemand: true
          params: [launchMode]
          persist: [ssid]
          activities:
            - { id: camera.remote.ap-launch, version: 1, displayRole: startingNetwork, defaultExpectedDurationMs: 1, interactionRequired: false, executorSpan: { sequence: steps, startStep: 0, endStepExclusive: 6 } }
          steps:
            - bleConnect: {}
            - bleDiscoverServices: {}
            - bleSubscribe: { gatt: apState, timeoutMs: 3000, mode: indicate }
            - bleWrite:
                gatt: functionLaunchRequest
                value: { runtime: launchMode, encoding: u16-le }
                notificationFence: apState
            - bleAwaitUntil:
                source: { notify: { gatt: apState, mode: indicate } }
                capture: { at: 0, length: 2, encoding: u16-le, name: apState }
                captureAs: apStateRaw
                until: { apState: { eq: "1" } }
                failWhen: { apState: { eq: "0" } }
                timeoutMs: 20000
            - bleRead: { gatt: cameraSSIDNameString, encoding: utf8-cstring, captureAs: ssid }
models:
  - id: xa7
    displayName: "X-A7"
    inherits: [fuji]
    manifest: xa7.yaml
"#;
    let body = "schema: camera-config/v1
camera: { manufacturer: FUJIFILM, model: X-A7 }";
    let store = ConfigStore::from_manufacturer_index(
        index.into(),
        vec![KeyValue {
            key: "xa7".into(),
            value: body.into(),
        }],
    )
    .expect("xa7 legacy store loads");
    let ap_state = "A68E3F66-0FCC-4395-8D4C-AA980B5877FA".to_string();
    let launch = "600655E6-3637-42F1-8FB2-44EFC5C63B13".to_string();
    let ssid_uuid = "BF6DC9CF-3606-4EC9-A4C8-D77576E93EA4".to_string();
    let plan_handle = "xa7:legacy-app-establish-wifi-ap".to_string();
    // Success case: 02 then 01
    let responder = BleResponder::new([ap_state.clone(), launch.clone(), ssid_uuid.clone()])
        .queue_notification_after_fenced_write(&ap_state, &launch, 1, &[0x02, 0x00])
        .queue_notification_after_fenced_write(&ap_state, &launch, 1, &[0x01, 0x00])
        .serve_read(&ssid_uuid, b"MY-AP");
    let transport = Arc::new(ResponderTransport::new(responder));
    let outcome = block_on(run_establishment(
        store.clone(),
        plan_handle.clone(),
        transport.clone(),
        Arc::new(Recorder::default()),
        Arc::new(ActivityRecorder::default()),
        vec![],
        vec![],
        vec![KeyValue {
            key: "launchMode".into(),
            value: "4".into(),
        }],
    ))
    .expect("legacy app walk completes on 01");
    assert_eq!(outcome.steps_run, 6);
    let log = Arc::try_unwrap(transport)
        .unwrap_or_else(|_| panic!("sole owner"))
        .into_log();
    assert!(
        !log.iter()
            .any(|e| matches!(e, BleEvent::Read { uuid } if uuid == &ap_state)),
        "no pre-launch read"
    );
    let writes: Vec<_> = log
        .iter()
        .filter_map(|e| match e {
            BleEvent::Write { uuid, value } if uuid == &launch => Some(value.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(writes, vec![vec![0x04, 0x00]]);

    // Failure case: 00 trips conditionRejected
    let responder = BleResponder::new([ap_state.clone(), launch.clone(), ssid_uuid.clone()])
        .queue_notification_after_fenced_write(&ap_state, &launch, 1, &[0x00, 0x00])
        .serve_read(&ssid_uuid, b"MY-AP");
    let transport = Arc::new(ResponderTransport::new(responder));
    let err = block_on(run_establishment(
        store.clone(),
        plan_handle.clone(),
        transport.clone(),
        Arc::new(Recorder::default()),
        Arc::new(ActivityRecorder::default()),
        vec![],
        vec![],
        vec![KeyValue {
            key: "launchMode".into(),
            value: "4".into(),
        }],
    ))
    .expect_err("00 must fail");
    assert!(matches!(
        err,
        ExecutorError::StepFailed {
            kind: ExecutorStepFailureKind::ConditionRejected,
            ..
        }
    ));

    // Transitional case: 02 alone does not satisfy
    let responder = BleResponder::new([ap_state.clone(), launch.clone(), ssid_uuid.clone()])
        .queue_notification_after_fenced_write(&ap_state, &launch, 1, &[0x02, 0x00])
        .serve_read(&ssid_uuid, b"MY-AP");
    let transport = Arc::new(ResponderTransport::new(responder));
    let err = block_on(run_establishment(
        store.clone(),
        plan_handle,
        transport,
        Arc::new(Recorder::default()),
        Arc::new(ActivityRecorder::default()),
        vec![],
        vec![],
        vec![KeyValue {
            key: "launchMode".into(),
            value: "4".into(),
        }],
    ))
    .expect_err("02 alone must deadline");
    assert!(matches!(
        err,
        ExecutorError::StepFailed {
            kind: ExecutorStepFailureKind::DeadlineExceeded,
            ..
        }
    ));
}
