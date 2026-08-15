//! `camera-sim-service` — the runnable simulator: PTP/IP listeners plus a local
//! control HTTP API. It is **lease-agnostic** (no NATS, pools, or lease logic —

//! `/shutdown` or `SIGTERM`.
//!
//! The command channel opens with the manifest-selected init shape and then
//! switches to the manifest-selected standard, compressed, or USB/PIMA
//! operation framing.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};

use camera_config::{
    direct_epistemic, no_loss, parse_hex_bytes, parse_hex_code, parse_hex_u32, BundleHeader,
    CameraContext, CameraManifest, CaptureClock, CaptureContext, CaptureInterface,
    CaptureInterfaceType, ClientContext, ClockType, ClockUnit, DataDirection, ExecutionContext,
    LifecycleMarker, LiveViewDeliveryKind, ObservationLine, ObservationRecorder, PayloadMetadata,
    PayloadMetadataBuilder, PcssKnock, PtpDataPhase, PtpEventRecord, PtpRequest, PtpResponse,
    PtpTransactionRecord, PtpTransport, TransactionOutcome, ValuePolicy,
};
use camera_config::{SocketRole, WireFraming};
use camera_media_store::{ByteSource, MediaStore};
use camera_sim::StateOverlay;
use camera_sim::{
    AppliedFault, AppliedStateOverlay, Engine, FrameSource, LoopingFrameSource, Phase, Reply,
    StreamCompletion, WirePlan,
};
use protocol_primitives::{
    fuji_framing, parse_app_init, parse_legacy_app_init, parse_pcss_discovery, parse_pcss_init,
    pcss_callback_ack_message, pcss_init_ack_message, pcss_notify_message,
};
use ptp_core::codes::{op, resp};
use ptp_core::{
    EventPacket, InitCommandAck, InitEventAck, InitFail, OperationRequest, ProbeResponse, PtpCodec,
    PtpIpPacket,
};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, TcpSocket, TcpStream, UdpSocket};
use tokio::sync::broadcast;
use tokio::sync::{Mutex, Notify};

pub mod control;
mod metrics;
mod state_callback;
mod state_json;
mod trace;

pub(crate) use metrics::Metrics;
use trace::{FaultTraceEvidence, TraceEndpoints, TraceLog};

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
    /// Number of manifest-authorized InitFail packets to emit before ack.
    /// The CLI flag retains its PCSS name for compatibility.
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

fn observation_recorder(
    config: &Config,
    manifest: &CameraManifest,
) -> std::io::Result<ObservationRecorder> {
    let file_id = config
        .instance_id
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '-' | '_') {
                character
            } else {
                '_'
            }
        })
        .collect::<String>();
    let path = config
        .media_root
        .join(".ptpsim")
        .join(format!("observations-{file_id}.jsonl"));
    let header = BundleHeader {
        schema: camera_config::OBSERVATION_SCHEMA_VERSION.into(),
        run_id: config.instance_id.clone(),
        record_id: "bundle-header".into(),
        ordinal: 0,
        camera: CameraContext {
            manufacturer: manifest.camera.manufacturer.clone(),
            model: manifest.camera.model.clone(),
            body_id: "simulated-body".into(),
            firmware: manifest.camera.firmware.clone(),
        },
        client: ClientContext {
            artifact: "camera-sim-service".into(),
            version: env!("CARGO_PKG_VERSION").into(),
            platform: std::env::consts::OS.into(),
        },
        capture: CaptureContext {
            interfaces: vec![CaptureInterface {
                id: "simulator".into(),
                interface_type: CaptureInterfaceType::Synthetic,
                role: "responder".into(),
            }],
            clocks: vec![CaptureClock {
                id: "process-monotonic".into(),
                clock_type: ClockType::Monotonic,
                unit: ClockUnit::Milliseconds,
            }],
            clock_mappings: Vec::new(),
            loss: no_loss(),
            redactions: Vec::new(),
            tool_versions: BTreeMap::from([(
                "camera-sim-service".into(),
                env!("CARGO_PKG_VERSION").into(),
            )]),
            artifacts: Vec::new(),
        },
        epistemic: direct_epistemic(),
    };
    ObservationRecorder::open(Some(path), header)
        .map_err(|error| std::io::Error::other(error.to_string()))
}

/// A bound, not-yet-serving instance. Binding first lets callers (tests) learn
/// the OS-assigned ports before the serve loop starts.
pub struct Server {
    config: Config,
    command: TcpListener,
    liveview: Option<AuxListener>,
    event: Option<AuxListener>,
    shared_standard_event_listener: bool,
    knock: Option<UdpSocket>,
    knock_config: Option<PcssKnock>,
    command_framing: WireFraming,
    expected_responder_guid: Option<[u8; 16]>,
    init_fail_reason: Option<u32>,
    event_framing: Option<WireFraming>,
    poll_live_view_op: Option<u16>,
    disallowed_ops: Arc<HashSet<u16>>,
    control: TcpListener,
    engine: Arc<Mutex<Engine>>,
    metrics: Metrics,
    /// Shared looping frame source (one cursor across all live-view clients —
    /// the normal lease shape is one camera = one client; cursor sharing is
    /// harmless if a smoke client connects alongside the real one).
    frames: Arc<Mutex<LoopingFrameSource>>,
    observations: ObservationRecorder,
}

enum AuxListener {
    Listening(TcpListener),
    Declared(Arc<DeclaredAuxListener>),
}

enum AuxAccepted {
    Connection(TcpStream),
    Rearmed,
}

impl AuxListener {
    fn local_addr(&self) -> std::io::Result<SocketAddr> {
        match self {
            Self::Listening(listener) => listener.local_addr(),
            Self::Declared(listener) => listener.local_addr(),
        }
    }

    fn declared(&self) -> Option<Arc<DeclaredAuxListener>> {
        match self {
            Self::Listening(_) => None,
            Self::Declared(listener) => Some(Arc::clone(listener)),
        }
    }

    async fn accept(&self) -> std::io::Result<AuxAccepted> {
        match self {
            Self::Listening(listener) => listener
                .accept()
                .await
                .map(|(stream, _)| AuxAccepted::Connection(stream)),
            Self::Declared(listener) => listener.accept().await,
        }
    }
}

pub(crate) struct DeclaredAuxListener {
    bind_addr: SocketAddr,
    state: StdMutex<DeclaredAuxListenerState>,
    ready: Notify,
    transition: Mutex<()>,
    rearm_epoch: AtomicU64,
    reported_rearm_epoch: AtomicU64,
}

enum DeclaredAuxListenerState {
    Bound(TcpSocket),
    Listening {
        listener: Arc<TcpListener>,
        cancel: Arc<Notify>,
        pending_accept: Option<Arc<Notify>>,
    },
    Rebinding,
}

struct DeclaredAcceptGuard<'a> {
    owner: &'a DeclaredAuxListener,
    completed: Arc<Notify>,
}

impl Drop for DeclaredAcceptGuard<'_> {
    fn drop(&mut self) {
        let mut state = self.owner.state.lock().expect("declared listener state");
        if let DeclaredAuxListenerState::Listening { pending_accept, .. } = &mut *state {
            if pending_accept
                .as_ref()
                .is_some_and(|pending| Arc::ptr_eq(pending, &self.completed))
            {
                *pending_accept = None;
            }
        }
        self.completed.notify_one();
    }
}

impl DeclaredAuxListener {
    fn new(socket: TcpSocket) -> std::io::Result<Self> {
        Ok(Self {
            bind_addr: socket.local_addr()?,
            state: StdMutex::new(DeclaredAuxListenerState::Bound(socket)),
            ready: Notify::new(),
            transition: Mutex::new(()),
            rearm_epoch: AtomicU64::new(0),
            reported_rearm_epoch: AtomicU64::new(0),
        })
    }

    fn local_addr(&self) -> std::io::Result<SocketAddr> {
        Ok(self.bind_addr)
    }

    async fn set_ready(&self, ready: bool) -> std::io::Result<()> {
        let _transition = self.transition.lock().await;
        if ready {
            self.activate()
        } else {
            self.deactivate().await
        }
    }

    fn activate(&self) -> std::io::Result<()> {
        let mut state = self.state.lock().expect("declared listener state");
        let current = std::mem::replace(&mut *state, DeclaredAuxListenerState::Rebinding);
        match current {
            DeclaredAuxListenerState::Bound(socket) => {
                self.listen(socket, &mut state)?;
            }
            listening @ DeclaredAuxListenerState::Listening { .. } => {
                *state = listening;
            }
            DeclaredAuxListenerState::Rebinding => {
                let socket = match bind_declared_socket(self.bind_addr) {
                    Ok(socket) => socket,
                    Err(error) => {
                        *state = DeclaredAuxListenerState::Rebinding;
                        return Err(error);
                    }
                };
                self.listen(socket, &mut state)?;
            }
        }
        Ok(())
    }

    async fn deactivate(&self) -> std::io::Result<()> {
        let pending_accept = {
            let mut state = self.state.lock().expect("declared listener state");
            let current = std::mem::replace(&mut *state, DeclaredAuxListenerState::Rebinding);
            match current {
                DeclaredAuxListenerState::Bound(socket) => {
                    *state = DeclaredAuxListenerState::Bound(socket);
                    return Ok(());
                }
                DeclaredAuxListenerState::Listening {
                    listener,
                    cancel,
                    pending_accept,
                } => {
                    self.rearm_epoch.fetch_add(1, Ordering::AcqRel);
                    cancel.notify_one();
                    drop(listener);
                    pending_accept
                }
                DeclaredAuxListenerState::Rebinding => None,
            }
        };
        if let Some(pending_accept) = pending_accept {
            pending_accept.notified().await;
        }
        let rebound = bind_declared_socket(self.bind_addr);
        let mut state = self.state.lock().expect("declared listener state");
        match rebound {
            Ok(socket) => {
                *state = DeclaredAuxListenerState::Bound(socket);
                Ok(())
            }
            Err(error) => {
                *state = DeclaredAuxListenerState::Rebinding;
                Err(error)
            }
        }
    }

    fn listen(
        &self,
        socket: TcpSocket,
        state: &mut DeclaredAuxListenerState,
    ) -> std::io::Result<()> {
        match socket.listen(1024) {
            Ok(listener) => {
                *state = DeclaredAuxListenerState::Listening {
                    listener: Arc::new(listener),
                    cancel: Arc::new(Notify::new()),
                    pending_accept: None,
                };
                self.ready.notify_one();
                Ok(())
            }
            Err(listen_error) => {
                match bind_declared_socket(self.bind_addr) {
                    Ok(socket) => *state = DeclaredAuxListenerState::Bound(socket),
                    Err(bind_error) => {
                        *state = DeclaredAuxListenerState::Rebinding;
                        return Err(std::io::Error::new(
                            listen_error.kind(),
                            format!(
                                "failed to listen on {}: {listen_error}; failed to rebind: {bind_error}",
                                self.bind_addr
                            ),
                        ));
                    }
                }
                Err(listen_error)
            }
        }
    }

    async fn accept(&self) -> std::io::Result<AuxAccepted> {
        loop {
            if self.take_rearm() {
                return Ok(AuxAccepted::Rearmed);
            }
            let notified = self.ready.notified();
            let listening = {
                let mut state = self.state.lock().expect("declared listener state");
                match &mut *state {
                    DeclaredAuxListenerState::Listening {
                        listener,
                        cancel,
                        pending_accept,
                    } => {
                        let completed = Arc::new(Notify::new());
                        assert!(pending_accept.replace(Arc::clone(&completed)).is_none());
                        Some((Arc::clone(listener), Arc::clone(cancel), completed))
                    }
                    DeclaredAuxListenerState::Bound(_) | DeclaredAuxListenerState::Rebinding => {
                        None
                    }
                }
            };
            let Some((listener, cancel, completed)) = listening else {
                notified.await;
                continue;
            };
            let _accept_guard = DeclaredAcceptGuard {
                owner: self,
                completed,
            };
            let accepted = tokio::select! {
                biased;
                _ = cancel.notified() => None,
                accepted = listener.accept() => Some(accepted),
            };
            drop(listener);
            match accepted {
                Some(accepted) => {
                    return accepted.map(|(stream, _)| AuxAccepted::Connection(stream));
                }
                None => {
                    let _ = self.take_rearm();
                    return Ok(AuxAccepted::Rearmed);
                }
            }
        }
    }

    fn take_rearm(&self) -> bool {
        let current = self.rearm_epoch.load(Ordering::Acquire);
        let mut reported = self.reported_rearm_epoch.load(Ordering::Acquire);
        while reported < current {
            match self.reported_rearm_epoch.compare_exchange_weak(
                reported,
                current,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return true,
                Err(actual) => reported = actual,
            }
        }
        false
    }
}

#[derive(Clone)]
struct CommandContext {
    connection: String,
    init_shape: String,
    command_framing: WireFraming,
    expected_responder_guid: Option<[u8; 16]>,
    init_fail_reason: Option<u32>,
    pcss_init_fails: u32,
    poll_live_view_op: Option<u16>,
    disallowed_ops: Arc<HashSet<u16>>,
}

#[derive(Clone)]
struct CommandResources {
    engine: Arc<Mutex<Engine>>,
    session_owner: Arc<Mutex<Option<u64>>>,
    frames: Arc<Mutex<LoopingFrameSource>>,
    event_tx: broadcast::Sender<camera_sim::QueuedEvent>,
    state_dirty: Arc<Notify>,
    context: CommandContext,
    trace: TraceLog,
    observations: ObservationRecorder,
    metrics: Metrics,
    standard_connections: Arc<StandardConnections>,
    declared_aux: Vec<(SocketRole, Arc<DeclaredAuxListener>)>,
    declared_aux_sync: Arc<Mutex<()>>,
}

#[derive(Default)]
struct StandardConnections {
    next: AtomicU32,
    active: StdMutex<HashMap<u32, StandardConnection>>,
}

struct StandardConnection {
    cancel: broadcast::Sender<()>,
    event_attached: bool,
}

impl StandardConnections {
    fn allocate(&self) -> (u32, broadcast::Receiver<()>) {
        let connection_number = self.next.fetch_add(1, Ordering::AcqRel).wrapping_add(1);
        let (cancel, receiver) = broadcast::channel(1);
        self.active
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(
                connection_number,
                StandardConnection {
                    cancel,
                    event_attached: false,
                },
            );
        (connection_number, receiver)
    }

    fn attach(
        self: &Arc<Self>,
        connection_number: u32,
    ) -> Option<(broadcast::Receiver<()>, StandardEventAttachGuard)> {
        let mut active = self
            .active
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let connection = active.get_mut(&connection_number)?;
        if connection.event_attached {
            return None;
        }
        connection.event_attached = true;
        Some((
            connection.cancel.subscribe(),
            StandardEventAttachGuard {
                connections: self.clone(),
                connection_number,
                armed: true,
            },
        ))
    }

    fn detach_event(&self, connection_number: u32) {
        if let Some(connection) = self
            .active
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get_mut(&connection_number)
        {
            connection.event_attached = false;
        }
    }

    fn close(&self, connection_number: u32) {
        let connection = self
            .active
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(&connection_number);
        if let Some(connection) = connection {
            let _ = connection.cancel.send(());
        }
    }
}

struct StandardEventAttachGuard {
    connections: Arc<StandardConnections>,
    connection_number: u32,
    armed: bool,
}

impl StandardEventAttachGuard {
    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for StandardEventAttachGuard {
    fn drop(&mut self) {
        if self.armed {
            self.connections.detach_event(self.connection_number);
        }
    }
}

struct StandardCommandGuard {
    connections: Arc<StandardConnections>,
    connection_number: u32,
}

impl Drop for StandardCommandGuard {
    fn drop(&mut self) {
        self.connections.close(self.connection_number);
    }
}

async fn bind_aux_listener(addr: SocketAddr, declared: bool) -> std::io::Result<AuxListener> {
    if !declared {
        return TcpListener::bind(addr).await.map(AuxListener::Listening);
    }
    let socket = bind_declared_socket(addr)?;
    Ok(AuxListener::Declared(Arc::new(DeclaredAuxListener::new(
        socket,
    )?)))
}

fn bind_declared_socket(addr: SocketAddr) -> std::io::Result<TcpSocket> {
    let socket = if addr.is_ipv4() {
        TcpSocket::new_v4()?
    } else {
        TcpSocket::new_v6()?
    };
    socket.set_reuseaddr(true)?;
    socket.bind(addr)?;
    Ok(socket)
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
        let event_declared = bindings.available_after(SocketRole::Event).is_some();
        let liveview_declared = bindings.available_after(SocketRole::LiveView).is_some();
        let command_framing = connection.command_framing.ok_or_else(|| {
            invalid_config(format!(
                "selected connection '{}' has no command framing",
                config.connection
            ))
        })?;
        let init_shape = connection.init_shape.as_deref().ok_or_else(|| {
            invalid_config(format!(
                "selected connection '{}' has no init shape",
                config.connection
            ))
        })?;
        let supported_shape = matches!(
            (init_shape, command_framing),
            ("app82", WireFraming::Compressed)
                | ("pcssKnock", WireFraming::Compressed)
                | ("legacyApp82", WireFraming::Usb)
                | ("standardPtpIp", WireFraming::Standard)
        );
        if !supported_shape {
            return Err(invalid_config(format!(
                "selected connection '{}' uses unsupported init shape/framing combination {init_shape}/{command_framing:?}",
                config.connection
            )));
        }
        let expected_responder_guid = match connection
            .init
            .as_ref()
            .and_then(|init| init.expected_responder_guid.as_deref())
        {
            Some(key) => Some(fixed_manifest_guid(&manifest, key)?),
            None => None,
        };
        if connection.init_shape.as_deref() == Some("legacyApp82")
            && expected_responder_guid.is_none()
        {
            return Err(invalid_config(format!(
                "selected connection '{}' legacyApp82 init has no expectedResponderGuid",
                config.connection
            )));
        }
        if bindings.port_for(SocketRole::Event).is_some()
            && !matches!(
                connection.event_framing,
                Some(WireFraming::Standard | WireFraming::Usb)
            )
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
        let init_fail_reason = connection
            .init_retries
            .as_ref()
            .and_then(|retries| retries.when_reasons.first())
            .and_then(|reason| parse_hex_u32(reason));
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
        let command_override = config.command_bind;
        let event_override = config.event_bind;
        let command_bind = role_bind(
            command_override,
            bindings.port_for(SocketRole::Command),
            "command",
        )?
        .ok_or_else(|| {
            invalid_config(format!(
                "selected connection '{}' has no command socket binding",
                config.connection
            ))
        })?;
        let manifest_shares_event_listener = bindings.port_for(SocketRole::Event)
            == bindings.port_for(SocketRole::Command)
            && bindings.port_for(SocketRole::Event).is_some();
        let event_bind = if manifest_shares_event_listener && event_override.is_none() {
            Some(command_bind)
        } else {
            role_bind(
                event_override,
                bindings.port_for(SocketRole::Event),
                "event",
            )?
        };
        let explicit_nonzero_shared_listener = command_override
            .zip(event_override)
            .is_some_and(|(command, event)| command == event && command.port() != 0);
        let shared_standard_event_listener = init_shape == "standardPtpIp"
            && ((manifest_shares_event_listener && event_override.is_none())
                || explicit_nonzero_shared_listener);
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
        let event_framing = connection.event_framing;
        let observations = observation_recorder(&config, &manifest)?;
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
            Some(addr) => Some(bind_aux_listener(addr, liveview_declared).await?),
            None => None,
        };
        let event = match event_bind.filter(|_| !shared_standard_event_listener) {
            Some(addr) => Some(bind_aux_listener(addr, event_declared).await?),
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
            shared_standard_event_listener,
            knock,
            knock_config,
            command_framing,
            expected_responder_guid,
            init_fail_reason,
            event_framing,
            poll_live_view_op,
            disallowed_ops,
            control,
            engine,
            metrics,
            frames,
            observations,
        })
    }

    pub fn command_addr(&self) -> SocketAddr {
        self.command.local_addr().unwrap()
    }

    pub fn liveview_addr_opt(&self) -> Option<SocketAddr> {
        self.liveview.as_ref().map(|l| l.local_addr().unwrap())
    }

    pub fn event_addr_opt(&self) -> Option<SocketAddr> {
        if self.shared_standard_event_listener {
            Some(self.command.local_addr().unwrap())
        } else {
            self.event.as_ref().map(|l| l.local_addr().unwrap())
        }
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
            shared_standard_event_listener,
            knock,
            knock_config,
            command_framing,
            expected_responder_guid,
            init_fail_reason,
            event_framing,
            poll_live_view_op,
            disallowed_ops,
            control,
            engine,
            metrics,
            frames,
            observations,
        } = self;
        let health = control::Health {
            instance_id: config.instance_id.clone(),
            profile: config.profile.clone(),
            connection: config.connection.clone(),
            command_addr: command.local_addr().unwrap(),
            knock_addr: knock
                .as_ref()
                .map(|listener| listener.local_addr().unwrap()),
            media_root: config.media_root.display().to_string(),
            metrics: metrics.clone(),
        };
        let command_context = CommandContext {
            connection: config.connection.clone(),
            init_shape: {
                let e = engine.lock().await;
                e.manifest()
                    .connections
                    .get(&config.connection)
                    .and_then(|c| c.init_shape.as_deref())
                    .expect("selected connection init shape validated at bind")
                    .to_string()
            },
            command_framing,
            expected_responder_guid,
            init_fail_reason,
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
        let (event_tx, _) = broadcast::channel::<camera_sim::QueuedEvent>(16);
        let standard_connections = Arc::new(StandardConnections::default());
        let session_owner = Arc::new(Mutex::new(None));
        let declared_aux = [
            (SocketRole::Event, event.as_ref()),
            (SocketRole::LiveView, liveview.as_ref()),
        ]
        .into_iter()
        .filter_map(|(role, listener)| Some((role, listener?.declared()?)))
        .collect::<Vec<_>>();
        let declared_aux_sync = Arc::new(Mutex::new(()));

        // #126/#214 state-callback: one shared notify the command loop bumps
        // after every op. Cheap when nobody listens; the single push task
        // consumes it, debounces, and POSTs to startup and runtime subscribers.
        let state_dirty = Arc::new(Notify::new());
        let state_callbacks = state_callback::Registry::default();
        let trace = TraceLog::default();
        if let Err(error) = observations.record_lifecycle(
            ExecutionContext {
                connection: config.connection.clone(),
                mode: "unselected".into(),
                state: "ready".into(),
            },
            LifecycleMarker::ConnectionOpened,
            None,
            None,
            BTreeMap::new(),
        ) {
            tracing::error!(%error, "canonical observation recording failed");
            return;
        }

        // Per-connection tasks live in a JoinSet per accept loop (audit in
        // docs/internal-async-notes.md): the `Some(_) = join_next()` arm reaps
        // finished tasks on a long-lived server, and JoinSet aborts everything
        // still running when dropped — which happens when run()'s select!
        // drops the loop future on shutdown. In-flight connections being cut
        // on exit is the documented run() contract.
        let command_port = command.local_addr().unwrap().port();
        let command_loop = {
            let resources = CommandResources {
                engine: engine.clone(),
                session_owner: session_owner.clone(),
                frames: frames.clone(),
                event_tx: event_tx.clone(),
                state_dirty: state_dirty.clone(),
                context: command_context.clone(),
                trace: trace.clone(),
                observations: observations.clone(),
                metrics: metrics.clone(),
                standard_connections: standard_connections.clone(),
                declared_aux: declared_aux.clone(),
                declared_aux_sync: declared_aux_sync.clone(),
            };
            let mut sub = shutdown_tx.subscribe();
            async move {
                let mut conns = tokio::task::JoinSet::new();
                loop {
                    tokio::select! {
                        accepted = command.accept() => {
                            if let Ok((stream, _)) = accepted {
                                if shared_standard_event_listener {
                                    conns.spawn(handle_standard_listener_conn(
                                        stream,
                                        resources.clone(),
                                    ));
                                } else {
                                    match reserve_command_session_identity(
                                        &resources.observations,
                                        &resources.context.connection,
                                    ) {
                                        Ok(session_sequence) => {
                                            conns.spawn(handle_command_conn(
                                                stream,
                                                resources.clone(),
                                                None,
                                                session_sequence,
                                            ));
                                        }
                                        Err(error) => {
                                            tracing::error!(%error, "canonical connection identity recording failed");
                                        }
                                    }
                                }
                            }
                        }
                        Some(_) = conns.join_next(), if !conns.is_empty() => {}
                        _ = sub.recv() => break,
                    }
                }
            }
        };

        let control_loop = {
            let context = control::Context {
                health,
                engine: engine.clone(),
                state_notify: state_dirty.clone(),
                callbacks: state_callbacks.clone(),
                trace: trace.clone(),
                observations: observations.clone(),
                metrics: metrics.clone(),
                shutdown: ctl_shutdown.clone(),
                declared_aux,
                declared_aux_sync,
            };
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
                                conns.spawn(control::handle(stream, context.clone()));
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
                            match accepted {
                                Ok(AuxAccepted::Connection(stream)) => {
                                    if !engine.lock().await.channel_ready(camera_config::SocketRole::LiveView) {
                                        drop(stream);
                                        continue;
                                    }
                                    let engine = engine.clone();
                                    let frames = frames.clone();
                                    let metrics = metrics.clone();
                                    conns.spawn(stream_liveview(stream, engine, frames, metrics));
                                }
                                Ok(AuxAccepted::Rearmed) => conns.shutdown().await,
                                Err(_) => {}
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
            let engine = engine.clone();
            let shutdown_tx = shutdown_tx.clone();
            let metrics = metrics.clone();
            let standard_connections = standard_connections.clone();
            let standard_events = command_context.init_shape == "standardPtpIp";
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
                            match accepted {
                                Ok(AuxAccepted::Connection(stream)) => {
                                    if !engine.lock().await.channel_ready(camera_config::SocketRole::Event) {
                                        drop(stream);
                                        continue;
                                    }
                                    if standard_events {
                                        conns.spawn(handle_standard_event_conn(
                                            stream,
                                            None,
                                            event_tx.subscribe(),
                                            standard_connections.clone(),
                                            metrics.clone(),
                                        ));
                                    } else {
                                        let events = event_tx.subscribe();
                                        let framing = event_framing.expect("bound event socket has framing");
                                        let metrics = metrics.clone();
                                        conns.spawn(async move {
                                            handle_event_conn(stream, events, framing, metrics).await;
                                            Ok::<(), std::io::Error>(())
                                        });
                                    }
                                }
                                Ok(AuxAccepted::Rearmed) => conns.shutdown().await,
                                Err(_) => {}
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
            let trace = trace.clone();
            async move {
                let Some(knock) = knock else {
                    let _ = sub.recv().await;
                    return;
                };
                let Some(knock_config) = knock_config else {
                    let _ = sub.recv().await;
                    return;
                };
                run_knock_loop(knock, knock_config, camera_name, command_port, trace, sub).await;
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
/// frame from the shared LoopingFrameSource and write the capture-compatible
/// `[u32 total length | 14-byte stream header | JPEG]` packet via the shared
/// framing primitive. Otherwise idle (the connection stays open but no
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
    let mut frame_counter = 0u32;
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
                let packet = protocol_primitives::liveview::frame_packet(&jpeg, frame_counter);
                frame_counter = frame_counter.wrapping_add(1);
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
async fn read_frame<R: AsyncRead + Unpin>(
    stream: &mut R,
    metrics: &Metrics,
) -> std::io::Result<Option<Vec<u8>>> {
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

fn response_outcome(code: u16) -> TransactionOutcome {
    if code == resp::OK {
        TransactionOutcome::Ok
    } else {
        TransactionOutcome::NonOk
    }
}

fn reply_is_ok(reply: &Reply) -> bool {
    match reply {
        Reply::Response(response)
        | Reply::Data { response, .. }
        | Reply::DataStream { response, .. } => response.code == resp::OK,
        Reply::NoResponse | Reply::Close => false,
    }
}

/// Persist the physical connection allocation before handing its socket to an
/// asynchronous task. Using the lifecycle record's ordinal makes the identity
/// unique across concurrent accepts and service restarts, including accepted
/// connections that close before producing a PTP transaction.
fn reserve_command_session_identity(
    observations: &ObservationRecorder,
    connection: &str,
) -> std::io::Result<u64> {
    observations
        .record_lifecycle(
            ExecutionContext {
                connection: connection.into(),
                mode: "unselected".into(),
                state: "command-accepted".into(),
            },
            LifecycleMarker::ConnectionOpened,
            None,
            None,
            BTreeMap::new(),
        )
        .map_err(|error| std::io::Error::other(error.to_string()))
}

async fn handle_standard_listener_conn(
    mut stream: TcpStream,
    resources: CommandResources,
) -> std::io::Result<()> {
    let Some(first) = read_frame(&mut stream, &resources.metrics).await? else {
        return Ok(());
    };
    match PtpIpPacket::decode(&first) {
        Ok(PtpIpPacket::InitCommandRequest(_)) => {
            let session_sequence = match reserve_command_session_identity(
                &resources.observations,
                &resources.context.connection,
            ) {
                Ok(session_sequence) => session_sequence,
                Err(error) => {
                    tracing::error!(%error, "canonical connection identity recording failed");
                    return Ok(());
                }
            };
            handle_command_conn(stream, resources, Some(first), session_sequence).await
        }
        Ok(PtpIpPacket::InitEventRequest(_)) => {
            handle_standard_event_conn(
                stream,
                Some(first),
                resources.event_tx.subscribe(),
                resources.standard_connections,
                resources.metrics,
            )
            .await
        }
        _ => Ok(()),
    }
}

async fn handle_standard_event_conn(
    mut stream: TcpStream,
    first_frame: Option<Vec<u8>>,
    mut events: broadcast::Receiver<camera_sim::QueuedEvent>,
    connections: Arc<StandardConnections>,
    metrics: Metrics,
) -> std::io::Result<()> {
    let Some(first) = (match first_frame {
        Some(frame) => Some(frame),
        None => read_frame(&mut stream, &metrics).await?,
    }) else {
        return Ok(());
    };
    let connection_number = match PtpIpPacket::decode(&first) {
        Ok(PtpIpPacket::InitEventRequest(request)) => request.connection_number,
        _ => return Ok(()),
    };
    let Some((mut cancel, mut attach_guard)) = connections.attach(connection_number) else {
        return Ok(());
    };
    acknowledge_standard_event(&mut stream, &metrics, &mut attach_guard).await?;

    let (mut reader, mut writer) = stream.into_split();
    loop {
        tokio::select! {
            event = events.recv() => {
                let Ok(event) = event else { break };
                let packet = PtpIpPacket::Event(EventPacket {
                    code: event.code,
                    transaction_id: 0,
                    params: event.params.clone(),
                });
                let bytes = ptp_core::encode(&packet).map_err(to_io)?;
                if writer.write_all(&bytes).await.is_err() {
                    break;
                }
                metrics.record_write(bytes.len());
                metrics.touch();
            }
            frame = read_frame(&mut reader, &metrics) => {
                let Ok(Some(frame)) = frame else { break };
                if matches!(PtpIpPacket::decode(&frame), Ok(PtpIpPacket::ProbeRequest(_))) {
                    let response = ptp_core::encode(&PtpIpPacket::ProbeResponse(ProbeResponse))
                        .map_err(to_io)?;
                    if writer.write_all(&response).await.is_err() {
                        break;
                    }
                    metrics.record_write(response.len());
                }
            }
            _ = cancel.recv() => break,
        }
    }
    connections.close(connection_number);
    let _ = writer.shutdown().await;
    Ok(())
}

async fn acknowledge_standard_event<W: AsyncWrite + Unpin>(
    stream: &mut W,
    metrics: &Metrics,
    attach_guard: &mut StandardEventAttachGuard,
) -> std::io::Result<()> {
    let ack = ptp_core::encode(&PtpIpPacket::InitEventAck(InitEventAck)).map_err(to_io)?;
    stream.write_all(&ack).await?;
    metrics.record_write(ack.len());
    attach_guard.disarm();
    Ok(())
}

async fn handle_command_conn(
    stream: TcpStream,
    resources: CommandResources,
    first_frame: Option<Vec<u8>>,
    session_sequence: u64,
) -> std::io::Result<()> {
    let engine = resources.engine.clone();
    let session_owner = resources.session_owner.clone();
    let result = handle_command_conn_inner(stream, resources, first_frame, session_sequence).await;
    // Session teardown belongs to the command connection that opened it. A
    // completed older task must not clear a session opened by a newer socket.
    let mut engine = engine.lock().await;
    let mut owner = session_owner.lock().await;
    if *owner == Some(session_sequence) {
        engine.transport_lost();
        *owner = None;
    }
    result
}

async fn handle_command_conn_inner(
    mut stream: TcpStream,
    resources: CommandResources,
    first_frame: Option<Vec<u8>>,
    session_sequence: u64,
) -> std::io::Result<()> {
    let CommandResources {
        engine,
        session_owner,
        frames,
        event_tx,
        state_dirty,
        context,
        trace,
        observations,
        metrics,
        standard_connections,
        declared_aux,
        declared_aux_sync,
    } = resources;
    let connection_instance = format!("simulator-connection-{session_sequence:016x}");
    let session = format!("ptp-session-{session_sequence:016x}");
    let endpoints = TraceEndpoints::connection(&stream);
    let is_pcss = context.init_shape == "pcssKnock";
    let is_legacy_app = context.init_shape == "legacyApp82";
    if is_pcss {
        trace.record(
            "ptpip.command.accepted",
            endpoints.clone(),
            None,
            Some("accepted".into()),
            None,
        );
    }

    // 1. Standard-framed init handshake.
    let Some(mut first) = (match first_frame {
        Some(frame) => Some(frame),
        None => read_frame(&mut stream, &metrics).await?,
    }) else {
        if is_pcss {
            trace.record(
                "ptpip.init_request.rejected",
                endpoints.clone(),
                None,
                Some("closed".into()),
                Some("command socket closed before InitCommandRequest".into()),
            );
        }
        return Ok(());
    };
    let mut standard_connection_number = None;
    let mut standard_cancel = None;
    let mut standard_guard = None;
    match context.init_shape.as_str() {
        "pcssKnock" => {
            for attempt in 0..=context.pcss_init_fails {
                trace.record(
                    "ptpip.init_request.received",
                    endpoints.clone(),
                    Some(&first),
                    Some(format!("attempt={}", attempt + 1)),
                    None,
                );
                if let Err(error) = parse_pcss_init(&first) {
                    trace.record(
                        "ptpip.init_request.rejected",
                        endpoints.clone(),
                        Some(&first),
                        Some(format!("attempt={}", attempt + 1)),
                        Some(error.to_string()),
                    );
                    return Ok(());
                }
                if attempt == context.pcss_init_fails {
                    break;
                }
                let reason = context
                    .init_fail_reason
                    .ok_or_else(|| invalid_config("configured InitFail has no manifest reason"))?;
                let fail = PtpIpPacket::InitFail(InitFail { reason });
                let bytes = ptp_core::encode(&fail).map_err(to_io)?;
                stream.write_all(&bytes).await?;
                metrics.record_write(bytes.len());
                trace.record(
                    "ptpip.init_fail.sent",
                    endpoints.clone(),
                    Some(&bytes),
                    Some(format!("attempt={};reason=0x{reason:08x}", attempt + 1)),
                    None,
                );
                let Some(next) = read_frame(&mut stream, &metrics).await? else {
                    trace.record(
                        "ptpip.init_request.rejected",
                        endpoints.clone(),
                        None,
                        Some(format!("attempt={}", attempt + 2)),
                        Some("command socket closed before retry".into()),
                    );
                    return Ok(());
                };
                first = next;
            }
        }
        "app82" => {
            let Ok(init_req) = parse_app_init(&first) else {
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
        "legacyApp82" => {
            let mut parsed = None;
            for attempt in 0..=context.pcss_init_fails {
                let Ok(init_req) = parse_legacy_app_init(&first) else {
                    return Ok(());
                };
                parsed = Some(init_req);
                if attempt == context.pcss_init_fails {
                    break;
                }
                let reason = context
                    .init_fail_reason
                    .ok_or_else(|| invalid_config("configured InitFail has no manifest reason"))?;
                let fail = PtpIpPacket::InitFail(InitFail { reason });
                let bytes = ptp_core::encode(&fail).map_err(to_io)?;
                stream.write_all(&bytes).await?;
                metrics.record_write(bytes.len());
                let Some(next) = read_frame(&mut stream, &metrics).await? else {
                    return Ok(());
                };
                first = next;
            }
            let init_req = parsed.expect("legacy manufacturer app loop parses at least once");
            // legacy manufacturer app does not use reference app's IMAGE_TRANSFER_SETTING arming
            // write. It does retain the registered BLE terminal-name check.
            let registered_name = engine.lock().await.link().device_name();
            if let Some(registered) = registered_name {
                if registered != init_req.friendly_name {
                    return Ok(());
                }
            }
        }
        "standardPtpIp" => {
            if !matches!(
                PtpIpPacket::decode(&first),
                Ok(PtpIpPacket::InitCommandRequest(_))
            ) {
                return Ok(());
            }
            let (connection_number, cancel) = standard_connections.allocate();
            standard_connection_number = Some(connection_number);
            standard_cancel = Some(cancel);
            standard_guard = Some(StandardCommandGuard {
                connections: standard_connections.clone(),
                connection_number,
            });
        }
        _ => return Ok(()),
    }
    let camera_name = {
        let e = engine.lock().await;
        e.manifest().camera.model.clone()
    };
    let ack_bytes = if is_pcss {
        pcss_init_ack_message(0, [0; 16], &camera_name).map_err(to_io)?
    } else if is_legacy_app {
        let responder_guid = context
            .expected_responder_guid
            .ok_or_else(|| invalid_config("legacyApp82 init requires a manifest responder GUID"))?;
        pcss_init_ack_message(1, responder_guid, &camera_name).map_err(to_io)?
    } else {
        let ack = PtpIpPacket::InitCommandAck(InitCommandAck {
            connection_number: standard_connection_number.unwrap_or(1),
            responder_guid: [0; 16],
            friendly_name: camera_name,
            protocol_version: 0x0001_0000,
        });
        ptp_core::encode(&ack).map_err(to_io)?
    };
    stream.write_all(&ack_bytes).await?;
    metrics.record_write(ack_bytes.len());
    if is_pcss {
        trace.record(
            "ptpip.init_ack.sent",
            endpoints.clone(),
            Some(&ack_bytes),
            Some("connection_number=0".into()),
            None,
        );
    }

    // 2. Manifest-framed operation loop.
    let mut first_operation_traced = false;
    loop {
        let next = if let Some(cancel) = standard_cancel.as_mut() {
            tokio::select! {
                frame = read_frame(&mut stream, &metrics) => frame?,
                _ = cancel.recv() => None,
            }
        } else {
            read_frame(&mut stream, &metrics).await?
        };
        let Some(frame) = next else {
            break;
        };
        let req = match decode_command_frame(context.command_framing, &frame) {
            Ok(PtpIpPacket::OperationRequest(req)) => req,
            Ok(PtpIpPacket::ProbeRequest(_))
                if context.command_framing == WireFraming::Standard =>
            {
                let response =
                    ptp_core::encode(&PtpIpPacket::ProbeResponse(ProbeResponse)).map_err(to_io)?;
                stream.write_all(&response).await?;
                metrics.record_write(response.len());
                continue;
            }
            Ok(other) => {
                if is_pcss && !first_operation_traced {
                    trace.record(
                        "ptpip.first_operation.rejected",
                        endpoints.clone(),
                        Some(&frame),
                        Some("wrong_packet_type".into()),
                        Some(format!(
                            "expected OperationRequest, got {}",
                            ptpip_packet_kind(&other)
                        )),
                    );
                }
                continue;
            }
            Err(error) => {
                if is_pcss && !first_operation_traced {
                    trace.record(
                        "ptpip.first_operation.rejected",
                        endpoints.clone(),
                        Some(&frame),
                        Some("decode_failed".into()),
                        Some(error.to_string()),
                    );
                }
                continue;
            }
        };
        if is_pcss && !first_operation_traced {
            trace.record(
                "ptpip.first_operation.received",
                endpoints.clone(),
                Some(&frame),
                Some(format!(
                    "code=0x{:04x};transaction_id={}",
                    req.code, req.transaction_id
                )),
                None,
            );
            first_operation_traced = true;
        }
        if context.disallowed_ops.contains(&req.code) {
            write_reply(
                &mut stream,
                &req,
                Reply::Response(ptp_core::OperationResponse {
                    code: resp::OPERATION_NOT_SUPPORTED,
                    transaction_id: req.transaction_id,
                    params: vec![],
                }),
                context.command_framing,
                &WirePlan::None,
                &metrics,
            )
            .await?;
            continue;
        }
        let data_in = if has_data_in(req.code) {
            collect_data_in(
                &mut stream,
                req.transaction_id,
                context.command_framing,
                &metrics,
            )
            .await?
        } else {
            None
        };
        let _declared_aux_sync = declared_aux_sync.lock().await;
        let (reply, applied_fault, events, is_poll_live_view, observation_context, aux_readiness) = {
            let mut e = engine.lock().await;
            let is_poll_live_view = context.poll_live_view_op == Some(req.code);
            let reply = e.on_operation(&req, data_in.as_deref());
            if reply_is_ok(&reply) {
                let mut owner = session_owner.lock().await;
                match req.code {
                    op::OPEN_SESSION => *owner = Some(session_sequence),
                    op::CLOSE_SESSION => *owner = None,
                    _ => {}
                }
            }
            let applied_fault = e.take_applied_fault();
            let observation_context = ExecutionContext {
                connection: context.connection.clone(),
                mode: e.state().manifest_mode_path().to_string(),
                state: e.phase().state_name().to_string(),
            };
            // Drain under the same lock so the queue is emptied atomically with
            // the op that produced it; forward (broadcast, non-blocking) outside.
            (
                reply,
                applied_fault,
                e.drain_events(),
                is_poll_live_view,
                observation_context,
                declared_aux
                    .iter()
                    .map(|(role, listener)| (Arc::clone(listener), e.channel_ready(*role)))
                    .collect::<Vec<_>>(),
            )
        };
        for (listener, ready) in aux_readiness {
            listener.set_ready(ready).await?;
        }
        drop(_declared_aux_sync);
        let reply = if is_poll_live_view {
            poll_live_view_reply(reply, &frames).await
        } else {
            reply
        };
        let request = PtpRequest {
            framing: observation_framing(context.command_framing).into(),
            operation: format!("0x{:04x}", req.code),
            parameters: req.params.clone(),
            data: data_in.as_deref().map(|bytes| PtpDataPhase {
                direction: DataDirection::HostToCamera,
                payload: camera_config::payload_metadata(bytes),
            }),
        };
        let wire = applied_fault
            .as_ref()
            .map(|fault| &fault.wire)
            .unwrap_or(&WirePlan::None);
        let write_result = write_reply(
            &mut stream,
            &req,
            reply,
            context.command_framing,
            wire,
            &metrics,
        )
        .await;
        if let Some(fault) = &applied_fault {
            trace.record_fault(FaultTraceEvidence {
                endpoints: endpoints.clone(),
                operation: req.code,
                transaction_id: req.transaction_id,
                response_code: write_result
                    .as_ref()
                    .ok()
                    .and_then(|written| written.response.as_ref())
                    .map(|response| response.code.as_str()),
                fault,
                payload: write_result
                    .as_ref()
                    .ok()
                    .and_then(|written| written.payload.as_ref()),
                applied: write_result
                    .as_ref()
                    .ok()
                    .and_then(|written| written.wire_outcome)
                    .map(str::to_string)
                    .unwrap_or_else(|| applied_fault_marker(fault)),
            });
        }
        let (response, outcome, completion, closed, write_error) = match write_result {
            Ok(written) => (
                written.response,
                written.outcome,
                written.completion,
                written.closed,
                None,
            ),
            Err(error) => (
                None,
                TransactionOutcome::TransportAbort,
                None,
                false,
                Some(error),
            ),
        };
        let transaction_record = observations
            .append(observation_context.clone(), |common| {
                ObservationLine::PtpTransaction(Box::new(PtpTransactionRecord {
                    common,
                    transport: PtpTransport::PtpIp,
                    connection_instance: connection_instance.clone(),
                    session: session.clone(),
                    endpoint_set: "command".into(),
                    transaction_id: req.transaction_id,
                    request,
                    response,
                    outcome,
                    evidence_basis: None,
                    observed_effect: None,
                    readback: None,
                }))
            })
            .map_err(|error| std::io::Error::other(error.to_string()))?;
        let transaction_record_id = format!("record-{transaction_record:016x}");
        for event in &events {
            observations
                .append(observation_context.clone(), |common| {
                    ObservationLine::PtpEvent(PtpEventRecord {
                        common,
                        connection_instance: connection_instance.clone(),
                        session: session.clone(),
                        endpoint_set: "event".into(),
                        // Event packets are emitted with the protocol-defined
                        // zero transaction ID. The record link carries the
                        // independently known causal transaction.
                        transaction_id: 0,
                        transaction_record_id: Some(transaction_record_id.clone()),
                        event: format!("0x{:04x}", event.code),
                        parameters: event.params.clone(),
                        payload: None,
                    })
                })
                .map_err(|error| std::io::Error::other(error.to_string()))?;
        }
        // #126: the one hook — nudge the state-callback task that the camera
        // state may have changed. Cheap; a no-op when no push task is running.
        state_dirty.notify_one();
        for event in events {
            // Err = no event-socket client connected; the push is dropped (the
            // completion is only meaningful to a listening client).
            let _ = event_tx.send(event);
        }
        if let Some(error) = write_error {
            return Err(error);
        }
        if let Some(completion) = completion {
            if engine.lock().await.complete_stream(completion) {
                state_dirty.notify_one();
            }
        }
        if closed {
            break;
        }
    }
    drop(standard_guard);
    Ok(())
}

async fn run_knock_loop(
    knock: UdpSocket,
    knock_config: PcssKnock,
    camera_name: String,
    command_port: u16,
    trace: TraceLog,
    mut shutdown: broadcast::Receiver<()>,
) {
    let mut buf = vec![0u8; 2048];
    let knock_local = knock.local_addr().ok().map(|address| address.to_string());
    loop {
        tokio::select! {
            received = knock.recv_from(&mut buf) => {
                let Ok((n, peer)) = received else {
                    continue;
                };
                let Some(discovery) = parse_pcss_discovery(&buf[..n], &knock_config.protocol) else {
                    trace.record(
                        "pcss.discovery.rejected",
                        TraceEndpoints {
                            local: knock_local.clone(),
                            peer: Some(peer.to_string()),
                            target: None,
                        },
                        Some(&buf[..n]),
                        Some("rejected".into()),
                        Some("payload does not match the selected PCSS protocol".into()),
                    );
                    continue;
                };
                let host_matches_peer = discovery
                    .host
                    .parse::<IpAddr>()
                    .is_ok_and(|host| host == peer.ip().to_canonical());
                if !host_matches_peer {
                    trace.record(
                        "pcss.discovery.rejected",
                        TraceEndpoints {
                            local: knock_local.clone(),
                            peer: Some(peer.to_string()),
                            target: None,
                        },
                        Some(&buf[..n]),
                        Some("rejected".into()),
                        Some("HOST does not match the datagram source address".into()),
                    );
                    continue;
                }
                let callback = SocketAddr::new(peer.ip(), knock_config.callback_port);
                let callback_target = callback.to_string();
                trace.record(
                    "pcss.discovery.received",
                    TraceEndpoints {
                        local: knock_local.clone(),
                        peer: Some(peer.to_string()),
                        target: Some(callback_target.clone()),
                    },
                    Some(&buf[..n]),
                    Some("accepted".into()),
                    None,
                );
                let camera_name = camera_name.clone();
                let protocol = knock_config.protocol.clone();
                let trace = trace.clone();
                tokio::spawn(async move {
                    trace.record(
                        "pcss.callback.connect_started",
                        TraceEndpoints {
                            peer: Some(peer.to_string()),
                            target: Some(callback_target.clone()),
                            ..TraceEndpoints::default()
                        },
                        None,
                        Some("started".into()),
                        None,
                    );
                    let mut callback_stream = match TcpStream::connect(callback).await {
                        Ok(stream) => stream,
                        Err(error) => {
                            trace.record(
                                "pcss.callback.connect_failed",
                                TraceEndpoints {
                                    peer: Some(peer.to_string()),
                                    target: Some(callback_target),
                                    ..TraceEndpoints::default()
                                },
                                None,
                                Some("failed".into()),
                                Some(error.to_string()),
                            );
                            return;
                        }
                    };
                    let callback_endpoints = TraceEndpoints {
                        local: callback_stream
                            .local_addr()
                            .ok()
                            .map(|address| address.to_string()),
                        peer: callback_stream
                            .peer_addr()
                            .ok()
                            .map(|address| address.to_string()),
                        target: Some(callback_target),
                    };
                    trace.record(
                        "pcss.callback.connected",
                        callback_endpoints.clone(),
                        None,
                        Some("connected".into()),
                        None,
                    );
                    let Some(camera_address) = callback_stream.local_addr().ok().and_then(|address| {
                        let IpAddr::V4(address) = address.ip() else {
                            return None;
                        };
                        Some(address)
                    }) else {
                        trace.record(
                            "pcss.notify.failed",
                            callback_endpoints,
                            None,
                            Some("failed".into()),
                            Some("callback route did not select an IPv4 address".into()),
                        );
                        return;
                    };
                    let notify = pcss_notify_message(
                        camera_address,
                        &camera_name,
                        command_port,
                        &protocol,
                    );
                    if let Err(error) = callback_stream.write_all(&notify).await {
                        trace.record(
                            "pcss.notify.failed",
                            callback_endpoints,
                            Some(&notify),
                            Some("failed".into()),
                            Some(error.to_string()),
                        );
                        return;
                    }
                    trace.record(
                        "pcss.notify.sent",
                        callback_endpoints.clone(),
                        Some(&notify),
                        Some("sent".into()),
                        None,
                    );
                    let expected_ack = pcss_callback_ack_message();
                    let mut ack = vec![0u8; expected_ack.len()];
                    match read_exact_with_prefix(&mut callback_stream, &mut ack).await {
                        Ok(_) if ack == expected_ack => trace.record(
                            "pcss.callback_ack.received",
                            callback_endpoints,
                            Some(&ack),
                            Some("accepted".into()),
                            None,
                        ),
                        Ok(_) => {
                            trace.record(
                                "pcss.callback_ack.invalid",
                                callback_endpoints,
                                Some(&ack),
                                Some("rejected".into()),
                                Some("callback acknowledgement did not match PCSS".into()),
                            );
                            let _ = callback_stream.shutdown().await;
                        }
                        Err((error, received)) => trace.record(
                            "pcss.callback_ack.failed",
                            callback_endpoints,
                            Some(&ack[..received]),
                            Some("failed".into()),
                            Some(error.to_string()),
                        ),
                    }
                });
            }
            _ = shutdown.recv() => break,
        }
    }
}

fn ptpip_packet_kind(packet: &PtpIpPacket) -> &'static str {
    match packet {
        PtpIpPacket::InitCommandRequest(_) => "InitCommandRequest",
        PtpIpPacket::InitCommandAck(_) => "InitCommandAck",
        PtpIpPacket::InitEventRequest(_) => "InitEventRequest",
        PtpIpPacket::InitEventAck(_) => "InitEventAck",
        PtpIpPacket::InitFail(_) => "InitFail",
        PtpIpPacket::OperationRequest(_) => "OperationRequest",
        PtpIpPacket::OperationResponse(_) => "OperationResponse",
        PtpIpPacket::Event(_) => "Event",
        PtpIpPacket::StartData(_) => "StartData",
        PtpIpPacket::Data(_) => "Data",
        PtpIpPacket::EndData(_) => "EndData",
        PtpIpPacket::ProbeRequest(_) => "ProbeRequest",
        PtpIpPacket::ProbeResponse(_) => "ProbeResponse",
    }
}

async fn read_exact_with_prefix<R: AsyncRead + Unpin>(
    reader: &mut R,
    buffer: &mut [u8],
) -> Result<(), (std::io::Error, usize)> {
    let mut received = 0;
    while received < buffer.len() {
        match reader.read(&mut buffer[received..]).await {
            Ok(0) => {
                return Err((
                    std::io::Error::new(
                        std::io::ErrorKind::UnexpectedEof,
                        "early EOF while reading PCSS callback acknowledgement",
                    ),
                    received,
                ));
            }
            Ok(count) => received += count,
            Err(error) => return Err((error, received)),
        }
    }
    Ok(())
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
/// it disconnects (#54). Each broadcast code uses the selected connection's
/// declared event framing. Mirrors [`stream_liveview`]'s read-half watcher: event
/// clients never send bytes, so a completed read means EOF/reset — the client is
/// gone, and without watching it a never-emitting session would hang the task.
async fn handle_event_conn(
    mut stream: TcpStream,
    mut events: broadcast::Receiver<camera_sim::QueuedEvent>,
    framing: WireFraming,
    metrics: Metrics,
) {
    let (mut rd, mut wr) = stream.split();
    let mut probe = [0u8; 64];
    loop {
        tokio::select! {
            recv = events.recv() => {
                match recv {
                    Ok(event) => {
                        let packet = PtpIpPacket::Event(EventPacket {
                            code: event.code,
                            transaction_id: 0,
                            params: event.params.clone(),
                        });
                        let bytes = match framing {
                            WireFraming::Standard => ptp_core::encode(&packet).ok(),
                            WireFraming::Usb => protocol_primitives::usb_ptp::encode(&packet).ok(),
                            WireFraming::Compressed => unreachable!("compressed event framing is rejected at bind"),
                        };
                        let Some(bytes) = bytes else { break };
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

fn decode_command_frame(framing: WireFraming, frame: &[u8]) -> Result<PtpIpPacket, String> {
    match framing {
        WireFraming::Standard => PtpIpPacket::decode(frame).map_err(|error| error.to_string()),
        WireFraming::Compressed => fuji_framing::decode(frame).map_err(|error| error.to_string()),
        WireFraming::Usb => {
            protocol_primitives::usb_ptp::decode(frame).map_err(|error| error.to_string())
        }
    }
}

fn observation_framing(framing: WireFraming) -> &'static str {
    match framing {
        WireFraming::Standard => "standard",
        WireFraming::Compressed => "compressed",
        WireFraming::Usb => "usb",
    }
}

fn encode_control_frame(framing: WireFraming, packet: &PtpIpPacket) -> std::io::Result<Vec<u8>> {
    match framing {
        WireFraming::Standard => ptp_core::encode(packet).map_err(to_io),
        WireFraming::Compressed => fuji_framing::encode(packet).map_err(to_io),
        WireFraming::Usb => protocol_primitives::usb_ptp::encode(packet).map_err(to_io),
    }
}

fn encode_data_frame(
    framing: WireFraming,
    op: u16,
    transaction_id: u32,
    payload: &[u8],
) -> std::io::Result<Vec<u8>> {
    match framing {
        WireFraming::Standard => {
            let mut bytes = ptp_core::encode(&PtpIpPacket::StartData(ptp_core::StartData {
                transaction_id,
                total_length: payload.len() as u64,
            }))
            .map_err(to_io)?;
            bytes.extend_from_slice(
                &ptp_core::encode(&PtpIpPacket::EndData(ptp_core::DataBlock {
                    transaction_id,
                    payload: payload.to_vec(),
                }))
                .map_err(to_io)?,
            );
            Ok(bytes)
        }
        WireFraming::Compressed => Ok(fuji_framing::encode_data(op, transaction_id, payload)),
        WireFraming::Usb => Ok(protocol_primitives::usb_ptp::encode_data(
            op,
            transaction_id,
            payload,
        )),
    }
}

fn data_in_err<S: Into<String>>(msg: S) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidData, msg.into())
}

/// Collect the data-in payload. Compressed and USB channels carry the whole
/// phase in one type-2 `Data` container. Standard PTP/IP carries StartData,
/// zero or more Data packets, and one EndData packet. Every shape is capped at
/// [`MAX_DATA_IN_BYTES`] and must retain the request transaction id.
async fn collect_data_in(
    stream: &mut TcpStream,
    tid: u32,
    framing: WireFraming,
    metrics: &Metrics,
) -> std::io::Result<Option<Vec<u8>>> {
    let Some(first) = read_frame(stream, metrics).await? else {
        return Err(data_in_err("data-in: stream closed before the data frame"));
    };
    let first = decode_command_frame(framing, &first)
        .map_err(|error| data_in_err(format!("data-in: decode failed: {error}")))?;
    if framing == WireFraming::Standard {
        let PtpIpPacket::StartData(start) = first else {
            return Err(data_in_err(format!(
                "data-in: expected StartData, got {first:?}"
            )));
        };
        if start.transaction_id != tid {
            return Err(data_in_err(format!(
                "data-in: StartData tid {} != request tid {}",
                start.transaction_id, tid
            )));
        }
        if start.total_length > MAX_DATA_IN_BYTES {
            return Err(data_in_err(format!(
                "data-in: declared payload {} exceeds cap {MAX_DATA_IN_BYTES}",
                start.total_length
            )));
        }
        let mut payload = Vec::with_capacity(start.total_length as usize);
        loop {
            let Some(frame) = read_frame(stream, metrics).await? else {
                return Err(data_in_err("data-in: stream closed before EndData"));
            };
            let packet = decode_command_frame(framing, &frame)
                .map_err(|error| data_in_err(format!("data-in: decode failed: {error}")))?;
            let (data, ended) = match packet {
                PtpIpPacket::Data(data) => (data, false),
                PtpIpPacket::EndData(data) => (data, true),
                other => {
                    return Err(data_in_err(format!(
                        "data-in: expected Data or EndData, got {other:?}"
                    )))
                }
            };
            if data.transaction_id != tid {
                return Err(data_in_err(format!(
                    "data-in: data tid {} != request tid {}",
                    data.transaction_id, tid
                )));
            }
            let next_len = payload.len().saturating_add(data.payload.len()) as u64;
            if next_len > start.total_length || next_len > MAX_DATA_IN_BYTES {
                return Err(data_in_err("data-in: payload exceeds declared length"));
            }
            payload.extend_from_slice(&data.payload);
            if ended {
                if payload.len() as u64 != start.total_length {
                    return Err(data_in_err(format!(
                        "data-in: received {} bytes, declared {}",
                        payload.len(),
                        start.total_length
                    )));
                }
                return Ok(Some(payload));
            }
        }
    }
    match first {
        PtpIpPacket::Data(d) => {
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
        other => Err(data_in_err(format!(
            "data-in: expected a data frame, got {other:?}"
        ))),
    }
}

/// How much of a data-phase body to read from the source per socket write. The
/// data phase is a single wire frame, but the body is streamed in 1 MiB reads so
/// peak in-process allocation stays bounded per DESIGN.md ("File downloads use
/// bounded chunk buffers") even for a multi-GB object.
const DATA_CHUNK_BYTES: usize = 1024 * 1024;

#[derive(Debug)]
struct WrittenReply {
    response: Option<PtpResponse>,
    outcome: TransactionOutcome,
    completion: Option<StreamCompletion>,
    payload: Option<PayloadMetadata>,
    closed: bool,
    wire_outcome: Option<&'static str>,
}

async fn write_reply<W: AsyncWrite + Unpin>(
    stream: &mut W,
    req: &OperationRequest,
    reply: Reply,
    framing: WireFraming,
    wire: &WirePlan,
    metrics: &Metrics,
) -> std::io::Result<WrittenReply> {
    let wire_outcome = match &reply {
        Reply::Response(_) => applied_wire_outcome(wire, false, true),
        Reply::Data { .. } | Reply::DataStream { .. } => applied_wire_outcome(wire, true, true),
        Reply::NoResponse | Reply::Close => applied_wire_outcome(wire, false, false),
    };
    match reply {
        Reply::Response(mut resp) => {
            if matches!(wire, WirePlan::CloseBeforeResponse) {
                return Ok(WrittenReply {
                    response: None,
                    outcome: TransactionOutcome::TransportAbort,
                    completion: None,
                    payload: None,
                    closed: true,
                    wire_outcome,
                });
            }
            delay_response(wire).await;
            if matches!(wire, WirePlan::SuppressResponse) {
                return Ok(WrittenReply {
                    response: None,
                    outcome: TransactionOutcome::Timeout,
                    completion: None,
                    payload: None,
                    closed: false,
                    wire_outcome,
                });
            }
            replace_response_transaction_id(wire, &mut resp);
            let observed = PtpResponse {
                code: format!("0x{:04x}", resp.code),
                parameters: resp.params.clone(),
                data: None,
            };
            let outcome = response_outcome(resp.code);
            let bytes = encode_control_frame(framing, &PtpIpPacket::OperationResponse(resp))?;
            stream.write_all(&bytes).await?;
            metrics.record_write(bytes.len());
            Ok(WrittenReply {
                response: Some(observed),
                outcome,
                completion: None,
                payload: None,
                closed: false,
                wire_outcome,
            })
        }
        Reply::Data { data, mut response } => {
            if matches!(wire, WirePlan::CloseBeforeData) {
                return Ok(WrittenReply {
                    response: None,
                    outcome: TransactionOutcome::TransportAbort,
                    completion: None,
                    payload: None,
                    closed: true,
                    wire_outcome,
                });
            }
            let code = response.code;
            let parameters = response.params.clone();
            let transaction_id = wire_transaction_id(wire, req.transaction_id);
            let mut payload = None;
            if !matches!(wire, WirePlan::SuppressData) {
                delay_data(wire).await;
                // The whole data phase is one type-2 frame whose code echoes the op.
                let data_bytes = encode_data_frame(
                    wire_data_framing(wire, framing),
                    req.code,
                    transaction_id,
                    &data,
                )?;
                stream.write_all(&data_bytes).await?;
                metrics.record_write(data_bytes.len());
                payload = Some(camera_config::payload_metadata(&data));
            }
            if matches!(wire, WirePlan::CloseBeforeResponse) {
                return Ok(WrittenReply {
                    response: None,
                    outcome: TransactionOutcome::TransportAbort,
                    completion: None,
                    payload,
                    closed: true,
                    wire_outcome,
                });
            }
            delay_response(wire).await;
            if matches!(wire, WirePlan::SuppressResponse) {
                return Ok(WrittenReply {
                    response: None,
                    outcome: TransactionOutcome::Timeout,
                    completion: None,
                    payload,
                    closed: false,
                    wire_outcome,
                });
            }
            replace_response_transaction_id(wire, &mut response);
            let response_bytes =
                encode_control_frame(framing, &PtpIpPacket::OperationResponse(response))?;
            stream.write_all(&response_bytes).await?;
            metrics.record_write(response_bytes.len());
            Ok(WrittenReply {
                response: Some(PtpResponse {
                    code: format!("0x{code:04x}"),
                    parameters,
                    data: payload.clone().map(|payload| PtpDataPhase {
                        direction: DataDirection::CameraToHost,
                        payload,
                    }),
                }),
                outcome: response_outcome(code),
                completion: None,
                payload,
                closed: false,
                wire_outcome,
            })
        }
        Reply::DataStream {
            source,
            mut response,
            completion,
        } => {
            if matches!(wire, WirePlan::CloseBeforeData) {
                return Ok(WrittenReply {
                    response: None,
                    outcome: TransactionOutcome::TransportAbort,
                    completion: None,
                    payload: None,
                    closed: true,
                    wire_outcome,
                });
            }
            let code = response.code;
            let parameters = response.params.clone();
            let transaction_id = wire_transaction_id(wire, req.transaction_id);
            let payload = if matches!(wire, WirePlan::SuppressData) {
                None
            } else {
                delay_data(wire).await;
                Some(
                    stream_data_phase(
                        stream,
                        wire_data_framing(wire, framing),
                        req.code,
                        transaction_id,
                        &source,
                        metrics,
                    )
                    .await?,
                )
            };
            if matches!(wire, WirePlan::CloseBeforeResponse) {
                return Ok(WrittenReply {
                    response: None,
                    outcome: TransactionOutcome::TransportAbort,
                    completion: None,
                    payload,
                    closed: true,
                    wire_outcome,
                });
            }
            delay_response(wire).await;
            if matches!(wire, WirePlan::SuppressResponse) {
                return Ok(WrittenReply {
                    response: None,
                    outcome: TransactionOutcome::Timeout,
                    completion: None,
                    payload,
                    closed: false,
                    wire_outcome,
                });
            }
            replace_response_transaction_id(wire, &mut response);
            let response_bytes =
                encode_control_frame(framing, &PtpIpPacket::OperationResponse(response))?;
            stream.write_all(&response_bytes).await?;
            metrics.record_write(response_bytes.len());
            Ok(WrittenReply {
                response: Some(PtpResponse {
                    code: format!("0x{code:04x}"),
                    parameters,
                    data: payload.clone().map(|payload| PtpDataPhase {
                        direction: DataDirection::CameraToHost,
                        payload,
                    }),
                }),
                outcome: response_outcome(code),
                completion: completion.filter(|_| !matches!(wire, WirePlan::SuppressData)),
                payload,
                closed: false,
                wire_outcome,
            })
        }
        Reply::NoResponse => Ok(WrittenReply {
            response: None,
            outcome: TransactionOutcome::Timeout,
            completion: None,
            payload: None,
            closed: false,
            wire_outcome,
        }),
        Reply::Close => Ok(WrittenReply {
            response: None,
            outcome: TransactionOutcome::TransportAbort,
            completion: None,
            payload: None,
            closed: true,
            wire_outcome,
        }),
    }
}

fn applied_wire_outcome(
    wire: &WirePlan,
    has_data_phase: bool,
    has_response_phase: bool,
) -> Option<&'static str> {
    match wire {
        WirePlan::CloseBeforeData => Some(if has_data_phase {
            "closedBeforeData"
        } else {
            "noDataPhase"
        }),
        WirePlan::CloseBeforeResponse => Some(if has_response_phase {
            "closedBeforeResponse"
        } else {
            "noResponsePhase"
        }),
        WirePlan::DelayData { .. } => Some(if has_data_phase {
            "delayedData"
        } else {
            "noDataPhase"
        }),
        WirePlan::DelayResponse { .. } => Some(if has_response_phase {
            "delayedResponse"
        } else {
            "noResponsePhase"
        }),
        WirePlan::SuppressData => Some(if has_data_phase {
            "suppressedData"
        } else {
            "noDataPhase"
        }),
        WirePlan::SuppressResponse => Some(if has_response_phase {
            "suppressedResponse"
        } else {
            "noResponsePhase"
        }),
        WirePlan::ReplaceTransactionId(_) => Some(if has_data_phase || has_response_phase {
            "replacedTransactionId"
        } else {
            "noWirePhase"
        }),
        WirePlan::DataFraming(_) => Some(if has_data_phase {
            "changedDataFraming"
        } else {
            "noDataPhase"
        }),
        WirePlan::None => None,
    }
}

fn wire_transaction_id(wire: &WirePlan, session_id: u32) -> u32 {
    match wire {
        WirePlan::ReplaceTransactionId(transaction_id) => *transaction_id,
        _ => session_id,
    }
}

fn replace_response_transaction_id(wire: &WirePlan, response: &mut ptp_core::OperationResponse) {
    if let WirePlan::ReplaceTransactionId(transaction_id) = wire {
        response.transaction_id = *transaction_id;
    }
}

fn wire_data_framing(wire: &WirePlan, session_framing: WireFraming) -> WireFraming {
    match wire {
        WirePlan::DataFraming(framing) => *framing,
        _ => session_framing,
    }
}

async fn delay_data(wire: &WirePlan) {
    if let WirePlan::DelayData { ms } = wire {
        tokio::time::sleep(std::time::Duration::from_millis(*ms)).await;
    }
}

async fn delay_response(wire: &WirePlan) {
    if let WirePlan::DelayResponse { ms } = wire {
        tokio::time::sleep(std::time::Duration::from_millis(*ms)).await;
    }
}

fn applied_fault_marker(fault: &AppliedFault) -> String {
    match fault.wire {
        WirePlan::CloseBeforeData => "closedBeforeData",
        WirePlan::CloseBeforeResponse => "closedBeforeResponse",
        WirePlan::DelayData { .. } => "delayedData",
        WirePlan::DelayResponse { .. } => "delayedResponse",
        WirePlan::SuppressData => "suppressedData",
        WirePlan::SuppressResponse => "suppressedResponse",
        WirePlan::ReplaceTransactionId(_) => "replacedTransactionId",
        WirePlan::DataFraming(_) => "changedDataFraming",
        WirePlan::None => match fault.kind.as_str() {
            "failResponse" => "failedResponse",
            "close" => "closedBeforeCommand",
            "truncateData" => "truncatedData",
            "replaceData" => "replacedData",
            "propertyReadback" => "replacedPropertyReadback",
            _ => "applied",
        },
    }
    .to_string()
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
    framing: WireFraming,
    op: u16,
    transaction_id: u32,
    source: &ByteSource,
    metrics: &Metrics,
) -> std::io::Result<PayloadMetadata> {
    let total_length = source.len();
    let payload_len = u32::try_from(total_length)
        .map_err(|_| std::io::Error::other("data-phase payload exceeds a single frame (u32)"))?;
    if framing == WireFraming::Standard {
        let start = ptp_core::encode(&PtpIpPacket::StartData(ptp_core::StartData {
            transaction_id,
            total_length,
        }))
        .map_err(to_io)?;
        stream.write_all(&start).await?;
        metrics.record_write(start.len());
    }
    let header = match framing {
        WireFraming::Standard => {
            let mut header = [0u8; 12];
            header[0..4].copy_from_slice(&(payload_len + 12).to_le_bytes());
            header[4..8].copy_from_slice(&12u32.to_le_bytes()); // PTP/IP EndData
            header[8..12].copy_from_slice(&transaction_id.to_le_bytes());
            header
        }
        WireFraming::Compressed | WireFraming::Usb => {
            fuji_framing::data_frame_header(op, transaction_id, payload_len)
        }
    };
    stream.write_all(&header).await?;
    metrics.record_write(header.len());

    let mut offset: u64 = 0;
    let mut metadata = PayloadMetadataBuilder::new();
    while offset < total_length {
        let take = ((total_length - offset) as usize).min(DATA_CHUNK_BYTES);
        let chunk = source
            .read_chunk(offset, take)
            .map_err(|e| std::io::Error::other(e.to_string()))?;
        if chunk.is_empty() {
            break;
        }
        stream.write_all(&chunk).await?;
        metrics.record_write(chunk.len());
        metadata.update(&chunk);
        offset += chunk.len() as u64;
    }
    if offset != total_length {
        return Err(std::io::Error::new(
            std::io::ErrorKind::UnexpectedEof,
            "stream source ended before its declared length",
        ));
    }
    Ok(metadata.metadata())
}

fn to_io<E: std::fmt::Display>(e: E) -> std::io::Error {
    std::io::Error::other(e.to_string())
}

fn invalid_config<S: Into<String>>(msg: S) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidInput, msg.into())
}

fn fixed_manifest_guid(manifest: &CameraManifest, key: &str) -> std::io::Result<[u8; 16]> {
    let Some(ValuePolicy::Fixed {
        value: serde_yaml::Value::String(value),
    }) = manifest.values.get(key)
    else {
        return Err(invalid_config(format!(
            "init responder GUID value '{key}' is not a fixed string"
        )));
    };
    let bytes = parse_hex_bytes(value)
        .and_then(|bytes| <[u8; 16]>::try_from(bytes).ok())
        .ok_or_else(|| {
            invalid_config(format!("init responder GUID value '{key}' is not 16 bytes"))
        })?;
    Ok(bytes)
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

    #[test]
    fn declared_aux_socket_enables_reuseaddr() {
        let socket = bind_declared_socket("127.0.0.1:0".parse().unwrap()).unwrap();

        assert!(socket.reuseaddr().unwrap());
    }

    #[tokio::test]
    async fn exact_read_reports_only_the_received_prefix() {
        let (mut writer, mut reader) = tokio::io::duplex(32);
        writer.write_all(b"HTTP").await.unwrap();
        writer.shutdown().await.unwrap();
        let mut buffer = [0u8; 18];

        let (error, received) = read_exact_with_prefix(&mut reader, &mut buffer)
            .await
            .unwrap_err();

        assert_eq!(error.kind(), std::io::ErrorKind::UnexpectedEof);
        assert_eq!(received, 4);
        assert_eq!(&buffer[..received], b"HTTP");
    }

    #[test]
    fn packet_kind_does_not_format_large_data_payloads() {
        let packet = PtpIpPacket::Data(ptp_core::DataBlock {
            transaction_id: 1,
            payload: vec![0xab; 4 * 1024 * 1024],
        });

        assert_eq!(ptpip_packet_kind(&packet), "Data");
    }

    #[tokio::test]
    async fn failed_event_ack_attachment_can_be_replaced_without_closing_command() {
        let connections = Arc::new(StandardConnections::default());
        let (connection_number, mut command_cancel) = connections.allocate();

        let (_event_cancel, mut attach_guard) = connections
            .attach(connection_number)
            .expect("first event attachment");
        let mut writer = FailAfter { remaining: 0 };
        assert_eq!(
            acknowledge_standard_event(&mut writer, &Metrics::default(), &mut attach_guard)
                .await
                .unwrap_err()
                .kind(),
            std::io::ErrorKind::BrokenPipe
        );
        drop(attach_guard); // matches the handler returning through `?`

        assert!(command_cancel.try_recv().is_err());
        let (_replacement_cancel, mut replacement_guard) = connections
            .attach(connection_number)
            .expect("replacement event attachment");
        replacement_guard.disarm();
        assert!(connections.attach(connection_number).is_none());
        assert!(command_cancel.try_recv().is_err());
    }

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
        let result = write_reply(
            &mut writer,
            &req,
            reply,
            WireFraming::Compressed,
            &WirePlan::None,
            &Metrics::default(),
        )
        .await;
        assert_eq!(result.unwrap_err().kind(), std::io::ErrorKind::BrokenPipe);
    }

    #[tokio::test]
    async fn successful_bounded_stream_metadata_matches_delivered_bytes() {
        let length = DATA_CHUNK_BYTES as u64 + 17;
        let source = ByteSource::Generated {
            len: length,
            seed: 0x5a,
        };
        let req = OperationRequest {
            data_phase_info: 1,
            code: op::GET_OBJECT,
            transaction_id: 9,
            params: vec![1],
        };
        let reply = Reply::DataStream {
            source: source.clone(),
            response: ptp_core::OperationResponse {
                code: resp::OK,
                transaction_id: 9,
                params: Vec::new(),
            },
            completion: None,
        };
        let mut delivered = Vec::new();

        let written = write_reply(
            &mut delivered,
            &req,
            reply,
            WireFraming::Compressed,
            &WirePlan::None,
            &Metrics::default(),
        )
        .await
        .unwrap();

        assert_eq!(written.outcome, TransactionOutcome::Ok);
        let metadata = written.response.unwrap().data.unwrap().payload;
        let body_end = 12 + length as usize;
        assert_eq!(delivered.len(), body_end + 12);
        assert_eq!(metadata.length, length);
        assert_eq!(
            metadata,
            camera_config::payload_metadata(&delivered[12..body_end])
        );
        assert_eq!(delivered[12..body_end], source.read().unwrap());
    }

    fn data_reply(transaction_id: u32) -> (OperationRequest, Reply) {
        (
            OperationRequest {
                data_phase_info: 1,
                code: op::GET_DEVICE_PROP_VALUE,
                transaction_id,
                params: vec![0x5007],
            },
            Reply::Data {
                data: vec![0xaa, 0xbb],
                response: ptp_core::OperationResponse {
                    code: resp::OK,
                    transaction_id,
                    params: Vec::new(),
                },
            },
        )
    }

    #[tokio::test]
    async fn wire_plans_change_only_the_selected_data_or_response_phase() {
        let metrics = Metrics::default();

        let (req, reply) = data_reply(7);
        let mut delivered = Vec::new();
        let written = write_reply(
            &mut delivered,
            &req,
            reply,
            WireFraming::Compressed,
            &WirePlan::SuppressData,
            &metrics,
        )
        .await
        .unwrap();
        assert!(written.response.unwrap().data.is_none());
        assert!(matches!(
            fuji_framing::decode(&delivered).unwrap(),
            PtpIpPacket::OperationResponse(response) if response.transaction_id == 7
        ));

        let (req, reply) = data_reply(7);
        let mut delivered = Vec::new();
        let written = write_reply(
            &mut delivered,
            &req,
            reply,
            WireFraming::Compressed,
            &WirePlan::SuppressResponse,
            &metrics,
        )
        .await
        .unwrap();
        assert_eq!(written.outcome, TransactionOutcome::Timeout);
        assert!(written.response.is_none());
        assert!(matches!(
            fuji_framing::decode(&delivered).unwrap(),
            PtpIpPacket::Data(data) if data.payload == [0xaa, 0xbb]
        ));

        for (wire, expected_len) in [
            (WirePlan::CloseBeforeData, 0),
            (WirePlan::CloseBeforeResponse, 14),
        ] {
            let (req, reply) = data_reply(7);
            let mut delivered = Vec::new();
            let written = write_reply(
                &mut delivered,
                &req,
                reply,
                WireFraming::Compressed,
                &wire,
                &metrics,
            )
            .await
            .unwrap();
            assert!(written.closed);
            assert_eq!(written.outcome, TransactionOutcome::TransportAbort);
            assert_eq!(delivered.len(), expected_len);
        }

        let (req, reply) = data_reply(7);
        let mut delivered = Vec::new();
        write_reply(
            &mut delivered,
            &req,
            reply,
            WireFraming::Compressed,
            &WirePlan::ReplaceTransactionId(99),
            &metrics,
        )
        .await
        .unwrap();
        let data_len = u32::from_le_bytes(delivered[0..4].try_into().unwrap()) as usize;
        assert!(matches!(
            fuji_framing::decode(&delivered[..data_len]).unwrap(),
            PtpIpPacket::Data(data) if data.transaction_id == 99
        ));
        assert!(matches!(
            fuji_framing::decode(&delivered[data_len..]).unwrap(),
            PtpIpPacket::OperationResponse(response) if response.transaction_id == 99
        ));

        let (req, reply) = data_reply(7);
        let mut delivered = Vec::new();
        write_reply(
            &mut delivered,
            &req,
            reply,
            WireFraming::Compressed,
            &WirePlan::DataFraming(WireFraming::Standard),
            &metrics,
        )
        .await
        .unwrap();
        assert!(matches!(
            PtpIpPacket::decode(&delivered[..20]).unwrap(),
            PtpIpPacket::StartData(start) if start.transaction_id == 7
        ));
        assert!(matches!(
            fuji_framing::decode(&delivered[34..]).unwrap(),
            PtpIpPacket::OperationResponse(response) if response.transaction_id == 7
        ));

        let (req, reply) = data_reply(7);
        let started = std::time::Instant::now();
        write_reply(
            &mut Vec::new(),
            &req,
            reply,
            WireFraming::Compressed,
            &WirePlan::DelayResponse { ms: 5 },
            &metrics,
        )
        .await
        .unwrap();
        assert!(started.elapsed() >= std::time::Duration::from_millis(5));
    }

    #[tokio::test]
    async fn no_fault_keeps_engine_response_transaction_ids_for_data_replies() {
        let req = OperationRequest {
            data_phase_info: 1,
            code: op::GET_DEVICE_PROP_VALUE,
            transaction_id: 7,
            params: vec![0x5007],
        };
        let replies = [
            Reply::Data {
                data: vec![0xaa, 0xbb],
                response: ptp_core::OperationResponse {
                    code: resp::OK,
                    transaction_id: 41,
                    params: Vec::new(),
                },
            },
            Reply::DataStream {
                source: ByteSource::Memory(vec![0xaa, 0xbb]),
                response: ptp_core::OperationResponse {
                    code: resp::OK,
                    transaction_id: 42,
                    params: Vec::new(),
                },
                completion: None,
            },
        ];

        for (reply, expected_response_id) in replies.into_iter().zip([41, 42]) {
            let mut delivered = Vec::new();
            write_reply(
                &mut delivered,
                &req,
                reply,
                WireFraming::Compressed,
                &WirePlan::None,
                &Metrics::default(),
            )
            .await
            .unwrap();

            let data_len = u32::from_le_bytes(delivered[0..4].try_into().unwrap()) as usize;
            assert!(matches!(
                fuji_framing::decode(&delivered[..data_len]).unwrap(),
                PtpIpPacket::Data(data) if data.transaction_id == req.transaction_id
            ));
            assert!(matches!(
                fuji_framing::decode(&delivered[data_len..]).unwrap(),
                PtpIpPacket::OperationResponse(response)
                    if response.transaction_id == expected_response_id
            ));
        }
    }

    #[tokio::test]
    async fn failed_stream_source_cannot_return_success_or_completion() {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let missing = std::env::temp_dir().join(format!(
            "ptpsim-missing-stream-source-{}-{nonce}",
            std::process::id(),
        ));
        let req = OperationRequest {
            data_phase_info: 1,
            code: op::GET_OBJECT,
            transaction_id: 10,
            params: vec![1],
        };
        let reply = Reply::DataStream {
            source: ByteSource::FileRange {
                path: missing,
                offset: 0,
                len: 32,
            },
            response: ptp_core::OperationResponse {
                code: resp::OK,
                transaction_id: 10,
                params: Vec::new(),
            },
            completion: None,
        };
        let mut delivered = Vec::new();

        let result = write_reply(
            &mut delivered,
            &req,
            reply,
            WireFraming::Compressed,
            &WirePlan::None,
            &Metrics::default(),
        )
        .await;

        assert_eq!(result.unwrap_err().kind(), std::io::ErrorKind::Other);
        assert_eq!(
            delivered.len(),
            12,
            "only the declared frame header was sent"
        );
    }
}
