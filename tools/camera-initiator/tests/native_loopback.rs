use std::io::{self, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use camera_config::{
    direct_epistemic, no_loss, validate_bundles, BundleHeader, CameraContext, CaptureClock,
    CaptureContext, CaptureInterface, CaptureInterfaceType, ClientContext, ClockType, ClockUnit,
    ObservationLine, ObservationRecorder, PayloadMetadataBuilder,
};
use camera_initiator::{
    prepare_session_plan, run_session_plan, NativePtpTransport, NativeSessionLifecycle,
    SessionPlan, SessionRunOptions, TraceFormat, TraceWriter, TransportConfig,
};
use camera_protocol_ffi::{
    run_initiator_action, run_mode_entry, ActionInvocationRequest, ActionRole, ConfigStore,
    ConnectionActivityEvent, ConnectionActivityObserver, PtpExecutorTransport, StepObserver,
    StepReport,
};
use camera_sim_service::{Config, Server};
#[cfg(target_os = "linux")]
use if_addrs::{get_if_addrs, IfAddr};
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

/// Creates a media root that belongs to one test. `tempfile` creates the
/// directory exclusively under a random name, so parallel tests never share a
/// root and each test removes only its own root when the guard drops.
fn temp_media_root() -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix("ptpsim-camera-initiator-")
        .tempdir()
        .expect("create temporary media root")
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
    let body = replace_once(
        body,
        "command: 55740",
        format!("command: {}", command.port()),
    );
    let body = replace_once(
        body,
        "event:\n        port: 55741",
        format!("event:\n        port: {}", event.port()),
    );
    replace_once(
        body,
        "liveView:\n        port: 55742",
        format!("liveView:\n        port: {}", live_view.port()),
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
    let media = temp_media_root();
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
async fn connect_named_pcss_notify(
    callback_port: u16,
    camera_name: &str,
    advertised_port: u16,
) -> tokio::net::TcpStream {
    let mut callback = tokio::net::TcpStream::connect((Ipv4Addr::LOCALHOST, callback_port))
        .await
        .expect("connect PCSS callback");
    let notify = format!(
        "NOTIFY * HTTP/1.1\r\nDSC: 127.0.0.1\r\nCAMERANAME: {camera_name}\r\nDSCPORT: {advertised_port}\r\nMX: 7\r\nSERVICE: PCSS/1.0\r\n"
    );
    callback
        .write_all(notify.as_bytes())
        .await
        .expect("write PCSS NOTIFY");
    callback
}

#[cfg(target_os = "linux")]
async fn pcss_callback_was_acknowledged(mut callback: tokio::net::TcpStream) -> bool {
    let mut acknowledgement = [0u8; 128];
    matches!(
        tokio::time::timeout(
            Duration::from_millis(250),
            callback.read(&mut acknowledgement)
        )
        .await,
        Ok(Ok(length)) if length > 0
    )
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

#[tokio::test]
async fn native_transport_runs_real_gfx_entry_over_tcp() {
    let body = data("fuji/gfx100ii/gfx100ii.yaml");
    let media = temp_media_root();
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
async fn session_runner_retains_gfx_entry_and_autofocus_actions() {
    let body = data("fuji/gfx100ii/gfx100ii.yaml");
    let media = temp_media_root();
    let live_view = media.path().join("session-liveview");
    std::fs::create_dir_all(&live_view).expect("create session live-view fixtures");
    std::fs::write(
        live_view.join("frame.jpg"),
        [
            0xff, 0xd8, 0xff, 0xc0, 0, 11, 8, 1, 224, 2, 128, 1, 1, 0x11, 0, 0xff, 0xda, 0, 8, 1,
            1, 0, 0, 63, 0, 1, 2, 3, 0xff, 0xd9,
        ],
    )
    .expect("write session live-view fixture");
    let loopback = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0);
    let server = Server::bind(Config {
        instance_id: "camera-initiator-session-loopback".into(),
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
    .expect("bind session simulator service");

    let command = server.command_addr();
    let event = server.event_addr_opt().expect("session event listener");
    let live_view = server
        .liveview_addr_opt()
        .expect("session live-view listener");
    let body = with_loopback_ports(body, command, event, live_view);
    let store = ConfigStore::from_tiers(body, Some(data("fuji/fuji.yaml")), Vec::new())
        .expect("load session manifest tiers");
    let plan = SessionPlan::from_yaml(
        r#"
schema: camera-initiator-session/v1
steps:
  - { id: enter-stills, kind: entry, to: shooting/stills }
  - id: focus-lock
    kind: action
    action: autofocusLock
    parameters: { afArea: 7 }
  - { id: focus-release, kind: action, action: autofocusRelease }
  - { id: frames, kind: samplePushFrames, frameCount: 2, maxFrameBytes: 4096, maxTotalBytes: 8192, timeoutMs: 2000 }
"#,
    )
    .expect("parse session plan");
    let prepared = prepare_session_plan(plan, &store, "app").expect("prepare session plan");

    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
    let server_task = tokio::spawn(server.run(shutdown_rx));
    let observation_path = media.path().join("session-observation.jsonl");
    let trace = Arc::new(TraceWriter::with_observations(
        TraceFormat::Jsonl,
        Box::new(io::sink()),
        observation_recorder(observation_path.clone(), "session-loopback"),
        "app".into(),
        "session".into(),
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
    .expect("construct session transport");
    let output_dir = media.path().join("session-output");
    let report = run_session_plan(
        &prepared,
        Arc::clone(&store),
        NativeSessionLifecycle::new(Arc::clone(&transport)),
        trace,
        SessionRunOptions {
            output_dir: output_dir.clone(),
            run_id: "session-loopback".into(),
            connection: "app".into(),
            trace_path: "-".into(),
            observation_path: observation_path.clone(),
            runtime_params: Vec::new(),
            handoff_timeout: Duration::from_secs(2),
        },
    )
    .await
    .expect("run retained session plan");

    assert_eq!(report.steps.len(), 4);
    assert_eq!(
        report
            .steps
            .iter()
            .map(|step| step.id.as_str())
            .collect::<Vec<_>>(),
        ["enter-stills", "focus-lock", "focus-release", "frames"]
    );
    assert!(report
        .steps
        .iter()
        .all(|step| step.session_index == Some(1)));
    let frames = report.steps[3].frames.as_ref().expect("push frame report");
    assert_eq!(
        frames.termination,
        camera_initiator::push_frames::PushFrameTermination::FrameCount
    );
    assert_eq!(frames.frames.len(), 2);
    for frame in &frames.frames {
        assert!(output_dir.join(&frame.path).is_file());
        assert_eq!(frame.jpeg.width, 640);
        assert_eq!(frame.jpeg.height, 480);
    }
    assert!(report.steps[3].transaction_ids.is_empty());
    assert!(report.cleanup_warning.is_none());
    assert!(
        transport.confirm_live_view_frame().await.is_err(),
        "cleanup closes the push socket"
    );
    let lock_ids = &report.steps[1].transaction_ids;
    let release_ids = &report.steps[2].transaction_ids;
    assert!(!lock_ids.is_empty());
    assert!(!release_ids.is_empty());
    assert!(lock_ids.windows(2).all(|ids| ids[0] < ids[1]));
    assert!(release_ids.windows(2).all(|ids| ids[0] < ids[1]));
    assert!(lock_ids.last().unwrap() < release_ids.first().unwrap());
    assert!(output_dir.join("session-report.json").is_file());

    let observation_bundle = std::fs::read_to_string(observation_path).unwrap();
    let validated = validate_bundles(&[&observation_bundle]).expect("validate session bundle");
    let actions: Vec<_> = validated
        .records
        .iter()
        .filter_map(|record| match record {
            ObservationLine::ActionInvocation(action) => Some(action),
            _ => None,
        })
        .collect();
    assert_eq!(actions.len(), 2);
    assert_eq!(actions[0].common.run_id, "session-loopback");
    assert_eq!(actions[0].action_id, "autofocusLock");
    assert_eq!(
        actions[0].parameters.get("afArea"),
        Some(&serde_json::json!(7))
    );
    assert_eq!(actions[1].action_id, "autofocusRelease");
    assert_eq!(actions[1].common.run_id, "session-loopback");

    let failure_plan = SessionPlan::from_yaml(
        r#"
schema: camera-initiator-session/v1
steps:
  - { id: enter, kind: entry, to: shooting/stills }
  - id: mismatch
    kind: action
    action: autofocusRelease
    expect: { stepsRun: 999 }
  - { id: omitted, kind: action, action: autofocusRelease }
"#,
    )
    .expect("parse expectation failure plan");
    let failure_plan =
        prepare_session_plan(failure_plan, &store, "app").expect("prepare failure plan");
    let failure_trace_buffer = TraceBuffer::default();
    let failure_observation_path = media.path().join("failure-observation.jsonl");
    let failure_trace = Arc::new(TraceWriter::with_observations(
        TraceFormat::Jsonl,
        Box::new(failure_trace_buffer.clone()),
        observation_recorder(failure_observation_path.clone(), "session-failure"),
        "app".into(),
        "session".into(),
    ));
    let failure_transport = NativePtpTransport::new(
        Arc::clone(&store),
        TransportConfig {
            camera: Some(IpAddr::V4(Ipv4Addr::LOCALHOST)),
            interface: None,
            connection: "app".into(),
            runtime_scope: vec![("terminalName".into(), "ptpsim".into())],
            connect_timeout: Duration::from_secs(2),
            max_frame_bytes: 1024 * 1024,
        },
        Arc::clone(&failure_trace),
    )
    .expect("construct expectation failure transport");
    let failure_output = media.path().join("session-failure-output");
    run_session_plan(
        &failure_plan,
        Arc::clone(&store),
        NativeSessionLifecycle::new(failure_transport),
        failure_trace,
        SessionRunOptions {
            output_dir: failure_output.clone(),
            run_id: "session-failure".into(),
            connection: "app".into(),
            trace_path: "-".into(),
            observation_path: failure_observation_path.clone(),
            runtime_params: Vec::new(),
            handoff_timeout: Duration::from_secs(2),
        },
    )
    .await
    .expect_err("expectation mismatch fails the session");
    let failure_report: serde_json::Value =
        serde_json::from_slice(&std::fs::read(failure_output.join("session-report.json")).unwrap())
            .unwrap();
    assert_eq!(failure_report["status"], "failed");
    assert_eq!(failure_report["steps"].as_array().unwrap().len(), 2);
    assert_eq!(failure_report["steps"][1]["id"], "mismatch");
    assert_eq!(
        failure_report["steps"][1]["expectationMismatch"]["expected"],
        999
    );
    assert!(failure_trace_buffer.records().iter().any(|record| {
        record.get("state").and_then(serde_json::Value::as_str) == Some("closed")
    }));
    let failure_bundle = std::fs::read_to_string(failure_observation_path).unwrap();
    let failure_bundle = validate_bundles(&[&failure_bundle]).unwrap();
    let recorded_actions: Vec<_> = failure_bundle
        .records
        .iter()
        .filter_map(|record| match record {
            ObservationLine::ActionInvocation(action) => Some(action),
            _ => None,
        })
        .collect();
    assert_eq!(recorded_actions.len(), 1);
    assert_eq!(recorded_actions[0].action_id, "autofocusRelease");
    assert_eq!(
        recorded_actions[0].outcome,
        camera_config::ActionOutcome::Succeeded
    );

    let _ = shutdown_tx.send(());
    server_task.await.expect("join session simulator service");
}

#[tokio::test]
async fn session_runner_records_pushed_frames_in_the_observation_bundle() {
    let body = data("fuji/gfx100ii/gfx100ii.yaml");
    let media = tempfile::tempdir().expect("create temporary media root");
    let live_view = media.path().join("push-liveview");
    std::fs::create_dir_all(&live_view).expect("create push live-view fixtures");
    std::fs::write(
        live_view.join("frame.jpg"),
        [
            0xff, 0xd8, 0xff, 0xc0, 0, 11, 8, 1, 224, 2, 128, 1, 1, 0x11, 0, 0xff, 0xda, 0, 8, 1,
            1, 0, 0, 63, 0, 1, 2, 3, 0xff, 0xd9,
        ],
    )
    .expect("write push live-view fixture");
    let loopback = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0);
    let server = Server::bind(Config {
        instance_id: "camera-initiator-push-observation".into(),
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
    .expect("bind push observation simulator");
    let body = with_loopback_ports(
        body,
        server.command_addr(),
        server.event_addr_opt().expect("push event listener"),
        server.liveview_addr_opt().expect("push live-view listener"),
    );
    let store = ConfigStore::from_tiers(body, Some(data("fuji/fuji.yaml")), Vec::new())
        .expect("load push manifest tiers");
    let plan = SessionPlan::from_yaml(
        r#"
schema: camera-initiator-session/v1
steps:
  - { id: enter-stills, kind: entry, to: shooting/stills }
  - { id: first, kind: samplePushFrames, frameCount: 2, maxFrameBytes: 4096, maxTotalBytes: 8192, timeoutMs: 2000 }
  - { id: second, kind: samplePushFrames, frameCount: 1, maxFrameBytes: 4096, maxTotalBytes: 4096, timeoutMs: 2000 }
"#,
    )
    .expect("parse push session plan");
    let prepared = prepare_session_plan(plan, &store, "app").expect("prepare push session plan");

    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
    let server_task = tokio::spawn(server.run(shutdown_rx));
    let observation_path = media.path().join("push-observation.jsonl");
    let trace = Arc::new(TraceWriter::with_observations(
        TraceFormat::Jsonl,
        Box::new(io::sink()),
        observation_recorder(observation_path.clone(), "push-observation"),
        "app".into(),
        "session".into(),
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
    .expect("construct push transport");
    let report = run_session_plan(
        &prepared,
        Arc::clone(&store),
        NativeSessionLifecycle::new(transport),
        trace,
        SessionRunOptions {
            output_dir: media.path().join("push-output"),
            run_id: "push-observation".into(),
            connection: "app".into(),
            trace_path: "-".into(),
            observation_path: observation_path.clone(),
            runtime_params: Vec::new(),
            handoff_timeout: Duration::from_secs(2),
        },
    )
    .await
    .expect("run push session plan");
    let _ = shutdown_tx.send(());
    server_task.await.expect("join push simulator");

    let retained = report.steps[1..]
        .iter()
        .flat_map(|step| &step.frames.as_ref().expect("push frame report").frames)
        .collect::<Vec<_>>();
    assert_eq!(retained.len(), 3);

    let bundle = std::fs::read_to_string(observation_path).expect("read push bundle");
    let validated = validate_bundles(&[&bundle]).expect("push bundle validates");
    let frames = validated
        .records
        .iter()
        .filter_map(|record| match record {
            ObservationLine::PushFrame(frame) => Some(frame),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(frames.len(), 3, "one record per retained frame");
    let transaction_instances = validated
        .records
        .iter()
        .filter_map(|record| match record {
            ObservationLine::PtpTransaction(transaction) => {
                Some(transaction.connection_instance.as_str())
            }
            _ => None,
        })
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(transaction_instances.len(), 1);
    for (index, (frame, retained)) in frames.iter().zip(&retained).enumerate() {
        assert!(transaction_instances.contains(frame.connection_instance.as_str()));
        assert_eq!(frame.stream, "liveView");
        assert_eq!(
            frame.sequence, index as u64,
            "sequence continues across steps"
        );
        assert_eq!(frame.frame_counter, Some(index as u64));
        assert_eq!(frame.common.context.connection, "app");
        assert_eq!(frame.common.context.mode, "shooting/stills");
        assert_eq!(frame.envelope.framing, "fuji-liveview-v1");
        assert_eq!(frame.envelope.body_offset, 18);
        assert_eq!(frame.envelope.header.length, 18);
        assert_eq!(frame.envelope.gap_sha256, None);
        assert_eq!(
            frame.envelope.declared_length,
            18 + frame.body.payload.length
        );
        let header = frame
            .envelope
            .header
            .inline_hex
            .as_deref()
            .expect("header bytes");
        assert_eq!(
            &header[16..24],
            format!("{:08x}", (index as u32).swap_bytes())
        );
        assert_eq!(frame.body.media_type, "image/jpeg");
        assert!(
            frame.body.payload.inline_hex.is_none(),
            "no image bytes in bundle"
        );
        assert_eq!(frame.body.payload.sha256, retained.sha256);
        assert_eq!(frame.body.payload.length, retained.length);
        let image = frame.body.image.expect("image geometry");
        assert_eq!((image.width, image.height), (640, 480));
        assert_eq!((image.precision, image.components), (8, 1));
        let first = &frame.host_read.first_read;
        let last = frame.host_read.last_read.as_ref().expect("last read");
        assert_eq!(first.clock, "process-monotonic");
        assert_eq!(last.clock, "process-monotonic");
        assert!(first.value <= last.value);
        assert!(last.value <= frame.common.time.value);
    }
    assert!(frames
        .windows(2)
        .all(|pair| pair[0].host_read.first_read.value <= pair[1].host_read.first_read.value));

    let proposal =
        camera_config::propose(&[&bundle]).expect("push bundle reaches generator intake");
    let identities = frames
        .iter()
        .map(|frame| format!("{}:{}", frame.common.run_id, frame.common.record_id))
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(
        proposal
            .record_dispositions
            .iter()
            .filter(|disposition| identities.contains(&disposition.identity)
                && disposition.status == camera_config::ProposalRecordStatus::EvidenceOnly)
            .count(),
        3
    );
}

#[tokio::test]
async fn native_transport_runs_pcss_rendezvous_retry_and_action() {
    let body = data("fuji/gfx100ii/gfx100ii.yaml");
    let media = temp_media_root();
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

#[tokio::test]
async fn session_runner_retains_ordinary_and_streaming_payload_metadata() {
    let body = data("fuji/gfx100ii/gfx100ii.yaml");
    let media = temp_media_root();
    let object = b"\xFF\xD8SESSION-PAYLOAD\xFF\xD9";
    let dcim = media.path().join("DCIM/100_FUJI");
    std::fs::create_dir_all(&dcim).expect("create session media directory");
    std::fs::write(dcim.join("SESSION.JPG"), object).expect("write session object");
    let loopback = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0);
    let callback_reservation =
        std::net::TcpListener::bind(loopback).expect("reserve session callback port");
    let callback_port = callback_reservation
        .local_addr()
        .expect("session callback address")
        .port();
    let server_body = replace_once(
        body,
        "callbackPort: 51560",
        format!("callbackPort: {callback_port}"),
    );
    let server = Server::bind(Config {
        instance_id: "camera-initiator-session-payloads".into(),
        profile: "fuji/gfx100ii/fw0230".into(),
        connection: "wireless-tether".into(),
        manifest_yaml: server_body.clone(),
        media_root: media.path().to_path_buf(),
        command_bind: Some(loopback),
        liveview_bind: None,
        event_bind: None,
        knock_bind: Some(loopback),
        pcss_init_fails: 0,
        pcss_shutter_enqueue_count: 0,
        control_bind: loopback,
        liveview_dir: None,
        state_callback: None,
    })
    .await
    .expect("bind session payload simulator");
    let command = server.command_addr();
    let knock = server.knock_addr_opt().expect("session knock listener");
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
    let store = ConfigStore::from_tiers(
        store_body,
        Some(data("fuji/fuji.yaml")),
        Vec::<String>::new(),
    )
    .expect("load session payload manifest tiers");
    let plan = SessionPlan::from_yaml(&format!(
        r#"
schema: camera-initiator-session/v1
steps:
  - {{ id: device-info, kind: action, action: readDeviceInfo }}
  - {{ id: idle, kind: hold, durationMs: 5 }}
  - id: object
    kind: action
    action: getObject
    parameters: {{ handle: 3 }}
    expectedBytes: {}
    expect:
      outputCount: 1
      outputs: [{{ index: 0, payloadBytes: {} }}]
"#,
        object.len(),
        object.len()
    ))
    .expect("parse session payload plan");
    let prepared = prepare_session_plan(plan, &store, "wireless-tether")
        .expect("prepare session payload plan");

    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
    let server_task = tokio::spawn(server.run(shutdown_rx));
    let trace = Arc::new(TraceWriter::new(TraceFormat::Jsonl, Box::new(io::sink())));
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
    .expect("construct session payload transport");
    drop(callback_reservation);

    let output_dir = media.path().join("payload-session-output");
    let report = run_session_plan(
        &prepared,
        Arc::clone(&store),
        NativeSessionLifecycle::new(transport),
        trace,
        SessionRunOptions {
            output_dir: output_dir.clone(),
            run_id: "payload-session".into(),
            connection: "wireless-tether".into(),
            trace_path: "-".into(),
            observation_path: media.path().join("unused-observation.jsonl"),
            runtime_params: Vec::new(),
            handoff_timeout: Duration::from_secs(2),
        },
    )
    .await
    .expect("run session payload plan");

    assert_eq!(report.steps.len(), 3);
    let hold = report.steps[1]
        .hold
        .as_ref()
        .expect("native idle observation");
    assert_eq!(
        hold.observation.end,
        camera_initiator::idle::IdleEnd::DurationElapsed
    );
    assert!(report.steps[1].transaction_ids.is_empty());
    assert!(report
        .steps
        .iter()
        .all(|step| step.session_index == Some(1)));
    assert!(hold.ended_elapsed_ns >= hold.started_elapsed_ns + 5_000_000);
    assert_eq!(report.steps[0].payloads.len(), 1);
    assert_eq!(report.steps[2].payloads.len(), 1);
    let ordinary = &report.steps[0].payloads[0];
    assert!(ordinary.path.starts_with("payloads/0000-device-info/"));
    assert_eq!(
        std::fs::metadata(output_dir.join(&ordinary.path))
            .unwrap()
            .len(),
        ordinary.length
    );
    let streamed = &report.steps[2].payloads[0];
    assert_eq!(streamed.path, "payloads/0002-object/0000_stream.bin");
    assert_eq!(
        std::fs::read(output_dir.join(&streamed.path)).unwrap(),
        object
    );
    let mut expected = PayloadMetadataBuilder::new();
    expected.update(object);
    let expected = expected.metadata();
    assert_eq!(streamed.length, expected.length);
    assert_eq!(streamed.sha256, expected.sha256);
    assert!(
        report.steps[0].transaction_ids.last().unwrap()
            < report.steps[2].transaction_ids.first().unwrap()
    );
    let report_json: serde_json::Value =
        serde_json::from_slice(&std::fs::read(output_dir.join("session-report.json")).unwrap())
            .unwrap();
    assert_eq!(
        report_json.pointer("/steps/2/payloads/0/sha256"),
        Some(&serde_json::json!(expected.sha256))
    );

    let _ = shutdown_tx.send(());
    server_task.await.expect("join session payload simulator");
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn native_transport_recovers_broadcast_discovery_with_unicast() {
    let body = data("fuji/gfx100ii/gfx100ii.yaml");
    let media = temp_media_root();
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
    let media = temp_media_root();
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
async fn native_transport_ignores_wrong_camera_name_then_accepts_selected_body() {
    let body = data("fuji/gfx100ii/gfx100ii.yaml");
    let media = temp_media_root();
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
        .expect("bind multi-responder PCSS knock listener");
    let knock_port = knock.local_addr().expect("PCSS knock address").port();
    let server = Server::bind(Config {
        instance_id: "camera-initiator-pcss-camera-name-loopback".into(),
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
    .expect("bind selected-body PCSS simulator service");
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
    .expect("load camera-name PCSS loopback manifest tiers");

    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
    let server_task = tokio::spawn(server.run(shutdown_rx));
    let responder_task = tokio::spawn(async move {
        let mut discovery = [0u8; 512];
        let (length, _) =
            tokio::time::timeout(Duration::from_secs(2), knock.recv_from(&mut discovery))
                .await
                .expect("wait for PCSS discovery")
                .expect("receive PCSS discovery");
        assert!(discovery[..length].starts_with(b"DISCOVERY * HTTP/1.1\r\n"));

        let wrong = connect_named_pcss_notify(callback_port, "X-A7", command.port()).await;
        let wrong_ack = tokio::spawn(pcss_callback_was_acknowledged(wrong));
        let correct = connect_named_pcss_notify(callback_port, "GFX100 II", command.port()).await;
        assert!(
            pcss_callback_was_acknowledged(correct).await,
            "selected-body callback was not acknowledged"
        );
        assert!(
            !wrong_ack.await.expect("join wrong-camera callback"),
            "wrong-camera callback was acknowledged"
        );
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
    .expect("construct camera-name PCSS transport");

    drop(callback_reservation);
    let opened = transport
        .open_command_session()
        .await
        .expect("ignore the wrong body and establish the selected body");
    assert_eq!(opened.response_code, 0x2001);
    transport
        .close_session_if_open()
        .await
        .expect("close selected-body PCSS session cleanly");

    let records = trace_buffer.records();
    assert!(records.iter().any(|record| {
        record.get("state").and_then(serde_json::Value::as_str) == Some("pcssCallbackIgnored")
            && record
                .pointer("/detail/reason")
                .and_then(serde_json::Value::as_str)
                == Some("callbackCameraNameMismatch")
            && record
                .pointer("/detail/cameraName")
                .and_then(serde_json::Value::as_str)
                == Some("X-A7")
            && record
                .pointer("/detail/expected")
                .and_then(serde_json::Value::as_str)
                == Some("GFX100 II")
    }));
    let discoveries: Vec<&serde_json::Value> = records
        .iter()
        .filter(|record| {
            record.get("state").and_then(serde_json::Value::as_str) == Some("pcssDiscoverySent")
        })
        .collect();
    assert_eq!(discoveries.len(), 1, "callbacks stayed within one attempt");
    assert_eq!(
        discoveries[0]
            .pointer("/detail/attempt")
            .and_then(serde_json::Value::as_u64),
        Some(1)
    );

    let _ = shutdown_tx.send(());
    server_task
        .await
        .expect("join selected-body PCSS simulator service");
    responder_task
        .await
        .expect("join multi-responder PCSS knock responder");
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn native_transport_explicit_unicast_rejects_wrong_camera_name_until_timeout() {
    let body = data("fuji/gfx100ii/gfx100ii.yaml");
    let loopback = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0);
    let callback_reservation =
        std::net::TcpListener::bind(loopback).expect("reserve ephemeral PCSS callback port");
    let callback_port = callback_reservation
        .local_addr()
        .expect("reserved callback address")
        .port();
    let knock = tokio::net::UdpSocket::bind(loopback)
        .await
        .expect("bind explicit-unicast PCSS knock listener");
    let knock_port = knock.local_addr().expect("PCSS knock address").port();
    let command = tokio::net::TcpListener::bind(loopback)
        .await
        .expect("bind command connection observer");
    let command_port = command
        .local_addr()
        .expect("command observer address")
        .port();
    let store_body = replace_once(
        replace_once(
            replace_once(
                replace_once(
                    body,
                    "callbackPort: 51560",
                    format!("callbackPort: {callback_port}"),
                ),
                "knockPort: 51562",
                format!("knockPort: {knock_port}"),
            ),
            "bindings: { command: 15740 }",
            format!("bindings: {{ command: {command_port} }}"),
        ),
        "retryIntervalMs: 1000",
        "retryIntervalMs: 50".into(),
    );
    let store_body = replace_once(store_body, "maxAttempts: 15", "maxAttempts: 2".into());
    let store = ConfigStore::from_tiers(
        store_body,
        Some(data("fuji/fuji.yaml")),
        Vec::<String>::new(),
    )
    .expect("load explicit-unicast camera-name manifest tiers");

    let responder_task = tokio::spawn(async move {
        for attempt in 1..=2 {
            let mut discovery = [0u8; 512];
            let (length, peer) =
                tokio::time::timeout(Duration::from_secs(2), knock.recv_from(&mut discovery))
                    .await
                    .expect("wait for explicit-unicast discovery")
                    .expect("receive explicit-unicast discovery");
            assert!(discovery[..length].starts_with(b"DISCOVERY * HTTP/1.1\r\n"));
            assert_eq!(peer.ip(), IpAddr::V4(Ipv4Addr::LOCALHOST));
            let wrong = connect_named_pcss_notify(callback_port, "X-A7", command_port).await;
            assert!(
                !pcss_callback_was_acknowledged(wrong).await,
                "wrong-camera callback on attempt {attempt} was acknowledged"
            );
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
            camera: Some(IpAddr::V4(Ipv4Addr::LOCALHOST)),
            interface: None,
            connection: "wireless-tether".into(),
            runtime_scope: vec![("terminalName".into(), "ptpsim".into())],
            connect_timeout: Duration::from_millis(250),
            max_frame_bytes: 1024 * 1024,
        },
        trace,
    )
    .expect("construct explicit-unicast camera-name PCSS transport");

    drop(callback_reservation);
    let error = transport
        .open_command_session()
        .await
        .expect_err("wrong camera name must exhaust the bounded callback search");
    assert!(
        error
            .to_string()
            .contains("transport operation timed out: PCSS callback"),
        "unexpected callback failure: {error}"
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(100), command.accept())
            .await
            .is_err(),
        "wrong-camera callback opened the command endpoint"
    );
    responder_task
        .await
        .expect("join explicit-unicast wrong-camera responder");

    let records = trace_buffer.records();
    let discoveries = records
        .iter()
        .filter(|record| {
            record.get("state").and_then(serde_json::Value::as_str) == Some("pcssDiscoverySent")
        })
        .count();
    assert_eq!(discoveries, 2, "manifest maxAttempts bounds discovery");
    let mismatches = records
        .iter()
        .filter(|record| {
            record.get("state").and_then(serde_json::Value::as_str) == Some("pcssCallbackIgnored")
                && record
                    .pointer("/detail/reason")
                    .and_then(serde_json::Value::as_str)
                    == Some("callbackCameraNameMismatch")
        })
        .count();
    assert_eq!(mismatches, 2);
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn native_transport_recovers_first_init_eof_with_learned_unicast() {
    let body = data("fuji/gfx100ii/gfx100ii.yaml");
    let media = temp_media_root();
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

#[tokio::test]
async fn capability_verification_retains_native_observations_without_promoting_results() {
    use camera_initiator::capability::{
        run_capability_verification, CandidateLedger, CapabilityOptions,
    };
    let body = data("fuji/gfx100ii/gfx100ii.yaml");
    let media = temp_media_root();
    let loopback = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0);
    let server = Server::bind(Config {
        instance_id: "capability-verification-loopback".into(),
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
        liveview_dir: None,
        state_callback: None,
    })
    .await
    .unwrap();
    let body = with_loopback_ports(
        body,
        server.command_addr(),
        server.event_addr_opt().unwrap(),
        server.liveview_addr_opt().unwrap(),
    );
    let store =
        ConfigStore::from_tiers(body, Some(data("fuji/fuji.yaml")), Vec::<String>::new()).unwrap();
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
    let server_task = tokio::spawn(server.run(shutdown_rx));
    let observation_path = media.path().join("capability-observation.jsonl");
    let trace = Arc::new(TraceWriter::with_observations(
        TraceFormat::Jsonl,
        Box::new(std::io::sink()),
        observation_recorder(observation_path.clone(), "capability-loopback"),
        "app".into(),
        "unspecified".into(),
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
        trace,
    )
    .unwrap();
    let ledger = CandidateLedger::from_json(&serde_json::json!({
        "schema":"camera-static-candidates/v1",
        "source":{"application":"Synthetic Client","version":"1","plugin":"test","sha256":"0".repeat(64)},
        "manufacturer":"FUJIFILM", "model":"GFX100 II",
        "candidates":[
            {"id":"aperture","kind":"property","code":"0x5007","status":"unverified","expectedType":"u16","nativeNames":["Aperture"],"disposition":"preserveReviewed","reason":"Synthetic read candidate","evidence":["simulator"]},
            {"id":"missing","kind":"property","code":"0xdfff","status":"unverified","nativeNames":["Unknown"],"disposition":"preserveReviewed","reason":"Synthetic missing candidate","evidence":["simulator"]}
        ]
    }).to_string()).unwrap();
    transport.open_command_session().await.unwrap();
    let report = run_capability_verification(
        &ledger,
        None,
        transport.command_framing(),
        transport.clone(),
        CapabilityOptions {
            selected_model: "GFX100 II".into(),
            manifest_firmware: "2.30".into(),
            connection: "app".into(),
            scope_mode: None,
            allow_unknown_model: false,
            allow_control: false,
            output_dir: media.path().join("capabilities"),
        },
    )
    .await
    .unwrap();
    transport.close_session_if_open().await.unwrap();
    assert!(report.error.is_none());
    assert_eq!(report.rows[0].outcome, "observed");
    assert_eq!(report.rows[1].outcome, "rejected");
    assert_eq!(report.rows[1].advertised, Some(false));
    assert!(report.rows.iter().all(|row| row.status == "unverified"));
    let observation = std::fs::read_to_string(observation_path).unwrap();
    let bundle = validate_bundles(&[&observation]).unwrap();
    let transactions: Vec<_> = bundle
        .records
        .iter()
        .filter_map(|line| match line {
            ObservationLine::PtpTransaction(record) => Some(record),
            _ => None,
        })
        .collect();
    for request in report.rows.iter().flat_map(|row| &row.requests) {
        let transaction = transactions
            .iter()
            .find(|r| Some(r.transaction_id) == request.transaction_id)
            .unwrap();
        assert_eq!(transaction.request.operation, request.opcode);
        assert_eq!(
            transaction.response.as_ref().unwrap().code,
            format!("0x{:04x}", request.response.unwrap())
        );
        if let Some(payload) = &request.payload {
            assert_eq!(
                transaction
                    .response
                    .as_ref()
                    .unwrap()
                    .data
                    .as_ref()
                    .unwrap()
                    .payload
                    .sha256,
                payload.sha256
            );
        }
    }
    let _ = shutdown_tx.send(());
    server_task.await.unwrap();
}

// --- Empty memory card slot 1 over a real socket (MANIFEST_SCHEMA §11.32, §11.33, #352) ---
//
// The shipped GFX100 II startup-state fixture puts the simulator in its
// `memoryCardSlot1Empty` state. The state answers 0x9050 with 0x2019 only in
// the detected `image-transfer` mode, and a new PTP session starts with no
// detected mode. A client therefore opens one session, runs the manifest's
// cold `image-transfer` mode entry, which writes the 0xdf01 mode selector,
// and then runs the action in that same session.

const GFX_CONSOLIDATED: &str = "fuji/gfx100ii/gfx100ii.consolidated.yaml";
const EMPTY_SLOT_FIXTURE: &str =
    "packages/fixtures/startup-state/gfx100ii-memory-card-slot1-empty.yaml";

/// A running GFX100 II simulator service on loopback, plus a client that has
/// opened a PTP session over the real TCP command socket and entered
/// `image-transfer` through the manifest's mode entry.
struct ImageTransferLoopback {
    store: Arc<ConfigStore>,
    transport: Arc<NativePtpTransport>,
    shutdown: tokio::sync::oneshot::Sender<()>,
    server_task: tokio::task::JoinHandle<()>,
    _media: tempfile::TempDir,
}

impl ImageTransferLoopback {
    /// Start the service with the shipped consolidated manifest on
    /// OS-assigned loopback ports. With `slot_one_empty`, apply the shipped
    /// startup-state fixture first, the way `--startup-state` does.
    async fn start(slot_one_empty: bool) -> Self {
        let body = data(GFX_CONSOLIDATED);
        let media = temp_media_root();
        let folder = media.path().join("DCIM/100_FUJI");
        std::fs::create_dir_all(&folder).expect("create card folder");
        std::fs::write(
            folder.join("DSCF0001.JPG"),
            b"\xFF\xD8\xFF\xE0LOOPBACK\xFF\xD9",
        )
        .expect("write card image");
        let loopback = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0);
        let server = Server::bind(Config {
            instance_id: "empty-slot-loopback".into(),
            // The fixture names this profile and connection, and the service
            // rejects a fixture whose profile or connection differs.
            profile: "fuji/gfx100ii".into(),
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
            liveview_dir: None,
            state_callback: None,
        })
        .await
        .expect("bind simulator service");
        if slot_one_empty {
            let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("../..")
                .join(EMPTY_SLOT_FIXTURE);
            let raw = std::fs::read_to_string(&path)
                .unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
            let overlay: camera_sim::StateOverlay =
                serde_yaml::from_str(&raw).expect("parse shipped startup-state fixture");
            let applied = server
                .apply_startup_state(&overlay)
                .await
                .expect("apply shipped startup-state fixture");
            assert_eq!(applied.states, 1);
        }

        // The client reads its ports from the manifest, so point the manifest
        // at the listeners the OS assigned.
        let command = server.command_addr();
        let event = server.event_addr_opt().expect("app event listener");
        let live_view = server.liveview_addr_opt().expect("app live-view listener");
        let body = with_loopback_ports(body, command, event, live_view);
        let store = ConfigStore::from_tiers(body, Some(data("fuji/fuji.yaml")), Vec::new())
            .expect("load GFX100 II manifest tiers");

        // The listeners are already bound, so the client can connect as soon
        // as the service task starts accepting.
        let (shutdown, shutdown_rx) = tokio::sync::oneshot::channel();
        let server_task = tokio::spawn(server.run(shutdown_rx));
        let transport = NativePtpTransport::new(
            Arc::clone(&store),
            TransportConfig {
                camera: Some(IpAddr::V4(Ipv4Addr::LOCALHOST)),
                interface: None,
                connection: "app".into(),
                runtime_scope: vec![("terminalName".into(), "LoopbackTest".into())],
                connect_timeout: Duration::from_secs(2),
                max_frame_bytes: 1024 * 1024,
            },
            Arc::new(TraceWriter::new(TraceFormat::Jsonl, Box::new(io::sink()))),
        )
        .expect("construct native transport");
        let opened = transport
            .open_command_session()
            .await
            .expect("initialize PTP/IP and open the session over TCP");
        assert_eq!(opened.response_code, 0x2001);

        let raw_transport: Arc<dyn PtpExecutorTransport> = transport.clone();
        run_mode_entry(
            Arc::clone(&store),
            "app".into(),
            None,
            "image-transfer".into(),
            raw_transport,
            Arc::new(NoopObserver),
            Arc::new(NoopObserver),
            Vec::new(),
        )
        .await
        .expect("cold image-transfer entry succeeds in either card state");

        Self {
            store,
            transport,
            shutdown,
            server_task,
            _media: media,
        }
    }

    /// Run an `image-transfer` action in the session opened by `start`.
    async fn run(
        &self,
        action_id: &str,
    ) -> Result<camera_protocol_ffi::PtpExecutionOutcome, camera_protocol_ffi::PtpExecutorError>
    {
        let raw_transport: Arc<dyn PtpExecutorTransport> = self.transport.clone();
        run_initiator_action(
            Arc::clone(&self.store),
            ActionInvocationRequest {
                catalog_revision: self.store.action_catalog().revision,
                action_id: action_id.into(),
                connection: "app".into(),
                mode: "image-transfer".into(),
                role: ActionRole::Initiator,
                parameters: Vec::new(),
            },
            raw_transport,
            Arc::new(NoopObserver),
            Arc::new(NoopObserver),
        )
        .await
    }

    /// Close the PTP session, then stop the service.
    async fn finish(self) {
        self.transport
            .close_session_if_open()
            .await
            .expect("close PTP session cleanly");
        let _ = self.shutdown.send(());
        self.server_task.await.expect("join simulator service");
    }
}

/// Assert `error` is the classified empty-slot failure of the 0x9050 sent at
/// `expected_step`.
fn assert_media_slot_unavailable(
    error: &camera_protocol_ffi::PtpExecutorError,
    expected_step: &str,
) {
    let camera_protocol_ffi::PtpExecutorError::StepFailed {
        step,
        kind,
        detail,
        context,
    } = error
    else {
        panic!("expected StepFailed, got {error:?}");
    };
    assert_eq!(
        *kind,
        camera_protocol_ffi::ExecutorStepFailureKind::ConditionRejected,
        "{error:?}"
    );
    assert_eq!(step, expected_step);
    assert_eq!(detail, "operation 0x9050 returned 0x2019");
    let pairs: Vec<(&str, &str)> = context
        .iter()
        .map(|entry| (entry.key.as_str(), entry.value.as_str()))
        .collect();
    let [operation, response, (transaction_key, transaction_id), condition, slot] =
        pairs.as_slice()
    else {
        panic!("expected five context entries, got {pairs:?}");
    };
    assert_eq!(*operation, ("operation", "0x9050"));
    assert_eq!(*response, ("response", "0x2019"));
    assert_eq!(*transaction_key, "transactionId");
    assert!(transaction_id.parse::<u32>().is_ok(), "{pairs:?}");
    assert_eq!(*condition, ("condition", "mediaSlotUnavailable"));
    assert_eq!(*slot, ("slot", "1"));
}

#[tokio::test]
async fn empty_slot_one_classifies_enumeration_over_tcp() {
    let loopback = ImageTransferLoopback::start(true).await;
    let error = loopback
        .run("enumerateObjects")
        .await
        .expect_err("the camera refuses every page selection with slot 1 empty");
    assert_media_slot_unavailable(&error, "steps[0].retry.steps[0].sendOp");
    loopback.finish().await;
}

#[tokio::test]
async fn enumeration_over_tcp_succeeds_without_the_startup_state() {
    let loopback = ImageTransferLoopback::start(false).await;
    let outcome = loopback
        .run("enumerateObjects")
        .await
        .expect("enumeration succeeds with a card in slot 1");
    assert!(outcome
        .collections
        .iter()
        .any(|collection| collection.key == "objectHandles"));
    loopback.finish().await;
}

#[tokio::test]
async fn empty_slot_one_classifies_import_over_tcp() {
    let loopback = ImageTransferLoopback::start(true).await;
    let error = loopback
        .run("importObjects")
        .await
        .expect_err("the camera refuses every page selection with slot 1 empty");
    // importObjects repeats the nine mode-entry steps before its prime retry.
    assert_media_slot_unavailable(&error, "steps[9].retry.steps[0].sendOp");
    loopback.finish().await;
}

#[tokio::test]
async fn import_over_tcp_succeeds_without_the_startup_state() {
    let loopback = ImageTransferLoopback::start(false).await;
    loopback
        .run("importObjects")
        .await
        .expect("import succeeds with a card in slot 1");
    loopback.finish().await;
}

const APP_VIDEO_SELECTOR_BUNDLE: &str =
    "fuji/gfx100ii/evidence/app-video-selector/2026-05-18-app-video-selector-ops.jsonl";

/// Turn the captured app video-selector transactions into manifest steps. The
/// `0xD246` writes are left out because the manifest's selector mode entries
/// send them. A parameter equal to an earlier replayed transaction's captured
/// id is bound to that transaction's runtime id, the way the capture's
/// `0x9021` carries the `0x9020` transaction id.
fn replay_steps_from_bundle() -> (Vec<serde_yaml::Value>, usize) {
    let bundle = data(APP_VIDEO_SELECTOR_BUNDLE);
    let validated = validate_bundles(&[&bundle]).expect("app video-selector bundle validates");
    let transactions: Vec<_> = validated
        .records
        .iter()
        .filter_map(|record| match record {
            ObservationLine::PtpTransaction(transaction)
                if transaction.request.operation != "0x1016" =>
            {
                Some(transaction.as_ref())
            }
            _ => None,
        })
        .collect();
    let mut steps: Vec<serde_yaml::Value> = Vec::new();
    for (index, transaction) in transactions.iter().enumerate() {
        let response = transaction.response.as_ref().expect("captured response");
        assert_eq!(response.code, "0x2001");
        assert!(response.data.is_none(), "the captured reply has no data");
        let params: Vec<serde_yaml::Value> = transaction
            .request
            .parameters
            .iter()
            .map(|param| {
                match transactions[..index]
                    .iter()
                    .find(|earlier| earlier.transaction_id == *param)
                {
                    Some(earlier) => serde_yaml::from_str(&format!(
                        "{{ runtime: tx{} }}",
                        earlier.transaction_id
                    ))
                    .unwrap(),
                    None => serde_yaml::Value::from(*param),
                }
            })
            .collect();
        let mut step = serde_yaml::Mapping::new();
        step.insert(
            "sendOp".into(),
            transaction.request.operation.clone().into(),
        );
        step.insert("params".into(), params.into());
        let referenced = transactions[index + 1..].iter().any(|later| {
            later
                .request
                .parameters
                .contains(&transaction.transaction_id)
        });
        if referenced {
            step.insert(
                "captures".into(),
                serde_yaml::from_str(&format!(
                    "[{{ bind: tx{}, as: transactionId }}]",
                    transaction.transaction_id
                ))
                .unwrap(),
            );
        }
        steps.push(step.into());
    }
    let count = steps.len();
    (steps, count)
}

/// #570: over real TCP on the shipped consolidated manifest, the app video
/// selector moves the simulator into `shooting/video` with live view intact.
/// The captured `0x9020`/`0x9021` pair replays with bare OK replies, a
/// stills-only operation is refused in video, and stills works again after
/// the selector returns. The `0x9027` refusal is the wire-visible
/// discriminator: before #570 the simulator stayed in stills and accepted it.
#[tokio::test]
async fn app_video_selector_replays_the_captured_pair_over_tcp() {
    let body = data(GFX_CONSOLIDATED);
    let media = temp_media_root();
    let live_view = media.path().join("video-liveview");
    std::fs::create_dir_all(&live_view).expect("create live-view fixture directory");
    std::fs::write(
        live_view.join("frame.jpg"),
        b"\xFF\xD8\xFF\xE0LOOPBACK\xFF\xD9",
    )
    .expect("write live-view fixture");
    let loopback = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0);
    let server = Server::bind(Config {
        instance_id: "app-video-selector-loopback".into(),
        profile: "fuji/gfx100ii".into(),
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

    // Client-side test actions only; the simulator runs the shipped manifest.
    // `keepalive` is unused on the app connection, so it carries the replay.
    // `autofocusRelease` is re-scoped to video so the client sends the
    // stills-only 0x9027 while the simulator is in video.
    let command = server.command_addr();
    let event = server.event_addr_opt().expect("app event listener");
    let live_view = server.liveview_addr_opt().expect("app live-view listener");
    let body = with_loopback_ports(body, command, event, live_view);
    let mut client: serde_yaml::Value = serde_yaml::from_str(&body).expect("parse manifest");
    let (replay_steps, replay_count) = replay_steps_from_bundle();
    let actions = &mut client["connections"]["app"]["actions"];
    assert!(actions.get("keepalive").is_none());
    let mut initiator = serde_yaml::Mapping::new();
    initiator.insert("steps".into(), replay_steps.into());
    let mut replay = serde_yaml::Mapping::new();
    replay.insert("mode".into(), "shooting/video".into());
    replay.insert("initiator".into(), initiator.into());
    actions["keepalive"] = replay.into();
    actions["autofocusRelease"]["mode"] = "shooting/video".into();
    let client = serde_yaml::to_string(&client).expect("serialize client manifest");
    let store = ConfigStore::from_tiers(client, Some(data("fuji/fuji.yaml")), Vec::new())
        .expect("load client manifest tiers");

    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
    let server_task = tokio::spawn(server.run(shutdown_rx));
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
        Arc::new(TraceWriter::new(TraceFormat::Jsonl, Box::new(io::sink()))),
    )
    .expect("construct native transport");
    let opened = transport
        .open_command_session_after_handoff(Instant::now() + Duration::from_secs(2))
        .await
        .expect("open the PTP session over TCP");
    assert_eq!(opened.response_code, 0x2001);

    let entry = |from: Option<&str>, to: &str| {
        let raw_transport: Arc<dyn PtpExecutorTransport> = transport.clone();
        run_mode_entry(
            Arc::clone(&store),
            "app".into(),
            from.map(str::to_string),
            to.into(),
            raw_transport,
            Arc::new(NoopObserver),
            Arc::new(NoopObserver),
            Vec::new(),
        )
    };
    let action = |action_id: &str, mode: &str| {
        let raw_transport: Arc<dyn PtpExecutorTransport> = transport.clone();
        run_initiator_action(
            Arc::clone(&store),
            ActionInvocationRequest {
                catalog_revision: store.action_catalog().revision,
                action_id: action_id.into(),
                connection: "app".into(),
                mode: mode.into(),
                role: ActionRole::Initiator,
                parameters: Vec::new(),
            },
            raw_transport,
            Arc::new(NoopObserver),
            Arc::new(NoopObserver),
        )
    };

    entry(None, "shooting/stills")
        .await
        .expect("cold live-view entry");
    assert!(
        transport
            .confirm_live_view_frame()
            .await
            .expect("stills frame")
            > 0
    );
    entry(Some("shooting/stills"), "shooting/video")
        .await
        .expect("D246=1 selector entry");

    let replayed = action("keepalive", "shooting/video")
        .await
        .expect("the captured 0x9020/0x9021 pair replays with OK replies");
    assert_eq!(replayed.steps_run as usize, replay_count);
    assert!(replayed
        .outputs
        .iter()
        .all(|output| output.payload.is_empty()));
    assert!(replayed.scope.iter().any(|value| value.key == "tx58"));

    let refused = action("autofocusRelease", "shooting/video")
        .await
        .expect_err("app video refuses the stills-only 0x9027");
    let camera_protocol_ffi::PtpExecutorError::StepFailed { detail, .. } = &refused else {
        panic!("expected StepFailed, got {refused:?}");
    };
    assert_eq!(detail, "operation 0x9027 returned 0x2005");

    entry(Some("shooting/video"), "shooting/stills")
        .await
        .expect("D246=0 selector entry");
    action("shutter", "shooting/stills")
        .await
        .expect("the postview shutter works in stills with streaming intact");
    assert!(
        transport
            .confirm_live_view_frame()
            .await
            .expect("stills frame again")
            > 0
    );

    transport
        .close_session_if_open()
        .await
        .expect("close PTP session cleanly");
    let _ = shutdown_tx.send(());
    server_task.await.expect("join simulator service");
}
