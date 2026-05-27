//! The **canonical** manifest generator: consumes `camera-config-evidence/v1` JSONL
//! (emitted by protocol-mapper's active probe) and produces a reviewable manifest
//! proposal. Lives next to the schema it must agree with.
//!
//! What it derives from active-probe evidence:
//! - **identity** (manufacturer/model/firmware) from the `identity` fragment;
//! - **operations**, gated by the **observed `(connection, mode)` scopes** — the
//!   orthogonal axes come straight from where each op responded `supported`;
//! - **properties** with `type`/`access`/`descriptor` (camera-enumerated, so the
//!   descriptor's value set is `source: camera`);
//! - the bare **connection/mode nodes** that were probed.
//!
//! What it deliberately does NOT emit: mode-entry **sequences** (`entries`) —
//! preludes, opcode "chords", and ordering are only visible in *wire capture*, not
//! active enumeration, so they stay hand-curated. The generated proposal composes
//! WITH the curated sequences; it never invents them. Unknown semantics stay
//! `raw_0x…`; names/labels/controls are curated downstream.

use crate::model::*;
use serde::Deserialize;
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Clone, Deserialize)]
struct Scope {
    manufacturer: String,
    model: String,
    firmware: String,
    connection: String,
    mode: String,
}

#[derive(Debug, Deserialize)]
struct DescFrag {
    form: String,
    #[serde(default)]
    values: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
struct Frag {
    kind: String,
    #[serde(default)]
    scope: Option<Scope>,
    #[serde(default)]
    code: Option<String>,
    #[serde(default)]
    supported: Option<bool>,
    #[serde(default, rename = "type")]
    ptype: Option<String>,
    #[serde(default)]
    access: Option<String>,
    #[serde(default)]
    descriptor: Option<DescFrag>,
}

#[derive(Default)]
struct PropAgg {
    ptype: Option<String>,
    access: Option<String>,
    form: Option<String>,
    values: Vec<i64>,
}

const EVIDENCE_ID: &str = "activeProbe";

/// Parse one or more concatenated `camera-config-evidence/v1` JSONL files and
/// propose a manifest. Identity is read from the evidence itself (the format
/// carries it), so no identity argument is needed.
pub fn generate_proposal(evidence_jsonl: &str) -> CameraManifest {
    let mut identity: Option<Scope> = None;
    let mut any_scope: Option<Scope> = None;
    let mut op_scopes: BTreeMap<u16, (BTreeSet<String>, BTreeSet<String>)> = BTreeMap::new();
    let mut props: BTreeMap<u16, PropAgg> = BTreeMap::new();
    let mut connections: BTreeSet<String> = BTreeSet::new();
    let mut modes: BTreeSet<String> = BTreeSet::new();

    for line in evidence_jsonl.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let frag: Frag = match serde_json::from_str(line) {
            Ok(f) => f,
            Err(_) => continue, // skip malformed / other-schema lines
        };
        let Some(scope) = frag.scope.clone() else {
            continue;
        };
        connections.insert(scope.connection.clone());
        modes.insert(scope.mode.clone());
        if any_scope.is_none() {
            any_scope = Some(scope.clone());
        }

        match frag.kind.as_str() {
            "identity" => identity = Some(scope),
            "operation" if frag.supported == Some(true) => {
                if let Some(code) = frag.code.as_deref().and_then(parse_hex_code) {
                    let e = op_scopes.entry(code).or_default();
                    e.0.insert(scope.connection);
                    e.1.insert(scope.mode);
                }
            }
            "property" if frag.supported == Some(true) => {
                if let Some(code) = frag.code.as_deref().and_then(parse_hex_code) {
                    let agg = props.entry(code).or_default();
                    if agg.ptype.is_none() {
                        agg.ptype = frag.ptype;
                    }
                    if agg.access.is_none() {
                        agg.access = frag.access;
                    }
                    if let Some(d) = frag.descriptor {
                        if agg.form.is_none() {
                            agg.form = Some(d.form);
                            agg.values = numeric_values(d.values.as_ref());
                        }
                    }
                }
            }
            _ => {}
        }
    }

    let scope = identity.or(any_scope);
    let camera = CameraIdentity {
        manufacturer: scope
            .as_ref()
            .map(|s| s.manufacturer.clone())
            .unwrap_or_default(),
        model: scope.as_ref().map(|s| s.model.clone()).unwrap_or_default(),
        firmware: scope
            .as_ref()
            .map(|s| s.firmware.clone())
            .unwrap_or_default(),
        identities: BTreeMap::new(),
    };

    let operations = op_scopes
        .into_iter()
        .map(|(code, (conns, modes))| {
            (
                format!("0x{code:04x}"),
                Operation {
                    name: format!("raw_0x{code:04x}"),
                    owner: String::new(),
                    data_phase: None,
                    params: Vec::new(),
                    workflows: Vec::new(),
                    handler: None,
                    property: None,
                    modes: modes.into_iter().collect(),
                    connections: conns.into_iter().collect(),
                    requires: None,
                    evidence: vec![EVIDENCE_ID.to_string()],
                },
            )
        })
        .collect();

    let properties = props
        .into_iter()
        .map(|(code, agg)| {
            let descriptor = agg.form.map(|form| Descriptor {
                form,
                values: agg.values,
                // The camera enumerated these (GetDevicePropDesc) → authoritative.
                source: Some(ValueSource::Camera),
            });
            (
                format!("0x{code:04x}"),
                Property {
                    name: format!("raw_0x{code:04x}"),
                    ptp_name: None,
                    ptype: agg.ptype,
                    access: agg.access,
                    descriptor,
                    controls: BTreeMap::new(),
                    labels: BTreeMap::new(),
                    evidence: vec![EVIDENCE_ID.to_string()],
                },
            )
        })
        .collect();

    let mut evidence = BTreeMap::new();
    evidence.insert(
        EVIDENCE_ID.to_string(),
        Evidence {
            kind: "wire-capture".to_string(),
            path: "evidence/probe/".to_string(),
            date: String::new(),
        },
    );

    CameraManifest {
        schema: crate::SCHEMA_VERSION.to_string(),
        camera,
        evidence,
        transports: BTreeMap::new(),
        operations,
        properties,
        workflows: BTreeMap::new(),
        media: None,
        events: BTreeMap::new(),
        quirks: Vec::new(),
        // Bare nodes — existence is observed; establishment + entries (preludes/
        // chords) are wire-discovered and curated, NOT emitted here.
        modes: modes.into_iter().map(|m| (m, Mode::default())).collect(),
        connections: connections
            .into_iter()
            .map(|c| (c, Connection::default()))
            .collect(),
        values: BTreeMap::new(),
    }
}

/// Extract numeric enum/range values; string value sets (e.g. ImageSize) are left
/// for curation (the schema's `values` is integer-typed).
fn numeric_values(v: Option<&serde_json::Value>) -> Vec<i64> {
    match v {
        Some(serde_json::Value::Array(a)) => {
            a.iter().filter_map(serde_json::Value::as_i64).collect()
        }
        _ => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const EVIDENCE: &str = r#"
{"schema":"camera-config-evidence/v1","kind":"identity","scope":{"manufacturer":"FUJIFILM","model":"GFX100 II","firmware":"2.30","connection":"usb","mode":"shooting/stills"},"deviceVersion":"2.30"}
{"schema":"camera-config-evidence/v1","kind":"operation","scope":{"manufacturer":"FUJIFILM","model":"GFX100 II","firmware":"2.30","connection":"usb","mode":"shooting/stills"},"code":"0x1014","supported":true}
{"schema":"camera-config-evidence/v1","kind":"operation","scope":{"manufacturer":"FUJIFILM","model":"GFX100 II","firmware":"2.30","connection":"wireless-tether","mode":"video"},"code":"0x1014","supported":true}
{"schema":"camera-config-evidence/v1","kind":"operation","scope":{"manufacturer":"FUJIFILM","model":"GFX100 II","firmware":"2.30","connection":"usb","mode":"shooting/stills"},"code":"0x9999","supported":false}
{"schema":"camera-config-evidence/v1","kind":"property","scope":{"manufacturer":"FUJIFILM","model":"GFX100 II","firmware":"2.30","connection":"usb","mode":"shooting/stills"},"code":"0x5007","supported":true,"type":"u16","access":"readWrite","descriptor":{"form":"enum","values":[280,400,560]}}
{"kind":"other","note":"ignored"}
"#;

    #[test]
    fn derives_identity_from_evidence() {
        let m = generate_proposal(EVIDENCE);
        assert_eq!(m.camera.model, "GFX100 II");
        assert_eq!(m.camera.firmware, "2.30");
    }

    #[test]
    fn operation_gating_is_the_union_of_observed_scopes() {
        let m = generate_proposal(EVIDENCE);
        let op = &m.operations["0x1014"];
        // Supported over usb/shooting-stills AND wireless-tether/video → both axes union.
        assert_eq!(
            op.connections,
            vec!["usb".to_string(), "wireless-tether".to_string()]
        );
        assert_eq!(
            op.modes,
            vec!["shooting/stills".to_string(), "video".to_string()]
        );
        // Unsupported op dropped.
        assert!(!m.operations.contains_key("0x9999"));
    }

    #[test]
    fn property_carries_type_access_and_camera_sourced_descriptor() {
        let m = generate_proposal(EVIDENCE);
        let p = &m.properties["0x5007"];
        assert_eq!(p.ptype.as_deref(), Some("u16"));
        assert_eq!(p.access.as_deref(), Some("readWrite"));
        let d = p.descriptor.as_ref().unwrap();
        assert_eq!(d.form, "enum");
        assert_eq!(d.values, vec![280, 400, 560]);
        assert_eq!(d.source, Some(ValueSource::Camera));
    }

    #[test]
    fn emits_bare_nodes_but_no_entries_or_establishment() {
        let m = generate_proposal(EVIDENCE);
        // Connection/mode nodes exist (observed), but carry no curated sequences.
        assert!(m.connections.contains_key("usb"));
        assert!(m.connections["usb"].entries.is_empty());
        assert!(m.connections["usb"].establishment.is_none());
        assert!(m.modes.contains_key("shooting/stills"));
    }
}
