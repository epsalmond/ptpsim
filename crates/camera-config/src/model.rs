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
    /// Canonical human form, e.g. `"2.30"` (matches PTP `GetDeviceInfo`). NB: the
    /// BLE GATT advert reports the zero-padded `"02.30"` for the same camera — a
    /// camera-reported firmware must be normalized via [`crate::version`], never
    /// raw-compared against this field.
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
    /// Camera-side state mutations this operation triggers — the simulator
    /// applies them so a poll-until (`awaitUntil`) flow round-trips (the §5.5 AF
    /// stub: `0x9026 LockS1Lock` → `0xd209 S1_LOCK_COLOR` flips to locked).
    /// Curated sim-behavior data (not probe-derivable). Distinct from
    /// [`ActionEffect`], which is an app-facing declaration the engine does NOT
    /// act on.
    #[serde(default)]
    pub effects: Vec<OpEffect>,
    /// Event codes this operation pushes when it succeeds (#54) — e.g. the AF tap
    /// pushes `0xC005` AFCAPTUER; a capture pushes `0xC004`→`0xC001`→`0x400D`. On
    /// an OK response the engine queues them; the live event socket forwards them
    /// to clients, and the reference executor's event-source `awaitUntil` reads
    /// them.
    ///
    /// Listed separately from [`effects`](Self::effects) because an event is a
    /// signal, not a value change. **Authoring rule:** an effect paired with an
    /// event must settle within one poll (`settle_after_polls` 0 or 1). The event
    /// means "the result is ready", and the one read that follows it is what makes
    /// the value visible — a single-shot event source has no loop to wait out a
    /// longer settle (§11.16).
    ///
    /// Curated sim-behavior; not mirrored to the app FFI (the app sends ops, the
    /// camera emits).
    #[serde(default)]
    pub emits: Vec<HexCode>,
    #[serde(default)]
    pub evidence: Vec<String>,
}

/// A camera-side state mutation an operation produces (consumed by the
/// simulator engine, NOT mirrored to the app FFI — the app sends ops; the
/// camera applies effects). `settle_after_polls` is the deterministic analogue
/// of §5.5's wall-clock AF delay: the new value becomes visible after that many
/// `GetDevicePropValue` polls of `set_prop` (0 = immediate). The reference
/// executor's poll-until loop iterates until the value settles — the PTP
/// analogue of the BLE walker's `serve_read_sequence`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OpEffect {
    /// Property whose value the operation changes.
    pub set_prop: HexCode,
    /// The value it settles to.
    pub value: i64,
    /// Polls of `set_prop` before the new value is visible (0 = immediate).
    #[serde(default)]
    pub settle_after_polls: u32,
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
    /// Composite-payload layout for a byte-array property whose value is a
    /// self-describing record stream of sub-property values (Fuji `0xD212`
    /// live-status). Absent for scalar properties. See [`Payload`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payload: Option<Payload>,
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

/// Layout of a composite byte-array property whose value is a bundle of
/// sub-property records — the Fuji `0xD212` live-status snapshot. The payload
/// is a self-describing **record stream** (not a fixed-offset struct), so
/// members are addressed by PTP prop code, not byte position. A consumer walks
/// records, accepting only `members`; each member's value is interpreted at
/// that property's own `type:` width. Evidence: operators `D212_TIGHT_FORMAT`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Payload {
    pub form: PayloadForm,
    /// Width of the leading element-count prefix, in bytes (`0xD212` → 2).
    #[serde(default)]
    pub count_width: Option<u8>,
    /// Per-record framing: the prop-code field and value field widths.
    #[serde(default)]
    pub record: Option<RecordLayout>,
    /// The prop codes the camera may emit inside this bundle (the poll
    /// allowlist). Each member's value width comes from its own property `type:`.
    #[serde(default)]
    pub members: Vec<HexCode>,
}

/// The framing of a composite payload. Only `recordStream` exists today; the
/// closed enum reserves room for a future fixed-layout bundle without a break.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum PayloadForm {
    RecordStream,
}

/// Per-record field widths in a [`PayloadForm::RecordStream`] (`0xD212`: a
/// 2-byte LE prop code + a 4-byte LE u32-padded value).
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RecordLayout {
    pub code_width: u8,
    pub value_width: u8,
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
    /// Object-format table (#36): PTP object-format code → metadata, so the app
    /// classifies objects (RAW/movie/vendor) from data instead of hardcoding
    /// per-vendor format literals.
    #[serde(default)]
    pub formats: BTreeMap<HexCode, MediaFormat>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, serde_yaml::Value>,
}

/// One object-format catalog row (#36): a PTP/vendor format code's name and
/// classification. The per-vendor RAW preview-extraction descriptor (RAF header
/// offsets, …) is a separate, evidence-gated follow-up.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MediaFormat {
    pub name: String,
    #[serde(default)]
    pub vendor: Option<String>,
    #[serde(default)]
    pub is_raw: bool,
    #[serde(default)]
    pub is_movie: bool,
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
    /// The PTP/IP establishment packet shape for this connection — the
    /// InitCommandRequest byte template (#82). When present, the engine/FFI can
    /// assemble the init bytes from manifest data alone (no client literals).
    #[serde(default)]
    pub init: Option<InitShape>,
    /// Which init/establishment template this connection uses (#81) — names an
    /// init shape (e.g. `app82`) so the app picks the establishment path by
    /// trait instead of branching on connection id. Companion to `init`.
    #[serde(default)]
    pub init_shape: Option<String>,
    /// How live-view frames arrive over this connection (#81): a continuous
    /// `stream` (reference app `app`) or a `poll` loop (`wireless-tether`).
    #[serde(default)]
    pub live_view_delivery: Option<LiveViewDelivery>,
    /// Which shutter recipe family this connection uses (#81) — the discriminator
    /// that replaces the app's per-connection shutter fork. The steps still live
    /// in `actions.shutter`.
    #[serde(default)]
    pub shutter_recipe: Option<ShutterRecipe>,
    /// The command-port (PTP/IP :55740) listener does NOT survive a transport-close
    /// on this connection: the keep-AP sentinel holds the Wi-Fi AP up, but the
    /// camera tears the listener down, so a `reopenSession`'s reconnect is refused
    /// ("Connection refused"). Mode switches must stay in-session (#103). Device-
    /// confirmed on the GFX100 II `app` Wi-Fi-AP path; default false (assume the
    /// listener survives) keeps other connections' reopen behavior unchanged.
    #[serde(default)]
    pub command_listener_volatile: bool,
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
    /// Tap-to-AF: `0x9026 LockS1Lock(packed area)` then await the lock result
    /// (#35). The packed focus-area u32 is an app-supplied runtime slot.
    AutofocusLock,
    /// Release the AF lock: `0x9027 UnlockS1Lock` (#35).
    AutofocusRelease,
}

/// How live-view frames are delivered over a connection (#81 per-connection
/// trait). `Stream` = a continuous frame channel (reference app `app`); `Poll` = the app
/// repeatedly issues `poll_op` (`wireless-tether`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LiveViewDelivery {
    pub kind: LiveViewDeliveryKind,
    /// The op the app polls when `kind = poll` (e.g. `0x9018`).
    #[serde(default)]
    pub poll_op: Option<HexCode>,
}

/// Live-view delivery mode (closed vocabulary — a new value needs a schema PR).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum LiveViewDeliveryKind {
    Stream,
    Poll,
}

/// Which shutter recipe family a connection uses (#81). The actual steps live in
/// `actions.shutter`; this is the discriminator that replaces the app's
/// per-connection shutter branch. Closed vocabulary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ShutterRecipe {
    /// `app`: the bare `0x100E` + `0x9022` postview take-cycle.
    AppPostview,
    /// `wireless-tether`: the 3-beat `0xD039` + `0x100E` virtual shutter.
    WirelessTether3Beat,
}

/// Declared side-effects an `Action` produces — the app reads `Action.triggers`
/// to plan UX (register receive handlers, show progress, etc.) without
/// connection-specific knowledge. Engine does NOT act on this; pure
/// declaration.
///
/// Closed vocabulary: exactly one variant field is set per `ActionEffect`,
/// and unknown fields fail to parse (`deny_unknown_fields`). Same shape as
/// `Step` (one-action-per-mapping) so YAML stays uniform across the
/// manifest. Adding a new effect requires a schema PR (new `Option` field
/// + variant struct).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ActionEffect {
    /// Camera auto-pushes between `min` and `max` captured images to the
    /// tether endpoint after `Shutter`. Cardinality is intrinsically variable:
    /// PCSS tether produces 1-3 per press depending on the user's
    /// JPEG / HEIF / RAW format selection; burst and bracket modes raise
    /// the max further. The app reads `max` as the upper bound for its
    /// receive timeout / progress UI, and may early-exit when it knows
    /// the exact count from its own format-selection state. Receiver
    /// MUST be wired up before invoking the action.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub images_pushed: Option<ImagesPushed>,
    /// Camera emits a postview / capture-complete event after `Shutter`
    /// (reference app `app` path: `0x9022` cleanup once `0xD212` clears). YAML body
    /// is the empty mapping: `postviewEvent: {}`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub postview_event: Option<PostviewEvent>,
    /// Continuous frame delivery starts (e.g. live-view through-stream on the
    /// `app` connection after `0x101C InitiateOpenCapture`). YAML body is
    /// the empty mapping: `liveViewStream: {}`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub live_view_stream: Option<LiveViewStream>,
}

impl ActionEffect {
    /// Whether exactly one variant field is set (structural lint, like
    /// `Step::is_well_formed`).
    pub fn is_well_formed(&self) -> bool {
        let n = [
            self.images_pushed.is_some(),
            self.postview_event.is_some(),
            self.live_view_stream.is_some(),
        ]
        .into_iter()
        .filter(|b| *b)
        .count();
        n == 1
    }
}

/// Parameters for the `ImagesPushed` effect: bounded count of images the
/// camera will spontaneously send after `Shutter`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImagesPushed {
    pub min: u32,
    pub max: u32,
}

/// Marker for the `PostviewEvent` effect (no fields).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PostviewEvent {}

/// Marker for the `LiveViewStream` effect (no fields).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LiveViewStream {}

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
    /// End the PTP/IP session, optionally keeping the Wi-Fi AP up (#82).
    #[serde(default)]
    pub close_session: Option<CloseSession>,
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
    /// Poll `source` until `until` holds over observed property values, running
    /// `on_each` each unsatisfied iteration — the PTP-IP await/poll-until verb
    /// (#29 postview, #42 AF). Mirrors the BLE `bleAwaitUntil` contract (§11.15):
    /// a condition-wait, NOT a bounded loop (that's `repeat`) and NOT a for-each
    /// over a collection (a distinct future construct). See [`AwaitUntil`].
    #[serde(default)]
    pub await_until: Option<AwaitUntil>,
}

/// Where a PTP-IP `awaitUntil` observes (§11.16): a property `poll` or an `event`
/// push. In YAML it's a single-entry mapping — `poll: <hex>` or `event: { code:
/// <hex>, thenPoll: <hex>? }`. Both `Serialize` and `Deserialize` are hand-written
/// to that exact shape: a derived externally-tagged `Serialize` emits a YAML tag
/// (`!event`) that the deserializer can't read, so the generator's `to_yaml →
/// from_yaml` consolidation round-trip would break. (Same shape as the BLE
/// grammar's `read`/`notify` source.)
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AwaitSource {
    /// Poll a property each iteration (`GetDevicePropValue`) — the #49 default.
    /// `until` evaluates over the accumulated [`crate::predicate::PropView`].
    Poll { prop: HexCode },
    /// Await a completion/lifecycle event push (the camera's `0xC0xx` channel),
    /// then do a SINGLE post-event value read of `then_poll` (#54 hybrid). The
    /// event signals the value is ready; one read makes it visible. `then_poll:
    /// None` = event arrival alone satisfies `until` over the existing scope.
    Event {
        code: HexCode,
        then_poll: Option<HexCode>,
    },
}

impl serde::Serialize for AwaitSource {
    /// Mirror the hand-written `Deserialize`: a single-entry mapping keyed by the
    /// variant, NOT serde's externally-tagged `!event` YAML tag (which the
    /// deserializer rejects). Keeps the generator's consolidation round-trip valid.
    fn serialize<S>(&self, s: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        use serde::ser::SerializeMap;
        let mut map = s.serialize_map(Some(1))?;
        match self {
            AwaitSource::Poll { prop } => map.serialize_entry("poll", prop)?,
            AwaitSource::Event { code, then_poll } => {
                #[derive(serde::Serialize)]
                #[serde(rename_all = "camelCase")]
                struct Body<'a> {
                    code: &'a str,
                    #[serde(skip_serializing_if = "Option::is_none")]
                    then_poll: Option<&'a str>,
                }
                map.serialize_entry(
                    "event",
                    &Body {
                        code,
                        then_poll: then_poll.as_deref(),
                    },
                )?;
            }
        }
        map.end()
    }
}

impl<'de> serde::Deserialize<'de> for AwaitSource {
    /// YAML form: a single-entry mapping — `poll: <hex>` (bare string) or
    /// `event: { code: <hex>, thenPoll: <hex>? }`. Mirrors the BLE `AwaitSource`
    /// `read`/`notify` dispatch.
    fn deserialize<D>(d: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        use serde::de::Error;
        let mapping = serde_yaml::Mapping::deserialize(d)?;
        if mapping.len() != 1 {
            return Err(D::Error::custom(format!(
                "awaitUntil source must be a single-entry mapping (got {} keys)",
                mapping.len()
            )));
        }
        let (key_v, body) = mapping.into_iter().next().unwrap();
        let key = key_v
            .as_str()
            .ok_or_else(|| D::Error::custom("awaitUntil source key must be a string"))?
            .to_string();
        match key.as_str() {
            "poll" => {
                let prop = body
                    .as_str()
                    .ok_or_else(|| D::Error::custom("poll: <hex> string required"))?
                    .to_string();
                Ok(AwaitSource::Poll { prop })
            }
            "event" => {
                #[derive(serde::Deserialize)]
                #[serde(rename_all = "camelCase", deny_unknown_fields)]
                struct E {
                    code: String,
                    #[serde(default)]
                    then_poll: Option<String>,
                }
                let e: E = serde_yaml::from_value(body)
                    .map_err(|err| D::Error::custom(format!("event: {err}")))?;
                Ok(AwaitSource::Event {
                    code: e.code,
                    then_poll: e.then_poll,
                })
            }
            other => Err(D::Error::custom(format!(
                "unknown awaitUntil source '{other}' (allowlist: poll, event)"
            ))),
        }
    }
}

/// The PTP-IP await/poll-until step body (§11.16 contract, mirrored from the BLE
/// grammar). [`source`](Self::source) is either a property `poll` or an `event`
/// push (see [`AwaitSource`]). For a poll, each `GetDevicePropValue` is itself the
/// capture: the typed value lands in the observed [`crate::predicate::PropView`]
/// keyed by prop code. `until` is the PTP [`Predicate`] over that view; `mask`
/// handles `0xd212`-style composite sub-fields.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AwaitUntil {
    /// Where to observe: a property `poll` (loop) or an `event` push (single-shot).
    pub source: AwaitSource,
    /// Satisfied when this predicate over observed values holds.
    pub until: Predicate,
    /// Steps run each iteration when `until` is NOT yet satisfied, before the
    /// next poll. Empty = a pure poll.
    #[serde(default)]
    pub on_each: Vec<Step>,
    /// Dispatcher wall-clock budget; the step fails (tolerant-aware) if `until`
    /// isn't met before it elapses. The reference executor models this as a
    /// deterministic iteration cap (the §11.15 analogue).
    pub timeout_ms: u32,
    /// Poll cadence (the dispatcher sleeps between polls). 0 = dispatcher default.
    #[serde(default)]
    pub interval_ms: u32,
}

/// Marker for the `reopen_session` action (empty body in YAML).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReopenSession {}

/// `closeSession` action: end the PTP/IP session. `keepAp: true` emits the
/// 8-byte `0xffffffff` keep-AP sentinel instead of a TCP FIN, so the camera
/// holds its Wi-Fi AP up across an in-place reopen (the graceful-close half of
/// #82).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CloseSession {
    #[serde(default)]
    pub keep_ap: bool,
}

/// The PTP/IP InitCommandRequest wire shape as manifest data: identity slots
/// (resolved via `values:`) framed with a literal vendor tail into the 82-byte
/// reference app init packet (`fuji_init::build_app_init`). #82.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct InitShape {
    /// Named-value refs for the identity slots, resolved via `values:`.
    pub identity: InitIdentity,
    /// Fixed width of the UTF-16LE friendly-name field, in bytes (reference app = 26).
    #[serde(default)]
    pub name_field_byte_count: u32,
    /// Literal vendor tail appended after the name field, as a hex string
    /// (decoded by the same path as `StepValue::Literal`). The GFX app slice's
    /// is the 28-byte `cc004f00…`; optional so a PCSS/zeros-tail shape may omit it.
    #[serde(default)]
    pub tail: Option<String>,
    /// Evidence id(s) backing the tail bytes.
    #[serde(default)]
    pub tail_evidence: Option<String>,
}

/// Named-value refs for the [`InitShape`] identity slots.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct InitIdentity {
    /// `values:` key naming the initiator GUID (e.g. `initiatorGuid`).
    pub guid: String,
    /// `values:` key naming the friendly name (e.g. `initFriendlyName`).
    pub friendly_name: String,
}

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
            self.close_session.is_some(),
            self.await_until.is_some(),
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
    /// Client-derived from a runtime slot the host fills from its own session
    /// state (e.g. the BLE-registered device name). `runtime` names the SAME slot
    /// the establishment plan writes (e.g. `terminalName` → the `deviceNameString`
    /// BLE write), so the PTP/IP friendly name and the BLE device name are one
    /// value by construction — never a literal. The camera silently drops
    /// `InitCommandRequest` if the two channels disagree (device 2026-06-28, #109).
    ClientDerived {
        runtime: String,
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
    fn await_until_step_parses() {
        // The #42 AF poll: tap-to-AF then poll S1_LOCK_COLOR until locked.
        let yaml = r#"
schema: camera-config/v1
camera: { manufacturer: FUJIFILM, model: GFX100 II, firmware: "2.30" }
connections:
  app:
    entries:
      - to: Shooting/Stills
        steps:
          - { sendOp: "0x9026", params: [0x09060403] }
          - awaitUntil:
              source: { poll: "0xd209" }
              until: { prop: "0xd209", eq: 1 }
              timeoutMs: 5000
              intervalMs: 250
              onEach:
                - { getProp: "0xd212", tolerant: true }
"#;
        let m = CameraManifest::from_yaml(yaml).unwrap();
        let steps = &m.connections["app"].entries[0].steps;
        assert_eq!(steps.len(), 2);
        assert_eq!(steps[0].send_op.as_deref(), Some("0x9026"));
        let aw = steps[1].await_until.as_ref().expect("awaitUntil parsed");
        assert_eq!(
            aw.source,
            AwaitSource::Poll {
                prop: "0xd209".into()
            }
        );
        assert_eq!(aw.timeout_ms, 5000);
        assert_eq!(aw.interval_ms, 250);
        assert_eq!(aw.on_each.len(), 1);
        assert_eq!(aw.on_each[0].get_prop.as_deref(), Some("0xd212"));
        // `until` is the PTP predicate over observed values.
        assert!(aw
            .until
            .eval(&crate::predicate::PropView::new().with(0xd209, 1)));
        assert!(!aw
            .until
            .eval(&crate::predicate::PropView::new().with(0xd209, 0)));
        // Exactly-one-action holds for the awaitUntil step too.
        assert!(steps.iter().all(Step::is_well_formed));
    }

    #[test]
    fn await_until_event_source_parses() {
        // #54 hybrid: await the 0xC005 completion push, then one read of 0xd209.
        let yaml = r#"
schema: camera-config/v1
camera: { manufacturer: FUJIFILM, model: GFX100 II, firmware: "2.30" }
connections:
  app:
    entries:
      - to: Shooting/Stills
        steps:
          - { sendOp: "0x9026", params: [0x09060403] }
          - awaitUntil:
              source: { event: { code: "0xc005", thenPoll: "0xd209" } }
              until: { prop: "0xd209", eq: 1 }
              timeoutMs: 5000
"#;
        let m = CameraManifest::from_yaml(yaml).unwrap();
        let aw = m.connections["app"].entries[0].steps[1]
            .await_until
            .as_ref()
            .expect("awaitUntil parsed");
        assert_eq!(
            aw.source,
            AwaitSource::Event {
                code: "0xc005".into(),
                then_poll: Some("0xd209".into()),
            }
        );
        // thenPoll omitted = event arrival alone.
        let bare = CameraManifest::from_yaml(
            r#"
schema: camera-config/v1
camera: { manufacturer: FUJIFILM, model: GFX100 II, firmware: "2.30" }
connections:
  app:
    entries:
      - to: Shooting/Stills
        steps:
          - awaitUntil:
              source: { event: { code: "0xc001" } }
              until: { prop: "0xd400", eq: 1 }
              timeoutMs: 5000
"#,
        )
        .unwrap();
        assert_eq!(
            bare.connections["app"].entries[0].steps[0]
                .await_until
                .as_ref()
                .unwrap()
                .source,
            AwaitSource::Event {
                code: "0xc001".into(),
                then_poll: None,
            }
        );
    }

    #[test]
    fn await_until_source_rejects_unknown_key() {
        // The single-entry-mapping allowlist (poll, event) rejects other keys.
        let yaml = r#"
schema: camera-config/v1
camera: { manufacturer: FUJIFILM, model: GFX100 II, firmware: "2.30" }
connections:
  app:
    entries:
      - to: Shooting/Stills
        steps:
          - awaitUntil:
              source: { notify: "0xd209" }
              until: { prop: "0xd209", eq: 1 }
              timeoutMs: 5000
"#;
        let err = CameraManifest::from_yaml(yaml).unwrap_err().to_string();
        assert!(
            err.contains("unknown awaitUntil source"),
            "expected allowlist error, got: {err}"
        );
    }

    #[test]
    fn await_source_serialize_round_trips_through_yaml() {
        // The generator's consolidation does manifest.to_yaml() → from_yaml(); a
        // derived externally-tagged Serialize emits `!event` which the hand-written
        // Deserialize rejects. Both source forms must survive the round-trip.
        for src in [
            AwaitSource::Event {
                code: "0xc001".into(),
                then_poll: Some("0xd212".into()),
            },
            AwaitSource::Event {
                code: "0xc005".into(),
                then_poll: None,
            },
            AwaitSource::Poll {
                prop: "0xd209".into(),
            },
        ] {
            let yaml = serde_yaml::to_string(&src).expect("serialize");
            assert!(!yaml.contains('!'), "must not emit a YAML tag, got: {yaml}");
            let back: AwaitSource = serde_yaml::from_str(&yaml).expect("deserialize");
            assert_eq!(back, src, "round-trip mismatch via:\n{yaml}");
        }
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
    fn client_derived_value_policy_parses() {
        // #109: a client-derived friendly name names the runtime slot the host fills
        // (the same slot the BLE deviceNameString write uses) — never a literal.
        let yaml = r#"
manufacturer: FUJIFILM
versionOrder: dotted-int
values:
  initFriendlyName: { type: client-derived, runtime: terminalName }
"#;
        let d = ManufacturerDefaults::from_yaml(yaml).unwrap();
        match &d.values["initFriendlyName"] {
            ValuePolicy::ClientDerived { runtime } => assert_eq!(runtime, "terminalName"),
            other => panic!("expected client-derived, got {other:?}"),
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
