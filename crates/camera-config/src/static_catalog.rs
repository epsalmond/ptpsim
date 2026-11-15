//! Reduced static client facts, kept separate from camera observations.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use crate::model::Evidence;
use crate::{CameraManifest, GenerationError};

pub const STATIC_CANDIDATE_SCHEMA: &str = "camera-static-candidates/v1";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct StaticCandidateDocument {
    pub schema: String,
    pub source: StaticCandidateSource,
    pub manufacturer: String,
    pub model: Option<String>,
    pub candidates: Vec<StaticCandidate>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct StaticCandidateSource {
    pub application: String,
    pub version: String,
    pub plugin: String,
    pub sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct StaticCandidate {
    pub id: String,
    pub kind: StaticCandidateKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
    pub status: StaticCandidateStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_type: Option<String>,
    pub native_names: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub accessor_roles: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub type_hints: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub catalog_name: Option<String>,
    pub disposition: StaticCandidateDisposition,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request: Option<StaticCandidateRequest>,
    pub reason: String,
    pub evidence: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum StaticCandidateKind {
    Property,
    Operation,
    Coverage,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum StaticCandidateStatus {
    Unverified,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum StaticCandidateDisposition {
    AddCatalog,
    NameCandidate,
    PreserveReviewed,
    UnknownModel,
    CoverageOnly,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct StaticCandidateRequest {
    pub opcode: String,
    pub params: Vec<u32>,
    pub data_phase: StaticDataPhase,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payload_hex: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum StaticDataPhase {
    None,
    In,
    Out,
}

impl StaticCandidateDocument {
    pub fn validate(&self) -> Result<(), GenerationError> {
        let reject = |message: &str| GenerationError::ProposalIntegrity(message.into());
        if self.schema != STATIC_CANDIDATE_SCHEMA
            || self.manufacturer.trim().is_empty()
            || self
                .model
                .as_ref()
                .is_some_and(|model| model.trim().is_empty())
            || self.source.application.trim().is_empty()
            || self.source.version.trim().is_empty()
            || self.source.plugin.is_empty()
            || !self
                .source
                .plugin
                .bytes()
                .all(|c| c.is_ascii_alphanumeric())
            || !canonical_hex(&self.source.sha256, 64)
        {
            return Err(reject("invalid static candidate document identity"));
        }
        let mut ids = BTreeSet::new();
        let mut codes = BTreeSet::new();
        for row in &self.candidates {
            if row.id.is_empty()
                || !ids.insert(&row.id)
                || row.reason.trim().is_empty()
                || row.native_names.is_empty()
                || row.evidence.is_empty()
                || row.native_names.iter().any(|name| name.trim().is_empty())
                || row.evidence.iter().any(|item| item.trim().is_empty())
            {
                return Err(reject("invalid or duplicate static candidate row"));
            }
            match (&row.code, row.kind) {
                (Some(code), kind) if valid_code(code) => {
                    if !codes.insert((format!("{kind:?}"), code)) {
                        return Err(reject("duplicate static candidate code"));
                    }
                }
                (None, StaticCandidateKind::Coverage) => {}
                _ => return Err(reject("static candidates require full canonical PTP codes")),
            }
            if self.model.is_none()
                && matches!(
                    row.disposition,
                    StaticCandidateDisposition::AddCatalog
                        | StaticCandidateDisposition::NameCandidate
                )
            {
                return Err(reject(
                    "unknown-model candidates cannot modify a camera catalog",
                ));
            }
            if matches!(
                row.disposition,
                StaticCandidateDisposition::AddCatalog | StaticCandidateDisposition::NameCandidate
            ) && (row.kind == StaticCandidateKind::Coverage
                || (row.disposition == StaticCandidateDisposition::AddCatalog
                    && row.kind != StaticCandidateKind::Property)
                || row.catalog_name.as_ref().is_none_or(|name| name.is_empty()))
            {
                return Err(reject(
                    "catalog changes require an eligible property or operation name",
                ));
            }
            if let Some(request) = &row.request {
                if !valid_code(&request.opcode)
                    || request.params.len() > 5
                    || request.payload_hex.as_ref().is_some_and(|hex| {
                        request.data_phase != StaticDataPhase::Out
                            || !hex.len().is_multiple_of(2)
                            || !canonical_hex(hex, hex.len())
                    })
                {
                    return Err(reject("invalid static request shape"));
                }
            }
        }
        Ok(())
    }
}

fn canonical_hex(value: &str, length: usize) -> bool {
    value.len() == length
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn valid_code(code: &str) -> bool {
    code.strip_prefix("0x")
        .is_some_and(|hex| canonical_hex(hex, 4))
}

pub(crate) fn apply_static_candidate(
    manifest: &mut CameraManifest,
    manufacturer: &str,
    model: &Option<String>,
    source: &StaticCandidateSource,
    row: &StaticCandidate,
) -> Result<(), GenerationError> {
    // Revalidate proposal content, including content supplied without the CLI.
    StaticCandidateDocument {
        schema: STATIC_CANDIDATE_SCHEMA.into(),
        source: source.clone(),
        manufacturer: manufacturer.into(),
        model: model.clone(),
        candidates: vec![row.clone()],
    }
    .validate()?;
    if !matches!(
        row.disposition,
        StaticCandidateDisposition::AddCatalog | StaticCandidateDisposition::NameCandidate
    ) {
        return Ok(());
    }
    if manifest.camera.manufacturer != manufacturer
        || model.as_ref() != Some(&manifest.camera.model)
    {
        return Err(GenerationError::ApplyConflict(
            "static candidate model differs from base manifest".into(),
        ));
    }
    let code = row.code.as_ref().expect("validated catalog code");
    let name = row.catalog_name.as_ref().expect("validated catalog name");
    let evidence_id = format!("staticCandidate{}", source.plugin);
    if row.kind == StaticCandidateKind::Operation {
        if manifest
            .operations
            .iter()
            .any(|(other_code, operation)| other_code != code && operation.name == *name)
        {
            return Err(GenerationError::ApplyConflict(format!(
                "static operation {code} duplicates catalog name {name}"
            )));
        }
        let operation = manifest.operations.get_mut(code).ok_or_else(|| {
            GenerationError::ApplyConflict(format!(
                "static operation name candidate {code} is absent"
            ))
        })?;
        apply_static_name(&mut operation.name, name, code)?;
        if !operation.evidence.contains(&evidence_id) {
            operation.evidence.push(evidence_id.clone());
        }
    } else {
        if manifest
            .properties
            .iter()
            .any(|(other_code, property)| other_code != code && property.name == *name)
        {
            return Err(GenerationError::ApplyConflict(format!(
                "static property {code} duplicates catalog name {name}"
            )));
        }
        if row.disposition == StaticCandidateDisposition::AddCatalog {
            if manifest.properties.contains_key(code) {
                return Err(GenerationError::ApplyConflict(format!(
                    "static addition {code} already exists"
                )));
            }
            let property = serde_json::from_value(serde_json::json!({
                "name": name,
                "kind": "catalogOnly",
                "evidence": [evidence_id],
            }))?;
            manifest.properties.insert(code.clone(), property);
        } else {
            let property = manifest.properties.get_mut(code).ok_or_else(|| {
                GenerationError::ApplyConflict(format!("static name candidate {code} is absent"))
            })?;
            apply_static_name(&mut property.name, name, code)?;
            if !property.evidence.contains(&evidence_id) {
                property.evidence.push(evidence_id.clone());
            }
        }
    }
    manifest.evidence.insert(
        evidence_id,
        Evidence {
            kind: "static-client-unverified".into(),
            path: format!(
                "../catalog/tether-app-{}.candidates.json",
                source.plugin.to_ascii_lowercase()
            ),
            date: String::new(),
        },
    );
    Ok(())
}

fn apply_static_name(current: &mut String, name: &str, code: &str) -> Result<(), GenerationError> {
    if *current != format!("raw_{code}") && *current != name {
        return Err(GenerationError::ApplyConflict(format!(
            "static name candidate {code} conflicts with curated name {current}"
        )));
    }
    *current = name.into();
    Ok(())
}

/// Mark every statically named catalog row in generated YAML, including rows
/// retained by a later observation apply. Comments do not affect execution.
pub fn annotate_static_candidates(manifest: &CameraManifest, yaml: &str) -> String {
    let mut section = "";
    let mut output = String::new();
    for line in yaml.lines() {
        if !line.starts_with(' ') {
            section = match line {
                "properties:" => "properties",
                "operations:" => "operations",
                _ => "",
            };
        }
        if !section.is_empty() && line.starts_with("  '0x") && line.ends_with("':") {
            let code = line.trim().trim_end_matches(':').trim_matches('\'');
            let evidence = if section == "properties" {
                manifest
                    .properties
                    .get(code)
                    .map(|property| &property.evidence)
            } else {
                manifest
                    .operations
                    .get(code)
                    .map(|operation| &operation.evidence)
            };
            if evidence.is_some_and(|evidence| {
                evidence.iter().any(|id| {
                    manifest
                        .evidence
                        .get(id)
                        .is_some_and(|evidence| evidence.kind == "static-client-unverified")
                })
            }) {
                output.push_str("  # unverified: static client identity; validate camera support, type and effects.\n");
            }
        }
        output.push_str(line);
        output.push('\n');
    }
    output
}
