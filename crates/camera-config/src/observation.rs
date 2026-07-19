//! Canonical, manufacturer-neutral observation records.
//!
//! `camera-observation/v1` is an exact discriminator. The Rust types in this
//! module generate the checked-in JSON Schema and are also the validator's
//! deserialization surface, so there is one structural authority.

use std::collections::BTreeMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

pub const OBSERVATION_SCHEMA_VERSION: &str = "camera-observation/v1";
pub const MAX_INLINE_PAYLOAD_BYTES: u64 = 4096;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "camelCase", deny_unknown_fields)]
pub enum ObservationLine {
    BundleHeader(BundleHeader),
    Lifecycle(LifecycleRecord),
    BleGatt(BleGattRecord),
    PtpTransaction(Box<PtpTransactionRecord>),
    PtpEvent(PtpEventRecord),
    HttpExchange(HttpExchangeRecord),
    Capability(Box<CapabilityRecord>),
    ActionInvocation(ActionInvocationRecord),
}

impl ObservationLine {
    pub fn schema(&self) -> &str {
        match self {
            Self::BundleHeader(value) => &value.schema,
            Self::Lifecycle(value) => &value.common.schema,
            Self::BleGatt(value) => &value.common.schema,
            Self::PtpTransaction(value) => &value.common.schema,
            Self::PtpEvent(value) => &value.common.schema,
            Self::HttpExchange(value) => &value.common.schema,
            Self::Capability(value) => &value.common.schema,
            Self::ActionInvocation(value) => &value.common.schema,
        }
    }

    pub fn run_id(&self) -> &str {
        match self {
            Self::BundleHeader(value) => &value.run_id,
            _ => &self.common().expect("non-header has common fields").run_id,
        }
    }

    pub fn record_id(&self) -> &str {
        match self {
            Self::BundleHeader(value) => &value.record_id,
            _ => {
                &self
                    .common()
                    .expect("non-header has common fields")
                    .record_id
            }
        }
    }

    pub fn ordinal(&self) -> u64 {
        match self {
            Self::BundleHeader(value) => value.ordinal,
            _ => self.common().expect("non-header has common fields").ordinal,
        }
    }

    pub fn common(&self) -> Option<&ObservationCommon> {
        match self {
            Self::BundleHeader(_) => None,
            Self::Lifecycle(value) => Some(&value.common),
            Self::BleGatt(value) => Some(&value.common),
            Self::PtpTransaction(value) => Some(&value.common),
            Self::PtpEvent(value) => Some(&value.common),
            Self::HttpExchange(value) => Some(&value.common),
            Self::Capability(value) => Some(&value.common),
            Self::ActionInvocation(value) => Some(&value.common),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BundleHeader {
    pub schema: String,
    pub run_id: String,
    pub record_id: String,
    pub ordinal: u64,
    pub camera: CameraContext,
    pub client: ClientContext,
    pub capture: CaptureContext,
    pub epistemic: EpistemicMetadata,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CameraContext {
    pub manufacturer: String,
    pub model: String,
    /// Sanitized stable pseudonym for the physical body; never a serial number.
    pub body_id: String,
    pub firmware: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ClientContext {
    pub artifact: String,
    pub version: String,
    pub platform: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CaptureContext {
    pub interfaces: Vec<CaptureInterface>,
    pub clocks: Vec<CaptureClock>,
    #[serde(default)]
    pub clock_mappings: Vec<ClockMapping>,
    pub loss: LossCounters,
    #[serde(default)]
    pub redactions: Vec<Redaction>,
    pub tool_versions: BTreeMap<String, String>,
    #[serde(default)]
    pub artifacts: Vec<ArtifactMetadata>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CaptureInterface {
    pub id: String,
    pub interface_type: CaptureInterfaceType,
    pub role: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum CaptureInterfaceType {
    Ble,
    Tcp,
    Usb,
    Http,
    Synthetic,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CaptureClock {
    pub id: String,
    pub clock_type: ClockType,
    pub unit: ClockUnit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum ClockType {
    Monotonic,
    Wall,
    Device,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum ClockUnit {
    Nanoseconds,
    Microseconds,
    Milliseconds,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ClockMapping {
    pub from: String,
    pub to: String,
    pub offset: i64,
    pub uncertainty: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct LossCounters {
    pub dropped_records: u64,
    pub dropped_bytes: u64,
    pub truncated_payloads: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Redaction {
    pub field: String,
    pub method: RedactionMethod,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum RedactionMethod {
    Removed,
    Pseudonymized,
    Hashed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ArtifactMetadata {
    pub id: String,
    pub length: u64,
    pub sha256: String,
    pub media_type: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ObservationCommon {
    pub schema: String,
    pub run_id: String,
    pub record_id: String,
    pub ordinal: u64,
    pub context: ExecutionContext,
    pub time: ClockPoint,
    #[serde(default)]
    pub physical_context: BTreeMap<String, String>,
    #[serde(default)]
    pub artifact_ranges: Vec<ArtifactRange>,
    pub epistemic: EpistemicMetadata,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ExecutionContext {
    pub connection: String,
    pub mode: String,
    pub state: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ClockPoint {
    pub clock: String,
    pub value: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ArtifactRange {
    pub artifact: String,
    pub offset: u64,
    pub length: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EpistemicMetadata {
    pub class: EpistemicClass,
    pub confidence: Confidence,
    #[serde(default)]
    pub alternatives: Vec<String>,
    #[serde(default)]
    pub falsifier: Option<String>,
    #[serde(default)]
    pub unknowns: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum EpistemicClass {
    DirectObservation,
    DeterministicReduction,
    Inference,
    SyntheticFixture,
}

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "camelCase")]
pub enum Confidence {
    Exact,
    High,
    Medium,
    Low,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct LifecycleRecord {
    #[serde(flatten)]
    pub common: ObservationCommon,
    pub marker: LifecycleMarker,
    #[serde(default)]
    pub transition: Option<StateTransition>,
    #[serde(default)]
    pub attempt: Option<u32>,
    #[serde(default)]
    pub detail: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum LifecycleMarker {
    Discovery,
    Association,
    ConnectionOpened,
    SessionOpened,
    ModeTransition,
    StateTransition,
    Retry,
    Teardown,
    SessionClosed,
    ConnectionClosed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct StateTransition {
    pub from: String,
    pub to: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BleGattRecord {
    #[serde(flatten)]
    pub common: ObservationCommon,
    pub connection_instance: String,
    pub operation: BleGattOperation,
    pub service: String,
    pub characteristic: String,
    pub outcome: TransportOutcome,
    #[serde(default)]
    pub payload: Option<PayloadMetadata>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum BleGattOperation {
    Discover,
    Read,
    Write,
    Subscribe,
    Notify,
    Indicate,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum TransportOutcome {
    Ok,
    Timeout,
    Abort,
    Incomplete,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PtpTransactionRecord {
    #[serde(flatten)]
    pub common: ObservationCommon,
    pub transport: PtpTransport,
    pub connection_instance: String,
    pub session: String,
    pub endpoint_set: String,
    pub transaction_id: u32,
    pub request: PtpRequest,
    #[serde(default)]
    pub response: Option<PtpResponse>,
    pub outcome: TransactionOutcome,
    #[serde(default)]
    pub evidence_basis: Option<ControlEvidenceBasis>,
    #[serde(default)]
    pub observed_effect: Option<ControlObservedEffect>,
    #[serde(default)]
    pub readback: Option<Readback>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum PtpTransport {
    PtpIp,
    Usb,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PtpRequest {
    pub framing: String,
    pub operation: String,
    #[serde(default)]
    pub parameters: Vec<u32>,
    #[serde(default)]
    pub data: Option<PtpDataPhase>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PtpResponse {
    pub code: String,
    #[serde(default)]
    pub parameters: Vec<u32>,
    #[serde(default)]
    pub data: Option<PtpDataPhase>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PtpDataPhase {
    pub direction: DataDirection,
    pub payload: PayloadMetadata,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum DataDirection {
    HostToCamera,
    CameraToHost,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum TransactionOutcome {
    Ok,
    NonOk,
    Timeout,
    TransportAbort,
    Incomplete,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum ControlEvidenceBasis {
    DescriptorOnly,
    WriteProbe,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum ControlObservedEffect {
    Confirmed,
    AckNoEffect,
    ProtocolRefused,
    DestructiveClamp,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(
    tag = "status",
    rename_all = "camelCase",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
pub enum Readback {
    Observed {
        baseline: serde_json::Value,
        request: serde_json::Value,
        settling: SettlingRule,
        observed: serde_json::Value,
        observed_at: ClockPoint,
        source: ReadbackSource,
    },
    NotObserved {
        reason: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SettlingRule {
    pub deadline_ms: u64,
    #[serde(default)]
    pub poll_interval_ms: Option<u64>,
    #[serde(default)]
    pub stable_samples: Option<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum ReadbackSource {
    DirectProperty,
    DeclaredReadback,
    Event,
    HttpResponse,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PtpEventRecord {
    #[serde(flatten)]
    pub common: ObservationCommon,
    pub connection_instance: String,
    pub session: String,
    pub endpoint_set: String,
    pub transaction_id: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transaction_record_id: Option<String>,
    pub event: String,
    #[serde(default)]
    pub parameters: Vec<u32>,
    #[serde(default)]
    pub payload: Option<PayloadMetadata>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct HttpExchangeRecord {
    #[serde(flatten)]
    pub common: ObservationCommon,
    pub connection_instance: String,
    pub request: HttpRequest,
    #[serde(default)]
    pub response: Option<HttpResponse>,
    pub outcome: TransportOutcome,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct HttpRequest {
    pub method: String,
    pub target: String,
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    #[serde(default)]
    pub body: Option<PayloadMetadata>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct HttpResponse {
    pub status: u16,
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    #[serde(default)]
    pub body: Option<PayloadMetadata>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CapabilityRecord {
    #[serde(flatten)]
    pub common: ObservationCommon,
    pub subject: CapabilitySubject,
    /// Completeness of the operation or property inventory in this record's
    /// exact camera + execution context. Omitted observations are partial and
    /// therefore cannot support negative capability assertions.
    #[serde(default, skip_serializing_if = "InventoryCompleteness::is_partial")]
    pub inventory_completeness: InventoryCompleteness,
    pub evidence_basis: ControlEvidenceBasis,
    pub observed_effect: ControlObservedEffect,
    pub readback: Readback,
}

#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "camelCase")]
pub enum InventoryCompleteness {
    #[default]
    Partial,
    Complete,
}

impl InventoryCompleteness {
    pub fn is_partial(&self) -> bool {
        *self == Self::Partial
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
pub enum CapabilitySubject {
    Identity {
        device_version: String,
    },
    Operation {
        code: String,
        supported: bool,
        #[serde(default)]
        canonical_name: Option<Box<SemanticNameAssertion>>,
    },
    Property {
        code: String,
        supported: bool,
        #[serde(default)]
        canonical_name: Option<Box<SemanticNameAssertion>>,
        #[serde(default)]
        source_native_name: Option<Box<SemanticNameAssertion>>,
        #[serde(default)]
        property_type: Option<String>,
        #[serde(default)]
        access: Option<String>,
        #[serde(default)]
        descriptor: Option<CapabilityDescriptor>,
        #[serde(default)]
        labels: BTreeMap<String, String>,
        #[serde(default)]
        value_rows: Vec<CapabilityValueRow>,
        #[serde(default)]
        value_profiles: Vec<CapabilityValueProfile>,
    },
}

/// A proposed consumer-neutral or source-native name together with the exact
/// evidence and epistemic claim that supports it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SemanticNameAssertion {
    pub name: String,
    pub provenance: AssertionProvenance,
}

/// Assertion-level provenance. This is separate from the enclosing record's
/// capture provenance because one capability record may carry independently
/// sourced names and value rows.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AssertionProvenance {
    pub evidence_reference: String,
    pub epistemic: EpistemicMetadata,
}

/// A global property-value semantic assertion. The tagged value preserves the
/// declared PTP representation without routing wide integers through JSON
/// floating-point numbers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CapabilityValueRow {
    pub value: TypedPropertyValue,
    pub label: String,
    pub provenance: AssertionProvenance,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, JsonSchema)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
pub enum TypedPropertyValue {
    I8 {
        value: i8,
    },
    I16 {
        value: i16,
    },
    I32 {
        value: i32,
    },
    /// Decimal text avoids precision loss in JSON and foreign-language seams.
    I64 {
        value: String,
    },
    /// Decimal text avoids precision loss in JSON and foreign-language seams.
    I128 {
        value: String,
    },
    U8 {
        value: u8,
    },
    U16 {
        value: u16,
    },
    U32 {
        value: u32,
    },
    /// Decimal text avoids precision loss in JSON and foreign-language seams.
    U64 {
        value: String,
    },
    /// Decimal text avoids precision loss in JSON and foreign-language seams.
    U128 {
        value: String,
    },
    String {
        value: String,
    },
}

impl TypedPropertyValue {
    pub fn property_type(&self) -> &'static str {
        match self {
            Self::I8 { .. } => "i8",
            Self::I16 { .. } => "i16",
            Self::I32 { .. } => "i32",
            Self::I64 { .. } => "i64",
            Self::I128 { .. } => "i128",
            Self::U8 { .. } => "u8",
            Self::U16 { .. } => "u16",
            Self::U32 { .. } => "u32",
            Self::U64 { .. } => "u64",
            Self::U128 { .. } => "u128",
            Self::String { .. } => "str",
        }
    }

    pub fn has_valid_representation(&self) -> bool {
        match self {
            Self::I64 { value } => parse_canonical_signed(value)
                .and_then(|value| i64::try_from(value).ok())
                .is_some(),
            Self::I128 { value } => parse_canonical_signed(value).is_some(),
            Self::U64 { value } => parse_canonical_unsigned(value)
                .and_then(|value| u64::try_from(value).ok())
                .is_some(),
            Self::U128 { value } => parse_canonical_unsigned(value).is_some(),
            _ => true,
        }
    }

    pub fn as_i64(&self) -> Option<i64> {
        match self {
            Self::I8 { value } => Some(i64::from(*value)),
            Self::I16 { value } => Some(i64::from(*value)),
            Self::I32 { value } => Some(i64::from(*value)),
            Self::I64 { value } | Self::I128 { value } => value
                .parse::<i128>()
                .ok()
                .and_then(|value| value.try_into().ok()),
            Self::U8 { value } => Some(i64::from(*value)),
            Self::U16 { value } => Some(i64::from(*value)),
            Self::U32 { value } => Some(i64::from(*value)),
            Self::U64 { value } | Self::U128 { value } => value
                .parse::<u128>()
                .ok()
                .and_then(|value| value.try_into().ok()),
            Self::String { .. } => None,
        }
    }
}

fn parse_canonical_signed(value: &str) -> Option<i128> {
    let parsed = value.parse::<i128>().ok()?;
    (parsed.to_string() == value).then_some(parsed)
}

fn parse_canonical_unsigned(value: &str) -> Option<u128> {
    let parsed = value.parse::<u128>().ok()?;
    (parsed.to_string() == value).then_some(parsed)
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CapabilityDescriptor {
    pub form: String,
    #[serde(default)]
    pub values: Vec<serde_json::Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CapabilityValueProfile {
    #[serde(default)]
    pub connection: Option<String>,
    #[serde(default)]
    pub mode: Option<String>,
    pub rows: Vec<CapabilityValueProfileRow>,
    #[serde(default)]
    pub evidence: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CapabilityValueProfileRow {
    pub label: String,
    pub raw: i64,
    #[serde(default = "default_true")]
    pub legal: bool,
    #[serde(default)]
    pub aliases: Vec<i64>,
    #[serde(default)]
    pub write_store_raw: Option<i64>,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ActionInvocationRecord {
    #[serde(flatten)]
    pub common: ObservationCommon,
    pub catalog_revision: String,
    pub action_id: String,
    pub role: ActionRole,
    pub parameters: BTreeMap<String, serde_json::Value>,
    pub outcome: ActionOutcome,
}

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "camelCase")]
pub enum ActionRole {
    Initiator,
    Responder,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum ActionOutcome {
    Succeeded,
    Failed,
    Rejected,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PayloadMetadata {
    pub length: u64,
    pub sha256: String,
    #[serde(default)]
    pub inline_hex: Option<String>,
    /// Contiguous byte ranges hashed as the payload crossed the recorder.
    /// Large transfers use these instead of retaining the body in memory.
    #[serde(default)]
    pub stream_ranges: Vec<PayloadRange>,
    /// Optional references into capture artifacts that retain the payload.
    #[serde(default)]
    pub ranges: Vec<ArtifactRange>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PayloadRange {
    pub offset: u64,
    pub length: u64,
    pub sha256: String,
}

pub fn generated_json_schema() -> Result<String, serde_json::Error> {
    let schema = schemars::schema_for!(ObservationLine);
    let mut text = serde_json::to_string_pretty(&schema)?;
    text.push('\n');
    Ok(text)
}
