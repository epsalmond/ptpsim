//! In-memory BLE GATT responder + reference establishment walker (issue #25
//! Phase 1). The simulator side of the BLE pair flow: the responder plays the
//! camera (advert constants, GATT catalog, per-characteristic read/notify
//! policy from manifest data + test scripting), the walker plays the app
//! dispatcher, executing a manifest establishment plan against it. No real
//! radio — Phase 2 (BlueZ) would implement the same surface over a stack.
//!
//! Like the PTP/IP [`crate::Engine`], this is generic: vendor behavior comes
//! from manifest data and per-test policy, never from code branches. The
//! walker doubles as the executable reference for what a platform dispatcher
//! must do per verb (resolution semantics delegate to
//! `camera_config::index::eval`, the engine-owned spec).

use std::collections::{BTreeMap, BTreeSet};

use camera_config::index::eval;
use camera_config::index::{
    AcquireSource, AwaitSource, BleAwaitUntilStep, BleNotifyUntil, CccdMode, Encoding,
    NotifyCapture, PredicateOp, Step, StepValue,
};

/// Reference-walker bound on a `bleAwaitUntil` loop: the deterministic
/// analogue of the dispatcher's wall-clock `timeout_ms`. A sticky-unsatisfied
/// source (a `serve_read` value that never meets `until`) hits this and fails
/// like a real timeout rather than spinning forever.
const MAX_AWAIT_ITERS: usize = 256;

/// One interaction the responder observed, in arrival order. Tests assert on
/// this log to prove a plan drove the camera in the expected reference app order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BleEvent {
    Connect,
    RequestMtu { requested: u16, negotiated: u16 },
    DiscoverServices,
    Read { uuid: String },
    Write { uuid: String, value: Vec<u8> },
    Subscribe { uuid: String, mode: CccdMode },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BleError {
    NotConnected,
    /// The characteristic isn't in this body's exposed catalog (or, for a
    /// read, has no value policy) — the in-memory analogue of a GATT
    /// attribute-not-found error.
    NotExposed(String),
}

impl std::fmt::Display for BleError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BleError::NotConnected => write!(f, "peripheral not connected"),
            BleError::NotExposed(uuid) => write!(f, "characteristic {uuid} not exposed"),
        }
    }
}

/// Deterministic in-memory GATT peripheral. Construct from the manifest's
/// symbolic-name → UUID catalog, then script per-characteristic behavior:
/// [`serve_read`](Self::serve_read) values and
/// [`queue_notification`](Self::queue_notification) payloads.
///
/// A characteristic in the catalog accepts writes and CCCD subscriptions; a
/// read additionally needs a served value (a catalogued-but-unserved
/// characteristic read fails like a real body that doesn't expose it — the
/// LEGACY `deviceIdentificationNumber` case).
pub struct BleResponder {
    catalog: BTreeSet<String>,
    read_values: BTreeMap<String, Vec<u8>>,
    /// Per-read evolving values (for `bleAwaitUntil` read-poll loops): each
    /// read pops the next, the last value is sticky. Checked before
    /// `read_values`.
    read_sequences: BTreeMap<String, Vec<Vec<u8>>>,
    notify_queues: BTreeMap<String, Vec<Vec<u8>>>,
    mtu_cap: u16,
    connected: bool,
    services_discovered: bool,
    log: Vec<BleEvent>,
}

impl BleResponder {
    /// `catalog` is the manifest's `gatt:` map values (UUIDs). Names are not
    /// kept — steps arrive with UUIDs already resolved by the loader (§11.3).
    pub fn new<I: IntoIterator<Item = String>>(catalog: I) -> Self {
        BleResponder {
            catalog: catalog.into_iter().collect(),
            read_values: BTreeMap::new(),
            read_sequences: BTreeMap::new(),
            notify_queues: BTreeMap::new(),
            mtu_cap: 247,
            connected: false,
            services_discovered: false,
            log: Vec::new(),
        }
    }

    /// Serve `bytes` on every read of `uuid` (also adds it to the catalog).
    pub fn serve_read(mut self, uuid: &str, bytes: &[u8]) -> Self {
        self.catalog.insert(uuid.to_string());
        self.read_values.insert(uuid.to_string(), bytes.to_vec());
        self
    }

    /// Serve an evolving sequence of read values on `uuid`: each `read` pops
    /// the next, the last value is sticky (returned on every read once the
    /// sequence is down to one). Drives `bleAwaitUntil` read-poll loops —
    /// e.g. `serve_read_sequence(color, [0, 0, 1])` models AF focusing then
    /// locking. Takes precedence over `serve_read` for the same uuid.
    pub fn serve_read_sequence(mut self, uuid: &str, values: Vec<Vec<u8>>) -> Self {
        self.catalog.insert(uuid.to_string());
        self.read_sequences.insert(uuid.to_string(), values);
        self
    }

    /// Queue a notification payload on `uuid` (FIFO; one per `bleNotify`).
    pub fn queue_notification(mut self, uuid: &str, payload: &[u8]) -> Self {
        self.catalog.insert(uuid.to_string());
        self.notify_queues
            .entry(uuid.to_string())
            .or_default()
            .push(payload.to_vec());
        self
    }

    /// Cap the negotiable ATT MTU (default 247, typical BLE 5 stack).
    pub fn with_mtu_cap(mut self, cap: u16) -> Self {
        self.mtu_cap = cap;
        self
    }

    pub fn connect(&mut self) {
        self.connected = true;
        // In-memory stack auto-discovers, like CoreBluetooth's connect path;
        // bleDiscoverServices is then a checkpoint (§11.4a).
        self.services_discovered = true;
        self.log.push(BleEvent::Connect);
    }

    pub fn request_mtu(&mut self, requested: u16) -> Result<u16, BleError> {
        if !self.connected {
            return Err(BleError::NotConnected);
        }
        let negotiated = requested.min(self.mtu_cap);
        self.log.push(BleEvent::RequestMtu {
            requested,
            negotiated,
        });
        Ok(negotiated)
    }

    pub fn discover_services(&mut self) -> Result<(), BleError> {
        if !self.connected {
            return Err(BleError::NotConnected);
        }
        self.services_discovered = true;
        self.log.push(BleEvent::DiscoverServices);
        Ok(())
    }

    fn require_char(&self, uuid: &str) -> Result<(), BleError> {
        if !self.connected {
            return Err(BleError::NotConnected);
        }
        if !self.catalog.contains(uuid) {
            return Err(BleError::NotExposed(uuid.to_string()));
        }
        Ok(())
    }

    pub fn read(&mut self, uuid: &str) -> Result<Vec<u8>, BleError> {
        self.require_char(uuid)?;
        self.log.push(BleEvent::Read {
            uuid: uuid.to_string(),
        });
        // Sequenced reads (await loops) take precedence: advance while more
        // than one remains, stick on the last.
        if let Some(seq) = self.read_sequences.get_mut(uuid) {
            if seq.len() > 1 {
                return Ok(seq.remove(0));
            }
            if let Some(last) = seq.first() {
                return Ok(last.clone());
            }
        }
        self.read_values
            .get(uuid)
            .cloned()
            .ok_or_else(|| BleError::NotExposed(uuid.to_string()))
    }

    pub fn write(&mut self, uuid: &str, value: &[u8]) -> Result<(), BleError> {
        self.require_char(uuid)?;
        self.log.push(BleEvent::Write {
            uuid: uuid.to_string(),
            value: value.to_vec(),
        });
        Ok(())
    }

    /// CCCD descriptor write — success IS the ack (§11.8 `bleSubscribe`).
    pub fn subscribe(&mut self, uuid: &str, mode: CccdMode) -> Result<(), BleError> {
        self.require_char(uuid)?;
        self.log.push(BleEvent::Subscribe {
            uuid: uuid.to_string(),
            mode,
        });
        Ok(())
    }

    /// Pop the next queued notification payload for `uuid`, if any.
    pub fn take_notification(&mut self, uuid: &str) -> Option<Vec<u8>> {
        let queue = self.notify_queues.get_mut(uuid)?;
        if queue.is_empty() {
            None
        } else {
            Some(queue.remove(0))
        }
    }

    /// Every interaction, in order.
    pub fn log(&self) -> &[BleEvent] {
        &self.log
    }

    /// Convenience: the payloads written to `uuid`, in order.
    pub fn written(&self, uuid: &str) -> Vec<&[u8]> {
        self.log
            .iter()
            .filter_map(|e| match e {
                BleEvent::Write { uuid: u, value } if u == uuid => Some(value.as_slice()),
                _ => None,
            })
            .collect()
    }

    /// Convenience: the CCCD-subscribed UUIDs, in order.
    pub fn subscribed(&self) -> Vec<&str> {
        self.log
            .iter()
            .filter_map(|e| match e {
                BleEvent::Subscribe { uuid, .. } => Some(uuid.as_str()),
                _ => None,
            })
            .collect()
    }
}

/// Walk failure: which step (by verb + position) and why. Tolerant steps
/// never produce one — their failures are skipped like a real dispatcher.
#[derive(Debug)]
pub struct WalkError {
    pub step: String,
    pub message: String,
}

impl std::fmt::Display for WalkError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.step, self.message)
    }
}

/// Result of a completed walk: the final scope (recognition seed + step
/// captures) and how many steps ran (if-branches counted inside-out).
pub struct WalkOutcome {
    pub scope: BTreeMap<String, String>,
    pub steps_run: usize,
}

struct WalkCtx<'a> {
    responder: &'a mut BleResponder,
    scope: BTreeMap<String, String>,
    /// Encoding each scope key was captured with — lets `{ captured: … }`
    /// writes re-encode integer captures (the RED `idNumber` u32) instead
    /// of guessing from the scope string.
    encodings: BTreeMap<String, Encoding>,
    runtime_params: BTreeMap<String, String>,
    steps_run: usize,
}

/// Execute an establishment plan against the responder — the reference
/// dispatcher. `retries`/`retryDelayMs` are honoured as semantics but not as
/// timing: the responder is deterministic, so a step either succeeds on the
/// first try or never (a retry loop would spin on the same answer). Regex
/// `until: matches` is unsupported here (the engine deliberately carries no
/// regex dependency); plans using it need a platform dispatcher.
///
/// `initial_encodings` carries the encoding each recognition-seeded capture
/// decoded with (`eval::advert_capture_encodings`), seeding `ctx.encodings`
/// just as in-walk `bleRead`/`bleNotify` captures do. Without it a later
/// `{ captured: … }` write-back of an advert capture falls back to the
/// scope-string heuristic, which silently hex-decodes an even-length all-hex
/// ASCII value instead of writing its bytes (#43).
pub fn walk_establishment(
    responder: &mut BleResponder,
    steps: &[Step],
    initial_scope: &BTreeMap<String, String>,
    initial_encodings: &BTreeMap<String, Encoding>,
    runtime_params: &BTreeMap<String, String>,
) -> Result<WalkOutcome, WalkError> {
    let mut ctx = WalkCtx {
        responder,
        scope: initial_scope.clone(),
        encodings: initial_encodings.clone(),
        runtime_params: runtime_params.clone(),
        steps_run: 0,
    };
    walk_steps(&mut ctx, steps, "steps")?;
    Ok(WalkOutcome {
        scope: ctx.scope,
        steps_run: ctx.steps_run,
    })
}

fn walk_steps(ctx: &mut WalkCtx<'_>, steps: &[Step], path: &str) -> Result<(), WalkError> {
    for (i, step) in steps.iter().enumerate() {
        let here = format!("{path}[{i}].{}", step.verb_name());
        let tolerant = match step {
            Step::If(s) => s.tolerant, // §11.6: If's tolerant gates predicate fields, not body errors
            other => other.options().tolerant,
        };
        match run_step(ctx, step, &here) {
            Ok(()) => ctx.steps_run += 1,
            Err(e) if tolerant && !matches!(step, Step::If(_)) => {
                // Tolerant step failure: skip and continue (§11.6).
                let _ = e;
                ctx.steps_run += 1;
            }
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// The scope slot an `acquire` delegate binds its result to — the delegate's
/// own explicit `capture_as`. `acquire` aliases THIS slot under its `name`, so
/// a delegate without one (e.g. a `bleNotify` using only field `capture`s) has
/// nothing for acquire to bind and is rejected rather than guessed at.
fn primary_capture_name(step: &Step) -> Option<&str> {
    match step {
        Step::BleRead(s) => Some(&s.capture_as),
        Step::BleNotify(s) => s.capture_as.as_deref(),
        Step::BleAwaitUntil(s) => s.capture_as.as_deref(),
        _ => None,
    }
}

fn run_step(ctx: &mut WalkCtx<'_>, step: &Step, here: &str) -> Result<(), WalkError> {
    let err = |message: String| WalkError {
        step: here.to_string(),
        message,
    };
    match step {
        Step::BleConnect(_) => {
            ctx.responder.connect();
            Ok(())
        }
        Step::BleRequestMtu(s) => {
            let negotiated = ctx
                .responder
                .request_mtu(s.mtu)
                .map_err(|e| err(e.to_string()))?;
            if negotiated < s.mtu {
                return Err(err(format!(
                    "negotiated MTU {negotiated} < required {}",
                    s.mtu
                )));
            }
            Ok(())
        }
        Step::BleDiscoverServices(_) => ctx
            .responder
            .discover_services()
            .map_err(|e| err(e.to_string())),
        Step::BleRead(s) => {
            let wire = ctx
                .responder
                .read(&s.gatt)
                .map_err(|e| err(e.to_string()))?;
            // §11.13 capture pipeline: bytes → transform chain → encoding.
            let bytes = eval::apply_transforms(&wire, &s.transform)
                .ok_or_else(|| err("transform chain failed".into()))?;
            let value = eval::decode_bytes(&bytes, s.encoding)
                .ok_or_else(|| err(format!("decode as {} failed", s.encoding.as_token())))?;
            ctx.scope.insert(s.capture_as.clone(), value);
            ctx.encodings.insert(s.capture_as.clone(), s.encoding);
            Ok(())
        }
        Step::BleWrite(s) => {
            let bytes = resolve_value(ctx, &s.value).map_err(err)?;
            ctx.responder
                .write(&s.gatt, &bytes)
                .map_err(|e| err(e.to_string()))
        }
        Step::BleSubscribe(s) => ctx
            .responder
            .subscribe(&s.gatt, s.mode)
            .map_err(|e| err(e.to_string())),
        Step::BleNotify(s) => {
            ctx.responder
                .subscribe(&s.gatt, s.mode)
                .map_err(|e| err(e.to_string()))?;
            let payload = ctx
                .responder
                .take_notification(&s.gatt)
                .ok_or_else(|| err("no notification arrived (queue empty)".into()))?;
            let accepted = match &s.until {
                BleNotifyUntil::Any => true,
                BleNotifyUntil::Equals { value, encoding } => {
                    let want = eval::yaml_literal_to_bytes(value, *encoding)
                        .ok_or_else(|| err("until.equals value undecodable".into()))?;
                    payload == want
                }
                BleNotifyUntil::Matches { .. } => {
                    return Err(err(
                        "until.matches (regex) is unsupported in the reference walker".into(),
                    ));
                }
            };
            if !accepted {
                return Err(err("notification payload did not satisfy until".into()));
            }
            apply_value_captures(ctx, &payload, &s.capture_as, &s.capture);
            Ok(())
        }
        Step::BleAwaitUntil(s) => run_await_until(ctx, s, here),
        Step::Acquire(s) => {
            // Run the delegate through `walk_steps` (a one-element slice) so its
            // OWN tolerant/retry options apply at its level rather than the
            // acquire's (#44 finding 2). Then alias the slot the delegate
            // explicitly declared it captures into, by name — not whatever key
            // a scope set-diff happens to surface, which mis-picks the
            // lexicographically-smallest key on a multi-capture delegate and
            // silently aliases nothing when the delegate overwrites a
            // pre-existing (e.g. recognize-seeded) key (#44 finding 1).
            walk_steps(ctx, std::slice::from_ref(s.from.as_ref()), &format!("{here}.from"))?;
            let target = primary_capture_name(&s.from).ok_or_else(|| {
                err(format!(
                    "acquire delegate `{}` declares no capture_as to bind",
                    s.from.verb_name()
                ))
            })?;
            // A tolerant delegate that failed bound nothing — there is then no
            // value to alias, and that is not an error (its own tolerance
            // already decided to continue).
            if let Some(v) = ctx.scope.get(target).cloned() {
                if let Some(enc) = ctx.encodings.get(target).copied() {
                    ctx.encodings.insert(s.name.clone(), enc);
                }
                ctx.scope.insert(s.name.clone(), v);
            }
            Ok(())
        }
        Step::AcquireFirmware(s) => match &s.from {
            AcquireSource::BleRead { gatt, encoding } => {
                let wire = ctx.responder.read(gatt).map_err(|e| err(e.to_string()))?;
                let value = eval::decode_bytes(&wire, *encoding)
                    .ok_or_else(|| err(format!("decode as {} failed", encoding.as_token())))?;
                ctx.scope.insert("firmware".to_string(), value);
                ctx.encodings.insert("firmware".to_string(), *encoding);
                Ok(())
            }
            other => Err(err(format!(
                "acquireFirmware source {other:?} unsupported in the reference walker"
            ))),
        },
        Step::If(s) => {
            let field_value = ctx.scope.get(&s.condition.field);
            let holds = match field_value {
                None if s.tolerant => false, // §11.6: unbound field → false
                None => {
                    return Err(err(format!(
                        "predicate field '{}' unbound in scope",
                        s.condition.field
                    )));
                }
                Some(actual) => predicate_holds(actual, s.condition.op, &s.condition.value),
            };
            let branch = if holds { &s.then } else { &s.else_branch };
            let branch_path = format!("{here}.{}", if holds { "then" } else { "else" });
            walk_steps(ctx, branch, &branch_path)
        }
    }
}

/// Bind a value (read result / notification payload) into scope: the whole
/// value under `capture_as` (hex), then each field capture through the
/// §11.13 pipeline (window → transform → encoding). Fail-soft: a capture
/// whose window/transform/decode fails is skipped, never an error. Shared by
/// `bleNotify` and each `bleAwaitUntil` iteration.
fn apply_value_captures(
    ctx: &mut WalkCtx<'_>,
    value: &[u8],
    capture_as: &Option<String>,
    captures: &[NotifyCapture],
) {
    if let Some(name) = capture_as {
        ctx.scope.insert(name.clone(), eval::hex_lower(value));
        ctx.encodings.insert(name.clone(), Encoding::Bytes);
    }
    for cap in captures {
        let end = match cap.length {
            Some(l) => cap.at.saturating_add(l),
            None => value.len(),
        };
        if cap.at > value.len() || end > value.len() {
            continue;
        }
        let Some(bytes) = eval::apply_transforms(&value[cap.at..end], &cap.transform) else {
            continue;
        };
        if let Some(decoded) = eval::decode_bytes(&bytes, cap.encoding) {
            ctx.scope.insert(cap.name.clone(), decoded);
            ctx.encodings.insert(cap.name.clone(), cap.encoding);
        }
    }
}

/// Execute a `bleAwaitUntil` step (§11.15): observe the source until `until`
/// holds over scope, running `on_each` between unsatisfied iterations.
/// Deterministic timeout = source exhaustion or [`MAX_AWAIT_ITERS`].
fn run_await_until(
    ctx: &mut WalkCtx<'_>,
    s: &BleAwaitUntilStep,
    here: &str,
) -> Result<(), WalkError> {
    let err = |message: String| WalkError {
        step: here.to_string(),
        message,
    };
    // CCCD-enable once up front for a notify source (the camera then streams).
    if let AwaitSource::Notify { gatt, mode } = &s.source {
        ctx.responder
            .subscribe(gatt, *mode)
            .map_err(|e| err(e.to_string()))?;
    }
    for _ in 0..MAX_AWAIT_ITERS {
        // Observe one value.
        let value = match &s.source {
            AwaitSource::Read { gatt } => {
                ctx.responder.read(gatt).map_err(|e| err(e.to_string()))?
            }
            AwaitSource::Notify { gatt, .. } => match ctx.responder.take_notification(gatt) {
                Some(p) => p,
                None => {
                    return Err(err(
                        "awaited notification never arrived (source exhausted before `until`)"
                            .into(),
                    ))
                }
            },
        };
        apply_value_captures(ctx, &value, &s.capture_as, &s.capture);

        // Satisfied? `until` is a Predicate over scope (the `if` vocabulary).
        let satisfied = match ctx.scope.get(&s.until.field) {
            Some(actual) => predicate_holds(actual, s.until.op, &s.until.value),
            // An unbound field can't satisfy the condition; keep observing
            // (a capture too short to bind it is the deterministic analogue
            // of "the camera hasn't reported it yet").
            None => false,
        };
        if satisfied {
            return Ok(());
        }
        // Not yet: act, then observe again. interval_ms is dispatcher cadence
        // — the deterministic walker doesn't sleep.
        walk_steps(ctx, &s.on_each, &format!("{here}.onEach"))?;
    }
    Err(err(format!(
        "`until` ({} {} {}) not satisfied within {MAX_AWAIT_ITERS} observations",
        s.until.field,
        s.until.op.as_token(),
        s.until.value
    )))
}

fn predicate_holds(actual: &str, op: PredicateOp, expected: &str) -> bool {
    // Numeric compare when both sides parse; string compare otherwise.
    let nums = (actual.parse::<i64>().ok(), expected.parse::<i64>().ok());
    match op {
        PredicateOp::Eq => actual == expected,
        PredicateOp::Ne => actual != expected,
        PredicateOp::Gt | PredicateOp::Gte | PredicateOp::Lt | PredicateOp::Lte => {
            let ord = match nums {
                (Some(a), Some(b)) => a.cmp(&b),
                _ => actual.cmp(expected),
            };
            match op {
                PredicateOp::Gt => ord.is_gt(),
                PredicateOp::Gte => ord.is_ge(),
                PredicateOp::Lt => ord.is_lt(),
                PredicateOp::Lte => ord.is_le(),
                _ => unreachable!(),
            }
        }
        PredicateOp::In => expected.split(',').map(str::trim).any(|v| v == actual),
    }
}

fn resolve_value(ctx: &WalkCtx<'_>, value: &StepValue) -> Result<Vec<u8>, String> {
    match value {
        StepValue::Literal { literal } => eval::yaml_literal_to_bytes(literal, None)
            .ok_or_else(|| "literal undecodable".to_string()),
        StepValue::Template {
            template,
            transform,
        } => {
            let mut out = String::new();
            let mut rest = template.as_str();
            while let Some(open) = rest.find('{') {
                out.push_str(&rest[..open]);
                let Some(close) = rest[open..].find('}') else {
                    return Err(format!("template '{template}': unclosed brace"));
                };
                let name = &rest[open + 1..open + close];
                let v = ctx
                    .scope
                    .get(name)
                    .or_else(|| ctx.runtime_params.get(name))
                    .ok_or_else(|| format!("template ref '{{{name}}}' unbound"))?;
                out.push_str(v);
                rest = &rest[open + close + 1..];
            }
            out.push_str(rest);
            eval::apply_transforms(out.as_bytes(), transform)
                .ok_or_else(|| "transform chain failed".to_string())
        }
        StepValue::Runtime {
            runtime,
            encoding,
            transform,
        } => {
            let v = ctx
                .runtime_params
                .get(runtime)
                .ok_or_else(|| format!("runtime slot '{runtime}' unbound"))?;
            let bytes = eval::scope_string_to_bytes(v, *encoding)
                .ok_or_else(|| format!("runtime '{runtime}' undecodable"))?;
            eval::apply_transforms(&bytes, transform)
                .ok_or_else(|| "transform chain failed".to_string())
        }
        StepValue::Captured {
            captured,
            transform,
        } => {
            let v = ctx
                .scope
                .get(captured)
                .ok_or_else(|| format!("captured '{captured}' unbound in scope"))?;
            let bytes = eval::scope_string_to_bytes(v, ctx.encodings.get(captured).copied())
                .ok_or_else(|| format!("captured '{captured}' undecodable"))?;
            eval::apply_transforms(&bytes, transform)
                .ok_or_else(|| "transform chain failed".to_string())
        }
    }
}
