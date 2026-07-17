//! Fail-closed `camera-observation/v1` validation, proposal, and reviewed apply.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::model::*;
use crate::observation::*;

pub const VALIDATION_REPORT_SCHEMA: &str = "camera-observation-validation/v1";
pub const PROPOSAL_SCHEMA: &str = "camera-config-proposal/v1";
pub const REVIEW_SCHEMA: &str = "camera-config-review/v1";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ValidationReport {
    pub schema: String,
    pub total_nonblank: usize,
    pub accepted: usize,
    pub rejected: usize,
    pub dispositions: Vec<RecordDisposition>,
}

impl ValidationReport {
    pub fn is_valid(&self) -> bool {
        self.rejected == 0
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RecordDisposition {
    pub identity: String,
    pub status: ValidationStatus,
    pub code: String,
    pub message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ValidationStatus {
    Accepted,
    Rejected,
}

#[derive(Debug, Clone)]
pub struct ValidatedObservations {
    pub headers: Vec<BundleHeader>,
    pub records: Vec<ObservationLine>,
    pub report: ValidationReport,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Proposal {
    pub schema: String,
    pub observation_schema: String,
    pub candidates: Vec<ProposalCandidate>,
    pub record_dispositions: Vec<ProposalRecordDisposition>,
    pub digest: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProposalRecordDisposition {
    pub identity: String,
    pub status: ProposalRecordStatus,
    pub candidate_ids: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ProposalRecordStatus {
    Proposed,
    EvidenceOnly,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProposalCandidate {
    pub id: String,
    pub assertion: CandidateAssertion,
    pub source_records: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
pub enum CandidateAssertion {
    CameraIdentity {
        manufacturer: String,
        model: String,
        firmware: String,
    },
    Operation {
        code: String,
        supported: bool,
        scopes: Vec<ExecutionContext>,
    },
    Property {
        code: String,
        supported: bool,
        property_type: Option<String>,
        access: Option<String>,
        descriptor: Option<CapabilityDescriptor>,
        labels: BTreeMap<String, String>,
        value_profiles: Vec<CapabilityValueProfile>,
        scopes: Vec<ExecutionContext>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProposalReview {
    pub schema: String,
    pub proposal_digest: String,
    pub decisions: BTreeMap<String, ReviewDisposition>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ReviewDisposition {
    Accept,
    Reject,
    Defer,
}

#[derive(Debug, Error)]
pub enum GenerationError {
    #[error("observation validation rejected {0} record(s)")]
    Rejected(usize),
    #[error("proposal serialization failed: {0}")]
    ProposalSerialization(#[from] serde_json::Error),
    #[error("proposal review uses schema {found:?}; expected {expected:?}")]
    ReviewSchema { found: String, expected: String },
    #[error("proposal review digest {found:?} does not match {expected:?}")]
    ReviewDigest { found: String, expected: String },
    #[error("proposal integrity check failed: {0}")]
    ProposalIntegrity(String),
    #[error("proposal review candidate set is incomplete: {0}")]
    ReviewCoverage(String),
    #[error("accepted proposal conflicts with the base manifest: {0}")]
    ApplyConflict(String),
}

#[derive(Debug)]
struct ParsedLine {
    identity: String,
    value: Option<ObservationLine>,
    errors: BTreeSet<(String, String)>,
    counts_as_record: bool,
}

impl ParsedLine {
    fn reject(&mut self, code: &str, message: impl Into<String>) {
        self.errors.insert((code.to_string(), message.into()));
    }
}

/// Validate one or more independent JSONL bundles. Each string must contain its
/// own header as the first nonblank line. File names are intentionally absent
/// from the API and report so host paths cannot affect deterministic output.
pub fn validate_bundles(inputs: &[&str]) -> Result<ValidatedObservations, ValidationReport> {
    let mut bundles = Vec::new();
    for input in inputs {
        bundles.push(parse_bundle(input));
    }

    for lines in &mut bundles {
        validate_bundle(lines);
    }
    validate_duplicate_runs(&mut bundles);
    validate_conflicts(&mut bundles);

    let total_nonblank = bundles
        .iter()
        .flatten()
        .filter(|line| line.counts_as_record)
        .count();
    let accepted = bundles
        .iter()
        .flatten()
        .filter(|line| line.counts_as_record && line.errors.is_empty())
        .count();
    let rejected = bundles
        .iter()
        .flatten()
        .filter(|line| !line.errors.is_empty())
        .count();

    let mut dispositions = Vec::new();
    let mut headers = Vec::new();
    let mut records = Vec::new();
    for lines in bundles {
        for line in lines {
            if line.errors.is_empty() {
                if let Some(value) = line.value {
                    if let ObservationLine::BundleHeader(header) = &value {
                        headers.push(header.clone());
                    } else {
                        records.push(value);
                    }
                }
                dispositions.push(RecordDisposition {
                    identity: line.identity,
                    status: ValidationStatus::Accepted,
                    code: "O000".to_string(),
                    message: "accepted".to_string(),
                });
            } else {
                for (code, message) in line.errors {
                    dispositions.push(RecordDisposition {
                        identity: line.identity.clone(),
                        status: ValidationStatus::Rejected,
                        code,
                        message,
                    });
                }
            }
        }
    }
    dispositions.sort_by(|left, right| {
        (&left.identity, &left.code, &left.message).cmp(&(
            &right.identity,
            &right.code,
            &right.message,
        ))
    });
    headers.sort_by(|left, right| left.run_id.cmp(&right.run_id));
    records.sort_by(|left, right| {
        (left.run_id(), left.ordinal(), left.record_id()).cmp(&(
            right.run_id(),
            right.ordinal(),
            right.record_id(),
        ))
    });
    let report = ValidationReport {
        schema: VALIDATION_REPORT_SCHEMA.to_string(),
        total_nonblank,
        accepted,
        rejected,
        dispositions,
    };
    if report.is_valid() {
        Ok(ValidatedObservations {
            headers,
            records,
            report,
        })
    } else {
        Err(report)
    }
}

fn parse_bundle(input: &str) -> Vec<ParsedLine> {
    let mut lines = input
        .lines()
        .filter_map(|raw| {
            let raw = raw.trim();
            if raw.is_empty() {
                return None;
            }
            let raw_identity = format!("raw:{}", sha256(raw.as_bytes()));
            let parsed_json: serde_json::Value = match serde_json::from_str(raw) {
                Ok(value) => value,
                Err(error) => {
                    let mut line = ParsedLine {
                        identity: raw_identity,
                        value: None,
                        errors: BTreeSet::new(),
                        counts_as_record: true,
                    };
                    line.reject("O001", format!("malformed JSON: {error}"));
                    return Some(line);
                }
            };
            let identity = parsed_json
                .get("recordId")
                .and_then(serde_json::Value::as_str)
                .map(|record| {
                    let run = parsed_json
                        .get("runId")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or("unknown-run");
                    format!("{run}:{record}")
                })
                .unwrap_or(raw_identity);
            let mut line = ParsedLine {
                identity,
                value: None,
                errors: BTreeSet::new(),
                counts_as_record: true,
            };
            match parsed_json
                .get("schema")
                .and_then(serde_json::Value::as_str)
            {
                Some(OBSERVATION_SCHEMA_VERSION) => {}
                Some(found) => {
                    line.reject("O002", format!("unknown schema {found:?}"));
                    return Some(line);
                }
                None => {
                    line.reject("O002", "record is missing string field 'schema'");
                    return Some(line);
                }
            }
            match serde_json::from_value(parsed_json) {
                Ok(value) => line.value = Some(value),
                Err(error) => line.reject("O003", format!("invalid or unknown record: {error}")),
            }
            Some(line)
        })
        .collect::<Vec<_>>();
    if lines.is_empty() {
        let mut line = ParsedLine {
            identity: "bundle:missing-header".to_string(),
            value: None,
            errors: BTreeSet::new(),
            counts_as_record: false,
        };
        line.reject("O010", "bundle requires exactly one header; found 0");
        lines.push(line);
    }
    lines
}

fn validate_bundle(lines: &mut [ParsedLine]) {
    let header_indices = lines
        .iter()
        .enumerate()
        .filter_map(|(index, line)| {
            matches!(line.value, Some(ObservationLine::BundleHeader(_))).then_some(index)
        })
        .collect::<Vec<_>>();
    if header_indices.len() != 1 {
        for line in lines.iter_mut() {
            line.reject(
                "O010",
                format!(
                    "bundle requires exactly one header; found {}",
                    header_indices.len()
                ),
            );
        }
        return;
    }
    let header_index = header_indices[0];
    if header_index != 0 {
        lines[header_index].reject("O011", "bundle header must be the first nonblank record");
    }
    let header = match &lines[header_index].value {
        Some(ObservationLine::BundleHeader(value)) => value.clone(),
        _ => unreachable!("header index selected above"),
    };
    if header.ordinal != 0 {
        lines[header_index].reject("O012", "bundle header ordinal must be zero");
    }
    validate_identifier(&header.run_id, "runId", &mut lines[header_index]);
    validate_identifier(&header.record_id, "recordId", &mut lines[header_index]);
    validate_header(&header, &mut lines[header_index]);

    let clock_ids = header
        .capture
        .clocks
        .iter()
        .map(|clock| clock.id.as_str())
        .collect::<BTreeSet<_>>();
    let clock_mappings = header
        .capture
        .clock_mappings
        .iter()
        .flat_map(|mapping| {
            [
                (mapping.from.as_str(), mapping.to.as_str()),
                (mapping.to.as_str(), mapping.from.as_str()),
            ]
        })
        .collect::<BTreeSet<_>>();
    let artifacts = header
        .capture
        .artifacts
        .iter()
        .map(|artifact| (artifact.id.as_str(), artifact.length))
        .collect::<BTreeMap<_, _>>();
    let mut ids = BTreeMap::new();
    let mut ordinals = BTreeMap::new();
    let mut transaction_correlations = BTreeMap::new();
    let mut transaction_ids = BTreeMap::new();
    for (index, line) in lines.iter().enumerate() {
        let Some(value) = &line.value else {
            continue;
        };
        if value.run_id() != header.run_id {
            continue;
        }
        ids.entry(value.record_id().to_string())
            .or_insert_with(Vec::new)
            .push(index);
        ordinals
            .entry(value.ordinal())
            .or_insert_with(Vec::new)
            .push(index);
        if let ObservationLine::PtpTransaction(transaction) = value {
            let key = (
                transaction.connection_instance.clone(),
                transaction.session.clone(),
                transaction.endpoint_set.clone(),
                transaction.transaction_id,
            );
            transaction_correlations
                .entry(key)
                .or_insert_with(Vec::new)
                .push(index);
            transaction_ids.insert(
                transaction.common.record_id.clone(),
                (
                    transaction.connection_instance.clone(),
                    transaction.session.clone(),
                ),
            );
        }
    }
    for duplicates in ids.values().filter(|indices| indices.len() > 1) {
        for index in duplicates {
            lines[*index].reject("O020", "duplicate recordId within run");
        }
    }
    for duplicates in ordinals.values().filter(|indices| indices.len() > 1) {
        for index in duplicates {
            lines[*index].reject("O021", "duplicate ordinal within run");
        }
    }
    for duplicates in transaction_correlations
        .values()
        .filter(|indices| indices.len() > 1)
    {
        for index in duplicates {
            lines[*index].reject("O022", "duplicate PTP transaction correlation identity");
        }
    }

    for line in lines.iter_mut() {
        let Some(value) = line.value.clone() else {
            continue;
        };
        if value.schema() != OBSERVATION_SCHEMA_VERSION {
            line.reject("O002", "record schema discriminator is not canonical");
        }
        if value.run_id() != header.run_id {
            line.reject("O014", "record runId does not match its bundle header");
        }
        validate_identifier(value.record_id(), "recordId", line);
        if !matches!(value, ObservationLine::BundleHeader(_)) && value.ordinal() == 0 {
            line.reject("O023", "non-header ordinal must be positive");
        }
        let Some(common) = value.common() else {
            continue;
        };
        if !clock_ids.contains(common.time.clock.as_str()) {
            line.reject("O030", format!("unknown clock {:?}", common.time.clock));
        }
        validate_context(&common.context, line);
        for range in &common.artifact_ranges {
            validate_range(range, &artifacts, line);
        }
        validate_record(
            &value,
            &artifacts,
            &transaction_ids,
            &clock_ids,
            &clock_mappings,
            line,
        );
    }
}

fn validate_duplicate_runs(bundles: &mut [Vec<ParsedLine>]) {
    let mut runs: BTreeMap<String, Vec<(usize, usize)>> = BTreeMap::new();
    for (bundle_index, lines) in bundles.iter().enumerate() {
        for (line_index, line) in lines.iter().enumerate() {
            if let Some(ObservationLine::BundleHeader(header)) = &line.value {
                runs.entry(header.run_id.clone())
                    .or_default()
                    .push((bundle_index, line_index));
            }
        }
    }
    for duplicates in runs.into_values().filter(|entries| entries.len() > 1) {
        for (bundle, line) in duplicates {
            bundles[bundle][line].reject("O013", "duplicate runId across bundles");
        }
    }
}

fn validate_header(header: &BundleHeader, line: &mut ParsedLine) {
    for (field, value) in [
        ("camera.manufacturer", header.camera.manufacturer.as_str()),
        ("camera.model", header.camera.model.as_str()),
        ("camera.bodyId", header.camera.body_id.as_str()),
        ("camera.firmware", header.camera.firmware.as_str()),
        ("client.artifact", header.client.artifact.as_str()),
        ("client.version", header.client.version.as_str()),
        ("client.platform", header.client.platform.as_str()),
    ] {
        validate_sanitized(value, field, line);
    }
    if header.capture.interfaces.is_empty() {
        line.reject("O040", "capture.interfaces must not be empty");
    }
    if header.capture.clocks.is_empty() {
        line.reject("O041", "capture.clocks must not be empty");
    }
    if header.capture.tool_versions.is_empty() {
        line.reject("O042", "capture.toolVersions must not be empty");
    }
    if header.capture.loss.dropped_records != 0
        || header.capture.loss.dropped_bytes != 0
        || header.capture.loss.truncated_payloads != 0
    {
        line.reject("O043", "loss or truncation makes the bundle non-proposable");
    }
    let mut interfaces = BTreeSet::new();
    for interface in &header.capture.interfaces {
        validate_identifier(&interface.id, "capture interface id", line);
        if !interfaces.insert(interface.id.as_str()) {
            line.reject(
                "O044",
                format!("duplicate capture interface {:?}", interface.id),
            );
        }
    }
    let mut clocks = BTreeSet::new();
    for clock in &header.capture.clocks {
        validate_identifier(&clock.id, "clock id", line);
        if !clocks.insert(clock.id.as_str()) {
            line.reject("O045", format!("duplicate clock {:?}", clock.id));
        }
    }
    for mapping in &header.capture.clock_mappings {
        if mapping.from == mapping.to
            || !clocks.contains(mapping.from.as_str())
            || !clocks.contains(mapping.to.as_str())
        {
            line.reject(
                "O046",
                "clock mapping must link two declared distinct clocks",
            );
        }
    }
    let mut artifacts = BTreeSet::new();
    for artifact in &header.capture.artifacts {
        validate_identifier(&artifact.id, "artifact id", line);
        validate_sha256(&artifact.sha256, line);
        if !artifacts.insert(artifact.id.as_str()) {
            line.reject("O047", format!("duplicate artifact {:?}", artifact.id));
        }
    }
}

fn validate_record(
    value: &ObservationLine,
    artifacts: &BTreeMap<&str, u64>,
    transaction_ids: &BTreeMap<String, (String, String)>,
    clock_ids: &BTreeSet<&str>,
    clock_mappings: &BTreeSet<(&str, &str)>,
    line: &mut ParsedLine,
) {
    match value {
        ObservationLine::BundleHeader(_) | ObservationLine::Lifecycle(_) => {}
        ObservationLine::BleGatt(record) => {
            validate_payload_opt(record.payload.as_ref(), artifacts, line);
            if record.outcome == TransportOutcome::Incomplete {
                line.reject("O100", "incomplete BLE record is not accepted");
            }
        }
        ObservationLine::PtpTransaction(record) => {
            validate_ptp_code(&record.request.operation, "operation", line);
            if let Some(data) = &record.request.data {
                validate_payload(&data.payload, artifacts, line);
            }
            if let Some(response) = &record.response {
                validate_ptp_code(&response.code, "response", line);
                if let Some(data) = &response.data {
                    validate_payload(&data.payload, artifacts, line);
                }
            }
            match record.outcome {
                TransactionOutcome::Ok => {
                    if record
                        .response
                        .as_ref()
                        .map(|response| response.code.as_str())
                        != Some("0x2001")
                    {
                        line.reject("O101", "ok PTP outcome requires response 0x2001");
                    }
                }
                TransactionOutcome::NonOk => {
                    if record.response.is_none()
                        || record
                            .response
                            .as_ref()
                            .is_some_and(|response| response.code == "0x2001")
                    {
                        line.reject("O102", "nonOk PTP outcome requires a non-0x2001 response");
                    }
                }
                TransactionOutcome::Timeout | TransactionOutcome::TransportAbort => {
                    if record.response.is_some() {
                        line.reject("O103", "timeout/transportAbort cannot carry a response");
                    }
                }
                TransactionOutcome::Incomplete => {
                    line.reject("O104", "incomplete PTP transaction is not accepted");
                }
            }
            let orthogonal_count = [
                record.evidence_basis.is_some(),
                record.observed_effect.is_some(),
                record.readback.is_some(),
            ]
            .into_iter()
            .filter(|present| *present)
            .count();
            if orthogonal_count != 0 && orthogonal_count != 3 {
                line.reject(
                    "O110",
                    "evidenceBasis, observedEffect, and readback must be supplied together",
                );
            } else if let (Some(basis), Some(effect), Some(readback)) = (
                record.evidence_basis,
                record.observed_effect,
                record.readback.as_ref(),
            ) {
                validate_orthogonal_claims(Some(record.outcome), basis, effect, readback, line);
            }
        }
        ObservationLine::PtpEvent(record) => {
            validate_ptp_code(&record.event, "event", line);
            validate_payload_opt(record.payload.as_ref(), artifacts, line);
            if let Some(transaction_record_id) = &record.transaction_record_id {
                match transaction_ids.get(transaction_record_id) {
                    None => line.reject("O120", "PTP event has a dangling transactionRecordId"),
                    Some((connection, session))
                        if connection != &record.connection_instance
                            || session != &record.session =>
                    {
                        line.reject(
                            "O121",
                            "PTP event transaction link crosses connection or session",
                        );
                    }
                    Some(_) => {}
                }
            }
        }
        ObservationLine::HttpExchange(record) => {
            validate_payload_opt(record.request.body.as_ref(), artifacts, line);
            if let Some(response) = &record.response {
                validate_payload_opt(response.body.as_ref(), artifacts, line);
            }
            match record.outcome {
                TransportOutcome::Ok if record.response.is_none() => {
                    line.reject("O130", "successful HTTP exchange requires a response");
                }
                TransportOutcome::Timeout | TransportOutcome::Abort
                    if record.response.is_some() =>
                {
                    line.reject(
                        "O131",
                        "timeout/abort HTTP exchange cannot carry a response",
                    );
                }
                TransportOutcome::Incomplete => {
                    line.reject("O132", "incomplete HTTP exchange is not accepted");
                }
                _ => {}
            }
        }
        ObservationLine::Capability(record) => {
            if let CapabilitySubject::Operation { code, .. }
            | CapabilitySubject::Property { code, .. } = &record.subject
            {
                validate_ptp_code(code, "capability code", line);
            }
            validate_orthogonal_claims(
                None,
                record.evidence_basis,
                record.observed_effect,
                &record.readback,
                line,
            );
        }
        ObservationLine::ActionInvocation(record) => {
            validate_identifier(&record.catalog_revision, "catalogRevision", line);
            validate_identifier(&record.action_id, "actionId", line);
        }
    }
    let Some(common) = value.common() else {
        return;
    };
    let readback = match value {
        ObservationLine::PtpTransaction(record) => record.readback.as_ref(),
        ObservationLine::Capability(record) => Some(&record.readback),
        _ => None,
    };
    if let Some(Readback::Observed { observed_at, .. }) = readback {
        if !clock_ids.contains(observed_at.clock.as_str()) {
            line.reject(
                "O031",
                format!("readback uses unknown clock {:?}", observed_at.clock),
            );
        } else if observed_at.clock != common.time.clock
            && !clock_mappings.contains(&(common.time.clock.as_str(), observed_at.clock.as_str()))
        {
            line.reject(
                "O032",
                "readback clock differs from record clock without a declared mapping",
            );
        }
    }
}

fn validate_orthogonal_claims(
    outcome: Option<TransactionOutcome>,
    basis: ControlEvidenceBasis,
    effect: ControlObservedEffect,
    readback: &Readback,
    line: &mut ParsedLine,
) {
    if basis == ControlEvidenceBasis::DescriptorOnly {
        if effect != ControlObservedEffect::Unknown
            || !matches!(readback, Readback::NotObserved { .. })
        {
            line.reject(
                "O111",
                "descriptorOnly requires unknown effect and notObserved readback",
            );
        }
        return;
    }
    match effect {
        ControlObservedEffect::Confirmed => match readback {
            Readback::Observed {
                request, observed, ..
            } if request == observed => {
                if outcome.is_some_and(|value| value != TransactionOutcome::Ok) {
                    line.reject("O112", "confirmed effect requires an ok transaction");
                }
            }
            _ => line.reject(
                "O112",
                "confirmed effect requires matching observed readback",
            ),
        },
        ControlObservedEffect::AckNoEffect => match readback {
            Readback::Observed {
                baseline,
                request,
                observed,
                ..
            } if observed == baseline && observed != request => {
                if outcome.is_some_and(|value| value != TransactionOutcome::Ok) {
                    line.reject("O113", "ackNoEffect requires an ok transaction");
                }
            }
            _ => line.reject(
                "O113",
                "ackNoEffect requires observed baseline distinct from request",
            ),
        },
        ControlObservedEffect::ProtocolRefused => {
            if !matches!(readback, Readback::NotObserved { .. })
                || outcome.is_some_and(|value| value != TransactionOutcome::NonOk)
            {
                line.reject(
                    "O114",
                    "protocolRefused requires nonOk transaction and notObserved readback",
                );
            }
        }
        ControlObservedEffect::DestructiveClamp => match readback {
            Readback::Observed {
                request, observed, ..
            } if request != observed => {
                if outcome.is_some_and(|value| value != TransactionOutcome::Ok) {
                    line.reject("O115", "destructiveClamp requires an ok transaction");
                }
            }
            _ => line.reject(
                "O115",
                "destructiveClamp requires an observed value distinct from request",
            ),
        },
        ControlObservedEffect::Unknown => {
            if !matches!(readback, Readback::NotObserved { .. }) {
                line.reject("O116", "unknown effect requires notObserved readback");
            }
        }
    }
}

fn validate_conflicts(bundles: &mut [Vec<ParsedLine>]) {
    type CapabilityFact = (CapabilitySubject, (usize, usize));
    let mut facts: BTreeMap<String, Vec<CapabilityFact>> = BTreeMap::new();
    type ScopedCapabilityFact = (CapabilitySubject, ExecutionContext, (usize, usize));
    let mut global_facts: BTreeMap<String, Vec<ScopedCapabilityFact>> = BTreeMap::new();
    for (bundle_index, lines) in bundles.iter().enumerate() {
        for (line_index, line) in lines.iter().enumerate() {
            let Some(ObservationLine::Capability(record)) = &line.value else {
                continue;
            };
            let (subject, code) = match &record.subject {
                CapabilitySubject::Identity { .. } => ("identity", "camera".to_string()),
                CapabilitySubject::Operation { code, .. } => ("operation", code.clone()),
                CapabilitySubject::Property { code, .. } => ("property", code.clone()),
            };
            let key = format!(
                "{}:{}:{}:{}:{}",
                subject,
                code,
                record.common.context.connection,
                record.common.context.mode,
                record.common.context.state
            );
            facts
                .entry(key)
                .or_default()
                .push((record.subject.clone(), (bundle_index, line_index)));
            if subject != "identity" {
                global_facts
                    .entry(format!("{subject}:{code}"))
                    .or_default()
                    .push((
                        record.subject.clone(),
                        record.common.context.clone(),
                        (bundle_index, line_index),
                    ));
            }
        }
    }
    for entries in facts.into_values() {
        let conflicts = entries.iter().enumerate().any(|(index, (left, _))| {
            entries[index + 1..]
                .iter()
                .any(|(right, _)| capability_subjects_conflict(left, right))
        });
        if conflicts {
            for (_, (bundle, line)) in entries {
                bundles[bundle][line]
                    .reject("O400", "conflicting capability fact in one exact scope");
            }
        }
    }
    for entries in global_facts.into_values() {
        let conflicts = entries
            .iter()
            .enumerate()
            .any(|(index, (left, left_context, _))| {
                entries[index + 1..]
                    .iter()
                    .any(|(right, right_context, _)| {
                        left_context != right_context && capability_subjects_conflict(left, right)
                    })
            });
        if conflicts {
            for (_, _, (bundle, line)) in entries {
                bundles[bundle][line].reject(
                    "O401",
                    "capability facts conflict across scopes that the manifest cannot represent",
                );
            }
        }
    }
}

fn capability_subjects_conflict(left: &CapabilitySubject, right: &CapabilitySubject) -> bool {
    match (left, right) {
        (
            CapabilitySubject::Identity {
                device_version: left,
            },
            CapabilitySubject::Identity {
                device_version: right,
            },
        ) => left != right,
        (
            CapabilitySubject::Operation {
                supported: left, ..
            },
            CapabilitySubject::Operation {
                supported: right, ..
            },
        ) => left != right,
        (
            CapabilitySubject::Property {
                supported: left_supported,
                property_type: left_type,
                access: left_access,
                descriptor: left_descriptor,
                labels: left_labels,
                value_profiles: left_profiles,
                ..
            },
            CapabilitySubject::Property {
                supported: right_supported,
                property_type: right_type,
                access: right_access,
                descriptor: right_descriptor,
                labels: right_labels,
                value_profiles: right_profiles,
                ..
            },
        ) => {
            left_supported != right_supported
                || option_conflicts(left_type.as_ref(), right_type.as_ref())
                || option_conflicts(left_access.as_ref(), right_access.as_ref())
                || option_conflicts(left_descriptor.as_ref(), right_descriptor.as_ref())
                || left_labels.iter().any(|(key, value)| {
                    right_labels
                        .get(key)
                        .is_some_and(|candidate| candidate != value)
                })
                || left_profiles.iter().any(|left| {
                    right_profiles.iter().any(|right| {
                        left.connection == right.connection
                            && left.mode == right.mode
                            && left.rows != right.rows
                    })
                })
        }
        _ => true,
    }
}

fn option_conflicts<T: PartialEq>(left: Option<&T>, right: Option<&T>) -> bool {
    matches!((left, right), (Some(left), Some(right)) if left != right)
}

fn validate_context(context: &ExecutionContext, line: &mut ParsedLine) {
    validate_sanitized(&context.connection, "context.connection", line);
    validate_sanitized(&context.mode, "context.mode", line);
    validate_sanitized(&context.state, "context.state", line);
}

fn validate_identifier(value: &str, field: &str, line: &mut ParsedLine) {
    if value.is_empty()
        || value.len() > 160
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"._:-@".contains(&byte))
    {
        line.reject("O050", format!("{field} is not a stable identifier"));
    }
}

fn validate_sanitized(value: &str, field: &str, line: &mut ParsedLine) {
    if value.trim().is_empty()
        || value.contains('\n')
        || value.contains('\r')
        || value.starts_with('/')
        || value.contains("\\")
        || value.contains("~/")
    {
        line.reject("O051", format!("{field} is empty or contains a host path"));
    }
}

fn validate_ptp_code(value: &str, field: &str, line: &mut ParsedLine) {
    if value.len() != 6
        || !value.starts_with("0x")
        || !value[2..]
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        line.reject(
            "O060",
            format!("{field} must be lowercase 0x followed by four hex digits"),
        );
    }
}

fn validate_sha256(value: &str, line: &mut ParsedLine) {
    if value.len() != 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        line.reject("O061", "SHA-256 must be 64 lowercase hexadecimal digits");
    }
}

fn validate_range(range: &ArtifactRange, artifacts: &BTreeMap<&str, u64>, line: &mut ParsedLine) {
    let Some(total) = artifacts.get(range.artifact.as_str()) else {
        line.reject("O070", format!("unknown artifact {:?}", range.artifact));
        return;
    };
    if range.length == 0
        || range
            .offset
            .checked_add(range.length)
            .is_none_or(|end| end > *total)
    {
        line.reject(
            "O071",
            "artifact range is empty, overflowing, or out of bounds",
        );
    }
}

fn validate_payload_opt(
    payload: Option<&PayloadMetadata>,
    artifacts: &BTreeMap<&str, u64>,
    line: &mut ParsedLine,
) {
    if let Some(payload) = payload {
        validate_payload(payload, artifacts, line);
    }
}

fn validate_payload(
    payload: &PayloadMetadata,
    artifacts: &BTreeMap<&str, u64>,
    line: &mut ParsedLine,
) {
    validate_sha256(&payload.sha256, line);
    let mut stream_offset = 0u64;
    for range in &payload.stream_ranges {
        validate_sha256(&range.sha256, line);
        if range.length == 0 || range.offset != stream_offset {
            line.reject(
                "O086",
                "stream ranges must be nonempty, contiguous, and start at zero",
            );
        }
        let Some(next_offset) = range.offset.checked_add(range.length) else {
            line.reject("O087", "stream range offset overflows");
            continue;
        };
        stream_offset = next_offset;
    }
    if !payload.stream_ranges.is_empty() && stream_offset != payload.length {
        line.reject("O088", "stream ranges do not account for payload length");
    }
    for range in &payload.ranges {
        validate_range(range, artifacts, line);
    }
    if let Some(inline) = &payload.inline_hex {
        if payload.length > MAX_INLINE_PAYLOAD_BYTES {
            line.reject("O080", "inline payload exceeds the recorder bound");
        }
        let Some(bytes) = decode_hex(inline) else {
            line.reject(
                "O081",
                "inlineHex must contain lowercase even-length hexadecimal",
            );
            return;
        };
        if bytes.len() as u64 != payload.length {
            line.reject("O082", "inlineHex length does not match payload length");
        }
        if sha256(&bytes) != payload.sha256 {
            line.reject("O083", "inlineHex SHA-256 does not match payload metadata");
        }
    } else if payload.length > 0 {
        if payload.stream_ranges.is_empty() {
            line.reject("O084", "non-inline payload requires streaming ranges");
        }
        let artifact_length = payload.ranges.iter().map(|range| range.length).sum::<u64>();
        if !payload.ranges.is_empty() && artifact_length != payload.length {
            line.reject("O085", "artifact ranges do not account for payload length");
        }
    }
}

fn decode_hex(value: &str) -> Option<Vec<u8>> {
    if !value.len().is_multiple_of(2)
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return None;
    }
    (0..value.len())
        .step_by(2)
        .map(|index| u8::from_str_radix(&value[index..index + 2], 16).ok())
        .collect()
}

/// Produce a deterministic proposal. Validation rejection prevents any
/// candidate output, rather than generating from an accepted subset.
pub fn propose(inputs: &[&str]) -> Result<Proposal, ValidationReport> {
    let validated = validate_bundles(inputs)?;
    let mut candidates = Vec::new();

    let identity_sources = validated
        .headers
        .iter()
        .map(|header| format!("{}:{}", header.run_id, header.record_id))
        .collect::<Vec<_>>();
    let identities = validated
        .headers
        .iter()
        .map(|header| {
            (
                header.camera.manufacturer.clone(),
                header.camera.model.clone(),
                header.camera.firmware.clone(),
            )
        })
        .collect::<BTreeSet<_>>();
    for (manufacturer, model, firmware) in identities {
        candidates.push(candidate(
            CandidateAssertion::CameraIdentity {
                manufacturer,
                model,
                firmware,
            },
            identity_sources.clone(),
        ));
    }

    #[derive(Default)]
    struct OperationAggregate {
        scopes: BTreeSet<ExecutionContext>,
        sources: BTreeSet<String>,
    }
    #[derive(Default)]
    struct PropertyAggregate {
        property_type: Option<String>,
        access: Option<String>,
        descriptor: Option<CapabilityDescriptor>,
        labels: BTreeMap<String, String>,
        value_profiles: Vec<CapabilityValueProfile>,
        capability_scopes: BTreeSet<ExecutionContext>,
        capability_sources: BTreeSet<String>,
        label_scopes: BTreeSet<ExecutionContext>,
        label_sources: BTreeSet<String>,
        profile_scopes: BTreeSet<ExecutionContext>,
        profile_sources: BTreeSet<String>,
    }
    let mut operations: BTreeMap<(String, bool), OperationAggregate> = BTreeMap::new();
    let mut properties: BTreeMap<(String, bool), PropertyAggregate> = BTreeMap::new();
    for line in &validated.records {
        let ObservationLine::Capability(record) = line else {
            continue;
        };
        let source = format!("{}:{}", record.common.run_id, record.common.record_id);
        match &record.subject {
            CapabilitySubject::Identity { .. } => {}
            CapabilitySubject::Operation { code, supported } => {
                let aggregate = operations.entry((code.clone(), *supported)).or_default();
                aggregate.scopes.insert(record.common.context.clone());
                aggregate.sources.insert(source);
            }
            CapabilitySubject::Property {
                code,
                supported,
                property_type,
                access,
                descriptor,
                labels,
                value_profiles,
            } => {
                let aggregate = properties.entry((code.clone(), *supported)).or_default();
                aggregate.property_type = aggregate.property_type.clone().or(property_type.clone());
                aggregate.access = aggregate.access.clone().or(access.clone());
                aggregate.descriptor = aggregate.descriptor.clone().or(descriptor.clone());
                aggregate
                    .capability_scopes
                    .insert(record.common.context.clone());
                aggregate.capability_sources.insert(source.clone());
                for (raw, label) in labels {
                    aggregate.labels.entry(raw.clone()).or_insert(label.clone());
                }
                if !labels.is_empty() {
                    aggregate.label_scopes.insert(record.common.context.clone());
                    aggregate.label_sources.insert(source.clone());
                }
                for profile in value_profiles {
                    if !aggregate.value_profiles.contains(profile) {
                        aggregate.value_profiles.push(profile.clone());
                    }
                }
                if !value_profiles.is_empty() {
                    aggregate
                        .profile_scopes
                        .insert(record.common.context.clone());
                    aggregate.profile_sources.insert(source);
                }
            }
        }
    }
    for ((code, supported), aggregate) in operations {
        candidates.push(candidate(
            CandidateAssertion::Operation {
                code,
                supported,
                scopes: aggregate.scopes.into_iter().collect(),
            },
            aggregate.sources.into_iter().collect(),
        ));
    }
    for ((code, supported), mut aggregate) in properties {
        aggregate.value_profiles.sort_by(|left, right| {
            serde_json::to_string(left)
                .expect("profile serializes")
                .cmp(&serde_json::to_string(right).expect("profile serializes"))
        });
        candidates.push(candidate(
            CandidateAssertion::Property {
                code: code.clone(),
                supported,
                property_type: aggregate.property_type,
                access: aggregate.access,
                descriptor: None,
                labels: BTreeMap::new(),
                value_profiles: Vec::new(),
                scopes: aggregate.capability_scopes.iter().cloned().collect(),
            },
            aggregate.capability_sources.iter().cloned().collect(),
        ));
        if let Some(descriptor) = aggregate.descriptor {
            candidates.push(candidate(
                CandidateAssertion::Property {
                    code: code.clone(),
                    supported,
                    property_type: None,
                    access: None,
                    descriptor: Some(descriptor),
                    labels: BTreeMap::new(),
                    value_profiles: Vec::new(),
                    scopes: aggregate.capability_scopes.iter().cloned().collect(),
                },
                aggregate.capability_sources.iter().cloned().collect(),
            ));
        }
        if !aggregate.labels.is_empty() {
            candidates.push(candidate(
                CandidateAssertion::Property {
                    code: code.clone(),
                    supported,
                    property_type: None,
                    access: None,
                    descriptor: None,
                    labels: aggregate.labels,
                    value_profiles: Vec::new(),
                    scopes: aggregate.label_scopes.into_iter().collect(),
                },
                aggregate.label_sources.into_iter().collect(),
            ));
        }
        if !aggregate.value_profiles.is_empty() {
            candidates.push(candidate(
                CandidateAssertion::Property {
                    code,
                    supported,
                    property_type: None,
                    access: None,
                    descriptor: None,
                    labels: BTreeMap::new(),
                    value_profiles: aggregate.value_profiles,
                    scopes: aggregate.profile_scopes.into_iter().collect(),
                },
                aggregate.profile_sources.into_iter().collect(),
            ));
        }
    }
    candidates.sort_by(|left, right| left.id.cmp(&right.id));
    let record_dispositions = validated
        .report
        .dispositions
        .iter()
        .map(|record| {
            let candidate_ids = candidates
                .iter()
                .filter(|candidate| candidate.source_records.contains(&record.identity))
                .map(|candidate| candidate.id.clone())
                .collect::<Vec<_>>();
            ProposalRecordDisposition {
                identity: record.identity.clone(),
                status: if candidate_ids.is_empty() {
                    ProposalRecordStatus::EvidenceOnly
                } else {
                    ProposalRecordStatus::Proposed
                },
                candidate_ids,
            }
        })
        .collect::<Vec<_>>();
    let digest = proposal_digest(&candidates, &record_dispositions);
    Ok(Proposal {
        schema: PROPOSAL_SCHEMA.to_string(),
        observation_schema: OBSERVATION_SCHEMA_VERSION.to_string(),
        candidates,
        record_dispositions,
        digest,
    })
}

fn candidate(assertion: CandidateAssertion, mut source_records: Vec<String>) -> ProposalCandidate {
    source_records.sort();
    source_records.dedup();
    let id = sha256(&serde_json::to_vec(&assertion).expect("assertion serializes"));
    ProposalCandidate {
        id,
        assertion,
        source_records,
    }
}

fn proposal_digest(
    candidates: &[ProposalCandidate],
    record_dispositions: &[ProposalRecordDisposition],
) -> String {
    #[derive(Serialize)]
    #[serde(rename_all = "camelCase")]
    struct ProposalCore<'a> {
        schema: &'a str,
        observation_schema: &'a str,
        candidates: &'a [ProposalCandidate],
        record_dispositions: &'a [ProposalRecordDisposition],
    }
    sha256(
        &serde_json::to_vec(&ProposalCore {
            schema: PROPOSAL_SCHEMA,
            observation_schema: OBSERVATION_SCHEMA_VERSION,
            candidates,
            record_dispositions,
        })
        .expect("proposal core serializes"),
    )
}

/// Apply only accepted candidates to a curated base. Review coverage and digest
/// binding are checked before cloning or mutating the manifest.
pub fn apply_review(
    base: &CameraManifest,
    proposal: &Proposal,
    review: &ProposalReview,
) -> Result<CameraManifest, GenerationError> {
    require_proposal_integrity(proposal)?;
    if review.schema != REVIEW_SCHEMA {
        return Err(GenerationError::ReviewSchema {
            found: review.schema.clone(),
            expected: REVIEW_SCHEMA.to_string(),
        });
    }
    if review.proposal_digest != proposal.digest {
        return Err(GenerationError::ReviewDigest {
            found: review.proposal_digest.clone(),
            expected: proposal.digest.clone(),
        });
    }
    let expected = proposal
        .candidates
        .iter()
        .map(|candidate| candidate.id.clone())
        .collect::<BTreeSet<_>>();
    let actual = review.decisions.keys().cloned().collect::<BTreeSet<_>>();
    if expected != actual {
        let missing = expected.difference(&actual).cloned().collect::<Vec<_>>();
        let extra = actual.difference(&expected).cloned().collect::<Vec<_>>();
        return Err(GenerationError::ReviewCoverage(format!(
            "missing={missing:?}, extra={extra:?}"
        )));
    }

    let mut manifest = base.clone();
    for candidate in &proposal.candidates {
        if review.decisions[&candidate.id] != ReviewDisposition::Accept {
            continue;
        }
        apply_candidate(&mut manifest, candidate)?;
    }
    manifest
        .require_supported_schema()
        .map_err(|error| GenerationError::ApplyConflict(error.to_string()))?;
    manifest
        .require_valid_mode_entries()
        .map_err(|error| GenerationError::ApplyConflict(error.to_string()))?;
    Ok(manifest)
}

fn require_proposal_integrity(proposal: &Proposal) -> Result<(), GenerationError> {
    if proposal.schema != PROPOSAL_SCHEMA
        || proposal.observation_schema != OBSERVATION_SCHEMA_VERSION
    {
        return Err(GenerationError::ProposalIntegrity(
            "proposal uses an unknown schema discriminator".into(),
        ));
    }
    if !proposal
        .candidates
        .windows(2)
        .all(|pair| pair[0].id < pair[1].id)
    {
        return Err(GenerationError::ProposalIntegrity(
            "candidate ids must be unique and sorted".into(),
        ));
    }
    if !proposal
        .record_dispositions
        .windows(2)
        .all(|pair| pair[0].identity < pair[1].identity)
    {
        return Err(GenerationError::ProposalIntegrity(
            "record dispositions must have unique sorted identities".into(),
        ));
    }
    let candidates = proposal
        .candidates
        .iter()
        .map(|candidate| (candidate.id.as_str(), candidate))
        .collect::<BTreeMap<_, _>>();
    let records = proposal
        .record_dispositions
        .iter()
        .map(|record| (record.identity.as_str(), record))
        .collect::<BTreeMap<_, _>>();
    for candidate in &proposal.candidates {
        let expected_id = sha256(
            &serde_json::to_vec(&candidate.assertion).expect("candidate assertion serializes"),
        );
        if candidate.id != expected_id
            || !candidate
                .source_records
                .windows(2)
                .all(|pair| pair[0] < pair[1])
            || candidate.source_records.is_empty()
        {
            return Err(GenerationError::ProposalIntegrity(format!(
                "candidate {:?} has an invalid id or source list",
                candidate.id
            )));
        }
        for source in &candidate.source_records {
            let Some(record) = records.get(source.as_str()) else {
                return Err(GenerationError::ProposalIntegrity(format!(
                    "candidate {:?} references undisposed record {source:?}",
                    candidate.id
                )));
            };
            if !record.candidate_ids.contains(&candidate.id) {
                return Err(GenerationError::ProposalIntegrity(format!(
                    "record {source:?} does not link back to candidate {:?}",
                    candidate.id
                )));
            }
        }
    }
    for record in &proposal.record_dispositions {
        if !record
            .candidate_ids
            .windows(2)
            .all(|pair| pair[0] < pair[1])
            || (record.status == ProposalRecordStatus::Proposed) != !record.candidate_ids.is_empty()
        {
            return Err(GenerationError::ProposalIntegrity(format!(
                "record {:?} has an incoherent disposition",
                record.identity
            )));
        }
        for candidate_id in &record.candidate_ids {
            let Some(candidate) = candidates.get(candidate_id.as_str()) else {
                return Err(GenerationError::ProposalIntegrity(format!(
                    "record {:?} references unknown candidate {candidate_id:?}",
                    record.identity
                )));
            };
            if !candidate.source_records.contains(&record.identity) {
                return Err(GenerationError::ProposalIntegrity(format!(
                    "candidate {candidate_id:?} does not link back to record {:?}",
                    record.identity
                )));
            }
        }
    }
    let expected_digest = proposal_digest(&proposal.candidates, &proposal.record_dispositions);
    if proposal.digest != expected_digest {
        return Err(GenerationError::ProposalIntegrity(format!(
            "proposal digest {:?} does not match its contents",
            proposal.digest
        )));
    }
    Ok(())
}

fn apply_candidate(
    manifest: &mut CameraManifest,
    candidate: &ProposalCandidate,
) -> Result<(), GenerationError> {
    match &candidate.assertion {
        CandidateAssertion::CameraIdentity {
            manufacturer,
            model,
            firmware,
        } => {
            for (field, current, proposed) in [
                ("manufacturer", &manifest.camera.manufacturer, manufacturer),
                ("model", &manifest.camera.model, model),
                ("firmware", &manifest.camera.firmware, firmware),
            ] {
                if !current.is_empty() && current != proposed {
                    return Err(GenerationError::ApplyConflict(format!(
                        "camera {field} {current:?} conflicts with {proposed:?}"
                    )));
                }
            }
            if manifest.camera.manufacturer.is_empty() {
                manifest.camera.manufacturer = manufacturer.clone();
            }
            if manifest.camera.model.is_empty() {
                manifest.camera.model = model.clone();
            }
            if manifest.camera.firmware.is_empty() {
                manifest.camera.firmware = firmware.clone();
            }
        }
        CandidateAssertion::Operation {
            code,
            supported,
            scopes,
        } => {
            if !supported {
                return Ok(());
            }
            register_observed_modes(manifest, scopes);
            let observed_scopes = scopes.iter().map(model_scope).collect::<Vec<_>>();
            let observed_modes = scopes
                .iter()
                .map(|scope| scope.mode.clone())
                .collect::<BTreeSet<_>>()
                .into_iter()
                .collect::<Vec<_>>();
            let observed_connections = scopes
                .iter()
                .map(|scope| scope.connection.clone())
                .collect::<BTreeSet<_>>()
                .into_iter()
                .collect::<Vec<_>>();
            let operation = manifest
                .operations
                .entry(code.clone())
                .or_insert_with(|| Operation {
                    name: parse_hex_code(code)
                        .and_then(crate::std_names::standard_operation_name)
                        .map(String::from)
                        .unwrap_or_else(|| format!("raw_{code}")),
                    owner: String::new(),
                    data_phase: None,
                    params: Vec::new(),
                    workflows: Vec::new(),
                    handler: None,
                    property: None,
                    object_size: None,
                    modes: observed_modes,
                    connections: observed_connections,
                    requires: None,
                    requires_gate: None,
                    effects: Vec::new(),
                    emits: Vec::new(),
                    evidence: vec!["canonicalObservation".to_string()],
                    observed_scopes: Vec::new(),
                });
            merge_scopes(&mut operation.observed_scopes, observed_scopes);
        }
        CandidateAssertion::Property {
            code,
            supported,
            property_type,
            access,
            descriptor,
            labels,
            value_profiles,
            scopes,
        } => {
            if !supported {
                return Ok(());
            }
            register_observed_modes(manifest, scopes);
            let proposal_descriptor = descriptor.as_ref().map(|descriptor| Descriptor {
                form: descriptor.form.clone(),
                values: descriptor
                    .values
                    .iter()
                    .filter_map(serde_json::Value::as_i64)
                    .collect(),
                source: Some(ValueSource::Camera),
            });
            let proposal_profiles = value_profiles
                .iter()
                .map(|profile| PropertyValueProfile {
                    connection: profile.connection.clone(),
                    mode: profile.mode.clone(),
                    rows: profile
                        .rows
                        .iter()
                        .map(|row| PropertyValueProfileRow {
                            label: row.label.clone(),
                            raw: row.raw,
                            legal: row.legal,
                            aliases: row.aliases.clone(),
                            write_store_raw: row.write_store_raw,
                        })
                        .collect(),
                    evidence: profile.evidence.clone(),
                })
                .collect::<Vec<_>>();
            let observed_scopes = scopes.iter().map(model_scope).collect::<Vec<_>>();
            let property = manifest
                .properties
                .entry(code.clone())
                .or_insert_with(|| Property {
                    name: parse_hex_code(code)
                        .and_then(crate::std_names::standard_property_name)
                        .map(String::from)
                        .unwrap_or_else(|| format!("raw_{code}")),
                    ptp_name: None,
                    ptype: property_type.clone(),
                    access: access.clone(),
                    initial_value: None,
                    kind: PropertyKind::Setting,
                    descriptor: proposal_descriptor.clone(),
                    payload: None,
                    controls: BTreeMap::new(),
                    labels: BTreeMap::new(),
                    value_rows: Vec::new(),
                    value_profiles: Vec::new(),
                    value_encoding: None,
                    structured_text: None,
                    requires_gate: None,
                    evidence: vec!["canonicalObservation".to_string()],
                    observed_scopes: Vec::new(),
                });
            if let Some(property_type) = property_type {
                property.ptype = Some(property_type.clone());
            }
            if let Some(access) = access {
                property.access = Some(access.clone());
            }
            if proposal_descriptor.is_some() {
                property.descriptor = proposal_descriptor;
            }
            for (raw, label) in labels {
                property.labels.insert(raw.clone(), label.clone());
            }
            for profile in proposal_profiles {
                property.value_profiles.retain(|current| {
                    current.connection != profile.connection || current.mode != profile.mode
                });
                property.value_profiles.push(profile);
            }
            property.value_profiles.sort_by(|left, right| {
                left.connection
                    .cmp(&right.connection)
                    .then_with(|| left.mode.cmp(&right.mode))
            });
            merge_scopes(&mut property.observed_scopes, observed_scopes);
        }
    }
    if !manifest.evidence.contains_key("canonicalObservation") {
        manifest.evidence.insert(
            "canonicalObservation".to_string(),
            Evidence {
                kind: "camera-observation".to_string(),
                path: "evidence/".to_string(),
                date: String::new(),
            },
        );
    }
    Ok(())
}

fn model_scope(scope: &ExecutionContext) -> ObservedScope {
    ObservedScope {
        connection: scope.connection.clone(),
        mode: scope.mode.clone(),
        state: scope.state.clone(),
    }
}

fn register_observed_modes(manifest: &mut CameraManifest, scopes: &[ExecutionContext]) {
    for scope in scopes {
        manifest.modes.entry(scope.mode.clone()).or_default();
    }
}

fn merge_scopes(target: &mut Vec<ObservedScope>, incoming: Vec<ObservedScope>) {
    target.extend(incoming);
    target.sort();
    target.dedup();
}

pub fn proposal_json(proposal: &Proposal) -> Result<String, serde_json::Error> {
    let mut output = serde_json::to_string_pretty(proposal)?;
    output.push('\n');
    Ok(output)
}

pub fn validation_report_json(report: &ValidationReport) -> Result<String, serde_json::Error> {
    let mut output = serde_json::to_string_pretty(report)?;
    output.push('\n');
    Ok(output)
}

fn sha256(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn canonical_header(run_id: &str, record_id: &str) -> serde_json::Value {
        serde_json::json!({
            "kind": "bundleHeader",
            "schema": OBSERVATION_SCHEMA_VERSION,
            "runId": run_id,
            "recordId": record_id,
            "ordinal": 0,
            "camera": {
                "manufacturer": "EXAMPLE",
                "model": "MODEL 1",
                "bodyId": "body-a",
                "firmware": "1.0"
            },
            "client": { "artifact": "test", "version": "1", "platform": "test" },
            "capture": {
                "interfaces": [{ "id": "fixture", "interfaceType": "synthetic", "role": "test" }],
                "clocks": [{ "id": "mono", "clockType": "monotonic", "unit": "nanoseconds" }],
                "clockMappings": [],
                "loss": { "droppedRecords": 0, "droppedBytes": 0, "truncatedPayloads": 0 },
                "redactions": [],
                "toolVersions": { "fixture": "1" },
                "artifacts": []
            },
            "epistemic": {
                "class": "syntheticFixture", "confidence": "exact",
                "alternatives": [], "unknowns": []
            }
        })
    }

    fn bundle_with_header(header: serde_json::Value, records: &[serde_json::Value]) -> String {
        std::iter::once(header)
            .chain(records.iter().cloned())
            .map(|value| serde_json::to_string(&value).unwrap())
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn canonical_bundle(records: &[serde_json::Value]) -> String {
        bundle_with_header(canonical_header("run-1", "header"), records)
    }

    fn canonical_bundle_for(
        run_id: &str,
        header_record_id: &str,
        records: &[serde_json::Value],
    ) -> String {
        bundle_with_header(canonical_header(run_id, header_record_id), records)
    }

    fn common(record_id: &str, ordinal: u64) -> serde_json::Value {
        serde_json::json!({
            "schema": OBSERVATION_SCHEMA_VERSION,
            "runId": "run-1",
            "recordId": record_id,
            "ordinal": ordinal,
            "context": { "connection": "usb", "mode": "shooting/stills", "state": "ready" },
            "time": { "clock": "mono", "value": ordinal },
            "physicalContext": {}, "artifactRanges": [],
            "epistemic": {
                "class": "syntheticFixture", "confidence": "exact",
                "alternatives": [], "unknowns": []
            }
        })
    }

    fn transaction(
        record_id: &str,
        ordinal: u64,
        session: &str,
        transaction_id: u32,
    ) -> serde_json::Value {
        let mut value = common(record_id, ordinal);
        let object = value.as_object_mut().unwrap();
        object.insert("kind".into(), serde_json::json!("ptpTransaction"));
        object.insert("transport".into(), serde_json::json!("ptpIp"));
        object.insert("connectionInstance".into(), serde_json::json!("conn-1"));
        object.insert("session".into(), serde_json::json!(session));
        object.insert("endpointSet".into(), serde_json::json!("command"));
        object.insert("transactionId".into(), serde_json::json!(transaction_id));
        object.insert(
            "request".into(),
            serde_json::json!({
                "framing": "standard", "operation": "0x1002", "parameters": [1]
            }),
        );
        object.insert("outcome".into(), serde_json::json!("timeout"));
        value
    }

    fn event(
        record_id: &str,
        ordinal: u64,
        session: &str,
        transaction_id: u32,
        transaction_record_id: Option<&str>,
    ) -> serde_json::Value {
        let mut value = common(record_id, ordinal);
        let object = value.as_object_mut().unwrap();
        object.insert("kind".into(), serde_json::json!("ptpEvent"));
        object.insert("connectionInstance".into(), serde_json::json!("conn-1"));
        object.insert("session".into(), serde_json::json!(session));
        object.insert("endpointSet".into(), serde_json::json!("event"));
        object.insert("transactionId".into(), serde_json::json!(transaction_id));
        if let Some(transaction_record_id) = transaction_record_id {
            object.insert(
                "transactionRecordId".into(),
                serde_json::json!(transaction_record_id),
            );
        }
        object.insert("event".into(), serde_json::json!("0x4002"));
        value
    }

    fn descriptor_property(
        record_id: &str,
        ordinal: u64,
        state: &str,
        values: &[u64],
    ) -> serde_json::Value {
        let mut record = common(record_id, ordinal);
        record["context"]["state"] = serde_json::json!(state);
        let record = record.as_object_mut().unwrap();
        record.insert("kind".into(), serde_json::json!("capability"));
        record.insert(
            "subject".into(),
            serde_json::json!({
                "type": "property",
                "code": "0xd001",
                "supported": true,
                "propertyType": "u16",
                "access": "readWrite",
                "descriptor": { "form": "enum", "values": values }
            }),
        );
        record.insert("evidenceBasis".into(), serde_json::json!("descriptorOnly"));
        record.insert("observedEffect".into(), serde_json::json!("unknown"));
        record.insert(
            "readback".into(),
            serde_json::json!({
                "status": "notObserved",
                "reason": "descriptor enumeration"
            }),
        );
        serde_json::Value::Object(record.clone())
    }

    fn observed_operation(record_id: &str, observed_clock: &str) -> serde_json::Value {
        let mut record = common(record_id, 1);
        let record = record.as_object_mut().unwrap();
        record.insert("kind".into(), serde_json::json!("capability"));
        record.insert(
            "subject".into(),
            serde_json::json!({
                "type": "operation", "code": "0x1014", "supported": true
            }),
        );
        record.insert("evidenceBasis".into(), serde_json::json!("writeProbe"));
        record.insert("observedEffect".into(), serde_json::json!("confirmed"));
        record.insert(
            "readback".into(),
            serde_json::json!({
                "status": "observed",
                "baseline": 0,
                "request": 1,
                "settling": { "deadlineMs": 100 },
                "observed": 1,
                "observedAt": { "clock": observed_clock, "value": 2 },
                "source": "directProperty"
            }),
        );
        serde_json::Value::Object(record.clone())
    }

    fn header_with_wall_clock(include_mapping: bool) -> serde_json::Value {
        let mut header = canonical_header("run-1", "header");
        header["capture"]["clocks"]
            .as_array_mut()
            .unwrap()
            .push(serde_json::json!({
                "id": "wall", "clockType": "wall", "unit": "milliseconds"
            }));
        if include_mapping {
            header["capture"]["clockMappings"]
                .as_array_mut()
                .unwrap()
                .push(serde_json::json!({
                    "from": "mono", "to": "wall", "offset": 0, "uncertainty": 1
                }));
        }
        header
    }

    #[test]
    fn malformed_or_unknown_records_fail_the_complete_bundle() {
        let input = canonical_bundle(&[serde_json::json!({
            "kind": "unknown", "schema": OBSERVATION_SCHEMA_VERSION,
            "runId": "run-1", "recordId": "bad", "ordinal": 1
        })]);
        let report = validate_bundles(&[&input]).unwrap_err();
        assert_eq!(report.total_nonblank, 2);
        assert_eq!(report.rejected, 1);
        assert!(report.dispositions.iter().any(|entry| entry.code == "O003"));
        assert!(propose(&[&input]).is_err());
    }

    #[test]
    fn ptp_events_accept_exact_links_and_explicitly_unmatched_events() {
        let input = canonical_bundle(&[
            transaction("transaction", 1, "session-1", 7),
            event("exact", 2, "session-1", 0, Some("transaction")),
            event("unmatched", 3, "session-1", 99, None),
        ]);

        let validated = validate_bundles(&[&input]).expect("event correlations are canonical");
        assert_eq!(validated.report.rejected, 0);
    }

    #[test]
    fn ptp_event_links_reject_cross_session_correlations() {
        let input = canonical_bundle(&[
            transaction("transaction", 1, "session-1", 7),
            event("cross-session", 2, "session-2", 0, Some("transaction")),
        ]);

        let report = validate_bundles(&[&input]).unwrap_err();
        assert!(report
            .dispositions
            .iter()
            .any(|entry| { entry.identity == "run-1:cross-session" && entry.code == "O121" }));
    }

    #[test]
    fn blank_and_whitespace_bundles_have_the_same_missing_header_rejection() {
        let blank = validate_bundles(&[""]).unwrap_err();
        let whitespace = validate_bundles(&[" \n\t\r\n"]).unwrap_err();
        assert_eq!(blank, whitespace);
        assert_eq!(blank.total_nonblank, 0);
        assert_eq!(blank.accepted, 0);
        assert_eq!(blank.rejected, 1);
        assert_eq!(blank.dispositions.len(), 1);
        assert_eq!(blank.dispositions[0].identity, "bundle:missing-header");
        assert_eq!(blank.dispositions[0].code, "O010");
        assert!(propose(&[""]).is_err());
        assert!(propose(&[" \n\t\r\n"]).is_err());
    }

    #[test]
    fn tagged_observation_line_rejects_an_unknown_field() {
        let mut record = descriptor_property("prop", 1, "ready", &[1, 2]);
        record["unexpected"] = serde_json::json!(true);
        let input = canonical_bundle(&[record]);
        let report = validate_bundles(&[&input]).unwrap_err();
        assert!(report.dispositions.iter().any(|entry| entry.code == "O003"));
    }

    #[test]
    fn tagged_readback_and_capability_subject_reject_unknown_fields() {
        let mut readback = descriptor_property("readback", 1, "ready", &[1, 2]);
        readback["readback"]["unexpected"] = serde_json::json!(true);
        let mut subject = descriptor_property("subject", 1, "ready", &[1, 2]);
        subject["subject"]["unexpected"] = serde_json::json!(true);
        for record in [readback, subject] {
            let input = canonical_bundle(&[record]);
            let report = validate_bundles(&[&input]).unwrap_err();
            assert!(report.dispositions.iter().any(|entry| entry.code == "O003"));
        }
    }

    #[test]
    fn cross_scope_structural_conflicts_are_order_independent_and_not_proposable() {
        let mut first = descriptor_property("prop-a", 1, "ready", &[1, 2]);
        first["runId"] = serde_json::json!("run-a");
        let mut second = descriptor_property("prop-b", 1, "recording", &[1, 3]);
        second["runId"] = serde_json::json!("run-b");
        let first = canonical_bundle_for("run-a", "header-a", &[first]);
        let second = canonical_bundle_for("run-b", "header-b", &[second]);

        let forward_report = validate_bundles(&[&first, &second]).unwrap_err();
        let reversed_report = validate_bundles(&[&second, &first]).unwrap_err();
        assert_eq!(forward_report, reversed_report);
        assert_eq!(forward_report.rejected, 2);
        assert_eq!(
            forward_report
                .dispositions
                .iter()
                .filter(|entry| entry.code == "O401")
                .count(),
            2
        );
        assert!(!forward_report
            .dispositions
            .iter()
            .any(|entry| entry.code == "O400"));
        assert_eq!(propose(&[&first, &second]).unwrap_err(), forward_report);
        assert_eq!(propose(&[&second, &first]).unwrap_err(), reversed_report);
    }

    #[test]
    fn observed_readback_rejects_an_unknown_clock() {
        let input = canonical_bundle(&[observed_operation("op", "unknown")]);
        let report = validate_bundles(&[&input]).unwrap_err();
        assert!(report.dispositions.iter().any(|entry| entry.code == "O031"));
    }

    #[test]
    fn observed_readback_rejects_a_missing_clock_mapping() {
        let input = bundle_with_header(
            header_with_wall_clock(false),
            &[observed_operation("op", "wall")],
        );
        let report = validate_bundles(&[&input]).unwrap_err();
        assert!(report.dispositions.iter().any(|entry| entry.code == "O032"));
    }

    #[test]
    fn observed_readback_accepts_a_declared_mapped_clock() {
        let input = bundle_with_header(
            header_with_wall_clock(true),
            &[observed_operation("op", "wall")],
        );
        let validated = validate_bundles(&[&input]).unwrap();
        assert_eq!(validated.report.rejected, 0);
        assert_eq!(validated.report.accepted, 2);
    }

    #[test]
    fn duplicate_run_rejection_marks_every_header_independent_of_input_order() {
        let first = canonical_bundle_for("duplicate-run", "header-a", &[]);
        let second = canonical_bundle_for("duplicate-run", "header-b", &[]);
        let forward = validate_bundles(&[&first, &second]).unwrap_err();
        let reversed = validate_bundles(&[&second, &first]).unwrap_err();
        assert_eq!(forward, reversed);
        assert_eq!(forward.rejected, 2);
        assert_eq!(
            forward
                .dispositions
                .iter()
                .map(|entry| (entry.identity.as_str(), entry.code.as_str()))
                .collect::<Vec<_>>(),
            vec![
                ("duplicate-run:header-a", "O013"),
                ("duplicate-run:header-b", "O013"),
            ]
        );
    }

    #[test]
    fn proposal_is_identical_when_records_are_reordered() {
        let mut op = common("op", 1);
        let op = op.as_object_mut().unwrap();
        op.insert("kind".into(), serde_json::json!("capability"));
        op.insert(
            "subject".into(),
            serde_json::json!({
                "type": "operation", "code": "0x1014", "supported": true
            }),
        );
        op.insert("evidenceBasis".into(), serde_json::json!("descriptorOnly"));
        op.insert("observedEffect".into(), serde_json::json!("unknown"));
        op.insert(
            "readback".into(),
            serde_json::json!({
                "status": "notObserved", "reason": "descriptor enumeration"
            }),
        );
        let mut property = common("prop", 2);
        let property = property.as_object_mut().unwrap();
        property.insert("kind".into(), serde_json::json!("capability"));
        property.insert(
            "subject".into(),
            serde_json::json!({
                "type": "property", "code": "0x500d", "supported": true,
                "propertyType": "u32", "access": "readWrite",
                "descriptor": { "form": "enum", "values": [1, 2] }
            }),
        );
        property.insert("evidenceBasis".into(), serde_json::json!("descriptorOnly"));
        property.insert("observedEffect".into(), serde_json::json!("unknown"));
        property.insert(
            "readback".into(),
            serde_json::json!({
                "status": "notObserved", "reason": "descriptor enumeration"
            }),
        );
        let op = serde_json::Value::Object(op.clone());
        let property = serde_json::Value::Object(property.clone());
        let first = canonical_bundle(&[op.clone(), property.clone()]);
        let second = canonical_bundle(&[property, op]);
        assert_eq!(
            proposal_json(&propose(&[&first]).unwrap()).unwrap(),
            proposal_json(&propose(&[&second]).unwrap()).unwrap()
        );
    }

    #[test]
    fn review_digest_and_candidate_coverage_are_fail_closed() {
        let input = canonical_bundle(&[]);
        let proposal = propose(&[&input]).unwrap();
        let review = ProposalReview {
            schema: REVIEW_SCHEMA.to_string(),
            proposal_digest: "stale".to_string(),
            decisions: BTreeMap::new(),
        };
        let base = CameraManifest {
            schema: crate::SCHEMA_VERSION.to_string(),
            camera: CameraIdentity {
                manufacturer: "EXAMPLE".to_string(),
                model: "MODEL 1".to_string(),
                firmware: "1.0".to_string(),
                identities: BTreeMap::new(),
            },
            evidence: BTreeMap::new(),
            sentinels: BTreeMap::new(),
            sequence_gates: BTreeMap::new(),
            camera_initiated_transfer: None,
            operations: BTreeMap::new(),
            properties: BTreeMap::new(),
            workflows: BTreeMap::new(),
            media: None,
            focus_grid: None,
            events: BTreeMap::new(),
            quirks: Vec::new(),
            modes: BTreeMap::new(),
            connections: BTreeMap::new(),
            values: BTreeMap::new(),
        };
        assert!(matches!(
            apply_review(&base, &proposal, &review),
            Err(GenerationError::ReviewDigest { .. })
        ));

        let mut tampered = proposal.clone();
        tampered.candidates[0]
            .source_records
            .push("forged:record".into());
        let review = ProposalReview {
            schema: REVIEW_SCHEMA.to_string(),
            proposal_digest: tampered.digest.clone(),
            decisions: tampered
                .candidates
                .iter()
                .map(|candidate| (candidate.id.clone(), ReviewDisposition::Reject))
                .collect(),
        };
        assert!(matches!(
            apply_review(&base, &tampered, &review),
            Err(GenerationError::ProposalIntegrity(_))
        ));
    }
}
