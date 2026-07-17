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
    // A non-zero vendor tail distinguishes the reference app layout from PCSS, whose
    // overlapping fixed fields require a zero tail.
    build_app_init(&[guid_byte; 16], friendly_name, &[1; 28]).unwrap()
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
camera: { manufacturer: FUJIFILM, model: X-A7 }
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
    let (command_addr, shutdown_tx, handle) = rt.block_on(async {
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
        let (tx, rx) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(server.run(rx));
        (command, tx, task)
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

fn read_d621_handles(s: &mut TcpStream, tid: u32) -> Vec<u32> {
    write_frame(s, &op(0x1015, tid, vec![0xd621]));
    let bytes = read_data_reply(s);
    let mut r = ptp_core::Reader::new(&bytes);
    r.ptp_array(|r| r.u32()).unwrap()
}

fn read_reserved_count(s: &mut TcpStream, tid: u32) -> u32 {
    write_frame(s, &op(0x1015, tid, vec![0xd212]));
    let bytes = read_data_reply(s);
    let count = u16::from_le_bytes([bytes[0], bytes[1]]) as usize;
    (0..count)
        .find_map(|i| {
            let offset = 2 + i * 6;
            let code = u16::from_le_bytes([bytes[offset], bytes[offset + 1]]);
            (code == 0xdf41).then(|| {
                u32::from_le_bytes([
                    bytes[offset + 2],
                    bytes[offset + 3],
                    bytes[offset + 4],
                    bytes[offset + 5],
                ])
            })
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

/// #54: a completion event emitted by an operation is pushed to a connected
/// event-socket client as a PTP/IP Event packet. The AF op (0x9026) `emits`
/// 0xC005 AFCAPTUER; a client on the event socket (55741) must receive it.
#[test]
fn service_pushes_completion_event_on_event_socket() {
    const AF_EVENT_MANIFEST: &str = r#"
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
  "0x9026":
    name: LockS1Lock
    connections: [app]
    emits: ["0xc005"]
properties: {}
"#;
    let root = tmp_card();
    let rt = tokio::runtime::Runtime::new().unwrap();
    let (command_addr, event_addr, shutdown_tx, handle) = rt.block_on(async {
        let config = Config {
            instance_id: "test".into(),
            profile: "fuji/gfx100ii/fw0230".into(),
            connection: "app".into(),
            manifest_yaml: AF_EVENT_MANIFEST.into(),
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
        let (tx, rx) = tokio::sync::oneshot::channel();
        let h = tokio::spawn(server.run(rx));
        (cmd, evt, tx, h)
    });

    // Connect the event socket FIRST — the real app opens it during session
    // setup, before triggering a capture. A read timeout turns a missing push
    // into a clear failure instead of a hang.
    let mut evt = TcpStream::connect(event_addr).unwrap();
    evt.set_read_timeout(Some(std::time::Duration::from_secs(2)))
        .unwrap();

    // Command-socket session bring-up. These round-trips also give the event
    // accept time to land (and subscribe) before the AF op broadcasts.
    let mut s = TcpStream::connect(command_addr).unwrap();
    write_frame(&mut s, &app_init_frame(1, "smoke"));
    let _ = read_frame(&mut s); // InitCommandAck
    write_frame(&mut s, &op(0x1002, 1, vec![1]));
    read_ok(&mut s);

    // Tap-to-AF: the op emits 0xC005 on its OK response.
    write_frame(&mut s, &op(0x9026, 2, vec![0x0906_0403]));
    read_ok(&mut s);

    // The push follows the manifest's USB/PIMA event framing.
    match protocol_primitives::usb_ptp::decode(&read_frame(&mut evt)).unwrap() {
        PtpIpPacket::Event(e) => {
            assert_eq!(e.code, 0xc005, "AFCAPTUER event code");
            assert_eq!(e.transaction_id, 0, "async event uses tid 0");
        }
        other => panic!("expected Event packet, got {other:?}"),
    }

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
    let (command_addr, shutdown_tx, handle) = rt.block_on(async {
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
        let (tx, rx) = tokio::sync::oneshot::channel();
        let h = tokio::spawn(server.run(rx));
        (cmd, tx, h)
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
    write_frame(&mut s, &app_init_frame(1, "app"));
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

/// Live-view smoke: the manifest's `openChannel` step controls TCP availability,
/// then frames flow once the initiator reaches Phase::Streaming.
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
    bindings: { command: 55740, event: 55741, liveView: 55742 }
    entries:
      - to: shooting/stills
        steps:
          - { setProp: "0xdf01", value: 22 }
          - { sendOp: "0x101c" }
          - { openChannel: event }
          - { openChannel: liveView }
operations:
  "0x1002": { name: OpenSession, connections: [app] }
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

    // Connect before the manifest boundary. The simulator accepts and then
    // rejects the early socket, matching an unavailable camera listener.
    let mut lv = TcpStream::connect(liveview_addr).unwrap();
    lv.set_read_timeout(Some(std::time::Duration::from_millis(150)))
        .unwrap();
    let mut probe = [0u8; 4];
    match lv.read(&mut probe) {
        Ok(0) => {}
        Ok(count) => panic!("early live-view channel returned {count} bytes instead of closing"),
        Err(error) => panic!("early live-view channel stayed open instead of closing: {error}"),
    }
    let mut event = TcpStream::connect(event_addr).unwrap();
    event
        .set_read_timeout(Some(std::time::Duration::from_millis(150)))
        .unwrap();
    match event.read(&mut probe) {
        Ok(0) => {}
        Ok(count) => panic!("early event channel returned {count} bytes instead of closing"),
        Err(error) => panic!("early event channel stayed open instead of closing: {error}"),
    }

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
    assert!(state.contains("\"0xd02a\":32769"), "state body: {state}");

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
