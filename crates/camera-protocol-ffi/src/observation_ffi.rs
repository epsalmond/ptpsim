//! Hand-written foreign-language mirror of `camera-observation/v1`.
//!
//! The Rust observation model remains the structural authority. This module
//! exposes every closed variant and field through UniFFI, while arbitrary JSON
//! leaves are carried as canonical JSON strings instead of defining a second
//! value grammar.

use camera_config as cc;

use crate::{ActionRole, ControlEvidenceBasis, ControlObservedEffect};

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct ObservationStringField {
    pub key: String,
    pub value: String,
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct ObservationJsonValue {
    pub canonical_json: String,
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct ObservationJsonField {
    pub key: String,
    pub value: ObservationJsonValue,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum ObservationKind {
    BundleHeader,
    Lifecycle,
    BleGatt,
    PtpTransaction,
    PtpEvent,
    HttpExchange,
    Capability,
    ActionInvocation,
}

#[derive(Debug, Clone, uniffi::Enum)]
// UniFFI record payloads stay as direct enum fields so generated Swift and
// Kotlin preserve this tagged value contract. Rust-only indirection would
// change the FFI surface without changing the canonical JSON model.
#[allow(clippy::large_enum_variant)]
pub enum ObservationValue {
    BundleHeader { value: ObservationBundleHeader },
    Lifecycle { value: ObservationLifecycle },
    BleGatt { value: ObservationBleGatt },
    PtpTransaction { value: ObservationPtpTransaction },
    PtpEvent { value: ObservationPtpEvent },
    HttpExchange { value: ObservationHttpExchange },
    Capability { value: ObservationCapability },
    ActionInvocation { value: ObservationActionInvocation },
}

#[derive(Debug, Clone, uniffi::Record)]
pub struct ObservationRecord {
    pub schema: String,
    pub kind: ObservationKind,
    pub run_id: String,
    pub record_id: String,
    pub ordinal: u64,
    pub value: ObservationValue,
    /// Exact normalized JSON emitted by the authoritative Rust model.
    pub canonical_json: String,
}

#[derive(Debug, thiserror::Error, uniffi::Error)]
pub enum ObservationError {
    #[error("invalid canonical observation: {detail}")]
    Invalid { detail: String },
}

#[derive(Debug, Clone, uniffi::Record)]
pub struct ObservationBundleHeader {
    pub schema: String,
    pub run_id: String,
    pub record_id: String,
    pub ordinal: u64,
    pub camera: ObservationCameraContext,
    pub client: ObservationClientContext,
    pub capture: ObservationCaptureContext,
    pub epistemic: ObservationEpistemicMetadata,
}

#[derive(Debug, Clone, uniffi::Record)]
pub struct ObservationCameraContext {
    pub manufacturer: String,
    pub model: String,
    pub body_id: String,
    pub firmware: String,
}

#[derive(Debug, Clone, uniffi::Record)]
pub struct ObservationClientContext {
    pub artifact: String,
    pub version: String,
    pub platform: String,
}

#[derive(Debug, Clone, uniffi::Record)]
pub struct ObservationCaptureContext {
    pub interfaces: Vec<ObservationCaptureInterface>,
    pub clocks: Vec<ObservationCaptureClock>,
    pub clock_mappings: Vec<ObservationClockMapping>,
    pub loss: ObservationLossCounters,
    pub redactions: Vec<ObservationRedaction>,
    pub tool_versions: Vec<ObservationStringField>,
    pub artifacts: Vec<ObservationArtifactMetadata>,
}

#[derive(Debug, Clone, uniffi::Record)]
pub struct ObservationCaptureInterface {
    pub id: String,
    pub interface_type: ObservationCaptureInterfaceType,
    pub role: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum ObservationCaptureInterfaceType {
    Ble,
    Tcp,
    Usb,
    Http,
    Synthetic,
}

#[derive(Debug, Clone, uniffi::Record)]
pub struct ObservationCaptureClock {
    pub id: String,
    pub clock_type: ObservationClockType,
    pub unit: ObservationClockUnit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum ObservationClockType {
    Monotonic,
    Wall,
    Device,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum ObservationClockUnit {
    Nanoseconds,
    Microseconds,
    Milliseconds,
}

#[derive(Debug, Clone, uniffi::Record)]
pub struct ObservationClockMapping {
    pub from: String,
    pub to: String,
    pub offset: i64,
    pub uncertainty: u64,
}

#[derive(Debug, Clone, uniffi::Record)]
pub struct ObservationLossCounters {
    pub dropped_records: u64,
    pub dropped_bytes: u64,
    pub truncated_payloads: u64,
}

#[derive(Debug, Clone, uniffi::Record)]
pub struct ObservationRedaction {
    pub field: String,
    pub method: ObservationRedactionMethod,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum ObservationRedactionMethod {
    Removed,
    Pseudonymized,
    Hashed,
}

#[derive(Debug, Clone, uniffi::Record)]
pub struct ObservationArtifactMetadata {
    pub id: String,
    pub length: u64,
    pub sha256: String,
    pub media_type: String,
}

#[derive(Debug, Clone, uniffi::Record)]
pub struct ObservationCommon {
    pub schema: String,
    pub run_id: String,
    pub record_id: String,
    pub ordinal: u64,
    pub context: ObservationExecutionContext,
    pub time: ObservationClockPoint,
    pub physical_context: Vec<ObservationStringField>,
    pub artifact_ranges: Vec<ObservationArtifactRange>,
    pub epistemic: ObservationEpistemicMetadata,
}

#[derive(Debug, Clone, uniffi::Record)]
pub struct ObservationExecutionContext {
    pub connection: String,
    pub mode: String,
    pub state: String,
}

#[derive(Debug, Clone, uniffi::Record)]
pub struct ObservationClockPoint {
    pub clock: String,
    pub value: u64,
}

#[derive(Debug, Clone, uniffi::Record)]
pub struct ObservationArtifactRange {
    pub artifact: String,
    pub offset: u64,
    pub length: u64,
}

#[derive(Debug, Clone, uniffi::Record)]
pub struct ObservationEpistemicMetadata {
    pub class: ObservationEpistemicClass,
    pub confidence: ObservationConfidence,
    pub alternatives: Vec<String>,
    pub falsifier: Option<String>,
    pub unknowns: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum ObservationEpistemicClass {
    DirectObservation,
    DeterministicReduction,
    Inference,
    SyntheticFixture,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum ObservationConfidence {
    Exact,
    High,
    Medium,
    Low,
    Unknown,
}

#[derive(Debug, Clone, uniffi::Record)]
pub struct ObservationLifecycle {
    pub common: ObservationCommon,
    pub marker: ObservationLifecycleMarker,
    pub transition: Option<ObservationStateTransition>,
    pub attempt: Option<u32>,
    pub detail: Vec<ObservationStringField>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum ObservationLifecycleMarker {
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

#[derive(Debug, Clone, uniffi::Record)]
pub struct ObservationStateTransition {
    pub from: String,
    pub to: String,
}

#[derive(Debug, Clone, uniffi::Record)]
pub struct ObservationBleGatt {
    pub common: ObservationCommon,
    pub connection_instance: String,
    pub operation: ObservationBleGattOperation,
    pub service: String,
    pub characteristic: String,
    pub outcome: ObservationTransportOutcome,
    pub payload: Option<ObservationPayloadMetadata>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum ObservationBleGattOperation {
    Discover,
    Read,
    Write,
    Subscribe,
    Notify,
    Indicate,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum ObservationTransportOutcome {
    Ok,
    Timeout,
    Abort,
    Incomplete,
}

#[derive(Debug, Clone, uniffi::Record)]
pub struct ObservationPtpTransaction {
    pub common: ObservationCommon,
    pub transport: ObservationPtpTransport,
    pub connection_instance: String,
    pub session: String,
    pub endpoint_set: String,
    pub transaction_id: u32,
    pub request: ObservationPtpRequest,
    pub response: Option<ObservationPtpResponse>,
    pub outcome: ObservationTransactionOutcome,
    pub evidence_basis: Option<ControlEvidenceBasis>,
    pub observed_effect: Option<ControlObservedEffect>,
    pub readback: Option<ObservationReadback>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum ObservationPtpTransport {
    PtpIp,
    Usb,
}

#[derive(Debug, Clone, uniffi::Record)]
pub struct ObservationPtpRequest {
    pub framing: String,
    pub operation: String,
    pub parameters: Vec<u32>,
    pub data: Option<ObservationPtpDataPhase>,
}

#[derive(Debug, Clone, uniffi::Record)]
pub struct ObservationPtpResponse {
    pub code: String,
    pub parameters: Vec<u32>,
    pub data: Option<ObservationPtpDataPhase>,
}

#[derive(Debug, Clone, uniffi::Record)]
pub struct ObservationPtpDataPhase {
    pub direction: ObservationDataDirection,
    pub payload: ObservationPayloadMetadata,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum ObservationDataDirection {
    HostToCamera,
    CameraToHost,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum ObservationTransactionOutcome {
    Ok,
    NonOk,
    Timeout,
    TransportAbort,
    Incomplete,
}

#[derive(Debug, Clone, uniffi::Enum)]
pub enum ObservationReadback {
    Observed {
        baseline: ObservationJsonValue,
        request: ObservationJsonValue,
        settling: ObservationSettlingRule,
        observed: ObservationJsonValue,
        observed_at: ObservationClockPoint,
        source: ObservationReadbackSource,
    },
    NotObserved {
        reason: String,
    },
}

#[derive(Debug, Clone, uniffi::Record)]
pub struct ObservationSettlingRule {
    pub deadline_ms: u64,
    pub poll_interval_ms: Option<u64>,
    pub stable_samples: Option<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum ObservationReadbackSource {
    DirectProperty,
    DeclaredReadback,
    Event,
    HttpResponse,
}

#[derive(Debug, Clone, uniffi::Record)]
pub struct ObservationPtpEvent {
    pub common: ObservationCommon,
    pub connection_instance: String,
    pub session: String,
    pub endpoint_set: String,
    pub transaction_id: u32,
    pub transaction_record_id: Option<String>,
    pub event: String,
    pub parameters: Vec<u32>,
    pub payload: Option<ObservationPayloadMetadata>,
}

#[derive(Debug, Clone, uniffi::Record)]
pub struct ObservationHttpExchange {
    pub common: ObservationCommon,
    pub connection_instance: String,
    pub request: ObservationHttpRequest,
    pub response: Option<ObservationHttpResponse>,
    pub outcome: ObservationTransportOutcome,
}

#[derive(Debug, Clone, uniffi::Record)]
pub struct ObservationHttpRequest {
    pub method: String,
    pub target: String,
    pub headers: Vec<ObservationStringField>,
    pub body: Option<ObservationPayloadMetadata>,
}

#[derive(Debug, Clone, uniffi::Record)]
pub struct ObservationHttpResponse {
    pub status: u16,
    pub headers: Vec<ObservationStringField>,
    pub body: Option<ObservationPayloadMetadata>,
}

#[derive(Debug, Clone, uniffi::Record)]
pub struct ObservationCapability {
    pub common: ObservationCommon,
    pub subject: ObservationCapabilitySubject,
    pub inventory_completeness: ObservationInventoryCompleteness,
    pub evidence_basis: ControlEvidenceBasis,
    pub observed_effect: ControlObservedEffect,
    pub readback: ObservationReadback,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum ObservationInventoryCompleteness {
    Partial,
    Complete,
}

impl From<cc::InventoryCompleteness> for ObservationInventoryCompleteness {
    fn from(value: cc::InventoryCompleteness) -> Self {
        match value {
            cc::InventoryCompleteness::Partial => Self::Partial,
            cc::InventoryCompleteness::Complete => Self::Complete,
        }
    }
}

// UniFFI exported enums cannot use Rust Box fields to shrink only the larger
// tagged variant without changing the generated foreign-language shape.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, uniffi::Enum)]
pub enum ObservationCapabilitySubject {
    Identity {
        device_version: String,
    },
    Operation {
        code: String,
        supported: bool,
        canonical_name: Option<ObservationSemanticNameAssertion>,
    },
    Property {
        code: String,
        supported: bool,
        canonical_name: Option<ObservationSemanticNameAssertion>,
        source_native_name: Option<ObservationSemanticNameAssertion>,
        property_type: Option<String>,
        access: Option<String>,
        descriptor: Option<ObservationCapabilityDescriptor>,
        labels: Vec<ObservationStringField>,
        value_rows: Vec<ObservationCapabilityValueRow>,
        value_profiles: Vec<ObservationCapabilityValueProfile>,
    },
}

#[derive(Debug, Clone, uniffi::Record)]
pub struct ObservationSemanticNameAssertion {
    pub name: String,
    pub provenance: ObservationAssertionProvenance,
}

#[derive(Debug, Clone, uniffi::Record)]
pub struct ObservationAssertionProvenance {
    pub evidence_reference: String,
    pub epistemic: ObservationEpistemicMetadata,
}

#[derive(Debug, Clone, uniffi::Record)]
pub struct ObservationCapabilityValueRow {
    pub value: ObservationTypedPropertyValue,
    pub label: String,
    pub provenance: ObservationAssertionProvenance,
}

#[derive(Debug, Clone, uniffi::Enum)]
pub enum ObservationTypedPropertyValue {
    I8 { value: i8 },
    I16 { value: i16 },
    I32 { value: i32 },
    I64 { value: String },
    I128 { value: String },
    U8 { value: u8 },
    U16 { value: u16 },
    U32 { value: u32 },
    U64 { value: String },
    U128 { value: String },
    String { value: String },
}

#[derive(Debug, Clone, uniffi::Record)]
pub struct ObservationCapabilityDescriptor {
    pub form: String,
    pub values: Vec<ObservationJsonValue>,
}

#[derive(Debug, Clone, uniffi::Record)]
pub struct ObservationCapabilityValueProfile {
    pub connection: Option<String>,
    pub mode: Option<String>,
    pub rows: Vec<ObservationCapabilityValueProfileRow>,
    pub evidence: Vec<String>,
}

#[derive(Debug, Clone, uniffi::Record)]
pub struct ObservationCapabilityValueProfileRow {
    pub label: String,
    pub raw: i64,
    pub legal: bool,
    pub aliases: Vec<i64>,
    pub write_store_raw: Option<i64>,
}

#[derive(Debug, Clone, uniffi::Record)]
pub struct ObservationActionInvocation {
    pub common: ObservationCommon,
    pub catalog_revision: String,
    pub action_id: String,
    pub role: ActionRole,
    pub parameters: Vec<ObservationJsonField>,
    pub outcome: ObservationActionOutcome,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum ObservationActionOutcome {
    Succeeded,
    Failed,
    Rejected,
}

#[derive(Debug, Clone, uniffi::Record)]
pub struct ObservationPayloadMetadata {
    pub length: u64,
    pub sha256: String,
    pub inline_hex: Option<String>,
    pub stream_ranges: Vec<ObservationPayloadRange>,
    pub ranges: Vec<ObservationArtifactRange>,
}

#[derive(Debug, Clone, uniffi::Record)]
pub struct ObservationPayloadRange {
    pub offset: u64,
    pub length: u64,
    pub sha256: String,
}

/// Parse one canonical JSON line through camera-config's exact discriminator
/// and map every field into the hand-written UniFFI mirror.
#[uniffi::export]
pub fn parse_observation_record(json: String) -> Result<ObservationRecord, ObservationError> {
    let line: cc::ObservationLine =
        serde_json::from_str(&json).map_err(|error| ObservationError::Invalid {
            detail: error.to_string(),
        })?;
    if line.schema() != cc::OBSERVATION_SCHEMA_VERSION {
        return Err(ObservationError::Invalid {
            detail: format!(
                "schema {:?} is not {:?}",
                line.schema(),
                cc::OBSERVATION_SCHEMA_VERSION
            ),
        });
    }
    let kind = match &line {
        cc::ObservationLine::BundleHeader(_) => ObservationKind::BundleHeader,
        cc::ObservationLine::Lifecycle(_) => ObservationKind::Lifecycle,
        cc::ObservationLine::BleGatt(_) => ObservationKind::BleGatt,
        cc::ObservationLine::PtpTransaction(_) => ObservationKind::PtpTransaction,
        cc::ObservationLine::PtpEvent(_) => ObservationKind::PtpEvent,
        cc::ObservationLine::HttpExchange(_) => ObservationKind::HttpExchange,
        cc::ObservationLine::Capability(_) => ObservationKind::Capability,
        cc::ObservationLine::ActionInvocation(_) => ObservationKind::ActionInvocation,
    };
    let canonical_json =
        serde_json::to_string(&line).map_err(|error| ObservationError::Invalid {
            detail: error.to_string(),
        })?;
    Ok(ObservationRecord {
        schema: line.schema().to_string(),
        kind,
        run_id: line.run_id().to_string(),
        record_id: line.record_id().to_string(),
        ordinal: line.ordinal(),
        value: ObservationValue::from(&line),
        canonical_json,
    })
}

impl From<&cc::ObservationLine> for ObservationValue {
    fn from(value: &cc::ObservationLine) -> Self {
        match value {
            cc::ObservationLine::BundleHeader(value) => Self::BundleHeader {
                value: value.into(),
            },
            cc::ObservationLine::Lifecycle(value) => Self::Lifecycle {
                value: value.into(),
            },
            cc::ObservationLine::BleGatt(value) => Self::BleGatt {
                value: value.into(),
            },
            cc::ObservationLine::PtpTransaction(value) => Self::PtpTransaction {
                value: value.as_ref().into(),
            },
            cc::ObservationLine::PtpEvent(value) => Self::PtpEvent {
                value: value.into(),
            },
            cc::ObservationLine::HttpExchange(value) => Self::HttpExchange {
                value: value.into(),
            },
            cc::ObservationLine::Capability(value) => Self::Capability {
                value: value.as_ref().into(),
            },
            cc::ObservationLine::ActionInvocation(value) => Self::ActionInvocation {
                value: value.into(),
            },
        }
    }
}

fn string_fields(
    values: &std::collections::BTreeMap<String, String>,
) -> Vec<ObservationStringField> {
    values
        .iter()
        .map(|(key, value)| ObservationStringField {
            key: key.clone(),
            value: value.clone(),
        })
        .collect()
}

fn json_value(value: &serde_json::Value) -> ObservationJsonValue {
    ObservationJsonValue {
        canonical_json: serde_json::to_string(value)
            .expect("JSON value serialization is infallible"),
    }
}

fn json_fields(
    values: &std::collections::BTreeMap<String, serde_json::Value>,
) -> Vec<ObservationJsonField> {
    values
        .iter()
        .map(|(key, value)| ObservationJsonField {
            key: key.clone(),
            value: json_value(value),
        })
        .collect()
}

impl From<&cc::BundleHeader> for ObservationBundleHeader {
    fn from(value: &cc::BundleHeader) -> Self {
        Self {
            schema: value.schema.clone(),
            run_id: value.run_id.clone(),
            record_id: value.record_id.clone(),
            ordinal: value.ordinal,
            camera: (&value.camera).into(),
            client: (&value.client).into(),
            capture: (&value.capture).into(),
            epistemic: (&value.epistemic).into(),
        }
    }
}

impl From<&cc::CameraContext> for ObservationCameraContext {
    fn from(value: &cc::CameraContext) -> Self {
        Self {
            manufacturer: value.manufacturer.clone(),
            model: value.model.clone(),
            body_id: value.body_id.clone(),
            firmware: value.firmware.clone(),
        }
    }
}

impl From<&cc::ClientContext> for ObservationClientContext {
    fn from(value: &cc::ClientContext) -> Self {
        Self {
            artifact: value.artifact.clone(),
            version: value.version.clone(),
            platform: value.platform.clone(),
        }
    }
}

impl From<&cc::CaptureContext> for ObservationCaptureContext {
    fn from(value: &cc::CaptureContext) -> Self {
        Self {
            interfaces: value.interfaces.iter().map(Into::into).collect(),
            clocks: value.clocks.iter().map(Into::into).collect(),
            clock_mappings: value.clock_mappings.iter().map(Into::into).collect(),
            loss: (&value.loss).into(),
            redactions: value.redactions.iter().map(Into::into).collect(),
            tool_versions: string_fields(&value.tool_versions),
            artifacts: value.artifacts.iter().map(Into::into).collect(),
        }
    }
}

impl From<&cc::CaptureInterface> for ObservationCaptureInterface {
    fn from(value: &cc::CaptureInterface) -> Self {
        Self {
            id: value.id.clone(),
            interface_type: value.interface_type.into(),
            role: value.role.clone(),
        }
    }
}

impl From<cc::CaptureInterfaceType> for ObservationCaptureInterfaceType {
    fn from(value: cc::CaptureInterfaceType) -> Self {
        match value {
            cc::CaptureInterfaceType::Ble => Self::Ble,
            cc::CaptureInterfaceType::Tcp => Self::Tcp,
            cc::CaptureInterfaceType::Usb => Self::Usb,
            cc::CaptureInterfaceType::Http => Self::Http,
            cc::CaptureInterfaceType::Synthetic => Self::Synthetic,
        }
    }
}

impl From<&cc::CaptureClock> for ObservationCaptureClock {
    fn from(value: &cc::CaptureClock) -> Self {
        Self {
            id: value.id.clone(),
            clock_type: value.clock_type.into(),
            unit: value.unit.into(),
        }
    }
}

impl From<cc::ClockType> for ObservationClockType {
    fn from(value: cc::ClockType) -> Self {
        match value {
            cc::ClockType::Monotonic => Self::Monotonic,
            cc::ClockType::Wall => Self::Wall,
            cc::ClockType::Device => Self::Device,
        }
    }
}

impl From<cc::ClockUnit> for ObservationClockUnit {
    fn from(value: cc::ClockUnit) -> Self {
        match value {
            cc::ClockUnit::Nanoseconds => Self::Nanoseconds,
            cc::ClockUnit::Microseconds => Self::Microseconds,
            cc::ClockUnit::Milliseconds => Self::Milliseconds,
        }
    }
}

impl From<&cc::ClockMapping> for ObservationClockMapping {
    fn from(value: &cc::ClockMapping) -> Self {
        Self {
            from: value.from.clone(),
            to: value.to.clone(),
            offset: value.offset,
            uncertainty: value.uncertainty,
        }
    }
}

impl From<&cc::LossCounters> for ObservationLossCounters {
    fn from(value: &cc::LossCounters) -> Self {
        Self {
            dropped_records: value.dropped_records,
            dropped_bytes: value.dropped_bytes,
            truncated_payloads: value.truncated_payloads,
        }
    }
}

impl From<&cc::Redaction> for ObservationRedaction {
    fn from(value: &cc::Redaction) -> Self {
        Self {
            field: value.field.clone(),
            method: value.method.into(),
        }
    }
}

impl From<cc::RedactionMethod> for ObservationRedactionMethod {
    fn from(value: cc::RedactionMethod) -> Self {
        match value {
            cc::RedactionMethod::Removed => Self::Removed,
            cc::RedactionMethod::Pseudonymized => Self::Pseudonymized,
            cc::RedactionMethod::Hashed => Self::Hashed,
        }
    }
}

impl From<&cc::ArtifactMetadata> for ObservationArtifactMetadata {
    fn from(value: &cc::ArtifactMetadata) -> Self {
        Self {
            id: value.id.clone(),
            length: value.length,
            sha256: value.sha256.clone(),
            media_type: value.media_type.clone(),
        }
    }
}

impl From<&cc::ObservationCommon> for ObservationCommon {
    fn from(value: &cc::ObservationCommon) -> Self {
        Self {
            schema: value.schema.clone(),
            run_id: value.run_id.clone(),
            record_id: value.record_id.clone(),
            ordinal: value.ordinal,
            context: (&value.context).into(),
            time: (&value.time).into(),
            physical_context: string_fields(&value.physical_context),
            artifact_ranges: value.artifact_ranges.iter().map(Into::into).collect(),
            epistemic: (&value.epistemic).into(),
        }
    }
}

impl From<&cc::ExecutionContext> for ObservationExecutionContext {
    fn from(value: &cc::ExecutionContext) -> Self {
        Self {
            connection: value.connection.clone(),
            mode: value.mode.clone(),
            state: value.state.clone(),
        }
    }
}

impl From<&cc::ClockPoint> for ObservationClockPoint {
    fn from(value: &cc::ClockPoint) -> Self {
        Self {
            clock: value.clock.clone(),
            value: value.value,
        }
    }
}

impl From<&cc::ArtifactRange> for ObservationArtifactRange {
    fn from(value: &cc::ArtifactRange) -> Self {
        Self {
            artifact: value.artifact.clone(),
            offset: value.offset,
            length: value.length,
        }
    }
}

impl From<&cc::EpistemicMetadata> for ObservationEpistemicMetadata {
    fn from(value: &cc::EpistemicMetadata) -> Self {
        Self {
            class: value.class.into(),
            confidence: value.confidence.into(),
            alternatives: value.alternatives.clone(),
            falsifier: value.falsifier.clone(),
            unknowns: value.unknowns.clone(),
        }
    }
}

impl From<cc::EpistemicClass> for ObservationEpistemicClass {
    fn from(value: cc::EpistemicClass) -> Self {
        match value {
            cc::EpistemicClass::DirectObservation => Self::DirectObservation,
            cc::EpistemicClass::DeterministicReduction => Self::DeterministicReduction,
            cc::EpistemicClass::Inference => Self::Inference,
            cc::EpistemicClass::SyntheticFixture => Self::SyntheticFixture,
        }
    }
}

impl From<cc::Confidence> for ObservationConfidence {
    fn from(value: cc::Confidence) -> Self {
        match value {
            cc::Confidence::Exact => Self::Exact,
            cc::Confidence::High => Self::High,
            cc::Confidence::Medium => Self::Medium,
            cc::Confidence::Low => Self::Low,
            cc::Confidence::Unknown => Self::Unknown,
        }
    }
}

impl From<&cc::LifecycleRecord> for ObservationLifecycle {
    fn from(value: &cc::LifecycleRecord) -> Self {
        Self {
            common: (&value.common).into(),
            marker: value.marker.into(),
            transition: value.transition.as_ref().map(Into::into),
            attempt: value.attempt,
            detail: string_fields(&value.detail),
        }
    }
}

impl From<cc::LifecycleMarker> for ObservationLifecycleMarker {
    fn from(value: cc::LifecycleMarker) -> Self {
        match value {
            cc::LifecycleMarker::Discovery => Self::Discovery,
            cc::LifecycleMarker::Association => Self::Association,
            cc::LifecycleMarker::ConnectionOpened => Self::ConnectionOpened,
            cc::LifecycleMarker::SessionOpened => Self::SessionOpened,
            cc::LifecycleMarker::ModeTransition => Self::ModeTransition,
            cc::LifecycleMarker::StateTransition => Self::StateTransition,
            cc::LifecycleMarker::Retry => Self::Retry,
            cc::LifecycleMarker::Teardown => Self::Teardown,
            cc::LifecycleMarker::SessionClosed => Self::SessionClosed,
            cc::LifecycleMarker::ConnectionClosed => Self::ConnectionClosed,
        }
    }
}

impl From<&cc::StateTransition> for ObservationStateTransition {
    fn from(value: &cc::StateTransition) -> Self {
        Self {
            from: value.from.clone(),
            to: value.to.clone(),
        }
    }
}

impl From<&cc::BleGattRecord> for ObservationBleGatt {
    fn from(value: &cc::BleGattRecord) -> Self {
        Self {
            common: (&value.common).into(),
            connection_instance: value.connection_instance.clone(),
            operation: value.operation.into(),
            service: value.service.clone(),
            characteristic: value.characteristic.clone(),
            outcome: value.outcome.into(),
            payload: value.payload.as_ref().map(Into::into),
        }
    }
}

impl From<cc::BleGattOperation> for ObservationBleGattOperation {
    fn from(value: cc::BleGattOperation) -> Self {
        match value {
            cc::BleGattOperation::Discover => Self::Discover,
            cc::BleGattOperation::Read => Self::Read,
            cc::BleGattOperation::Write => Self::Write,
            cc::BleGattOperation::Subscribe => Self::Subscribe,
            cc::BleGattOperation::Notify => Self::Notify,
            cc::BleGattOperation::Indicate => Self::Indicate,
        }
    }
}

impl From<cc::TransportOutcome> for ObservationTransportOutcome {
    fn from(value: cc::TransportOutcome) -> Self {
        match value {
            cc::TransportOutcome::Ok => Self::Ok,
            cc::TransportOutcome::Timeout => Self::Timeout,
            cc::TransportOutcome::Abort => Self::Abort,
            cc::TransportOutcome::Incomplete => Self::Incomplete,
        }
    }
}

impl From<&cc::PtpTransactionRecord> for ObservationPtpTransaction {
    fn from(value: &cc::PtpTransactionRecord) -> Self {
        Self {
            common: (&value.common).into(),
            transport: value.transport.into(),
            connection_instance: value.connection_instance.clone(),
            session: value.session.clone(),
            endpoint_set: value.endpoint_set.clone(),
            transaction_id: value.transaction_id,
            request: (&value.request).into(),
            response: value.response.as_ref().map(Into::into),
            outcome: value.outcome.into(),
            evidence_basis: value.evidence_basis.map(Into::into),
            observed_effect: value.observed_effect.map(Into::into),
            readback: value.readback.as_ref().map(Into::into),
        }
    }
}

impl From<cc::PtpTransport> for ObservationPtpTransport {
    fn from(value: cc::PtpTransport) -> Self {
        match value {
            cc::PtpTransport::PtpIp => Self::PtpIp,
            cc::PtpTransport::Usb => Self::Usb,
        }
    }
}

impl From<&cc::PtpRequest> for ObservationPtpRequest {
    fn from(value: &cc::PtpRequest) -> Self {
        Self {
            framing: value.framing.clone(),
            operation: value.operation.clone(),
            parameters: value.parameters.clone(),
            data: value.data.as_ref().map(Into::into),
        }
    }
}

impl From<&cc::PtpResponse> for ObservationPtpResponse {
    fn from(value: &cc::PtpResponse) -> Self {
        Self {
            code: value.code.clone(),
            parameters: value.parameters.clone(),
            data: value.data.as_ref().map(Into::into),
        }
    }
}

impl From<&cc::PtpDataPhase> for ObservationPtpDataPhase {
    fn from(value: &cc::PtpDataPhase) -> Self {
        Self {
            direction: value.direction.into(),
            payload: (&value.payload).into(),
        }
    }
}

impl From<cc::DataDirection> for ObservationDataDirection {
    fn from(value: cc::DataDirection) -> Self {
        match value {
            cc::DataDirection::HostToCamera => Self::HostToCamera,
            cc::DataDirection::CameraToHost => Self::CameraToHost,
        }
    }
}

impl From<cc::TransactionOutcome> for ObservationTransactionOutcome {
    fn from(value: cc::TransactionOutcome) -> Self {
        match value {
            cc::TransactionOutcome::Ok => Self::Ok,
            cc::TransactionOutcome::NonOk => Self::NonOk,
            cc::TransactionOutcome::Timeout => Self::Timeout,
            cc::TransactionOutcome::TransportAbort => Self::TransportAbort,
            cc::TransactionOutcome::Incomplete => Self::Incomplete,
        }
    }
}

impl From<&cc::Readback> for ObservationReadback {
    fn from(value: &cc::Readback) -> Self {
        match value {
            cc::Readback::Observed {
                baseline,
                request,
                settling,
                observed,
                observed_at,
                source,
            } => Self::Observed {
                baseline: json_value(baseline),
                request: json_value(request),
                settling: settling.into(),
                observed: json_value(observed),
                observed_at: observed_at.into(),
                source: (*source).into(),
            },
            cc::Readback::NotObserved { reason } => Self::NotObserved {
                reason: reason.clone(),
            },
        }
    }
}

impl From<&cc::SettlingRule> for ObservationSettlingRule {
    fn from(value: &cc::SettlingRule) -> Self {
        Self {
            deadline_ms: value.deadline_ms,
            poll_interval_ms: value.poll_interval_ms,
            stable_samples: value.stable_samples,
        }
    }
}

impl From<cc::ReadbackSource> for ObservationReadbackSource {
    fn from(value: cc::ReadbackSource) -> Self {
        match value {
            cc::ReadbackSource::DirectProperty => Self::DirectProperty,
            cc::ReadbackSource::DeclaredReadback => Self::DeclaredReadback,
            cc::ReadbackSource::Event => Self::Event,
            cc::ReadbackSource::HttpResponse => Self::HttpResponse,
        }
    }
}

impl From<&cc::PayloadMetadata> for ObservationPayloadMetadata {
    fn from(value: &cc::PayloadMetadata) -> Self {
        Self {
            length: value.length,
            sha256: value.sha256.clone(),
            inline_hex: value.inline_hex.clone(),
            stream_ranges: value.stream_ranges.iter().map(Into::into).collect(),
            ranges: value.ranges.iter().map(Into::into).collect(),
        }
    }
}

impl From<&cc::PayloadRange> for ObservationPayloadRange {
    fn from(value: &cc::PayloadRange) -> Self {
        Self {
            offset: value.offset,
            length: value.length,
            sha256: value.sha256.clone(),
        }
    }
}

impl From<&cc::PtpEventRecord> for ObservationPtpEvent {
    fn from(value: &cc::PtpEventRecord) -> Self {
        Self {
            common: (&value.common).into(),
            connection_instance: value.connection_instance.clone(),
            session: value.session.clone(),
            endpoint_set: value.endpoint_set.clone(),
            transaction_id: value.transaction_id,
            transaction_record_id: value.transaction_record_id.clone(),
            event: value.event.clone(),
            parameters: value.parameters.clone(),
            payload: value.payload.as_ref().map(Into::into),
        }
    }
}

impl From<&cc::HttpExchangeRecord> for ObservationHttpExchange {
    fn from(value: &cc::HttpExchangeRecord) -> Self {
        Self {
            common: (&value.common).into(),
            connection_instance: value.connection_instance.clone(),
            request: (&value.request).into(),
            response: value.response.as_ref().map(Into::into),
            outcome: value.outcome.into(),
        }
    }
}

impl From<&cc::HttpRequest> for ObservationHttpRequest {
    fn from(value: &cc::HttpRequest) -> Self {
        Self {
            method: value.method.clone(),
            target: value.target.clone(),
            headers: string_fields(&value.headers),
            body: value.body.as_ref().map(Into::into),
        }
    }
}

impl From<&cc::HttpResponse> for ObservationHttpResponse {
    fn from(value: &cc::HttpResponse) -> Self {
        Self {
            status: value.status,
            headers: string_fields(&value.headers),
            body: value.body.as_ref().map(Into::into),
        }
    }
}

impl From<&cc::CapabilityRecord> for ObservationCapability {
    fn from(value: &cc::CapabilityRecord) -> Self {
        Self {
            common: (&value.common).into(),
            subject: (&value.subject).into(),
            inventory_completeness: value.inventory_completeness.into(),
            evidence_basis: value.evidence_basis.into(),
            observed_effect: value.observed_effect.into(),
            readback: (&value.readback).into(),
        }
    }
}

impl From<&cc::CapabilitySubject> for ObservationCapabilitySubject {
    fn from(value: &cc::CapabilitySubject) -> Self {
        match value {
            cc::CapabilitySubject::Identity { device_version } => Self::Identity {
                device_version: device_version.clone(),
            },
            cc::CapabilitySubject::Operation {
                code,
                supported,
                canonical_name,
            } => Self::Operation {
                code: code.clone(),
                supported: *supported,
                canonical_name: canonical_name.as_deref().map(Into::into),
            },
            cc::CapabilitySubject::Property {
                code,
                supported,
                canonical_name,
                source_native_name,
                property_type,
                access,
                descriptor,
                labels,
                value_rows,
                value_profiles,
            } => Self::Property {
                code: code.clone(),
                supported: *supported,
                canonical_name: canonical_name.as_deref().map(Into::into),
                source_native_name: source_native_name.as_deref().map(Into::into),
                property_type: property_type.clone(),
                access: access.clone(),
                descriptor: descriptor.as_ref().map(Into::into),
                labels: string_fields(labels),
                value_rows: value_rows.iter().map(Into::into).collect(),
                value_profiles: value_profiles.iter().map(Into::into).collect(),
            },
        }
    }
}

impl From<&cc::SemanticNameAssertion> for ObservationSemanticNameAssertion {
    fn from(value: &cc::SemanticNameAssertion) -> Self {
        Self {
            name: value.name.clone(),
            provenance: (&value.provenance).into(),
        }
    }
}

impl From<&cc::AssertionProvenance> for ObservationAssertionProvenance {
    fn from(value: &cc::AssertionProvenance) -> Self {
        Self {
            evidence_reference: value.evidence_reference.clone(),
            epistemic: (&value.epistemic).into(),
        }
    }
}

impl From<&cc::CapabilityValueRow> for ObservationCapabilityValueRow {
    fn from(value: &cc::CapabilityValueRow) -> Self {
        Self {
            value: (&value.value).into(),
            label: value.label.clone(),
            provenance: (&value.provenance).into(),
        }
    }
}

impl From<&cc::TypedPropertyValue> for ObservationTypedPropertyValue {
    fn from(value: &cc::TypedPropertyValue) -> Self {
        match value {
            cc::TypedPropertyValue::I8 { value } => Self::I8 { value: *value },
            cc::TypedPropertyValue::I16 { value } => Self::I16 { value: *value },
            cc::TypedPropertyValue::I32 { value } => Self::I32 { value: *value },
            cc::TypedPropertyValue::I64 { value } => Self::I64 {
                value: value.clone(),
            },
            cc::TypedPropertyValue::I128 { value } => Self::I128 {
                value: value.clone(),
            },
            cc::TypedPropertyValue::U8 { value } => Self::U8 { value: *value },
            cc::TypedPropertyValue::U16 { value } => Self::U16 { value: *value },
            cc::TypedPropertyValue::U32 { value } => Self::U32 { value: *value },
            cc::TypedPropertyValue::U64 { value } => Self::U64 {
                value: value.clone(),
            },
            cc::TypedPropertyValue::U128 { value } => Self::U128 {
                value: value.clone(),
            },
            cc::TypedPropertyValue::String { value } => Self::String {
                value: value.clone(),
            },
        }
    }
}

impl From<&cc::CapabilityDescriptor> for ObservationCapabilityDescriptor {
    fn from(value: &cc::CapabilityDescriptor) -> Self {
        Self {
            form: value.form.clone(),
            values: value.values.iter().map(json_value).collect(),
        }
    }
}

impl From<&cc::CapabilityValueProfile> for ObservationCapabilityValueProfile {
    fn from(value: &cc::CapabilityValueProfile) -> Self {
        Self {
            connection: value.connection.clone(),
            mode: value.mode.clone(),
            rows: value.rows.iter().map(Into::into).collect(),
            evidence: value.evidence.clone(),
        }
    }
}

impl From<&cc::CapabilityValueProfileRow> for ObservationCapabilityValueProfileRow {
    fn from(value: &cc::CapabilityValueProfileRow) -> Self {
        Self {
            label: value.label.clone(),
            raw: value.raw,
            legal: value.legal,
            aliases: value.aliases.clone(),
            write_store_raw: value.write_store_raw,
        }
    }
}

impl From<&cc::ActionInvocationRecord> for ObservationActionInvocation {
    fn from(value: &cc::ActionInvocationRecord) -> Self {
        Self {
            common: (&value.common).into(),
            catalog_revision: value.catalog_revision.clone(),
            action_id: value.action_id.clone(),
            role: crate::cc_to_ffi_action_role(value.role),
            parameters: json_fields(&value.parameters),
            outcome: value.outcome.into(),
        }
    }
}

impl From<cc::ActionOutcome> for ObservationActionOutcome {
    fn from(value: cc::ActionOutcome) -> Self {
        match value {
            cc::ActionOutcome::Succeeded => Self::Succeeded,
            cc::ActionOutcome::Failed => Self::Failed,
            cc::ActionOutcome::Rejected => Self::Rejected,
        }
    }
}
