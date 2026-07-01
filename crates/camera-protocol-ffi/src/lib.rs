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

pub mod mfg_index;
pub use mfg_index::{
    AcquireSource, AwaitSource, BleActionPlan, BleAdRecord, BleManufacturerData, BleNotifyUntil,
    BleServiceData, CccdMode, ChunkField, ChunkFrameField, Confidence, EstablishmentPlan,
    ModelMatch, NotifyCapture, Observation, Predicate, PredicateOp, Recognition, Step, StepOptions,
    StepValue, Transform,
};

uniffi::setup_scaffolding!();

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
#[derive(Debug, uniffi::Enum)]
pub enum ValueWidth {
    U16,
    U32,
    I16,
    I32,
}

impl From<ValueWidth> for protocol_primitives::ValueWidth {
    fn from(w: ValueWidth) -> Self {
        match w {
            ValueWidth::U16 => protocol_primitives::ValueWidth::U16,
            ValueWidth::U32 => protocol_primitives::ValueWidth::U32,
            ValueWidth::I16 => protocol_primitives::ValueWidth::I16,
            ValueWidth::I32 => protocol_primitives::ValueWidth::I32,
        }
    }
}

/// G1 — build the 82-byte Fuji reference app `InitCommandRequest`. Identity/tail come from
/// the manifest; this frames them.
#[uniffi::export]
pub fn build_app_init(
    guid: Vec<u8>,
    friendly_name: String,
    tail: Vec<u8>,
) -> Result<Vec<u8>, CodecError> {
    protocol_primitives::build_app_init(&guid, &friendly_name, &tail)
        .map_err(|e| CodecError::Encode(e.to_string()))
}

/// The 8-byte keep-AP sentinel (`0xffffffff` framed) the app sends instead of a
/// TCP FIN to hold the camera's Wi-Fi AP up across an in-place reopen (#82).
#[uniffi::export]
pub fn keep_ap_sentinel() -> Vec<u8> {
    protocol_primitives::keep_ap_sentinel()
}

#[uniffi::export]
pub fn validate_init_ack(packet: Vec<u8>) -> Result<(), CodecError> {
    protocol_primitives::validate_init_ack(&packet).map_err(|e| CodecError::Encode(e.to_string()))
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
/// the app's `FujiFocusArea` so tap-to-focus carries no camera math (#135).
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
    FujiCompressed,
    Usb,
}

fn frame_encode(framing: PtpFraming, pkt: &ptp_core::PtpIpPacket) -> Result<Vec<u8>, CodecError> {
    match framing {
        PtpFraming::Standard => ptp_core::encode(pkt).map_err(codec_encode),
        PtpFraming::FujiCompressed => {
            protocol_primitives::fuji_framing::encode(pkt).map_err(codec_encode)
        }
        PtpFraming::Usb => protocol_primitives::usb_ptp::encode(pkt).map_err(codec_encode),
    }
}

fn frame_decode(framing: PtpFraming, bytes: &[u8]) -> Result<ptp_core::PtpIpPacket, CodecError> {
    use ptp_core::PtpCodec;
    match framing {
        PtpFraming::Standard => ptp_core::PtpIpPacket::decode(bytes),
        PtpFraming::FujiCompressed => protocol_primitives::fuji_framing::decode(bytes),
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
        PtpFraming::FujiCompressed => Ok(protocol_primitives::fuji_framing::encode_data(
            op, txn, &payload,
        )),
        PtpFraming::Standard | PtpFraming::Usb => {
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
/// socket), so `FujiCompressed` will reject an event frame.
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
#[derive(Debug, uniffi::Enum)]
pub enum PtpValue {
    U8 { value: u8 },
    U16 { value: u16 },
    U32 { value: u32 },
    U64 { value: u64 },
    Str { value: String },
}

impl From<&ptp_core::PropValue> for PtpValue {
    fn from(v: &ptp_core::PropValue) -> Self {
        match v {
            ptp_core::PropValue::U8(x) => PtpValue::U8 { value: *x },
            ptp_core::PropValue::U16(x) => PtpValue::U16 { value: *x },
            ptp_core::PropValue::U32(x) => PtpValue::U32 { value: *x },
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

/// A decoded Fuji `0xD212`-style live-status record stream (each entry a
/// `(prop code, value)` pair; the member set is manifest-driven, #107).
#[derive(uniffi::Record)]
pub struct LiveStatus {
    pub records: Vec<PropObservation>,
}

/// Parse a live-status record-stream payload into its property observations.
#[uniffi::export]
pub fn parse_live_status(payload: Vec<u8>) -> Result<LiveStatus, CodecError> {
    let records = protocol_primitives::quirk::parse_record_stream(&payload)
        .map_err(codec_decode)?
        .into_iter()
        .map(|(code, value)| PropObservation {
            code,
            value: value as i64,
        })
        .collect();
    Ok(LiveStatus { records })
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
            cc::WireFraming::FujiCompressed => PtpFraming::FujiCompressed,
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

/// The InitCommandRequest for a connection, assembled from manifest data (#82):
/// the resolved identity + literal vendor tail, plus the pre-built 82-byte
/// packet — so the app replays bytes with no client-side literals.
#[derive(uniffi::Record)]
pub struct InitShapeInfo {
    pub guid: Vec<u8>,
    pub friendly_name: String,
    pub name_field_byte_count: u32,
    pub tail: Vec<u8>,
    pub packet: Vec<u8>,
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

#[derive(uniffi::Record)]
pub struct ControlInfo {
    pub set_method: Option<String>,
    pub operation: Option<u16>,
    pub readback: Option<u16>,
}

/// Mirror of `cc::Payload` — how a composite byte-array property (`0xD212`
/// live-status) decomposes into a record stream the app walks. Full mirror:
/// dropping `members` would silently lose the poll allowlist the consumer needs.
#[derive(uniffi::Record)]
pub struct PayloadInfo {
    pub form: PayloadForm,
    pub count_width: Option<u8>,
    pub record: Option<RecordLayoutInfo>,
    pub members: Vec<u16>,
}

/// One row of the property catalog (#50): code, name, wire type, access, the
/// allowed value set, and value→label pairs. Lets the app present settings
/// without hardcoding a per-vendor catalog.
#[derive(uniffi::Record)]
pub struct PropertyInfo {
    pub code: u16,
    pub name: String,
    pub ptype: Option<String>,
    pub access: Option<String>,
    pub values: Vec<i64>,
    pub labels: Vec<KeyValue>,
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
        PayloadInfo {
            form: match p.form {
                cc::PayloadForm::RecordStream => PayloadForm::RecordStream,
            },
            count_width: p.count_width,
            record: p.record.as_ref().map(|r| RecordLayoutInfo {
                code_width: r.code_width,
                value_width: r.value_width,
            }),
            members: p.members.iter().filter_map(|m| parse_hex_code(m)).collect(),
        }
    }
}

/// A `send_op` parameter: a literal, or a named runtime slot the app binds from its
/// own session state (e.g. the live-view open-capture txid). Declarative — not a
/// computed variable.
#[derive(Debug, uniffi::Enum)]
pub enum EntryParam {
    Literal { value: u32 },
    Runtime { slot: String },
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

impl From<&cc::Predicate> for FfiPredicate {
    fn from(p: &cc::Predicate) -> Self {
        match p {
            cc::Predicate::All { all } => FfiPredicate::All {
                all: all.iter().map(FfiPredicate::from).collect(),
            },
            cc::Predicate::Any { any } => FfiPredicate::Any {
                any: any.iter().map(FfiPredicate::from).collect(),
            },
            cc::Predicate::Not { not } => FfiPredicate::Not {
                not: vec![FfiPredicate::from(not.as_ref())],
            },
            cc::Predicate::Leaf(l) => FfiPredicate::Leaf {
                prop: parse_hex_code(&l.prop).unwrap_or(0),
                mask: l.mask,
                eq: l.eq,
                ne: l.ne,
                lt: l.lt,
                gt: l.gt,
            },
        }
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
    GetProp {
        prop: u16,
        tolerant: bool,
    },
    ReadEcho {
        prop: u16,
        tolerant: bool,
    },
    SendOp {
        op: u16,
        params: Vec<EntryParam>,
        repeat: u32,
        tolerant: bool,
    },
    /// Re-establish the PTP/IP session in-place — close the current TCP
    /// socket, send the 8-byte `0xffffffff` sentinel, open a new socket to
    /// the connection's command port, replay the cached InitCommandRequest,
    /// and OpenSession again. Reuses the connection's cached identity, so
    /// the verb carries no parameters. Wire-confirmed for the reference app
    /// `app` → image-transfer (Take→Get) flow per MODE_CHANGES.md §5.
    ReopenSession {
        tolerant: bool,
    },
    /// End the PTP/IP session. `keep_ap` emits the 8-byte `0xffffffff` keep-AP
    /// sentinel (`keep_ap_sentinel()`) instead of a TCP FIN, so the camera holds
    /// its Wi-Fi AP up across an in-place reopen (#82).
    CloseSession {
        keep_ap: bool,
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
        timeout_ms: u32,
        interval_ms: u32,
        tolerant: bool,
    },
    /// A closed declarative loop (#46): `ForEach` over a captured collection (each
    /// element binds a runtime slot for `body`), or `Chunk` by fixed size (the
    /// dispatcher owns the offset/length cursor; `total` names the scope slot a
    /// preceding GetObjectInfo captured). The nested `body` crosses the seam.
    Loop {
        kind: FfiLoopKind,
        tolerant: bool,
    },
}

/// The two `Loop` shapes (#46). Mirrors `cc::Loop`. `ForEach` iterates the
/// array-valued property `in_prop`, binding each element to `bind`; `Chunk` walks
/// `total` bytes in `size`-byte windows, binding `offset_bind`/`length_bind` each
/// iteration. The dispatcher owns all cursor advancement — no author arithmetic.
#[derive(Debug, uniffi::Enum)]
pub enum FfiLoopKind {
    ForEach {
        in_prop: u16,
        bind: String,
        body: Vec<EntryStep>,
    },
    Chunk {
        total: String,
        size: u32,
        offset_bind: String,
        length_bind: String,
        body: Vec<EntryStep>,
    },
}

/// Where a PTP-IP `awaitUntil` observes (#54). Mirrors `cc::AwaitSource`. `Poll`
/// is the #49 default (poll a property each iteration); `Event` awaits a
/// completion push on the event socket then does one value read (`then_poll`).
/// On the `Event` happy path the dispatcher: opens/reads the connection's event
/// socket (55741), awaits an event packet with `code`, issues one
/// `GetDevicePropValue(then_poll)`, then evaluates `until` once.
#[derive(Debug, Clone, uniffi::Enum)]
pub enum FfiAwaitSource {
    Poll { prop: u16 },
    Event { code: u16, then_poll: Option<u16> },
}

#[derive(uniffi::Record)]
pub struct ModeEntryPlan {
    pub to: String,
    pub from: Option<String>,
    pub steps: Vec<EntryStep>,
    pub user_instruction: Option<String>,
}

// ----------------------------------------------------------------------------
// Action surface — named, parameterized recipes that run within a mode.
// Mirrors camera-config's `Connection.actions` block (docs/plans/action-verbs.md).
// ActionEffect ships as a uniffi tagged enum here (vs. the flat struct in
// camera-config) so consumer Swift / Kotlin gets clean exhaustive-switch
// ergonomics:
//
//     switch shutter.triggers[0] {
//     case .imagesPushed(let min, let max): // wire up receive handler
//     case .postviewEvent:                  // wait via 0xD212 polling
//     case .liveViewStream:                 // continuous frame delivery
//     }
// ----------------------------------------------------------------------------

/// Closed verb vocabulary for named in-mode actions. Mirrors
/// `cc::ActionVerb`; new verbs require an FFI-side variant alongside the
/// camera-config-side addition (same fail-fast as Step verbs).
#[derive(Debug, Clone, Copy, uniffi::Enum)]
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
}

/// Declared post-conditions an action produces — the consumer plans UX
/// around them without per-transport knowledge. Engine does NOT act on
/// triggers; pure declaration.
#[derive(Debug, Clone, Copy, uniffi::Enum)]
pub enum ActionEffect {
    /// Camera auto-pushes between `min` and `max` images to the tether
    /// endpoint after `Shutter`. Receiver MUST be wired up before invoking.
    /// PCSS shutter: `min=1, max=3` depending on the user's JPEG/HEIF/RAW
    /// selection.
    ImagesPushed { min: u32, max: u32 },
    /// Camera emits a post-shutter state change the consumer polls for
    /// (reference app `app` path: `0xD212` clears the JPEG-saved flag, then `0x9022`).
    PostviewEvent,
    /// Continuous frame delivery starts (e.g. live-view through-stream
    /// after `0x101C InitiateOpenCapture`).
    LiveViewStream,
}

/// A parameterized recipe runnable within a mode. Returned by
/// [`ConfigStore::action`]. The consumer reads `params` to know which
/// runtime slots to bind for `EntryParam::Runtime` references in `steps`,
/// then executes `steps` via its own I/O. `triggers` declares what arrives
/// after the action completes.
#[derive(Debug, uniffi::Record)]
pub struct Action {
    pub mode: String,
    pub params: Vec<String>,
    pub steps: Vec<EntryStep>,
    pub triggers: Vec<ActionEffect>,
    pub evidence: Vec<String>,
}

#[derive(Debug, Clone, uniffi::Record)]
pub struct KeyValue {
    pub key: String,
    pub value: String,
}

/// How to bring a known connection up (data only — the app drives the
/// GATT/UDP/TCP I/O). Returned by [`ConfigStore::connection_establishment`].
///
/// **Renamed in P1** (was `EstablishmentPlan`) — the manufacturer-index
/// pull-model flow took the `EstablishmentPlan` name; this type covers the
/// older single-connection query (`establishment("ble")` → "here are the
/// GATT UUIDs and knock ports for the ble connection").
#[derive(uniffi::Record)]
pub struct ConnectionEstablishmentInfo {
    pub target_connection: String,
    pub mechanism: Option<String>,
    pub user_instruction: Option<String>,
    pub params: Vec<KeyValue>,
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

/// One bound socket for a connection: which role, on which port (#140).
#[derive(Debug, uniffi::Record)]
pub struct SocketBindingInfo {
    pub role: SocketRole,
    pub port: u16,
}

/// The transport-close frame a connection sends before an image-transfer reopen,
/// with the manifest's named sentinel resolved to bytes (#140).
#[derive(Debug, uniffi::Record)]
pub struct TransportCloseInfo {
    /// The frame bytes to send.
    pub packet: Vec<u8>,
    /// When to send it (e.g. `before-image-transfer-reopen`), if declared.
    pub when: Option<String>,
}

// ----------------------------------------------------------------------------
// ConfigStore — the loaded, queryable seam
// ----------------------------------------------------------------------------

#[derive(uniffi::Object)]
pub struct ConfigStore {
    inner: cc::ConfigStore,
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
        let bodies: BTreeMap<String, String> = model_bodies
            .into_iter()
            .map(|kv| (kv.key, kv.value))
            .collect();
        let inner = cc::ConfigStore::from_manufacturer_index(&index_yaml, bodies)?;
        // Unwrap the Arc<cc::ConfigStore> into a fresh FFI ConfigStore. The
        // inner Arc is private to camera-config; here we own the FFI-level
        // Arc<ConfigStore>.
        let inner = Arc::try_unwrap(inner).unwrap_or_else(|arc| (*arc).clone());
        Ok(Arc::new(ConfigStore { inner }))
    }

    // -----------------------------------------------------------------------
    // Manufacturer-index pull model (§3.2 + §3.3 + §11)
    // -----------------------------------------------------------------------

    /// Observation → decision. Returns [`Recognition::NoMatch`] when no
    /// signature fires; [`Recognition::Candidate`] for a single match (the
    /// MVP case); [`Recognition::Disambiguate`] when multiple models match
    /// the same signature.
    pub fn recognize(&self, observation: Observation) -> Recognition {
        let Some(index) = &self.inner.index else {
            return Recognition::NoMatch;
        };
        match observation {
            Observation::BleAdvert {
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
        }
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
        let mechanism = self
            .inner
            .manifest
            .connections
            .get(&connection)?
            .establishment
            .clone()?;
        mfg_index::build_establishment(index, &model, &connection, &mechanism, &initial_scope)
    }

    /// The BLE-native control action plan registered under `action` for `model`
    /// (#91) — e.g. `remote-shutter`, `write-gps`. Runnable from the resting
    /// BLE-connected link without Wi-Fi. `None` if the model or action is unknown.
    pub fn ble_action(&self, model: String, action: String) -> Option<BleActionPlan> {
        let index = self.inner.index.as_ref()?;
        mfg_index::build_ble_action(index, &model, &action)
    }

    /// Per §11.5: returns ONLY the unwalked tail; the dispatcher splices it
    /// onto its existing plan at `next_step_index`. When `None` is returned,
    /// the dispatcher leaves the existing tail in place (graceful degrade —
    /// no matching overlay → use body's default sequence).
    ///
    /// **MVP stub:** always returns `None`. The BLE-only YAML in
    /// `packages/camera-config-data/fuji/index.yaml` has no firmware-
    /// branching `if:` blocks, so there is no overlay to apply. The P2
    /// expansion (FilmSimulation enum growth across fw 2.50, the GFX100 II's
    /// fw 2.30→02.40 transport flip already modeled in
    /// `gfx100ii/fw2.40.yaml`) wires real overlay resolution here.
    pub fn refine_establishment(
        &self,
        _plan_handle: String,
        _firmware: String,
        _scope: Vec<KeyValue>,
        _next_step_index: u32,
    ) -> Option<Vec<Step>> {
        // TODO(P2): walk the family/model establishment.steps with the new
        // firmware context, evaluate any `if:` predicates that resolve
        // against `scope` ∪ {"firmware": firmware}, return the resulting
        // tail from `next_step_index` onward.
        None
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
    /// **Renamed in P1** (was `establishment(connection)`) — the
    /// `establishment(model, connection, initial_scope)` name now belongs to
    /// the manufacturer-index pull-model flow per plan §3.3.
    pub fn connection_establishment(
        &self,
        connection: String,
    ) -> Option<ConnectionEstablishmentInfo> {
        let c = self.inner.manifest.connections.get(&connection)?;
        let mut params = Vec::new();
        for block in ["knock", "gatt"] {
            if let Some(serde_yaml::Value::Mapping(m)) = c.extra.get(block) {
                for (k, v) in m {
                    if let (Some(k), Some(v)) = (k.as_str(), yaml_scalar(v)) {
                        params.push(KeyValue {
                            key: k.to_string(),
                            value: v,
                        });
                    }
                }
            }
        }
        Some(ConnectionEstablishmentInfo {
            target_connection: connection,
            mechanism: c.establishment.clone(),
            user_instruction: None,
            params,
        })
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
        .filter_map(|(role, port)| port.map(|port| SocketBindingInfo { role, port }))
        .collect()
    }

    /// The transport-close frame `connection` sends before reopening an
    /// image-transfer session, with the named sentinel resolved to bytes (#140).
    pub fn transport_close(&self, connection: String) -> Option<TransportCloseInfo> {
        let tc = self
            .inner
            .manifest
            .connections
            .get(&connection)?
            .transport_close
            .as_ref()?;
        let packet = match tc.sentinel.as_str() {
            "keepApSentinel" => protocol_primitives::keep_ap_sentinel(),
            _ => return None,
        };
        Some(TransportCloseInfo {
            packet,
            when: tc.when.clone(),
        })
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

    /// The wire-action plan to enter `to` (optionally from a known mode — a cheaper
    /// teardown-free switch). Steps, or a `user_instruction` when not app-driven.
    pub fn mode_entry(
        &self,
        connection: String,
        from: Option<String>,
        to: String,
    ) -> Option<ModeEntryPlan> {
        let c = self.inner.manifest.connections.get(&connection)?;
        let e = c.entries.iter().find(|e| e.to == to && e.from == from)?;
        Some(ModeEntryPlan {
            to: e.to.clone(),
            from: e.from.clone(),
            steps: e.steps.iter().filter_map(map_step).collect(),
            user_instruction: e.user_instruction.clone(),
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
        Some(Action {
            mode: a.mode.clone(),
            params: a.params.clone(),
            steps: a.steps.iter().filter_map(map_step).collect(),
            triggers: a.triggers.iter().filter_map(map_action_effect).collect(),
            evidence: a.evidence.clone(),
        })
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

    /// Value-policy resolution (fixed initiator identity, generated session ids, …),
    /// body overriding manufacturer.
    pub fn value(&self, key: String) -> Option<ResolvedValue> {
        match self.inner.value(&key)? {
            cc::ValuePolicy::Fixed { value } => Some(ResolvedValue::Fixed {
                value: yaml_scalar(value).unwrap_or_default(),
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
    /// data: resolved GUID + friendly name (via `values:`) + the literal vendor
    /// tail, plus the pre-built 82-byte packet — so the app replays bytes with no
    /// client-side literals. `None` if the connection declares no `init` shape
    /// (e.g. usb) or the identity/tail can't resolve. (#82)
    ///
    /// Returns `None` when the friendly name is `client-derived` (#109): the name
    /// is not a manifest literal but the host's own device name (which must equal
    /// the BLE `deviceNameString` it registered), so the consumer must supply it
    /// and build the packet itself. Baking the name from a host-supplied slot is
    /// deferred to the consumer-adoption work (#29).
    pub fn connection_init(&self, connection: String) -> Option<InitShapeInfo> {
        let c = self.inner.manifest.connections.get(&connection)?;
        let init = c.init.as_ref()?;
        let friendly_name = self.fixed_value(&init.identity.friendly_name)?;
        let guid = hex_value(&self.fixed_value(&init.identity.guid)?)?;
        let tail = match &init.tail {
            Some(t) => hex_value(t)?,
            None => Vec::new(),
        };
        let packet = protocol_primitives::build_app_init(&guid, &friendly_name, &tail).ok()?;
        Some(InitShapeInfo {
            guid,
            friendly_name,
            name_field_byte_count: init.name_field_byte_count,
            tail,
            packet,
        })
    }

    /// Runtime-aware InitCommandRequest assembly (#109/#29). Same shape as
    /// [`connection_init`], but `client-derived` identity slots resolve from the
    /// caller's runtime scope (for Fuji, `terminalName`). This lets consumers keep
    /// the BLE deviceNameString and PTP/IP friendlyName single-sourced while still
    /// replaying the manifest-owned vendor tail with no app-side byte literal.
    pub fn connection_init_with_runtime(
        &self,
        connection: String,
        runtime_scope: Vec<KeyValue>,
    ) -> Option<InitShapeInfo> {
        let c = self.inner.manifest.connections.get(&connection)?;
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
        let tail = match &init.tail {
            Some(t) => hex_value(t)?,
            None => Vec::new(),
        };
        let packet = protocol_primitives::build_app_init(&guid, &friendly_name, &tail).ok()?;
        Some(InitShapeInfo {
            guid,
            friendly_name,
            name_field_byte_count: init.name_field_byte_count,
            tail,
            packet,
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

    /// The encoder width for a property, resolved from the manifest's `type`
    /// (`u16`→U16, `u32`→U32). `None` for an unknown property or an unsupported
    /// type (e.g. `u8a`) — pair with `encode_value(raw, width)`.
    pub fn property_value_width(&self, prop: u16) -> Option<ValueWidth> {
        match self.inner.manifest.property(prop)?.ptype.as_deref() {
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
            .filter_map(|(code, p)| {
                Some(PropertyInfo {
                    code: parse_hex_code(code)?,
                    name: p.name.clone(),
                    ptype: p.ptype.clone(),
                    access: p.access.clone(),
                    values: p
                        .descriptor
                        .as_ref()
                        .map(|d| d.values.clone())
                        .unwrap_or_default(),
                    labels: p
                        .labels
                        .iter()
                        .map(|(k, v)| KeyValue {
                            key: k.clone(),
                            value: v.clone(),
                        })
                        .collect(),
                })
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

    /// The wireless transfer size ceiling (#136): an object whose compressed size
    /// is `>=` this must come off the memory card, not the wireless transport.
    /// `None` if the camera declares no such bound.
    pub fn wireless_transfer_ceiling(&self) -> Option<u64> {
        self.inner
            .manifest
            .media
            .as_ref()?
            .wireless_transfer_ceiling
    }
}

// ----------------------------------------------------------------------------
// helpers
// ----------------------------------------------------------------------------

fn build_store(
    m: cc::CameraManifest,
    manufacturer_yaml: Option<String>,
) -> Result<Arc<ConfigStore>, ConfigError> {
    m.require_supported_schema()
        .map_err(|e| ConfigError::Schema(e.to_string()))?;
    let mut store = cc::ConfigStore::new(m);
    if let Some(my) = manufacturer_yaml {
        let d = cc::ManufacturerDefaults::from_yaml(&my)
            .map_err(|e| ConfigError::Parse(e.to_string()))?;
        store = store.with_manufacturer(d);
    }
    Ok(Arc::new(ConfigStore { inner: store }))
}

fn prop_view(observed: &[PropObservation]) -> cc::PropView {
    observed.iter().map(|p| (p.code, p.value)).collect()
}

fn map_step(s: &cc::Step) -> Option<EntryStep> {
    let tolerant = s.tolerant;
    if let Some(p) = &s.set_prop {
        return Some(EntryStep::SetProp {
            prop: parse_hex_code(p)?,
            value: s.value.unwrap_or(0),
            tolerant,
        });
    }
    if let Some(p) = &s.get_prop {
        return Some(EntryStep::GetProp {
            prop: parse_hex_code(p)?,
            tolerant,
        });
    }
    if let Some(p) = &s.read_echo {
        return Some(EntryStep::ReadEcho {
            prop: parse_hex_code(p)?,
            tolerant,
        });
    }
    if let Some(o) = &s.send_op {
        return Some(EntryStep::SendOp {
            op: parse_hex_code(o)?,
            params: s.params.iter().map(map_param).collect(),
            repeat: s.repeat,
            tolerant,
        });
    }
    if s.reopen_session.is_some() {
        return Some(EntryStep::ReopenSession { tolerant });
    }
    if let Some(cs) = &s.close_session {
        return Some(EntryStep::CloseSession {
            keep_ap: cs.keep_ap,
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
            until: (&aw.until).into(),
            on_each: aw.on_each.iter().filter_map(map_step).collect(),
            timeout_ms: aw.timeout_ms,
            interval_ms: aw.interval_ms,
            tolerant,
        });
    }
    if let Some(lp) = &s.r#loop {
        let kind = match lp {
            cc::Loop::ForEach {
                in_prop,
                bind,
                body,
            } => FfiLoopKind::ForEach {
                // A malformed `in` prop `?`-drops the whole step (same hazard as a
                // bad op code — guarded by the seam test).
                in_prop: parse_hex_code(in_prop)?,
                bind: bind.clone(),
                body: body.iter().filter_map(map_step).collect(),
            },
            cc::Loop::Chunk {
                total,
                size,
                offset_bind,
                length_bind,
                body,
            } => FfiLoopKind::Chunk {
                total: total.clone(),
                size: *size,
                offset_bind: offset_bind.clone(),
                length_bind: length_bind.clone(),
                body: body.iter().filter_map(map_step).collect(),
            },
        };
        return Some(EntryStep::Loop { kind, tolerant });
    }
    None
}

fn map_param(p: &cc::StepParam) -> EntryParam {
    match p {
        cc::StepParam::Literal(v) => EntryParam::Literal { value: *v },
        cc::StepParam::Runtime { runtime } => EntryParam::Runtime {
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
    }
}

/// Translate camera-config's flat-struct `ActionEffect` (one optional
/// field per variant) to the FFI's tagged-enum form. Returns `None` for
/// malformed effects (no variant set) — `is_well_formed()` is the
/// camera-config-side check that's expected to hold.
fn map_action_effect(e: &cc::ActionEffect) -> Option<ActionEffect> {
    if let Some(ip) = &e.images_pushed {
        return Some(ActionEffect::ImagesPushed {
            min: ip.min,
            max: ip.max,
        });
    }
    if e.postview_event.is_some() {
        return Some(ActionEffect::PostviewEvent);
    }
    if e.live_view_stream.is_some() {
        return Some(ActionEffect::LiveViewStream);
    }
    None
}

fn platform_ok(c: &cc::Connection, p: &Platform) -> bool {
    match c.extra.get("platforms") {
        Some(serde_yaml::Value::Sequence(seq)) => {
            seq.iter().any(|v| v.as_str() == Some(p.as_str()))
        }
        _ => true, // no restriction declared
    }
}

/// A scalar YAML value (string/int/bool) rendered to a string; `None` for compound.
/// Decode an even-length hex string (optionally `0x`-prefixed) to bytes —
/// matches `index::eval::yaml_literal_to_bytes`'s hex path, for the init GUID
/// and vendor tail.
fn hex_value(s: &str) -> Option<Vec<u8>> {
    let p = s.strip_prefix("0x").unwrap_or(s);
    if p.is_empty() || p.len() % 2 != 0 || !p.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    (0..p.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&p[i..i + 2], 16).ok())
        .collect()
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

    /// The hand-mirror seam: an `awaitUntil` step (with a nested `onEach` and a
    /// multi-leaf `all` predicate) must map to `EntryStep::AwaitUntil` and NOT
    /// be silently dropped by `map_step`'s `filter_map`.
    #[test]
    fn close_session_step_maps_and_is_not_dropped() {
        // An EntryStep that can't represent a step would silently DROP it; the
        // closeSession marker (#82) must survive map_step like reopenSession.
        let step = cc::Step {
            close_session: Some(cc::CloseSession { keep_ap: true }),
            tolerant: true,
            ..Default::default()
        };
        match map_step(&step).expect("closeSession must not be dropped") {
            EntryStep::CloseSession { keep_ap, tolerant } => {
                assert!(keep_ap);
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

    /// `await_until_satisfied` evaluates the mirrored predicate via the canonical
    /// engine logic (no Swift-side re-implementation).
    #[test]
    fn await_until_satisfied_evaluates_via_engine() {
        let until = FfiPredicate::from(&cc::Predicate::All {
            all: vec![leaf("0xd209", 1)],
        });
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
