//! Black-box smoke: spin up the service on ephemeral loopback ports and drive a
//! real PTP/IP image-import flow over TCP, plus a `/healthz` check. This is the
//! service-level counterpart to design gates #5 and #6.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream, UdpSocket};
use std::path::PathBuf;

use camera_sim_service::{Config, Server};
use protocol_primitives::{
    build_legacy_app_init, build_app_init, fuji_framing, parse_pcss_init_ack, usb_ptp,
    validate_legacy_app_init_ack,
};
use ptp_core::{PtpCodec, PtpIpPacket};

const MANIFEST: &str = r#"
schema: camera-config/v1
camera:
  manufacturer: FUJIFILM
  model: GFX100 II
  firmware: "2.30"
connections:
  app:
    kind: ptpip-app
    initShape: app82
    liveViewDelivery: { kind: stream }
    commandFraming: compressed
    eventFraming: usb
    bindings: { command: 55740, event: 55741, liveView: 55742 }
operations:
  "0x1002": { name: OpenSession, connections: [app] }
properties: {}
"#;

const PROPERTY_TRANSITION_MANIFEST: &str = r#"
schema: camera-config/v1
camera: { manufacturer: Test, model: Test, firmware: "1" }
connections:
  app:
    kind: ptpip-app
    initShape: app82
    liveViewDelivery: { kind: stream }
    commandFraming: compressed
    eventFraming: usb
    bindings: { command: 15740, event: 15741, liveView: 15742 }
    actions:
      autofocusLock:
        mode: shooting/stills
        responder:
          params:
            - { name: result, kind: u32, min: 2, max: 3 }
          mutation:
            kind: propertyTransition
            target: "0xd001"
            initial: 1
            terminal: { kind: parameter, parameter: result }
            settleAfterPolls: 2
        triggers: []
operations:
  "0x1002": { name: OpenSession, connections: [app] }
  "0x1015": { name: GetDevicePropValue, connections: [app] }
properties:
  "0xd001": { name: result, type: u16, access: readOnly }
"#;

const FAULT_MANIFEST: &str = r#"
schema: camera-config/v1
camera: { manufacturer: Test, model: Fault Camera, firmware: "1" }
connections:
  app:
    kind: ptpip-app
    initShape: app82
    liveViewDelivery: { kind: stream }
    commandFraming: compressed
    eventFraming: usb
    bindings: { command: 15740, event: 15741, liveView: 15742 }
operations:
  "0x1001": { name: GetDeviceInfo, connections: [app] }
  "0x1002": { name: OpenSession, connections: [app] }
  "0x1003": { name: CloseSession, connections: [app] }
  "0x1015": { name: GetDevicePropValue, connections: [app] }
  "0x1016": { name: SetDevicePropValue, connections: [app] }
properties:
  "0x5007": { name: aperture, type: u16, access: readWrite }
"#;

const STANDARD_FAULT_MANIFEST: &str = r#"
schema: camera-config/v1
camera: { manufacturer: Test, model: Standard Fault Camera, firmware: "1" }
values:
  initiatorGuid: { type: fixed, value: "00112233445566778899aabbccddeeff" }
  initiatorName: { type: fixed, value: ptpsim }
connections:
  app:
    kind: ptpip
    initShape: standardPtpIp
    init: { identity: { guid: initiatorGuid, friendlyName: initiatorName } }
    commandFraming: standard
    eventFraming: standard
    bindings: { command: 15740, event: 15740 }
operations:
  "0x1001": { name: GetDeviceInfo, connections: [app] }
properties: {}
"#;

fn start_fault_server(
    runtime: &tokio::runtime::Runtime,
    root: &std::path::Path,
) -> (
    std::net::SocketAddr,
    std::net::SocketAddr,
    tokio::sync::oneshot::Sender<()>,
    tokio::task::JoinHandle<()>,
) {
    runtime.block_on(async {
        let server = Server::bind(Config {
            instance_id: "fault-test".into(),
            profile: "test/fault".into(),
            connection: "app".into(),
            manifest_yaml: FAULT_MANIFEST.into(),
            media_root: root.to_path_buf(),
            command_bind: Some("127.0.0.1:0".parse().unwrap()),
            liveview_bind: Some("127.0.0.1:0".parse().unwrap()),
            event_bind: Some("127.0.0.1:0".parse().unwrap()),
            knock_bind: None,
            pcss_init_fails: 0,
            pcss_shutter_enqueue_count: 0,
            control_bind: "127.0.0.1:0".parse().unwrap(),
            liveview_dir: None,
            state_callback: None,
        })
        .await
        .unwrap();
        let command = server.command_addr();
        let control = server.control_addr();
        let (tx, rx) = tokio::sync::oneshot::channel();
        let handle = tokio::spawn(server.run(rx));
        (command, control, tx, handle)
    })
}

fn start_standard_fault_server(
    runtime: &tokio::runtime::Runtime,
    root: &std::path::Path,
) -> (
    std::net::SocketAddr,
    std::net::SocketAddr,
    tokio::sync::oneshot::Sender<()>,
    tokio::task::JoinHandle<()>,
) {
    runtime.block_on(async {
        let server = Server::bind(Config {
            instance_id: "standard-fault-test".into(),
            profile: "test/standard-fault".into(),
            connection: "app".into(),
            manifest_yaml: STANDARD_FAULT_MANIFEST.into(),
            media_root: root.to_path_buf(),
            command_bind: Some("127.0.0.1:0".parse().unwrap()),
            liveview_bind: None,
            event_bind: None,
            knock_bind: None,
            pcss_init_fails: 0,
            pcss_shutter_enqueue_count: 0,
            control_bind: "127.0.0.1:0".parse().unwrap(),
            liveview_dir: None,
            state_callback: None,
        })
        .await
        .unwrap();
        let command = server.command_addr();
        let control = server.control_addr();
        let (tx, rx) = tokio::sync::oneshot::channel();
        let handle = tokio::spawn(server.run(rx));
        (command, control, tx, handle)
    })
}

fn tmp_card() -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let root = std::env::temp_dir().join(format!("ptpsim-svc-{nanos}"));
    let dir = root.join("DCIM/100_FUJI");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("DSCF0001.JPG"), b"\xFF\xD8HELLOJPEG\xFF\xD9").unwrap();
    root
}

fn tmp_card_with_jpegs(count: usize) -> PathBuf {
    let root = tmp_card();
    let dir = root.join("DCIM/100_FUJI");
    for i in 2..=count {
        let mut bytes = b"\xFF\xD8HELLO".to_vec();
        bytes.extend_from_slice(format!("{i:04}").as_bytes());
        bytes.extend_from_slice(b"\xFF\xD9");
        std::fs::write(dir.join(format!("DSCF{i:04}.JPG")), bytes).unwrap();
    }
    root
}

fn tmp_card_with_movie() -> PathBuf {
    let root = tmp_card();
    std::fs::write(root.join("DCIM/100_FUJI/DSCF0002.MOV"), b"ftypqt  mov").unwrap();
    root
}

fn write_frame(s: &mut TcpStream, bytes: &[u8]) {
    s.write_all(bytes).unwrap();
}

fn read_frame(s: &mut TcpStream) -> Vec<u8> {
    let mut len = [0u8; 4];
    s.read_exact(&mut len).unwrap();
    let n = u32::from_le_bytes(len) as usize;
    let mut buf = vec![0u8; n];
    buf[0..4].copy_from_slice(&len);
    s.read_exact(&mut buf[4..]).unwrap();
    buf
}

fn op(code: u16, tid: u32, params: Vec<u32>) -> Vec<u8> {
    fuji_framing::encode(&PtpIpPacket::OperationRequest(ptp_core::OperationRequest {
        data_phase_info: 1,
        code,
        transaction_id: tid,
        params,
    }))
    .unwrap()
}

fn real_gfx_manifest() -> String {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../packages/camera-config-data/fuji/gfx100ii/gfx100ii.consolidated.yaml");
    std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("read {}: {e}", p.display()))
}

fn connect_ptpip(command_addr: std::net::SocketAddr, friendly_name: &str) -> TcpStream {
    let mut s = TcpStream::connect(command_addr).unwrap();
    write_frame(&mut s, &app_init_frame(1, friendly_name));
    match PtpIpPacket::decode(&read_frame(&mut s)).unwrap() {
        PtpIpPacket::InitCommandAck(_) => {}
        other => panic!("expected InitCommandAck, got {other:?}"),
    }
    s
}

fn app_init_frame(guid_byte: u8, friendly_name: &str) -> Vec<u8> {
    build_app_init(&[guid_byte; 16], friendly_name).unwrap()
}

fn pcss_init_frame(hostname: &str) -> Vec<u8> {
    let mut bytes = vec![0u8; 82];
    bytes[0..4].copy_from_slice(&82u32.to_le_bytes());
    bytes[4..8].copy_from_slice(&1u32.to_le_bytes());
    bytes[8..24].copy_from_slice(&[
        0xf2, 0xe4, 0x53, 0x8f, 0xad, 0xa5, 0x48, 0x5d, 0x87, 0xb2, 0x7f, 0x0b, 0xd3, 0xd5, 0xde,
        0xd0,
    ]);
    bytes[0x18..0x1c].copy_from_slice(&[0x31, 0x07, 0xa8, 0xc0]);
    let mut off = 0x1c;
    for unit in hostname.encode_utf16() {
        bytes[off..off + 2].copy_from_slice(&unit.to_le_bytes());
        off += 2;
    }
    bytes
}

#[test]
fn simulator_rejects_unsupported_init_shape_before_listening() {
    let root = tmp_card();
    let manifest = MANIFEST.replacen("initShape: app82", "initShape: unknown82", 1);
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let result = runtime.block_on(Server::bind(Config {
        instance_id: "invalid-shape".into(),
        profile: "test".into(),
        connection: "app".into(),
        manifest_yaml: manifest,
        media_root: root.clone(),
        command_bind: Some("127.0.0.1:0".parse().unwrap()),
        liveview_bind: None,
        event_bind: None,
        knock_bind: None,
        pcss_init_fails: 0,
        pcss_shutter_enqueue_count: 0,
        control_bind: "127.0.0.1:0".parse().unwrap(),
        liveview_dir: None,
        state_callback: None,
    }));
    let error = match result {
        Ok(_) => panic!("unsupported init shape must fail bind"),
        Err(error) => error,
    };
    assert!(
        error.to_string().contains("unsupported init shape/framing"),
        "{error}"
    );
    std::fs::remove_dir_all(root).ok();
}

#[test]
fn standard_ptpip_demultiplexes_event_socket_and_coordinates_disconnect() {
    const STANDARD_MANIFEST: &str = r#"
schema: camera-config/v1
camera: { manufacturer: TEST, model: Standard Camera, firmware: "1.0" }
values:
  initiatorGuid: { type: fixed, value: "00112233445566778899aabbccddeeff" }
  initiatorName: { type: fixed, value: SnapBridge }
connections:
  app:
    kind: ptpip
    initShape: standardPtpIp
    init:
      identity: { guid: initiatorGuid, friendlyName: initiatorName }
    commandFraming: standard
    eventFraming: standard
    bindings: { command: 15740, event: 15740 }
operations:
  "0x1001": { name: GetDeviceInfo, connections: [app] }
  "0x1002": { name: OpenSession, connections: [app] }
properties: {}
"#;

    let root = tmp_card();
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let (address, shutdown_tx, handle) = runtime.block_on(async {
        let server = Server::bind(Config {
            instance_id: "standard-ptpip".into(),
            profile: "test/standard".into(),
            connection: "app".into(),
            manifest_yaml: STANDARD_MANIFEST.into(),
            media_root: root.clone(),
            command_bind: Some("127.0.0.1:0".parse().unwrap()),
            liveview_bind: None,
            event_bind: None,
            knock_bind: None,
            pcss_init_fails: 0,
            pcss_shutter_enqueue_count: 0,
            control_bind: "127.0.0.1:0".parse().unwrap(),
            liveview_dir: None,
            state_callback: None,
        })
        .await
        .unwrap();
        assert_eq!(server.command_addr(), server.event_addr_opt().unwrap());
        let address = server.command_addr();
        let (tx, rx) = tokio::sync::oneshot::channel();
        let handle = tokio::spawn(server.run(rx));
        (address, tx, handle)
    });

    let mut command = TcpStream::connect(address).unwrap();
    let init = PtpIpPacket::InitCommandRequest(ptp_core::InitCommandRequest {
        initiator_guid: [0x11; 16],
        friendly_name: "SnapBridge".into(),
        protocol_version: 0x0001_0000,
    });
    write_frame(&mut command, &ptp_core::encode(&init).unwrap());
    let connection_number = match PtpIpPacket::decode(&read_frame(&mut command)).unwrap() {
        PtpIpPacket::InitCommandAck(ack) => ack.connection_number,
        other => panic!("expected InitCommandAck, got {other:?}"),
    };

    let mut mismatched = TcpStream::connect(address).unwrap();
    let bad_event = PtpIpPacket::InitEventRequest(ptp_core::InitEventRequest {
        connection_number: connection_number.wrapping_add(1),
    });
    write_frame(&mut mismatched, &ptp_core::encode(&bad_event).unwrap());
    mismatched
        .set_read_timeout(Some(std::time::Duration::from_secs(1)))
        .unwrap();
    let mut eof = [0u8; 1];
    assert_eq!(mismatched.read(&mut eof).unwrap(), 0);

    let mut event = TcpStream::connect(address).unwrap();
    let event_init =
        PtpIpPacket::InitEventRequest(ptp_core::InitEventRequest { connection_number });
    write_frame(&mut event, &ptp_core::encode(&event_init).unwrap());
    assert!(matches!(
        PtpIpPacket::decode(&read_frame(&mut event)).unwrap(),
        PtpIpPacket::InitEventAck(_)
    ));

    let device_info = PtpIpPacket::OperationRequest(ptp_core::OperationRequest {
        data_phase_info: 1,
        code: 0x1001,
        transaction_id: 0,
        params: vec![],
    });
    write_frame(&mut command, &ptp_core::encode(&device_info).unwrap());
    assert!(matches!(
        PtpIpPacket::decode(&read_frame(&mut command)).unwrap(),
        PtpIpPacket::StartData(start) if start.transaction_id == 0
    ));
    assert!(matches!(
        PtpIpPacket::decode(&read_frame(&mut command)).unwrap(),
        PtpIpPacket::EndData(data) if data.transaction_id == 0
    ));
    assert!(matches!(
        PtpIpPacket::decode(&read_frame(&mut command)).unwrap(),
        PtpIpPacket::OperationResponse(response)
            if response.transaction_id == 0 && response.code == 0x2001
    ));

    write_frame(
        &mut event,
        &ptp_core::encode(&PtpIpPacket::ProbeRequest(ptp_core::ProbeRequest)).unwrap(),
    );
    assert!(matches!(
        PtpIpPacket::decode(&read_frame(&mut event)).unwrap(),
        PtpIpPacket::ProbeResponse(_)
    ));

    let open = PtpIpPacket::OperationRequest(ptp_core::OperationRequest {
        data_phase_info: 1,
        code: 0x1002,
        transaction_id: 1,
        params: vec![1],
    });
    write_frame(&mut command, &ptp_core::encode(&open).unwrap());
    assert!(matches!(
        PtpIpPacket::decode(&read_frame(&mut command)).unwrap(),
        PtpIpPacket::OperationResponse(response)
            if response.transaction_id == 1 && response.code == 0x2001
    ));

    drop(event);
    command
        .set_read_timeout(Some(std::time::Duration::from_secs(1)))
        .unwrap();
    assert_eq!(command.read(&mut eof).unwrap(), 0);

    let mut command = TcpStream::connect(address).unwrap();
    write_frame(&mut command, &ptp_core::encode(&init).unwrap());
    let connection_number = match PtpIpPacket::decode(&read_frame(&mut command)).unwrap() {
        PtpIpPacket::InitCommandAck(ack) => ack.connection_number,
        other => panic!("expected second InitCommandAck, got {other:?}"),
    };
    let mut event = TcpStream::connect(address).unwrap();
    write_frame(
        &mut event,
        &ptp_core::encode(&PtpIpPacket::InitEventRequest(ptp_core::InitEventRequest {
            connection_number,
        }))
        .unwrap(),
    );
    assert!(matches!(
        PtpIpPacket::decode(&read_frame(&mut event)).unwrap(),
        PtpIpPacket::InitEventAck(_)
    ));
    drop(command);
    event
        .set_read_timeout(Some(std::time::Duration::from_secs(1)))
        .unwrap();
    assert_eq!(event.read(&mut eof).unwrap(), 0);

    shutdown_tx.send(()).ok();
    runtime.block_on(handle).unwrap();
    std::fs::remove_dir_all(root).ok();
}

#[tokio::test]
async fn standard_explicit_ephemeral_overrides_bind_independent_listeners() {
    const MANIFEST: &str = r#"
schema: camera-config/v1
camera: { manufacturer: TEST, model: Standard Camera }
values:
  initiatorGuid: { type: fixed, value: "00112233445566778899aabbccddeeff" }
  initiatorName: { type: fixed, value: SnapBridge }
connections:
  app:
    kind: ptpip
    initShape: standardPtpIp
    init: { identity: { guid: initiatorGuid, friendlyName: initiatorName } }
    commandFraming: standard
    eventFraming: standard
    bindings: { command: 15740, event: 15740 }
operations:
  "0x1001": { name: GetDeviceInfo, connections: [app] }
  "0x1002": { name: OpenSession, connections: [app] }
properties: {}
"#;
    let root = tmp_card();
    let ephemeral = "127.0.0.1:0".parse().unwrap();
    let server = Server::bind(Config {
        instance_id: "standard-explicit-ephemeral".into(),
        profile: "test/standard".into(),
        connection: "app".into(),
        manifest_yaml: MANIFEST.into(),
        media_root: root.clone(),
        command_bind: Some(ephemeral),
        liveview_bind: None,
        event_bind: Some(ephemeral),
        knock_bind: None,
        pcss_init_fails: 0,
        pcss_shutter_enqueue_count: 0,
        control_bind: ephemeral,
        liveview_dir: None,
        state_callback: None,
    })
    .await
    .unwrap();

    assert_ne!(server.command_addr(), server.event_addr_opt().unwrap());
    drop(server);
    std::fs::remove_dir_all(root).ok();
}

#[test]
fn legacy_app_init_and_usb_ptp_list_images_over_loopback() {
    const RESPONDER: [u8; 16] = [
        0x08, 0x70, 0xb0, 0x61, 0x0a, 0x8b, 0x45, 0x93, 0xb2, 0xe7, 0x93, 0x57, 0xdd, 0x36, 0xe0,
        0x50,
    ];
    const CAMERA_REMOTE_MANIFEST: &str = r#"
schema: camera-config/v1
camera: { manufacturer: FUJIFILM, model: X-A7, firmware: "1.00" }
values:
  unusedGuid: { type: fixed, value: "f2e4538fada5485d87b27f0bd3d5ded0" }
  unusedName: { type: fixed, value: ptpsim }
  unusedClientIpv4: { type: client-derived, runtime: clientIpv4 }
  legacyAppResponderGuid: { type: fixed, value: "0870b0610a8b4593b2e79357dd36e050" }
connections:
  legacy-app:
    kind: ptpip-legacy-app
    initShape: legacyApp82
    init:
      identity: { guid: unusedGuid, friendlyName: unusedName, clientIpv4: unusedClientIpv4 }
      nameFieldByteCount: 54
      expectedResponderGuid: legacyAppResponderGuid
    initRetries: { max: 5, backoffMs: 500, whenReasons: ["0x2019"] }
    commandFraming: usb
    bindings: { command: 55740 }
operations:
  "0x1002": { name: OpenSession, connections: [legacy-app] }
  "0x1007": { name: GetObjectHandles, connections: [legacy-app] }
properties: {}
"#;

    let root = tmp_card();
    let rt = tokio::runtime::Runtime::new().unwrap();
    let (command_addr, control_addr, shutdown_tx, handle) = rt.block_on(async {
        let server = Server::bind(Config {
            instance_id: "legacy-app-test".into(),
            profile: "fuji/xa7/static".into(),
            connection: "legacy-app".into(),
            manifest_yaml: CAMERA_REMOTE_MANIFEST.into(),
            media_root: root.clone(),
            command_bind: Some("127.0.0.1:0".parse().unwrap()),
            liveview_bind: None,
            event_bind: None,
            knock_bind: None,
            pcss_init_fails: 1,
            pcss_shutter_enqueue_count: 0,
            control_bind: "127.0.0.1:0".parse().unwrap(),
            liveview_dir: None,
            state_callback: None,
        })
        .await
        .unwrap();
        let command = server.command_addr();
        let control = server.control_addr();
        let (tx, rx) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(server.run(rx));
        (command, control, tx, task)
    });

    let mut stream = TcpStream::connect(command_addr).unwrap();
    let init = build_legacy_app_init(
        &[
            0xf2, 0xe4, 0x53, 0x8f, 0xad, 0xa5, 0x48, 0x5d, 0x87, 0xb2, 0x7f, 0x0b, 0xd3, 0xd5,
            0xde, 0xd0,
        ],
        "127.0.0.1".parse().unwrap(),
        "ptpsim",
    )
    .unwrap();
    write_frame(&mut stream, &init);
    assert!(matches!(
        PtpIpPacket::decode(&read_frame(&mut stream)).unwrap(),
        PtpIpPacket::InitFail(failure) if failure.reason == 0x2019
    ));
    write_frame(&mut stream, &init);
    validate_legacy_app_init_ack(&read_frame(&mut stream), &RESPONDER).unwrap();

    let open = PtpIpPacket::OperationRequest(ptp_core::OperationRequest {
        data_phase_info: 1,
        code: 0x1002,
        transaction_id: 1,
        params: vec![1],
    });
    write_frame(&mut stream, &usb_ptp::encode(&open).unwrap());
    assert!(matches!(
        usb_ptp::decode(&read_frame(&mut stream)).unwrap(),
        PtpIpPacket::OperationResponse(response) if response.code == 0x2001
    ));

    let list = PtpIpPacket::OperationRequest(ptp_core::OperationRequest {
        data_phase_info: 1,
        code: 0x1007,
        transaction_id: 2,
        params: vec![0xffff_ffff, 0],
    });
    write_frame(&mut stream, &usb_ptp::encode(&list).unwrap());
    let payload = match usb_ptp::decode(&read_frame(&mut stream)).unwrap() {
        PtpIpPacket::Data(data) => data.payload,
        other => panic!("expected USB data container, got {other:?}"),
    };
    let mut reader = ptp_core::Reader::new(&payload);
    assert_eq!(reader.ptp_array(|reader| reader.u32()).unwrap().len(), 1);
    assert!(matches!(
        usb_ptp::decode(&read_frame(&mut stream)).unwrap(),
        PtpIpPacket::OperationResponse(response) if response.code == 0x2001
    ));

    let observations = http_get(control_addr, "/observations?after=0");
    let export: serde_json::Value = serde_json::from_str(
        observations
            .split_once("\r\n\r\n")
            .expect("observation response body")
            .1,
    )
    .unwrap();
    let records = export["records"].as_array().unwrap();
    let transaction = records
        .iter()
        .find(|record| record["kind"] == "ptpTransaction" && record["transactionId"] == 2)
        .expect("USB-framed GetObjectHandles transaction observation");
    assert_eq!(transaction["request"]["framing"], "usb");
    let bundle = std::iter::once(export["header"].to_string())
        .chain(records.iter().map(serde_json::Value::to_string))
        .collect::<Vec<_>>()
        .join("\n");
    camera_config::validate_bundles(&[&bundle])
        .expect("legacy manufacturer app USB export is canonical input");

    let _ = shutdown_tx.send(());
    rt.block_on(handle).unwrap();
    std::fs::remove_dir_all(&root).ok();
}

fn connect_pcss(command_addr: std::net::SocketAddr, hostname: &str) -> TcpStream {
    let mut s = TcpStream::connect(command_addr).unwrap();
    write_frame(&mut s, &pcss_init_frame(hostname));
    let ack = parse_pcss_init_ack(&read_frame(&mut s)).expect("fixed PCSS InitCommandAck");
    assert_eq!(ack.connection_number, 0);
    s
}

fn set_prop(s: &mut TcpStream, tid: u32, prop: u16, data: &[u8]) {
    write_frame(s, &op(0x1016, tid, vec![prop as u32]));
    write_frame(s, &fuji_framing::encode_data(0x1016, tid, data));
    read_ok(s);
}

fn assert_read_timeout(s: &mut TcpStream) {
    let mut buf = [0u8; 4];
    match s.read_exact(&mut buf) {
        Err(e)
            if matches!(
                e.kind(),
                std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
            ) => {}
        other => panic!("expected read timeout/no-response, got {other:?}"),
    }
}

/// Read a data reply: one type-2 `Data` frame (the whole payload) followed by
/// `OperationResponse(OK)`. The compressed channel has no StartData/EndData.
fn read_data_reply(s: &mut TcpStream) -> Vec<u8> {
    let data = match fuji_framing::decode(&read_frame(s)).unwrap() {
        PtpIpPacket::Data(d) => d.payload,
        other => panic!("expected Data, got {other:?}"),
    };
    match fuji_framing::decode(&read_frame(s)).unwrap() {
        PtpIpPacket::OperationResponse(r) => assert_eq!(r.code, 0x2001, "OK expected"),
        other => panic!("expected response, got {other:?}"),
    }
    data
}

fn read_ok(s: &mut TcpStream) {
    assert_eq!(read_response_code(s), 0x2001);
}

fn read_response_code(s: &mut TcpStream) -> u16 {
    match fuji_framing::decode(&read_frame(s)).unwrap() {
        PtpIpPacket::OperationResponse(r) => r.code,
        other => panic!("expected response, got {other:?}"),
    }
}

fn open_session(s: &mut TcpStream) {
    write_frame(s, &op(0x1002, 1, vec![1]));
    read_ok(s);
}

fn read_handles(s: &mut TcpStream, tid: u32) -> Vec<u32> {
    write_frame(s, &op(0x1007, tid, vec![0xffff_ffff, 0]));
    let bytes = read_data_reply(s);
    let mut r = ptp_core::Reader::new(&bytes);
    r.ptp_array(|r| r.u32()).unwrap()
}

fn read_d620_count(s: &mut TcpStream, tid: u32) -> u32 {
    write_frame(s, &op(0x1015, tid, vec![0xd620]));
    let bytes = read_data_reply(s);
    let mut r = ptp_core::Reader::new(&bytes);
    r.u32().unwrap()
}

fn read_u16_prop(s: &mut TcpStream, tid: u32, property: u16) -> u16 {
    write_frame(s, &op(0x1015, tid, vec![u32::from(property)]));
    let bytes = read_data_reply(s);
    let mut reader = ptp_core::Reader::new(&bytes);
    reader.u16().unwrap()
}

fn read_d621_handles(s: &mut TcpStream, tid: u32) -> Vec<u32> {
    write_frame(s, &op(0x1015, tid, vec![0xd621]));
    let bytes = read_data_reply(s);
    let mut r = ptp_core::Reader::new(&bytes);
    r.ptp_array(|r| r.u32()).unwrap()
}

fn read_reserved_count(s: &mut TcpStream, tid: u32) -> u32 {
    write_frame(s, &op(0x1015, tid, vec![0xd212]));
    let bytes = read_data_reply(s);
    let manifest = camera_config::CameraManifest::from_yaml(&real_gfx_manifest()).unwrap();
    let payload = manifest.properties["0xd212"].payload.as_ref().unwrap();
    let (count_width, code_width, default_value_width) = payload.record_widths();
    let descriptor = protocol_primitives::quirk::RecordStreamDescriptor::new(
        protocol_primitives::quirk::RecordStreamLayout::new(
            count_width,
            code_width,
            default_value_width,
        )
        .unwrap(),
        payload.members.iter().map(|member| {
            let code = camera_config::parse_hex_code(member.code()).unwrap();
            let encoding = match member.encoding(default_value_width) {
                camera_config::RecordValueEncoding::Fixed { width } => {
                    protocol_primitives::quirk::RecordValueEncoding::Fixed { width }
                }
                camera_config::RecordValueEncoding::Signed { width } => {
                    protocol_primitives::quirk::RecordValueEncoding::Signed { width }
                }
                camera_config::RecordValueEncoding::PtpString => {
                    protocol_primitives::quirk::RecordValueEncoding::PtpString
                }
            };
            (code, encoding)
        }),
    )
    .unwrap();
    protocol_primitives::quirk::parse_typed_record_stream(&bytes, &descriptor)
        .unwrap()
        .records
        .into_iter()
        .find_map(|(code, value)| match (code, value) {
            (0xdf41, ptp_core::PropValue::U32(value)) => Some(value),
            _ => None,
        })
        .expect("DF41 reserved count in D212")
}

fn pcss_shutter(s: &mut TcpStream, first_tid: u32) {
    let phases = [0x0001_0000u32, 0x0002_0000, 0x0000_0001];
    let mut tid = first_tid;
    for phase in phases {
        set_prop(s, tid, 0xd039, &phase.to_le_bytes());
        tid += 1;
        write_frame(s, &op(0x100e, tid, vec![0, 0]));
        read_ok(s);
        tid += 1;
    }
}

#[test]
fn service_drives_image_import_over_tcp() {
    let root = tmp_card();
    let rt = tokio::runtime::Runtime::new().unwrap();
    let (command_addr, liveview_addr, control_addr, shutdown_tx, handle) = rt.block_on(async {
        let config = Config {
            instance_id: "test".into(),
            profile: "fuji/gfx100ii/fw0230".into(),
            connection: "app".into(),
            manifest_yaml: MANIFEST.into(),
            media_root: root.clone(),
            command_bind: Some("127.0.0.1:0".parse().unwrap()),
            liveview_bind: Some("127.0.0.1:0".parse().unwrap()),
            event_bind: Some("127.0.0.1:0".parse().unwrap()),
            knock_bind: None,
            pcss_init_fails: 0,
            pcss_shutter_enqueue_count: 0,
            control_bind: "127.0.0.1:0".parse().unwrap(),
            liveview_dir: None,
            state_callback: None,
        };
        let server = Server::bind(config).await.unwrap();
        let cmd = server.command_addr();
        let lv = server
            .liveview_addr_opt()
            .expect("app connection has live-view socket");
        let ctl = server.control_addr();
        let (tx, rx) = tokio::sync::oneshot::channel();
        let h = tokio::spawn(server.run(rx));
        (cmd, lv, ctl, tx, h)
    });

    // --- PTP/IP client flow ---
    let mut s = TcpStream::connect(command_addr).unwrap();
    // Init handshake (standard framing).
    write_frame(&mut s, &app_init_frame(1, "smoke"));
    match PtpIpPacket::decode(&read_frame(&mut s)).unwrap() {
        PtpIpPacket::InitCommandAck(a) => assert_eq!(a.friendly_name, "GFX100 II"),
        other => panic!("expected InitCommandAck, got {other:?}"),
    }

    // OpenSession.
    write_frame(&mut s, &op(0x1002, 1, vec![1]));
    read_ok(&mut s);

    // GetDeviceInfo -> contains model "GFX100 II".
    write_frame(&mut s, &op(0x1001, 2, vec![]));
    let di = ptp_core::DeviceInfo::decode(&read_data_reply(&mut s)).unwrap();
    assert_eq!(di.model, "GFX100 II");

    // GetObjectHandles -> one file.
    write_frame(&mut s, &op(0x1007, 3, vec![0x00010001, 0, 0]));
    let handles_bytes = read_data_reply(&mut s);
    let mut r = ptp_core::Reader::new(&handles_bytes);
    let handles = r.ptp_array(|r| r.u32()).unwrap();
    assert_eq!(handles.len(), 1);

    // Download the JPEG via GetPartialObject (first 7 bytes).
    write_frame(&mut s, &op(0x101b, 4, vec![handles[0], 0, 7]));
    let part = read_data_reply(&mut s);
    assert_eq!(&part, b"\xFF\xD8HELLO");

    // (Live-view framing is covered in `service_streams_liveview_after_open_capture` —
    // image-import path doesn't reach Phase::Streaming so the socket is correctly idle.)
    let _ = liveview_addr;

    // --- control /healthz ---
    let body = http_get(control_addr, "/healthz");
    assert!(body.contains("\"ok\":true"), "healthz body: {body}");
    assert!(
        body.contains("\"sessions\":1"),
        "session should be open: {body}"
    );
    let health_body = body.split("\r\n\r\n").nth(1).unwrap_or("");
    let health: serde_json::Value =
        serde_json::from_str(health_body).expect("healthz body is JSON");
    assert!(
        health["metrics"]["bytes_transferred"].as_u64().unwrap_or(0) > 0,
        "health metrics should count transferred bytes: {body}"
    );
    assert!(
        health["metrics"]["memory_allocated_bytes"]
            .as_u64()
            .is_some(),
        "health metrics should include process memory: {body}"
    );
    assert!(
        health["metrics"]["uptime_ms"].as_u64().is_some(),
        "health metrics should include uptime: {body}"
    );
    assert!(
        health["metrics"]["idle_ms"].as_u64().is_some(),
        "health metrics should include idle time: {body}"
    );
    let observations = http_get(control_addr, "/observations?after=0");
    let export: serde_json::Value = serde_json::from_str(
        observations
            .split_once("\r\n\r\n")
            .expect("observation response body")
            .1,
    )
    .unwrap();
    let bundle = std::iter::once(export["header"].to_string())
        .chain(
            export["records"]
                .as_array()
                .unwrap()
                .iter()
                .map(serde_json::Value::to_string),
        )
        .collect::<Vec<_>>()
        .join("\n");
    camera_config::validate_bundles(&[&bundle]).expect("PTP export is canonical input");

    // Shutdown via control plane.
    let _ = http_post(control_addr, "/shutdown");
    rt.block_on(async {
        let _ = shutdown_tx.send(());
        let _ = handle.await;
    });
    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn service_acknowledges_camera_initiated_queue_after_tcp_delivery() {
    let root = tmp_card_with_jpegs(2);
    let rt = tokio::runtime::Runtime::new().unwrap();
    let (command_addr, control_addr, shutdown_tx, handle) = rt.block_on(async {
        let config = Config {
            instance_id: "reserved-transfer".into(),
            profile: "fuji/gfx100ii/fw0230".into(),
            connection: "app".into(),
            manifest_yaml: real_gfx_manifest(),
            media_root: root,
            command_bind: Some("127.0.0.1:0".parse().unwrap()),
            liveview_bind: Some("127.0.0.1:0".parse().unwrap()),
            event_bind: Some("127.0.0.1:0".parse().unwrap()),
            knock_bind: None,
            pcss_init_fails: 0,
            pcss_shutter_enqueue_count: 0,
            control_bind: "127.0.0.1:0".parse().unwrap(),
            liveview_dir: None,
            state_callback: None,
        };
        let server = Server::bind(config).await.unwrap();
        let command = server.command_addr();
        let control = server.control_addr();
        let (tx, rx) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(server.run(rx));
        (command, control, tx, task)
    });

    let activation = http_patch(
        control_addr,
        "/state",
        r#"{"camera_initiated_transfer_active":true}"#,
    );
    assert!(activation.contains("\"ok\":true"), "body: {activation}");
    let state = http_get(control_addr, "/state");
    let state: serde_json::Value =
        serde_json::from_str(state.split_once("\r\n\r\n").unwrap().1).unwrap();
    assert_eq!(state["transfer_queues"]["camera_initiated"]["queued"], 2);
    assert_eq!(state["transfer_queues"]["camera_initiated"]["completed"], 0);

    let mut stream = connect_ptpip(command_addr, "smoke");
    open_session(&mut stream);
    assert_eq!(read_reserved_count(&mut stream, 2), 2);

    write_frame(&mut stream, &op(0x1008, 3, vec![1]));
    let before_mode = ptp_core::ObjectInfo::decode(&read_data_reply(&mut stream)).unwrap();
    assert_eq!(before_mode.filename, "DSCF0001.JPG");
    set_prop(&mut stream, 4, 0xdf01, &21u16.to_le_bytes());
    write_frame(&mut stream, &op(0x1015, 5, vec![0xdf29]));
    assert_eq!(read_data_reply(&mut stream), 0u32.to_le_bytes());
    set_prop(&mut stream, 6, 0xdf29, &3u32.to_le_bytes());
    write_frame(&mut stream, &op(0x1008, 7, vec![1]));
    let first = ptp_core::ObjectInfo::decode(&read_data_reply(&mut stream)).unwrap();
    assert_eq!(first.filename, before_mode.filename);

    write_frame(
        &mut stream,
        &op(0x101b, 8, vec![1, 0, first.object_compressed_size]),
    );
    assert_eq!(
        read_data_reply(&mut stream).len(),
        first.object_compressed_size as usize
    );
    assert_eq!(read_reserved_count(&mut stream, 9), 1);
    let state = http_get(control_addr, "/state");
    let state: serde_json::Value =
        serde_json::from_str(state.split_once("\r\n\r\n").unwrap().1).unwrap();
    assert_eq!(state["transfer_queues"]["camera_initiated"]["queued"], 1);
    assert_eq!(state["transfer_queues"]["camera_initiated"]["completed"], 1);

    write_frame(&mut stream, &op(0x1008, 10, vec![1]));
    let second = ptp_core::ObjectInfo::decode(&read_data_reply(&mut stream)).unwrap();
    assert_eq!(second.filename, "DSCF0002.JPG");
    write_frame(
        &mut stream,
        &op(0x101b, 11, vec![1, 0, second.object_compressed_size]),
    );
    read_data_reply(&mut stream);
    assert_eq!(read_reserved_count(&mut stream, 12), 0);
    let state = http_get(control_addr, "/state");
    let state: serde_json::Value =
        serde_json::from_str(state.split_once("\r\n\r\n").unwrap().1).unwrap();
    assert_eq!(state["transfer_queues"]["camera_initiated"]["queued"], 0);
    assert_eq!(state["transfer_queues"]["camera_initiated"]["completed"], 2);

    write_frame(&mut stream, &op(0x1003, 13, vec![]));
    read_ok(&mut stream);

    let _ = shutdown_tx.send(());
    rt.block_on(handle).unwrap();
}

#[test]
fn service_times_out_d620_until_image_import_bootstrap_completes() {
    let root = tmp_card();
    let rt = tokio::runtime::Runtime::new().unwrap();
    let (command_addr, shutdown_tx, handle) = rt.block_on(async {
        let config = Config {
            instance_id: "test".into(),
            profile: "fuji/gfx100ii/fw0230".into(),
            connection: "app".into(),
            manifest_yaml: real_gfx_manifest(),
            media_root: root.clone(),
            command_bind: Some("127.0.0.1:0".parse().unwrap()),
            liveview_bind: Some("127.0.0.1:0".parse().unwrap()),
            event_bind: Some("127.0.0.1:0".parse().unwrap()),
            knock_bind: None,
            pcss_init_fails: 0,
            pcss_shutter_enqueue_count: 0,
            control_bind: "127.0.0.1:0".parse().unwrap(),
            liveview_dir: None,
            state_callback: None,
        };
        let server = Server::bind(config).await.unwrap();
        let cmd = server.command_addr();
        let (tx, rx) = tokio::sync::oneshot::channel();
        let h = tokio::spawn(server.run(rx));
        (cmd, tx, h)
    });

    let mut s = connect_ptpip(command_addr, "smoke");
    s.set_read_timeout(Some(std::time::Duration::from_secs(2)))
        .unwrap();
    write_frame(&mut s, &op(0x1002, 1, vec![1]));
    read_ok(&mut s);

    write_frame(&mut s, &op(0x1007, 20, vec![0x00010001, 0, 0]));
    assert_eq!(
        read_response_code(&mut s),
        0x2005,
        "app persona rejects GetObjectHandles; it enumerates through D620/D621"
    );

    write_frame(&mut s, &op(0x1016, 2, vec![0xdf01]));
    write_frame(
        &mut s,
        &fuji_framing::encode_data(0x1016, 2, &0x14u16.to_le_bytes()),
    );
    read_ok(&mut s);

    s.set_read_timeout(Some(std::time::Duration::from_millis(250)))
        .unwrap();
    write_frame(&mut s, &op(0x1015, 3, vec![0xd620]));
    assert_read_timeout(&mut s);
    s.set_read_timeout(Some(std::time::Duration::from_secs(2)))
        .unwrap();

    write_frame(&mut s, &op(0x1015, 4, vec![0xd212]));
    let _ = read_data_reply(&mut s); // D212 preamble starts the gate.
    set_prop(&mut s, 5, 0xdf01, &0x14u16.to_le_bytes());
    write_frame(&mut s, &op(0x1015, 6, vec![0xdf28]));
    let _ = read_data_reply(&mut s);
    set_prop(&mut s, 7, 0xdf28, &3u32.to_le_bytes());
    set_prop(&mut s, 8, 0xd226, &0u16.to_le_bytes());
    set_prop(&mut s, 9, 0xd227, &0u16.to_le_bytes());
    write_frame(&mut s, &op(0x1015, 10, vec![0xd244]));
    let _ = read_data_reply(&mut s);
    write_frame(&mut s, &op(0x9054, 11, vec![0x1000_0001]));
    read_ok(&mut s);
    write_frame(&mut s, &op(0x9055, 12, vec![0x1000_0001]));
    read_ok(&mut s);
    write_frame(&mut s, &op(0x9050, 13, vec![]));
    read_ok(&mut s);
    write_frame(&mut s, &op(0x1015, 14, vec![0xd212]));
    let _ = read_data_reply(&mut s);
    write_frame(&mut s, &op(0x1015, 15, vec![0xd22b]));
    let _ = read_data_reply(&mut s);
    write_frame(&mut s, &op(0x9053, 16, vec![0, 0x7530]));
    read_ok(&mut s);
    write_frame(&mut s, &op(0x1015, 17, vec![0xd212]));
    let _ = read_data_reply(&mut s);

    write_frame(&mut s, &op(0x1015, 18, vec![0xd620]));
    let count = read_data_reply(&mut s);
    let mut r = ptp_core::Reader::new(&count);
    assert_eq!(r.u32().unwrap(), 1);
    write_frame(&mut s, &op(0x1015, 19, vec![0xd621]));
    let handles_bytes = read_data_reply(&mut s);
    let mut r = ptp_core::Reader::new(&handles_bytes);
    let handles = r.ptp_array(|r| r.u32()).unwrap();
    assert_eq!(handles.len(), 1);

    write_frame(&mut s, &op(0x1008, 21, vec![handles[0]]));
    let info = read_data_reply(&mut s);
    let oi = ptp_core::ObjectInfo::decode(&info).unwrap();
    assert_eq!(oi.object_format, 0x3801);

    write_frame(&mut s, &op(0x1015, 22, vec![0xd235]));
    let chunk = read_data_reply(&mut s);
    let mut r = ptp_core::Reader::new(&chunk);
    assert_eq!(r.u32().unwrap(), 0x00bfffe0);

    write_frame(&mut s, &op(0x101b, 23, vec![handles[0], 0, 7, 0]));
    assert_eq!(read_data_reply(&mut s), b"\xFF\xD8HELLO");

    let _ = shutdown_tx.send(());
    rt.block_on(handle).unwrap();
    std::fs::remove_dir_all(&root).ok();
}

/// Completion events from the production GFX100 II manifest reach the real
/// app-persona event socket instead of leaving shutter/AF await steps at their
/// timeout ceilings.
#[test]
fn service_pushes_gfx_shutter_and_autofocus_events_on_event_socket() {
    let root = tmp_card();
    let rt = tokio::runtime::Runtime::new().unwrap();
    let (command_addr, event_addr, control_addr, shutdown_tx, handle) = rt.block_on(async {
        let config = Config {
            instance_id: "test".into(),
            profile: "fuji/gfx100ii/fw0230".into(),
            connection: "app".into(),
            manifest_yaml: real_gfx_manifest(),
            media_root: root.clone(),
            command_bind: Some("127.0.0.1:0".parse().unwrap()),
            liveview_bind: Some("127.0.0.1:0".parse().unwrap()),
            event_bind: Some("127.0.0.1:0".parse().unwrap()),
            knock_bind: None,
            pcss_init_fails: 0,
            pcss_shutter_enqueue_count: 0,
            control_bind: "127.0.0.1:0".parse().unwrap(),
            liveview_dir: None,
            state_callback: None,
        };
        let server = Server::bind(config).await.unwrap();
        let cmd = server.command_addr();
        let evt = server
            .event_addr_opt()
            .expect("app connection has event socket");
        let control = server.control_addr();
        let (tx, rx) = tokio::sync::oneshot::channel();
        let h = tokio::spawn(server.run(rx));
        (cmd, evt, control, tx, h)
    });

    // Command-socket session and live-view entry. The real manifest does not
    // accept the event role until its openChannel boundary is reached.
    let mut s = TcpStream::connect(command_addr).unwrap();
    write_frame(&mut s, &app_init_frame(1, "smoke"));
    let _ = read_frame(&mut s); // InitCommandAck
    write_frame(&mut s, &op(0x1002, 1, vec![1]));
    read_ok(&mut s);
    set_prop(&mut s, 2, 0xdf00, &6u16.to_le_bytes());
    set_prop(&mut s, 3, 0xdf01, &0x16u16.to_le_bytes());
    write_frame(&mut s, &op(0x1015, 4, vec![0xdf2a]));
    let df2a = read_data_reply(&mut s);
    set_prop(&mut s, 5, 0xdf2a, &df2a);
    for tid in 6..10 {
        write_frame(&mut s, &op(0x902b, tid, vec![]));
        read_ok(&mut s);
    }
    write_frame(&mut s, &op(0x101c, 10, vec![]));
    read_ok(&mut s);

    let mut evt = TcpStream::connect(event_addr).unwrap();
    evt.set_read_timeout(Some(std::time::Duration::from_secs(2)))
        .unwrap();
    write_frame(&mut s, &op(0x1015, 11, vec![0xd212]));
    let _ = read_data_reply(&mut s);
    // Accepting the auxiliary socket installs its broadcast subscription on a
    // sibling task; let that bounded setup finish before emitting the event.
    std::thread::sleep(std::time::Duration::from_millis(20));

    // Shutter: 0x100E emits the 0xC001 postview-complete event awaited by the
    // manifest action before its cleanup operation.
    write_frame(&mut s, &op(0x100e, 12, vec![0, 0]));
    read_ok(&mut s);
    match protocol_primitives::usb_ptp::decode(&read_frame(&mut evt)).unwrap() {
        PtpIpPacket::Event(e) => assert_eq!(e.code, 0xc001),
        other => panic!("expected shutter Event packet, got {other:?}"),
    }

    // Tap-to-AF: the op emits 0xC005 and its D209 result settles through the
    // same command socket the app uses for the post-event await.
    write_frame(&mut s, &op(0x9026, 13, vec![0x0906_0403]));
    read_ok(&mut s);

    // The push follows the manifest's USB/PIMA event framing.
    match protocol_primitives::usb_ptp::decode(&read_frame(&mut evt)).unwrap() {
        PtpIpPacket::Event(e) => {
            assert_eq!(e.code, 0xc005, "AFCAPTUER event code");
            assert_eq!(e.transaction_id, 0, "async event uses tid 0");
        }
        other => panic!("expected Event packet, got {other:?}"),
    }

    write_frame(&mut s, &op(0x1015, 14, vec![0xd209]));
    let first = read_data_reply(&mut s);
    assert_eq!(u16::from_le_bytes([first[0], first[1]]), 0);
    write_frame(&mut s, &op(0x1015, 15, vec![0xd209]));
    let settled = read_data_reply(&mut s);
    assert_eq!(u16::from_le_bytes([settled[0], settled[1]]), 1);

    let observations = http_get(control_addr, "/observations?after=0");
    let export: serde_json::Value = serde_json::from_str(
        observations
            .split_once("\r\n\r\n")
            .expect("observation response body")
            .1,
    )
    .unwrap();
    let records = export["records"].as_array().unwrap();
    let transaction = records
        .iter()
        .find(|record| record["kind"] == "ptpTransaction" && record["transactionId"] == 13)
        .expect("AF transaction observation");
    let event = records
        .iter()
        .find(|record| record["kind"] == "ptpEvent" && record["event"] == "0xc005")
        .expect("AF event observation");
    assert_eq!(event["transactionId"], 0);
    assert_eq!(event["transactionRecordId"], transaction["recordId"]);
    assert_eq!(
        event["connectionInstance"],
        transaction["connectionInstance"]
    );
    assert_eq!(event["session"], transaction["session"]);

    rt.block_on(async {
        let _ = shutdown_tx.send(());
        let _ = handle.await;
    });
    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn service_serves_a_large_object_in_a_single_frame() {
    // A 2 MiB JPEG exceeds the sim's 1 MiB internal read chunk, so the body is
    // streamed from disk in multiple reads — yet it arrives as one type-2 frame,
    // matching real Fuji (a whole GetObject is a single frame, even at 14.5 MB).
    let root = {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let r = std::env::temp_dir().join(format!("ptpsim-bigobj-{nanos}"));
        std::fs::create_dir_all(r.join("DCIM/100_FUJI")).unwrap();
        let mut body = vec![0u8; 2 * 1024 * 1024];
        body[0] = 0xFF;
        body[1] = 0xD8;
        let n = body.len();
        body[n - 2] = 0xFF;
        body[n - 1] = 0xD9;
        std::fs::write(r.join("DCIM/100_FUJI/BIG.JPG"), &body).unwrap();
        r
    };

    let rt = tokio::runtime::Runtime::new().unwrap();
    let (command_addr, control_addr, shutdown_tx, handle) = rt.block_on(async {
        let config = Config {
            instance_id: "test".into(),
            profile: "fuji/gfx100ii".into(),
            connection: "app".into(),
            manifest_yaml: MANIFEST.into(),
            media_root: root.clone(),
            command_bind: Some("127.0.0.1:0".parse().unwrap()),
            liveview_bind: Some("127.0.0.1:0".parse().unwrap()),
            event_bind: Some("127.0.0.1:0".parse().unwrap()),
            knock_bind: None,
            pcss_init_fails: 0,
            pcss_shutter_enqueue_count: 0,
            control_bind: "127.0.0.1:0".parse().unwrap(),
            liveview_dir: None,
            state_callback: None,
        };
        let server = Server::bind(config).await.unwrap();
        let cmd = server.command_addr();
        let control = server.control_addr();
        let (tx, rx) = tokio::sync::oneshot::channel();
        let h = tokio::spawn(server.run(rx));
        (cmd, control, tx, h)
    });

    let mut s = TcpStream::connect(command_addr).unwrap();
    write_frame(&mut s, &app_init_frame(1, "bigobj"));
    match PtpIpPacket::decode(&read_frame(&mut s)).unwrap() {
        PtpIpPacket::InitCommandAck(_) => {}
        other => panic!("expected InitCommandAck, got {other:?}"),
    }
    write_frame(&mut s, &op(0x1002, 1, vec![1]));
    read_ok(&mut s);

    // GetObjectHandles -> one file.
    write_frame(&mut s, &op(0x1007, 2, vec![0x00010001, 0, 0]));
    let handles_bytes = read_data_reply(&mut s);
    let mut r = ptp_core::Reader::new(&handles_bytes);
    let handles = r.ptp_array(|r| r.u32()).unwrap();
    assert_eq!(handles.len(), 1);

    // GetObject — the 2 MiB JPEG, delivered whole in a single frame.
    write_frame(&mut s, &op(0x1009, 3, vec![handles[0]]));
    let payload = read_data_reply(&mut s);
    assert_eq!(payload.len(), 2 * 1024 * 1024);
    assert_eq!(&payload[0..2], &[0xFF, 0xD8]);
    assert_eq!(&payload[payload.len() - 2..], &[0xFF, 0xD9]);

    let observations = http_get(control_addr, "/observations?after=0");
    let export: serde_json::Value = serde_json::from_str(
        observations
            .split_once("\r\n\r\n")
            .expect("observation response body")
            .1,
    )
    .unwrap();
    let transaction = export["records"]
        .as_array()
        .unwrap()
        .iter()
        .find(|record| record["kind"] == "ptpTransaction" && record["transactionId"] == 3)
        .expect("GetObject transaction observation");
    let metadata = &transaction["response"]["data"]["payload"];
    let expected = camera_config::payload_metadata(&payload);
    assert_eq!(transaction["outcome"], "ok");
    assert_eq!(metadata["length"], expected.length);
    assert_eq!(metadata["sha256"], expected.sha256);

    rt.block_on(async {
        let _ = shutdown_tx.send(());
        let _ = handle.await;
    });
    std::fs::remove_dir_all(&root).ok();
}

struct LiveViewPacket {
    total_len: u32,
    reserved0: u32,
    frame_counter: u32,
    jpeg_body_offset_adjust: u32,
    reserved_pad: u16,
    jpeg: Vec<u8>,
}

/// Read the raw capture-compatible packet so service coverage verifies the header rather than
/// round-tripping through the protocol primitive that produced it.
fn read_frame_lv(s: &mut TcpStream) -> LiveViewPacket {
    let mut header = [0u8; protocol_primitives::liveview::HEADER_LEN];
    s.read_exact(&mut header).unwrap();
    let total_len = u32::from_le_bytes(header[0..4].try_into().unwrap());
    let jpeg_len = total_len as usize - header.len();
    let mut jpeg = vec![0u8; jpeg_len];
    s.read_exact(&mut jpeg).unwrap();
    LiveViewPacket {
        total_len,
        reserved0: u32::from_le_bytes(header[4..8].try_into().unwrap()),
        frame_counter: u32::from_le_bytes(header[8..12].try_into().unwrap()),
        jpeg_body_offset_adjust: u32::from_le_bytes(header[12..16].try_into().unwrap()),
        reserved_pad: u16::from_le_bytes(header[16..18].try_into().unwrap()),
        jpeg,
    }
}

fn http_get(addr: std::net::SocketAddr, path: &str) -> String {
    let mut s = TcpStream::connect(addr).unwrap();
    write!(
        s,
        "GET {path} HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n"
    )
    .unwrap();
    let mut out = String::new();
    s.read_to_string(&mut out).unwrap();
    out
}

fn trace_json(addr: std::net::SocketAddr) -> serde_json::Value {
    let response = http_get(addr, "/trace?after=0");
    let body = response
        .split_once("\r\n\r\n")
        .expect("trace HTTP response has a body")
        .1;
    serde_json::from_str(body).expect("trace response is JSON")
}

fn http_post(addr: std::net::SocketAddr, path: &str) -> String {
    let mut s = TcpStream::connect(addr).unwrap();
    write!(
        s,
        "POST {path} HTTP/1.1\r\nHost: x\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
    )
    .unwrap();
    let mut out = String::new();
    let _ = s.read_to_string(&mut out);
    out
}

fn http_post_json(addr: std::net::SocketAddr, path: &str, body: &str) -> String {
    let mut s = TcpStream::connect(addr).unwrap();
    write!(
        s,
        "POST {path} HTTP/1.1\r\nHost: x\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
    .unwrap();
    let mut out = String::new();
    let _ = s.read_to_string(&mut out);
    out
}

fn http_delete(addr: std::net::SocketAddr, path: &str) -> String {
    let mut s = TcpStream::connect(addr).unwrap();
    write!(
        s,
        "DELETE {path} HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n"
    )
    .unwrap();
    let mut out = String::new();
    let _ = s.read_to_string(&mut out);
    out
}

fn http_patch(addr: std::net::SocketAddr, path: &str, body: &str) -> String {
    let mut s = TcpStream::connect(addr).unwrap();
    write!(
        s,
        "PATCH {path} HTTP/1.1\r\nHost: x\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
    .unwrap();
    let mut out = String::new();
    let _ = s.read_to_string(&mut out);
    out
}

fn http_body(response: &str) -> &str {
    response
        .split_once("\r\n\r\n")
        .expect("HTTP response body")
        .1
}

#[test]
fn fault_registry_crud_round_trips_every_mutation_and_rejects_invalid_specs() {
    let root = tmp_card();
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let (_, control, shutdown, handle) = start_fault_server(&runtime, &root);
    let mutations = [
        serde_json::json!({"type":"failResponse","response":"0x2019"}),
        serde_json::json!({"type":"close","stage":"command"}),
        serde_json::json!({"type":"delay","stage":"data","ms":25}),
        serde_json::json!({"type":"suppress","stage":"response"}),
        serde_json::json!({"type":"truncateData","keep":3}),
        serde_json::json!({"type":"replaceData","bytesHex":"deadbeef"}),
        serde_json::json!({"type":"replaceTransactionId","transactionId":42}),
        serde_json::json!({"type":"dataFraming","framing":"compressed"}),
        serde_json::json!({"type":"propertyReadback","value":-7}),
    ];
    let mut ids = Vec::new();
    for mutation in &mutations {
        let body = serde_json::json!({
            "operation": "0x1015",
            "params": [53],
            "skip": 2,
            "count": 1,
            "mutation": mutation,
        });
        let response = http_post_json(control, "/faults", &body.to_string());
        assert!(response.starts_with("HTTP/1.1 200"), "{response}");
        let response: serde_json::Value = serde_json::from_str(http_body(&response)).unwrap();
        ids.push(response["id"].as_u64().unwrap());
    }

    let response = http_get(control, "/faults");
    let snapshot: serde_json::Value = serde_json::from_str(http_body(&response)).unwrap();
    let faults = snapshot["faults"].as_array().unwrap();
    assert_eq!(faults.len(), mutations.len());
    for (fault, mutation) in faults.iter().zip(mutations.iter()) {
        assert_eq!(&fault["mutation"], mutation);
        assert_eq!(fault["seen"], 0);
        assert_eq!(fault["applied"], 0);
        assert_eq!(fault["exhausted"], false);
    }
    assert_eq!(snapshot["lastApplied"], serde_json::Value::Null);

    let response = http_delete(control, &format!("/faults/{}", ids[3]));
    assert!(response.starts_with("HTTP/1.1 200"));
    let response = http_get(control, "/faults");
    let snapshot: serde_json::Value = serde_json::from_str(http_body(&response)).unwrap();
    assert_eq!(
        snapshot["faults"].as_array().unwrap().len(),
        mutations.len() - 1
    );
    assert!(http_delete(control, "/faults/99999").starts_with("HTTP/1.1 404"));

    let invalid = [
        r#"{"operation":"1015","mutation":{"type":"suppress","stage":"data"}}"#.to_string(),
        r#"{"operation":"0x1015","mutation":{"type":"replaceData","bytesHex":"abc"}}"#.to_string(),
        serde_json::json!({
            "operation":"0x1015",
            "mutation":{"type":"replaceData","bytesHex":"00".repeat(4097)}
        })
        .to_string(),
        r#"{"operation":"0x1015","mutation":{"type":"delay","stage":"data","ms":60001}}"#
            .to_string(),
        r#"{"operation":"0x1015","mutation":{"type":"close","stage":"init"}}"#.to_string(),
        r#"{"operation":"0x1015","mutation":{"type":"corrupt"}}"#.to_string(),
        r#"{"operation":"0x1015","mutation":{"type":"suppress","stage":"data"},"extra":true}"#
            .to_string(),
    ];
    for body in invalid {
        let response = http_post_json(control, "/faults", &body);
        assert!(response.starts_with("HTTP/1.1 400"), "{response}");
    }

    assert!(http_delete(control, "/faults").starts_with("HTTP/1.1 200"));
    let response = http_get(control, "/faults");
    let snapshot: serde_json::Value = serde_json::from_str(http_body(&response)).unwrap();
    assert!(snapshot["faults"].as_array().unwrap().is_empty());

    shutdown.send(()).ok();
    runtime.block_on(handle).unwrap();
    std::fs::remove_dir_all(root).ok();
}

#[test]
fn occurrence_windows_cross_reconnects_and_fault_trace_is_structured() {
    let root = tmp_card();
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let (command, control, shutdown, handle) = start_fault_server(&runtime, &root);
    let spec = r#"{"operation":"0x1002","skip":2,"count":1,"mutation":{"type":"failResponse","response":"0x2019"}}"#;
    let response = http_post_json(control, "/faults", spec);
    assert!(response.starts_with("HTTP/1.1 200"), "{response}");

    let mut delivered = Vec::new();
    for occurrence in 0..4 {
        let mut stream = connect_ptpip(command, &format!("fault-{occurrence}"));
        let request = op(0x1002, 1, vec![1]);
        write_frame(&mut stream, &request);
        let response = read_frame(&mut stream);
        delivered.push(response.clone());
        let code = match fuji_framing::decode(&response).unwrap() {
            PtpIpPacket::OperationResponse(response) => response.code,
            other => panic!("expected response, got {other:?}"),
        };
        assert_eq!(code, if occurrence == 2 { 0x2019 } else { 0x2001 });
        if occurrence != 2 {
            // The engine refuses a second open while a session is active
            // (#407), so release the session before the next connection opens
            // one. Occurrence 2's fault short-circuits before the session
            // opens, leaving nothing to close.
            write_frame(&mut stream, &op(0x1003, 2, vec![]));
            read_ok(&mut stream);
        }
    }
    assert_eq!(delivered[0], delivered[1]);
    assert_eq!(delivered[1], delivered[3]);

    let mut reopened = connect_ptpip(command, "close-open");
    write_frame(&mut reopened, &op(0x1002, 2, vec![1]));
    read_ok(&mut reopened);
    write_frame(&mut reopened, &op(0x1003, 3, vec![]));
    read_ok(&mut reopened);
    write_frame(&mut reopened, &op(0x1002, 4, vec![1]));
    read_ok(&mut reopened);

    let snapshot: serde_json::Value =
        serde_json::from_str(http_body(&http_get(control, "/faults"))).unwrap();
    assert_eq!(snapshot["faults"][0]["seen"], 6);
    assert_eq!(snapshot["faults"][0]["applied"], 1);
    assert_eq!(snapshot["faults"][0]["exhausted"], true);

    let trace = trace_json(control);
    let fault_events = trace["events"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|event| event["kind"] == "ptpip.fault.applied")
        .collect::<Vec<_>>();
    assert_eq!(fault_events.len(), 1);
    let event = fault_events[0];
    assert_eq!(event["operation"], "0x1002");
    assert_eq!(event["transaction_id"], 1);
    assert_eq!(event["response_code"], "0x2019");
    assert_eq!(event["fault_id"], 1);
    assert_eq!(event["fault_kind"], "failResponse");
    assert_eq!(event["applied"], "failedResponse");
    assert!(event
        .get("payload_hex")
        .is_some_and(serde_json::Value::is_null));

    shutdown.send(()).ok();
    runtime.block_on(handle).unwrap();
    std::fs::remove_dir_all(root).ok();
}

#[test]
fn deleting_an_unapplied_fault_restores_reference_bytes_and_emits_no_fault_trace() {
    let root = tmp_card();
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let (command, control, shutdown, handle) = start_fault_server(&runtime, &root);

    let mut reference = connect_ptpip(command, "reference");
    write_frame(&mut reference, &op(0x1002, 1, vec![1]));
    let reference_bytes = read_frame(&mut reference);
    // Free the shared session so the next connection can open one (#407).
    write_frame(&mut reference, &op(0x1003, 2, vec![]));
    read_ok(&mut reference);

    let response = http_post_json(
        control,
        "/faults",
        r#"{"operation":"0x1002","mutation":{"type":"failResponse","response":"0x2019"}}"#,
    );
    let id = serde_json::from_str::<serde_json::Value>(http_body(&response)).unwrap()["id"]
        .as_u64()
        .unwrap();
    assert!(http_delete(control, &format!("/faults/{id}")).starts_with("HTTP/1.1 200"));

    let mut after_delete = connect_ptpip(command, "after-delete");
    write_frame(&mut after_delete, &op(0x1002, 1, vec![1]));
    assert_eq!(read_frame(&mut after_delete), reference_bytes);
    let trace = trace_json(control);
    for event in trace["events"].as_array().unwrap() {
        assert_ne!(event["kind"], "ptpip.fault.applied");
        assert!(event.get("fault_id").is_none());
        assert!(event.get("fault_kind").is_none());
    }

    shutdown.send(()).ok();
    runtime.block_on(handle).unwrap();
    std::fs::remove_dir_all(root).ok();
}

fn with_fault_service(test: impl FnOnce(std::net::SocketAddr, std::net::SocketAddr)) {
    let root = tmp_card();
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let (command, control, shutdown, handle) = start_fault_server(&runtime, &root);
    test(command, control);
    shutdown.send(()).ok();
    runtime.block_on(handle).unwrap();
    std::fs::remove_dir_all(root).ok();
}

fn with_standard_fault_service(test: impl FnOnce(std::net::SocketAddr, std::net::SocketAddr)) {
    let root = tmp_card();
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let (command, control, shutdown, handle) = start_standard_fault_server(&runtime, &root);
    test(command, control);
    shutdown.send(()).ok();
    runtime.block_on(handle).unwrap();
    std::fs::remove_dir_all(root).ok();
}

fn connect_standard_ptpip(command: std::net::SocketAddr) -> TcpStream {
    let mut stream = TcpStream::connect(command).unwrap();
    let request = PtpIpPacket::InitCommandRequest(ptp_core::InitCommandRequest {
        initiator_guid: [0x11; 16],
        friendly_name: "fault-test".into(),
        protocol_version: 0x0001_0000,
    });
    write_frame(&mut stream, &ptp_core::encode(&request).unwrap());
    assert!(matches!(
        PtpIpPacket::decode(&read_frame(&mut stream)).unwrap(),
        PtpIpPacket::InitCommandAck(_)
    ));
    stream
}

fn install_fault(control: std::net::SocketAddr, body: serde_json::Value) -> u64 {
    let response = http_post_json(control, "/faults", &body.to_string());
    assert!(response.starts_with("HTTP/1.1 200"), "{response}");
    serde_json::from_str::<serde_json::Value>(http_body(&response)).unwrap()["id"]
        .as_u64()
        .unwrap()
}

fn clear_faults(control: std::net::SocketAddr) {
    assert!(http_delete(control, "/faults").starts_with("HTTP/1.1 200"));
}

fn assert_socket_closed(stream: &mut TcpStream) {
    stream
        .set_read_timeout(Some(std::time::Duration::from_secs(1)))
        .unwrap();
    let mut byte = [0u8; 1];
    assert_eq!(stream.read(&mut byte).unwrap(), 0);
}

#[test]
fn fail_response_fault_is_observed_as_a_non_ok_response() {
    with_fault_service(|command, control| {
        install_fault(
            control,
            serde_json::json!({
                "operation":"0x1002",
                "count":1,
                "mutation":{"type":"failResponse","response":"0x2019"}
            }),
        );
        let mut stream = connect_ptpip(command, "fail-response");
        write_frame(&mut stream, &op(0x1002, 1, vec![1]));
        assert_eq!(read_response_code(&mut stream), 0x2019);
    });
}

#[test]
fn close_fault_drops_the_socket_at_command_data_and_response_stages() {
    with_fault_service(|command, control| {
        for stage in ["command", "data"] {
            install_fault(
                control,
                serde_json::json!({
                    "operation":"0x1001",
                    "count":1,
                    "mutation":{"type":"close","stage":stage}
                }),
            );
            let mut stream = connect_ptpip(command, stage);
            write_frame(&mut stream, &op(0x1001, 1, vec![]));
            assert_socket_closed(&mut stream);
            clear_faults(control);
        }

        install_fault(
            control,
            serde_json::json!({
                "operation":"0x1001",
                "count":1,
                "mutation":{"type":"close","stage":"response"}
            }),
        );
        let mut stream = connect_ptpip(command, "close-response");
        write_frame(&mut stream, &op(0x1001, 1, vec![]));
        assert!(matches!(
            fuji_framing::decode(&read_frame(&mut stream)).unwrap(),
            PtpIpPacket::Data(_)
        ));
        assert_socket_closed(&mut stream);
    });
}

#[test]
fn data_stage_fault_on_response_only_reply_reports_no_data_phase() {
    with_fault_service(|command, control| {
        install_fault(
            control,
            serde_json::json!({
                "operation":"0x1002",
                "count":1,
                "mutation":{"type":"close","stage":"data"}
            }),
        );
        let mut stream = connect_ptpip(command, "close-data-without-data");
        write_frame(&mut stream, &op(0x1002, 1, vec![1]));
        read_ok(&mut stream);

        write_frame(&mut stream, &op(0x1003, 2, vec![]));
        read_ok(&mut stream);

        let trace = trace_json(control);
        let event = trace["events"]
            .as_array()
            .unwrap()
            .iter()
            .find(|event| event["kind"] == "ptpip.fault.applied")
            .unwrap();
        assert_eq!(event["operation"], "0x1002");
        assert_eq!(event["response_code"], "0x2001");
        assert_eq!(event["applied"], "noDataPhase");
    });
}

#[test]
fn delay_fault_waits_before_the_selected_data_or_response_phase() {
    with_fault_service(|command, control| {
        install_fault(
            control,
            serde_json::json!({
                "operation":"0x1001",
                "count":1,
                "mutation":{"type":"delay","stage":"data","ms":20}
            }),
        );
        let mut stream = connect_ptpip(command, "delay-data");
        let started = std::time::Instant::now();
        write_frame(&mut stream, &op(0x1001, 1, vec![]));
        let _ = read_frame(&mut stream);
        assert!(started.elapsed() >= std::time::Duration::from_millis(20));
        let _ = read_frame(&mut stream);
        clear_faults(control);

        install_fault(
            control,
            serde_json::json!({
                "operation":"0x1001",
                "count":1,
                "mutation":{"type":"delay","stage":"response","ms":20}
            }),
        );
        let mut stream = connect_ptpip(command, "delay-response");
        let started = std::time::Instant::now();
        write_frame(&mut stream, &op(0x1001, 1, vec![]));
        let _ = read_frame(&mut stream);
        let _ = read_frame(&mut stream);
        assert!(started.elapsed() >= std::time::Duration::from_millis(20));
    });
}

#[test]
fn suppress_fault_omits_only_the_selected_data_or_response_phase() {
    with_fault_service(|command, control| {
        install_fault(
            control,
            serde_json::json!({
                "operation":"0x1001",
                "count":1,
                "mutation":{"type":"suppress","stage":"data"}
            }),
        );
        let mut stream = connect_ptpip(command, "suppress-data");
        write_frame(&mut stream, &op(0x1001, 1, vec![]));
        assert!(matches!(
            fuji_framing::decode(&read_frame(&mut stream)).unwrap(),
            PtpIpPacket::OperationResponse(response) if response.code == 0x2001
        ));
        clear_faults(control);

        install_fault(
            control,
            serde_json::json!({
                "operation":"0x1001",
                "count":1,
                "mutation":{"type":"suppress","stage":"response"}
            }),
        );
        let mut stream = connect_ptpip(command, "suppress-response");
        stream
            .set_read_timeout(Some(std::time::Duration::from_millis(100)))
            .unwrap();
        write_frame(&mut stream, &op(0x1001, 1, vec![]));
        assert!(matches!(
            fuji_framing::decode(&read_frame(&mut stream)).unwrap(),
            PtpIpPacket::Data(_)
        ));
        assert_read_timeout(&mut stream);
    });
}

#[test]
fn truncate_data_fault_delivers_a_short_but_framed_payload() {
    with_fault_service(|command, control| {
        install_fault(
            control,
            serde_json::json!({
                "operation":"0x1001",
                "count":1,
                "mutation":{"type":"truncateData","keep":3}
            }),
        );
        let mut stream = connect_ptpip(command, "truncate-data");
        write_frame(&mut stream, &op(0x1001, 1, vec![]));
        assert_eq!(read_data_reply(&mut stream).len(), 3);
    });
}

#[test]
fn replace_data_fault_delivers_exact_configured_bytes() {
    with_fault_service(|command, control| {
        install_fault(
            control,
            serde_json::json!({
                "operation":"0x1001",
                "count":1,
                "mutation":{"type":"replaceData","bytesHex":"deadbeef"}
            }),
        );
        let mut stream = connect_ptpip(command, "replace-data");
        write_frame(&mut stream, &op(0x1001, 1, vec![]));
        assert_eq!(read_data_reply(&mut stream), [0xde, 0xad, 0xbe, 0xef]);
        let trace = trace_json(control);
        let event = trace["events"]
            .as_array()
            .unwrap()
            .iter()
            .find(|event| event["kind"] == "ptpip.fault.applied")
            .unwrap();
        let expected = camera_config::payload_metadata(&[0xde, 0xad, 0xbe, 0xef]);
        assert_eq!(event["payload_summary"]["length"], expected.length);
        assert_eq!(event["payload_summary"]["sha256"], expected.sha256);
        assert_eq!(event["payload_hex"], serde_json::Value::Null);
    });
}

#[test]
fn replace_transaction_id_fault_changes_data_and_response_ids() {
    with_fault_service(|command, control| {
        install_fault(
            control,
            serde_json::json!({
                "operation":"0x1001",
                "count":1,
                "mutation":{"type":"replaceTransactionId","transactionId":99}
            }),
        );
        let mut stream = connect_ptpip(command, "replace-tid");
        write_frame(&mut stream, &op(0x1001, 7, vec![]));
        assert!(matches!(
            fuji_framing::decode(&read_frame(&mut stream)).unwrap(),
            PtpIpPacket::Data(data) if data.transaction_id == 99
        ));
        assert!(matches!(
            fuji_framing::decode(&read_frame(&mut stream)).unwrap(),
            PtpIpPacket::OperationResponse(response) if response.transaction_id == 99
        ));
    });
}

#[test]
fn data_framing_fault_changes_only_the_data_phase_codec() {
    with_standard_fault_service(|command, control| {
        install_fault(
            control,
            serde_json::json!({
                "operation":"0x1001",
                "count":1,
                "mutation":{"type":"dataFraming","framing":"compressed"}
            }),
        );
        let mut stream = connect_standard_ptpip(command);
        let request = PtpIpPacket::OperationRequest(ptp_core::OperationRequest {
            data_phase_info: 1,
            code: 0x1001,
            transaction_id: 7,
            params: vec![],
        });
        write_frame(&mut stream, &ptp_core::encode(&request).unwrap());
        assert!(matches!(
            fuji_framing::decode(&read_frame(&mut stream)).unwrap(),
            PtpIpPacket::Data(data) if data.transaction_id == 7
        ));
        assert!(matches!(
            PtpIpPacket::decode(&read_frame(&mut stream)).unwrap(),
            PtpIpPacket::OperationResponse(response) if response.transaction_id == 7
        ));
    });
}

#[test]
fn property_readback_fault_reencodes_through_the_manifest_datatype() {
    with_fault_service(|command, control| {
        let mut stream = connect_ptpip(command, "property-readback");
        open_session(&mut stream);
        set_prop(&mut stream, 2, 0x5007, &280u16.to_le_bytes());
        install_fault(
            control,
            serde_json::json!({
                "operation":"0x1015",
                "params":[0x5007],
                "count":1,
                "mutation":{"type":"propertyReadback","value":400}
            }),
        );
        assert_eq!(read_u16_prop(&mut stream, 3, 0x5007), 400);
    });
}

#[test]
fn service_rejects_oversized_data_in() {
    // Reproducer for the "no upper bound on data-in collection" finding: a
    // client that declares an 8-byte data phase but keeps sending Data frames
    // is shut down rather than accumulated forever. The fix must close the
    // connection cleanly (TCP RST or EOF) — never accept the runaway payload.
    let root = {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let r = std::env::temp_dir().join(format!("ptpsim-overflow-{nanos}"));
        std::fs::create_dir_all(r.join("DCIM/100_FUJI")).unwrap();
        std::fs::write(r.join("DCIM/100_FUJI/X.JPG"), b"\xFF\xD8\xFF\xD9").unwrap();
        r
    };

    let manifest = r#"
schema: camera-config/v1
camera: { manufacturer: FUJIFILM, model: GFX100 II, firmware: "2.30" }
connections:
  app:
    kind: ptpip-app
    initShape: app82
    liveViewDelivery: { kind: stream }
    commandFraming: compressed
    eventFraming: usb
    bindings: { command: 55740, event: 55741, liveView: 55742 }
operations:
  "0x1002": { name: OpenSession, connections: [app] }
properties:
  "0xdf01": { name: functionMode, type: u16, access: readWrite }
"#;
    let rt = tokio::runtime::Runtime::new().unwrap();
    let (command_addr, shutdown_tx, handle) = rt.block_on(async {
        let config = Config {
            instance_id: "test".into(),
            profile: "fuji/gfx100ii".into(),
            connection: "app".into(),
            manifest_yaml: manifest.into(),
            media_root: root.clone(),
            command_bind: Some("127.0.0.1:0".parse().unwrap()),
            liveview_bind: Some("127.0.0.1:0".parse().unwrap()),
            event_bind: Some("127.0.0.1:0".parse().unwrap()),
            knock_bind: None,
            pcss_init_fails: 0,
            pcss_shutter_enqueue_count: 0,
            control_bind: "127.0.0.1:0".parse().unwrap(),
            liveview_dir: None,
            state_callback: None,
        };
        let server = Server::bind(config).await.unwrap();
        let cmd = server.command_addr();
        let (tx, rx) = tokio::sync::oneshot::channel();
        let h = tokio::spawn(server.run(rx));
        (cmd, tx, h)
    });

    let mut s = TcpStream::connect(command_addr).unwrap();
    s.set_read_timeout(Some(std::time::Duration::from_secs(2)))
        .unwrap();
    write_frame(&mut s, &app_init_frame(9, "overflow"));
    match PtpIpPacket::decode(&read_frame(&mut s)).unwrap() {
        PtpIpPacket::InitCommandAck(_) => {}
        other => panic!("expected InitCommandAck, got {other:?}"),
    }
    write_frame(&mut s, &op(0x1002, 1, vec![1]));
    read_ok(&mut s);

    // SetDevicePropValue df01 with a malicious data-in: a single 2 MiB Data frame,
    // over the 1 MiB data-in cap. The server must reject it.
    write_frame(&mut s, &op(0x1016, 2, vec![0xdf01]));
    let oversized = fuji_framing::encode_data(0x1016, 2, &vec![0u8; 2 * 1024 * 1024]);
    let _ = s.write_all(&oversized);

    // The connection should now be closed by the server (EOF or RST). Reading
    // anything back must either return 0 bytes or fail — never an OK response.
    let mut buf = [0u8; 64];
    match s.read(&mut buf) {
        Ok(0) => {} // clean EOF
        Ok(_) => {
            // If anything came back, it must NOT be an OK OperationResponse for tid 2.
            panic!("server accepted oversized data-in (read returned bytes)");
        }
        Err(_) => {} // connection reset / timeout — also acceptable
    }

    rt.block_on(async {
        let _ = shutdown_tx.send(());
        let _ = handle.await;
    });
    std::fs::remove_dir_all(&root).ok();
}

/// Write the single-frame data-in for a SetDevicePropValue(0x1016) payload.
fn write_set_prop_data(s: &mut TcpStream, tid: u32, payload: &[u8]) {
    write_frame(s, &fuji_framing::encode_data(0x1016, tid, payload));
}

/// Wireless-tether live-view is command-channel polling: the AP through-picture
/// stream socket stays idle, while the manifest-selected poll op returns a JPEG
/// in a Fuji compressed data frame.
#[test]
fn service_serves_poll_liveview_on_command_socket() {
    let root = tmp_card();
    let lv_dir = root.join("liveview");
    std::fs::create_dir_all(&lv_dir).unwrap();
    let lv_jpeg = b"\xFF\xD8\xFF\xE0POLLFRAME\xFF\xD9";
    std::fs::write(lv_dir.join("frame_001.jpg"), lv_jpeg).unwrap();

    let manifest = r#"
schema: camera-config/v1
camera:
  manufacturer: FUJIFILM
  model: GFX100 II
  firmware: "2.30"
connections:
  wireless-tether:
    kind: ptpip-direct
    initShape: pcssKnock
    liveViewDelivery: { kind: poll, pollOp: "0x9018" }
    commandFraming: compressed
    bindings: { command: 15740 }
operations:
  "0x1002": { name: OpenSession, connections: [wireless-tether] }
  "0x9018": { name: PcssPollLiveViewData, connections: [wireless-tether] }
properties: {}
"#;

    let rt = tokio::runtime::Runtime::new().unwrap();
    let (command_addr, shutdown_tx, handle) = rt.block_on(async {
        let config = Config {
            instance_id: "test".into(),
            profile: "fuji/gfx100ii".into(),
            connection: "wireless-tether".into(),
            manifest_yaml: manifest.into(),
            media_root: root.clone(),
            command_bind: Some("127.0.0.1:0".parse().unwrap()),
            liveview_bind: None,
            event_bind: None,
            knock_bind: None,
            pcss_init_fails: 0,
            pcss_shutter_enqueue_count: 0,
            control_bind: "127.0.0.1:0".parse().unwrap(),
            liveview_dir: Some(lv_dir.clone()),
            state_callback: None,
        };
        let server = Server::bind(config).await.unwrap();
        let cmd = server.command_addr();
        assert!(server.liveview_addr_opt().is_none());
        assert!(server.event_addr_opt().is_none());
        let (tx, rx) = tokio::sync::oneshot::channel();
        let h = tokio::spawn(async move { server.run(rx).await });
        (cmd, tx, h)
    });

    let mut s = connect_pcss(command_addr, "mbp");
    write_frame(&mut s, &op(0x1002, 1, vec![1]));
    read_ok(&mut s);

    write_frame(&mut s, &op(0x9018, 2, vec![]));
    let frame = read_data_reply(&mut s);
    assert_eq!(&frame[..], lv_jpeg, "poll op returns the fixture JPEG");

    drop(s);
    rt.block_on(async {
        let _ = shutdown_tx.send(());
        let _ = handle.await;
    });
    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn pcss_init_fail_retries_then_accepts() {
    let root = tmp_card();
    let manifest = r#"
schema: camera-config/v1
camera: { manufacturer: FUJIFILM, model: GFX100 II, firmware: "2.30" }
connections:
  wireless-tether:
    kind: ptpip-direct
    initShape: pcssKnock
    commandFraming: compressed
    bindings: { command: 15740 }
    initRetries: { max: 3, backoffMs: 500, whenReasons: ["0x2019"] }
operations:
  "0x1002": { name: OpenSession, connections: [wireless-tether] }
properties: {}
"#;

    let rt = tokio::runtime::Runtime::new().unwrap();
    let (command_addr, control_addr, shutdown_tx, handle) = rt.block_on(async {
        let config = Config {
            instance_id: "test".into(),
            profile: "fuji/gfx100ii".into(),
            connection: "wireless-tether".into(),
            manifest_yaml: manifest.into(),
            media_root: root.clone(),
            command_bind: Some("127.0.0.1:0".parse().unwrap()),
            liveview_bind: None,
            event_bind: None,
            knock_bind: None,
            pcss_init_fails: 1,
            pcss_shutter_enqueue_count: 0,
            control_bind: "127.0.0.1:0".parse().unwrap(),
            liveview_dir: None,
            state_callback: None,
        };
        let server = Server::bind(config).await.unwrap();
        let cmd = server.command_addr();
        let control = server.control_addr();
        let (tx, rx) = tokio::sync::oneshot::channel();
        let h = tokio::spawn(async move { server.run(rx).await });
        (cmd, control, tx, h)
    });

    let init = pcss_init_frame("mbp");
    let mut s = TcpStream::connect(command_addr).unwrap();
    write_frame(&mut s, &init);
    match PtpIpPacket::decode(&read_frame(&mut s)).unwrap() {
        PtpIpPacket::InitFail(f) => assert_eq!(f.reason, 0x2019),
        other => panic!("expected InitFail, got {other:?}"),
    }
    write_frame(&mut s, &init);
    let ack = read_frame(&mut s);
    assert_eq!(ack.len(), 68);
    assert_eq!(
        parse_pcss_init_ack(&ack)
            .expect("fixed PCSS InitCommandAck after retries")
            .connection_number,
        0
    );
    write_frame(&mut s, &op(0x1002, 1, vec![1]));
    read_ok(&mut s);

    let trace = trace_json(control_addr);
    let events = trace["events"].as_array().unwrap();
    let requests = events
        .iter()
        .filter(|event| event["kind"] == "ptpip.init_request.received")
        .collect::<Vec<_>>();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0]["payload_hex"], requests[1]["payload_hex"]);
    assert!(events.iter().any(|event| {
        event["kind"] == "ptpip.init_fail.sent"
            && event["payload_hex"] == "0c0000000500000019200000"
    }));
    assert!(events.iter().any(|event| {
        event["kind"] == "ptpip.init_ack.sent" && event["outcome"] == "connection_number=0"
    }));
    assert!(events.iter().any(|event| {
        event["kind"] == "ptpip.first_operation.received"
            && event["payload_hex"] == "10000000010002100100000001000000"
    }));

    drop(s);
    rt.block_on(async {
        let _ = shutdown_tx.send(());
        let _ = handle.await;
    });
    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn pcss_startup_queue_downloads_and_delete_drains() {
    let root = tmp_card();
    let rt = tokio::runtime::Runtime::new().unwrap();
    let (command_addr, control_addr, shutdown_tx, handle) = rt.block_on(async {
        let config = Config {
            instance_id: "test".into(),
            profile: "fuji/gfx100ii/fw0230".into(),
            connection: "wireless-tether".into(),
            manifest_yaml: real_gfx_manifest(),
            media_root: root.clone(),
            command_bind: Some("127.0.0.1:0".parse().unwrap()),
            liveview_bind: None,
            event_bind: None,
            knock_bind: None,
            pcss_init_fails: 0,
            pcss_shutter_enqueue_count: 0,
            control_bind: "127.0.0.1:0".parse().unwrap(),
            liveview_dir: None,
            state_callback: None,
        };
        let server = Server::bind(config).await.unwrap();
        let cmd = server.command_addr();
        let control = server.control_addr();
        let (tx, rx) = tokio::sync::oneshot::channel();
        let h = tokio::spawn(server.run(rx));
        (cmd, control, tx, h)
    });

    let state = http_get(control_addr, "/state");
    let state: serde_json::Value =
        serde_json::from_str(state.split_once("\r\n\r\n").unwrap().1).unwrap();
    assert_eq!(state["transfer_queues"]["standard"]["queued"], 1);
    assert_eq!(state["transfer_queues"]["standard"]["completed"], 0);

    let mut s = connect_pcss(command_addr, "mbp");
    open_session(&mut s);

    let handles = read_handles(&mut s, 2);
    assert_eq!(handles.len(), 1);
    let handle_id = handles[0];
    assert_eq!(read_d620_count(&mut s, 3), 1);
    assert_eq!(read_d621_handles(&mut s, 4), handles);

    write_frame(&mut s, &op(0x1008, 5, vec![handle_id]));
    let info = read_data_reply(&mut s);
    let oi = ptp_core::ObjectInfo::decode(&info).unwrap();
    assert_eq!(oi.object_format, 0x3801);

    write_frame(&mut s, &op(0x100a, 6, vec![handle_id]));
    let thumb = read_data_reply(&mut s);
    assert!(thumb.starts_with(b"\xFF\xD8"));

    write_frame(&mut s, &op(0x1009, 7, vec![handle_id]));
    let object = read_data_reply(&mut s);
    assert_eq!(&object, b"\xFF\xD8HELLOJPEG\xFF\xD9");

    write_frame(&mut s, &op(0x100b, 8, vec![handle_id]));
    read_ok(&mut s);
    assert!(read_handles(&mut s, 9).is_empty());
    assert_eq!(read_d620_count(&mut s, 10), 0);
    assert!(read_d621_handles(&mut s, 11).is_empty());
    let state = http_get(control_addr, "/state");
    let state: serde_json::Value =
        serde_json::from_str(state.split_once("\r\n\r\n").unwrap().1).unwrap();
    assert_eq!(state["transfer_queues"]["standard"]["queued"], 0);
    assert_eq!(state["transfer_queues"]["standard"]["completed"], 1);

    write_frame(&mut s, &op(0x1008, 12, vec![handle_id]));
    assert_eq!(read_response_code(&mut s), 0x2009);

    write_frame(&mut s, &op(0x101b, 13, vec![handle_id, 0, 1, 0]));
    assert_eq!(
        read_response_code(&mut s),
        0x2005,
        "PCSS does not support GetPartialObject"
    );

    rt.block_on(async {
        let _ = shutdown_tx.send(());
        let _ = handle.await;
    });
    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn pcss_startup_queue_excludes_movies_until_pcss_mov_transfer_is_captured() {
    let root = tmp_card_with_movie();
    let rt = tokio::runtime::Runtime::new().unwrap();
    let (command_addr, shutdown_tx, handle) = rt.block_on(async {
        let config = Config {
            instance_id: "test".into(),
            profile: "fuji/gfx100ii/fw0230".into(),
            connection: "wireless-tether".into(),
            manifest_yaml: real_gfx_manifest(),
            media_root: root.clone(),
            command_bind: Some("127.0.0.1:0".parse().unwrap()),
            liveview_bind: None,
            event_bind: None,
            knock_bind: None,
            pcss_init_fails: 0,
            pcss_shutter_enqueue_count: 0,
            control_bind: "127.0.0.1:0".parse().unwrap(),
            liveview_dir: None,
            state_callback: None,
        };
        let server = Server::bind(config).await.unwrap();
        let cmd = server.command_addr();
        let (tx, rx) = tokio::sync::oneshot::channel();
        let h = tokio::spawn(server.run(rx));
        (cmd, tx, h)
    });

    let mut s = connect_pcss(command_addr, "mbp");
    open_session(&mut s);

    let handles = read_handles(&mut s, 2);
    assert_eq!(handles.len(), 1);
    assert_eq!(read_d620_count(&mut s, 3), 1);
    assert_eq!(read_d621_handles(&mut s, 4), handles);

    write_frame(&mut s, &op(0x1008, 5, vec![handles[0]]));
    let info = read_data_reply(&mut s);
    let oi = ptp_core::ObjectInfo::decode(&info).unwrap();
    assert_eq!(oi.object_format, 0x3801);

    rt.block_on(async {
        let _ = shutdown_tx.send(());
        let _ = handle.await;
    });
    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn pcss_shutter_fed_queue_enqueues_next_media_handles() {
    let root = tmp_card_with_jpegs(3);
    let rt = tokio::runtime::Runtime::new().unwrap();
    let (command_addr, shutdown_tx, handle) = rt.block_on(async {
        let config = Config {
            instance_id: "test".into(),
            profile: "fuji/gfx100ii/fw0230".into(),
            connection: "wireless-tether".into(),
            manifest_yaml: real_gfx_manifest(),
            media_root: root.clone(),
            command_bind: Some("127.0.0.1:0".parse().unwrap()),
            liveview_bind: None,
            event_bind: None,
            knock_bind: None,
            pcss_init_fails: 0,
            pcss_shutter_enqueue_count: 2,
            control_bind: "127.0.0.1:0".parse().unwrap(),
            liveview_dir: None,
            state_callback: None,
        };
        let server = Server::bind(config).await.unwrap();
        let cmd = server.command_addr();
        let (tx, rx) = tokio::sync::oneshot::channel();
        let h = tokio::spawn(server.run(rx));
        (cmd, tx, h)
    });

    let mut s = connect_pcss(command_addr, "mbp");
    open_session(&mut s);
    assert!(read_handles(&mut s, 2).is_empty());

    pcss_shutter(&mut s, 10);
    let first_batch = read_handles(&mut s, 20);
    assert_eq!(first_batch.len(), 2);
    for handle_id in &first_batch {
        write_frame(&mut s, &op(0x100b, 30 + *handle_id, vec![*handle_id]));
        read_ok(&mut s);
    }
    assert!(read_handles(&mut s, 40).is_empty());

    pcss_shutter(&mut s, 50);
    let second_batch = read_handles(&mut s, 60);
    assert_eq!(second_batch.len(), 1);
    assert!(
        !first_batch.contains(&second_batch[0]),
        "queue must not reuse drained handles"
    );

    rt.block_on(async {
        let _ = shutdown_tx.send(());
        let _ = handle.await;
    });
    std::fs::remove_dir_all(&root).ok();
}

#[tokio::test]
async fn bind_rejects_pcss_shutter_enqueue_count_above_manifest_max() {
    let root = tmp_card();
    let config = Config {
        instance_id: "test".into(),
        profile: "fuji/gfx100ii/fw0230".into(),
        connection: "wireless-tether".into(),
        manifest_yaml: real_gfx_manifest(),
        media_root: root.clone(),
        command_bind: Some("127.0.0.1:0".parse().unwrap()),
        liveview_bind: None,
        event_bind: None,
        knock_bind: None,
        pcss_init_fails: 0,
        pcss_shutter_enqueue_count: 4,
        control_bind: "127.0.0.1:0".parse().unwrap(),
        liveview_dir: None,
        state_callback: None,
    };
    let err = match Server::bind(config).await {
        Ok(_) => panic!("expected bind to reject excessive PCSS queue count"),
        Err(err) => err,
    };
    assert!(
        err.to_string().contains("objectsAvailable max 3"),
        "unexpected error: {err}"
    );
    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn pcss_rejects_app_init_shape() {
    let root = tmp_card();
    let manifest = r#"
schema: camera-config/v1
camera: { manufacturer: FUJIFILM, model: GFX100 II, firmware: "2.30" }
connections:
  wireless-tether:
    kind: ptpip-direct
    initShape: pcssKnock
    commandFraming: compressed
    bindings: { command: 15740 }
operations: {}
properties: {}
"#;

    let rt = tokio::runtime::Runtime::new().unwrap();
    let (command_addr, shutdown_tx, handle) = rt.block_on(async {
        let config = Config {
            instance_id: "test".into(),
            profile: "fuji/gfx100ii".into(),
            connection: "wireless-tether".into(),
            manifest_yaml: manifest.into(),
            media_root: root.clone(),
            command_bind: Some("127.0.0.1:0".parse().unwrap()),
            liveview_bind: None,
            event_bind: None,
            knock_bind: None,
            pcss_init_fails: 0,
            pcss_shutter_enqueue_count: 0,
            control_bind: "127.0.0.1:0".parse().unwrap(),
            liveview_dir: None,
            state_callback: None,
        };
        let server = Server::bind(config).await.unwrap();
        let cmd = server.command_addr();
        let (tx, rx) = tokio::sync::oneshot::channel();
        let h = tokio::spawn(async move { server.run(rx).await });
        (cmd, tx, h)
    });

    let mut s = TcpStream::connect(command_addr).unwrap();
    s.set_read_timeout(Some(std::time::Duration::from_millis(500)))
        .unwrap();
    // Use a name beyond PCSS's shorter field so the two fixed layouts are
    // distinguishable on the wire.
    write_frame(&mut s, &app_init_frame(1, "app-name-is-long"));
    let mut buf = [0u8; 4];
    if s.read_exact(&mut buf).is_ok() {
        panic!("PCSS path accepted an reference app init packet");
    }

    rt.block_on(async {
        let _ = shutdown_tx.send(());
        let _ = handle.await;
    });
    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn pcss_knock_notifies_callback_with_command_port() {
    let root = tmp_card();
    let callback = TcpListener::bind("127.0.0.1:0").unwrap();
    let callback_port = callback.local_addr().unwrap().port();
    let manifest = format!(
        r#"
schema: camera-config/v1
camera: {{ manufacturer: FUJIFILM, model: GFX100 II, firmware: "2.30" }}
connections:
  wireless-tether:
    kind: ptpip-direct
    initShape: pcssKnock
    commandFraming: compressed
    bindings: {{ command: 15740 }}
    knock:
      callbackPort: {callback_port}
      knockPort: 51562
      protocol: "PCSS/1.0"
      discoveryTargets:
        default: subnetBroadcast
        supported: [subnetBroadcast, explicitUnicast]
        retryDiscoveredUnicast: true
operations:
  "0x1002": {{ name: OpenSession, connections: [wireless-tether] }}
properties: {{}}
"#
    );

    let rt = tokio::runtime::Runtime::new().unwrap();
    let (command_addr, knock_addr, control_addr, shutdown_tx, handle) = rt.block_on(async {
        let config = Config {
            instance_id: "test".into(),
            profile: "fuji/gfx100ii".into(),
            connection: "wireless-tether".into(),
            manifest_yaml: manifest,
            media_root: root.clone(),
            command_bind: Some("127.0.0.1:0".parse().unwrap()),
            liveview_bind: None,
            event_bind: None,
            knock_bind: Some("127.0.0.1:0".parse().unwrap()),
            pcss_init_fails: 0,
            pcss_shutter_enqueue_count: 0,
            control_bind: "127.0.0.1:0".parse().unwrap(),
            liveview_dir: None,
            state_callback: None,
        };
        let server = Server::bind(config).await.unwrap();
        let cmd = server.command_addr();
        let knock = server.knock_addr_opt().expect("knock listener bound");
        let control = server.control_addr();
        let (tx, rx) = tokio::sync::oneshot::channel();
        let h = tokio::spawn(async move { server.run(rx).await });
        (cmd, knock, control, tx, h)
    });

    let udp = UdpSocket::bind("127.0.0.1:0").unwrap();
    udp.send_to(
        b"DISCOVERY * HTTP/1.1\r\nHOST: 127.0.0.1\r\nMX: 5\r\nSERVICE: PCSS/1.0\r\n\0",
        knock_addr,
    )
    .unwrap();
    callback.set_nonblocking(true).unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    let (mut callback_stream, _) = loop {
        match callback.accept() {
            Ok(accepted) => break accepted,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                assert!(
                    std::time::Instant::now() < deadline,
                    "timed out waiting for PCSS NOTIFY callback"
                );
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            Err(e) => panic!("callback accept failed: {e}"),
        }
    };
    callback_stream
        .set_read_timeout(Some(std::time::Duration::from_secs(2)))
        .unwrap();
    let mut notify = Vec::new();
    let mut buf = [0u8; 256];
    let n = callback_stream.read(&mut buf).unwrap();
    notify.extend_from_slice(&buf[..n]);
    callback_stream.write_all(b"HTTP/1.1 200 OK\r\n\0").unwrap();
    let notify = String::from_utf8_lossy(&notify);
    assert!(notify.starts_with("NOTIFY * HTTP/1.1\r\n"), "{notify}");
    assert!(notify.contains("DSC: 127.0.0.1\r\n"), "{notify}");
    assert!(notify.contains("CAMERANAME: GFX100 II\r\n"), "{notify}");
    assert!(notify.contains("SERVICE: PCSS/1.0\r\n"), "{notify}");
    assert!(
        notify.contains(&format!("DSCPORT: {}\r\n", command_addr.port())),
        "{notify}"
    );

    let mut s = connect_pcss(command_addr, "mbp");
    write_frame(&mut s, &op(0x1002, 1, vec![1]));
    read_ok(&mut s);

    let trace = trace_json(control_addr);
    let events = trace["events"].as_array().unwrap();
    let expected_discovery =
        b"DISCOVERY * HTTP/1.1\r\nHOST: 127.0.0.1\r\nMX: 5\r\nSERVICE: PCSS/1.0\r\n\0"
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
    assert!(events.iter().any(|event| {
        event["kind"] == "pcss.discovery.received" && event["payload_hex"] == expected_discovery
    }));
    assert!(events
        .iter()
        .any(|event| event["kind"] == "pcss.callback.connect_started"));
    assert!(events
        .iter()
        .any(|event| event["kind"] == "pcss.callback.connected"));
    assert!(events
        .iter()
        .any(|event| event["kind"] == "pcss.notify.sent"));
    assert!(events.iter().any(|event| {
        event["kind"] == "pcss.callback_ack.received"
            && event["payload_hex"] == "485454502f312e3120323030204f4b0d0a00"
    }));

    drop(s);
    rt.block_on(async {
        let _ = shutdown_tx.send(());
        let _ = handle.await;
    });
    std::fs::remove_dir_all(&root).ok();
}

fn assert_pcss_discovery_host_rejected(host: &str) {
    let root = tmp_card();
    let callback = TcpListener::bind("127.0.0.1:0").unwrap();
    let callback_port = callback.local_addr().unwrap().port();
    let manifest = format!(
        r#"
schema: camera-config/v1
camera: {{ manufacturer: FUJIFILM, model: GFX100 II, firmware: "2.30" }}
connections:
  wireless-tether:
    kind: ptpip-direct
    initShape: pcssKnock
    commandFraming: compressed
    bindings: {{ command: 15740 }}
    knock:
      callbackPort: {callback_port}
      knockPort: 51562
      protocol: "PCSS/1.0"
      discoveryTargets:
        default: subnetBroadcast
        supported: [subnetBroadcast, explicitUnicast]
        retryDiscoveredUnicast: true
operations:
  "0x1002": {{ name: OpenSession, connections: [wireless-tether] }}
properties: {{}}
"#
    );

    let rt = tokio::runtime::Runtime::new().unwrap();
    let (knock_addr, control_addr, shutdown_tx, handle) = rt.block_on(async {
        let config = Config {
            instance_id: "test".into(),
            profile: "fuji/gfx100ii".into(),
            connection: "wireless-tether".into(),
            manifest_yaml: manifest,
            media_root: root.clone(),
            command_bind: Some("127.0.0.1:0".parse().unwrap()),
            liveview_bind: None,
            event_bind: None,
            knock_bind: Some("127.0.0.1:0".parse().unwrap()),
            pcss_init_fails: 0,
            pcss_shutter_enqueue_count: 0,
            control_bind: "127.0.0.1:0".parse().unwrap(),
            liveview_dir: None,
            state_callback: None,
        };
        let server = Server::bind(config).await.unwrap();
        let knock = server.knock_addr_opt().expect("knock listener bound");
        let control = server.control_addr();
        let (tx, rx) = tokio::sync::oneshot::channel();
        let h = tokio::spawn(async move { server.run(rx).await });
        (knock, control, tx, h)
    });

    let udp = UdpSocket::bind("127.0.0.1:0").unwrap();
    udp.send_to(
        format!("DISCOVERY * HTTP/1.1\r\nHOST: {host}\r\nMX: 5\r\nSERVICE: PCSS/1.0\r\n\0")
            .as_bytes(),
        knock_addr,
    )
    .unwrap();
    callback.set_nonblocking(true).unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    loop {
        match callback.accept() {
            Ok(_) => panic!("rejected PCSS discovery opened a callback connection"),
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                if std::time::Instant::now() >= deadline {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            Err(e) => panic!("callback accept failed: {e}"),
        }
    }

    let trace = trace_json(control_addr);
    let events = trace["events"].as_array().unwrap();
    assert!(events.iter().any(|event| {
        event["kind"] == "pcss.discovery.rejected"
            && event["outcome"] == "rejected"
            && event["error"] == "HOST does not match the datagram source address"
    }));

    rt.block_on(async {
        let _ = shutdown_tx.send(());
        let _ = handle.await;
    });
    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn pcss_knock_rejects_mismatched_discovery_host() {
    assert_pcss_discovery_host_rejected("203.0.113.7");
}

#[test]
fn pcss_knock_rejects_non_ip_discovery_host() {
    assert_pcss_discovery_host_rejected("attacker.example");
}

#[test]
fn app_persona_does_not_serve_wireless_tether_poll_liveview() {
    let root = tmp_card();
    let lv_dir = root.join("liveview");
    std::fs::create_dir_all(&lv_dir).unwrap();
    std::fs::write(lv_dir.join("frame_001.jpg"), b"\xFF\xD8PCSS\xFF\xD9").unwrap();

    let manifest = r#"
schema: camera-config/v1
camera: { manufacturer: FUJIFILM, model: GFX100 II, firmware: "2.30" }
connections:
  app:
    kind: ptpip-app
    initShape: app82
    liveViewDelivery: { kind: stream }
    commandFraming: compressed
    eventFraming: usb
    bindings: { command: 55740, event: 55741, liveView: 55742 }
  wireless-tether:
    kind: ptpip-direct
    initShape: app82
    liveViewDelivery: { kind: poll, pollOp: "0x9018" }
    commandFraming: compressed
    bindings: { command: 15740 }
operations:
  "0x1002": { name: OpenSession, connections: [app, wireless-tether] }
  "0x9018": { name: PcssPollLiveViewData, connections: [wireless-tether] }
properties: {}
"#;

    let rt = tokio::runtime::Runtime::new().unwrap();
    let (command_addr, shutdown_tx, handle) = rt.block_on(async {
        let config = Config {
            instance_id: "test".into(),
            profile: "fuji/gfx100ii".into(),
            connection: "app".into(),
            manifest_yaml: manifest.into(),
            media_root: root.clone(),
            command_bind: Some("127.0.0.1:0".parse().unwrap()),
            liveview_bind: Some("127.0.0.1:0".parse().unwrap()),
            event_bind: Some("127.0.0.1:0".parse().unwrap()),
            knock_bind: None,
            pcss_init_fails: 0,
            pcss_shutter_enqueue_count: 0,
            control_bind: "127.0.0.1:0".parse().unwrap(),
            liveview_dir: Some(lv_dir.clone()),
            state_callback: None,
        };
        let server = Server::bind(config).await.unwrap();
        let cmd = server.command_addr();
        let (tx, rx) = tokio::sync::oneshot::channel();
        let h = tokio::spawn(async move { server.run(rx).await });
        (cmd, tx, h)
    });

    let mut s = TcpStream::connect(command_addr).unwrap();
    write_frame(&mut s, &app_init_frame(0, "test"));
    match PtpIpPacket::decode(&read_frame(&mut s)).unwrap() {
        PtpIpPacket::InitCommandAck(_) => {}
        other => panic!("expected InitCommandAck, got {other:?}"),
    }
    write_frame(&mut s, &op(0x1002, 1, vec![1]));
    read_ok(&mut s);

    write_frame(&mut s, &op(0x9018, 2, vec![]));
    match fuji_framing::decode(&read_frame(&mut s)).unwrap() {
        PtpIpPacket::OperationResponse(r) => assert_eq!(r.code, 0x2005),
        other => panic!("expected unsupported response, got {other:?}"),
    }

    drop(s);
    rt.block_on(async {
        let _ = shutdown_tx.send(());
        let _ = handle.await;
    });
    std::fs::remove_dir_all(&root).ok();
}

#[tokio::test]
async fn bind_rejects_absent_role_override_for_selected_connection() {
    let root = tmp_card();
    let manifest = r#"
schema: camera-config/v1
camera: { manufacturer: FUJIFILM, model: GFX100 II, firmware: "2.30" }
connections:
  wireless-tether:
    kind: ptpip-direct
    initShape: app82
    commandFraming: compressed
    bindings: { command: 15740 }
operations: {}
properties: {}
"#;
    let config = Config {
        instance_id: "test".into(),
        profile: "fuji/gfx100ii".into(),
        connection: "wireless-tether".into(),
        manifest_yaml: manifest.into(),
        media_root: root.clone(),
        command_bind: Some("127.0.0.1:0".parse().unwrap()),
        liveview_bind: None,
        event_bind: Some("127.0.0.1:0".parse().unwrap()),
        knock_bind: None,
        pcss_init_fails: 0,
        pcss_shutter_enqueue_count: 0,
        control_bind: "127.0.0.1:0".parse().unwrap(),
        liveview_dir: None,
        state_callback: None,
    };
    let err = match Server::bind(config).await {
        Err(err) => err,
        Ok(_) => panic!("event override must fail when the selected connection has no event role"),
    };
    assert!(
        err.to_string().contains("no event socket"),
        "error should name the absent role: {err}"
    );
    let _ = std::fs::remove_dir_all(root);
}

/// A mode entry that opens auxiliary channels before its declared operation
/// observes a real TCP refusal at the same step.
#[test]
fn service_refuses_aux_channels_before_declared_operation() {
    let root = tmp_card();
    let manifest = r#"
schema: camera-config/v1
camera:
  manufacturer: FUJIFILM
  model: GFX100 II
  firmware: "2.30"
connections:
  app:
    kind: ptpip-app
    initShape: app82
    liveViewDelivery: { kind: stream }
    commandFraming: compressed
    eventFraming: usb
    bindings:
      command: 55740
      event: { port: 55741, availableAfter: { operation: "0x101c" } }
      liveView: { port: 55742, availableAfter: { operation: "0x101c" } }
    entries:
      - to: shooting/stills
        steps:
          - { setProp: "0xdf01", value: 22 }
          - { openChannel: event }
          - { openChannel: liveView }
          - { sendOp: "0x101c" }
modes:
  shooting/stills:
    detect: { prop: "0xdf01", eq: 22 }
    phase: liveView
operations:
  "0x1002": { name: OpenSession, connections: [app] }
  "0x1003": { name: CloseSession, connections: [app] }
  "0x101c": { name: InitiateOpenCapture, connections: [app] }
properties:
  "0xdf01": { name: functionMode, type: u16, access: readWrite }
"#;

    let rt = tokio::runtime::Runtime::new().unwrap();
    let (command_addr, event_addr, liveview_addr, shutdown_tx, handle) = rt.block_on(async {
        let server = Server::bind(Config {
            instance_id: "test".into(),
            profile: "fuji/gfx100ii".into(),
            connection: "app".into(),
            manifest_yaml: manifest.into(),
            media_root: root.clone(),
            command_bind: Some("127.0.0.1:0".parse().unwrap()),
            liveview_bind: Some("127.0.0.1:0".parse().unwrap()),
            event_bind: Some("127.0.0.1:0".parse().unwrap()),
            knock_bind: None,
            pcss_init_fails: 0,
            pcss_shutter_enqueue_count: 0,
            control_bind: "127.0.0.1:0".parse().unwrap(),
            liveview_dir: None,
            state_callback: None,
        })
        .await
        .unwrap();
        let command = server.command_addr();
        let event = server.event_addr_opt().unwrap();
        let liveview = server.liveview_addr_opt().unwrap();
        let (tx, rx) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(async move { server.run(rx).await });
        (command, event, liveview, tx, task)
    });

    let mut command = TcpStream::connect(command_addr).unwrap();
    write_frame(&mut command, &app_init_frame(0, "test"));
    assert!(matches!(
        PtpIpPacket::decode(&read_frame(&mut command)).unwrap(),
        PtpIpPacket::InitCommandAck(_)
    ));
    write_frame(&mut command, &op(0x1002, 1, vec![1]));
    read_ok(&mut command);
    write_frame(&mut command, &op(0x1016, 2, vec![0xdf01]));
    write_set_prop_data(&mut command, 2, &22u16.to_le_bytes());
    read_ok(&mut command);

    assert!(
        TcpStream::connect(event_addr).is_err(),
        "misordered event openChannel must observe a TCP refusal"
    );
    assert!(
        TcpStream::connect(liveview_addr).is_err(),
        "misordered live-view openChannel must observe a TCP refusal"
    );

    write_frame(&mut command, &op(0x101c, 3, vec![]));
    read_ok(&mut command);
    assert!(TcpStream::connect(event_addr).is_ok());
    assert!(TcpStream::connect(liveview_addr).is_ok());

    write_frame(&mut command, &op(0x1003, 4, vec![]));
    read_ok(&mut command);
    assert!(
        TcpStream::connect(event_addr).is_err(),
        "session close must restore event-port refusal"
    );
    assert!(
        TcpStream::connect(liveview_addr).is_err(),
        "session close must restore live-view-port refusal"
    );

    write_frame(&mut command, &op(0x1002, 5, vec![1]));
    read_ok(&mut command);
    assert!(
        TcpStream::connect(event_addr).is_err(),
        "reopened session must keep the event port refused before the gate operation"
    );
    assert!(
        TcpStream::connect(liveview_addr).is_err(),
        "reopened session must keep the live-view port refused before the gate operation"
    );

    write_frame(&mut command, &op(0x1016, 6, vec![0xdf01]));
    write_set_prop_data(&mut command, 6, &22u16.to_le_bytes());
    read_ok(&mut command);
    assert!(TcpStream::connect(event_addr).is_err());
    assert!(TcpStream::connect(liveview_addr).is_err());

    write_frame(&mut command, &op(0x101c, 7, vec![]));
    read_ok(&mut command);
    assert!(TcpStream::connect(event_addr).is_ok());
    assert!(TcpStream::connect(liveview_addr).is_ok());

    drop(command);
    rt.block_on(async {
        let _ = shutdown_tx.send(());
        let _ = handle.await;
    });
    std::fs::remove_dir_all(&root).ok();
}

/// Live-view smoke: the declared operation controls TCP availability, then
/// frames flow once the initiator reaches Phase::Streaming.
#[test]
fn service_streams_liveview_after_open_capture() {
    let root = tmp_card();
    let lv_dir = root.join("liveview");
    std::fs::create_dir_all(&lv_dir).unwrap();
    let lv_jpeg = b"\xFF\xD8\xFF\xE0FRAME\xFF\xD9";
    std::fs::write(lv_dir.join("frame_001.jpg"), lv_jpeg).unwrap();

    let manifest = r#"
schema: camera-config/v1
camera:
  manufacturer: FUJIFILM
  model: GFX100 II
  firmware: "2.30"
connections:
  app:
    kind: ptpip-app
    initShape: app82
    liveViewDelivery: { kind: stream }
    commandFraming: compressed
    eventFraming: usb
    bindings:
      command: 55740
      event: { port: 55741, availableAfter: { operation: "0x101c" } }
      liveView: { port: 55742, availableAfter: { operation: "0x101c" } }
    entries:
      - to: shooting/stills
        steps:
          - { setProp: "0xdf01", value: 22 }
          - { sendOp: "0x101c" }
          - { openChannel: event }
          - { openChannel: liveView }
modes:
  shooting/stills:
    detect: { prop: "0xdf01", eq: 22 }
    phase: liveView
operations:
  "0x1002": { name: OpenSession, connections: [app] }
  "0x101c": { name: InitiateOpenCapture, connections: [app] }
properties:
  "0xdf01": { name: functionMode, type: u16, access: readWrite }
"#;

    let rt = tokio::runtime::Runtime::new().unwrap();
    let (command_addr, event_addr, liveview_addr, shutdown_tx, handle) = rt.block_on(async {
        let config = Config {
            instance_id: "test".into(),
            profile: "fuji/gfx100ii".into(),
            connection: "app".into(),
            manifest_yaml: manifest.into(),
            media_root: root.clone(),
            command_bind: Some("127.0.0.1:0".parse().unwrap()),
            liveview_bind: Some("127.0.0.1:0".parse().unwrap()),
            event_bind: Some("127.0.0.1:0".parse().unwrap()),
            knock_bind: None,
            pcss_init_fails: 0,
            pcss_shutter_enqueue_count: 0,
            control_bind: "127.0.0.1:0".parse().unwrap(),
            liveview_dir: Some(lv_dir.clone()),
            state_callback: None,
        };
        let server = Server::bind(config).await.unwrap();
        let cmd = server.command_addr();
        let lv = server
            .liveview_addr_opt()
            .expect("app connection has live-view socket");
        let event = server
            .event_addr_opt()
            .expect("app connection has event socket");
        let (tx, rx) = tokio::sync::oneshot::channel();
        let h = tokio::spawn(async move { server.run(rx).await });
        (cmd, event, lv, tx, h)
    });

    assert!(TcpStream::connect(liveview_addr).is_err());
    assert!(TcpStream::connect(event_addr).is_err());

    // Drive the command channel into Phase::Streaming.
    let mut s = TcpStream::connect(command_addr).unwrap();
    write_frame(&mut s, &app_init_frame(0, "test"));
    match PtpIpPacket::decode(&read_frame(&mut s)).unwrap() {
        PtpIpPacket::InitCommandAck(_) => {}
        other => panic!("expected InitCommandAck, got {other:?}"),
    }
    write_frame(&mut s, &op(0x1002, 1, vec![1]));
    read_ok(&mut s);

    // SetDevicePropValue df01=22 (u16 LE) -> Phase::LiveView
    write_frame(&mut s, &op(0x1016, 2, vec![0xdf01]));
    write_set_prop_data(&mut s, 2, &22u16.to_le_bytes());
    read_ok(&mut s);

    // InitiateOpenCapture -> Phase::Streaming
    write_frame(&mut s, &op(0x101c, 3, vec![]));
    read_ok(&mut s);

    let mut lv = TcpStream::connect(liveview_addr).unwrap();
    let event = TcpStream::connect(event_addr).unwrap();

    // Now frames flow: read two and confirm they're the fixture (single-frame loop).
    lv.set_read_timeout(Some(std::time::Duration::from_millis(500)))
        .unwrap();
    let frame0 = read_frame_lv(&mut lv);
    let frame1 = read_frame_lv(&mut lv);
    assert_eq!(frame0.total_len as usize, 18 + lv_jpeg.len());
    assert_eq!(frame0.reserved0, 0);
    assert_eq!(frame0.jpeg_body_offset_adjust, 0);
    assert_eq!(frame0.reserved_pad, 0);
    assert_eq!(frame0.frame_counter, 0);
    assert_eq!(frame1.frame_counter, 1);
    assert_eq!(
        &frame0.jpeg[..],
        lv_jpeg,
        "first frame matches the fixture JPEG"
    );
    assert_eq!(frame0.jpeg, frame1.jpeg, "single-frame loop repeats");

    drop(lv);
    drop(event);
    let mut reopened = TcpStream::connect(liveview_addr).unwrap();
    reopened
        .set_read_timeout(Some(std::time::Duration::from_millis(500)))
        .unwrap();
    let reopened_frame = read_frame_lv(&mut reopened);
    assert_eq!(
        reopened_frame.frame_counter, 0,
        "a new stream resets the counter"
    );
    drop(reopened);
    drop(s);
    rt.block_on(async {
        let _ = shutdown_tx.send(());
        let _ = handle.await;
    });
    std::fs::remove_dir_all(&root).ok();
}

/// #26(1): a manifest authored against a schema this build doesn't support
/// must fail at bind, not misbehave at request time.
#[tokio::test]
async fn bind_rejects_unsupported_manifest_schema() {
    let root = tmp_card();
    let config = Config {
        instance_id: "test".into(),
        profile: "fuji/gfx100ii".into(),
        connection: "app".into(),
        manifest_yaml: MANIFEST.replace("camera-config/v1", "camera-config/v999"),
        media_root: root.clone(),
        command_bind: Some("127.0.0.1:0".parse().unwrap()),
        liveview_bind: Some("127.0.0.1:0".parse().unwrap()),
        event_bind: Some("127.0.0.1:0".parse().unwrap()),
        knock_bind: None,
        pcss_init_fails: 0,
        pcss_shutter_enqueue_count: 0,
        control_bind: "127.0.0.1:0".parse().unwrap(),
        liveview_dir: None,
        state_callback: None,
    };
    let err = match Server::bind(config).await {
        Err(e) => e,
        Ok(_) => panic!("stale schema must not boot"),
    };
    assert!(
        err.to_string().contains("camera-config/v999"),
        "error names the offending schema: {err}"
    );
    let _ = std::fs::remove_dir_all(root);
}

/// #26(2): a control client that connects and stalls before sending its
/// request line must not block the accept loop — a second client's /healthz
/// completes while the first sits idle.
#[tokio::test]
async fn idle_control_connection_does_not_block_healthz() {
    let root = tmp_card();
    let config = Config {
        instance_id: "test".into(),
        profile: "fuji/gfx100ii".into(),
        connection: "app".into(),
        manifest_yaml: MANIFEST.into(),
        media_root: root.clone(),
        command_bind: Some("127.0.0.1:0".parse().unwrap()),
        liveview_bind: Some("127.0.0.1:0".parse().unwrap()),
        event_bind: Some("127.0.0.1:0".parse().unwrap()),
        knock_bind: None,
        pcss_init_fails: 0,
        pcss_shutter_enqueue_count: 0,
        control_bind: "127.0.0.1:0".parse().unwrap(),
        liveview_dir: None,
        state_callback: None,
    };
    let server = Server::bind(config).await.unwrap();
    let ctl = server.control_addr();
    let (tx, rx) = tokio::sync::oneshot::channel();
    let h = tokio::spawn(server.run(rx));

    // Client A: connect and go silent (held open for the whole test).
    let stalled = TcpStream::connect(ctl).unwrap();

    // Client B: full /healthz round-trip must complete despite A.
    let healthz = tokio::task::spawn_blocking(move || {
        let mut s = TcpStream::connect(ctl).unwrap();
        s.write_all(b"GET /healthz HTTP/1.1\r\nHost: x\r\n\r\n")
            .unwrap();
        let mut out = String::new();
        s.read_to_string(&mut out).unwrap();
        out
    });
    let resp = tokio::time::timeout(std::time::Duration::from_secs(5), healthz)
        .await
        .expect("healthz must not hang behind an idle control connection")
        .unwrap();
    assert!(resp.starts_with("HTTP/1.1 200 OK"), "got: {resp}");
    let body = resp.split("\r\n\r\n").nth(1).unwrap_or("");
    let parsed: serde_json::Value = serde_json::from_str(body).expect("healthz body is JSON");
    assert_eq!(parsed["ok"], serde_json::json!(true));
    assert_eq!(parsed["instance_id"], serde_json::json!("test"));

    drop(stalled);
    let _ = tx.send(());
    let _ = h.await;
    let _ = std::fs::remove_dir_all(root);
}

/// #27: repeated bind/run/shutdown cycles with held-open connections must
/// tear down promptly — per-connection tasks are owned by their accept
/// loop's JoinSet, which aborts them when run() exits. A leak shows up here
/// as run() futures that never resolve (or runaway task accumulation under
/// --test-threads=1).
#[tokio::test]
async fn bind_teardown_loop_with_live_connections_is_clean() {
    for cycle in 0..5 {
        let root = tmp_card();
        let config = Config {
            instance_id: format!("cycle-{cycle}"),
            profile: "fuji/gfx100ii".into(),
            connection: "app".into(),
            manifest_yaml: MANIFEST.into(),
            media_root: root.clone(),
            command_bind: Some("127.0.0.1:0".parse().unwrap()),
            liveview_bind: Some("127.0.0.1:0".parse().unwrap()),
            event_bind: Some("127.0.0.1:0".parse().unwrap()),
            knock_bind: None,
            pcss_init_fails: 0,
            pcss_shutter_enqueue_count: 0,
            control_bind: "127.0.0.1:0".parse().unwrap(),
            liveview_dir: None,
            state_callback: None,
        };
        let server = Server::bind(config).await.unwrap();
        let cmd = server.command_addr();
        let lv = server
            .liveview_addr_opt()
            .expect("app connection has live-view socket");
        let ctl = server.control_addr();
        let (tx, rx) = tokio::sync::oneshot::channel();
        let run = tokio::spawn(server.run(rx));

        // Hold one idle connection of each flavor open across the shutdown:
        // a command client mid-handshake, an idle liveview client, and a
        // control client that never sends its request line.
        let held_cmd = TcpStream::connect(cmd).unwrap();
        let held_lv = TcpStream::connect(lv).unwrap();
        let held_ctl = TcpStream::connect(ctl).unwrap();

        // Also complete one real round-trip so the cycle isn't vacuous.
        // spawn_blocking: #[tokio::test] is a current-thread runtime shared
        // with the server tasks — a blocking read inline would deadlock.
        let out = tokio::task::spawn_blocking(move || {
            let mut s = TcpStream::connect(ctl).unwrap();
            s.write_all(b"GET /healthz HTTP/1.1\r\nHost: x\r\n\r\n")
                .unwrap();
            let mut out = String::new();
            s.read_to_string(&mut out).unwrap();
            out
        })
        .await
        .unwrap();
        assert!(out.starts_with("HTTP/1.1 200 OK"), "cycle {cycle}: {out}");

        let _ = tx.send(());
        tokio::time::timeout(std::time::Duration::from_secs(5), run)
            .await
            .unwrap_or_else(|_| panic!("cycle {cycle}: run() did not resolve after shutdown"))
            .unwrap();

        drop((held_cmd, held_lv, held_ctl));
        let _ = std::fs::remove_dir_all(root);
    }
}

/// #27: a liveview client that disconnects while the engine is NOT streaming
/// must not leave its task ticking forever — the read-half watch breaks the
/// loop on EOF. Observable proxy: server shutdown stays prompt after many
/// connect/disconnect cycles with no frame ever written.
#[tokio::test]
async fn idle_liveview_disconnects_are_reaped() {
    let root = tmp_card();
    let config = Config {
        instance_id: "test".into(),
        profile: "fuji/gfx100ii".into(),
        connection: "app".into(),
        manifest_yaml: MANIFEST.into(),
        media_root: root.clone(),
        command_bind: Some("127.0.0.1:0".parse().unwrap()),
        liveview_bind: Some("127.0.0.1:0".parse().unwrap()),
        event_bind: Some("127.0.0.1:0".parse().unwrap()),
        knock_bind: None,
        pcss_init_fails: 0,
        pcss_shutter_enqueue_count: 0,
        control_bind: "127.0.0.1:0".parse().unwrap(),
        liveview_dir: None,
        state_callback: None,
    };
    let server = Server::bind(config).await.unwrap();
    let lv = server
        .liveview_addr_opt()
        .expect("app connection has live-view socket");
    let (tx, rx) = tokio::sync::oneshot::channel();
    let run = tokio::spawn(server.run(rx));

    for _ in 0..20 {
        let s = TcpStream::connect(lv).unwrap();
        drop(s); // immediate disconnect, engine never streams
    }
    // Give the read-half watchers a moment to observe EOF.
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    let _ = tx.send(());
    tokio::time::timeout(std::time::Duration::from_secs(5), run)
        .await
        .expect("run() resolves promptly despite liveview connect/disconnect churn")
        .unwrap();
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn unarmed_engine_drops_init_command_request() {
    // #102: a BLE AP handoff that function-launched WITHOUT the IMAGE_TRANSFER_SETTING
    // prep write leaves the engine unarmed — the service must drop InitCommandRequest
    // with NO ack (the camera accepts the TCP, then silently hangs up). The default
    // (standalone, armed) path is covered by `service_drives_image_import_over_tcp`.
    let root = tmp_card();
    let rt = tokio::runtime::Runtime::new().unwrap();
    let (command_addr, shutdown_tx, handle) = rt.block_on(async {
        let config = Config {
            instance_id: "test".into(),
            profile: "fuji/gfx100ii/fw0230".into(),
            connection: "app".into(),
            manifest_yaml: MANIFEST.into(),
            media_root: root.clone(),
            command_bind: Some("127.0.0.1:0".parse().unwrap()),
            liveview_bind: Some("127.0.0.1:0".parse().unwrap()),
            event_bind: Some("127.0.0.1:0".parse().unwrap()),
            knock_bind: None,
            pcss_init_fails: 0,
            pcss_shutter_enqueue_count: 0,
            control_bind: "127.0.0.1:0".parse().unwrap(),
            liveview_dir: None,
            state_callback: None,
        };
        let server = Server::bind(config).await.unwrap();
        let cmd = server.command_addr();
        // Model the AP handoff function-launch with no preceding prep write → unarmed.
        server.camera_link().await.launch_ap();
        let (tx, rx) = tokio::sync::oneshot::channel();
        let h = tokio::spawn(server.run(rx));
        (cmd, tx, h)
    });

    let mut s = TcpStream::connect(command_addr).unwrap();
    s.set_read_timeout(Some(std::time::Duration::from_secs(5)))
        .unwrap();
    write_frame(&mut s, &app_init_frame(1, "smoke"));
    // The server hangs up without acking → a clean EOF (0 bytes), not an ack frame.
    let mut buf = [0u8; 4];
    let n = s.read(&mut buf).expect("read returns (EOF), not a timeout");
    assert_eq!(
        n, 0,
        "unarmed engine must drop InitCommandRequest with no ack (#102)"
    );

    let _ = shutdown_tx.send(());
    rt.block_on(handle).unwrap();
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn mismatched_friendly_name_is_dropped_a_matching_one_is_acked() {
    // #109: the camera gates InitCommandRequest on the PTP/IP friendly name matching
    // the device name the host registered over BLE (deviceNameString). A mismatch is
    // silently dropped (no ack); the matching name is acked. Standalone init (no BLE
    // registration, name None) stays ungated — that path is covered by
    // `service_drives_image_import_over_tcp` (friendly_name "smoke" → ack).
    let root = tmp_card();
    let rt = tokio::runtime::Runtime::new().unwrap();
    let (command_addr, shutdown_tx, handle) = rt.block_on(async {
        let config = Config {
            instance_id: "test".into(),
            profile: "fuji/gfx100ii/fw0230".into(),
            connection: "app".into(),
            manifest_yaml: MANIFEST.into(),
            media_root: root.clone(),
            command_bind: Some("127.0.0.1:0".parse().unwrap()),
            liveview_bind: Some("127.0.0.1:0".parse().unwrap()),
            event_bind: Some("127.0.0.1:0".parse().unwrap()),
            knock_bind: None,
            pcss_init_fails: 0,
            pcss_shutter_enqueue_count: 0,
            control_bind: "127.0.0.1:0".parse().unwrap(),
            liveview_dir: None,
            state_callback: None,
        };
        let server = Server::bind(config).await.unwrap();
        let cmd = server.command_addr();
        // Model the BLE pairing write: the host registered its own device name.
        server.camera_link().await.note_device_name("iphone".into());
        let (tx, rx) = tokio::sync::oneshot::channel();
        let h = tokio::spawn(server.run(rx));
        (cmd, tx, h)
    });

    // A friendly name that disagrees with the BLE-registered "iphone" → dropped, no ack.
    let mut s = TcpStream::connect(command_addr).unwrap();
    s.set_read_timeout(Some(std::time::Duration::from_secs(5)))
        .unwrap();
    write_frame(&mut s, &app_init_frame(1, "Pixel-6-4976"));
    let mut buf = [0u8; 4];
    let n = s.read(&mut buf).expect("read returns (EOF), not a timeout");
    assert_eq!(
        n, 0,
        "a friendly name != the BLE-registered device name must be dropped (#109)"
    );

    // The matching name → acked.
    let mut s2 = TcpStream::connect(command_addr).unwrap();
    s2.set_read_timeout(Some(std::time::Duration::from_secs(5)))
        .unwrap();
    write_frame(&mut s2, &app_init_frame(1, "iphone"));
    match PtpIpPacket::decode(&read_frame(&mut s2)).unwrap() {
        PtpIpPacket::InitCommandAck(_) => {}
        other => panic!("expected InitCommandAck for the matching name, got {other:?}"),
    }

    let _ = shutdown_tx.send(());
    rt.block_on(handle).unwrap();
    let _ = std::fs::remove_dir_all(root);
}

/// Return the POST body once the buffer holds full headers + the declared
/// Content-Length, else None (keep reading).
fn http_body_if_complete(buf: &[u8]) -> Option<String> {
    let s = std::str::from_utf8(buf).ok()?;
    let split = s.find("\r\n\r\n")?;
    let body = &s[split + 4..];
    let clen: usize = s[..split]
        .lines()
        .find_map(|l| {
            l.strip_prefix("Content-Length:")
                .or_else(|| l.strip_prefix("content-length:"))
        })
        .and_then(|v| v.trim().parse().ok())?;
    (body.len() >= clen).then(|| body[..clen].to_string())
}

async fn state_observer(
    expected_posts: usize,
) -> (std::net::SocketAddr, std::sync::mpsc::Receiver<String>) {
    let (body_tx, body_rx) = std::sync::mpsc::channel::<String>();
    let receiver = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let recv_addr = receiver.local_addr().unwrap();
    tokio::spawn(async move {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        for _ in 0..expected_posts {
            if let Ok((mut sock, _)) = receiver.accept().await {
                let mut buf = Vec::new();
                let mut chunk = [0u8; 1024];
                while let Ok(n) = sock.read(&mut chunk).await {
                    if n == 0 {
                        break;
                    }
                    buf.extend_from_slice(&chunk[..n]);
                    if let Some(body) = http_body_if_complete(&buf) {
                        let _ = sock
                            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n")
                            .await;
                        let _ = body_tx.send(body);
                        break;
                    }
                }
            }
        }
    });
    (recv_addr, body_rx)
}

/// #126: with `--state-callback` set, a state-changing op POSTs a JSON snapshot
/// of camera state to the observer URL. Also confirms the responder keeps
/// serving when the observer is the only HTTP party (fire-and-forget).
#[test]
fn state_callback_posts_camera_state_on_change() {
    let root = tmp_card();
    let rt = tokio::runtime::Runtime::new().unwrap();
    let (command_addr, shutdown_tx, handle, body_rx) = rt.block_on(async {
        let (recv_addr, body_rx) = state_observer(2).await;

        let config = Config {
            instance_id: "test".into(),
            profile: "fuji/gfx100ii/fw0230".into(),
            connection: "app".into(),
            manifest_yaml: MANIFEST.into(),
            media_root: root.clone(),
            command_bind: Some("127.0.0.1:0".parse().unwrap()),
            liveview_bind: Some("127.0.0.1:0".parse().unwrap()),
            event_bind: Some("127.0.0.1:0".parse().unwrap()),
            knock_bind: None,
            pcss_init_fails: 0,
            pcss_shutter_enqueue_count: 0,
            control_bind: "127.0.0.1:0".parse().unwrap(),
            liveview_dir: None,
            state_callback: Some(format!("http://{recv_addr}/state")),
        };
        let server = Server::bind(config).await.unwrap();
        let cmd = server.command_addr();
        let (tx, rx) = tokio::sync::oneshot::channel();
        let h = tokio::spawn(server.run(rx));
        (cmd, tx, h, body_rx)
    });

    // Drive: init handshake + OpenSession (flips session_open=true and the phase).
    let mut s = TcpStream::connect(command_addr).unwrap();
    write_frame(&mut s, &app_init_frame(1, "smoke"));
    let _ = read_frame(&mut s); // InitCommandAck
    write_frame(&mut s, &op(0x1002, 1, vec![1]));
    read_ok(&mut s);

    let first = body_rx
        .recv_timeout(std::time::Duration::from_secs(5))
        .expect("observer should receive initial state POST");
    assert!(
        first.contains("\"phase\":\"disconnected\""),
        "initial body: {first}"
    );

    // The mutation push is debounced (~150ms); wait for the observer to receive it.
    let body = body_rx
        .recv_timeout(std::time::Duration::from_secs(5))
        .expect("observer should receive a state POST after OpenSession");
    assert!(body.contains("\"phase\":\"sessionOpen\""), "body: {body}");
    assert!(body.contains("\"session_open\":true"), "body: {body}");

    // The responder is still alive after the fire-and-forget push.
    write_frame(&mut s, &op(0x1001, 2, vec![]));
    let di = ptp_core::DeviceInfo::decode(&read_data_reply(&mut s)).unwrap();
    assert_eq!(di.model, "GFX100 II");

    rt.block_on(async {
        let _ = shutdown_tx.send(());
        let _ = handle.await;
    });
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn runtime_callback_subscribe_posts_initial_and_later_state() {
    let root = tmp_card();
    let rt = tokio::runtime::Runtime::new().unwrap();
    let (control_addr, shutdown_tx, handle, body_rx, recv_addr) = rt.block_on(async {
        let (recv_addr, body_rx) = state_observer(2).await;
        let config = Config {
            instance_id: "test".into(),
            profile: "fuji/gfx100ii/fw0230".into(),
            connection: "app".into(),
            manifest_yaml: real_gfx_manifest(),
            media_root: root.clone(),
            command_bind: Some("127.0.0.1:0".parse().unwrap()),
            liveview_bind: Some("127.0.0.1:0".parse().unwrap()),
            event_bind: Some("127.0.0.1:0".parse().unwrap()),
            knock_bind: None,
            pcss_init_fails: 0,
            pcss_shutter_enqueue_count: 0,
            control_bind: "127.0.0.1:0".parse().unwrap(),
            liveview_dir: None,
            state_callback: None,
        };
        let server = Server::bind(config).await.unwrap();
        let ctl = server.control_addr();
        let (tx, rx) = tokio::sync::oneshot::channel();
        let h = tokio::spawn(server.run(rx));
        (ctl, tx, h, body_rx, recv_addr)
    });

    let body = http_post_json(
        control_addr,
        "/callbacks",
        &format!(r#"{{"url":"http://{recv_addr}/state"}}"#),
    );
    assert!(body.contains("\"ok\":true"), "subscribe body: {body}");

    let first = body_rx
        .recv_timeout(std::time::Duration::from_secs(5))
        .expect("runtime observer should receive initial state POST");
    assert!(
        first.contains("\"phase\":\"disconnected\""),
        "initial body: {first}"
    );

    let body = http_patch(control_addr, "/state", r#"{"phase":"liveView"}"#);
    assert!(body.contains("\"ok\":true"), "patch body: {body}");
    let pushed = body_rx
        .recv_timeout(std::time::Duration::from_secs(5))
        .expect("runtime observer should receive state POST after PATCH /state");
    assert!(pushed.contains("\"phase\":\"liveView\""), "body: {pushed}");

    let _ = http_post(control_addr, "/shutdown");
    rt.block_on(async {
        let _ = shutdown_tx.send(());
        let _ = handle.await;
    });
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn control_patch_state_updates_shared_snapshot() {
    let root = tmp_card();
    let rt = tokio::runtime::Runtime::new().unwrap();
    let (control_addr, shutdown_tx, handle) = rt.block_on(async {
        let config = Config {
            instance_id: "test".into(),
            profile: "fuji/gfx100ii/fw0230".into(),
            connection: "app".into(),
            manifest_yaml: real_gfx_manifest(),
            media_root: root.clone(),
            command_bind: Some("127.0.0.1:0".parse().unwrap()),
            liveview_bind: Some("127.0.0.1:0".parse().unwrap()),
            event_bind: Some("127.0.0.1:0".parse().unwrap()),
            knock_bind: None,
            pcss_init_fails: 0,
            pcss_shutter_enqueue_count: 0,
            control_bind: "127.0.0.1:0".parse().unwrap(),
            liveview_dir: None,
            state_callback: None,
        };
        let server = Server::bind(config).await.unwrap();
        let ctl = server.control_addr();
        let (tx, rx) = tokio::sync::oneshot::channel();
        let h = tokio::spawn(server.run(rx));
        (ctl, tx, h)
    });

    let body = http_patch(
        control_addr,
        "/state",
        r#"{"profile":"fuji/gfx100ii/fw0230","connection":"app","props":{"0xd02a":2000}}"#,
    );
    assert!(body.contains("\"ok\":true"), "patch body: {body}");
    assert!(body.contains("\"props\":1"), "patch body: {body}");

    let state = http_get(control_addr, "/state");
    assert!(state.contains("\"0xd02a\":2000"), "state body: {state}");
    assert!(
        state.contains("\"property_labels\""),
        "state should expose property labels: {state}"
    );
    assert!(
        state.contains("\"0xd02a\":\"stillIso\""),
        "state should expose manifest-backed property names: {state}"
    );

    let _ = http_post(control_addr, "/shutdown");
    rt.block_on(async {
        let _ = shutdown_tx.send(());
        let _ = handle.await;
    });
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn failed_control_patch_state_is_atomic() {
    let root = tmp_card();
    let rt = tokio::runtime::Runtime::new().unwrap();
    let (control_addr, shutdown_tx, handle) = rt.block_on(async {
        let config = Config {
            instance_id: "test".into(),
            profile: "fuji/gfx100ii/fw0230".into(),
            connection: "app".into(),
            manifest_yaml: real_gfx_manifest(),
            media_root: root.clone(),
            command_bind: Some("127.0.0.1:0".parse().unwrap()),
            liveview_bind: Some("127.0.0.1:0".parse().unwrap()),
            event_bind: Some("127.0.0.1:0".parse().unwrap()),
            knock_bind: None,
            pcss_init_fails: 0,
            pcss_shutter_enqueue_count: 0,
            control_bind: "127.0.0.1:0".parse().unwrap(),
            liveview_dir: None,
            state_callback: None,
        };
        let server = Server::bind(config).await.unwrap();
        let ctl = server.control_addr();
        let (tx, rx) = tokio::sync::oneshot::channel();
        let h = tokio::spawn(server.run(rx));
        (ctl, tx, h)
    });

    let body = http_patch(
        control_addr,
        "/state",
        r#"{"phase":"streaming","session_open":true,"props":{"0xd02a":2000,"0xffff":1}}"#,
    );
    assert!(
        body.contains("400 Bad Request") || body.contains("\"error\""),
        "patch body: {body}"
    );

    let state = http_get(control_addr, "/state");
    assert!(
        state.contains("\"phase\":\"disconnected\""),
        "state body: {state}"
    );
    assert!(
        state.contains("\"session_open\":false"),
        "state body: {state}"
    );
    assert!(state.contains("\"0xd02a\":200"), "state body: {state}");

    let _ = http_post(control_addr, "/shutdown");
    rt.block_on(async {
        let _ = shutdown_tx.send(());
        let _ = handle.await;
    });
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn control_patch_state_notifies_callback_observer() {
    let root = tmp_card();
    let rt = tokio::runtime::Runtime::new().unwrap();
    let (control_addr, shutdown_tx, handle, body_rx) = rt.block_on(async {
        let (recv_addr, body_rx) = state_observer(2).await;
        let config = Config {
            instance_id: "test".into(),
            profile: "fuji/gfx100ii/fw0230".into(),
            connection: "app".into(),
            manifest_yaml: real_gfx_manifest(),
            media_root: root.clone(),
            command_bind: Some("127.0.0.1:0".parse().unwrap()),
            liveview_bind: Some("127.0.0.1:0".parse().unwrap()),
            event_bind: Some("127.0.0.1:0".parse().unwrap()),
            knock_bind: None,
            pcss_init_fails: 0,
            pcss_shutter_enqueue_count: 0,
            control_bind: "127.0.0.1:0".parse().unwrap(),
            liveview_dir: None,
            state_callback: Some(format!("http://{recv_addr}/state")),
        };
        let server = Server::bind(config).await.unwrap();
        let ctl = server.control_addr();
        let (tx, rx) = tokio::sync::oneshot::channel();
        let h = tokio::spawn(server.run(rx));
        (ctl, tx, h, body_rx)
    });

    let first = body_rx
        .recv_timeout(std::time::Duration::from_secs(5))
        .expect("observer should receive initial state POST");
    assert!(
        first.contains("\"phase\":\"disconnected\""),
        "initial body: {first}"
    );

    let body = http_patch(control_addr, "/state", r#"{"props":{"0xd02a":2000}}"#);
    assert!(body.contains("\"ok\":true"), "patch body: {body}");
    let pushed = body_rx
        .recv_timeout(std::time::Duration::from_secs(5))
        .expect("observer should receive state POST after PATCH /state");
    assert!(pushed.contains("\"0xd02a\":2000"), "body: {pushed}");

    let _ = http_post(control_addr, "/shutdown");
    rt.block_on(async {
        let _ = shutdown_tx.send(());
        let _ = handle.await;
    });
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn action_catalog_preflights_responder_mutation_and_exports_observation() {
    let root = tmp_card_with_jpegs(3);
    let rt = tokio::runtime::Runtime::new().unwrap();
    let (control_addr, shutdown_tx, handle) = rt.block_on(async {
        let config = Config {
            instance_id: "action-contract".into(),
            profile: "fuji/gfx100ii/fw0230".into(),
            connection: "wireless-tether".into(),
            manifest_yaml: real_gfx_manifest(),
            media_root: root.clone(),
            command_bind: Some("127.0.0.1:0".parse().unwrap()),
            liveview_bind: None,
            event_bind: None,
            knock_bind: None,
            pcss_init_fails: 0,
            pcss_shutter_enqueue_count: 1,
            control_bind: "127.0.0.1:0".parse().unwrap(),
            liveview_dir: None,
            state_callback: None,
        };
        let server = Server::bind(config).await.unwrap();
        let control = server.control_addr();
        let (tx, rx) = tokio::sync::oneshot::channel();
        let handle = tokio::spawn(server.run(rx));
        (control, tx, handle)
    });

    let catalog_response = http_get(control_addr, "/actions");
    let catalog: serde_json::Value = serde_json::from_str(
        catalog_response
            .split_once("\r\n\r\n")
            .expect("catalog response body")
            .1,
    )
    .unwrap();
    let revision = catalog["revision"].as_str().unwrap();
    let shutter = catalog["actions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|action| action["connection"] == "wireless-tether" && action["actionId"] == "shutter")
        .expect("wireless shutter in catalog");
    assert_eq!(
        shutter["supportedRoles"],
        serde_json::json!(["initiator", "responder"])
    );

    let wrong_mode = serde_json::json!({
        "catalogRevision": revision,
        "mode": "image-transfer",
        "role": "responder",
        "parameters": [],
    });
    let rejected = http_post_json(control_addr, "/actions/shutter", &wrong_mode.to_string());
    assert!(rejected.starts_with("HTTP/1.1 400"), "{rejected}");
    assert!(rejected.contains("wrongMode"), "{rejected}");

    let accepted = serde_json::json!({
        "catalogRevision": revision,
        "mode": "shooting/stills",
        "role": "responder",
        "parameters": [],
    });
    let response = http_post_json(control_addr, "/actions/shutter", &accepted.to_string());
    assert!(response.starts_with("HTTP/1.1 200"), "{response}");
    assert!(response.contains("\"affectedObjects\":1"), "{response}");

    let observations = http_get(control_addr, "/observations?after=0");
    let body: serde_json::Value = serde_json::from_str(
        observations
            .split_once("\r\n\r\n")
            .expect("observation response body")
            .1,
    )
    .unwrap();
    assert_eq!(body["schema"], "camera-observation/v1");
    let records = body["records"].as_array().unwrap();
    assert!(records.iter().any(|record| {
        record["kind"] == "actionInvocation"
            && record["actionId"] == "shutter"
            && record["outcome"] == "rejected"
    }));
    assert!(records.iter().any(|record| {
        record["kind"] == "actionInvocation"
            && record["actionId"] == "shutter"
            && record["outcome"] == "succeeded"
    }));
    let bundle = std::iter::once(body["header"].to_string())
        .chain(records.iter().map(serde_json::Value::to_string))
        .collect::<Vec<_>>()
        .join("\n");
    camera_config::validate_bundles(&[&bundle]).expect("service export is canonical input");

    let _ = shutdown_tx.send(());
    rt.block_on(async {
        let _ = handle.await;
    });
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn responder_action_has_no_side_effect_when_observation_append_fails() {
    let root = tmp_card_with_jpegs(2);
    let rt = tokio::runtime::Runtime::new().unwrap();
    let (control_addr, shutdown_tx, handle) = rt.block_on(async {
        let config = Config {
            instance_id: "action-recording-failure".into(),
            profile: "fuji/gfx100ii/fw0230".into(),
            connection: "wireless-tether".into(),
            manifest_yaml: real_gfx_manifest(),
            media_root: root.clone(),
            command_bind: Some("127.0.0.1:0".parse().unwrap()),
            liveview_bind: None,
            event_bind: None,
            knock_bind: None,
            pcss_init_fails: 0,
            pcss_shutter_enqueue_count: 1,
            control_bind: "127.0.0.1:0".parse().unwrap(),
            liveview_dir: None,
            state_callback: None,
        };
        let server = Server::bind(config).await.unwrap();
        let control = server.control_addr();
        let (tx, rx) = tokio::sync::oneshot::channel();
        let handle = tokio::spawn(server.run(rx));
        (control, tx, handle)
    });

    let catalog_response = http_get(control_addr, "/actions");
    let catalog: serde_json::Value = serde_json::from_str(
        catalog_response
            .split_once("\r\n\r\n")
            .expect("catalog response body")
            .1,
    )
    .unwrap();
    let request = serde_json::json!({
        "catalogRevision": catalog["revision"],
        "mode": "shooting/stills",
        "role": "responder",
        "parameters": [],
    });
    let before = http_get(control_addr, "/state");
    let before: serde_json::Value =
        serde_json::from_str(before.split_once("\r\n\r\n").unwrap().1).unwrap();
    assert_eq!(before["transfer_queues"]["standard"]["queued"], 0);

    std::fs::remove_file(
        root.join(".ptpsim")
            .join("observations-action-recording-failure.jsonl"),
    )
    .unwrap();
    let response = http_post_json(control_addr, "/actions/shutter", &request.to_string());
    assert!(response.starts_with("HTTP/1.1 500"), "{response}");
    let after = http_get(control_addr, "/state");
    let after: serde_json::Value =
        serde_json::from_str(after.split_once("\r\n\r\n").unwrap().1).unwrap();
    assert_eq!(after["transfer_queues"]["standard"]["queued"], 0);

    let _ = shutdown_tx.send(());
    rt.block_on(async {
        let _ = handle.await;
    });
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn property_transition_is_atomic_and_state_reads_do_not_advance_it() {
    let root = tmp_card();
    let rt = tokio::runtime::Runtime::new().unwrap();
    let (command_addr, control_addr, shutdown_tx, handle) = rt.block_on(async {
        let server = Server::bind(Config {
            instance_id: "property-transition-atomic".into(),
            profile: "test/property-transition".into(),
            connection: "app".into(),
            manifest_yaml: PROPERTY_TRANSITION_MANIFEST.into(),
            media_root: root.clone(),
            command_bind: Some("127.0.0.1:0".parse().unwrap()),
            liveview_bind: None,
            event_bind: None,
            knock_bind: None,
            pcss_init_fails: 0,
            pcss_shutter_enqueue_count: 0,
            control_bind: "127.0.0.1:0".parse().unwrap(),
            liveview_dir: None,
            state_callback: None,
        })
        .await
        .unwrap();
        let command = server.command_addr();
        let control = server.control_addr();
        let (tx, rx) = tokio::sync::oneshot::channel();
        let handle = tokio::spawn(server.run(rx));
        (command, control, tx, handle)
    });

    let catalog_response = http_get(control_addr, "/actions");
    let catalog: serde_json::Value = serde_json::from_str(
        catalog_response
            .split_once("\r\n\r\n")
            .expect("catalog response body")
            .1,
    )
    .unwrap();
    let invoke = |result| {
        serde_json::json!({
            "catalogRevision": catalog["revision"],
            "mode": "shooting/stills",
            "role": "responder",
            "parameters": [{ "name": "result", "value": result }],
        })
    };

    let response = http_post_json(
        control_addr,
        "/actions/autofocusLock",
        &invoke(2).to_string(),
    );
    assert!(response.starts_with("HTTP/1.1 200"), "{response}");
    let response: serde_json::Value =
        serde_json::from_str(response.split_once("\r\n\r\n").unwrap().1).unwrap();
    assert_eq!(response["affectedObjects"], 0);
    assert_eq!(
        response["affectedProperties"],
        serde_json::json!([{
            "target": "0xd001",
            "initial": 1,
            "terminal": 2,
            "settleAfterPolls": 2,
        }])
    );

    let mut stream = connect_ptpip(command_addr, "transition-smoke");
    open_session(&mut stream);

    for _ in 0..2 {
        let state = http_get(control_addr, "/state");
        let state: serde_json::Value =
            serde_json::from_str(state.split_once("\r\n\r\n").unwrap().1).unwrap();
        assert_eq!(state["props"]["0xd001"], 1);
    }

    let observation_path = root
        .join(".ptpsim")
        .join("observations-property-transition-atomic.jsonl");
    std::fs::remove_file(&observation_path).unwrap();
    let failed = http_post_json(
        control_addr,
        "/actions/autofocusLock",
        &invoke(3).to_string(),
    );
    assert!(failed.starts_with("HTTP/1.1 500"), "{failed}");

    let state = http_get(control_addr, "/state");
    let state: serde_json::Value =
        serde_json::from_str(state.split_once("\r\n\r\n").unwrap().1).unwrap();
    assert_eq!(state["props"]["0xd001"], 1);

    std::fs::File::create(observation_path).unwrap();
    assert_eq!(read_u16_prop(&mut stream, 2, 0xd001), 1);
    assert_eq!(
        read_u16_prop(&mut stream, 3, 0xd001),
        2,
        "failed re-arm must not replace the pending terminal"
    );

    let _ = shutdown_tx.send(());
    rt.block_on(async {
        let _ = handle.await;
    });
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn observation_cursor_survives_service_restart() {
    let root = tmp_card_with_jpegs(2);
    let rt = tokio::runtime::Runtime::new().unwrap();
    let config = Config {
        instance_id: "observation-restart".into(),
        profile: "fuji/gfx100ii/fw0230".into(),
        connection: "wireless-tether".into(),
        manifest_yaml: real_gfx_manifest(),
        media_root: root.clone(),
        command_bind: Some("127.0.0.1:0".parse().unwrap()),
        liveview_bind: None,
        event_bind: None,
        knock_bind: None,
        pcss_init_fails: 0,
        pcss_shutter_enqueue_count: 0,
        control_bind: "127.0.0.1:0".parse().unwrap(),
        liveview_dir: None,
        state_callback: None,
    };

    let start = |config: Config| async move {
        let server = Server::bind(config).await.unwrap();
        let control = server.control_addr();
        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
        let handle = tokio::spawn(server.run(shutdown_rx));
        (control, shutdown_tx, handle)
    };
    let (control, shutdown_tx, handle) = rt.block_on(start(config.clone()));
    let catalog_response = http_get(control, "/actions");
    let catalog: serde_json::Value = serde_json::from_str(
        catalog_response
            .split_once("\r\n\r\n")
            .expect("catalog body")
            .1,
    )
    .unwrap();
    let request = serde_json::json!({
        "catalogRevision": catalog["revision"],
        "mode": "shooting/stills",
        "role": "responder",
        "parameters": [],
    });
    assert!(
        http_post_json(control, "/actions/shutter", &request.to_string())
            .starts_with("HTTP/1.1 200")
    );
    let export = http_get(control, "/observations?after=0");
    let export: serde_json::Value =
        serde_json::from_str(export.split_once("\r\n\r\n").expect("observation body").1).unwrap();
    let cursor = export["cursor"].as_u64().unwrap();
    assert!(cursor > 0);
    let _ = shutdown_tx.send(());
    rt.block_on(handle).unwrap();

    let (control, shutdown_tx, handle) = rt.block_on(start(config));
    let resumed = http_get(control, &format!("/observations?after={cursor}"));
    let resumed: serde_json::Value = serde_json::from_str(
        resumed
            .split_once("\r\n\r\n")
            .expect("resumed observation body")
            .1,
    )
    .unwrap();
    let resumed_records = resumed["records"].as_array().unwrap();
    assert_eq!(resumed_records.len(), 1);
    assert_eq!(resumed_records[0]["kind"], "lifecycle");
    assert_eq!(resumed_records[0]["ordinal"], cursor + 1);
    let restart_cursor = resumed["cursor"].as_u64().unwrap();
    assert!(
        http_post_json(control, "/actions/shutter", &request.to_string())
            .starts_with("HTTP/1.1 200")
    );
    let advanced = http_get(control, &format!("/observations?after={restart_cursor}"));
    let advanced: serde_json::Value = serde_json::from_str(
        advanced
            .split_once("\r\n\r\n")
            .expect("advanced observation body")
            .1,
    )
    .unwrap();
    assert_eq!(advanced["records"].as_array().unwrap().len(), 1);
    assert!(advanced["cursor"].as_u64().unwrap() > restart_cursor);

    let _ = shutdown_tx.send(());
    rt.block_on(handle).unwrap();
    let _ = std::fs::remove_dir_all(root);
}
