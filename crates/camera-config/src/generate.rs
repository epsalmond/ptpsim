//! The **canonical** manifest generator: it consumes a `protocol-mapper`
//! observation bundle (JSONL of `ptpip.fact` records) and emits a reviewable
//! manifest proposal. This lives next to the schema it must agree with, and it
//! serves every bundle source (probe, capture import, app-integrated probe) —
//! not just the Python prober. Unknown semantics stay named `raw_0x…`; nothing
//! is labelled without later human review.

use crate::model::*;
use serde::Deserialize;
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Deserialize)]
struct Fact {
    kind: String,
    #[serde(default)]
    transport: Option<String>,
    #[serde(default)]
    mode: Option<String>,
    subject: Subject,
    #[serde(default)]
    result: FactResult,
}

#[derive(Debug, Deserialize)]
struct Subject {
    kind: String,
    code: String,
    // `op` (the PTP op name on a prop fact) is present in the bundle but not
    // needed for proposal generation; serde ignores unmodelled fields.
}

#[derive(Debug, Default, Deserialize)]
struct FactResult {
    #[serde(default)]
    response: Option<String>,
}

/// Parse a bundle and propose a manifest. `identity` supplies the camera the
/// operator says they probed (the bundle itself is intentionally identity-light
/// after redaction).
pub fn generate_proposal(bundle_jsonl: &str, identity: CameraIdentity) -> CameraManifest {
    // Each op/prop code -> the (transport/mode) contexts it was observed in.
    let mut op_workflows: BTreeMap<u16, BTreeSet<String>> = BTreeMap::new();
    let mut prop_codes: BTreeSet<u16> = BTreeSet::new();

    for line in bundle_jsonl.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let fact: Fact = match serde_json::from_str(line) {
            Ok(f) => f,
            Err(_) => continue, // skip non-fact / malformed lines
        };
        if fact.kind != "ptpip.fact" {
            continue;
        }
        let Some(code) = parse_hex_code(&fact.subject.code) else {
            continue;
        };
        // Only treat OK responses as evidence of support.
        let ok = fact
            .result
            .response
            .as_deref()
            .map(is_ok_response)
            .unwrap_or(true);
        if !ok {
            continue;
        }
        let context = workflow_label(fact.transport.as_deref(), fact.mode.as_deref());
        match fact.subject.kind.as_str() {
            "op" => {
                op_workflows.entry(code).or_default().insert(context);
            }
            "prop" => {
                prop_codes.insert(code);
            }
            _ => {}
        }
    }

    let operations = op_workflows
        .into_iter()
        .map(|(code, ctxs)| {
            (
                format!("0x{code:04x}"),
                Operation {
                    name: format!("raw_0x{code:04x}"),
                    owner: String::new(),
                    data_phase: None,
                    params: Vec::new(),
                    workflows: ctxs.into_iter().collect(),
                    handler: None,
                    property: None,
                    evidence: Vec::new(),
                },
            )
        })
        .collect();

    let properties = prop_codes
        .into_iter()
        .map(|code| {
            (
                format!("0x{code:04x}"),
                Property {
                    name: format!("raw_0x{code:04x}"),
                    ptp_name: None,
                    ptype: None,
                    access: None,
                    descriptor: None,
                    controls: BTreeMap::new(),
                    labels: BTreeMap::new(),
                    evidence: Vec::new(),
                },
            )
        })
        .collect();

    CameraManifest {
        schema: crate::SCHEMA_VERSION.to_string(),
        camera: identity,
        evidence: BTreeMap::new(),
        transports: BTreeMap::new(),
        operations,
        properties,
        workflows: BTreeMap::new(),
        media: None,
        events: BTreeMap::new(),
        quirks: Vec::new(),
    }
}

fn is_ok_response(resp: &str) -> bool {
    parse_hex_code(resp) == Some(0x2001)
}

fn workflow_label(transport: Option<&str>, mode: Option<&str>) -> String {
    match (transport, mode) {
        (Some(t), Some(m)) => format!("{t}/{m}"),
        (Some(t), None) => t.to_string(),
        _ => "unknown".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn proposes_ops_and_props_from_bundle() {
        let bundle = r#"
{"kind":"ptpip.fact","transport":"app","mode":"import","subject":{"kind":"op","code":"0x9054"},"result":{"response":"0x2001"}}
{"kind":"ptpip.fact","transport":"app","mode":"import","subject":{"kind":"prop","code":"0xdf28","op":"GetDevicePropValue"},"result":{"response":"0x2001"}}
{"kind":"ptpip.fact","transport":"app","mode":"liveview","subject":{"kind":"op","code":"0x101c"},"result":{"response":"0x2001"}}
{"kind":"ptpip.fact","transport":"app","mode":"liveview","subject":{"kind":"op","code":"0x9999"},"result":{"response":"0x2005"}}
{"kind":"other","note":"ignored"}
"#;
        let m = generate_proposal(
            bundle,
            CameraIdentity {
                manufacturer: "FUJIFILM".into(),
                model: "GFX100 II".into(),
                firmware: "02.30".into(),
                identities: Default::default(),
            },
        );
        // 0x9054 and 0x101c proposed; 0x9999 dropped (not OK).
        assert!(m.operations.contains_key("0x9054"));
        assert!(m.operations.contains_key("0x101c"));
        assert!(!m.operations.contains_key("0x9999"));
        // Unknown semantics stay raw.
        assert_eq!(m.operations["0x9054"].name, "raw_0x9054");
        assert_eq!(
            m.operations["0x9054"].workflows,
            vec!["app/import".to_string()]
        );
        // Property discovered.
        assert!(m.properties.contains_key("0xdf28"));
    }
}
