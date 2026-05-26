//! The manifest data model. This is the reviewed source of truth for one
//! camera's behavior, loaded from YAML. Field naming follows the YAML schema in
//! `DESIGN.md` (camelCase). Most sections default to empty so partial manifests
//! and append-only growth are valid.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// A PTP code written in the manifest as a hex string (e.g. `"0x101b"`).
pub type HexCode = String;

/// Parse a `"0x101b"` style key into a `u16`. Returns `None` for malformed keys.
pub fn parse_hex_code(s: &str) -> Option<u16> {
    let t = s.trim();
    let hex = t.strip_prefix("0x").or_else(|| t.strip_prefix("0X")).unwrap_or(t);
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
