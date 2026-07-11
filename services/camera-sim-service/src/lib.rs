//! `camera-sim-service` — the runnable simulator: PTP/IP listeners plus a local
//! control HTTP API. It is **lease-agnostic** (no NATS, pools, or lease logic —

//! `/shutdown` or `SIGTERM`.
//!
//! Wire convention: the command channel opens with a standard-framed
//! `InitCommandRequest`/`Ack`, then switches to Fuji compressed framing for
//! operations and data phases — matching `parse_v6_ptpip.py`. (Exact init
//! payload bytes are reconciled against capture in the GFX100 II manifest work.)

use std::collections::HashSet;
use std::net::SocketAddr;
use std::sync::Arc;

use camera_config::{parse_hex_code, CameraManifest, LiveViewDeliveryKind, PcssKnock};
use camera_config::{SocketRole, WireFraming};
use camera_media_store::{ByteSource, MediaStore};
use camera_sim::StateOverlay;
use camera_sim::{
    AppliedStateOverlay, Engine, FrameSource, LoopingFrameSource, Phase, Reply, StreamCompletion,
};
use protocol_primitives::{
    fuji_framing, parse_pcss_discovery, parse_pcss_init, pcss_notify_message,
};
use ptp_core::codes::{op, resp};
use ptp_core::{EventPacket, InitCommandAck, InitFail, OperationRequest, PtpCodec, PtpIpPacket};
use tokio::io::{AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream, UdpSocket};
use tokio::sync::broadcast;
use tokio::sync::{Mutex, Notify};

pub mod control;
mod metrics;
mod state_callback;
mod state_json;

pub(crate) use metrics::Metrics;

#[derive(Clone)]
pub struct Config {
    pub instance_id: String,
    pub profile: String,
    /// Manifest connection id to serve. Defaults to `app` at the CLI/wrapper.
    pub connection: String,
    pub manifest_yaml: String,
    pub media_root: std::path::PathBuf,
    /// Optional bind address for the PTP command socket. When absent, the
    /// selected manifest connection's command port is bound on `[::]`.
    pub command_bind: Option<SocketAddr>,
    /// Optional through-picture stream socket bind. Must be absent when the
    /// selected manifest connection has no live-view socket role.
    pub liveview_bind: Option<SocketAddr>,
    /// Optional event socket bind. Must be absent when the selected manifest
    /// connection has no event socket role.
    pub event_bind: Option<SocketAddr>,
    /// Optional PCSS UDP knock listener. Hosted direct-connect instances leave
    /// this unset; LAN-fidelity tests opt in.
    pub knock_bind: Option<SocketAddr>,
    /// Number of PCSS InitFail packets to emit before InitCommandAck.
    pub pcss_init_fails: u32,
    /// When nonzero, standard object queues start empty and enqueue this many
    /// media handles after each manifest shutter action with objectsAvailable.
    pub pcss_shutter_enqueue_count: u32,
    pub control_bind: SocketAddr,
    /// Directory of JPEG frames to loop on the live-view socket (sorted by
    /// filename, gated on Phase::Streaming). None / empty dir => no frames.
    pub liveview_dir: Option<std::path::PathBuf>,
    /// Optional observer URL (#126). When `Some`, the service POSTs a JSON
    /// snapshot of camera state to it on every change (debounced, fire-and-
    /// forget). `http://host[:port][/path]`; invalid URLs are logged and ignored.
    pub state_callback: Option<String>,
}

/// Live-view frame pacing (~30 fps).
const FRAME_INTERVAL_MS: u64 = 33;

/// Read every `.jpg` from a directory (sorted by filename) into memory once at
/// bind. Returning an empty Vec on no-dir / empty-dir is fine; the frame loop
/// then idles cleanly.
fn load_liveview_frames(dir: Option<&std::path::Path>) -> std::io::Result<Vec<Vec<u8>>> {
    let Some(dir) = dir else {
        return Ok(Vec::new());
    };
    let mut paths: Vec<std::path::PathBuf> = std::fs::read_dir(dir)?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| {
            p.extension()
                .and_then(|s| s.to_str())
                .is_some_and(|s| s.eq_ignore_ascii_case("jpg"))
        })
        .collect();
    paths.sort();
    let mut out = Vec::with_capacity(paths.len());
    for p in paths {
        out.push(std::fs::read(&p)?);
    }
    Ok(out)
}

/// A bound, not-yet-serving instance. Binding first lets callers (tests) learn
/// the OS-assigned ports before the serve loop starts.
pub struct Server {
    config: Config,
    command: TcpListener,
    liveview: Option<TcpListener>,
    event: Option<TcpListener>,
    knock: Option<UdpSocket>,
    knock_config: Option<PcssKnock>,
    poll_live_view_op: Option<u16>,
    disallowed_ops: Arc<HashSet<u16>>,
    control: TcpListener,
    engine: Arc<Mutex<Engine>>,
    metrics: Metrics,
    /// Shared looping frame source (one cursor across all live-view clients —
    /// the normal lease shape is one camera = one client; cursor sharing is
    /// harmless if a smoke client connects alongside the real one).
    frames: Arc<Mutex<LoopingFrameSource>>,
}

#[derive(Clone)]
struct CommandContext {
    init_shape: String,
    pcss_init_fails: u32,
    poll_live_view_op: Option<u16>,
    disallowed_ops: Arc<HashSet<u16>>,
}

impl Server {
    pub async fn bind(config: Config) -> std::io::Result<Self> {
        let manifest = CameraManifest::from_yaml(&config.manifest_yaml)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()))?;
        // Refuse to boot with a manifest this build can't faithfully serve —
        // a newer-schema manifest would otherwise misbehave at request time.
        manifest
            .require_supported_schema()
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()))?;
        let mut store = MediaStore::open(&config.media_root)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::NotFound, e.to_string()))?;
        store
            .scan()
            .map_err(|e| std::io::Error::other(e.to_string()))?;
        let connection = manifest
            .connections
            .get(&config.connection)
            .ok_or_else(|| {
                invalid_config(format!(
                    "selected connection '{}' is not present in the manifest",
                    config.connection
                ))
            })?;
        let bindings = connection.bindings.as_ref().ok_or_else(|| {
            invalid_config(format!(
                "selected connection '{}' has no socket bindings",
                config.connection
            ))
        })?;
        if !matches!(connection.command_framing, Some(WireFraming::Compressed)) {
            return Err(invalid_config(format!(
                "selected connection '{}' uses unsupported command framing {:?}",
                config.connection, connection.command_framing
            )));
        }
        if bindings.port_for(SocketRole::Event).is_some()
            && !matches!(connection.event_framing, Some(WireFraming::Usb))
        {
            return Err(invalid_config(format!(
                "selected connection '{}' uses unsupported event framing {:?}",
                config.connection, connection.event_framing
            )));
        }
        if config.pcss_init_fails > 0 {
            let Some(retries) = &connection.init_retries else {
                return Err(invalid_config(format!(
                    "selected connection '{}' has no initRetries but --pcss-init-fails was supplied",
                    config.connection
                )));
            };
            if config.pcss_init_fails > retries.max {
                return Err(invalid_config(format!(
                    "--pcss-init-fails {} exceeds selected connection '{}' max {}",
                    config.pcss_init_fails, config.connection, retries.max
                )));
            }
        }
        let poll_live_view_op = connection
            .live_view_delivery
            .as_ref()
            .filter(|delivery| delivery.kind == LiveViewDeliveryKind::Poll)
            .and_then(|delivery| delivery.poll_op.as_deref())
            .and_then(parse_hex_code);
        let disallowed_ops = Arc::new(
            manifest
                .operations
                .iter()
                .filter_map(|(code, opdef)| {
                    (!opdef.connections.is_empty()
                        && !opdef.connections.iter().any(|c| c == &config.connection))
                    .then(|| parse_hex_code(code))
                    .flatten()
                })
                .collect::<HashSet<_>>(),
        );
        let command_bind = role_bind(
            config.command_bind,
            bindings.port_for(SocketRole::Command),
            "command",
        )?
        .ok_or_else(|| {
            invalid_config(format!(
                "selected connection '{}' has no command socket binding",
                config.connection
            ))
        })?;
        let event_bind = role_bind(
            config.event_bind,
            bindings.port_for(SocketRole::Event),
            "event",
        )?;
        let liveview_bind = role_bind(
            config.liveview_bind,
            bindings.port_for(SocketRole::LiveView),
            "live-view",
        )?;
        let knock_config = if config.knock_bind.is_some() {
            Some(connection.knock.clone().ok_or_else(|| {
                invalid_config(format!(
                    "--knock-bind was supplied, but selected connection '{}' has no PCSS knock block",
                    config.connection
                ))
            })?)
        } else {
            None
        };
        let mut engine_value = Engine::new(manifest, store);
        engine_value.bind_connection(&config.connection);
        engine_value
            .configure_standard_object_queue(&config.connection, config.pcss_shutter_enqueue_count)
            .map_err(invalid_config)?;
        let engine = Arc::new(Mutex::new(engine_value));
        let frame_bytes = load_liveview_frames(config.liveview_dir.as_deref())?;
        let frame_count = frame_bytes.len();
        let frames = Arc::new(Mutex::new(LoopingFrameSource::new(frame_bytes)));
        tracing::info!(frame_count, "live-view frame source loaded");
        let command = TcpListener::bind(command_bind).await?;
        let liveview = match liveview_bind {
            Some(addr) => Some(TcpListener::bind(addr).await?),
            None => None,
        };
        let event = match event_bind {
            Some(addr) => Some(TcpListener::bind(addr).await?),
            None => None,
        };
        let knock = match config.knock_bind {
            Some(addr) => Some(UdpSocket::bind(addr).await?),
            None => None,
        };
        let control = TcpListener::bind(config.control_bind).await?;
        let metrics = Metrics::default();
        Ok(Server {
            config,
            command,
            liveview,
            event,
            knock,
            knock_config,
            poll_live_view_op,
            disallowed_ops,
            control,
            engine,
            metrics,
            frames,
        })
    }

    pub fn command_addr(&self) -> SocketAddr {
        self.command.local_addr().unwrap()
    }

    pub fn liveview_addr_opt(&self) -> Option<SocketAddr> {
        self.liveview.as_ref().map(|l| l.local_addr().unwrap())
    }

    pub fn event_addr_opt(&self) -> Option<SocketAddr> {
        self.event.as_ref().map(|l| l.local_addr().unwrap())
    }

    pub fn knock_addr_opt(&self) -> Option<SocketAddr> {
        self.knock.as_ref().map(|l| l.local_addr().unwrap())
    }

    pub fn control_addr(&self) -> SocketAddr {
        self.control.local_addr().unwrap()
    }

    /// This server's arming link (#102), to hand to a BLE responder so a modeled
    /// AP handoff arms (or fails to arm) the engine that answers `InitCommandRequest`.
    pub async fn camera_link(&self) -> camera_sim::SharedLink {
        self.engine.lock().await.link()
    }

    /// Apply a boot-time state overlay before `run()`. This uses the same engine
    /// mutation path as the control API so startup files and live controls cannot
    /// drift.
    pub async fn apply_startup_state(
        &self,
        overlay: &StateOverlay,
    ) -> Result<AppliedStateOverlay, String> {
        overlay.validate_context(&self.config.profile, &self.config.connection)?;
        self.engine.lock().await.apply_state_overlay(overlay)
    }

    /// Serve until `shutdown` resolves (the `/shutdown` endpoint or a SIGTERM
    /// handler fires it). In-flight command connections are dropped on exit.
    pub async fn run(self, shutdown: tokio::sync::oneshot::Receiver<()>) {
        let Server {
            config,
            command,
            liveview,
            event,
            knock,
            knock_config,
            poll_live_view_op,
            disallowed_ops,
            control,
            engine,
            metrics,
            frames,
        } = self;
        let health = control::Health {
            instance_id: config.instance_id.clone(),
            profile: config.profile.clone(),
            connection: config.connection.clone(),
            command_addr: command.local_addr().unwrap(),
            media_root: config.media_root.display().to_string(),
            metrics: metrics.clone(),
        };
        let command_context = CommandContext {
            init_shape: {
                let e = engine.lock().await;
                e.manifest()
                    .connections
                    .get(&config.connection)
                    .and_then(|c| c.init_shape.as_deref())
                    .unwrap_or("app82")
                    .to_string()
            },
            pcss_init_fails: config.pcss_init_fails,
            poll_live_view_op,
            disallowed_ops,
        };
        let (shutdown_tx, _) = tokio::sync::broadcast::channel::<()>(1);
        let ctl_shutdown = shutdown_tx.clone();
        // Completion/lifecycle event push (#54): the command loop drains the
        // engine's queued `emits` after each operation and broadcasts the codes;
        // each connected event-socket client writes them as PTP/IP Event packets.
        // Fire-and-forget to currently-connected clients (broadcast) — the real
        // app opens the event socket during session setup, before triggering a
        // capture, so the subscriber is live when the completion fires.
        let (event_tx, _) = broadcast::channel::<u16>(16);

        // #126/#214 state-callback: one shared notify the command loop bumps
        // after every op. Cheap when nobody listens; the single push task
        // consumes it, debounces, and POSTs to startup and runtime subscribers.
        let state_dirty = Arc::new(Notify::new());
        let state_callbacks = state_callback::Registry::default();

        // Per-connection tasks live in a JoinSet per accept loop (audit in
        // docs/internal-async-notes.md): the `Some(_) = join_next()` arm reaps
        // finished tasks on a long-lived server, and JoinSet aborts everything
        // still running when dropped — which happens when run()'s select!
        // drops the loop future on shutdown. In-flight connections being cut
        // on exit is the documented run() contract.
        let command_port = command.local_addr().unwrap().port();
        let command_loop = {
            let engine = engine.clone();
            let frames = frames.clone();
            let event_tx = event_tx.clone();
            let state_dirty = state_dirty.clone();
            let command_context = command_context.clone();
            let metrics = metrics.clone();
            let mut sub = shutdown_tx.subscribe();
            async move {
                let mut conns = tokio::task::JoinSet::new();
                loop {
                    tokio::select! {
                        accepted = command.accept() => {
                            if let Ok((stream, _)) = accepted {
                                let engine = engine.clone();
                                let frames = frames.clone();
                                let event_tx = event_tx.clone();
                                let state_dirty = state_dirty.clone();
                                let command_context = command_context.clone();
                                let metrics = metrics.clone();
                                conns.spawn(async move {
                                    let _ = handle_command_conn(stream, engine, frames, event_tx, state_dirty, command_context, metrics).await;
                                });
                            }
                        }
                        Some(_) = conns.join_next(), if !conns.is_empty() => {}
                        _ = sub.recv() => break,
                    }
                }
            }
        };

        let control_loop = {
            let engine = engine.clone();
            let state_dirty = state_dirty.clone();
            let state_callbacks = state_callbacks.clone();
            let metrics = metrics.clone();
            let mut sub = shutdown_tx.subscribe();
            async move {
                let mut conns = tokio::task::JoinSet::new();
                loop {
                    tokio::select! {
                        accepted = control.accept() => {
                            if let Ok((stream, _)) = accepted {
                                // Per-connection task — a client that connects and
                                // stalls before its request line must not block the
                                // accept loop (and with it /healthz + /shutdown).
                                conns.spawn(control::handle(
                                    stream,
                                    health.clone(),
                                    engine.clone(),
                                    state_dirty.clone(),
                                    state_callbacks.clone(),
                                    metrics.clone(),
                                    ctl_shutdown.clone(),
                                ));
                            }
                        }
                        Some(_) = conns.join_next(), if !conns.is_empty() => {}
                        _ = sub.recv() => break,
                    }
                }
            }
        };

        // Live-view: emit length-prefixed JPEG frames at ~30 fps, BUT only while
        // the engine is in Phase::Streaming (after InitiateOpenCapture). Connection
        // stays open and idle when not streaming — matches a real camera.
        let liveview_loop = {
            let engine = engine.clone();
            let frames = frames.clone();
            let shutdown_tx = shutdown_tx.clone();
            let metrics = metrics.clone();
            async move {
                let mut sub = shutdown_tx.subscribe();
                let Some(liveview) = liveview else {
                    let _ = sub.recv().await;
                    return;
                };
                let mut conns = tokio::task::JoinSet::new();
                loop {
                    tokio::select! {
                        accepted = liveview.accept() => {
                            if let Ok((stream, _)) = accepted {
                                let engine = engine.clone();
                                let frames = frames.clone();
                                let metrics = metrics.clone();
                                conns.spawn(stream_liveview(stream, engine, frames, metrics));
                            }
                        }
                        Some(_) = conns.join_next(), if !conns.is_empty() => {}
                        _ = sub.recv() => break,
                    }
                }
            }
        };

        // Event socket: push PTP/IP Event packets to connected clients. Each
        // accepted connection subscribes to the broadcast (resubscribed in the
        // accept arm so it only sees events emitted after it connected) and
        // writes the codes the command loop forwards.
        let event_loop = {
            let shutdown_tx = shutdown_tx.clone();
            let metrics = metrics.clone();
            async move {
                let mut sub = shutdown_tx.subscribe();
                let Some(event) = event else {
                    let _ = sub.recv().await;
                    return;
                };
                let mut conns = tokio::task::JoinSet::new();
                loop {
                    tokio::select! {
                        accepted = event.accept() => {
                            if let Ok((stream, _)) = accepted {
                                conns.spawn(handle_event_conn(stream, event_tx.subscribe(), metrics.clone()));
                            }
                        }
                        Some(_) = conns.join_next(), if !conns.is_empty() => {}
                        _ = sub.recv() => break,
                    }
                }
            }
        };

        let knock_loop = {
            let camera_name = {
                let e = engine.lock().await;
                e.manifest().camera.model.clone()
            };
            let mut sub = shutdown_tx.subscribe();
            async move {
                let Some(knock) = knock else {
                    let _ = sub.recv().await;
                    return;
                };
                let Some(knock_config) = knock_config else {
                    let _ = sub.recv().await;
                    return;
                };
                run_knock_loop(knock, knock_config, camera_name, command_port, sub).await;
            }
        };

        // #126/#214: spawn one state-callback push task. Startup callbacks are
        // registered up front; runtime callbacks join via POST /callbacks.
        tokio::spawn(state_callback::state_callback_loop(
            state_dirty.clone(),
            engine.clone(),
            state_callbacks.clone(),
            metrics.clone(),
            shutdown_tx.subscribe(),
        ));
        if let Some(url) = config.state_callback.as_deref() {
            if state_callbacks.add_url(url).await.is_err() {
                tracing::warn!(url, "invalid --state-callback URL; state callback disabled")
            }
        }
        state_dirty.notify_one();

        tokio::select! {
            _ = shutdown => {}
            _ = command_loop => {}
            _ = control_loop => {}
            _ = liveview_loop => {}
            _ = event_loop => {}
            _ = knock_loop => {}
        }
        let _ = shutdown_tx.send(());
    }
}

/// Stream live-view frames to one connected client until it disconnects or the
/// write fails. Each tick: if the engine is in Phase::Streaming, pull the next
/// frame from the shared LoopingFrameSource and write [u32 len | JPEG] via the
/// shared framing primitive. Otherwise idle (the connection stays open but no
/// bytes flow — matching a real camera between OpenCapture cycles).
///
/// The read half is watched concurrently: liveview clients never send bytes,
/// so a completed read means EOF/reset — the client is gone. Without this, a
/// client that disconnects while the engine is NOT streaming would leave the
/// task ticking forever (the write that would surface the error never runs).
async fn stream_liveview(
    mut stream: TcpStream,
    engine: Arc<Mutex<Engine>>,
    frames: Arc<Mutex<LoopingFrameSource>>,
    metrics: Metrics,
) {
    let (mut rd, mut wr) = stream.split();
    let mut tick = tokio::time::interval(std::time::Duration::from_millis(FRAME_INTERVAL_MS));
    let mut probe = [0u8; 64];
    let mut frame_index = 0u32;
    loop {
        tokio::select! {
            _ = tick.tick() => {
                if !matches!(engine.lock().await.phase(), Phase::Streaming) {
                    continue;
                }
                // Lock → pull frame → guard drops at end of statement; the
                // network write below never executes under either mutex.
                let Some(jpeg) = frames.lock().await.next_frame() else {
                    continue;
                };
                let packet = protocol_primitives::liveview::frame_packet(&jpeg, frame_index);
                frame_index = frame_index.wrapping_add(1);
                if wr.write_all(&packet).await.is_err() {
                    break;
                }
                metrics.record_liveview_frame(packet.len());
            }
            r = rd.read(&mut probe) => {
                match r {
                    Ok(0) | Err(_) => break, // EOF / reset — client gone
                    Ok(n) => metrics.record_read(n), // unexpected bytes; ignore
                }
            }
        }
    }
}

/// Read one length-prefixed PTP/IP frame (`u32` total length including itself).
async fn read_frame(stream: &mut TcpStream, metrics: &Metrics) -> std::io::Result<Option<Vec<u8>>> {
    let mut len_buf = [0u8; 4];
    match stream.read_exact(&mut len_buf).await {
        Ok(_) => metrics.record_read(4),
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let len = u32::from_le_bytes(len_buf) as usize;
    if !(8..=64 * 1024 * 1024).contains(&len) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "implausible frame length",
        ));
    }
    let mut buf = vec![0u8; len];
    buf[0..4].copy_from_slice(&len_buf);
    stream.read_exact(&mut buf[4..]).await?;
    metrics.record_read(len.saturating_sub(4));
    metrics.touch();
    Ok(Some(buf))
}

/// Ops that carry an initiator (host→camera) data phase we must collect before
/// dispatching. Kept minimal — the flows we drive use SetDevicePropValue.
fn has_data_in(code: u16) -> bool {
    code == op::SET_DEVICE_PROP_VALUE
}

async fn handle_command_conn(
    mut stream: TcpStream,
    engine: Arc<Mutex<Engine>>,
    frames: Arc<Mutex<LoopingFrameSource>>,
    event_tx: broadcast::Sender<u16>,
    state_dirty: Arc<Notify>,
    context: CommandContext,
    metrics: Metrics,
) -> std::io::Result<()> {
    // 1. Standard-framed init handshake.
    let Some(mut first) = read_frame(&mut stream, &metrics).await? else {
        return Ok(());
    };
    match context.init_shape.as_str() {
        "pcssKnock" => {
            for _ in 0..context.pcss_init_fails {
                if parse_pcss_init(&first).is_err() {
                    return Ok(());
                }
                let fail = PtpIpPacket::InitFail(InitFail {
                    reason: resp::DEVICE_BUSY as u32,
                });
                let bytes = ptp_core::encode(&fail).map_err(to_io)?;
                stream.write_all(&bytes).await?;
                metrics.record_write(bytes.len());
                let Some(next) = read_frame(&mut stream, &metrics).await? else {
                    return Ok(());
                };
                first = next;
            }
            if parse_pcss_init(&first).is_err() {
                return Ok(());
            }
        }
        _ => {
            let Ok(PtpIpPacket::InitCommandRequest(init_req)) = PtpIpPacket::decode(&first) else {
                return Ok(()); // not a PTP/IP initiator
            };
            // The camera drops InitCommandRequest when a BLE AP handoff launched without
            // the IMAGE_TRANSFER_SETTING arming prep write (#102): no ack, just hang up.
            if !engine.lock().await.accepts_init() {
                return Ok(());
            }
            // If a device name was registered over BLE during pairing, the PTP/IP friendly
            // name MUST match it — the camera silently drops a mismatch (#109): no ack.
            // Ungated when no name was registered (standalone init), so smoke paths pass.
            let registered_name = engine.lock().await.link().device_name();
            if let Some(registered) = registered_name {
                if registered != init_req.friendly_name {
                    return Ok(());
                }
            }
        }
    }
    let ack = PtpIpPacket::InitCommandAck(InitCommandAck {
        connection_number: 1,
        responder_guid: [0; 16],
        friendly_name: {
            let e = engine.lock().await;
            e.manifest().camera.model.clone()
        },
        protocol_version: 0x0001_0000,
    });
    let ack_bytes = ptp_core::encode(&ack).map_err(to_io)?;
    stream.write_all(&ack_bytes).await?;
    metrics.record_write(ack_bytes.len());

    // 2. Compressed-framed operation loop.
    loop {
        let Some(frame) = read_frame(&mut stream, &metrics).await? else {
            break;
        };
        let Ok(PtpIpPacket::OperationRequest(req)) = fuji_framing::decode(&frame) else {
            continue;
        };
        if context.disallowed_ops.contains(&req.code) {
            write_reply(
                &mut stream,
                &req,
                Reply::Response(ptp_core::OperationResponse {
                    code: resp::OPERATION_NOT_SUPPORTED,
                    transaction_id: req.transaction_id,
                    params: vec![],
                }),
                &metrics,
            )
            .await?;
            continue;
        }
        let data_in = if has_data_in(req.code) {
            collect_data_in(&mut stream, req.transaction_id, &metrics).await?
        } else {
            None
        };
        let (reply, events, is_poll_live_view) = {
            let mut e = engine.lock().await;
            let is_poll_live_view = context.poll_live_view_op == Some(req.code);
            let reply = e.on_operation(&req, data_in.as_deref());
            // Drain under the same lock so the queue is emptied atomically with
            // the op that produced it; forward (broadcast, non-blocking) outside.
            (reply, e.drain_events(), is_poll_live_view)
        };
        let reply = if is_poll_live_view {
            poll_live_view_reply(reply, &frames).await
        } else {
            reply
        };
        // #126: the one hook — nudge the state-callback task that the camera
        // state may have changed. Cheap; a no-op when no push task is running.
        state_dirty.notify_one();
        for code in events {
            // Err = no event-socket client connected; the push is dropped (the
            // completion is only meaningful to a listening client).
            let _ = event_tx.send(code);
        }
        if let Some(completion) = write_reply(&mut stream, &req, reply, &metrics).await? {
            if engine.lock().await.complete_stream(completion) {
                state_dirty.notify_one();
            }
        }
    }
    Ok(())
}

async fn run_knock_loop(
    knock: UdpSocket,
    knock_config: PcssKnock,
    camera_name: String,
    command_port: u16,
    mut shutdown: broadcast::Receiver<()>,
) {
    let mut buf = vec![0u8; 2048];
    loop {
        tokio::select! {
            received = knock.recv_from(&mut buf) => {
                let Ok((n, _peer)) = received else {
                    continue;
                };
                let Some(discovery) = parse_pcss_discovery(&buf[..n], &knock_config.protocol) else {
                    continue;
                };
                let callback = format!("{}:{}", discovery.host, knock_config.callback_port);
                let camera_name = camera_name.clone();
                let protocol = knock_config.protocol.clone();
                tokio::spawn(async move {
                    if let Ok(mut callback) = TcpStream::connect(callback).await {
                        let notify = pcss_notify_message(&camera_name, command_port, &protocol);
                        let _ = callback.write_all(&notify).await;
                        let mut ack = [0u8; 256];
                        let _ = callback.read(&mut ack).await;
                    }
                });
            }
            _ = shutdown.recv() => break,
        }
    }
}

async fn poll_live_view_reply(reply: Reply, frames: &Arc<Mutex<LoopingFrameSource>>) -> Reply {
    let Reply::Response(response) = reply else {
        return reply;
    };
    if response.code != resp::OK {
        return Reply::Response(response);
    }
    let Some(data) = frames.lock().await.next_frame() else {
        return Reply::Response(response);
    };
    Reply::Data { data, response }
}

/// Push completion/lifecycle events to one connected event-socket client until
/// it disconnects (#54). Each broadcast code is written as a standard-framed
/// PTP/IP `Event` packet. Mirrors [`stream_liveview`]'s read-half watcher: event
/// clients never send bytes, so a completed read means EOF/reset — the client is
/// gone, and without watching it a never-emitting session would hang the task.
async fn handle_event_conn(
    mut stream: TcpStream,
    mut events: broadcast::Receiver<u16>,
    metrics: Metrics,
) {
    let (mut rd, mut wr) = stream.split();
    let mut probe = [0u8; 64];
    loop {
        tokio::select! {
            recv = events.recv() => {
                match recv {
                    Ok(code) => {
                        let packet = PtpIpPacket::Event(EventPacket {
                            code,
                            transaction_id: 0,
                            params: vec![],
                        });
                        let Ok(bytes) = ptp_core::encode(&packet) else { break };
                        if wr.write_all(&bytes).await.is_err() {
                            break; // client gone
                        }
                        metrics.record_write(bytes.len());
                        metrics.touch();
                    }
                    // Closed: the server is shutting down. Lagged: this slow
                    // client overflowed the buffer — drop it rather than send
                    // out-of-order completions.
                    Err(_) => break,
                }
            }
            r = rd.read(&mut probe) => {
                match r {
                    Ok(0) | Err(_) => break, // EOF / reset — client gone
                    Ok(n) => metrics.record_read(n), // unexpected bytes; ignore
                }
            }
        }
    }
}

/// Absolute ceiling on an initiator data-in payload. Realistic data-in is a
/// single property value (≤ 8 bytes for scalars, short strings) so 1 MiB is
/// generous. A larger declared `total_length` is rejected outright rather than
/// trusted, so a client cannot use the data-in channel for unbounded growth.
const MAX_DATA_IN_BYTES: u64 = 1024 * 1024;

fn data_in_err<S: Into<String>>(msg: S) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidData, msg.into())
}

/// Collect the data-in payload. The compressed channel carries a whole data
/// phase in a single type-2 `Data` frame, so we read exactly one frame, require
/// it to be a `Data` for this `tid`, and cap the payload at [`MAX_DATA_IN_BYTES`]
/// (realistic data-in is a single property value). A transaction-id mismatch or
/// any other packet type is a protocol error.
async fn collect_data_in(
    stream: &mut TcpStream,
    tid: u32,
    metrics: &Metrics,
) -> std::io::Result<Option<Vec<u8>>> {
    let Some(frame) = read_frame(stream, metrics).await? else {
        return Err(data_in_err("data-in: stream closed before the data frame"));
    };
    match fuji_framing::decode(&frame) {
        Ok(PtpIpPacket::Data(d)) => {
            if d.transaction_id != tid {
                return Err(data_in_err(format!(
                    "data-in: Data tid {} != request tid {}",
                    d.transaction_id, tid
                )));
            }
            if d.payload.len() as u64 > MAX_DATA_IN_BYTES {
                return Err(data_in_err(format!(
                    "data-in: payload {} exceeds cap {MAX_DATA_IN_BYTES}",
                    d.payload.len()
                )));
            }
            Ok(Some(d.payload))
        }
        Ok(other) => Err(data_in_err(format!(
            "data-in: expected a data frame, got {other:?}"
        ))),
        Err(e) => Err(data_in_err(format!("data-in: decode failed: {e}"))),
    }
}

/// How much of a data-phase body to read from the source per socket write. The
/// data phase is a single wire frame, but the body is streamed in 1 MiB reads so
/// peak in-process allocation stays bounded per DESIGN.md ("File downloads use
/// bounded chunk buffers") even for a multi-GB object.
const DATA_CHUNK_BYTES: usize = 1024 * 1024;

async fn write_reply<W: AsyncWrite + Unpin>(
    stream: &mut W,
    req: &OperationRequest,
    reply: Reply,
    metrics: &Metrics,
) -> std::io::Result<Option<StreamCompletion>> {
    match reply {
        Reply::Response(resp) => {
            let bytes =
                fuji_framing::encode(&PtpIpPacket::OperationResponse(resp)).map_err(to_io)?;
            stream.write_all(&bytes).await?;
            metrics.record_write(bytes.len());
        }
        Reply::Data { data, response } => {
            // The whole data phase is one type-2 frame whose code echoes the op.
            let data_bytes = fuji_framing::encode_data(req.code, req.transaction_id, &data);
            stream.write_all(&data_bytes).await?;
            metrics.record_write(data_bytes.len());
            let response_bytes =
                fuji_framing::encode(&PtpIpPacket::OperationResponse(response)).map_err(to_io)?;
            stream.write_all(&response_bytes).await?;
            metrics.record_write(response_bytes.len());
        }
        Reply::DataStream {
            source,
            response,
            completion,
        } => {
            stream_data_phase(stream, req.code, req.transaction_id, &source, metrics).await?;
            let response_bytes =
                fuji_framing::encode(&PtpIpPacket::OperationResponse(response)).map_err(to_io)?;
            stream.write_all(&response_bytes).await?;
            metrics.record_write(response_bytes.len());
            return Ok(completion);
        }
        Reply::NoResponse => {}
        Reply::Close => {}
    }
    Ok(None)
}

/// Emit the data phase as a single type-2 `Data` frame — the whole payload
/// arrives in one length-prefixed frame on the wire, matching real Fuji (a
/// 14.5 MB `GetObject` is one frame). The 12-byte header goes out first with the
/// total length; the body is then streamed from `source` one `DATA_CHUNK_BYTES`
/// read at a time, so peak in-process allocation stays bounded regardless of
/// `source.len()` even though the client sees a single frame. `op` is echoed in
/// the frame's code field.
async fn stream_data_phase<W: AsyncWrite + Unpin>(
    stream: &mut W,
    op: u16,
    transaction_id: u32,
    source: &ByteSource,
    metrics: &Metrics,
) -> std::io::Result<()> {
    let total_length = source.len();
    let payload_len = u32::try_from(total_length)
        .map_err(|_| std::io::Error::other("data-phase payload exceeds a single frame (u32)"))?;
    let header = fuji_framing::data_frame_header(op, transaction_id, payload_len);
    stream.write_all(&header).await?;
    metrics.record_write(header.len());

    let mut offset: u64 = 0;
    while offset < total_length {
        let take = ((total_length - offset) as usize).min(DATA_CHUNK_BYTES);
        let chunk = source
            .read_chunk(offset, take)
            .map_err(|e| std::io::Error::other(e.to_string()))?;
        if chunk.is_empty() {
            break;
        }
        offset += chunk.len() as u64;
        stream.write_all(&chunk).await?;
        metrics.record_write(chunk.len());
    }
    if offset != total_length {
        return Err(std::io::Error::new(
            std::io::ErrorKind::UnexpectedEof,
            "stream source ended before its declared length",
        ));
    }
    Ok(())
}

fn to_io<E: std::fmt::Display>(e: E) -> std::io::Error {
    std::io::Error::other(e.to_string())
}

fn invalid_config<S: Into<String>>(msg: S) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidInput, msg.into())
}

fn role_bind(
    override_addr: Option<SocketAddr>,
    manifest_port: Option<u16>,
    role: &str,
) -> std::io::Result<Option<SocketAddr>> {
    match (override_addr, manifest_port) {
        (Some(addr), Some(_)) => Ok(Some(addr)),
        (Some(_), None) => Err(invalid_config(format!(
            "--{role}-bind was supplied, but the selected connection has no {role} socket"
        ))),
        (None, Some(port)) => Ok(Some(format!("[::]:{port}").parse().map_err(|e| {
            invalid_config(format!("invalid manifest {role} port {port}: {e}"))
        })?)),
        (None, None) => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use std::pin::Pin;
    use std::task::{Context, Poll};

    use super::*;

    struct FailAfter {
        remaining: usize,
    }

    impl AsyncWrite for FailAfter {
        fn poll_write(
            mut self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<std::io::Result<usize>> {
            if self.remaining == 0 {
                return Poll::Ready(Err(std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    "injected writer failure",
                )));
            }
            let written = self.remaining.min(buf.len());
            self.remaining -= written;
            Poll::Ready(Ok(written))
        }

        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }

        fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    #[tokio::test]
    async fn failed_stream_write_cannot_return_a_completion() {
        let req = OperationRequest {
            data_phase_info: 1,
            code: op::GET_PARTIAL_OBJECT,
            transaction_id: 7,
            params: vec![1, 0, 32],
        };
        let reply = Reply::DataStream {
            source: ByteSource::Memory(vec![0x5a; 32]),
            response: ptp_core::OperationResponse {
                code: resp::OK,
                transaction_id: 7,
                params: vec![32],
            },
            completion: None,
        };
        let mut writer = FailAfter { remaining: 14 };
        let result = write_reply(&mut writer, &req, reply, &Metrics::default()).await;
        assert_eq!(result.unwrap_err().kind(), std::io::ErrorKind::BrokenPipe);
    }
}
