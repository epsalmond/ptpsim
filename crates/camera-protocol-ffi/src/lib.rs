//! `camera-protocol-ffi` — the iOS/macOS (Swift) seam over `camera-config`.
//!
//! Designed `(connection, mode)`-keyed (see `docs/plans/ffi-surface.md`) so that
//! adding wireless-tether/USB to the app is a manifest row + the app's own socket
//! I/O — never a change to this surface. Sans-io: every query is pure over manifest
//! data + observed values the app supplies; nothing here touches a socket/USB/BLE.
//!
//! This is §A (the transport-abstraction query surface). §B (the byte codecs,
//! G1–G3) flows through the same crate and is a parallel workstream.

#![allow(clippy::missing_safety_doc)]

use camera_config as cc;
use cc::parse_hex_code;
use std::collections::BTreeSet;
use std::sync::Arc;

uniffi::setup_scaffolding!();

/// Crate version, exposed so an FFI consumer can assert ABI/build expectations.
#[uniffi::export]
pub fn version() -> String {
    env!("CARGO_PKG_VERSION").to_string()
}

// ----------------------------------------------------------------------------
// Codec functions (§B / G1–G2): pure intents↔bytes. Sans-io — the app writes
// the returned bytes to its own socket/USB.
// ----------------------------------------------------------------------------

#[derive(Debug, thiserror::Error, uniffi::Error)]
pub enum CodecError {
    #[error("{0}")]
    Encode(String),
}

/// Property value width on the wire (mirrors `protocol_primitives::ValueWidth`).
#[derive(Debug, uniffi::Enum)]
pub enum ValueWidth {
    U16,
    U32,
}

impl From<ValueWidth> for protocol_primitives::ValueWidth {
    fn from(w: ValueWidth) -> Self {
        match w {
            ValueWidth::U16 => protocol_primitives::ValueWidth::U16,
            ValueWidth::U32 => protocol_primitives::ValueWidth::U32,
        }
    }
}

/// G1 — build the 82-byte Fuji reference app `InitCommandRequest`. Identity/tail come from
/// the manifest; this frames them.
#[uniffi::export]
pub fn build_app_init(
    guid: Vec<u8>,
    friendly_name: String,
    tail: Vec<u8>,
) -> Result<Vec<u8>, CodecError> {
    protocol_primitives::build_app_init(&guid, &friendly_name, &tail)
        .map_err(|e| CodecError::Encode(e.to_string()))
}

#[uniffi::export]
pub fn validate_init_ack(packet: Vec<u8>) -> Result<(), CodecError> {
    protocol_primitives::validate_init_ack(&packet).map_err(|e| CodecError::Encode(e.to_string()))
}

/// G2 — encode a resolved raw value at its property width (the per-value semantics
/// live in the manifest; this just writes the bytes).
#[uniffi::export]
pub fn encode_value(raw: u32, width: ValueWidth) -> Result<Vec<u8>, CodecError> {
    protocol_primitives::encode_value(raw, width.into())
        .map_err(|e| CodecError::Encode(e.to_string()))
}

// ----------------------------------------------------------------------------
// Errors / enums / records
// ----------------------------------------------------------------------------

#[derive(Debug, thiserror::Error, uniffi::Error)]
pub enum ConfigError {
    #[error("manifest parse error: {0}")]
    Parse(String),
    #[error("unsupported schema: {0}")]
    Schema(String),
}

/// The calling platform — used to hide connections it can't host (USB/tether on iOS).
#[derive(uniffi::Enum)]
pub enum Platform {
    Ios,
    Macos,
    Android,
    Linux,
}

impl Platform {
    fn as_str(&self) -> &'static str {
        match self {
            Platform::Ios => "ios",
            Platform::Macos => "macos",
            Platform::Android => "android",
            Platform::Linux => "linux",
        }
    }
}

/// Availability of an operation across the orthogonal `(connection, mode)` axes
/// plus its runtime prerequisite.
#[derive(Debug, uniffi::Enum)]
pub enum Availability {
    Available,
    WrongMode,
    WrongConnection,
    Blocked,
    Unavailable,
}

impl From<cc::Availability> for Availability {
    fn from(a: cc::Availability) -> Self {
        match a {
            cc::Availability::Available => Availability::Available,
            cc::Availability::WrongMode => Availability::WrongMode,
            cc::Availability::WrongConnection => Availability::WrongConnection,
            cc::Availability::Blocked => Availability::Blocked,
            cc::Availability::Unavailable => Availability::Unavailable,
        }
    }
}

#[derive(uniffi::Record)]
pub struct ConnectionInfo {
    pub id: String,
    pub kind: String,
    pub discovery: String,
    pub auto_discoverable: bool,
}

#[derive(uniffi::Record)]
pub struct ModeInfo {
    pub path: String,
    pub capabilities: Vec<String>,
}

/// An observed property value the app read off the wire (sans-io: the engine never
/// reads it itself).
#[derive(uniffi::Record)]
pub struct PropObservation {
    pub code: u16,
    pub value: i64,
}

#[derive(uniffi::Record)]
pub struct ControlInfo {
    pub set_method: Option<String>,
    pub operation: Option<u16>,
    pub readback: Option<u16>,
}

/// One wire action in a mode-entry sequence (closed vocabulary, no branches).
#[derive(Debug, uniffi::Enum)]
pub enum EntryStep {
    SetProp { prop: u16, value: i64 },
    GetProp { prop: u16 },
    ReadEcho { prop: u16 },
    SendOp { op: u16, repeat: u32 },
}

#[derive(uniffi::Record)]
pub struct ModeEntryPlan {
    pub to: String,
    pub from: Option<String>,
    pub steps: Vec<EntryStep>,
    pub user_instruction: Option<String>,
}

#[derive(uniffi::Record)]
pub struct KeyValue {
    pub key: String,
    pub value: String,
}

/// How to bring a connection up (data only — the app drives the GATT/UDP/TCP I/O).
#[derive(uniffi::Record)]
pub struct EstablishmentPlan {
    pub target_connection: String,
    pub mechanism: Option<String>,
    pub user_instruction: Option<String>,
    pub params: Vec<KeyValue>,
}

#[derive(Debug, uniffi::Enum)]
pub enum ResolvedValue {
    Fixed { value: String },
    Generated { scheme: String, persist: bool },
    FromPairing { source: String },
}

/// Evaluation of one predicate leaf (telemetry / config iteration).
#[derive(Debug, uniffi::Record)]
pub struct LeafEval {
    pub prop: String,
    pub observed: Option<i64>,
    pub effective: Option<i64>,
    pub test: String,
    pub passed: bool,
}

#[derive(Debug, uniffi::Record)]
pub struct PredicateOutcome {
    pub passed: bool,
    pub leaves: Vec<LeafEval>,
    pub summary: String,
}

/// The serializable "why" behind a gating decision — capture into telemetry.
#[derive(Debug, uniffi::Record)]
pub struct ResolutionTrace {
    pub query: String,
    pub connection: String,
    pub mode: String,
    pub op: u16,
    pub outcome: String,
    pub connection_ok: bool,
    pub mode_ok: bool,
    pub requires: Option<PredicateOutcome>,
    pub reason: String,
}

/// An availability decision plus the trace explaining it.
#[derive(Debug, uniffi::Record)]
pub struct GateExplanation {
    pub availability: Availability,
    pub trace: ResolutionTrace,
}

impl From<cc::LeafEval> for LeafEval {
    fn from(l: cc::LeafEval) -> Self {
        LeafEval {
            prop: l.prop,
            observed: l.observed,
            effective: l.effective,
            test: l.test,
            passed: l.passed,
        }
    }
}

impl From<cc::PredicateOutcome> for PredicateOutcome {
    fn from(p: cc::PredicateOutcome) -> Self {
        PredicateOutcome {
            passed: p.passed,
            leaves: p.leaves.into_iter().map(Into::into).collect(),
            summary: p.summary,
        }
    }
}

impl From<cc::ResolutionTrace> for ResolutionTrace {
    fn from(t: cc::ResolutionTrace) -> Self {
        ResolutionTrace {
            query: t.query,
            connection: t.connection,
            mode: t.mode,
            op: t.op,
            outcome: t.outcome,
            connection_ok: t.connection_ok,
            mode_ok: t.mode_ok,
            requires: t.requires.map(Into::into),
            reason: t.reason,
        }
    }
}

// ----------------------------------------------------------------------------
// ConfigStore — the loaded, queryable seam
// ----------------------------------------------------------------------------

#[derive(uniffi::Object)]
pub struct ConfigStore {
    inner: cc::ConfigStore,
}

#[uniffi::export]
impl ConfigStore {
    /// Build from bundled YAML: the body manifest, plus optional manufacturer-tier
    /// defaults (`fuji.yaml`: versionOrder + the fixed initiator identity).
    #[uniffi::constructor]
    pub fn from_bundle(
        body_yaml: String,
        manufacturer_yaml: Option<String>,
    ) -> Result<Arc<Self>, ConfigError> {
        let m = cc::CameraManifest::from_yaml(&body_yaml)
            .map_err(|e| ConfigError::Parse(e.to_string()))?;
        build_store(m, manufacturer_yaml)
    }

    /// Like `from_bundle`, but with firmware-tier overlays deep-merged onto the body
    /// (most-specific last), e.g. `fw_overlays = [fw2.40.yaml]` flips XLV to HTTPS.
    /// Field-level merge — an overlay overrides only the keys it names.
    #[uniffi::constructor]
    pub fn from_tiers(
        body_yaml: String,
        manufacturer_yaml: Option<String>,
        fw_overlays: Vec<String>,
    ) -> Result<Arc<Self>, ConfigError> {
        let refs: Vec<&str> = fw_overlays.iter().map(String::as_str).collect();
        let m = cc::CameraManifest::from_tiers(&body_yaml, &refs)
            .map_err(|e| ConfigError::Parse(e.to_string()))?;
        build_store(m, manufacturer_yaml)
    }

    /// Connections valid on `platform` under the camera's firmware (instax filtered
    /// by `availableWhen`; USB/tether hidden where `platforms:` excludes — all data).
    pub fn connections(&self, platform: Platform) -> Vec<ConnectionInfo> {
        let available: BTreeSet<&str> = self.inner.connections_available().into_iter().collect();
        self.inner
            .manifest
            .connections
            .iter()
            .filter(|(id, c)| available.contains(id.as_str()) && platform_ok(c, &platform))
            .map(|(id, c)| ConnectionInfo {
                id: id.clone(),
                kind: c.kind.clone().unwrap_or_default(),
                discovery: yaml_path_str(&c.extra, &["discovery", "mechanism"]).unwrap_or_default(),
                auto_discoverable: yaml_path_bool(&c.extra, &["discovery", "autoDiscoverable"])
                    .unwrap_or(true),
            })
            .collect()
    }

    /// How to bring `connection` up: its establishment mechanism + params (knock
    /// ports, GATT char uuids) as DATA. Returns `None` for an unknown connection.
    pub fn establishment(&self, connection: String) -> Option<EstablishmentPlan> {
        let c = self.inner.manifest.connections.get(&connection)?;
        let mut params = Vec::new();
        for block in ["knock", "gatt"] {
            if let Some(serde_yaml::Value::Mapping(m)) = c.extra.get(block) {
                for (k, v) in m {
                    if let (Some(k), Some(v)) = (k.as_str(), yaml_scalar(v)) {
                        params.push(KeyValue {
                            key: k.to_string(),
                            value: v,
                        });
                    }
                }
            }
        }
        Some(EstablishmentPlan {
            target_connection: connection,
            mechanism: c.establishment.clone(),
            user_instruction: None,
            params,
        })
    }

    /// Modes reachable over `connection`, with inherited capabilities.
    pub fn modes(&self, connection: String) -> Vec<ModeInfo> {
        let Some(c) = self.inner.manifest.connections.get(&connection) else {
            return Vec::new();
        };
        c.modes
            .iter()
            .map(|path| ModeInfo {
                path: path.clone(),
                capabilities: self.capabilities(connection.clone(), path.clone()),
            })
            .collect()
    }

    pub fn capabilities(&self, _connection: String, mode: String) -> Vec<String> {
        self.inner
            .manifest
            .capabilities(&mode)
            .into_iter()
            .map(String::from)
            .collect()
    }

    /// Which mode the observed props indicate (evaluates `detect` predicates).
    /// `None` → app should present a picker over [`Self::modes`].
    pub fn detect_mode(
        &self,
        _connection: String,
        observed: Vec<PropObservation>,
    ) -> Option<String> {
        self.inner
            .manifest
            .detect_mode(&prop_view(&observed))
            .map(String::from)
    }

    /// The wire-action plan to enter `to` (optionally from a known mode — a cheaper
    /// teardown-free switch). Steps, or a `user_instruction` when not app-driven.
    pub fn mode_entry(
        &self,
        connection: String,
        from: Option<String>,
        to: String,
    ) -> Option<ModeEntryPlan> {
        let c = self.inner.manifest.connections.get(&connection)?;
        let e = c.entries.iter().find(|e| e.to == to && e.from == from)?;
        Some(ModeEntryPlan {
            to: e.to.clone(),
            from: e.from.clone(),
            steps: e.steps.iter().filter_map(map_step).collect(),
            user_instruction: e.user_instruction.clone(),
        })
    }

    /// Is `op` usable over `connection` in `mode` given `observed`? Intersects the
    /// orthogonal axes and evaluates the `requires` prerequisite.
    pub fn operation_available(
        &self,
        connection: String,
        mode: String,
        op: u16,
        observed: Vec<PropObservation>,
    ) -> Availability {
        self.inner
            .manifest
            .operation_available(&connection, &mode, op, &prop_view(&observed))
            .into()
    }

    /// Like `operation_available`, but also returns the trace explaining the
    /// decision (gating checks + the `requires` predicate's leaf evaluations) —
    /// capture into telemetry for fast config iteration.
    pub fn operation_available_explained(
        &self,
        connection: String,
        mode: String,
        op: u16,
        observed: Vec<PropObservation>,
    ) -> GateExplanation {
        let (availability, trace) = self.inner.manifest.operation_available_explained(
            &connection,
            &mode,
            op,
            &prop_view(&observed),
        );
        GateExplanation {
            availability: availability.into(),
            trace: trace.into(),
        }
    }

    /// Intent→mechanism: how to set `prop` over this connection/mode (App vendor-step
    /// vs tether absolute). Tries the connection-keyed control, then the mode-keyed.
    pub fn control_for(&self, connection: String, mode: String, prop: u16) -> Option<ControlInfo> {
        let m = &self.inner.manifest;
        let ctl = m
            .control_for(prop, &connection)
            .or_else(|| m.control_for(prop, &mode))?;
        Some(ControlInfo {
            set_method: ctl.set_method.clone(),
            operation: ctl.operation.as_deref().and_then(parse_hex_code),
            readback: ctl.readback.as_deref().and_then(parse_hex_code),
        })
    }

    /// Value-policy resolution (fixed initiator identity, generated session ids, …),
    /// body overriding manufacturer.
    pub fn value(&self, key: String) -> Option<ResolvedValue> {
        match self.inner.value(&key)? {
            cc::ValuePolicy::Fixed { value } => Some(ResolvedValue::Fixed {
                value: yaml_scalar(value).unwrap_or_default(),
            }),
            cc::ValuePolicy::Generated { scheme, persist } => Some(ResolvedValue::Generated {
                scheme: scheme.clone(),
                persist: *persist,
            }),
            cc::ValuePolicy::FromPairing { source } => Some(ResolvedValue::FromPairing {
                source: source.clone(),
            }),
        }
    }

    pub fn value_label(&self, prop: u16, value: i64) -> Option<String> {
        self.inner
            .manifest
            .value_label(prop, value)
            .map(String::from)
    }
}

// ----------------------------------------------------------------------------
// helpers
// ----------------------------------------------------------------------------

fn build_store(
    m: cc::CameraManifest,
    manufacturer_yaml: Option<String>,
) -> Result<Arc<ConfigStore>, ConfigError> {
    m.require_supported_schema()
        .map_err(|e| ConfigError::Schema(e.to_string()))?;
    let mut store = cc::ConfigStore::new(m);
    if let Some(my) = manufacturer_yaml {
        let d = cc::ManufacturerDefaults::from_yaml(&my)
            .map_err(|e| ConfigError::Parse(e.to_string()))?;
        store = store.with_manufacturer(d);
    }
    Ok(Arc::new(ConfigStore { inner: store }))
}

fn prop_view(observed: &[PropObservation]) -> cc::PropView {
    observed.iter().map(|p| (p.code, p.value)).collect()
}

fn map_step(s: &cc::Step) -> Option<EntryStep> {
    if let Some(p) = &s.set_prop {
        return Some(EntryStep::SetProp {
            prop: parse_hex_code(p)?,
            value: s.value.unwrap_or(0),
        });
    }
    if let Some(p) = &s.get_prop {
        return Some(EntryStep::GetProp {
            prop: parse_hex_code(p)?,
        });
    }
    if let Some(p) = &s.read_echo {
        return Some(EntryStep::ReadEcho {
            prop: parse_hex_code(p)?,
        });
    }
    if let Some(o) = &s.send_op {
        return Some(EntryStep::SendOp {
            op: parse_hex_code(o)?,
            repeat: s.repeat,
        });
    }
    None
}

fn platform_ok(c: &cc::Connection, p: &Platform) -> bool {
    match c.extra.get("platforms") {
        Some(serde_yaml::Value::Sequence(seq)) => {
            seq.iter().any(|v| v.as_str() == Some(p.as_str()))
        }
        _ => true, // no restriction declared
    }
}

/// A scalar YAML value (string/int/bool) rendered to a string; `None` for compound.
fn yaml_scalar(v: &serde_yaml::Value) -> Option<String> {
    match v {
        serde_yaml::Value::String(s) => Some(s.clone()),
        serde_yaml::Value::Number(n) => Some(n.to_string()),
        serde_yaml::Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

fn yaml_path_str(
    extra: &std::collections::BTreeMap<String, serde_yaml::Value>,
    path: &[&str],
) -> Option<String> {
    yaml_path(extra, path).and_then(|v| v.as_str().map(String::from))
}

fn yaml_path_bool(
    extra: &std::collections::BTreeMap<String, serde_yaml::Value>,
    path: &[&str],
) -> Option<bool> {
    yaml_path(extra, path).and_then(|v| v.as_bool())
}

fn yaml_path<'a>(
    extra: &'a std::collections::BTreeMap<String, serde_yaml::Value>,
    path: &[&str],
) -> Option<&'a serde_yaml::Value> {
    let (first, rest) = path.split_first()?;
    let mut cur = extra.get(*first)?;
    for key in rest {
        cur = cur.get(*key)?;
    }
    Some(cur)
}
