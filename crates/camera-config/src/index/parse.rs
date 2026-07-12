//! Loading + resolution for the manufacturer-index schema.
//!
//! The loader takes a raw `index_yaml` string and produces a
//! [`ResolvedManufacturerIndex`] — every model with its family fields merged
//! in, every static `{family.path}` template ref substituted to a literal
//! value, and every step's symbolic GATT name resolved to its UUID.
//!
//! See [plan §11] for the contracts this implements:
//! * §11.1 template grammar (the *FFI load* phase)
//! * §11.3 GATT-name → UUID at index-build
//! * §11.7 signature match precedence (file declaration order)
//! * §11.9 inheritance merge rules (scalars override, maps merge, arrays
//!   REPLACE)
//! * §11.10 fail-fast loader contract
//!
//! [plan §11]: ../../../../../docs/plans/ios-rewrite-p0-p1-ble-mvp.md

use std::collections::BTreeMap;

use serde_yaml::Value;

use super::types::{
    BleAdvertSignature, EstablishmentBlock, FamilyBleBlock, IndexedModel, ManufacturerIndex,
    ModelView, Predicate, PredicateOp, Signature, SignatureKind, Step, StepValue,
};
use crate::error::ConfigError;

/// The loader's output: one [`ModelView`] per declared model, in declaration
/// order (§11.7). The order is preserved so signature-match callers can rely
/// on top-of-file-first precedence.
#[derive(Debug, Clone)]
pub struct ResolvedManufacturerIndex {
    pub manufacturer: String,
    pub models: Vec<ModelView>,
}

impl ResolvedManufacturerIndex {
    /// Parse, inheritance-merge, substitute static refs, resolve GATT names.
    ///
    /// `index_yaml` is the raw `fuji/index.yaml` text. Model bodies are NOT
    /// loaded here — the caller passes them separately to
    /// [`crate::ConfigStore::from_manufacturer_index`] together with this
    /// index. Per §11.10 the load is fail-fast: any error aborts.
    pub fn from_yaml(index_yaml: &str) -> Result<Self, ConfigError> {
        // Stage 1: typed parse of the index itself.
        let index: ManufacturerIndex =
            serde_yaml::from_str(index_yaml).map_err(ConfigError::IndexParse)?;

        // Stage 2: build resolved per-model views.
        let mut models = Vec::with_capacity(index.models.len());
        for model in &index.models {
            models.push(resolve_one(&index, model)?);
        }
        Ok(ResolvedManufacturerIndex {
            manufacturer: index.manufacturer,
            models,
        })
    }
}

fn resolve_one(index: &ManufacturerIndex, model: &IndexedModel) -> Result<ModelView, ConfigError> {
    // -- BLE block: family-merged + GATT-resolved (operates on raw YAML
    //    Values throughout the merge; typed decode happens at the very end).
    let (ble, ble_value_for_resolve) = build_ble_block(index, model)?;

    // -- Signatures: per-signature, plant the merged BLE Value as a sibling
    //    so paths like `{ble.advert.manufacturerCompanyId}` resolve, then typed-decode.
    //    File-declaration order is preserved (§11.7).
    let mut signatures: Vec<(String, Signature)> = Vec::with_capacity(model.signatures.len());
    for (sig_name, raw_sig_value) in &model.signatures {
        let mut envelope = Value::Mapping(Default::default());
        if let Value::Mapping(m) = &mut envelope {
            m.insert(Value::String("__sig__".into()), raw_sig_value.clone());
            m.insert(Value::String("ble".into()), ble_value_for_resolve.clone());
        }
        let root_snapshot = envelope.clone();
        substitute_static_paths(
            &mut envelope,
            &root_snapshot,
            &format!("models.{}.signatures.{}", model.id, sig_name),
        )?;
        let cleaned = match envelope {
            Value::Mapping(mut m) => m
                .remove(Value::String("__sig__".into()))
                .unwrap_or(Value::Null),
            _ => Value::Null,
        };

        let sig = parse_signature(cleaned).map_err(|e| ConfigError::Validation {
            path: format!("models.{}.signatures.{}", model.id, sig_name),
            message: e,
        })?;
        signatures.push((sig_name.clone(), sig));
    }

    validate_reconnect_contract(model, ble.as_ref(), &signatures)?;

    Ok(ModelView {
        id: model.id.clone(),
        display_name: model.display_name.clone(),
        manifest_path: model.manifest.clone(),
        ble,
        signatures,
    })
}

fn validate_reconnect_contract(
    model: &IndexedModel,
    ble: Option<&FamilyBleBlock>,
    signatures: &[(String, Signature)],
) -> Result<(), ConfigError> {
    let Some(ble) = ble else {
        return Ok(());
    };
    if ble
        .reconnect
        .as_ref()
        .is_some_and(|policy| policy.scan_timeout_ms == 0)
    {
        return Err(ConfigError::Validation {
            path: format!("models.{}.ble.reconnect.scanTimeoutMs", model.id),
            message: "must be greater than zero".into(),
        });
    }

    for (name, signature) in signatures {
        let Signature::BleAdvert(signature) = signature;
        let Some(route) = &signature.reconnect else {
            continue;
        };
        let path = format!("models.{}.signatures.{name}.reconnect", model.id);
        if ble.reconnect.is_none() {
            return Err(ConfigError::Validation {
                path,
                message: "requires a family reconnect policy".into(),
            });
        }
        if route.identity.is_empty() {
            return Err(ConfigError::Validation {
                path: format!("{path}.identity"),
                message: "must declare at least one identity key".into(),
            });
        }
        if !ble.establishments.contains_key(&route.mechanism) {
            return Err(ConfigError::Validation {
                path: format!("{path}.mechanism"),
                message: format!("unknown establishment '{}'", route.mechanism),
            });
        }
        for key in &route.identity {
            let available = signature.scope.contains_key(key)
                || signature.capture.iter().any(|capture| &capture.name == key);
            if !available {
                return Err(ConfigError::Validation {
                    path: format!("{path}.identity"),
                    message: format!("identity key '{key}' is not captured or scoped"),
                });
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// BLE block: inheritance merge + GATT-name resolution
// ---------------------------------------------------------------------------

/// Build the merged + resolved BLE block for one model. Returns both the
/// typed [`FamilyBleBlock`] (for [`ModelView`]) and the raw `Value` form
/// (for downstream signature static-ref resolution — paths like
/// `{ble.advert.manufacturerCompanyId}` (and nested-map paths like
/// `{ble.advert.serviceUuids.fileTransfer}`) dot-walk into this Value).
fn build_ble_block(
    index: &ManufacturerIndex,
    model: &IndexedModel,
) -> Result<(Option<FamilyBleBlock>, Value), ConfigError> {
    // Collect inherited family BLE Values and deep-merge per §11.9 (maps
    // merge, arrays REPLACE, scalars overridden by the most-specific layer).
    let mut merged: Value = Value::Null;
    for fam_id in &model.inherits {
        let fam_value = index
            .families
            .get(fam_id)
            .ok_or_else(|| ConfigError::UnknownFamily {
                model_id: model.id.clone(),
                family_id: fam_id.clone(),
            })?;
        let Some(ble_v) = fam_value.get("ble").cloned() else {
            continue;
        };
        merged = deep_merge(merged, ble_v);
    }

    if matches!(merged, Value::Null) {
        return Ok((None, Value::Null));
    }

    // Resolve symbolic GATT names on every step → UUID strings (§11.3).
    let gatt_map: BTreeMap<String, String> = ble_gatt_from_merged(&merged);
    if let Some(plans) = merged
        .get_mut("establishments")
        .and_then(|e| e.as_mapping_mut())
    {
        for (name, plan) in plans.iter_mut() {
            let mech = name.as_str().unwrap_or("?").to_string();
            if let Some(steps) = plan
                .get_mut("postExitReadiness")
                .and_then(|s| s.as_sequence_mut())
            {
                resolve_gatt_names_in_steps(
                    steps,
                    &gatt_map,
                    &format!(
                        "models.{}.establishments.{mech}.postExitReadiness",
                        model.id
                    ),
                )?;
            }
            if let Some(steps) = plan.get_mut("steps").and_then(|s| s.as_sequence_mut()) {
                resolve_gatt_names_in_steps(
                    steps,
                    &gatt_map,
                    &format!("models.{}.establishments.{mech}.steps", model.id),
                )?;
            }
        }
    }
    // BLE control actions (#91) carry the same step grammar — resolve their
    // symbolic GATT names too, or `bleWrite { gatt: shootingRequest }` reaches
    // the walker unresolved and the write fails "characteristic not exposed".
    if let Some(actions) = merged.get_mut("actions").and_then(|a| a.as_mapping_mut()) {
        for (name, action) in actions.iter_mut() {
            let act = name.as_str().unwrap_or("?").to_string();
            if let Some(steps) = action.get_mut("steps").and_then(|s| s.as_sequence_mut()) {
                resolve_gatt_names_in_steps(
                    steps,
                    &gatt_map,
                    &format!("models.{}.actions.{act}.steps", model.id),
                )?;
            }
        }
    }

    // Snapshot the resolved-for-signature-substitution form (still pre
    // typed-decode) and then typed-decode for the ModelView field.
    let value_for_resolve = merged.clone();
    let ble: FamilyBleBlock =
        serde_yaml::from_value(merged).map_err(|e| ConfigError::Validation {
            path: format!("models.{}.ble", model.id),
            message: format!("typed ble decode: {e}"),
        })?;
    for (mech, est) in &ble.establishments {
        validate_establishment(est, &ble.gatt, &model.id, mech)?;
    }
    Ok((Some(ble), value_for_resolve))
}

fn ble_gatt_from_merged(v: &Value) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    if let Some(Value::Mapping(m)) = v.get("gatt").map(|x| x.to_owned()).as_ref() {
        for (k, val) in m {
            if let (Some(k), Some(val)) = (k.as_str(), val.as_str()) {
                out.insert(k.to_string(), val.to_string());
            }
        }
    }
    out
}

fn resolve_gatt_names_in_steps(
    steps: &mut serde_yaml::Sequence,
    gatt: &BTreeMap<String, String>,
    path_ctx: &str,
) -> Result<(), ConfigError> {
    for (i, step) in steps.iter_mut().enumerate() {
        let Value::Mapping(m) = step else {
            continue;
        };
        // Externally tagged: exactly one key per step.
        let Some((verb_key, body)) = m.iter_mut().next() else {
            continue;
        };
        let verb = verb_key.as_str().unwrap_or("");
        let here = format!("{path_ctx}[{i}].{verb}");
        match verb {
            "bleConnect" | "bleAwaitDisconnect" | "bleRequestMtu" | "bleDiscoverServices" => {}
            "bleRead" | "bleWrite" | "bleSubscribe" | "bleNotify" | "bleWriteChunk" => {
                resolve_gatt_field(body, gatt, &here)?;
            }
            "acquire" => {
                if let Some(inner) = body
                    .as_mapping_mut()
                    .and_then(|m| m.get_mut(Value::String("from".into())))
                {
                    let mut single = serde_yaml::Sequence::new();
                    single.push(inner.clone());
                    resolve_gatt_names_in_steps(&mut single, gatt, &format!("{here}.from"))?;
                    *inner = single.into_iter().next().unwrap_or(Value::Null);
                }
            }
            "acquireFirmware" => {
                // `from:` is an AcquireSource — only BleRead inside it has a
                // gatt: field.
                if let Some(from) = body
                    .as_mapping_mut()
                    .and_then(|m| m.get_mut(Value::String("from".into())))
                {
                    if let Some(from_map) = from.as_mapping_mut() {
                        if let Some(bleread) = from_map.get_mut(Value::String("bleRead".into())) {
                            resolve_gatt_field(bleread, gatt, &format!("{here}.from.bleRead"))?;
                        }
                    }
                }
            }
            "bleAwaitUntil" => {
                if let Some(body_map) = body.as_mapping_mut() {
                    if let Some(source) = body_map.get_mut(Value::String("source".into())) {
                        resolve_await_source_gatt(source, gatt, &format!("{here}.source"))?;
                    }
                    if let Some(Value::Sequence(on_each)) =
                        body_map.get_mut(Value::String("onEach".into()))
                    {
                        resolve_gatt_names_in_steps(on_each, gatt, &format!("{here}.onEach"))?;
                    }
                }
            }
            "if" => {
                if let Some(body_map) = body.as_mapping_mut() {
                    if let Some(Value::Sequence(then_seq)) =
                        body_map.get_mut(Value::String("then".into()))
                    {
                        resolve_gatt_names_in_steps(then_seq, gatt, &format!("{here}.then"))?;
                    }
                    if let Some(Value::Sequence(else_seq)) =
                        body_map.get_mut(Value::String("else".into()))
                    {
                        resolve_gatt_names_in_steps(else_seq, gatt, &format!("{here}.else"))?;
                    }
                }
            }
            other => {
                return Err(ConfigError::Validation {
                    path: here.clone(),
                    message: format!("unknown step verb '{other}' (allowlist: bleConnect, bleAwaitDisconnect, bleRequestMtu, bleDiscoverServices, bleRead, bleWrite, bleSubscribe, bleNotify, bleAwaitUntil, bleWriteChunk, acquire, acquireFirmware, if)"),
                });
            }
        }
    }
    Ok(())
}

fn resolve_gatt_field(
    step_body: &mut Value,
    gatt: &BTreeMap<String, String>,
    path_ctx: &str,
) -> Result<(), ConfigError> {
    let Some(m) = step_body.as_mapping_mut() else {
        return Ok(());
    };
    let gatt_key = Value::String("gatt".into());
    let Some(Value::String(name)) = m.get(&gatt_key).cloned() else {
        return Ok(());
    };
    let resolved = resolve_one_gatt_name(&name, gatt, path_ctx)?;
    m.insert(gatt_key, Value::String(resolved));
    Ok(())
}

/// Resolve a single symbolic GATT name to its UUID (or accept an inline UUID).
pub(crate) fn resolve_one_gatt_name(
    name: &str,
    gatt: &BTreeMap<String, String>,
    path_ctx: &str,
) -> Result<String, ConfigError> {
    if let Some(uuid) = gatt.get(name) {
        Ok(uuid.clone())
    } else if looks_like_uuid(name) {
        // Authored as a full UUID inline — accept verbatim.
        Ok(name.to_string())
    } else {
        Err(ConfigError::Validation {
            path: format!("{path_ctx}.gatt"),
            message: format!("undefined gatt symbolic name '{name}'"),
        })
    }
}

/// Resolve the gatt name carried inside a `bleAwaitUntil` `source:` block
/// (`read: <name>` bare string, or `notify: { gatt: <name>, ... }`).
fn resolve_await_source_gatt(
    source: &mut Value,
    gatt: &BTreeMap<String, String>,
    path_ctx: &str,
) -> Result<(), ConfigError> {
    let Some(m) = source.as_mapping_mut() else {
        return Ok(());
    };
    let read_key = Value::String("read".into());
    if let Some(Value::String(name)) = m.get(&read_key).cloned() {
        let resolved = resolve_one_gatt_name(&name, gatt, &format!("{path_ctx}.read"))?;
        m.insert(read_key, Value::String(resolved));
        return Ok(());
    }
    if let Some(notify) = m.get_mut(Value::String("notify".into())) {
        resolve_gatt_field(notify, gatt, &format!("{path_ctx}.notify"))?;
    }
    Ok(())
}

fn looks_like_uuid(s: &str) -> bool {
    // 8-4-4-4-12 hex digits with hyphens.
    let parts: Vec<&str> = s.split('-').collect();
    parts.len() == 5
        && parts[0].len() == 8
        && parts[1].len() == 4
        && parts[2].len() == 4
        && parts[3].len() == 4
        && parts[4].len() == 12
        && parts
            .iter()
            .all(|p| p.bytes().all(|b| b.is_ascii_hexdigit()))
}

/// Validate the establishment block after deserialization: every step's
/// verb is in the MVP allowlist (Step's enum already enforces that, but
/// some nested validations live here too).
fn validate_establishment(
    est: &EstablishmentBlock,
    _gatt: &BTreeMap<String, String>,
    model_id: &str,
    mechanism: &str,
) -> Result<(), ConfigError> {
    for (i, step) in est.post_exit_readiness.iter().enumerate() {
        let path = format!("models.{model_id}.establishments.{mechanism}.postExitReadiness[{i}]");
        validate_step(step, &path)?;
        forbid_acquire_firmware(step, &path)?;
    }
    for (i, step) in est.steps.iter().enumerate() {
        validate_step(
            step,
            &format!("models.{model_id}.establishments.{mechanism}.steps[{i}]"),
        )?;
    }
    Ok(())
}

fn validate_step(step: &Step, path: &str) -> Result<(), ConfigError> {
    // Per-step structural checks beyond what serde already enforces.
    if let Step::If(s) = step {
        for (i, inner) in s.then.iter().enumerate() {
            validate_step(inner, &format!("{path}.then[{i}]"))?;
        }
        for (i, inner) in s.else_branch.iter().enumerate() {
            validate_step(inner, &format!("{path}.else[{i}]"))?;
        }
    }
    if let Step::Acquire(s) = step {
        validate_step(&s.from, &format!("{path}.from"))?;
    }
    if let Step::BleAwaitUntil(s) = step {
        if s.timeout_ms == 0 {
            return Err(ConfigError::Validation {
                path: format!("{path}.timeoutMs"),
                message: "bleAwaitUntil timeoutMs must be > 0 (an await needs a budget)"
                    .to_string(),
            });
        }
        for (i, cap) in s.capture.iter().enumerate() {
            if cap.length == Some(0) {
                return Err(ConfigError::Validation {
                    path: format!("{path}.capture[{i}].length"),
                    message: "capture length 0 can never capture bytes".to_string(),
                });
            }
        }
        for (i, inner) in s.on_each.iter().enumerate() {
            validate_step(inner, &format!("{path}.onEach[{i}]"))?;
        }
    }
    if let Step::BleAwaitDisconnect(s) = step {
        if s.timeout_ms == 0 {
            return Err(ConfigError::Validation {
                path: format!("{path}.timeoutMs"),
                message: "bleAwaitDisconnect timeoutMs must be > 0".to_string(),
            });
        }
    }
    // Mutually-exclusive length forms on mfg-data ranges live with the
    // signature validation; nothing further here for steps in MVP.
    let _ = step.verb_name();
    Ok(())
}

/// `postExitReadiness` is a fixed replayability gate: §11.5 firmware tiering
/// (`acquireFirmware` → `refineEstablishment` tail splice) applies to `steps`
/// only, and executors walk the gate without a refinement context — an
/// `acquireFirmware` here would bind firmware and then silently skip the
/// refinement it exists to trigger. Reject it at parse time instead.
fn forbid_acquire_firmware(step: &Step, path: &str) -> Result<(), ConfigError> {
    match step {
        Step::AcquireFirmware(_) => Err(ConfigError::Validation {
            path: path.to_string(),
            message: "acquireFirmware is not allowed in postExitReadiness \
                      (firmware tiering applies to steps only)"
                .to_string(),
        }),
        Step::Acquire(s) => forbid_acquire_firmware(&s.from, &format!("{path}.from")),
        Step::If(s) => {
            for (i, inner) in s.then.iter().enumerate() {
                forbid_acquire_firmware(inner, &format!("{path}.then[{i}]"))?;
            }
            for (i, inner) in s.else_branch.iter().enumerate() {
                forbid_acquire_firmware(inner, &format!("{path}.else[{i}]"))?;
            }
            Ok(())
        }
        Step::BleAwaitUntil(s) => {
            for (i, inner) in s.on_each.iter().enumerate() {
                forbid_acquire_firmware(inner, &format!("{path}.onEach[{i}]"))?;
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

// ---------------------------------------------------------------------------
// Static path substitution (§11.1)
// ---------------------------------------------------------------------------

/// Replace every `"{family.path.dotted}"` string scalar in `value` with the
/// literal located at that path in `root`. The substitution is **whole-
/// string-only** (§11.1): `"prefix{path}suffix"` is rejected.
fn substitute_static_paths(
    value: &mut Value,
    root: &Value,
    path_ctx: &str,
) -> Result<(), ConfigError> {
    match value {
        Value::String(s) => {
            let trimmed = s.trim();
            if let Some(path) = whole_brace_path(trimmed) {
                let resolved = lookup_path(root, path).ok_or_else(|| ConfigError::Validation {
                    path: path_ctx.to_string(),
                    message: format!("unresolved static ref '{{{path}}}'"),
                })?;
                *value = resolved;
            } else if has_brace(trimmed) {
                return Err(ConfigError::Validation {
                    path: path_ctx.to_string(),
                    message: format!(
                        "string '{trimmed}' has embedded '{{...}}' — only whole-string \
                         static refs are allowed at index-load (§11.1)"
                    ),
                });
            }
            Ok(())
        }
        Value::Mapping(m) => {
            for (k, v) in m.iter_mut() {
                let key = k.as_str().unwrap_or("?");
                substitute_static_paths(v, root, &format!("{path_ctx}.{key}"))?;
            }
            Ok(())
        }
        Value::Sequence(seq) => {
            for (i, v) in seq.iter_mut().enumerate() {
                substitute_static_paths(v, root, &format!("{path_ctx}[{i}]"))?;
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

/// `"{ble.advert.manufacturerCompanyId}"` → `Some("ble.advert.manufacturerCompanyId")`.
/// Returns `None` for anything else.
fn whole_brace_path(s: &str) -> Option<&str> {
    let s = s.strip_prefix('{')?.strip_suffix('}')?;
    if s.is_empty() || s.contains('{') || s.contains('}') {
        return None;
    }
    Some(s)
}

fn has_brace(s: &str) -> bool {
    s.contains('{') || s.contains('}')
}

fn lookup_path(root: &Value, path: &str) -> Option<Value> {
    let mut cur = root;
    for seg in path.split('.') {
        cur = match cur {
            Value::Mapping(m) => m.get(Value::String(seg.to_string()))?,
            _ => return None,
        };
    }
    Some(cur.clone())
}

// ---------------------------------------------------------------------------
// Inheritance merge (§11.9)
// ---------------------------------------------------------------------------

/// Maps merge per-key (recursive); scalars and arrays from `overlay` REPLACE
/// the base. This deliberately matches the existing [`crate::merge_yaml`]
/// semantics so the loader behaviour is consistent across the per-model and
/// per-manufacturer-index schemas.
fn deep_merge(base: Value, overlay: Value) -> Value {
    use Value::Mapping;
    match (base, overlay) {
        (Mapping(mut b), Mapping(o)) => {
            for (k, ov) in o {
                let merged = match b.remove(&k) {
                    Some(bv) => deep_merge(bv, ov),
                    None => ov,
                };
                b.insert(k, merged);
            }
            Mapping(b)
        }
        (_, overlay) => overlay,
    }
}

// ---------------------------------------------------------------------------
// Signatures — typed body deserialization
// ---------------------------------------------------------------------------

/// Dispatch on the `kind:` field of a (template-substituted) signature value.
/// Today only `bleAdvert` is supported; future kinds extend the enum and
/// this dispatch.
fn parse_signature(mut value: Value) -> Result<Signature, String> {
    let kind = {
        let Value::Mapping(m) = &mut value else {
            return Err("signature must be a mapping".into());
        };
        let kind_v = m
            .remove(Value::String("kind".into()))
            .ok_or_else(|| "signature missing required `kind` field".to_string())?;
        let kind: SignatureKind =
            serde_yaml::from_value(kind_v).map_err(|e| format!("signature kind: {e}"))?;
        kind
    };
    match kind {
        SignatureKind::BleAdvert => {
            let sig: BleAdvertSignature = serde_yaml::from_value(value)
                .map_err(|e| format!("typed BleAdvert decode: {e}"))?;
            validate_advert_predicate(&sig.require)?;
            for (i, cap) in sig.capture.iter().enumerate() {
                if cap.length == Some(0) {
                    return Err(format!("capture[{i}]: length 0 can never capture bytes"));
                }
            }
            Ok(Signature::BleAdvert(sig))
        }
    }
}

/// Static checks the predicate grammar can't express in types alone (§11.14):
/// non-empty combinators, mutually-exclusive length forms, exactly-one
/// local-name form, at-least-one TX-power bound, non-vacuous payloads,
/// non-zero bit masks.
fn validate_advert_predicate(p: &super::types::AdvertPredicate) -> Result<(), String> {
    use super::types::AdvertPredicate as P;
    let payload_checks = |ctx: &str, pl: &super::types::PayloadPredicate| -> Result<(), String> {
        if pl.length.is_some() && pl.min_length.is_some() {
            return Err(format!(
                "{ctx}: length and minLength are mutually exclusive"
            ));
        }
        for b in &pl.assert_bits {
            if b.mask == 0 {
                return Err(format!("{ctx}: assertBits mask 0 always yields 0"));
            }
        }
        Ok(())
    };
    match p {
        P::All(children) | P::Any(children) => {
            if children.is_empty() {
                return Err("all/any: empty predicate list".to_string());
            }
            for c in children {
                validate_advert_predicate(c)?;
            }
            Ok(())
        }
        P::Not(inner) => validate_advert_predicate(inner),
        P::ManufacturerData(m) => {
            if m.company_id.is_none() && m.payload.is_empty() {
                return Err(
                    "manufacturerData: no companyId and no payload constraint (vacuous)"
                        .to_string(),
                );
            }
            payload_checks("manufacturerData", &m.payload)
        }
        P::ServiceUuids { contains } => {
            if contains.is_empty() {
                return Err("serviceUuids.contains: empty UUID".to_string());
            }
            Ok(())
        }
        P::ServiceData { uuid, payload } => {
            if uuid.is_empty() {
                return Err("serviceData: empty UUID".to_string());
            }
            payload_checks("serviceData", payload)
        }
        P::LocalName(n) => {
            let forms = [&n.equals, &n.prefix, &n.contains]
                .iter()
                .filter(|f| f.is_some())
                .count();
            if forms != 1 {
                return Err("localName: exactly one of equals/prefix/contains required".to_string());
            }
            Ok(())
        }
        P::TxPower { min, max } => {
            if min.is_none() && max.is_none() {
                return Err("txPower: at least one of min/max required".to_string());
            }
            Ok(())
        }
        P::RawAdRecord { payload, .. } => payload_checks("rawAdRecord", payload),
    }
}

// ---------------------------------------------------------------------------
// Predicate custom Deserialize — compact YAML form
// ---------------------------------------------------------------------------

impl<'de> serde::Deserialize<'de> for Predicate {
    /// Deserialize the §2.1 compact form `{ <field-name>: { <op>: <value> } }`
    /// into the canonical `{ field, op, value }` shape. `value` is stringified
    /// (§11.2 — scope is always strings).
    fn deserialize<D>(d: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        use serde::de::Error;
        let raw: BTreeMap<String, BTreeMap<String, serde_yaml::Value>> = BTreeMap::deserialize(d)?;
        if raw.len() != 1 {
            return Err(D::Error::custom(
                "predicate must have exactly one field name at the top level",
            ));
        }
        let (field, inner) = raw.into_iter().next().unwrap();
        if inner.len() != 1 {
            return Err(D::Error::custom(format!(
                "predicate on '{field}' must have exactly one operator"
            )));
        }
        let (op_str, val) = inner.into_iter().next().unwrap();
        let op = PredicateOp::from_token(&op_str).ok_or_else(|| {
            D::Error::custom(format!(
                "unknown predicate operator '{op_str}' (allowlist: eq, ne, gt, gte, lt, lte, in)"
            ))
        })?;
        let value = yaml_scalar_to_string(&val).ok_or_else(|| {
            D::Error::custom(format!(
                "predicate value for '{field}.{op_str}' must be a scalar"
            ))
        })?;
        Ok(Predicate { field, op, value })
    }
}

fn yaml_scalar_to_string(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.clone()),
        Value::Bool(b) => Some(b.to_string()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Custom Deserialize for shapes the default external tagging doesn't fit.
// ---------------------------------------------------------------------------

use super::types::{AcquireSource, BleNotifyUntil};

impl<'de> serde::Deserialize<'de> for Step {
    /// YAML form: `- bleConnect: {}` / `- bleRead: { ... }` etc. — a
    /// single-entry mapping whose key names the verb.
    fn deserialize<D>(d: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        use serde::de::Error;
        let mapping = serde_yaml::Mapping::deserialize(d)?;
        if mapping.len() != 1 {
            return Err(D::Error::custom(format!(
                "step must be a single-entry mapping (got {} keys)",
                mapping.len()
            )));
        }
        let (verb_v, body) = mapping.into_iter().next().unwrap();
        let verb = verb_v
            .as_str()
            .ok_or_else(|| D::Error::custom("step verb key must be a string"))?
            .to_string();
        let dec_err =
            |what: &str, e: serde_yaml::Error| D::Error::custom(format!("decoding {what}: {e}"));
        match verb.as_str() {
            "bleConnect" => Ok(Step::BleConnect(
                serde_yaml::from_value(body).map_err(|e| dec_err("bleConnect", e))?,
            )),
            "bleAwaitDisconnect" => Ok(Step::BleAwaitDisconnect(
                serde_yaml::from_value(body).map_err(|e| dec_err("bleAwaitDisconnect", e))?,
            )),
            "bleRequestMtu" => Ok(Step::BleRequestMtu(
                serde_yaml::from_value(body).map_err(|e| dec_err("bleRequestMtu", e))?,
            )),
            "bleDiscoverServices" => Ok(Step::BleDiscoverServices(
                serde_yaml::from_value(body).map_err(|e| dec_err("bleDiscoverServices", e))?,
            )),
            "bleRead" => Ok(Step::BleRead(
                serde_yaml::from_value(body).map_err(|e| dec_err("bleRead", e))?,
            )),
            "bleWrite" => Ok(Step::BleWrite(
                serde_yaml::from_value(body).map_err(|e| dec_err("bleWrite", e))?,
            )),
            "bleSubscribe" => Ok(Step::BleSubscribe(
                serde_yaml::from_value(body).map_err(|e| dec_err("bleSubscribe", e))?,
            )),
            "bleNotify" => Ok(Step::BleNotify(
                serde_yaml::from_value(body).map_err(|e| dec_err("bleNotify", e))?,
            )),
            "bleAwaitUntil" => Ok(Step::BleAwaitUntil(
                serde_yaml::from_value(body).map_err(|e| dec_err("bleAwaitUntil", e))?,
            )),
            "bleWriteChunk" => Ok(Step::BleWriteChunk(
                serde_yaml::from_value(body).map_err(|e| dec_err("bleWriteChunk", e))?,
            )),
            "acquire" => Ok(Step::Acquire(
                serde_yaml::from_value(body).map_err(|e| dec_err("acquire", e))?,
            )),
            "acquireFirmware" => Ok(Step::AcquireFirmware(
                serde_yaml::from_value(body).map_err(|e| dec_err("acquireFirmware", e))?,
            )),
            "if" => Ok(Step::If(
                serde_yaml::from_value(body).map_err(|e| dec_err("if", e))?,
            )),
            other => Err(D::Error::custom(format!(
                "unknown step verb '{other}' (allowlist: bleConnect, bleAwaitDisconnect, bleRequestMtu, bleDiscoverServices, bleRead, bleWrite, bleSubscribe, bleNotify, bleAwaitUntil, bleWriteChunk, acquire, acquireFirmware, if)"
            ))),
        }
    }
}

impl<'de> serde::Deserialize<'de> for StepValue {
    /// YAML form: a single-entry mapping whose key names the value form
    /// (`literal`, `template`, `runtime`, `captured`), with optional
    /// `encoding:` / `transform:` siblings.
    fn deserialize<D>(d: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        use serde::de::Error;
        let mapping = serde_yaml::Mapping::deserialize(d)?;
        let mut keys: Vec<&str> = mapping
            .iter()
            .map(|(k, _)| k.as_str().unwrap_or(""))
            .collect();
        keys.sort();
        let read_transform =
            |m: &serde_yaml::Mapping| -> Result<Vec<super::types::Transform>, D::Error> {
                match m.get(Value::String("transform".into())) {
                    Some(v) => transform_chain(v.clone())
                        .map_err(|e| D::Error::custom(format!("transform: {e}"))),
                    None => Ok(Vec::new()),
                }
            };
        if mapping.contains_key(Value::String("literal".into())) {
            let literal = mapping
                .get(Value::String("literal".into()))
                .unwrap()
                .clone();
            return Ok(StepValue::Literal { literal });
        }
        if mapping.contains_key(Value::String("template".into())) {
            let template = mapping
                .get(Value::String("template".into()))
                .and_then(Value::as_str)
                .ok_or_else(|| D::Error::custom("template: <string> required"))?
                .to_string();
            let transform = read_transform(&mapping)?;
            return Ok(StepValue::Template {
                template,
                transform,
            });
        }
        if mapping.contains_key(Value::String("captured".into())) {
            let captured = mapping
                .get(Value::String("captured".into()))
                .and_then(Value::as_str)
                .ok_or_else(|| D::Error::custom("captured: <string> required"))?
                .to_string();
            let transform = read_transform(&mapping)?;
            return Ok(StepValue::Captured {
                captured,
                transform,
            });
        }
        if mapping.contains_key(Value::String("runtime".into())) {
            let runtime = mapping
                .get(Value::String("runtime".into()))
                .and_then(Value::as_str)
                .ok_or_else(|| D::Error::custom("runtime: <string> required"))?
                .to_string();
            let encoding = match mapping.get(Value::String("encoding".into())) {
                Some(v) => Some(
                    serde_yaml::from_value::<super::types::Encoding>(v.clone())
                        .map_err(|e| D::Error::custom(format!("runtime.encoding: {e}")))?,
                ),
                None => None,
            };
            let transform = read_transform(&mapping)?;
            return Ok(StepValue::Runtime {
                runtime,
                encoding,
                transform,
            });
        }
        Err(D::Error::custom(format!(
            "stepValue must be one of: literal, template, runtime, captured (got keys: {keys:?})"
        )))
    }
}

/// Normalize a `transform:` value into a chain: a single mapping is a
/// 1-element chain; a sequence is the chain in application order (§11.13).
pub(crate) fn transform_chain(v: Value) -> Result<Vec<super::types::Transform>, serde_yaml::Error> {
    match v {
        Value::Sequence(seq) => seq.into_iter().map(serde_yaml::from_value).collect(),
        other => Ok(vec![serde_yaml::from_value(other)?]),
    }
}

impl<'de> serde::Deserialize<'de> for super::types::Transform {
    /// YAML form: a single-entry mapping whose key names the transform.
    /// Operand shapes per primitive: `bitOr`/`bitAnd` take a u64,
    /// `dropPrefix` a usize, `slice`/`bits` a mapping body,
    /// `reverseBytes`/`uuidFromBytes` an empty mapping (or nothing).
    /// Statically-invalid operands (zero-length slice, zero mask) are
    /// load errors — they could never succeed at walk time.
    fn deserialize<D>(d: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        use super::types::Transform;
        use serde::de::Error;
        let mapping = serde_yaml::Mapping::deserialize(d)?;
        if mapping.len() != 1 {
            return Err(D::Error::custom(format!(
                "transform must be a single-entry mapping (got {} keys)",
                mapping.len()
            )));
        }
        let (key_v, val) = mapping.into_iter().next().unwrap();
        let key = key_v
            .as_str()
            .ok_or_else(|| D::Error::custom("transform key must be a string"))?
            .to_string();
        let u64_operand = |val: &Value| {
            val.as_u64()
                .ok_or_else(|| D::Error::custom(format!("{key}: <u64 operand> required")))
        };
        let empty_body = |val: &Value| match val {
            Value::Null => Ok(()),
            Value::Mapping(m) if m.is_empty() => Ok(()),
            _ => Err(D::Error::custom(format!(
                "{key}: takes no operand (use {{}})"
            ))),
        };
        match key.as_str() {
            "bitOr" => Ok(Transform::BitOr(u64_operand(&val)?)),
            "bitAnd" => Ok(Transform::BitAnd(u64_operand(&val)?)),
            "dropPrefix" => Ok(Transform::DropPrefix(u64_operand(&val)? as usize)),
            "reverseBytes" => {
                empty_body(&val)?;
                Ok(Transform::ReverseBytes)
            }
            "uuidFromBytes" => {
                empty_body(&val)?;
                Ok(Transform::UuidFromBytes)
            }
            "slice" => {
                #[derive(serde::Deserialize)]
                #[serde(deny_unknown_fields)]
                struct R {
                    at: usize,
                    #[serde(default)]
                    length: Option<usize>,
                }
                let r: R = serde_yaml::from_value(val)
                    .map_err(|e| D::Error::custom(format!("slice: {e}")))?;
                if r.length == Some(0) {
                    return Err(D::Error::custom("slice: length 0 can never capture bytes"));
                }
                Ok(Transform::Slice {
                    at: r.at,
                    length: r.length,
                })
            }
            "bits" => {
                #[derive(serde::Deserialize)]
                #[serde(deny_unknown_fields)]
                struct R {
                    mask: u64,
                    #[serde(default)]
                    shift: u32,
                }
                let r: R = serde_yaml::from_value(val)
                    .map_err(|e| D::Error::custom(format!("bits: {e}")))?;
                if r.mask == 0 {
                    return Err(D::Error::custom("bits: mask 0 always yields 0"));
                }
                if r.shift >= 64 {
                    return Err(D::Error::custom("bits: shift must be < 64"));
                }
                Ok(Transform::Bits {
                    mask: r.mask,
                    shift: r.shift,
                })
            }
            other => Err(D::Error::custom(format!(
                "unknown transform '{other}' (allowlist: bitOr, bitAnd, slice, dropPrefix, reverseBytes, uuidFromBytes, bits)"
            ))),
        }
    }
}

impl<'de> serde::Deserialize<'de> for AcquireSource {
    /// YAML form: a single-entry mapping whose key names the source
    /// (`bleAdvert`, `bleRead`, `userPrompt`).
    fn deserialize<D>(d: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        use serde::de::Error;
        let mapping = serde_yaml::Mapping::deserialize(d)?;
        if mapping.len() != 1 {
            return Err(D::Error::custom(format!(
                "acquireSource must be a single-entry mapping (got {} keys)",
                mapping.len()
            )));
        }
        let (key_v, body) = mapping.into_iter().next().unwrap();
        let key = key_v
            .as_str()
            .ok_or_else(|| D::Error::custom("acquireSource key must be a string"))?
            .to_string();
        match key.as_str() {
            "bleAdvert" => {
                #[derive(serde::Deserialize)]
                #[serde(rename_all = "camelCase")]
                struct R {
                    offset: u32,
                    length: u32,
                    encoding: super::types::Encoding,
                }
                let r: R = serde_yaml::from_value(body).map_err(D::Error::custom)?;
                Ok(AcquireSource::BleAdvert {
                    offset: r.offset,
                    length: r.length,
                    encoding: r.encoding,
                })
            }
            "bleRead" => {
                #[derive(serde::Deserialize)]
                #[serde(rename_all = "camelCase")]
                struct R {
                    gatt: String,
                    encoding: super::types::Encoding,
                }
                let r: R = serde_yaml::from_value(body).map_err(D::Error::custom)?;
                Ok(AcquireSource::BleRead {
                    gatt: r.gatt,
                    encoding: r.encoding,
                })
            }
            "userPrompt" => {
                #[derive(serde::Deserialize)]
                #[serde(rename_all = "camelCase")]
                struct R {
                    text: String,
                }
                let r: R = serde_yaml::from_value(body).map_err(D::Error::custom)?;
                Ok(AcquireSource::UserPrompt { text: r.text })
            }
            other => Err(D::Error::custom(format!(
                "unknown acquireSource '{other}' (allowlist: bleAdvert, bleRead, userPrompt)"
            ))),
        }
    }
}

impl serde::Serialize for super::types::AwaitSource {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        use super::types::AwaitSource;
        use serde::ser::SerializeMap;

        let mut source = serializer.serialize_map(Some(1))?;
        match self {
            AwaitSource::Read { gatt } => source.serialize_entry("read", gatt)?,
            AwaitSource::Notify {
                gatt,
                mode,
                seed_read,
            } => {
                #[derive(serde::Serialize)]
                #[serde(rename_all = "camelCase")]
                struct Notify<'a> {
                    gatt: &'a str,
                    mode: super::types::CccdMode,
                    #[serde(skip_serializing_if = "is_false")]
                    seed_read: bool,
                }

                fn is_false(value: &bool) -> bool {
                    !*value
                }

                source.serialize_entry(
                    "notify",
                    &Notify {
                        gatt,
                        mode: *mode,
                        seed_read: *seed_read,
                    },
                )?;
            }
        }
        source.end()
    }
}

impl<'de> serde::Deserialize<'de> for super::types::AwaitSource {
    /// YAML form: a single-entry mapping — `read: <gatt>` (bare string) or
    /// `notify: { gatt: <gatt>, mode: <notify|indicate>?, seedRead: <bool>? }`.
    /// The gatt name is resolved to a UUID by the loader's GATT pass before
    /// this typed decode.
    fn deserialize<D>(d: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        use super::types::AwaitSource;
        use serde::de::Error;
        let mapping = serde_yaml::Mapping::deserialize(d)?;
        if mapping.len() != 1 {
            return Err(D::Error::custom(format!(
                "awaitSource must be a single-entry mapping (got {} keys)",
                mapping.len()
            )));
        }
        let (key_v, body) = mapping.into_iter().next().unwrap();
        let key = key_v
            .as_str()
            .ok_or_else(|| D::Error::custom("awaitSource key must be a string"))?
            .to_string();
        match key.as_str() {
            "read" => {
                let gatt = body
                    .as_str()
                    .ok_or_else(|| D::Error::custom("read: <gatt> string required"))?
                    .to_string();
                Ok(AwaitSource::Read { gatt })
            }
            "notify" => {
                #[derive(serde::Deserialize)]
                #[serde(rename_all = "camelCase", deny_unknown_fields)]
                struct R {
                    gatt: String,
                    #[serde(default)]
                    mode: super::types::CccdMode,
                    #[serde(default)]
                    seed_read: bool,
                }
                let r: R = serde_yaml::from_value(body)
                    .map_err(|e| D::Error::custom(format!("notify: {e}")))?;
                Ok(AwaitSource::Notify {
                    gatt: r.gatt,
                    mode: r.mode,
                    seed_read: r.seed_read,
                })
            }
            other => Err(D::Error::custom(format!(
                "unknown awaitSource '{other}' (allowlist: read, notify)"
            ))),
        }
    }
}

impl<'de> serde::Deserialize<'de> for super::types::AdvertPredicate {
    /// YAML form: a single-entry mapping whose key names the predicate kind
    /// (`manufacturerData`, `serviceUuids`, `serviceData`, `localName`,
    /// `txPower`, `rawAdRecord`) or combinator (`all`, `any`, `not`).
    /// Recursive for combinators.
    fn deserialize<D>(d: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        use super::types::AdvertPredicate as P;
        use serde::de::Error;
        let mapping = serde_yaml::Mapping::deserialize(d)?;
        if mapping.len() != 1 {
            return Err(D::Error::custom(format!(
                "advert predicate must be a single-entry mapping (got {} keys); wrap siblings in all:/any:",
                mapping.len()
            )));
        }
        let (key_v, body) = mapping.into_iter().next().unwrap();
        let key = key_v
            .as_str()
            .ok_or_else(|| D::Error::custom("advert predicate key must be a string"))?
            .to_string();
        let dec = |what: &str, e: serde_yaml::Error| D::Error::custom(format!("{what}: {e}"));
        match key.as_str() {
            "all" => Ok(P::All(
                serde_yaml::from_value(body).map_err(|e| dec("all", e))?,
            )),
            "any" => Ok(P::Any(
                serde_yaml::from_value(body).map_err(|e| dec("any", e))?,
            )),
            "not" => Ok(P::Not(Box::new(
                serde_yaml::from_value(body).map_err(|e| dec("not", e))?,
            ))),
            "manufacturerData" => Ok(P::ManufacturerData(
                serde_yaml::from_value(body).map_err(|e| dec("manufacturerData", e))?,
            )),
            "serviceUuids" => {
                #[derive(serde::Deserialize)]
                #[serde(deny_unknown_fields)]
                struct R {
                    contains: String,
                }
                let r: R = serde_yaml::from_value(body).map_err(|e| dec("serviceUuids", e))?;
                Ok(P::ServiceUuids { contains: r.contains })
            }
            "serviceData" => {
                #[derive(serde::Deserialize)]
                #[serde(rename_all = "camelCase")]
                struct R {
                    uuid: String,
                    #[serde(flatten)]
                    payload: super::types::PayloadPredicate,
                }
                let r: R = serde_yaml::from_value(body).map_err(|e| dec("serviceData", e))?;
                Ok(P::ServiceData {
                    uuid: r.uuid,
                    payload: r.payload,
                })
            }
            "localName" => Ok(P::LocalName(
                serde_yaml::from_value(body).map_err(|e| dec("localName", e))?,
            )),
            "txPower" => {
                #[derive(serde::Deserialize)]
                #[serde(deny_unknown_fields)]
                struct R {
                    #[serde(default)]
                    min: Option<i8>,
                    #[serde(default)]
                    max: Option<i8>,
                }
                let r: R = serde_yaml::from_value(body).map_err(|e| dec("txPower", e))?;
                Ok(P::TxPower {
                    min: r.min,
                    max: r.max,
                })
            }
            "rawAdRecord" => {
                #[derive(serde::Deserialize)]
                #[serde(rename_all = "camelCase")]
                struct R {
                    ad_type: u8,
                    #[serde(flatten)]
                    payload: super::types::PayloadPredicate,
                }
                let r: R = serde_yaml::from_value(body).map_err(|e| dec("rawAdRecord", e))?;
                Ok(P::RawAdRecord {
                    ad_type: r.ad_type,
                    payload: r.payload,
                })
            }
            other => Err(D::Error::custom(format!(
                "unknown advert predicate '{other}' (allowlist: all, any, not, manufacturerData, serviceUuids, serviceData, localName, txPower, rawAdRecord)"
            ))),
        }
    }
}

impl<'de> serde::Deserialize<'de> for super::types::AdvertByteSource {
    /// YAML form: bare string (`manufacturerData`, `localName`) or a
    /// single-entry mapping (`{ rawAdRecord: <adType> }`,
    /// `{ serviceData: "<uuid>" }`).
    fn deserialize<D>(d: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        use super::types::AdvertByteSource as S;
        use serde::de::Error;
        let v = Value::deserialize(d)?;
        match v {
            Value::String(s) => match s.as_str() {
                "manufacturerData" => Ok(S::ManufacturerData),
                "localName" => Ok(S::LocalName),
                other => Err(D::Error::custom(format!(
                    "unknown capture source '{other}' (bare-string allowlist: manufacturerData, localName)"
                ))),
            },
            Value::Mapping(m) => {
                if m.len() != 1 {
                    return Err(D::Error::custom(
                        "capture source mapping must have exactly one key",
                    ));
                }
                let (key_v, body) = m.into_iter().next().unwrap();
                let key = key_v
                    .as_str()
                    .ok_or_else(|| D::Error::custom("capture source key must be a string"))?;
                match key {
                    "rawAdRecord" => {
                        let ad_type = body.as_u64().filter(|n| *n <= 0xFF).ok_or_else(|| {
                            D::Error::custom("rawAdRecord: <u8 AD type> required")
                        })?;
                        Ok(S::RawAdRecord {
                            ad_type: ad_type as u8,
                        })
                    }
                    "serviceData" => {
                        let uuid = body
                            .as_str()
                            .ok_or_else(|| D::Error::custom("serviceData: <uuid string> required"))?
                            .to_string();
                        Ok(S::ServiceData { uuid })
                    }
                    other => Err(D::Error::custom(format!(
                        "unknown capture source '{other}' (mapping allowlist: rawAdRecord, serviceData)"
                    ))),
                }
            }
            _ => Err(D::Error::custom("capture source: invalid shape")),
        }
    }
}

impl<'de> serde::Deserialize<'de> for BleNotifyUntil {
    /// YAML form: either the bare string `any`, or a single-entry mapping
    /// (`{ equals: <value>, encoding: <name>? }` or `{ matches: <regex> }`).
    fn deserialize<D>(d: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        use serde::de::Error;
        let v = Value::deserialize(d)?;
        match v {
            Value::String(s) if s == "any" => Ok(BleNotifyUntil::Any),
            Value::String(s) => Err(D::Error::custom(format!(
                "bleNotify until: '{s}' (only the bare 'any' string form is supported; use a mapping for equals/matches)"
            ))),
            Value::Mapping(m) => {
                if let Some(eq_val) = m.get(Value::String("equals".into())) {
                    let encoding = match m.get(Value::String("encoding".into())) {
                        Some(v) => Some(
                            serde_yaml::from_value::<super::types::Encoding>(v.clone())
                                .map_err(D::Error::custom)?,
                        ),
                        None => None,
                    };
                    return Ok(BleNotifyUntil::Equals {
                        value: eq_val.clone(),
                        encoding,
                    });
                }
                if let Some(pat) = m.get(Value::String("matches".into())) {
                    let pattern = pat
                        .as_str()
                        .ok_or_else(|| D::Error::custom("matches: <string> required"))?
                        .to_string();
                    return Ok(BleNotifyUntil::Matches { pattern });
                }
                Err(D::Error::custom(
                    "bleNotify until: expected `any`, `{equals:...}`, or `{matches:...}`",
                ))
            }
            _ => Err(D::Error::custom("bleNotify until: invalid shape")),
        }
    }
}
