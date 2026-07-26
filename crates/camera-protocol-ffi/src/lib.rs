//! `camera-protocol-ffi` — the iOS/macOS (Swift) seam over `camera-config`.
//!
//! Designed `(connection, mode)`-keyed (see `docs/plans/ffi-surface.md`) so that
//! adding wireless-tether/USB to the app is a manifest row + the app's own socket
//! I/O — never a change to this surface. Sans-io: every query is pure over manifest
//! data + observed values the app supplies; nothing here touches a socket/USB/BLE.
//!
//! This is §A (the transport-abstraction query surface). §B (the byte codecs,
//! G1–G3) flows through the same crate and is a parallel workstream.

#![allow(clippy::missing_safety_doc)]

use camera_config as cc;
use cc::parse_hex_code;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

pub mod executor;
pub use executor::{
    run_ble_action, run_establishment, run_post_exit_readiness, BleExecutorTransport,
    ConnectionActivityEvent, ConnectionActivityFailure, ConnectionActivityObserver,
    ConnectionActivityRetry, ConnectionActivityTerminalSummary, EstablishmentConfirmOutcome,
    EstablishmentWalkSummary, ExecutionOutcome, ExecutorError, ExecutorStepFailureKind,
    StepObserver, StepOutcome, StepReport, TransportError,
};
pub mod ptp_executor;
pub use ptp_executor::{
    run_initiator_action, run_initiator_action_to_sink, run_mode_entry,
    run_mode_reestablishment_exit, run_selected_object_preparation, PtpCollectionValue,
    PtpDataOutput, PtpDataOutputSink, PtpDataOutputSinkError, PtpExecutionOutcome,
    PtpExecutorError, PtpExecutorTransport, PtpRuntimeValue, PtpScopeValue, PtpSessionOpenResult,
    PtpTransportError,
};
pub mod pcss_executor;
pub use pcss_executor::{
    run_pcss_auto_establishment, run_pcss_known_address_establishment, PcssCallback,
    PcssEstablishmentOutcome, PcssExecutorError, PcssExecutorTransport,
};
pub mod streaming_executor;
pub use streaming_executor::{
    run_streaming_action, run_streaming_operation, PtpStreamingError, PtpStreamingOutcome,
    PtpStreamingSink, PtpStreamingSinkError, PtpStreamingTransport,
};
pub mod mfg_index;
pub use mfg_index::{
    AcquireSource, AwaitSource, BleActionPlan, BleAdRecord, BleManufacturerData, BleNotifyUntil,
    BleServiceData, CccdMode, ChunkField, ChunkFrameField, Confidence, ConnectionActivityBinding,
    ConnectionActivityDescriptor, ConnectionActivityDisplayRole, ConnectionActivitySequence,
    EstablishmentPlan, EstablishmentRefinement, HostEstablishment, ModelMatch, NotifyCapture,
    Predicate, PredicateOp, Recognition, ReconnectDecision, ReconnectPolicy, ScanObservation, Step,
    StepConfirmation, StepOptions, StepValue, Transform,
};
mod observation_ffi;
pub use observation_ffi::*;

uniffi::setup_scaffolding!();

#[allow(dead_code)] // ReplaceTail is exercised through the executor's injected resolver seam.
pub(crate) enum NativeEstablishmentRefinement {
    NoChange,
    ReplaceTail {
        steps: Vec<camera_config::index::Step>,
        activities: Vec<camera_config::ConnectionActivityDescriptor>,
    },
}

/// Crate version, exposed so an FFI consumer can assert ABI/build expectations.
#[uniffi::export]
pub fn version() -> String {
    env!("CARGO_PKG_VERSION").to_string()
}

// ----------------------------------------------------------------------------
// Codec functions (§B / G1–G2): pure intents↔bytes. Sans-io — the app writes
// the returned bytes to its own socket/USB.
// ----------------------------------------------------------------------------

#[derive(Debug, thiserror::Error, uniffi::Error)]
pub enum CodecError {
    #[error("{0}")]
    Encode(String),
    #[error("{0}")]
    Decode(String),
}

fn codec_encode<E: std::fmt::Display>(e: E) -> CodecError {
    CodecError::Encode(e.to_string())
}

fn codec_decode<E: std::fmt::Display>(e: E) -> CodecError {
    CodecError::Decode(e.to_string())
}

/// Property value width on the wire (mirrors `protocol_primitives::ValueWidth`).
/// Signed widths (`I16`/`I32`) carry the camera's declared datatype so a consumer
/// encodes negative values (exposure-bias, ISO auto sentinels) two's-complement.
#[derive(Debug, Clone, Copy, uniffi::Enum)]
pub enum ValueWidth {
    U8,
    U16,
    U32,
    I16,
    I32,
}

impl From<ValueWidth> for protocol_primitives::ValueWidth {
    fn from(w: ValueWidth) -> Self {
        match w {
            ValueWidth::U8 => protocol_primitives::ValueWidth::U8,
            ValueWidth::U16 => protocol_primitives::ValueWidth::U16,
            ValueWidth::U32 => protocol_primitives::ValueWidth::U32,
            ValueWidth::I16 => protocol_primitives::ValueWidth::I16,
            ValueWidth::I32 => protocol_primitives::ValueWidth::I32,
        }
    }
}

/// G1 — build the 82-byte Fuji reference app `InitCommandRequest` from resolved identity.
#[uniffi::export]
pub fn build_app_init(guid: Vec<u8>, friendly_name: String) -> Result<Vec<u8>, CodecError> {
    protocol_primitives::build_app_init(&guid, &friendly_name)
        .map_err(|e| CodecError::Encode(e.to_string()))
}

#[uniffi::export]
pub fn validate_init_ack(packet: Vec<u8>) -> Result<(), CodecError> {
    protocol_primitives::validate_init_ack(&packet).map_err(|e| CodecError::Encode(e.to_string()))
}

/// Validate legacy manufacturer app's fixed 68-byte InitCommandAck against the
/// manifest-resolved responder GUID. Unlike PCSS, legacy manufacturer app does not
/// constrain the bytes after the GUID as a UTF-16 camera name plus zero tail.
#[uniffi::export]
pub fn validate_legacy_app_init_ack(
    packet: Vec<u8>,
    expected_responder_guid: Vec<u8>,
) -> Result<(), CodecError> {
    protocol_primitives::validate_legacy_app_init_ack(&packet, &expected_responder_guid)
        .map_err(|e| CodecError::Decode(e.to_string()))
}

/// Typed result of decoding a standard PTP/IP command-init response.
#[derive(Debug, Clone, uniffi::Enum)]
pub enum InitCommandResponse {
    Acknowledged {
        connection_number: u32,
        responder_guid: Vec<u8>,
        friendly_name: String,
        protocol_version: Option<u32>,
    },
    Failed {
        reason: u32,
    },
}

/// Decode either InitCommandAck or InitFail without making a host recognize
/// packet types or offsets itself.
#[uniffi::export]
pub fn decode_init_command_response(packet: Vec<u8>) -> Result<InitCommandResponse, CodecError> {
    use ptp_core::PtpCodec;

    let is_fixed_pcss_ack = packet.len() == 68
        && packet
            .get(4..8)
            .is_some_and(|bytes| bytes == 2u32.to_le_bytes());
    if is_fixed_pcss_ack {
        let ack = protocol_primitives::parse_pcss_init_ack(&packet).map_err(codec_decode)?;
        return Ok(InitCommandResponse::Acknowledged {
            connection_number: ack.connection_number,
            responder_guid: ack.responder_guid.to_vec(),
            friendly_name: ack.friendly_name,
            protocol_version: None,
        });
    }

    let decoded = ptp_core::PtpIpPacket::decode(&packet).map_err(codec_decode)?;
    match decoded {
        ptp_core::PtpIpPacket::InitCommandAck(ack) => {
            let decoded = ptp_core::PtpIpPacket::InitCommandAck(ack.clone());
            require_canonical_init_response(&packet, &decoded)?;
            Ok(InitCommandResponse::Acknowledged {
                connection_number: ack.connection_number,
                responder_guid: ack.responder_guid.to_vec(),
                friendly_name: ack.friendly_name,
                protocol_version: Some(ack.protocol_version),
            })
        }
        ptp_core::PtpIpPacket::InitFail(fail) => {
            let decoded = ptp_core::PtpIpPacket::InitFail(fail.clone());
            require_canonical_init_response(&packet, &decoded)?;
            Ok(InitCommandResponse::Failed {
                reason: fail.reason,
            })
        }
        other => Err(CodecError::Decode(format!(
            "expected InitCommandAck or InitFail, got {}",
            ptpip_packet_kind(&other)
        ))),
    }
}

fn require_canonical_init_response(
    packet: &[u8],
    decoded: &ptp_core::PtpIpPacket,
) -> Result<(), CodecError> {
    let canonical = ptp_core::encode(decoded).map_err(codec_decode)?;
    if canonical == packet {
        Ok(())
    } else {
        Err(CodecError::Decode(
            "non-canonical PTP/IP init response".into(),
        ))
    }
}

fn ptpip_packet_kind(packet: &ptp_core::PtpIpPacket) -> &'static str {
    match packet {
        ptp_core::PtpIpPacket::InitCommandRequest(_) => "InitCommandRequest",
        ptp_core::PtpIpPacket::InitCommandAck(_) => "InitCommandAck",
        ptp_core::PtpIpPacket::InitEventRequest(_) => "InitEventRequest",
        ptp_core::PtpIpPacket::InitEventAck(_) => "InitEventAck",
        ptp_core::PtpIpPacket::InitFail(_) => "InitFail",
        ptp_core::PtpIpPacket::OperationRequest(_) => "OperationRequest",
        ptp_core::PtpIpPacket::OperationResponse(_) => "OperationResponse",
        ptp_core::PtpIpPacket::Event(_) => "Event",
        ptp_core::PtpIpPacket::StartData(_) => "StartData",
        ptp_core::PtpIpPacket::Data(_) => "Data",
        ptp_core::PtpIpPacket::EndData(_) => "EndData",
        ptp_core::PtpIpPacket::ProbeRequest(_) => "ProbeRequest",
        ptp_core::PtpIpPacket::ProbeResponse(_) => "ProbeResponse",
    }
}

/// G1 — normalize a raw host device name into the canonical client name written
/// to both the BLE `deviceNameString` and the PTP/IP friendly name. The host
/// calls this once and feeds the result to the `terminalName` runtime slot, so
/// the two channels are one value (the camera drops an init whose channels
/// disagree, #109). Replaces the app's own name-normalization (#139).
#[uniffi::export]
pub fn normalize_client_name(raw: String) -> String {
    protocol_primitives::normalize_client_name(&raw)
}

/// G1 — pack a normalized tap `(x, y)` (each `0.0..=1.0`) into the `0x9026`
/// LockS1Lock AF-area u32 for a `columns`×`rows` grid (read from `focus_grid()`).
/// Aspect comes from the prior `0xD17C` lock state, defaulting to 4:3. Replaces
/// the app's hand-rolled focus-area math so tap-to-focus carries no camera knowledge (#135).
#[uniffi::export]
pub fn pack_af_area(x: f64, y: f64, columns: u32, rows: u32, prior_lock_state: Option<u32>) -> u32 {
    protocol_primitives::pack_af_area(x, y, columns, rows, prior_lock_state)
}

/// G2 — encode a resolved value at its property width (the per-value semantics
/// live in the manifest; this just writes the bytes). `value` is signed so signed
/// widths (`I16`/`I32`) can carry negative exposure-bias / ISO auto sentinels.
#[uniffi::export]
pub fn encode_value(value: i64, width: ValueWidth) -> Result<Vec<u8>, CodecError> {
    protocol_primitives::encode_value(value, width.into())
        .map_err(|e| CodecError::Encode(e.to_string()))
}

// ----------------------------------------------------------------------------
// G3 — PTP/IP packet framing + dataset codecs. The app builds/parses its own
// wire bytes over its own socket; these turn intents↔bytes. Sans-io.
// ----------------------------------------------------------------------------

/// Which PTP/IP wire framing to build or parse with. The consumer reads this from
/// the manifest — `ConnectionInfo.command_framing` / `event_framing` — so the
/// connection→framing choice is data, never a mapping in the app's own code. All
/// three share the same logical packets; only the header differs.
#[derive(Debug, Clone, Copy, uniffi::Enum)]
pub enum PtpFraming {
    Standard,
    Compressed,
    Usb,
}

pub(crate) fn frame_encode(
    framing: PtpFraming,
    pkt: &ptp_core::PtpIpPacket,
) -> Result<Vec<u8>, CodecError> {
    match framing {
        PtpFraming::Standard => ptp_core::encode(pkt).map_err(codec_encode),
        PtpFraming::Compressed => {
            protocol_primitives::fuji_framing::encode(pkt).map_err(codec_encode)
        }
        PtpFraming::Usb => protocol_primitives::usb_ptp::encode(pkt).map_err(codec_encode),
    }
}

pub(crate) fn frame_decode(
    framing: PtpFraming,
    bytes: &[u8],
) -> Result<ptp_core::PtpIpPacket, CodecError> {
    use ptp_core::PtpCodec;
    match framing {
        PtpFraming::Standard => ptp_core::PtpIpPacket::decode(bytes),
        PtpFraming::Compressed => protocol_primitives::fuji_framing::decode(bytes),
        PtpFraming::Usb => protocol_primitives::usb_ptp::decode(bytes),
    }
    .map_err(codec_decode)
}

/// Build an operation-request frame. The standard framing's `DataPhaseInfo`
/// defaults to 1 (no data-out); the compressed/USB framings carry no such field.
/// A data-out command is expressed by following this with [`build_data`].
#[uniffi::export]
pub fn build_command(
    framing: PtpFraming,
    op: u16,
    txn: u32,
    params: Vec<u32>,
) -> Result<Vec<u8>, CodecError> {
    let pkt = ptp_core::PtpIpPacket::OperationRequest(ptp_core::OperationRequest {
        data_phase_info: 1,
        code: op,
        transaction_id: txn,
        params,
    });
    frame_encode(framing, &pkt)
}

/// Build a data-phase frame carrying `payload` for transaction `txn` of operation
/// `op`. Fuji's compressed framing puts the whole data phase in one type-2 frame
/// whose code field echoes `op`; standard framing's `Data` frame carries no
/// opcode, so `op` is unused there.
#[uniffi::export]
pub fn build_data(
    framing: PtpFraming,
    op: u16,
    txn: u32,
    payload: Vec<u8>,
) -> Result<Vec<u8>, CodecError> {
    match framing {
        PtpFraming::Compressed => Ok(protocol_primitives::fuji_framing::encode_data(
            op, txn, &payload,
        )),
        PtpFraming::Usb => Ok(protocol_primitives::usb_ptp::encode_data(op, txn, &payload)),
        PtpFraming::Standard => {
            let pkt = ptp_core::PtpIpPacket::Data(ptp_core::DataBlock {
                transaction_id: txn,
                payload,
            });
            frame_encode(framing, &pkt)
        }
    }
}

/// A decoded PTP operation response.
#[derive(Debug, uniffi::Record)]
pub struct ResponseFrame {
    pub response_code: u16,
    pub txn: u32,
    pub params: Vec<u32>,
}

/// Parse an operation-response frame. Errors if the frame is not a response.
#[uniffi::export]
pub fn parse_response(framing: PtpFraming, packet: Vec<u8>) -> Result<ResponseFrame, CodecError> {
    match frame_decode(framing, &packet)? {
        ptp_core::PtpIpPacket::OperationResponse(r) => Ok(ResponseFrame {
            response_code: r.code,
            txn: r.transaction_id,
            params: r.params,
        }),
        other => Err(CodecError::Decode(format!(
            "expected an operation response, got {other:?}"
        ))),
    }
}

/// Extract the payload bytes from a data-phase frame (`Data` or `EndData`).
#[uniffi::export]
pub fn parse_data_payload(framing: PtpFraming, packet: Vec<u8>) -> Result<Vec<u8>, CodecError> {
    match frame_decode(framing, &packet)? {
        ptp_core::PtpIpPacket::Data(d) | ptp_core::PtpIpPacket::EndData(d) => Ok(d.payload),
        other => Err(CodecError::Decode(format!(
            "expected a data-phase frame, got {other:?}"
        ))),
    }
}

/// Which data-phase frame this is. Standard PTP/IP streams a transfer as `Start`
/// (announces the total length) → zero or more `Data` → `End`. The Fuji
/// compressed and USB container channels deliver the whole payload in a single
/// `Data` frame (no `Start`/`End`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum DataPhaseKind {
    Start,
    Data,
    End,
}

/// One decoded data-phase frame. `total_length` is set only on `Start`; `payload`
/// carries the bytes of a `Data`/`End` (on a single-frame channel that one `Data`
/// is the entire payload). The operation code is not in the data phase — a
/// consumer correlates by `txn` with the preceding operation request.
#[derive(Debug, uniffi::Record)]
pub struct DataPhaseFrame {
    pub kind: DataPhaseKind,
    pub txn: u32,
    pub total_length: Option<u64>,
    pub payload: Vec<u8>,
}

/// Decode one data-phase frame. Standard framing distinguishes
/// `StartData`/`Data`/`EndData` so a consumer can drive a streamed transfer; the
/// Fuji compressed and USB channels deliver the whole payload in one `Data` frame.
/// Either way the consumer accumulates `payload` until the operation response
/// arrives — no framing-specific branching. Yields typed errors.
#[uniffi::export]
pub fn parse_data_phase(
    framing: PtpFraming,
    packet: Vec<u8>,
) -> Result<DataPhaseFrame, CodecError> {
    match frame_decode(framing, &packet)? {
        ptp_core::PtpIpPacket::StartData(s) => Ok(DataPhaseFrame {
            kind: DataPhaseKind::Start,
            txn: s.transaction_id,
            total_length: Some(s.total_length),
            payload: Vec::new(),
        }),
        ptp_core::PtpIpPacket::Data(d) => Ok(DataPhaseFrame {
            kind: DataPhaseKind::Data,
            txn: d.transaction_id,
            total_length: None,
            payload: d.payload,
        }),
        ptp_core::PtpIpPacket::EndData(d) => Ok(DataPhaseFrame {
            kind: DataPhaseKind::End,
            txn: d.transaction_id,
            total_length: None,
            payload: d.payload,
        }),
        other => Err(CodecError::Decode(format!(
            "expected a data-phase frame (start/data/end), got {other:?}"
        ))),
    }
}

/// A decoded PTP event packet.
#[derive(Debug, uniffi::Record)]
pub struct CameraEvent {
    pub code: u16,
    pub txn: u32,
    pub params: Vec<u32>,
}

/// Parse a frame from the event socket. `None` when the frame decodes to a
/// non-event packet; an error when the bytes don't decode in `framing`. Standard
/// PTP/IP events are packet-type 8; the USB/PIMA event container is type 4. The
/// Fuji compressed command channel carries no events (they ride a separate
/// socket), so `Compressed` will reject an event frame.
#[uniffi::export]
pub fn parse_event(
    framing: PtpFraming,
    packet: Vec<u8>,
) -> Result<Option<CameraEvent>, CodecError> {
    match frame_decode(framing, &packet)? {
        ptp_core::PtpIpPacket::Event(e) => Ok(Some(CameraEvent {
            code: e.code,
            txn: e.transaction_id,
            params: e.params,
        })),
        _ => Ok(None),
    }
}

/// A typed PTP property value (mirrors `ptp_core::PropValue`, lossless).
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Enum)]
pub enum PtpValue {
    I8 { value: i8 },
    U8 { value: u8 },
    I16 { value: i16 },
    U16 { value: u16 },
    I32 { value: i32 },
    U32 { value: u32 },
    I64 { value: i64 },
    U64 { value: u64 },
    Str { value: String },
}

impl From<&ptp_core::PropValue> for PtpValue {
    fn from(v: &ptp_core::PropValue) -> Self {
        match v {
            ptp_core::PropValue::I8(x) => PtpValue::I8 { value: *x },
            ptp_core::PropValue::U8(x) => PtpValue::U8 { value: *x },
            ptp_core::PropValue::I16(x) => PtpValue::I16 { value: *x },
            ptp_core::PropValue::U16(x) => PtpValue::U16 { value: *x },
            ptp_core::PropValue::I32(x) => PtpValue::I32 { value: *x },
            ptp_core::PropValue::U32(x) => PtpValue::U32 { value: *x },
            ptp_core::PropValue::I64(x) => PtpValue::I64 { value: *x },
            ptp_core::PropValue::U64(x) => PtpValue::U64 { value: *x },
            ptp_core::PropValue::Str(s) => PtpValue::Str { value: s.clone() },
        }
    }
}

/// The value-constraint form of a `DevicePropDesc` (mirrors `ptp_core::PropForm`).
#[derive(Debug, uniffi::Enum)]
pub enum PtpPropForm {
    None,
    Range {
        min: PtpValue,
        max: PtpValue,
        step: PtpValue,
    },
    Enum {
        values: Vec<PtpValue>,
    },
}

impl From<&ptp_core::PropForm> for PtpPropForm {
    fn from(f: &ptp_core::PropForm) -> Self {
        match f {
            ptp_core::PropForm::None => PtpPropForm::None,
            ptp_core::PropForm::Range { min, max, step } => PtpPropForm::Range {
                min: min.into(),
                max: max.into(),
                step: step.into(),
            },
            ptp_core::PropForm::Enum(values) => PtpPropForm::Enum {
                values: values.iter().map(PtpValue::from).collect(),
            },
        }
    }
}

/// A decoded `DevicePropDesc` (generic PTP; presentation/labels are #134 data).
#[derive(Debug, uniffi::Record)]
pub struct PtpDevicePropDesc {
    pub code: u16,
    pub datatype: u16,
    pub get_set: u8,
    pub factory_default: PtpValue,
    pub current: PtpValue,
    pub form: PtpPropForm,
}

/// Parse a `GetDevicePropDesc` data payload.
#[uniffi::export]
pub fn parse_device_prop_desc(payload: Vec<u8>) -> Result<PtpDevicePropDesc, CodecError> {
    let d = ptp_core::DevicePropDesc::decode(&payload).map_err(codec_decode)?;
    Ok(PtpDevicePropDesc {
        code: d.code,
        datatype: d.datatype,
        get_set: d.get_set,
        factory_default: (&d.factory_default).into(),
        current: (&d.current).into(),
        form: (&d.form).into(),
    })
}

/// A decoded `ObjectInfo` (generic ISO-15740 fields only; media classification —
/// still/movie/RAW, Photos-compat — is `media_format()` + #136).
#[derive(Debug, uniffi::Record)]
pub struct PtpObjectInfo {
    pub storage_id: u32,
    pub object_format: u16,
    pub protection_status: u16,
    pub object_compressed_size: u32,
    pub thumb_format: u16,
    pub thumb_compressed_size: u32,
    pub thumb_pix_width: u32,
    pub thumb_pix_height: u32,
    pub image_pix_width: u32,
    pub image_pix_height: u32,
    pub image_bit_depth: u32,
    pub parent_object: u32,
    pub association_type: u16,
    pub association_desc: u32,
    pub sequence_number: u32,
    pub filename: String,
    pub capture_date: String,
    pub modification_date: String,
    pub keywords: String,
}

/// The standard PTP DeviceInfo dataset (0x1001 data phase). `serial_number`
/// is the *actual body's* unit identity — the saved-camera merge key — as
/// opposed to `ConfigStore::camera_identity()`, which is the manifest's
/// *declared* (per-model, sim-synthetic) identity. Don't conflate them (#173).
#[derive(uniffi::Record)]
pub struct PtpDeviceInfo {
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

/// Parse a `GetDeviceInfo` (0x1001) data payload. The operation to send comes
/// from the `readDeviceInfo` action — the app never spells the opcode.
#[uniffi::export]
pub fn parse_device_info(payload: Vec<u8>) -> Result<PtpDeviceInfo, CodecError> {
    let d = ptp_core::DeviceInfo::decode(&payload).map_err(codec_decode)?;
    Ok(PtpDeviceInfo {
        standard_version: d.standard_version,
        vendor_extension_id: d.vendor_extension_id,
        vendor_extension_version: d.vendor_extension_version,
        vendor_extension_desc: d.vendor_extension_desc,
        functional_mode: d.functional_mode,
        operations_supported: d.operations_supported,
        events_supported: d.events_supported,
        device_properties_supported: d.device_properties_supported,
        capture_formats: d.capture_formats,
        image_formats: d.image_formats,
        manufacturer: d.manufacturer,
        model: d.model,
        device_version: d.device_version,
        serial_number: d.serial_number,
    })
}

/// Parse a `GetObjectInfo` data payload.
#[uniffi::export]
pub fn parse_object_info(payload: Vec<u8>) -> Result<PtpObjectInfo, CodecError> {
    let o = ptp_core::ObjectInfo::decode(&payload).map_err(codec_decode)?;
    Ok(PtpObjectInfo {
        storage_id: o.storage_id,
        object_format: o.object_format,
        protection_status: o.protection_status,
        object_compressed_size: o.object_compressed_size,
        thumb_format: o.thumb_format,
        thumb_compressed_size: o.thumb_compressed_size,
        thumb_pix_width: o.thumb_pix_width,
        thumb_pix_height: o.thumb_pix_height,
        image_pix_width: o.image_pix_width,
        image_pix_height: o.image_pix_height,
        image_bit_depth: o.image_bit_depth,
        parent_object: o.parent_object,
        association_type: o.association_type,
        association_desc: o.association_desc,
        sequence_number: o.sequence_number,
        filename: o.filename,
        capture_date: o.capture_date,
        modification_date: o.modification_date,
        keywords: o.keywords,
    })
}

/// One typed record from a manifest-declared record stream.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct RecordStreamRecord {
    pub code: u16,
    pub value: PtpValue,
}

/// A successfully decoded record stream. Member absence is represented by no
/// matching record, independently of malformed input and numeric zero.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct RecordStreamResult {
    pub records: Vec<RecordStreamRecord>,
}

/// Parse a record-stream payload at the manifest-declared widths — pass the
/// property's [`PayloadInfo`] from the catalog. Omitted widths take the schema
/// defaults (2/2/4), mirroring `camera_config::Payload::record_widths` (a seam
/// test guards the mirror). Widths the codec can't honor are a
/// [`CodecError::Decode`], never a silent misread (#161).
#[uniffi::export]
pub fn parse_record_stream(
    payload: Vec<u8>,
    info: PayloadInfo,
) -> Result<RecordStreamResult, CodecError> {
    let descriptor = record_stream_descriptor(&info).map_err(codec_decode)?;
    let records = protocol_primitives::quirk::parse_typed_record_stream(&payload, &descriptor)
        .map_err(codec_decode)?
        .into_iter()
        .map(|(code, value)| RecordStreamRecord {
            code,
            value: (&value).into(),
        })
        .collect();
    Ok(RecordStreamResult { records })
}

fn record_stream_descriptor(
    info: &PayloadInfo,
) -> Result<
    protocol_primitives::quirk::RecordStreamDescriptor,
    protocol_primitives::quirk::RecordStreamError,
> {
    use protocol_primitives::quirk::{RecordStreamDescriptor, RecordValueEncoding};

    RecordStreamDescriptor::new(
        info.count_width.unwrap_or(2),
        info.record
            .as_ref()
            .map(|record| record.code_width)
            .unwrap_or(2),
        info.members.iter().map(|member| {
            let encoding = match member.encoding {
                RecordValueEncodingInfo::Fixed { width } => RecordValueEncoding::Fixed { width },
                RecordValueEncodingInfo::PtpString => RecordValueEncoding::PtpString,
            };
            (member.code, encoding)
        }),
    )
}

/// Look up one typed member after a successful parse. `None` is a valid absent
/// member; malformed input is returned by [`parse_record_stream`] instead.
#[uniffi::export]
pub fn record_stream_value(stream: RecordStreamResult, member: u16) -> Option<PtpValue> {
    stream
        .records
        .into_iter()
        .find(|record| record.code == member)
        .map(|record| record.value)
}

/// Parse a `u32`-counted PTP object-handle array (e.g. the `0xD621` object-list
/// quirk or a standard `GetObjectHandles` response payload).
#[uniffi::export]
pub fn parse_object_handle_list(payload: Vec<u8>) -> Result<Vec<u32>, CodecError> {
    let mut r = ptp_core::Reader::new(&payload);
    r.ptp_array(|r| r.u32()).map_err(codec_decode)
}

// ----------------------------------------------------------------------------
// Errors / enums / records
// ----------------------------------------------------------------------------

#[derive(Debug, thiserror::Error, uniffi::Error)]
pub enum ConfigError {
    #[error("manifest parse error: {0}")]
    Parse(String),
    #[error("unsupported schema: {0}")]
    Schema(String),
    #[error("invalid manifest consumer contract: {0}")]
    Contract(String),
}

#[derive(Debug, thiserror::Error, uniffi::Error)]
pub enum EstablishmentError {
    #[error("invalid plan handle: {0}")]
    InvalidPlanHandle(String),
    #[error("unknown establishment plan: {0}")]
    UnknownPlan(String),
    #[error("invalid next step index: {0}")]
    InvalidNextStepIndex(String),
}

#[derive(Debug, thiserror::Error, uniffi::Error)]
pub enum TransportCloseError {
    #[error("unknown transport-close sentinel: {0}")]
    UnknownSentinel(String),
    #[error("invalid bytes for transport-close sentinel: {0}")]
    InvalidSentinelBytes(String),
}

/// The calling platform — used to hide connections it can't host (USB/tether on iOS).
#[derive(uniffi::Enum)]
pub enum Platform {
    Ios,
    Macos,
    Android,
    Linux,
}

impl Platform {
    fn as_str(&self) -> &'static str {
        match self {
            Platform::Ios => "ios",
            Platform::Macos => "macos",
            Platform::Android => "android",
            Platform::Linux => "linux",
        }
    }
}

/// Availability of an operation across the orthogonal `(connection, mode)` axes
/// plus its runtime prerequisite.
#[derive(Debug, uniffi::Enum)]
pub enum Availability {
    Available,
    WrongMode,
    WrongConnection,
    Blocked,
    Unavailable,
}

impl From<cc::Availability> for Availability {
    fn from(a: cc::Availability) -> Self {
        match a {
            cc::Availability::Available => Availability::Available,
            cc::Availability::WrongMode => Availability::WrongMode,
            cc::Availability::WrongConnection => Availability::WrongConnection,
            cc::Availability::Blocked => Availability::Blocked,
            cc::Availability::Unavailable => Availability::Unavailable,
        }
    }
}

#[derive(uniffi::Record)]
pub struct ConnectionInfo {
    pub id: String,
    pub kind: String,
    pub discovery: String,
    pub auto_discoverable: bool,
    // --- #81 per-connection traits: the app selects behavior from these
    // instead of branching on `id`. `None` → the app falls back. ---
    /// Closing the active command transport may remove its listener, so a
    /// consumer must not assume it can immediately redial the same endpoint as
    /// generic recovery. An outer connection re-establishment may create a new
    /// listener. False means this restriction is not declared.
    pub command_listener_volatile: bool,
    pub init_shape: Option<String>,
    pub live_view_delivery: Option<FfiLiveViewDelivery>,
    pub shutter_recipe: Option<ShutterRecipe>,
    /// The wire framing to pass the codecs for this connection's command channel
    /// (#133) — declared in the manifest so the app never maps `kind` to a framing
    /// itself. `None` → not modeled for this connection yet.
    pub command_framing: Option<PtpFraming>,
    /// The wire framing for this connection's event socket, when it differs from
    /// the command channel (the Fuji `app` event socket is USB/PIMA type-4).
    pub event_framing: Option<PtpFraming>,
}

impl From<cc::WireFraming> for PtpFraming {
    fn from(f: cc::WireFraming) -> Self {
        match f {
            cc::WireFraming::Standard => PtpFraming::Standard,
            cc::WireFraming::Compressed => PtpFraming::Compressed,
            cc::WireFraming::Usb => PtpFraming::Usb,
        }
    }
}

/// Mirror of `cc::LiveViewDelivery` (#81): how live-view frames arrive over a
/// connection — a continuous `stream` or a `poll` loop issuing `poll_op`.
#[derive(uniffi::Record)]
pub struct FfiLiveViewDelivery {
    pub kind: LiveViewDeliveryKind,
    pub poll_op: Option<u16>,
}

#[derive(uniffi::Enum)]
pub enum LiveViewDeliveryKind {
    Stream,
    Poll,
}

/// Mirror of `cc::ShutterRecipe` (#81): the shutter recipe family, replacing the
/// app's per-connection shutter fork.
#[derive(uniffi::Enum)]
pub enum ShutterRecipe {
    AppPostview,
    WirelessTether3Beat,
}

impl From<&cc::LiveViewDelivery> for FfiLiveViewDelivery {
    fn from(d: &cc::LiveViewDelivery) -> Self {
        FfiLiveViewDelivery {
            kind: match d.kind {
                cc::LiveViewDeliveryKind::Stream => LiveViewDeliveryKind::Stream,
                cc::LiveViewDeliveryKind::Poll => LiveViewDeliveryKind::Poll,
            },
            poll_op: d.poll_op.as_deref().and_then(parse_hex_code),
        }
    }
}

impl From<cc::ShutterRecipe> for ShutterRecipe {
    fn from(r: cc::ShutterRecipe) -> Self {
        match r {
            cc::ShutterRecipe::AppPostview => ShutterRecipe::AppPostview,
            cc::ShutterRecipe::WirelessTether3Beat => ShutterRecipe::WirelessTether3Beat,
        }
    }
}

/// The InitCommandRequest for a connection, assembled from manifest identity
/// and the declared fixed field width. #82.
#[derive(uniffi::Record)]
pub struct InitShapeInfo {
    pub guid: Vec<u8>,
    pub friendly_name: String,
    pub client_ipv4: Option<String>,
    pub name_field_byte_count: u32,
    pub expected_responder_guid: Vec<u8>,
    pub packet: Vec<u8>,
}

/// Resolved initiator identity shared by every PTP/IP init shape.
#[derive(Debug, Clone, uniffi::Record)]
pub struct ResolvedInitIdentityInfo {
    pub guid: Vec<u8>,
    pub friendly_name: String,
}

/// Manifest-selected InitFail retry policy. `max_retries` is the number of
/// retries after the initial request.
#[derive(Debug, Clone, uniffi::Record)]
pub struct InitRetryPolicyInfo {
    pub max_retries: u32,
    pub backoff_ms: u32,
    pub when_reasons: Vec<u32>,
}

#[derive(uniffi::Record)]
pub struct CameraIdentityInfo {
    pub manufacturer: String,
    pub model: String,
    pub firmware: String,
    pub identities: Vec<KeyValue>,
}

/// The camera's AF grid for tap-to-focus (#135). The app reads these dims from
/// data and feeds them to [`pack_af_area`] — it never hardcodes the grid.
#[derive(uniffi::Record)]
pub struct FocusGridInfo {
    pub columns: u32,
    pub rows: u32,
}

#[derive(uniffi::Record)]
pub struct ModeInfo {
    pub path: String,
    pub capabilities: Vec<String>,
}

/// An observed property value the app read off the wire (sans-io: the engine never
/// reads it itself).
#[derive(uniffi::Record)]
pub struct PropObservation {
    pub code: u16,
    pub value: i64,
}

#[derive(Debug, uniffi::Record)]
pub struct ControlInfo {
    pub set_method: Option<String>,
    pub operation: Option<u16>,
    pub readback: Option<u16>,
}

#[derive(Debug, Clone, Copy, uniffi::Enum)]
pub enum ControlRole {
    Iso,
    ShutterSpeed,
    Aperture,
    ExposureBias,
    FocusArea,
}

#[derive(Debug, Clone, Copy, uniffi::Enum)]
pub enum ControlEvidenceBasis {
    DescriptorOnly,
    WriteProbe,
}

#[derive(Debug, Clone, Copy, uniffi::Enum)]
pub enum ControlObservedEffect {
    Confirmed,
    AckNoEffect,
    ProtocolRefused,
    DestructiveClamp,
    Unknown,
}

#[derive(Debug, Clone, Copy, uniffi::Enum)]
pub enum ControlOwner {
    Client,
    Camera,
    Body,
    ModeGated,
    Unknown,
}

#[derive(Debug, Clone, Copy, uniffi::Enum)]
pub enum ControlReadSource {
    DirectProperty,
    DeclaredReadback,
}

#[derive(Debug, uniffi::Record)]
pub struct ControlSurfaceInfo {
    pub role: ControlRole,
    pub property: u16,
    pub read_source: ControlReadSource,
    pub evidence_basis: ControlEvidenceBasis,
    pub observed_effect: ControlObservedEffect,
    pub effective_owner: Option<ControlOwner>,
    pub control: ControlInfo,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum ObjectTransferStrategy {
    Chunked,
    WholeObject,
}

#[derive(Debug, Clone, Copy, uniffi::Enum)]
pub enum ObjectTransferResumePolicy {
    ByteOffset,
    RestartFromZero,
}

#[derive(Debug, Clone, Copy, uniffi::Enum)]
pub enum ObjectTransferFormatSupport {
    Confirmed,
    Experimental,
    Unsupported,
}

#[derive(Debug, Clone, Copy, uniffi::Enum)]
pub enum ObjectTransferCompletionTiming {
    LocalCommit,
}

#[derive(Debug, uniffi::Record)]
pub struct ObjectTransferFormatInfo {
    pub code: u16,
    pub support: ObjectTransferFormatSupport,
}

#[derive(Debug, uniffi::Record)]
pub struct ObjectTransferContractInfo {
    pub strategy: ObjectTransferStrategy,
    pub resume_policy: ObjectTransferResumePolicy,
    pub read_action: ActionVerb,
    pub completion_action: Option<ActionVerb>,
    pub completion_after: Option<ObjectTransferCompletionTiming>,
    pub formats: Vec<ObjectTransferFormatInfo>,
}

/// Mirror of `cc::Payload` — how a composite byte-array property (`0xD212`
/// live-status) decomposes into a record stream the app walks. Full mirror:
/// dropping `members` would silently lose the poll allowlist the consumer needs.
#[derive(uniffi::Record)]
pub struct PayloadInfo {
    pub form: PayloadForm,
    pub count_width: Option<u8>,
    pub record: Option<RecordLayoutInfo>,
    pub members: Vec<RecordMemberInfo>,
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct RecordMemberInfo {
    pub code: u16,
    pub encoding: RecordValueEncodingInfo,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum RecordValueEncodingInfo {
    Fixed { width: u8 },
    PtpString,
}

/// One exact positive observation tuple. The surrounding store fixes camera
/// model and firmware; these fields retain connection, mode, and state as one
/// atomic scope rather than independent sets.
#[derive(Debug, Clone, uniffi::Record)]
pub struct ObservedScopeInfo {
    pub connection: String,
    pub mode: String,
    pub state: String,
}

impl From<&cc::ObservedScope> for ObservedScopeInfo {
    fn from(scope: &cc::ObservedScope) -> Self {
        Self {
            connection: scope.connection.clone(),
            mode: scope.mode.clone(),
            state: scope.state.clone(),
        }
    }
}

/// Whether an operation row is safe to execute or is only a positive catalog
/// advertisement observed from the camera.
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum OperationKind {
    Executable,
    AdvertisedOnly,
}

impl From<cc::OperationKind> for OperationKind {
    fn from(kind: cc::OperationKind) -> Self {
        match kind {
            cc::OperationKind::Executable => Self::Executable,
            cc::OperationKind::AdvertisedOnly => Self::AdvertisedOnly,
        }
    }
}

/// One row of the operation catalog. `name` is either a reviewed semantic name
/// or the stable `raw_0x...` fallback retained by the generator.
#[derive(Debug, uniffi::Record)]
pub struct OperationInfo {
    pub code: u16,
    pub name: String,
    pub kind: OperationKind,
    pub observed_scopes: Vec<ObservedScopeInfo>,
    pub evidence: Vec<String>,
    pub canonical_name_provenance: Vec<ObservationAssertionProvenance>,
}

/// One row of the property catalog (#50): code, name, wire type, access, the
/// allowed value set, and value→label pairs. Lets the app present settings
/// without hardcoding a per-vendor catalog.
#[derive(uniffi::Record)]
pub struct PropertyInfo {
    pub code: u16,
    pub name: String,
    pub ptp_name: Option<String>,
    pub ptype: Option<String>,
    pub access: Option<String>,
    pub initial_value: Option<i64>,
    pub kind: PropertyKind,
    pub observed_scopes: Vec<ObservedScopeInfo>,
    pub descriptor_form: Option<String>,
    pub descriptor_source: Option<DescriptorSource>,
    pub evidence: Vec<String>,
    pub values: Vec<i64>,
    pub labels: Vec<KeyValue>,
    pub value_rows: Vec<PropertyValueInfo>,
    pub value_profiles: Vec<PropertyValueProfileInfo>,
    pub value_encoding: Option<PropertyValueEncodingInfo>,
    pub structured_text: Option<StructuredTextLayoutInfo>,
    pub canonical_name_provenance: Vec<ObservationAssertionProvenance>,
    pub source_native_name_provenance: Vec<ObservationAssertionProvenance>,
    pub semantic_value_rows: Vec<SemanticPropertyValueInfo>,
    pub semantic_value_profiles: Vec<SemanticPropertyValueProfileInfo>,
}

/// A reviewed typed value-label assertion from the durable semantic ledger.
#[derive(Debug, Clone, uniffi::Record)]
pub struct SemanticPropertyValueInfo {
    pub value: ObservationTypedPropertyValue,
    pub label: String,
    pub provenance: Vec<ObservationAssertionProvenance>,
}

/// A reviewed scoped profile and its assertion-level provenance.
#[derive(Debug, Clone, uniffi::Record)]
pub struct SemanticPropertyValueProfileInfo {
    pub profile: PropertyValueProfileInfo,
    pub provenance: Vec<ObservationAssertionProvenance>,
}

/// Whether a manifest property is a user-facing setting or protocol machinery.
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum PropertyKind {
    Setting,
    Scaffold,
    CatalogOnly,
}

impl From<cc::PropertyKind> for PropertyKind {
    fn from(kind: cc::PropertyKind) -> Self {
        match kind {
            cc::PropertyKind::Setting => Self::Setting,
            cc::PropertyKind::Scaffold => Self::Scaffold,
            cc::PropertyKind::CatalogOnly => Self::CatalogOnly,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum DescriptorSource {
    Camera,
    Manifest,
}

impl From<cc::ValueSource> for DescriptorSource {
    fn from(source: cc::ValueSource) -> Self {
        match source {
            cc::ValueSource::Camera => Self::Camera,
            cc::ValueSource::Manifest => Self::Manifest,
        }
    }
}

/// A manifest-backed property choice: label for UI, raw value for the camera.
#[derive(Debug, Clone, uniffi::Record)]
pub struct PropertyValueInfo {
    pub label: String,
    pub raw: i64,
    pub evidence: Vec<String>,
}

/// A scoped property-value capability profile. These rows may represent an
/// camera/body capability path or empirical write policy, not necessarily a
/// standard PTP `DevicePropDesc`.
#[derive(Debug, Clone, uniffi::Record)]
pub struct PropertyValueProfileInfo {
    pub connection: Option<String>,
    pub mode: Option<String>,
    pub rows: Vec<PropertyValueProfileRowInfo>,
    pub evidence: Vec<String>,
}

#[derive(Debug, Clone, uniffi::Record)]
pub struct PropertyValueProfileRowInfo {
    pub label: String,
    pub raw: i64,
    pub legal: bool,
    pub aliases: Vec<i64>,
    pub write_store_raw: Option<i64>,
}

/// Generic property value encoding metadata. This is intentionally shape-based
/// rather than camera-branded, so consumers can present grouped/sentinel values
/// without carrying vendor formulas.
#[derive(Debug, Clone, uniffi::Record)]
pub struct PropertyValueEncodingInfo {
    pub sentinel: Option<SentinelMaskInfo>,
    pub masks: Vec<SentinelMaskInfo>,
}

#[derive(Debug, Clone, uniffi::Record)]
pub struct SentinelMaskInfo {
    pub mask: i64,
    pub equals: i64,
    pub meaning: Option<String>,
    pub label_prefix: String,
}

/// Consumer-neutral grammar for a structured PTP string property.
#[derive(Debug, Clone, uniffi::Record)]
pub struct StructuredTextLayoutInfo {
    pub delimiter: String,
    pub fields: Vec<StructuredTextFieldInfo>,
}

#[derive(Debug, Clone, uniffi::Record)]
pub struct StructuredTextFieldInfo {
    pub name: String,
    pub scalar: StructuredTextScalar,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum StructuredTextScalar {
    SignedInteger,
}

/// An object-format classification from the manifest media table (#36): the
/// PTP/vendor format code's name, vendor, and RAW/movie flags — so the app
/// holds no per-vendor format literals.
#[derive(uniffi::Record)]
pub struct MediaFormatInfo {
    pub code: u16,
    pub name: String,
    pub vendor: Option<String>,
    pub is_raw: bool,
    pub is_movie: bool,
    /// Whether the app may hand this format to the OS photo library (#136).
    pub is_photos_compatible: bool,
    /// Where this RAW format's embedded full-size JPEG lives (#101), so the app
    /// can pull it with GetPartialObject. `None` for non-RAW / no embedded JPEG.
    pub embedded_jpeg: Option<EmbeddedJpegInfo>,
}

/// Embedded-JPEG locator surfaced to the app (#101): verify `magic` at offset 0,
/// then read a u32 JPEG start offset at `offset_at` and a u32 length at
/// `length_at`, in the byte order `big_endian` selects. ptpsim only *describes*
/// the layout; the app does the extraction from the bytes ptpsim serves.
#[derive(uniffi::Record)]
pub struct EmbeddedJpegInfo {
    pub magic: String,
    pub offset_at: u16,
    pub length_at: u16,
    pub big_endian: bool,
}

#[derive(uniffi::Enum)]
pub enum PayloadForm {
    RecordStream,
}

#[derive(uniffi::Record)]
pub struct RecordLayoutInfo {
    pub code_width: u8,
    pub value_width: u8,
}

impl From<&cc::Payload> for PayloadInfo {
    fn from(p: &cc::Payload) -> Self {
        let (_, _, default_value_width) = p.record_widths();
        PayloadInfo {
            form: match p.form {
                cc::PayloadForm::RecordStream => PayloadForm::RecordStream,
            },
            count_width: p.count_width,
            record: p.record.as_ref().map(|r| RecordLayoutInfo {
                code_width: r.code_width,
                value_width: r.value_width,
            }),
            members: p
                .members
                .iter()
                .filter_map(|member| {
                    Some(RecordMemberInfo {
                        code: parse_hex_code(member.code())?,
                        encoding: match member.encoding(default_value_width) {
                            cc::RecordValueEncoding::Fixed { width } => {
                                RecordValueEncodingInfo::Fixed { width }
                            }
                            cc::RecordValueEncoding::PtpString => {
                                RecordValueEncodingInfo::PtpString
                            }
                        },
                    })
                })
                .collect(),
        }
    }
}

impl From<&cc::PropertyValueRow> for PropertyValueInfo {
    fn from(row: &cc::PropertyValueRow) -> Self {
        PropertyValueInfo {
            label: row.label.clone(),
            raw: row.raw,
            evidence: row.evidence.clone(),
        }
    }
}

impl From<&cc::PropertyValueProfile> for PropertyValueProfileInfo {
    fn from(profile: &cc::PropertyValueProfile) -> Self {
        PropertyValueProfileInfo {
            connection: profile.connection.clone(),
            mode: profile.mode.clone(),
            rows: profile
                .rows
                .iter()
                .map(PropertyValueProfileRowInfo::from)
                .collect(),
            evidence: profile.evidence.clone(),
        }
    }
}

impl From<&cc::ProvenancedPropertyValueRow> for SemanticPropertyValueInfo {
    fn from(row: &cc::ProvenancedPropertyValueRow) -> Self {
        Self {
            value: (&row.value).into(),
            label: row.label.clone(),
            provenance: row.provenance.iter().map(Into::into).collect(),
        }
    }
}

impl From<&cc::ProvenancedPropertyValueProfile> for SemanticPropertyValueProfileInfo {
    fn from(profile: &cc::ProvenancedPropertyValueProfile) -> Self {
        Self {
            profile: (&profile.profile).into(),
            provenance: profile.provenance.iter().map(Into::into).collect(),
        }
    }
}

impl From<&cc::PropertyValueProfileRow> for PropertyValueProfileRowInfo {
    fn from(row: &cc::PropertyValueProfileRow) -> Self {
        PropertyValueProfileRowInfo {
            label: row.label.clone(),
            raw: row.raw,
            legal: row.legal,
            aliases: row.aliases.clone(),
            write_store_raw: row.write_store_raw,
        }
    }
}

impl From<&cc::PropertyValueEncoding> for PropertyValueEncodingInfo {
    fn from(enc: &cc::PropertyValueEncoding) -> Self {
        PropertyValueEncodingInfo {
            sentinel: enc.sentinel.as_ref().map(SentinelMaskInfo::from),
            masks: enc.masks.iter().map(SentinelMaskInfo::from).collect(),
        }
    }
}

impl From<&cc::StructuredTextLayout> for StructuredTextLayoutInfo {
    fn from(layout: &cc::StructuredTextLayout) -> Self {
        StructuredTextLayoutInfo {
            delimiter: layout.delimiter.clone(),
            fields: layout
                .fields
                .iter()
                .map(StructuredTextFieldInfo::from)
                .collect(),
        }
    }
}

impl From<&cc::StructuredTextField> for StructuredTextFieldInfo {
    fn from(field: &cc::StructuredTextField) -> Self {
        StructuredTextFieldInfo {
            name: field.name.clone(),
            scalar: match field.scalar {
                cc::StructuredTextScalar::SignedInteger => StructuredTextScalar::SignedInteger,
            },
        }
    }
}

impl From<&cc::SentinelMask> for SentinelMaskInfo {
    fn from(s: &cc::SentinelMask) -> Self {
        SentinelMaskInfo {
            mask: s.mask,
            equals: s.equals.unwrap_or(s.mask),
            meaning: s.meaning.clone(),
            label_prefix: s.label_prefix.clone(),
        }
    }
}

/// A `send_op` parameter: a literal, or a named runtime slot the app binds from its
/// own session state (e.g. the live-view open-capture txid). Declarative — not a
/// computed variable.
#[derive(Debug, uniffi::Enum)]
pub enum EntryParam {
    Literal {
        value: u32,
    },
    Runtime {
        slot: String,
        shift: u32,
        mask: Option<u64>,
    },
}

#[derive(Debug, Clone, uniffi::Record)]
pub struct CaptureInfo {
    pub bind: String,
    pub source: CaptureSourceInfo,
}

#[derive(Debug, Clone, uniffi::Enum)]
pub enum CaptureSourceInfo {
    ObjectInfoCompressedSize,
    PropValue,
    U32Le,
    U64Le,
    PtpU32Array,
    TransactionId,
}

/// FFI mirror of [`camera_config::RetryFailureClass`] (§11.21): a whole class
/// of step failure a `retry` may select. `Decode` = OK response, undecodable
/// data payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum FfiRetryFailureClass {
    Decode,
}

/// The PTP condition vocabulary (`cc::Predicate`) mirrored for the app: a
/// closed tree of property-value comparisons used by `awaitUntil`'s `until`.
/// Distinct from the BLE-recognition `Predicate` (a string-scope compare in
/// `mfg_index`). A FULL mirror is required — a partial one would silently drop
/// `all`/`any`/`not` conditions (the exact hand-mirror hazard this surface
/// guards against). Evaluate it with [`await_until_satisfied`] so Swift never
/// re-implements masking/connective logic.
#[derive(Debug, Clone, uniffi::Enum)]
pub enum FfiPredicate {
    /// All children hold (conjunction).
    All { all: Vec<FfiPredicate> },
    /// Any child holds (disjunction).
    Any { any: Vec<FfiPredicate> },
    /// Negation. One-element by construction — `not(all(children))` preserves
    /// `cc::Predicate::Not`'s single-operand semantics through uniffi's
    /// `Vec`-based recursion (uniffi has no bare boxed-enum field).
    Not { not: Vec<FfiPredicate> },
    /// Compare one (optionally masked) property value.
    Leaf {
        prop: u16,
        mask: Option<i64>,
        eq: Option<i64>,
        ne: Option<i64>,
        lt: Option<i64>,
        gt: Option<i64>,
    },
}

impl TryFrom<&cc::Predicate> for FfiPredicate {
    type Error = ConfigError;

    fn try_from(p: &cc::Predicate) -> Result<Self, Self::Error> {
        Ok(match p {
            cc::Predicate::All { all } => FfiPredicate::All {
                all: all
                    .iter()
                    .map(FfiPredicate::try_from)
                    .collect::<Result<_, _>>()?,
            },
            cc::Predicate::Any { any } => FfiPredicate::Any {
                any: any
                    .iter()
                    .map(FfiPredicate::try_from)
                    .collect::<Result<_, _>>()?,
            },
            cc::Predicate::Not { not } => FfiPredicate::Not {
                not: vec![FfiPredicate::try_from(not.as_ref())?],
            },
            cc::Predicate::Leaf(l) => FfiPredicate::Leaf {
                prop: parse_hex_code(&l.prop).ok_or_else(|| {
                    ConfigError::Contract(format!(
                        "predicate leaf prop `{}` is not a hex property code",
                        l.prop
                    ))
                })?,
                mask: l.mask,
                eq: l.eq,
                ne: l.ne,
                lt: l.lt,
                gt: l.gt,
            },
        })
    }
}

impl From<&FfiPredicate> for cc::Predicate {
    fn from(p: &FfiPredicate) -> Self {
        match p {
            FfiPredicate::All { all } => cc::Predicate::All {
                all: all.iter().map(cc::Predicate::from).collect(),
            },
            FfiPredicate::Any { any } => cc::Predicate::Any {
                any: any.iter().map(cc::Predicate::from).collect(),
            },
            FfiPredicate::Not { not } => cc::Predicate::Not {
                not: Box::new(cc::Predicate::All {
                    all: not.iter().map(cc::Predicate::from).collect(),
                }),
            },
            FfiPredicate::Leaf {
                prop,
                mask,
                eq,
                ne,
                lt,
                gt,
            } => cc::Predicate::Leaf(cc::Leaf {
                prop: format!("0x{prop:04x}"),
                mask: *mask,
                eq: *eq,
                ne: *ne,
                lt: *lt,
                gt: *gt,
            }),
        }
    }
}

/// Evaluate an `awaitUntil` `until` predicate against observed property values
/// — the dispatcher calls this each poll instead of re-implementing the PTP
/// predicate logic in Swift. Reuses the canonical `cc::Predicate::eval`.
#[uniffi::export]
pub fn await_until_satisfied(until: FfiPredicate, observed: Vec<PropObservation>) -> bool {
    let pred: cc::Predicate = (&until).into();
    pred.eval(&prop_view(&observed))
}

/// One wire action in a mode-entry sequence (closed vocabulary, no branches).
/// `tolerant` = a non-OK PTP response is acceptable (log + continue; transport
/// failure still aborts). `params` carry `send_op` arguments.
#[derive(Debug, uniffi::Enum)]
pub enum EntryStep {
    SetProp {
        prop: u16,
        value: i64,
        tolerant: bool,
    },
    SetPropRuntime {
        prop: u16,
        slot: String,
        if_missing: FfiMissingRuntimeValue,
        tolerant: bool,
    },
    GetProp {
        prop: u16,
        captures: Vec<CaptureInfo>,
        tolerant: bool,
    },
    ReadEcho {
        prop: u16,
        captures: Vec<CaptureInfo>,
        tolerant: bool,
    },
    SendOp {
        op: u16,
        params: Vec<EntryParam>,
        captures: Vec<CaptureInfo>,
        repeat: u32,
        tolerant: bool,
    },
    /// Open a manifest-selected auxiliary PTP/IP channel at this point in the
    /// mode-entry/action sequence. The host owns the socket; Rust owns when the
    /// callback runs.
    OpenChannel { role: SocketRole, tolerant: bool },
    /// Re-establish the PTP/IP session in-place — close the current TCP
    /// socket, send the connection's manifest-declared transport-close frame,
    /// open a new socket to the connection's command port, replay the cached
    /// InitCommandRequest, and OpenSession again. Reuses the connection's cached
    /// identity, so the verb carries no parameters. Wire-confirmed for reference app
    /// Get→Take. Take→Get uses an outer connection re-establishment (#244).
    ReopenSession { tolerant: bool },
    /// End the PTP/IP session. `transport_close` means use the connection's
    /// manifest-declared orderly transport-close frame instead of a bare TCP
    /// close. It does not promise immediate endpoint redialability.
    CloseSession {
        transport_close: bool,
        tolerant: bool,
    },
    /// Observe until `until` holds, running `on_each` each unsatisfied iteration —
    /// the PTP-IP await/poll-until verb (#29 postview, #42 AF), mirroring the BLE
    /// `bleAwaitUntil` contract (§11.16). [`source`](Self::AwaitUntil::source) is
    /// either a property `Poll` (loop) or an `Event` push (single-shot
    /// push-then-read, #54). The dispatcher owns the loop + `timeout_ms`/
    /// `interval_ms`; evaluate `until` with [`await_until_satisfied`].
    AwaitUntil {
        source: FfiAwaitSource,
        until: FfiPredicate,
        on_each: Vec<EntryStep>,
        captures: Vec<CaptureInfo>,
        timeout_ms: u32,
        interval_ms: u32,
        tolerant: bool,
    },
    /// Replay a complete logical PTP sequence only after an exact
    /// manifest-declared non-OK response code or failure class (§11.21).
    Retry {
        steps: Vec<EntryStep>,
        when_response_codes: Vec<u16>,
        when_failure_classes: Vec<FfiRetryFailureClass>,
        max_attempts: u32,
        retry_delay_ms: u32,
        tolerant: bool,
    },
    /// A closed declarative loop (#46): `ForEach` over a captured collection (each
    /// element binds a runtime slot for `body`), or `Chunk` by fixed size (the
    /// dispatcher owns the offset/length cursor; `total` names the scope slot a
    /// preceding GetObjectInfo captured). The nested `body` crosses the seam.
    Loop { kind: FfiLoopKind, tolerant: bool },
    If {
        slot: String,
        equals: u64,
        then_steps: Vec<EntryStep>,
        tolerant: bool,
    },
    /// Conditional with an explicit false branch. Appended rather than changing
    /// `If`'s payload so previously generated UniFFI bindings retain their wire
    /// layout and discriminants.
    IfElse {
        slot: String,
        equals: u64,
        then_steps: Vec<EntryStep>,
        else_steps: Vec<EntryStep>,
        tolerant: bool,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum FfiMissingRuntimeValue {
    Error,
    Skip,
}

/// The two `Loop` shapes (#46). Mirrors `cc::Loop`. `ForEach` iterates a named
/// collection captured by an earlier step, binding each element to `bind`;
/// `Chunk` walks `total` bytes in `size`-byte windows, binding
/// `offset_bind`/`length_bind` each iteration. The dispatcher owns all cursor
/// advancement — no author arithmetic or implicit property reads.
#[derive(Debug, uniffi::Enum)]
pub enum FfiLoopKind {
    ForEach {
        collection: String,
        bind: String,
        body: Vec<EntryStep>,
    },
    Chunk {
        total: String,
        size: FfiChunkSize,
        offset_bind: String,
        length_bind: String,
        body: Vec<EntryStep>,
    },
}

#[derive(Debug, Clone, uniffi::Enum)]
pub enum FfiChunkSize {
    Literal { value: u32 },
    Runtime { slot: String },
}

/// Where a PTP-IP `awaitUntil` observes (#54). Mirrors `cc::AwaitSource`. `Poll`
/// is the #49 default (poll a property each iteration); `Event` awaits a
/// completion push on the event socket then re-polls `then_poll` (#185). On the
/// `Event` path the dispatcher: opens/reads the connection's event socket
/// (55741), awaits an event packet with `code`, then re-issues
/// `GetDevicePropValue(then_poll)` at `interval_ms` cadence until `until` holds
/// or `timeout_ms` elapses — the event acknowledges the operation, it does not
/// guarantee the polled value has settled (client application#157). `then_poll: None`
/// evaluates `until` once on event arrival.
#[derive(Debug, Clone, uniffi::Enum)]
pub enum FfiAwaitSource {
    Poll { prop: u16 },
    Event { code: u16, then_poll: Option<u16> },
}

#[derive(uniffi::Record)]
pub struct ModeEntryPlan {
    pub to: String,
    pub from: Option<String>,
    pub requires: Option<FfiPredicate>,
    pub execution: ModeEntryExecution,
    pub activities: Vec<ConnectionActivityDescriptor>,
}

#[derive(Debug, uniffi::Enum)]
pub enum ModeEntryExecution {
    Ptp {
        steps: Vec<EntryStep>,
    },
    ReestablishConnection {
        connection: String,
        exit_steps: Vec<EntryStep>,
        establishment_params: Vec<KeyValue>,
    },
    UserInstruction {
        instruction: String,
    },
}

// ----------------------------------------------------------------------------
// Action surface — named, parameterized recipes that run within a mode.
// Mirrors camera-config's `Connection.actions` block (docs/plans/action-verbs.md).
// ActionEffect ships as a uniffi tagged enum here (vs. the flat struct in
// camera-config) so consumer Swift / Kotlin gets clean exhaustive-switch
// ergonomics:
//
//     switch shutter.triggers[0] {
//     case .objectsAvailable(let min, let max): // poll the object queue
//     case .postviewEvent:                  // wait via 0xD212 polling
//     case .liveViewStream:                 // continuous frame delivery
//     }
// ----------------------------------------------------------------------------

/// Closed verb vocabulary for named in-mode actions. Mirrors
/// `cc::ActionVerb`; new verbs require an FFI-side variant alongside the
/// camera-config-side addition (same fail-fast as Step verbs).
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum ActionVerb {
    Shutter,
    EnumerateObjects,
    GetObjectInfo,
    GetThumb,
    GetObject,
    DeleteObject,
    AutofocusLock,
    AutofocusRelease,
    ImportObjects,
    ReadDeviceInfo,
    StartLiveView,
    PollLiveView,
    StopLiveView,
    Keepalive,
}

/// Parse the schema's exact camelCase action vocabulary.
#[uniffi::export]
pub fn parse_action_verb(value: String) -> Option<ActionVerb> {
    value.parse::<cc::ActionVerb>().ok().map(cc_to_ffi_verb)
}

/// Declared post-conditions an action produces — the consumer plans UX
/// around them without per-transport knowledge. Engine does NOT act on
/// triggers; pure declaration.
#[derive(Debug, Clone, Copy, uniffi::Enum)]
pub enum ActionEffect {
    /// Camera makes between `min` and `max` captured objects available after
    /// `Shutter`. PCSS shutter: `min=1, max=3` depending on the user's
    /// JPEG/HEIF/RAW selection.
    ObjectsAvailable { min: u32, max: u32 },
    /// Camera emits a post-shutter state change the consumer polls for
    /// (reference app `app` path: `0xD212` clears the JPEG-saved flag, then `0x9022`).
    PostviewEvent,
    /// Continuous frame delivery starts (e.g. live-view through-stream
    /// after `0x101C InitiateOpenCapture`).
    LiveViewStream,
}

/// One shared action identity with explicit initiator and responder bindings.
#[derive(Debug, uniffi::Record)]
pub struct Action {
    pub mode: String,
    pub initiator: Option<ActionInitiator>,
    pub responder: Option<ActionResponder>,
    pub triggers: Vec<ActionEffect>,
    pub evidence: Vec<String>,
}

#[derive(Debug, uniffi::Record)]
pub struct ActionInitiator {
    pub params: Vec<ActionInitiatorParameter>,
    pub steps: Vec<EntryStep>,
    pub activities: Vec<ConnectionActivityDescriptor>,
}

#[derive(Debug, Clone, uniffi::Record)]
pub struct ActionInitiatorParameter {
    pub name: String,
    pub kind: ActionCatalogParameterKind,
    pub required: bool,
}

impl PartialEq<String> for ActionInitiatorParameter {
    fn eq(&self, other: &String) -> bool {
        self.name == *other
    }
}

impl PartialEq<&str> for ActionInitiatorParameter {
    fn eq(&self, other: &&str) -> bool {
        self.name == *other
    }
}

#[derive(Debug, Clone, uniffi::Record)]
pub struct ActionResponder {
    pub params: Vec<ActionParameter>,
    pub mutation: ResponderMutation,
}

#[derive(Debug, Clone, uniffi::Record)]
pub struct ActionParameter {
    pub name: String,
    pub kind: ActionParameterKind,
    pub default: Option<u32>,
    pub min: Option<u32>,
    pub max: Option<u32>,
}

#[derive(Debug, Clone, Copy, uniffi::Enum)]
pub enum ActionParameterKind {
    U32,
}

#[derive(Debug, Clone, uniffi::Enum)]
pub enum ResponderMutation {
    EnqueueObjects {
        count_param: String,
    },
    PropertyTransition {
        target: u16,
        initial: Option<i64>,
        terminal: PropertyTransitionTerminal,
        settle_after_polls: u32,
    },
}

#[derive(Debug, Clone, uniffi::Enum)]
pub enum PropertyTransitionTerminal {
    Fixed { value: i64 },
    Parameter { parameter: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum ActionRole {
    Initiator,
    Responder,
}

#[derive(Debug, Clone, Copy, uniffi::Enum)]
pub enum ActionCatalogParameterKind {
    U32,
    U64,
    String,
}

#[derive(Debug, Clone, uniffi::Record)]
pub struct ActionCatalogParameter {
    pub name: String,
    pub kind: ActionCatalogParameterKind,
    pub required: bool,
    pub default: Option<u64>,
    pub min: Option<u64>,
    pub max: Option<u64>,
}

#[derive(Debug, Clone, uniffi::Record)]
pub struct ActionRoleParameters {
    pub role: ActionRole,
    pub parameters: Vec<ActionCatalogParameter>,
}

#[derive(Debug, Clone, uniffi::Record)]
pub struct ActionCatalogEntry {
    pub action_id: String,
    pub connection: String,
    pub mode: String,
    pub supported_roles: Vec<ActionRole>,
    pub parameters: Vec<ActionRoleParameters>,
    pub triggers: Vec<ActionEffect>,
    pub available: bool,
}

#[derive(Debug, Clone, uniffi::Record)]
pub struct ActionCatalog {
    pub revision: String,
    pub actions: Vec<ActionCatalogEntry>,
}

#[derive(Debug, Clone, uniffi::Record)]
pub struct ActionArgument {
    pub name: String,
    pub value: ActionValue,
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Enum)]
pub enum ActionValue {
    U64 { value: u64 },
    String { value: String },
}

impl ActionValue {
    pub const fn as_u64(&self) -> Option<u64> {
        match self {
            Self::U64 { value } => Some(*value),
            Self::String { .. } => None,
        }
    }
}

impl PartialEq<u64> for ActionValue {
    fn eq(&self, other: &u64) -> bool {
        self.as_u64() == Some(*other)
    }
}

impl PartialOrd<u64> for ActionValue {
    fn partial_cmp(&self, other: &u64) -> Option<std::cmp::Ordering> {
        self.as_u64()?.partial_cmp(other)
    }
}

#[derive(Debug, Clone, uniffi::Record)]
pub struct ActionInvocationRequest {
    pub catalog_revision: String,
    pub action_id: String,
    pub connection: String,
    pub mode: String,
    pub role: ActionRole,
    pub parameters: Vec<ActionArgument>,
}

#[derive(Debug, Clone, uniffi::Record)]
pub struct ResolvedActionInvocation {
    pub action: ActionVerb,
    pub role: ActionRole,
    pub parameters: Vec<ActionArgument>,
    pub responder_mutation: Option<ResponderMutation>,
}

#[derive(Debug, thiserror::Error, uniffi::Error)]
pub enum ActionResolutionError {
    #[error("action invocation rejected ({code}): {detail}")]
    Rejected { code: String, detail: String },
}

/// Typed view of the per-object prefix inside the manifest's canonical
/// `ImportObjects` action. A gallery consumer prepares exactly one selected
/// handle, then streams `read` repeatedly with the resolved u64 total/window.
/// Slot names remain manifest data and are surfaced here so consumers never
/// inspect the all-object action's nested AST.
#[derive(Debug, uniffi::Record)]
pub struct SelectedObjectTransferInfo {
    /// Runtime slots required by `preparation_steps` (currently the selected handle).
    pub params: Vec<String>,
    /// Per-object steps before the canonical chunk loop.
    pub preparation_steps: Vec<EntryStep>,
    /// Index of the preparation step whose data response contains ObjectInfo.
    pub object_info_step_index: u32,
    /// Binding populated with the resolved transfer total (reported or extension u64).
    pub transfer_size_slot: String,
    /// Binding populated with the camera-declared chunk window.
    pub chunk_size_slot: String,
    /// Existing per-chunk read action, including manifest-derived offset splitting.
    pub read: Action,
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct KeyValue {
    pub key: String,
    pub value: String,
}

/// A model a manufacturer index declares: its id plus the body-manifest path
/// the index names for it (relative to the manufacturer directory, e.g.
/// `xa7/xa7.yaml`).
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct IndexModelRef {
    pub id: String,
    pub manifest_path: String,
}

/// Parse-only enumeration of the models a manufacturer index declares —
/// exactly the bodies [`ConfigStore::from_manufacturer_index`] will demand.
/// Lets a host resolve the body list from the vendored index itself instead
/// of hardcoding model ids, so shipping a new model is pure manifest data
/// with no host code change (#308 review follow-through). Declaration order
/// is preserved (§11.7: first model is the primary manifest).
#[uniffi::export]
pub fn index_model_refs(index_yaml: String) -> Result<Vec<IndexModelRef>, ConfigError> {
    let index = cc::index::ResolvedManufacturerIndex::from_yaml(&index_yaml)?;
    Ok(index
        .models
        .iter()
        .map(|model| IndexModelRef {
            id: model.id.clone(),
            manifest_path: model.manifest_path.to_string_lossy().into_owned(),
        })
        .collect())
}

/// How to bring a known connection up (data only — the app drives the
/// GATT/UDP/TCP I/O). Returned by [`ConfigStore::connection_establishment`].
///
/// Distinct from the manufacturer-index pull model's `EstablishmentPlan`:
/// this type answers the single-connection query (`connection_establishment("ble")`
/// → "here are the GATT UUIDs and knock ports for the ble connection").
#[derive(uniffi::Record)]
pub struct ConnectionEstablishmentInfo {
    pub target_connection: String,
    pub mechanism: Option<String>,
    pub user_instruction: Option<String>,
    pub requires: Option<FfiPredicate>,
    pub params: Vec<KeyValue>,
    pub activities: Vec<ConnectionActivityDescriptor>,
}

/// Manifest-owned PCSS rendezvous parameters. A consumer binds
/// `callback_port`, selects one of the manifest-supported discovery targets,
/// sends discovery to `knock_port`, and connects to the callback peer at the
/// port returned by the parsed callback.
#[derive(Debug, Clone, uniffi::Record)]
pub struct PcssRendezvousInfo {
    pub callback_port: u16,
    pub knock_port: u16,
    pub protocol: String,
    pub camera_name: Option<String>,
    pub default_discovery_target: PcssDiscoveryTarget,
    pub supported_discovery_targets: Vec<PcssDiscoveryTarget>,
    /// Whether an unavailable broadcast-discovered command endpoint or first
    /// Init transport attempt may trigger a fresh unicast rendezvous to the
    /// validated discovered address.
    pub retry_discovered_unicast: bool,
    /// Exact manifest-derived bytes, including the final CRLF, that complete
    /// the callback even when the camera keeps the TCP socket open.
    pub callback_message_terminator: Vec<u8>,
    pub retry_interval_ms: u32,
    pub max_attempts: u32,
    pub connect_timeout_ms: u32,
    pub init_retries_max: u32,
    pub init_retries_backoff_ms: u32,
    pub init_retry_reasons: Vec<u32>,
}

/// Where a PCSS discovery datagram is addressed. `SubnetBroadcast` discovers
/// the camera from callback DSC validated against the peer and therefore
/// requires no camera address; `ExplicitUnicast` requires one and peer-matches
/// the callback against it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum PcssDiscoveryTarget {
    SubnetBroadcast,
    ExplicitUnicast,
}

impl From<cc::PcssDiscoveryTarget> for PcssDiscoveryTarget {
    fn from(value: cc::PcssDiscoveryTarget) -> Self {
        match value {
            cc::PcssDiscoveryTarget::SubnetBroadcast => Self::SubnetBroadcast,
            cc::PcssDiscoveryTarget::ExplicitUnicast => Self::ExplicitUnicast,
        }
    }
}

#[derive(Debug, Clone, uniffi::Record)]
pub struct PcssNotifyInfo {
    /// Camera IPv4 from the required DSC header. The transport must verify it
    /// equals the callback TCP peer before treating the callback as discovery.
    pub camera_ipv4: String,
    pub camera_name: String,
    pub command_port: u16,
    pub service: String,
}

#[derive(Debug, uniffi::Enum)]
pub enum ResolvedValue {
    Fixed {
        value: String,
    },
    Generated {
        scheme: String,
        persist: bool,
    },
    FromPairing {
        source: String,
    },
    /// Client-derived from a runtime slot the host fills (e.g. the BLE-registered
    /// device name). The consumer supplies the value; the manifest only names the
    /// slot — it is never a literal. See `ValuePolicy::ClientDerived` (#109).
    ClientDerived {
        runtime: String,
    },
}

/// Evaluation of one predicate leaf (telemetry / config iteration).
#[derive(Debug, uniffi::Record)]
pub struct LeafEval {
    pub prop: String,
    pub observed: Option<i64>,
    pub effective: Option<i64>,
    pub test: String,
    pub passed: bool,
}

#[derive(Debug, uniffi::Record)]
pub struct PredicateOutcome {
    pub passed: bool,
    pub leaves: Vec<LeafEval>,
    pub summary: String,
}

/// The serializable "why" behind a gating decision — capture into telemetry.
#[derive(Debug, uniffi::Record)]
pub struct ResolutionTrace {
    pub query: String,
    pub connection: String,
    pub mode: String,
    pub op: u16,
    pub outcome: String,
    pub connection_ok: bool,
    pub mode_ok: bool,
    pub requires: Option<PredicateOutcome>,
    pub reason: String,
}

/// An availability decision plus the trace explaining it.
#[derive(Debug, uniffi::Record)]
pub struct GateExplanation {
    pub availability: Availability,
    pub trace: ResolutionTrace,
}

impl From<cc::LeafEval> for LeafEval {
    fn from(l: cc::LeafEval) -> Self {
        LeafEval {
            prop: l.prop,
            observed: l.observed,
            effective: l.effective,
            test: l.test,
            passed: l.passed,
        }
    }
}

impl From<cc::PredicateOutcome> for PredicateOutcome {
    fn from(p: cc::PredicateOutcome) -> Self {
        PredicateOutcome {
            passed: p.passed,
            leaves: p.leaves.into_iter().map(Into::into).collect(),
            summary: p.summary,
        }
    }
}

impl From<cc::ResolutionTrace> for ResolutionTrace {
    fn from(t: cc::ResolutionTrace) -> Self {
        ResolutionTrace {
            query: t.query,
            connection: t.connection,
            mode: t.mode,
            op: t.op,
            outcome: t.outcome,
            connection_ok: t.connection_ok,
            mode_ok: t.mode_ok,
            requires: t.requires.map(Into::into),
            reason: t.reason,
        }
    }
}

/// A PTP/IP socket role a consumer binds (mirrors `cc::SocketRole`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum SocketRole {
    Command,
    Event,
    LiveView,
}

impl From<SocketRole> for cc::SocketRole {
    fn from(r: SocketRole) -> Self {
        match r {
            SocketRole::Command => cc::SocketRole::Command,
            SocketRole::Event => cc::SocketRole::Event,
            SocketRole::LiveView => cc::SocketRole::LiveView,
        }
    }
}

impl From<cc::SocketRole> for SocketRole {
    fn from(r: cc::SocketRole) -> Self {
        match r {
            cc::SocketRole::Command => SocketRole::Command,
            cc::SocketRole::Event => SocketRole::Event,
            cc::SocketRole::LiveView => SocketRole::LiveView,
        }
    }
}

/// One bound socket for a connection: which role, on which port (#140).
#[derive(Debug, uniffi::Record)]
pub struct SocketBindingInfo {
    pub role: SocketRole,
    pub host: Option<String>,
    pub port: u16,
}

#[derive(Debug, uniffi::Enum)]
pub enum CameraInitiatedTriggerMatch {
    All,
}

#[derive(Debug, uniffi::Enum)]
pub enum CameraInitiatedCompletion {
    ReadToEof,
}

#[derive(Debug, uniffi::Record)]
pub struct BleStateTriggerInfo {
    pub gatt_uuid: String,
    pub trigger_values: Vec<Vec<u8>>,
    pub baseline_values: Vec<Vec<u8>>,
}

#[derive(Debug, uniffi::Record)]
pub struct CameraInitiatedTriggerInfo {
    pub match_mode: CameraInitiatedTriggerMatch,
    pub states: Vec<BleStateTriggerInfo>,
}

#[derive(Debug, uniffi::Record)]
pub struct BleLiteralWriteInfo {
    pub gatt_uuid: String,
    pub value: Vec<u8>,
    pub required: bool,
}

#[derive(Debug, uniffi::Record)]
pub struct CameraInitiatedHandoffInfo {
    pub connection: String,
    pub socket_role: SocketRole,
    pub endpoint_host: Option<String>,
    pub endpoint_port: u16,
    pub cached_credentials_allowed: bool,
    pub function_launch: Option<BleLiteralWriteInfo>,
}

#[derive(Debug, uniffi::Record)]
pub struct CameraInitiatedReceiveInfo {
    pub mode: String,
    pub count_property: u16,
    pub count_member: u16,
    pub head_index: u32,
    pub metadata_operation: u16,
    pub metadata_phases: Vec<CameraInitiatedMetadataPhase>,
    pub data_operation: u16,
    pub chunk_limit_property: u16,
    pub completion: CameraInitiatedCompletion,
}

#[derive(Debug, Clone, Copy, uniffi::Enum)]
pub enum CameraInitiatedMetadataPhase {
    AfterCountBeforeModeEntry,
    AfterModeEntry,
}

#[derive(Debug, uniffi::Record)]
pub struct CameraInitiatedTransferInfo {
    pub trigger: CameraInitiatedTriggerInfo,
    pub handoff: CameraInitiatedHandoffInfo,
    pub monitor_recovery: Option<CameraInitiatedMonitorRecovery>,
    pub receive: CameraInitiatedReceiveInfo,
    pub evidence: Vec<String>,
}

/// Manifest-owned recovery route for a camera-initiated transfer monitor.
#[derive(Debug, Clone, Copy, uniffi::Enum)]
pub enum CameraInitiatedMonitorRecovery {
    SavedCameraReconnect,
}

/// The transport-close frame a connection sends before an image-transfer reopen,
/// with the manifest's named sentinel resolved to bytes (#140).
#[derive(Debug, uniffi::Record)]
pub struct TransportCloseInfo {
    /// The frame bytes to send.
    pub packet: Vec<u8>,
    /// When to send it (e.g. `before-image-transfer-reestablishment`), if declared.
    pub when: Option<String>,
}

// ----------------------------------------------------------------------------
// ConfigStore — the loaded, queryable seam
// ----------------------------------------------------------------------------

#[derive(uniffi::Object)]
pub struct ConfigStore {
    pub(crate) inner: cc::ConfigStore,
}

#[uniffi::export]
impl ConfigStore {
    /// Build from bundled YAML: the body manifest, plus optional manufacturer-tier
    /// defaults (`fuji.yaml`: versionOrder + the fixed initiator identity).
    #[uniffi::constructor]
    pub fn from_bundle(
        body_yaml: String,
        manufacturer_yaml: Option<String>,
    ) -> Result<Arc<Self>, ConfigError> {
        let m = cc::CameraManifest::from_yaml(&body_yaml)
            .map_err(|e| ConfigError::Parse(e.to_string()))?;
        build_store(m, manufacturer_yaml)
    }

    /// Like `from_bundle`, but with firmware-tier overlays deep-merged onto the body
    /// (most-specific last), e.g. `fw_overlays = [fw2.40.yaml]` flips XLV to HTTPS.
    /// Field-level merge — an overlay overrides only the keys it names.
    #[uniffi::constructor]
    pub fn from_tiers(
        body_yaml: String,
        manufacturer_yaml: Option<String>,
        fw_overlays: Vec<String>,
    ) -> Result<Arc<Self>, ConfigError> {
        let refs: Vec<&str> = fw_overlays.iter().map(String::as_str).collect();
        let m = cc::CameraManifest::from_tiers(&body_yaml, &refs)
            .map_err(|e| ConfigError::Parse(e.to_string()))?;
        build_store(m, manufacturer_yaml)
    }

    /// Load a manufacturer index + every model body it references (plan §3.1).
    /// `model_bodies` carries `(model_id, yaml_text)` pairs; missing entries
    /// surface as a parse-style [`ConfigError`].
    #[uniffi::constructor]
    pub fn from_manufacturer_index(
        index_yaml: String,
        model_bodies: Vec<KeyValue>,
    ) -> Result<Arc<Self>, ConfigError> {
        build_manufacturer_index_store(index_yaml, model_bodies, None)
    }

    /// Load a manufacturer index and validate every model body after applying
    /// shared manufacturer-tier defaults. Use this when indexed bodies refer
    /// to shared transport values such as the initiator identity.
    #[uniffi::constructor]
    pub fn from_manufacturer_index_with_defaults(
        index_yaml: String,
        manufacturer_yaml: String,
        model_bodies: Vec<KeyValue>,
    ) -> Result<Arc<Self>, ConfigError> {
        build_manufacturer_index_store(index_yaml, model_bodies, Some(manufacturer_yaml))
    }

    /// Return a direct-query store for one body loaded through a manufacturer
    /// index. The returned store preserves shared defaults and recognition data,
    /// but APIs such as `connections` and `camera_identity` use `model_id`.
    pub fn model_store(&self, model_id: String) -> Option<Arc<Self>> {
        self.inner
            .model_store(&model_id)
            .map(|inner| Arc::new(Self { inner }))
    }

    // -----------------------------------------------------------------------
    // Manufacturer-index pull model (§3.2 + §3.3 + §11)
    // -----------------------------------------------------------------------

    /// ScanObservation → decision. Returns [`Recognition::NoMatch`] when no
    /// signature fires; [`Recognition::Candidate`] for a single match (the
    /// MVP case); [`Recognition::Disambiguate`] when multiple models match
    /// the same signature.
    pub fn recognize(&self, observation: ScanObservation) -> Recognition {
        let Some(index) = &self.inner.index else {
            return Recognition::NoMatch;
        };
        match observation {
            ScanObservation::BleAdvert {
                service_uuids,
                manufacturer_data,
                service_data,
                local_name,
                tx_power,
                ad_records,
            } => {
                let facts = cc::index::eval::BleAdvertFacts {
                    service_uuids,
                    manufacturer_data: manufacturer_data.map(|m| (m.company_id, m.payload)),
                    service_data: service_data
                        .into_iter()
                        .map(|s| (s.uuid, s.payload))
                        .collect(),
                    local_name,
                    tx_power,
                    ad_records: ad_records
                        .into_iter()
                        .map(|r| (r.ad_type, r.payload))
                        .collect(),
                };
                mfg_index::recognize_ble(index, &facts)
            }
            ScanObservation::PcssNotify {
                camera_ipv4,
                camera_name,
                command_port,
                service,
            } => {
                mfg_index::recognize_pcss(index, &camera_ipv4, &camera_name, command_port, &service)
            }
        }
    }

    /// Manifest-authored timeout for scanning one known saved camera.
    pub fn reconnect_policy(&self, model: String) -> Option<ReconnectPolicy> {
        let index = self.inner.index.as_ref()?;
        mfg_index::reconnect_policy(index, &model)
    }

    /// Classify one advertisement for a known saved camera. Identity matching,
    /// standby-vs-awake state, and plan selection all come from manifest data.
    pub fn reconnect_decision(
        &self,
        model: String,
        observation: ScanObservation,
        persisted_scope: Vec<KeyValue>,
    ) -> ReconnectDecision {
        let Some(index) = &self.inner.index else {
            return ReconnectDecision::NoMatch;
        };
        let ScanObservation::BleAdvert {
            service_uuids,
            manufacturer_data,
            service_data,
            local_name,
            tx_power,
            ad_records,
        } = observation
        else {
            return ReconnectDecision::NoMatch;
        };
        let facts = cc::index::eval::BleAdvertFacts {
            service_uuids,
            manufacturer_data: manufacturer_data.map(|m| (m.company_id, m.payload)),
            service_data: service_data
                .into_iter()
                .map(|s| (s.uuid, s.payload))
                .collect(),
            local_name,
            tx_power,
            ad_records: ad_records
                .into_iter()
                .map(|r| (r.ad_type, r.payload))
                .collect(),
        };
        mfg_index::reconnect_decision(index, &model, &facts, &persisted_scope)
    }

    /// Per-(model, connection) establishment plan with the given
    /// `initial_scope` (typically the runtime_scope from a
    /// [`Recognition::Candidate`]).
    ///
    /// Returns `None` if the model is unknown, the connection declares no
    /// establishment mechanism (e.g. `usb`), or no plan is registered under
    /// that mechanism. The plan's [`Step`] values keep their structured
    /// `Captured` / `Runtime` / `Template` forms — scope is resolved by the
    /// dispatcher mid-walk (plan §11.1).
    pub fn establishment(
        &self,
        model: String,
        connection: String,
        initial_scope: Vec<KeyValue>,
    ) -> Option<EstablishmentPlan> {
        let index = self.inner.index.as_ref()?;
        // The body manifest maps connection → establishment mechanism; the
        // index registry holds the plan under that mechanism name.
        let body = self.inner.body(&model)?;
        let connection_config = body.connections.get(&connection)?;
        let mechanism = connection_config.establishment.clone()?;
        let mut plan =
            mfg_index::build_establishment(index, &model, &connection, &mechanism, &initial_scope)?;
        plan.activities
            .extend(connection_config.activities.iter().map(Into::into));
        Some(plan)
    }

    /// The BLE-native control action plan registered under `action` for `model`
    /// (#91) — e.g. `remote-shutter`, `write-gps`. Runnable from the resting
    /// BLE-connected link without Wi-Fi. `None` if the model or action is unknown.
    pub fn ble_action(&self, model: String, action: String) -> Option<BleActionPlan> {
        let index = self.inner.index.as_ref()?;
        mfg_index::build_ble_action(index, &model, &action)
    }

    /// Per §11.5: validate the plan handle and return either "keep the existing
    /// tail" or a replacement unwalked tail for the dispatcher to splice at
    /// `next_step_index`.
    ///
    /// Current manifests have no firmware-branching establishment overlays, so a
    /// valid plan returns [`EstablishmentRefinement::NoChange`]. Bad handles and
    /// impossible step indices are explicit errors instead of silent no-ops.
    pub fn refine_establishment(
        &self,
        plan_handle: String,
        firmware: String,
        scope: Vec<KeyValue>,
        next_step_index: u32,
    ) -> Result<EstablishmentRefinement, EstablishmentError> {
        Ok(refinement_to_ffi(refine_establishment_native(
            self,
            plan_handle,
            firmware,
            scope,
            next_step_index,
        )?))
    }

    /// Connections valid on `platform` under the camera's firmware (instax filtered
    /// by `availableWhen`; USB/tether hidden where `platforms:` excludes — all data).
    pub fn connections(&self, platform: Platform) -> Vec<ConnectionInfo> {
        let available: BTreeSet<&str> = self.inner.connections_available().into_iter().collect();
        self.inner
            .manifest
            .connections
            .iter()
            .filter(|(id, c)| available.contains(id.as_str()) && platform_ok(c, &platform))
            .map(|(id, c)| ConnectionInfo {
                id: id.clone(),
                kind: c.kind.clone().unwrap_or_default(),
                discovery: yaml_path_str(&c.extra, &["discovery", "mechanism"]).unwrap_or_default(),
                auto_discoverable: yaml_path_bool(&c.extra, &["discovery", "autoDiscoverable"])
                    .unwrap_or(true),
                command_listener_volatile: c.command_listener_volatile,
                init_shape: c.init_shape.clone(),
                live_view_delivery: c.live_view_delivery.as_ref().map(Into::into),
                shutter_recipe: c.shutter_recipe.map(Into::into),
                command_framing: c.command_framing.map(Into::into),
                event_framing: c.event_framing.map(Into::into),
            })
            .collect()
    }

    /// How to bring `connection` up: its establishment mechanism + params (knock
    /// ports, GATT char uuids) as DATA. Returns `None` for an unknown connection.
    ///
    /// Distinct from `establishment(model, connection, initial_scope)`, the
    /// manufacturer-index pull-model flow (plan §3.3): this is the direct
    /// per-connection lookup on an already-loaded body config.
    pub fn connection_establishment(
        &self,
        connection: String,
    ) -> Option<ConnectionEstablishmentInfo> {
        let c = self.inner.manifest.connections.get(&connection)?;
        let mut params = Vec::new();
        if let Some(knock) = &c.knock {
            params.push(KeyValue {
                key: "callbackPort".into(),
                value: knock.callback_port.to_string(),
            });
            params.push(KeyValue {
                key: "knockPort".into(),
                value: knock.knock_port.to_string(),
            });
            params.push(KeyValue {
                key: "protocol".into(),
                value: knock.protocol.clone(),
            });
        }
        if let Some(retries) = &c.init_retries {
            params.push(KeyValue {
                key: "initRetriesMax".into(),
                value: retries.max.to_string(),
            });
            params.push(KeyValue {
                key: "initRetriesBackoffMs".into(),
                value: retries.backoff_ms.to_string(),
            });
        }
        for (k, v) in flattened_establishment_params(&c.extra) {
            params.push(KeyValue { key: k, value: v });
        }
        Some(ConnectionEstablishmentInfo {
            target_connection: connection,
            mechanism: c.establishment.clone(),
            user_instruction: None,
            requires: None,
            params,
            activities: c.activities.iter().map(Into::into).collect(),
        })
    }

    /// Resolve a manifest-authored connection-bring-up edge, optionally
    /// qualified by the target mode. The returned params are fixed bindings for
    /// the target establishment plan, so callers never carry vendor feature
    /// selector literals in app code.
    pub fn connection_transition(
        &self,
        from_connection: String,
        target_connection: String,
        target_mode: Option<String>,
    ) -> Option<ConnectionEstablishmentInfo> {
        let source = self.inner.manifest.connections.get(&from_connection)?;
        let transition = source
            .enables
            .iter()
            .find(|candidate| candidate.to == target_connection && candidate.mode == target_mode)?;
        let target = self.inner.manifest.connections.get(&target_connection)?;
        Some(ConnectionEstablishmentInfo {
            target_connection,
            mechanism: transition.mechanism.clone(),
            user_instruction: transition.user_instruction.clone(),
            requires: transition.requires.as_ref().map(|predicate| {
                FfiPredicate::try_from(predicate)
                    .expect("connection transition requires validated at store load")
            }),
            params: transition
                .params
                .iter()
                .map(|(key, value)| KeyValue {
                    key: key.clone(),
                    value: value.clone(),
                })
                .collect(),
            activities: target.activities.iter().map(Into::into).collect(),
        })
    }

    /// Typed PCSS rendezvous contract for `connection`, or `None` when that
    /// connection does not use the PCSS knock/callback handshake.
    pub fn pcss_rendezvous(&self, connection: String) -> Option<PcssRendezvousInfo> {
        let candidate = self.inner.manifest.connections.get(&connection)?;
        let knock = candidate.knock.as_ref()?;
        let retries = candidate.init_retries.as_ref();
        Some(PcssRendezvousInfo {
            callback_port: knock.callback_port,
            knock_port: knock.knock_port,
            protocol: knock.protocol.clone(),
            camera_name: knock.camera_name.clone(),
            default_discovery_target: knock.discovery_targets.default.into(),
            supported_discovery_targets: knock
                .discovery_targets
                .supported
                .iter()
                .copied()
                .map(Into::into)
                .collect(),
            retry_discovered_unicast: knock.discovery_targets.retry_discovered_unicast,
            callback_message_terminator: protocol_primitives::pcss_notify_message_terminator(
                &knock.protocol,
            ),
            retry_interval_ms: knock.retry_interval_ms,
            max_attempts: knock.max_attempts,
            connect_timeout_ms: knock.connect_timeout_ms,
            init_retries_max: retries.map_or(0, |retries| retries.max),
            init_retries_backoff_ms: retries.map_or(0, |retries| retries.backoff_ms),
            init_retry_reasons: retries
                .into_iter()
                .flat_map(|retries| retries.when_reasons.iter())
                .filter_map(|reason| cc::model::parse_hex_u32(reason))
                .collect(),
        })
    }

    /// Build the PCSS discovery datagram from the selected connection's
    /// protocol and the route-selected local IPv4 callback address.
    pub fn build_pcss_discovery(
        &self,
        connection: String,
        callback_ipv4: String,
    ) -> Result<Vec<u8>, CodecError> {
        let knock = self
            .inner
            .manifest
            .connections
            .get(&connection)
            .and_then(|candidate| candidate.knock.as_ref())
            .ok_or_else(|| {
                CodecError::Encode(format!("connection '{connection}' has no PCSS rendezvous"))
            })?;
        let host = callback_ipv4
            .parse::<std::net::Ipv4Addr>()
            .map_err(codec_encode)?;
        Ok(protocol_primitives::pcss_discovery_message(
            host,
            &knock.protocol,
        ))
    }

    /// Build the fixed PCSS InitCommandRequest after rendezvous. The host
    /// supplies its route-selected IPv4, 16-byte initiator GUID, and friendly
    /// name; all layout details remain inside protocol-primitives.
    pub fn build_pcss_init(
        &self,
        connection: String,
        initiator_guid: Vec<u8>,
        client_ipv4: String,
        friendly_name: String,
    ) -> Result<Vec<u8>, CodecError> {
        self.inner
            .manifest
            .connections
            .get(&connection)
            .filter(|candidate| candidate.init_shape.as_deref() == Some("pcssKnock"))
            .ok_or_else(|| {
                CodecError::Encode(format!("connection '{connection}' has no PCSS init"))
            })?;
        let initiator_guid: [u8; 16] = initiator_guid.try_into().map_err(|guid: Vec<u8>| {
            CodecError::Encode(format!(
                "PCSS initiator GUID is {} bytes, expected 16",
                guid.len()
            ))
        })?;
        let client_ip = client_ipv4
            .parse::<std::net::Ipv4Addr>()
            .map_err(codec_encode)?;
        protocol_primitives::pcss_init_message(initiator_guid, client_ip, &friendly_name)
            .map_err(codec_encode)
    }

    /// Parse and protocol-check the camera's PCSS callback.
    pub fn parse_pcss_notify(
        &self,
        connection: String,
        payload: Vec<u8>,
    ) -> Result<PcssNotifyInfo, CodecError> {
        let knock = self
            .inner
            .manifest
            .connections
            .get(&connection)
            .and_then(|candidate| candidate.knock.as_ref())
            .ok_or_else(|| {
                CodecError::Decode(format!("connection '{connection}' has no PCSS rendezvous"))
            })?;
        let parsed = protocol_primitives::parse_pcss_notify(&payload, &knock.protocol)
            .map_err(codec_decode)?;
        Ok(PcssNotifyInfo {
            camera_ipv4: parsed.camera_address.to_string(),
            camera_name: parsed.camera_name,
            command_port: parsed.command_port,
            service: parsed.service,
        })
    }

    /// Byte-exact acknowledgement for a validated callback. Requiring a PCSS
    /// connection keeps clients from treating this as a generic HTTP response.
    pub fn build_pcss_callback_ack(&self, connection: String) -> Result<Vec<u8>, CodecError> {
        self.inner
            .manifest
            .connections
            .get(&connection)
            .and_then(|candidate| candidate.knock.as_ref())
            .ok_or_else(|| {
                CodecError::Encode(format!("connection '{connection}' has no PCSS rendezvous"))
            })?;
        Ok(protocol_primitives::pcss_callback_ack_message())
    }

    /// The port a consumer binds for `role` on `connection` (command / event /
    /// live-view), or `None` if this connection has no such socket. Replaces the
    /// app's hardcoded Fuji command-port + `+1`/`+2` offsets (#140).
    pub fn port_for_role(&self, connection: String, role: SocketRole) -> Option<u16> {
        self.inner
            .manifest
            .connections
            .get(&connection)?
            .bindings
            .as_ref()?
            .port_for(role.into())
    }

    /// Every bound socket for `connection`, keyed by role, in `command → event →
    /// live-view` order (roles the connection lacks are omitted).
    pub fn socket_bindings(&self, connection: String) -> Vec<SocketBindingInfo> {
        let Some(b) = self
            .inner
            .manifest
            .connections
            .get(&connection)
            .and_then(|c| c.bindings.as_ref())
        else {
            return Vec::new();
        };
        [
            (SocketRole::Command, Some(b.command)),
            (SocketRole::Event, b.event),
            (SocketRole::LiveView, b.live_view),
        ]
        .into_iter()
        .filter_map(|(role, port)| {
            port.map(|port| SocketBindingInfo {
                role,
                host: b.host.clone(),
                port,
            })
        })
        .collect()
    }

    /// The camera-status-triggered private media pull for a recognized model. The
    /// manufacturer-index loader has already resolved symbolic GATT names and
    /// exact wire bytes. Single-body stores return `None`.
    pub fn camera_initiated_transfer(&self, model: String) -> Option<CameraInitiatedTransferInfo> {
        self.inner
            .camera_initiated_transfer(&model)
            .map(map_camera_initiated_transfer)
    }

    /// The transport-close frame `connection` sends before reopening an
    /// image-transfer session, with the named sentinel resolved through manifest
    /// data (#140).
    pub fn transport_close(
        &self,
        connection: String,
    ) -> Result<Option<TransportCloseInfo>, TransportCloseError> {
        let Some(tc) = self
            .inner
            .manifest
            .connections
            .get(&connection)
            .and_then(|c| c.transport_close.as_ref())
        else {
            return Ok(None);
        };
        let frame = self
            .inner
            .manifest
            .sentinels
            .get(&tc.sentinel)
            .ok_or_else(|| TransportCloseError::UnknownSentinel(tc.sentinel.clone()))?;
        let packet = cc::parse_hex_bytes(&frame.bytes)
            .ok_or_else(|| TransportCloseError::InvalidSentinelBytes(tc.sentinel.clone()))?;
        Ok(Some(TransportCloseInfo {
            packet,
            when: tc.when.clone(),
        }))
    }

    /// Modes reachable over `connection`, with inherited capabilities.
    pub fn modes(&self, connection: String) -> Vec<ModeInfo> {
        let Some(c) = self.inner.manifest.connections.get(&connection) else {
            return Vec::new();
        };
        c.modes
            .iter()
            .map(|path| ModeInfo {
                path: path.clone(),
                capabilities: self.capabilities(connection.clone(), path.clone()),
            })
            .collect()
    }

    pub fn capabilities(&self, _connection: String, mode: String) -> Vec<String> {
        self.inner
            .manifest
            .capabilities(&mode)
            .into_iter()
            .map(String::from)
            .collect()
    }

    /// Which mode the observed props indicate (evaluates `detect` predicates).
    /// `None` → app should present a picker over [`Self::modes`].
    pub fn detect_mode(
        &self,
        _connection: String,
        observed: Vec<PropObservation>,
    ) -> Option<String> {
        self.inner
            .manifest
            .detect_mode(&prop_view(&observed))
            .map(String::from)
    }

    /// The typed execution plan to enter `to`: current-session PTP steps, a manual
    /// instruction, or an outer connection re-establishment followed by the cold
    /// entry for `to`.
    pub fn mode_entry(
        &self,
        connection: String,
        from: Option<String>,
        to: String,
    ) -> Option<ModeEntryPlan> {
        let c = self.inner.manifest.connections.get(&connection)?;
        let e = c.entries.iter().find(|e| e.to == to && e.from == from)?;
        let execution = match &e.execution {
            cc::ModeEntryExecution::Ptp { steps } => ModeEntryExecution::Ptp {
                steps: steps
                    .iter()
                    .map(|step| map_step(step).expect("mode entries validated at store load"))
                    .collect(),
            },
            cc::ModeEntryExecution::ReestablishConnection(reestablish) => {
                ModeEntryExecution::ReestablishConnection {
                    connection,
                    exit_steps: reestablish
                        .exit_steps
                        .iter()
                        .map(|step| map_step(step).expect("mode entries validated at store load"))
                        .collect(),
                    establishment_params: reestablish
                        .params
                        .iter()
                        .map(|(key, value)| KeyValue {
                            key: key.clone(),
                            value: value.clone(),
                        })
                        .collect(),
                }
            }
            cc::ModeEntryExecution::UserInstruction { instruction } => {
                ModeEntryExecution::UserInstruction {
                    instruction: instruction.clone(),
                }
            }
        };
        Some(ModeEntryPlan {
            to: e.to.clone(),
            from: e.from.clone(),
            requires: e.requires.as_ref().map(|predicate| {
                FfiPredicate::try_from(predicate)
                    .expect("mode entry requires validated at store load")
            }),
            execution,
            activities: e.activities.iter().map(Into::into).collect(),
        })
    }

    /// Named in-mode action recipe (`docs/plans/action-verbs.md`). Returns
    /// `None` if the connection doesn't declare an action for this verb;
    /// the consumer surfaces it as "not supported on this transport"
    /// without encoding a negative list itself.
    ///
    /// The returned `Action.params` names runtime slots the caller MUST bind
    /// for `EntryParam::Runtime` references in `Action.steps` to resolve.
    /// `Action.triggers` declares post-conditions to plan UX against.
    pub fn action(&self, connection: String, verb: ActionVerb) -> Option<Action> {
        let cc_verb = ffi_to_cc_verb(verb);
        let a = self.inner.manifest.action(&connection, cc_verb)?;
        Some(map_action(a))
    }

    /// Deterministic manifest action registry. Its revision binds invocation
    /// requests to the exact catalog the caller displayed.
    pub fn action_catalog(&self) -> ActionCatalog {
        map_action_catalog(self.inner.manifest.action_catalog())
    }

    /// Resolve and validate an invocation without performing transport I/O or
    /// simulator mutation.
    pub fn resolve_action_invocation(
        &self,
        request: ActionInvocationRequest,
    ) -> Result<ResolvedActionInvocation, ActionResolutionError> {
        let request = cc::ActionInvocationRequest {
            catalog_revision: request.catalog_revision,
            action_id: request.action_id,
            connection: request.connection,
            mode: request.mode,
            role: ffi_to_cc_action_role(request.role),
            parameters: request
                .parameters
                .into_iter()
                .map(|argument| cc::ActionArgument {
                    name: argument.name,
                    value: match argument.value {
                        ActionValue::U64 { value } => cc::ActionArgumentValue::U64(value),
                        ActionValue::String { value } => cc::ActionArgumentValue::String(value),
                    },
                })
                .collect(),
        };
        self.inner
            .manifest
            .resolve_action_invocation(&request)
            .map(map_resolved_action)
            .map_err(|error| ActionResolutionError::Rejected {
                code: error.code().to_string(),
                detail: error.to_string(),
            })
    }

    /// Typed object-transfer policy for a connection.
    pub fn object_transfer_contract(
        &self,
        connection: String,
    ) -> Option<ObjectTransferContractInfo> {
        let contract = self
            .inner
            .manifest
            .connections
            .get(&connection)?
            .object_transfer
            .as_ref()?;
        Some(ObjectTransferContractInfo {
            strategy: contract.strategy.into(),
            resume_policy: contract.resume_policy.into(),
            read_action: cc_to_ffi_verb(contract.read_action),
            completion_action: contract
                .completion
                .as_ref()
                .map(|completion| cc_to_ffi_verb(completion.action)),
            completion_after: contract
                .completion
                .as_ref()
                .map(|completion| completion.after.into()),
            formats: contract
                .formats
                .iter()
                .filter_map(|(code, support)| {
                    Some(ObjectTransferFormatInfo {
                        code: parse_hex_code(code)?,
                        support: (*support).into(),
                    })
                })
                .collect(),
        })
    }

    /// The manifest-owned preparation/read contract for one selected object.
    ///
    /// `ImportObjects` remains the canonical all-handles reference recipe. This
    /// method projects its typed per-handle prefix (everything before the nested
    /// chunk loop) plus the existing `GetObject` read action, so a lazy gallery
    /// does not run every handle or depend on capture-slot names/AST nesting.
    pub fn selected_object_transfer(
        &self,
        connection: String,
    ) -> Result<Option<SelectedObjectTransferInfo>, ConfigError> {
        let Some(import) = self
            .inner
            .manifest
            .action(&connection, cc::ActionVerb::ImportObjects)
        else {
            return Ok(None);
        };
        let Some(read) = self
            .inner
            .manifest
            .action(&connection, cc::ActionVerb::GetObject)
        else {
            return Ok(None);
        };
        project_selected_object_transfer(import, read).map(Some)
    }

    /// Is `op` usable over `connection` in `mode` given `observed`? Intersects the
    /// orthogonal axes and evaluates the `requires` prerequisite.
    pub fn operation_available(
        &self,
        connection: String,
        mode: String,
        op: u16,
        observed: Vec<PropObservation>,
    ) -> Availability {
        self.inner
            .manifest
            .operation_available(&connection, &mode, op, &prop_view(&observed))
            .into()
    }

    /// Like `operation_available`, but also returns the trace explaining the
    /// decision (gating checks + the `requires` predicate's leaf evaluations) —
    /// capture into telemetry for fast config iteration.
    pub fn operation_available_explained(
        &self,
        connection: String,
        mode: String,
        op: u16,
        observed: Vec<PropObservation>,
    ) -> GateExplanation {
        let (availability, trace) = self.inner.manifest.operation_available_explained(
            &connection,
            &mode,
            op,
            &prop_view(&observed),
        );
        GateExplanation {
            availability: availability.into(),
            trace: trace.into(),
        }
    }

    /// Intent→mechanism: how to set `prop` over this connection/mode (App vendor-step
    /// vs tether absolute). Tries the connection-keyed control, then the mode-keyed.
    pub fn control_for(&self, connection: String, mode: String, prop: u16) -> Option<ControlInfo> {
        let m = &self.inner.manifest;
        let ctl = m
            .control_for(prop, &connection)
            .or_else(|| m.control_for(prop, &mode))?;
        Some(ControlInfo {
            set_method: ctl.set_method.clone(),
            operation: ctl.operation.as_deref().and_then(parse_hex_code),
            readback: ctl.readback.as_deref().and_then(parse_hex_code),
        })
    }

    /// Semantic controls for a connection/mode. Property codes and write
    /// mechanisms are returned as data; the caller never maps intent to a
    /// camera-specific property itself.
    pub fn control_surface(&self, connection: String, mode: String) -> Vec<ControlSurfaceInfo> {
        let Some(surface) = self
            .inner
            .manifest
            .connections
            .get(&connection)
            .and_then(|candidate| candidate.control_surfaces.get(&mode))
        else {
            return Vec::new();
        };
        surface
            .iter()
            .filter_map(|(role, entry)| {
                let property = parse_hex_code(&entry.property)?;
                let control = self.control_for(connection.clone(), mode.clone(), property)?;
                Some(ControlSurfaceInfo {
                    role: (*role).into(),
                    property,
                    read_source: entry.read_source.into(),
                    evidence_basis: entry.evidence_basis.into(),
                    observed_effect: entry.observed_effect.into(),
                    effective_owner: entry.effective_owner.map(Into::into),
                    control,
                })
            })
            .collect()
    }

    /// Value-policy resolution (fixed initiator identity, generated session ids, …),
    /// body overriding manufacturer.
    pub fn value(&self, key: String) -> Option<ResolvedValue> {
        match self.inner.value(&key)? {
            cc::ValuePolicy::Fixed { value } => Some(ResolvedValue::Fixed {
                value: yaml_scalar(value).expect("fixed values validated at store load"),
            }),
            cc::ValuePolicy::Generated { scheme, persist } => Some(ResolvedValue::Generated {
                scheme: scheme.clone(),
                persist: *persist,
            }),
            cc::ValuePolicy::FromPairing { source } => Some(ResolvedValue::FromPairing {
                source: source.clone(),
            }),
            cc::ValuePolicy::ClientDerived { runtime } => Some(ResolvedValue::ClientDerived {
                runtime: runtime.clone(),
            }),
        }
    }

    /// The InitCommandRequest for `connection`, assembled entirely from manifest
    /// data: resolved GUID + friendly name (via `values:`), plus the pre-built
    /// packet — so the app replays bytes with no client-side literals. `None` if
    /// the connection declares no `init` shape (e.g. usb) or the identity can't
    /// resolve. (#82)
    ///
    /// Returns `None` when the friendly name is `client-derived` (#109): the name
    /// is not a manifest literal but the host's own device name (which must equal
    /// the BLE `deviceNameString` it registered), so the consumer must supply it
    /// and build the packet itself. Baking the name from a host-supplied slot is
    /// deferred to the consumer-adoption work (#29).
    pub fn connection_init(&self, connection: String) -> Option<InitShapeInfo> {
        let c = self.inner.manifest.connections.get(&connection)?;
        let shape = c.init_shape.as_deref()?;
        if !matches!(shape, "app82" | "legacyApp82" | "standardPtpIp") {
            return None;
        }
        let init = c.init.as_ref()?;
        let friendly_name = self.fixed_value(&init.identity.friendly_name)?;
        let guid = hex_value(&self.fixed_value(&init.identity.guid)?)?;
        let client_ipv4 = match &init.identity.client_ipv4 {
            Some(key) => Some(self.fixed_value(key)?),
            None => None,
        };
        let expected_responder_guid = match &init.expected_responder_guid {
            Some(key) => Some(hex_value(&self.fixed_value(key)?)?),
            None => None,
        };
        let packet = match shape {
            "app82" => protocol_primitives::build_app_init(&guid, &friendly_name).ok()?,
            "legacyApp82" => protocol_primitives::build_legacy_app_init(
                &guid,
                client_ipv4.as_ref()?.parse().ok()?,
                &friendly_name,
            )
            .ok()?,
            "standardPtpIp" => standard_ptpip_init(&guid, &friendly_name)?,
            _ => unreachable!(),
        };
        Some(InitShapeInfo {
            guid,
            friendly_name,
            client_ipv4,
            name_field_byte_count: init.name_field_byte_count,
            expected_responder_guid: expected_responder_guid.unwrap_or_default(),
            packet,
        })
    }

    /// Runtime-aware InitCommandRequest assembly (#109/#29). Same shape as
    /// [`connection_init`], but `client-derived` identity slots resolve from the
    /// caller's runtime scope (for Fuji, `terminalName`). This lets consumers keep
    /// the BLE deviceNameString and PTP/IP friendlyName single-sourced.
    pub fn connection_init_with_runtime(
        &self,
        connection: String,
        runtime_scope: Vec<KeyValue>,
    ) -> Option<InitShapeInfo> {
        let c = self.inner.manifest.connections.get(&connection)?;
        let shape = c.init_shape.as_deref()?;
        if !matches!(shape, "app82" | "legacyApp82" | "standardPtpIp") {
            return None;
        }
        let init = c.init.as_ref()?;
        let scope: BTreeMap<String, String> = runtime_scope
            .into_iter()
            .map(|kv| (kv.key, kv.value))
            .collect();
        let friendly_name = value_with_runtime(&self.inner, &init.identity.friendly_name, &scope)?;
        let guid = hex_value(&value_with_runtime(
            &self.inner,
            &init.identity.guid,
            &scope,
        )?)?;
        let client_ipv4 = match &init.identity.client_ipv4 {
            Some(key) => Some(value_with_runtime(&self.inner, key, &scope)?),
            None => None,
        };
        let expected_responder_guid = match &init.expected_responder_guid {
            Some(key) => Some(hex_value(&value_with_runtime(&self.inner, key, &scope)?)?),
            None => None,
        };
        let packet = match shape {
            "app82" => protocol_primitives::build_app_init(&guid, &friendly_name).ok()?,
            "legacyApp82" => protocol_primitives::build_legacy_app_init(
                &guid,
                client_ipv4.as_ref()?.parse().ok()?,
                &friendly_name,
            )
            .ok()?,
            "standardPtpIp" => standard_ptpip_init(&guid, &friendly_name)?,
            _ => unreachable!(),
        };
        Some(InitShapeInfo {
            guid,
            friendly_name,
            client_ipv4,
            name_field_byte_count: init.name_field_byte_count,
            expected_responder_guid: expected_responder_guid.unwrap_or_default(),
            packet,
        })
    }

    /// Resolve the manifest-owned initiator identity without assuming an init
    /// packet shape. reference app and PCSS use the same identity policy but different
    /// byte layouts.
    pub fn connection_init_identity_with_runtime(
        &self,
        connection: String,
        runtime_scope: Vec<KeyValue>,
    ) -> Option<ResolvedInitIdentityInfo> {
        let init = self
            .inner
            .manifest
            .connections
            .get(&connection)?
            .init
            .as_ref()?;
        let scope: BTreeMap<String, String> = runtime_scope
            .into_iter()
            .map(|kv| (kv.key, kv.value))
            .collect();
        let friendly_name = value_with_runtime(&self.inner, &init.identity.friendly_name, &scope)?;
        let guid = hex_value(&value_with_runtime(
            &self.inner,
            &init.identity.guid,
            &scope,
        )?)?;
        if guid.len() != 16 {
            return None;
        }
        Some(ResolvedInitIdentityInfo {
            guid,
            friendly_name,
        })
    }

    /// Fixed responder GUID declared for an init shape, if any.
    pub fn connection_expected_responder_guid(&self, connection: String) -> Option<Vec<u8>> {
        let key = self
            .inner
            .manifest
            .connections
            .get(&connection)?
            .init
            .as_ref()?
            .expected_responder_guid
            .as_ref()?;
        hex_value(&self.fixed_value(key)?)
    }

    /// Typed InitFail retry selection for one connection.
    pub fn connection_init_retry_policy(&self, connection: String) -> Option<InitRetryPolicyInfo> {
        let retries = self
            .inner
            .manifest
            .connections
            .get(&connection)?
            .init_retries
            .as_ref()?;
        Some(InitRetryPolicyInfo {
            max_retries: retries.max,
            backoff_ms: retries.backoff_ms,
            when_reasons: retries
                .when_reasons
                .iter()
                .filter_map(|reason| cc::model::parse_hex_u32(reason))
                .collect(),
        })
    }

    pub fn camera_identity(&self) -> CameraIdentityInfo {
        let camera = &self.inner.manifest.camera;
        CameraIdentityInfo {
            manufacturer: camera.manufacturer.clone(),
            model: camera.model.clone(),
            firmware: camera.firmware.clone(),
            identities: camera
                .identities
                .iter()
                .map(|(key, value)| KeyValue {
                    key: key.clone(),
                    value: value.clone(),
                })
                .collect(),
        }
    }

    /// The camera's tap-to-focus AF grid (#135), or `None` if it declares none.
    pub fn focus_grid(&self) -> Option<FocusGridInfo> {
        self.inner
            .manifest
            .focus_grid
            .as_ref()
            .map(|g| FocusGridInfo {
                columns: g.columns,
                rows: g.rows,
            })
    }

    /// Resolve a `values:` key to its fixed scalar string (`None` for non-fixed).
    fn fixed_value(&self, key: &str) -> Option<String> {
        match self.inner.value(key)? {
            cc::ValuePolicy::Fixed { value } => yaml_scalar(value),
            _ => None,
        }
    }

    pub fn value_label(&self, prop: u16, value: i64) -> Option<String> {
        self.inner
            .manifest
            .value_label(prop, value)
            .map(String::from)
    }

    /// Decode a raw property value into its manifest presentation row. Exact
    /// rows/labels win; generic sentinel/mask metadata can compose labels such
    /// as `AUTO 6400` from the same table data.
    pub fn decode_property(&self, prop: u16, raw: i64) -> Option<PropertyValueInfo> {
        if let Some(row) = self
            .inner
            .manifest
            .property(prop)?
            .value_rows
            .iter()
            .find(|row| row.raw == raw)
        {
            return Some(PropertyValueInfo::from(row));
        }
        self.inner
            .manifest
            .decode_property_label(prop, raw)
            .map(|label| PropertyValueInfo {
                label,
                raw,
                evidence: Vec::new(),
            })
    }

    /// Encode a property label to wire bytes using the manifest row/sentinel data
    /// and the property's declared width. This is the app-facing replacement for
    /// per-vendor value switch tables.
    pub fn encode_property(&self, prop: u16, label: String) -> Result<Vec<u8>, CodecError> {
        let raw = self
            .inner
            .manifest
            .encode_property_raw(prop, &label)
            .ok_or_else(|| {
                CodecError::Encode(format!(
                    "property 0x{prop:04x} has no encodable label {label:?}"
                ))
            })?;
        let width = self.property_value_width(prop).ok_or_else(|| {
            CodecError::Encode(format!("property 0x{prop:04x} has no encodable width"))
        })?;
        protocol_primitives::encode_value(raw, width.into()).map_err(codec_encode)
    }

    /// Encode a manifest-declared PTP string property. This keeps structured
    /// text values (for example comma-separated coordinate tuples) on the same
    /// generic property path without teaching the FFI about camera-specific
    /// field names or limits.
    pub fn encode_property_text(&self, prop: u16, value: String) -> Result<Vec<u8>, CodecError> {
        let property = self
            .inner
            .manifest
            .property(prop)
            .filter(|property| property.ptype.as_deref() == Some("str"))
            .ok_or_else(|| {
                CodecError::Encode(format!(
                    "property 0x{prop:04x} is not a manifest-declared string"
                ))
            })?;
        let _ = property;
        let mut writer = ptp_core::Writer::new();
        writer.ptp_string(&value).map_err(codec_encode)?;
        Ok(writer.into_vec())
    }

    /// Encode integer fields according to a manifest-declared structured-text
    /// grammar. Field count and separators are data; numeric bounds remain
    /// unspecified until supported by evidence.
    pub fn encode_structured_integer_property(
        &self,
        prop: u16,
        values: Vec<i64>,
    ) -> Result<Vec<u8>, CodecError> {
        let property = self
            .inner
            .manifest
            .property(prop)
            .filter(|property| property.ptype.as_deref() == Some("str"))
            .ok_or_else(|| {
                CodecError::Encode(format!(
                    "property 0x{prop:04x} is not a manifest-declared string"
                ))
            })?;
        let layout = property.structured_text.as_ref().ok_or_else(|| {
            CodecError::Encode(format!(
                "property 0x{prop:04x} has no structured-text grammar"
            ))
        })?;
        if values.len() != layout.fields.len()
            || layout
                .fields
                .iter()
                .any(|field| field.scalar != cc::StructuredTextScalar::SignedInteger)
        {
            return Err(CodecError::Encode(format!(
                "property 0x{prop:04x} requires {} signed integer fields",
                layout.fields.len()
            )));
        }
        self.encode_property_text(
            prop,
            values
                .iter()
                .map(i64::to_string)
                .collect::<Vec<_>>()
                .join(&layout.delimiter),
        )
    }

    /// The encoder width for a property, resolved from the manifest's `type`
    /// (`u8`→U8, `u16`→U16, `u32`→U32, `i16`→I16, `i32`→I32). `None` for an
    /// unknown property or an unsupported type (e.g. `u8a`) — pair with
    /// `encode_value(raw, width)`.
    pub fn property_value_width(&self, prop: u16) -> Option<ValueWidth> {
        match self.inner.manifest.property(prop)?.ptype.as_deref() {
            Some("u8") => Some(ValueWidth::U8),
            Some("u16") => Some(ValueWidth::U16),
            Some("u32") => Some(ValueWidth::U32),
            Some("i16") => Some(ValueWidth::I16),
            Some("i32") => Some(ValueWidth::I32),
            _ => None,
        }
    }

    /// The composite-payload layout for a property — the `0xD212` record-stream
    /// framing plus its member poll allowlist — or `None` for a scalar property.
    /// Lets the app walk the live-status bundle without re-implementing the
    /// Fuji-specific parse.
    pub fn property_payload(&self, prop: u16) -> Option<PayloadInfo> {
        self.inner
            .manifest
            .property(prop)?
            .payload
            .as_ref()
            .map(PayloadInfo::from)
    }

    /// Enumerate the full property catalog (#50) — every declared property's
    /// code, name, type, access, allowed value set, and value labels — so the
    /// app presents settings without hardcoding a per-vendor catalog. The point
    /// lookups (property_value_width, value_label, control_for, property_payload)
    /// remain for targeted queries.
    pub fn properties(&self) -> Vec<PropertyInfo> {
        self.inner
            .manifest
            .properties
            .iter()
            .map(|(code, p)| {
                let semantic = self.inner.manifest.semantic_assertions.properties.get(code);
                PropertyInfo {
                    code: parse_hex_code(code).expect("property map key validated at store load"),
                    name: p.name.clone(),
                    ptp_name: p.ptp_name.clone(),
                    ptype: p.ptype.clone(),
                    access: p.access.clone(),
                    initial_value: p.initial_value,
                    kind: p.kind.into(),
                    observed_scopes: p.observed_scopes.iter().map(Into::into).collect(),
                    descriptor_form: p.descriptor.as_ref().map(|d| d.form.clone()),
                    descriptor_source: p.descriptor.as_ref().and_then(|d| d.source).map(Into::into),
                    evidence: p.evidence.clone(),
                    values: p
                        .descriptor
                        .as_ref()
                        .map(|d| {
                            d.values
                                .iter()
                                .filter_map(cc::DescriptorValue::as_i64)
                                .collect()
                        })
                        .unwrap_or_default(),
                    labels: p
                        .labels
                        .iter()
                        .map(|(k, v)| KeyValue {
                            key: k.clone(),
                            value: v.clone(),
                        })
                        .collect(),
                    value_rows: p.value_rows.iter().map(PropertyValueInfo::from).collect(),
                    value_profiles: p
                        .value_profiles
                        .iter()
                        .map(PropertyValueProfileInfo::from)
                        .collect(),
                    value_encoding: p
                        .value_encoding
                        .as_ref()
                        .map(PropertyValueEncodingInfo::from),
                    structured_text: p
                        .structured_text
                        .as_ref()
                        .map(StructuredTextLayoutInfo::from),
                    canonical_name_provenance: semantic
                        .and_then(|assertions| assertions.canonical_name.as_ref())
                        .map(|name| name.provenance.iter().map(Into::into).collect())
                        .unwrap_or_default(),
                    source_native_name_provenance: semantic
                        .and_then(|assertions| assertions.source_native_name.as_ref())
                        .map(|name| name.provenance.iter().map(Into::into).collect())
                        .unwrap_or_default(),
                    semantic_value_rows: semantic
                        .map(|assertions| {
                            assertions
                                .value_rows
                                .iter()
                                .map(SemanticPropertyValueInfo::from)
                                .collect()
                        })
                        .unwrap_or_default(),
                    semantic_value_profiles: semantic
                        .map(|assertions| {
                            assertions
                                .value_profiles
                                .iter()
                                .map(SemanticPropertyValueProfileInfo::from)
                                .collect()
                        })
                        .unwrap_or_default(),
                }
            })
            .collect()
    }

    /// Enumerate the full operation catalog. Inventory-only rows remain
    /// queryable but carry `advertisedOnly`, and availability resolution will
    /// never promote them to executable.
    pub fn operations(&self) -> Vec<OperationInfo> {
        self.inner
            .manifest
            .operations
            .iter()
            .map(|(code, operation)| {
                let semantic = self.inner.manifest.semantic_assertions.operations.get(code);
                OperationInfo {
                    code: parse_hex_code(code).expect("operation map key validated at store load"),
                    name: operation.name.clone(),
                    kind: operation.kind.into(),
                    observed_scopes: operation.observed_scopes.iter().map(Into::into).collect(),
                    evidence: operation.evidence.clone(),
                    canonical_name_provenance: semantic
                        .and_then(|assertions| assertions.canonical_name.as_ref())
                        .map(|name| name.provenance.iter().map(Into::into).collect())
                        .unwrap_or_default(),
                }
            })
            .collect()
    }

    /// Classify an object-format code from the manifest media table (#36) — name,
    /// vendor, and RAW/movie flags — so the app holds no per-vendor format
    /// literals. `None` if the format is not in the table.
    pub fn media_format(&self, code: u16) -> Option<MediaFormatInfo> {
        let media = self.inner.manifest.media.as_ref()?;
        let f = media
            .formats
            .iter()
            .find(|(k, _)| parse_hex_code(k) == Some(code))
            .map(|(_, f)| f)?;
        Some(MediaFormatInfo {
            code,
            name: f.name.clone(),
            vendor: f.vendor.clone(),
            is_raw: f.is_raw,
            is_movie: f.is_movie,
            is_photos_compatible: f.is_photos_compatible,
            embedded_jpeg: f.embedded_jpeg.as_ref().map(|e| EmbeddedJpegInfo {
                magic: e.magic.clone(),
                offset_at: e.offset_at,
                length_at: e.length_at,
                big_endian: matches!(e.endian, cc::model::Endian::Big),
            }),
        })
    }

    /// Reported `ObjectInfo.ObjectCompressedSize` sentinel for oversized objects.
    /// `None` if the camera declares no such sentinel.
    pub fn object_info_size_sentinel(&self) -> Option<u64> {
        self.inner
            .manifest
            .media
            .as_ref()?
            .object_info_size_sentinel
    }

    /// Deprecated compatibility alias for the former name. The value is a
    /// reported-size sentinel, not a transfer prohibition.
    pub fn wireless_transfer_ceiling(&self) -> Option<u64> {
        self.object_info_size_sentinel()
    }
}

fn flattened_establishment_params(
    extra: &BTreeMap<String, serde_yaml::Value>,
) -> Vec<(String, String)> {
    let mut params = Vec::new();
    for block in ["knock", "gatt"] {
        if let Some(serde_yaml::Value::Mapping(m)) = extra.get(block) {
            for (k, v) in m {
                if let (Some(k), Some(v)) = (k.as_str(), yaml_scalar(v)) {
                    params.push((k.to_string(), v));
                }
            }
        }
    }
    params
}

// ----------------------------------------------------------------------------
// helpers
// ----------------------------------------------------------------------------

fn refine_establishment_native(
    store: &ConfigStore,
    plan_handle: String,
    firmware: String,
    scope: Vec<KeyValue>,
    next_step_index: u32,
) -> Result<NativeEstablishmentRefinement, EstablishmentError> {
    let (model, selector) = plan_handle
        .split_once(':')
        .ok_or_else(|| EstablishmentError::InvalidPlanHandle(plan_handle.clone()))?;
    if model.is_empty() || selector.is_empty() || selector.contains(':') {
        return Err(EstablishmentError::InvalidPlanHandle(plan_handle));
    }

    let Some(index) = &store.inner.index else {
        return Err(EstablishmentError::UnknownPlan(format!(
            "{model}:{selector}: store has no manufacturer index"
        )));
    };
    let Some(body) = store.inner.body(model) else {
        return Err(EstablishmentError::UnknownPlan(format!(
            "{model}:{selector}: unknown model"
        )));
    };
    let mechanism = match body.connections.get(selector) {
        Some(connection) => connection.establishment.clone().ok_or_else(|| {
            EstablishmentError::UnknownPlan(format!(
                "{model}:{selector}: connection has no establishment"
            ))
        })?,
        None => selector.to_string(),
    };
    let Some(plan) = mfg_index::build_establishment(index, model, selector, &mechanism, &scope)
    else {
        return Err(EstablishmentError::UnknownPlan(format!(
            "{model}:{selector}: missing mechanism {mechanism}"
        )));
    };
    if next_step_index as usize > plan.steps.len() {
        return Err(EstablishmentError::InvalidNextStepIndex(format!(
            "{model}:{selector}: next_step_index {next_step_index} > plan length {}",
            plan.steps.len()
        )));
    }

    let _ = firmware;
    Ok(NativeEstablishmentRefinement::NoChange)
}

fn refinement_to_ffi(native: NativeEstablishmentRefinement) -> EstablishmentRefinement {
    match native {
        NativeEstablishmentRefinement::NoChange => EstablishmentRefinement::NoChange,
        NativeEstablishmentRefinement::ReplaceTail { steps, activities } => {
            EstablishmentRefinement::ReplaceTail {
                steps: steps
                    .iter()
                    .map(Step::try_from)
                    .collect::<Result<_, _>>()
                    .expect("BLE plans validated at store load"),
                activities: activities.iter().map(Into::into).collect(),
            }
        }
    }
}

#[cfg(test)]
mod refinement_tests {
    use super::*;

    #[test]
    fn replacement_tail_mirrors_steps_and_relative_spans() {
        let activity: camera_config::ConnectionActivityDescriptor = serde_yaml::from_str(
            r#"
id: camera.test.refined
version: 1
displayRole: preparingConnection
defaultExpectedDurationMs: 1
interactionRequired: false
executorSpan: { sequence: steps, startStep: 0, endStepExclusive: 1 }
"#,
        )
        .unwrap();
        let ffi = refinement_to_ffi(NativeEstablishmentRefinement::ReplaceTail {
            steps: vec![camera_config::index::Step::BleConnect(
                camera_config::index::BleConnectStep {
                    opts: camera_config::index::StepOptions::default(),
                },
            )],
            activities: vec![activity],
        });
        match ffi {
            EstablishmentRefinement::ReplaceTail { steps, activities } => {
                assert!(matches!(steps.as_slice(), [Step::BleConnect { .. }]));
                assert!(matches!(
                    activities[0].binding,
                    ConnectionActivityBinding::ExecutorSpan {
                        sequence: ConnectionActivitySequence::Steps,
                        start_step: 0,
                        end_step_exclusive: 1,
                    }
                ));
            }
            EstablishmentRefinement::NoChange => panic!("expected replacement"),
        }
    }
}

fn build_store(
    m: cc::CameraManifest,
    manufacturer_yaml: Option<String>,
) -> Result<Arc<ConfigStore>, ConfigError> {
    m.require_supported_schema()
        .map_err(|e| ConfigError::Schema(e.to_string()))?;
    validate_catalog_codes(&m)?;
    validate_mode_entry_mappings(&m)?;
    let mut store = cc::ConfigStore::new(m);
    if let Some(my) = manufacturer_yaml {
        let d = cc::ManufacturerDefaults::from_yaml(&my)
            .map_err(|e| ConfigError::Parse(e.to_string()))?;
        store = store.with_manufacturer(d);
    }
    validate_fixed_values(&store)?;
    validate_resolved_init_shapes(&store)?;
    Ok(Arc::new(ConfigStore { inner: store }))
}

fn build_manufacturer_index_store(
    index_yaml: String,
    model_bodies: Vec<KeyValue>,
    manufacturer_yaml: Option<String>,
) -> Result<Arc<ConfigStore>, ConfigError> {
    let bodies: BTreeMap<String, String> = model_bodies
        .into_iter()
        .map(|kv| (kv.key, kv.value))
        .collect();
    let inner = cc::ConfigStore::from_manufacturer_index(&index_yaml, bodies)?;
    // Unwrap the Arc<cc::ConfigStore> into a fresh FFI ConfigStore. The
    // inner Arc is private to camera-config; here we own the FFI-level store.
    let mut inner = Arc::try_unwrap(inner).unwrap_or_else(|arc| (*arc).clone());
    let index = inner
        .index
        .as_ref()
        .expect("manufacturer index store has a resolved index");
    mfg_index::validate_ble_plan_mappings(index)?;
    let manufacturer = manufacturer_yaml
        .map(|yaml| {
            cc::ManufacturerDefaults::from_yaml(&yaml)
                .map_err(|error| ConfigError::Parse(error.to_string()))
        })
        .transpose()?;

    for body in inner.bodies.values() {
        validate_catalog_codes(body)?;
        validate_mode_entry_mappings(body)?;
        let mut resolved = cc::ConfigStore::new(body.clone());
        if let Some(defaults) = &manufacturer {
            resolved = resolved.with_manufacturer(defaults.clone());
        }
        validate_fixed_values(&resolved)?;
        validate_resolved_init_shapes(&resolved)?;
    }
    if let Some(defaults) = manufacturer {
        inner = inner.with_manufacturer(defaults);
    }
    Ok(Arc::new(ConfigStore { inner }))
}

fn validate_fixed_values(store: &cc::ConfigStore) -> Result<(), ConfigError> {
    let manufacturer_values = store
        .manufacturer
        .as_ref()
        .into_iter()
        .flat_map(|defaults| defaults.values.iter());
    for (key, policy) in store.manifest.values.iter().chain(manufacturer_values) {
        if let cc::ValuePolicy::Fixed { value } = policy {
            if yaml_scalar(value).is_none() {
                return Err(ConfigError::Contract(format!(
                    "values.{key}: fixed value is not a scalar"
                )));
            }
        }
    }
    Ok(())
}

fn validate_resolved_init_shapes(store: &cc::ConfigStore) -> Result<(), ConfigError> {
    for (connection_id, connection) in &store.manifest.connections {
        let Some(shape @ ("app82" | "legacyApp82" | "standardPtpIp")) =
            connection.init_shape.as_deref()
        else {
            continue;
        };
        let init = match connection.init.as_ref() {
            Some(init) => init,
            None if shape == "app82" => continue,
            None => {
                return Err(ConfigError::Contract(format!(
                    "connections.{connection_id}.init is required for {shape}"
                )))
            }
        };
        let path = format!("connections.{connection_id}.init");
        if shape == "app82"
            && store.value(&init.identity.guid).is_none()
            && store.value(&init.identity.friendly_name).is_none()
        {
            // Body-only stores may intentionally omit the manufacturer tier
            // that owns both reference app identity policies. Validate once that tier is
            // present; a partially resolved identity still fails below.
            continue;
        }
        let resolve = |key: &str, field: &str| {
            store.value(key).ok_or_else(|| {
                ConfigError::Contract(format!("{path}.{field} references unknown value '{key}'"))
            })
        };

        let guid = resolve(&init.identity.guid, "identity.guid")?;
        let cc::ValuePolicy::Fixed {
            value: serde_yaml::Value::String(guid),
        } = guid
        else {
            return Err(ConfigError::Contract(format!(
                "{path}.identity.guid must resolve to fixed hexadecimal bytes"
            )));
        };
        if cc::parse_hex_bytes(guid).is_none_or(|bytes| bytes.len() != 16) {
            return Err(ConfigError::Contract(format!(
                "{path}.identity.guid must resolve to exactly 16 bytes"
            )));
        }

        let friendly = resolve(&init.identity.friendly_name, "identity.friendlyName")?;
        match friendly {
            cc::ValuePolicy::Fixed { value } => {
                let name = yaml_scalar(value).ok_or_else(|| {
                    ConfigError::Contract(format!(
                        "{path}.identity.friendlyName fixed value must be scalar"
                    ))
                })?;
                let max_name_units = if shape == "standardPtpIp" { 254 } else { 26 };
                if name.contains('\0') || name.encode_utf16().count() > max_name_units {
                    return Err(ConfigError::Contract(format!(
                        "{path}.identity.friendlyName must fit {max_name_units} UTF-16 units and contain no NUL"
                    )));
                }
            }
            cc::ValuePolicy::ClientDerived { runtime } if !runtime.trim().is_empty() => {}
            _ => {
                return Err(ConfigError::Contract(format!(
                    "{path}.identity.friendlyName must resolve to fixed or client-derived text"
                )))
            }
        }

        if matches!(shape, "standardPtpIp" | "app82") {
            continue;
        }

        let client_ip_key = init
            .identity
            .client_ipv4
            .as_deref()
            .expect("legacyApp82 structural validation requires clientIpv4");
        let client_ip = resolve(client_ip_key, "identity.clientIpv4")?;
        match client_ip {
            cc::ValuePolicy::Fixed { value } => {
                let address = yaml_scalar(value).ok_or_else(|| {
                    ConfigError::Contract(format!(
                        "{path}.identity.clientIpv4 fixed value must be scalar"
                    ))
                })?;
                address.parse::<std::net::Ipv4Addr>().map_err(|_| {
                    ConfigError::Contract(format!(
                        "{path}.identity.clientIpv4 fixed value is not an IPv4 address"
                    ))
                })?;
            }
            cc::ValuePolicy::ClientDerived { runtime } if !runtime.trim().is_empty() => {}
            _ => {
                return Err(ConfigError::Contract(format!(
                "{path}.identity.clientIpv4 must resolve to a fixed or client-derived IPv4 value"
            )))
            }
        }

        let responder_key = init
            .expected_responder_guid
            .as_deref()
            .expect("legacyApp82 structural validation requires expectedResponderGuid");
        let responder = resolve(responder_key, "expectedResponderGuid")?;
        let cc::ValuePolicy::Fixed {
            value: serde_yaml::Value::String(responder),
        } = responder
        else {
            return Err(ConfigError::Contract(format!(
                "{path}.expectedResponderGuid must resolve to fixed hexadecimal bytes"
            )));
        };
        if cc::parse_hex_bytes(responder).is_none_or(|bytes| bytes.len() != 16) {
            return Err(ConfigError::Contract(format!(
                "{path}.expectedResponderGuid must resolve to exactly 16 bytes"
            )));
        }
    }
    Ok(())
}

fn validate_catalog_codes(manifest: &cc::CameraManifest) -> Result<(), ConfigError> {
    for code in manifest.properties.keys() {
        if cc::parse_hex_code(code).is_none() {
            return Err(ConfigError::Contract(format!(
                "properties: map key `{code}` is not a hex property code"
            )));
        }
    }
    for code in manifest.operations.keys() {
        if cc::parse_hex_code(code).is_none() {
            return Err(ConfigError::Contract(format!(
                "operations: map key `{code}` is not a hex operation code"
            )));
        }
    }
    Ok(())
}

fn validate_mode_entry_mappings(manifest: &cc::CameraManifest) -> Result<(), ConfigError> {
    for (connection, definition) in &manifest.connections {
        for (index, entry) in definition.entries.iter().enumerate() {
            let context = format!("mode entry {connection}[{index}]");
            if let Some(requires) = &entry.requires {
                FfiPredicate::try_from(requires)?;
            }
            match &entry.execution {
                cc::ModeEntryExecution::Ptp { steps } => {
                    try_map_steps(steps, &context)?;
                }
                cc::ModeEntryExecution::ReestablishConnection(reestablish) => {
                    try_map_steps(&reestablish.exit_steps, &format!("{context} exitSteps"))?;
                }
                cc::ModeEntryExecution::UserInstruction { .. } => {}
            }
        }
        for transition in &definition.enables {
            if let Some(requires) = &transition.requires {
                FfiPredicate::try_from(requires)?;
            }
        }
    }
    Ok(())
}

fn prop_view(observed: &[PropObservation]) -> cc::PropView {
    observed.iter().map(|p| (p.code, p.value)).collect()
}

fn map_camera_initiated_transfer(
    transfer: &cc::ResolvedCameraInitiatedTransfer,
) -> CameraInitiatedTransferInfo {
    let match_mode = match transfer.trigger_match {
        cc::TriggerMatch::All => CameraInitiatedTriggerMatch::All,
    };
    let completion = match transfer.completion {
        cc::TransferCompletion::ReadToEof => CameraInitiatedCompletion::ReadToEof,
    };
    CameraInitiatedTransferInfo {
        trigger: CameraInitiatedTriggerInfo {
            match_mode,
            states: transfer
                .trigger_states
                .iter()
                .map(|state| BleStateTriggerInfo {
                    gatt_uuid: state.gatt_uuid.clone(),
                    trigger_values: state.trigger_values.clone(),
                    baseline_values: state.baseline_values.clone(),
                })
                .collect(),
        },
        handoff: CameraInitiatedHandoffInfo {
            connection: transfer.connection.clone(),
            socket_role: transfer.socket_role.into(),
            endpoint_host: transfer.endpoint_host.clone(),
            endpoint_port: transfer.endpoint_port,
            cached_credentials_allowed: transfer.cached_credentials_allowed,
            function_launch: transfer
                .function_launch
                .as_ref()
                .map(|launch| BleLiteralWriteInfo {
                    gatt_uuid: launch.gatt_uuid.clone(),
                    value: launch.value.clone(),
                    required: launch.required,
                }),
        },
        monitor_recovery: transfer.monitor_recovery.map(|recovery| match recovery {
            cc::CameraInitiatedMonitorRecovery::SavedCameraReconnect => {
                CameraInitiatedMonitorRecovery::SavedCameraReconnect
            }
        }),
        receive: CameraInitiatedReceiveInfo {
            mode: transfer.mode.clone(),
            count_property: transfer.count_property,
            count_member: transfer.count_member,
            head_index: transfer.head_index,
            metadata_operation: transfer.metadata_operation,
            metadata_phases: transfer
                .metadata_phases
                .iter()
                .map(|phase| match phase {
                    cc::model::CameraInitiatedMetadataPhase::AfterCountBeforeModeEntry => {
                        CameraInitiatedMetadataPhase::AfterCountBeforeModeEntry
                    }
                    cc::model::CameraInitiatedMetadataPhase::AfterModeEntry => {
                        CameraInitiatedMetadataPhase::AfterModeEntry
                    }
                })
                .collect(),
            data_operation: transfer.data_operation,
            chunk_limit_property: transfer.chunk_limit_property,
            completion,
        },
        evidence: transfer.evidence.clone(),
    }
}

fn map_step(s: &cc::Step) -> Option<EntryStep> {
    let tolerant = s.tolerant;
    if let Some(p) = &s.set_prop {
        let prop = parse_hex_code(p)?;
        return Some(match s.value.as_ref() {
            None => EntryStep::SetProp {
                prop,
                value: 0,
                tolerant,
            },
            Some(cc::SetPropValue::Literal(value)) => EntryStep::SetProp {
                prop,
                value: *value,
                tolerant,
            },
            Some(cc::SetPropValue::Runtime(reference)) => EntryStep::SetPropRuntime {
                prop,
                slot: reference.runtime.clone(),
                if_missing: match reference.if_missing {
                    cc::MissingRuntimeValue::Error => FfiMissingRuntimeValue::Error,
                    cc::MissingRuntimeValue::Skip => FfiMissingRuntimeValue::Skip,
                },
                tolerant,
            },
        });
    }
    if let Some(p) = &s.get_prop {
        return Some(EntryStep::GetProp {
            prop: parse_hex_code(p)?,
            captures: s.captures.iter().map(map_capture).collect(),
            tolerant,
        });
    }
    if let Some(p) = &s.read_echo {
        return Some(EntryStep::ReadEcho {
            prop: parse_hex_code(p)?,
            captures: s.captures.iter().map(map_capture).collect(),
            tolerant,
        });
    }
    if let Some(o) = &s.send_op {
        return Some(EntryStep::SendOp {
            op: parse_hex_code(o)?,
            params: s.params.iter().map(map_param).collect(),
            captures: s.captures.iter().map(map_capture).collect(),
            repeat: s.repeat,
            tolerant,
        });
    }
    if let Some(role) = s.open_channel {
        return Some(EntryStep::OpenChannel {
            role: role.into(),
            tolerant,
        });
    }
    if s.reopen_session.is_some() {
        return Some(EntryStep::ReopenSession { tolerant });
    }
    if let Some(cs) = &s.close_session {
        return Some(EntryStep::CloseSession {
            transport_close: cs.transport_close,
            tolerant,
        });
    }
    if let Some(aw) = &s.await_until {
        let source = match &aw.source {
            cc::AwaitSource::Poll { prop } => FfiAwaitSource::Poll {
                prop: parse_hex_code(prop)?,
            },
            cc::AwaitSource::Event { code, then_poll } => FfiAwaitSource::Event {
                code: parse_hex_code(code)?,
                // A present-but-malformed thenPoll `?`-drops the whole step
                // (same hazard as a bad source — guarded by the seam test).
                then_poll: match then_poll {
                    Some(tp) => Some(parse_hex_code(tp)?),
                    None => None,
                },
            },
        };
        return Some(EntryStep::AwaitUntil {
            source,
            until: FfiPredicate::try_from(&aw.until).ok()?,
            on_each: aw.on_each.iter().map(map_step).collect::<Option<_>>()?,
            captures: s.captures.iter().map(map_capture).collect(),
            timeout_ms: aw.timeout_ms,
            interval_ms: aw.interval_ms,
            tolerant,
        });
    }
    if let Some(retry) = &s.retry {
        return Some(EntryStep::Retry {
            steps: retry.steps.iter().map(map_step).collect::<Option<_>>()?,
            when_response_codes: retry
                .when_response_codes
                .iter()
                .map(|code| parse_hex_code(code))
                .collect::<Option<Vec<_>>>()?,
            when_failure_classes: retry
                .when_failure_classes
                .iter()
                .map(|class| match class {
                    cc::RetryFailureClass::Decode => FfiRetryFailureClass::Decode,
                })
                .collect(),
            max_attempts: retry.max_attempts,
            retry_delay_ms: retry.retry_delay_ms,
            tolerant,
        });
    }
    if let Some(lp) = &s.r#loop {
        let kind = match lp {
            cc::Loop::ForEach {
                collection,
                bind,
                body,
            } => FfiLoopKind::ForEach {
                collection: collection.clone(),
                bind: bind.clone(),
                body: body.iter().map(map_step).collect::<Option<_>>()?,
            },
            cc::Loop::Chunk {
                total,
                size,
                offset_bind,
                length_bind,
                body,
            } => FfiLoopKind::Chunk {
                total: total.clone(),
                size: map_chunk_size(size),
                offset_bind: offset_bind.clone(),
                length_bind: length_bind.clone(),
                body: body.iter().map(map_step).collect::<Option<_>>()?,
            },
        };
        return Some(EntryStep::Loop { kind, tolerant });
    }
    if let Some(cond) = &s.if_step {
        let slot = cond.slot.clone();
        let equals = cond.equals;
        let then_steps = cond
            .then_steps
            .iter()
            .map(map_step)
            .collect::<Option<_>>()?;
        return if cond.else_steps.is_empty() {
            Some(EntryStep::If {
                slot,
                equals,
                then_steps,
                tolerant,
            })
        } else {
            Some(EntryStep::IfElse {
                slot,
                equals,
                then_steps,
                else_steps: cond
                    .else_steps
                    .iter()
                    .map(map_step)
                    .collect::<Option<_>>()?,
                tolerant,
            })
        };
    }
    None
}

fn map_param(p: &cc::StepParam) -> EntryParam {
    match p {
        cc::StepParam::Literal(v) => EntryParam::Literal { value: *v },
        cc::StepParam::Runtime {
            runtime,
            shift,
            mask,
        } => EntryParam::Runtime {
            slot: runtime.clone(),
            shift: *shift,
            mask: *mask,
        },
    }
}

fn map_capture(c: &cc::model::Capture) -> CaptureInfo {
    CaptureInfo {
        bind: c.bind.clone(),
        source: match c.source {
            cc::model::CaptureSource::ObjectInfoCompressedSize => {
                CaptureSourceInfo::ObjectInfoCompressedSize
            }
            cc::model::CaptureSource::PropValue => CaptureSourceInfo::PropValue,
            cc::model::CaptureSource::U32Le => CaptureSourceInfo::U32Le,
            cc::model::CaptureSource::U64Le => CaptureSourceInfo::U64Le,
            cc::model::CaptureSource::PtpU32Array => CaptureSourceInfo::PtpU32Array,
            cc::model::CaptureSource::TransactionId => CaptureSourceInfo::TransactionId,
        },
    }
}

fn map_chunk_size(size: &cc::model::ChunkSize) -> FfiChunkSize {
    match size {
        cc::model::ChunkSize::Literal(value) => FfiChunkSize::Literal { value: *value },
        cc::model::ChunkSize::Runtime { runtime } => FfiChunkSize::Runtime {
            slot: runtime.clone(),
        },
    }
}

fn ffi_to_cc_verb(v: ActionVerb) -> cc::ActionVerb {
    match v {
        ActionVerb::Shutter => cc::ActionVerb::Shutter,
        ActionVerb::EnumerateObjects => cc::ActionVerb::EnumerateObjects,
        ActionVerb::GetObjectInfo => cc::ActionVerb::GetObjectInfo,
        ActionVerb::GetThumb => cc::ActionVerb::GetThumb,
        ActionVerb::GetObject => cc::ActionVerb::GetObject,
        ActionVerb::DeleteObject => cc::ActionVerb::DeleteObject,
        ActionVerb::AutofocusLock => cc::ActionVerb::AutofocusLock,
        ActionVerb::AutofocusRelease => cc::ActionVerb::AutofocusRelease,
        ActionVerb::ImportObjects => cc::ActionVerb::ImportObjects,
        ActionVerb::ReadDeviceInfo => cc::ActionVerb::ReadDeviceInfo,
        ActionVerb::StartLiveView => cc::ActionVerb::StartLiveView,
        ActionVerb::PollLiveView => cc::ActionVerb::PollLiveView,
        ActionVerb::StopLiveView => cc::ActionVerb::StopLiveView,
        ActionVerb::Keepalive => cc::ActionVerb::Keepalive,
    }
}

fn cc_to_ffi_verb(v: cc::ActionVerb) -> ActionVerb {
    match v {
        cc::ActionVerb::Shutter => ActionVerb::Shutter,
        cc::ActionVerb::EnumerateObjects => ActionVerb::EnumerateObjects,
        cc::ActionVerb::GetObjectInfo => ActionVerb::GetObjectInfo,
        cc::ActionVerb::GetThumb => ActionVerb::GetThumb,
        cc::ActionVerb::GetObject => ActionVerb::GetObject,
        cc::ActionVerb::DeleteObject => ActionVerb::DeleteObject,
        cc::ActionVerb::AutofocusLock => ActionVerb::AutofocusLock,
        cc::ActionVerb::AutofocusRelease => ActionVerb::AutofocusRelease,
        cc::ActionVerb::ImportObjects => ActionVerb::ImportObjects,
        cc::ActionVerb::ReadDeviceInfo => ActionVerb::ReadDeviceInfo,
        cc::ActionVerb::StartLiveView => ActionVerb::StartLiveView,
        cc::ActionVerb::PollLiveView => ActionVerb::PollLiveView,
        cc::ActionVerb::StopLiveView => ActionVerb::StopLiveView,
        cc::ActionVerb::Keepalive => ActionVerb::Keepalive,
    }
}

fn cc_to_ffi_action_role(role: cc::ActionRole) -> ActionRole {
    match role {
        cc::ActionRole::Initiator => ActionRole::Initiator,
        cc::ActionRole::Responder => ActionRole::Responder,
    }
}

fn ffi_to_cc_action_role(role: ActionRole) -> cc::ActionRole {
    match role {
        ActionRole::Initiator => cc::ActionRole::Initiator,
        ActionRole::Responder => cc::ActionRole::Responder,
    }
}

fn map_responder_mutation(mutation: cc::ResponderMutation) -> ResponderMutation {
    match mutation {
        cc::ResponderMutation::EnqueueObjects { count_param } => {
            ResponderMutation::EnqueueObjects { count_param }
        }
        cc::ResponderMutation::PropertyTransition {
            target,
            initial,
            terminal,
            settle_after_polls,
        } => ResponderMutation::PropertyTransition {
            target: parse_hex_code(&target).expect("property transition target validated at load"),
            initial,
            terminal: match terminal {
                cc::PropertyTransitionTerminal::Fixed { value } => {
                    PropertyTransitionTerminal::Fixed { value }
                }
                cc::PropertyTransitionTerminal::Parameter { parameter } => {
                    PropertyTransitionTerminal::Parameter { parameter }
                }
            },
            settle_after_polls,
        },
    }
}

fn map_action_catalog(catalog: cc::ActionCatalog) -> ActionCatalog {
    ActionCatalog {
        revision: catalog.revision,
        actions: catalog
            .actions
            .into_iter()
            .map(|entry| ActionCatalogEntry {
                action_id: entry.action_id,
                connection: entry.connection,
                mode: entry.mode,
                supported_roles: entry
                    .supported_roles
                    .into_iter()
                    .map(cc_to_ffi_action_role)
                    .collect(),
                parameters: entry
                    .parameters
                    .into_iter()
                    .map(|role| ActionRoleParameters {
                        role: cc_to_ffi_action_role(role.role),
                        parameters: role
                            .parameters
                            .into_iter()
                            .map(|parameter| ActionCatalogParameter {
                                name: parameter.name,
                                kind: match parameter.kind {
                                    cc::ActionCatalogParameterKind::U32 => {
                                        ActionCatalogParameterKind::U32
                                    }
                                    cc::ActionCatalogParameterKind::U64 => {
                                        ActionCatalogParameterKind::U64
                                    }
                                    cc::ActionCatalogParameterKind::String => {
                                        ActionCatalogParameterKind::String
                                    }
                                },
                                required: parameter.required,
                                default: parameter.default,
                                min: parameter.min,
                                max: parameter.max,
                            })
                            .collect(),
                    })
                    .collect(),
                triggers: entry
                    .triggers
                    .iter()
                    .map(map_action_effect)
                    .collect::<Result<_, _>>()
                    .expect("catalog actions validated at store load"),
                available: matches!(entry.availability, cc::ActionAvailability::Available),
            })
            .collect(),
    }
}

fn map_resolved_action(resolved: cc::ResolvedActionInvocation) -> ResolvedActionInvocation {
    ResolvedActionInvocation {
        action: cc_to_ffi_verb(resolved.action),
        role: cc_to_ffi_action_role(resolved.role),
        parameters: resolved
            .parameters
            .into_iter()
            .map(|(name, value)| ActionArgument {
                name,
                value: match value {
                    cc::ActionArgumentValue::U64(value) => ActionValue::U64 { value },
                    cc::ActionArgumentValue::String(value) => ActionValue::String { value },
                },
            })
            .collect(),
        responder_mutation: resolved.responder_mutation.map(map_responder_mutation),
    }
}

impl From<cc::ControlRole> for ControlRole {
    fn from(value: cc::ControlRole) -> Self {
        match value {
            cc::ControlRole::Iso => Self::Iso,
            cc::ControlRole::ShutterSpeed => Self::ShutterSpeed,
            cc::ControlRole::Aperture => Self::Aperture,
            cc::ControlRole::ExposureBias => Self::ExposureBias,
            cc::ControlRole::FocusArea => Self::FocusArea,
        }
    }
}

impl From<cc::ControlReadSource> for ControlReadSource {
    fn from(value: cc::ControlReadSource) -> Self {
        match value {
            cc::ControlReadSource::DirectProperty => Self::DirectProperty,
            cc::ControlReadSource::DeclaredReadback => Self::DeclaredReadback,
        }
    }
}

impl From<cc::ControlEvidenceBasis> for ControlEvidenceBasis {
    fn from(value: cc::ControlEvidenceBasis) -> Self {
        match value {
            cc::ControlEvidenceBasis::DescriptorOnly => Self::DescriptorOnly,
            cc::ControlEvidenceBasis::WriteProbe => Self::WriteProbe,
        }
    }
}

impl From<cc::ControlObservedEffect> for ControlObservedEffect {
    fn from(value: cc::ControlObservedEffect) -> Self {
        match value {
            cc::ControlObservedEffect::Confirmed => Self::Confirmed,
            cc::ControlObservedEffect::AckNoEffect => Self::AckNoEffect,
            cc::ControlObservedEffect::ProtocolRefused => Self::ProtocolRefused,
            cc::ControlObservedEffect::DestructiveClamp => Self::DestructiveClamp,
            cc::ControlObservedEffect::Unknown => Self::Unknown,
        }
    }
}

impl From<cc::ControlOwner> for ControlOwner {
    fn from(value: cc::ControlOwner) -> Self {
        match value {
            cc::ControlOwner::Client => Self::Client,
            cc::ControlOwner::Camera => Self::Camera,
            cc::ControlOwner::Body => Self::Body,
            cc::ControlOwner::ModeGated => Self::ModeGated,
            cc::ControlOwner::Unknown => Self::Unknown,
        }
    }
}

impl From<cc::ObjectTransferStrategy> for ObjectTransferStrategy {
    fn from(value: cc::ObjectTransferStrategy) -> Self {
        match value {
            cc::ObjectTransferStrategy::Chunked => Self::Chunked,
            cc::ObjectTransferStrategy::WholeObject => Self::WholeObject,
        }
    }
}

impl From<cc::ObjectTransferResumePolicy> for ObjectTransferResumePolicy {
    fn from(value: cc::ObjectTransferResumePolicy) -> Self {
        match value {
            cc::ObjectTransferResumePolicy::ByteOffset => Self::ByteOffset,
            cc::ObjectTransferResumePolicy::RestartFromZero => Self::RestartFromZero,
        }
    }
}

impl From<cc::ObjectTransferFormatSupport> for ObjectTransferFormatSupport {
    fn from(value: cc::ObjectTransferFormatSupport) -> Self {
        match value {
            cc::ObjectTransferFormatSupport::Confirmed => Self::Confirmed,
            cc::ObjectTransferFormatSupport::Experimental => Self::Experimental,
            cc::ObjectTransferFormatSupport::Unsupported => Self::Unsupported,
        }
    }
}

impl From<cc::ObjectTransferCompletionTiming> for ObjectTransferCompletionTiming {
    fn from(value: cc::ObjectTransferCompletionTiming) -> Self {
        match value {
            cc::ObjectTransferCompletionTiming::LocalCommit => Self::LocalCommit,
        }
    }
}

fn project_selected_object_transfer(
    import: &cc::Action,
    read: &cc::Action,
) -> Result<SelectedObjectTransferInfo, ConfigError> {
    let import = import
        .initiator()
        .ok_or_else(|| ConfigError::Contract("importObjects has no initiator binding".into()))?;
    let (handle_slot, body) = import
        .steps
        .iter()
        .find_map(|step| match &step.r#loop {
            Some(cc::Loop::ForEach { bind, body, .. }) => Some((bind, body)),
            _ => None,
        })
        .ok_or_else(|| {
            ConfigError::Contract("importObjects has no per-handle forEach loop".into())
        })?;
    let chunk_index = body
        .iter()
        .position(|step| matches!(step.r#loop, Some(cc::Loop::Chunk { .. })))
        .ok_or_else(|| {
            ConfigError::Contract("importObjects per-handle loop has no chunk loop".into())
        })?;
    let (transfer_size_slot, chunk_size_slot) = match body[chunk_index].r#loop.as_ref() {
        Some(cc::Loop::Chunk { total, size, .. }) => {
            let cc::model::ChunkSize::Runtime { runtime } = size else {
                return Err(ConfigError::Contract(
                    "importObjects chunk size is not a runtime slot".into(),
                ));
            };
            (total.clone(), runtime.clone())
        }
        _ => unreachable!("chunk_index identifies a chunk loop"),
    };

    let preparation = &body[..chunk_index];
    let object_info_step_index = preparation
        .iter()
        .position(|step| {
            step.captures.iter().any(|capture| {
                matches!(
                    capture.source,
                    cc::model::CaptureSource::ObjectInfoCompressedSize
                ) && capture.bind == transfer_size_slot
            })
        })
        .ok_or_else(|| {
            ConfigError::Contract(
                "importObjects preparation does not capture ObjectInfo size into the chunk total slot"
                    .into(),
            )
        })?;
    let object_info_captures = &preparation[object_info_step_index].captures;
    let has_true_size_fallback = preparation.iter().any(|step| {
        step.if_step.as_ref().is_some_and(|condition| {
            condition.equals == 0xffff_ffff
                && object_info_captures.iter().any(|capture| {
                    capture.bind == condition.slot
                        && matches!(
                            capture.source,
                            cc::model::CaptureSource::ObjectInfoCompressedSize
                        )
                })
                && steps_capture(
                    &condition.then_steps,
                    &transfer_size_slot,
                    cc::model::CaptureSource::U64Le,
                )
        })
    });
    if !has_true_size_fallback {
        return Err(ConfigError::Contract(
            "importObjects preparation has no sentinel-gated u64 true-size capture into the chunk total slot"
                .into(),
        ));
    }
    if !steps_capture(
        preparation,
        &chunk_size_slot,
        cc::model::CaptureSource::PropValue,
    ) {
        return Err(ConfigError::Contract(
            "importObjects preparation does not capture a property value into the runtime chunk-size slot"
                .into(),
        ));
    }
    let preparation_steps = try_map_steps(preparation, "importObjects preparation")?;
    let read = try_map_action(read, "getObject")?;

    Ok(SelectedObjectTransferInfo {
        params: vec![handle_slot.clone()],
        preparation_steps,
        object_info_step_index: object_info_step_index as u32,
        transfer_size_slot,
        chunk_size_slot,
        read,
    })
}

fn steps_capture(steps: &[cc::Step], bind: &str, source: cc::model::CaptureSource) -> bool {
    steps.iter().any(|step| {
        step.captures
            .iter()
            .any(|capture| capture.bind == bind && capture.source == source)
            || step
                .await_until
                .as_ref()
                .is_some_and(|await_until| steps_capture(&await_until.on_each, bind, source))
            || step
                .retry
                .as_ref()
                .is_some_and(|retry| steps_capture(&retry.steps, bind, source))
            || step.r#loop.as_ref().is_some_and(|r#loop| match r#loop {
                cc::Loop::ForEach { body, .. } | cc::Loop::Chunk { body, .. } => {
                    steps_capture(body, bind, source)
                }
            })
            || step.if_step.as_ref().is_some_and(|condition| {
                steps_capture(&condition.then_steps, bind, source)
                    || steps_capture(&condition.else_steps, bind, source)
            })
    })
}

fn try_map_action(a: &cc::Action, context: &str) -> Result<Action, ConfigError> {
    let initiator = a
        .initiator
        .as_ref()
        .map(|binding| -> Result<ActionInitiator, ConfigError> {
            Ok(ActionInitiator {
                params: binding
                    .params
                    .iter()
                    .map(|parameter| {
                        let parameter = parameter.normalized();
                        ActionInitiatorParameter {
                            name: parameter.name,
                            kind: match parameter.kind {
                                cc::ActionInitiatorParameterKind::U64 => {
                                    ActionCatalogParameterKind::U64
                                }
                                cc::ActionInitiatorParameterKind::String => {
                                    ActionCatalogParameterKind::String
                                }
                            },
                            required: parameter.required,
                        }
                    })
                    .collect(),
                steps: try_map_steps(&binding.steps, context)?,
                activities: binding.activities.iter().map(Into::into).collect(),
            })
        })
        .transpose()?;
    let responder = a.responder.as_ref().map(|binding| ActionResponder {
        params: binding
            .params
            .iter()
            .map(|parameter| ActionParameter {
                name: parameter.name.clone(),
                kind: match parameter.kind {
                    cc::ActionParameterKind::U32 => ActionParameterKind::U32,
                },
                default: parameter.default,
                min: parameter.min,
                max: parameter.max,
            })
            .collect(),
        mutation: map_responder_mutation(binding.mutation.clone()),
    });
    Ok(Action {
        mode: a.mode.clone(),
        initiator,
        responder,
        triggers: a
            .triggers
            .iter()
            .map(map_action_effect)
            .collect::<Result<_, _>>()?,
        evidence: a.evidence.clone(),
    })
}

fn try_map_steps(steps: &[cc::Step], context: &str) -> Result<Vec<EntryStep>, ConfigError> {
    for step in steps {
        validate_step_mapping(step, context)?;
    }
    Ok(steps
        .iter()
        .map(|step| map_step(step).expect("validated step must map"))
        .collect())
}

fn validate_step_mapping(step: &cc::Step, context: &str) -> Result<(), ConfigError> {
    if let Some(await_until) = &step.await_until {
        FfiPredicate::try_from(&await_until.until)?;
    }
    if !step.is_well_formed() || map_step(step).is_none() {
        return Err(ConfigError::Contract(format!(
            "{context} contains an unmappable step"
        )));
    }
    if let Some(await_until) = &step.await_until {
        for nested in &await_until.on_each {
            validate_step_mapping(nested, context)?;
        }
    }
    if let Some(retry) = &step.retry {
        for nested in &retry.steps {
            validate_step_mapping(nested, context)?;
        }
    }
    if let Some(r#loop) = &step.r#loop {
        let body = match r#loop {
            cc::Loop::ForEach { body, .. } | cc::Loop::Chunk { body, .. } => body,
        };
        for nested in body {
            validate_step_mapping(nested, context)?;
        }
    }
    if let Some(condition) = &step.if_step {
        for nested in condition
            .then_steps
            .iter()
            .chain(condition.else_steps.iter())
        {
            validate_step_mapping(nested, context)?;
        }
    }
    Ok(())
}

fn map_action(a: &cc::Action) -> Action {
    try_map_action(a, "validated action").expect("actions validated at store load")
}

/// Translate camera-config's flat-struct `ActionEffect` (one optional
/// field per variant) to the FFI's tagged-enum form.
fn map_action_effect(e: &cc::ActionEffect) -> Result<ActionEffect, ConfigError> {
    if !e.is_well_formed() {
        return Err(ConfigError::Contract(
            "action trigger must contain exactly one variant".into(),
        ));
    }
    if let Some(ip) = &e.objects_available {
        return Ok(ActionEffect::ObjectsAvailable {
            min: ip.min,
            max: ip.max,
        });
    }
    if e.postview_event.is_some() {
        return Ok(ActionEffect::PostviewEvent);
    }
    if e.live_view_stream.is_some() {
        return Ok(ActionEffect::LiveViewStream);
    }
    Err(ConfigError::Contract(
        "action contains an unmappable trigger".into(),
    ))
}

fn platform_ok(c: &cc::Connection, p: &Platform) -> bool {
    match c.extra.get("platforms") {
        Some(serde_yaml::Value::Sequence(seq)) => {
            seq.iter().any(|v| v.as_str() == Some(p.as_str()))
        }
        _ => true, // no restriction declared
    }
}

/// Decode an even-length hex string (optionally `0x`-prefixed) to bytes —
/// matches `index::eval::yaml_literal_to_bytes`'s hex path, for the init GUID
/// and responder identity.
fn hex_value(s: &str) -> Option<Vec<u8>> {
    let p = s.strip_prefix("0x").unwrap_or(s);
    if p.is_empty() || !p.len().is_multiple_of(2) || !p.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    (0..p.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&p[i..i + 2], 16).ok())
        .collect()
}

fn standard_ptpip_init(guid: &[u8], friendly_name: &str) -> Option<Vec<u8>> {
    let initiator_guid: [u8; 16] = guid.try_into().ok()?;
    ptp_core::encode(&ptp_core::PtpIpPacket::InitCommandRequest(
        ptp_core::InitCommandRequest {
            initiator_guid,
            friendly_name: friendly_name.to_string(),
            protocol_version: 0x0001_0000,
        },
    ))
    .ok()
}

fn yaml_scalar(v: &serde_yaml::Value) -> Option<String> {
    match v {
        serde_yaml::Value::String(s) => Some(s.clone()),
        serde_yaml::Value::Number(n) => Some(n.to_string()),
        serde_yaml::Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

fn value_with_runtime(
    store: &cc::ConfigStore,
    key: &str,
    runtime_scope: &BTreeMap<String, String>,
) -> Option<String> {
    match store.value(key)? {
        cc::ValuePolicy::Fixed { value } => yaml_scalar(value),
        cc::ValuePolicy::ClientDerived { runtime } => runtime_scope.get(runtime).cloned(),
        _ => None,
    }
}

fn yaml_path_str(
    extra: &std::collections::BTreeMap<String, serde_yaml::Value>,
    path: &[&str],
) -> Option<String> {
    yaml_path(extra, path).and_then(|v| v.as_str().map(String::from))
}

fn yaml_path_bool(
    extra: &std::collections::BTreeMap<String, serde_yaml::Value>,
    path: &[&str],
) -> Option<bool> {
    yaml_path(extra, path).and_then(|v| v.as_bool())
}

fn yaml_path<'a>(
    extra: &'a std::collections::BTreeMap<String, serde_yaml::Value>,
    path: &[&str],
) -> Option<&'a serde_yaml::Value> {
    let (first, rest) = path.split_first()?;
    let mut cur = extra.get(*first)?;
    for key in rest {
        cur = cur.get(*key)?;
    }
    Some(cur)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn leaf(prop: &str, eq: i64) -> cc::Predicate {
        cc::Predicate::Leaf(cc::Leaf {
            prop: prop.into(),
            mask: None,
            eq: Some(eq),
            ne: None,
            lt: None,
            gt: None,
        })
    }

    fn selected_transfer_actions() -> (cc::Action, cc::Action) {
        let object_info = cc::Step {
            send_op: Some("0x1008".into()),
            captures: vec![
                cc::model::Capture {
                    bind: "objectReportedSize".into(),
                    source: cc::model::CaptureSource::ObjectInfoCompressedSize,
                },
                cc::model::Capture {
                    bind: "objectTransferSize".into(),
                    source: cc::model::CaptureSource::ObjectInfoCompressedSize,
                },
            ],
            ..Default::default()
        };
        let true_size = cc::Step {
            if_step: Some(cc::model::IfStep {
                slot: "objectReportedSize".into(),
                equals: 0xffff_ffff,
                then_steps: vec![cc::Step {
                    send_op: Some("0x9803".into()),
                    captures: vec![cc::model::Capture {
                        bind: "objectTransferSize".into(),
                        source: cc::model::CaptureSource::U64Le,
                    }],
                    ..Default::default()
                }],
                else_steps: Vec::new(),
            }),
            ..Default::default()
        };
        let chunk_size = cc::Step {
            get_prop: Some("0xd235".into()),
            captures: vec![cc::model::Capture {
                bind: "chunkSize".into(),
                source: cc::model::CaptureSource::PropValue,
            }],
            ..Default::default()
        };
        let chunk = cc::Step {
            r#loop: Some(cc::Loop::Chunk {
                total: "objectTransferSize".into(),
                size: cc::model::ChunkSize::Runtime {
                    runtime: "chunkSize".into(),
                },
                offset_bind: "offset".into(),
                length_bind: "length".into(),
                body: vec![],
            }),
            ..Default::default()
        };
        let import = cc::Action {
            initiator: Some(cc::ActionInitiator {
                steps: vec![
                    cc::Step {
                        get_prop: Some("0xd621".into()),
                        captures: vec![cc::model::Capture {
                            bind: "objectHandles".into(),
                            source: cc::model::CaptureSource::PtpU32Array,
                        }],
                        ..Default::default()
                    },
                    cc::Step {
                        r#loop: Some(cc::Loop::ForEach {
                            collection: "objectHandles".into(),
                            bind: "handle".into(),
                            body: vec![object_info, true_size, chunk_size, chunk],
                        }),
                        ..Default::default()
                    },
                ],
                ..Default::default()
            }),
            ..Default::default()
        };
        (import, cc::Action::default())
    }

    fn assert_selected_transfer_contract_error(import: &cc::Action, expected: &str) {
        let error = project_selected_object_transfer(import, &cc::Action::default())
            .expect_err("malformed projection must fail");
        assert!(
            matches!(error, ConfigError::Contract(ref message) if message.contains(expected)),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn selected_transfer_projection_rejects_missing_for_each() {
        let action = cc::Action {
            initiator: Some(cc::ActionInitiator::default()),
            ..Default::default()
        };
        assert_selected_transfer_contract_error(&action, "forEach");
    }

    #[test]
    fn selected_transfer_projection_rejects_missing_chunk() {
        let (mut import, _) = selected_transfer_actions();
        let Some(cc::Loop::ForEach { body, .. }) =
            import.initiator.as_mut().unwrap().steps[1].r#loop.as_mut()
        else {
            panic!("fixture forEach");
        };
        body.pop();
        assert_selected_transfer_contract_error(&import, "chunk loop");
    }

    #[test]
    fn selected_transfer_projection_rejects_literal_chunk_size() {
        let (mut import, _) = selected_transfer_actions();
        let Some(cc::Loop::ForEach { body, .. }) =
            import.initiator.as_mut().unwrap().steps[1].r#loop.as_mut()
        else {
            panic!("fixture forEach");
        };
        let Some(cc::Loop::Chunk { size, .. }) = body[3].r#loop.as_mut() else {
            panic!("fixture chunk");
        };
        *size = cc::model::ChunkSize::Literal(1024);
        assert_selected_transfer_contract_error(&import, "runtime slot");
    }

    #[test]
    fn selected_transfer_projection_rejects_missing_object_info_capture() {
        let (mut import, _) = selected_transfer_actions();
        let Some(cc::Loop::ForEach { body, .. }) =
            import.initiator.as_mut().unwrap().steps[1].r#loop.as_mut()
        else {
            panic!("fixture forEach");
        };
        body[0].captures.clear();
        assert_selected_transfer_contract_error(&import, "ObjectInfo");
    }

    #[test]
    fn selected_transfer_projection_rejects_mismatched_total_capture() {
        let (mut import, _) = selected_transfer_actions();
        let Some(cc::Loop::ForEach { body, .. }) =
            import.initiator.as_mut().unwrap().steps[1].r#loop.as_mut()
        else {
            panic!("fixture forEach");
        };
        body[0].captures[1].bind = "differentSlot".into();
        assert_selected_transfer_contract_error(&import, "chunk total slot");
    }

    #[test]
    fn selected_transfer_projection_rejects_missing_true_size_override() {
        let (mut import, _) = selected_transfer_actions();
        let Some(cc::Loop::ForEach { body, .. }) =
            import.initiator.as_mut().unwrap().steps[1].r#loop.as_mut()
        else {
            panic!("fixture forEach");
        };
        body[1].if_step = None;
        body[1].reopen_session = Some(cc::model::ReopenSession {});
        assert_selected_transfer_contract_error(&import, "u64 true-size");
    }

    #[test]
    fn selected_transfer_projection_rejects_missing_chunk_size_capture() {
        let (mut import, _) = selected_transfer_actions();
        let Some(cc::Loop::ForEach { body, .. }) =
            import.initiator.as_mut().unwrap().steps[1].r#loop.as_mut()
        else {
            panic!("fixture forEach");
        };
        body[2].captures.clear();
        assert_selected_transfer_contract_error(&import, "chunk-size slot");
    }

    #[test]
    fn selected_transfer_projection_rejects_unmappable_nested_step() {
        let (mut import, _) = selected_transfer_actions();
        let Some(cc::Loop::ForEach { body, .. }) =
            import.initiator.as_mut().unwrap().steps[1].r#loop.as_mut()
        else {
            panic!("fixture forEach");
        };
        let Some(condition) = body[1].if_step.as_mut() else {
            panic!("fixture condition");
        };
        condition.then_steps[0].send_op = Some("not-a-hex-op".into());
        assert_selected_transfer_contract_error(&import, "unmappable step");
    }

    #[test]
    fn selected_transfer_projection_rejects_unmappable_read_step() {
        let (import, mut read) = selected_transfer_actions();
        read.initiator
            .get_or_insert_with(cc::ActionInitiator::default)
            .steps
            .push(cc::Step {
                send_op: Some("not-a-hex-op".into()),
                ..Default::default()
            });
        let error = project_selected_object_transfer(&import, &read)
            .expect_err("malformed read action must fail");
        assert!(matches!(
            error,
            ConfigError::Contract(ref message)
                if message.contains("getObject") && message.contains("unmappable step")
        ));
    }

    /// The hand-mirror seam: an `awaitUntil` step (with a nested `onEach` and a
    /// multi-leaf `all` predicate) must map to `EntryStep::AwaitUntil` and fail
    /// validation instead of disappearing if any nested step is unmappable.
    #[test]
    fn close_session_step_maps_and_is_not_dropped() {
        // An EntryStep that can't represent a step would silently DROP it; the
        // closeSession marker (#82) must survive map_step like reopenSession.
        let step = cc::Step {
            close_session: Some(cc::CloseSession {
                transport_close: true,
            }),
            tolerant: true,
            ..Default::default()
        };
        match map_step(&step).expect("closeSession must not be dropped") {
            EntryStep::CloseSession {
                transport_close,
                tolerant,
            } => {
                assert!(transport_close);
                assert!(tolerant);
            }
            _ => panic!("expected EntryStep::CloseSession"),
        }
    }

    #[test]
    fn await_until_step_maps_and_is_not_dropped() {
        let step = cc::Step {
            await_until: Some(cc::AwaitUntil {
                source: cc::AwaitSource::Poll {
                    prop: "0xd209".into(),
                },
                until: cc::Predicate::All {
                    all: vec![leaf("0xd209", 1), leaf("0xd17c", 0)],
                },
                on_each: vec![cc::Step {
                    get_prop: Some("0xd212".into()),
                    tolerant: true,
                    ..Default::default()
                }],
                timeout_ms: 5000,
                interval_ms: 250,
            }),
            ..Default::default()
        };
        let mapped = map_step(&step).expect("awaitUntil must not be dropped");
        match mapped {
            EntryStep::AwaitUntil {
                source,
                until,
                on_each,
                captures: _,
                timeout_ms,
                interval_ms,
                tolerant,
            } => {
                assert!(matches!(source, FfiAwaitSource::Poll { prop: 0xd209 }));
                assert_eq!(timeout_ms, 5000);
                assert_eq!(interval_ms, 250);
                assert!(!tolerant);
                // Nested onEach mapped recursively.
                assert_eq!(on_each.len(), 1);
                assert!(matches!(
                    on_each[0],
                    EntryStep::GetProp { prop: 0xd212, .. }
                ));
                // The multi-leaf `all` predicate survived (not flattened to one).
                match until {
                    FfiPredicate::All { all } => assert_eq!(all.len(), 2),
                    other => panic!("expected All predicate, got {other:?}"),
                }
            }
            other => panic!("expected AwaitUntil, got {other:?}"),
        }
    }

    /// #54: the event-source `awaitUntil` must also map to `EntryStep::AwaitUntil`
    /// (carrying `FfiAwaitSource::Event`) and NOT be silently dropped.
    #[test]
    fn await_until_event_source_maps_and_is_not_dropped() {
        let step = cc::Step {
            await_until: Some(cc::AwaitUntil {
                source: cc::AwaitSource::Event {
                    code: "0xc005".into(),
                    then_poll: Some("0xd209".into()),
                },
                until: cc::Predicate::All {
                    all: vec![leaf("0xd209", 1)],
                },
                on_each: vec![],
                timeout_ms: 5000,
                interval_ms: 0,
            }),
            ..Default::default()
        };
        let mapped = map_step(&step).expect("event-source awaitUntil must not be dropped");
        match mapped {
            EntryStep::AwaitUntil {
                source: FfiAwaitSource::Event { code, then_poll },
                ..
            } => {
                assert_eq!(code, 0xc005);
                assert_eq!(then_poll, Some(0xd209));
            }
            other => panic!("expected Event-source AwaitUntil, got {other:?}"),
        }
    }

    #[test]
    fn map_step_rejects_a_malformed_predicate() {
        let step = cc::Step {
            await_until: Some(cc::AwaitUntil {
                source: cc::AwaitSource::Poll {
                    prop: "0xd209".into(),
                },
                until: leaf("0xzz", 1),
                on_each: vec![],
                timeout_ms: 5000,
                interval_ms: 0,
            }),
            ..Default::default()
        };
        assert!(map_step(&step).is_none());
    }

    /// `await_until_satisfied` evaluates the mirrored predicate via the canonical
    /// engine logic (no Swift-side re-implementation).
    #[test]
    fn await_until_satisfied_evaluates_via_engine() {
        let until = FfiPredicate::try_from(&cc::Predicate::All {
            all: vec![leaf("0xd209", 1)],
        })
        .unwrap();
        assert!(await_until_satisfied(
            until.clone(),
            vec![PropObservation {
                code: 0xd209,
                value: 1
            }]
        ));
        assert!(!await_until_satisfied(
            until,
            vec![PropObservation {
                code: 0xd209,
                value: 0
            }]
        ));
    }
}
