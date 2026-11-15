//! Scoped observations of static candidates. Results deliberately remain unverified.
use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use camera_config::{
    payload_metadata, PayloadMetadata, StaticCandidateDisposition as Disposition,
    StaticCandidateDocument, StaticCandidateKind as CandidateKind,
    StaticCandidateRequest as CandidateRequest, StaticCandidateSource as CandidateSource,
    StaticDataPhase as DataPhase,
};
use camera_protocol_ffi::{
    build_command, build_data, parse_data_phase, parse_response, DataPhaseKind,
    PtpExecutorTransport, PtpFraming,
};
use ptp_core::{DeviceInfo, DevicePropDesc, PropValue, Reader};
use serde::{Deserialize, Serialize};

pub use camera_config::STATIC_CANDIDATE_SCHEMA as CANDIDATE_SCHEMA;
pub const CAPABILITY_PLAN_SCHEMA: &str = "camera-capability-plan/v1";
pub const CAPABILITY_REPORT_SCHEMA: &str = "camera-capability-report/v1";
const MAX_PAYLOAD: usize = 16 * 1024 * 1024;

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(transparent)]
pub struct CandidateLedger(StaticCandidateDocument);

impl std::ops::Deref for CandidateLedger {
    type Target = StaticCandidateDocument;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}
impl std::ops::DerefMut for CandidateLedger {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CapabilityPlan {
    pub schema: String,
    pub connection: String,
    pub probes: Vec<CapabilityProbe>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CapabilityProbe {
    pub candidate_id: String,
    pub kind: ProbeKind,
    pub mode: String,
    pub shape_evidence: Vec<String>,
    pub request: PlannedRequest,
    pub baseline: Option<PlannedRequest>,
    pub verify: Option<PlannedRequest>,
    pub restore: Option<PlannedRequest>,
    pub verify_restored: Option<PlannedRequest>,
    pub state_change: Option<String>,
    pub cleanup_limitation: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum ProbeKind {
    Read,
    Action,
    ReversibleWrite,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PlannedRequest {
    pub request: CandidateRequest,
    #[serde(default = "ok_response")]
    pub accepted_responses: Vec<u16>,
    pub expect_payload_hex: Option<String>,
    #[serde(default = "default_timeout")]
    pub timeout_ms: u64,
}

fn ok_response() -> Vec<u16> {
    vec![0x2001]
}
fn default_timeout() -> u64 {
    10_000
}

pub fn parse_code(code: &str) -> Result<u16> {
    if code.len() != 6
        || !code.starts_with("0x")
        || !code[2..].bytes().all(|b| b.is_ascii_hexdigit())
    {
        bail!("code must contain all four hex digits with a 0x prefix: {code:?}")
    }
    u16::from_str_radix(&code[2..], 16).context("parse code")
}

fn decode_hex(value: &str) -> Result<Vec<u8>> {
    if !value.len().is_multiple_of(2) || !value.bytes().all(|b| b.is_ascii_hexdigit()) {
        bail!("payloadHex must contain complete ASCII hex bytes")
    }
    if value.len() > MAX_PAYLOAD * 2 {
        bail!("payloadHex exceeds 16 MiB")
    }
    value
        .as_bytes()
        .chunks_exact(2)
        .map(|chunk| Ok(u8::from_str_radix(std::str::from_utf8(chunk)?, 16)?))
        .collect()
}

fn datatype(hint: &str) -> Option<u16> {
    Some(match hint {
        "i8" => 1,
        "u8" => 2,
        "i16" => 3,
        "u16" => 4,
        "i32" => 5,
        "u32" => 6,
        "i64" => 7,
        "u64" => 8,
        "i128" => 9,
        "u128" => 10,
        "str" | "string" => 0xffff,
        "ai8" => 0x4001,
        "au8" => 0x4002,
        "ai16" => 0x4003,
        "au16" => 0x4004,
        "ai32" => 0x4005,
        "au32" => 0x4006,
        "ai64" => 0x4007,
        "au64" => 0x4008,
        "ai128" => 0x4009,
        "au128" => 0x400a,
        _ => return None,
    })
}

impl CandidateLedger {
    pub fn from_json(input: &str) -> Result<Self> {
        let ledger: Self = serde_json::from_str(input).context("parse candidate ledger")?;
        ledger.validate()?;
        Ok(ledger)
    }

    pub fn validate(&self) -> Result<()> {
        self.0.validate()?;
        for row in &self.candidates {
            if row
                .expected_type
                .as_ref()
                .is_some_and(|s| datatype(s).is_none())
            {
                bail!("unknown expectedType on {}", row.id)
            }
        }
        Ok(())
    }
}

fn validate_request(request: &CandidateRequest, baseline_override: bool) -> Result<()> {
    parse_code(&request.opcode)?;
    if request.params.len() > 5 {
        bail!("request has more than five parameters")
    }
    match (
        request.data_phase,
        request.payload_hex.as_ref(),
        baseline_override,
    ) {
        (DataPhase::Out, None, false) => bail!("data-out request needs payloadHex"),
        (DataPhase::Out, _, _) => {}
        (_, Some(_), _) => bail!("payloadHex requires dataPhase out"),
        _ => {}
    }
    if let Some(payload) = &request.payload_hex {
        decode_hex(payload)?;
    }
    Ok(())
}

impl CapabilityPlan {
    pub fn from_yaml(input: &str, ledger: &CandidateLedger) -> Result<Self> {
        let plan: Self = serde_yaml::from_str(input).context("parse capability plan")?;
        plan.validate(ledger)?;
        Ok(plan)
    }

    pub fn validate_framing(&self, framing: PtpFraming) -> Result<()> {
        if matches!(framing, PtpFraming::Standard)
            && self
                .probes
                .iter()
                .any(|p| p.request.request.data_phase == DataPhase::Out)
        {
            bail!("standard-framing data-out plans are unsupported; use retained session actions")
        }
        Ok(())
    }

    pub fn validate(&self, ledger: &CandidateLedger) -> Result<()> {
        if self.connection.trim().is_empty() {
            bail!("capability plan needs a connection precondition")
        }
        if self.schema != CAPABILITY_PLAN_SCHEMA {
            bail!("expected {CAPABILITY_PLAN_SCHEMA}")
        }
        let mut ids = BTreeSet::new();
        for probe in &self.probes {
            if !ids.insert(&probe.candidate_id) {
                bail!("duplicate candidate plan")
            }
            let row = ledger
                .candidates
                .iter()
                .find(|row| row.id == probe.candidate_id)
                .context("plan references unknown candidate")?;
            if probe.mode.trim().is_empty()
                || probe.shape_evidence.is_empty()
                || probe
                    .shape_evidence
                    .iter()
                    .any(|item| item.trim().is_empty())
            {
                bail!("probe needs a mode precondition and shapeEvidence")
            }
            if row.kind == CandidateKind::Operation
                && row.code.as_ref() != Some(&probe.request.request.opcode)
            {
                bail!("operation plan opcode differs from candidate code")
            }
            if let Some(request) = &row.request {
                if request != &probe.request.request {
                    bail!("plan request differs from ledger request for {}", row.id)
                }
            }
            probe.request.validate(false)?;
            if probe.kind == ProbeKind::Read && probe.request.request.data_phase != DataPhase::In {
                bail!("read probe requires an inbound data phase")
            }
            let auxiliaries = [
                &probe.baseline,
                &probe.verify,
                &probe.restore,
                &probe.verify_restored,
            ];
            if probe.kind == ProbeKind::ReversibleWrite {
                let [Some(baseline), Some(verify), Some(restore), Some(restored)] = auxiliaries
                else {
                    bail!("reversibleWrite requires baseline, verify, restore, verifyRestored")
                };
                baseline.validate(false)?;
                verify.validate(false)?;
                restore.validate(true)?;
                restored.validate(false)?;
                if [baseline, &probe.request, verify, restore, restored]
                    .iter()
                    .any(|p| p.accepted_responses != [0x2001])
                {
                    bail!("reversibleWrite requests must accept only OK")
                }
                if probe.request.request.data_phase != DataPhase::Out
                    || restore.request.data_phase != DataPhase::Out
                {
                    bail!("reversibleWrite set and restore must use data-out")
                }
                if restore.request.payload_hex.is_some() {
                    bail!("restore payload must come from baseline")
                }
                let mut expected_restore = probe.request.request.clone();
                expected_restore.payload_hex = None;
                if restore.request != expected_restore
                    || baseline.request != verify.request
                    || baseline.request != restored.request
                    || baseline.request.params != probe.request.request.params
                {
                    bail!("reversibleWrite must read and restore the same target")
                }
                if baseline.request.data_phase != DataPhase::In
                    || verify.expect_payload_hex.is_none()
                {
                    bail!("reversibleWrite requires inbound baseline and expected verify bytes")
                }
            } else if auxiliaries.iter().any(|p| p.is_some()) {
                bail!("baseline/verify/restore fields require reversibleWrite")
            }
            if probe.kind == ProbeKind::Action
                && (probe
                    .state_change
                    .as_ref()
                    .is_none_or(|v| v.trim().is_empty())
                    || probe
                        .cleanup_limitation
                        .as_ref()
                        .is_none_or(|v| v.trim().is_empty()))
            {
                bail!("action needs stateChange and cleanupLimitation")
            }
        }
        Ok(())
    }
}

impl PlannedRequest {
    fn validate(&self, baseline_override: bool) -> Result<()> {
        validate_request(&self.request, baseline_override)?;
        if matches!(parse_code(&self.request.opcode)?, 0x900c | 0x900d) {
            bail!("firmware update requests are excluded-dangerous")
        }
        if self.timeout_ms == 0 || self.timeout_ms > 120_000 {
            bail!("timeoutMs must be in 1..120000")
        }
        if self.accepted_responses.is_empty()
            || self
                .accepted_responses
                .iter()
                .any(|v| !(0x2000..=0x2fff).contains(v))
        {
            bail!("acceptedResponses must contain PTP response codes")
        }
        if let Some(expected) = &self.expect_payload_hex {
            decode_hex(expected)?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone)]
pub struct CapabilityOptions {
    pub selected_model: String,
    pub manifest_firmware: String,
    pub connection: String,
    pub scope_mode: Option<String>,
    pub allow_unknown_model: bool,
    pub allow_control: bool,
    pub output_dir: PathBuf,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CapabilityReport {
    pub schema: String,
    pub source: CandidateSource,
    pub source_model: Option<String>,
    pub selected_model: String,
    pub manifest_firmware: String,
    pub observed_manufacturer: Option<String>,
    pub observed_model: Option<String>,
    pub device_version: Option<String>,
    pub connection: String,
    pub operator_mode: Option<String>,
    pub allow_unknown_model: bool,
    pub device_info: Option<RequestResult>,
    pub error: Option<String>,
    pub rows: Vec<CandidateResult>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CandidateResult {
    pub id: String,
    pub code: Option<String>,
    pub status: String,
    pub disposition: Disposition,
    pub reason: String,
    pub evidence: Vec<String>,
    pub outcome: String,
    pub detail: String,
    pub advertised: Option<bool>,
    pub expected_type: Option<String>,
    pub descriptor_type: Option<u16>,
    pub requests: Vec<RequestResult>,
    pub restored: Option<bool>,
    pub restore_error: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RequestResult {
    pub label: String,
    pub opcode: String,
    pub parameters: Vec<u32>,
    pub transaction_id: Option<u32>,
    pub response: Option<u16>,
    pub response_parameters: Vec<u32>,
    pub payload: Option<PayloadMetadata>,
    pub payload_file: Option<String>,
    pub error: Option<String>,
}

struct Exchange {
    record: RequestResult,
    payload: Vec<u8>,
}

impl Exchange {
    fn ensure(&self, request: &PlannedRequest) -> Result<()> {
        if let Some(error) = &self.record.error {
            bail!("{error}")
        }
        if !self
            .record
            .response
            .is_some_and(|code| request.accepted_responses.contains(&code))
        {
            bail!("unaccepted response {:?}", self.record.response)
        }
        if let Some(expected) = &request.expect_payload_hex {
            if self.payload != decode_hex(expected)? {
                bail!("payload expectation mismatch")
            }
        }
        Ok(())
    }
}

/// Run on an already opened command session. No request here promotes catalog assertions.
pub async fn run_capability_verification<T: PtpExecutorTransport + 'static>(
    ledger: &CandidateLedger,
    plan: Option<&CapabilityPlan>,
    framing: PtpFraming,
    transport: Arc<T>,
    options: CapabilityOptions,
) -> Result<CapabilityReport> {
    ledger.validate()?;
    if let Some(plan) = plan {
        plan.validate(ledger)?;
        plan.validate_framing(framing)?;
    }
    std::fs::create_dir(&options.output_dir).context("create new capability output directory")?;
    if let Some(plan) = plan {
        std::fs::write(
            options.output_dir.join("plan.yaml"),
            serde_yaml::to_string(plan)?,
        )?;
    }
    std::fs::write(
        options.output_dir.join("candidates.json"),
        serde_json::to_vec_pretty(ledger)?,
    )?;
    let mut report = CapabilityReport {
        schema: CAPABILITY_REPORT_SCHEMA.into(),
        source: ledger.source.clone(),
        source_model: ledger.model.clone(),
        selected_model: options.selected_model.clone(),
        manifest_firmware: options.manifest_firmware.clone(),
        observed_manufacturer: None,
        observed_model: None,
        device_version: None,
        connection: options.connection.clone(),
        operator_mode: options.scope_mode.clone(),
        allow_unknown_model: options.allow_unknown_model,
        device_info: None,
        error: None,
        rows: ledger
            .candidates
            .iter()
            .map(|row| CandidateResult {
                id: row.id.clone(),
                code: row.code.clone(),
                status: "unverified".into(),
                disposition: row.disposition,
                reason: row.reason.clone(),
                evidence: row.evidence.clone(),
                outcome: "skipped".into(),
                detail: "not reached".into(),
                advertised: None,
                expected_type: row.expected_type.clone(),
                descriptor_type: None,
                requests: vec![],
                restored: None,
                restore_error: None,
            })
            .collect(),
    };
    let device = exchange(
        standard_request(0x1001, vec![]),
        "device-info",
        None,
        framing,
        transport.clone(),
        &options,
    )
    .await;
    let info = device
        .ensure(&standard_request(0x1001, vec![]))
        .and_then(|_| DeviceInfo::decode(&device.payload).context("decode DeviceInfo"));
    report.device_info = Some(device.record);
    let info = match info {
        Ok(info) => info,
        Err(error) => {
            report.error = Some(format!("DeviceInfo failed: {error:#}"));
            for row in &mut report.rows {
                row.detail = "DeviceInfo unavailable; scope cannot be established".into();
            }
            write_report(&options, &report)?;
            return Ok(report);
        }
    };
    report.observed_manufacturer = Some(info.manufacturer.clone());
    report.observed_model = Some(info.model.clone());
    report.device_version = Some(info.device_version.clone());
    let scope_error = if info.model != options.selected_model {
        Some("observed DeviceInfo model differs from selected manifest")
    } else if !info.manufacturer.eq_ignore_ascii_case(&ledger.manufacturer) {
        Some("observed manufacturer differs from ledger")
    } else {
        match &ledger.model {
            Some(model) if model != &info.model => {
                Some("candidate source model differs from observed model")
            }
            None if !options.allow_unknown_model => Some(
                "candidate source model unknown; --allow-unknown-model required for exploration",
            ),
            _ => None,
        }
    };
    let mut aborted = false;
    for (index, (candidate, row)) in ledger.candidates.iter().zip(&mut report.rows).enumerate() {
        let code = candidate.code.as_deref().map(parse_code).transpose()?;
        row.advertised = code.map(|code| {
            if candidate.kind == CandidateKind::Property {
                info.device_properties_supported.contains(&code)
            } else {
                info.operations_supported.contains(&code)
            }
        });
        if code.is_some_and(|c| matches!(c, 0x900c | 0x900d)) {
            row.outcome = "excluded-dangerous".into();
            row.detail = "firmware update requests are excluded".into();
            continue;
        }
        if let Some(reason) = scope_error {
            row.detail = reason.into();
            continue;
        }
        if aborted {
            row.detail = "earlier transport/protocol failure; session no longer trusted".into();
            continue;
        }
        let probe = plan.and_then(|p| p.probes.iter().find(|p| p.candidate_id == candidate.id));
        let prefix = format!("row-{index:04}");
        if let Some(probe) = probe {
            if plan.is_some_and(|plan| plan.connection != options.connection) {
                row.detail = "plan connection precondition differs from selected connection".into();
                continue;
            }
            if options.scope_mode.as_deref() != Some(probe.mode.as_str()) {
                row.detail = format!(
                    "mode precondition {} requires matching --scope-mode operator claim",
                    probe.mode
                );
                continue;
            }
            if probe.kind != ProbeKind::Read && !options.allow_control {
                row.detail = "explicit --allow-control required for write/action probe".into();
                continue;
            }
            aborted =
                run_explicit_probe(probe, row, &prefix, framing, transport.clone(), &options).await;
        } else if candidate.kind == CandidateKind::Property
            && candidate.disposition != Disposition::CoverageOnly
        {
            aborted = run_property(
                code.expect("validated property code"),
                row,
                &prefix,
                framing,
                transport.clone(),
                &options,
            )
            .await;
        } else {
            row.detail = if candidate.request.is_some() {
                "known request shape retained; select it in an explicit capability plan".into()
            } else {
                format!("coverage only: {}", candidate.reason)
            };
        }
        if row.outcome == "expectationFailed" {
            report.error = Some(format!(
                "explicit request expectation failed at {}",
                candidate.id
            ));
        }
        if aborted {
            report.error = Some(format!(
                "request failure at {}; remaining requests skipped",
                candidate.id
            ));
        }
    }
    write_report(&options, &report)?;
    Ok(report)
}

fn write_report(options: &CapabilityOptions, report: &CapabilityReport) -> Result<()> {
    std::fs::write(
        options.output_dir.join("capability-report.json"),
        serde_json::to_vec_pretty(report)?,
    )?;
    Ok(())
}

fn standard_request(opcode: u16, params: Vec<u32>) -> PlannedRequest {
    PlannedRequest {
        request: CandidateRequest {
            opcode: format!("0x{opcode:04x}"),
            params,
            data_phase: DataPhase::In,
            payload_hex: None,
        },
        accepted_responses: ok_response(),
        expect_payload_hex: None,
        timeout_ms: default_timeout(),
    }
}

async fn run_property<T: PtpExecutorTransport + 'static>(
    code: u16,
    row: &mut CandidateResult,
    prefix: &str,
    framing: PtpFraming,
    transport: Arc<T>,
    options: &CapabilityOptions,
) -> bool {
    let desc = exchange(
        standard_request(0x1014, vec![code.into()]),
        &format!("{prefix}-descriptor"),
        None,
        framing,
        transport.clone(),
        options,
    )
    .await;
    if desc.record.error.is_some() {
        row.outcome = "transportError".into();
        row.detail = desc.record.error.clone().unwrap();
        row.requests.push(desc.record);
        return true;
    }
    if desc.payload.len() >= 4 {
        row.descriptor_type = Some(u16::from_le_bytes([desc.payload[2], desc.payload[3]]));
    }
    let decoded = if desc.record.response == Some(0x2001) {
        DevicePropDesc::decode(&desc.payload)
            .context("decode property descriptor")
            .and_then(|value| {
                if value.code != code {
                    bail!("descriptor code does not match requested property")
                }
                Ok(value)
            })
    } else {
        Err(anyhow::anyhow!(
            "descriptor rejected with {:?}",
            desc.record.response
        ))
    };
    row.requests.push(desc.record);
    let value = exchange(
        standard_request(0x1015, vec![code.into()]),
        &format!("{prefix}-value"),
        None,
        framing,
        transport,
        options,
    )
    .await;
    let aborted = value.record.error.is_some();
    row.outcome = "observed".into();
    row.detail = "descriptor and current value decoded; semantic effect remains unverified".into();
    if aborted {
        row.outcome = "transportError".into();
        row.detail = value.record.error.clone().unwrap();
    } else if row.requests[0].response != Some(0x2001) || value.record.response != Some(0x2001) {
        row.outcome = "rejected".into();
        row.detail = "descriptor or current-value request rejected in this scope".into();
    } else {
        match decoded {
            Err(error) => {
                row.outcome = "decodeError".into();
                row.detail = format!("{error:#}");
            }
            Ok(desc) => {
                row.descriptor_type = Some(desc.datatype);
                let mut reader = Reader::new(&value.payload);
                let decoded_value = PropValue::decode(&mut reader, desc.datatype);
                if decoded_value.is_err() || reader.remaining() != 0 {
                    row.outcome = "decodeError".into();
                    row.detail = "current value does not match wire descriptor datatype".into();
                } else if row
                    .expected_type
                    .as_deref()
                    .and_then(datatype)
                    .is_some_and(|hint| hint != desc.datatype)
                {
                    row.outcome = "typeConflict".into();
                    row.detail = "client datatype hint conflicts with camera descriptor; camera type preserved".into();
                }
            }
        }
    }
    row.requests.push(value.record);
    aborted
}

async fn run_explicit_probe<T: PtpExecutorTransport + 'static>(
    probe: &CapabilityProbe,
    row: &mut CandidateResult,
    prefix: &str,
    framing: PtpFraming,
    transport: Arc<T>,
    options: &CapabilityOptions,
) -> bool {
    if probe.kind != ProbeKind::ReversibleWrite {
        let result = exchange(
            probe.request.clone(),
            prefix,
            None,
            framing,
            transport,
            options,
        )
        .await;
        let aborted = result.record.error.is_some();
        match result.ensure(&probe.request) {
            Ok(()) => {
                row.outcome = "observed".into();
                row.detail = "request expectations met; semantic effect remains unverified".into();
            }
            Err(error) => {
                row.outcome = if aborted {
                    "transportError"
                } else {
                    "expectationFailed"
                }
                .into();
                row.detail = format!("{error:#}");
            }
        }
        row.requests.push(result.record);
        return aborted;
    }
    let baseline_plan = probe.baseline.as_ref().expect("validated baseline");
    let baseline = exchange(
        baseline_plan.clone(),
        &format!("{prefix}-baseline"),
        None,
        framing,
        transport.clone(),
        options,
    )
    .await;
    let mut aborted = baseline.record.error.is_some();
    let baseline_ok = baseline.ensure(baseline_plan);
    row.requests.push(baseline.record);
    if let Err(error) = baseline_ok {
        row.outcome = "expectationFailed".into();
        row.detail = format!("baseline failed: {error:#}");
        return aborted;
    }
    let set = exchange(
        probe.request.clone(),
        &format!("{prefix}-set"),
        None,
        framing,
        transport.clone(),
        options,
    )
    .await;
    aborted |= set.record.error.is_some();
    let mut primary = set.ensure(&probe.request);
    row.requests.push(set.record);
    if primary.is_ok() {
        let verify = probe.verify.as_ref().expect("validated verify");
        let result = exchange(
            verify.clone(),
            &format!("{prefix}-verify"),
            None,
            framing,
            transport.clone(),
            options,
        )
        .await;
        aborted |= result.record.error.is_some();
        primary = result.ensure(verify);
        row.requests.push(result.record);
    }
    // Restore even when set/verify failed. A timed-out session may still reject cleanup.
    let restore = probe.restore.as_ref().expect("validated restore");
    let result = exchange(
        restore.clone(),
        &format!("{prefix}-restore"),
        Some(baseline.payload.clone()),
        framing,
        transport.clone(),
        options,
    )
    .await;
    aborted |= result.record.error.is_some();
    let mut restored = result.ensure(restore);
    row.requests.push(result.record);
    if restored.is_ok() {
        let verify = probe
            .verify_restored
            .as_ref()
            .expect("validated restoration read");
        let result = exchange(
            verify.clone(),
            &format!("{prefix}-verify-restored"),
            None,
            framing,
            transport,
            options,
        )
        .await;
        aborted |= result.record.error.is_some();
        restored = result.ensure(verify).and_then(|_| {
            if result.payload != baseline.payload {
                bail!("restored value differs from baseline")
            }
            Ok(())
        });
        row.requests.push(result.record);
    }
    row.restored = Some(restored.is_ok());
    row.restore_error = restored.err().map(|e| format!("{e:#}"));
    row.outcome = if primary.is_ok() && row.restored == Some(true) {
        "observed"
    } else {
        "expectationFailed"
    }
    .into();
    row.detail = primary
        .err()
        .map(|e| format!("write/verification failed: {e:#}"))
        .unwrap_or_else(|| {
            "write response and readback observed; semantic effect remains unverified".into()
        });
    aborted || row.restored != Some(true)
}

async fn exchange<T: PtpExecutorTransport + 'static>(
    request: PlannedRequest,
    label: &str,
    override_payload: Option<Vec<u8>>,
    framing: PtpFraming,
    transport: Arc<T>,
    options: &CapabilityOptions,
) -> Exchange {
    let mut result = Exchange {
        record: RequestResult {
            label: label.into(),
            opcode: request.request.opcode.clone(),
            parameters: request.request.params.clone(),
            transaction_id: None,
            response: None,
            response_parameters: vec![],
            payload: None,
            payload_file: None,
            error: None,
        },
        payload: vec![],
    };
    let mut saw_data = false;
    let operation = async {
        let code = parse_code(&request.request.opcode)?;
        let txn = transport.reserve_transaction_id().await?;
        result.record.transaction_id = Some(txn);
        transport
            .send_command_frame(build_command(
                framing,
                code,
                txn,
                request.request.params.clone(),
            )?)
            .await?;
        if request.request.data_phase == DataPhase::Out {
            let data = match override_payload {
                Some(data) => data,
                None => decode_hex(
                    request
                        .request
                        .payload_hex
                        .as_deref()
                        .context("outbound payload missing")?,
                )?,
            };

            transport
                .send_command_frame(build_data(framing, code, txn, data)?)
                .await?;
        }
        let mut total = None;
        let mut started = false;
        let mut ended = false;
        for _ in 0..1024 {
            let frame = transport.next_command_frame().await?;
            if let Ok(response) = parse_response(framing, frame.clone()) {
                if response.txn != txn {
                    bail!("response transaction mismatch")
                }
                result.record.response = Some(response.response_code);
                result.record.response_parameters = response.params;
                if started && !ended {
                    bail!("incomplete data phase")
                }
                if response.response_code == 0x2001
                    && request.request.data_phase == DataPhase::In
                    && !started
                {
                    bail!("OK response without data phase")
                }
                return Ok::<_, anyhow::Error>(());
            }
            if request.request.data_phase != DataPhase::In {
                bail!("unexpected inbound data phase")
            }
            let data = parse_data_phase(framing, frame)?;
            saw_data = true;
            if data.txn != txn {
                bail!("data transaction mismatch")
            }
            match data.kind {
                DataPhaseKind::Start if !started && matches!(framing, PtpFraming::Standard) => {
                    started = true;
                    total = data.total_length;
                    if total.is_none_or(|v| v > MAX_PAYLOAD as u64) {
                        bail!("declared payload exceeds 16 MiB")
                    }
                }
                DataPhaseKind::Data if !ended => {
                    if matches!(framing, PtpFraming::Standard) && !started {
                        bail!("data before StartData")
                    }
                    if !matches!(framing, PtpFraming::Standard) && started {
                        bail!("duplicate data phase")
                    }
                    started = true;
                    ended = !matches!(framing, PtpFraming::Standard);
                }
                DataPhaseKind::End
                    if started && !ended && matches!(framing, PtpFraming::Standard) =>
                {
                    ended = true;
                }
                _ => bail!("invalid data sequence"),
            }
            if result.payload.len().saturating_add(data.payload.len()) > MAX_PAYLOAD {
                bail!("payload exceeds 16 MiB")
            }
            result.payload.extend(data.payload);
            if ended && total.is_some_and(|len| len != result.payload.len() as u64) {
                bail!("payload length mismatch")
            }
        }
        bail!("data frame count exceeded 1024")
    };
    match tokio::time::timeout(Duration::from_millis(request.timeout_ms), operation).await {
        Ok(Ok(())) => {}
        Ok(Err(error)) => result.record.error = Some(format!("{error:#}")),
        Err(_) => {
            result.record.error = Some(format!("request timed out after {} ms", request.timeout_ms))
        }
    }
    if saw_data || !result.payload.is_empty() {
        let filename = format!("{label}.bin");
        result.record.payload = Some(payload_metadata(&result.payload));
        match std::fs::write(options.output_dir.join(&filename), &result.payload) {
            Ok(()) => result.record.payload_file = Some(filename),
            Err(error) => result.record.error = Some(format!("retain payload: {error}")),
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
    use std::sync::Mutex;
    use std::time::{SystemTime, UNIX_EPOCH};

    use async_trait::async_trait;
    use camera_protocol_ffi::{PtpSessionOpenResult, PtpTransportError, SocketRole};
    use ptp_core::{
        DataBlock, OperationResponse, PropForm, PtpCodec, PtpIpPacket, StartData, Writer,
    };
    use serde_json::json;

    struct TestDir(PathBuf);
    impl TestDir {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "capability-{}-{}",
                std::process::id(),
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            std::fs::create_dir(&path).unwrap();
            Self(path)
        }
        fn options(&self) -> CapabilityOptions {
            CapabilityOptions {
                selected_model: "Test Body".into(),
                manifest_firmware: "1.0".into(),
                connection: "tether".into(),
                scope_mode: None,
                allow_unknown_model: false,
                allow_control: false,
                output_dir: self.0.join("output"),
            }
        }
    }
    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn ledger() -> CandidateLedger {
        CandidateLedger::from_json(&json!({
            "schema": CANDIDATE_SCHEMA,
            "source": {"application":"Synthetic Client","version":"1","plugin":"test","sha256":"a".repeat(64)},
            "manufacturer":"Test", "model":"Test Body",
            "candidates": [
                {"id":"valid","kind":"property","code":"0xd000","status":"unverified","expectedType":"u16","nativeNames":["Value"],"disposition":"preserveReviewed","reason":"Static identity","evidence":["synthetic"]},
                {"id":"conflict","kind":"property","code":"0xd001","status":"unverified","expectedType":"u16","nativeNames":["Value"],"disposition":"preserveReviewed","reason":"Static type hint","evidence":["synthetic"]},
                {"id":"missing","kind":"property","code":"0xd002","status":"unverified","nativeNames":["Value"],"disposition":"preserveReviewed","reason":"Static identity","evidence":["synthetic"]},
                {"id":"malformed","kind":"property","code":"0xd003","status":"unverified","nativeNames":["Value"],"disposition":"preserveReviewed","reason":"Static identity","evidence":["synthetic"]},
                {"id":"liveview","kind":"operation","code":"0x9018","status":"unverified","nativeNames":["LiveView"],"disposition":"coverageOnly","request":{"opcode":"0x9018","params":[],"dataPhase":"in"},"reason":"Known static shape","evidence":["synthetic"]},
                {"id":"firmware","kind":"operation","code":"0x900c","status":"unverified","nativeNames":["Upload"],"disposition":"coverageOnly","reason":"Dangerous upload","evidence":["synthetic"]},
                {"id":"playback","kind":"operation","code":"0x9008","status":"unverified","nativeNames":["Playback"],"disposition":"coverageOnly","reason":"Missing dynamic handle recipe","evidence":["synthetic"]}
            ]
        }).to_string()).unwrap()
    }

    struct Synthetic {
        framing: PtpFraming,
        id: AtomicU32,
        frames: Mutex<VecDeque<Vec<u8>>>,
        sent: Mutex<Vec<(u16, Vec<u32>)>>,
        writes: Mutex<Vec<Vec<u8>>>,
        pending: Mutex<Option<(u16, u32)>>,
        value: Mutex<Vec<u8>>,
        fail_restore: AtomicBool,
        disconnect: AtomicBool,
    }
    impl Synthetic {
        fn new() -> Arc<Self> {
            Self::with_framing(PtpFraming::Compressed)
        }
        fn with_framing(framing: PtpFraming) -> Arc<Self> {
            Arc::new(Self {
                framing,
                id: AtomicU32::new(1),
                frames: Mutex::new(VecDeque::new()),
                sent: Mutex::new(vec![]),
                writes: Mutex::new(vec![]),
                pending: Mutex::new(None),
                value: Mutex::new(vec![0, 0]),
                fail_restore: AtomicBool::new(false),
                disconnect: AtomicBool::new(false),
            })
        }
        fn enqueue(&self, code: u16, txn: u32, response: u16, payload: Option<Vec<u8>>) {
            let mut frames = self.frames.lock().unwrap();
            if let Some(payload) = payload {
                if matches!(self.framing, PtpFraming::Standard) {
                    let split = payload.len() / 2;
                    for packet in [
                        PtpIpPacket::StartData(StartData {
                            transaction_id: txn,
                            total_length: payload.len() as u64,
                        }),
                        PtpIpPacket::Data(DataBlock {
                            transaction_id: txn,
                            payload: payload[..split].to_vec(),
                        }),
                        PtpIpPacket::EndData(DataBlock {
                            transaction_id: txn,
                            payload: payload[split..].to_vec(),
                        }),
                    ] {
                        frames.push_back(ptp_core::encode(&packet).unwrap());
                    }
                } else {
                    frames.push_back(build_data(self.framing, code, txn, payload).unwrap());
                }
            }
            let packet = PtpIpPacket::OperationResponse(OperationResponse {
                code: response,
                transaction_id: txn,
                params: vec![],
            });
            frames.push_back(if matches!(self.framing, PtpFraming::Standard) {
                ptp_core::encode(&packet).unwrap()
            } else {
                protocol_primitives::fuji_framing::encode(&packet).unwrap()
            });
        }
    }

    #[async_trait]
    impl PtpExecutorTransport for Synthetic {
        async fn reserve_transaction_id(&self) -> Result<u32, PtpTransportError> {
            Ok(self.id.fetch_add(1, Ordering::Relaxed))
        }
        async fn send_command_frame(&self, frame: Vec<u8>) -> Result<(), PtpTransportError> {
            let packet = if matches!(self.framing, PtpFraming::Standard) {
                PtpIpPacket::decode(&frame).unwrap()
            } else {
                protocol_primitives::fuji_framing::decode(&frame).unwrap()
            };
            match packet {
                PtpIpPacket::Data(data) => {
                    let (code, txn) = self.pending.lock().unwrap().take().unwrap();
                    self.writes.lock().unwrap().push(data.payload.clone());
                    let response =
                        if data.payload == [0, 0] && self.fail_restore.load(Ordering::Relaxed) {
                            0x2002
                        } else {
                            *self.value.lock().unwrap() = data.payload;
                            0x2001
                        };
                    self.enqueue(code, txn, response, None);
                }
                PtpIpPacket::OperationRequest(req) => {
                    self.sent
                        .lock()
                        .unwrap()
                        .push((req.code, req.params.clone()));
                    if req.code == 0x1016 {
                        *self.pending.lock().unwrap() = Some((req.code, req.transaction_id));
                        return Ok(());
                    }
                    if req.code == 0x1014 && self.disconnect.load(Ordering::Relaxed) {
                        return Err(PtpTransportError::NotConnected);
                    }
                    if req.params.first() == Some(&0xd002) {
                        self.enqueue(req.code, req.transaction_id, 0x200a, None);
                        return Ok(());
                    }
                    let payload = match req.code {
                        0x1001 => {
                            let mut writer = Writer::new();
                            DeviceInfo {
                                manufacturer: "Test".into(),
                                model: "Test Body".into(),
                                device_version: "3.0".into(),
                                device_properties_supported: vec![0xd000],
                                operations_supported: vec![0x1001, 0x1014, 0x1015, 0x9018],
                                ..Default::default()
                            }
                            .encode(&mut writer)
                            .unwrap();
                            writer.into_vec()
                        }
                        0x1014 if req.params == [0xd003] => vec![],
                        0x1014 => {
                            let mut writer = Writer::new();
                            let code = req.params[0] as u16;
                            let (datatype, value) = if code == 0xd001 {
                                (6, PropValue::U32(0))
                            } else {
                                (4, PropValue::U16(0))
                            };
                            DevicePropDesc {
                                code,
                                datatype,
                                get_set: 1,
                                factory_default: value.clone(),
                                current: value,
                                form: PropForm::None,
                            }
                            .encode(&mut writer)
                            .unwrap();
                            writer.into_vec()
                        }
                        0x1015 if req.params == [0xd001] => vec![0, 0, 0, 0],
                        0x1015 => self.value.lock().unwrap().clone(),
                        0x9018 => b"image".to_vec(),
                        _ => panic!("unexpected operation {:04x}", req.code),
                    };
                    self.enqueue(req.code, req.transaction_id, 0x2001, Some(payload));
                }
                _ => panic!("unexpected packet"),
            }
            Ok(())
        }
        async fn next_command_frame(&self) -> Result<Vec<u8>, PtpTransportError> {
            self.frames
                .lock()
                .unwrap()
                .pop_front()
                .ok_or(PtpTransportError::NotConnected)
        }
        async fn next_event_frame(&self, _: u16) -> Result<Vec<u8>, PtpTransportError> {
            Err(PtpTransportError::NotConnected)
        }
        async fn open_channel(&self, _: SocketRole) -> Result<(), PtpTransportError> {
            Err(PtpTransportError::NotConnected)
        }
        async fn close_command_channel(&self, _: Option<Vec<u8>>) -> Result<(), PtpTransportError> {
            Ok(())
        }
        async fn reopen_command_session(&self) -> Result<PtpSessionOpenResult, PtpTransportError> {
            Err(PtpTransportError::NotConnected)
        }
        async fn sleep(&self, _: u32) -> Result<(), PtpTransportError> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn standard_framing_reassembles_bounded_fragmented_data() {
        let dir = TestDir::new();
        let report = run_capability_verification(
            &ledger(),
            None,
            PtpFraming::Standard,
            Synthetic::with_framing(PtpFraming::Standard),
            dir.options(),
        )
        .await
        .unwrap();
        assert!(report.error.is_none());
        assert_eq!(report.rows[0].outcome, "observed");
        assert_eq!(report.rows[1].outcome, "typeConflict");
        assert_eq!(report.rows[3].outcome, "decodeError");
        let mut plan = write_plan(&ledger());
        assert!(plan.validate_framing(PtpFraming::Standard).is_err());
        plan.probes.clear();
        assert!(plan.validate_framing(PtpFraming::Standard).is_ok());
    }

    #[test]
    fn checked_in_ledgers_and_known_request_plan_share_the_schema() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../packages/camera-config-data/fuji/catalog");
        let gfx = CandidateLedger::from_json(
            &std::fs::read_to_string(root.join("tether-app-ff0018.candidates.json")).unwrap(),
        )
        .unwrap();
        let unknown = CandidateLedger::from_json(
            &std::fs::read_to_string(root.join("tether-app-ff0021.candidates.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(gfx.model.as_deref(), Some("GFX100 II"));
        assert!(unknown.model.is_none());
        let plan = CapabilityPlan::from_yaml(include_str!("../plans/tether-app-ff0018.yaml"), &gfx)
            .unwrap();
        assert_eq!(plan.probes[1].request.request.params, [0, 0]);
        assert_eq!(plan.probes[0].kind, ProbeKind::Read);
    }

    #[tokio::test]
    async fn explicit_unadvertised_properties_preserve_conflicts_rejections_and_raw_bytes() {
        let dir = TestDir::new();
        let transport = Synthetic::new();
        let report = run_capability_verification(
            &ledger(),
            None,
            PtpFraming::Compressed,
            transport.clone(),
            dir.options(),
        )
        .await
        .unwrap();
        assert_eq!(
            report
                .rows
                .iter()
                .map(|r| r.outcome.as_str())
                .collect::<Vec<_>>(),
            [
                "observed",
                "typeConflict",
                "rejected",
                "decodeError",
                "skipped",
                "excluded-dangerous",
                "skipped"
            ]
        );
        assert_eq!(report.rows[1].advertised, Some(false));
        assert_eq!(report.rows[1].descriptor_type, Some(6));
        assert!(report.rows.iter().all(|row| row.status == "unverified"));
        assert_eq!(
            std::fs::read(dir.0.join("output/row-0001-value.bin")).unwrap(),
            [0, 0, 0, 0]
        );
        assert!(dir.0.join("output/row-0003-descriptor.bin").is_file());
        assert!(report.rows[6].detail.contains("Missing dynamic handle"));
        let sent = transport.sent.lock().unwrap();
        assert_eq!(sent.len(), 9);
        assert!(sent
            .iter()
            .all(|(code, _)| [0x1001, 0x1014, 0x1015].contains(code)));
    }

    #[tokio::test]
    async fn unknown_and_wrong_model_scope_never_imply_camera_support() {
        for model in [None, Some("Other Body".into())] {
            let dir = TestDir::new();
            let transport = Synthetic::new();
            let mut ledger = ledger();
            ledger.model = model;
            let report = run_capability_verification(
                &ledger,
                None,
                PtpFraming::Compressed,
                transport.clone(),
                dir.options(),
            )
            .await
            .unwrap();
            assert_eq!(transport.sent.lock().unwrap().len(), 1);
            assert!(report
                .rows
                .iter()
                .all(|r| r.requests.is_empty() && r.status == "unverified"));
        }
        let dir = TestDir::new();
        let mut ledger = ledger();
        ledger.model = None;
        let mut options = dir.options();
        options.allow_unknown_model = true;
        let report = run_capability_verification(
            &ledger,
            None,
            PtpFraming::Compressed,
            Synthetic::new(),
            options,
        )
        .await
        .unwrap();
        assert!(report.source_model.is_none());
        assert_eq!(report.rows[0].outcome, "observed");
        assert_eq!(report.rows[0].status, "unverified");
    }

    #[tokio::test]
    async fn transport_failure_retains_report_then_skips_remaining_requests() {
        let dir = TestDir::new();
        let transport = Synthetic::new();
        transport.disconnect.store(true, Ordering::Relaxed);
        let report = run_capability_verification(
            &ledger(),
            None,
            PtpFraming::Compressed,
            transport.clone(),
            dir.options(),
        )
        .await
        .unwrap();
        assert!(report.error.is_some());
        assert_eq!(report.rows[0].outcome, "transportError");
        assert_eq!(transport.sent.lock().unwrap().len(), 2);
        assert!(dir.0.join("output/capability-report.json").is_file());
        assert!(report.rows[1].detail.contains("earlier transport"));
    }

    fn write_plan(ledger: &CandidateLedger) -> CapabilityPlan {
        CapabilityPlan::from_yaml(
            r#"
schema: camera-capability-plan/v1
connection: tether
probes:
  - candidateId: valid
    kind: reversibleWrite
    mode: test
    shapeEvidence: [synthetic]
    request:
      request: {opcode: '0x1016', params: [53248], dataPhase: out, payloadHex: '0100'}
    baseline:
      request: {opcode: '0x1015', params: [53248], dataPhase: in}
    verify:
      request: {opcode: '0x1015', params: [53248], dataPhase: in}
      expectPayloadHex: '0200'
    restore:
      request: {opcode: '0x1016', params: [53248], dataPhase: out}
    verifyRestored:
      request: {opcode: '0x1015', params: [53248], dataPhase: in}
"#,
            ledger,
        )
        .unwrap()
    }

    #[tokio::test]
    async fn writes_require_permission_then_restore_after_readback_failure() {
        let ledger = ledger();
        let plan = write_plan(&ledger);
        let dir = TestDir::new();
        let transport = Synthetic::new();
        let mut options = dir.options();
        options.scope_mode = Some("test".into());
        let report = run_capability_verification(
            &ledger,
            Some(&plan),
            PtpFraming::Compressed,
            transport.clone(),
            options,
        )
        .await
        .unwrap();
        assert!(transport.writes.lock().unwrap().is_empty());
        assert!(report.rows[0].detail.contains("--allow-control"));
        for fail_restore in [false, true] {
            let dir = TestDir::new();
            let transport = Synthetic::new();
            transport
                .fail_restore
                .store(fail_restore, Ordering::Relaxed);
            let mut options = dir.options();
            options.scope_mode = Some("test".into());
            options.allow_control = true;
            let report = run_capability_verification(
                &ledger,
                Some(&plan),
                PtpFraming::Compressed,
                transport.clone(),
                options,
            )
            .await
            .unwrap();
            assert_eq!(*transport.writes.lock().unwrap(), [vec![1, 0], vec![0, 0]]);
            assert_eq!(report.rows[0].outcome, "expectationFailed");
            assert_eq!(report.rows[0].restored, Some(!fail_restore));
            assert_eq!(report.rows[0].restore_error.is_some(), fail_restore);
            assert_eq!(report.rows[0].status, "unverified");
        }
    }

    #[test]
    fn rejects_incomplete_or_dangerous_plans() {
        let ledger = ledger();
        let mut plan = write_plan(&ledger);
        plan.probes[0].restore = None;
        assert!(plan.validate(&ledger).is_err());
        let mut plan = write_plan(&ledger);
        plan.probes[0].request.request.opcode = "0x900d".into();
        assert!(plan
            .validate(&ledger)
            .unwrap_err()
            .to_string()
            .contains("excluded-dangerous"));
        let mut wrong_target = write_plan(&ledger);
        wrong_target.probes[0].request.request.params = vec![53249];
        wrong_target.probes[0]
            .restore
            .as_mut()
            .unwrap()
            .request
            .params = vec![53249];
        assert!(wrong_target.validate(&ledger).is_err());
        assert!(parse_code("0xd0").is_err());
        assert!(parse_code("0xd017").is_ok());
        assert!(decode_hex("0€").is_err());
    }

    #[tokio::test]
    async fn known_read_requires_explicit_plan_and_mode_claim() {
        let ledger = ledger();
        let plan = CapabilityPlan::from_yaml(
            r#"
schema: camera-capability-plan/v1
connection: tether
probes:
  - candidateId: liveview
    kind: read
    mode: liveview
    shapeEvidence: [synthetic]
    request:
      request: {opcode: '0x9018', params: [], dataPhase: in}
      expectPayloadHex: '696d616765'
"#,
            &ledger,
        )
        .unwrap();
        for mode in [None, Some("liveview".into())] {
            let dir = TestDir::new();
            let transport = Synthetic::new();
            let mut options = dir.options();
            options.scope_mode = mode;
            let report = run_capability_verification(
                &ledger,
                Some(&plan),
                PtpFraming::Compressed,
                transport.clone(),
                options.clone(),
            )
            .await
            .unwrap();
            assert_eq!(
                report.rows[4].outcome,
                if options.scope_mode.is_some() {
                    "observed"
                } else {
                    "skipped"
                }
            );
            assert_eq!(report.rows[4].status, "unverified");
        }
    }
}
