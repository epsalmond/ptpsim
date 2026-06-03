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
    /// Optional classification used by clients to filter what surfaces as a
    /// user setting. `kind: scaffold` marks props that LOOK settable on the
    /// wire but are actually protocol mechanics (keepalives, virtual-shutter
    /// state machines) — clients MUST NOT expose them as user-facing settings.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
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
    /// Mode-graph edges reachable over this connection (decision #6, §3a). An edge
    /// carries a wire-action `steps` sequence OR a `userInstruction`; optionally
    /// `from`-qualified (a cheaper Shooting↔ImageTransfer switch vs a cold entry).
    #[serde(default)]
    pub entries: Vec<ModeEntry>,
    /// Named, parameterized step sequences that run *within* a mode (vs `entries`,
    /// which transition *between* modes). The verb namespace is closed
    /// (`ActionVerb` enum); unknown YAML keys here fail to load — same fail-fast
    /// as the Step verb allowlist. See `docs/plans/action-verbs.md`.
    #[serde(default)]
    pub actions: BTreeMap<ActionVerb, Action>,
    /// Connection-bring-up edges: from this connection, activate *another* (the
    /// BLE→WiFi-AP handover). Distinct from `entries` (mode transitions within a
    /// connection) — this is the establishment edge in the state graph.
    #[serde(default)]
    pub enables: Vec<ConnectionTransition>,
    /// Free-form bind/discovery/establishment detail (e.g. GATT characteristic
    /// UUIDs) until those are modeled / split to a private overlay.
    #[serde(flatten)]
    pub extra: BTreeMap<String, serde_yaml::Value>,
}

/// An establishment edge: from one connection, bring up another. Carries a named
/// `mechanism` (an establishment workflow id, e.g. the GATT credential handover)
/// and/or a `user_instruction` (some handovers are partly manual). NOT a PTP
/// `Step` sequence — establishment is GATT/OS-level, a separate concern from the
/// PTP wire actions in a `ModeEntry`.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ConnectionTransition {
    /// Target connection id this edge brings up.
    pub to: String,
    /// Named establishment mechanism/workflow (resolved elsewhere).
    #[serde(default)]
    pub mechanism: Option<String>,
    #[serde(default)]
    pub user_instruction: Option<String>,
    #[serde(default)]
    pub requires: Option<Predicate>,
}

/// A mode-graph transition edge: how to get *into* mode `to`. `from` qualifies the
/// source (None = cold/any entry; a Shooting→ImageTransfer edge can be cheaper than
/// cold). Carries either a `steps` wire sequence or a `user_instruction` (some
/// transitions — connection switches — can only be requested, not app-driven).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ModeEntry {
    pub to: String,
    #[serde(default)]
    pub from: Option<String>,
    #[serde(default)]
    pub steps: Vec<Step>,
    #[serde(default)]
    pub user_instruction: Option<String>,
    /// Optional runtime prerequisite for taking this edge.
    #[serde(default)]
    pub requires: Option<Predicate>,
}

/// Named verbs the app invokes on a connection while in a mode. Closed
/// vocabulary; new verbs require a schema PR (same fail-fast policy as
/// `Step`). YAML uses camelCase (`shutter`, `getObject`, …).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ActionVerb {
    /// Fire the shutter. Wire bytes are connection-specific (`app` = bare
    /// `0x100E + 0x9022`; `wireless-tether` = 3-beat `0xD039 + 0x100E`).
    Shutter,
    /// Enumerate object handles on the SD card.
    EnumerateObjects,
    /// Read object metadata (PTP ObjectInfo) for a handle.
    GetObjectInfo,
    /// Read the thumbnail JPEG for a handle.
    GetThumb,
    /// Read the whole object (image bytes) for a handle.
    GetObject,
    /// Delete an object by handle.
    DeleteObject,
}

/// Declared side-effects an `Action` produces — the app reads `Action.triggers`
/// to plan UX (register receive handlers, show progress, etc.) without
/// connection-specific knowledge. Engine does NOT act on this; pure
/// declaration. Closed vocabulary; new effects require a schema PR.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ActionEffect {
    /// Camera auto-pushes the captured image to the tether endpoint after
    /// `Shutter` (PCSS-tether). Receiver must be wired up before invoking
    /// the action.
    ImagePushed,
    /// Camera emits a postview / capture-complete event after `Shutter`
    /// (reference app `app` path: `0x9022` cleanup once `0xD212` clears).
    PostviewEvent,
    /// Continuous frame delivery starts (e.g. live-view through-stream on the
    /// `app` connection after `0x101C InitiateOpenCapture`).
    LiveViewStream,
}

/// A parameterized recipe runnable within a mode. Step sequence is the same
/// `Step` grammar as `ModeEntry.steps`; `params` declares runtime slots the
/// caller MUST bind for `StepParam::Runtime` references in `steps` to resolve;
/// `triggers` declares post-conditions the app uses to plan UX.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Action {
    /// Mode this action is valid in (gating; same path-prefix match as
    /// `Operation.modes`).
    pub mode: String,
    /// Runtime slot names the caller binds; each must be referenced by at
    /// least one `StepParam::Runtime { runtime: <slot> }` in `steps`.
    #[serde(default)]
    pub params: Vec<String>,
    /// The wire sequence. Reuses the `Step` vocabulary unchanged.
    #[serde(default)]
    pub steps: Vec<Step>,
    /// Post-conditions the camera produces after this action completes —
    /// the app plans UX around them without connection-specific knowledge.
    #[serde(default)]
    pub triggers: Vec<ActionEffect>,
    #[serde(default)]
    pub evidence: Vec<String>,
}

/// One wire action in a mode-entry sequence. A **closed step vocabulary** (not a
/// script): exactly one action field is set; `value` parameterizes `setProp`;
/// `repeat` (default 1) covers bounded loops like the live-view `902B ×4`. No
/// runtime branches — the day a transition needs "if response X then Y", add a
/// named action here, never a scripting hook.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct Step {
    /// `SetDevicePropValue prop = value` (width from the property's `type`).
    #[serde(default)]
    pub set_prop: Option<HexCode>,
    /// `GetDevicePropValue prop` (discard / negotiate).
    #[serde(default)]
    pub get_prop: Option<HexCode>,
    /// Read `prop`, then write the same value back (the live-view `0xdf2a` echo).
    #[serde(default)]
    pub read_echo: Option<HexCode>,
    /// Send operation `op` (e.g. `0x101c` InitiateOpenCapture).
    #[serde(default)]
    pub send_op: Option<HexCode>,
    /// Re-establish the PTP/IP session in-place (reference app Take↔Get switch on `app`):
    /// CloseSession 0x1003 → 8B `0xffffffff` sentinel → new TCP socket to the
    /// connection's command port → cached 82B InitCmdReq → InitCmdAck →
    /// OpenSession sid=1. Engine reuses the connection's cached identity, so
    /// the action carries no params — `reopenSession: {}`.
    #[serde(default)]
    pub reopen_session: Option<ReopenSession>,
    /// Value for `set_prop`.
    #[serde(default)]
    pub value: Option<i64>,
    /// Operation parameters for `send_op`: literals, or a named runtime slot the
    /// I/O-owning client binds (e.g. the live-view open-capture txid for `0x1018`).
    #[serde(default)]
    pub params: Vec<StepParam>,
    /// If true, a non-OK PTP *response* to this step is acceptable — the client
    /// logs it and continues (advisory setup like `0xdf28`/`0xd226`/`0x9054` that
    /// some bodies/responders reject). Only a *transport* failure aborts.
    #[serde(default)]
    pub tolerant: bool,
    /// Bounded repeat count (default 1).
    #[serde(default = "one")]
    pub repeat: u32,
}

/// Marker for the `reopen_session` action (empty body in YAML).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReopenSession {}

/// A `send_op` parameter: a literal, or a **named runtime slot** the client fills
/// from its own session state. Declarative binding (cf. value-policy `from-pairing`),
/// NOT a computed variable — there is no arithmetic, branching, or looping over it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum StepParam {
    Literal(u32),
    Runtime { runtime: String },
}

fn one() -> u32 {
    1
}

impl Step {
    /// Whether exactly one action field is set (a structural lint, not enforced
    /// at load — keeps loading total).
    pub fn is_well_formed(&self) -> bool {
        let n = [
            self.set_prop.is_some(),
            self.get_prop.is_some(),
            self.read_echo.is_some(),
            self.send_op.is_some(),
            self.reopen_session.is_some(),
        ]
        .into_iter()
        .filter(|b| *b)
        .count();
        n == 1
    }
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
    fn mode_entry_steps_parse() {
        // The ground-truth live-view entry from FujiCameraAPISession.
        let yaml = r#"
schema: camera-config/v1
camera: { manufacturer: FUJIFILM, model: GFX100 II, firmware: "2.30" }
connections:
  app:
    kind: ptpip-app
    entries:
      - to: Shooting/Stills
        steps:
          - { setProp: "0xdf00", value: 6 }
          - { setProp: "0xdf01", value: 0x16 }
          - { readEcho: "0xdf2a" }
          - { sendOp: "0x902b", repeat: 4 }
          - { sendOp: "0x101c" }
      - to: ImageTransfer
        from: Shooting/Stills
        steps:
          - { sendOp: "0x1018" }
          - { setProp: "0xdf01", value: 0x14 }
"#;
        let m = CameraManifest::from_yaml(yaml).unwrap();
        let entries = &m.connections["app"].entries;
        assert_eq!(entries.len(), 2);
        let lv = &entries[0];
        assert_eq!(lv.to, "Shooting/Stills");
        assert!(lv.from.is_none(), "cold entry");
        assert_eq!(lv.steps.len(), 5);
        assert_eq!(lv.steps[0].set_prop.as_deref(), Some("0xdf00"));
        assert_eq!(lv.steps[0].value, Some(6));
        assert_eq!(lv.steps[3].repeat, 4); // 902B ×4
        assert_eq!(lv.steps[4].send_op.as_deref(), Some("0x101c"));
        assert!(lv.steps.iter().all(Step::is_well_formed));
        // from-qualified switch (no full teardown path).
        assert_eq!(entries[1].from.as_deref(), Some("Shooting/Stills"));
    }

    #[test]
    fn step_params_tolerant_and_runtime_slots_parse() {
        let yaml = r#"
schema: camera-config/v1
camera: { manufacturer: FUJIFILM, model: GFX100 II, firmware: "2.30" }
connections:
  app:
    entries:
      - to: ImageTransfer
        steps:
          - { getProp: "0xdf28", tolerant: true }            # read-before-write, advisory
          - { setProp: "0xdf28", value: 3 }                  # uint32 width from the property type
          - { sendOp: "0x9053", params: [0, 0x7530], tolerant: true }   # op with literal params
          - { sendOp: "0x1018", params: [{ runtime: openCaptureTxId }] } # runtime-bound param
"#;
        let m = CameraManifest::from_yaml(yaml).unwrap();
        let steps = &m.connections["app"].entries[0].steps;
        assert!(steps[0].tolerant && steps[0].get_prop.as_deref() == Some("0xdf28"));
        assert_eq!(steps[1].set_prop.as_deref(), Some("0xdf28"));
        assert_eq!(
            steps[2].params,
            vec![StepParam::Literal(0), StepParam::Literal(0x7530)]
        );
        assert_eq!(
            steps[3].params,
            vec![StepParam::Runtime {
                runtime: "openCaptureTxId".into()
            }]
        );
        assert!(steps.iter().all(Step::is_well_formed));
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
