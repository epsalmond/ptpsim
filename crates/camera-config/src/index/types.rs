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
/// may still hold `{ble.advert.fujiCompanyId}`-style template refs, and
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

/// Family-shared facts. Today carries only BLE; future families may add USB,
/// PCSS, etc.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct FamilyBlock {
    #[serde(default)]
    pub ble: Option<FamilyBleBlock>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FamilyBleBlock {
    /// `gattSymbolicName -> UUID string`. Steps reference `gatt: <name>` and
    /// the loader resolves to the UUID at index-build time (§11.3).
    #[serde(default)]
    pub gatt: BTreeMap<String, String>,
    pub advert: BleAdvertConstants,
    pub establishment: EstablishmentBlock,
}

/// Family-wide advert detectors used by recognize() to match a BLE advert to
/// this family and classify its style. The field name `fuji_company_id` is the
/// authored YAML key; on RED-style cameras the company ID is still the
/// Fujifilm BT-SIG value (0x04D8) — the "fuji" in the field name marks the
/// family, not the protocol.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BleAdvertConstants {
    /// Bluetooth-SIG manufacturer company ID. Used as the lookup key into the
    /// advertiser's manufacturer-specific data map (1240 / 0x04D8 for
    /// Fujifilm).
    pub fuji_company_id: u16,
    /// Service UUID whose presence in an advert classifies it as the
    /// pre-RED "legacy" style. Optional — some families don't need a
    /// per-style detector.
    #[serde(default)]
    pub legacy_service_uuid: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EstablishmentBlock {
    pub mechanism: String,
    #[serde(default)]
    pub steps: Vec<Step>,
}

// ---------------------------------------------------------------------------
// Step grammar (BLE-only in the MVP)
// ---------------------------------------------------------------------------

/// The seven MVP step verbs. Authored in YAML as a one-entry mapping whose
/// key names the verb: `- bleConnect: {}` / `- bleRead: { gatt: ..., ... }`.
/// Custom `Deserialize` (see [`super::parse`]) dispatches on the verb key.
/// Verbs not in the MVP allowlist (`usbEnumerate`, `tcpListen`, …) fail with
/// an explicit "unknown step verb" message.
///
/// Serialize side keeps the externally-tagged default — Step values aren't
/// re-emitted into YAML in the MVP, so the round-trip asymmetry is fine.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum Step {
    BleConnect(BleConnectStep),
    BleRead(BleReadStep),
    BleWrite(BleWriteStep),
    BleSubscribe(BleSubscribeStep),
    BleNotify(BleNotifyStep),
    Acquire(AcquireStep),
    AcquireFirmware(AcquireFirmwareStep),
    If(IfStep),
}

impl Step {
    /// One of the MVP verbs (always true for a deserialized `Step`).
    /// Used by validation passes that walk untyped trees.
    pub fn verb_name(&self) -> &'static str {
        match self {
            Step::BleConnect(_) => "bleConnect",
            Step::BleRead(_) => "bleRead",
            Step::BleWrite(_) => "bleWrite",
            Step::BleSubscribe(_) => "bleSubscribe",
            Step::BleNotify(_) => "bleNotify",
            Step::Acquire(_) => "acquire",
            Step::AcquireFirmware(_) => "acquireFirmware",
            Step::If(_) => "if",
        }
    }

    /// The shared step-level options (`tolerant`, `retries`, `retryDelayMs`).
    /// Returns the default for `If` (it has its own `tolerant: bool` per
    /// §11.6 with different semantics).
    pub fn options(&self) -> StepOptions {
        match self {
            Step::BleConnect(s) => s.opts.clone(),
            Step::BleRead(s) => s.opts.clone(),
            Step::BleWrite(s) => s.opts.clone(),
            Step::BleSubscribe(s) => s.opts.clone(),
            Step::BleNotify(s) => s.opts.clone(),
            Step::Acquire(s) => s.opts.clone(),
            Step::AcquireFirmware(s) => s.opts.clone(),
            Step::If(_) => StepOptions::default(),
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
    #[serde(flatten, default)]
    pub opts: StepOptions,
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
    #[serde(flatten, default)]
    pub opts: StepOptions,
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
    /// Optional scope slot that receives the matching payload.
    #[serde(default, alias = "capture_as")]
    pub capture_as: Option<String>,
    pub timeout_ms: u32,
    #[serde(flatten, default)]
    pub opts: StepOptions,
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
    /// Identifying token as authored in YAML.
    pub fn as_token(self) -> &'static str {
        match self {
            Encoding::Utf8 => "utf8",
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
}

/// Post-resolution typed signature. Today the MVP only ships
/// `Signature::BleAdvert`; later transports extend the enum.
#[derive(Debug, Clone)]
pub enum Signature {
    BleAdvert(BleAdvertSignature),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BleAdvertSignature {
    pub require: BleAdvertRequire,
    pub manufacturer_data: BleAdvertMfgData,
    /// Literal scope facts injected into runtime_scope on match
    /// (e.g. `style: legacy`). Plan §11.1 stores all scope as strings.
    #[serde(default)]
    pub scope: BTreeMap<String, String>,
    pub suggests: SuggestsBlock,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BleAdvertRequire {
    pub manufacturer_company_id: u16,
    #[serde(default)]
    pub advert_contains_service: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BleAdvertMfgData {
    /// Exact length, in bytes. Mutually exclusive with `min_length`
    /// (validated at load).
    #[serde(default)]
    pub length: Option<usize>,
    #[serde(default)]
    pub min_length: Option<usize>,
    /// Byte-level equality assertions. Authored either as a single map (the
    /// §2.1 compact form) or as a list — the loader's custom Deserialize
    /// accepts both.
    #[serde(default, deserialize_with = "deserialize_one_or_many")]
    pub assert_byte: Vec<MfgByteAssertion>,
    #[serde(default)]
    pub capture_bytes: Vec<MfgByteCapture>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MfgByteAssertion {
    pub index: usize,
    pub equals: u8,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MfgByteCapture {
    /// Start offset in the mfg-data byte array.
    pub from: usize,
    pub length: usize,
    pub encoding: Encoding,
    /// Scope key the captured bytes are bound to.
    pub name: String,
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
    pub manifest_path: PathBuf,
    /// Merged family + model BLE block, with GATT names already resolved on
    /// every Step's `gatt:` field.
    pub ble: Option<FamilyBleBlock>,
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
