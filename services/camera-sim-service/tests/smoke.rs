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

/// Read a data reply: StartData, EndData(payload), OperationResponse(OK).
fn read_data_reply(s: &mut TcpStream) -> Vec<u8> {
    let _start = fuji_framing::decode(&read_frame(s)).unwrap();
    let end = fuji_framing::decode(&read_frame(s)).unwrap();
    let resp = fuji_framing::decode(&read_frame(s)).unwrap();
    let data = match end {
        PtpIpPacket::EndData(d) => d.payload,
        other => panic!("expected EndData, got {other:?}"),
    };
    match resp {
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

/// Read a streamed data reply, returning the reassembled payload and the
/// count of `Data` frames that preceded `EndData`. Proves chunking: a payload
/// larger than `DATA_CHUNK_BYTES` must arrive in at least one `Data` frame
/// followed by one `EndData`, never a single oversized EndData.
fn read_chunked_data_reply(s: &mut TcpStream) -> (Vec<u8>, usize) {
    let _start = match fuji_framing::decode(&read_frame(s)).unwrap() {
        PtpIpPacket::StartData(p) => p,
        other => panic!("expected StartData, got {other:?}"),
    };
    let mut payload = Vec::new();
    let mut data_frames = 0usize;
    loop {
        match fuji_framing::decode(&read_frame(s)).unwrap() {
            PtpIpPacket::Data(d) => {
                payload.extend_from_slice(&d.payload);
                data_frames += 1;
            }
            PtpIpPacket::EndData(d) => {
                payload.extend_from_slice(&d.payload);
                break;
            }
            other => panic!("unexpected packet in data phase: {other:?}"),
        }
    }
    match fuji_framing::decode(&read_frame(s)).unwrap() {
        PtpIpPacket::OperationResponse(r) => assert_eq!(r.code, 0x2001, "OK expected"),
        other => panic!("expected response, got {other:?}"),
    }
    (payload, data_frames)
}

#[test]
fn service_streams_get_object_in_bounded_chunks() {
    // A 2 MiB JPEG forces at least two on-wire chunks at the 1 MiB chunk size.
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

    // GetObject — the 2 MiB JPEG.
    write_frame(&mut s, &op(0x1009, 3, vec![handles[0]]));
    let (payload, data_frames) = read_chunked_data_reply(&mut s);
    assert_eq!(payload.len(), 2 * 1024 * 1024);
    assert_eq!(&payload[0..2], &[0xFF, 0xD8]);
    assert_eq!(&payload[payload.len() - 2..], &[0xFF, 0xD9]);
    assert!(
        data_frames >= 1,
        "expected at least one Data frame before EndData, got {data_frames}"
    );

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
fn service_rejects_data_in_that_overflows_declared_length() {
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

    // SetDevicePropValue df01 with a malicious data-in: declared = 2 bytes,
    // but ship a 256 KiB Data frame. The server must reject.
    write_frame(&mut s, &op(0x1016, 2, vec![0xdf01]));
    let start = PtpIpPacket::StartData(ptp_core::StartData {
        transaction_id: 2,
        total_length: 2,
    });
    write_frame(&mut s, &fuji_framing::encode(&start).unwrap());
    let oversized = PtpIpPacket::Data(ptp_core::DataBlock {
        transaction_id: 2,
        payload: vec![0u8; 256 * 1024],
    });
    let _ = s.write_all(&fuji_framing::encode(&oversized).unwrap());

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

/// Write the StartData/EndData wire dance for a single SetDevicePropValue payload.
fn write_set_prop_data(s: &mut TcpStream, tid: u32, payload: &[u8]) {
    let start = PtpIpPacket::StartData(ptp_core::StartData {
        transaction_id: tid,
        total_length: payload.len() as u64,
    });
    let end = PtpIpPacket::EndData(ptp_core::DataBlock {
        transaction_id: tid,
        payload: payload.to_vec(),
    });
    write_frame(s, &fuji_framing::encode(&start).unwrap());
    write_frame(s, &fuji_framing::encode(&end).unwrap());
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
