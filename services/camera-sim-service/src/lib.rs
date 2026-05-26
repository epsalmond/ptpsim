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
use camera_media_store::MediaStore;
use camera_sim::{Engine, Reply};
use ptp_core::codes::op;
use ptp_core::{InitCommandAck, OperationRequest, PtpCodec, PtpIpPacket};
use protocol_primitives::fuji_framing;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Mutex;

pub mod control;

#[derive(Clone)]
pub struct Config {
    pub instance_id: String,
    pub profile: String,
    pub manifest_yaml: String,
    pub media_root: std::path::PathBuf,
    /// Bind address for the PTP command socket. Use port 0 for an OS-assigned
    /// port (tests).
    pub command_bind: SocketAddr,
    /// Through-picture (live-view) stream socket. Per fw0230 capture this is
    /// 55741.
    pub liveview_bind: SocketAddr,
    /// Async event socket (55742).
    pub event_bind: SocketAddr,
    pub control_bind: SocketAddr,
}

/// A minimal valid JPEG-ish test frame (SOI … EOI) for the generated live-view
/// source. A real source streams MJPEG frames from a directory or transcode.
const TEST_FRAME: &[u8] = &[0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10, b'p', b't', b'p', b's', b'i', b'm', 0xFF, 0xD9];

/// Live-view frame pacing (~30 fps).
const FRAME_INTERVAL_MS: u64 = 33;

/// A bound, not-yet-serving instance. Binding first lets callers (tests) learn
/// the OS-assigned ports before the serve loop starts.
pub struct Server {
    config: Config,
    command: TcpListener,
    liveview: TcpListener,
    event: TcpListener,
    control: TcpListener,
    engine: Arc<Mutex<Engine>>,
}

impl Server {
    pub async fn bind(config: Config) -> std::io::Result<Self> {
        let manifest = CameraManifest::from_yaml(&config.manifest_yaml)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()))?;
        let mut store = MediaStore::open(&config.media_root)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::NotFound, e.to_string()))?;
        store.scan().map_err(|e| std::io::Error::other(e.to_string()))?;
        let engine = Arc::new(Mutex::new(Engine::new(manifest, store)));
        let command = TcpListener::bind(config.command_bind).await?;
        let liveview = TcpListener::bind(config.liveview_bind).await?;
        let event = TcpListener::bind(config.event_bind).await?;
        let control = TcpListener::bind(config.control_bind).await?;
        Ok(Server { config, command, liveview, event, control, engine })
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

    /// Serve until `shutdown` resolves (the `/shutdown` endpoint or a SIGTERM
    /// handler fires it). In-flight command connections are dropped on exit.
    pub async fn run(self, shutdown: tokio::sync::oneshot::Receiver<()>) {
        let Server { config, command, liveview, event, control, engine } = self;
        let health = control::Health {
            instance_id: config.instance_id.clone(),
            profile: config.profile.clone(),
            command_addr: command.local_addr().unwrap(),
            media_root: config.media_root.display().to_string(),
        };
        let (shutdown_tx, _) = tokio::sync::broadcast::channel::<()>(1);
        let ctl_shutdown = shutdown_tx.clone();

        let command_loop = {
            let engine = engine.clone();
            let mut sub = shutdown_tx.subscribe();
            async move {
                loop {
                    tokio::select! {
                        accepted = command.accept() => {
                            if let Ok((stream, _)) = accepted {
                                let engine = engine.clone();
                                tokio::spawn(async move {
                                    let _ = handle_command_conn(stream, engine).await;
                                });
                            }
                        }
                        _ = sub.recv() => break,
                    }
                }
            }
        };

        let control_loop = {
            let mut sub = shutdown_tx.subscribe();
            async move {
                loop {
                    tokio::select! {
                        accepted = control.accept() => {
                            if let Ok((stream, _)) = accepted {
                                control::handle(stream, health.clone(), &engine, ctl_shutdown.clone()).await;
                            }
                        }
                        _ = sub.recv() => break,
                    }
                }
            }
        };

        // Live-view: stream length-prefixed JPEG frames to any connected client.
        let liveview_loop = {
            let mut sub = shutdown_tx.subscribe();
            async move {
                loop {
                    tokio::select! {
                        accepted = liveview.accept() => {
                            if let Ok((stream, _)) = accepted {
                                tokio::spawn(stream_liveview(stream));
                            }
                        }
                        _ = sub.recv() => break,
                    }
                }
            }
        };

        // Event socket: accept and hold open. Event emission is manifest-driven
        // and arrives with the CameraControls/capture work; the socket is real
        // now so clients (and tests) can confirm all three sockets open.
        let event_loop = {
            let mut sub = shutdown_tx.subscribe();
            async move {
                loop {
                    tokio::select! {
                        accepted = event.accept() => { let _ = accepted; }
                        _ = sub.recv() => break,
                    }
                }
            }
        };

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
/// write fails. Frames are length-prefixed via the shared primitive.
async fn stream_liveview(mut stream: TcpStream) {
    let mut tick = tokio::time::interval(std::time::Duration::from_millis(FRAME_INTERVAL_MS));
    loop {
        tick.tick().await;
        let packet = protocol_primitives::liveview::frame_packet(TEST_FRAME);
        if stream.write_all(&packet).await.is_err() {
            break;
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
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "implausible frame length"));
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
) -> std::io::Result<()> {
    // 1. Standard-framed init handshake.
    let Some(first) = read_frame(&mut stream).await? else { return Ok(()) };
    let Ok(PtpIpPacket::InitCommandRequest(_)) = PtpIpPacket::decode(&first) else {
        return Ok(()); // not a PTP/IP initiator
    };
    let ack = PtpIpPacket::InitCommandAck(InitCommandAck {
        connection_number: 1,
        responder_guid: [0; 16],
        friendly_name: {
            let e = engine.lock().await;
            e.manifest().camera.model.clone()
        },
        protocol_version: 0x0001_0000,
    });
    stream.write_all(&ptp_core::encode(&ack).map_err(to_io)?).await?;

    // 2. Compressed-framed operation loop.
    loop {
        let Some(frame) = read_frame(&mut stream).await? else { break };
        let Ok(PtpIpPacket::OperationRequest(req)) = fuji_framing::decode(&frame) else {
            continue;
        };
        let data_in = if has_data_in(req.code) {
            collect_data_in(&mut stream).await?
        } else {
            None
        };
        let reply = {
            let mut e = engine.lock().await;
            e.on_operation(&req, data_in.as_deref())
        };
        write_reply(&mut stream, &req, reply).await?;
    }
    Ok(())
}

/// Collect a StartData/(Data*)/EndData sequence into the data-in payload.
async fn collect_data_in(stream: &mut TcpStream) -> std::io::Result<Option<Vec<u8>>> {
    let mut data = Vec::new();
    loop {
        let Some(frame) = read_frame(stream).await? else { break };
        match fuji_framing::decode(&frame) {
            Ok(PtpIpPacket::StartData(_)) => {}
            Ok(PtpIpPacket::Data(d)) => data.extend_from_slice(&d.payload),
            Ok(PtpIpPacket::EndData(d)) => {
                data.extend_from_slice(&d.payload);
                break;
            }
            _ => break,
        }
    }
    Ok(Some(data))
}

async fn write_reply(
    stream: &mut TcpStream,
    req: &OperationRequest,
    reply: Reply,
) -> std::io::Result<()> {
    match reply {
        Reply::Response(resp) => {
            stream.write_all(&fuji_framing::encode(&PtpIpPacket::OperationResponse(resp)).map_err(to_io)?).await?;
        }
        Reply::Data { data, response } => {
            let start = PtpIpPacket::StartData(ptp_core::StartData {
                transaction_id: req.transaction_id,
                total_length: data.len() as u64,
            });
            let end = PtpIpPacket::EndData(ptp_core::DataBlock {
                transaction_id: req.transaction_id,
                payload: data,
            });
            stream.write_all(&fuji_framing::encode(&start).map_err(to_io)?).await?;
            stream.write_all(&fuji_framing::encode(&end).map_err(to_io)?).await?;
            stream.write_all(&fuji_framing::encode(&PtpIpPacket::OperationResponse(response)).map_err(to_io)?).await?;
        }
        Reply::Close => {}
    }
    Ok(())
}

fn to_io<E: std::fmt::Display>(e: E) -> std::io::Error {
    std::io::Error::other(e.to_string())
}
