use std::io::{self, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use camera_initiator::{NativePtpTransport, TraceFormat, TraceWriter, TransportConfig};
use camera_protocol_ffi::{
    run_action, run_mode_entry, ActionVerb, ConfigStore, ConnectionActivityEvent,
    ConnectionActivityObserver, PtpExecutorTransport, StepObserver, StepReport,
};
use camera_sim_service::{Config, Server};
#[cfg(target_os = "linux")]
use if_addrs::{get_if_addrs, IfAddr};
#[cfg(target_os = "linux")]
use tokio::io::{AsyncReadExt, AsyncWriteExt};

struct NoopObserver;

impl StepObserver for NoopObserver {
    fn on_step(&self, _report: StepReport) {}
}

impl ConnectionActivityObserver for NoopObserver {
    fn on_activity(&self, _event: ConnectionActivityEvent) {}
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
        trace,
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
            connect_timeout: Duration::from_secs(2),
            max_frame_bytes: 1024 * 1024,
        },
        trace,
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
    let outcome = run_action(
        Arc::clone(&store),
        "wireless-tether".into(),
        ActionVerb::ReadDeviceInfo,
        raw_transport,
        Arc::new(NoopObserver),
        Arc::new(NoopObserver),
        Vec::new(),
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
