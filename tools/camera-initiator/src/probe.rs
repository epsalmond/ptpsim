use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use camera_config::{payload_metadata, PayloadMetadata, PayloadMetadataBuilder};
use camera_protocol_ffi::{
    build_command, build_data, parse_data_phase, parse_response, run_streaming_operation,
    DataPhaseKind, PtpExecutorTransport, PtpFraming, PtpStreamingError, PtpStreamingSink,
    PtpStreamingSinkError, PtpStreamingTransport,
};
use ptp_core::{DeviceInfo, DevicePropDesc, ObjectInfo, PropValue, Reader, Writer};
use serde::{Deserialize, Serialize};
use tokio::io::AsyncWriteExt;
use tokio::sync::Mutex;

pub const PROBE_PLAN_SCHEMA: &str = "camera-initiator-pcss-probe/v1";
const PROPERTY_CODE: &str = "propertyCode";
const OBJECT_HANDLE: &str = "objectHandle";

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProbePlan {
    pub schema: String,
    pub operations: BTreeMap<String, ProbeOperation>,
    pub inventory: InventoryPlan,
    pub object_probe: ObjectProbePlan,
    #[serde(default)]
    pub explicit_steps: Vec<ProbeInvocation>,
    #[serde(default)]
    pub reversible_writes: Vec<ReversibleWritePlan>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProbeOperation {
    pub code: u16,
    #[serde(default)]
    pub params: Vec<ProbeParameter>,
    pub data_phase: DataPhase,
    pub accepted_responses: Vec<u16>,
    pub output: OutputPolicy,
    #[serde(default = "default_timeout_ms")]
    pub timeout_ms: u64,
    #[serde(default = "default_attempts")]
    pub max_attempts: u8,
    #[serde(default = "default_retry_delay_ms")]
    pub retry_delay_ms: u64,
    #[serde(default)]
    pub retry_responses: Vec<u16>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum ProbeParameter {
    Literal(u32),
    Binding(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum DataPhase {
    None,
    In,
    Out,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum OutputPolicy {
    Discard,
    Memory,
    File,
    Stream,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct InventoryPlan {
    pub device_info: String,
    pub property_descriptor: String,
    pub property_value: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ObjectProbePlan {
    pub catalog: String,
    pub object_info: String,
    pub read_object: String,
    pub filename: String,
    pub exact_size: u64,
    #[serde(default = "default_repetitions")]
    pub repetitions: u8,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProbeInvocation {
    pub name: String,
    pub operation: String,
    #[serde(default)]
    pub args: BTreeMap<String, u32>,
    #[serde(default)]
    pub payload_hex: Option<String>,
    #[serde(default)]
    pub expect_payload_hex: Option<String>,
    #[serde(default)]
    pub output_name: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ReversibleWritePlan {
    pub name: String,
    pub baseline: ProbeInvocation,
    pub set: ProbeInvocation,
    pub verify: ProbeInvocation,
    pub restore: ProbeInvocation,
    pub verify_restored: ProbeInvocation,
}

impl ProbePlan {
    pub fn from_yaml(yaml: &str) -> Result<Self> {
        let plan: Self = serde_yaml::from_str(yaml).context("parse PCSS probe plan")?;
        plan.validate()?;
        Ok(plan)
    }

    pub fn validate(&self) -> Result<()> {
        if self.schema != PROBE_PLAN_SCHEMA {
            bail!(
                "unsupported probe-plan schema '{}'; expected '{PROBE_PLAN_SCHEMA}'",
                self.schema
            )
        }
        if self.operations.is_empty() {
            bail!("probe plan must declare operations")
        }
        for (name, operation) in &self.operations {
            validate_operation(name, operation)?;
        }
        self.require_operation(
            &self.inventory.device_info,
            DataPhase::In,
            OutputPolicy::Memory,
        )?;
        self.require_operation(
            &self.inventory.property_descriptor,
            DataPhase::In,
            OutputPolicy::Memory,
        )?;
        self.require_operation(
            &self.inventory.property_value,
            DataPhase::In,
            OutputPolicy::Memory,
        )?;
        self.require_bindings(&self.inventory.device_info, &[])?;
        self.require_bindings(&self.inventory.property_descriptor, &[PROPERTY_CODE])?;
        self.require_bindings(&self.inventory.property_value, &[PROPERTY_CODE])?;

        self.require_operation(
            &self.object_probe.catalog,
            DataPhase::In,
            OutputPolicy::Memory,
        )?;
        self.require_operation(
            &self.object_probe.object_info,
            DataPhase::In,
            OutputPolicy::Memory,
        )?;
        self.require_operation(
            &self.object_probe.read_object,
            DataPhase::In,
            OutputPolicy::Stream,
        )?;
        self.require_bindings(&self.object_probe.catalog, &[])?;
        self.require_bindings(&self.object_probe.object_info, &[OBJECT_HANDLE])?;
        self.require_bindings(&self.object_probe.read_object, &[OBJECT_HANDLE])?;
        if self.operations.iter().any(|(name, operation)| {
            operation.output == OutputPolicy::Stream && name != &self.object_probe.read_object
        }) {
            bail!("only objectProbe.readObject may use stream output")
        }
        let read_object = &self.operations[&self.object_probe.read_object];
        if read_object.accepted_responses != [ptp_core::codes::resp::OK] {
            bail!("objectProbe.readObject must accept only the standard OK response")
        }
        if self.object_probe.filename.is_empty() {
            bail!("objectProbe.filename must not be empty")
        }
        if self.object_probe.exact_size > u32::MAX as u64 {
            bail!("objectProbe.exactSize exceeds the standard ObjectInfo u32 size field")
        }
        if self.object_probe.repetitions != 3 {
            bail!("objectProbe.repetitions must be exactly 3")
        }

        let mut names = BTreeSet::new();
        for invocation in &self.explicit_steps {
            if !names.insert(invocation.name.as_str()) {
                bail!("duplicate explicit step name '{}'", invocation.name)
            }
            self.validate_invocation(invocation, InvocationUse::Explicit)?;
        }
        for write in &self.reversible_writes {
            if write.name.is_empty() {
                bail!("reversible write name must not be empty")
            }
            self.validate_invocation(&write.baseline, InvocationUse::Read)?;
            self.validate_invocation(&write.set, InvocationUse::Set)?;
            self.validate_invocation(&write.verify, InvocationUse::Read)?;
            self.validate_invocation(&write.restore, InvocationUse::Restore)?;
            self.validate_invocation(&write.verify_restored, InvocationUse::Read)?;
        }
        Ok(())
    }

    fn require_operation(
        &self,
        name: &str,
        data_phase: DataPhase,
        output: OutputPolicy,
    ) -> Result<&ProbeOperation> {
        let operation = self
            .operations
            .get(name)
            .with_context(|| format!("probe plan references unknown operation '{name}'"))?;
        if operation.data_phase != data_phase || operation.output != output {
            bail!("operation '{name}' must use dataPhase={data_phase:?} and output={output:?}")
        }
        Ok(operation)
    }

    fn require_bindings(&self, name: &str, expected: &[&str]) -> Result<()> {
        let operation = self
            .operations
            .get(name)
            .with_context(|| format!("probe plan references unknown operation '{name}'"))?;
        let actual = operation
            .params
            .iter()
            .filter_map(|parameter| match parameter {
                ProbeParameter::Literal(_) => None,
                ProbeParameter::Binding(binding) => Some(binding.as_str()),
            })
            .collect::<Vec<_>>();
        if actual != expected {
            bail!("operation '{name}' parameter bindings must be {expected:?}, got {actual:?}")
        }
        Ok(())
    }

    fn validate_invocation(
        &self,
        invocation: &ProbeInvocation,
        usage: InvocationUse,
    ) -> Result<()> {
        if invocation.name.is_empty() {
            bail!("probe invocation name must not be empty")
        }
        let operation = self
            .operations
            .get(&invocation.operation)
            .with_context(|| {
                format!(
                    "invocation '{}' references unknown operation '{}'",
                    invocation.name, invocation.operation
                )
            })?;
        let bindings = operation
            .params
            .iter()
            .filter_map(|parameter| match parameter {
                ProbeParameter::Literal(_) => None,
                ProbeParameter::Binding(binding) => Some(binding.as_str()),
            })
            .collect::<BTreeSet<_>>();
        let args = invocation
            .args
            .keys()
            .map(String::as_str)
            .collect::<BTreeSet<_>>();
        if bindings != args {
            bail!(
                "invocation '{}' args must bind {bindings:?}, got {args:?}",
                invocation.name
            )
        }
        if let Some(payload) = &invocation.payload_hex {
            decode_hex(payload).with_context(|| {
                format!("invocation '{}' has invalid payloadHex", invocation.name)
            })?;
        }
        if let Some(payload) = &invocation.expect_payload_hex {
            decode_hex(payload).with_context(|| {
                format!(
                    "invocation '{}' has invalid expectPayloadHex",
                    invocation.name
                )
            })?;
        }
        match usage {
            InvocationUse::Explicit => match operation.data_phase {
                DataPhase::Out if invocation.payload_hex.is_none() => {
                    bail!(
                        "outbound invocation '{}' requires payloadHex",
                        invocation.name
                    )
                }
                DataPhase::None if invocation.payload_hex.is_some() => {
                    bail!(
                        "no-data invocation '{}' cannot carry payloadHex",
                        invocation.name
                    )
                }
                DataPhase::In if invocation.payload_hex.is_some() => {
                    bail!(
                        "data-in invocation '{}' cannot carry payloadHex",
                        invocation.name
                    )
                }
                _ => {}
            },
            InvocationUse::Read => {
                if operation.data_phase != DataPhase::In || operation.output != OutputPolicy::Memory
                {
                    bail!(
                        "read invocation '{}' must use a memory data-in operation",
                        invocation.name
                    )
                }
                if invocation.payload_hex.is_some() || invocation.output_name.is_some() {
                    bail!(
                        "read invocation '{}' has invalid output fields",
                        invocation.name
                    )
                }
            }
            InvocationUse::Set => {
                if operation.data_phase != DataPhase::Out
                    || operation.output != OutputPolicy::Discard
                    || invocation.payload_hex.is_none()
                {
                    bail!(
                        "set invocation '{}' must use a discard data-out operation with payloadHex",
                        invocation.name
                    )
                }
            }
            InvocationUse::Restore => {
                if operation.data_phase != DataPhase::Out
                    || operation.output != OutputPolicy::Discard
                    || invocation.payload_hex.is_some()
                {
                    bail!(
                        "restore invocation '{}' must use a discard data-out operation without payloadHex",
                        invocation.name
                    )
                }
            }
        }
        if operation.data_phase != DataPhase::In && invocation.expect_payload_hex.is_some() {
            bail!(
                "non-data-in invocation '{}' cannot use expectPayloadHex",
                invocation.name
            )
        }
        match operation.output {
            OutputPolicy::File => {
                let output_name = invocation.output_name.as_deref().with_context(|| {
                    format!("file invocation '{}' requires outputName", invocation.name)
                })?;
                validate_output_name(output_name)?;
            }
            _ if invocation.output_name.is_some() => {
                bail!(
                    "non-file invocation '{}' cannot use outputName",
                    invocation.name
                )
            }
            _ => {}
        }
        Ok(())
    }
}

fn validate_operation(name: &str, operation: &ProbeOperation) -> Result<()> {
    if name.is_empty() {
        bail!("operation name must not be empty")
    }
    if operation.accepted_responses.is_empty() {
        bail!("operation '{name}' must declare acceptedResponses")
    }
    if !(1..=600_000).contains(&operation.timeout_ms) {
        bail!("operation '{name}' timeoutMs must be within 1..=600000")
    }
    if !(1..=10).contains(&operation.max_attempts) {
        bail!("operation '{name}' maxAttempts must be within 1..=10")
    }
    if operation.retry_delay_ms > 10_000 {
        bail!("operation '{name}' retryDelayMs must be within 0..=10000")
    }
    ensure_unique(name, "acceptedResponses", &operation.accepted_responses)?;
    ensure_unique(name, "retryResponses", &operation.retry_responses)?;
    if operation.max_attempts == 1 && !operation.retry_responses.is_empty() {
        bail!("operation '{name}' declares retryResponses with maxAttempts=1")
    }
    if operation
        .retry_responses
        .iter()
        .any(|response| operation.accepted_responses.contains(response))
    {
        bail!("operation '{name}' accepts a response that it also retries")
    }
    match (operation.data_phase, operation.output) {
        (DataPhase::In, _) => {}
        (_, OutputPolicy::Discard) => {}
        _ => bail!("operation '{name}' output requires a data-in phase"),
    }
    let bindings = operation
        .params
        .iter()
        .filter_map(|parameter| match parameter {
            ProbeParameter::Literal(_) => None,
            ProbeParameter::Binding(binding) => Some(binding),
        })
        .collect::<Vec<_>>();
    if bindings.iter().any(|binding| binding.is_empty()) {
        bail!("operation '{name}' has an empty parameter binding")
    }
    ensure_unique(name, "parameter bindings", &bindings)?;
    Ok(())
}

fn ensure_unique<T: Ord>(operation: &str, field: &str, values: &[T]) -> Result<()> {
    let unique = values.iter().collect::<BTreeSet<_>>();
    if unique.len() != values.len() {
        bail!("operation '{operation}' has duplicate {field}")
    }
    Ok(())
}

fn validate_output_name(name: &str) -> Result<()> {
    let path = std::path::Path::new(name);
    if name.is_empty()
        || path.is_absolute()
        || path.components().count() != 1
        || matches!(name, "." | "..")
    {
        bail!("outputName '{name}' must be one relative filename")
    }
    Ok(())
}

pub(crate) fn decode_hex(value: &str) -> Result<Vec<u8>> {
    if !value.is_ascii() {
        bail!("hex byte string must contain only ASCII digits")
    }
    if !value.len().is_multiple_of(2) {
        bail!("hex byte string must contain an even number of digits")
    }
    value
        .as_bytes()
        .chunks_exact(2)
        .map(|pair| {
            let text = std::str::from_utf8(pair)
                .context("hex byte string must contain only ASCII digits")?;
            u8::from_str_radix(text, 16).with_context(|| format!("invalid hex byte '{text}'"))
        })
        .collect()
}

fn default_timeout_ms() -> u64 {
    10_000
}

fn default_attempts() -> u8 {
    1
}

fn default_retry_delay_ms() -> u64 {
    100
}

fn default_repetitions() -> u8 {
    3
}

#[derive(Debug, Clone, Copy)]
enum InvocationUse {
    Explicit,
    Read,
    Set,
    Restore,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProbeReport {
    pub schema: String,
    pub device_info: DeviceInfoProbeReport,
    pub advertised_properties: Vec<u16>,
    pub properties: Vec<PropertyProbeReport>,
    pub explicit_steps: Vec<OperationProbeReport>,
    pub reversible_writes: Vec<ReversibleWriteReport>,
    pub object_probe: ObjectProbeReport,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DeviceInfoProbeReport {
    pub payload: PayloadMetadata,
    pub operations_supported: Vec<u16>,
    pub events_supported: Vec<u16>,
    pub device_properties_supported: Vec<u16>,
    pub capture_formats: Vec<u16>,
    pub image_formats: Vec<u16>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PropertyProbeReport {
    pub code: u16,
    pub datatype: Option<u16>,
    pub get_set: Option<u8>,
    pub descriptor_response: u16,
    pub descriptor: Option<PayloadMetadata>,
    pub current_value_response: u16,
    pub current_value: Option<PayloadMetadata>,
    pub descriptor_decode_error: Option<String>,
    pub current_value_decode_error: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OperationProbeReport {
    pub name: String,
    pub operation: u16,
    pub transaction_id: u32,
    pub response: u16,
    pub response_params: Vec<u32>,
    pub attempts: u8,
    pub command_duration_ms: u64,
    pub payload: Option<PayloadMetadata>,
    pub output_name: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReversibleWriteReport {
    pub name: String,
    pub baseline: PayloadMetadata,
    pub set_payload: PayloadMetadata,
    pub restored: bool,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ObjectProbeReport {
    pub catalog_handles: Vec<u32>,
    pub selected_handle: u32,
    pub filename: String,
    pub exact_size: u64,
    pub repetitions: Vec<ObjectRepetitionReport>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ObjectRepetitionReport {
    pub repetition: u8,
    pub transaction_id: u32,
    pub byte_count: u64,
    pub sha256: String,
    pub command_duration_ms: u64,
    pub end_to_end_duration_ms: u64,
    pub output_name: String,
}

struct OperationResult {
    operation: u16,
    transaction_id: u32,
    response: u16,
    response_params: Vec<u32>,
    attempts: u8,
    duration: Duration,
    payload: Vec<u8>,
}

/// Execute a validated plan over the already-selected shipping command
/// transport. The caller opens and closes the PTP session, so PCSS rendezvous,
/// init retries, observation assembly, and teardown remain owned by
/// `NativePtpTransport`.
pub async fn run_probe_plan<T>(
    plan: &ProbePlan,
    framing: PtpFraming,
    transport: Arc<T>,
    trace: Arc<crate::TraceWriter>,
    output_dir: PathBuf,
) -> Result<ProbeReport>
where
    T: PtpExecutorTransport + PtpStreamingTransport + 'static,
{
    plan.validate()?;
    if !matches!(framing, PtpFraming::Compressed) {
        bail!("runtime PCSS probe plans require manifest-selected compressed framing")
    }
    tokio::fs::create_dir(&output_dir)
        .await
        .with_context(|| format!("create new probe output directory {}", output_dir.display()))?;

    let device_info_operation = &plan.operations[&plan.inventory.device_info];
    let device_info_result = run_operation(
        device_info_operation,
        &BTreeMap::new(),
        None,
        Arc::clone(&transport),
        framing,
    )
    .await
    .context("read DeviceInfo")?;
    let device_info = decode_device_info_exact(&device_info_result.payload)?;
    ensure_no_duplicates(
        "DeviceInfo.DevicePropertiesSupported",
        &device_info.device_properties_supported,
    )?;
    ensure_no_duplicates(
        "DeviceInfo.OperationsSupported",
        &device_info.operations_supported,
    )?;
    ensure_no_duplicates("DeviceInfo.EventsSupported", &device_info.events_supported)?;
    ensure_no_duplicates("DeviceInfo.CaptureFormats", &device_info.capture_formats)?;
    ensure_no_duplicates("DeviceInfo.ImageFormats", &device_info.image_formats)?;

    let mut properties = Vec::with_capacity(device_info.device_properties_supported.len());
    for code in &device_info.device_properties_supported {
        let bindings = BTreeMap::from([(PROPERTY_CODE.to_string(), u32::from(*code))]);
        let descriptor_result = run_inventory_operation(
            &plan.operations[&plan.inventory.property_descriptor],
            &bindings,
            None,
            Arc::clone(&transport),
            framing,
        )
        .await
        .with_context(|| format!("read descriptor for advertised property 0x{code:04x}"))?;
        let descriptor_header = if descriptor_result.response == ptp_core::codes::resp::OK {
            let header =
                decode_descriptor_header(&descriptor_result.payload).with_context(|| {
                    format!("decode descriptor header for advertised property 0x{code:04x}")
                })?;
            if header.0 != *code {
                bail!(
                    "descriptor census mismatch: requested 0x{code:04x}, received 0x{:04x}",
                    header.0
                )
            }
            Some(header)
        } else {
            None
        };
        let descriptor =
            descriptor_header.map(|_| decode_descriptor_exact(&descriptor_result.payload));
        let value_result = run_inventory_operation(
            &plan.operations[&plan.inventory.property_value],
            &bindings,
            None,
            Arc::clone(&transport),
            framing,
        )
        .await
        .with_context(|| format!("read current value for advertised property 0x{code:04x}"))?;
        let descriptor_decode_error = descriptor
            .as_ref()
            .and_then(|result| result.as_ref().err())
            .map(|error| format!("{error:#}"));
        let current_value_decode_error =
            match descriptor.as_ref().and_then(|result| result.as_ref().ok()) {
                Some(descriptor) if value_result.response == ptp_core::codes::resp::OK => {
                    decode_property_value_exact(&value_result.payload, descriptor.datatype)
                        .err()
                        .map(|error| format!("{error:#}"))
                }
                None if value_result.response == ptp_core::codes::resp::OK => {
                    Some("not decoded because the property descriptor was not decodable".into())
                }
                _ => None,
            };
        properties.push(PropertyProbeReport {
            code: *code,
            datatype: descriptor_header.map(|header| header.1),
            get_set: descriptor_header.map(|header| header.2),
            descriptor_response: descriptor_result.response,
            descriptor: (descriptor_result.response == ptp_core::codes::resp::OK)
                .then(|| payload_metadata(&descriptor_result.payload)),
            current_value_response: value_result.response,
            current_value: (value_result.response == ptp_core::codes::resp::OK)
                .then(|| payload_metadata(&value_result.payload)),
            descriptor_decode_error,
            current_value_decode_error,
        });
    }

    let mut explicit_steps = Vec::with_capacity(plan.explicit_steps.len());
    for invocation in &plan.explicit_steps {
        explicit_steps.push(
            run_invocation(
                plan,
                invocation,
                None,
                Arc::clone(&transport),
                framing,
                &output_dir,
            )
            .await?,
        );
    }

    let mut reversible_writes = Vec::with_capacity(plan.reversible_writes.len());
    for write in &plan.reversible_writes {
        reversible_writes.push(
            run_reversible_write(plan, write, Arc::clone(&transport), framing, &output_dir).await?,
        );
    }

    let object_probe =
        run_object_probe(plan, Arc::clone(&transport), framing, trace, &output_dir).await?;
    let report = ProbeReport {
        schema: PROBE_PLAN_SCHEMA.into(),
        device_info: DeviceInfoProbeReport {
            payload: payload_metadata(&device_info_result.payload),
            operations_supported: device_info.operations_supported,
            events_supported: device_info.events_supported,
            device_properties_supported: device_info.device_properties_supported.clone(),
            capture_formats: device_info.capture_formats,
            image_formats: device_info.image_formats,
        },
        advertised_properties: device_info.device_properties_supported,
        properties,
        explicit_steps,
        reversible_writes,
        object_probe,
    };
    write_new_json(&output_dir.join("probe-report.json"), &report).await?;
    Ok(report)
}

async fn run_invocation<T>(
    plan: &ProbePlan,
    invocation: &ProbeInvocation,
    payload_override: Option<Vec<u8>>,
    transport: Arc<T>,
    framing: PtpFraming,
    output_dir: &Path,
) -> Result<OperationProbeReport>
where
    T: PtpExecutorTransport + 'static,
{
    let operation = &plan.operations[&invocation.operation];
    let payload = payload_override.or_else(|| {
        invocation
            .payload_hex
            .as_deref()
            .map(decode_hex)
            .transpose()
            .expect("validated invocation payload")
    });
    let result = run_operation(
        operation,
        &invocation.args,
        payload.as_deref(),
        transport,
        framing,
    )
    .await
    .with_context(|| format!("probe invocation '{}'", invocation.name))?;
    if let Some(expected) = &invocation.expect_payload_hex {
        let expected = decode_hex(expected).expect("validated expected payload");
        if result.payload != expected {
            bail!(
                "invocation '{}' payload did not match expectPayloadHex",
                invocation.name
            )
        }
    }
    let output_name = if operation.output == OutputPolicy::File {
        let name = invocation
            .output_name
            .as_ref()
            .expect("validated file output name");
        write_new(&output_dir.join(name), &result.payload).await?;
        Some(name.clone())
    } else {
        None
    };
    let payload = matches!(operation.output, OutputPolicy::Memory | OutputPolicy::File)
        .then(|| payload_metadata(&result.payload));
    Ok(OperationProbeReport {
        name: invocation.name.clone(),
        operation: result.operation,
        transaction_id: result.transaction_id,
        response: result.response,
        response_params: result.response_params,
        attempts: result.attempts,
        command_duration_ms: duration_ms(result.duration),
        payload,
        output_name,
    })
}

async fn run_reversible_write<T>(
    plan: &ProbePlan,
    write: &ReversibleWritePlan,
    transport: Arc<T>,
    framing: PtpFraming,
    output_dir: &Path,
) -> Result<ReversibleWriteReport>
where
    T: PtpExecutorTransport + 'static,
{
    let baseline = run_invocation(
        plan,
        &write.baseline,
        None,
        Arc::clone(&transport),
        framing,
        output_dir,
    )
    .await
    .with_context(|| format!("reversible write '{}' baseline", write.name))?;
    let baseline_bytes = run_operation_payload_against_report(plan, &write.baseline, &baseline)?;
    let set_bytes = decode_hex(
        write
            .set
            .payload_hex
            .as_deref()
            .expect("validated reversible set payload"),
    )?;

    let primary = async {
        run_invocation(
            plan,
            &write.set,
            None,
            Arc::clone(&transport),
            framing,
            output_dir,
        )
        .await?;
        let verified = run_invocation(
            plan,
            &write.verify,
            None,
            Arc::clone(&transport),
            framing,
            output_dir,
        )
        .await?;
        let verified_bytes = run_operation_payload_against_report(plan, &write.verify, &verified)?;
        if verified_bytes != set_bytes {
            bail!("set verification payload did not match the requested write")
        }
        Ok::<(), anyhow::Error>(())
    }
    .await;

    // A set can have taken effect even when its response or verification
    // fails, so cleanup is unconditional after the set attempt begins.
    let cleanup = async {
        run_invocation(
            plan,
            &write.restore,
            Some(baseline_bytes.clone()),
            Arc::clone(&transport),
            framing,
            output_dir,
        )
        .await?;
        let restored = run_invocation(
            plan,
            &write.verify_restored,
            None,
            transport,
            framing,
            output_dir,
        )
        .await?;
        let restored_bytes =
            run_operation_payload_against_report(plan, &write.verify_restored, &restored)?;
        if restored_bytes != baseline_bytes {
            bail!("restored-value verification did not match the baseline")
        }
        Ok::<(), anyhow::Error>(())
    }
    .await;

    match (primary, cleanup) {
        (Ok(()), Ok(())) => Ok(ReversibleWriteReport {
            name: write.name.clone(),
            baseline: payload_metadata(&baseline_bytes),
            set_payload: payload_metadata(&set_bytes),
            restored: true,
        }),
        (Err(primary), Ok(())) => {
            bail!(
                "reversible write '{}' failed: {primary}; cleanup succeeded",
                write.name
            )
        }
        (Ok(()), Err(cleanup)) => {
            bail!(
                "reversible write '{}' cleanup failed: {cleanup}",
                write.name
            )
        }
        (Err(primary), Err(cleanup)) => bail!(
            "reversible write '{}' failed: {primary}; cleanup also failed: {cleanup}",
            write.name
        ),
    }
}

fn run_operation_payload_against_report(
    plan: &ProbePlan,
    invocation: &ProbeInvocation,
    report: &OperationProbeReport,
) -> Result<Vec<u8>> {
    let metadata = report
        .payload
        .as_ref()
        .with_context(|| format!("invocation '{}' did not retain a payload", invocation.name))?;
    let inline = metadata.inline_hex.as_deref().with_context(|| {
        format!(
            "invocation '{}' payload exceeded the reversible-write memory ceiling",
            invocation.name
        )
    })?;
    let bytes = decode_hex(inline)?;
    let operation = &plan.operations[&invocation.operation];
    if operation.output != OutputPolicy::Memory {
        bail!("invocation '{}' did not use memory output", invocation.name)
    }
    Ok(bytes)
}

async fn run_object_probe<T>(
    plan: &ProbePlan,
    transport: Arc<T>,
    framing: PtpFraming,
    trace: Arc<crate::TraceWriter>,
    output_dir: &Path,
) -> Result<ObjectProbeReport>
where
    T: PtpExecutorTransport + PtpStreamingTransport + 'static,
{
    let catalog = run_operation(
        &plan.operations[&plan.object_probe.catalog],
        &BTreeMap::new(),
        None,
        Arc::clone(&transport),
        framing,
    )
    .await
    .context("read complete object catalog")?;
    let handles = decode_handle_catalog_exact(&catalog.payload)?;
    ensure_no_duplicates("object handle catalog", &handles)?;

    let mut matches = Vec::new();
    for handle in &handles {
        let bindings = BTreeMap::from([(OBJECT_HANDLE.to_string(), *handle)]);
        let result = run_operation(
            &plan.operations[&plan.object_probe.object_info],
            &bindings,
            None,
            Arc::clone(&transport),
            framing,
        )
        .await
        .with_context(|| format!("read ObjectInfo for catalog handle 0x{handle:08x}"))?;
        let info = decode_object_info_exact(&result.payload)
            .with_context(|| format!("decode ObjectInfo for catalog handle 0x{handle:08x}"))?;
        if info.filename == plan.object_probe.filename
            && u64::from(info.object_compressed_size) == plan.object_probe.exact_size
        {
            matches.push((*handle, info));
        }
    }
    if matches.len() != 1 {
        bail!(
            "object catalog selection for filename {:?} and exact size {} matched {} objects",
            plan.object_probe.filename,
            plan.object_probe.exact_size,
            matches.len()
        )
    }
    let (selected_handle, selected_info) = matches.pop().expect("one object match");
    let operation = &plan.operations[&plan.object_probe.read_object];
    let params = resolve_params(
        operation,
        &BTreeMap::from([(OBJECT_HANDLE.to_string(), selected_handle)]),
    )?;
    let mut repetitions = Vec::with_capacity(3);
    for repetition in 1..=plan.object_probe.repetitions {
        let output_name = format!("object-repetition-{repetition:02}.bin");
        let sink = Arc::new(StreamingFileSink::new(output_dir.join(&output_name))?);
        let end_to_end_started = Instant::now();
        let mut attempt = 0;
        let (outcome, command_duration) = loop {
            attempt += 1;
            let command_started = Instant::now();
            let raw: Arc<dyn PtpStreamingTransport> = transport.clone();
            let result = tokio::time::timeout(
                Duration::from_millis(operation.timeout_ms),
                run_streaming_operation(
                    framing,
                    operation.code,
                    params.clone(),
                    raw,
                    sink.clone(),
                    Some(plan.object_probe.exact_size),
                ),
            )
            .await
            .map_err(|_| {
                anyhow::anyhow!(
                    "whole-object repetition {repetition} timed out after {} ms",
                    operation.timeout_ms
                )
            })?;
            let command_duration = command_started.elapsed();
            match result {
                Ok(outcome) => break (outcome, command_duration),
                Err(PtpStreamingError::Response {
                    response_code,
                    transaction_id,
                    response_params,
                }) => {
                    let started = sink.started().await;
                    let payload = if started {
                        Some(sink.payload_metadata().await)
                    } else {
                        None
                    };
                    trace.complete_streaming_response(
                        transaction_id,
                        response_code,
                        response_params.clone(),
                        payload,
                    )?;
                    if operation.retry_responses.contains(&response_code)
                        && attempt < operation.max_attempts
                        && !started
                    {
                        PtpStreamingTransport::sleep(
                            transport.as_ref(),
                            operation.retry_delay_ms as u32,
                        )
                        .await?;
                    } else {
                        return Err(PtpStreamingError::Response {
                            response_code,
                            transaction_id,
                            response_params,
                        }
                        .into());
                    }
                }
                Err(error) => return Err(error.into()),
            }
        };
        let metadata = sink.payload_metadata().await;
        trace.complete_streaming_transaction(
            outcome.transaction_id,
            outcome.response_params.clone(),
            metadata.clone(),
        )?;
        sink.commit().await?;
        repetitions.push(ObjectRepetitionReport {
            repetition,
            transaction_id: outcome.transaction_id,
            byte_count: metadata.length,
            sha256: metadata.sha256,
            command_duration_ms: duration_ms(command_duration),
            end_to_end_duration_ms: duration_ms(end_to_end_started.elapsed()),
            output_name,
        });
    }
    let expected_hash = &repetitions[0].sha256;
    if repetitions.iter().any(|repetition| {
        repetition.byte_count != plan.object_probe.exact_size || repetition.sha256 != *expected_hash
    }) {
        bail!("whole-object repetitions produced a byte-count or SHA-256 mismatch")
    }
    Ok(ObjectProbeReport {
        catalog_handles: handles,
        selected_handle,
        filename: selected_info.filename,
        exact_size: u64::from(selected_info.object_compressed_size),
        repetitions,
    })
}

async fn run_operation<T>(
    operation: &ProbeOperation,
    bindings: &BTreeMap<String, u32>,
    outbound_payload: Option<&[u8]>,
    transport: Arc<T>,
    framing: PtpFraming,
) -> Result<OperationResult>
where
    T: PtpExecutorTransport + 'static,
{
    run_operation_with_policy(
        operation,
        bindings,
        outbound_payload,
        transport,
        framing,
        false,
    )
    .await
}

async fn run_inventory_operation<T>(
    operation: &ProbeOperation,
    bindings: &BTreeMap<String, u32>,
    outbound_payload: Option<&[u8]>,
    transport: Arc<T>,
    framing: PtpFraming,
) -> Result<OperationResult>
where
    T: PtpExecutorTransport + 'static,
{
    run_operation_with_policy(
        operation,
        bindings,
        outbound_payload,
        transport,
        framing,
        true,
    )
    .await
}

async fn run_operation_with_policy<T>(
    operation: &ProbeOperation,
    bindings: &BTreeMap<String, u32>,
    outbound_payload: Option<&[u8]>,
    transport: Arc<T>,
    framing: PtpFraming,
    allow_response_errors: bool,
) -> Result<OperationResult>
where
    T: PtpExecutorTransport + 'static,
{
    let params = resolve_params(operation, bindings)?;
    let operation_started = Instant::now();
    for attempt in 1..=operation.max_attempts {
        let result = tokio::time::timeout(
            Duration::from_millis(operation.timeout_ms),
            run_operation_attempt(
                operation,
                params.clone(),
                outbound_payload,
                Arc::clone(&transport),
                framing,
            ),
        )
        .await
        .map_err(|_| {
            anyhow::anyhow!(
                "operation 0x{:04x} attempt {attempt} timed out after {} ms",
                operation.code,
                operation.timeout_ms
            )
        })??;
        if operation.retry_responses.contains(&result.response) {
            if result.saw_data {
                bail!(
                    "operation 0x{:04x} returned retry response 0x{:04x} after a data phase",
                    operation.code,
                    result.response
                )
            }
            if attempt == operation.max_attempts {
                bail!(
                    "operation 0x{:04x} exhausted {} attempts on response 0x{:04x}",
                    operation.code,
                    operation.max_attempts,
                    result.response
                )
            }
            PtpExecutorTransport::sleep(transport.as_ref(), operation.retry_delay_ms as u32)
                .await?;
            continue;
        }
        let response_is_accepted = operation.accepted_responses.contains(&result.response);
        let response_is_error = result.response != ptp_core::codes::resp::OK;
        if allow_response_errors && response_is_error && !result.payload.is_empty() {
            bail!(
                "operation 0x{:04x} returned non-empty data with error response 0x{:04x}",
                operation.code,
                result.response
            )
        }
        if !response_is_accepted {
            bail!(
                "operation 0x{:04x} returned unexpected response 0x{:04x}",
                operation.code,
                result.response
            )
        }
        let recordable_response_error = allow_response_errors && response_is_error;
        if operation.data_phase == DataPhase::In && !result.saw_data && !recordable_response_error {
            bail!(
                "operation 0x{:04x} returned an accepted response without its data phase",
                operation.code
            )
        }
        if operation.data_phase != DataPhase::In && result.saw_data {
            bail!(
                "operation 0x{:04x} returned an unexpected data phase",
                operation.code
            )
        }
        return Ok(OperationResult {
            operation: operation.code,
            transaction_id: result.transaction_id,
            response: result.response,
            response_params: result.response_params,
            attempts: attempt,
            duration: operation_started.elapsed(),
            payload: if operation.output == OutputPolicy::Discard {
                Vec::new()
            } else {
                result.payload
            },
        });
    }
    unreachable!("validated operation has at least one attempt")
}

struct OperationAttempt {
    transaction_id: u32,
    response: u16,
    response_params: Vec<u32>,
    saw_data: bool,
    payload: Vec<u8>,
}

async fn run_operation_attempt<T>(
    operation: &ProbeOperation,
    params: Vec<u32>,
    outbound_payload: Option<&[u8]>,
    transport: Arc<T>,
    framing: PtpFraming,
) -> Result<OperationAttempt>
where
    T: PtpExecutorTransport + 'static,
{
    let transaction_id = PtpExecutorTransport::reserve_transaction_id(transport.as_ref()).await?;
    let command = build_command(framing, operation.code, transaction_id, params)?;
    PtpExecutorTransport::send_command_frame(transport.as_ref(), command).await?;
    match operation.data_phase {
        DataPhase::Out => {
            let payload = outbound_payload.context("data-out operation has no payload")?;
            let data = build_data(framing, operation.code, transaction_id, payload.to_vec())?;
            PtpExecutorTransport::send_command_frame(transport.as_ref(), data).await?;
        }
        DataPhase::None | DataPhase::In => {
            if outbound_payload.is_some() {
                bail!("non-data-out operation received an outbound payload")
            }
        }
    }

    let mut payload = Vec::new();
    let mut saw_data = false;
    for _ in 0..3 {
        let frame = PtpExecutorTransport::next_command_frame(transport.as_ref()).await?;
        if let Ok(response) = parse_response(framing, frame.clone()) {
            if response.txn != transaction_id {
                bail!(
                    "operation 0x{:04x} expected response transaction {transaction_id}, got {}",
                    operation.code,
                    response.txn
                )
            }
            return Ok(OperationAttempt {
                transaction_id,
                response: response.response_code,
                response_params: response.params,
                saw_data,
                payload,
            });
        }
        let data = parse_data_phase(framing, frame).with_context(|| {
            format!(
                "operation 0x{:04x} received neither a data frame nor a response",
                operation.code
            )
        })?;
        if data.txn != transaction_id {
            bail!(
                "operation 0x{:04x} expected data transaction {transaction_id}, got {}",
                operation.code,
                data.txn
            )
        }
        if data.kind != DataPhaseKind::Data || saw_data {
            bail!(
                "operation 0x{:04x} returned an invalid compressed data sequence",
                operation.code
            )
        }
        saw_data = true;
        payload = data.payload;
    }
    bail!(
        "operation 0x{:04x} exceeded the bounded command-frame sequence",
        operation.code
    )
}

fn resolve_params(
    operation: &ProbeOperation,
    bindings: &BTreeMap<String, u32>,
) -> Result<Vec<u32>> {
    operation
        .params
        .iter()
        .map(|parameter| match parameter {
            ProbeParameter::Literal(value) => Ok(*value),
            ProbeParameter::Binding(name) => bindings
                .get(name)
                .copied()
                .with_context(|| format!("operation parameter binding '{name}' is missing")),
        })
        .collect()
}

fn decode_device_info_exact(payload: &[u8]) -> Result<DeviceInfo> {
    let value = DeviceInfo::decode(payload).context("decode DeviceInfo")?;
    let mut encoded = Writer::new();
    value.encode(&mut encoded).context("re-encode DeviceInfo")?;
    if encoded.as_slice() != payload {
        bail!("DeviceInfo payload is non-canonical, truncated, or has trailing bytes")
    }
    Ok(value)
}

fn decode_descriptor_exact(payload: &[u8]) -> Result<DevicePropDesc> {
    let value = DevicePropDesc::decode(payload).context("decode DevicePropDesc")?;
    let mut encoded = Writer::new();
    value
        .encode(&mut encoded)
        .context("re-encode DevicePropDesc")?;
    if encoded.as_slice() != payload {
        bail!("DevicePropDesc payload is non-canonical, truncated, or has trailing bytes")
    }
    Ok(value)
}

fn decode_descriptor_header(payload: &[u8]) -> Result<(u16, u16, u8)> {
    if payload.len() < 5 {
        bail!("DevicePropDesc payload is too short for its fixed header")
    }
    Ok((
        u16::from_le_bytes([payload[0], payload[1]]),
        u16::from_le_bytes([payload[2], payload[3]]),
        payload[4],
    ))
}

fn decode_property_value_exact(payload: &[u8], datatype: u16) -> Result<PropValue> {
    let mut reader = Reader::new(payload);
    let value = PropValue::decode(&mut reader, datatype).context("decode property value")?;
    if reader.remaining() != 0 {
        bail!("property value payload has trailing bytes")
    }
    Ok(value)
}

fn decode_handle_catalog_exact(payload: &[u8]) -> Result<Vec<u32>> {
    let mut reader = Reader::new(payload);
    let handles = reader
        .ptp_array(|reader| reader.u32())
        .context("decode object handle catalog")?;
    if reader.remaining() != 0 {
        bail!("object handle catalog has trailing bytes")
    }
    Ok(handles)
}

fn decode_object_info_exact(payload: &[u8]) -> Result<ObjectInfo> {
    let value = ObjectInfo::decode(payload).context("decode ObjectInfo")?;
    let mut encoded = Writer::new();
    value.encode(&mut encoded).context("re-encode ObjectInfo")?;
    if encoded.as_slice() != payload {
        bail!("ObjectInfo payload is non-canonical, truncated, or has trailing bytes")
    }
    Ok(value)
}

fn ensure_no_duplicates<T: Ord + std::fmt::Debug>(name: &str, values: &[T]) -> Result<()> {
    let unique = values.iter().collect::<BTreeSet<_>>();
    if unique.len() != values.len() {
        bail!("{name} contains duplicate entries: {values:?}")
    }
    Ok(())
}

fn duration_ms(duration: Duration) -> u64 {
    duration.as_millis().try_into().unwrap_or(u64::MAX)
}

async fn write_new(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut file = tokio::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .await
        .with_context(|| format!("create new output {}", path.display()))?;
    file.write_all(bytes)
        .await
        .with_context(|| format!("write output {}", path.display()))?;
    file.sync_all()
        .await
        .with_context(|| format!("sync output {}", path.display()))
}

async fn write_new_json(path: &Path, value: &impl Serialize) -> Result<()> {
    let mut bytes = serde_json::to_vec_pretty(value)?;
    bytes.push(b'\n');
    write_new(path, &bytes).await
}

pub struct StreamingFileSink {
    final_path: PathBuf,
    partial_path: PathBuf,
    file: Mutex<Option<tokio::fs::File>>,
    payload: Mutex<PayloadMetadataBuilder>,
    started: Mutex<bool>,
}

impl StreamingFileSink {
    pub fn new(final_path: PathBuf) -> Result<Self> {
        if final_path.exists() {
            bail!("stream output already exists: {}", final_path.display())
        }
        let mut partial_name = final_path.as_os_str().to_os_string();
        partial_name.push(".partial");
        let partial_path = PathBuf::from(partial_name);
        if partial_path.exists() {
            bail!(
                "stream partial output already exists: {}",
                partial_path.display()
            )
        }
        Ok(Self {
            final_path,
            partial_path,
            file: Mutex::new(None),
            payload: Mutex::new(PayloadMetadataBuilder::new()),
            started: Mutex::new(false),
        })
    }

    pub async fn commit(&self) -> Result<()> {
        let mut guard = self.file.lock().await;
        let file = guard
            .take()
            .context("stream sink did not open its partial file")?;
        file.sync_all().await?;
        drop(file);
        tokio::fs::hard_link(&self.partial_path, &self.final_path)
            .await
            .with_context(|| {
                format!(
                    "link {} to new output {}",
                    self.partial_path.display(),
                    self.final_path.display()
                )
            })?;
        tokio::fs::remove_file(&self.partial_path)
            .await
            .with_context(|| format!("remove committed partial {}", self.partial_path.display()))
    }

    pub async fn payload_metadata(&self) -> PayloadMetadata {
        self.payload.lock().await.metadata()
    }

    async fn started(&self) -> bool {
        *self.started.lock().await
    }
}

#[async_trait]
impl PtpStreamingSink for StreamingFileSink {
    async fn begin(&self, _total_bytes: u64) -> Result<(), PtpStreamingSinkError> {
        let file = tokio::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&self.partial_path)
            .await
            .map_err(|error| PtpStreamingSinkError::Failed {
                detail: format!("create new {}: {error}", self.partial_path.display()),
            })?;
        *self.file.lock().await = Some(file);
        *self.started.lock().await = true;
        Ok(())
    }

    async fn write(&self, chunk: Vec<u8>) -> Result<(), PtpStreamingSinkError> {
        let mut guard = self.file.lock().await;
        let file = guard
            .as_mut()
            .ok_or_else(|| PtpStreamingSinkError::Failed {
                detail: "stream sink write arrived before begin".into(),
            })?;
        file.write_all(&chunk)
            .await
            .map_err(|error| PtpStreamingSinkError::Failed {
                detail: format!("write {}: {error}", self.partial_path.display()),
            })?;
        self.payload.lock().await.update(&chunk);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
    use std::sync::Mutex as StdMutex;
    use std::time::{SystemTime, UNIX_EPOCH};

    use camera_protocol_ffi::{PtpSessionOpenResult, PtpTransportError, SocketRole};
    use ptp_core::codes::{datatype_code, op, resp};
    use ptp_core::{OperationResponse, PtpIpPacket};

    static NEXT_TEST_DIRECTORY: AtomicUsize = AtomicUsize::new(0);

    const PLAN: &str = r#"
schema: camera-initiator-pcss-probe/v1
operations:
  info: { code: 4097, dataPhase: in, acceptedResponses: [8193], output: memory }
  desc: { code: 4116, params: [propertyCode], dataPhase: in, acceptedResponses: [8193], output: memory }
  value: { code: 4117, params: [propertyCode], dataPhase: in, acceptedResponses: [8193], output: memory }
  handles: { code: 4103, params: [4294967295, 0, 0], dataPhase: in, acceptedResponses: [8193], output: memory }
  objectInfo: { code: 4104, params: [objectHandle], dataPhase: in, acceptedResponses: [8193], output: memory }
  object: { code: 4105, params: [objectHandle], dataPhase: in, acceptedResponses: [8193], output: stream }
  readToggle: { code: 36865, params: [7], dataPhase: in, acceptedResponses: [8193], output: memory }
  writeToggle: { code: 36866, params: [7], dataPhase: out, acceptedResponses: [8193], output: discard }
inventory: { deviceInfo: info, propertyDescriptor: desc, propertyValue: value }
objectProbe:
  catalog: handles
  objectInfo: objectInfo
  readObject: object
  filename: SYNTHETIC.BIN
  exactSize: 4
  repetitions: 3
reversibleWrites:
  - name: toggle
    baseline: { name: baseline, operation: readToggle }
    set: { name: set, operation: writeToggle, payloadHex: "0100" }
    verify: { name: verify, operation: readToggle }
    restore: { name: restore, operation: writeToggle }
    verifyRestored: { name: verify-restored, operation: readToggle }
"#;

    #[test]
    fn parses_a_strict_generic_probe_plan() {
        let plan = ProbePlan::from_yaml(PLAN).expect("valid generic plan");
        assert_eq!(plan.object_probe.repetitions, 3);
        assert_eq!(plan.operations["desc"].code, 0x1014);
    }

    #[test]
    fn rejects_unknown_fields_and_non_threefold_object_reads() {
        let unknown = PLAN.replacen(
            "exactSize: 4",
            "exactSize: 4\n  manufacturerCatalog: [1, 2]",
            1,
        );
        assert!(ProbePlan::from_yaml(&unknown)
            .unwrap_err()
            .to_string()
            .contains("parse PCSS probe plan"));

        let repetitions = PLAN.replacen("repetitions: 3", "repetitions: 2", 1);
        assert!(ProbePlan::from_yaml(&repetitions)
            .unwrap_err()
            .to_string()
            .contains("exactly 3"));
    }

    #[test]
    fn rejects_unbounded_or_ambiguous_operation_policy() {
        let retry = PLAN.replacen(
            "info: { code: 4097, dataPhase: in, acceptedResponses: [8193], output: memory }",
            "info: { code: 4097, dataPhase: in, acceptedResponses: [8193], output: memory, retryResponses: [8193], maxAttempts: 2 }",
            1,
        );
        assert!(ProbePlan::from_yaml(&retry)
            .unwrap_err()
            .to_string()
            .contains("also retries"));

        assert!(decode_hex("a€")
            .unwrap_err()
            .to_string()
            .contains("only ASCII digits"));
    }

    #[tokio::test]
    async fn synthetic_loopback_fans_out_properties_selects_stable_identity_and_never_deletes() {
        let plan = ProbePlan::from_yaml(PLAN).unwrap();
        for handle in [7, 0x00ff_1020] {
            let transport = Arc::new(SyntheticLoopback::new(handle));
            let temporary = TestDirectory::new();
            let output = temporary.path().join("probe-output");
            let report = run_probe_plan(
                &plan,
                PtpFraming::Compressed,
                Arc::clone(&transport),
                Arc::new(crate::TraceWriter::new(
                    crate::TraceFormat::Jsonl,
                    Box::new(std::io::sink()),
                )),
                output.clone(),
            )
            .await
            .expect("synthetic loopback probe succeeds");

            assert_eq!(report.advertised_properties, [0x5001, 0x5002]);
            assert_eq!(report.properties.len(), 2);
            assert_eq!(report.object_probe.selected_handle, handle);
            assert_eq!(report.object_probe.repetitions.len(), 3);
            assert!(report
                .object_probe
                .repetitions
                .windows(2)
                .all(|pair| pair[0].byte_count == pair[1].byte_count
                    && pair[0].sha256 == pair[1].sha256));
            assert!(output.join("probe-report.json").is_file());
            for repetition in 1..=3 {
                assert_eq!(
                    std::fs::read(output.join(format!("object-repetition-{repetition:02}.bin")))
                        .unwrap(),
                    b"DATA"
                );
            }

            let operations = transport.operations();
            assert_eq!(operations.iter().filter(|&&code| code == 0x1014).count(), 2);
            assert_eq!(operations.iter().filter(|&&code| code == 0x1015).count(), 2);
            assert_eq!(operations.iter().filter(|&&code| code == 0x1009).count(), 3);
            assert!(!operations.contains(&op::DELETE_OBJECT));
            assert_eq!(transport.write_payloads(), [vec![1, 0], vec![0, 0]]);
        }
    }

    #[tokio::test]
    async fn property_census_preserves_undecodable_descriptor_and_continues() {
        let plan = ProbePlan::from_yaml(PLAN).unwrap();
        let transport = Arc::new(SyntheticLoopback::with_failure(
            7,
            SyntheticFailure::NonCanonicalDescriptor,
        ));
        let temporary = TestDirectory::new();
        let report = run_probe_plan(
            &plan,
            PtpFraming::Compressed,
            Arc::clone(&transport),
            Arc::new(crate::TraceWriter::new(
                crate::TraceFormat::Jsonl,
                Box::new(std::io::sink()),
            )),
            temporary.path().join("probe-output"),
        )
        .await
        .expect("an undecodable descriptor is recorded without truncating the census");

        assert_eq!(report.properties.len(), 2);
        let first = &report.properties[0];
        assert_eq!(first.code, 0x5001);
        assert_eq!(first.datatype, Some(datatype_code::UINT16));
        assert_eq!(first.get_set, Some(0));
        assert!(first.descriptor_decode_error.is_some());
        assert_eq!(
            first.current_value_decode_error.as_deref(),
            Some("not decoded because the property descriptor was not decodable")
        );
        assert_eq!(first.current_value.as_ref().unwrap().length, 2);
        assert_eq!(
            transport
                .operations()
                .iter()
                .filter(|&&code| code == 0x1015)
                .count(),
            2
        );
    }

    #[tokio::test]
    async fn property_census_rejects_an_unlisted_error_response() {
        let plan = ProbePlan::from_yaml(PLAN).unwrap();
        let transport = Arc::new(SyntheticLoopback::with_failure(
            7,
            SyntheticFailure::UnexpectedResponse,
        ));
        let temporary = TestDirectory::new();
        let error = run_probe_plan(
            &plan,
            PtpFraming::Compressed,
            transport,
            Arc::new(crate::TraceWriter::new(
                crate::TraceFormat::Jsonl,
                Box::new(std::io::sink()),
            )),
            temporary.path().join("probe-output"),
        )
        .await
        .expect_err("an unlisted response must fail closed");

        assert!(format!("{error:#}").contains("unexpected response 0x2002"));
    }

    #[tokio::test]
    async fn property_census_records_an_allowlisted_error_response() {
        let yaml = PLAN.replacen(
            "value: { code: 4117, params: [propertyCode], dataPhase: in, acceptedResponses: [8193], output: memory }",
            "value: { code: 4117, params: [propertyCode], dataPhase: in, acceptedResponses: [8193, 8194], output: memory }",
            1,
        );
        let plan = ProbePlan::from_yaml(&yaml).unwrap();
        let transport = Arc::new(SyntheticLoopback::with_failure(
            7,
            SyntheticFailure::UnexpectedResponse,
        ));
        let temporary = TestDirectory::new();
        let report = run_probe_plan(
            &plan,
            PtpFraming::Compressed,
            transport,
            Arc::new(crate::TraceWriter::new(
                crate::TraceFormat::Jsonl,
                Box::new(std::io::sink()),
            )),
            temporary.path().join("probe-output"),
        )
        .await
        .expect("an allowlisted rejected property value is recorded");

        assert_eq!(report.properties.len(), 2);
        assert!(report.properties.iter().all(|property| {
            property.current_value_response == resp::GENERAL_ERROR
                && property.current_value.is_none()
                && property.current_value_decode_error.is_none()
        }));
    }

    #[tokio::test]
    async fn property_census_rejects_nonempty_data_with_an_error_response() {
        let yaml = PLAN.replacen(
            "value: { code: 4117, params: [propertyCode], dataPhase: in, acceptedResponses: [8193], output: memory }",
            "value: { code: 4117, params: [propertyCode], dataPhase: in, acceptedResponses: [8193, 8194], output: memory }",
            1,
        );
        let plan = ProbePlan::from_yaml(&yaml).unwrap();
        let transport = Arc::new(SyntheticLoopback::with_failure(
            7,
            SyntheticFailure::ErrorResponseWithData,
        ));
        let temporary = TestDirectory::new();
        let error = run_probe_plan(
            &plan,
            PtpFraming::Compressed,
            transport,
            Arc::new(crate::TraceWriter::new(
                crate::TraceFormat::Jsonl,
                Box::new(std::io::sink()),
            )),
            temporary.path().join("probe-output"),
        )
        .await
        .expect_err("an error response with non-empty data must fail closed");

        assert!(format!("{error:#}").contains("non-empty data with error response 0x2002"));
    }

    #[tokio::test]
    async fn object_probe_reports_successful_attempt_separately_from_retry_delay() {
        let yaml = PLAN.replacen(
            "object: { code: 4105, params: [objectHandle], dataPhase: in, acceptedResponses: [8193], output: stream }",
            "object: { code: 4105, params: [objectHandle], dataPhase: in, acceptedResponses: [8193], output: stream, retryResponses: [8217], maxAttempts: 2, retryDelayMs: 30 }",
            1,
        );
        let plan = ProbePlan::from_yaml(&yaml).unwrap();
        let transport = Arc::new(SyntheticLoopback::with_failure(
            7,
            SyntheticFailure::RetryOnce,
        ));
        let temporary = TestDirectory::new();
        let report = run_probe_plan(
            &plan,
            PtpFraming::Compressed,
            transport,
            Arc::new(crate::TraceWriter::new(
                crate::TraceFormat::Jsonl,
                Box::new(std::io::sink()),
            )),
            temporary.path().join("probe-output"),
        )
        .await
        .expect("retryable first response succeeds on retry");

        let first = &report.object_probe.repetitions[0];
        assert!(first.end_to_end_duration_ms >= first.command_duration_ms + 25);
    }

    #[tokio::test]
    async fn synthetic_loopback_preserves_primary_and_cleanup_failures() {
        let plan = ProbePlan::from_yaml(PLAN).unwrap();
        let transport = Arc::new(SyntheticLoopback::new(7));
        transport
            .wrong_set_verification
            .store(true, Ordering::Release);
        transport.fail_restore.store(true, Ordering::Release);
        let temporary = TestDirectory::new();
        let output = temporary.path().join("probe-output");
        let error = run_probe_plan(
            &plan,
            PtpFraming::Compressed,
            Arc::clone(&transport),
            Arc::new(crate::TraceWriter::new(
                crate::TraceFormat::Jsonl,
                Box::new(std::io::sink()),
            )),
            output,
        )
        .await
        .expect_err("verification and restore both fail closed");
        let detail = format!("{error:#}");
        assert!(detail.contains("set verification payload did not match"));
        assert!(detail.contains("cleanup also failed"));
        assert_eq!(transport.write_payloads(), [vec![1, 0], vec![0, 0]]);
    }

    #[tokio::test]
    async fn synthetic_loopback_rejects_catalog_and_transfer_ambiguity() {
        for failure in [
            SyntheticFailure::DuplicateHandle,
            SyntheticFailure::TrailingCatalogBytes,
            SyntheticFailure::DuplicateObjectIdentity,
            SyntheticFailure::HashMismatch,
        ] {
            let plan = ProbePlan::from_yaml(PLAN).unwrap();
            let transport = Arc::new(SyntheticLoopback::with_failure(7, failure));
            let temporary = TestDirectory::new();
            let output = temporary.path().join("probe-output");
            let error = run_probe_plan(
                &plan,
                PtpFraming::Compressed,
                transport,
                Arc::new(crate::TraceWriter::new(
                    crate::TraceFormat::Jsonl,
                    Box::new(std::io::sink()),
                )),
                output,
            )
            .await
            .expect_err("synthetic failure must fail closed");
            let detail = format!("{error:#}");
            match failure {
                SyntheticFailure::DuplicateHandle => assert!(detail.contains("duplicate entries")),
                SyntheticFailure::TrailingCatalogBytes => {
                    assert!(detail.contains("trailing bytes"))
                }
                SyntheticFailure::DuplicateObjectIdentity => {
                    assert!(detail.contains("matched 2 objects"))
                }
                SyntheticFailure::UnexpectedResponse | SyntheticFailure::ErrorResponseWithData => {
                    unreachable!("not a catalog or transfer ambiguity case")
                }
                SyntheticFailure::HashMismatch => assert!(detail.contains("SHA-256 mismatch")),
                SyntheticFailure::NonCanonicalDescriptor | SyntheticFailure::RetryOnce => {
                    unreachable!("not a fail-closed case")
                }
            }
        }
    }

    #[tokio::test]
    async fn probe_outputs_never_overwrite_an_existing_path() {
        let plan = ProbePlan::from_yaml(PLAN).unwrap();
        let transport = Arc::new(SyntheticLoopback::new(7));
        let temporary = TestDirectory::new();
        let output = temporary.path().join("probe-output");
        std::fs::create_dir(&output).unwrap();
        std::fs::write(output.join("sentinel"), b"keep").unwrap();

        let error = run_probe_plan(
            &plan,
            PtpFraming::Compressed,
            transport,
            Arc::new(crate::TraceWriter::new(
                crate::TraceFormat::Jsonl,
                Box::new(std::io::sink()),
            )),
            output.clone(),
        )
        .await
        .expect_err("existing output directory is rejected");
        assert!(format!("{error:#}").contains("create new probe output directory"));
        assert_eq!(std::fs::read(output.join("sentinel")).unwrap(), b"keep");
    }

    #[derive(Debug, Clone, Copy)]
    enum SyntheticFailure {
        DuplicateHandle,
        TrailingCatalogBytes,
        DuplicateObjectIdentity,
        NonCanonicalDescriptor,
        UnexpectedResponse,
        ErrorResponseWithData,
        HashMismatch,
        RetryOnce,
    }

    struct SyntheticLoopback {
        handle: u32,
        failure: Option<SyntheticFailure>,
        next_transaction: AtomicU32,
        ordinary_frames: StdMutex<VecDeque<Vec<u8>>>,
        streaming_bytes: StdMutex<VecDeque<u8>>,
        pending_write: StdMutex<Option<(u16, u32)>>,
        operations: StdMutex<Vec<u16>>,
        write_payloads: StdMutex<Vec<Vec<u8>>>,
        toggle: StdMutex<Vec<u8>>,
        stream_index: AtomicUsize,
        wrong_set_verification: AtomicBool,
        fail_restore: AtomicBool,
        invalidated: AtomicBool,
    }

    impl SyntheticLoopback {
        fn new(handle: u32) -> Self {
            Self::with_optional_failure(handle, None)
        }

        fn with_failure(handle: u32, failure: SyntheticFailure) -> Self {
            Self::with_optional_failure(handle, Some(failure))
        }

        fn with_optional_failure(handle: u32, failure: Option<SyntheticFailure>) -> Self {
            Self {
                handle,
                failure,
                next_transaction: AtomicU32::new(2),
                ordinary_frames: StdMutex::new(VecDeque::new()),
                streaming_bytes: StdMutex::new(VecDeque::new()),
                pending_write: StdMutex::new(None),
                operations: StdMutex::new(Vec::new()),
                write_payloads: StdMutex::new(Vec::new()),
                toggle: StdMutex::new(vec![0, 0]),
                stream_index: AtomicUsize::new(0),
                wrong_set_verification: AtomicBool::new(false),
                fail_restore: AtomicBool::new(false),
                invalidated: AtomicBool::new(false),
            }
        }

        fn operations(&self) -> Vec<u16> {
            self.operations.lock().unwrap().clone()
        }

        fn write_payloads(&self) -> Vec<Vec<u8>> {
            self.write_payloads.lock().unwrap().clone()
        }

        fn next_id(&self) -> u32 {
            self.next_transaction.fetch_add(1, Ordering::AcqRel)
        }

        fn send(&self, frame: Vec<u8>) -> Result<(), PtpTransportError> {
            let packet = protocol_primitives::fuji_framing::decode(&frame)
                .map_err(|error| synthetic_transport_error(error.to_string()))?;
            match packet {
                PtpIpPacket::OperationRequest(request) => {
                    self.operations.lock().unwrap().push(request.code);
                    if request.code == 0x9002 {
                        *self.pending_write.lock().unwrap() =
                            Some((request.code, request.transaction_id));
                        return Ok(());
                    }
                    if request.code == 0x1009 {
                        self.enqueue_stream(request.code, request.transaction_id);
                    } else {
                        self.enqueue_ordinary(
                            request.code,
                            request.transaction_id,
                            &request.params,
                        )?;
                    }
                    Ok(())
                }
                PtpIpPacket::Data(data) => {
                    let Some((operation, transaction_id)) =
                        self.pending_write.lock().unwrap().take()
                    else {
                        return Err(synthetic_transport_error("unexpected outbound data"));
                    };
                    if transaction_id != data.transaction_id {
                        return Err(synthetic_transport_error("outbound transaction mismatch"));
                    }
                    self.write_payloads
                        .lock()
                        .unwrap()
                        .push(data.payload.clone());
                    *self.toggle.lock().unwrap() = data.payload.clone();
                    let response =
                        if data.payload == [0, 0] && self.fail_restore.load(Ordering::Acquire) {
                            resp::GENERAL_ERROR
                        } else {
                            resp::OK
                        };
                    self.enqueue_response(operation, transaction_id, response);
                    Ok(())
                }
                other => Err(synthetic_transport_error(format!(
                    "unexpected initiator packet {other:?}"
                ))),
            }
        }

        fn enqueue_ordinary(
            &self,
            operation: u16,
            transaction_id: u32,
            params: &[u32],
        ) -> Result<(), PtpTransportError> {
            let payload = match operation {
                0x1001 => {
                    let mut writer = Writer::new();
                    DeviceInfo {
                        standard_version: 100,
                        operations_supported: vec![0x1001, 0x1007, 0x1008, 0x1009],
                        device_properties_supported: vec![0x5001, 0x5002],
                        manufacturer: "Synthetic".into(),
                        model: "Loopback".into(),
                        ..Default::default()
                    }
                    .encode(&mut writer)
                    .map_err(|error| synthetic_transport_error(error.to_string()))?;
                    writer.into_vec()
                }
                0x1014 => {
                    let code = u16::try_from(params[0])
                        .map_err(|_| synthetic_transport_error("property code overflow"))?;
                    let mut writer = Writer::new();
                    DevicePropDesc {
                        code,
                        datatype: datatype_code::UINT16,
                        get_set: 0,
                        factory_default: PropValue::U16(code),
                        current: PropValue::U16(code + 1),
                        form: ptp_core::PropForm::None,
                    }
                    .encode(&mut writer)
                    .map_err(|error| synthetic_transport_error(error.to_string()))?;
                    let mut payload = writer.into_vec();
                    if code == 0x5001
                        && matches!(self.failure, Some(SyntheticFailure::NonCanonicalDescriptor))
                    {
                        payload.push(0xff);
                    }
                    payload
                }
                0x1015 => (u16::try_from(params[0]).unwrap() + 1)
                    .to_le_bytes()
                    .to_vec(),
                0x1007 => {
                    let handles = match self.failure {
                        Some(SyntheticFailure::DuplicateHandle) => vec![self.handle, self.handle],
                        Some(SyntheticFailure::DuplicateObjectIdentity) => {
                            vec![self.handle, self.handle + 1]
                        }
                        _ => vec![self.handle],
                    };
                    let mut writer = Writer::new();
                    writer.ptp_array(&handles, |writer, handle| writer.u32(*handle));
                    let mut payload = writer.into_vec();
                    if matches!(self.failure, Some(SyntheticFailure::TrailingCatalogBytes)) {
                        payload.push(0xff);
                    }
                    payload
                }
                0x1008 => {
                    let mut writer = Writer::new();
                    ObjectInfo {
                        object_compressed_size: 4,
                        filename: "SYNTHETIC.BIN".into(),
                        ..Default::default()
                    }
                    .encode(&mut writer)
                    .map_err(|error| synthetic_transport_error(error.to_string()))?;
                    writer.into_vec()
                }
                0x9001 => {
                    let toggle = self.toggle.lock().unwrap().clone();
                    if toggle == [1, 0] && self.wrong_set_verification.load(Ordering::Acquire) {
                        vec![2, 0]
                    } else {
                        toggle
                    }
                }
                _ => return Err(synthetic_transport_error("unknown synthetic operation")),
            };
            let response_error = operation == 0x1015
                && matches!(
                    self.failure,
                    Some(
                        SyntheticFailure::UnexpectedResponse
                            | SyntheticFailure::ErrorResponseWithData
                    )
                );
            let response = if response_error {
                resp::GENERAL_ERROR
            } else {
                resp::OK
            };
            let data_payload = if response_error
                && matches!(self.failure, Some(SyntheticFailure::UnexpectedResponse))
            {
                Vec::new()
            } else {
                payload
            };
            let data = build_data(
                PtpFraming::Compressed,
                operation,
                transaction_id,
                data_payload,
            )
            .map_err(|error| synthetic_transport_error(error.to_string()))?;
            self.ordinary_frames.lock().unwrap().push_back(data);
            self.enqueue_response(operation, transaction_id, response);
            Ok(())
        }

        fn enqueue_response(&self, operation: u16, transaction_id: u32, response: u16) {
            let frame = protocol_primitives::fuji_framing::encode(&PtpIpPacket::OperationResponse(
                OperationResponse {
                    code: response,
                    transaction_id,
                    params: Vec::new(),
                },
            ))
            .unwrap();
            let _ = operation;
            self.ordinary_frames.lock().unwrap().push_back(frame);
        }

        fn enqueue_stream(&self, operation: u16, transaction_id: u32) {
            let index = self.stream_index.fetch_add(1, Ordering::AcqRel);
            if index == 0 && matches!(self.failure, Some(SyntheticFailure::RetryOnce)) {
                let response = protocol_primitives::fuji_framing::encode(
                    &PtpIpPacket::OperationResponse(OperationResponse {
                        code: resp::DEVICE_BUSY,
                        transaction_id,
                        params: Vec::new(),
                    }),
                )
                .unwrap();
                self.streaming_bytes.lock().unwrap().extend(response);
                return;
            }
            let payload =
                if index == 1 && matches!(self.failure, Some(SyntheticFailure::HashMismatch)) {
                    b"DAXA".as_slice()
                } else {
                    b"DATA".as_slice()
                };
            let data = build_data(
                PtpFraming::Compressed,
                operation,
                transaction_id,
                payload.to_vec(),
            )
            .unwrap();
            let response = protocol_primitives::fuji_framing::encode(
                &PtpIpPacket::OperationResponse(OperationResponse {
                    code: resp::OK,
                    transaction_id,
                    params: Vec::new(),
                }),
            )
            .unwrap();
            self.streaming_bytes.lock().unwrap().extend(data);
            self.streaming_bytes.lock().unwrap().extend(response);
        }
    }

    #[async_trait]
    impl PtpExecutorTransport for SyntheticLoopback {
        async fn reserve_transaction_id(&self) -> Result<u32, PtpTransportError> {
            Ok(self.next_id())
        }

        async fn send_command_frame(&self, frame: Vec<u8>) -> Result<(), PtpTransportError> {
            self.send(frame)
        }

        async fn next_command_frame(&self) -> Result<Vec<u8>, PtpTransportError> {
            self.ordinary_frames
                .lock()
                .unwrap()
                .pop_front()
                .ok_or_else(|| synthetic_transport_error("ordinary response queue is empty"))
        }

        async fn next_event_frame(&self, _event_code: u16) -> Result<Vec<u8>, PtpTransportError> {
            Err(PtpTransportError::NotConnected)
        }

        async fn open_channel(&self, _role: SocketRole) -> Result<(), PtpTransportError> {
            Err(PtpTransportError::NotConnected)
        }

        async fn close_command_channel(
            &self,
            _transport_close_frame: Option<Vec<u8>>,
        ) -> Result<(), PtpTransportError> {
            Ok(())
        }

        async fn reopen_command_session(&self) -> Result<PtpSessionOpenResult, PtpTransportError> {
            Err(PtpTransportError::NotConnected)
        }

        async fn sleep(&self, _ms: u32) -> Result<(), PtpTransportError> {
            Ok(())
        }
    }

    #[async_trait]
    impl PtpStreamingTransport for SyntheticLoopback {
        async fn reserve_transaction_id(&self) -> Result<u32, PtpTransportError> {
            Ok(self.next_id())
        }

        async fn send_command_frame(&self, frame: Vec<u8>) -> Result<(), PtpTransportError> {
            self.send(frame)
        }

        async fn receive_command_bytes(
            &self,
            max_bytes: u32,
        ) -> Result<Vec<u8>, PtpTransportError> {
            let mut queue = self.streaming_bytes.lock().unwrap();
            if queue.is_empty() {
                return Err(synthetic_transport_error(
                    "streaming response queue is empty",
                ));
            }
            let take = usize::try_from(max_bytes).unwrap().min(queue.len());
            Ok(queue.drain(..take).collect())
        }

        async fn sleep(&self, ms: u32) -> Result<(), PtpTransportError> {
            tokio::time::sleep(Duration::from_millis(u64::from(ms))).await;
            Ok(())
        }

        fn invalidate_command_session(&self, _reason: String) {
            self.invalidated.store(true, Ordering::Release);
        }
    }

    fn synthetic_transport_error(detail: impl Into<String>) -> PtpTransportError {
        PtpTransportError::Failed {
            detail: detail.into(),
        }
    }

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new() -> Self {
            let nonce = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path = std::env::temp_dir().join(format!(
                "ptpsim-pcss-probe-test-{}-{nonce}-{}",
                std::process::id(),
                NEXT_TEST_DIRECTORY.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir(&path).unwrap();
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
}
