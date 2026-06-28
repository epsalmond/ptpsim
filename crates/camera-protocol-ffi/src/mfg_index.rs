//! Manufacturer-index FFI surface (plan §3.2 + §3.3 + §11).
//!
//! This is the **pull-model** query surface the iOS BLE-MVP consumes:
//!
//! 1. App boots and calls [`crate::ConfigStore::from_manufacturer_index`].
//! 2. BLE scan delivers an advert → app calls [`crate::ConfigStore::recognize`]
//!    with an [`Observation::BleAdvert`] → receives a [`Recognition`]
//!    carrying the matched signature's facts in `runtime_scope`.
//! 3. On `Candidate`, app calls [`crate::ConfigStore::establishment`]
//!    (model, connection, initial_scope) → receives an [`EstablishmentPlan`]
//!    whose `steps` the dispatcher walks.
//! 4. Optional: [`crate::ConfigStore::refine_establishment`] when firmware is
//!    discovered mid-walk.
//!
//! Types here intentionally match plan §3.3 verbatim — they form the iOS
//! contract. The conversion adapters from [`camera_config::index`] live in
//! the bottom half of this file.

use camera_config as cc;
use camera_config::index as ix;

use crate::KeyValue;

// ---------------------------------------------------------------------------
// Observation → Recognition (§3.2)
// ---------------------------------------------------------------------------

/// The pull-model input: what the app observed, the FFI decides what it means.
/// Plan §3.2 — only BLE is in the MVP. Later transports extend the enum
/// without changing callers.
///
/// Populate every field your platform exposes and leave the rest
/// `None`/empty — predicates over an absent field evaluate false, never
/// error (§11.14). CoreBluetooth cannot supply `ad_records` (no raw AD
/// access) and exposes TX power only when the advert carries it.
#[derive(Debug, uniffi::Enum)]
pub enum Observation {
    /// A BLE advertisement seen during scan. Apple delivers service UUIDs as a
    /// list; some bodies advertise multiple — the matcher iterates the whole
    /// list.
    BleAdvert {
        service_uuids: Vec<String>,
        /// The manufacturer-specific AD record, split into company id +
        /// post-id payload — signature payload offsets are relative to the
        /// payload (§11.14). Consumers split iOS
        /// `CBAdvertisementDataManufacturerDataKey` into
        /// `(company_id_LE, payload)`; Android's
        /// `getManufacturerSpecificData(companyId)` is already the payload.
        manufacturer_data: Option<BleManufacturerData>,
        /// Service-data AD records, one entry per advertised UUID.
        service_data: Vec<BleServiceData>,
        local_name: Option<String>,
        /// Advertised TX power level (dBm), when the advert carries one.
        tx_power: Option<i8>,
        /// Raw AD records exactly as seen on air, for platforms that expose
        /// them (Android `ScanRecord.getBytes()`); empty on iOS.
        ad_records: Vec<BleAdRecord>,
    },
}

/// The manufacturer-specific AD record split per §11.14: `payload` excludes
/// the 2-byte LE company id.
#[derive(Debug, Clone, uniffi::Record)]
pub struct BleManufacturerData {
    pub company_id: u16,
    pub payload: Vec<u8>,
}

#[derive(Debug, Clone, uniffi::Record)]
pub struct BleServiceData {
    pub uuid: String,
    pub payload: Vec<u8>,
}

/// One raw AD record as seen on air — for `ad_type` 0xFF the payload
/// INCLUDES the 2-byte LE company id.
#[derive(Debug, Clone, uniffi::Record)]
pub struct BleAdRecord {
    pub ad_type: u8,
    pub payload: Vec<u8>,
}

#[derive(Debug, uniffi::Enum)]
pub enum Recognition {
    /// No signature matched. App keeps scanning.
    NoMatch,
    /// Exactly one model + connection identified. `runtime_scope` carries
    /// every fact the signature derived (style, key bytes, etc.). App feeds
    /// it verbatim into `establishment(...)` as `initial_scope`.
    Candidate {
        model: String,
        connection: String,
        confidence: Confidence,
        runtime_scope: Vec<KeyValue>,
    },
    /// Multiple models matched the same signature (e.g. an advert that
    /// fits several Fuji bodies). The FFI does NOT auto-pick — the app
    /// prompts the user. `runtime_scope` here holds facts true for ALL
    /// candidates (e.g. `style: "legacy"`) and is passed to
    /// `establishment()` once the user narrows to a model.
    Disambiguate {
        family: String,
        candidates: Vec<ModelMatch>,
        runtime_scope: Vec<KeyValue>,
        hint: Option<String>,
    },
}

#[derive(Debug, Clone, Copy, uniffi::Enum)]
pub enum Confidence {
    High,
    Medium,
    Low,
}

#[derive(Debug, Clone, uniffi::Record)]
pub struct ModelMatch {
    pub model: String,
    pub display_name: String,
    pub connection_hint: Option<String>,
}

// ---------------------------------------------------------------------------
// EstablishmentPlan + Step grammar (§3.3 + §11)
// ---------------------------------------------------------------------------

/// The output of [`crate::ConfigStore::establishment`]: a walkable step
/// sequence. `plan_handle` is the opaque token the dispatcher echoes back to
/// [`crate::ConfigStore::refine_establishment`] when firmware is discovered.
#[derive(Debug, Clone, uniffi::Record)]
pub struct EstablishmentPlan {
    pub plan_handle: String,
    pub mechanism: String,
    /// Mechanism that must complete before this plan (e.g.
    /// `ble-establish-wifi-ap` carries `Some("ble-pair")`). Advisory — the
    /// consumer sequences on it; the reference walker does not enforce it.
    pub prerequisite: Option<String>,
    /// User-initiated from an established BLE link, NOT auto-chained after
    /// `prerequisite` (#91): the consumer rests at BLE-connected and runs this
    /// on a user action.
    pub on_demand: bool,
    /// Runtime parameter names the consumer binds before walking (e.g.
    /// `["launchMode"]`). Empty for plans that take no runtime input.
    pub params: Vec<String>,
    /// Slot names the host should persist after this plan to replay on a later
    /// `ble-reconnect` (#91). Empty for plans with nothing to cache.
    pub persist: Vec<String>,
    pub steps: Vec<Step>,
}

/// Common per-step options (§11.6). The dispatcher's retry loop wraps every
/// verb body uniformly — adding a verb in P2 doesn't change option handling.
#[derive(Debug, Clone, Default, uniffi::Record)]
pub struct StepOptions {
    pub tolerant: bool,
    pub retries: u32,
    pub retry_delay_ms: u32,
}

/// The BLE step verbs (plan §3.3 + §11). Externally inlined so each variant
/// is a flat record at the uniffi layer.
#[derive(Debug, Clone, uniffi::Enum)]
pub enum Step {
    BleConnect {
        opts: StepOptions,
    },
    /// Request an ATT MTU before GATT traffic. On platforms without an
    /// explicit request API (CoreBluetooth), succeed if the negotiated MTU
    /// is ≥ `mtu`, else step failure.
    BleRequestMtu {
        mtu: u16,
        opts: StepOptions,
    },
    /// Explicit GATT service-discovery checkpoint. On auto-discovering
    /// stacks, complete when discovery has completed — don't re-trigger.
    BleDiscoverServices {
        opts: StepOptions,
    },
    BleWrite {
        gatt: String,
        value: StepValue,
        opts: StepOptions,
    },
    BleRead {
        gatt: String,
        encoding: String,
        capture_as: String,
        /// Applied to the wire bytes BEFORE `encoding` decode (§11.13).
        transform: Vec<Transform>,
        opts: StepOptions,
    },
    /// CCCD-enable only. Success on descriptor-write ack — no notification
    /// payload is waited for. Use for pair-finalization rounds where the
    /// camera advances on the CCCD write itself.
    BleSubscribe {
        gatt: String,
        timeout_ms: u32,
        /// Which CCCD value to write (§11.8) — notify or indicate.
        mode: CccdMode,
        opts: StepOptions,
    },
    /// CCCD-enable AND wait for a matching notification payload. Use when
    /// the plan needs to capture or gate on a specific notification value.
    BleNotify {
        gatt: String,
        until: BleNotifyUntil,
        /// Whole matching payload → scope (kept alongside field captures).
        capture_as: Option<String>,
        /// Field captures: window → transform chain → encoding → scope.
        /// A failing capture is skipped, it does not fail the step.
        capture: Vec<NotifyCapture>,
        /// CCCD value the subscribe phase writes — notify or indicate.
        mode: CccdMode,
        timeout_ms: u32,
        opts: StepOptions,
    },
    /// Observe a characteristic until `until` holds, optionally acting each
    /// unsatisfied iteration (§11.15). The dispatcher loops: observe (poll a
    /// `read` source or await the `notify` stream) → apply captures into
    /// scope → if `until` holds, done; else run `on_each` and observe again,
    /// up to `timeout_ms`. Reference semantics:
    /// `camera_sim::ble::run_await_until`.
    BleAwaitUntil {
        source: AwaitSource,
        /// Field captures applied to each observed value before `until`
        /// (window → transform → encoding → scope). Fail-soft.
        capture: Vec<NotifyCapture>,
        /// Whole observed value → scope (hex) each iteration.
        capture_as: Option<String>,
        /// Satisfied when this predicate holds over scope.
        until: Predicate,
        /// Steps run each iteration `until` is not yet met, before the next
        /// observe. `Vec<Step>` (may be empty for a pure poll).
        on_each: Vec<Step>,
        timeout_ms: u32,
        /// Poll cadence for a `read` source (ms); ignored for `notify`.
        interval_ms: u32,
        opts: StepOptions,
    },
    Acquire {
        name: String,
        /// `Vec<Step>` of length 1 holding the inner step. uniffi 0.31 does
        /// not implement `Lift<UniFfiTag>` for `Box<T>` where `T` is a
        /// recursive uniffi::Enum (only `Arc<T>` is supported, and `Arc`
        /// would be semantically wrong here — Acquire owns its child step,
        /// it doesn't share it). The Vec wrapper is a uniffi-level
        /// workaround; the dispatcher's length-1 invariant is documented
        /// in the iOS implementation notes.
        from: Vec<Step>,
        opts: StepOptions,
    },
    AcquireFirmware {
        from: AcquireSource,
        opts: StepOptions,
    },
    If {
        condition: Predicate,
        then_branch: Vec<Step>,
        else_branch: Vec<Step>,
        /// §11.6: when true, an unbound predicate field evaluates as false
        /// (else-branch runs / step is skipped) rather than erroring.
        tolerant: bool,
    },
}

/// Step value forms (§11.1). At the FFI boundary:
/// * `Literal { bytes }` — the loader decoded the YAML hex string to bytes.
/// * `Template { value, transform }` — interpolate `{name}` against scope at
///   walk time, then apply the [`Transform`] chain in order.
/// * `Runtime { slot, encoding?, transform }` — app supplies before walk.
/// * `Captured { name, transform }` — earlier step / recognize-seed named
///   this slot; the RED `F557D96B` echo write uses
///   `Captured { name: "idNumber", transform: [BitOr(0x20000000)] }`.
///
/// An empty `transform` vec means no transform.
#[derive(Debug, Clone, uniffi::Enum)]
pub enum StepValue {
    Literal {
        bytes: Vec<u8>,
    },
    Template {
        value: String,
        transform: Vec<Transform>,
    },
    Runtime {
        slot: String,
        encoding: Option<String>,
        transform: Vec<Transform>,
    },
    Captured {
        name: String,
        transform: Vec<Transform>,
    },
}

/// Closed byte→byte transform vocabulary (§11.13). The dispatcher applies
/// the chain in order between resolving bytes and using them (write value)
/// or before `encoding`-decoding them (read/notify captures). Semantics are
/// specified by `camera_config::index::eval::apply_transforms` — implement
/// the dispatcher side to match its unit tests. Chain failure (out-of-range
/// slice, integer op on > 8 bytes, wrong width for `uuidFromBytes`) counts
/// as step failure under §11.6.
#[derive(Debug, Clone, uniffi::Enum)]
pub enum Transform {
    /// Input ≤ 8 bytes LE; re-emit at input width.
    BitOr {
        operand: u64,
    },
    /// Input ≤ 8 bytes LE; re-emit at input width.
    BitAnd {
        operand: u64,
    },
    /// Window `[at, at+length)`; `length` absent = to end.
    Slice {
        at: u64,
        length: Option<u64>,
    },
    /// Same as `Slice { at: count, length: None }`.
    DropPrefix {
        count: u64,
    },
    ReverseBytes,
    /// Exactly 16 bytes → 36 ASCII bytes of the canonical uppercase UUID.
    UuidFromBytes,
    /// Input ≤ 8 bytes LE: `(value & mask) >> shift`, re-emit at input width.
    Bits {
        mask: u64,
        shift: u32,
    },
}

/// Where an `acquire` / `acquireFirmware` step pulls its value from.
#[derive(Debug, Clone, uniffi::Enum)]
pub enum AcquireSource {
    BleAdvert {
        offset: u32,
        length: u32,
        encoding: String,
    },
    BleRead {
        gatt: String,
        encoding: String,
    },
    UserPrompt {
        text: String,
    },
}

/// Where `bleAwaitUntil` observes (§11.15): poll a readable characteristic,
/// or consume a characteristic's notification stream.
#[derive(Debug, Clone, uniffi::Enum)]
pub enum AwaitSource {
    Read { gatt: String },
    Notify { gatt: String, mode: CccdMode },
}

/// CCCD subscription mode (§11.8): `ENABLE_NOTIFICATION_VALUE` vs
/// `ENABLE_INDICATION_VALUE` in Android terms; on iOS both map to
/// `setNotifyValue(true)` (CoreBluetooth picks per the characteristic's
/// properties) — the mode is still carried so non-CoreBluetooth
/// dispatchers write the right descriptor value.
#[derive(Debug, Clone, Copy, uniffi::Enum)]
pub enum CccdMode {
    Notify,
    Indicate,
}

/// One field capture from a notification payload (§11.13 capture pipeline:
/// window `[at, at+length)` → transform chain → `encoding` decode → bind to
/// scope under `name`). `length` absent = to end.
#[derive(Debug, Clone, uniffi::Record)]
pub struct NotifyCapture {
    pub at: u64,
    pub length: Option<u64>,
    pub transform: Vec<Transform>,
    pub encoding: String,
    pub name: String,
}

/// `bleNotify` acceptance condition (§11.8). The Equals variant carries the
/// decoded payload bytes — the loader applied `encoding:` if it was present
/// in YAML.
#[derive(Debug, Clone, uniffi::Enum)]
pub enum BleNotifyUntil {
    Any,
    Equals { value: Vec<u8> },
    Matches { pattern: String },
}

/// `if:` predicate (§3.3). `value` is always stringified — runtime_scope
/// carries strings (§11.2 encoding rules govern how bytes/ints round-trip).
#[derive(Debug, Clone, uniffi::Record)]
pub struct Predicate {
    pub field: String,
    pub op: PredicateOp,
    pub value: String,
}

#[derive(Debug, Clone, Copy, uniffi::Enum)]
pub enum PredicateOp {
    Eq,
    Ne,
    Gt,
    Gte,
    Lt,
    Lte,
    In,
}

// ---------------------------------------------------------------------------
// Camera-config index types → FFI types (conversion adapters)
// ---------------------------------------------------------------------------

impl From<&ix::StepOptions> for StepOptions {
    fn from(o: &ix::StepOptions) -> Self {
        StepOptions {
            tolerant: o.tolerant,
            retries: o.retries,
            retry_delay_ms: o.retry_delay_ms,
        }
    }
}

impl From<ix::PredicateOp> for PredicateOp {
    fn from(op: ix::PredicateOp) -> Self {
        match op {
            ix::PredicateOp::Eq => PredicateOp::Eq,
            ix::PredicateOp::Ne => PredicateOp::Ne,
            ix::PredicateOp::Gt => PredicateOp::Gt,
            ix::PredicateOp::Gte => PredicateOp::Gte,
            ix::PredicateOp::Lt => PredicateOp::Lt,
            ix::PredicateOp::Lte => PredicateOp::Lte,
            ix::PredicateOp::In => PredicateOp::In,
        }
    }
}

impl From<&ix::Predicate> for Predicate {
    fn from(p: &ix::Predicate) -> Self {
        Predicate {
            field: p.field.clone(),
            op: p.op.into(),
            value: p.value.clone(),
        }
    }
}

impl From<&ix::Transform> for Transform {
    fn from(t: &ix::Transform) -> Self {
        match t {
            ix::Transform::BitOr(operand) => Transform::BitOr { operand: *operand },
            ix::Transform::BitAnd(operand) => Transform::BitAnd { operand: *operand },
            ix::Transform::Slice { at, length } => Transform::Slice {
                at: *at as u64,
                length: length.map(|l| l as u64),
            },
            ix::Transform::DropPrefix(count) => Transform::DropPrefix {
                count: *count as u64,
            },
            ix::Transform::ReverseBytes => Transform::ReverseBytes,
            ix::Transform::UuidFromBytes => Transform::UuidFromBytes,
            ix::Transform::Bits { mask, shift } => Transform::Bits {
                mask: *mask,
                shift: *shift,
            },
        }
    }
}

fn transforms(chain: &[ix::Transform]) -> Vec<Transform> {
    chain.iter().map(Into::into).collect()
}

impl From<ix::CccdMode> for CccdMode {
    fn from(m: ix::CccdMode) -> Self {
        match m {
            ix::CccdMode::Notify => CccdMode::Notify,
            ix::CccdMode::Indicate => CccdMode::Indicate,
        }
    }
}

impl From<&ix::NotifyCapture> for NotifyCapture {
    fn from(c: &ix::NotifyCapture) -> Self {
        NotifyCapture {
            at: c.at as u64,
            length: c.length.map(|l| l as u64),
            transform: transforms(&c.transform),
            encoding: c.encoding.as_token().to_string(),
            name: c.name.clone(),
        }
    }
}

impl From<&ix::StepValue> for StepValue {
    fn from(v: &ix::StepValue) -> Self {
        match v {
            ix::StepValue::Literal { literal } => StepValue::Literal {
                bytes: ix::eval::yaml_literal_to_bytes(literal, None).unwrap_or_default(),
            },
            ix::StepValue::Template {
                template,
                transform,
            } => StepValue::Template {
                value: template.clone(),
                transform: transforms(transform),
            },
            ix::StepValue::Runtime {
                runtime,
                encoding,
                transform,
            } => StepValue::Runtime {
                slot: runtime.clone(),
                encoding: encoding.map(|e| e.as_token().to_string()),
                transform: transforms(transform),
            },
            ix::StepValue::Captured {
                captured,
                transform,
            } => StepValue::Captured {
                name: captured.clone(),
                transform: transforms(transform),
            },
        }
    }
}

impl From<&ix::AcquireSource> for AcquireSource {
    fn from(s: &ix::AcquireSource) -> Self {
        match s {
            ix::AcquireSource::BleAdvert {
                offset,
                length,
                encoding,
            } => AcquireSource::BleAdvert {
                offset: *offset,
                length: *length,
                encoding: encoding.as_token().to_string(),
            },
            ix::AcquireSource::BleRead { gatt, encoding } => AcquireSource::BleRead {
                gatt: gatt.clone(),
                encoding: encoding.as_token().to_string(),
            },
            ix::AcquireSource::UserPrompt { text } => {
                AcquireSource::UserPrompt { text: text.clone() }
            }
        }
    }
}

impl From<&ix::BleNotifyUntil> for BleNotifyUntil {
    fn from(u: &ix::BleNotifyUntil) -> Self {
        match u {
            ix::BleNotifyUntil::Any => BleNotifyUntil::Any,
            ix::BleNotifyUntil::Equals { value, encoding } => BleNotifyUntil::Equals {
                value: ix::eval::yaml_literal_to_bytes(value, *encoding).unwrap_or_default(),
            },
            ix::BleNotifyUntil::Matches { pattern } => BleNotifyUntil::Matches {
                pattern: pattern.clone(),
            },
        }
    }
}

impl From<&ix::AwaitSource> for AwaitSource {
    fn from(s: &ix::AwaitSource) -> Self {
        match s {
            ix::AwaitSource::Read { gatt } => AwaitSource::Read { gatt: gatt.clone() },
            ix::AwaitSource::Notify { gatt, mode } => AwaitSource::Notify {
                gatt: gatt.clone(),
                mode: (*mode).into(),
            },
        }
    }
}

impl From<&ix::Step> for Step {
    fn from(s: &ix::Step) -> Self {
        match s {
            ix::Step::BleConnect(inner) => Step::BleConnect {
                opts: (&inner.opts).into(),
            },
            ix::Step::BleRequestMtu(inner) => Step::BleRequestMtu {
                mtu: inner.mtu,
                opts: (&inner.opts).into(),
            },
            ix::Step::BleDiscoverServices(inner) => Step::BleDiscoverServices {
                opts: (&inner.opts).into(),
            },
            ix::Step::BleRead(inner) => Step::BleRead {
                gatt: inner.gatt.clone(),
                encoding: inner.encoding.as_token().to_string(),
                capture_as: inner.capture_as.clone(),
                transform: transforms(&inner.transform),
                opts: (&inner.opts).into(),
            },
            ix::Step::BleWrite(inner) => Step::BleWrite {
                gatt: inner.gatt.clone(),
                value: (&inner.value).into(),
                opts: (&inner.opts).into(),
            },
            ix::Step::BleSubscribe(inner) => Step::BleSubscribe {
                gatt: inner.gatt.clone(),
                timeout_ms: inner.timeout_ms,
                mode: inner.mode.into(),
                opts: (&inner.opts).into(),
            },
            ix::Step::BleNotify(inner) => Step::BleNotify {
                gatt: inner.gatt.clone(),
                until: (&inner.until).into(),
                capture_as: inner.capture_as.clone(),
                capture: inner.capture.iter().map(Into::into).collect(),
                mode: inner.mode.into(),
                timeout_ms: inner.timeout_ms,
                opts: (&inner.opts).into(),
            },
            ix::Step::BleAwaitUntil(inner) => Step::BleAwaitUntil {
                source: (&inner.source).into(),
                capture: inner.capture.iter().map(Into::into).collect(),
                capture_as: inner.capture_as.clone(),
                until: (&inner.until).into(),
                on_each: inner.on_each.iter().map(Step::from).collect(),
                timeout_ms: inner.timeout_ms,
                interval_ms: inner.interval_ms,
                opts: (&inner.opts).into(),
            },
            ix::Step::Acquire(inner) => Step::Acquire {
                name: inner.name.clone(),
                from: vec![Step::from(&*inner.from)],
                opts: (&inner.opts).into(),
            },
            ix::Step::AcquireFirmware(inner) => Step::AcquireFirmware {
                from: (&inner.from).into(),
                opts: (&inner.opts).into(),
            },
            ix::Step::If(inner) => Step::If {
                condition: (&inner.condition).into(),
                then_branch: inner.then.iter().map(Step::from).collect(),
                else_branch: inner.else_branch.iter().map(Step::from).collect(),
                tolerant: inner.tolerant,
            },
        }
    }
}

// ---------------------------------------------------------------------------
// recognize() — observation → decision
// ---------------------------------------------------------------------------

/// Match a [`Observation::BleAdvert`] (converted to
/// [`ix::eval::BleAdvertFacts`]) against every (model, signature) pair in
/// the resolved index, in file-declaration order (§11.7).
///
/// For the MVP the family fact "all Fuji adverts" never disambiguates by
/// model (the GFX100 II is the only declared model). When P2 adds more
/// models with overlapping signatures, this surfaces `Disambiguate` with
/// scope facts common to all matches.
pub fn recognize_ble(
    index: &ix::ResolvedManufacturerIndex,
    facts: &ix::eval::BleAdvertFacts,
) -> Recognition {
    // Walk models in declaration order; per model walk signatures in file
    // order. The MVP returns the FIRST matching signature; multi-model
    // disambiguation gets added when a second body matches the same family
    // signature.
    let mut matches: Vec<(String, String, &ix::BleAdvertSignature)> = Vec::new();
    for model in &index.models {
        for (_sig_name, sig) in &model.signatures {
            let ix::Signature::BleAdvert(ble_sig) = sig;
            if !ix::eval::advert_matches(ble_sig, facts) {
                continue;
            }
            matches.push((model.id.clone(), model.display_name.clone(), ble_sig));
            // §11.7: first matching signature for THIS model wins; do not
            // try further signatures for the same model.
            break;
        }
    }

    match matches.len() {
        0 => Recognition::NoMatch,
        1 => {
            let (model_id, _display, sig) = &matches[0];
            let runtime_scope = ix::eval::advert_scope(sig, facts)
                .into_iter()
                .map(|(key, value)| KeyValue { key, value })
                .collect();
            Recognition::Candidate {
                model: model_id.clone(),
                connection: sig.suggests.connection.clone(),
                confidence: confidence_from(sig.suggests.confidence),
                runtime_scope,
            }
        }
        _ => {
            // Multi-model match: surface scope facts true across all
            // candidates (intersection of literal scopes). Mfg-data
            // captures vary per model, so they're left out of the

            let intersection = intersect_scope(matches.iter().map(|(_, _, s)| *s));
            let runtime_scope = intersection
                .into_iter()
                .map(|(k, v)| KeyValue { key: k, value: v })
                .collect();
            // Family inference: for MVP the family id is hard-coded as
            // the manufacturer index's manufacturer name lowercased. P2
            // will surface this from the signature/model graph properly.
            let family = index.manufacturer.to_lowercase();
            let candidates = matches
                .iter()
                .map(|(id, display, _)| ModelMatch {
                    model: id.clone(),
                    display_name: display.clone(),
                    connection_hint: None,
                })
                .collect();
            Recognition::Disambiguate {
                family,
                candidates,
                runtime_scope,
                hint: None,
            }
        }
    }
}

fn intersect_scope<'a>(
    sigs: impl Iterator<Item = &'a ix::BleAdvertSignature>,
) -> Vec<(String, String)> {
    let mut maps: Vec<&std::collections::BTreeMap<String, String>> =
        sigs.map(|s| &s.scope).collect();
    if maps.is_empty() {
        return Vec::new();
    }
    let first = maps.remove(0);
    let mut out = Vec::new();
    for (k, v) in first {
        if maps.iter().all(|m| m.get(k) == Some(v)) {
            out.push((k.clone(), v.clone()));
        }
    }
    out
}

fn confidence_from(c: ix::Confidence) -> Confidence {
    match c {
        ix::Confidence::High => Confidence::High,
        ix::Confidence::Medium => Confidence::Medium,
        ix::Confidence::Low => Confidence::Low,
    }
}

// ---------------------------------------------------------------------------
// establishment() — model + connection + initial_scope → plan
// ---------------------------------------------------------------------------

/// Build the establishment plan registered under `mechanism` for `model`.
/// The caller resolves `mechanism` from the body manifest's
/// `connections[connection].establishment`; this looks it up in the index
/// family BLE `establishments` registry. Returns `None` if the model has no
/// BLE block or no plan is registered under `mechanism`.
///
/// `initial_scope` is currently informational — the plan's steps don't
/// inline-resolve scope at this layer (the dispatcher does that mid-walk).
/// Per §11.1 *establishment-call phase*, the plan is returned with
/// structured `Captured` / `Runtime` / `Template` step values intact.
pub fn build_establishment(
    index: &ix::ResolvedManufacturerIndex,
    model: &str,
    connection: &str,
    mechanism: &str,
    _initial_scope: &[KeyValue],
) -> Option<EstablishmentPlan> {
    let model_view = index.models.iter().find(|m| m.id == model)?;
    let ble = model_view.ble.as_ref()?;
    let block = ble.establishment(mechanism)?;
    let steps = block.steps.iter().map(Step::from).collect();
    Some(EstablishmentPlan {
        plan_handle: format!("{model}:{connection}"),
        mechanism: block.mechanism.clone(),
        prerequisite: block.prerequisite.clone(),
        on_demand: block.on_demand,
        params: block.params.clone(),
        persist: block.persist.clone(),
        steps,
    })
}

/// The output of [`crate::ConfigStore::ble_action`]: a walkable BLE-native
/// control action over an established link (#91) — `remote-shutter`,
/// `write-time`, `write-gps`. The `Step` values keep their structured forms; the
/// host binds `params` and walks the steps from the resting BLE link.
#[derive(Debug, Clone, uniffi::Record)]
pub struct BleActionPlan {
    pub action: String,
    pub params: Vec<String>,
    pub steps: Vec<Step>,
    pub evidence: Vec<String>,
}

/// Build the BLE action plan registered under `action` for `model`. Looks it up
/// in the index family BLE `actions` registry. Returns `None` if the model has
/// no BLE block or no action is registered under `action`.
pub fn build_ble_action(
    index: &ix::ResolvedManufacturerIndex,
    model: &str,
    action: &str,
) -> Option<BleActionPlan> {
    let model_view = index.models.iter().find(|m| m.id == model)?;
    let ble = model_view.ble.as_ref()?;
    let block = ble.action(action)?;
    let steps = block.steps.iter().map(Step::from).collect();
    Some(BleActionPlan {
        action: action.to_string(),
        params: block.params.clone(),
        steps,
        evidence: block.evidence.clone(),
    })
}

// ---------------------------------------------------------------------------
// Local helpers
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// ConfigError conversion (camera-config side → FFI side)
// ---------------------------------------------------------------------------

impl From<cc::ConfigError> for crate::ConfigError {
    fn from(e: cc::ConfigError) -> Self {
        crate::ConfigError::Parse(e.to_string())
    }
}
