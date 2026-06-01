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
#[derive(Debug, uniffi::Enum)]
pub enum Observation {
    /// A BLE advertisement seen during scan. Apple delivers service UUIDs as a
    /// list; some bodies advertise multiple — the matcher iterates the whole
    /// list. `manufacturer_data` is the RAW bytes from
    /// `CBAdvertisementDataManufacturerDataKey` (the FFI parses the BT-SIG
    /// company-ID prefix; the app does not).
    BleAdvert {
        service_uuids: Vec<String>,
        manufacturer_data: Vec<u8>,
        local_name: Option<String>,
    },
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
    /// MVP: always empty. Reserved for connection-bring-up chains (e.g. BLE
    /// pairing must complete before WiFi-AP handover). The FFI never sees a
    /// `Some(prereq)` until the §B WiFi-AP family lands in P2.
    pub prerequisite: Option<String>,
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

/// The seven MVP step verbs (plan §3.3). Externally inlined so each variant
/// is a flat record at the uniffi layer.
#[derive(Debug, Clone, uniffi::Enum)]
pub enum Step {
    BleConnect {
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
        opts: StepOptions,
    },
    BleNotify {
        gatt: String,
        until: BleNotifyUntil,
        capture_as: Option<String>,
        timeout_ms: u32,
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
///   walk time, then optionally apply a [`ValueTransform`] (e.g. `bitOr`).
/// * `Runtime { slot, encoding?, transform }` — app supplies before walk.
/// * `Captured { name, transform }` — earlier step / recognize-seed named
///   this slot; the RED `F557D96B` echo write uses
///   `Captured { name: "idNumber", transform: BitOr(0x20000000) }`.
#[derive(Debug, Clone, uniffi::Enum)]
pub enum StepValue {
    Literal {
        bytes: Vec<u8>,
    },
    Template {
        value: String,
        transform: Option<ValueTransform>,
    },
    Runtime {
        slot: String,
        encoding: Option<String>,
        transform: Option<ValueTransform>,
    },
    Captured {
        name: String,
        transform: Option<ValueTransform>,
    },
}

/// Allowlisted post-resolution byte transforms (BLE-MVP follow-up). The
/// dispatcher applies the transform between resolving the captured/runtime
/// bytes and writing them. Operand width matches the input bytes (most
/// commonly 4 bytes / u32). The allowlist starts tiny by design — extend
/// later via one schema field + one dispatcher match arm.
#[derive(Debug, Clone, Copy, uniffi::Enum)]
pub enum ValueTransform {
    BitOr { operand: u64 },
    BitAnd { operand: u64 },
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

impl From<ix::ValueTransform> for ValueTransform {
    fn from(t: ix::ValueTransform) -> Self {
        match t {
            ix::ValueTransform::BitOr(operand) => ValueTransform::BitOr { operand },
            ix::ValueTransform::BitAnd(operand) => ValueTransform::BitAnd { operand },
        }
    }
}

impl From<&ix::StepValue> for StepValue {
    fn from(v: &ix::StepValue) -> Self {
        match v {
            ix::StepValue::Literal { literal } => StepValue::Literal {
                bytes: yaml_value_to_bytes(literal, None).unwrap_or_default(),
            },
            ix::StepValue::Template {
                template,
                transform,
            } => StepValue::Template {
                value: template.clone(),
                transform: transform.map(Into::into),
            },
            ix::StepValue::Runtime {
                runtime,
                encoding,
                transform,
            } => StepValue::Runtime {
                slot: runtime.clone(),
                encoding: encoding.map(|e| e.as_token().to_string()),
                transform: transform.map(Into::into),
            },
            ix::StepValue::Captured {
                captured,
                transform,
            } => StepValue::Captured {
                name: captured.clone(),
                transform: transform.map(Into::into),
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
                value: yaml_value_to_bytes(value, *encoding).unwrap_or_default(),
            },
            ix::BleNotifyUntil::Matches { pattern } => BleNotifyUntil::Matches {
                pattern: pattern.clone(),
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
            ix::Step::BleRead(inner) => Step::BleRead {
                gatt: inner.gatt.clone(),
                encoding: inner.encoding.as_token().to_string(),
                capture_as: inner.capture_as.clone(),
                opts: (&inner.opts).into(),
            },
            ix::Step::BleWrite(inner) => Step::BleWrite {
                gatt: inner.gatt.clone(),
                value: (&inner.value).into(),
                opts: (&inner.opts).into(),
            },
            ix::Step::BleNotify(inner) => Step::BleNotify {
                gatt: inner.gatt.clone(),
                until: (&inner.until).into(),
                capture_as: inner.capture_as.clone(),
                timeout_ms: inner.timeout_ms,
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

/// Match a [`Observation::BleAdvert`] against every (model, signature) pair
/// in the resolved index, in file-declaration order (§11.7).
///
/// For the MVP the family fact "all Fuji adverts" never disambiguates by
/// model (the GFX100 II is the only declared model). When P2 adds more
/// models with overlapping signatures, this surfaces `Disambiguate` with
/// scope facts common to all matches.
pub fn recognize_ble(
    index: &ix::ResolvedManufacturerIndex,
    service_uuids: &[String],
    manufacturer_data: &[u8],
    _local_name: Option<&str>,
) -> Recognition {
    let service_uuids_upper: Vec<String> = service_uuids.iter().map(|s| s.to_uppercase()).collect();

    // Walk models in declaration order; per model walk signatures in file
    // order. The MVP returns the FIRST matching signature; multi-model
    // disambiguation gets added when a second body matches the same family
    // signature.
    let mut matches: Vec<(String, String, &ix::BleAdvertSignature)> = Vec::new();
    for model in &index.models {
        for (_sig_name, sig) in &model.signatures {
            let ix::Signature::BleAdvert(ble_sig) = sig;
            if !signature_matches(ble_sig, &service_uuids_upper, manufacturer_data) {
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
            let runtime_scope = build_runtime_scope(sig, manufacturer_data);
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

fn signature_matches(
    sig: &ix::BleAdvertSignature,
    service_uuids_upper: &[String],
    mfg_data: &[u8],
) -> bool {
    // require.advertContainsService (optional)
    if let Some(svc) = sig.require.advert_contains_service.as_deref() {
        let want = svc.to_uppercase();
        if !service_uuids_upper.contains(&want) {
            return false;
        }
    }
    // mfg-data length envelope
    if let Some(len) = sig.manufacturer_data.length {
        if mfg_data.len() != len {
            return false;
        }
    }
    if let Some(min) = sig.manufacturer_data.min_length {
        if mfg_data.len() < min {
            return false;
        }
    }
    // byte assertions
    for asrt in &sig.manufacturer_data.assert_byte {
        if mfg_data.get(asrt.index) != Some(&asrt.equals) {
            return false;
        }
    }
    // company-id verification: per §3.2 the FFI parses the company-ID
    // prefix. For Fuji and most manufacturers, the company-ID is the
    // KEY in CBAdvertisementDataManufacturerDataKey, not in the bytes
    // themselves — but the API hands the raw bytes through. Apple's
    // CBAdvertisementDataManufacturerDataKey already filtered by company.
    // The signature's require.manufacturer_company_id is a structural
    // assertion the caller (the app) should pre-filter on; here we treat
    // it as already-verified and don't re-check. If we later expose
    // company-id-bearing bytes, swap this.
    let _ = sig.require.manufacturer_company_id;
    true
}

fn build_runtime_scope(sig: &ix::BleAdvertSignature, mfg_data: &[u8]) -> Vec<KeyValue> {
    let mut out: Vec<KeyValue> =
        Vec::with_capacity(sig.scope.len() + sig.manufacturer_data.capture_bytes.len());
    // Literal scope facts (style, etc.) first.
    for (k, v) in &sig.scope {
        out.push(KeyValue {
            key: k.clone(),
            value: v.clone(),
        });
    }
    // Captured byte ranges, decoded per §11.2.
    for cap in &sig.manufacturer_data.capture_bytes {
        let end = cap.from + cap.length;
        if end > mfg_data.len() {
            continue; // capture would read past the buffer; skip
        }
        let bytes = &mfg_data[cap.from..end];
        if let Some(value) = decode_bytes(bytes, cap.encoding) {
            out.push(KeyValue {
                key: cap.name.clone(),
                value,
            });
        }
    }
    out
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

fn decode_bytes(bytes: &[u8], encoding: ix::Encoding) -> Option<String> {
    match encoding {
        ix::Encoding::Utf8 => std::str::from_utf8(bytes).ok().map(String::from),
        ix::Encoding::Ascii => {
            if bytes.iter().all(u8::is_ascii) {
                std::str::from_utf8(bytes).ok().map(String::from)
            } else {
                None
            }
        }
        ix::Encoding::Bytes | ix::Encoding::BytesRaw | ix::Encoding::BytesLe => {
            Some(hex_lower(bytes))
        }
        ix::Encoding::BytesBe => Some(hex_lower(bytes)),
        ix::Encoding::U8 => {
            if bytes.len() == 1 {
                Some((bytes[0] as u64).to_string())
            } else {
                None
            }
        }
        ix::Encoding::U16Le => {
            if bytes.len() == 2 {
                Some((u16::from_le_bytes([bytes[0], bytes[1]]) as u64).to_string())
            } else {
                None
            }
        }
        ix::Encoding::U16Be => {
            if bytes.len() == 2 {
                Some((u16::from_be_bytes([bytes[0], bytes[1]]) as u64).to_string())
            } else {
                None
            }
        }
        ix::Encoding::U32 | ix::Encoding::U32Le => {
            if bytes.len() == 4 {
                Some(
                    (u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as u64)
                        .to_string(),
                )
            } else {
                None
            }
        }
        ix::Encoding::U32Be => {
            if bytes.len() == 4 {
                Some(
                    (u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as u64)
                        .to_string(),
                )
            } else {
                None
            }
        }
    }
}

fn hex_lower(b: &[u8]) -> String {
    let mut s = String::with_capacity(b.len() * 2);
    for byte in b {
        use std::fmt::Write;
        let _ = write!(s, "{byte:02x}");
    }
    s
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

/// Build the per-(model, connection) plan. The MVP supports only `connection
/// == "ble"`; everything else returns `None`.
///
/// `initial_scope` is currently informational — the plan's steps don't
/// inline-resolve scope at this layer (the dispatcher does that mid-walk).
/// Per §11.1 *establishment-call phase*, the plan is returned with
/// structured `Captured` / `Runtime` / `Template` step values intact.
pub fn build_establishment(
    index: &ix::ResolvedManufacturerIndex,
    model: &str,
    connection: &str,
    _initial_scope: &[KeyValue],
) -> Option<EstablishmentPlan> {
    let model_view = index.models.iter().find(|m| m.id == model)?;
    if connection != "ble" {
        return None;
    }
    let ble = model_view.ble.as_ref()?;
    let steps = ble.establishment.steps.iter().map(Step::from).collect();
    Some(EstablishmentPlan {
        plan_handle: format!("{model}:{connection}"),
        mechanism: ble.establishment.mechanism.clone(),
        prerequisite: None,
        steps,
    })
}

// ---------------------------------------------------------------------------
// Local helpers
// ---------------------------------------------------------------------------

/// Best-effort decode of a YAML literal value into bytes. Used for
/// `StepValue::Literal { literal }` and `BleNotifyUntil::Equals { value }`.
/// MVP coverage:
/// * String + encoding `bytes-raw` / no encoding hint with hex digits →
///   decoded as lowercase hex.
/// * Sequence of u8 numbers → bytes verbatim.
/// * Integer u8/u16/u32 + an encoding → little-endian bytes (most common
///   on-wire form for Fuji adverts).
///
/// Returns `None` for shapes the MVP doesn't cover; callers fall back to an
/// empty `Vec<u8>` and a tolerant-aware error surface mid-walk.
fn yaml_value_to_bytes(v: &serde_yaml::Value, encoding: Option<ix::Encoding>) -> Option<Vec<u8>> {
    use ix::Encoding::*;
    match v {
        serde_yaml::Value::String(s) => {
            let trimmed = s.trim();
            let payload = trimmed.strip_prefix("0x").unwrap_or(trimmed);
            if payload.chars().all(|c| c.is_ascii_hexdigit()) && payload.len() % 2 == 0 {
                let mut out = Vec::with_capacity(payload.len() / 2);
                let bytes = payload.as_bytes();
                for chunk in bytes.chunks(2) {
                    let hi = (chunk[0] as char).to_digit(16)? as u8;
                    let lo = (chunk[1] as char).to_digit(16)? as u8;
                    out.push((hi << 4) | lo);
                }
                return Some(out);
            }
            if matches!(encoding, Some(Utf8)) {
                return Some(s.as_bytes().to_vec());
            }
            None
        }
        serde_yaml::Value::Number(n) => {
            let n_u = n.as_u64()?;
            match encoding {
                Some(U8) => Some(vec![n_u as u8]),
                Some(U16Le) => Some((n_u as u16).to_le_bytes().to_vec()),
                Some(U16Be) => Some((n_u as u16).to_be_bytes().to_vec()),
                Some(U32) | Some(U32Le) => Some((n_u as u32).to_le_bytes().to_vec()),
                Some(U32Be) => Some((n_u as u32).to_be_bytes().to_vec()),
                _ => None,
            }
        }
        serde_yaml::Value::Sequence(seq) => {
            let mut out = Vec::with_capacity(seq.len());
            for v in seq {
                let b = v.as_u64()?;
                if b > 255 {
                    return None;
                }
                out.push(b as u8);
            }
            Some(out)
        }
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// ConfigError conversion (camera-config side → FFI side)
// ---------------------------------------------------------------------------

impl From<cc::ConfigError> for crate::ConfigError {
    fn from(e: cc::ConfigError) -> Self {
        crate::ConfigError::Parse(e.to_string())
    }
}
