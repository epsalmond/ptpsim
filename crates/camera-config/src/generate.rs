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
    /// Value→label pairs for this property, keyed by stringified raw value. Not
    /// wire-observable (the camera sends integers); carried here so a static
    /// app-catalog source can flow through the same evidence→generator pipeline.
    #[serde(default)]
    labels: Option<BTreeMap<String, String>>,
}

#[derive(Default)]
struct PropAgg {
    ptype: Option<String>,
    access: Option<String>,
    form: Option<String>,
    values: Vec<i64>,
    labels: BTreeMap<String, String>,
    /// Set when a fragment supplied wire-probed structure (type/access/descriptor)
    /// — drives the `activeProbe` citation.
    has_structure: bool,
    /// Set when a fragment supplied labels — drives the `appCatalog` (app-source)
    /// citation, so static labels aren't mis-cited as wire-capture.
    has_labels: bool,
}

const EVIDENCE_ID: &str = "activeProbe";
/// Provenance for labels: a static app/catalog source (e.g. client application's
/// `FujiCameraPropertyCatalog`), distinct from the wire `activeProbe`. Wire
/// labels would outrank this on conflict (see `enrich`'s per-value fill).
const LABELS_EVIDENCE_ID: &str = "appCatalog";

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
                    // Wire-probed structure (any of type/access/descriptor) → the
                    // fragment is probe-sourced; track it for the activeProbe cite.
                    agg.has_structure |=
                        frag.ptype.is_some() || frag.access.is_some() || frag.descriptor.is_some();
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
                    if let Some(labels) = frag.labels {
                        agg.has_labels |= !labels.is_empty();
                        // Fill: accumulate across fragments, first writer wins.
                        for (v, l) in labels {
                            agg.labels.entry(v).or_insert(l);
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
                    name: crate::std_names::standard_operation_name(code)
                        .map(String::from)
                        .unwrap_or_else(|| format!("raw_0x{code:04x}")),
                    owner: String::new(),
                    data_phase: None,
                    params: Vec::new(),
                    workflows: Vec::new(),
                    handler: None,
                    property: None,
                    modes: modes.into_iter().collect(),
                    connections: conns.into_iter().collect(),
                    requires: None,
                    // Op-effects and emitted events are curated sim-behavior,
                    // never probe-derived.
                    effects: Vec::new(),
                    emits: Vec::new(),
                    evidence: vec![EVIDENCE_ID.to_string()],
                },
            )
        })
        .collect();

    let any_labels = props.values().any(|a| a.has_labels);
    let properties = props
        .into_iter()
        .map(|(code, agg)| {
            let descriptor = agg.form.map(|form| Descriptor {
                form,
                values: agg.values,
                // The camera enumerated these (GetDevicePropDesc) → authoritative.
                source: Some(ValueSource::Camera),
            });
            // Cite each source that actually contributed: wire probe for probed
            // structure, app-catalog for static labels. A prop seen only in label
            // evidence (e.g. an unprobed control) cites appCatalog alone — never
            // mis-attributed to wire-capture.
            let mut evidence = Vec::new();
            if agg.has_structure {
                evidence.push(EVIDENCE_ID.to_string());
            }
            if agg.has_labels {
                evidence.push(LABELS_EVIDENCE_ID.to_string());
            }
            if evidence.is_empty() {
                evidence.push(EVIDENCE_ID.to_string());
            }
            (
                format!("0x{code:04x}"),
                Property {
                    name: crate::std_names::standard_property_name(code)
                        .map(String::from)
                        .unwrap_or_else(|| format!("raw_0x{code:04x}")),
                    ptp_name: None,
                    ptype: agg.ptype,
                    access: agg.access,
                    kind: None,
                    descriptor,
                    payload: None,
                    controls: BTreeMap::new(),
                    labels: agg.labels,
                    value_rows: Vec::new(),
                    value_encoding: None,
                    evidence,
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
    if any_labels {
        evidence.insert(
            LABELS_EVIDENCE_ID.to_string(),
            Evidence {
                kind: "app-source".to_string(),
                path: "evidence/labels/".to_string(),
                date: String::new(),
            },
        );
    }

    CameraManifest {
        schema: crate::SCHEMA_VERSION.to_string(),
        camera,
        evidence,
        transports: BTreeMap::new(),
        operations,
        properties,
        workflows: BTreeMap::new(),
        media: None,
        // The AF grid (#135) is curated in the base manifest, merged via --enrich;
        // it is not synthesized from probe evidence.
        focus_grid: None,
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

/// Enrich a curated `base` manifest with a generated `proposal` (from probe evidence).
/// **Curated structure wins**: entries, establishment, connection/mode definitions,
/// operation names + gating, and property names + labels are preserved. The proposal
/// only *adds* — new properties/operations, and it fills a curated property's missing
/// `type`/`access`/`descriptor`. This is the generator→merge→review step: the output
/// is a first-pass for human reconciliation (probe mode-name casing vs curated;
/// connections the probe didn't cover), not a silent overwrite.
pub fn enrich(mut base: CameraManifest, proposal: CameraManifest) -> CameraManifest {
    for (code, pp) in proposal.properties {
        base.properties
            .entry(code)
            .and_modify(|bp| {
                if bp.ptype.is_none() {
                    bp.ptype = pp.ptype.clone();
                }
                if bp.access.is_none() {
                    bp.access = pp.access.clone();
                }
                if bp.descriptor.is_none() {
                    bp.descriptor = pp.descriptor.clone();
                }
                // Labels fill per-value: a curated/wire label for a given value
                // wins (feedback: wire-capture outranks static app-catalog); the
                // proposal only fills values the base doesn't already label.
                let mut filled_label = false;
                for (v, l) in &pp.labels {
                    if !bp.labels.contains_key(v) {
                        bp.labels.insert(v.clone(), l.clone());
                        filled_label = true;
                    }
                }
                // When the proposal supplied labels, record where they came from
                // so the curated property cites its label source (e.g. appCatalog).
                if filled_label {
                    for e in &pp.evidence {
                        if !bp.evidence.contains(e) {
                            bp.evidence.push(e.clone());
                        }
                    }
                }
            })
            .or_insert(pp);
    }
    // Curated operations win entirely (name + gating); the proposal adds the rest.
    for (code, po) in proposal.operations {
        base.operations.entry(code).or_insert(po);
    }
    // Proposal-only connection/mode nodes are added (bare) for review; curated win.
    for (id, c) in proposal.connections {
        base.connections.entry(id).or_insert(c);
    }
    for (path, m) in proposal.modes {
        base.modes.entry(path).or_insert(m);
    }
    for (id, e) in proposal.evidence {
        base.evidence.entry(id).or_insert(e);
    }
    base
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
{"schema":"camera-config-evidence/v1","kind":"property","scope":{"manufacturer":"FUJIFILM","model":"GFX100 II","firmware":"2.30","connection":"usb","mode":"shooting/stills"},"code":"0x5007","supported":true,"type":"u16","access":"readWrite","descriptor":{"form":"enum","values":[280,400,560]},"labels":{"280":"f/2.8","400":"f/4.0"}}
{"schema":"camera-config-evidence/v1","kind":"property","scope":{"manufacturer":"FUJIFILM","model":"GFX100 II","firmware":"2.30","connection":"usb","mode":"shooting/stills"},"code":"0xd240","supported":true,"labels":{"1000":"1/1000"}}
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
        assert_eq!(p.name, "FNumber"); // standard PTP code named from the spec table
        assert_eq!(p.ptype.as_deref(), Some("u16"));
        assert_eq!(p.access.as_deref(), Some("readWrite"));
        let d = p.descriptor.as_ref().unwrap();
        assert_eq!(d.form, "enum");
        assert_eq!(d.values, vec![280, 400, 560]);
        assert_eq!(d.source, Some(ValueSource::Camera));
    }

    #[test]
    fn generate_proposal_emits_labels_with_app_source_provenance() {
        let m = generate_proposal(EVIDENCE);
        let p = &m.properties["0x5007"];
        // Labels from the evidence fragment surface on the property.
        assert_eq!(p.labels.get("280").map(String::as_str), Some("f/2.8"));
        assert_eq!(p.labels.get("400").map(String::as_str), Some("f/4.0"));
        // A labeled property cites BOTH the wire probe and the static catalog,
        // so static labels are not mis-attributed to wire-capture.
        assert!(p.evidence.iter().any(|e| e == "activeProbe"));
        assert!(p.evidence.iter().any(|e| e == "appCatalog"));
        // The app-source provenance entry is registered with the right kind.
        let app = &m.evidence["appCatalog"];
        assert_eq!(app.kind, "app-source");
    }

    #[test]
    fn label_only_property_cites_app_catalog_not_wire_probe() {
        // 0xd240 appears ONLY in label evidence (no probed type/access/descriptor),
        // like ISO/shutter that the camera didn't enumerate. It must NOT be
        // mis-attributed to wire-capture.
        let m = generate_proposal(EVIDENCE);
        let p = &m.properties["0xd240"];
        assert_eq!(p.labels.get("1000").map(String::as_str), Some("1/1000"));
        assert!(p.evidence.iter().any(|e| e == "appCatalog"));
        assert!(
            !p.evidence.iter().any(|e| e == "activeProbe"),
            "an unprobed, label-only property must not cite the wire probe"
        );
    }

    #[test]
    fn enrich_fills_empty_label_keys_without_clobbering_curated() {
        let proposal = generate_proposal(EVIDENCE); // 0x5007 labels: 280→f/2.8, 400→f/4.0
        let curated = CameraManifest::from_yaml(
            r#"
schema: camera-config/v1
camera: { manufacturer: FUJIFILM, model: GFX100 II, firmware: "2.30" }
properties:
  "0x5007": { name: aperture, labels: { 280: "CURATED-2.8" } }
"#,
        )
        .unwrap();
        let m = enrich(curated, proposal);
        let ap = &m.properties["0x5007"];
        // Curated per-value label wins (wire/curated outranks static app-catalog).
        assert_eq!(
            ap.labels.get("280").map(String::as_str),
            Some("CURATED-2.8")
        );
        // The value the base did not label is filled from the proposal.
        assert_eq!(ap.labels.get("400").map(String::as_str), Some("f/4.0"));
    }

    #[test]
    fn enrich_adds_probe_props_but_curated_structure_wins() {
        let proposal = generate_proposal(EVIDENCE);
        let curated = CameraManifest::from_yaml(
            r#"
schema: camera-config/v1
camera: { manufacturer: FUJIFILM, model: GFX100 II, firmware: "2.30" }
operations:
  "0x1014": { name: GetDevicePropDesc, modes: [Shooting/Stills] }
properties:
  "0x5007": { name: aperture, labels: { 280: "f/2.8" } }
connections:
  app: { kind: ptpip-app }
"#,
        )
        .unwrap();
        let m = enrich(curated, proposal);
        // Curated op keeps its name + gating (proposal's raw_0x1014 does NOT overwrite).
        assert_eq!(m.operations["0x1014"].name, "GetDevicePropDesc");
        assert_eq!(
            m.operations["0x1014"].modes,
            vec!["Shooting/Stills".to_string()]
        );
        // Curated property keeps its name/labels but gains the probe's type/descriptor.
        let ap = &m.properties["0x5007"];
        assert_eq!(ap.name, "aperture");
        assert_eq!(ap.labels.get("280").map(String::as_str), Some("f/2.8"));
        assert_eq!(ap.ptype.as_deref(), Some("u16")); // filled from the proposal
        assert!(ap.descriptor.is_some());
        // Curated connection preserved; probe-only connection added for review.
        assert_eq!(m.connections["app"].kind.as_deref(), Some("ptpip-app"));
        assert!(m.connections.contains_key("usb"));
    }

    #[test]
    fn curated_op_effects_survive_enrich() {
        // Op-effects are curated sim-behavior (not probe-derivable). The merge
        // must preserve them: curated operations win entirely, and proposal ops
        // carry no effects.
        let proposal = generate_proposal(EVIDENCE);
        let curated = CameraManifest::from_yaml(
            r#"
schema: camera-config/v1
camera: { manufacturer: FUJIFILM, model: GFX100 II, firmware: "2.30" }
operations:
  "0x9026":
    name: LockS1Lock
    effects:
      - { setProp: "0xd209", value: 1, settleAfterPolls: 2 }
"#,
        )
        .unwrap();
        let m = enrich(curated, proposal);
        let af = &m.operations["0x9026"];
        assert_eq!(af.effects.len(), 1, "curated op-effect preserved");
        assert_eq!(af.effects[0].set_prop, "0xd209");
        assert_eq!(af.effects[0].value, 1);
        assert_eq!(af.effects[0].settle_after_polls, 2);
        // A probe-only operation is added with no effects.
        assert!(m.operations["0x1014"].effects.is_empty());
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
