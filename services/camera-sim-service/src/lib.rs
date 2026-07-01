//! `camera-sim-service` — the runnable simulator: PTP/IP listeners plus a local
//! control HTTP API. It is **lease-agnostic** (no NATS, pools, or lease logic —

//! `/shutdown` or `SIGTERM`.
//!
//! Wire convention: the command channel opens with a standard-framed
//! `InitCommandRequest`/`Ack`, then switches to Fuji compressed framing for
//! operations and data phases — matching `parse_v6_ptpip.py`. (Exact init
//! payload bytes are reconciled against capture in the GFX100 II manifest work.)

use std::net::SocketAddr;
use std::sync::Arc;

use camera_config::CameraManifest;
use camera_media_store::{ByteSource, MediaStore};
use camera_sim::{Engine, FrameSource, LoopingFrameSource, Phase, Reply};
use protocol_primitives::fuji_framing;
use ptp_core::codes::op;
use ptp_core::{EventPacket, InitCommandAck, OperationRequest, PtpCodec, PtpIpPacket};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::broadcast;
use tokio::sync::{Mutex, Notify};

pub mod control;
mod state_callback;

#[derive(Clone)]
pub struct Config {
    pub instance_id: String,
    pub profile: String,
    pub manifest_yaml: String,
    pub media_root: std::path::PathBuf,
    /// Bind address for the PTP command socket. Use port 0 for an OS-assigned
    /// port (tests).
    pub command_bind: SocketAddr,
    /// Through-picture (live-view) stream socket. Per the shipping app this is
    /// command+2 = 55742.
    pub liveview_bind: SocketAddr,
    /// Async event socket (command+1 = 55741).
    pub event_bind: SocketAddr,
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
    liveview: TcpListener,
    event: TcpListener,
    control: TcpListener,
    engine: Arc<Mutex<Engine>>,
    /// Shared looping frame source (one cursor across all live-view clients —
    /// the normal lease shape is one camera = one client; cursor sharing is
    /// harmless if a smoke client connects alongside the real one).
    frames: Arc<Mutex<LoopingFrameSource>>,
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
        let engine = Arc::new(Mutex::new(Engine::new(manifest, store)));
        let frame_bytes = load_liveview_frames(config.liveview_dir.as_deref())?;
        let frame_count = frame_bytes.len();
        let frames = Arc::new(Mutex::new(LoopingFrameSource::new(frame_bytes)));
        tracing::info!(frame_count, "live-view frame source loaded");
        let command = TcpListener::bind(config.command_bind).await?;
        let liveview = TcpListener::bind(config.liveview_bind).await?;
        let event = TcpListener::bind(config.event_bind).await?;
        let control = TcpListener::bind(config.control_bind).await?;
        Ok(Server {
            config,
            command,
            liveview,
            event,
            control,
            engine,
            frames,
        })
    }

    pub fn command_addr(&self) -> SocketAddr {
        self.command.local_addr().unwrap()
    }

    pub fn liveview_addr(&self) -> SocketAddr {
        self.liveview.local_addr().unwrap()
    }

    pub fn event_addr(&self) -> SocketAddr {
        self.event.local_addr().unwrap()
    }

    pub fn control_addr(&self) -> SocketAddr {
        self.control.local_addr().unwrap()
    }

    /// This server's arming link (#102), to hand to a BLE responder so a modeled
    /// AP handoff arms (or fails to arm) the engine that answers `InitCommandRequest`.
    pub async fn camera_link(&self) -> camera_sim::SharedLink {
        self.engine.lock().await.link()
    }

    /// Serve until `shutdown` resolves (the `/shutdown` endpoint or a SIGTERM
    /// handler fires it). In-flight command connections are dropped on exit.
    pub async fn run(self, shutdown: tokio::sync::oneshot::Receiver<()>) {
        let Server {
            config,
            command,
            liveview,
            event,
            control,
            engine,
            frames,
        } = self;
        let health = control::Health {
            instance_id: config.instance_id.clone(),
            profile: config.profile.clone(),
            command_addr: command.local_addr().unwrap(),
            media_root: config.media_root.display().to_string(),
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

        // #126 state-callback: one shared notify the command loop bumps after
        // every op. Cheap when nobody listens; the single push task (spawned
        // below only if --state-callback parses) consumes it, debounces, and POSTs.
        let state_dirty = Arc::new(Notify::new());

        // Per-connection tasks live in a JoinSet per accept loop (audit in
        // docs/internal-async-notes.md): the `Some(_) = join_next()` arm reaps
        // finished tasks on a long-lived server, and JoinSet aborts everything
        // still running when dropped — which happens when run()'s select!
        // drops the loop future on shutdown. In-flight connections being cut
        // on exit is the documented run() contract.
        let command_loop = {
            let engine = engine.clone();
            let event_tx = event_tx.clone();
            let state_dirty = state_dirty.clone();
            let mut sub = shutdown_tx.subscribe();
            async move {
                let mut conns = tokio::task::JoinSet::new();
                loop {
                    tokio::select! {
                        accepted = command.accept() => {
                            if let Ok((stream, _)) = accepted {
                                let engine = engine.clone();
                                let event_tx = event_tx.clone();
                                let state_dirty = state_dirty.clone();
                                conns.spawn(async move {
                                    let _ = handle_command_conn(stream, engine, event_tx, state_dirty).await;
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
            let mut sub = shutdown_tx.subscribe();
            async move {
                let mut conns = tokio::task::JoinSet::new();
                loop {
                    tokio::select! {
                        accepted = liveview.accept() => {
                            if let Ok((stream, _)) = accepted {
                                let engine = engine.clone();
                                let frames = frames.clone();
                                conns.spawn(stream_liveview(stream, engine, frames));
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
            let mut sub = shutdown_tx.subscribe();
            async move {
                let mut conns = tokio::task::JoinSet::new();
                loop {
                    tokio::select! {
                        accepted = event.accept() => {
                            if let Ok((stream, _)) = accepted {
                                conns.spawn(handle_event_conn(stream, event_tx.subscribe()));
                            }
                        }
                        Some(_) = conns.join_next(), if !conns.is_empty() => {}
                        _ = sub.recv() => break,
                    }
                }
            }
        };

        // #126: spawn the single state-callback push task if a valid URL was
        // given. Parsed up front; an invalid URL is logged once and ignored
        // (never fatal). It ends on the shutdown broadcast like the loops.
        if let Some(url) = config.state_callback.as_deref() {
            match state_callback::parse_url(url) {
                Some(target) => {
                    tokio::spawn(state_callback::state_callback_loop(
                        state_dirty.clone(),
                        engine.clone(),
                        target,
                        shutdown_tx.subscribe(),
                    ));
                }
                None => {
                    tracing::warn!(url, "invalid --state-callback URL; state callback disabled")
                }
            }
        }

        tokio::select! {
            _ = shutdown => {}
            _ = command_loop => {}
            _ = control_loop => {}
            _ = liveview_loop => {}
            _ = event_loop => {}
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
) {
    let (mut rd, mut wr) = stream.split();
    let mut tick = tokio::time::interval(std::time::Duration::from_millis(FRAME_INTERVAL_MS));
    let mut probe = [0u8; 64];
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
                let packet = protocol_primitives::liveview::frame_packet(&jpeg);
                if wr.write_all(&packet).await.is_err() {
                    break;
                }
            }
            r = rd.read(&mut probe) => {
                match r {
                    Ok(0) | Err(_) => break, // EOF / reset — client gone
                    Ok(_) => {}              // unexpected bytes; ignore
                }
            }
        }
    }
}

/// Read one length-prefixed PTP/IP frame (`u32` total length including itself).
async fn read_frame(stream: &mut TcpStream) -> std::io::Result<Option<Vec<u8>>> {
    let mut len_buf = [0u8; 4];
    match stream.read_exact(&mut len_buf).await {
        Ok(_) => {}
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
    event_tx: broadcast::Sender<u16>,
    state_dirty: Arc<Notify>,
) -> std::io::Result<()> {
    // 1. Standard-framed init handshake.
    let Some(first) = read_frame(&mut stream).await? else {
        return Ok(());
    };
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
    let ack = PtpIpPacket::InitCommandAck(InitCommandAck {
        connection_number: 1,
        responder_guid: [0; 16],
        friendly_name: {
            let e = engine.lock().await;
            e.manifest().camera.model.clone()
        },
        protocol_version: 0x0001_0000,
    });
    stream
        .write_all(&ptp_core::encode(&ack).map_err(to_io)?)
        .await?;

    // 2. Compressed-framed operation loop.
    loop {
        let Some(frame) = read_frame(&mut stream).await? else {
            break;
        };
        let Ok(PtpIpPacket::OperationRequest(req)) = fuji_framing::decode(&frame) else {
            continue;
        };
        let data_in = if has_data_in(req.code) {
            collect_data_in(&mut stream, req.transaction_id).await?
        } else {
            None
        };
        let (reply, events) = {
            let mut e = engine.lock().await;
            let reply = e.on_operation(&req, data_in.as_deref());
            // Drain under the same lock so the queue is emptied atomically with
            // the op that produced it; forward (broadcast, non-blocking) outside.
            (reply, e.drain_events())
        };
        // #126: the one hook — nudge the state-callback task that the camera
        // state may have changed. Cheap; a no-op when no push task is running.
        state_dirty.notify_one();
        for code in events {
            // Err = no event-socket client connected; the push is dropped (the
            // completion is only meaningful to a listening client).
            let _ = event_tx.send(code);
        }
        write_reply(&mut stream, &req, reply).await?;
    }
    Ok(())
}

/// Push completion/lifecycle events to one connected event-socket client until
/// it disconnects (#54). Each broadcast code is written as a standard-framed
/// PTP/IP `Event` packet. Mirrors [`stream_liveview`]'s read-half watcher: event
/// clients never send bytes, so a completed read means EOF/reset — the client is
/// gone, and without watching it a never-emitting session would hang the task.
async fn handle_event_conn(mut stream: TcpStream, mut events: broadcast::Receiver<u16>) {
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
                    Ok(_) => {}              // unexpected bytes; ignore
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
async fn collect_data_in(stream: &mut TcpStream, tid: u32) -> std::io::Result<Option<Vec<u8>>> {
    let Some(frame) = read_frame(stream).await? else {
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

async fn write_reply(
    stream: &mut TcpStream,
    req: &OperationRequest,
    reply: Reply,
) -> std::io::Result<()> {
    match reply {
        Reply::Response(resp) => {
            stream
                .write_all(
                    &fuji_framing::encode(&PtpIpPacket::OperationResponse(resp)).map_err(to_io)?,
                )
                .await?;
        }
        Reply::Data { data, response } => {
            // The whole data phase is one type-2 frame whose code echoes the op.
            stream
                .write_all(&fuji_framing::encode_data(
                    req.code,
                    req.transaction_id,
                    &data,
                ))
                .await?;
            stream
                .write_all(
                    &fuji_framing::encode(&PtpIpPacket::OperationResponse(response))
                        .map_err(to_io)?,
                )
                .await?;
        }
        Reply::DataStream { source, response } => {
            stream_data_phase(stream, req.code, req.transaction_id, &source).await?;
            stream
                .write_all(
                    &fuji_framing::encode(&PtpIpPacket::OperationResponse(response))
                        .map_err(to_io)?,
                )
                .await?;
        }
        Reply::Close => {}
    }
    Ok(())
}

/// Emit the data phase as a single type-2 `Data` frame — the whole payload
/// arrives in one length-prefixed frame on the wire, matching real Fuji (a
/// 14.5 MB `GetObject` is one frame). The 12-byte header goes out first with the
/// total length; the body is then streamed from `source` one `DATA_CHUNK_BYTES`
/// read at a time, so peak in-process allocation stays bounded regardless of
/// `source.len()` even though the client sees a single frame. `op` is echoed in
/// the frame's code field.
async fn stream_data_phase(
    stream: &mut TcpStream,
    op: u16,
    transaction_id: u32,
    source: &ByteSource,
) -> std::io::Result<()> {
    let total_length = source.len();
    let payload_len = u32::try_from(total_length)
        .map_err(|_| std::io::Error::other("data-phase payload exceeds a single frame (u32)"))?;
    stream
        .write_all(&fuji_framing::data_frame_header(
            op,
            transaction_id,
            payload_len,
        ))
        .await?;

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
    }
    Ok(())
}

fn to_io<E: std::fmt::Display>(e: E) -> std::io::Error {
    std::io::Error::other(e.to_string())
}
