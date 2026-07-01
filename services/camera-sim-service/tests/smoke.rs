//! Black-box smoke: spin up the service on ephemeral loopback ports and drive a
//! real PTP/IP image-import flow over TCP, plus a `/healthz` check. This is the
//! service-level counterpart to design gates #5 and #6.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;

use camera_sim_service::{Config, Server};
use protocol_primitives::fuji_framing;
use ptp_core::{InitCommandRequest, PtpCodec, PtpIpPacket};

const MANIFEST: &str = r#"
schema: camera-config/v1
camera:
  manufacturer: FUJIFILM
  model: GFX100 II
  firmware: "2.30"
operations:
  "0x1002": { name: OpenSession }
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
    match fuji_framing::decode(&read_frame(s)).unwrap() {
        PtpIpPacket::OperationResponse(r) => assert_eq!(r.code, 0x2001),
        other => panic!("expected OK, got {other:?}"),
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
            manifest_yaml: MANIFEST.into(),
            media_root: root.clone(),
            command_bind: "127.0.0.1:0".parse().unwrap(),
            liveview_bind: "127.0.0.1:0".parse().unwrap(),
            event_bind: "127.0.0.1:0".parse().unwrap(),
            control_bind: "127.0.0.1:0".parse().unwrap(),
            liveview_dir: None,
            state_callback: None,
        };
        let server = Server::bind(config).await.unwrap();
        let cmd = server.command_addr();
        let lv = server.liveview_addr();
        let ctl = server.control_addr();
        let (tx, rx) = tokio::sync::oneshot::channel();
        let h = tokio::spawn(server.run(rx));
        (cmd, lv, ctl, tx, h)
    });

    // --- PTP/IP client flow ---
    let mut s = TcpStream::connect(command_addr).unwrap();
    // Init handshake (standard framing).
    let init = PtpIpPacket::InitCommandRequest(InitCommandRequest {
        initiator_guid: [1; 16],
        friendly_name: "smoke".into(),
        protocol_version: 0x0001_0000,
    });
    write_frame(&mut s, &ptp_core::encode(&init).unwrap());
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

    // Shutdown via control plane.
    let _ = http_post(control_addr, "/shutdown");
    rt.block_on(async {
        let _ = shutdown_tx.send(());
        let _ = handle.await;
    });
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
operations:
  "0x1002": { name: OpenSession }
  "0x9026":
    name: LockS1Lock
    emits: ["0xc005"]
properties: {}
"#;
    let root = tmp_card();
    let rt = tokio::runtime::Runtime::new().unwrap();
    let (command_addr, event_addr, shutdown_tx, handle) = rt.block_on(async {
        let config = Config {
            instance_id: "test".into(),
            profile: "fuji/gfx100ii/fw0230".into(),
            manifest_yaml: AF_EVENT_MANIFEST.into(),
            media_root: root.clone(),
            command_bind: "127.0.0.1:0".parse().unwrap(),
            liveview_bind: "127.0.0.1:0".parse().unwrap(),
            event_bind: "127.0.0.1:0".parse().unwrap(),
            control_bind: "127.0.0.1:0".parse().unwrap(),
            liveview_dir: None,
            state_callback: None,
        };
        let server = Server::bind(config).await.unwrap();
        let cmd = server.command_addr();
        let evt = server.event_addr();
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
    let init = PtpIpPacket::InitCommandRequest(InitCommandRequest {
        initiator_guid: [1; 16],
        friendly_name: "smoke".into(),
        protocol_version: 0x0001_0000,
    });
    write_frame(&mut s, &ptp_core::encode(&init).unwrap());
    let _ = read_frame(&mut s); // InitCommandAck
    write_frame(&mut s, &op(0x1002, 1, vec![1]));
    read_ok(&mut s);

    // Tap-to-AF: the op emits 0xC005 on its OK response.
    write_frame(&mut s, &op(0x9026, 2, vec![0x0906_0403]));
    read_ok(&mut s);

    // The push arrives on the event socket as a standard-framed Event packet.
    match PtpIpPacket::decode(&read_frame(&mut evt)).unwrap() {
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
            manifest_yaml: MANIFEST.into(),
            media_root: root.clone(),
            command_bind: "127.0.0.1:0".parse().unwrap(),
            liveview_bind: "127.0.0.1:0".parse().unwrap(),
            event_bind: "127.0.0.1:0".parse().unwrap(),
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
    let init = PtpIpPacket::InitCommandRequest(InitCommandRequest {
        initiator_guid: [1; 16],
        friendly_name: "bigobj".into(),
        protocol_version: 0x0001_0000,
    });
    write_frame(&mut s, &ptp_core::encode(&init).unwrap());
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

/// Read one live-view length-prefixed frame and return the JPEG payload.
fn read_frame_lv(s: &mut TcpStream) -> Vec<u8> {
    let mut len = [0u8; 4];
    s.read_exact(&mut len).unwrap();
    let n = u32::from_le_bytes(len) as usize;
    let mut buf = vec![0u8; n];
    s.read_exact(&mut buf).unwrap();
    buf
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
operations:
  "0x1002": { name: OpenSession }
properties:
  "0xdf01": { name: functionMode, type: u16, access: readWrite }
"#;
    let rt = tokio::runtime::Runtime::new().unwrap();
    let (command_addr, shutdown_tx, handle) = rt.block_on(async {
        let config = Config {
            instance_id: "test".into(),
            profile: "fuji/gfx100ii".into(),
            manifest_yaml: manifest.into(),
            media_root: root.clone(),
            command_bind: "127.0.0.1:0".parse().unwrap(),
            liveview_bind: "127.0.0.1:0".parse().unwrap(),
            event_bind: "127.0.0.1:0".parse().unwrap(),
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
    let init = PtpIpPacket::InitCommandRequest(InitCommandRequest {
        initiator_guid: [9; 16],
        friendly_name: "overflow".into(),
        protocol_version: 0x0001_0000,
    });
    write_frame(&mut s, &ptp_core::encode(&init).unwrap());
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

/// Live-view smoke: gate-#4 at the TCP boundary. The simulator only emits
/// frames after the initiator reaches Phase::Streaming (df01=22 -> InitiateOpenCapture);
/// before that, the socket is open but idle.
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
operations:
  "0x1002": { name: OpenSession }
properties:
  "0xdf01": { name: functionMode, type: u16, access: readWrite }
"#;

    let rt = tokio::runtime::Runtime::new().unwrap();
    let (command_addr, liveview_addr, shutdown_tx, handle) = rt.block_on(async {
        let config = Config {
            instance_id: "test".into(),
            profile: "fuji/gfx100ii".into(),
            manifest_yaml: manifest.into(),
            media_root: root.clone(),
            command_bind: "127.0.0.1:0".parse().unwrap(),
            liveview_bind: "127.0.0.1:0".parse().unwrap(),
            event_bind: "127.0.0.1:0".parse().unwrap(),
            control_bind: "127.0.0.1:0".parse().unwrap(),
            liveview_dir: Some(lv_dir.clone()),
            state_callback: None,
        };
        let server = Server::bind(config).await.unwrap();
        let cmd = server.command_addr();
        let lv = server.liveview_addr();
        let (tx, rx) = tokio::sync::oneshot::channel();
        let h = tokio::spawn(async move { server.run(rx).await });
        (cmd, lv, tx, h)
    });

    // Connect to live-view BEFORE driving the engine — reads must block (gated).
    let mut lv = TcpStream::connect(liveview_addr).unwrap();
    lv.set_read_timeout(Some(std::time::Duration::from_millis(150)))
        .unwrap();
    let mut probe = [0u8; 4];
    match lv.read_exact(&mut probe) {
        Err(e)
            if e.kind() == std::io::ErrorKind::WouldBlock
                || e.kind() == std::io::ErrorKind::TimedOut => {}
        Ok(_) => panic!("frames leaked before Phase::Streaming"),
        Err(e) => panic!("unexpected error: {e}"),
    }

    // Drive the command channel into Phase::Streaming.
    let mut s = TcpStream::connect(command_addr).unwrap();
    let init = PtpIpPacket::InitCommandRequest(InitCommandRequest {
        initiator_guid: [0; 16],
        friendly_name: "test".into(),
        protocol_version: 0x0001_0000,
    });
    write_frame(&mut s, &ptp_core::encode(&init).unwrap());
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

    // Now frames flow: read two and confirm they're the fixture (single-frame loop).
    lv.set_read_timeout(Some(std::time::Duration::from_millis(500)))
        .unwrap();
    let frame0 = read_frame_lv(&mut lv);
    let frame1 = read_frame_lv(&mut lv);
    assert_eq!(&frame0[..], lv_jpeg, "first frame matches the fixture JPEG");
    assert_eq!(frame0, frame1, "single-frame loop repeats");

    drop(lv);
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
        manifest_yaml: MANIFEST.replace("camera-config/v1", "camera-config/v999"),
        media_root: root.clone(),
        command_bind: "127.0.0.1:0".parse().unwrap(),
        liveview_bind: "127.0.0.1:0".parse().unwrap(),
        event_bind: "127.0.0.1:0".parse().unwrap(),
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
        manifest_yaml: MANIFEST.into(),
        media_root: root.clone(),
        command_bind: "127.0.0.1:0".parse().unwrap(),
        liveview_bind: "127.0.0.1:0".parse().unwrap(),
        event_bind: "127.0.0.1:0".parse().unwrap(),
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
            manifest_yaml: MANIFEST.into(),
            media_root: root.clone(),
            command_bind: "127.0.0.1:0".parse().unwrap(),
            liveview_bind: "127.0.0.1:0".parse().unwrap(),
            event_bind: "127.0.0.1:0".parse().unwrap(),
            control_bind: "127.0.0.1:0".parse().unwrap(),
            liveview_dir: None,
            state_callback: None,
        };
        let server = Server::bind(config).await.unwrap();
        let cmd = server.command_addr();
        let lv = server.liveview_addr();
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
        manifest_yaml: MANIFEST.into(),
        media_root: root.clone(),
        command_bind: "127.0.0.1:0".parse().unwrap(),
        liveview_bind: "127.0.0.1:0".parse().unwrap(),
        event_bind: "127.0.0.1:0".parse().unwrap(),
        control_bind: "127.0.0.1:0".parse().unwrap(),
        liveview_dir: None,
        state_callback: None,
    };
    let server = Server::bind(config).await.unwrap();
    let lv = server.liveview_addr();
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
            manifest_yaml: MANIFEST.into(),
            media_root: root.clone(),
            command_bind: "127.0.0.1:0".parse().unwrap(),
            liveview_bind: "127.0.0.1:0".parse().unwrap(),
            event_bind: "127.0.0.1:0".parse().unwrap(),
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
    let init = PtpIpPacket::InitCommandRequest(InitCommandRequest {
        initiator_guid: [1; 16],
        friendly_name: "smoke".into(),
        protocol_version: 0x0001_0000,
    });
    write_frame(&mut s, &ptp_core::encode(&init).unwrap());
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
            manifest_yaml: MANIFEST.into(),
            media_root: root.clone(),
            command_bind: "127.0.0.1:0".parse().unwrap(),
            liveview_bind: "127.0.0.1:0".parse().unwrap(),
            event_bind: "127.0.0.1:0".parse().unwrap(),
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
    let mismatch = PtpIpPacket::InitCommandRequest(InitCommandRequest {
        initiator_guid: [1; 16],
        friendly_name: "Pixel-6-4976".into(),
        protocol_version: 0x0001_0000,
    });
    write_frame(&mut s, &ptp_core::encode(&mismatch).unwrap());
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
    let matching = PtpIpPacket::InitCommandRequest(InitCommandRequest {
        initiator_guid: [1; 16],
        friendly_name: "iphone".into(),
        protocol_version: 0x0001_0000,
    });
    write_frame(&mut s2, &ptp_core::encode(&matching).unwrap());
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

/// #126: with `--state-callback` set, a state-changing op POSTs a JSON snapshot
/// of camera state to the observer URL. Also confirms the responder keeps
/// serving when the observer is the only HTTP party (fire-and-forget).
#[test]
fn state_callback_posts_camera_state_on_change() {
    let root = tmp_card();
    let rt = tokio::runtime::Runtime::new().unwrap();
    let (body_tx, body_rx) = std::sync::mpsc::channel::<String>();
    let (command_addr, shutdown_tx, handle) = rt.block_on(async {
        // Observer: accept one POST, hand its JSON body to the test thread.
        let receiver = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let recv_addr = receiver.local_addr().unwrap();
        tokio::spawn(async move {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
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
        });

        let config = Config {
            instance_id: "test".into(),
            profile: "fuji/gfx100ii/fw0230".into(),
            manifest_yaml: MANIFEST.into(),
            media_root: root.clone(),
            command_bind: "127.0.0.1:0".parse().unwrap(),
            liveview_bind: "127.0.0.1:0".parse().unwrap(),
            event_bind: "127.0.0.1:0".parse().unwrap(),
            control_bind: "127.0.0.1:0".parse().unwrap(),
            liveview_dir: None,
            state_callback: Some(format!("http://{recv_addr}/state")),
        };
        let server = Server::bind(config).await.unwrap();
        let cmd = server.command_addr();
        let (tx, rx) = tokio::sync::oneshot::channel();
        let h = tokio::spawn(server.run(rx));
        (cmd, tx, h)
    });

    // Drive: init handshake + OpenSession (flips session_open=true and the phase).
    let mut s = TcpStream::connect(command_addr).unwrap();
    let init = PtpIpPacket::InitCommandRequest(InitCommandRequest {
        initiator_guid: [1; 16],
        friendly_name: "smoke".into(),
        protocol_version: 0x0001_0000,
    });
    write_frame(&mut s, &ptp_core::encode(&init).unwrap());
    let _ = read_frame(&mut s); // InitCommandAck
    write_frame(&mut s, &op(0x1002, 1, vec![1]));
    read_ok(&mut s);

    // The push is debounced (~150ms); wait for the observer to receive it.
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
