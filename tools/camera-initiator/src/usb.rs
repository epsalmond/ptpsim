//! Standalone raw USB inventory through the shared establishment and action executors.

use std::collections::{BTreeMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use camera_config::{ActionOutcome, PayloadMetadata, PayloadMetadataBuilder};
use camera_protocol_ffi::{
    run_initiator_action, run_usb_establishment, ActionInvocationRequest, ActionVerb, ConfigStore,
    ConnectionActivityEvent, ConnectionActivityObserver, EntryStep, EstablishmentPlan,
    FfiSessionOwnership, Platform, PtpExecutorTransport, PtpFraming, PtpSessionOpenResult,
    PtpTransportError, SocketRole, Step, StepObserver, StepReport, StepValue, UsbExecutorTransport,
    UsbTransportError,
};
use nusb::transfer::{Buffer, Bulk, In, Out};
use ptp_core::{DeviceInfo, OperationRequest, PtpIpPacket};
use serde::Serialize;
use serde_json::json;
use tokio::sync::Mutex;

use crate::{prepare_action_request, TraceWriter};

const TRANSFER_LIMIT: usize = 64 * 1024;
const HEADER_LENGTH: usize = 12;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UsbEndpointDescriptor {
    pub address: u8,
    pub transfer_type: UsbTransferType,
    pub max_packet_size: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum UsbTransferType {
    Bulk,
    Interrupt,
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UsbInterfaceDescriptor {
    pub number: u8,
    pub alternate: u8,
    pub class: u8,
    pub subclass: u8,
    pub protocol: u8,
    pub endpoints: Vec<UsbEndpointDescriptor>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UsbSelection {
    pub interface: u8,
    pub bulk_in: u8,
    pub bulk_out: u8,
    pub interrupt_in: u8,
}

/// Select one default alternate setting with an unambiguous descriptor-derived endpoint set.
pub fn select_usb_interface(
    descriptors: &[UsbInterfaceDescriptor],
    triple: (u8, u8, u8),
) -> Result<UsbSelection> {
    let matching = descriptors
        .iter()
        .filter(|d| d.alternate == 0 && (d.class, d.subclass, d.protocol) == triple)
        .collect::<Vec<_>>();
    let [descriptor] = matching.as_slice() else {
        bail!("USB interface triple {triple:?} must match exactly one default alternate setting")
    };
    let endpoint = |kind, incoming| -> Result<u8> {
        let matches = descriptor
            .endpoints
            .iter()
            .filter(|ep| ep.transfer_type == kind && (ep.address & 0x80 != 0) == incoming)
            .collect::<Vec<_>>();
        let [ep] = matches.as_slice() else {
            bail!(
                "USB interface {} has no unique {kind:?} {} endpoint",
                descriptor.number,
                if incoming { "IN" } else { "OUT" }
            )
        };
        if ep.address & 0x0f == 0
            || ep.address & 0x70 != 0
            || ep.max_packet_size == 0
            || ep.max_packet_size > 1024
        {
            bail!("invalid USB endpoint descriptor")
        }
        Ok(ep.address)
    };
    Ok(UsbSelection {
        interface: descriptor.number,
        bulk_in: endpoint(UsbTransferType::Bulk, true)?,
        bulk_out: endpoint(UsbTransferType::Bulk, false)?,
        interrupt_in: endpoint(UsbTransferType::Interrupt, true)?,
    })
}

/// Host USB boundary. Implementations never detach another driver's interface.
/// Dropping an I/O future must cancel its endpoint transfers.
#[async_trait]
pub trait UsbInventoryIo: Send + Sync {
    fn descriptors(&self) -> &[UsbInterfaceDescriptor];
    async fn claim(&mut self, selection: &UsbSelection) -> Result<()>;
    async fn write(&mut self, endpoint: u8, bytes: &[u8]) -> Result<usize>;
    async fn read(&mut self, endpoint: u8, max_length: usize) -> Result<Vec<u8>>;
    async fn release(&mut self) -> Result<()>;
}

/// Native adapter. Opening and descriptor inspection do not claim an interface.
pub struct NativeUsbIo {
    device: Option<nusb::Device>,
    interface: Option<nusb::Interface>,
    descriptors: Vec<UsbInterfaceDescriptor>,
    buffered: VecDeque<u8>,
}

impl NativeUsbIo {
    pub async fn open_selected(bus: &str, address: u8) -> Result<Self> {
        if bus.is_empty() || address == 0 || address > 127 {
            bail!("explicit USB bus and device address (1..127) are required")
        }
        let matches = nusb::list_devices()
            .await
            .context("enumerate USB devices")?
            .filter(|device| device.bus_id() == bus && device.device_address() == address)
            .collect::<Vec<_>>();
        let [info] = matches.as_slice() else {
            bail!("explicit USB bus/address did not match exactly one available device")
        };
        let device = info
            .open()
            .await
            .context("open explicitly selected USB device")?;
        let descriptors = device
            .active_configuration()
            .context("read active USB configuration")?
            .interface_alt_settings()
            .map(|interface| UsbInterfaceDescriptor {
                number: interface.interface_number(),
                alternate: interface.alternate_setting(),
                class: interface.class(),
                subclass: interface.subclass(),
                protocol: interface.protocol(),
                endpoints: interface
                    .endpoints()
                    .map(|ep| UsbEndpointDescriptor {
                        address: ep.address(),
                        transfer_type: match ep.transfer_type() {
                            nusb::descriptors::TransferType::Bulk => UsbTransferType::Bulk,
                            nusb::descriptors::TransferType::Interrupt => {
                                UsbTransferType::Interrupt
                            }
                            _ => UsbTransferType::Other,
                        },
                        max_packet_size: ep.max_packet_size(),
                    })
                    .collect(),
            })
            .collect();
        Ok(Self {
            device: Some(device),
            interface: None,
            descriptors,
            buffered: VecDeque::new(),
        })
    }
}

#[async_trait]
impl UsbInventoryIo for NativeUsbIo {
    fn descriptors(&self) -> &[UsbInterfaceDescriptor] {
        &self.descriptors
    }

    async fn claim(&mut self, selection: &UsbSelection) -> Result<()> {
        if self.interface.is_some() {
            bail!("USB interface already claimed")
        }
        // claim_interface does not invoke nusb's separate detach_and_claim_interface API.
        self.interface = Some(
            self.device
                .as_ref()
                .context("USB device closed")?
                .claim_interface(selection.interface)
                .await
                .context("claim USB interface without detaching its owner")?,
        );
        if self.interface.as_ref().unwrap().get_alt_setting() != 0 {
            self.interface.take();
            bail!("USB inventory requires the default alternate setting")
        }
        Ok(())
    }

    async fn write(&mut self, endpoint: u8, bytes: &[u8]) -> Result<usize> {
        let interface = self
            .interface
            .as_ref()
            .context("USB interface is not claimed")?;
        let mut pipe = interface.endpoint::<Bulk, Out>(endpoint)?;
        pipe.submit(bytes.to_vec().into());
        let completion = pipe.next_complete().await;
        completion.status.context("USB bulk OUT")?;
        Ok(completion.actual_len)
    }

    async fn read(&mut self, endpoint: u8, max_length: usize) -> Result<Vec<u8>> {
        if !self.buffered.is_empty() {
            let count = max_length.min(self.buffered.len());
            return Ok(self.buffered.drain(..count).collect());
        }
        let interface = self
            .interface
            .as_ref()
            .context("USB interface is not claimed")?;
        let mut pipe = interface.endpoint::<Bulk, In>(endpoint)?;
        let packet_size = pipe.max_packet_size();
        let request_length = max_length.div_ceil(packet_size) * packet_size;
        // nusb requires packet alignment. Keep any excess for the next bounded read.
        if request_length > TRANSFER_LIMIT {
            bail!("USB native read exceeds transfer bound")
        }
        pipe.submit(Buffer::new(request_length));
        let completion = pipe.next_complete().await;
        completion.status.context("USB bulk IN")?;
        let bytes = completion.buffer.to_vec();
        let count = bytes.len().min(max_length);
        self.buffered.extend(&bytes[count..]);
        Ok(bytes[..count].to_vec())
    }

    async fn release(&mut self) -> Result<()> {
        self.interface.take();
        self.device.take();
        Ok(())
    }
}

pub struct UsbInventoryPlan {
    connection: String,
    establishment: EstablishmentPlan,
    request: ActionInvocationRequest,
    triple: (u8, u8, u8),
}

impl UsbInventoryPlan {
    /// Prove the complete executable scope before opening the selected device.
    pub fn prepare(
        store: &ConfigStore,
        model: &str,
        connection: &str,
        max_bytes: usize,
    ) -> Result<Self> {
        if !(TRANSFER_LIMIT..=64 * 1024 * 1024).contains(&max_bytes) {
            bail!("USB max-frame-bytes must be between 65536 and 67108864")
        }
        let platform = match std::env::consts::OS {
            "linux" => Platform::Linux,
            "macos" => Platform::Macos,
            _ => bail!("native USB inventory is supported on Linux and macOS"),
        };
        let info = store
            .connections(platform)
            .into_iter()
            .find(|c| c.id == connection)
            .context("USB connection is unavailable on this platform or firmware")?;
        if info.kind != "usb"
            || !matches!(info.command_framing, Some(PtpFraming::Usb))
            || !matches!(
                info.session_ownership,
                Some(FfiSessionOwnership::InitiatorOwned)
            )
        {
            bail!(
                "USB inventory requires raw usb, USB framing, and initiatorOwned session ownership"
            )
        }
        let establishment = store
            .establishment(model.into(), connection.into(), vec![])
            .context("USB inventory requires a manufacturer-index establishment plan")?;
        if establishment.prerequisite.is_some()
            || !establishment.params.is_empty()
            || !establishment.post_exit_readiness.is_empty()
            || establishment.steps.len() > 5
        {
            bail!("USB inventory does not support conditional or parameterized establishment")
        }
        let Some(Step::UsbClaim {
            class,
            subclass,
            protocol,
            opts,
        }) = establishment.steps.first()
        else {
            bail!("USB establishment must start with usbClaim")
        };
        if opts.tolerant || opts.retries != 0 {
            bail!("USB claim must not be tolerant or retried")
        }
        let triple = (*class, *subclass, *protocol);
        // Initial support is claim + OpenSession, optionally followed by GetDeviceInfo.
        if establishment.steps.len() != 3 && establishment.steps.len() != 5 {
            bail!("USB establishment must claim then open, with optional GetDeviceInfo")
        }
        for (index, pair) in establishment.steps[1..].chunks_exact(2).enumerate() {
            let [Step::UsbBulkOut {
                data: StepValue::Literal { bytes },
                opts: out_opts,
            }, Step::UsbBulkIn {
                max_length,
                opts: in_opts,
                ..
            }] = pair
            else {
                bail!("USB inventory establishment permits literal command/read pairs only")
            };
            if out_opts.tolerant
                || out_opts.retries != 0
                || in_opts.tolerant
                || in_opts.retries != 0
                || *max_length == 0
                || *max_length as usize > TRANSFER_LIMIT
            {
                bail!("USB establishment requires bounded, non-tolerant commands without retries")
            }
            let PtpIpPacket::OperationRequest(command) = decode(bytes)? else {
                bail!("USB establishment contains a non-command write")
            };
            if command.transaction_id != index as u32
                || match index {
                    0 => {
                        command.code != 0x1002
                            || command.params.len() != 1
                            || command.params[0] == 0
                    }
                    1 => command.code != 0x1001 || !command.params.is_empty(),
                    _ => true,
                }
            {
                bail!("USB establishment exceeds the OpenSession/GetDeviceInfo inventory scope")
            }
        }
        let action = store
            .action(connection.into(), ActionVerb::ReadDeviceInfo)
            .context("USB connection has no readDeviceInfo action")?;
        let initiator = action
            .initiator
            .as_ref()
            .context("readDeviceInfo has no initiator binding")?;
        if !initiator.params.is_empty()
            || !matches!(initiator.steps.as_slice(),
            [EntryStep::SendOp { op: 0x1001, params, repeat: 1, tolerant: false, .. }] if params.is_empty())
        {
            bail!("USB readDeviceInfo must be exactly one parameter-free 0x1001 operation")
        }
        let request = prepare_action_request(
            store,
            connection,
            ActionVerb::ReadDeviceInfo,
            &action.mode,
            vec![],
        )?;
        Ok(Self {
            connection: connection.into(),
            establishment,
            request,
            triple,
        })
    }

    pub fn interface_triple(&self) -> (u8, u8, u8) {
        self.triple
    }
}

/// Load the index registry while retaining the explicit body/model selection.
pub fn load_usb_store(
    manifest: &Path,
    index: &Path,
    model: &str,
    manufacturer: Option<&Path>,
    overlays: &[PathBuf],
) -> Result<Arc<ConfigStore>> {
    let index_yaml = std::fs::read_to_string(index).context("read manufacturer index")?;
    let refs = camera_protocol_ffi::index_model_refs(index_yaml.clone())?;
    let root = index
        .parent()
        .context("manufacturer index has no parent directory")?;
    let selected = refs
        .iter()
        .find(|entry| entry.id == model)
        .context("model is absent from manufacturer index")?;
    if std::fs::canonicalize(root.join(&selected.manifest_path))?
        != std::fs::canonicalize(manifest)?
    {
        bail!("--manifest does not match the selected manufacturer-index model")
    }
    let overlay_yaml = overlays
        .iter()
        .map(std::fs::read_to_string)
        .collect::<std::io::Result<Vec<_>>>()?;
    let mut bodies = Vec::new();
    for entry in refs {
        let mut body = std::fs::read_to_string(root.join(entry.manifest_path))?;
        if entry.id == model && !overlay_yaml.is_empty() {
            let overlay_refs = overlay_yaml.iter().map(String::as_str).collect::<Vec<_>>();
            body = serde_yaml::to_string(&camera_config::CameraManifest::from_tiers(
                &body,
                &overlay_refs,
            )?)?;
        }
        bodies.push(camera_protocol_ffi::KeyValue {
            key: entry.id,
            value: body,
        });
    }
    let store = if let Some(manufacturer) = manufacturer {
        ConfigStore::from_manufacturer_index_with_defaults(
            index_yaml,
            std::fs::read_to_string(manufacturer)?,
            bodies,
        )?
    } else {
        ConfigStore::from_manufacturer_index(index_yaml, bodies)?
    };
    store
        .model_store(model.into())
        .context("selected USB model store is unavailable")
}

/// Reject output aliases before creating artifacts or claiming the interface.
pub fn validate_usb_artifact_paths(
    inputs: &[PathBuf],
    output_dir: &Path,
    trace: &str,
    observation: &Path,
) -> Result<()> {
    let resolve = |path: &Path| -> Result<PathBuf> {
        let absolute = if path.is_absolute() {
            path.to_path_buf()
        } else {
            std::env::current_dir()?.join(path)
        };
        if absolute.exists() {
            return Ok(std::fs::canonicalize(absolute)?);
        }
        let parent = absolute.parent().context("artifact has no parent")?;
        Ok(std::fs::canonicalize(parent)?
            .join(absolute.file_name().context("artifact has no file name")?))
    };
    if output_dir.exists() || observation.exists() || (trace != "-" && Path::new(trace).exists()) {
        bail!("USB inventory requires new output, trace, and observation paths")
    }
    let output = resolve(output_dir)?;
    let observation = resolve(observation)?;
    let trace = if trace == "-" {
        None
    } else {
        Some(resolve(Path::new(trace))?)
    };
    let inputs = inputs
        .iter()
        .map(|path| resolve(path))
        .collect::<Result<Vec<_>>>()?;
    if observation.starts_with(&output)
        || inputs
            .iter()
            .any(|input| input.starts_with(&output) || *input == observation)
        || trace.as_ref().is_some_and(|trace| {
            trace.starts_with(&output) || *trace == observation || inputs.contains(trace)
        })
    {
        bail!("USB inventory artifact paths overlap an input or reserved output")
    }
    Ok(())
}

struct Pending {
    operation: u16,
    transaction: u32,
    had_data: bool,
}

#[derive(Default)]
struct ProtocolState {
    claimed: bool,
    released: bool,
    cleanup_failure: Option<String>,
    session_open: bool,
    next_transaction: u32,
    last_response: Option<u16>,
    pending: Option<Pending>,
    partial: Vec<u8>,
    frames: VecDeque<Vec<u8>>,
    inventory_payloads: Vec<(u32, Vec<u8>)>,
    failure: Option<String>,
}

pub struct UsbInventoryTransport {
    io: Mutex<Box<dyn UsbInventoryIo>>,
    state: Mutex<ProtocolState>,
    selection: UsbSelection,
    triple: (u8, u8, u8),
    connection: String,
    timeout: Duration,
    max_bytes: usize,
    trace: Arc<TraceWriter>,
}

impl UsbInventoryTransport {
    pub fn new(
        io: Box<dyn UsbInventoryIo>,
        plan: &UsbInventoryPlan,
        timeout: Duration,
        max_bytes: usize,
        trace: Arc<TraceWriter>,
    ) -> Result<Arc<Self>> {
        if !(TRANSFER_LIMIT..=64 * 1024 * 1024).contains(&max_bytes) {
            bail!("USB max-frame-bytes must be between 65536 and 67108864")
        }
        if timeout.is_zero() {
            bail!("USB transfer timeout must be positive")
        }
        let selection = select_usb_interface(io.descriptors(), plan.triple)?;
        Ok(Arc::new(Self {
            io: Mutex::new(io),
            state: Mutex::new(ProtocolState::default()),
            selection,
            triple: plan.triple,
            connection: plan.connection.clone(),
            timeout,
            max_bytes,
            trace,
        }))
    }

    async fn write_command(&self, bytes: Vec<u8>) -> Result<()> {
        let mut state = self.state.lock().await;
        if !state.claimed {
            bail!("USB interface is not claimed")
        }
        if state.failure.is_some()
            || state.pending.is_some()
            || !state.partial.is_empty()
            || !state.frames.is_empty()
        {
            bail!("USB command stream is not synchronized")
        }
        let PtpIpPacket::OperationRequest(command) = decode(&bytes)? else {
            bail!("USB inventory accepts command containers only")
        };
        let allowed = match command.code {
            0x1002 => !state.session_open && command.params.len() == 1 && command.params[0] != 0,
            0x1001 | 0x1003 => state.session_open && command.params.is_empty(),
            _ => false,
        };
        if !allowed || command.transaction_id != state.next_transaction {
            bail!("USB operation or transaction is outside the inventory sequence")
        }
        // Reserve before I/O. A partial write cannot authorize replay of the same transaction.
        state.next_transaction = state
            .next_transaction
            .checked_add(1)
            .context("USB transaction ID exhausted")?;
        state.pending = Some(Pending {
            operation: command.code,
            transaction: command.transaction_id,
            had_data: false,
        });
        let mut io = self.io.lock().await;
        let count = tokio::time::timeout(self.timeout, io.write(self.selection.bulk_out, &bytes))
            .await
            .context("USB bulk OUT timed out")??;
        if count != bytes.len() {
            bail!("USB bulk OUT was short ({count}/{})", bytes.len())
        }
        self.trace.wire("tx", "usb-bulk-out", &bytes)?;
        self.trace.ptp_frame("tx", PtpFraming::Usb, &bytes)?;
        Ok(())
    }

    async fn read_transfer(&self, max_length: usize, queue: bool) -> Result<Vec<u8>> {
        let result = self.read_transfer_inner(max_length, queue).await;
        if let Err(error) = &result {
            self.state.lock().await.failure = Some(error.to_string());
        }
        result
    }

    async fn read_transfer_inner(&self, max_length: usize, queue: bool) -> Result<Vec<u8>> {
        let mut state = self.state.lock().await;
        if !state.claimed {
            bail!("USB interface is not claimed")
        }
        let mut io = self.io.lock().await;
        let bytes = tokio::time::timeout(self.timeout, io.read(self.selection.bulk_in, max_length))
            .await
            .context("USB bulk IN timed out")??;
        if bytes.len() > max_length {
            bail!("USB adapter exceeded its read bound")
        }
        self.trace.wire("rx", "usb-bulk-in", &bytes)?;
        state.partial.extend_from_slice(&bytes);
        loop {
            if state.partial.len() < 4 {
                break;
            }
            let length = u32::from_le_bytes(state.partial[..4].try_into().unwrap()) as usize;
            if length < HEADER_LENGTH || length > self.max_bytes {
                bail!("USB container length {length} is outside the configured bound")
            }
            if state.partial.len() < length {
                break;
            }
            let frame = state.partial.drain(..length).collect::<Vec<_>>();
            self.accept_frame(&mut state, &frame)?;
            if queue {
                state.frames.push_back(frame);
            }
        }
        Ok(bytes)
    }

    fn accept_frame(&self, state: &mut ProtocolState, frame: &[u8]) -> Result<()> {
        let packet = decode(frame)?;
        let pending = state
            .pending
            .as_mut()
            .context("unsolicited USB command container")?;
        match &packet {
            PtpIpPacket::Data(data)
                if pending.operation == 0x1001 && data.transaction_id == pending.transaction =>
            {
                if pending.had_data
                    || u16::from_le_bytes(frame[6..8].try_into().unwrap()) != pending.operation
                {
                    bail!("USB DeviceInfo data container is duplicated or has the wrong operation")
                }
                pending.had_data = true;
                state
                    .inventory_payloads
                    .push((data.transaction_id, data.payload.clone()));
            }
            PtpIpPacket::OperationResponse(response)
                if response.transaction_id == pending.transaction =>
            {
                state.last_response = Some(response.code);
                let pending = state.pending.take().unwrap();
                if response.code == 0x2001 {
                    match pending.operation {
                        0x1002 => {
                            state.session_open = true;
                            self.trace
                                .session("opened", json!({"connection": self.connection}))?;
                        }
                        0x1003 => {
                            state.session_open = false;
                            self.trace
                                .session("closed", json!({"connection": self.connection}))?;
                        }
                        0x1001 => {}
                        _ => unreachable!(),
                    }
                }
            }
            _ => bail!("USB response does not match the pending operation and transaction"),
        }
        self.trace.ptp_frame("rx", PtpFraming::Usb, frame)?;
        Ok(())
    }

    async fn finish_establishment(&self) -> Result<()> {
        tokio::time::timeout(self.timeout, async {
            loop {
                let state = self.state.lock().await;
                if state.pending.is_none() && state.partial.is_empty() {
                    break;
                }
                drop(state);
                self.read_transfer(TRANSFER_LIMIT, false).await?;
            }
            let state = self.state.lock().await;
            if !state.session_open {
                bail!("USB OpenSession did not return OK; no session ownership acquired")
            }
            Ok(())
        })
        .await
        .context("USB establishment completion timed out")?
    }

    async fn close_session(&self) -> Result<()> {
        let state = self.state.lock().await;
        if !state.session_open {
            return Ok(());
        }
        if state.pending.is_some()
            || !state.partial.is_empty()
            || !state.frames.is_empty()
            || state.failure.is_some()
        {
            bail!("USB session could not be closed because the command stream is not synchronized")
        }
        let transaction_id = state.next_transaction;
        drop(state);
        self.write_command(encode_command(0x1003, transaction_id, vec![])?)
            .await?;
        tokio::time::timeout(self.timeout, async {
            loop {
                if self.state.lock().await.pending.is_none() {
                    break;
                }
                self.read_transfer(TRANSFER_LIMIT, false).await?;
            }
            if self.state.lock().await.session_open {
                bail!("USB CloseSession did not return OK")
            }
            Ok(())
        })
        .await
        .context("USB CloseSession timed out")?
    }

    async fn cleanup(&self) -> Result<()> {
        {
            let state = self.state.lock().await;
            if state.released {
                return if let Some(failure) = &state.cleanup_failure {
                    Err(anyhow::anyhow!(failure.clone()))
                } else {
                    Ok(())
                };
            }
        }
        let close = self.close_session().await;
        let mut state = self.state.lock().await;
        let mut io = self.io.lock().await;
        let release = io.release().await;
        state.claimed = false;
        state.released = true;
        let result = close.and(release);
        state.cleanup_failure = result.as_ref().err().map(ToString::to_string);
        result
    }
}

#[async_trait]
impl UsbExecutorTransport for UsbInventoryTransport {
    async fn claim_interface(
        &self,
        class: u8,
        subclass: u8,
        protocol: u8,
    ) -> Result<(), UsbTransportError> {
        if (class, subclass, protocol) != self.triple {
            return Err(usb_error("USB claim differs from preflight"));
        }
        let mut state = self.state.lock().await;
        if state.claimed {
            return Err(usb_error("USB interface already claimed"));
        }
        let mut io = self.io.lock().await;
        tokio::time::timeout(self.timeout, io.claim(&self.selection))
            .await
            .context("USB claim timed out")
            .map_err(usb_error)?
            .map_err(usb_error)?;
        state.claimed = true;
        self.trace.begin_ptp_session().map_err(usb_error)?;
        Ok(())
    }
    async fn bulk_out(&self, bytes: Vec<u8>) -> Result<(), UsbTransportError> {
        self.write_command(bytes).await.map_err(usb_error)
    }
    async fn bulk_in(&self, max_length: u32) -> Result<Vec<u8>, UsbTransportError> {
        if max_length == 0 || max_length as usize > TRANSFER_LIMIT {
            return Err(usb_error("USB read exceeds transfer bound"));
        }
        let result = tokio::time::timeout(self.timeout, async {
            let mut bytes = Vec::new();
            loop {
                if bytes.len() == max_length as usize {
                    bail!("USB establishment response exceeds its read bound")
                }
                bytes.extend(
                    self.read_transfer(max_length as usize - bytes.len(), false)
                        .await?,
                );
                let state = self.state.lock().await;
                if state.pending.is_none() && state.partial.is_empty() {
                    if state.last_response != Some(0x2001) {
                        bail!(
                            "USB establishment returned non-OK response {:?}",
                            state.last_response
                        )
                    }
                    return Ok(bytes);
                }
            }
        })
        .await
        .context("USB establishment read timed out")
        .and_then(|value| value);
        result.map_err(usb_error)
    }
    async fn next_interrupt_event(&self) -> Result<Vec<u8>, UsbTransportError> {
        Err(usb_error("USB inventory does not consume interrupt events"))
    }
    async fn release_and_close(&self) -> Result<(), UsbTransportError> {
        self.cleanup().await.map_err(usb_error)
    }
    async fn sleep(&self, ms: u32) -> Result<(), UsbTransportError> {
        tokio::time::sleep(Duration::from_millis(ms.into())).await;
        Ok(())
    }
}

#[async_trait]
impl PtpExecutorTransport for UsbInventoryTransport {
    async fn reserve_transaction_id(&self) -> Result<u32, PtpTransportError> {
        Ok(self.state.lock().await.next_transaction)
    }
    async fn send_command_frame(&self, frame: Vec<u8>) -> Result<(), PtpTransportError> {
        self.write_command(frame).await.map_err(ptp_error)
    }
    async fn next_command_frame(&self) -> Result<Vec<u8>, PtpTransportError> {
        let result = tokio::time::timeout(self.timeout, async {
            loop {
                if let Some(frame) = self.state.lock().await.frames.pop_front() {
                    return Ok(frame);
                }
                self.read_transfer(TRANSFER_LIMIT, true).await?;
            }
        })
        .await
        .context("USB container read timed out")
        .and_then(|value| value);
        if let Err(error) = &result {
            self.state.lock().await.failure = Some(error.to_string());
        }
        result.map_err(ptp_error)
    }
    async fn next_event_frame(&self, _event_code: u16) -> Result<Vec<u8>, PtpTransportError> {
        Err(ptp_error("USB inventory does not consume events"))
    }
    async fn open_channel(&self, _role: SocketRole) -> Result<(), PtpTransportError> {
        Err(ptp_error("USB inventory has no auxiliary channels"))
    }
    async fn close_command_channel(
        &self,
        _frame: Option<Vec<u8>>,
    ) -> Result<(), PtpTransportError> {
        Err(ptp_error(
            "USB inventory cannot close from a manifest action",
        ))
    }
    async fn reopen_command_session(&self) -> Result<PtpSessionOpenResult, PtpTransportError> {
        Err(ptp_error("USB inventory does not reopen sessions"))
    }
    async fn sleep(&self, ms: u32) -> Result<(), PtpTransportError> {
        tokio::time::sleep(Duration::from_millis(ms.into())).await;
        Ok(())
    }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UsbInventoryReport {
    pub schema: String,
    pub selection: UsbSelection,
    pub device_info_payload: PathBuf,
    pub payload: PayloadMetadata,
    pub standard_version: u16,
    pub vendor_extension_id: u32,
    pub vendor_extension_version: u16,
    pub vendor_extension_desc: String,
    pub functional_mode: u16,
    pub operations_supported: Vec<u16>,
    pub events_supported: Vec<u16>,
    pub device_properties_supported: Vec<u16>,
    pub capture_formats: Vec<u16>,
    pub image_formats: Vec<u16>,
    pub manufacturer: String,
    pub model: String,
    pub device_version: String,
    pub serial_number: String,
}

/// Run the restricted inventory sequence, always attempting session cleanup and release.
pub async fn run_usb_inventory(
    plan: UsbInventoryPlan,
    store: Arc<ConfigStore>,
    transport: Arc<UsbInventoryTransport>,
    trace: Arc<TraceWriter>,
    output_dir: &Path,
) -> Result<UsbInventoryReport> {
    if output_dir.exists() {
        bail!(
            "USB inventory output directory already exists: {}",
            output_dir.display()
        )
    }
    std::fs::create_dir(output_dir).context("create USB inventory output directory")?;
    let action_transaction = ((plan.establishment.steps.len() - 1) / 2) as u32;
    let observer = Arc::new(UsbTraceObserver(Arc::clone(&trace)));
    let result = async {
        run_usb_establishment(
            Arc::clone(&store),
            plan.establishment.plan_handle,
            transport.clone(),
            observer.clone(),
            observer.clone(),
            vec![],
            vec![],
            vec![],
        )
        .await?;
        transport.finish_establishment().await?;
        let outcome = run_initiator_action(
            Arc::clone(&store),
            plan.request,
            transport.clone(),
            observer.clone(),
            observer,
        )
        .await?;
        let [output] = outcome.outputs.as_slice() else {
            bail!("readDeviceInfo did not produce exactly one retained payload")
        };
        let payload_path = PathBuf::from("device-info.bin");
        std::fs::write(output_dir.join(&payload_path), &output.payload)
            .context("retain DeviceInfo payload")?;
        // Decode only after retaining the complete wire payload, including malformed datasets.
        let info = DeviceInfo::decode(&output.payload)
            .context("decode complete USB DeviceInfo dataset")?;
        let mut metadata = PayloadMetadataBuilder::new();
        metadata.update(&output.payload);
        Ok(UsbInventoryReport {
            schema: "camera-initiator-usb-inventory/v1".into(),
            selection: transport.selection.clone(),
            device_info_payload: payload_path,
            payload: metadata.metadata(),
            standard_version: info.standard_version,
            vendor_extension_id: info.vendor_extension_id,
            vendor_extension_version: info.vendor_extension_version,
            vendor_extension_desc: info.vendor_extension_desc,
            functional_mode: info.functional_mode,
            operations_supported: info.operations_supported,
            events_supported: info.events_supported,
            device_properties_supported: info.device_properties_supported,
            capture_formats: info.capture_formats,
            image_formats: info.image_formats,
            manufacturer: info.manufacturer,
            model: info.model,
            device_version: info.device_version,
            serial_number: info.serial_number,
        })
    }
    .await;
    let retention = {
        let state = transport.state.lock().await;
        state
            .inventory_payloads
            .iter()
            .try_for_each(|(transaction, payload)| {
                let name = if *transaction < action_transaction {
                    format!("establishment-device-info-{transaction}.bin")
                } else {
                    "device-info.bin".into()
                };
                std::fs::write(output_dir.join(name), payload)
                    .context("retain received USB DeviceInfo payload")
            })
    };
    let cleanup = transport.cleanup().await;
    let result = match (result, retention) {
        (Ok(report), Ok(())) => Ok(report),
        (Err(primary), Ok(())) => Err(primary),
        (Ok(_), Err(retention)) => Err(retention),
        (Err(primary), Err(retention)) => {
            Err(primary.context(format!("USB payload retention also failed: {retention}")))
        }
    };
    trace.action(
        store.action_catalog().revision,
        "readDeviceInfo".into(),
        BTreeMap::new(),
        if result.is_ok() {
            ActionOutcome::Succeeded
        } else {
            ActionOutcome::Failed
        },
    )?;
    match (result, cleanup) {
        (Ok(report), Ok(())) => {
            std::fs::write(
                output_dir.join("report.json"),
                serde_json::to_vec_pretty(&report)?,
            )?;
            Ok(report)
        }
        (Err(primary), Ok(())) => Err(primary),
        (Err(primary), Err(cleanup)) => {
            Err(primary.context(format!("USB cleanup also failed: {cleanup}")))
        }
        (Ok(_), Err(cleanup)) => Err(cleanup.context("USB inventory completed but cleanup failed")),
    }
    .with_context(|| format!("USB inventory on {}", plan.connection))
}

struct UsbTraceObserver(Arc<TraceWriter>);
impl StepObserver for UsbTraceObserver {
    fn on_step(&self, report: StepReport) {
        let _ = self.0.step(&report);
    }
}
impl ConnectionActivityObserver for UsbTraceObserver {
    fn on_activity(&self, event: ConnectionActivityEvent) {
        let _ = self.0.activity(&event);
    }
}

fn encode_command(code: u16, transaction_id: u32, params: Vec<u32>) -> Result<Vec<u8>> {
    protocol_primitives::usb_ptp::encode(&PtpIpPacket::OperationRequest(OperationRequest {
        data_phase_info: 1,
        code,
        transaction_id,
        params,
    }))
    .map_err(|error| anyhow::anyhow!("encode USB command: {error:?}"))
}
fn decode(bytes: &[u8]) -> Result<PtpIpPacket> {
    protocol_primitives::usb_ptp::decode(bytes)
        .map_err(|error| anyhow::anyhow!("decode USB container: {error:?}"))
}
fn usb_error(error: impl std::fmt::Display) -> UsbTransportError {
    UsbTransportError::Failed {
        detail: error.to_string(),
    }
}
fn ptp_error(error: impl std::fmt::Display) -> PtpTransportError {
    PtpTransportError::Failed {
        detail: error.to_string(),
    }
}
