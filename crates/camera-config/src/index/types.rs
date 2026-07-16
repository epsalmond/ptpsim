//! Typed data shapes for the manufacturer index (plan §2.2 + §3.3 + §11).
//!
//! Everything here is the **post-resolution** shape — by the time these structs
//! exist, family inheritance has been merged into the model (§11.9), static
//! `{family.path}` refs have been substituted to literal values (§11.1), and
//! GATT symbolic names on Steps have been resolved to UUID strings (§11.3).
//! The loader in [`super::parse`] is what does that work; the typed structs
//! never see template strings or symbolic names.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;

use crate::ConnectionActivityDescriptor;

// ---------------------------------------------------------------------------
// Top-level index
// ---------------------------------------------------------------------------

/// The raw (pre-resolution) shape of `fuji/index.yaml`. Read directly from
/// YAML; family inheritance is applied later by [`super::parse`].
///
/// `families` stay as raw `serde_yaml::Value`s through the merge phase — see
/// the parse-module note on why round-tripping typed Step values through
/// serde_yaml drops the external-tag information.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ManufacturerIndex {
    pub manufacturer: String,
    #[serde(default)]
    pub families: BTreeMap<String, serde_yaml::Value>,
    #[serde(default)]
    pub models: Vec<IndexedModel>,
}

/// One model entry inside a `ManufacturerIndex`. Pre-resolution — `signatures`
/// may still hold `{ble.advert.manufacturerCompanyId}`-style template refs, and
/// `establishment.steps[*].gatt` may still hold symbolic names if the model
/// declares an inline establishment.
///
/// `signatures` is stored as a `Vec<(name, IndexedSignature)>` to preserve
/// file-declaration order (§11.7 precedence contract). A `BTreeMap` would
/// silently re-sort alphabetically.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct IndexedModel {
    pub id: String,
    pub display_name: String,
    /// Family ids this model inherits from (most-specific last; see §11.9).
    #[serde(default)]
    pub inherits: Vec<String>,
    /// Relative path (from the index file) to the body manifest. Used by
    /// callers to know which body-yaml string to feed
    /// [`crate::ConfigStore::from_manufacturer_index`] for this model.
    pub manifest: PathBuf,
    /// Family-baseline model marker (#311). A `fallback: true` model carries
    /// the connection truth every body of the family shares: its signatures
    /// are name-guard-free family shapes, so any unknown body recognizes
    /// through it. Specific models refine the baseline and win by being more
    /// specific — during ranking a non-fallback match suppresses all baseline
    /// matches (see `recognize_ble`, the "most specific wins" rule). Declared
    /// last so file-order precedence keeps specific signatures ahead of it.
    #[serde(default)]
    pub fallback: bool,
    /// Signatures kept as raw YAML values until the template-substitution
    /// pass runs — the typed Signature deserialize comes after that. Stored
    /// as a `Vec<(name, value)>` to preserve file-declaration order (§11.7).
    #[serde(default, deserialize_with = "deserialize_ordered_signatures_raw")]
    pub signatures: Vec<(String, serde_yaml::Value)>,
}

/// Preserve YAML mapping insertion order (file order) for signatures.
/// `BTreeMap` would lose this; see §11.7.
fn deserialize_ordered_signatures_raw<'de, D>(
    d: D,
) -> Result<Vec<(String, serde_yaml::Value)>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de::Error;
    let mapping = serde_yaml::Mapping::deserialize(d)?;
    let mut out = Vec::with_capacity(mapping.len());
    for (k, v) in mapping {
        let name = k
            .as_str()
            .ok_or_else(|| D::Error::custom("signature key must be a string"))?
            .to_string();
        out.push((name, v));
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Families
// ---------------------------------------------------------------------------

/// Family-shared discovery and establishment facts.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct FamilyBlock {
    #[serde(default)]
    pub ble: Option<FamilyBleBlock>,
    #[serde(default)]
    pub pcss: Option<FamilyPcssBlock>,
}

/// PCSS facts available before a body manifest has been selected. The caller
/// supplies the route-specific subnet broadcast address at runtime.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FamilyPcssBlock {
    pub callback_port: u16,
    pub knock_port: u16,
    pub protocol: String,
    pub discovery: PcssDiscoveryPolicy,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PcssDiscoveryPolicy {
    pub retry_interval_ms: u32,
    pub max_attempts: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FamilyBleBlock {
    /// `gattSymbolicName -> UUID string`. Steps reference `gatt: <name>` and
    /// the loader resolves to the UUID at index-build time (§11.3).
    #[serde(default)]
    pub gatt: BTreeMap<String, String>,
    #[serde(default)]
    pub advert: BleAdvertConstants,
    /// Named establishment plans keyed by mechanism (`ble-pair`,
    /// `ble-establish-wifi-ap`, …). A body connection's `establishment:`
    /// mechanism selects one (§11). Resolve via [`Self::establishment`].
    #[serde(default)]
    pub establishments: BTreeMap<String, EstablishmentBlock>,
    /// Saved-camera reconnect policy. Advert signatures opt into this policy
    /// with a wake/ready route; consumers keep scanning for at most this
    /// manifest-authored window before surfacing unavailable guidance.
    #[serde(default)]
    pub reconnect: Option<BleReconnectPolicy>,
    /// Named BLE-native control actions keyed by name (`remote-shutter`,
    /// `write-time`, `write-gps`) — runnable from the resting BLE link without
    /// Wi-Fi (#91). Resolve via [`Self::action`].
    #[serde(default)]
    pub actions: BTreeMap<String, BleActionBlock>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BleReconnectPolicy {
    pub scan_timeout_ms: u32,
}

impl FamilyBleBlock {
    /// The establishment plan registered under `mechanism`, if any.
    pub fn establishment(&self, mechanism: &str) -> Option<&EstablishmentBlock> {
        self.establishments.get(mechanism)
    }

    /// The BLE-native control action registered under `name`, if any.
    pub fn action(&self, name: &str) -> Option<&BleActionBlock> {
        self.actions.get(name)
    }
}

/// Family-wide advert constants that signatures reference via
/// `{ble.advert.…}` template refs (§11.1). Both fields optional — Nikon-style
/// families recognize by service UUID + local name and need neither.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BleAdvertConstants {
    /// Bluetooth-SIG manufacturer company ID this family advertises under
    /// (1240 / 0x04D8 for Fujifilm — note that on Fuji RED-style cameras
    /// the company ID is still the Fujifilm value; the constant marks the
    /// family, not the protocol style).
    #[serde(default)]
    pub manufacturer_company_id: Option<u16>,
    /// Named service UUIDs (`serviceUuids.<name>` in template refs) used by
    /// signatures and steps — e.g. Fuji's `fileTransfer` UUID whose advert
    /// presence classifies the pre-RED "legacy" style.
    #[serde(default)]
    pub service_uuids: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EstablishmentBlock {
    pub mechanism: String,
    /// Mechanism that must complete before this plan runs (e.g.
    /// `ble-establish-wifi-ap` requires `ble-pair`). Advisory sequencing for
    /// the consumer; the reference walker does not enforce it.
    #[serde(default)]
    pub prerequisite: Option<String>,
    /// User-initiated from an already-established BLE link, NOT auto-chained
    /// after `prerequisite` (#91): the consumer rests at a BLE-connected home
    /// and runs this on a user action (e.g. tap "Shoot" → `ble-establish-wifi-ap`).
    #[serde(default)]
    pub on_demand: bool,
    /// Runtime parameter names the consumer binds before walking (e.g.
    /// `launchMode`). Steps reference them via `{ runtime: <name> }`.
    #[serde(default)]
    pub params: Vec<String>,
    /// Captured/runtime slot names whose values the host should persist after
    /// this plan completes, to replay on a later `ble-reconnect` (#91) — e.g.
    /// `ble-pair` persists `pairingKeyBytes`; `ble-establish-wifi-ap` the Wi-Fi
    /// creds. Declarative; the reference walker does not act on it.
    #[serde(default)]
    pub persist: Vec<String>,
    /// Stable semantic progress spans over the two top-level executor
    /// sequences (schema §11.23).
    #[serde(default)]
    pub activities: Vec<ConnectionActivityDescriptor>,
    /// Optional manifest-authored sequence that proves the camera has returned
    /// to a state from which this establishment can be replayed after an orderly
    /// feature exit. Empty means the connection declares no post-exit gate.
    #[serde(default)]
    pub post_exit_readiness: Vec<Step>,
    #[serde(default)]
    pub steps: Vec<Step>,
}

/// A named, runnable BLE-native control action over an already-established link
/// (#91) — `remote-shutter`, `write-time`, `write-gps`. Reuses the establishment
/// [`Step`] vocab unchanged; the host runs it from the resting BLE-connected
/// state without Wi-Fi. Distinct from a PTP/IP `Action` (a different grammar).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BleActionBlock {
    /// Runtime parameter names the host binds before walking (e.g. a host-packed
    /// `utcTimezonePayload`). Steps reference them via `{ runtime: <name> }`.
    #[serde(default)]
    pub params: Vec<String>,
    #[serde(default)]
    pub steps: Vec<Step>,
    /// Evidence ids backing the action's GATT choreography.
    #[serde(default)]
    pub evidence: Vec<String>,
}

// ---------------------------------------------------------------------------
// Step grammar (BLE-only in the MVP)
// ---------------------------------------------------------------------------

/// The BLE step verbs. Authored in YAML as a one-entry mapping whose
/// key names the verb: `- bleConnect: {}` / `- bleRead: { gatt: ..., ... }`.
/// Custom `Deserialize` (see [`super::parse`]) dispatches on the verb key.
/// Verbs not in the allowlist (`usbEnumerate`, `tcpListen`, …) fail with
/// an explicit "unknown step verb" message.
///
/// Serialize side keeps the externally-tagged default — Step values aren't
/// re-emitted into YAML in the MVP, so the round-trip asymmetry is fine.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum Step {
    BleConnect(BleConnectStep),
    BleAwaitDisconnect(BleAwaitDisconnectStep),
    BleRequestMtu(BleRequestMtuStep),
    BleDiscoverServices(BleDiscoverServicesStep),
    BleRead(BleReadStep),
    BleWrite(BleWriteStep),
    BleSubscribe(BleSubscribeStep),
    BleNotify(BleNotifyStep),
    BleAwaitUntil(BleAwaitUntilStep),
    BleWriteChunk(BleWriteChunkStep),
    Acquire(AcquireStep),
    AcquireFirmware(AcquireFirmwareStep),
    If(IfStep),
    Retry(RetryStep),
}

impl Step {
    /// One of the allowlisted verbs (always true for a deserialized `Step`).
    /// Used by validation passes that walk untyped trees.
    pub fn verb_name(&self) -> &'static str {
        match self {
            Step::BleConnect(_) => "bleConnect",
            Step::BleAwaitDisconnect(_) => "bleAwaitDisconnect",
            Step::BleRequestMtu(_) => "bleRequestMtu",
            Step::BleDiscoverServices(_) => "bleDiscoverServices",
            Step::BleRead(_) => "bleRead",
            Step::BleWrite(_) => "bleWrite",
            Step::BleSubscribe(_) => "bleSubscribe",
            Step::BleNotify(_) => "bleNotify",
            Step::BleAwaitUntil(_) => "bleAwaitUntil",
            Step::BleWriteChunk(_) => "bleWriteChunk",
            Step::Acquire(_) => "acquire",
            Step::AcquireFirmware(_) => "acquireFirmware",
            Step::If(_) => "if",
            Step::Retry(_) => "retry",
        }
    }

    /// The shared step-level options (`tolerant`, `retries`, `retryDelayMs`).
    /// Returns the default for `If` (it has its own `tolerant: bool` per
    /// §11.6 with different semantics).
    pub fn options(&self) -> StepOptions {
        match self {
            Step::BleConnect(s) => s.opts.clone(),
            Step::BleAwaitDisconnect(s) => s.opts.clone(),
            Step::BleRequestMtu(s) => s.opts.clone(),
            Step::BleDiscoverServices(s) => s.opts.clone(),
            Step::BleRead(s) => s.opts.clone(),
            Step::BleWrite(s) => s.opts.clone(),
            Step::BleSubscribe(s) => s.opts.clone(),
            Step::BleNotify(s) => s.opts.clone(),
            Step::BleAwaitUntil(s) => s.opts.clone(),
            Step::BleWriteChunk(s) => s.opts.clone(),
            Step::Acquire(s) => s.opts.clone(),
            Step::AcquireFirmware(s) => s.opts.clone(),
            Step::If(_) => StepOptions::default(),
            Step::Retry(_) => StepOptions::default(),
        }
    }
}

/// Per-step retry + tolerance options. Same semantics as the existing
/// `entries[].steps tolerant: true` annotation in `gfx100ii.consolidated.yaml`,
/// extended with `retries` + `retryDelayMs` (plan §11.6).
///
/// The dispatcher's retry loop wraps every verb's body uniformly — adding a
/// new verb in P2 does not change the option-handling code.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct StepOptions {
    pub tolerant: bool,
    pub retries: u32,
    pub retry_delay_ms: u32,
}

/// `bleConnect: {}` — no fields. The peripheral is already in app scope
/// from recognition; the dispatcher's BLE primitive holds the binding
/// (plan §11.4).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct BleConnectStep {
    #[serde(flatten)]
    pub opts: StepOptions,
}

/// `bleAwaitDisconnect: { timeoutMs: 60000 }` — wait for the peer to drop
/// the active BLE link. Used by manifest-authored remote-boot flows where the
/// GATT connect is the wake trigger and the camera disconnects while booting.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BleAwaitDisconnectStep {
    pub timeout_ms: u32,
    #[serde(flatten, default)]
    pub opts: StepOptions,
}

/// `bleRequestMtu: { mtu: 158 }` — ask the link for an ATT MTU before GATT
/// traffic. Sony pairing/Wi-Fi flows request 158. On platforms without an
/// explicit request API (CoreBluetooth negotiates automatically), the
/// dispatcher treats this as a checkpoint: succeed if the negotiated MTU
/// is ≥ `mtu`, else step failure (tolerant-aware as usual).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BleRequestMtuStep {
    pub mtu: u16,
    #[serde(flatten, default)]
    pub opts: StepOptions,
}

/// `bleDiscoverServices: {}` — make GATT service discovery an explicit state
/// transition (Sony/Canon/Nikon apps all gate on it). On platforms whose BLE
/// stack auto-discovers, this is a completion checkpoint, not a re-trigger.
/// Discovery timeout is dispatcher policy.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct BleDiscoverServicesStep {
    #[serde(flatten)]
    pub opts: StepOptions,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BleReadStep {
    /// Resolved GATT characteristic UUID (the loader resolved the symbolic
    /// name authored in YAML to the UUID at index-build time per §11.3).
    pub gatt: String,
    pub encoding: Encoding,
    /// Scope slot that receives the read value, decoded per `encoding`.
    #[serde(alias = "capture_as")]
    pub capture_as: String,
    /// Transform chain applied to the wire bytes BEFORE `encoding` decode
    /// (§11.13 capture pipeline). Empty = decode the raw payload.
    #[serde(default, deserialize_with = "deserialize_one_or_many")]
    pub transform: Vec<Transform>,
    #[serde(flatten, default)]
    pub opts: StepOptions,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BleWriteStep {
    pub gatt: String,
    pub value: StepValue,
    /// Optional subscribed characteristic whose already-buffered notification
    /// prefix the transport atomically fences immediately before issuing this
    /// write. Notifications caused by the write remain consumable.
    #[serde(default)]
    pub notification_fence: Option<String>,
    #[serde(flatten, default)]
    pub opts: StepOptions,
}

/// `bleWriteChunk` — write ONE framed window of a host-supplied blob to `gatt`,
/// selected by a captured chunk index (#112). The upload counterpart of a
/// `bleAwaitUntil` read-loop: each `fileTransactionState` notification announces
/// the next index, and this verb (run from `onEach`) frames + writes that chunk.
///
/// A closed, declarative verb — NOT a scripting hook. The walker owns the slice
/// math AND the frame assembly; the manifest declares only policy (source blob,
/// window size, frame header layout, sentinel index). The blob is supplied once
/// as a `bytes-raw` hex runtime param, like the host-packed write-gps/write-time
/// payloads (#114). Frame: the declared header fields, then the window payload.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BleWriteChunkStep {
    /// Runtime slot holding the whole blob as a hex string (`bytes-raw` shape).
    pub source: String,
    /// Scope key holding the current chunk index (a `bleAwaitUntil` capture of
    /// the notification's index field). Selects which window to frame + write.
    pub index: String,
    /// Window size in bytes. Full windows are `size` bytes; the final window is
    /// the remainder and is labelled `sentinel_index`. Must be > 0 (validated).
    pub size: u32,
    /// Resolved GATT characteristic each framed window is written to
    /// (`filePartialData`). The loader resolves the symbolic name → UUID.
    pub gatt: String,
    /// The DECLARED frame header, emitted before the window payload, in order.
    /// Fuji: `[{index, u16-le}, {length, u32-le}]`. Declaring it as data keeps
    /// the header out of engine code — another family could frame differently
    /// without a code change.
    pub frame: Vec<ChunkFrameField>,
    /// The index the FINAL (remainder) window carries — Fuji `65535` / `0xffff`.
    /// The last window holds real data, not a separate empty frame.
    pub sentinel_index: u32,
    #[serde(flatten, default)]
    pub opts: StepOptions,
}

/// One field of a declared chunk-frame header (#112): a computed quantity the
/// walker supplies (`field`), emitted at the wire width/order `encoding`. Closed
/// set of `field` kinds — a layout declaration, not a formula language.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChunkFrameField {
    pub field: ChunkField,
    pub encoding: Encoding,
}

/// The computed quantities a [`ChunkFrameField`] can emit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ChunkField {
    /// The window's chunk index (the `sentinel_index` value on the final window).
    Index,
    /// The window's payload length in bytes.
    Length,
}

/// `bleSubscribe` — enable notifications (CCCD descriptor write) on a
/// characteristic. Success is signalled by the descriptor-write callback;
/// the step does NOT wait for an actual notification payload to arrive.
///
/// Use this for the CCCD-enable rounds in pair flows where the camera
/// advances its own state on the descriptor-write ack and never emits a
/// notification on the subscribed characteristic. Use [`BleNotifyStep`]
/// instead when the plan needs to wait for and capture a payload.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BleSubscribeStep {
    pub gatt: String,
    /// Cap on how long the descriptor write may take before the dispatcher
    /// gives up. Standard BLE stacks ack in well under 1s; values of
    /// 1000–5000ms are typical.
    pub timeout_ms: u32,
    /// Which CCCD value to write (§11.8). Canon and Nikon enable
    /// indications on some characteristics; defaults to `notify`.
    #[serde(default)]
    pub mode: CccdMode,
    #[serde(flatten, default)]
    pub opts: StepOptions,
}

/// CCCD subscription mode (§11.8): which descriptor value `bleSubscribe` /
/// `bleNotify` writes — `ENABLE_NOTIFICATION_VALUE` or
/// `ENABLE_INDICATION_VALUE` in Android terms.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CccdMode {
    #[default]
    Notify,
    Indicate,
}

/// `bleNotify` — subscribe to a characteristic (CCCD enable) AND wait for
/// the first notification whose payload satisfies `until`. The matching
/// payload is optionally stashed under `capture_as`.
///
/// For pure CCCD-enable (no payload to wait on), use [`BleSubscribeStep`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BleNotifyStep {
    pub gatt: String,
    pub until: BleNotifyUntil,
    /// Optional scope slot that receives the WHOLE matching payload —
    /// preserved for debugging and unknown record layouts even when
    /// `capture` extracts fields.
    #[serde(default, alias = "capture_as")]
    pub capture_as: Option<String>,
    /// Transformed field captures from the matching payload (§11.13
    /// capture pipeline). Sony's Wi-Fi handoff status byte is the
    /// motivating case.
    #[serde(default)]
    pub capture: Vec<NotifyCapture>,
    /// CCCD value this step's subscribe phase writes; defaults to `notify`.
    #[serde(default)]
    pub mode: CccdMode,
    pub timeout_ms: u32,
    #[serde(flatten, default)]
    pub opts: StepOptions,
}

/// One field capture from a notification payload: window, transform chain,
/// decode, bind. Same pipeline as advert/read captures (§11.13). A capture
/// that fails (window out of range, chain failure, decode mismatch) is
/// skipped — it does not fail the step.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NotifyCapture {
    /// Start offset into the matching payload.
    #[serde(default)]
    pub at: usize,
    /// Window length; omitted = to end of payload.
    #[serde(default)]
    pub length: Option<usize>,
    #[serde(default, deserialize_with = "deserialize_one_or_many")]
    pub transform: Vec<Transform>,
    pub encoding: Encoding,
    /// Scope key the decoded value binds to.
    pub name: String,
}

/// `bleAwaitUntil` — the await / poll-until control-flow primitive (§11.15).
/// Observe a characteristic (poll a `read` or consume its `notify` stream)
/// repeatedly until `until` holds over runtime_scope, optionally running
/// `on_each` steps between iterations when it does not yet hold.
///
/// Condition uniformity: each iteration captures into scope (via `capture`
/// /`capture_as`), then evaluates `until` — a [`Predicate`] over scope (the
/// same vocabulary `if:` uses) — so read-source and notify-source share one
/// condition shape. The motivating case is Sony's Wi-Fi handoff V2: observe
/// the launch-status characteristic until launched, writing the launch
/// request each iteration it isn't.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BleAwaitUntilStep {
    pub source: AwaitSource,
    /// Transformed field captures applied to each read value / notification
    /// payload before `until` is checked (§11.13 pipeline). Fail-soft.
    #[serde(default, deserialize_with = "deserialize_one_or_many")]
    pub capture: Vec<NotifyCapture>,
    /// Optional whole-value capture (hex) per iteration, like `bleNotify`.
    #[serde(default, alias = "capture_as")]
    pub capture_as: Option<String>,
    /// Satisfied when this predicate over runtime_scope holds (evaluated
    /// after each iteration's captures land).
    pub until: Predicate,
    /// Optional terminal condition evaluated after `until`. Without
    /// `failure_evidence`, a match fails immediately as `conditionRejected`;
    /// otherwise the evidence probe must confirm it. `until` takes precedence.
    /// Invalid with a seeded notify source because callback transports cannot
    /// reliably distinguish a read response from a racing notification.
    #[serde(default)]
    pub fail_when: Option<Predicate>,
    /// Optional evidence probe for a matching `fail_when`. The probe runs
    /// inside the await budget; rejection becomes terminal only when its
    /// `when` predicate matches the freshly-probed scope.
    #[serde(default)]
    pub failure_evidence: Option<BleAwaitFailureEvidence>,
    /// Steps run each iteration when `until` is NOT yet satisfied, before the
    /// next poll/notification (Sony's launch-request write). Empty = pure
    /// observe.
    #[serde(default)]
    pub on_each: Vec<Step>,
    /// Wall-clock budget; the step fails (tolerant-aware) if `until` isn't met
    /// before it elapses. The reference walker models this as
    /// source-exhaustion + an iteration cap (deterministic analogue).
    pub timeout_ms: u32,
    /// Poll cadence for a `read` source (sleep between reads). Ignored for
    /// `notify` (cadence is the camera's). 0 = dispatcher default.
    #[serde(default)]
    pub interval_ms: u32,
    #[serde(flatten, default)]
    pub opts: StepOptions,
}

/// Additional evidence required before a `bleAwaitUntil.failWhen` match is
/// classified as `conditionRejected`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BleAwaitFailureEvidence {
    /// Steps that acquire the evidence. The `when` field is cleared first so
    /// a tolerated probe failure cannot reuse a stale value.
    pub steps: Vec<Step>,
    /// Terminal only when this predicate holds after `steps` completes.
    pub when: Predicate,
}

/// Stable failure classes a [`RetryStep`] may select without inspecting an
/// implementation-defined error string.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum RetryFailureKind {
    DeadlineExceeded,
    ConditionRejected,
    Other,
}

/// Predicate-gated recovery around a group of steps. Diagnostics run in the
/// same scope after a selected failure and before the retry predicate.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RetryStep {
    pub steps: Vec<Step>,
    pub when_failure: RetryFailureKind,
    #[serde(default)]
    pub on_failure: Vec<Step>,
    pub retry_when: Predicate,
    pub max_attempts: u32,
    #[serde(default)]
    pub retry_delay_ms: u32,
    #[serde(default)]
    pub failure_context: Vec<String>,
}

/// Where `bleAwaitUntil` observes. Authored in YAML as a single-entry mapping
/// (`read: <gatt>` / `notify: { gatt: <gatt>, mode: <notify|indicate>?,
/// seedRead: <bool>? }`);
/// custom Deserialize dispatches on the key (see [`super::parse`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AwaitSource {
    /// Poll a readable characteristic; each iteration is a fresh read.
    Read { gatt: String },
    /// Consume a characteristic's notification stream (CCCD enabled with
    /// `mode`). When `seed_read` is true, subscribe first, issue one read
    /// through the same capture/predicate path, then remain notification-only.
    /// A seeded notify cannot declare `fail_when`.
    Notify {
        gatt: String,
        mode: CccdMode,
        seed_read: bool,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AcquireStep {
    pub name: String,
    /// The acquire delegates its inner work to a nested step (typically
    /// `bleRead`). Boxed because `Step` is an enum that recursively contains
    /// `Step` (also via `If`).
    pub from: Box<Step>,
    #[serde(flatten, default)]
    pub opts: StepOptions,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AcquireFirmwareStep {
    pub from: AcquireSource,
    #[serde(flatten, default)]
    pub opts: StepOptions,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct IfStep {
    pub condition: Predicate,
    #[serde(default)]
    pub then: Vec<Step>,
    /// Defaults to empty — `if:` blocks without an `else:` simply skip past
    /// the conditional when the predicate is false.
    #[serde(default, rename = "else")]
    pub else_branch: Vec<Step>,
    /// §11.6 If's `tolerant: bool`: when `true`, an unbound predicate field
    /// evaluates as `false` (else-branch runs / step is skipped) instead of
    /// erroring. Defaults to `false` (strict).
    #[serde(default)]
    pub tolerant: bool,
}

/// Sources an `acquire`-flavored step can pull from. Today only
/// `AcquireFirmware` uses this; future verbs may extend. Authored in YAML as
/// a one-entry mapping (`{ bleRead: { gatt: ... } }`); custom Deserialize
/// dispatches on the key.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum AcquireSource {
    BleAdvert {
        offset: u32,
        length: u32,
        encoding: Encoding,
    },
    BleRead {
        gatt: String,
        encoding: Encoding,
    },
    UserPrompt {
        text: String,
    },
}

/// Step value forms (plan §11.1). Authored in YAML as a single-entry mapping
/// whose key names the form, with optional siblings (`encoding:`,
/// `transform:`): `{ captured: pairingKeyBytes }`,
/// `{ runtime: terminalName, encoding: utf8 }`,
/// `{ captured: idNumber, transform: { bitOr: 0x20000000 } }`. Custom
/// Deserialize dispatches on the form key. `transform:` accepts a single
/// mapping (1-element chain) or a list (§11.13); empty Vec = no transform.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum StepValue {
    /// Literal bytes baked in at index-build time. Authored as a hex string;
    /// the loader decodes to bytes before this struct is constructed.
    /// Transform-free: anything a chain could do to a literal belongs in the
    /// authored bytes themselves.
    Literal { literal: serde_yaml::Value },
    /// `{ template: "...{name}..." }` — interpolate scope + runtime_params at
    /// walk time. `transform:` post-processes the assembled bytes.
    Template {
        template: String,
        transform: Vec<Transform>,
    },
    /// `{ runtime: <slot>, encoding: <name>? }` — app supplies before walk
    /// (terminal name, host IP, etc.). `transform:` post-processes the
    /// decoded bytes.
    Runtime {
        runtime: String,
        encoding: Option<Encoding>,
        transform: Vec<Transform>,
    },
    /// `{ captured: <name> }` — an earlier step (or recognize-seed) named
    /// this. `transform:` post-processes the captured bytes (the RED
    /// `F557D96B` echo: read 4 bytes, `| 0x20000000`, write back).
    Captured {
        captured: String,
        transform: Vec<Transform>,
    },
}

/// Closed, total byte-buffer → byte-buffer transform vocabulary (plan §11.13).
/// Authored in YAML as a single-entry mapping naming the primitive
/// (`{ bitOr: 0x20000000 }`, `{ slice: { at: 3, length: 1 } }`) or a list of
/// such mappings forming a chain applied in order. Custom Deserialize
/// dispatches on the key.
///
/// Every transform is bytes → bytes so the vocabulary stays closed under
/// composition; integer *decode* lives in the `Encoding` allowlist applied
/// after the chain (§11.2). A transform that cannot apply (out-of-range
/// slice, integer op on > 8 bytes) is a step/capture failure — tolerant-aware,
/// never a panic. Evaluation lives in [`super::eval::apply_transforms`].
///
/// The allowlist stays finite by design — same spirit as the encoding
/// allowlist. No arbitrary expressions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub enum Transform {
    /// Input ≤ 8 bytes, read LE; result re-emitted at input width LE.
    BitOr(u64),
    /// Input ≤ 8 bytes, read LE; result re-emitted at input width LE.
    BitAnd(u64),
    /// Window `[at, at+length)`; `length` omitted = to end. Out-of-range
    /// fails the chain.
    Slice {
        at: usize,
        length: Option<usize>,
    },
    /// Sugar for `Slice { at: n, length: None }`.
    DropPrefix(usize),
    ReverseBytes,
    /// Exactly 16 bytes → the 36 ASCII bytes of the canonical uppercase
    /// 8-4-4-4-12 UUID string (bind with `encoding: ascii`).
    UuidFromBytes,
    /// Input ≤ 8 bytes, read LE: `(value & mask) >> shift`, re-emitted at
    /// input width LE.
    Bits {
        mask: u64,
        shift: u32,
    },
}

/// Encoding allowlist (plan §11.2). Any other token in an `encoding:` field
/// fails schema-validation at load time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Encoding {
    /// UTF-8 text. Round-trip fails on invalid UTF-8 (tolerant-aware).
    Utf8,
    /// UTF-8 text in a fixed-width, NUL-padded field: decode stops at the
    /// first NUL (C-string semantics) so trailing `\0` padding never reaches
    /// scope. Invalid UTF-8 in the live prefix fails the round-trip
    /// (tolerant-aware). Mirrors reference app `Utils.getStringFromByteArray`.
    #[serde(rename = "utf8-cstring")]
    Utf8Cstring,
    /// ASCII text. Round-trip fails on non-ASCII bytes (tolerant-aware).
    Ascii,
    /// Lowercase hex string, no separators. No byte-order semantic.
    #[serde(rename = "bytes")]
    Bytes,
    #[serde(rename = "bytes-raw")]
    BytesRaw,
    #[serde(rename = "bytes-le")]
    BytesLe,
    #[serde(rename = "bytes-be")]
    BytesBe,
    U8,
    /// 4-byte unsigned int, decimal string. Same wire representation
    /// either explicit-endian variant produces, so `encoding: u32` defaults
    /// to LE (the byte-order families ship). Distinct enum so it can be
    /// re-parameterized later without breaking parses.
    U32,
    #[serde(rename = "u16-le")]
    U16Le,
    #[serde(rename = "u16-be")]
    U16Be,
    #[serde(rename = "u32-le")]
    U32Le,
    #[serde(rename = "u32-be")]
    U32Be,
}

impl Encoding {
    /// Inverse of [`Self::as_token`] — resolve an authored token.
    pub fn from_token(s: &str) -> Option<Self> {
        match s {
            "utf8" => Some(Encoding::Utf8),
            "utf8-cstring" => Some(Encoding::Utf8Cstring),
            "ascii" => Some(Encoding::Ascii),
            "bytes" => Some(Encoding::Bytes),
            "bytes-raw" => Some(Encoding::BytesRaw),
            "bytes-le" => Some(Encoding::BytesLe),
            "bytes-be" => Some(Encoding::BytesBe),
            "u8" => Some(Encoding::U8),
            "u32" => Some(Encoding::U32),
            "u16-le" => Some(Encoding::U16Le),
            "u16-be" => Some(Encoding::U16Be),
            "u32-le" => Some(Encoding::U32Le),
            "u32-be" => Some(Encoding::U32Be),
            _ => None,
        }
    }

    /// Identifying token as authored in YAML.
    pub fn as_token(self) -> &'static str {
        match self {
            Encoding::Utf8 => "utf8",
            Encoding::Utf8Cstring => "utf8-cstring",
            Encoding::Ascii => "ascii",
            Encoding::Bytes => "bytes",
            Encoding::BytesRaw => "bytes-raw",
            Encoding::BytesLe => "bytes-le",
            Encoding::BytesBe => "bytes-be",
            Encoding::U8 => "u8",
            Encoding::U32 => "u32",
            Encoding::U16Le => "u16-le",
            Encoding::U16Be => "u16-be",
            Encoding::U32Le => "u32-le",
            Encoding::U32Be => "u32-be",
        }
    }
}

/// `bleNotify` acceptance condition (plan §11.8). Authored in YAML as either:
/// * the bare string `any`,
/// * `{ equals: <value>, encoding: <name>? }`, or
/// * `{ matches: "<regex>" }`.
///
/// Custom Deserialize bridges the YAML shape.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum BleNotifyUntil {
    /// First notification, any payload.
    Any,
    /// Payload byte-equals (or, if authored as `equals` + `encoding`, the
    /// loader decoded to bytes already).
    Equals {
        value: serde_yaml::Value,
        encoding: Option<Encoding>,
    },
    /// Regex match on UTF-8 decoding of payload.
    Matches { pattern: String },
}

// ---------------------------------------------------------------------------
// Predicates
// ---------------------------------------------------------------------------

/// Predicate for `if:` step branching (plan §3.3). Stored in canonical
/// `{ field, op, value }` form internally; the YAML form is the compact
/// `{ field-name: { op: value } }` shape (§2.1) — see [`super::parse`] for
/// the custom Deserialize that bridges them.
///
/// This is intentionally *distinct* from [`crate::predicate::Predicate`]:
/// the existing one compares observed PTP property values (`{prop, eq, mask,
/// ...}`); this one compares runtime_scope keys carried from recognize() and
/// step captures.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Predicate {
    pub field: String,
    pub op: PredicateOp,
    pub value: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PredicateOp {
    Eq,
    Ne,
    Gt,
    Gte,
    Lt,
    Lte,
    In,
}

impl PredicateOp {
    /// Token name as authored in YAML.
    pub fn as_token(self) -> &'static str {
        match self {
            PredicateOp::Eq => "eq",
            PredicateOp::Ne => "ne",
            PredicateOp::Gt => "gt",
            PredicateOp::Gte => "gte",
            PredicateOp::Lt => "lt",
            PredicateOp::Lte => "lte",
            PredicateOp::In => "in",
        }
    }

    pub fn from_token(s: &str) -> Option<Self> {
        match s {
            "eq" => Some(PredicateOp::Eq),
            "ne" => Some(PredicateOp::Ne),
            "gt" => Some(PredicateOp::Gt),
            "gte" => Some(PredicateOp::Gte),
            "lt" => Some(PredicateOp::Lt),
            "lte" => Some(PredicateOp::Lte),
            "in" => Some(PredicateOp::In),
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------------
// Signatures (observation → decision)
// ---------------------------------------------------------------------------

/// Pre-resolution signature shape (template refs may still be present in
/// `require` fields). Distinct from [`Signature`] which is the post-
/// resolution typed shape with literal values.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct IndexedSignature {
    pub kind: SignatureKind,
    /// Raw YAML for the rest of the signature; kept as `Value` until
    /// inheritance + template resolution complete, then deserialized into
    /// the appropriate typed [`Signature`] variant.
    #[serde(flatten)]
    pub body: serde_yaml::Value,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SignatureKind {
    BleAdvert,
    PcssNotify,
}

/// Post-resolution typed signature.
#[derive(Debug, Clone)]
pub enum Signature {
    BleAdvert(BleAdvertSignature),
    PcssNotify(PcssNotifySignature),
}

/// Recognition predicate over an already parsed PCSS callback.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PcssNotifySignature {
    pub require: PcssNotifyPredicate,
    pub suggests: SuggestsBlock,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PcssNotifyPredicate {
    pub camera_name: String,
    pub service: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BleAdvertSignature {
    /// Predicate over the observed advert (§11.14). A signature matches when
    /// its predicate evaluates true.
    pub require: AdvertPredicate,
    /// Field captures bound into runtime_scope on match (§11.13 pipeline).
    /// A failing capture is skipped, never an error.
    #[serde(default)]
    pub capture: Vec<AdvertCapture>,
    /// Literal scope facts injected into runtime_scope on match
    /// (e.g. `style: legacy`). Plan §11.1 stores all scope as strings.
    #[serde(default)]
    pub scope: BTreeMap<String, String>,
    pub suggests: SuggestsBlock,
    /// Whether normal discovery may surface this signature as an Add Camera
    /// candidate. Startup and already-paired advertisements remain available
    /// to `reconnect_decision` without becoming pairing rows.
    #[serde(default = "default_true")]
    pub discoverable: bool,
    /// Optional saved-camera route evaluated with the signature's captured
    /// scope and the host's persisted scope.
    #[serde(default)]
    pub reconnect: Option<ReconnectSuggestion>,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ReconnectDisposition {
    Wake,
    Ready,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReconnectSuggestion {
    pub disposition: ReconnectDisposition,
    /// Establishment registry key (`ble-wake`, `ble-reconnect`, ...).
    pub mechanism: String,
    /// Scope keys that must exist and compare equal in both the observed
    /// signature scope and the persisted saved-camera scope.
    pub identity: Vec<String>,
}

/// Predicate over an observed BLE advert (§11.14). Authored in YAML as a
/// single-entry mapping whose key names the predicate kind or combinator;
/// custom Deserialize (see [`super::parse`]) dispatches on the key.
///
/// **Absent-field rule:** a predicate over a field the advert did not carry
/// (no manufacturer data, no local name, no TX power, empty AD-record list)
/// evaluates **false**, never an error. Mind `not:` over such a predicate —
/// it evaluates true.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum AdvertPredicate {
    /// Every child predicate must hold. Empty list rejected at load.
    All(Vec<AdvertPredicate>),
    /// At least one child predicate must hold. Empty list rejected at load.
    Any(Vec<AdvertPredicate>),
    Not(Box<AdvertPredicate>),
    /// Over the manufacturer-specific AD record. `companyId` optional —
    /// Nikon recognition needs none. Payload constraints are over the
    /// post-company-id payload (§11.14 offset semantics).
    ManufacturerData(MfgDataPredicate),
    /// The advert's service-UUID list contains this UUID
    /// (case-insensitive compare).
    ServiceUuids {
        contains: String,
    },
    /// Constraints over the service-data payload advertised for `uuid`.
    ServiceData {
        uuid: String,
        #[serde(flatten)]
        payload: PayloadPredicate,
    },
    LocalName(LocalNamePredicate),
    /// Advertised TX power within `[min, max]`; at least one bound
    /// required at load.
    TxPower {
        min: Option<i8>,
        max: Option<i8>,
    },
    /// Constraints over a raw AD record's payload exactly as seen on air —
    /// for `ad_type` 0xFF that INCLUDES the 2-byte LE company id (§11.14).
    RawAdRecord {
        ad_type: u8,
        #[serde(flatten)]
        payload: PayloadPredicate,
    },
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MfgDataPredicate {
    /// BT-SIG manufacturer company id; when present the observed company id
    /// must equal it. Optional so vendors recognizable without one (Nikon)
    /// stay expressible — but data SHOULD pin it when the vendor uses
    /// manufacturer data at all (false-positive window otherwise; see #23).
    #[serde(default)]
    pub company_id: Option<u16>,
    #[serde(flatten)]
    pub payload: PayloadPredicate,
}

/// Byte-level constraints over a payload (manufacturer data after the
/// company id, service data, or a raw AD record).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PayloadPredicate {
    /// Exact length, in bytes. Mutually exclusive with `min_length`
    /// (validated at load).
    #[serde(default)]
    pub length: Option<usize>,
    #[serde(default)]
    pub min_length: Option<usize>,
    /// Byte-level equality assertions. Authored either as a single map (the
    /// §2.1 compact form) or as a list.
    #[serde(default, deserialize_with = "deserialize_one_or_many")]
    pub assert_byte: Vec<ByteAssertion>,
    /// Bitfield assertions (Sony feature flags). Authored single-map or list.
    #[serde(default, deserialize_with = "deserialize_one_or_many")]
    pub assert_bits: Vec<BitsAssertion>,
}

impl PayloadPredicate {
    /// True when no constraint is declared (used by load validation to
    /// reject vacuous predicates).
    pub fn is_empty(&self) -> bool {
        self.length.is_none()
            && self.min_length.is_none()
            && self.assert_byte.is_empty()
            && self.assert_bits.is_empty()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ByteAssertion {
    pub index: usize,
    pub equals: u8,
}

/// Read the minimum LE-bytes covering `mask` starting at byte `offset`;
/// predicate is `(value & mask) == equals`. A payload too short for the
/// read evaluates false (absent-field rule), never an error.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BitsAssertion {
    #[serde(default)]
    pub offset: usize,
    pub mask: u64,
    pub equals: u64,
}

/// Local-name string predicate; exactly one of the three forms (validated
/// at load). Plain string ops only — regex evaluation is deliberately not
/// in the engine-side vocabulary.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LocalNamePredicate {
    #[serde(default)]
    pub equals: Option<String>,
    #[serde(default)]
    pub prefix: Option<String>,
    #[serde(default)]
    pub contains: Option<String>,
}

/// One advert field capture: source bytes → window `[at, at+length)` →
/// transform chain → `encoding` decode → runtime_scope under `name`
/// (§11.13 pipeline). A capture that fails anywhere is skipped.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AdvertCapture {
    pub source: AdvertByteSource,
    /// Start offset into the source bytes.
    #[serde(default)]
    pub at: usize,
    /// Window length; omitted = to end of the source bytes.
    #[serde(default)]
    pub length: Option<usize>,
    #[serde(default, deserialize_with = "deserialize_one_or_many")]
    pub transform: Vec<Transform>,
    pub encoding: Encoding,
    /// Scope key the captured value binds to.
    pub name: String,
}

/// Where an advert capture reads its bytes. Authored as a bare string
/// (`manufacturerData`, `localName`) or a single-entry mapping
/// (`{ rawAdRecord: 0x21 }`, `{ serviceData: "<uuid>" }`); custom
/// Deserialize dispatches.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum AdvertByteSource {
    /// The post-company-id manufacturer-data payload (§11.14).
    ManufacturerData,
    /// A raw AD record's payload as seen on air, selected by AD type.
    RawAdRecord { ad_type: u8 },
    /// The service-data payload advertised for this UUID.
    ServiceData { uuid: String },
    /// The UTF-8 bytes of the advertised local name.
    LocalName,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SuggestsBlock {
    /// Connection id this signature suggests (`ble`, `usb`, ...). Free-form
    /// string; not validated against a closed set in P0.
    pub connection: String,
    pub confidence: Confidence,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Confidence {
    High,
    Medium,
    Low,
}

// ---------------------------------------------------------------------------
// Resolved per-model view
// ---------------------------------------------------------------------------

/// The merged + resolved view for one model. This is what `ConfigStore` holds
/// after [`crate::ConfigStore::from_manufacturer_index`] runs inheritance
/// merge + template substitution + GATT-name resolution.
///
/// Queries hit this resolved view — there is no re-walking of inheritance at
/// query time (plan §2.2).
///
/// `signatures` is ordered by file-declaration order (§11.7) so a
/// signature-match caller can iterate top-first to honour precedence.
#[derive(Debug, Clone)]
pub struct ModelView {
    pub id: String,
    pub display_name: String,
    /// True for the family-baseline model (#311). Ranking suppresses baseline
    /// matches whenever a more-specific model also matches.
    pub fallback: bool,
    pub manifest_path: PathBuf,
    /// Merged family + model BLE block, with GATT names already resolved on
    /// every Step's `gatt:` field.
    pub ble: Option<FamilyBleBlock>,
    /// Merged family PCSS discovery policy.
    pub pcss: Option<FamilyPcssBlock>,
    /// Signatures in file-declaration order (top-of-file first), with all
    /// `{family.path}` refs resolved to literals.
    pub signatures: Vec<(String, Signature)>,
}

impl ModelView {
    /// Look up a signature by name. O(n) — fine for MVP (typical model has
    /// ≤ 4 signatures).
    pub fn signature(&self, name: &str) -> Option<&Signature> {
        self.signatures
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, s)| s)
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Accept either a single map (the §2.1 compact form: `assertByte: { index:
/// 0, equals: 0x02 }`) or a list. Normalizes to a Vec.
fn deserialize_one_or_many<'de, D, T>(d: D) -> Result<Vec<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: serde::Deserialize<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum OneOrMany<U> {
        One(U),
        Many(Vec<U>),
    }
    match OneOrMany::<T>::deserialize(d)? {
        OneOrMany::One(v) => Ok(vec![v]),
        OneOrMany::Many(v) => Ok(v),
    }
}
