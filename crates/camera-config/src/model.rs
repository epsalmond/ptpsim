//! The manifest data model. This is the reviewed source of truth for one
//! camera's behavior, loaded from YAML. Field naming follows the YAML schema in
//! `DESIGN.md` (camelCase). Most sections default to empty so partial manifests
//! and append-only growth are valid.

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

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CameraManifest {
    pub schema: String,
    pub camera: CameraIdentity,
    #[serde(default)]
    pub evidence: BTreeMap<String, Evidence>,
    #[serde(default)]
    pub transports: BTreeMap<String, Transport>,
    #[serde(default)]
    pub operations: BTreeMap<HexCode, Operation>,
    #[serde(default)]
    pub properties: BTreeMap<HexCode, Property>,
    #[serde(default)]
    pub workflows: BTreeMap<String, Workflow>,
    #[serde(default)]
    pub media: Option<Media>,
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
    #[serde(default)]
    pub firmware: String,
    #[serde(default)]
    pub identities: BTreeMap<String, String>,
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

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Transport {
    pub kind: String,
    #[serde(default)]
    pub status: Option<String>,
    /// Free-form bind/port/init detail; structure varies by transport kind.
    #[serde(flatten)]
    pub extra: BTreeMap<String, serde_yaml::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Operation {
    pub name: String,
    #[serde(default)]
    pub owner: String,
    #[serde(default)]
    pub data_phase: Option<String>,
    #[serde(default)]
    pub params: Vec<serde_yaml::Value>,
    #[serde(default)]
    pub workflows: Vec<String>,
    #[serde(default)]
    pub handler: Option<String>,
    #[serde(default)]
    pub property: Option<HexCode>,
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
    #[serde(default)]
    pub evidence: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Property {
    pub name: String,
    #[serde(default)]
    pub ptp_name: Option<String>,
    #[serde(default, rename = "type")]
    pub ptype: Option<String>,
    #[serde(default)]
    pub access: Option<String>,
    #[serde(default)]
    pub descriptor: Option<Descriptor>,
    #[serde(default)]
    pub controls: BTreeMap<String, Control>,
    /// Value -> human label, e.g. `280: "f/2.8"`.
    #[serde(default)]
    pub labels: BTreeMap<String, String>,
    #[serde(default)]
    pub evidence: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Descriptor {
    pub form: String,
    #[serde(default)]
    pub values: Vec<i64>,
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
    #[serde(flatten)]
    pub extra: BTreeMap<String, serde_yaml::Value>,
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
    #[serde(default)]
    pub modes: Vec<String>,
    /// Free-form bind/discovery/entries detail until those are modeled.
    #[serde(flatten)]
    pub extra: BTreeMap<String, serde_yaml::Value>,
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
  "0xRRRR":
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
            values: vec![1, 2],
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
}
