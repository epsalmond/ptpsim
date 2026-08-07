//! The manifest data model. This is the reviewed source of truth for one
//! camera's behavior, loaded from YAML. Field naming follows the YAML schema in
//! `DESIGN.md` (camelCase). Most sections default to empty so partial manifests
//! and append-only growth are valid.

use crate::activity::ConnectionActivityDescriptor;
use crate::observation::{
    AssertionProvenance, ControlEvidenceBasis, ControlObservedEffect, TypedPropertyValue,
};
use crate::predicate::Predicate;
use crate::version::{compare, VersionScheme};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// A PTP code written in the manifest as a hex string (e.g. `"0x101b"`).
pub type HexCode = String;

/// Parse a `"0x101b"` style key into a `u16`. Returns `None` for malformed keys.
pub fn parse_hex_code(s: &str) -> Option<u16> {
    let t = s.trim();
    let hex = t
        .strip_prefix("0x")
        .or_else(|| t.strip_prefix("0X"))
        .unwrap_or(t);
    u16::from_str_radix(hex, 16).ok()
}

/// Parse a hexadecimal manifest value that may exceed the 16-bit PTP code
/// range, such as the u32 reason carried by PTP/IP InitFail.
pub fn parse_hex_u32(s: &str) -> Option<u32> {
    let t = s.trim();
    let hex = t
        .strip_prefix("0x")
        .or_else(|| t.strip_prefix("0X"))
        .unwrap_or(t);
    u32::from_str_radix(hex, 16).ok()
}

/// Decode an even-length hex byte string, optionally `0x`-prefixed.
pub fn parse_hex_bytes(s: &str) -> Option<Vec<u8>> {
    let p = s
        .strip_prefix("0x")
        .or_else(|| s.strip_prefix("0X"))
        .unwrap_or(s);
    if p.is_empty() || !p.len().is_multiple_of(2) || !p.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    (0..p.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&p[i..i + 2], 16).ok())
        .collect()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CameraManifest {
    pub schema: String,
    pub camera: CameraIdentity,
    #[serde(default)]
    pub evidence: BTreeMap<String, Evidence>,
    /// Reviewed semantic assertions and their assertion-level provenance.
    /// This ledger is query metadata only; the simulator never consults it for
    /// availability, state, gates, responses, or descriptor behavior.
    #[serde(default, skip_serializing_if = "SemanticAssertionLedger::is_empty")]
    pub semantic_assertions: SemanticAssertionLedger,
    #[serde(default)]
    pub sentinels: BTreeMap<String, SentinelFrame>,
    /// Named ordered wire-precondition gates the simulator can enforce. A gate
    /// is satisfied only after manifest steps marked with `startsGate` /
    /// `completesGate` run successfully in order. Consumers still receive the
    /// executable steps; the gate is simulator oracle metadata.
    #[serde(default)]
    pub sequence_gates: BTreeMap<String, SequenceGate>,
    /// The camera-signalled private media queue that consumers pull over a
    /// declared connection.
    #[serde(default)]
    pub camera_initiated_transfer: Option<CameraInitiatedTransfer>,
    #[serde(default)]
    pub operations: BTreeMap<HexCode, Operation>,
    #[serde(default)]
    pub properties: BTreeMap<HexCode, Property>,
    #[serde(default)]
    pub workflows: BTreeMap<String, Workflow>,
    #[serde(default)]
    pub media: Option<Media>,
    /// AF grid for tap-to-focus (#135). Absent for cameras without app-driven AF.
    #[serde(default)]
    pub focus_grid: Option<FocusGrid>,
    #[serde(default)]
    pub events: BTreeMap<HexCode, Event>,
    #[serde(default)]
    pub quirks: Vec<Quirk>,
    /// id-keyed mode records (hierarchical paths, e.g. `"Shooting/Stills"`).
    #[serde(default)]
    pub modes: BTreeMap<String, Mode>,
    /// id-keyed connection records. An entry is either an inline definition
    /// (mechanism) or a `ref` to a shared definition plus this body's usage
    /// conditions — see [`Connection`].
    #[serde(default)]
    pub connections: BTreeMap<String, Connection>,
    /// Named values resolved by policy (initiator identity, init tail, …).
    #[serde(default)]
    pub values: BTreeMap<String, ValuePolicy>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CameraIdentity {
    pub manufacturer: String,
    pub model: String,
    /// Canonical human form, e.g. `"2.30"` (matches PTP `GetDeviceInfo`). NB: the
    /// BLE GATT advert reports the zero-padded `"02.30"` for the same camera — a
    /// camera-reported firmware must be normalized via [`crate::version`], never
    /// raw-compared against this field.
    #[serde(default)]
    pub firmware: String,
    /// Named identity strings for this body. Known keys: `ptpDeviceName` (the
    /// PTP-side friendly name — channel-prefixed because BLE adverts carry a
    /// different name) and `serialNumber` (served as `DeviceInfo.serial_number`
    /// in GetDeviceInfo; on real bodies it equals the BLE full-serial
    /// characteristic, making it the channel-neutral saved-camera key, #152).
    #[serde(default)]
    pub identities: BTreeMap<String, String>,
}

/// The camera's AF grid dimensions for tap-to-focus (#135). A screen tap maps to
/// a 1-indexed cell of a `columns`×`rows` grid, packed into the `0x9026` param by
/// [`crate::model`]'s consumer via `protocol_primitives::pack_af_area`. GFX100 II
/// stills: 9×6. The app reads these dims from data — never hardcodes the grid.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FocusGrid {
    pub columns: u32,
    pub rows: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Evidence {
    pub kind: String,
    #[serde(default)]
    pub path: String,
    #[serde(default)]
    pub date: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SemanticAssertionLedger {
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub operations: BTreeMap<HexCode, OperationSemanticAssertions>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub properties: BTreeMap<HexCode, PropertySemanticAssertions>,
}

impl SemanticAssertionLedger {
    pub fn is_empty(&self) -> bool {
        self.operations.is_empty() && self.properties.is_empty()
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OperationSemanticAssertions {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub canonical_name: Option<ProvenancedName>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PropertySemanticAssertions {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub canonical_name: Option<ProvenancedName>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_native_name: Option<ProvenancedName>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub value_rows: Vec<ProvenancedPropertyValueRow>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub value_profiles: Vec<ProvenancedPropertyValueProfile>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProvenancedName {
    pub name: String,
    pub provenance: Vec<AssertionProvenance>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProvenancedPropertyValueRow {
    pub value: TypedPropertyValue,
    pub label: String,
    pub provenance: Vec<AssertionProvenance>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProvenancedPropertyValueProfile {
    pub profile: PropertyValueProfile,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub provenance: Vec<AssertionProvenance>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct SequenceGate {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub evidence: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GateRequirement {
    pub name: String,
    #[serde(default)]
    pub failure: GateFailure,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub enum GateFailure {
    #[default]
    NoResponse,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Operation {
    pub name: String,
    /// Whether this catalog row is safe to execute. Authored operations omit
    /// the field and remain executable; inventory-only generator rows are
    /// explicitly [`OperationKind::AdvertisedOnly`].
    #[serde(default, skip_serializing_if = "OperationKind::is_executable")]
    pub kind: OperationKind,
    #[serde(default)]
    pub owner: String,
    #[serde(default)]
    pub data_phase: Option<String>,
    #[serde(default)]
    pub params: Vec<serde_yaml::Value>,
    #[serde(default)]
    pub workflows: Vec<String>,
    /// Simulator dispatch behavior for this operation. Closed set; an unknown
    /// string is a load error. Absent = executable no-op (a cataloged op with
    /// no simulated camera-side effect).
    #[serde(default)]
    pub handler: Option<OperationHandler>,
    #[serde(default)]
    pub property: Option<HexCode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub object_size: Option<ObjectSizeHandler>,
    /// Modes (by path) this operation is valid in; prefix-matched, so a
    /// `Shooting`-level entry covers `Shooting/Stills`. Empty = all modes.
    #[serde(default)]
    pub modes: Vec<String>,
    /// Connection ids this operation is valid over. Empty = all connections.
    #[serde(default)]
    pub connections: Vec<String>,
    /// Runtime prerequisite over observed property values (card-inserted,
    /// not-writing, …); evaluated by the engine, not a tree edge.
    #[serde(default)]
    pub requires: Option<Predicate>,
    /// Ordered wire-precondition gate required before the simulator serves this
    /// operation. Distinct from `requires`, which is predicate-state gating.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requires_gate: Option<GateRequirement>,
    /// Camera-side state mutations this operation triggers — the simulator
    /// applies them so a poll-until (`awaitUntil`) flow round-trips (the §5.5 AF
    /// stub: `0x9026 LockS1Lock` → `0xd209 S1_LOCK_COLOR` flips to locked).
    /// Curated sim-behavior data (not probe-derivable). Distinct from
    /// [`ActionEffect`], which is an app-facing declaration the engine does NOT
    /// act on.
    #[serde(default)]
    pub effects: Vec<OpEffect>,
    /// Event codes this operation pushes when it succeeds (#54) — e.g. the AF tap
    /// pushes `0xC005` AFCAPTUER; a capture pushes `0xC004`→`0xC001`→`0x400D`. On
    /// an OK response the engine queues them; the live event socket forwards them
    /// to clients, and the reference executor's event-source `awaitUntil` reads
    /// them.
    ///
    /// Listed separately from [`effects`](Self::effects) because an event is a
    /// signal, not a value change. **Authoring rule:** an effect paired with an
    /// event must settle within one poll (`settle_after_polls` 0 or 1). The event
    /// means "the result is ready", and the one read that follows it is what makes
    /// the value visible — a single-shot event source has no loop to wait out a
    /// longer settle (§11.16).
    ///
    /// Curated sim-behavior; not mirrored to the app FFI (the app sends ops, the
    /// camera emits).
    #[serde(default)]
    pub emits: Vec<HexCode>,
    #[serde(default)]
    pub evidence: Vec<String>,
    /// Exact observation tuples backing generated availability. Kept atomic so
    /// independent connection/mode sets cannot invent a Cartesian product.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub observed_scopes: Vec<ObservedScope>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum OperationKind {
    #[default]
    Executable,
    AdvertisedOnly,
}

impl OperationKind {
    fn is_executable(&self) -> bool {
        *self == Self::Executable
    }
}

/// The dispatch behavior a cataloged operation carries. Closed set so a
/// handler typo fails at manifest load instead of silently dispatching as a
/// successful no-op (#407).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum OperationHandler {
    /// Step the `property` enum one slot; the request's first parameter is the
    /// direction.
    #[serde(rename = "property.step")]
    PropertyStep,
    /// Answer an extended object-size query; the `object_size` block declares
    /// the parameter layout and encoding.
    #[serde(rename = "object.size")]
    ObjectSize,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ObservedScope {
    pub connection: String,
    pub mode: String,
    pub state: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ObjectSizeHandler {
    pub handle_param: usize,
    pub encoding: ScalarEncoding,
    #[serde(default)]
    pub required_params: Vec<ParamEquals>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ParamEquals {
    pub index: usize,
    pub equals: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ScalarEncoding {
    #[serde(rename = "u32Le")]
    U32Le,
    #[serde(rename = "u64Le")]
    U64Le,
}

/// A camera-side state mutation an operation produces (consumed by the
/// simulator engine, NOT mirrored to the app FFI — the app sends ops; the
/// camera applies effects). `settle_after_polls` is the deterministic analogue
/// of §5.5's wall-clock AF delay: the new value becomes visible after that many
/// `GetDevicePropValue` polls of `set_prop` (0 = immediate). The reference
/// executor's poll-until loop iterates until the value settles — the PTP
/// analogue of the BLE walker's `serve_read_sequence`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OpEffect {
    /// Property whose value the operation changes.
    pub set_prop: HexCode,
    /// The fixed value it settles to. Ignored when `from_param` is set (the value
    /// is then taken from the operation's request parameters instead).
    #[serde(default)]
    pub value: i64,
    /// Polls of `set_prop` before the new value is visible (0 = immediate).
    #[serde(default)]
    pub settle_after_polls: u32,
    /// When set, the effect value comes from an operation *request* parameter
    /// rather than the fixed `value` — e.g. 0x9026 copies its packed AF-area
    /// param into 0xD17C (§5.5). Mutually exclusive with `value`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from_param: Option<ParamSource>,
}

/// Selects an effect value from an operation request's parameters, with an
/// optional bit-slice so a packed field can be pulled out: the chosen param is
/// shifted right by `shift`, then ANDed with `mask` (default: the whole value).
/// 0x9026 uses the identity form (`index: 0`, no shift/mask) — the entire packed
/// AF-area u32 copies into 0xD17C.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ParamSource {
    /// Index into the operation's request parameter list.
    pub index: usize,
    /// Right-shift applied to the raw param before masking (default 0).
    #[serde(default)]
    pub shift: u32,
    /// AND-mask applied after the shift (default: no mask — the full value).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mask: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Property {
    pub name: String,
    #[serde(default)]
    pub ptp_name: Option<String>,
    #[serde(default, rename = "type")]
    pub ptype: Option<String>,
    /// Declared PTP access. Closed set; the simulator rejects writes unless
    /// this is `readWrite` (#407). Absent = no write claim, matching the
    /// get-only `DevicePropDesc` it serves.
    #[serde(default)]
    pub access: Option<PropertyAccess>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub initial_value: Option<DescriptorValue>,
    /// Closed classification used by clients to filter what surfaces as a user
    /// setting. Omitted manifests default to [`PropertyKind::Setting`].
    #[serde(default, skip_serializing_if = "PropertyKind::is_setting")]
    pub kind: PropertyKind,
    #[serde(default)]
    pub descriptor: Option<Descriptor>,
    /// Composite-payload layout for a byte-array property whose value is a
    /// self-describing record stream of sub-property values (Fuji `0xD212`
    /// live-status). Absent for scalar properties. See [`Payload`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payload: Option<Payload>,
    /// Simulator-computed value source: the read value is derived from engine
    /// state (object store / transfer queue) instead of stored property state.
    /// Closed set; absent = stored value.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub computed: Option<ComputedValue>,
    #[serde(default)]
    pub controls: BTreeMap<String, Control>,
    /// Value -> human label, e.g. `280: "f/2.8"`.
    #[serde(default)]
    pub labels: BTreeMap<String, String>,
    /// Ordered property value choices for presentation and label→raw encoding.
    /// This is the explicit row form of `labels`: each row carries the raw value
    /// the camera expects and the label a client presents. Bulk rows come from
    /// evidence/generator output; hand-authored manifests keep this small.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub value_rows: Vec<PropertyValueRow>,
    /// Scoped value choices/capabilities for a property. Unlike `descriptor`,
    /// these may come from a connection/mode-specific capability path or an
    /// empirical write walk rather than standard `GetDevicePropDesc`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub value_profiles: Vec<PropertyValueProfile>,
    /// Generic value encoding hints for rows that share a bit-level form, such
    /// as a high-bit sentinel plus a low-bit literal. The descriptor names the
    /// shape; it does not bake manufacturer formulas into Rust.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value_encoding: Option<PropertyValueEncoding>,
    /// Generic field layout for a structured PTP string. This describes the
    /// wire grammar without assigning camera-specific coordinate bounds or
    /// behavior to the engine.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub structured_text: Option<StructuredTextLayout>,
    /// Ordered wire-precondition gate required before the simulator serves this
    /// property. Distinct from `Operation.requires` predicate gating.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requires_gate: Option<GateRequirement>,
    #[serde(default)]
    pub evidence: Vec<String>,
    /// Exact observation tuples that supplied generated property facts.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub observed_scopes: Vec<ObservedScope>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum PropertyKind {
    #[default]
    Setting,
    Scaffold,
    CatalogOnly,
}

impl PropertyKind {
    fn is_setting(&self) -> bool {
        *self == Self::Setting
    }
}

/// Declared PTP access for a property (#407). Closed set so a typo fails at
/// manifest load instead of silently widening or narrowing write behavior.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PropertyAccess {
    #[serde(rename = "readOnly")]
    ReadOnly,
    #[serde(rename = "readWrite")]
    ReadWrite,
}

/// A simulator-computed property value source (#407). The manifest declares
/// WHICH engine quantity a property serves; no property code is special in the
/// engine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ComputedValue {
    /// Count of currently enumerable objects (served as u32).
    #[serde(rename = "objectCount")]
    ObjectCount,
    /// Currently enumerable object handles (served as a count-prefixed u32
    /// array, same encoding as GetObjectHandles).
    #[serde(rename = "objectHandles")]
    ObjectHandles,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PropertyValueRow {
    pub label: String,
    pub raw: i64,
    /// Evidence backing this individual semantic mapping. This is intentionally
    /// row-scoped because one enum may mix captured and reference-defined values.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub evidence: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PropertyValueProfile {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub connection: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rows: Vec<PropertyValueProfileRow>,
    #[serde(default)]
    pub evidence: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PropertyValueProfileRow {
    pub label: String,
    pub raw: i64,
    #[serde(default = "default_legal")]
    pub legal: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub aliases: Vec<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub write_store_raw: Option<i64>,
}

fn default_legal() -> bool {
    true
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PropertyValueEncoding {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sentinel: Option<SentinelMask>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub masks: Vec<SentinelMask>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StructuredTextLayout {
    pub delimiter: String,
    pub fields: Vec<StructuredTextField>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StructuredTextField {
    pub name: String,
    pub scalar: StructuredTextScalar,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum StructuredTextScalar {
    SignedInteger,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SentinelMask {
    /// Bits that identify the sentinel form. Value bits are the complement.
    pub mask: i64,
    /// Masked value that means the sentinel is present. Defaults to `mask`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub equals: Option<i64>,
    /// Stable semantic name for clients that want grouping or display policy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub meaning: Option<String>,
    /// Prefix used by the generic codec when composing/decomposing sentinel
    /// labels, e.g. `AUTO` + base label `6400` -> `AUTO 6400`.
    pub label_prefix: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum DescriptorValue {
    Int(i64),
    Str(String),
}

impl DescriptorValue {
    pub fn as_i64(&self) -> Option<i64> {
        match self {
            Self::Int(value) => Some(*value),
            Self::Str(_) => None,
        }
    }
}

impl std::fmt::Display for DescriptorValue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Int(value) => value.fmt(f),
            Self::Str(value) => value.fmt(f),
        }
    }
}

impl From<i64> for DescriptorValue {
    fn from(value: i64) -> Self {
        Self::Int(value)
    }
}

impl From<&str> for DescriptorValue {
    fn from(value: &str) -> Self {
        Self::Str(value.to_owned())
    }
}

impl From<String> for DescriptorValue {
    fn from(value: String) -> Self {
        Self::Str(value)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Descriptor {
    pub form: String,
    #[serde(default)]
    pub values: Vec<DescriptorValue>,
    /// Where the allowed value set comes from. Absent → inferred: `manifest` if
    /// `values` is non-empty, else `camera`.
    #[serde(default)]
    pub source: Option<ValueSource>,
}

impl Descriptor {
    /// Resolve the effective value-set source. Runtime-discovered (`camera`)
    /// beats manifest-declared; the manifest fills only what the camera doesn't
    /// enumerate (labels, gating, non-enumerated sets).
    pub fn effective_source(&self) -> ValueSource {
        self.source.unwrap_or(if self.values.is_empty() {
            ValueSource::Camera
        } else {
            ValueSource::Manifest
        })
    }
}

/// Layout of a composite byte-array property whose value is a bundle of
/// sub-property records — the Fuji `0xD212` live-status snapshot. The payload
/// is a self-describing **record stream** (not a fixed-offset struct), so
/// members are addressed by PTP prop code, not byte position. A consumer walks
/// records, accepting only `members`; each member's value is interpreted at
/// its payload-local declared encoding.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Payload {
    pub form: PayloadForm,
    /// Width of the leading element-count prefix, in bytes (`0xD212` → 2).
    #[serde(default)]
    pub count_width: Option<u8>,
    /// Per-record framing: the prop-code field and value field widths.
    #[serde(default)]
    pub record: Option<RecordLayout>,
    /// The prop codes the camera may emit inside this bundle (the poll
    /// allowlist). A scalar code uses `record.valueWidth`; a detailed member
    /// may override that encoding for this payload only.
    #[serde(default)]
    pub members: Vec<RecordMember>,
}

impl Payload {
    /// The `(count, code, value)` field widths, with the schema defaults
    /// (`0xD212`'s 2/2/4) filled in for omitted declarations. The single
    /// defaulting source — codecs must frame at THESE widths, never assume
    /// the D212 shape (#161). The FFI's `parse_record_stream` mirrors this
    /// defaulting; a seam test guards the mirror.
    pub fn record_widths(&self) -> (u8, u8, u8) {
        (
            self.count_width.unwrap_or(2),
            self.record.map(|r| r.code_width).unwrap_or(2),
            self.record.map(|r| r.value_width).unwrap_or(4),
        )
    }
}

/// The framing of a composite payload. Only `recordStream` exists today; the
/// closed enum reserves room for a future fixed-layout bundle without a break.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum PayloadForm {
    RecordStream,
}

/// Per-record field widths in a [`PayloadForm::RecordStream`] (`0xD212`: a
/// 2-byte LE prop code + a 4-byte LE u32-padded value).
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RecordLayout {
    pub code_width: u8,
    pub value_width: u8,
}

/// One allowed record-stream member. The scalar form preserves the original
/// fixed-width grammar; the detailed form carries a payload-local encoding and
/// optional simulator fallback without changing the global property datatype.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum RecordMember {
    Code(HexCode),
    Detailed(RecordMemberDetail),
}

impl RecordMember {
    pub fn code(&self) -> &HexCode {
        match self {
            Self::Code(code) => code,
            Self::Detailed(member) => &member.code,
        }
    }

    pub fn encoding(&self, default_width: u8) -> RecordValueEncoding {
        match self {
            Self::Code(_) => RecordValueEncoding::Fixed {
                width: default_width,
            },
            Self::Detailed(member) => member.encoding,
        }
    }

    pub fn simulator_value(&self) -> Option<&RecordValueLiteral> {
        match self {
            Self::Code(_) => None,
            Self::Detailed(member) => member.simulator_value.as_ref(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RecordMemberDetail {
    pub code: HexCode,
    pub encoding: RecordValueEncoding,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub simulator_value: Option<RecordValueLiteral>,
}

/// Wire encoding of one record-stream value. `Fixed` is a raw unsigned
/// little-endian field. `Signed` is a signed little-endian field whose declared
/// width controls sign extension. `PtpString` is the standard length-prefixed
/// UTF-16LE PTP string grammar.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum RecordValueEncoding {
    Fixed { width: u8 },
    Signed { width: u8 },
    PtpString,
}

/// Literal used only when the simulator has no mutable property state for a
/// detailed record member.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum RecordValueLiteral {
    Unsigned(u32),
    Signed(i32),
    String(String),
}

/// Where a descriptor's allowed value set is sourced.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ValueSource {
    /// The camera enumerates it at runtime (DevicePropDesc) — authoritative.
    Camera,
    /// The manifest declares it (camera doesn't report it, or needs labels/gating).
    Manifest,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Control {
    #[serde(default)]
    pub set_method: Option<String>,
    #[serde(default)]
    pub operation: Option<HexCode>,
    #[serde(default)]
    pub readback: Option<HexCode>,
}

/// Semantic control intent exposed by a connection/mode. The role lets a
/// consumer build controls without knowing property codes; the referenced
/// property still owns the wire operation and readback recipe.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ControlRole {
    Iso,
    ShutterSpeed,
    Aperture,
    ExposureBias,
    FocusArea,
}

/// Who ultimately owns the effective value after a client writes it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ControlOwner {
    Client,
    Camera,
    Body,
    ModeGated,
    Unknown,
}

/// Where the consumer verifies the effective value after a write.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ControlReadSource {
    /// Read the semantic surface's own `property` directly.
    DirectProperty,
    /// Read the control recipe's separately declared `readback` property.
    DeclaredReadback,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ControlSurfaceEntry {
    pub property: HexCode,
    pub read_source: ControlReadSource,
    pub evidence_basis: ControlEvidenceBasis,
    pub observed_effect: ControlObservedEffect,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effective_owner: Option<ControlOwner>,
}

/// How object bytes are read over a connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ObjectTransferStrategy {
    Chunked,
    WholeObject,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ObjectTransferResumePolicy {
    ByteOffset,
    RestartFromZero,
}

/// Evidence level for one object format on one transfer surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ObjectTransferFormatSupport {
    Confirmed,
    Experimental,
    Unsupported,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ObjectTransferCompletionTiming {
    LocalCommit,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ObjectTransferCompletionPolicy {
    pub action: ActionVerb,
    pub after: ObjectTransferCompletionTiming,
}

/// Connection-owned object-transfer policy. Actions continue to own the wire
/// recipe; this record declares how the consumer composes them safely.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ObjectTransferContract {
    pub strategy: ObjectTransferStrategy,
    pub resume_policy: ObjectTransferResumePolicy,
    pub read_action: ActionVerb,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completion: Option<ObjectTransferCompletionPolicy>,
    #[serde(default)]
    pub formats: BTreeMap<HexCode, ObjectTransferFormatSupport>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Workflow {
    #[serde(default)]
    pub transport: Option<String>,
    #[serde(default)]
    pub states: Vec<String>,
    #[serde(default)]
    pub transitions: Vec<serde_yaml::Value>,
    #[serde(default)]
    pub sockets: BTreeMap<String, String>,
    #[serde(default)]
    pub evidence: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Media {
    /// Object-format table (#36): PTP object-format code → metadata, so the app
    /// classifies objects (RAW/movie/vendor) from data instead of hardcoding
    /// per-vendor format literals.
    #[serde(default)]
    pub formats: BTreeMap<HexCode, MediaFormat>,
    /// Reported 32-bit `ObjectInfo.ObjectCompressedSize` sentinel. Some cameras
    /// report oversized objects at this value while exposing a separate true
    /// transfer size.
    #[serde(
        default,
        alias = "wirelessTransferCeiling",
        skip_serializing_if = "Option::is_none"
    )]
    pub object_info_size_sentinel: Option<u64>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, serde_yaml::Value>,
}

/// One object-format catalog row (#36): a PTP/vendor format code's name and
/// classification, plus an optional embedded-JPEG locator (#101) for RAW formats.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MediaFormat {
    pub name: String,
    #[serde(default)]
    pub vendor: Option<String>,
    #[serde(default)]
    pub is_raw: bool,
    #[serde(default)]
    pub is_movie: bool,
    /// Whether this object format can be handed to the OS photo library (#136):
    /// full stills (JPEG/HEIF/RAW) and movies, but not non-image PTP objects
    /// (associations, scripts). The app checks this instead of its own
    /// still/movie format tables.
    #[serde(default)]
    pub is_photos_compatible: bool,
    /// Where this RAW format's embedded full-size JPEG lives (#101). Absent for
    /// non-RAW formats and RAWs that don't embed a JPEG.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub embedded_jpeg: Option<EmbeddedJpeg>,
}

/// Byte order for reading a container's multi-byte header fields. RAW containers
/// differ: Fuji RAF is big-endian; TIFF-based RAWs are commonly little-endian.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Endian {
    Big,
    Little,
}

/// Where a RAW container's embedded full-size JPEG lives, surfaced so a client
/// can pull it with `GetPartialObject` without carrying a per-format parser. The
/// client verifies `magic` at offset 0, then reads a u32 JPEG start offset at
/// `offset_at` and a u32 length at `length_at`, both in `endian`. Fuji RAF:
/// magic `FUJIFILMCCD-RAW` at 0x00, offset at 0x54, length at 0x58, big-endian —
/// per exiftool `FujiFilm::ProcessRAF` and dcraw `parse_fuji`. The simulator
/// itself never parses this: it serves object bytes and only *describes* the
/// layout here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EmbeddedJpeg {
    /// ASCII magic identifying the container, verified at offset 0.
    pub magic: String,
    /// Byte offset of the u32 field holding the embedded JPEG's start offset.
    pub offset_at: u16,
    /// Byte offset of the u32 field holding the embedded JPEG's length.
    pub length_at: u16,
    /// Byte order of the `offset_at` / `length_at` u32 fields.
    pub endian: Endian,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Event {
    pub name: String,
    #[serde(default)]
    pub destination: Option<String>,
    #[serde(default)]
    pub evidence: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Quirk {
    pub id: String,
    #[serde(default)]
    pub applies_to: Vec<String>,
    #[serde(default)]
    pub behavior: String,
    #[serde(default)]
    pub evidence: String,
}

/// A camera mode, keyed by hierarchical path (`"Shooting/Stills"`). Capabilities
/// are inherited by child paths (prefix match). `detect` (when present) is the
/// predicate over observed props that identifies this mode.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct Mode {
    #[serde(default)]
    pub capabilities: Vec<String>,
    #[serde(default)]
    pub detect: Option<Predicate>,
    /// Simulator workflow phase this mode corresponds to (e.g. `imageImport`,
    /// `liveView`). When mode detection selects this mode, the engine enters
    /// the declared phase. Absent = detection changes the mode but not the
    /// phase.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phase: Option<String>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, serde_yaml::Value>,
}

/// A connection. Composition by id-keyed reference (decision #14): an entry is
/// EITHER an inline definition (mechanism: `kind`/`establishment`/`modes`/…) when
/// `ref` is absent, OR a `ref` to a shared definition elsewhere plus this body's
/// usage conditions (`availableWhen`/`requiresHardware`). One type serves both so
/// a definition can move from inline to a shared file with no schema change.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct Connection {
    /// If set, the mechanism is defined elsewhere under this id; the remaining
    /// fields are this body's conditions/overrides.
    #[serde(default, rename = "ref")]
    pub ref_id: Option<String>,
    /// Firmware-range availability (e.g. instax-printer: present ≤2.30, removed
    /// at 2.40). Evaluated via the version comparator.
    #[serde(default)]
    pub available_when: Option<AvailableWhen>,
    /// Hardware that must be present for this connection (e.g. the FT-XH adapter
    /// that provides XLV/HTTP on bodies without it built in).
    #[serde(default)]
    pub requires_hardware: Option<String>,
    // --- inline definition (mechanism) ---
    #[serde(default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub establishment: Option<String>,
    /// Host-owned semantic checkpoints that complete this connection after
    /// the manufacturer-index executor plan (schema §11.23).
    #[serde(default)]
    pub activities: Vec<ConnectionActivityDescriptor>,
    /// The PTP/IP establishment packet shape for this connection — the
    /// InitCommandRequest byte template (#82). When present, the engine/FFI can
    /// assemble the init bytes from manifest data alone (no client literals).
    #[serde(default)]
    pub init: Option<InitShape>,
    /// Which init/establishment template this connection uses (#81) — names an
    /// init shape (e.g. `app82`) so the app picks the establishment path by
    /// trait instead of branching on connection id. Companion to `init`.
    #[serde(default)]
    pub init_shape: Option<String>,
    /// How live-view frames arrive over this connection (#81): a continuous
    /// `stream` (reference app `app`) or a `poll` loop (`wireless-tether`).
    #[serde(default)]
    pub live_view_delivery: Option<LiveViewDelivery>,
    /// Which shutter recipe family this connection uses (#81) — the discriminator
    /// that replaces the app's per-connection shutter fork. The steps still live
    /// in `actions.shutter`.
    #[serde(default)]
    pub shutter_recipe: Option<ShutterRecipe>,
    /// The PTP/IP wire framing of this connection's command channel (#133/#140):
    /// so a consumer picks the codec from data instead of mapping the connection
    /// kind to a framing in its own code.
    #[serde(default)]
    pub command_framing: Option<WireFraming>,
    /// The wire framing of this connection's event socket, when it differs from
    /// the command channel (the Fuji `app` event socket carries USB/PIMA type-4
    /// event containers, not the compressed command framing).
    #[serde(default)]
    pub event_framing: Option<WireFraming>,
    /// Closing the active command transport may remove its listener, so a caller
    /// must not assume it can immediately redial the same endpoint as generic
    /// recovery. A manifest-authored outer connection re-establishment may create
    /// a new listener. Default false keeps reconnect behavior unchanged for
    /// connections without this constraint (#243).
    #[serde(default)]
    pub command_listener_volatile: bool,
    /// The PTP/IP sockets a consumer binds for this connection, keyed by role
    /// (command / event / live-view). Promotes the former free-form `bind` block
    /// to typed data (#140) so the app binds by role instead of hardcoding the
    /// Fuji command port + `+1`/`+2` offsets.
    #[serde(default)]
    pub bindings: Option<SocketBindings>,
    /// An optional transport-close frame for the lifecycle context named by
    /// [`TransportClose::when`] (#140). It is not a generic teardown frame and
    /// does not by itself guarantee redialability. Companion to
    /// `command_listener_volatile`.
    #[serde(default)]
    pub transport_close: Option<TransportClose>,
    /// PCSS LAN discovery/callback parameters for wireless tethering.
    #[serde(default)]
    pub knock: Option<PcssKnock>,
    /// InitFail retry tolerance observed on PCSS establishment.
    #[serde(default)]
    pub init_retries: Option<InitRetries>,
    /// Safe object-read and completion policy for this connection.
    #[serde(default)]
    pub object_transfer: Option<ObjectTransferContract>,
    /// Mode-qualified semantic controls, keyed by mode then intent role.
    #[serde(default)]
    pub control_surfaces: BTreeMap<String, BTreeMap<ControlRole, ControlSurfaceEntry>>,
    #[serde(default)]
    pub modes: Vec<String>,
    /// Mode-graph edges reachable over this connection (decision #6, §3a). An edge
    /// carries exactly one typed execution and may be `from`-qualified.
    #[serde(default)]
    pub entries: Vec<ModeEntry>,
    /// Named, parameterized step sequences that run *within* a mode (vs `entries`,
    /// which transition *between* modes). The verb namespace is closed
    /// (`ActionVerb` enum); unknown YAML keys here fail to load — same fail-fast
    /// as the Step verb allowlist. See `docs/plans/action-verbs.md`.
    #[serde(default)]
    pub actions: BTreeMap<ActionVerb, Action>,
    /// Connection-bring-up edges: from this connection, activate *another* (the
    /// BLE→WiFi-AP handover). Distinct from `entries` (mode transitions within a
    /// connection) — this is the establishment edge in the state graph.
    #[serde(default)]
    pub enables: Vec<ConnectionTransition>,
    /// Free-form bind/discovery/establishment detail (e.g. GATT characteristic
    /// UUIDs) until those are modeled / split to a private overlay.
    #[serde(flatten)]
    pub extra: BTreeMap<String, serde_yaml::Value>,
}

/// A PTP/IP socket role a consumer binds. `Command` is the control channel;
/// `Event` and `LiveView` are the derived sockets (Fuji: command `+1`/`+2`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SocketRole {
    Command,
    Event,
    LiveView,
}

/// The PTP/IP sockets a consumer binds for a connection, keyed by role (#140).
/// Resolved ports are authoritative (they come from the shipping app); a role a
/// connection lacks (e.g. `wireless-tether` has no event socket) is `None`. On the
/// Fuji `app` path `event = command + 1` and `live_view = command + 2`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct SocketBindings {
    /// Static responder host when the connection establishes a known endpoint.
    /// Absent for dynamically discovered peers such as PCSS cameras on a LAN.
    #[serde(default)]
    pub host: Option<String>,
    /// The PTP/IP command-port (control channel). Fuji default 55740.
    pub command: SocketBinding,
    /// The event socket, if this connection has one.
    #[serde(default)]
    pub event: Option<SocketBinding>,
    /// The live-view through-picture stream socket, if this connection has one.
    #[serde(default)]
    pub live_view: Option<SocketBinding>,
}

/// One socket binding. A bare port preserves the compact form for listeners
/// with no declared availability constraint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum SocketBinding {
    Port(u16),
    Descriptor(SocketBindingDescriptor),
}

impl Default for SocketBinding {
    fn default() -> Self {
        Self::Port(0)
    }
}

impl SocketBinding {
    pub fn port(&self) -> u16 {
        match self {
            Self::Port(port) => *port,
            Self::Descriptor(descriptor) => descriptor.port,
        }
    }

    pub fn available_after(&self) -> Option<&SocketAvailability> {
        match self {
            Self::Port(_) => None,
            Self::Descriptor(descriptor) => descriptor.available_after.as_ref(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SocketBindingDescriptor {
    /// The concrete TCP port for this role.
    pub port: u16,
    /// The condition that must complete before the listener is available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub available_after: Option<SocketAvailability>,
}

/// A successful camera operation that makes an auxiliary listener available.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SocketAvailability {
    /// The successful operation that makes the listener available.
    pub operation: HexCode,
}

/// A camera-status-triggered private media pull. BLE announces availability and
/// handoff state; object bytes still move through request/response PTP operations.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CameraInitiatedTransfer {
    pub trigger: CameraInitiatedTrigger,
    pub handoff: CameraInitiatedHandoff,
    /// The manifest-owned route used after a transfer monitor loses its link.
    #[serde(default)]
    pub monitor_recovery: Option<CameraInitiatedMonitorRecovery>,
    pub receive: CameraInitiatedReceive,
    #[serde(default)]
    pub evidence: Vec<String>,
}

/// Recovery route for a camera-initiated-transfer monitor. This deliberately
/// names a pre-existing generic reconnect contract rather than embedding retry
/// constants or transport probes in a consumer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum CameraInitiatedMonitorRecovery {
    SavedCameraReconnect,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CameraInitiatedTrigger {
    #[serde(rename = "match")]
    pub match_mode: TriggerMatch,
    pub states: Vec<BleStateTrigger>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum TriggerMatch {
    All,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BleStateTrigger {
    pub gatt: String,
    pub trigger_values: Vec<String>,
    #[serde(default)]
    pub baseline_values: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CameraInitiatedHandoff {
    pub connection: String,
    pub socket_role: SocketRole,
    #[serde(default)]
    pub cached_credentials_allowed: bool,
    #[serde(default)]
    pub function_launch: Option<BleLiteralWrite>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BleLiteralWrite {
    pub gatt: String,
    pub value: String,
    #[serde(default)]
    pub required: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CameraInitiatedReceive {
    pub mode: String,
    pub count: RecordMemberRef,
    pub head_index: u32,
    pub metadata: CameraInitiatedMetadata,
    pub data: CameraInitiatedData,
    pub completion: TransferCompletion,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RecordMemberRef {
    pub property: HexCode,
    pub member: HexCode,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CameraInitiatedMetadata {
    pub operation: HexCode,
    pub phases: Vec<CameraInitiatedMetadataPhase>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum CameraInitiatedMetadataPhase {
    AfterCountBeforeModeEntry,
    AfterModeEntry,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CameraInitiatedData {
    pub operation: HexCode,
    pub chunk_limit_property: HexCode,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum TransferCompletion {
    ReadToEof,
}

impl SocketBindings {
    pub fn binding_for(&self, role: SocketRole) -> Option<&SocketBinding> {
        match role {
            SocketRole::Command => Some(&self.command),
            SocketRole::Event => self.event.as_ref(),
            SocketRole::LiveView => self.live_view.as_ref(),
        }
    }

    /// The bound port for `role`, or `None` if this connection has no such socket.
    pub fn port_for(&self, role: SocketRole) -> Option<u16> {
        self.binding_for(role).map(SocketBinding::port)
    }

    /// The declared availability condition for `role`, if any.
    pub fn available_after(&self, role: SocketRole) -> Option<&SocketAvailability> {
        self.binding_for(role)?.available_after()
    }
}

/// A named byte frame the manifest can reference from other records.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SentinelFrame {
    /// Hex bytes of the frame, e.g. `08000000ffffffff`.
    pub bytes: String,
    /// Evidence id(s) backing this byte frame.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub evidence: Vec<String>,
}

/// The transport-close frame a connection sends before reopening an image-transfer
/// session (#140). The bytes are named (not inlined) and resolve through the
/// manifest's top-level `sentinels` map.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct TransportClose {
    /// Names a top-level `sentinels` entry.
    pub sentinel: String,
    /// When the consumer sends it (e.g. `before-image-transfer-reestablishment`).
    #[serde(default)]
    pub when: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PcssKnock {
    pub callback_port: u16,
    pub knock_port: u16,
    pub protocol: String,
    #[serde(default)]
    pub camera_name: Option<String>,
    /// Where the discovery datagram may be sent and which target is selected
    /// when a caller does not override it.
    pub discovery_targets: PcssDiscoveryTargets,
    /// Delay between discovery datagrams while awaiting the callback.
    #[serde(default = "default_pcss_retry_interval_ms")]
    pub retry_interval_ms: u32,
    /// Maximum discovery datagrams sent for one rendezvous attempt.
    #[serde(default = "default_pcss_max_attempts")]
    pub max_attempts: u32,
    /// Deadline for one attempt to open the advertised command endpoint.
    #[serde(default = "default_pcss_connect_timeout_ms")]
    pub connect_timeout_ms: u32,
}

fn default_pcss_retry_interval_ms() -> u32 {
    10_000
}

fn default_pcss_max_attempts() -> u32 {
    10
}

fn default_pcss_connect_timeout_ms() -> u32 {
    5_000
}

/// Manifest-authored PCSS discovery destination policy. The default must be a
/// member of the non-empty, duplicate-free supported set.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PcssDiscoveryTargets {
    pub default: PcssDiscoveryTarget,
    pub supported: Vec<PcssDiscoveryTarget>,
    /// After a broadcast callback's command endpoint or first Init transport
    /// attempt is unavailable, perform one fresh rendezvous by unicast to the
    /// callback's validated DSC.
    pub retry_discovered_unicast: bool,
}

/// Addressing mode for the byte-identical PCSS discovery datagram.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum PcssDiscoveryTarget {
    /// Send to the selected IPv4 interface's subnet-directed broadcast. The
    /// callback's DSC, validated against its peer, identifies the camera, so no
    /// camera address is required.
    SubnetBroadcast,
    /// Send directly to a caller-supplied camera IPv4 address.
    ExplicitUnicast,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct InitRetries {
    pub max: u32,
    pub backoff_ms: u32,
    /// Typed InitFail reasons that authorize a same-socket replay.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub when_reasons: Vec<HexCode>,
}

/// An establishment edge: from one connection, bring up another. Carries a named
/// `mechanism` (an establishment workflow id, e.g. the GATT credential handover)
/// and/or a `user_instruction` (some handovers are partly manual). NOT a PTP
/// `Step` sequence — establishment is GATT/OS-level, a separate concern from the
/// PTP wire actions in a `ModeEntry`.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ConnectionTransition {
    /// Target connection id this edge brings up.
    pub to: String,
    /// Named establishment mechanism/workflow (resolved elsewhere).
    #[serde(default)]
    pub mechanism: Option<String>,
    #[serde(default)]
    pub user_instruction: Option<String>,
    /// Optional target mode this establishment edge selects. Multiple edges may
    /// reach the same connection when the handoff itself carries a feature
    /// selector (legacy manufacturer app's function-launch request).
    #[serde(default)]
    pub mode: Option<String>,
    /// Fixed runtime bindings for the target establishment plan.
    #[serde(default)]
    pub params: BTreeMap<String, String>,
    #[serde(default)]
    pub requires: Option<Predicate>,
}

/// A mode-graph transition edge: how to get *into* mode `to`. `from` qualifies the
/// source (`None` = cold entry). Execution is a closed choice so PTP steps cannot
/// be accidentally combined with a manual or outer connection lifecycle.
#[derive(Debug, Clone)]
pub struct ModeEntry {
    pub to: String,
    pub from: Option<String>,
    pub execution: ModeEntryExecution,
    /// Optional semantic spans over the executable top-level step sequence.
    pub activities: Vec<ConnectionActivityDescriptor>,
    /// Optional runtime prerequisite for taking this edge.
    pub requires: Option<Predicate>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ModeEntryExecution {
    /// Ordered PTP wire steps executed on the current session.
    Ptp { steps: Vec<Step> },
    /// Leave the current connection lifecycle, establish it again, then run the
    /// target mode's cold PTP entry on a fresh session.
    ReestablishConnection(ReestablishConnection),
    /// A camera-menu or host action that cannot be driven by ptpsim.
    UserInstruction { instruction: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReestablishConnection {
    /// PTP steps that orderly leave the old session before host/network teardown.
    pub exit_steps: Vec<Step>,
    /// Fixed bindings for the connection's existing establishment plan.
    #[serde(default)]
    pub params: BTreeMap<String, String>,
}

impl ModeEntry {
    pub fn ptp_steps(&self) -> Option<&[Step]> {
        match &self.execution {
            ModeEntryExecution::Ptp { steps } => Some(steps),
            _ => None,
        }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ModeEntryWire {
    to: String,
    #[serde(default)]
    from: Option<String>,
    #[serde(default)]
    steps: Option<Vec<Step>>,
    #[serde(default)]
    reestablish_connection: Option<ReestablishConnection>,
    #[serde(default)]
    user_instruction: Option<String>,
    #[serde(default)]
    activities: Vec<ConnectionActivityDescriptor>,
    #[serde(default)]
    requires: Option<Predicate>,
}

impl<'de> Deserialize<'de> for ModeEntry {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let wire = ModeEntryWire::deserialize(deserializer)?;
        let variants = [
            wire.steps.is_some(),
            wire.reestablish_connection.is_some(),
            wire.user_instruction.is_some(),
        ]
        .into_iter()
        .filter(|present| *present)
        .count();
        if variants != 1 {
            return Err(serde::de::Error::custom(
                "mode entry requires exactly one of steps, reestablishConnection, or userInstruction",
            ));
        }
        let execution = if let Some(steps) = wire.steps {
            ModeEntryExecution::Ptp { steps }
        } else if let Some(reestablish) = wire.reestablish_connection {
            ModeEntryExecution::ReestablishConnection(reestablish)
        } else {
            ModeEntryExecution::UserInstruction {
                instruction: wire.user_instruction.expect("variant count checked"),
            }
        };
        Ok(Self {
            to: wire.to,
            from: wire.from,
            execution,
            activities: wire.activities,
            requires: wire.requires,
        })
    }
}

impl Serialize for ModeEntry {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        use serde::ser::SerializeStruct;

        let fields = 2
            + usize::from(self.from.is_some())
            + usize::from(!self.activities.is_empty())
            + usize::from(self.requires.is_some());
        let mut out = serializer.serialize_struct("ModeEntry", fields)?;
        out.serialize_field("to", &self.to)?;
        if let Some(from) = &self.from {
            out.serialize_field("from", from)?;
        }
        match &self.execution {
            ModeEntryExecution::Ptp { steps } => out.serialize_field("steps", steps)?,
            ModeEntryExecution::ReestablishConnection(reestablish) => {
                out.serialize_field("reestablishConnection", reestablish)?;
            }
            ModeEntryExecution::UserInstruction { instruction } => {
                out.serialize_field("userInstruction", instruction)?;
            }
        }
        if !self.activities.is_empty() {
            out.serialize_field("activities", &self.activities)?;
        }
        if let Some(requires) = &self.requires {
            out.serialize_field("requires", requires)?;
        }
        out.end()
    }
}

/// Named verbs the app invokes on a connection while in a mode. Closed
/// vocabulary; new verbs require a schema PR (same fail-fast policy as
/// `Step`). YAML uses camelCase (`shutter`, `getObject`, …).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ActionVerb {
    /// Fire the shutter. Wire bytes are connection-specific (`app` = bare
    /// `0x100E + 0x9022`; `wireless-tether` = 3-beat `0xD039 + 0x100E`).
    Shutter,
    /// Enumerate object handles on the SD card.
    EnumerateObjects,
    /// Read object metadata (PTP ObjectInfo) for a handle.
    GetObjectInfo,
    /// Read the thumbnail JPEG for a handle.
    GetThumb,
    /// Read the whole object (image bytes) for a handle.
    GetObject,
    /// Delete an object by handle.
    DeleteObject,
    /// Tap-to-AF: `0x9026 LockS1Lock(packed area)` then await the lock result
    /// (#35). The packed focus-area u32 is an app-supplied runtime slot.
    AutofocusLock,
    /// Release the AF lock: `0x9027 UnlockS1Lock` (#35).
    AutofocusRelease,
    /// The full image-transfer choreography (#46): arm → enumerate → for-each
    /// handle { get size → chunk-download until exhausted } → idle. The loop
    /// lives in the manifest; the reference executor walks it end-to-end.
    ImportObjects,
    /// Read the camera's DeviceInfo dataset (standard `0x1001`) — the actual
    /// body's identity (unit serial = the saved-camera merge key, #173) plus
    /// its supported-ops surface. Not mode-gated: valid whenever a session is
    /// open.
    ReadDeviceInfo,
    /// Start connection-specific live-view delivery. The manifest owns the
    /// operation parameters because PCSS and reference app do not share request shape.
    StartLiveView,
    /// Request exactly one frame from a connection whose live-view delivery is
    /// polled. The manifest owns the operation and response payload shape.
    PollLiveView,
    /// Stop connection-specific live-view delivery. The manifest owns the
    /// operation parameters and any connection-scoped transaction semantics.
    StopLiveView,
    /// One session-maintenance keepalive iteration. The caller owns cadence;
    /// the manifest only names the wire writes that keep the session current.
    Keepalive,
}

impl ActionVerb {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Shutter => "shutter",
            Self::EnumerateObjects => "enumerateObjects",
            Self::GetObjectInfo => "getObjectInfo",
            Self::GetThumb => "getThumb",
            Self::GetObject => "getObject",
            Self::DeleteObject => "deleteObject",
            Self::AutofocusLock => "autofocusLock",
            Self::AutofocusRelease => "autofocusRelease",
            Self::ImportObjects => "importObjects",
            Self::ReadDeviceInfo => "readDeviceInfo",
            Self::StartLiveView => "startLiveView",
            Self::PollLiveView => "pollLiveView",
            Self::StopLiveView => "stopLiveView",
            Self::Keepalive => "keepalive",
        }
    }
}

impl std::str::FromStr for ActionVerb {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "shutter" => Ok(Self::Shutter),
            "enumerateObjects" => Ok(Self::EnumerateObjects),
            "getObjectInfo" => Ok(Self::GetObjectInfo),
            "getThumb" => Ok(Self::GetThumb),
            "getObject" => Ok(Self::GetObject),
            "deleteObject" => Ok(Self::DeleteObject),
            "autofocusLock" => Ok(Self::AutofocusLock),
            "autofocusRelease" => Ok(Self::AutofocusRelease),
            "importObjects" => Ok(Self::ImportObjects),
            "readDeviceInfo" => Ok(Self::ReadDeviceInfo),
            "startLiveView" => Ok(Self::StartLiveView),
            "pollLiveView" => Ok(Self::PollLiveView),
            "stopLiveView" => Ok(Self::StopLiveView),
            "keepalive" => Ok(Self::Keepalive),
            other => Err(format!("unknown action verb '{other}'")),
        }
    }
}

/// How live-view frames are delivered over a connection (#81 per-connection
/// trait). `Stream` = a continuous frame channel (reference app `app`); `Poll` = the app
/// repeatedly issues `poll_op` (`wireless-tether`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LiveViewDelivery {
    pub kind: LiveViewDeliveryKind,
    /// The op the app polls when `kind = poll` (e.g. `0x9018`).
    #[serde(default)]
    pub poll_op: Option<HexCode>,
}

/// Live-view delivery mode (closed vocabulary — a new value needs a schema PR).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum LiveViewDeliveryKind {
    Stream,
    Poll,
}

/// Which shutter recipe family a connection uses (#81). The actual steps live in
/// `actions.shutter`; this is the discriminator that replaces the app's
/// per-connection shutter branch. Closed vocabulary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ShutterRecipe {
    /// `app`: the bare `0x100E` + `0x9022` postview take-cycle.
    AppPostview,
    /// `wireless-tether`: the 3-beat `0xD039` + `0x100E` virtual shutter.
    WirelessTether3Beat,
}

/// A PTP/IP wire framing (#133/#140). Declared per connection/channel so a
/// consumer selects the byte codec from manifest data — never by mapping the
/// connection kind to a framing in its own code. Closed vocabulary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum WireFraming {
    /// ISO-15740 standard PTP/IP framing (8-byte header, DataPhaseInfo present).
    Standard,
    /// The "compressed" command framing: a PIMA-style 12-byte container without the
    /// standard PTP/IP wrapper — single-frame data phase, no event type. Named for
    /// the narrower header, not actual compression; wire-verified on Fuji reference app
    /// channels but a schema-generic framing kind.
    Compressed,
    /// The PIMA/USB container framing (12-byte header, type 4 = event).
    Usb,
}

/// Declared side-effects an `Action` produces — the app reads `Action.triggers`
/// to plan UX (poll object queues, show progress, etc.) without connection-
/// specific knowledge. Engine does NOT act on this; pure declaration.
///
/// Closed vocabulary: exactly one variant field is set per `ActionEffect`,
/// and unknown fields fail to parse (`deny_unknown_fields`). Same shape as
/// `Step` (one-action-per-mapping) so YAML stays uniform across the
/// manifest. Adding a new effect requires a schema PR (new `Option` field
/// + variant struct).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ActionEffect {
    /// Camera makes between `min` and `max` captured objects available after
    /// `Shutter`. Cardinality is intrinsically variable: PCSS tether produces
    /// 1-3 per press depending on the user's JPEG / HEIF / RAW format selection;
    /// burst and bracket modes raise the max further. The app reads `max` as
    /// the upper bound for queue polling / progress UI, and may early-exit when
    /// it knows the exact count from its own format-selection state.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub objects_available: Option<ObjectsAvailable>,
    /// Camera emits a postview / capture-complete event after `Shutter`
    /// (reference app `app` path: `0x9022` cleanup once `0xD212` clears). YAML body
    /// is the empty mapping: `postviewEvent: {}`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub postview_event: Option<PostviewEvent>,
    /// Continuous frame delivery starts (e.g. live-view through-stream on the
    /// `app` connection after `0x101C InitiateOpenCapture`). YAML body is
    /// the empty mapping: `liveViewStream: {}`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub live_view_stream: Option<LiveViewStream>,
}

impl ActionEffect {
    /// Whether exactly one variant field is set (structural lint, like
    /// `Step::is_well_formed`).
    pub fn is_well_formed(&self) -> bool {
        let n = [
            self.objects_available.is_some(),
            self.postview_event.is_some(),
            self.live_view_stream.is_some(),
        ]
        .into_iter()
        .filter(|b| *b)
        .count();
        n == 1
    }
}

/// Parameters for the `ObjectsAvailable` effect: bounded count of captured
/// objects the camera will make available after `Shutter`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ObjectsAvailable {
    pub min: u32,
    pub max: u32,
}

/// Marker for the `PostviewEvent` effect (no fields).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PostviewEvent {}

/// Marker for the `LiveViewStream` effect (no fields).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LiveViewStream {}

/// One shared action identity with explicit execution-role bindings. Triggers
/// and evidence are declarative and belong to the identity, never to one role.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Action {
    /// Mode this action is valid in (gating; same path-prefix match as
    /// `Operation.modes`). Empty = not mode-gated: valid in any mode while
    /// the connection is up (e.g. `readDeviceInfo`).
    pub mode: String,
    /// Real-camera execution through the Rust-owned PTP executor.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub initiator: Option<ActionInitiator>,
    /// Optional simulator mutation/replay proof for the same action identity.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub responder: Option<ActionResponder>,
    /// Post-conditions the camera produces after this action completes —
    /// the app plans UX around them without connection-specific knowledge.
    #[serde(default)]
    pub triggers: Vec<ActionEffect>,
    #[serde(default)]
    pub evidence: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ActionInitiator {
    /// Runtime slots the caller binds. A bare name is shorthand for a required
    /// `u64`; the expanded form also supports strings and optional values.
    #[serde(default)]
    pub params: Vec<ActionInitiatorParameter>,
    #[serde(default)]
    pub steps: Vec<Step>,
    #[serde(default)]
    pub activities: Vec<ConnectionActivityDescriptor>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ActionInitiatorParameter {
    Shorthand(String),
    Expanded(ActionInitiatorParameterDeclaration),
}

impl ActionInitiatorParameter {
    pub fn normalized(&self) -> ActionInitiatorParameterDeclaration {
        match self {
            Self::Shorthand(name) => ActionInitiatorParameterDeclaration {
                name: name.clone(),
                kind: ActionInitiatorParameterKind::U64,
                required: true,
            },
            Self::Expanded(declaration) => declaration.clone(),
        }
    }

    pub fn name(&self) -> &str {
        match self {
            Self::Shorthand(name) => name,
            Self::Expanded(declaration) => &declaration.name,
        }
    }
}

impl PartialEq<String> for ActionInitiatorParameter {
    fn eq(&self, other: &String) -> bool {
        self.name() == other
    }
}

impl PartialEq<&str> for ActionInitiatorParameter {
    fn eq(&self, other: &&str) -> bool {
        self.name() == *other
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ActionInitiatorParameterDeclaration {
    pub name: String,
    pub kind: ActionInitiatorParameterKind,
    #[serde(default = "default_true")]
    pub required: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ActionInitiatorParameterKind {
    U64,
    String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ActionResponder {
    #[serde(default)]
    pub params: Vec<ActionParameter>,
    pub mutation: ResponderMutation,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ActionParameter {
    pub name: String,
    pub kind: ActionParameterKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max: Option<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ActionParameterKind {
    U32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
pub enum ResponderMutation {
    EnqueueObjects {
        count_param: String,
    },
    PropertyTransition {
        target: HexCode,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        initial: Option<i64>,
        terminal: PropertyTransitionTerminal,
        #[serde(default)]
        settle_after_polls: u32,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
pub enum PropertyTransitionTerminal {
    Fixed { value: i64 },
    Parameter { parameter: String },
}

impl Action {
    pub fn initiator(&self) -> Option<&ActionInitiator> {
        self.initiator.as_ref()
    }
}

/// One wire action in a mode-entry sequence. A **closed step vocabulary** (not a
/// script): exactly one action field is set; `value` parameterizes `setProp`;
/// `repeat` (default 1) covers bounded loops like the live-view `902B ×4`.
/// Runtime control flow is limited to the closed `if`/`retry`/`loop` forms below;
/// manifests cannot inject arbitrary scripting hooks.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Step {
    /// `SetDevicePropValue prop = value` (width from the property's `type`).
    #[serde(default)]
    pub set_prop: Option<HexCode>,
    /// `GetDevicePropValue prop` (discard / negotiate).
    #[serde(default)]
    pub get_prop: Option<HexCode>,
    /// Read `prop`, then write the same value back (the live-view `0xdf2a` echo).
    #[serde(default)]
    pub read_echo: Option<HexCode>,
    /// Send operation `op` (e.g. `0x101c` InitiateOpenCapture).
    #[serde(default)]
    pub send_op: Option<HexCode>,
    /// Ask the host-owned transport to open an auxiliary PTP/IP channel at this
    /// exact point in the manifest-authored sequence. The command channel is
    /// established before plan execution; event and live-view listeners may not
    /// exist until a preceding camera operation has completed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub open_channel: Option<SocketRole>,
    /// Re-establish the PTP/IP session in-place: CloseSession 0x1003 → 8B
    /// `0xffffffff` sentinel → new TCP socket to the connection's command port →
    /// cached 82B InitCmdReq → InitCmdAck → OpenSession sid=1. Engine reuses the
    /// connection's cached identity, so the action carries no params —
    /// `reopenSession: {}`. A reference-app Get→Take trace exhibits this shape,
    /// but GFX100 II fw 2.30 refuses the manifest executor's reconnect, so that
    /// body's canonical Get→Take edge stays in-session.
    #[serde(default)]
    pub reopen_session: Option<ReopenSession>,
    /// End the PTP/IP session, optionally using the connection's declared
    /// orderly transport-close frame (#82/#244).
    #[serde(default)]
    pub close_session: Option<CloseSession>,
    /// Value for `set_prop`: a legacy integer literal or an action runtime slot.
    #[serde(default)]
    pub value: Option<SetPropValue>,
    /// Operation parameters for `send_op`: literals, or a named runtime slot the
    /// I/O-owning client binds (e.g. the live-view open-capture txid for `0x1018`).
    #[serde(default)]
    pub params: Vec<StepParam>,
    /// Bind scalar values from this step's successful data/response phase into
    /// the PTP-IP action scope for later conditionals, loops, or params.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub captures: Vec<Capture>,
    /// Start tracking a named simulator sequence gate at this step. The marker
    /// does not change what the consumer sends; the engine uses it to build a
    /// negative oracle for order-sensitive preconditions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub starts_gate: Option<String>,
    /// Satisfy a named simulator sequence gate when this step and all steps
    /// since the matching `startsGate` have succeeded in order.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completes_gate: Option<String>,
    /// If true, a non-OK PTP *response* to this step is acceptable — the client
    /// logs it and continues (advisory setup like `0xdf28`/`0xd226`/`0x9054` that
    /// some bodies/responders reject). Only a *transport* failure aborts.
    #[serde(default)]
    pub tolerant: bool,
    /// Bounded repeat count (default 1).
    #[serde(default = "one")]
    pub repeat: u32,
    /// Poll `source` until `until` holds over observed property values, running
    /// `on_each` each unsatisfied iteration — the PTP-IP await/poll-until verb
    /// (#29 postview, #42 AF). Mirrors the BLE `bleAwaitUntil` contract (§11.15):
    /// a condition-wait, NOT a bounded loop (that's `repeat`) and NOT a for-each
    /// over a collection (a distinct future construct). See [`AwaitUntil`].
    #[serde(default)]
    pub await_until: Option<AwaitUntil>,
    /// Replay a logical PTP sequence only after explicitly selected failures:
    /// named non-OK response codes and/or whole failure classes. Transport
    /// failures and unselected failures escape.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry: Option<StepRetry>,
    /// A closed declarative loop (#46): `forEach` over a captured collection (each
    /// element binds a runtime slot), or `chunk`-by-size over the current object
    /// (the executor owns the offset/length cursor). The sanctioned for-each
    /// construct the `await_until` doc defers to; a sibling of `repeat`/`await_until`,
    /// not a scripting hook. Skipped when absent to keep the consolidated diff small.
    #[serde(default, rename = "loop", skip_serializing_if = "Option::is_none")]
    pub r#loop: Option<Loop>,
    #[serde(default, rename = "if", skip_serializing_if = "Option::is_none")]
    pub if_step: Option<IfStep>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum SetPropValue {
    Literal(i64),
    Runtime(RuntimeSetPropValue),
}

impl SetPropValue {
    pub const fn literal(&self) -> Option<i64> {
        match self {
            Self::Literal(value) => Some(*value),
            Self::Runtime(_) => None,
        }
    }
}

impl From<i64> for SetPropValue {
    fn from(value: i64) -> Self {
        Self::Literal(value)
    }
}

impl PartialEq<i64> for SetPropValue {
    fn eq(&self, other: &i64) -> bool {
        self.literal() == Some(*other)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RuntimeSetPropValue {
    pub runtime: String,
    #[serde(default)]
    pub if_missing: MissingRuntimeValue,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum MissingRuntimeValue {
    #[default]
    Error,
    Skip,
}

impl Step {
    pub fn is_sequence_gate_matchable(&self) -> bool {
        if self.set_prop.is_some() || self.get_prop.is_some() {
            return true;
        }
        self.send_op.is_some()
            && self
                .params
                .iter()
                .all(|p| matches!(p, StepParam::Literal(_)))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Capture {
    pub bind: String,
    #[serde(rename = "as")]
    pub source: CaptureSource,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CaptureSource {
    #[serde(rename = "objectInfoCompressedSize")]
    ObjectInfoCompressedSize,
    #[serde(rename = "propValue")]
    PropValue,
    #[serde(rename = "u32Le")]
    U32Le,
    #[serde(rename = "u64Le")]
    U64Le,
    /// Standard PTP array framing: little-endian u32 count followed by u32 items.
    #[serde(rename = "ptpU32Array")]
    PtpU32Array,
    /// The transaction id allocated to the successful `sendOp` request.
    #[serde(rename = "transactionId")]
    TransactionId,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct IfStep {
    pub slot: String,
    pub equals: u64,
    #[serde(default, rename = "then")]
    pub then_steps: Vec<Step>,
    /// Defaults to empty so existing `if` steps retain their skip-on-false
    /// behavior. An explicit branch lets a manifest select exactly one wire
    /// mutation from a captured value without issuing a speculative write.
    #[serde(default, rename = "else")]
    pub else_steps: Vec<Step>,
}

/// Failure-selected retry for a logical PTP sequence (§11.21). At least one of
/// `when_response_codes`/`when_failure_classes` must be non-empty.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct StepRetry {
    pub steps: Vec<Step>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub when_response_codes: Vec<HexCode>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub when_failure_classes: Vec<RetryFailureClass>,
    pub max_attempts: u32,
    #[serde(default)]
    pub retry_delay_ms: u32,
}

/// A whole class of step failure a `retry` may select (§11.21). Closed
/// vocabulary; `decode` is the only member: the step's PTP response was OK but
/// its data payload failed to decode. Shape/contract errors and transport
/// failures are never selectable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum RetryFailureClass {
    Decode,
}

/// Where a PTP-IP `awaitUntil` observes (§11.16): a property `poll` or an `event`
/// push. In YAML it's a single-entry mapping — `poll: <hex>` or `event: { code:
/// <hex>, thenPoll: <hex>? }`. Both `Serialize` and `Deserialize` are hand-written
/// to that exact shape: a derived externally-tagged `Serialize` emits a YAML tag
/// (`!event`) that the deserializer can't read, so the generator's `to_yaml →
/// from_yaml` consolidation round-trip would break. (Same shape as the BLE
/// grammar's `read`/`notify` source.)
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AwaitSource {
    /// Poll a property each iteration (`GetDevicePropValue`) — the #49 default.
    /// `until` evaluates over the accumulated [`crate::predicate::PropView`].
    Poll { prop: HexCode },
    /// Await a completion/lifecycle event push (the camera's `0xC0xx` channel),
    /// then re-poll `then_poll` until `until` holds or the budget runs out
    /// (#54 hybrid, re-poll semantics per #185). The event acknowledges the
    /// operation but does NOT guarantee the value has settled — fw02.30 fires
    /// 0xC005 ~100ms after LockS1Lock while 0xD209 still reads pre-settle
    /// (client application#157) — so consumers pace post-event reads by `interval_ms`
    /// within `timeout_ms`. `then_poll: None` = event arrival alone satisfies
    /// `until` over the existing scope (nothing to re-read; single evaluation).
    Event {
        code: HexCode,
        then_poll: Option<HexCode>,
    },
}

impl serde::Serialize for AwaitSource {
    /// Mirror the hand-written `Deserialize`: a single-entry mapping keyed by the
    /// variant, NOT serde's externally-tagged `!event` YAML tag (which the
    /// deserializer rejects). Keeps the generator's consolidation round-trip valid.
    fn serialize<S>(&self, s: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        use serde::ser::SerializeMap;
        let mut map = s.serialize_map(Some(1))?;
        match self {
            AwaitSource::Poll { prop } => map.serialize_entry("poll", prop)?,
            AwaitSource::Event { code, then_poll } => {
                #[derive(serde::Serialize)]
                #[serde(rename_all = "camelCase")]
                struct Body<'a> {
                    code: &'a str,
                    #[serde(skip_serializing_if = "Option::is_none")]
                    then_poll: Option<&'a str>,
                }
                map.serialize_entry(
                    "event",
                    &Body {
                        code,
                        then_poll: then_poll.as_deref(),
                    },
                )?;
            }
        }
        map.end()
    }
}

impl<'de> serde::Deserialize<'de> for AwaitSource {
    /// YAML form: a single-entry mapping — `poll: <hex>` (bare string) or
    /// `event: { code: <hex>, thenPoll: <hex>? }`. Mirrors the BLE `AwaitSource`
    /// `read`/`notify` dispatch.
    fn deserialize<D>(d: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        use serde::de::Error;
        let mapping = serde_yaml::Mapping::deserialize(d)?;
        if mapping.len() != 1 {
            return Err(D::Error::custom(format!(
                "awaitUntil source must be a single-entry mapping (got {} keys)",
                mapping.len()
            )));
        }
        let (key_v, body) = mapping.into_iter().next().unwrap();
        let key = key_v
            .as_str()
            .ok_or_else(|| D::Error::custom("awaitUntil source key must be a string"))?
            .to_string();
        match key.as_str() {
            "poll" => {
                let prop = body
                    .as_str()
                    .ok_or_else(|| D::Error::custom("poll: <hex> string required"))?
                    .to_string();
                Ok(AwaitSource::Poll { prop })
            }
            "event" => {
                #[derive(serde::Deserialize)]
                #[serde(rename_all = "camelCase", deny_unknown_fields)]
                struct E {
                    code: String,
                    #[serde(default)]
                    then_poll: Option<String>,
                }
                let e: E = serde_yaml::from_value(body)
                    .map_err(|err| D::Error::custom(format!("event: {err}")))?;
                Ok(AwaitSource::Event {
                    code: e.code,
                    then_poll: e.then_poll,
                })
            }
            other => Err(D::Error::custom(format!(
                "unknown awaitUntil source '{other}' (allowlist: poll, event)"
            ))),
        }
    }
}

/// The PTP-IP await/poll-until step body (§11.16 contract, mirrored from the BLE
/// grammar). [`source`](Self::source) is either a property `poll` or an `event`
/// push (see [`AwaitSource`]). For a poll, each `GetDevicePropValue` is itself the
/// capture: the typed value lands in the observed [`crate::predicate::PropView`]
/// keyed by prop code. `until` is the PTP [`Predicate`] over that view; `mask`
/// handles `0xd212`-style composite sub-fields.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AwaitUntil {
    /// Where to observe: a property `poll` (loop) or an `event` push (single-shot).
    pub source: AwaitSource,
    /// Satisfied when this predicate over observed values holds.
    pub until: Predicate,
    /// Steps run each iteration when `until` is NOT yet satisfied, before the
    /// next poll. Empty = a pure poll.
    #[serde(default)]
    pub on_each: Vec<Step>,
    /// Dispatcher wall-clock budget; the step fails (tolerant-aware) if `until`
    /// isn't met before it elapses. The reference executor models this as a
    /// deterministic iteration cap (the §11.15 analogue).
    pub timeout_ms: u32,
    /// Poll cadence (the dispatcher sleeps between polls), for both the Poll
    /// source and Event-source post-event re-polls. 0 = dispatcher default.
    #[serde(default)]
    pub interval_ms: u32,
}

/// A closed, declarative loop control-flow construct (#46) — the sanctioned
/// for-each the `await_until` doc defers to. NOT a scripting language: exactly two
/// shapes, the executor owns every cursor/offset advance, the author declares only
/// policy. Each loop runs under a deterministic iteration cap (the §11.15 analogue
/// of `await_until`'s timeout). YAML is a single-entry mapping — `forEach: {...}`
/// or `chunk: {...}` — hand-(de)serialized like [`AwaitSource`] so the generator's
/// consolidation round-trip survives (serde's `!forEach` tag can't be reparsed).
#[derive(Debug, Clone)]
pub enum Loop {
    /// Iterate a named collection captured by an earlier step, binding each
    /// element into the runtime slot `bind` for the body's `StepParam::Runtime`
    /// references. Collection acquisition is explicit wire I/O; this loop never
    /// reads a property and therefore cannot replay its body through read retry.
    ForEach {
        collection: String,
        bind: String,
        body: Vec<Step>,
    },
    /// Walk the current object in `size`-byte windows. The executor owns the
    /// cursor: `offset` starts at 0 and advances by the window it just bound,
    /// `length` is `min(total - offset, size)`, terminating when `offset` reaches
    /// `total`. `total` names a scope slot (e.g. `objectSize`) captured from the
    /// real `0x1008` ObjectInfo — there is no author-written arithmetic.
    Chunk {
        total: String,
        size: ChunkSize,
        offset_bind: String,
        length_bind: String,
        body: Vec<Step>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ChunkSize {
    Literal(u32),
    Runtime { runtime: String },
}

impl ChunkSize {
    pub fn literal(value: u32) -> Self {
        Self::Literal(value)
    }
}

impl serde::Serialize for Loop {
    /// Mirror the hand-written `Deserialize`: a single-entry mapping keyed by the
    /// variant, not serde's externally-tagged YAML tag. Keeps the generator's
    /// consolidation round-trip valid (same contract as [`AwaitSource`]).
    fn serialize<S>(&self, s: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        use serde::ser::SerializeMap;
        let mut map = s.serialize_map(Some(1))?;
        match self {
            Loop::ForEach {
                collection,
                bind,
                body,
            } => {
                #[derive(serde::Serialize)]
                #[serde(rename_all = "camelCase")]
                struct Body<'a> {
                    #[serde(rename = "in")]
                    collection: &'a str,
                    bind: &'a str,
                    body: &'a [Step],
                }
                map.serialize_entry(
                    "forEach",
                    &Body {
                        collection: collection.as_str(),
                        bind: bind.as_str(),
                        body: body.as_slice(),
                    },
                )?;
            }
            Loop::Chunk {
                total,
                size,
                offset_bind,
                length_bind,
                body,
            } => {
                #[derive(serde::Serialize)]
                #[serde(rename_all = "camelCase")]
                struct Body<'a> {
                    total: &'a str,
                    size: &'a ChunkSize,
                    offset_bind: &'a str,
                    length_bind: &'a str,
                    body: &'a [Step],
                }
                map.serialize_entry(
                    "chunk",
                    &Body {
                        total: total.as_str(),
                        size,
                        offset_bind: offset_bind.as_str(),
                        length_bind: length_bind.as_str(),
                        body: body.as_slice(),
                    },
                )?;
            }
        }
        map.end()
    }
}

impl<'de> serde::Deserialize<'de> for Loop {
    /// YAML form: a single-entry mapping — `forEach: { in: <hex>, bind: <slot>,
    /// body: [...] }` or `chunk: { total: <slot>, size: <u32>, offsetBind: <slot>,
    /// lengthBind: <slot>, body: [...] }`. Mirrors [`AwaitSource`]'s dispatch.
    fn deserialize<D>(d: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        use serde::de::Error;
        let mapping = serde_yaml::Mapping::deserialize(d)?;
        if mapping.len() != 1 {
            return Err(D::Error::custom(format!(
                "loop must be a single-entry mapping (got {} keys)",
                mapping.len()
            )));
        }
        let (key_v, body) = mapping.into_iter().next().unwrap();
        let key = key_v
            .as_str()
            .ok_or_else(|| D::Error::custom("loop key must be a string"))?
            .to_string();
        match key.as_str() {
            "forEach" => {
                #[derive(serde::Deserialize)]
                #[serde(rename_all = "camelCase", deny_unknown_fields)]
                struct F {
                    #[serde(rename = "in")]
                    collection: String,
                    bind: String,
                    #[serde(default)]
                    body: Vec<Step>,
                }
                let f: F = serde_yaml::from_value(body)
                    .map_err(|err| D::Error::custom(format!("forEach: {err}")))?;
                Ok(Loop::ForEach {
                    collection: f.collection,
                    bind: f.bind,
                    body: f.body,
                })
            }
            "chunk" => {
                #[derive(serde::Deserialize)]
                #[serde(rename_all = "camelCase", deny_unknown_fields)]
                struct C {
                    total: String,
                    size: ChunkSize,
                    offset_bind: String,
                    length_bind: String,
                    #[serde(default)]
                    body: Vec<Step>,
                }
                let c: C = serde_yaml::from_value(body)
                    .map_err(|err| D::Error::custom(format!("chunk: {err}")))?;
                Ok(Loop::Chunk {
                    total: c.total,
                    size: c.size,
                    offset_bind: c.offset_bind,
                    length_bind: c.length_bind,
                    body: c.body,
                })
            }
            other => Err(D::Error::custom(format!(
                "unknown loop kind '{other}' (allowlist: forEach, chunk)"
            ))),
        }
    }
}

/// Marker for the `reopen_session` action (empty body in YAML).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReopenSession {}

/// `closeSession` action: end the PTP/IP session. `transportClose: true` uses the
/// connection's manifest-declared orderly transport-close frame instead of a
/// bare TCP close. It does not promise that the endpoint remains redialable.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CloseSession {
    #[serde(default)]
    pub transport_close: bool,
}

/// The PTP/IP InitCommandRequest wire shape as manifest data: identity slots
/// (resolved via `values:`), fixed field widths, and evidence for the declared
/// shape. #82.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct InitShape {
    /// Named-value refs for the identity slots, resolved via `values:`.
    pub identity: InitIdentity,
    /// Fixed width of the UTF-16LE friendly-name field, in bytes.
    #[serde(default)]
    pub name_field_byte_count: u32,
    /// Evidence ids backing the declared init shape.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub evidence: Vec<String>,
    /// Optional `values:` key naming the responder GUID an init ack must carry.
    /// legacy manufacturer app validates this fixed identity before opening a session.
    #[serde(default)]
    pub expected_responder_guid: Option<String>,
}

/// Named-value refs for the [`InitShape`] identity slots.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct InitIdentity {
    /// `values:` key naming the initiator GUID (e.g. `initiatorGuid`).
    pub guid: String,
    /// `values:` key naming the friendly name (e.g. `initFriendlyName`).
    pub friendly_name: String,
    /// Optional `values:` key naming the route-selected local IPv4 address.
    /// Present for legacy manufacturer app's 82-byte request; absent for reference app.
    #[serde(default)]
    pub client_ipv4: Option<String>,
}

/// A `send_op` parameter: a literal, or a **named runtime slot** the client fills
/// from its own session state. Declarative binding (cf. value-policy `from-pairing`),
/// NOT a computed variable — there is no arithmetic, branching, or looping over it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum StepParam {
    Literal(u32),
    Runtime {
        runtime: String,
        #[serde(default, skip_serializing_if = "is_zero")]
        shift: u32,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        mask: Option<u64>,
    },
}

fn one() -> u32 {
    1
}

fn default_true() -> bool {
    true
}

fn is_zero(v: &u32) -> bool {
    *v == 0
}

impl Step {
    /// Whether exactly one action field is set.
    pub fn is_well_formed(&self) -> bool {
        let n = [
            self.set_prop.is_some(),
            self.get_prop.is_some(),
            self.read_echo.is_some(),
            self.send_op.is_some(),
            self.open_channel.is_some(),
            self.reopen_session.is_some(),
            self.close_session.is_some(),
            self.await_until.is_some(),
            self.retry.is_some(),
            self.r#loop.is_some(),
            self.if_step.is_some(),
        ]
        .into_iter()
        .filter(|b| *b)
        .count();
        n == 1
    }
}

/// A condition under which a connection is available on a body.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct AvailableWhen {
    #[serde(default)]
    pub firmware: Option<VersionCond>,
}

impl AvailableWhen {
    /// Does this condition hold for `firmware` under `scheme`? An absent firmware
    /// condition is unconditionally available.
    pub fn matches(&self, firmware: &str, scheme: VersionScheme) -> bool {
        self.firmware
            .as_ref()
            .is_none_or(|c| c.matches(firmware, scheme))
    }
}

/// A firmware comparison. `eq` is exact-string (identity); `lt`/`le`/`gt`/`ge`
/// use the version comparator. All present bounds must hold (conjunction).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct VersionCond {
    #[serde(default)]
    pub eq: Option<String>,
    #[serde(default)]
    pub lt: Option<String>,
    #[serde(default)]
    pub le: Option<String>,
    #[serde(default)]
    pub gt: Option<String>,
    #[serde(default)]
    pub ge: Option<String>,
}

impl VersionCond {
    /// **Fail-soft:** an ordered bound against an unparseable version fails
    /// (returns `false`) rather than panicking — a connection is never enabled
    /// under a firmware it can't be ordered against.
    pub fn matches(&self, fw: &str, scheme: VersionScheme) -> bool {
        use std::cmp::Ordering::*;
        if let Some(b) = &self.eq {
            if fw != b {
                return false;
            }
        }
        if let Some(b) = &self.lt {
            if compare(fw, b, scheme) != Some(Less) {
                return false;
            }
        }
        if let Some(b) = &self.le {
            if !matches!(compare(fw, b, scheme), Some(Less | Equal)) {
                return false;
            }
        }
        if let Some(b) = &self.gt {
            if compare(fw, b, scheme) != Some(Greater) {
                return false;
            }
        }
        if let Some(b) = &self.ge {
            if !matches!(compare(fw, b, scheme), Some(Greater | Equal)) {
                return false;
            }
        }
        true
    }
}

/// How a named value is determined. The engine resolves `generated`/`fromPairing`
/// at runtime; `fixed` is the literal. Tagged by a `type` field in YAML, e.g.
/// `{ type: fixed, value: "..." }`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum ValuePolicy {
    Fixed {
        value: serde_yaml::Value,
    },
    Generated {
        scheme: String,
        #[serde(default)]
        persist: bool,
    },
    FromPairing {
        source: String,
    },
    /// Client-derived from a runtime slot the host fills from its own session
    /// state (e.g. the BLE-registered device name). `runtime` names the SAME slot
    /// the establishment plan writes (e.g. `terminalName` → the `deviceNameString`
    /// BLE write), so the PTP/IP friendly name and the BLE device name are one
    /// value by construction — never a literal. The camera silently drops
    /// `InitCommandRequest` if the two channels disagree (device 2026-06-28, #109).
    ClientDerived {
        runtime: String,
    },
}

/// Manufacturer-tier defaults (`fuji.yaml`) — shared by every body of a make and
/// genuinely NOT a camera (no model/fw). Holds the version-ordering scheme,
/// initiator identity, and fallback values.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ManufacturerDefaults {
    pub manufacturer: String,
    /// Names a [`VersionScheme`]; absent → the default (`dotted-int`).
    #[serde(default)]
    pub version_order: Option<String>,
    #[serde(default)]
    pub values: BTreeMap<String, ValuePolicy>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, serde_yaml::Value>,
}

impl ManufacturerDefaults {
    pub fn from_yaml(text: &str) -> Result<Self, crate::ManifestError> {
        Ok(serde_yaml::from_str(text)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn property_kind_is_closed_and_defaults_to_setting() {
        let setting: Property = serde_yaml::from_str("name: aperture\n").unwrap();
        assert_eq!(setting.kind, PropertyKind::Setting);
        let setting_yaml = serde_yaml::to_string(&setting).unwrap();
        assert!(
            !setting_yaml.contains("kind:"),
            "default classification should stay implicit: {setting_yaml}"
        );

        let scaffold: Property = serde_yaml::from_str("name: keepalive\nkind: scaffold\n").unwrap();
        assert_eq!(scaffold.kind, PropertyKind::Scaffold);
        assert!(serde_yaml::to_string(&scaffold)
            .unwrap()
            .contains("kind: scaffold"));

        let catalog: Property =
            serde_yaml::from_str("name: raw_0xd001\nkind: catalogOnly\n").unwrap();
        assert_eq!(catalog.kind, PropertyKind::CatalogOnly);

        let err = serde_yaml::from_str::<Property>("name: mystery\nkind: internal\n")
            .unwrap_err()
            .to_string();
        assert!(err.contains("unknown variant `internal`"), "got: {err}");
    }

    #[test]
    fn operation_kind_is_closed_and_defaults_to_executable() {
        let operation: Operation = serde_yaml::from_str("name: OpenSession\n").unwrap();
        assert_eq!(operation.kind, OperationKind::Executable);
        assert!(!serde_yaml::to_string(&operation).unwrap().contains("kind:"));

        let catalog: Operation =
            serde_yaml::from_str("name: raw_0x9000\nkind: advertisedOnly\n").unwrap();
        assert_eq!(catalog.kind, OperationKind::AdvertisedOnly);
        assert!(serde_yaml::to_string(&catalog)
            .unwrap()
            .contains("kind: advertisedOnly"));

        let error = serde_yaml::from_str::<Operation>("name: bad\nkind: inferred\n")
            .unwrap_err()
            .to_string();
        assert!(error.contains("unknown variant `inferred`"), "got: {error}");
    }

    // A body manifest exercising the 2b vocabulary against the one body we own.
    const GROWN: &str = r#"
schema: camera-config/v1
camera: { manufacturer: FUJIFILM, model: GFX100 II, firmware: "2.30" }
operations:
  "0x101c":
    name: InitiateOpenCapture
    modes: [Shooting]
    requires: { prop: "0xd212", mask: 0x00ff, ne: 0 }
  "0x902d":
    name: StepFNumber
    modes: [Shooting/Stills]
    connections: [xlv-http]
properties:
  "0xd001":
    name: recordingMode
    descriptor: { form: enum, source: camera }
modes:
  Shooting: { capabilities: [exposureControl] }
  Shooting/Stills:
    capabilities: [liveView]
    detect: { prop: "0xdf01", eq: 0x1600 }
connections:
  xlv-http:
    kind: http
    modes: [Shooting/Video]
  instax-printer:
    ref: instax-printer
    availableWhen: { firmware: { lt: "2.40" } }
values:
  initiatorGuid: { type: fixed, value: "f2e4538f-..." }
  sessionId: { type: generated, scheme: uuidv4, persist: true }
"#;

    #[test]
    fn grown_schema_loads() {
        let m = CameraManifest::from_yaml(GROWN).unwrap();
        assert_eq!(m.modes.len(), 2);
        assert!(m.modes["Shooting/Stills"].detect.is_some());
        assert_eq!(m.operations["0x902d"].connections, vec!["xlv-http"]);
        assert!(m.operations["0x101c"].requires.is_some());
    }

    #[test]
    fn mode_entry_steps_parse() {
        // The ground-truth live-view entry from FujiCameraAPISession.
        let yaml = r#"
schema: camera-config/v1
camera: { manufacturer: FUJIFILM, model: GFX100 II, firmware: "2.30" }
connections:
  app:
    kind: ptpip-app
    entries:
      - to: Shooting/Stills
        steps:
          - { setProp: "0xdf00", value: 6 }
          - { setProp: "0xdf01", value: 0x16 }
          - { readEcho: "0xdf2a" }
          - { sendOp: "0x902b", repeat: 4 }
          - { sendOp: "0x101c" }
      - to: ImageTransfer
        from: Shooting/Stills
        steps:
          - { sendOp: "0x1018" }
          - { setProp: "0xdf01", value: 0x14 }
"#;
        let m = CameraManifest::from_yaml(yaml).unwrap();
        let entries = &m.connections["app"].entries;
        assert_eq!(entries.len(), 2);
        let lv = &entries[0];
        assert_eq!(lv.to, "Shooting/Stills");
        assert!(lv.from.is_none(), "cold entry");
        let steps = lv.ptp_steps().expect("PTP execution");
        assert_eq!(steps.len(), 5);
        assert_eq!(steps[0].set_prop.as_deref(), Some("0xdf00"));
        assert_eq!(steps[0].value, Some(6.into()));
        assert_eq!(steps[3].repeat, 4); // 902B ×4
        assert_eq!(steps[4].send_op.as_deref(), Some("0x101c"));
        assert!(steps.iter().all(Step::is_well_formed));
        // from-qualified switch (no full teardown path).
        assert_eq!(entries[1].from.as_deref(), Some("Shooting/Stills"));
    }

    #[test]
    fn step_params_tolerant_and_runtime_slots_parse() {
        let yaml = r#"
schema: camera-config/v1
camera: { manufacturer: FUJIFILM, model: GFX100 II, firmware: "2.30" }
connections:
  app:
    entries:
      - to: ImageTransfer
        steps:
          - { getProp: "0xdf28", tolerant: true }            # read-before-write, advisory
          - { setProp: "0xdf28", value: 3 }                  # uint32 width from the property type
          - { sendOp: "0x9053", params: [0, 0x7530], tolerant: true }   # op with literal params
          - { sendOp: "0x1018", params: [{ runtime: openCaptureTxId }] } # runtime-bound param
"#;
        let m = CameraManifest::from_yaml(yaml).unwrap();
        let steps = m.connections["app"].entries[0]
            .ptp_steps()
            .expect("PTP execution");
        assert!(steps[0].tolerant && steps[0].get_prop.as_deref() == Some("0xdf28"));
        assert_eq!(steps[1].set_prop.as_deref(), Some("0xdf28"));
        assert_eq!(
            steps[2].params,
            vec![StepParam::Literal(0), StepParam::Literal(0x7530)]
        );
        assert_eq!(
            steps[3].params,
            vec![StepParam::Runtime {
                runtime: "openCaptureTxId".into(),
                shift: 0,
                mask: None,
            }]
        );
        assert!(steps.iter().all(Step::is_well_formed));
    }

    #[test]
    fn await_until_step_parses() {
        // The #42 AF poll: tap-to-AF then poll S1_LOCK_COLOR until locked.
        let yaml = r#"
schema: camera-config/v1
camera: { manufacturer: FUJIFILM, model: GFX100 II, firmware: "2.30" }
connections:
  app:
    entries:
      - to: Shooting/Stills
        steps:
          - { sendOp: "0x9026", params: [0x09060403] }
          - awaitUntil:
              source: { poll: "0xd209" }
              until: { prop: "0xd209", eq: 1 }
              timeoutMs: 5000
              intervalMs: 250
              onEach:
                - { getProp: "0xd212", tolerant: true }
"#;
        let m = CameraManifest::from_yaml(yaml).unwrap();
        let steps = m.connections["app"].entries[0]
            .ptp_steps()
            .expect("PTP execution");
        assert_eq!(steps.len(), 2);
        assert_eq!(steps[0].send_op.as_deref(), Some("0x9026"));
        let aw = steps[1].await_until.as_ref().expect("awaitUntil parsed");
        assert_eq!(
            aw.source,
            AwaitSource::Poll {
                prop: "0xd209".into()
            }
        );
        assert_eq!(aw.timeout_ms, 5000);
        assert_eq!(aw.interval_ms, 250);
        assert_eq!(aw.on_each.len(), 1);
        assert_eq!(aw.on_each[0].get_prop.as_deref(), Some("0xd212"));
        // `until` is the PTP predicate over observed values.
        assert!(aw
            .until
            .eval(&crate::predicate::PropView::new().with(0xd209, 1)));
        assert!(!aw
            .until
            .eval(&crate::predicate::PropView::new().with(0xd209, 0)));
        // Exactly-one-action holds for the awaitUntil step too.
        assert!(steps.iter().all(Step::is_well_formed));
    }

    #[test]
    fn await_until_event_source_parses() {
        // #54 hybrid: await the 0xC005 completion push, then one read of 0xd209.
        let yaml = r#"
schema: camera-config/v1
camera: { manufacturer: FUJIFILM, model: GFX100 II, firmware: "2.30" }
connections:
  app:
    entries:
      - to: Shooting/Stills
        steps:
          - { sendOp: "0x9026", params: [0x09060403] }
          - awaitUntil:
              source: { event: { code: "0xc005", thenPoll: "0xd209" } }
              until: { prop: "0xd209", eq: 1 }
              timeoutMs: 5000
"#;
        let m = CameraManifest::from_yaml(yaml).unwrap();
        let aw = m.connections["app"].entries[0].ptp_steps().unwrap()[1]
            .await_until
            .as_ref()
            .expect("awaitUntil parsed");
        assert_eq!(
            aw.source,
            AwaitSource::Event {
                code: "0xc005".into(),
                then_poll: Some("0xd209".into()),
            }
        );
        // thenPoll omitted = event arrival alone.
        let bare = CameraManifest::from_yaml(
            r#"
schema: camera-config/v1
camera: { manufacturer: FUJIFILM, model: GFX100 II, firmware: "2.30" }
connections:
  app:
    entries:
      - to: Shooting/Stills
        steps:
          - awaitUntil:
              source: { event: { code: "0xc001" } }
              until: { prop: "0xd400", eq: 1 }
              timeoutMs: 5000
"#,
        )
        .unwrap();
        assert_eq!(
            bare.connections["app"].entries[0].ptp_steps().unwrap()[0]
                .await_until
                .as_ref()
                .unwrap()
                .source,
            AwaitSource::Event {
                code: "0xc001".into(),
                then_poll: None,
            }
        );
    }

    #[test]
    fn await_until_source_rejects_unknown_key() {
        // The single-entry-mapping allowlist (poll, event) rejects other keys.
        let yaml = r#"
schema: camera-config/v1
camera: { manufacturer: FUJIFILM, model: GFX100 II, firmware: "2.30" }
connections:
  app:
    entries:
      - to: Shooting/Stills
        steps:
          - awaitUntil:
              source: { notify: "0xd209" }
              until: { prop: "0xd209", eq: 1 }
              timeoutMs: 5000
"#;
        let err = CameraManifest::from_yaml(yaml).unwrap_err().to_string();
        assert!(
            err.contains("unknown awaitUntil source"),
            "expected allowlist error, got: {err}"
        );
    }

    #[test]
    fn await_source_serialize_round_trips_through_yaml() {
        // The generator's consolidation does manifest.to_yaml() → from_yaml(); a
        // derived externally-tagged Serialize emits `!event` which the hand-written
        // Deserialize rejects. Both source forms must survive the round-trip.
        for src in [
            AwaitSource::Event {
                code: "0xc001".into(),
                then_poll: Some("0xd212".into()),
            },
            AwaitSource::Event {
                code: "0xc005".into(),
                then_poll: None,
            },
            AwaitSource::Poll {
                prop: "0xd209".into(),
            },
        ] {
            let yaml = serde_yaml::to_string(&src).expect("serialize");
            assert!(!yaml.contains('!'), "must not emit a YAML tag, got: {yaml}");
            let back: AwaitSource = serde_yaml::from_str(&yaml).expect("deserialize");
            assert_eq!(back, src, "round-trip mismatch via:\n{yaml}");
        }
    }

    #[test]
    fn connection_inline_vs_ref() {
        let m = CameraManifest::from_yaml(GROWN).unwrap();
        let xlv = &m.connections["xlv-http"];
        assert!(xlv.ref_id.is_none(), "inline definition has no ref");
        assert_eq!(xlv.kind.as_deref(), Some("http"));
        let instax = &m.connections["instax-printer"];
        assert_eq!(instax.ref_id.as_deref(), Some("instax-printer"));
        assert!(instax.available_when.is_some());
    }

    #[test]
    fn instax_fw_gate_present_on_230_gone_on_240() {
        let m = CameraManifest::from_yaml(GROWN).unwrap();
        let cond = m.connections["instax-printer"]
            .available_when
            .as_ref()
            .unwrap();
        let s = VersionScheme::DottedInt;
        assert!(cond.matches("2.30", s), "instax available on 2.30");
        assert!(cond.matches("2.39", s));
        assert!(!cond.matches("2.40", s), "instax removed at 2.40");
        assert!(!cond.matches("3.00", s));
    }

    #[test]
    fn version_cond_failsoft_on_unparseable() {
        let cond = VersionCond {
            lt: Some("2.40".into()),
            ..Default::default()
        };
        // Unorderable fw → bound fails → not available (safe), no panic.
        assert!(!cond.matches("beta", VersionScheme::DottedInt));
    }

    #[test]
    fn value_source_inference() {
        // Explicit source wins.
        let cam = Descriptor {
            form: "enum".into(),
            values: vec![],
            source: Some(ValueSource::Camera),
        };
        assert_eq!(cam.effective_source(), ValueSource::Camera);
        // Inferred: values present → manifest; empty → camera.
        let declared = Descriptor {
            form: "enum".into(),
            values: vec![1.into(), 2.into()],
            source: None,
        };
        assert_eq!(declared.effective_source(), ValueSource::Manifest);
        let empty = Descriptor {
            form: "enum".into(),
            values: vec![],
            source: None,
        };
        assert_eq!(empty.effective_source(), ValueSource::Camera);
    }

    #[test]
    fn descriptor_values_round_trip_as_plain_yaml_scalars() {
        let descriptor: Descriptor = serde_yaml::from_str(
            r#"
form: enum
values: [1, 2, "4000x2664"]
"#,
        )
        .unwrap();
        assert_eq!(
            descriptor.values,
            [
                DescriptorValue::Int(1),
                DescriptorValue::Int(2),
                DescriptorValue::Str("4000x2664".into()),
            ]
        );

        let yaml = serde_yaml::to_string(&descriptor).unwrap();
        assert!(yaml.contains("- 1"), "integer scalar missing from:\n{yaml}");
        assert!(
            yaml.contains("- 4000x2664"),
            "string scalar missing from:\n{yaml}"
        );
        assert!(!yaml.contains("Int:"), "enum tag leaked into:\n{yaml}");
        assert!(!yaml.contains("Str:"), "enum tag leaked into:\n{yaml}");
        let round_trip: Descriptor = serde_yaml::from_str(&yaml).unwrap();
        assert_eq!(round_trip.values, descriptor.values);
    }

    #[test]
    fn value_policy_variants_parse() {
        let m = CameraManifest::from_yaml(GROWN).unwrap();
        assert!(matches!(
            m.values["initiatorGuid"],
            ValuePolicy::Fixed { .. }
        ));
        match &m.values["sessionId"] {
            ValuePolicy::Generated { scheme, persist } => {
                assert_eq!(scheme, "uuidv4");
                assert!(persist);
            }
            other => panic!("expected generated, got {other:?}"),
        }
    }

    #[test]
    fn client_derived_value_policy_parses() {
        // #109: a client-derived friendly name names the runtime slot the host fills
        // (the same slot the BLE deviceNameString write uses) — never a literal.
        let yaml = r#"
manufacturer: FUJIFILM
versionOrder: dotted-int
values:
  initFriendlyName: { type: client-derived, runtime: terminalName }
"#;
        let d = ManufacturerDefaults::from_yaml(yaml).unwrap();
        match &d.values["initFriendlyName"] {
            ValuePolicy::ClientDerived { runtime } => assert_eq!(runtime, "terminalName"),
            other => panic!("expected client-derived, got {other:?}"),
        }
    }

    #[test]
    fn manufacturer_defaults_is_not_a_camera() {
        let fuji = r#"
manufacturer: FUJIFILM
versionOrder: dotted-int
values:
  initiatorGuid: { type: fixed, value: "f2e4538f-..." }
"#;
        let d = ManufacturerDefaults::from_yaml(fuji).unwrap();
        assert_eq!(d.manufacturer, "FUJIFILM");
        assert_eq!(d.version_order.as_deref(), Some("dotted-int"));
        assert!(d.values.contains_key("initiatorGuid"));
    }

    #[test]
    fn action_verb_parser_is_exact_and_round_trips() {
        for verb in [
            ActionVerb::Shutter,
            ActionVerb::EnumerateObjects,
            ActionVerb::GetObjectInfo,
            ActionVerb::GetThumb,
            ActionVerb::GetObject,
            ActionVerb::DeleteObject,
            ActionVerb::AutofocusLock,
            ActionVerb::AutofocusRelease,
            ActionVerb::ImportObjects,
            ActionVerb::ReadDeviceInfo,
            ActionVerb::StartLiveView,
            ActionVerb::PollLiveView,
            ActionVerb::StopLiveView,
            ActionVerb::Keepalive,
        ] {
            assert_eq!(verb.as_str().parse::<ActionVerb>(), Ok(verb));
        }
        for invalid in ["GetObject", "get-object", " getObject", "unknown"] {
            assert!(
                invalid.parse::<ActionVerb>().is_err(),
                "accepted {invalid:?}"
            );
        }
    }
}
