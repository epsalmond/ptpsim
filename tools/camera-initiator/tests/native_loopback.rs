#[cfg(target_os = "linux")]
use std::collections::BTreeMap;
use std::io::{self, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use camera_config::{
    direct_epistemic, no_loss, validate_bundles, BundleHeader, CameraContext, CaptureClock,
    CaptureContext, CaptureInterface, CaptureInterfaceType, ClientContext, ClockType, ClockUnit,
    ObservationLine, ObservationRecorder,
};
use camera_initiator::{NativePtpTransport, TraceFormat, TraceWriter, TransportConfig};
use camera_protocol_ffi::{
    run_initiator_action, run_mode_entry, ActionInvocationRequest, ActionRole, ConfigStore,
    ConnectionActivityEvent, ConnectionActivityObserver, PtpExecutorTransport, StepObserver,
    StepReport,
};
#[cfg(target_os = "linux")]
use camera_protocol_ffi::{KeyValue, Observation, Recognition};
#[cfg(target_os = "linux")]
use camera_sim::{walk_establishment, BleResponder};
use camera_sim_service::{Config, Server};
#[cfg(target_os = "linux")]
use if_addrs::{get_if_addrs, IfAddr};
#[cfg(target_os = "linux")]
use protocol_primitives::{NikonLssAuthenticationSelection, NikonLssClient, NikonLssServer};
#[cfg(target_os = "linux")]
use ptp_core::{PtpCodec, PtpIpPacket};
#[cfg(target_os = "linux")]
use tokio::io::{AsyncReadExt, AsyncWriteExt};

struct NoopObserver;

impl StepObserver for NoopObserver {
    fn on_step(&self, _report: StepReport) {}
}

impl ConnectionActivityObserver for NoopObserver {
    fn on_activity(&self, _event: ConnectionActivityEvent) {}
}

struct TraceObserver(Arc<TraceWriter>);

impl StepObserver for TraceObserver {
    fn on_step(&self, report: StepReport) {
        self.0.step(&report).expect("record executor step");
    }
}

#[derive(Clone, Default)]
struct TraceBuffer(Arc<Mutex<Vec<u8>>>);

impl TraceBuffer {
    fn records(&self) -> Vec<serde_json::Value> {
        let bytes = self.0.lock().expect("trace buffer").clone();
        String::from_utf8(bytes)
            .expect("UTF-8 JSONL trace")
            .lines()
            .map(|line| serde_json::from_str(line).expect("valid JSONL trace record"))
            .collect()
    }
}

impl Write for TraceBuffer {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

struct TempMediaRoot(PathBuf);

impl TempMediaRoot {
    fn new() -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock after Unix epoch")
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "ptpsim-camera-initiator-{}-{nonce}",
            std::process::id()
        ));
        std::fs::create_dir_all(&path).expect("create temporary media root");
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempMediaRoot {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn data(relative: &str) -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../packages/camera-config-data")
        .join(relative);
    std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("read {}: {error}", path.display()))
}

fn observation_recorder(path: PathBuf, run_id: &str) -> ObservationRecorder {
    ObservationRecorder::open(
        Some(path),
        BundleHeader {
            schema: camera_config::OBSERVATION_SCHEMA_VERSION.into(),
            run_id: run_id.into(),
            record_id: "header".into(),
            ordinal: 0,
            camera: CameraContext {
                manufacturer: "FUJIFILM".into(),
                model: "GFX100 II".into(),
                body_id: "loopback-body".into(),
                firmware: "2.30".into(),
            },
            client: ClientContext {
                artifact: "camera-initiator-test".into(),
                version: "test".into(),
                platform: "test".into(),
            },
            capture: CaptureContext {
                interfaces: vec![CaptureInterface {
                    id: "loopback".into(),
                    interface_type: CaptureInterfaceType::Tcp,
                    role: "initiator".into(),
                }],
                clocks: vec![CaptureClock {
                    id: "process-monotonic".into(),
                    clock_type: ClockType::Monotonic,
                    unit: ClockUnit::Milliseconds,
                }],
                clock_mappings: Vec::new(),
                loss: no_loss(),
                redactions: Vec::new(),
                tool_versions: std::collections::BTreeMap::from([(
                    "camera-initiator-test".into(),
                    "test".into(),
                )]),
                artifacts: Vec::new(),
            },
            epistemic: direct_epistemic(),
        },
    )
    .expect("open test observation recorder")
}

fn with_loopback_ports(
    body: String,
    command: SocketAddr,
    event: SocketAddr,
    live_view: SocketAddr,
) -> String {
    body.replacen("command: 55740", &format!("command: {}", command.port()), 1)
        .replacen("event: 55741", &format!("event: {}", event.port()), 1)
        .replacen(
            "liveView: 55742",
            &format!("liveView: {}", live_view.port()),
            1,
        )
}

fn replace_once(body: String, from: &str, to: String) -> String {
    assert_eq!(
        body.matches(from).count(),
        1,
        "expected one manifest field matching {from:?}"
    );
    body.replacen(from, &to, 1)
}

#[cfg(target_os = "linux")]
fn standard_loopback_body(port: u16, model: &str) -> String {
    format!(
        r#"
schema: camera-config/v1
camera: {{ manufacturer: TEST, model: {model}, firmware: "1.0" }}
values:
  initiatorGuid: {{ type: fixed, value: "00112233445566778899aabbccddeeff" }}
  initiatorName: {{ type: fixed, value: SnapBridge }}
connections:
  app:
    kind: ptpip
    initShape: standardPtpIp
    init:
      identity: {{ guid: initiatorGuid, friendlyName: initiatorName }}
    commandFraming: standard
    eventFraming: standard
    bindings: {{ command: {port}, event: {port} }}
operations:
  "0x1001": {{ name: GetDeviceInfo, connections: [app] }}
  "0x1002": {{ name: OpenSession, connections: [app] }}
  "0x1003": {{ name: CloseSession, connections: [app] }}
properties: {{}}
"#
    )
}

#[tokio::test]
async fn standard_ptpip_opens_event_reads_device_info_then_opens_session_and_probes() {
    let reserved = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
        .expect("reserve standard PTP/IP port");
    let port = reserved.local_addr().unwrap().port();
    drop(reserved);
    let body = format!(
        r#"
schema: camera-config/v1
camera: {{ manufacturer: TEST, model: Standard Camera, firmware: "1.0" }}
values:
  initiatorGuid: {{ type: fixed, value: "00112233445566778899aabbccddeeff" }}
  initiatorName: {{ type: fixed, value: SnapBridge }}
connections:
  app:
    kind: ptpip
    initShape: standardPtpIp
    init:
      identity: {{ guid: initiatorGuid, friendlyName: initiatorName }}
    commandFraming: standard
    eventFraming: standard
    bindings: {{ command: {port}, event: {port} }}
operations:
  "0x1001": {{ name: GetDeviceInfo, connections: [app] }}
  "0x1002": {{ name: OpenSession, connections: [app] }}
  "0x1003": {{ name: CloseSession, connections: [app] }}
properties: {{}}
"#
    );
    let media = TempMediaRoot::new();
    let server = Server::bind(Config {
        instance_id: "standard-initiator-loopback".into(),
        profile: "test/standard".into(),
        connection: "app".into(),
        manifest_yaml: body.clone(),
        media_root: media.path().to_path_buf(),
        command_bind: Some(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port)),
        liveview_bind: None,
        event_bind: None,
        knock_bind: None,
        pcss_init_fails: 0,
        pcss_shutter_enqueue_count: 0,
        control_bind: SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0),
        liveview_dir: None,
        state_callback: None,
    })
    .await
    .expect("bind standard simulator");
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
    let server_task = tokio::spawn(server.run(shutdown_rx));

    let store = ConfigStore::from_bundle(body, None).expect("load standard manifest");
    let trace_buffer = TraceBuffer::default();
    let transport = NativePtpTransport::new(
        store,
        TransportConfig {
            camera: Some(IpAddr::V4(Ipv4Addr::LOCALHOST)),
            interface: None,
            connection: "app".into(),
            runtime_scope: vec![],
            connect_timeout: Duration::from_secs(2),
            max_frame_bytes: 1024 * 1024,
        },
        Arc::new(TraceWriter::new(
            TraceFormat::Jsonl,
            Box::new(trace_buffer.clone()),
        )),
    )
    .expect("construct standard transport");

    let opened = transport
        .open_command_session()
        .await
        .expect("open standard command and event session");
    assert_eq!(opened.transaction_id, 1);
    assert_eq!(opened.response_code, ptp_core::codes::resp::OK);
    transport
        .probe_event_channel()
        .await
        .expect("standard probe response");
    transport
        .close_session_if_open()
        .await
        .expect("close standard session");

    let records = trace_buffer.records();
    let wire_channels: Vec<_> = records
        .iter()
        .filter(|record| record["kind"] == "wire")
        .filter_map(|record| record["channel"].as_str())
        .collect();
    assert!(wire_channels.starts_with(&["init", "init", "eventInit", "eventInit"]));
    assert!(records.iter().any(|record| {
        record["kind"] == "session"
            && record["state"] == "deviceInfo"
            && record["detail"]["model"] == "Standard Camera"
    }));

    shutdown_tx.send(()).ok();
    server_task.await.unwrap();
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn nikon_snapbridge_milestone_reaches_d850_device_info_and_probe() {
    const AUTH: &str = "00002000-3DD4-4255-8D62-6DC7B9BD5561";
    const CLIENT_NAME: &str = "00002002-3DD4-4255-8D62-6DC7B9BD5561";
    const SERVER_NAME: &str = "00002003-3DD4-4255-8D62-6DC7B9BD5561";
    const CONFIGURATION: &str = "00002004-3DD4-4255-8D62-6DC7B9BD5561";
    const ESTABLISHMENT: &str = "00002005-3DD4-4255-8D62-6DC7B9BD5561";
    const FEATURE: &str = "00002009-3DD4-4255-8D62-6DC7B9BD5561";
    const SERIAL: &str = "0000200B-3DD4-4255-8D62-6DC7B9BD5561";
    const MODEL: &str = "00002A24-0000-1000-8000-00805F9B34FB";
    const FIRMWARE: &str = "00002A26-0000-1000-8000-00805F9B34FB";
    const LSS_SERVICE: &str = "0000DE00-3DD4-4255-8D62-6DC7B9BD5561";

    let index_yaml = data("nikon/index.yaml");
    let ffi_store = ConfigStore::from_manufacturer_index_with_defaults(
        index_yaml.clone(),
        data("nikon/nikon.yaml"),
        vec![
            KeyValue {
                key: "nikon-camera".into(),
                value: data("nikon/nikon-camera/nikon-camera.yaml"),
            },
            KeyValue {
                key: "d850".into(),
                value: data("nikon/d850/d850.yaml"),
            },
        ],
    )
    .expect("load provisional Nikon index");
    assert!(matches!(
        ffi_store.recognize(Observation::BleAdvert {
            service_uuids: vec![LSS_SERVICE.into()],
            manufacturer_data: None,
            service_data: vec![],
            local_name: Some("D850".into()),
            tx_power: None,
            ad_records: vec![],
        }),
        Recognition::Candidate { model, .. } if model == "nikon-camera"
    ));

    let index = camera_config::index::ResolvedManufacturerIndex::from_yaml(&index_yaml)
        .expect("resolve Nikon family plans");
    let ble = index.models[0].ble.as_ref().expect("Nikon BLE family");
    let pair = ble.establishment("ble-pair").expect("pair plan");
    let wifi = ble
        .establishment("ble-establish-wifi-ap")
        .expect("Wi-Fi plan");
    let catalog = || ble.gatt.values().cloned().collect::<Vec<_>>();

    let client_device_id = [0x10; 8];
    let pair_nonce = [0x20; 8];
    let mut pair_client = NikonLssClient::new(client_device_id, pair_nonce);
    let pair_stage1 = pair_client.stage1_record().unwrap();
    let mut pair_server = NikonLssServer::new(
        NikonLssAuthenticationSelection::new(7).unwrap(),
        [0x30; 8],
        [0x40; 8],
    );
    let pair_stage2 = pair_server.handle_stage1(&pair_stage1).unwrap();
    let pair_stage3 = pair_client.handle_stage2(&pair_stage2).unwrap();
    let (pair_stage4, _) = pair_server.finish_stage3(&pair_stage3).unwrap();
    let snapbridge_name = "Android_Pixel_8_1234";
    let mut padded_name = snapbridge_name.as_bytes().to_vec();
    padded_name.resize(32, 0);
    let mut pair_responder = BleResponder::new(catalog())
        .expect_exact_write(AUTH, &pair_stage1)
        .queue_ordered_indication(AUTH, &pair_stage2)
        .expect_exact_write(AUTH, &pair_stage3)
        .queue_ordered_indication(AUTH, &pair_stage4)
        .expect_exact_write(CLIENT_NAME, &padded_name)
        .serve_read(SERVER_NAME, b"D850\0")
        .serve_read_sequence(
            FEATURE,
            vec![vec![0x01], vec![0x02], vec![0x04], vec![0x08]],
        )
        .serve_read(MODEL, b"D850\0")
        .serve_read(SERIAL, b"D850-SIM-0001\0")
        .serve_read(FIRMWARE, b"1.31\0");
    let pair_runtime = BTreeMap::from([
        ("clientDeviceId".into(), "1010101010101010".into()),
        ("clientNonce".into(), "2020202020202020".into()),
        ("snapBridgeClientName".into(), snapbridge_name.into()),
    ]);
    let pair_outcome = walk_establishment(
        &mut pair_responder,
        &pair.steps,
        &BTreeMap::new(),
        &BTreeMap::new(),
        &pair_runtime,
    )
    .expect("walk Nikon pairing");
    assert_eq!(
        pair_outcome.scope.get("model").map(String::as_str),
        Some("D850")
    );
    assert_eq!(
        pair_responder.written(CLIENT_NAME),
        vec![padded_name.as_slice()]
    );

    let wifi_nonce = [0x21; 8];
    let mut wifi_client = NikonLssClient::new(client_device_id, wifi_nonce);
    let wifi_stage1 = wifi_client.stage1_record().unwrap();
    let mut wifi_server = NikonLssServer::new(
        NikonLssAuthenticationSelection::new(6).unwrap(),
        [0x31; 8],
        [0x41; 8],
    );
    let wifi_stage2 = wifi_server.handle_stage1(&wifi_stage1).unwrap();
    let wifi_stage3 = wifi_client.handle_stage2(&wifi_stage2).unwrap();
    let (wifi_stage4, wifi_session) = wifi_server.finish_stage3(&wifi_stage3).unwrap();
    let mut ssid = b"D850_SIM_AP".to_vec();
    ssid.resize(32, 0);
    let mut password = b"snapbridge-sim-password".to_vec();
    password.resize(64, 0);
    let mut encrypted_configuration = vec![0x03];
    encrypted_configuration.extend(wifi_session.encrypt(&ssid).unwrap());
    encrypted_configuration.extend(wifi_session.encrypt(&password).unwrap());
    encrypted_configuration.push(1);
    encrypted_configuration.extend(512_u32.to_le_bytes());
    let mut wifi_responder = BleResponder::new(catalog())
        .expect_exact_write(AUTH, &wifi_stage1)
        .queue_ordered_indication(AUTH, &wifi_stage2)
        .expect_exact_write(AUTH, &wifi_stage3)
        .queue_ordered_indication(AUTH, &wifi_stage4)
        .expect_exact_write(ESTABLISHMENT, &[0x02])
        .serve_read(CONFIGURATION, &encrypted_configuration);
    let wifi_runtime = BTreeMap::from([
        ("clientDeviceId".into(), "1010101010101010".into()),
        ("clientNonce".into(), "2121212121212121".into()),
    ]);
    let wifi_outcome = walk_establishment(
        &mut wifi_responder,
        &wifi.steps,
        &BTreeMap::new(),
        &BTreeMap::new(),
        &wifi_runtime,
    )
    .expect("walk encrypted Nikon Wi-Fi handoff");
    assert_eq!(
        wifi_outcome.scope.get("ssid").map(String::as_str),
        Some("D850_SIM_AP")
    );
    assert_eq!(
        wifi_outcome.scope.get("password").map(String::as_str),
        Some("snapbridge-sim-password")
    );
    assert_eq!(wifi_responder.written(ESTABLISHMENT), vec![&[0x02][..]]);

    let reserved = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
        .expect("reserve D850 standard PTP/IP port");
    let port = reserved.local_addr().unwrap().port();
    drop(reserved);
    let body = data("nikon/d850/d850.yaml")
        .replacen(
            "connections:",
            r#"values:
  initiatorGuid: { type: fixed, value: "00112233445566778899aabbccddeeff" }
  initFriendlyName: { type: fixed, value: "Android Device" }
connections:"#,
            1,
        )
        .replacen(
            "bindings: { command: 15740, event: 15740 }",
            &format!("bindings: {{ command: {port}, event: {port} }}"),
            1,
        );
    let media = TempMediaRoot::new();
    let server = Server::bind(Config {
        instance_id: "nikon-d850-milestone".into(),
        profile: "nikon/d850-provisional".into(),
        connection: "app".into(),
        manifest_yaml: body.clone(),
        media_root: media.path().to_path_buf(),
        command_bind: Some(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port)),
        liveview_bind: None,
        event_bind: None,
        knock_bind: None,
        pcss_init_fails: 0,
        pcss_shutter_enqueue_count: 0,
        control_bind: SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0),
        liveview_dir: None,
        state_callback: None,
    })
    .await
    .expect("bind provisional D850 simulator");
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
    let server_task = tokio::spawn(server.run(shutdown_rx));
    let transport_store = ConfigStore::from_bundle(body, None).expect("load D850 loopback body");
    let trace_buffer = TraceBuffer::default();
    let transport = NativePtpTransport::new(
        transport_store,
        TransportConfig {
            camera: Some(IpAddr::V4(Ipv4Addr::LOCALHOST)),
            interface: None,
            connection: "app".into(),
            runtime_scope: vec![],
            connect_timeout: Duration::from_secs(2),
            max_frame_bytes: 1024 * 1024,
        },
        Arc::new(TraceWriter::new(
            TraceFormat::Jsonl,
            Box::new(trace_buffer.clone()),
        )),
    )
    .expect("construct D850 standard transport");
    transport
        .open_command_session()
        .await
        .expect("command init, event init, DeviceInfo, OpenSession");
    transport
        .probe_event_channel()
        .await
        .expect("D850 probe response");
    let records = trace_buffer.records();
    assert!(records.iter().any(|record| {
        record["kind"] == "session"
            && record["state"] == "deviceInfo"
            && record["detail"]["model"] == "D850"
    }));
    transport.close_session_if_open().await.unwrap();
    shutdown_tx.send(()).ok();
    server_task.await.unwrap();
}

#[cfg(target_os = "linux")]
async fn standard_vendor_discovery_case(advertised: bool) {
    const GET_VENDOR_CODES: u16 = 0x9439;
    let listener = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
        .await
        .expect("bind standard PTP/IP vendor-code listener");
    let port = listener.local_addr().unwrap().port();
    let body = standard_loopback_body(port, "Vendor Code Camera");

    let camera_task = tokio::spawn(async move {
        let (mut command, _) = listener.accept().await.expect("accept command socket");
        assert!(matches!(
            PtpIpPacket::decode(&read_declared_frame(&mut command).await),
            Ok(PtpIpPacket::InitCommandRequest(_))
        ));
        command
            .write_all(
                &ptp_core::encode(&PtpIpPacket::InitCommandAck(ptp_core::InitCommandAck {
                    connection_number: 52,
                    responder_guid: [0x52; 16],
                    friendly_name: "Vendor Code Camera".into(),
                    protocol_version: 0x0001_0000,
                }))
                .unwrap(),
            )
            .await
            .unwrap();

        let (mut event, _) = listener.accept().await.expect("accept event socket");
        assert!(matches!(
            PtpIpPacket::decode(&read_declared_frame(&mut event).await),
            Ok(PtpIpPacket::InitEventRequest(request)) if request.connection_number == 52
        ));
        event
            .write_all(
                &ptp_core::encode(&PtpIpPacket::InitEventAck(ptp_core::InitEventAck)).unwrap(),
            )
            .await
            .unwrap();

        assert!(matches!(
            PtpIpPacket::decode(&read_declared_frame(&mut command).await),
            Ok(PtpIpPacket::OperationRequest(request))
                if request.code == ptp_core::codes::op::GET_DEVICE_INFO
                    && request.transaction_id == 0
        ));
        let mut operations = vec![
            ptp_core::codes::op::GET_DEVICE_INFO,
            ptp_core::codes::op::OPEN_SESSION,
            ptp_core::codes::op::CLOSE_SESSION,
        ];
        if advertised {
            operations.push(GET_VENDOR_CODES);
        }
        let info = ptp_core::DeviceInfo {
            standard_version: 100,
            operations_supported: operations,
            manufacturer: "TEST".into(),
            model: "Vendor Code Camera".into(),
            device_version: "1.0".into(),
            ..Default::default()
        };
        let mut writer = ptp_core::Writer::new();
        info.encode(&mut writer).unwrap();
        let payload = writer.into_vec();
        for packet in [
            PtpIpPacket::StartData(ptp_core::StartData {
                transaction_id: 0,
                total_length: payload.len() as u64,
            }),
            PtpIpPacket::EndData(ptp_core::DataBlock {
                transaction_id: 0,
                payload,
            }),
            PtpIpPacket::OperationResponse(ptp_core::OperationResponse {
                code: ptp_core::codes::resp::OK,
                transaction_id: 0,
                params: vec![],
            }),
        ] {
            command
                .write_all(&ptp_core::encode(&packet).unwrap())
                .await
                .unwrap();
        }

        assert!(matches!(
            PtpIpPacket::decode(&read_declared_frame(&mut command).await),
            Ok(PtpIpPacket::OperationRequest(request))
                if request.code == ptp_core::codes::op::OPEN_SESSION
                    && request.transaction_id == 1
        ));
        command
            .write_all(
                &ptp_core::encode(&PtpIpPacket::OperationResponse(
                    ptp_core::OperationResponse {
                        code: ptp_core::codes::resp::OK,
                        transaction_id: 1,
                        params: vec![],
                    },
                ))
                .unwrap(),
            )
            .await
            .unwrap();

        if advertised {
            assert!(matches!(
                PtpIpPacket::decode(&read_declared_frame(&mut command).await),
                Ok(PtpIpPacket::OperationRequest(request))
                    if request.data_phase_info == 1
                        && request.code == GET_VENDOR_CODES
                        && request.transaction_id == 2
                        && request.params == vec![0x0000_0009]
            ));
            let mut writer = ptp_core::Writer::new();
            writer.ptp_array(&[0x0000_0001, 0x0000_2001], |writer, code| {
                writer.u32(*code)
            });
            let payload = writer.into_vec();
            for packet in [
                PtpIpPacket::StartData(ptp_core::StartData {
                    transaction_id: 2,
                    total_length: payload.len() as u64,
                }),
                PtpIpPacket::Data(ptp_core::DataBlock {
                    transaction_id: 2,
                    payload: payload[..4].to_vec(),
                }),
                PtpIpPacket::EndData(ptp_core::DataBlock {
                    transaction_id: 2,
                    payload: payload[4..].to_vec(),
                }),
                PtpIpPacket::OperationResponse(ptp_core::OperationResponse {
                    code: ptp_core::codes::resp::OK,
                    transaction_id: 2,
                    params: vec![],
                }),
            ] {
                command
                    .write_all(&ptp_core::encode(&packet).unwrap())
                    .await
                    .unwrap();
            }
        }

        let close_transaction_id = if advertised { 3 } else { 2 };
        assert!(matches!(
            PtpIpPacket::decode(&read_declared_frame(&mut command).await),
            Ok(PtpIpPacket::OperationRequest(request))
                if request.code == ptp_core::codes::op::CLOSE_SESSION
                    && request.transaction_id == close_transaction_id
        ));
        command
            .write_all(
                &ptp_core::encode(&PtpIpPacket::OperationResponse(
                    ptp_core::OperationResponse {
                        code: ptp_core::codes::resp::OK,
                        transaction_id: close_transaction_id,
                        params: vec![],
                    },
                ))
                .unwrap(),
            )
            .await
            .unwrap();
    });

    let store = ConfigStore::from_bundle(body, None).expect("load standard manifest");
    let trace_buffer = TraceBuffer::default();
    let transport = NativePtpTransport::new(
        store,
        TransportConfig {
            camera: Some(IpAddr::V4(Ipv4Addr::LOCALHOST)),
            interface: None,
            connection: "app".into(),
            runtime_scope: vec![],
            connect_timeout: Duration::from_secs(2),
            max_frame_bytes: 1024 * 1024,
        },
        Arc::new(TraceWriter::new(
            TraceFormat::Jsonl,
            Box::new(trace_buffer.clone()),
        )),
    )
    .unwrap();
    transport.open_command_session().await.unwrap();
    transport.close_session_if_open().await.unwrap();
    camera_task.await.unwrap();

    let vendor_trace = trace_buffer
        .records()
        .into_iter()
        .find(|record| record["kind"] == "session" && record["state"] == "vendorCodes");
    if advertised {
        let trace = vendor_trace.expect("trace advertised vendor codes");
        assert_eq!(trace["detail"]["count"], 2);
        assert_eq!(trace["detail"]["codes"], serde_json::json!([1, 8193]));
    } else {
        assert!(vendor_trace.is_none(), "unadvertised discovery was traced");
    }
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn standard_ptpip_discovers_advertised_vendor_codes_after_open_session() {
    standard_vendor_discovery_case(true).await;
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn standard_ptpip_skips_unadvertised_vendor_codes() {
    standard_vendor_discovery_case(false).await;
}

#[cfg(target_os = "linux")]
#[derive(Clone, Copy)]
enum MalformedDeviceInfo {
    DataBeforeStart,
    OversizedDeclaration,
}

#[cfg(target_os = "linux")]
async fn standard_malformed_device_info_case(malformed: MalformedDeviceInfo, expected_error: &str) {
    let listener = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
        .await
        .expect("bind malformed DeviceInfo listener");
    let port = listener.local_addr().unwrap().port();
    let body = standard_loopback_body(port, "Malformed DeviceInfo Camera");
    let camera_task = tokio::spawn(async move {
        let (mut command, _) = listener.accept().await.expect("accept command socket");
        assert!(matches!(
            PtpIpPacket::decode(&read_declared_frame(&mut command).await),
            Ok(PtpIpPacket::InitCommandRequest(_))
        ));
        command
            .write_all(
                &ptp_core::encode(&PtpIpPacket::InitCommandAck(ptp_core::InitCommandAck {
                    connection_number: 63,
                    responder_guid: [0x63; 16],
                    friendly_name: "Malformed DeviceInfo Camera".into(),
                    protocol_version: 0x0001_0000,
                }))
                .unwrap(),
            )
            .await
            .unwrap();

        let (mut event, _) = listener.accept().await.expect("accept event socket");
        assert!(matches!(
            PtpIpPacket::decode(&read_declared_frame(&mut event).await),
            Ok(PtpIpPacket::InitEventRequest(request)) if request.connection_number == 63
        ));
        event
            .write_all(
                &ptp_core::encode(&PtpIpPacket::InitEventAck(ptp_core::InitEventAck)).unwrap(),
            )
            .await
            .unwrap();

        assert!(matches!(
            PtpIpPacket::decode(&read_declared_frame(&mut command).await),
            Ok(PtpIpPacket::OperationRequest(request))
                if request.code == ptp_core::codes::op::GET_DEVICE_INFO
                    && request.transaction_id == 0
        ));
        let packet = match malformed {
            MalformedDeviceInfo::DataBeforeStart => PtpIpPacket::Data(ptp_core::DataBlock {
                transaction_id: 0,
                payload: vec![0],
            }),
            MalformedDeviceInfo::OversizedDeclaration => {
                PtpIpPacket::StartData(ptp_core::StartData {
                    transaction_id: 0,
                    total_length: 65,
                })
            }
        };
        command
            .write_all(&ptp_core::encode(&packet).unwrap())
            .await
            .unwrap();
    });

    let store = ConfigStore::from_bundle(body, None).expect("load standard manifest");
    let transport = NativePtpTransport::new(
        store,
        TransportConfig {
            camera: Some(IpAddr::V4(Ipv4Addr::LOCALHOST)),
            interface: None,
            connection: "app".into(),
            runtime_scope: vec![],
            connect_timeout: Duration::from_secs(2),
            max_frame_bytes: 64,
        },
        Arc::new(TraceWriter::new(TraceFormat::Jsonl, Box::new(io::sink()))),
    )
    .unwrap();
    let error = transport
        .open_command_session()
        .await
        .expect_err("reject malformed DeviceInfo data phase");
    assert!(
        error.to_string().contains(expected_error),
        "unexpected malformed data error: {error}"
    );
    camera_task.await.unwrap();
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn standard_ptpip_rejects_device_info_data_before_start() {
    standard_malformed_device_info_case(
        MalformedDeviceInfo::DataBeforeStart,
        "data arrived before StartData",
    )
    .await;
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn standard_ptpip_rejects_oversized_device_info_declaration() {
    standard_malformed_device_info_case(
        MalformedDeviceInfo::OversizedDeclaration,
        "declared data length 65 exceeds limit 64",
    )
    .await;
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn standard_probe_retains_event_that_arrives_before_response() {
    let listener = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
        .await
        .expect("bind standard PTP/IP loopback listener");
    let port = listener.local_addr().unwrap().port();
    let body = format!(
        r#"
schema: camera-config/v1
camera: {{ manufacturer: TEST, model: Probe Camera, firmware: "1.0" }}
values:
  initiatorGuid: {{ type: fixed, value: "00112233445566778899aabbccddeeff" }}
  initiatorName: {{ type: fixed, value: SnapBridge }}
connections:
  app:
    kind: ptpip
    initShape: standardPtpIp
    init:
      identity: {{ guid: initiatorGuid, friendlyName: initiatorName }}
    commandFraming: standard
    eventFraming: standard
    bindings: {{ command: {port}, event: {port} }}
operations:
  "0x1001": {{ name: GetDeviceInfo, connections: [app] }}
  "0x1002": {{ name: OpenSession, connections: [app] }}
  "0x1003": {{ name: CloseSession, connections: [app] }}
properties: {{}}
"#
    );

    let camera_task = tokio::spawn(async move {
        let (mut command, _) = listener.accept().await.expect("accept command socket");
        assert!(matches!(
            PtpIpPacket::decode(&read_declared_frame(&mut command).await),
            Ok(PtpIpPacket::InitCommandRequest(_))
        ));
        command
            .write_all(
                &ptp_core::encode(&PtpIpPacket::InitCommandAck(ptp_core::InitCommandAck {
                    connection_number: 41,
                    responder_guid: [0x22; 16],
                    friendly_name: "Probe Camera".into(),
                    protocol_version: 0x0001_0000,
                }))
                .unwrap(),
            )
            .await
            .unwrap();

        let (mut event, _) = listener.accept().await.expect("accept event socket");
        assert!(matches!(
            PtpIpPacket::decode(&read_declared_frame(&mut event).await),
            Ok(PtpIpPacket::InitEventRequest(request)) if request.connection_number == 41
        ));
        event
            .write_all(
                &ptp_core::encode(&PtpIpPacket::InitEventAck(ptp_core::InitEventAck)).unwrap(),
            )
            .await
            .unwrap();

        assert!(matches!(
            PtpIpPacket::decode(&read_declared_frame(&mut command).await),
            Ok(PtpIpPacket::OperationRequest(request))
                if request.code == ptp_core::codes::op::GET_DEVICE_INFO
                    && request.transaction_id == 0
        ));
        let info = ptp_core::DeviceInfo {
            standard_version: 100,
            operations_supported: vec![
                ptp_core::codes::op::GET_DEVICE_INFO,
                ptp_core::codes::op::OPEN_SESSION,
                ptp_core::codes::op::CLOSE_SESSION,
            ],
            manufacturer: "TEST".into(),
            model: "Probe Camera".into(),
            device_version: "1.0".into(),
            ..Default::default()
        };
        let mut writer = ptp_core::Writer::new();
        info.encode(&mut writer).unwrap();
        let payload = writer.into_vec();
        for packet in [
            PtpIpPacket::StartData(ptp_core::StartData {
                transaction_id: 0,
                total_length: payload.len() as u64,
            }),
            PtpIpPacket::EndData(ptp_core::DataBlock {
                transaction_id: 0,
                payload,
            }),
            PtpIpPacket::OperationResponse(ptp_core::OperationResponse {
                code: ptp_core::codes::resp::OK,
                transaction_id: 0,
                params: vec![],
            }),
        ] {
            command
                .write_all(&ptp_core::encode(&packet).unwrap())
                .await
                .unwrap();
        }

        assert!(matches!(
            PtpIpPacket::decode(&read_declared_frame(&mut command).await),
            Ok(PtpIpPacket::OperationRequest(request))
                if request.code == ptp_core::codes::op::OPEN_SESSION
                    && request.transaction_id == 1
        ));
        command
            .write_all(
                &ptp_core::encode(&PtpIpPacket::OperationResponse(
                    ptp_core::OperationResponse {
                        code: ptp_core::codes::resp::OK,
                        transaction_id: 1,
                        params: vec![],
                    },
                ))
                .unwrap(),
            )
            .await
            .unwrap();

        assert!(matches!(
            PtpIpPacket::decode(&read_declared_frame(&mut event).await),
            Ok(PtpIpPacket::ProbeRequest(_))
        ));
        event
            .write_all(
                &ptp_core::encode(&PtpIpPacket::Event(ptp_core::EventPacket {
                    code: 0xc005,
                    transaction_id: 0,
                    params: vec![],
                }))
                .unwrap(),
            )
            .await
            .unwrap();
        event
            .write_all(
                &ptp_core::encode(&PtpIpPacket::ProbeResponse(ptp_core::ProbeResponse)).unwrap(),
            )
            .await
            .unwrap();

        assert!(matches!(
            PtpIpPacket::decode(&read_declared_frame(&mut command).await),
            Ok(PtpIpPacket::OperationRequest(request))
                if request.code == ptp_core::codes::op::CLOSE_SESSION
                    && request.transaction_id == 2
        ));
        command
            .write_all(
                &ptp_core::encode(&PtpIpPacket::OperationResponse(
                    ptp_core::OperationResponse {
                        code: ptp_core::codes::resp::OK,
                        transaction_id: 2,
                        params: vec![],
                    },
                ))
                .unwrap(),
            )
            .await
            .unwrap();
    });

    let store = ConfigStore::from_bundle(body, None).expect("load standard manifest");
    let transport = NativePtpTransport::new(
        store,
        TransportConfig {
            camera: Some(IpAddr::V4(Ipv4Addr::LOCALHOST)),
            interface: None,
            connection: "app".into(),
            runtime_scope: vec![],
            connect_timeout: Duration::from_secs(2),
            max_frame_bytes: 1024 * 1024,
        },
        Arc::new(TraceWriter::new(TraceFormat::Jsonl, Box::new(io::sink()))),
    )
    .unwrap();
    transport.open_command_session().await.unwrap();
    transport.probe_event_channel().await.unwrap();
    let retained = PtpExecutorTransport::next_event_frame(transport.as_ref(), 0xc005)
        .await
        .expect("racing event was retained");
    assert!(matches!(
        PtpIpPacket::decode(&retained),
        Ok(PtpIpPacket::Event(event)) if event.code == 0xc005
    ));
    transport.close_session_if_open().await.unwrap();
    camera_task.await.unwrap();
}

#[cfg(target_os = "linux")]
fn linux_loopback_interface() -> (String, Ipv4Addr) {
    get_if_addrs()
        .expect("enumerate loopback interface")
        .into_iter()
        .find_map(|interface| {
            let IfAddr::V4(address) = interface.addr else {
                return None;
            };
            if !address.is_loopback() {
                return None;
            }
            let broadcast = address.broadcast.unwrap_or_else(|| {
                Ipv4Addr::from(u32::from(address.ip) | !u32::from(address.netmask))
            });
            Some((interface.name, broadcast))
        })
        .expect("usable IPv4 loopback interface")
}

#[cfg(target_os = "linux")]
async fn receive_discovery_and_notify(
    knock: &tokio::net::UdpSocket,
    callback_port: u16,
    advertised_port: u16,
) {
    let mut discovery = [0u8; 512];
    let (length, _) = tokio::time::timeout(Duration::from_secs(2), knock.recv_from(&mut discovery))
        .await
        .expect("wait for PCSS discovery")
        .expect("receive PCSS discovery");
    assert!(discovery[..length].starts_with(b"DISCOVERY * HTTP/1.1\r\n"));

    let mut callback = tokio::net::TcpStream::connect((Ipv4Addr::LOCALHOST, callback_port))
        .await
        .expect("connect PCSS callback");
    let notify = format!(
        "NOTIFY * HTTP/1.1\r\nDSC: 127.0.0.1\r\nCAMERANAME: GFX100 II\r\nDSCPORT: {advertised_port}\r\nMX: 7\r\nSERVICE: PCSS/1.0\r\n"
    );
    callback
        .write_all(notify.as_bytes())
        .await
        .expect("write PCSS NOTIFY");
    let mut acknowledgement = [0u8; 128];
    let acknowledged =
        tokio::time::timeout(Duration::from_secs(2), callback.read(&mut acknowledgement))
            .await
            .expect("wait for callback acknowledgement")
            .expect("read callback acknowledgement");
    assert!(acknowledged > 0, "callback acknowledgement was empty");
}

#[cfg(target_os = "linux")]
async fn read_declared_frame(stream: &mut tokio::net::TcpStream) -> Vec<u8> {
    let mut header = [0u8; 4];
    tokio::time::timeout(Duration::from_secs(2), stream.read_exact(&mut header))
        .await
        .expect("wait for frame header")
        .expect("read frame header");
    let length = u32::from_le_bytes(header) as usize;
    assert!(length >= 4, "invalid declared frame length {length}");
    let mut frame = vec![0u8; length];
    frame[..4].copy_from_slice(&header);
    tokio::time::timeout(Duration::from_secs(2), stream.read_exact(&mut frame[4..]))
        .await
        .expect("wait for frame body")
        .expect("read frame body");
    frame
}

#[cfg(target_os = "linux")]
fn usb_response(code: u16, transaction_id: u32) -> Vec<u8> {
    protocol_primitives::usb_ptp::encode(&ptp_core::PtpIpPacket::OperationResponse(
        ptp_core::OperationResponse {
            code,
            transaction_id,
            params: Vec::new(),
        },
    ))
    .expect("encode USB response")
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn legacy_app_recovers_stale_session_and_sends_close_sentinel() {
    const RESPONDER: [u8; 16] = [
        0x08, 0x70, 0xb0, 0x61, 0x0a, 0x8b, 0x45, 0x93, 0xb2, 0xe7, 0x93, 0x57, 0xdd, 0x36, 0xe0,
        0x50,
    ];
    let listener = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
        .await
        .expect("bind legacy manufacturer app loopback listener");
    let command = listener.local_addr().expect("loopback command address");
    let body = replace_once(
        data("fuji/xa7/xa7.yaml"),
        "command: 55740",
        format!("command: {}", command.port()),
    );
    let store = ConfigStore::from_tiers(body, Some(data("fuji/fuji.yaml")), Vec::<String>::new())
        .expect("load X-A7 legacy manufacturer app manifest");
    let media = TempMediaRoot::new();
    let observation_path = media.path().join("legacy-app-recovery.jsonl");

    let camera_task = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.expect("accept command socket");
        let init = read_declared_frame(&mut socket).await;
        assert_eq!(&init[..8], &[82, 0, 0, 0, 1, 0, 0, 0]);

        // Keep the post-GUID bytes deliberately non-PCSS-shaped: legacy manufacturer app
        // constrains only length, type, and responder GUID.
        let mut ack = vec![0xa5; 68];
        ack[0..4].copy_from_slice(&68u32.to_le_bytes());
        ack[4..8].copy_from_slice(&2u32.to_le_bytes());
        ack[8..12].copy_from_slice(&1u32.to_le_bytes());
        ack[12..28].copy_from_slice(&RESPONDER);
        socket.write_all(&ack).await.expect("write init ack");

        let open = protocol_primitives::usb_ptp::decode(&read_declared_frame(&mut socket).await)
            .expect("decode first OpenSession");
        assert!(matches!(
            open,
            ptp_core::PtpIpPacket::OperationRequest(ref request)
                if request.code == ptp_core::codes::op::OPEN_SESSION
                    && request.transaction_id == 1
        ));
        socket
            .write_all(&usb_response(
                ptp_core::codes::resp::SESSION_ALREADY_OPEN,
                1,
            ))
            .await
            .expect("write SessionAlreadyOpen");

        let close = protocol_primitives::usb_ptp::decode(&read_declared_frame(&mut socket).await)
            .expect("decode recovery CloseSession");
        assert!(matches!(
            close,
            ptp_core::PtpIpPacket::OperationRequest(ref request)
                if request.code == ptp_core::codes::op::CLOSE_SESSION
                    && request.transaction_id == 2
        ));
        socket
            .write_all(&usb_response(ptp_core::codes::resp::OK, 2))
            .await
            .expect("ack recovery CloseSession");

        let retry = protocol_primitives::usb_ptp::decode(&read_declared_frame(&mut socket).await)
            .expect("decode retried OpenSession");
        assert!(matches!(
            retry,
            ptp_core::PtpIpPacket::OperationRequest(ref request)
                if request.code == ptp_core::codes::op::OPEN_SESSION
                    && request.transaction_id == 1
        ));
        socket
            .write_all(&usb_response(ptp_core::codes::resp::OK, 1))
            .await
            .expect("ack retried OpenSession");

        let cleanup = protocol_primitives::usb_ptp::decode(&read_declared_frame(&mut socket).await)
            .expect("decode cleanup CloseSession");
        assert!(matches!(
            cleanup,
            ptp_core::PtpIpPacket::OperationRequest(ref request)
                if request.code == ptp_core::codes::op::CLOSE_SESSION
                    && request.transaction_id == 2
        ));
        socket
            .write_all(&usb_response(ptp_core::codes::resp::OK, 2))
            .await
            .expect("ack cleanup CloseSession");
        assert_eq!(
            read_declared_frame(&mut socket).await,
            vec![0x08, 0, 0, 0, 0xff, 0xff, 0xff, 0xff]
        );
    });

    let trace_buffer = TraceBuffer::default();
    let trace = Arc::new(TraceWriter::with_observations(
        TraceFormat::Jsonl,
        Box::new(trace_buffer.clone()),
        observation_recorder(observation_path.clone(), "legacy-app-session-recovery"),
        "legacy-app".into(),
        "connection".into(),
    ));
    let transport = NativePtpTransport::new(
        Arc::clone(&store),
        TransportConfig {
            camera: Some(IpAddr::V4(Ipv4Addr::LOCALHOST)),
            interface: None,
            connection: "legacy-app".into(),
            runtime_scope: vec![("terminalName".into(), "ptpsim".into())],
            connect_timeout: Duration::from_secs(2),
            max_frame_bytes: 1024 * 1024,
        },
        trace,
    )
    .expect("construct legacy manufacturer app transport");
    let opened = transport
        .open_command_session()
        .await
        .expect("recover stale legacy manufacturer app session");
    assert_eq!(opened.transaction_id, 1);
    transport
        .close_session_if_open()
        .await
        .expect("send orderly legacy manufacturer app cleanup");
    camera_task.await.expect("join legacy manufacturer app responder");
    assert!(trace_buffer.records().iter().any(|record| {
        record.get("state").and_then(serde_json::Value::as_str) == Some("sessionAlreadyOpen")
    }));

    let observation_bundle = std::fs::read_to_string(observation_path)
        .expect("reconstruct legacy manufacturer app recovery observation bundle");
    let validated = validate_bundles(&[&observation_bundle])
        .expect("legacy manufacturer app recovery observations are canonical");
    let transactions = validated
        .records
        .iter()
        .filter_map(|record| match record {
            ObservationLine::PtpTransaction(transaction) => Some(transaction),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(transactions.len(), 4);
    assert!(transactions
        .iter()
        .all(|transaction| transaction.connection_instance == transactions[0].connection_instance));
    let tid_one_sessions = transactions
        .iter()
        .filter(|transaction| transaction.transaction_id == 1)
        .map(|transaction| transaction.session.as_str())
        .collect::<Vec<_>>();
    assert_eq!(tid_one_sessions.len(), 2);
    assert_ne!(tid_one_sessions[0], tid_one_sessions[1]);
}

#[tokio::test]
async fn native_transport_runs_real_gfx_entry_over_tcp() {
    let body = data("fuji/gfx100ii/gfx100ii.yaml");
    let media = TempMediaRoot::new();
    let live_view = media.path().join("liveview");
    std::fs::create_dir_all(&live_view).expect("create live-view fixture directory");
    std::fs::write(
        live_view.join("frame.jpg"),
        b"\xFF\xD8\xFF\xE0LOOPBACK\xFF\xD9",
    )
    .expect("write live-view fixture");
    let loopback = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0);
    let server = Server::bind(Config {
        instance_id: "camera-initiator-loopback".into(),
        profile: "fuji/gfx100ii/fw0230".into(),
        connection: "app".into(),
        manifest_yaml: body.clone(),
        media_root: media.path().to_path_buf(),
        command_bind: Some(loopback),
        liveview_bind: Some(loopback),
        event_bind: Some(loopback),
        knock_bind: None,
        pcss_init_fails: 0,
        pcss_shutter_enqueue_count: 0,
        control_bind: loopback,
        liveview_dir: Some(live_view),
        state_callback: None,
    })
    .await
    .expect("bind simulator service");

    let command = server.command_addr();
    let event = server.event_addr_opt().expect("app event listener");
    let live_view = server.liveview_addr_opt().expect("app live-view listener");
    let body = with_loopback_ports(body, command, event, live_view);
    let store = ConfigStore::from_tiers(body, Some(data("fuji/fuji.yaml")), Vec::<String>::new())
        .expect("load GFX100 II manifest tiers");

    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
    let server_task = tokio::spawn(server.run(shutdown_rx));
    let trace = Arc::new(TraceWriter::new(
        TraceFormat::Jsonl,
        Box::new(std::io::sink()),
    ));
    let transport = NativePtpTransport::new(
        Arc::clone(&store),
        TransportConfig {
            camera: Some(IpAddr::V4(Ipv4Addr::LOCALHOST)),
            interface: None,
            connection: "app".into(),
            runtime_scope: vec![("terminalName".into(), "ptpsim".into())],
            connect_timeout: Duration::from_secs(2),
            max_frame_bytes: 1024 * 1024,
        },
        Arc::clone(&trace),
    )
    .expect("construct native transport");

    let opened = transport
        .open_command_session_after_handoff(Instant::now() + Duration::from_secs(2))
        .await
        .expect("retain the first restored reference app socket through init and OpenSession");
    assert_eq!(opened.transaction_id, 1);
    assert_eq!(opened.response_code, 0x2001);

    let raw_transport: Arc<dyn PtpExecutorTransport> = transport.clone();
    let outcome = run_mode_entry(
        store,
        "app".into(),
        None,
        "shooting/stills".into(),
        raw_transport,
        Arc::new(NoopObserver),
        Arc::new(NoopObserver),
        Vec::new(),
    )
    .await
    .expect("execute cold shooting entry through FFI executor");

    assert_eq!(outcome.steps_run, 7);
    assert!(outcome.scope.iter().any(|value| {
        value.key == "openCaptureTxId" && value.value > opened.transaction_id as u64
    }));
    assert!(
        transport
            .confirm_live_view_frame()
            .await
            .expect("read frame after manifest opened live-view channel")
            > 0
    );
    transport
        .close_session_if_open()
        .await
        .expect("close PTP session cleanly");

    let _ = shutdown_tx.send(());
    server_task.await.expect("join simulator service");
}

#[tokio::test]
async fn native_transport_runs_pcss_rendezvous_retry_and_action() {
    let body = data("fuji/gfx100ii/gfx100ii.yaml");
    let media = TempMediaRoot::new();
    let loopback = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0);

    // Keep the OS-selected callback port reserved until immediately before the
    // initiator binds it. The simulator learns this port from the same in-memory
    // manifest contract the initiator consumes.
    let callback_reservation =
        std::net::TcpListener::bind(loopback).expect("reserve ephemeral PCSS callback port");
    let callback_port = callback_reservation
        .local_addr()
        .expect("reserved callback address")
        .port();
    let server_body = replace_once(
        body,
        "callbackPort: 51560",
        format!("callbackPort: {callback_port}"),
    );

    let server = Server::bind(Config {
        instance_id: "camera-initiator-pcss-loopback".into(),
        profile: "fuji/gfx100ii/fw0230".into(),
        connection: "wireless-tether".into(),
        manifest_yaml: server_body.clone(),
        media_root: media.path().to_path_buf(),
        command_bind: Some(loopback),
        liveview_bind: None,
        event_bind: None,
        knock_bind: Some(loopback),
        pcss_init_fails: 2,
        pcss_shutter_enqueue_count: 0,
        control_bind: loopback,
        liveview_dir: None,
        state_callback: None,
    })
    .await
    .expect("bind PCSS simulator service");

    let command = server.command_addr();
    let knock = server.knock_addr_opt().expect("PCSS knock listener");
    let nominal_command_port = if command.port() == u16::MAX {
        command.port() - 1
    } else {
        command.port() + 1
    };
    let store_body = replace_once(
        server_body,
        "knockPort: 51562",
        format!("knockPort: {}", knock.port()),
    );
    let store_body = replace_once(
        store_body,
        "bindings: { command: 15740 }",
        format!("bindings: {{ command: {nominal_command_port} }}"),
    );
    let store_body = replace_once(
        store_body,
        "retryIntervalMs: 1000",
        "retryIntervalMs: 50".into(),
    );
    let store = ConfigStore::from_tiers(
        store_body,
        Some(data("fuji/fuji.yaml")),
        Vec::<String>::new(),
    )
    .expect("load PCSS loopback manifest tiers");

    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
    let server_task = tokio::spawn(server.run(shutdown_rx));
    let trace_buffer = TraceBuffer::default();
    let observation_path = media.path().join("initiator-observation.jsonl");
    let trace = Arc::new(TraceWriter::with_observations(
        TraceFormat::Jsonl,
        Box::new(trace_buffer.clone()),
        observation_recorder(observation_path.clone(), "pcss-initiator-loopback"),
        "wireless-tether".into(),
        "connection".into(),
    ));
    let transport = NativePtpTransport::new(
        Arc::clone(&store),
        TransportConfig {
            camera: Some(IpAddr::V4(Ipv4Addr::LOCALHOST)),
            interface: None,
            connection: "wireless-tether".into(),
            runtime_scope: vec![("terminalName".into(), "ptpsim".into())],
            connect_timeout: Duration::from_secs(2),
            max_frame_bytes: 1024 * 1024,
        },
        Arc::clone(&trace),
    )
    .expect("construct native PCSS transport");
    assert_eq!(
        transport
            .command_endpoint()
            .expect("configured endpoint")
            .port(),
        nominal_command_port
    );
    assert_ne!(
        transport
            .command_endpoint()
            .expect("configured endpoint")
            .port(),
        command.port()
    );

    drop(callback_reservation);
    let opened = transport
        .open_command_session()
        .await
        .expect("perform PCSS knock, callback, InitFail retries, and OpenSession");
    assert_eq!(opened.transaction_id, 1);
    assert_eq!(opened.response_code, 0x2001);

    let raw_transport: Arc<dyn PtpExecutorTransport> = transport.clone();
    let catalog = store.action_catalog();
    let outcome = run_initiator_action(
        Arc::clone(&store),
        ActionInvocationRequest {
            catalog_revision: catalog.revision,
            action_id: "readDeviceInfo".into(),
            connection: "wireless-tether".into(),
            mode: String::new(),
            role: ActionRole::Initiator,
            parameters: Vec::new(),
        },
        raw_transport,
        Arc::new(TraceObserver(Arc::clone(&trace))),
        Arc::new(NoopObserver),
    )
    .await
    .expect("run wireless-tether action through FFI executor");
    assert_eq!(outcome.steps_run, 1);
    assert_eq!(outcome.outputs.len(), 1);
    assert_eq!(outcome.outputs[0].operation, 0x1001);
    assert_eq!(outcome.outputs[0].transaction_id, 2);
    assert!(!outcome.outputs[0].payload.is_empty());

    transport
        .close_session_if_open()
        .await
        .expect("close PCSS PTP session cleanly");

    let records = trace_buffer.records();
    let retries: Vec<&serde_json::Value> = records
        .iter()
        .filter(|record| {
            record.get("state").and_then(serde_json::Value::as_str) == Some("initRetry")
        })
        .collect();
    assert_eq!(retries.len(), 2);
    assert!(retries.iter().all(|record| {
        record
            .pointer("/detail/reason")
            .and_then(serde_json::Value::as_u64)
            == Some(0x2019)
    }));
    assert!(records.iter().any(|record| {
        record.get("channel").and_then(serde_json::Value::as_str) == Some("pcssDiscovery")
            && record.get("direction").and_then(serde_json::Value::as_str) == Some("tx")
    }));

    let observation_bundle = std::fs::read_to_string(observation_path).unwrap();
    let validated = validate_bundles(&[&observation_bundle]).unwrap();
    assert!(validated.records.iter().any(|record| {
        matches!(
            record,
            ObservationLine::PtpTransaction(transaction)
                if transaction.request.operation == "0x1001"
                    && transaction.request.parameters.is_empty()
                    && transaction
                        .response
                        .as_ref()
                        .and_then(|response| response.data.as_ref())
                        .is_some_and(|data| data.payload.length > 0)
        )
    }));
    assert!(records.iter().any(|record| {
        record.get("channel").and_then(serde_json::Value::as_str) == Some("pcssCallback")
            && record.get("direction").and_then(serde_json::Value::as_str) == Some("rx")
    }));

    let _ = shutdown_tx.send(());
    server_task.await.expect("join PCSS simulator service");
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn native_transport_recovers_broadcast_discovery_with_unicast() {
    let body = data("fuji/gfx100ii/gfx100ii.yaml");
    let media = TempMediaRoot::new();
    let loopback = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0);
    let any_ipv4 = SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0);
    let (loopback_name, loopback_broadcast) = get_if_addrs()
        .expect("enumerate loopback interface")
        .into_iter()
        .find_map(|interface| {
            let IfAddr::V4(address) = interface.addr else {
                return None;
            };
            if !address.is_loopback() {
                return None;
            }
            let broadcast = address.broadcast.unwrap_or_else(|| {
                Ipv4Addr::from(u32::from(address.ip) | !u32::from(address.netmask))
            });
            Some((interface.name, broadcast))
        })
        .expect("usable IPv4 loopback interface");

    let callback_reservation =
        std::net::TcpListener::bind(loopback).expect("reserve ephemeral PCSS callback port");
    let callback_port = callback_reservation
        .local_addr()
        .expect("reserved callback address")
        .port();
    let server_body = replace_once(
        body,
        "callbackPort: 51560",
        format!("callbackPort: {callback_port}"),
    );
    let knock = tokio::net::UdpSocket::bind(any_ipv4)
        .await
        .expect("bind custom PCSS knock responder");
    let knock_port = knock.local_addr().expect("PCSS knock address").port();
    let unavailable_command =
        std::net::TcpListener::bind(loopback).expect("reserve unavailable first command endpoint");
    let unavailable_command_port = unavailable_command
        .local_addr()
        .expect("unavailable command address")
        .port();
    let server = Server::bind(Config {
        instance_id: "camera-initiator-pcss-broadcast-loopback".into(),
        profile: "fuji/gfx100ii/fw0230".into(),
        connection: "wireless-tether".into(),
        manifest_yaml: server_body.clone(),
        media_root: media.path().to_path_buf(),
        command_bind: Some(loopback),
        liveview_bind: None,
        event_bind: None,
        knock_bind: None,
        pcss_init_fails: 0,
        pcss_shutter_enqueue_count: 0,
        control_bind: loopback,
        liveview_dir: None,
        state_callback: None,
    })
    .await
    .expect("bind broadcast PCSS simulator service");

    let command = server.command_addr();
    let store_body = replace_once(
        replace_once(
            server_body,
            "knockPort: 51562",
            format!("knockPort: {knock_port}"),
        ),
        "bindings: { command: 15740 }",
        format!("bindings: {{ command: {} }}", command.port()),
    );
    let store = ConfigStore::from_tiers(
        store_body,
        Some(data("fuji/fuji.yaml")),
        Vec::<String>::new(),
    )
    .expect("load broadcast PCSS loopback manifest tiers");

    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
    let server_task = tokio::spawn(server.run(shutdown_rx));
    let responder_task = tokio::spawn(async move {
        for advertised_port in [unavailable_command_port, command.port()] {
            let mut discovery = [0u8; 512];
            let (length, _) =
                tokio::time::timeout(Duration::from_secs(2), knock.recv_from(&mut discovery))
                    .await
                    .expect("wait for PCSS discovery")
                    .expect("receive PCSS discovery");
            assert!(discovery[..length].starts_with(b"DISCOVERY * HTTP/1.1\r\n"));

            let mut callback = tokio::net::TcpStream::connect((Ipv4Addr::LOCALHOST, callback_port))
                .await
                .expect("connect PCSS callback");
            let notify = format!(
                "NOTIFY * HTTP/1.1\r\nDSC: 127.0.0.1\r\nCAMERANAME: GFX100 II\r\nDSCPORT: {advertised_port}\r\nMX: 7\r\nSERVICE: PCSS/1.0\r\n"
            );
            callback
                .write_all(notify.as_bytes())
                .await
                .expect("write PCSS NOTIFY");
            let mut acknowledgement = [0u8; 128];
            let acknowledged =
                tokio::time::timeout(Duration::from_secs(2), callback.read(&mut acknowledgement))
                    .await
                    .expect("wait for callback acknowledgement")
                    .expect("read callback acknowledgement");
            assert!(acknowledged > 0, "callback acknowledgement was empty");
        }
    });
    let trace_buffer = TraceBuffer::default();
    let trace = Arc::new(TraceWriter::new(
        TraceFormat::Jsonl,
        Box::new(trace_buffer.clone()),
    ));
    let transport = NativePtpTransport::new(
        Arc::clone(&store),
        TransportConfig {
            camera: None,
            interface: Some(loopback_name),
            connection: "wireless-tether".into(),
            runtime_scope: vec![("terminalName".into(), "ptpsim".into())],
            connect_timeout: Duration::from_secs(2),
            max_frame_bytes: 1024 * 1024,
        },
        trace,
    )
    .expect("construct broadcast PCSS transport");

    drop(callback_reservation);
    drop(unavailable_command);
    let opened = transport
        .open_command_session()
        .await
        .expect("recover broadcast discovery through learned unicast");
    assert_eq!(opened.response_code, 0x2001);
    transport
        .close_session_if_open()
        .await
        .expect("close broadcast PCSS session cleanly");

    let records = trace_buffer.records();
    assert!(records.iter().any(|record| {
        record.get("state").and_then(serde_json::Value::as_str) == Some("pcssDiscoverySent")
            && record
                .pointer("/detail/mode")
                .and_then(serde_json::Value::as_str)
                == Some("subnetBroadcast")
            && record
                .pointer("/detail/destination")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|destination| {
                    destination.starts_with(&format!("{loopback_broadcast}:"))
                })
    }));
    assert!(records.iter().any(|record| {
        record.get("state").and_then(serde_json::Value::as_str) == Some("pcssRediscovery")
    }));
    assert!(records.iter().any(|record| {
        record.get("state").and_then(serde_json::Value::as_str) == Some("pcssDiscoverySent")
            && record
                .pointer("/detail/mode")
                .and_then(serde_json::Value::as_str)
                == Some("explicitUnicast")
    }));
    assert!(records.iter().any(|record| {
        record.get("state").and_then(serde_json::Value::as_str) == Some("pcssCallbackAccepted")
            && record
                .pointer("/detail/dsc")
                .and_then(serde_json::Value::as_str)
                == Some("127.0.0.1")
    }));

    let _ = shutdown_tx.send(());
    server_task
        .await
        .expect("join broadcast PCSS simulator service");
    responder_task
        .await
        .expect("join custom PCSS knock responder");
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn native_transport_uses_first_broadcast_callback_without_rediscovery() {
    let body = data("fuji/gfx100ii/gfx100ii.yaml");
    let media = TempMediaRoot::new();
    let loopback = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0);
    let any_ipv4 = SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0);
    let (loopback_name, loopback_broadcast) = linux_loopback_interface();

    let callback_reservation =
        std::net::TcpListener::bind(loopback).expect("reserve ephemeral PCSS callback port");
    let callback_port = callback_reservation
        .local_addr()
        .expect("reserved callback address")
        .port();
    let server_body = replace_once(
        body,
        "callbackPort: 51560",
        format!("callbackPort: {callback_port}"),
    );
    let knock = tokio::net::UdpSocket::bind(any_ipv4)
        .await
        .expect("bind custom PCSS knock responder");
    let knock_port = knock.local_addr().expect("PCSS knock address").port();
    let server = Server::bind(Config {
        instance_id: "camera-initiator-pcss-direct-broadcast-loopback".into(),
        profile: "fuji/gfx100ii/fw0230".into(),
        connection: "wireless-tether".into(),
        manifest_yaml: server_body.clone(),
        media_root: media.path().to_path_buf(),
        command_bind: Some(loopback),
        liveview_bind: None,
        event_bind: None,
        knock_bind: None,
        pcss_init_fails: 0,
        pcss_shutter_enqueue_count: 0,
        control_bind: loopback,
        liveview_dir: None,
        state_callback: None,
    })
    .await
    .expect("bind direct-broadcast PCSS simulator service");

    let command = server.command_addr();
    let store_body = replace_once(
        replace_once(
            server_body,
            "knockPort: 51562",
            format!("knockPort: {knock_port}"),
        ),
        "bindings: { command: 15740 }",
        format!("bindings: {{ command: {} }}", command.port()),
    );
    let store = ConfigStore::from_tiers(
        store_body,
        Some(data("fuji/fuji.yaml")),
        Vec::<String>::new(),
    )
    .expect("load direct-broadcast PCSS loopback manifest tiers");

    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
    let server_task = tokio::spawn(server.run(shutdown_rx));
    let responder_task = tokio::spawn(async move {
        receive_discovery_and_notify(&knock, callback_port, command.port()).await;
    });
    let trace_buffer = TraceBuffer::default();
    let trace = Arc::new(TraceWriter::new(
        TraceFormat::Jsonl,
        Box::new(trace_buffer.clone()),
    ));
    let transport = NativePtpTransport::new(
        Arc::clone(&store),
        TransportConfig {
            camera: None,
            interface: Some(loopback_name),
            connection: "wireless-tether".into(),
            runtime_scope: vec![("terminalName".into(), "ptpsim".into())],
            connect_timeout: Duration::from_secs(2),
            max_frame_bytes: 1024 * 1024,
        },
        trace,
    )
    .expect("construct direct-broadcast PCSS transport");

    drop(callback_reservation);
    let opened = transport
        .open_command_session()
        .await
        .expect("use the command endpoint from the first broadcast callback");
    assert_eq!(opened.response_code, 0x2001);
    transport
        .close_session_if_open()
        .await
        .expect("close direct-broadcast PCSS session cleanly");

    let records = trace_buffer.records();
    let discoveries: Vec<&serde_json::Value> = records
        .iter()
        .filter(|record| {
            record.get("state").and_then(serde_json::Value::as_str) == Some("pcssDiscoverySent")
        })
        .collect();
    assert_eq!(discoveries.len(), 1);
    assert_eq!(
        discoveries[0]
            .pointer("/detail/mode")
            .and_then(serde_json::Value::as_str),
        Some("subnetBroadcast")
    );
    assert!(discoveries[0]
        .pointer("/detail/destination")
        .and_then(serde_json::Value::as_str)
        .is_some_and(|destination| destination.starts_with(&format!("{loopback_broadcast}:"))));
    assert!(!records.iter().any(|record| {
        record.get("state").and_then(serde_json::Value::as_str) == Some("pcssRediscovery")
    }));
    assert!(!discoveries.iter().any(|record| {
        record
            .pointer("/detail/mode")
            .and_then(serde_json::Value::as_str)
            == Some("explicitUnicast")
    }));

    let _ = shutdown_tx.send(());
    server_task
        .await
        .expect("join direct-broadcast PCSS simulator service");
    responder_task
        .await
        .expect("join direct-broadcast PCSS knock responder");
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn native_transport_recovers_first_init_eof_with_learned_unicast() {
    let body = data("fuji/gfx100ii/gfx100ii.yaml");
    let media = TempMediaRoot::new();
    let loopback = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0);
    let any_ipv4 = SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0);
    let (loopback_name, _) = linux_loopback_interface();

    let callback_reservation =
        std::net::TcpListener::bind(loopback).expect("reserve ephemeral PCSS callback port");
    let callback_port = callback_reservation
        .local_addr()
        .expect("reserved callback address")
        .port();
    let server_body = replace_once(
        body,
        "callbackPort: 51560",
        format!("callbackPort: {callback_port}"),
    );
    let knock = tokio::net::UdpSocket::bind(any_ipv4)
        .await
        .expect("bind custom PCSS knock responder");
    let knock_port = knock.local_addr().expect("PCSS knock address").port();
    let first_command = tokio::net::TcpListener::bind(loopback)
        .await
        .expect("bind first command endpoint");
    let first_command_port = first_command
        .local_addr()
        .expect("first command address")
        .port();
    let server = Server::bind(Config {
        instance_id: "camera-initiator-pcss-init-eof-loopback".into(),
        profile: "fuji/gfx100ii/fw0230".into(),
        connection: "wireless-tether".into(),
        manifest_yaml: server_body.clone(),
        media_root: media.path().to_path_buf(),
        command_bind: Some(loopback),
        liveview_bind: None,
        event_bind: None,
        knock_bind: None,
        pcss_init_fails: 0,
        pcss_shutter_enqueue_count: 0,
        control_bind: loopback,
        liveview_dir: None,
        state_callback: None,
    })
    .await
    .expect("bind recovered PCSS simulator service");

    let command = server.command_addr();
    let store_body = replace_once(
        replace_once(
            server_body,
            "knockPort: 51562",
            format!("knockPort: {knock_port}"),
        ),
        "bindings: { command: 15740 }",
        format!("bindings: {{ command: {} }}", command.port()),
    );
    let store = ConfigStore::from_tiers(
        store_body,
        Some(data("fuji/fuji.yaml")),
        Vec::<String>::new(),
    )
    .expect("load init-EOF PCSS loopback manifest tiers");

    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
    let server_task = tokio::spawn(server.run(shutdown_rx));
    let responder_task = tokio::spawn(async move {
        receive_discovery_and_notify(&knock, callback_port, first_command_port).await;
        receive_discovery_and_notify(&knock, callback_port, command.port()).await;
    });
    let first_command_task = tokio::spawn(async move {
        let (mut socket, _) = tokio::time::timeout(Duration::from_secs(2), first_command.accept())
            .await
            .expect("wait for first command connection")
            .expect("accept first command connection");
        let mut init = [0u8; 82];
        tokio::time::timeout(Duration::from_secs(2), socket.read_exact(&mut init))
            .await
            .expect("wait for first init request")
            .expect("read first init request");
        assert_eq!(&init[..8], &[82, 0, 0, 0, 1, 0, 0, 0]);
        // Drop the socket without an InitCommandAck. The initiator must treat
        // this first-init EOF as endpoint failure and rediscover exactly once.
    });
    let trace_buffer = TraceBuffer::default();
    let trace = Arc::new(TraceWriter::new(
        TraceFormat::Jsonl,
        Box::new(trace_buffer.clone()),
    ));
    let transport = NativePtpTransport::new(
        Arc::clone(&store),
        TransportConfig {
            camera: None,
            interface: Some(loopback_name),
            connection: "wireless-tether".into(),
            runtime_scope: vec![("terminalName".into(), "ptpsim".into())],
            connect_timeout: Duration::from_secs(2),
            max_frame_bytes: 1024 * 1024,
        },
        trace,
    )
    .expect("construct init-EOF PCSS transport");

    drop(callback_reservation);
    let opened = transport
        .open_command_session()
        .await
        .expect("recover first-init EOF through learned unicast");
    assert_eq!(opened.response_code, 0x2001);
    transport
        .close_session_if_open()
        .await
        .expect("close recovered PCSS session cleanly");

    let records = trace_buffer.records();
    let discoveries: Vec<&serde_json::Value> = records
        .iter()
        .filter(|record| {
            record.get("state").and_then(serde_json::Value::as_str) == Some("pcssDiscoverySent")
        })
        .collect();
    assert_eq!(discoveries.len(), 2);
    assert_eq!(
        discoveries[0]
            .pointer("/detail/mode")
            .and_then(serde_json::Value::as_str),
        Some("subnetBroadcast")
    );
    assert_eq!(
        discoveries[1]
            .pointer("/detail/mode")
            .and_then(serde_json::Value::as_str),
        Some("explicitUnicast")
    );
    let rediscoveries: Vec<&serde_json::Value> = records
        .iter()
        .filter(|record| {
            record.get("state").and_then(serde_json::Value::as_str) == Some("pcssRediscovery")
        })
        .collect();
    assert_eq!(rediscoveries.len(), 1);
    assert_eq!(
        rediscoveries[0]
            .pointer("/detail/reason")
            .and_then(serde_json::Value::as_str),
        Some("commandSessionUnavailable")
    );

    let _ = shutdown_tx.send(());
    server_task
        .await
        .expect("join recovered PCSS simulator service");
    responder_task
        .await
        .expect("join recovered PCSS knock responder");
    first_command_task
        .await
        .expect("join first command endpoint");
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn native_transport_rejects_invalid_pcss_names_before_discovery() {
    let body = data("fuji/gfx100ii/gfx100ii.yaml");
    let loopback = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0);
    let any_ipv4 = SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0);
    let (loopback_name, _) = linux_loopback_interface();

    let callback_reservation =
        std::net::TcpListener::bind(loopback).expect("reserve ephemeral PCSS callback port");
    let callback_port = callback_reservation
        .local_addr()
        .expect("reserved callback address")
        .port();
    let knock = tokio::net::UdpSocket::bind(any_ipv4)
        .await
        .expect("bind PCSS discovery observer");
    let knock_port = knock.local_addr().expect("PCSS knock address").port();
    let store_body = replace_once(
        replace_once(
            body,
            "callbackPort: 51560",
            format!("callbackPort: {callback_port}"),
        ),
        "knockPort: 51562",
        format!("knockPort: {knock_port}"),
    );
    let store = ConfigStore::from_tiers(
        store_body,
        Some(data("fuji/fuji.yaml")),
        Vec::<String>::new(),
    )
    .expect("load invalid-identity PCSS loopback manifest tiers");
    drop(callback_reservation);
    for (terminal_name, expected_error) in [
        (
            "thirteen-units",
            "PCSS init hostname exceeds 12 UTF-16 code units",
        ),
        ("invalid\0name", "PCSS init hostname is not valid UTF-16LE"),
    ] {
        let trace_buffer = TraceBuffer::default();
        let trace = Arc::new(TraceWriter::new(
            TraceFormat::Jsonl,
            Box::new(trace_buffer.clone()),
        ));
        let transport = NativePtpTransport::new(
            Arc::clone(&store),
            TransportConfig {
                camera: None,
                interface: Some(loopback_name.clone()),
                connection: "wireless-tether".into(),
                runtime_scope: vec![("terminalName".into(), terminal_name.into())],
                connect_timeout: Duration::from_millis(100),
                max_frame_bytes: 1024 * 1024,
            },
            trace,
        )
        .expect("construct invalid-identity PCSS transport");

        let error = transport
            .open_command_session()
            .await
            .expect_err("reject an invalid PCSS terminal name");
        assert!(
            error.to_string().contains(expected_error),
            "unexpected identity validation error: {error}"
        );

        let mut datagram = [0u8; 512];
        assert!(
            tokio::time::timeout(Duration::from_millis(100), knock.recv_from(&mut datagram))
                .await
                .is_err(),
            "invalid identity emitted a PCSS discovery datagram"
        );
        assert!(!trace_buffer.records().iter().any(|record| {
            record.get("state").and_then(serde_json::Value::as_str) == Some("pcssDiscoverySent")
        }));
    }
}
