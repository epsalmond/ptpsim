use std::collections::VecDeque;
use std::io;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use bytes::BytesMut;
use camera_protocol_ffi::{
    build_command, parse_event, parse_response, ConfigStore, KeyValue, PcssDiscoveryTarget,
    PcssNotifyInfo, Platform, PtpExecutorTransport, PtpFraming, PtpSessionOpenResult,
    PtpStreamingTransport, PtpTransportError, ResponseFrame, SocketRole,
};
use if_addrs::{get_if_addrs, IfAddr};
use ptp_core::codes::{op, resp};
use ptp_core::{DataBlock, DeviceInfo, InitEventRequest, ProbeRequest, PtpCodec, PtpIpPacket};
use serde_json::json;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::{TcpListener, TcpStream, UdpSocket};
use tokio::sync::{Mutex, RwLock};

use crate::TraceWriter;

const INIT_FRAME_LIMIT: usize = 1024 * 1024;
const EVENT_QUEUE_LIMIT: usize = 1024;
const PCSS_CALLBACK_LIMIT: usize = 4096;
const GET_VENDOR_CODES: u16 = 0x9439;
/// SnapBridge's startup query asks 0x9439 for vendor operation codes. The
/// operation also accepts selector 13 for vendor properties, but that is not
/// part of the direct-camera startup sequence.
const GET_VENDOR_CODES_DOMAIN: u32 = 0x0000_0009;

#[derive(Debug, Clone, Copy)]
struct PcssEndpoint {
    camera_ip: Ipv4Addr,
    local_ip: Ipv4Addr,
    command_port: u16,
}

struct PcssConnectedTarget {
    stream: TcpStream,
    init_packet: Vec<u8>,
    recovery: Option<PcssRecovery>,
}

struct PcssRecovery {
    callback: TcpListener,
    camera_ip: Ipv4Addr,
    identity_guid: Vec<u8>,
    friendly_name: String,
}

struct EstablishConnectedError {
    error: PtpTransportError,
    endpoint_unavailable: bool,
}

impl EstablishConnectedError {
    fn endpoint(error: PtpTransportError) -> Self {
        Self {
            error,
            endpoint_unavailable: true,
        }
    }

    fn protocol(error: PtpTransportError) -> Self {
        Self {
            error,
            endpoint_unavailable: false,
        }
    }
}

#[derive(Debug, Clone)]
struct PcssInterface {
    name: String,
    local_ip: Ipv4Addr,
    broadcast: Ipv4Addr,
}

#[derive(Debug, Clone)]
pub struct TransportConfig {
    pub camera: Option<IpAddr>,
    pub interface: Option<String>,
    pub connection: String,
    pub runtime_scope: Vec<(String, String)>,
    pub connect_timeout: Duration,
    pub max_frame_bytes: usize,
}

pub struct NativePtpTransport {
    store: Arc<ConfigStore>,
    config: TransportConfig,
    framing: PtpFraming,
    init_shape: String,
    command_port: u16,
    event_port: Option<u16>,
    live_view_port: Option<u16>,
    active: RwLock<Option<Arc<CommandChannel>>>,
    event: Mutex<Option<EventChannel>>,
    live_view: Mutex<Option<PassiveChannel>>,
    trace: Arc<TraceWriter>,
    stream_trace: StdMutex<StreamTrace>,
    session_poisoned: Arc<AtomicBool>,
}

impl NativePtpTransport {
    pub fn new(
        store: Arc<ConfigStore>,
        config: TransportConfig,
        trace: Arc<TraceWriter>,
    ) -> Result<Arc<Self>, PtpTransportError> {
        let platform = current_platform();
        let info = store
            .connections(platform)
            .into_iter()
            .find(|candidate| candidate.id == config.connection)
            .ok_or_else(|| {
                failed(format!(
                    "unknown or unavailable connection '{}'",
                    config.connection
                ))
            })?;
        let framing = info.command_framing.ok_or_else(|| {
            failed(format!(
                "connection '{}' has no command framing",
                config.connection
            ))
        })?;
        let init_shape = info.init_shape.ok_or_else(|| {
            failed(format!(
                "connection '{}' has no init shape",
                config.connection
            ))
        })?;
        if init_shape != "pcssKnock" && config.camera.is_none() {
            return Err(failed(format!(
                "connection '{}' requires --camera",
                config.connection
            )));
        }
        if config.camera.is_some() && config.interface.is_some() {
            return Err(failed(
                "--interface selects subnet-broadcast discovery and cannot be combined with --camera",
            ));
        }
        let bindings = store.socket_bindings(config.connection.clone());
        let command_port = bindings
            .iter()
            .find(|binding| binding.role == SocketRole::Command)
            .map(|binding| binding.port)
            .ok_or_else(|| failed("connection has no command socket binding"))?;
        let event_port = bindings
            .iter()
            .find(|binding| binding.role == SocketRole::Event)
            .map(|binding| binding.port);
        let live_view_port = bindings
            .iter()
            .find(|binding| binding.role == SocketRole::LiveView)
            .map(|binding| binding.port);
        Ok(Arc::new(Self {
            store,
            config,
            framing,
            init_shape,
            command_port,
            event_port,
            live_view_port,
            active: RwLock::new(None),
            event: Mutex::new(None),
            live_view: Mutex::new(None),
            trace,
            stream_trace: StdMutex::new(StreamTrace::default()),
            session_poisoned: Arc::new(AtomicBool::new(false)),
        }))
    }

    pub fn command_endpoint(&self) -> Option<SocketAddr> {
        self.config
            .camera
            .map(|camera| SocketAddr::new(camera, self.command_port))
    }

    pub fn command_framing(&self) -> PtpFraming {
        self.framing
    }

    pub async fn open_command_session(&self) -> Result<PtpSessionOpenResult, PtpTransportError> {
        self.prepare_command_session().await?;
        let (channel, opened) = if self.init_shape == "pcssKnock" {
            let target = self.pcss_connected_target().await?;
            let PcssConnectedTarget {
                stream,
                init_packet,
                recovery,
            } = target;
            match self.establish_observed(stream, init_packet).await {
                Ok(opened) => opened,
                Err(error) if error.endpoint_unavailable && recovery.is_some() => {
                    let recovery = recovery.expect("recovery was checked as present");
                    self.trace
                        .session(
                            "pcssRediscovery",
                            json!({
                                "reason": "commandSessionUnavailable",
                                "camera": recovery.camera_ip.to_string(),
                                "error": error.error.to_string(),
                            }),
                        )
                        .map_err(trace_error)?;
                    // The failed channel is gone. A cancelled or failed write
                    // deliberately poisoned that channel, but rediscovery
                    // creates a wholly new command session.
                    self.session_poisoned.store(false, Ordering::Release);
                    let recovered = self.pcss_recovered_target(recovery).await?;
                    self.establish_observed(recovered.stream, recovered.init_packet)
                        .await
                        .map_err(|error| error.error)?
                }
                Err(error) => return Err(error.error),
            }
        } else {
            let (endpoint, init_packet) = self.direct_command_target()?;
            let stream = self.connect(endpoint).await?;
            self.establish_observed(stream, init_packet)
                .await
                .map_err(|error| error.error)?
        };
        self.install_command_session(channel, opened).await
    }

    /// Wait for the handoff's replacement listener without consuming its first
    /// accepted socket as a throwaway readiness probe. Only TCP connection
    /// failures retry; once a socket connects, terminal init/protocol failures
    /// escape from that single establishment attempt.
    pub async fn open_command_session_after_handoff(
        &self,
        deadline: Instant,
    ) -> Result<PtpSessionOpenResult, PtpTransportError> {
        self.prepare_command_session().await?;
        let (endpoint, init_packet) = self.direct_command_target()?;
        let stream = self.connect_until(endpoint, deadline).await?;
        let (channel, opened) = self
            .establish_observed(stream, init_packet)
            .await
            .map_err(|error| error.error)?;
        self.install_command_session(channel, opened).await
    }

    async fn prepare_command_session(&self) -> Result<(), PtpTransportError> {
        if self.active.read().await.is_some() {
            return Err(failed("command session is already open"));
        }
        self.session_poisoned.store(false, Ordering::Release);
        *self
            .stream_trace
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = StreamTrace::default();
        Ok(())
    }

    async fn establish_observed(
        &self,
        stream: TcpStream,
        init_packet: Vec<u8>,
    ) -> Result<(Arc<CommandChannel>, PtpSessionOpenResult), EstablishConnectedError> {
        self.trace
            .begin_ptp_session()
            .map_err(trace_error)
            .map_err(EstablishConnectedError::protocol)?;
        self.establish_connected(stream, init_packet).await
    }

    async fn install_command_session(
        &self,
        channel: Arc<CommandChannel>,
        opened: PtpSessionOpenResult,
    ) -> Result<PtpSessionOpenResult, PtpTransportError> {
        self.trace
            .session(
                "opened",
                json!({
                    "connection": self.config.connection,
                    "transactionId": opened.transaction_id,
                    "response": opened.response_code,
                }),
            )
            .map_err(trace_error)?;
        *self.active.write().await = Some(channel);
        Ok(opened)
    }

    pub async fn open_auxiliary(&self, include_live_view: bool) -> Result<(), PtpTransportError> {
        if self.event_port.is_some() {
            self.open_manifest_channel(SocketRole::Event).await?;
        }
        if include_live_view && self.live_view_port.is_some() {
            self.open_manifest_channel(SocketRole::LiveView).await?;
        }
        Ok(())
    }

    async fn open_manifest_channel(&self, role: SocketRole) -> Result<(), PtpTransportError> {
        let camera = self.config.camera.ok_or_else(|| {
            failed(format!(
                "{role:?} socket requires a configured camera address"
            ))
        })?;
        match role {
            SocketRole::Command => Err(failed(
                "openChannel cannot open the command channel after session start",
            )),
            SocketRole::Event => {
                let port = self
                    .event_port
                    .ok_or_else(|| failed("connection has no event socket binding"))?;
                let mut event = self.event.lock().await;
                if event.is_some() {
                    return Ok(());
                }
                let stream = self.connect(SocketAddr::new(camera, port)).await?;
                let (reader, writer) = stream.into_split();
                *event = Some(EventChannel {
                    reader: Mutex::new(FrameReader::new(reader)),
                    writer: Mutex::new(writer),
                    retained: Mutex::new(VecDeque::new()),
                    framing: self
                        .store
                        .connections(current_platform())
                        .into_iter()
                        .find(|candidate| candidate.id == self.config.connection)
                        .and_then(|candidate| candidate.event_framing)
                        .unwrap_or(self.framing),
                });
                self.trace
                    .session("eventConnected", json!({ "port": port }))
                    .map_err(trace_error)
            }
            SocketRole::LiveView => {
                let port = self
                    .live_view_port
                    .ok_or_else(|| failed("connection has no live-view socket binding"))?;
                let mut live_view = self.live_view.lock().await;
                if live_view.is_some() {
                    return Ok(());
                }
                let stream = self.connect(SocketAddr::new(camera, port)).await?;
                let (reader, writer) = stream.into_split();
                *live_view = Some(PassiveChannel {
                    reader: FrameReader::new(reader),
                    _writer: writer,
                });
                self.trace
                    .session("liveViewConnected", json!({ "port": port }))
                    .map_err(trace_error)
            }
        }
    }

    pub async fn confirm_live_view_frame(&self) -> Result<usize, PtpTransportError> {
        let mut guard = self.live_view.lock().await;
        let channel = guard
            .as_mut()
            .ok_or_else(|| failed("connection has no open live-view socket"))?;
        let frame = tokio::time::timeout(
            self.config.connect_timeout,
            channel.reader.read_frame(self.config.max_frame_bytes),
        )
        .await
        .map_err(|_| timeout_error("first live-view frame"))?
        .map_err(io_error)?;
        protocol_primitives::liveview::parse_frame(&frame)
            .filter(|jpeg| jpeg.starts_with(&[0xff, 0xd8]))
            .ok_or_else(|| failed("first live-view packet is not a valid JPEG frame"))?;
        self.trace.live_view(frame.len()).map_err(trace_error)?;
        Ok(frame.len())
    }

    /// Send a standard PTP/IP probe on the initialized event channel and wait
    /// for its paired response.
    pub async fn probe_event_channel(&self) -> Result<(), PtpTransportError> {
        let event_guard = self.event.lock().await;
        let channel = event_guard
            .as_ref()
            .ok_or_else(|| failed("event socket is not connected"))?;
        if !matches!(channel.framing, PtpFraming::Standard) {
            return Err(failed("probes require standard PTP/IP event framing"));
        }
        let request = ptp_core::encode(&PtpIpPacket::ProbeRequest(ProbeRequest))
            .map_err(|error| failed(error.to_string()))?;
        self.trace
            .wire("tx", "event", &request)
            .map_err(trace_error)?;
        channel
            .writer
            .lock()
            .await
            .write_all(&request)
            .await
            .map_err(io_error)?;
        loop {
            let response = tokio::time::timeout(
                self.config.connect_timeout,
                channel
                    .reader
                    .lock()
                    .await
                    .read_frame(self.config.max_frame_bytes),
            )
            .await
            .map_err(|_| timeout_error("probe response"))?
            .map_err(io_error)?;
            self.trace
                .wire("rx", "event", &response)
                .map_err(trace_error)?;
            match PtpIpPacket::decode(&response) {
                Ok(PtpIpPacket::ProbeResponse(_)) => return Ok(()),
                Ok(PtpIpPacket::Event(_)) => {
                    let mut retained = channel.retained.lock().await;
                    if retained.len() >= EVENT_QUEUE_LIMIT {
                        return Err(failed("unrelated event queue overflow"));
                    }
                    retained.push_back(response);
                }
                _ => return Err(failed("event socket returned a malformed ProbeResponse")),
            }
        }
    }

    pub async fn close_session_if_open(&self) -> Result<(), PtpTransportError> {
        let Some(channel) = self.active.read().await.clone() else {
            return Ok(());
        };
        let orderly = !self.session_poisoned.load(Ordering::Acquire);
        let transport_close_frame = if orderly {
            self.store
                .transport_close(self.config.connection.clone())
                .map_err(|error| failed(error.to_string()))?
                .and_then(|close| {
                    (close.when.as_deref() == Some("feature-exit")).then_some(close.packet)
                })
        } else {
            None
        };
        let close_result = if orderly {
            async {
                let transaction_id = channel.next_tid.fetch_add(1, Ordering::AcqRel);
                let frame =
                    build_command(self.framing, op::CLOSE_SESSION, transaction_id, Vec::new())
                        .map_err(|error| failed(error.to_string()))?;
                self.timed_write(&channel, frame, "cleanup", "CloseSession write")
                    .await?;
                let response = self
                    .timed_read(
                        &channel,
                        self.config.max_frame_bytes,
                        "cleanup",
                        "CloseSession response",
                    )
                    .await?;
                let parsed = parse_response(self.framing, response)
                    .map_err(|error| failed(error.to_string()))?;
                if parsed.txn != transaction_id {
                    return Err(failed(format!(
                        "CloseSession response transaction {} != {transaction_id}",
                        parsed.txn
                    )));
                }
                if parsed.response_code != resp::OK {
                    return Err(failed(format!(
                        "CloseSession returned 0x{:04x}",
                        parsed.response_code
                    )));
                }
                Ok(())
            }
            .await
        } else {
            Ok(())
        };
        let channel_result = self.close_command_channel(transport_close_frame).await;
        close_result.and(channel_result)
    }

    pub async fn endpoint_accepts_tcp(&self) -> bool {
        let Some(endpoint) = self.command_endpoint() else {
            return false;
        };
        tokio::time::timeout(Duration::from_millis(500), TcpStream::connect(endpoint))
            .await
            .is_ok_and(|result| result.is_ok())
    }

    fn direct_command_target(&self) -> Result<(SocketAddr, Vec<u8>), PtpTransportError> {
        match self.init_shape.as_str() {
            "app82" => {
                let init = self
                    .store
                    .connection_init_with_runtime(
                        self.config.connection.clone(),
                        self.runtime_key_values(),
                    )
                    .ok_or_else(|| failed("reference app init identity could not be resolved"))?;
                let max_name_units = init
                    .name_field_byte_count
                    .saturating_div(2)
                    .saturating_sub(1);
                let name_units = init.friendly_name.encode_utf16().count();
                if init.friendly_name.contains('\0') || name_units > max_name_units as usize {
                    return Err(failed(format!(
                        "reference app friendly name must contain at most {max_name_units} UTF-16 units and no NUL"
                    )));
                }
                Ok((
                    self.command_endpoint()
                        .ok_or_else(|| failed("reference app requires a camera address"))?,
                    init.packet,
                ))
            }
            "legacyApp82" => {
                let endpoint = self
                    .command_endpoint()
                    .ok_or_else(|| failed("legacy manufacturer app requires a camera address"))?;
                let camera = match endpoint.ip() {
                    IpAddr::V4(camera) => camera,
                    IpAddr::V6(_) => {
                        return Err(failed("legacy manufacturer app requires an IPv4 camera address"))
                    }
                };
                let local_ip = route_selected_ipv4(camera)?;
                let mut runtime = self.runtime_key_values();
                runtime.push(KeyValue {
                    key: "clientIpv4".into(),
                    value: local_ip.to_string(),
                });
                let init = self
                    .store
                    .connection_init_with_runtime(self.config.connection.clone(), runtime)
                    .ok_or_else(|| failed("legacy manufacturer app init identity could not be resolved"))?;
                Ok((endpoint, init.packet))
            }
            "standardPtpIp" => {
                let init = self
                    .store
                    .connection_init_with_runtime(
                        self.config.connection.clone(),
                        self.runtime_key_values(),
                    )
                    .ok_or_else(|| failed("standard PTP/IP init identity could not be resolved"))?;
                Ok((
                    self.command_endpoint()
                        .ok_or_else(|| failed("standard PTP/IP requires a camera address"))?,
                    init.packet,
                ))
            }
            "pcssKnock" => Err(failed(
                "PCSS establishment requires discovery before selecting an endpoint",
            )),
            other => Err(failed(format!("unsupported init shape '{other}'"))),
        }
    }

    async fn establish_connected(
        &self,
        stream: TcpStream,
        init_packet: Vec<u8>,
    ) -> Result<(Arc<CommandChannel>, PtpSessionOpenResult), EstablishConnectedError> {
        let channel = Arc::new(CommandChannel::new(
            stream,
            Arc::clone(&self.session_poisoned),
        ));
        let retry = self
            .store
            .connection_init_retry_policy(self.config.connection.clone());
        let mut retries_used = 0;
        let init_reply = loop {
            self.establishment_write(
                &channel,
                init_packet.clone(),
                "init",
                "init request write",
                retries_used == 0,
            )
            .await?;
            let reply = self
                .establishment_read(
                    &channel,
                    INIT_FRAME_LIMIT,
                    "init",
                    "init response",
                    retries_used == 0,
                )
                .await?;
            let expected_responder_guid = self
                .store
                .connection_expected_responder_guid(self.config.connection.clone())
                .unwrap_or_default();
            if init_ack_is_valid(&self.init_shape, &reply, &expected_responder_guid) {
                break reply;
            }
            let reason = match canonical_init_fail_reason(&reply) {
                Some(reason) => reason,
                None => {
                    return Err(EstablishConnectedError::protocol(failed(
                        "camera returned a malformed init response",
                    )))
                }
            };
            let Some(policy) = &retry else {
                return Err(EstablishConnectedError::protocol(failed(format!(
                    "camera rejected init with reason 0x{reason:08x}"
                ))));
            };
            if !policy.when_reasons.contains(&reason) || retries_used >= policy.max_retries {
                return Err(EstablishConnectedError::protocol(failed(format!(
                    "camera rejected init with terminal reason 0x{reason:08x}"
                ))));
            }
            retries_used += 1;
            self.trace
                .session(
                    "initRetry",
                    json!({ "reason": reason, "retry": retries_used, "limit": policy.max_retries }),
                )
                .map_err(trace_error)
                .map_err(EstablishConnectedError::protocol)?;
            tokio::time::sleep(Duration::from_millis(policy.backoff_ms as u64)).await;
        };

        let discover_vendor_codes = if self.init_shape == "standardPtpIp" {
            let connection_number = standard_connection_number(&init_reply).ok_or_else(|| {
                EstablishConnectedError::protocol(failed(
                    "standard InitCommandAck omitted its connection number",
                ))
            })?;
            self.open_standard_event_channel(connection_number)
                .await
                .map_err(EstablishConnectedError::protocol)?;
            let device_info = self
                .standard_get_device_info(&channel)
                .await
                .map_err(EstablishConnectedError::protocol)?;
            self.trace
                .session(
                    "deviceInfo",
                    json!({
                        "model": device_info.model,
                        "manufacturer": device_info.manufacturer,
                        "operationsSupported": device_info.operations_supported,
                    }),
                )
                .map_err(trace_error)
                .map_err(EstablishConnectedError::protocol)?;
            device_info.operations_supported.contains(&GET_VENDOR_CODES)
        } else {
            false
        };

        let mut response = self
            .session_command(&channel, op::OPEN_SESSION, 1, vec![1], "OpenSession")
            .await
            .map_err(EstablishConnectedError::protocol)?;
        if response.response_code == resp::SESSION_ALREADY_OPEN {
            self.trace
                .session(
                    "sessionAlreadyOpen",
                    json!({ "recovery": "close-and-retry", "retry": 1 }),
                )
                .map_err(trace_error)
                .map_err(EstablishConnectedError::protocol)?;
            let close = self
                .session_command(
                    &channel,
                    op::CLOSE_SESSION,
                    2,
                    Vec::new(),
                    "CloseSession recovery",
                )
                .await
                .map_err(EstablishConnectedError::protocol)?;
            if close.response_code != resp::OK {
                return Err(EstablishConnectedError::protocol(failed(format!(
                    "CloseSession recovery returned 0x{:04x}",
                    close.response_code
                ))));
            }
            self.trace
                .retry_logical_ptp_session()
                .map_err(trace_error)
                .map_err(EstablishConnectedError::protocol)?;
            response = self
                .session_command(&channel, op::OPEN_SESSION, 1, vec![1], "OpenSession retry")
                .await
                .map_err(EstablishConnectedError::protocol)?;
            // legacy manufacturer app resets its PTP transaction counter before every
            // OpenSession call: the retry is transaction 1 and the next command
            // starts at 2 again.
            channel.next_tid.store(2, Ordering::Release);
        }
        let opened = PtpSessionOpenResult {
            transaction_id: response.txn,
            response_code: response.response_code,
            response_params: response.params,
        };
        if opened.response_code != resp::OK {
            return Err(EstablishConnectedError::protocol(failed(format!(
                "OpenSession returned 0x{:04x}",
                opened.response_code
            ))));
        }
        if discover_vendor_codes {
            let codes = self
                .standard_get_vendor_codes(&channel, 2)
                .await
                .map_err(EstablishConnectedError::protocol)?;
            channel.next_tid.store(3, Ordering::Release);
            self.trace
                .session(
                    "vendorCodes",
                    json!({ "count": codes.len(), "codes": codes }),
                )
                .map_err(trace_error)
                .map_err(EstablishConnectedError::protocol)?;
        }
        Ok((channel, opened))
    }

    async fn open_standard_event_channel(
        &self,
        connection_number: u32,
    ) -> Result<(), PtpTransportError> {
        let camera = self
            .config
            .camera
            .ok_or_else(|| failed("standard event socket requires a camera address"))?;
        let port = self
            .event_port
            .ok_or_else(|| failed("standard PTP/IP connection has no event socket binding"))?;
        let stream = self.connect(SocketAddr::new(camera, port)).await?;
        let (reader, mut writer) = stream.into_split();
        let request = ptp_core::encode(&PtpIpPacket::InitEventRequest(InitEventRequest {
            connection_number,
        }))
        .map_err(|error| failed(error.to_string()))?;
        self.trace
            .wire("tx", "eventInit", &request)
            .map_err(trace_error)?;
        writer.write_all(&request).await.map_err(io_error)?;
        let mut reader = FrameReader::new(reader);
        let ack = tokio::time::timeout(
            self.config.connect_timeout,
            reader.read_frame(INIT_FRAME_LIMIT),
        )
        .await
        .map_err(|_| timeout_error("event init acknowledgement"))?
        .map_err(io_error)?;
        self.trace
            .wire("rx", "eventInit", &ack)
            .map_err(trace_error)?;
        if !matches!(PtpIpPacket::decode(&ack), Ok(PtpIpPacket::InitEventAck(_))) {
            return Err(failed("camera returned a malformed InitEventAck"));
        }
        *self.event.lock().await = Some(EventChannel {
            reader: Mutex::new(reader),
            writer: Mutex::new(writer),
            retained: Mutex::new(VecDeque::new()),
            framing: PtpFraming::Standard,
        });
        self.trace
            .session(
                "eventConnected",
                json!({ "port": port, "connectionNumber": connection_number }),
            )
            .map_err(trace_error)
    }

    async fn standard_get_device_info(
        &self,
        channel: &CommandChannel,
    ) -> Result<DeviceInfo, PtpTransportError> {
        let frame = build_command(PtpFraming::Standard, op::GET_DEVICE_INFO, 0, Vec::new())
            .map_err(|error| failed(error.to_string()))?;
        self.timed_write(channel, frame, "command", "GetDeviceInfo write")
            .await?;
        let payload = self
            .standard_read_data_in(channel, 0, "GetDeviceInfo")
            .await?;
        DeviceInfo::decode(&payload).map_err(|error| failed(error.to_string()))
    }

    async fn standard_get_vendor_codes(
        &self,
        channel: &CommandChannel,
        transaction_id: u32,
    ) -> Result<Vec<u32>, PtpTransportError> {
        let frame = build_command(
            PtpFraming::Standard,
            GET_VENDOR_CODES,
            transaction_id,
            vec![GET_VENDOR_CODES_DOMAIN],
        )
        .map_err(|error| failed(error.to_string()))?;
        self.timed_write(channel, frame, "command", "GetVendorCodes write")
            .await?;
        let payload = self
            .standard_read_data_in(channel, transaction_id, "GetVendorCodes")
            .await?;
        decode_u32_array(&payload).map_err(|error| failed(format!("GetVendorCodes {error}")))
    }

    async fn standard_read_data_in(
        &self,
        channel: &CommandChannel,
        transaction_id: u32,
        stage: &str,
    ) -> Result<Vec<u8>, PtpTransportError> {
        let mut payload = Vec::new();
        let mut declared: Option<u64> = None;
        let mut ended = false;
        loop {
            let frame = self
                .timed_read(
                    channel,
                    self.config.max_frame_bytes,
                    "command",
                    &format!("{stage} response"),
                )
                .await?;
            match PtpIpPacket::decode(&frame).map_err(|error| failed(error.to_string()))? {
                PtpIpPacket::StartData(start) if start.transaction_id == transaction_id => {
                    if declared.is_some() {
                        return Err(failed(format!("{stage} returned duplicate StartData")));
                    }
                    if start.total_length > self.config.max_frame_bytes as u64 {
                        return Err(failed(format!(
                            "{stage} declared data length {} exceeds limit {}",
                            start.total_length, self.config.max_frame_bytes
                        )));
                    }
                    declared = Some(start.total_length);
                }
                PtpIpPacket::Data(DataBlock {
                    transaction_id: packet_transaction_id,
                    payload: chunk,
                }) if packet_transaction_id == transaction_id => {
                    if declared.is_none() {
                        return Err(failed(format!("{stage} data arrived before StartData")));
                    }
                    if ended {
                        return Err(failed(format!("{stage} data arrived after EndData")));
                    }
                    append_standard_data_chunk(
                        &mut payload,
                        &chunk,
                        declared.expect("declared length was checked"),
                        self.config.max_frame_bytes,
                        stage,
                    )?;
                }
                PtpIpPacket::EndData(DataBlock {
                    transaction_id: packet_transaction_id,
                    payload: chunk,
                }) if packet_transaction_id == transaction_id => {
                    if declared.is_none() {
                        return Err(failed(format!("{stage} EndData arrived before StartData")));
                    }
                    if ended {
                        return Err(failed(format!("{stage} returned duplicate EndData")));
                    }
                    let declared = declared.expect("declared length was checked");
                    append_standard_data_chunk(
                        &mut payload,
                        &chunk,
                        declared,
                        self.config.max_frame_bytes,
                        stage,
                    )?;
                    if payload.len() as u64 != declared {
                        return Err(failed(format!(
                            "{stage} data length {} did not match declared length {declared}",
                            payload.len()
                        )));
                    }
                    ended = true;
                }
                PtpIpPacket::OperationResponse(response)
                    if response.transaction_id == transaction_id =>
                {
                    if response.code != resp::OK {
                        return Err(failed(format!("{stage} returned 0x{:04x}", response.code)));
                    }
                    if declared.is_none() {
                        return Err(failed(format!("{stage} returned no data phase")));
                    }
                    if !ended {
                        return Err(failed(format!("{stage} response arrived before EndData")));
                    }
                    return Ok(payload);
                }
                other => return Err(failed(format!("unexpected {stage} packet {other:?}"))),
            }
        }
    }

    async fn session_command(
        &self,
        channel: &CommandChannel,
        operation: u16,
        transaction_id: u32,
        params: Vec<u32>,
        stage: &str,
    ) -> Result<ResponseFrame, PtpTransportError> {
        let frame = build_command(self.framing, operation, transaction_id, params)
            .map_err(|error| failed(error.to_string()))?;
        self.timed_write(channel, frame, "command", &format!("{stage} write"))
            .await?;
        let frame = self
            .timed_read(
                channel,
                self.config.max_frame_bytes,
                "command",
                &format!("{stage} response"),
            )
            .await?;
        let response =
            parse_response(self.framing, frame).map_err(|error| failed(error.to_string()))?;
        if response.txn != transaction_id {
            return Err(failed(format!(
                "{stage} response transaction {} != {transaction_id}",
                response.txn
            )));
        }
        Ok(response)
    }

    async fn pcss_connected_target(&self) -> Result<PcssConnectedTarget, PtpTransportError> {
        let identity = self
            .store
            .connection_init_identity_with_runtime(
                self.config.connection.clone(),
                self.runtime_key_values(),
            )
            .ok_or_else(|| failed("PCSS init identity could not be resolved"))?;
        // Reject an invalid GUID/name before the first discovery datagram. The
        // callback-selected local address is substituted into the real packet
        // below and does not affect identity validation.
        self.store
            .build_pcss_init(
                self.config.connection.clone(),
                identity.guid.clone(),
                Ipv4Addr::UNSPECIFIED.to_string(),
                identity.friendly_name.clone(),
            )
            .map_err(|error| failed(error.to_string()))?;
        let rendezvous = self
            .store
            .pcss_rendezvous(self.config.connection.clone())
            .ok_or_else(|| failed("connection has no PCSS rendezvous contract"))?;
        let (mode, explicit_camera) = match self.config.camera {
            Some(IpAddr::V4(camera)) => (PcssDiscoveryTarget::ExplicitUnicast, Some(camera)),
            Some(IpAddr::V6(_)) => {
                return Err(failed("PCSS rendezvous requires an IPv4 camera address"))
            }
            None if rendezvous.default_discovery_target == PcssDiscoveryTarget::SubnetBroadcast => {
                (PcssDiscoveryTarget::SubnetBroadcast, None)
            }
            None => {
                return Err(failed(
                    "the manifest's default PCSS discovery target requires --camera",
                ))
            }
        };

        let (first, callback) = self
            .pcss_rendezvous_round(mode, explicit_camera, None)
            .await?;
        let first_endpoint = SocketAddr::new(IpAddr::V4(first.camera_ip), first.command_port);
        let first_init = self
            .store
            .build_pcss_init(
                self.config.connection.clone(),
                identity.guid.clone(),
                first.local_ip.to_string(),
                identity.friendly_name.clone(),
            )
            .map_err(|error| failed(error.to_string()))?;
        let recovery = (mode == PcssDiscoveryTarget::SubnetBroadcast
            && rendezvous.retry_discovered_unicast)
            .then(|| PcssRecovery {
                callback,
                camera_ip: first.camera_ip,
                identity_guid: identity.guid,
                friendly_name: identity.friendly_name,
            });
        match self
            .connect_pcss_endpoint(
                first_endpoint,
                Duration::from_millis(rendezvous.connect_timeout_ms as u64),
            )
            .await
        {
            Ok(stream) => Ok(PcssConnectedTarget {
                stream,
                init_packet: first_init,
                recovery,
            }),
            Err(error) if error.endpoint_unavailable && recovery.is_some() => {
                let recovery = recovery.expect("recovery was checked as present");
                self.trace
                    .session(
                        "pcssRediscovery",
                        json!({
                            "reason": "commandEndpointUnavailable",
                            "camera": first.camera_ip.to_string(),
                            "endpoint": first_endpoint.to_string(),
                            "error": error.error.to_string(),
                        }),
                    )
                    .map_err(trace_error)?;
                self.pcss_recovered_target(recovery).await
            }
            Err(error) => Err(error.error),
        }
    }

    async fn pcss_recovered_target(
        &self,
        recovery: PcssRecovery,
    ) -> Result<PcssConnectedTarget, PtpTransportError> {
        let rendezvous = self
            .store
            .pcss_rendezvous(self.config.connection.clone())
            .ok_or_else(|| failed("connection has no PCSS rendezvous contract"))?;
        let (recovered, _) = self
            .pcss_rendezvous_round(
                PcssDiscoveryTarget::ExplicitUnicast,
                Some(recovery.camera_ip),
                Some(recovery.callback),
            )
            .await?;
        let recovered_endpoint =
            SocketAddr::new(IpAddr::V4(recovered.camera_ip), recovered.command_port);
        let recovered_init = self
            .store
            .build_pcss_init(
                self.config.connection.clone(),
                recovery.identity_guid,
                recovered.local_ip.to_string(),
                recovery.friendly_name,
            )
            .map_err(|error| failed(error.to_string()))?;
        let stream = self
            .connect_with_timeout(
                recovered_endpoint,
                Duration::from_millis(rendezvous.connect_timeout_ms as u64),
            )
            .await?;
        Ok(PcssConnectedTarget {
            stream,
            init_packet: recovered_init,
            recovery: None,
        })
    }

    async fn pcss_rendezvous_round(
        &self,
        mode: PcssDiscoveryTarget,
        explicit_camera: Option<Ipv4Addr>,
        callback: Option<TcpListener>,
    ) -> Result<(PcssEndpoint, TcpListener), PtpTransportError> {
        let rendezvous = self
            .store
            .pcss_rendezvous(self.config.connection.clone())
            .ok_or_else(|| failed("connection has no PCSS rendezvous contract"))?;
        let callback_timeout = Duration::from_millis(rendezvous.connect_timeout_ms as u64);
        if !rendezvous.supported_discovery_targets.contains(&mode) {
            return Err(failed(format!(
                "manifest does not support PCSS discovery target '{}'",
                pcss_target_name(mode)
            )));
        }

        let (local_ip, destination, interface_name) = match mode {
            PcssDiscoveryTarget::ExplicitUnicast => {
                let camera = explicit_camera
                    .ok_or_else(|| failed("explicit PCSS unicast requires a camera address"))?;
                let local_ip = route_selected_ipv4(camera)?;
                (
                    local_ip,
                    SocketAddr::new(IpAddr::V4(camera), rendezvous.knock_port),
                    None,
                )
            }
            PcssDiscoveryTarget::SubnetBroadcast => {
                let interface = select_pcss_interface(self.config.interface.as_deref())?;
                (
                    interface.local_ip,
                    SocketAddr::new(IpAddr::V4(interface.broadcast), rendezvous.knock_port),
                    Some(interface.name),
                )
            }
        };

        let callback = if let Some(callback) = callback {
            let callback_addr = callback.local_addr().map_err(io_error)?;
            if callback_addr != SocketAddr::new(IpAddr::V4(local_ip), rendezvous.callback_port) {
                return Err(failed(format!(
                    "PCSS rediscovery route changed callback address from {} to {}:{}",
                    callback_addr, local_ip, rendezvous.callback_port
                )));
            }
            callback
        } else {
            TcpListener::bind((local_ip, rendezvous.callback_port))
                .await
                .map_err(io_error)?
        };
        let discovery_socket = UdpSocket::bind((local_ip, 0)).await.map_err(io_error)?;
        if mode == PcssDiscoveryTarget::SubnetBroadcast {
            discovery_socket.set_broadcast(true).map_err(io_error)?;
        }
        let discovery = self
            .store
            .build_pcss_discovery(self.config.connection.clone(), local_ip.to_string())
            .map_err(|error| failed(error.to_string()))?;

        'attempts: for attempt in 1..=rendezvous.max_attempts {
            discovery_socket
                .send_to(&discovery, destination)
                .await
                .map_err(io_error)?;
            self.trace
                .session(
                    "pcssDiscoverySent",
                    json!({
                        "attempt": attempt,
                        "mode": pcss_target_name(mode),
                        "destination": destination.to_string(),
                        "interface": interface_name,
                        "localIp": local_ip.to_string(),
                    }),
                )
                .map_err(trace_error)?;
            self.trace
                .wire("tx", "pcssDiscovery", &discovery)
                .map_err(trace_error)?;
            let callback_deadline = tokio::time::Instant::now()
                + Duration::from_millis(rendezvous.retry_interval_ms as u64);
            loop {
                let remaining =
                    callback_deadline.saturating_duration_since(tokio::time::Instant::now());
                match tokio::time::timeout(remaining, callback.accept()).await {
                    Ok(Ok((mut socket, peer))) => {
                        let read_timeout = callback_timeout.min(remaining);
                        let callback_result = tokio::time::timeout(
                            read_timeout,
                            self.read_pcss_notify(
                                &mut socket,
                                &rendezvous.callback_message_terminator,
                            ),
                        )
                        .await;
                        let (notify, parsed) = match callback_result {
                            Ok(Ok(value)) => value,
                            Ok(Err(error)) => {
                                self.trace
                                    .session(
                                        "pcssCallbackIgnored",
                                        json!({
                                            "attempt": attempt,
                                            "peer": peer.to_string(),
                                            "reason": error.to_string(),
                                        }),
                                    )
                                    .map_err(trace_error)?;
                                continue;
                            }
                            Err(_) => {
                                self.trace
                                    .session(
                                        "pcssCallbackIgnored",
                                        json!({
                                            "attempt": attempt,
                                            "peer": peer.to_string(),
                                            "reason": "callbackReadTimeout",
                                        }),
                                    )
                                    .map_err(trace_error)?;
                                continue;
                            }
                        };
                        self.trace
                            .wire("rx", "pcssCallback", &notify)
                            .map_err(trace_error)?;
                        let camera_ip = parsed
                            .camera_ipv4
                            .parse::<Ipv4Addr>()
                            .map_err(|_| failed("PCSS callback DSC is not IPv4"))?;
                        if !pcss_callback_matches(peer.ip(), camera_ip, explicit_camera) {
                            self.trace
                                .session(
                                    "pcssCallbackIgnored",
                                    json!({
                                        "attempt": attempt,
                                        "peer": peer.to_string(),
                                        "dsc": camera_ip.to_string(),
                                        "reason": "callbackIdentityMismatch",
                                    }),
                                )
                                .map_err(trace_error)?;
                            continue;
                        }
                        let ack = self
                            .store
                            .build_pcss_callback_ack(self.config.connection.clone())
                            .map_err(|error| failed(error.to_string()))?;
                        self.trace
                            .wire("tx", "pcssCallback", &ack)
                            .map_err(trace_error)?;
                        tokio::time::timeout(callback_timeout, socket.write_all(&ack))
                            .await
                            .map_err(|_| timeout_error("PCSS callback acknowledgement"))?
                            .map_err(io_error)?;
                        socket.shutdown().await.map_err(io_error)?;
                        self.trace
                            .session(
                                "pcssCallbackAccepted",
                                json!({
                                    "attempt": attempt,
                                    "mode": pcss_target_name(mode),
                                    "peer": peer.to_string(),
                                    "dsc": camera_ip.to_string(),
                                    "cameraName": parsed.camera_name,
                                    "commandPort": parsed.command_port,
                                }),
                            )
                            .map_err(trace_error)?;
                        return Ok((
                            PcssEndpoint {
                                camera_ip,
                                local_ip,
                                command_port: parsed.command_port,
                            },
                            callback,
                        ));
                    }
                    Ok(Err(error)) => return Err(io_error(error)),
                    Err(_) if attempt < rendezvous.max_attempts => continue 'attempts,
                    Err(_) => return Err(timeout_error("PCSS callback")),
                }
            }
        }
        Err(timeout_error("PCSS callback"))
    }

    async fn read_pcss_notify(
        &self,
        socket: &mut TcpStream,
        terminator: &[u8],
    ) -> Result<(Vec<u8>, PcssNotifyInfo), PtpTransportError> {
        if terminator.is_empty() {
            return Err(failed("PCSS callback terminator must not be empty"));
        }
        let mut message = Vec::new();
        let mut chunk = [0u8; 512];
        loop {
            if message.len() >= PCSS_CALLBACK_LIMIT {
                return Err(failed("PCSS callback exceeds the frame limit"));
            }
            let remaining = (PCSS_CALLBACK_LIMIT - message.len()).min(chunk.len());
            let read = socket
                .read(&mut chunk[..remaining])
                .await
                .map_err(io_error)?;
            if read == 0 {
                return Err(failed("PCSS callback ended before a complete NOTIFY"));
            }
            message.extend_from_slice(&chunk[..read]);
            let complete = message.ends_with(terminator)
                || message
                    .strip_suffix(&[0])
                    .is_some_and(|without_nul| without_nul.ends_with(terminator));
            if complete {
                let parsed = self
                    .store
                    .parse_pcss_notify(self.config.connection.clone(), message.clone())
                    .map_err(|error| failed(error.to_string()))?;
                return Ok((message, parsed));
            }
        }
    }

    async fn connect(&self, endpoint: SocketAddr) -> Result<TcpStream, PtpTransportError> {
        self.connect_with_timeout(endpoint, self.config.connect_timeout)
            .await
    }

    async fn connect_with_timeout(
        &self,
        endpoint: SocketAddr,
        timeout: Duration,
    ) -> Result<TcpStream, PtpTransportError> {
        match tokio::time::timeout(timeout, TcpStream::connect(endpoint)).await {
            Ok(Ok(stream)) => {
                stream.set_nodelay(true).map_err(io_error)?;
                Ok(stream)
            }
            Ok(Err(error)) => Err(io_error(error)),
            Err(_) => Err(timeout_error(&format!("connect {endpoint}"))),
        }
    }

    async fn connect_pcss_endpoint(
        &self,
        endpoint: SocketAddr,
        timeout: Duration,
    ) -> Result<TcpStream, EstablishConnectedError> {
        match tokio::time::timeout(timeout, TcpStream::connect(endpoint)).await {
            Ok(Ok(stream)) => {
                stream
                    .set_nodelay(true)
                    .map_err(io_error)
                    .map_err(EstablishConnectedError::protocol)?;
                Ok(stream)
            }
            Ok(Err(error)) if is_endpoint_io_failure(error.kind()) => {
                Err(EstablishConnectedError::endpoint(io_error(error)))
            }
            Ok(Err(error)) => Err(EstablishConnectedError::protocol(io_error(error))),
            Err(_) => Err(EstablishConnectedError::endpoint(timeout_error(&format!(
                "connect {endpoint}"
            )))),
        }
    }

    async fn connect_until(
        &self,
        endpoint: SocketAddr,
        deadline: Instant,
    ) -> Result<TcpStream, PtpTransportError> {
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(timeout_error("handoff command endpoint"));
            }
            let attempt_timeout = self.config.connect_timeout.min(remaining);
            if let Ok(Ok(stream)) =
                tokio::time::timeout(attempt_timeout, TcpStream::connect(endpoint)).await
            {
                stream.set_nodelay(true).map_err(io_error)?;
                return Ok(stream);
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(timeout_error("handoff command endpoint"));
            }
            tokio::time::sleep(Duration::from_millis(500).min(remaining)).await;
        }
    }

    async fn timed_write(
        &self,
        channel: &CommandChannel,
        frame: Vec<u8>,
        trace_channel: &str,
        stage: &str,
    ) -> Result<(), PtpTransportError> {
        tokio::time::timeout(
            self.config.connect_timeout,
            channel.write_frame(&frame, &self.trace, trace_channel),
        )
        .await
        .map_err(|_| timeout_error(stage))??;
        self.trace
            .ptp_frame("tx", self.framing, &frame)
            .map_err(trace_error)
    }

    /// Establishment I/O keeps local trace failures terminal while allowing
    /// exactly the first init's socket timeout/EOF/write failure to select the
    /// manifest's learned-unicast recovery path.
    async fn establishment_write(
        &self,
        channel: &CommandChannel,
        frame: Vec<u8>,
        trace_channel: &str,
        stage: &str,
        endpoint_eligible: bool,
    ) -> Result<(), EstablishConnectedError> {
        channel
            .require_usable()
            .map_err(EstablishConnectedError::protocol)?;
        let mut writer = channel.writer.lock().await;
        self.trace
            .wire("tx", trace_channel, &frame)
            .map_err(trace_error)
            .map_err(EstablishConnectedError::protocol)?;
        let mut poison = WritePoison::new(&channel.poisoned);
        match tokio::time::timeout(self.config.connect_timeout, writer.write_all(&frame)).await {
            Ok(Ok(())) => {
                poison.disarm();
                Ok(())
            }
            Err(_) if endpoint_eligible => {
                Err(EstablishConnectedError::endpoint(timeout_error(stage)))
            }
            Err(_) => Err(EstablishConnectedError::protocol(timeout_error(stage))),
            Ok(Err(error)) if endpoint_eligible && is_endpoint_io_failure(error.kind()) => {
                Err(EstablishConnectedError::endpoint(io_error(error)))
            }
            Ok(Err(error)) => Err(EstablishConnectedError::protocol(io_error(error))),
        }
    }

    async fn establishment_read(
        &self,
        channel: &CommandChannel,
        max: usize,
        trace_channel: &str,
        stage: &str,
        endpoint_eligible: bool,
    ) -> Result<Vec<u8>, EstablishConnectedError> {
        channel
            .require_usable()
            .map_err(EstablishConnectedError::protocol)?;
        let mut reader = channel.reader.lock().await;
        let frame =
            match tokio::time::timeout(self.config.connect_timeout, reader.read_frame(max)).await {
                Ok(Ok(frame)) => frame,
                Err(_) if endpoint_eligible => {
                    return Err(EstablishConnectedError::endpoint(timeout_error(stage)));
                }
                Err(_) => return Err(EstablishConnectedError::protocol(timeout_error(stage))),
                Ok(Err(error)) if endpoint_eligible && is_endpoint_io_failure(error.kind()) => {
                    return Err(EstablishConnectedError::endpoint(io_error(error)));
                }
                Ok(Err(error)) => return Err(EstablishConnectedError::protocol(io_error(error))),
            };
        self.trace
            .wire("rx", trace_channel, &frame)
            .map_err(trace_error)
            .map_err(EstablishConnectedError::protocol)?;
        Ok(frame)
    }

    async fn timed_read(
        &self,
        channel: &CommandChannel,
        max: usize,
        trace_channel: &str,
        stage: &str,
    ) -> Result<Vec<u8>, PtpTransportError> {
        let frame = tokio::time::timeout(
            self.config.connect_timeout,
            channel.read_frame(max, &self.trace, trace_channel),
        )
        .await
        .map_err(|_| timeout_error(stage))??;
        self.trace
            .ptp_frame("rx", self.framing, &frame)
            .map_err(trace_error)?;
        Ok(frame)
    }

    fn runtime_key_values(&self) -> Vec<KeyValue> {
        self.config
            .runtime_scope
            .iter()
            .map(|(key, value)| KeyValue {
                key: key.clone(),
                value: value.clone(),
            })
            .collect()
    }

    async fn active_channel(&self) -> Result<Arc<CommandChannel>, PtpTransportError> {
        if self.session_poisoned.load(Ordering::Acquire) {
            return Err(failed("command session is poisoned"));
        }
        self.active
            .read()
            .await
            .clone()
            .ok_or(PtpTransportError::NotConnected)
    }

    async fn close_auxiliary(&self) {
        self.event.lock().await.take();
        self.live_view.lock().await.take();
    }

    fn trace_stream_bytes(&self, bytes: &[u8]) -> Result<(), PtpTransportError> {
        let mut state = self
            .stream_trace
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.pending.extend_from_slice(bytes);
        loop {
            if state.current.is_none() {
                if state.pending.len() < 4 {
                    return Ok(());
                }
                let total = u32::from_le_bytes(state.pending[0..4].try_into().unwrap()) as u64;
                if total < 12 {
                    state.pending.clear();
                    return Err(failed(format!(
                        "streamed frame length {total} is smaller than its header"
                    )));
                }
                let id = self
                    .trace
                    .begin_wire_frame("rx", "command", total)
                    .map_err(trace_error)?;
                state.current = Some(StreamFrame {
                    id,
                    offset: 0,
                    remaining: total,
                });
            }
            let Some(mut current) = state.current.take() else {
                continue;
            };
            if state.pending.is_empty() {
                state.current = Some(current);
                return Ok(());
            }
            let take = usize::try_from(current.remaining)
                .unwrap_or(usize::MAX)
                .min(state.pending.len());
            let chunk = state.pending.split_to(take).to_vec();
            self.trace
                .wire_chunk("rx", "command", current.id, current.offset, &chunk)
                .map_err(trace_error)?;
            current.offset += take as u64;
            current.remaining -= take as u64;
            if current.remaining > 0 {
                state.current = Some(current);
                return Ok(());
            }
        }
    }
}

#[async_trait]
impl PtpExecutorTransport for NativePtpTransport {
    async fn open_channel(&self, role: SocketRole) -> Result<(), PtpTransportError> {
        self.open_manifest_channel(role).await
    }

    async fn reserve_transaction_id(&self) -> Result<u32, PtpTransportError> {
        let channel = self.active_channel().await?;
        channel.require_usable()?;
        Ok(channel.next_tid.fetch_add(1, Ordering::AcqRel))
    }

    async fn send_command_frame(&self, frame: Vec<u8>) -> Result<(), PtpTransportError> {
        self.active_channel()
            .await?
            .write_frame(&frame, &self.trace, "command")
            .await?;
        self.trace
            .ptp_frame("tx", self.framing, &frame)
            .map_err(trace_error)
    }

    async fn next_command_frame(&self) -> Result<Vec<u8>, PtpTransportError> {
        let frame = self
            .active_channel()
            .await?
            .read_frame(self.config.max_frame_bytes, &self.trace, "command")
            .await?;
        self.trace
            .ptp_frame("rx", self.framing, &frame)
            .map_err(trace_error)?;
        Ok(frame)
    }

    async fn next_event_frame(&self, event_code: u16) -> Result<Vec<u8>, PtpTransportError> {
        let event_guard = self.event.lock().await;
        let channel = event_guard
            .as_ref()
            .ok_or_else(|| failed("event socket is not connected"))?;
        {
            let mut retained = channel.retained.lock().await;
            if let Some(index) = retained.iter().position(|frame| {
                parse_event(channel.framing, frame.clone())
                    .ok()
                    .flatten()
                    .is_some_and(|event| event.code == event_code)
            }) {
                return Ok(retained.remove(index).expect("retained index exists"));
            }
        }
        loop {
            let frame = channel
                .reader
                .lock()
                .await
                .read_frame(self.config.max_frame_bytes)
                .await
                .map_err(io_error)?;
            self.trace
                .wire("rx", "event", &frame)
                .map_err(trace_error)?;
            let event = parse_event(channel.framing, frame.clone())
                .map_err(|error| failed(error.to_string()))?
                .ok_or_else(|| failed("event socket returned a non-event frame"))?;
            self.trace.ptp_event(&event).map_err(trace_error)?;
            if event.code == event_code {
                return Ok(frame);
            }
            let mut retained = channel.retained.lock().await;
            if retained.len() >= EVENT_QUEUE_LIMIT {
                return Err(failed("unrelated event queue overflow"));
            }
            retained.push_back(frame);
        }
    }

    async fn close_command_channel(
        &self,
        transport_close_frame: Option<Vec<u8>>,
    ) -> Result<(), PtpTransportError> {
        self.close_auxiliary().await;
        let channel = self
            .active
            .write()
            .await
            .take()
            .ok_or(PtpTransportError::NotConnected)?;
        if let Some(frame) = transport_close_frame {
            channel
                .write_frame(&frame, &self.trace, "transportClose")
                .await?;
        }
        channel.shutdown().await?;
        self.trace
            .session("closed", json!({ "connection": self.config.connection }))
            .map_err(trace_error)
    }

    async fn reopen_command_session(&self) -> Result<PtpSessionOpenResult, PtpTransportError> {
        self.open_command_session().await
    }

    async fn sleep(&self, ms: u32) -> Result<(), PtpTransportError> {
        tokio::time::sleep(Duration::from_millis(ms as u64)).await;
        Ok(())
    }
}

#[async_trait]
impl PtpStreamingTransport for NativePtpTransport {
    async fn reserve_transaction_id(&self) -> Result<u32, PtpTransportError> {
        PtpExecutorTransport::reserve_transaction_id(self).await
    }

    async fn send_command_frame(&self, frame: Vec<u8>) -> Result<(), PtpTransportError> {
        PtpExecutorTransport::send_command_frame(self, frame).await
    }

    async fn receive_command_bytes(&self, max_bytes: u32) -> Result<Vec<u8>, PtpTransportError> {
        if max_bytes == 0 {
            return Err(failed("streaming read requested zero bytes"));
        }
        let channel = self.active_channel().await?;
        channel.require_usable()?;
        let bytes = channel
            .reader
            .lock()
            .await
            .read_some(max_bytes as usize)
            .await
            .map_err(io_error)?;
        self.trace_stream_bytes(&bytes)?;
        Ok(bytes)
    }

    async fn sleep(&self, ms: u32) -> Result<(), PtpTransportError> {
        PtpExecutorTransport::sleep(self, ms).await
    }

    fn invalidate_command_session(&self, reason: String) {
        self.session_poisoned.store(true, Ordering::Release);
        let _ = self.trace.session("poisoned", json!({ "reason": reason }));
    }
}

struct CommandChannel {
    reader: Mutex<FrameReader<OwnedReadHalf>>,
    writer: Mutex<OwnedWriteHalf>,
    next_tid: AtomicU32,
    poisoned: Arc<AtomicBool>,
}

impl CommandChannel {
    fn new(stream: TcpStream, poisoned: Arc<AtomicBool>) -> Self {
        let (reader, writer) = stream.into_split();
        Self {
            reader: Mutex::new(FrameReader::new(reader)),
            writer: Mutex::new(writer),
            next_tid: AtomicU32::new(2),
            poisoned,
        }
    }

    fn require_usable(&self) -> Result<(), PtpTransportError> {
        if self.poisoned.load(Ordering::Acquire) {
            Err(failed("command session is poisoned"))
        } else {
            Ok(())
        }
    }

    async fn write_frame(
        &self,
        frame: &[u8],
        trace: &TraceWriter,
        channel: &str,
    ) -> Result<(), PtpTransportError> {
        self.require_usable()?;
        let mut writer = self.writer.lock().await;
        trace.wire("tx", channel, frame).map_err(trace_error)?;
        let mut poison = WritePoison::new(&self.poisoned);
        writer.write_all(frame).await.map_err(io_error)?;
        poison.disarm();
        Ok(())
    }

    async fn read_frame(
        &self,
        max: usize,
        trace: &TraceWriter,
        channel: &str,
    ) -> Result<Vec<u8>, PtpTransportError> {
        self.require_usable()?;
        let frame = self
            .reader
            .lock()
            .await
            .read_frame(max)
            .await
            .map_err(io_error)?;
        trace.wire("rx", channel, &frame).map_err(trace_error)?;
        Ok(frame)
    }

    async fn shutdown(&self) -> Result<(), PtpTransportError> {
        let mut writer = self.writer.lock().await;
        writer.shutdown().await.map_err(io_error)
    }
}

struct EventChannel {
    reader: Mutex<FrameReader<OwnedReadHalf>>,
    writer: Mutex<OwnedWriteHalf>,
    retained: Mutex<VecDeque<Vec<u8>>>,
    framing: PtpFraming,
}

struct PassiveChannel {
    reader: FrameReader<OwnedReadHalf>,
    _writer: OwnedWriteHalf,
}

struct FrameReader<R> {
    reader: R,
    buffered: BytesMut,
}

impl<R: AsyncRead + Unpin> FrameReader<R> {
    fn new(reader: R) -> Self {
        Self {
            reader,
            buffered: BytesMut::new(),
        }
    }
    async fn read_frame(&mut self, max: usize) -> io::Result<Vec<u8>> {
        self.fill(4).await?;
        let length = u32::from_le_bytes(self.buffered[0..4].try_into().unwrap()) as usize;
        if !(8..=max).contains(&length) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("frame length {length} outside 8..={max}"),
            ));
        }
        self.fill(length).await?;
        Ok(self.buffered.split_to(length).to_vec())
    }

    async fn read_some(&mut self, max: usize) -> io::Result<Vec<u8>> {
        if !self.buffered.is_empty() {
            let take = max.min(self.buffered.len());
            return Ok(self.buffered.split_to(take).to_vec());
        }
        let mut chunk = vec![0; max.min(64 * 1024)];
        let count = self.reader.read(&mut chunk).await?;
        if count == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "command stream closed during frame",
            ));
        }
        chunk.truncate(count);
        Ok(chunk)
    }

    async fn fill(&mut self, target: usize) -> io::Result<()> {
        while self.buffered.len() < target {
            let count = self.reader.read_buf(&mut self.buffered).await?;
            if count == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "socket closed during frame",
                ));
            }
        }
        Ok(())
    }
}

struct WritePoison<'a> {
    poisoned: &'a AtomicBool,
    armed: bool,
}

impl<'a> WritePoison<'a> {
    fn new(poisoned: &'a AtomicBool) -> Self {
        Self {
            poisoned,
            armed: true,
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for WritePoison<'_> {
    fn drop(&mut self) {
        if self.armed {
            self.poisoned.store(true, Ordering::Release);
        }
    }
}

#[derive(Default)]
struct StreamTrace {
    pending: BytesMut,
    current: Option<StreamFrame>,
}

struct StreamFrame {
    id: u64,
    offset: u64,
    remaining: u64,
}

fn pcss_target_name(target: PcssDiscoveryTarget) -> &'static str {
    match target {
        PcssDiscoveryTarget::SubnetBroadcast => "subnetBroadcast",
        PcssDiscoveryTarget::ExplicitUnicast => "explicitUnicast",
    }
}

fn route_selected_ipv4(destination: Ipv4Addr) -> Result<Ipv4Addr, PtpTransportError> {
    let socket = std::net::UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).map_err(io_error)?;
    socket.connect((destination, 9)).map_err(io_error)?;
    match socket.local_addr().map_err(io_error)?.ip() {
        IpAddr::V4(local_ip) => Ok(local_ip),
        IpAddr::V6(_) => Err(failed("route selected a non-IPv4 address")),
    }
}

fn select_pcss_interface(requested: Option<&str>) -> Result<PcssInterface, PtpTransportError> {
    let preferred_ip = requested
        .is_none()
        .then(|| route_selected_ipv4(Ipv4Addr::new(192, 0, 2, 1)))
        .transpose()?;
    let interfaces = get_if_addrs().map_err(io_error)?;
    for interface in interfaces {
        if requested.is_some_and(|name| interface.name != name) {
            continue;
        }
        let IfAddr::V4(address) = interface.addr else {
            continue;
        };
        if preferred_ip.is_some_and(|preferred| preferred != address.ip) {
            continue;
        }
        if requested.is_none() && address.is_loopback() {
            continue;
        }
        let Some(broadcast) = address
            .broadcast
            .or_else(|| directed_broadcast(address.ip, address.netmask))
        else {
            continue;
        };
        return Ok(PcssInterface {
            name: interface.name,
            local_ip: address.ip,
            broadcast,
        });
    }
    Err(failed(match requested {
        Some(name) => format!("interface '{name}' has no usable IPv4 subnet broadcast address"),
        None => {
            "default-route interface has no usable IPv4 subnet broadcast address; pass --interface"
                .into()
        }
    }))
}

fn directed_broadcast(ip: Ipv4Addr, netmask: Ipv4Addr) -> Option<Ipv4Addr> {
    let ip = u32::from(ip);
    let inverted_mask = !u32::from(netmask);
    if inverted_mask < 3 || inverted_mask & inverted_mask.wrapping_add(1) != 0 {
        return None;
    }
    Some(Ipv4Addr::from(ip | inverted_mask))
}

fn pcss_callback_matches(peer: IpAddr, dsc: Ipv4Addr, explicit_target: Option<Ipv4Addr>) -> bool {
    peer == IpAddr::V4(dsc) && explicit_target.is_none_or(|target| target == dsc)
}

fn init_ack_is_valid(init_shape: &str, packet: &[u8], expected_responder_guid: &[u8]) -> bool {
    match init_shape {
        "pcssKnock" => protocol_primitives::parse_pcss_init_ack(packet).is_ok(),
        "legacyApp82" => {
            protocol_primitives::validate_legacy_app_init_ack(packet, expected_responder_guid)
                .is_ok()
        }
        _ => protocol_primitives::validate_init_ack(packet).is_ok(),
    }
}

fn standard_connection_number(packet: &[u8]) -> Option<u32> {
    match PtpIpPacket::decode(packet).ok()? {
        PtpIpPacket::InitCommandAck(ack) => Some(ack.connection_number),
        _ => None,
    }
}

fn canonical_init_fail_reason(packet: &[u8]) -> Option<u32> {
    if packet.len() != 12 {
        return None;
    }
    match PtpIpPacket::decode(packet).ok()? {
        PtpIpPacket::InitFail(failure) => Some(failure.reason),
        _ => None,
    }
}

fn append_standard_data_chunk(
    payload: &mut Vec<u8>,
    chunk: &[u8],
    declared: u64,
    limit: usize,
    stage: &str,
) -> Result<(), PtpTransportError> {
    let cumulative = payload
        .len()
        .checked_add(chunk.len())
        .ok_or_else(|| failed(format!("{stage} data length overflow")))?;
    if cumulative > limit {
        return Err(failed(format!(
            "{stage} cumulative data length {cumulative} exceeds limit {limit}"
        )));
    }
    if cumulative as u64 > declared {
        return Err(failed(format!(
            "{stage} cumulative data length {cumulative} exceeds declared length {declared}"
        )));
    }
    payload.extend_from_slice(chunk);
    Ok(())
}

fn decode_u32_array(payload: &[u8]) -> Result<Vec<u32>, String> {
    let count_bytes: [u8; 4] = payload
        .get(..4)
        .ok_or_else(|| "returned a truncated u32 array count".to_string())?
        .try_into()
        .expect("slice length was checked");
    let count = u32::from_le_bytes(count_bytes) as usize;
    let expected = count
        .checked_mul(4)
        .and_then(|bytes| bytes.checked_add(4))
        .ok_or_else(|| "returned an overflowing u32 array count".to_string())?;
    if payload.len() != expected {
        return Err(format!(
            "u32 array length {} did not match count {count}",
            payload.len()
        ));
    }
    Ok(payload[4..]
        .chunks_exact(4)
        .map(|chunk| u32::from_le_bytes(chunk.try_into().expect("chunk length is four")))
        .collect())
}

fn current_platform() -> Platform {
    #[cfg(target_os = "macos")]
    {
        Platform::Macos
    }
    #[cfg(target_os = "android")]
    {
        Platform::Android
    }
    #[cfg(not(any(target_os = "macos", target_os = "android")))]
    {
        Platform::Linux
    }
}

fn failed(detail: impl Into<String>) -> PtpTransportError {
    PtpTransportError::Failed {
        detail: detail.into(),
    }
}

fn timeout_error(detail: &str) -> PtpTransportError {
    PtpTransportError::Timeout {
        detail: detail.to_string(),
    }
}

fn is_endpoint_io_failure(kind: io::ErrorKind) -> bool {
    matches!(
        kind,
        io::ErrorKind::BrokenPipe
            | io::ErrorKind::ConnectionAborted
            | io::ErrorKind::ConnectionRefused
            | io::ErrorKind::ConnectionReset
            | io::ErrorKind::NotConnected
            | io::ErrorKind::TimedOut
            | io::ErrorKind::UnexpectedEof
            | io::ErrorKind::WriteZero
    )
}

fn io_error(error: io::Error) -> PtpTransportError {
    failed(error.to_string())
}

fn trace_error(error: io::Error) -> PtpTransportError {
    failed(format!("trace write failed: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncWriteExt;

    #[test]
    fn vendor_code_array_requires_an_exact_u32_count() {
        assert_eq!(
            decode_u32_array(&[2, 0, 0, 0, 1, 0, 0, 0, 1, 32, 0, 0,]).unwrap(),
            vec![1, 0x2001]
        );
        for malformed in [
            Vec::new(),
            vec![1, 0, 0, 0],
            vec![0, 0, 0, 0, 0],
            vec![2, 0, 0, 0, 1, 0, 0, 0],
        ] {
            assert!(decode_u32_array(&malformed).is_err());
        }
    }

    #[test]
    fn standard_data_chunks_cannot_exceed_declared_or_configured_length() {
        let mut payload = vec![1, 2];
        append_standard_data_chunk(&mut payload, &[3, 4], 4, 4, "test").unwrap();
        assert_eq!(payload, vec![1, 2, 3, 4]);

        let declared_error = append_standard_data_chunk(&mut payload, &[5], 4, 8, "test")
            .expect_err("reject data beyond declared length");
        assert!(declared_error.to_string().contains("declared length 4"));

        let limit_error = append_standard_data_chunk(&mut payload, &[5], 8, 4, "test")
            .expect_err("reject data beyond configured limit");
        assert!(limit_error.to_string().contains("exceeds limit 4"));
    }

    #[tokio::test]
    async fn frame_reader_handles_fragmentation_and_coalescing() {
        let (mut writer, reader) = tokio::io::duplex(64);
        let task = tokio::spawn(async move {
            writer.write_all(&[8, 0]).await.unwrap();
            writer.write_all(&[0, 0, 2, 0, 0, 0]).await.unwrap();
            writer.write_all(&[8, 0, 0, 0, 3, 0, 0, 0]).await.unwrap();
        });
        let mut reader = FrameReader::new(reader);
        assert_eq!(
            reader.read_frame(64).await.unwrap(),
            [8, 0, 0, 0, 2, 0, 0, 0]
        );
        assert_eq!(
            reader.read_frame(64).await.unwrap(),
            [8, 0, 0, 0, 3, 0, 0, 0]
        );
        task.await.unwrap();
    }

    #[tokio::test]
    async fn frame_reader_rejects_oversize_before_body_allocation() {
        let (mut writer, reader) = tokio::io::duplex(16);
        writer.write_all(&1024u32.to_le_bytes()).await.unwrap();
        let mut reader = FrameReader::new(reader);
        assert_eq!(
            reader.read_frame(64).await.unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
    }

    #[test]
    fn derives_subnet_directed_broadcast() {
        assert_eq!(
            directed_broadcast(
                Ipv4Addr::new(192, 168, 5, 228),
                Ipv4Addr::new(255, 255, 252, 0),
            ),
            Some(Ipv4Addr::new(192, 168, 7, 255))
        );
    }

    #[test]
    fn rejects_point_to_point_and_invalid_netmasks() {
        assert_eq!(
            directed_broadcast(
                Ipv4Addr::new(192, 168, 4, 2),
                Ipv4Addr::new(255, 255, 255, 254),
            ),
            None
        );
        assert_eq!(
            directed_broadcast(Ipv4Addr::new(192, 168, 4, 2), Ipv4Addr::new(255, 0, 255, 0),),
            None
        );
    }

    #[test]
    fn callback_identity_requires_peer_dsc_and_explicit_target_match() {
        let camera = Ipv4Addr::new(192, 168, 4, 94);
        assert!(pcss_callback_matches(IpAddr::V4(camera), camera, None));
        assert!(pcss_callback_matches(
            IpAddr::V4(camera),
            camera,
            Some(camera)
        ));
        assert!(!pcss_callback_matches(
            IpAddr::V4(Ipv4Addr::new(192, 168, 4, 95)),
            camera,
            None
        ));
        assert!(!pcss_callback_matches(
            IpAddr::V4(camera),
            camera,
            Some(Ipv4Addr::new(192, 168, 4, 95))
        ));
    }

    #[test]
    fn recovery_only_classifies_endpoint_io_failures() {
        assert!(is_endpoint_io_failure(io::ErrorKind::UnexpectedEof));
        assert!(is_endpoint_io_failure(io::ErrorKind::ConnectionReset));
        assert!(is_endpoint_io_failure(io::ErrorKind::BrokenPipe));
        assert!(!is_endpoint_io_failure(io::ErrorKind::InvalidData));
        assert!(!is_endpoint_io_failure(io::ErrorKind::InvalidInput));
        assert!(!is_endpoint_io_failure(io::ErrorKind::AddrNotAvailable));
        assert!(!is_endpoint_io_failure(io::ErrorKind::PermissionDenied));
    }

    #[test]
    fn pcss_init_ack_requires_the_complete_fixed_width_packet() {
        let header_only = [8, 0, 0, 0, 2, 0, 0, 0];
        assert!(protocol_primitives::validate_init_ack(&header_only).is_ok());
        assert!(!init_ack_is_valid("pcssKnock", &header_only, &[]));

        let complete = protocol_primitives::pcss_init_ack_message(7, [0x5a; 16], "GFX100 II")
            .expect("build fixed-width PCSS acknowledgement");
        assert!(init_ack_is_valid("pcssKnock", &complete, &[]));
    }

    #[test]
    fn init_fail_requires_the_canonical_fixed_body() {
        let mut malformed = ptp_core::encode(&PtpIpPacket::InitFail(ptp_core::InitFail {
            reason: 0x2019,
        }))
        .expect("encode canonical InitFail");
        assert_eq!(canonical_init_fail_reason(&malformed), Some(0x2019));

        malformed.extend_from_slice(&[0, 0, 0, 0]);
        let malformed_len = malformed.len() as u32;
        malformed[0..4].copy_from_slice(&malformed_len.to_le_bytes());
        assert!(matches!(
            PtpIpPacket::decode(&malformed),
            Ok(PtpIpPacket::InitFail(_))
        ));
        assert_eq!(canonical_init_fail_reason(&malformed), None);
    }
}
