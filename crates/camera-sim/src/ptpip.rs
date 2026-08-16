//! Reference executor for the **PTP-IP action/mode-entry step grammar**
//! (`camera_config::model::Step`). The sibling of the BLE reference walker
//! [`crate::ble`]: it plays the iOS dispatcher, driving the generic
//! [`crate::Engine`] (the PTP responder) through a manifest step sequence and
//! validating the new `awaitUntil` poll-until verb end-to-end.
//!
//! Like the BLE walker, this is the executable reference a platform dispatcher
//! must match — not a second responder. The Engine already models property
//! state, `on_operation`, and the computed `0xd212` bundle; this walker only
//! translates each `Step` into `OperationRequest`s and accumulates the observed
//! values into a [`PropView`] — the PTP-IP **scope**. A `getProp` IS the capture
//! (the typed value lands keyed by prop code), so `until` reuses the existing
//! PTP [`camera_config::Predicate`] over that view with no byte-window pipeline.
//!
//! The walker is sans-io and generic: vendor behavior lives in manifest data
//! (op-effects, property datatypes), never in code branches here.

use std::collections::BTreeMap;

use camera_config::model::{
    AwaitSource, AwaitUntil, Capture, CaptureSource, ChunkSize, Loop, MissingRuntimeValue,
    SetPropValue, Step, StepParam,
};
use camera_config::{parse_hex_code, PropView, RetryFailureClass};
use camera_media_store::ByteSource;
use protocol_primitives::quirk::{
    parse_typed_record_stream, RecordStreamDescriptor, RecordStreamLayout, RecordValueEncoding,
};
use ptp_core::codes::{op, resp};
use ptp_core::dataset::PropValue;
use ptp_core::{ObjectInfo, OperationRequest, Reader, Writer};

use crate::engine::{Engine, Reply};
use crate::state::{datatype_of, typed, Phase};

#[cfg(test)]
use std::sync::atomic::{AtomicU64, Ordering};

#[cfg(test)]
fn unique_temp_root(prefix: &str) -> std::path::PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let count = COUNTER.fetch_add(1, Ordering::SeqCst);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!(
        "{prefix}-{nanos}-{count}-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ))
}

/// Reference-executor bound on an `awaitUntil` loop: the deterministic analogue
/// of the dispatcher's wall-clock `timeout_ms` (§11.15). A condition that never
/// holds hits this and fails like a real timeout rather than spinning forever.
/// Mirrors `crate::ble::MAX_AWAIT_ITERS`.
const MAX_AWAIT_ITERS: usize = 256;

/// Defensive runaway guard on a `forEach` loop (#46). The handle list is a finite
/// `Vec` the engine returns, so this is never hit in practice — it bounds a
/// corrupt array count, not real iteration. Set high enough for any real card.
const MAX_FOREACH_ITERS: usize = 100_000;

/// Collection captures feed `forEach`, so reject a declared count the walker
/// could never consume before allocating or reading the payload.
const MAX_CAPTURED_U32S: u32 = MAX_FOREACH_ITERS as u32;

/// Deterministic cap on a `chunk` loop's windows. Covers the 4 GiB `SIZE_CEILING`
/// at any realistic chunk size (4 GiB / 12 MiB ≈ 358) with wide headroom; a
/// degenerate tiny window hits it instead of spinning. The §11.15 analogue.
const MAX_CHUNK_ITERS: usize = 4096;

/// Reserved scope slot the executor binds from a `GetObjectInfo` (0x1008) response
/// — the object's compressed size, sourced for a following `chunk` loop's `total`
/// exactly as a real client learns it (from the ObjectInfo data phase). See #46.
const OBJECT_SIZE_SLOT: &str = "objectSize";

/// Result of a completed PTP-IP walk: the observed property values (the scope
/// the dispatcher accumulated from polls) and per-`awaitUntil` iteration counts
/// (so tests can assert a poll-until loop actually iterated before satisfying).
#[derive(Debug)]
pub struct PtpIpOutcome {
    pub observed: PropView,
    pub steps_run: usize,
    /// One entry per `awaitUntil` step executed, in order: how many polls it
    /// took to satisfy `until`.
    pub await_iterations: Vec<usize>,
    /// One entry per `loop` step executed, in order: how many iterations it ran
    /// (forEach element count / chunk window count). Lets tests assert a transfer
    /// actually walked the handles and chunked each object.
    pub loop_iterations: Vec<usize>,
    /// Manifest-declared backoffs consumed by response-selected retries.
    pub retry_delays_ms: Vec<u32>,
}

/// Walk failure: which step (by verb + position) and why. Tolerant steps never
/// produce one — their non-OK responses are skipped like a real dispatcher.
#[derive(Debug)]
pub struct PtpIpError {
    pub step: String,
    pub message: String,
    pub response_code: Option<u16>,
}

impl std::fmt::Display for PtpIpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.step, self.message)
    }
}

/// Execute a PTP-IP step sequence against the engine — the reference dispatcher.
/// Opens a session up front (the establishment that precedes mode-entry/action
/// steps is out of this grammar's scope), then walks `steps`. `runtime_params`
/// bind `StepParam::Runtime` slots (e.g. the live-view open-capture txid).
pub fn walk_ptpip(
    engine: &mut Engine,
    steps: &[Step],
    runtime_params: &BTreeMap<String, String>,
) -> Result<PtpIpOutcome, PtpIpError> {
    walk_ptpip_in(engine, steps, runtime_params, None)
}

/// Like [`walk_ptpip`], but bound to a named connection so its traits gate the
/// walk. Specifically `command_listener_volatile` (#103): a `reopenSession`
/// from active live-view is refused because the camera tears down the command
/// listener on that transport-close. The image-import reverse edge can still
/// reconnect because no open-capture stream owns the command listener there.
pub fn walk_ptpip_in(
    engine: &mut Engine,
    steps: &[Step],
    runtime_params: &BTreeMap<String, String>,
    connection: Option<&str>,
) -> Result<PtpIpOutcome, PtpIpError> {
    engine.bind_connection(connection.unwrap_or(Engine::DEFAULT_CONNECTION));
    let command_listener_volatile = connection
        .and_then(|id| engine.manifest().connections.get(id))
        .is_some_and(|c| c.command_listener_volatile);
    let session_already_open = engine.state().session_open;
    let mut ctx = Ctx {
        engine,
        observed: PropView::new(),
        collections: BTreeMap::new(),
        runtime_params: runtime_params.clone(),
        tid: 1,
        steps_run: 0,
        await_iterations: Vec::new(),
        loop_iterations: Vec::new(),
        retry_delays_ms: Vec::new(),
        last_response_code: None,
        last_failure_class: None,
        bindings: BTreeMap::new(),
        command_listener_volatile,
    };
    // Session bring-up (idempotent): the responder rejects most ops before it.
    if !session_already_open {
        ctx.simple_op(op::OPEN_SESSION, vec![1], false)
            .map_err(|message| PtpIpError {
                step: "openSession".into(),
                message,
                response_code: None,
            })?;
    }
    ctx.walk_steps(steps, "steps")?;
    Ok(PtpIpOutcome {
        observed: ctx.observed,
        steps_run: ctx.steps_run,
        await_iterations: ctx.await_iterations,
        loop_iterations: ctx.loop_iterations,
        retry_delays_ms: ctx.retry_delays_ms,
    })
}

struct Ctx<'a> {
    engine: &'a mut Engine,
    /// The PTP-IP scope: observed property values accumulated from polls. A
    /// `getProp`/`readEcho`/`awaitUntil` poll lands the typed value here keyed
    /// by prop code; `until` predicates evaluate over it.
    observed: PropView,
    /// Collection scope is separate from scalar bindings and `PropView`.
    collections: BTreeMap<String, Vec<u64>>,
    runtime_params: BTreeMap<String, String>,
    /// Loop-bound scalar slots (#46): the forEach element + the chunk offset/length
    /// the executor advances, plus the `objectSize` captured from GetObjectInfo.
    /// Resolved by `StepParam::Runtime` ahead of `runtime_params` — loop vars
    /// shadow caller slots within a loop body.
    bindings: BTreeMap<String, u64>,
    tid: u32,
    steps_run: usize,
    await_iterations: Vec<usize>,
    loop_iterations: Vec<usize>,
    retry_delays_ms: Vec<u32>,
    /// Captured directly from the last wire reply; retry control never parses
    /// a diagnostic string.
    last_response_code: Option<u16>,
    /// Payload-decode classification for the current step. Reset before each
    /// step so transport and shape failures cannot inherit an earlier decode.
    last_failure_class: Option<RetryFailureClass>,
    /// The active connection's `command_listener_volatile` trait (#103): when set,
    /// a live-view `reopenSession` is refused because that transport-close tears
    /// down the command-port listener, so the reconnect gets "Connection refused".
    command_listener_volatile: bool,
}

impl Ctx<'_> {
    fn next_tid(&mut self) -> u32 {
        let t = self.tid;
        self.tid += 1;
        t
    }

    /// Datatype width for `code` from the manifest property entry (default u16,
    /// matching `state::datatype_of`). Immutable borrow released before any
    /// subsequent `on_operation`.
    fn datatype(&self, code: u16) -> u16 {
        datatype_of(
            self.engine
                .manifest()
                .property(code)
                .and_then(|p| p.ptype.as_deref()),
        )
    }

    fn walk_steps(&mut self, steps: &[Step], path: &str) -> Result<(), PtpIpError> {
        for (i, step) in steps.iter().enumerate() {
            let here = format!("{path}[{i}].{}", verb_name(step));
            // `repeat` wraps an action verb (e.g. 902B ×4). awaitUntil's repeat
            // is 1 (the poll loop is its own iteration); the cap below is a no-op.
            for _ in 0..step.repeat.max(1) {
                self.last_response_code = None;
                self.last_failure_class = None;
                if let Err(mut error) = self.run_step(step, &here) {
                    if error.response_code.is_none() {
                        error.response_code = self
                            .last_response_code
                            .filter(|response| *response != resp::OK);
                    }
                    return Err(error);
                }
            }
            self.steps_run += 1;
        }
        Ok(())
    }

    fn run_step(&mut self, step: &Step, here: &str) -> Result<(), PtpIpError> {
        let err = |message: String| PtpIpError {
            step: here.to_string(),
            message,
            response_code: None,
        };
        if let Some(p) = &step.set_prop {
            let code = parse_hex_code(p).ok_or_else(|| err(format!("bad prop code {p:?}")))?;
            let value = match step.value.as_ref() {
                None => 0,
                Some(SetPropValue::Literal(value)) => *value,
                Some(SetPropValue::Runtime(reference)) => {
                    let Some(value) = self.runtime_params.get(&reference.runtime) else {
                        return if reference.if_missing == MissingRuntimeValue::Skip {
                            Ok(())
                        } else {
                            Err(err(format!("runtime slot {:?} unbound", reference.runtime)))
                        };
                    };
                    if let Some(value) = self.runtime_string_value(code, value).map_err(err)? {
                        return self.set_prop_value(code, value, step.tolerant).map_err(err);
                    }
                    value.parse::<i64>().map_err(|_| {
                        err(format!(
                            "runtime slot {:?} is not a numeric setProp value",
                            reference.runtime
                        ))
                    })?
                }
            };
            self.set_prop(code, value, step.tolerant).map_err(err)
        } else if let Some(p) = &step.get_prop {
            let code = parse_hex_code(p).ok_or_else(|| err(format!("bad prop code {p:?}")))?;
            if step
                .captures
                .iter()
                .any(|capture| capture.source == CaptureSource::PtpU32Array)
            {
                let Some(values) = self.poll_collection(code, step.tolerant).map_err(err)? else {
                    // Tolerated non-OK advisory read: skip, capture nothing.
                    return Ok(());
                };
                self.capture_collection(&step.captures, &values)
                    .map_err(err)?;
            } else {
                let has_fallback = step
                    .captures
                    .iter()
                    .any(|capture| capture.fallback.is_some());
                let Some(v) = self
                    .poll_prop(code, step.tolerant || has_fallback)
                    .map_err(err)?
                else {
                    let response_code = self.last_response_code;
                    if response_code.is_some_and(|response_code| {
                        self.bind_capture_fallbacks(&step.captures, response_code)
                    }) {
                        return Ok(());
                    }
                    if step.tolerant {
                        return Ok(());
                    }
                    return Err(err(format!(
                        "GetDevicePropValue({code:#06x}) -> response {:#06x}",
                        response_code.unwrap_or_default()
                    )));
                };
                self.observed.set(code, v);
                self.capture_prop_value(&step.captures, v).map_err(err)?;
            }
            Ok(())
        } else if let Some(p) = &step.read_echo {
            // Read, then write the same value back (the live-view 0xdf2a echo).
            let code = parse_hex_code(p).ok_or_else(|| err(format!("bad prop code {p:?}")))?;
            let Some(v) = self.poll_prop(code, step.tolerant).map_err(err)? else {
                return Ok(());
            };
            self.observed.set(code, v);
            self.capture_prop_value(&step.captures, v).map_err(err)?;
            self.set_prop(code, v, step.tolerant).map_err(err)
        } else if let Some(o) = &step.send_op {
            let code = parse_hex_code(o).ok_or_else(|| err(format!("bad op code {o:?}")))?;
            let params = self.resolve_params(&step.params).map_err(err)?;
            let (transaction_id, reply) = self.issue_op(code, params);
            check_ok(&reply, code, step.tolerant).map_err(err)?;
            let completion = match &reply {
                Reply::DataStream {
                    source,
                    completion: Some(completion),
                    ..
                } => Some((source.clone(), completion.clone())),
                _ => None,
            };
            if response_code(&reply) == Some(resp::OK) {
                let captures_collection = step
                    .captures
                    .iter()
                    .any(|capture| capture.source == CaptureSource::PtpU32Array);
                if let Some((source, _)) = completion.as_ref().filter(|_| !captures_collection) {
                    consume_stream(source).map_err(err)?;
                }
                if let Err(message) =
                    self.apply_captures(&step.captures, code, transaction_id, &reply)
                {
                    self.last_failure_class = Some(RetryFailureClass::Decode);
                    if !step.tolerant {
                        return Err(err(message));
                    }
                } else if code == op::GET_OBJECT_INFO && step.captures.is_empty() {
                    if let Err(message) = self.capture_object_info(OBJECT_SIZE_SLOT, &reply) {
                        self.last_failure_class = Some(RetryFailureClass::Decode);
                        if !step.tolerant {
                            return Err(err(message));
                        }
                    }
                }
                if let Some((_, completion)) = completion {
                    self.engine.complete_stream(completion);
                }
            }
            Ok(())
        } else if let Some(role) = step.open_channel {
            if self.engine.channel_ready(role) {
                Ok(())
            } else {
                Err(err(format!(
                    "openChannel {role:?} reached before its manifest causal boundary"
                )))
            }
        } else if step.reopen_session.is_some() {
            if self.command_listener_volatile
                && matches!(self.engine.phase(), Phase::LiveView | Phase::Streaming)
            {
                // The camera tore down the command-port listener on the transport-
                // close while live-view was active, so an immediate reconnect is
                // refused. The caller must use an outer re-establishment (#244).
                return Err(err(
                    "reopenSession: camera refused the reconnect — the command-port \
                     listener does not survive a live-view transport-close on this \
                     connection (#103)"
                        .into(),
                ));
            }
            // Deterministic analogue of the TCP teardown/reconnect: close then
            // re-open the session in place.
            self.simple_op(op::CLOSE_SESSION, vec![], step.tolerant)
                .map_err(err)?;
            self.simple_op(op::OPEN_SESSION, vec![1], step.tolerant)
                .map_err(err)
        } else if step.close_session.is_some() {
            // Socket-role shutdown and the optional transport-close frame are
            // host I/O responsibilities. The sans-I/O walker models the PTP
            // CloseSession operation and resulting engine state.
            self.simple_op(op::CLOSE_SESSION, vec![], step.tolerant)
                .map_err(err)
        } else if let Some(retry) = &step.retry {
            self.run_step_retry(retry, step.tolerant, here)
        } else if let Some(aw) = &step.await_until {
            self.run_await_until(aw, step.tolerant, here)
        } else if let Some(lp) = &step.r#loop {
            self.run_loop(lp, step.tolerant, here)
        } else if let Some(cond) = &step.if_step {
            let actual = *self
                .bindings
                .get(&cond.slot)
                .ok_or_else(|| err(format!("if slot '{}' unbound", cond.slot)))?;
            if actual == cond.equals {
                self.walk_steps(&cond.then_steps, &format!("{here}.then"))
            } else {
                self.walk_steps(&cond.else_steps, &format!("{here}.else"))
            }
        } else {
            Err(err("step sets no action verb".into()))
        }
    }

    fn run_step_retry(
        &mut self,
        retry: &camera_config::StepRetry,
        tolerant: bool,
        here: &str,
    ) -> Result<(), PtpIpError> {
        let selected: Vec<u16> = retry
            .when_response_codes
            .iter()
            .filter_map(|code| parse_hex_code(code))
            .collect();
        let max_attempts = retry.max_attempts.max(1);
        for attempt in 0..max_attempts {
            match self.walk_steps(&retry.steps, &format!("{here}.steps")) {
                Ok(()) => return Ok(()),
                Err(mut error) => {
                    let decode_selected = self.last_failure_class
                        == Some(RetryFailureClass::Decode)
                        && retry
                            .when_failure_classes
                            .contains(&RetryFailureClass::Decode);
                    let response_code = error.response_code.or_else(|| {
                        self.last_response_code
                            .filter(|response| *response != resp::OK)
                    });
                    error.response_code = response_code;
                    let response_selected =
                        response_code.is_some_and(|code| selected.contains(&code));
                    if !response_selected && !decode_selected {
                        return Err(error);
                    }
                    if attempt + 1 == max_attempts {
                        if (response_selected || decode_selected)
                            && retry
                                .fallback
                                .as_ref()
                                .is_some_and(|steps| !steps.is_empty())
                        {
                            return match self.walk_steps(
                                retry.fallback.as_deref().unwrap_or_default(),
                                &format!("{here}.fallback"),
                            ) {
                                Err(error) if tolerant && error.response_code.is_some() => Ok(()),
                                result => result,
                            };
                        }
                        return if tolerant && response_selected {
                            Ok(())
                        } else {
                            Err(error)
                        };
                    }
                    self.retry_delays_ms.push(retry.retry_delay_ms);
                }
            }
        }
        unreachable!("max_attempts is at least one")
    }

    /// A closed declarative loop (#46): `forEach` over a captured collection or a
    /// `chunk`-by-size walk. The executor owns all cursor advancement; the body
    /// reuses the ordinary step grammar with the bound slots resolvable as runtime
    /// params. Each runs under a deterministic cap (the `awaitUntil` analogue).
    fn run_loop(&mut self, lp: &Loop, tolerant: bool, here: &str) -> Result<(), PtpIpError> {
        let err = |message: String| PtpIpError {
            step: here.to_string(),
            message,
            response_code: None,
        };
        match lp {
            Loop::ForEach {
                collection,
                bind,
                body,
            } => {
                let items = self.collections.get(collection).cloned().ok_or_else(|| {
                    err(format!("forEach collection slot '{collection}' unbound"))
                })?;
                if items.len() > MAX_FOREACH_ITERS && !tolerant {
                    return Err(err(format!(
                        "forEach over collection '{collection}' has {} elements, exceeds cap {MAX_FOREACH_ITERS}",
                        items.len()
                    )));
                }
                let n_iter = items.len().min(MAX_FOREACH_ITERS);
                for (n, item) in items.iter().take(n_iter).enumerate() {
                    let prev = self.bindings.insert(bind.clone(), *item);
                    let res = self.walk_steps(body, &format!("{here}.forEach[{n}]"));
                    restore(&mut self.bindings, bind, prev);
                    res?;
                }
                self.loop_iterations.push(n_iter);
                Ok(())
            }
            Loop::Chunk {
                total,
                size,
                offset_bind,
                length_bind,
                body,
            } => {
                let total_bytes = *self.bindings.get(total).ok_or_else(|| {
                    err(format!(
                        "chunk total slot '{total}' unbound — a preceding getObjectInfo must capture it"
                    ))
                })?;
                let window = self.resolve_chunk_size(size).map_err(err)?.max(1);
                let mut offset: u64 = 0;
                let mut iters = 0usize;
                while offset < total_bytes {
                    if iters >= MAX_CHUNK_ITERS {
                        if tolerant {
                            break;
                        }
                        return Err(err(format!(
                            "chunk loop exceeded {MAX_CHUNK_ITERS} windows for total {total_bytes}"
                        )));
                    }
                    // The executor owns the arithmetic — the manifest never sees it.
                    let length = (total_bytes - offset).min(window);
                    let po = self.bindings.insert(offset_bind.clone(), offset);
                    let pl = self.bindings.insert(length_bind.clone(), length);
                    let res = self.walk_steps(body, &format!("{here}.chunk[{iters}]"));
                    restore(&mut self.bindings, offset_bind, po);
                    restore(&mut self.bindings, length_bind, pl);
                    res?;
                    offset += length;
                    iters += 1;
                }
                self.loop_iterations.push(iters);
                Ok(())
            }
        }
    }

    /// GetDevicePropValue for an array-valued property (e.g. `0xd621`, the handle
    /// list) → decode the count-prefixed `u32` array → `Vec<u64>`. The result is
    /// captured into collection scope before `forEach`; a scalar/non-array reply
    /// is a hard error. A non-OK response with `tolerant` returns `Ok(None)`
    /// (skipped advisory read); decode failures stay fatal (#407).
    fn poll_collection(&mut self, code: u16, tolerant: bool) -> Result<Option<Vec<u64>>, String> {
        let tid = self.next_tid();
        let req = OperationRequest {
            data_phase_info: 1,
            code: op::GET_DEVICE_PROP_VALUE,
            transaction_id: tid,
            params: vec![code as u32],
        };
        let reply = self.engine.on_operation(&req, None);
        self.last_response_code = response_code(&reply);
        match reply {
            Reply::Data { data, response } if response.code == resp::OK => {
                let mut r = Reader::new(&data);
                let decoded = r
                    .ptp_array(|r| r.u32())
                    .map_err(|e| format!("decode array prop {code:#06x}: {e:?}"));
                if decoded.is_err() {
                    self.last_failure_class = Some(RetryFailureClass::Decode);
                }
                decoded.map(|items| Some(items.into_iter().map(|v| v as u64).collect()))
            }
            // A coded non-OK response is the advisory skip; `Close` and
            // `NoResponse` have no response code and stay fatal even when
            // tolerant (check_ok precedent, #455 review).
            other if tolerant && response_code(&other).is_some() => Ok(None),
            other => Err(format!(
                "GetDevicePropValue({code:#06x}) -> {}",
                describe_reply(&other)
            )),
        }
    }

    /// `awaitUntil` (§11.16): observe until `until` holds. A `poll` source loops —
    /// it polls `source` each iteration and runs `on_each` when unsatisfied, up to
    /// the deterministic timeout [`MAX_AWAIT_ITERS`]. An `event` source is
    /// single-shot: take the completion event off the engine queue, do one
    /// post-event read of `then_poll`, then evaluate `until` once (push-then-read).
    /// A non-numeric/unsupported source poll is a hard error.
    fn run_await_until(
        &mut self,
        aw: &AwaitUntil,
        tolerant: bool,
        here: &str,
    ) -> Result<(), PtpIpError> {
        let err = |message: String| PtpIpError {
            step: here.to_string(),
            message,
            response_code: None,
        };
        match &aw.source {
            AwaitSource::Poll { prop } => {
                let code =
                    parse_hex_code(prop).ok_or_else(|| err(format!("bad source prop {prop:?}")))?;
                for iter in 1..=MAX_AWAIT_ITERS {
                    let v = self
                        .poll_prop(code, false)
                        .map_err(err)?
                        .expect("strict poll returns Err, never a skip");
                    self.observed.set(code, v);
                    if aw.until.eval(&self.observed) {
                        self.await_iterations.push(iter);
                        return Ok(());
                    }
                    // Not yet: act, then poll again. interval_ms is dispatcher
                    // cadence — the deterministic executor doesn't sleep.
                    self.walk_steps(&aw.on_each, &format!("{here}.onEach"))?;
                }
                if tolerant {
                    self.await_iterations.push(MAX_AWAIT_ITERS);
                    return Ok(());
                }
                Err(err(format!(
                    "`until` not satisfied polling {code:#06x} within {MAX_AWAIT_ITERS} observations"
                )))
            }
            AwaitSource::Event { code, then_poll } => {
                let ev =
                    parse_hex_code(code).ok_or_else(|| err(format!("bad event code {code:?}")))?;
                // The event wait itself is single-shot: by now the triggering op's
                // `emits` has either queued this event or it never will (the same
                // outcome BLE calls notify "source exhausted").
                if !self.engine.take_event(ev) {
                    if tolerant {
                        self.await_iterations.push(0);
                        return Ok(());
                    }
                    return Err(err(format!("awaited event {ev:#06x} was not emitted")));
                }
                // The event acknowledges the operation, it does NOT guarantee the
                // value has settled: fw02.30 fires 0xc005 ~100ms after LockS1Lock
                // while 0xd209 still reads pre-settle (client application#157 wire capture),
                // so `thenPoll` re-polls until `until` holds — wall-clock
                // dispatchers pace by interval_ms within timeout_ms; this
                // deterministic executor iterates like the Poll branch (#185).
                if let Some(tp) = then_poll {
                    let pc = parse_hex_code(tp)
                        .ok_or_else(|| err(format!("bad thenPoll prop {tp:?}")))?;
                    for iter in 1..=MAX_AWAIT_ITERS {
                        let v = self
                            .poll_prop(pc, false)
                            .map_err(err)?
                            .expect("strict poll returns Err, never a skip");
                        self.observed.set(pc, v);
                        if aw.until.eval(&self.observed) {
                            self.await_iterations.push(iter);
                            return Ok(());
                        }
                        self.walk_steps(&aw.on_each, &format!("{here}.onEach"))?;
                    }
                    if tolerant {
                        self.await_iterations.push(MAX_AWAIT_ITERS);
                        return Ok(());
                    }
                    return Err(err(format!(
                        "`until` not satisfied re-polling {pc:#06x} after event {ev:#06x} within {MAX_AWAIT_ITERS} observations"
                    )));
                }
                // Event-only source: the push already happened, so there is
                // nothing to re-read — a single predicate evaluation.
                if aw.until.eval(&self.observed) || tolerant {
                    self.await_iterations.push(1);
                    Ok(())
                } else {
                    Err(err(format!(
                        "`until` not satisfied after event {ev:#06x} (no thenPoll to re-read)"
                    )))
                }
            }
        }
    }

    fn runtime_string_value(&self, code: u16, value: &str) -> Result<Option<PropValue>, String> {
        let Some(property) = self.engine.manifest().property(code) else {
            return Ok(None);
        };
        if property.ptype.as_deref() != Some("str") {
            return Ok(None);
        }
        let value = if let Some(layout) = &property.structured_text {
            let fields = value.split(&layout.delimiter).collect::<Vec<_>>();
            if fields.len() != layout.fields.len() {
                return Err(format!(
                    "property {code:#06x} requires {} signed integer fields",
                    layout.fields.len()
                ));
            }
            fields
                .into_iter()
                .map(parse_signed_decimal)
                .collect::<Option<Vec<_>>>()
                .ok_or_else(|| {
                    format!(
                        "property {code:#06x} requires {} signed integer fields",
                        layout.fields.len()
                    )
                })?
                .into_iter()
                .map(|field| field.to_string())
                .collect::<Vec<_>>()
                .join(&layout.delimiter)
        } else {
            value.to_string()
        };
        Ok(Some(PropValue::Str(value)))
    }

    /// SetDevicePropValue: encode `value` at the property's datatype width and
    /// drive the data-out phase.
    fn set_prop(&mut self, code: u16, value: i64, tolerant: bool) -> Result<(), String> {
        let dt = self.datatype(code);
        self.set_prop_value(code, typed(dt, value), tolerant)
    }

    fn set_prop_value(
        &mut self,
        code: u16,
        value: PropValue,
        tolerant: bool,
    ) -> Result<(), String> {
        let mut w = Writer::new();
        value
            .encode(&mut w)
            .map_err(|e| format!("encode prop {code:#06x}: {e:?}"))?;
        let data = w.into_vec();
        let tid = self.next_tid();
        let req = OperationRequest {
            data_phase_info: 2,
            code: op::SET_DEVICE_PROP_VALUE,
            transaction_id: tid,
            params: vec![code as u32],
        };
        let reply = self.engine.on_operation(&req, Some(&data));
        self.last_response_code = response_code(&reply);
        check_ok(&reply, op::SET_DEVICE_PROP_VALUE, tolerant)
    }

    /// GetDevicePropValue → decode at the property's datatype width → i64.
    /// A non-OK response with `tolerant` returns `Ok(None)` (skipped advisory
    /// read); transport and decode failures stay fatal (#407).
    fn poll_prop(&mut self, code: u16, tolerant: bool) -> Result<Option<i64>, String> {
        let dt = self.datatype(code);
        let tid = self.next_tid();
        let req = OperationRequest {
            data_phase_info: 1,
            code: op::GET_DEVICE_PROP_VALUE,
            transaction_id: tid,
            params: vec![code as u32],
        };
        let reply = self.engine.on_operation(&req, None);
        self.last_response_code = response_code(&reply);
        match reply {
            Reply::Data { data, response } if response.code == resp::OK => {
                if let Some(payload) = self
                    .engine
                    .manifest()
                    .property(code)
                    .and_then(|property| property.payload.as_ref())
                {
                    let (count_width, code_width, value_width) = payload.record_widths();
                    let decoded = RecordStreamDescriptor::new(
                        RecordStreamLayout::new(count_width, code_width, value_width)
                            .map_err(|error| error.to_string())?,
                        payload.members.iter().filter_map(|member| {
                            let code = parse_hex_code(member.code())?;
                            let encoding = match member.encoding(value_width) {
                                camera_config::RecordValueEncoding::Fixed { width } => {
                                    RecordValueEncoding::Fixed { width }
                                }
                                camera_config::RecordValueEncoding::Signed { width } => {
                                    RecordValueEncoding::Signed { width }
                                }
                                camera_config::RecordValueEncoding::PtpString => {
                                    RecordValueEncoding::PtpString
                                }
                            };
                            Some((code, encoding))
                        }),
                    )
                    .map_err(|error| error.to_string())
                    .and_then(|descriptor| {
                        parse_typed_record_stream(&data, &descriptor)
                            .map_err(|error| format!("decode prop {code:#06x}: {error:?}"))
                            .and_then(|decoded| {
                                if decoded.diagnostics.is_empty() {
                                    return Ok(());
                                }
                                let diagnostics = decoded
                                    .diagnostics
                                    .into_iter()
                                    .map(|diagnostic| match diagnostic {
                                        protocol_primitives::quirk::RecordStreamDiagnostic::SkippedUndeclaredMember {
                                            code,
                                            value,
                                        } => format!(
                                            "skipped undeclared member {code:#06x} value {value:#010x}"
                                        ),
                                    })
                                    .collect::<Vec<_>>()
                                    .join(", ");
                                Err(format!("decode prop {code:#06x}: {diagnostics}"))
                            })
                    });
                    if let Err(message) = decoded {
                        self.last_failure_class = Some(RetryFailureClass::Decode);
                        return Err(message);
                    }
                }
                let mut r = Reader::new(&data);
                let decoded = PropValue::decode(&mut r, dt)
                    .map_err(|e| format!("decode prop {code:#06x}: {e:?}"))
                    .and_then(|value| {
                        prop_value_to_i64(&value)
                            .ok_or_else(|| format!("prop {code:#06x} is non-numeric (string)"))
                    });
                if decoded.is_err() {
                    self.last_failure_class = Some(RetryFailureClass::Decode);
                }
                decoded.map(Some)
            }
            // A coded non-OK response is the advisory skip; `Close` and
            // `NoResponse` have no response code and stay fatal even when
            // tolerant (check_ok precedent, #455 review).
            other if tolerant && response_code(&other).is_some() => Ok(None),
            other => Err(format!(
                "GetDevicePropValue({code:#06x}) -> {}",
                describe_reply(&other)
            )),
        }
    }

    /// A no-data operation (send_op / session ops). `repeat` is applied by the
    /// caller; this fires exactly one request.
    fn simple_op(&mut self, code: u16, params: Vec<u32>, tolerant: bool) -> Result<(), String> {
        let (_, reply) = self.issue_op(code, params);
        check_ok(&reply, code, tolerant)
    }

    fn issue_op(&mut self, code: u16, params: Vec<u32>) -> (u32, Reply) {
        let tid = self.next_tid();
        let req = OperationRequest {
            data_phase_info: 1,
            code,
            transaction_id: tid,
            params,
        };
        let reply = self.engine.on_operation(&req, None);
        self.last_response_code = response_code(&reply);
        (tid, reply)
    }

    fn apply_captures(
        &mut self,
        captures: &[Capture],
        code: u16,
        transaction_id: u32,
        reply: &Reply,
    ) -> Result<(), String> {
        let mut captured = Vec::with_capacity(captures.len());
        for capture in captures {
            let value = match capture.source {
                CaptureSource::ObjectInfoCompressedSize => {
                    if code != op::GET_OBJECT_INFO {
                        return Err(
                            "objectInfoCompressedSize capture requires GetObjectInfo".into()
                        );
                    }
                    self.decode_object_info_size(reply)?
                }
                CaptureSource::PropValue => return Err("propValue capture requires getProp".into()),
                CaptureSource::PtpU32Array => {
                    let collection = decode_collection_capture(reply)?;
                    self.collections.insert(capture.bind.clone(), collection);
                    continue;
                }
                CaptureSource::U32Le => {
                    let data = scalar_capture_head(reply, 4, "u32Le")?;
                    let mut r = Reader::new(&data);
                    r.u32()
                        .map_err(|e| format!("decode captured u32Le: {e:?}"))?
                        as u64
                }
                CaptureSource::U64Le => {
                    let data = scalar_capture_head(reply, 8, "u64Le")?;
                    let mut r = Reader::new(&data);
                    r.u64()
                        .map_err(|e| format!("decode captured u64Le: {e:?}"))?
                }
                CaptureSource::TransactionId => transaction_id as u64,
            };
            captured.push((capture.bind.clone(), value));
        }
        for (bind, value) in captured {
            self.bindings.insert(bind, value);
        }
        Ok(())
    }

    fn capture_object_info(&mut self, slot: &str, reply: &Reply) -> Result<(), String> {
        let value = self.decode_object_info_size(reply)?;
        self.bindings.insert(slot.to_string(), value);
        Ok(())
    }

    fn decode_object_info_size(&self, reply: &Reply) -> Result<u64, String> {
        if let Reply::Data { data, response } = reply {
            if response.code == resp::OK {
                let oi = ObjectInfo::decode(data)
                    .map_err(|e| format!("decode ObjectInfo capture: {e:?}"))?;
                return Ok(oi.object_compressed_size as u64);
            }
        }
        Err("ObjectInfo capture requires an OK data reply".into())
    }

    fn capture_prop_value(&mut self, captures: &[Capture], value: i64) -> Result<(), String> {
        for capture in captures {
            if capture.source != CaptureSource::PropValue {
                return Err("getProp captures only support propValue".into());
            }
            self.bindings.insert(capture.bind.clone(), value as u64);
        }
        Ok(())
    }

    fn bind_capture_fallbacks(&mut self, captures: &[Capture], response_code: u16) -> bool {
        let selected = captures
            .iter()
            .map(|capture| {
                capture
                    .fallback
                    .as_ref()
                    .filter(|fallback| {
                        fallback
                            .when_response_codes
                            .iter()
                            .filter_map(|code| parse_hex_code(code))
                            .any(|code| code == response_code)
                    })
                    .map(|fallback| (capture.bind.clone(), fallback.value))
            })
            .collect::<Option<Vec<_>>>();
        let Some(selected) = selected.filter(|values| !values.is_empty()) else {
            return false;
        };
        for (bind, value) in selected {
            self.bindings.insert(bind, value);
        }
        true
    }

    fn capture_collection(&mut self, captures: &[Capture], values: &[u64]) -> Result<(), String> {
        for capture in captures {
            if capture.source != CaptureSource::PtpU32Array {
                return Err("array-valued getProp captures only support ptpU32Array".into());
            }
            self.collections
                .insert(capture.bind.clone(), values.to_vec());
        }
        Ok(())
    }

    fn resolve_chunk_size(&self, size: &ChunkSize) -> Result<u64, String> {
        match size {
            ChunkSize::Literal(v) => Ok(*v as u64),
            ChunkSize::Runtime { runtime } => self
                .bindings
                .get(runtime)
                .copied()
                .ok_or_else(|| format!("chunk size slot '{runtime}' unbound")),
        }
    }

    fn resolve_params(&self, params: &[StepParam]) -> Result<Vec<u32>, String> {
        params
            .iter()
            .map(|p| match p {
                StepParam::Literal(v) => Ok(*v),
                StepParam::Runtime {
                    runtime,
                    shift,
                    mask,
                } => {
                    // Loop-bound vars (forEach element, chunk offset/length) shadow
                    // caller-supplied runtime_params within a loop body (#46).
                    if let Some(b) = self.bindings.get(runtime) {
                        let value = transform_runtime_value(*b, *shift, *mask);
                        return u32::try_from(value).map_err(|_| {
                            format!("loop slot '{runtime}' value {value} out of u32 range")
                        });
                    }
                    let raw = self
                        .runtime_params
                        .get(runtime)
                        .ok_or_else(|| format!("runtime slot '{runtime}' unbound"))?;
                    let value = parse_u64(raw).ok_or_else(|| {
                        format!("runtime slot '{runtime}' value {raw:?} is not a u64")
                    })?;
                    let value = transform_runtime_value(value, *shift, *mask);
                    u32::try_from(value).map_err(|_| {
                        format!("runtime slot '{runtime}' value {value} out of u32 range")
                    })
                }
            })
            .collect()
    }
}

/// Restore a loop-bound slot to its prior value after an iteration, so nested
/// loops don't leak bindings (push/pop discipline). `prev` is the value the
/// binding shadowed, if any.
fn restore(bindings: &mut BTreeMap<String, u64>, slot: &str, prev: Option<u64>) {
    match prev {
        Some(v) => {
            bindings.insert(slot.to_string(), v);
        }
        None => {
            bindings.remove(slot);
        }
    }
}

/// The verb a model `Step` invokes, for error paths. Distinct from the BLE
/// `Step::verb_name` (a different grammar). Falls back to `?` if no field is set
/// (a malformed step, caught by `run_step`).
fn verb_name(s: &Step) -> &'static str {
    if s.set_prop.is_some() {
        "setProp"
    } else if s.get_prop.is_some() {
        "getProp"
    } else if s.read_echo.is_some() {
        "readEcho"
    } else if s.send_op.is_some() {
        "sendOp"
    } else if s.open_channel.is_some() {
        "openChannel"
    } else if s.reopen_session.is_some() {
        "reopenSession"
    } else if s.await_until.is_some() {
        "awaitUntil"
    } else if s.retry.is_some() {
        "retry"
    } else if s.r#loop.is_some() {
        "loop"
    } else if s.if_step.is_some() {
        "if"
    } else {
        "?"
    }
}

/// OK unless a non-OK response (tolerated -> skipped) or a transport-level
/// failure (`NoResponse` timeout / `Close` dropped socket).
fn check_ok(reply: &Reply, code: u16, tolerant: bool) -> Result<(), String> {
    if let Some(response) = response_code(reply) {
        if response == resp::OK || tolerant {
            return Ok(());
        }
        return Err(format!("op {code:#06x} -> response {response:#06x}"));
    }
    match reply {
        Reply::NoResponse => Err(format!("op {code:#06x} timed out with no response")),
        Reply::Close => Err(format!("op {code:#06x} closed the connection")),
        Reply::Response(_) | Reply::Data { .. } | Reply::DataStream { .. } => unreachable!(),
    }
}

fn response_code(reply: &Reply) -> Option<u16> {
    match reply {
        Reply::Response(r)
        | Reply::Data { response: r, .. }
        | Reply::DataStream { response: r, .. } => Some(r.code),
        Reply::NoResponse | Reply::Close => None,
    }
}

fn describe_reply(reply: &Reply) -> String {
    match reply {
        Reply::Response(_) => format!("response {:#06x}", response_code(reply).unwrap()),
        Reply::Data { .. } => format!("data + response {:#06x}", response_code(reply).unwrap()),
        Reply::DataStream { .. } => {
            format!("stream + response {:#06x}", response_code(reply).unwrap())
        }
        Reply::NoResponse => "no response".into(),
        Reply::Close => "connection closed".into(),
    }
}

fn scalar_capture_head(reply: &Reply, width: usize, label: &str) -> Result<Vec<u8>, String> {
    match reply {
        Reply::Data { data, .. } => Ok(data[..data.len().min(width)].to_vec()),
        Reply::DataStream { source, .. } => source
            .read_chunk(0, width)
            .map_err(|e| format!("read {label} capture stream head: {e}")),
        Reply::Response(_) | Reply::NoResponse | Reply::Close => {
            Err(format!("{label} capture requires a data or stream reply"))
        }
    }
}

fn decode_collection_capture(reply: &Reply) -> Result<Vec<u64>, String> {
    match reply {
        Reply::Data { data, .. } => decode_collection_bytes(data),
        Reply::DataStream { source, .. } => decode_collection_stream(source),
        Reply::Response(_) | Reply::NoResponse | Reply::Close => {
            Err("ptpU32Array capture requires a data or stream reply".into())
        }
    }
}

fn decode_collection_bytes(data: &[u8]) -> Result<Vec<u64>, String> {
    let count_bytes: [u8; 4] = data
        .get(..4)
        .ok_or_else(|| "decode captured ptpU32Array: count needs 4 bytes".to_string())?
        .try_into()
        .expect("four-byte count");
    let count = u32::from_le_bytes(count_bytes);
    if count > MAX_CAPTURED_U32S {
        return Err(format!(
            "decode captured ptpU32Array: count {count} exceeds {MAX_CAPTURED_U32S}"
        ));
    }
    let expected =
        4_usize
            .checked_add((count as usize).checked_mul(4).ok_or_else(|| {
                "decode captured ptpU32Array: payload length overflow".to_string()
            })?)
            .ok_or_else(|| "decode captured ptpU32Array: payload length overflow".to_string())?;
    if data.len() > expected {
        return Err(format!(
            "decode captured ptpU32Array: {} trailing bytes",
            data.len() - expected
        ));
    }
    if data.len() != expected {
        return Err(format!(
            "decode captured ptpU32Array: expected {expected} bytes for {count} values, got {}",
            data.len()
        ));
    }
    Ok(data[4..]
        .chunks_exact(4)
        .map(|bytes| {
            u64::from(u32::from_le_bytes(
                bytes.try_into().expect("four-byte value"),
            ))
        })
        .collect())
}

fn decode_collection_stream(source: &ByteSource) -> Result<Vec<u64>, String> {
    let header = source
        .read_chunk(0, 4)
        .map_err(|error| format!("read ptpU32Array capture count: {error}"))?;
    if header.len() != 4 {
        return Err(format!(
            "decode captured ptpU32Array: count needs 4 bytes, got {}",
            header.len()
        ));
    }
    let count = u32::from_le_bytes(header.try_into().expect("four-byte header"));
    if count > MAX_CAPTURED_U32S {
        return Err(format!(
            "decode captured ptpU32Array: count {count} exceeds {MAX_CAPTURED_U32S}"
        ));
    }
    let expected =
        4_u64
            .checked_add(u64::from(count).checked_mul(4).ok_or_else(|| {
                "decode captured ptpU32Array: payload length overflow".to_string()
            })?)
            .ok_or_else(|| "decode captured ptpU32Array: payload length overflow".to_string())?;
    if source.len() != expected {
        return Err(format!(
            "decode captured ptpU32Array: expected {expected} bytes for {count} values, got {}",
            source.len()
        ));
    }

    let mut values = Vec::with_capacity(count as usize);
    let mut offset = 4_u64;
    while offset < expected {
        let bytes = source
            .read_chunk(offset, 4)
            .map_err(|error| format!("read ptpU32Array capture value: {error}"))?;
        if bytes.len() != 4 {
            return Err(format!(
                "decode captured ptpU32Array: value at {offset} needs 4 bytes, got {}",
                bytes.len()
            ));
        }
        values.push(u64::from(u32::from_le_bytes(
            bytes.try_into().expect("four-byte value"),
        )));
        offset += 4;
    }
    Ok(values)
}

fn consume_stream(source: &ByteSource) -> Result<(), String> {
    const CHUNK_BYTES: usize = 1024 * 1024;
    let total = source.len();
    let mut offset = 0;
    while offset < total {
        let chunk = source
            .read_chunk(offset, CHUNK_BYTES)
            .map_err(|error| format!("consume streamed reply: {error}"))?;
        if chunk.is_empty() {
            return Err(format!(
                "consume streamed reply: source ended at {offset} of {total} bytes"
            ));
        }
        offset += chunk.len() as u64;
    }
    Ok(())
}

fn prop_value_to_i64(v: &PropValue) -> Option<i64> {
    Some(match v {
        PropValue::I8(x) => *x as i64,
        PropValue::U8(x) => *x as i64,
        PropValue::I16(x) => *x as i64,
        PropValue::U16(x) => *x as i64,
        PropValue::I32(x) => *x as i64,
        PropValue::U32(x) => *x as i64,
        PropValue::I64(x) => *x,
        PropValue::U64(x) => *x as i64,
        PropValue::Str(_) => return None,
    })
}

fn parse_signed_decimal(value: &str) -> Option<i64> {
    let digits = value
        .strip_prefix('-')
        .or_else(|| value.strip_prefix('+'))
        .unwrap_or(value);
    if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    value.parse().ok()
}

fn parse_u64(s: &str) -> Option<u64> {
    let t = s.trim();
    if let Some(hex) = t.strip_prefix("0x").or_else(|| t.strip_prefix("0X")) {
        u64::from_str_radix(hex, 16).ok()
    } else {
        t.parse::<u64>().ok()
    }
}

fn transform_runtime_value(value: u64, shift: u32, mask: Option<u64>) -> u64 {
    value.checked_shr(shift).unwrap_or(0) & mask.unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fault::{FaultMutation, FaultSelector, FaultSpec, FaultStage};
    use camera_config::{CameraManifest, Leaf, Predicate};
    use camera_media_store::{MediaStore, ObjectQuery};

    /// `{ prop: <hex>, eq: <val> }` as the PTP predicate (avoids a serde_yaml dep).
    fn leaf_eq(prop: &str, val: i64) -> Predicate {
        Predicate::Leaf(Leaf {
            prop: prop.into(),
            mask: None,
            eq: Some(val),
            ne: None,
            lt: None,
            gt: None,
        })
    }

    /// An empty media card — these tests exercise property state, not media.
    fn empty_store() -> MediaStore {
        let root = unique_temp_root("ptpsim-ptpip");
        std::fs::create_dir_all(&root).unwrap();
        MediaStore::open(&root).unwrap()
    }

    fn engine(yaml: &str) -> Engine {
        let manifest = CameraManifest::from_yaml(yaml).expect("manifest loads");
        Engine::new(manifest, empty_store())
    }

    fn engine_with_file(yaml: &str, bytes: &[u8]) -> (Engine, u32) {
        let manifest = CameraManifest::from_yaml(yaml).expect("manifest loads");
        let root = unique_temp_root("ptpsim-ptpip-media");
        let path = root.join("DCIM/100_FUJI/DSCF0001.JPG");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, bytes).unwrap();
        let mut store = MediaStore::open(&root).unwrap();
        store.scan().unwrap();
        let handle = store.handles(ObjectQuery {
            parent: None,
            format: Some(ptp_core::codes::format::EXIF_JPEG),
        })[0];
        (Engine::new(manifest, store), handle)
    }

    const MANIFEST: &str = r#"
schema: camera-config/v1
camera: { manufacturer: FUJIFILM, model: GFX100 II, firmware: "2.30" }
operations:
  "0x101c": { name: InitiateOpenCapture }
properties:
  "0xdf00": { name: functionPriority, type: u16, access: readWrite }
  "0xdf01": { name: functionMode,     type: u16, access: readWrite }
"#;

    const RESERVED_MANIFEST: &str = r#"
schema: camera-config/v1
camera: { manufacturer: TEST, model: Reserved Queue }
cameraInitiatedTransfer:
  trigger:
    match: all
    states: [{ gatt: "00000000-0000-0000-0000-000000000001", triggerValues: ["01"] }]
  handoff: { connection: app, socketRole: command }
  receive:
    mode: reserved-photo-receive
    count: { property: "0xd212", member: "0xdf41" }
    headIndex: 1
    metadata: { operation: "0x1008", phases: [afterCountBeforeModeEntry, afterModeEntry] }
    data: { operation: "0x101b", chunkLimitProperty: "0xd235" }
    completion: readToEof
connections:
  app:
    modes: [reserved-photo-receive]
    bindings: { command: 55740 }
modes:
  reserved-photo-receive: { detect: { prop: "0xdf01", eq: 21 } }
operations:
  "0x1002": { name: OpenSession }
  "0x1008": { name: GetObjectInfo }
  "0x1015": { name: GetDevicePropValue }
  "0x1016": { name: SetDevicePropValue }
  "0x101b": { name: GetPartialObject }
properties:
  "0xdf01": { name: functionMode, type: u16, access: readWrite }
  "0xdf41": { name: reservedCount, type: u32, access: readOnly }
  "0xd235": { name: chunkLimit, type: u32, access: readOnly, initialValue: 1024 }
  "0xd212":
    name: status
    type: u8a
    access: readOnly
    payload: { form: recordStream, members: ["0xdf41"] }
media:
  formats:
    "0x3801": { name: jpeg, isPhotosCompatible: true }
"#;

    #[test]
    fn plain_set_get_sendop_sequence_round_trips() {
        let mut e = engine(MANIFEST);
        let steps = vec![
            Step {
                set_prop: Some("0xdf00".into()),
                value: Some(6.into()),
                ..Default::default()
            },
            Step {
                get_prop: Some("0xdf00".into()),
                ..Default::default()
            },
            Step {
                send_op: Some("0x101c".into()),
                tolerant: true, // 0x101c needs a live-view phase; tolerate the reject here
                ..Default::default()
            },
        ];
        let out = walk_ptpip(&mut e, &steps, &BTreeMap::new()).expect("walk ok");
        // get_prop read back what set_prop wrote.
        assert_eq!(out.observed.get(0xdf00), Some(6));
        assert_eq!(out.steps_run, 3);
        assert!(out.await_iterations.is_empty());
    }

    #[test]
    fn malformed_ptp_u32_array_capture_fails_loud() {
        let mut engine = engine(MANIFEST);
        let error = walk_ptpip(
            &mut engine,
            &[Step {
                get_prop: Some("0xdf00".into()),
                captures: vec![Capture {
                    bind: "items".into(),
                    source: CaptureSource::PtpU32Array,
                    fallback: None,
                }],
                ..Default::default()
            }],
            &BTreeMap::new(),
        )
        .expect_err("a scalar payload is not a count-prefixed u32 array");
        assert!(error.message.contains("decode array prop"));
    }

    #[test]
    fn record_stream_self_check_reports_skipped_undeclared_member() {
        let mut engine = engine(
            r#"
schema: camera-config/v1
camera: { manufacturer: TEST, model: Record Stream, firmware: "1" }
properties:
  "0xd209": { name: result, type: u32, access: readOnly, initialValue: 0 }
  "0xd212":
    name: status
    type: u8a
    access: readOnly
    payload: { form: recordStream, members: ["0xd209"] }
"#,
        );
        engine
            .install_fault(crate::FaultSpec {
                selector: crate::FaultSelector {
                    operation: op::GET_DEVICE_PROP_VALUE,
                    params: Some(vec![0xd212]),
                    skip: 0,
                    count: Some(1),
                },
                mutation: crate::FaultMutation::ReplaceData {
                    bytes: vec![
                        0x01, 0x00, // count
                        0xff, 0x5f, 0xef, 0xbe, 0xad, 0xde, // undeclared fixed member
                    ],
                },
            })
            .unwrap();

        let error = walk_ptpip(
            &mut engine,
            &[Step {
                get_prop: Some("0xd212".into()),
                ..Default::default()
            }],
            &BTreeMap::new(),
        )
        .expect_err("sim self-check rejects its own undeclared member");

        assert!(error.message.contains("0x5fff"));
        assert!(error.message.contains("0xdeadbeef"));
    }

    #[test]
    fn ptp_u32_array_capture_rejects_trailing_bytes() {
        let mut data = 1_u32.to_le_bytes().to_vec();
        data.extend_from_slice(&7_u32.to_le_bytes());
        data.push(0xff);

        let error = decode_collection_bytes(&data).expect_err("trailing data is malformed");
        assert!(error.contains("trailing bytes"));
    }

    #[test]
    fn ptp_u32_array_capture_rejects_over_ceiling_header_before_allocation() {
        let data = (MAX_CAPTURED_U32S + 1).to_le_bytes();

        let error = decode_collection_bytes(&data).expect_err("count exceeds walker ceiling");
        assert!(error.contains("exceeds 100000"));
    }

    #[test]
    fn generated_collection_stream_rejects_huge_declared_source_before_realizing_it() {
        let source = ByteSource::Generated {
            len: u64::MAX,
            seed: 0,
        };

        let error = decode_collection_stream(&source)
            .expect_err("an unbounded generated source is not a collection payload");
        assert!(error.contains("exceeds"));
    }

    #[test]
    fn send_op_ptp_u32_array_capture_populates_collection() {
        let manifest = r#"
schema: camera-config/v1
camera: { manufacturer: TEST, model: SendOp Collection }
connections:
  wireless-tether:
    modes: [image-transfer]
operations:
  "0x1002": { name: OpenSession, connections: [wireless-tether] }
  "0x1007": { name: GetObjectHandles, connections: [wireless-tether] }
  "0x1008": { name: GetObjectInfo, connections: [wireless-tether] }
properties: {}
"#;
        let (mut engine, _) = engine_with_file(manifest, b"jpeg");
        engine.bind_connection("wireless-tether");
        let reply = engine.on_operation(
            &OperationRequest {
                data_phase_info: 1,
                code: op::OPEN_SESSION,
                transaction_id: 1,
                params: vec![1],
            },
            None,
        );
        assert!(matches!(reply, Reply::Response(response) if response.code == resp::OK));

        let outcome = walk_ptpip_in(
            &mut engine,
            &[
                Step {
                    send_op: Some("0x1007".into()),
                    params: vec![StepParam::Literal(u32::MAX), StepParam::Literal(0)],
                    captures: vec![Capture {
                        bind: "objectHandles".into(),
                        source: CaptureSource::PtpU32Array,
                        fallback: None,
                    }],
                    ..Default::default()
                },
                Step {
                    r#loop: Some(Loop::ForEach {
                        collection: "objectHandles".into(),
                        bind: "handle".into(),
                        body: vec![Step {
                            send_op: Some("0x1008".into()),
                            params: vec![StepParam::Runtime {
                                runtime: "handle".into(),
                                shift: 0,
                                mask: None,
                            }],
                            ..Default::default()
                        }],
                    }),
                    ..Default::default()
                },
            ],
            &BTreeMap::new(),
            Some("wireless-tether"),
        )
        .expect("sendOp collection capture succeeds");

        assert_eq!(outcome.loop_iterations, vec![1]);
    }

    #[test]
    fn completion_stream_read_failure_leaves_reserved_head_queued() {
        let manifest = CameraManifest::from_yaml(RESERVED_MANIFEST).expect("manifest loads");
        let root = unique_temp_root("ptpsim-ptpip-missing");
        let path = root.join("DCIM/100_FUJI/DSCF0001.JPG");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"jpeg-body").unwrap();
        let mut store = MediaStore::open(&root).unwrap();
        store.scan().unwrap();
        let mut engine = Engine::new(manifest, store);
        let activation: crate::StateOverlay = serde_json::from_value(serde_json::json!({
            "camera_initiated_transfer_active": true
        }))
        .unwrap();
        engine.apply_state_overlay(&activation).unwrap();
        std::fs::remove_file(&path).unwrap();

        let steps = vec![
            Step {
                send_op: Some("0x1008".into()),
                params: vec![StepParam::Literal(1)],
                ..Default::default()
            },
            Step {
                set_prop: Some("0xdf01".into()),
                value: Some(21.into()),
                ..Default::default()
            },
            Step {
                send_op: Some("0x101b".into()),
                params: vec![
                    StepParam::Literal(1),
                    StepParam::Literal(0),
                    StepParam::Literal(9),
                ],
                ..Default::default()
            },
        ];
        let error = walk_ptpip(&mut engine, &steps, &BTreeMap::new()).unwrap_err();
        assert!(
            error.message.contains("consume streamed reply"),
            "unexpected error: {error}"
        );
        assert_eq!(engine.state().props.get(&0xdf41), Some(&PropValue::U32(1)));
    }

    #[test]
    fn tolerant_send_op_capture_decode_failure_does_not_abort_walk() {
        let (mut e, handle) = engine_with_file(
            MANIFEST,
            b"\xff\xd8\x01\x02\x03\x04\x05\x06\x07\x08\xff\xd9",
        );
        let steps = vec![
            Step {
                send_op: Some("0x101b".into()),
                params: vec![
                    StepParam::Literal(handle),
                    StepParam::Literal(0),
                    StepParam::Literal(4),
                ],
                captures: vec![Capture {
                    bind: "tooWide".into(),
                    source: CaptureSource::U64Le,
                    fallback: None,
                }],
                tolerant: true,
                ..Default::default()
            },
            Step {
                set_prop: Some("0xdf00".into()),
                value: Some(7.into()),
                ..Default::default()
            },
            Step {
                get_prop: Some("0xdf00".into()),
                ..Default::default()
            },
        ];

        let out = walk_ptpip(&mut e, &steps, &BTreeMap::new()).expect("tolerant capture skips");
        assert_eq!(out.observed.get(0xdf00), Some(7));
    }

    #[test]
    fn strict_send_op_capture_decode_failure_aborts_walk() {
        let (mut e, handle) = engine_with_file(
            MANIFEST,
            b"\xff\xd8\x01\x02\x03\x04\x05\x06\x07\x08\xff\xd9",
        );
        let steps = vec![Step {
            send_op: Some("0x101b".into()),
            params: vec![
                StepParam::Literal(handle),
                StepParam::Literal(0),
                StepParam::Literal(4),
            ],
            captures: vec![Capture {
                bind: "tooWide".into(),
                source: CaptureSource::U64Le,
                fallback: None,
            }],
            ..Default::default()
        }];

        let err = walk_ptpip(&mut e, &steps, &BTreeMap::new()).unwrap_err();
        assert!(
            err.message.contains("decode captured u64Le"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn send_op_capture_bindings_are_atomic_on_later_failure() {
        let (mut e, handle) = engine_with_file(
            MANIFEST,
            b"\x01\x02\x03\x04\x05\x06\x07\x08stream-tail\xff\xd9",
        );
        let steps = vec![
            Step {
                send_op: Some("0x101b".into()),
                params: vec![
                    StepParam::Literal(handle),
                    StepParam::Literal(0),
                    StepParam::Literal(4),
                ],
                captures: vec![
                    Capture {
                        bind: "firstCapture".into(),
                        source: CaptureSource::U32Le,
                        fallback: None,
                    },
                    Capture {
                        bind: "secondCapture".into(),
                        source: CaptureSource::U64Le,
                        fallback: None,
                    },
                ],
                tolerant: true,
                ..Default::default()
            },
            Step {
                if_step: Some(camera_config::model::IfStep {
                    slot: "firstCapture".into(),
                    equals: 0x0403_0201,
                    then_steps: vec![Step {
                        set_prop: Some("0xdf00".into()),
                        value: Some(13.into()),
                        ..Default::default()
                    }],
                    else_steps: Vec::new(),
                }),
                ..Default::default()
            },
            Step {
                get_prop: Some("0xdf00".into()),
                ..Default::default()
            },
        ];

        let err = walk_ptpip(&mut e, &steps, &BTreeMap::new()).unwrap_err();
        assert!(
            err.message.contains("if slot 'firstCapture' unbound"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn tolerant_non_ok_send_op_skips_captures() {
        let mut e = engine(MANIFEST);
        let steps = vec![
            Step {
                send_op: Some("0x101b".into()),
                params: vec![
                    StepParam::Literal(0xfeed_beef),
                    StepParam::Literal(0),
                    StepParam::Literal(4),
                ],
                captures: vec![Capture {
                    bind: "missingObjectHead".into(),
                    source: CaptureSource::U32Le,
                    fallback: None,
                }],
                tolerant: true,
                ..Default::default()
            },
            Step {
                if_step: Some(camera_config::model::IfStep {
                    slot: "missingObjectHead".into(),
                    equals: 0,
                    then_steps: vec![Step {
                        set_prop: Some("0xdf00".into()),
                        value: Some(17.into()),
                        ..Default::default()
                    }],
                    else_steps: Vec::new(),
                }),
                ..Default::default()
            },
            Step {
                get_prop: Some("0xdf00".into()),
                ..Default::default()
            },
        ];

        let err = walk_ptpip(&mut e, &steps, &BTreeMap::new()).unwrap_err();
        assert!(
            err.message.contains("if slot 'missingObjectHead' unbound"),
            "unexpected error: {err}"
        );
    }

    /// #407: a tolerant `getProp` skips a non-OK advisory read instead of
    /// aborting the walk; a strict read still fails.
    #[test]
    fn tolerant_get_prop_skips_non_ok_reads() {
        let mut e = engine(MANIFEST);
        let steps = vec![
            Step {
                get_prop: Some("0x4321".into()), // undeclared → DevicePropNotSupported
                tolerant: true,
                ..Default::default()
            },
            Step {
                get_prop: Some("0xdf00".into()),
                ..Default::default()
            },
        ];
        let out = walk_ptpip(&mut e, &steps, &BTreeMap::new()).expect("tolerant read skips");
        assert_eq!(out.observed.get(0x4321), None, "skipped read binds nothing");
        assert_eq!(out.observed.get(0xdf00), Some(0));
        assert_eq!(out.steps_run, 2);
    }

    #[test]
    fn strict_get_prop_aborts_on_non_ok_reads() {
        let mut e = engine(MANIFEST);
        let steps = vec![Step {
            get_prop: Some("0x4321".into()),
            ..Default::default()
        }];
        let err = walk_ptpip(&mut e, &steps, &BTreeMap::new()).unwrap_err();
        assert!(
            err.message.contains("GetDevicePropValue(0x4321)"),
            "unexpected error: {err}"
        );
        assert_eq!(err.response_code, Some(resp::DEVICE_PROP_NOT_SUPPORTED));
    }

    #[test]
    fn prop_value_fallback_selects_only_its_authored_response() {
        let fallback = camera_config::model::CaptureResponseFallback {
            value: 0x0020_0000,
            when_response_codes: vec!["0x200a".into()],
        };
        let capture = Capture {
            bind: "chunkSize".into(),
            source: CaptureSource::PropValue,
            fallback: Some(fallback),
        };

        let mut success = engine(MANIFEST);
        let out = walk_ptpip(
            &mut success,
            &[
                Step {
                    get_prop: Some("0xdf00".into()),
                    captures: vec![capture.clone()],
                    ..Default::default()
                },
                Step {
                    if_step: Some(camera_config::model::IfStep {
                        slot: "chunkSize".into(),
                        equals: 0,
                        then_steps: vec![Step {
                            set_prop: Some("0xdf00".into()),
                            value: Some(13.into()),
                            ..Default::default()
                        }],
                        else_steps: Vec::new(),
                    }),
                    ..Default::default()
                },
                Step {
                    get_prop: Some("0xdf00".into()),
                    ..Default::default()
                },
            ],
            &BTreeMap::new(),
        )
        .expect("successful property capture wins over the fallback");
        assert_eq!(out.observed.get(0xdf00), Some(13));

        let mut selected = engine(MANIFEST);
        let out = walk_ptpip(
            &mut selected,
            &[
                Step {
                    get_prop: Some("0x4321".into()),
                    captures: vec![capture.clone()],
                    ..Default::default()
                },
                Step {
                    if_step: Some(camera_config::model::IfStep {
                        slot: "chunkSize".into(),
                        equals: 0x0020_0000,
                        then_steps: vec![Step {
                            set_prop: Some("0xdf00".into()),
                            value: Some(17.into()),
                            ..Default::default()
                        }],
                        else_steps: Vec::new(),
                    }),
                    ..Default::default()
                },
                Step {
                    get_prop: Some("0xdf00".into()),
                    ..Default::default()
                },
            ],
            &BTreeMap::new(),
        )
        .expect("selected response binds the authored fallback");
        assert_eq!(out.observed.get(0xdf00), Some(17));

        let mut unselected = engine(MANIFEST);
        unselected
            .install_fault(FaultSpec {
                selector: FaultSelector {
                    operation: op::GET_DEVICE_PROP_VALUE,
                    params: Some(vec![0x4321]),
                    skip: 0,
                    count: None,
                },
                mutation: FaultMutation::FailResponse { response: 0x2002 },
            })
            .unwrap();
        let error = walk_ptpip(
            &mut unselected,
            &[Step {
                get_prop: Some("0x4321".into()),
                captures: vec![capture],
                ..Default::default()
            }],
            &BTreeMap::new(),
        )
        .expect_err("unselected response remains terminal");
        assert_eq!(error.response_code, Some(0x2002));
    }

    /// #455 review: a tolerant read skips coded non-OK responses, but a
    /// dropped socket (`Reply::Close`) stays fatal, matching `check_ok`.
    #[test]
    fn tolerant_get_prop_still_fails_on_close_fault() {
        let mut e = engine(MANIFEST);
        e.install_fault(FaultSpec {
            selector: FaultSelector {
                operation: op::GET_DEVICE_PROP_VALUE,
                params: Some(vec![0x4321]),
                skip: 0,
                count: None,
            },
            mutation: FaultMutation::Close {
                stage: FaultStage::Command,
            },
        })
        .unwrap();
        let steps = vec![Step {
            get_prop: Some("0x4321".into()),
            tolerant: true,
            ..Default::default()
        }];
        let err = walk_ptpip(&mut e, &steps, &BTreeMap::new()).unwrap_err();
        assert!(
            err.message.contains("GetDevicePropValue(0x4321)"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn scalar_capture_can_decode_from_stream_reply_head() {
        let stream_head = 0x0807_0605_0403_0201u64;
        let (mut e, handle) = engine_with_file(
            MANIFEST,
            b"\x01\x02\x03\x04\x05\x06\x07\x08stream-tail\xff\xd9",
        );
        let steps = vec![
            Step {
                send_op: Some("0x101b".into()),
                params: vec![
                    StepParam::Literal(handle),
                    StepParam::Literal(0),
                    StepParam::Literal(8),
                ],
                captures: vec![Capture {
                    bind: "streamHead".into(),
                    source: CaptureSource::U64Le,
                    fallback: None,
                }],
                ..Default::default()
            },
            Step {
                if_step: Some(camera_config::model::IfStep {
                    slot: "streamHead".into(),
                    equals: stream_head,
                    then_steps: vec![Step {
                        set_prop: Some("0xdf00".into()),
                        value: Some(11.into()),
                        ..Default::default()
                    }],
                    else_steps: Vec::new(),
                }),
                ..Default::default()
            },
            Step {
                get_prop: Some("0xdf00".into()),
                ..Default::default()
            },
        ];

        let out = walk_ptpip(&mut e, &steps, &BTreeMap::new()).expect("stream capture binds");
        assert_eq!(out.observed.get(0xdf00), Some(11));
    }

    #[test]
    fn runtime_value_transform_does_not_overflow_shift() {
        assert_eq!(transform_runtime_value(0x0000_0001_2345_6789, 32, None), 1);
        assert_eq!(
            transform_runtime_value(0x0000_0001_2345_6789, 0, Some(0xffff_ffff)),
            0x2345_6789
        );
        assert_eq!(transform_runtime_value(0xffff_ffff_ffff_ffff, 64, None), 0);
        assert_eq!(
            transform_runtime_value(0xffff_ffff_ffff_ffff, 128, Some(0xffff_ffff)),
            0
        );
    }

    #[test]
    fn await_until_satisfied_on_first_poll() {
        // An unset prop reads its typed default (0); `until eq 0` holds at once.
        let mut e = engine(MANIFEST);
        let steps = vec![Step {
            await_until: Some(AwaitUntil {
                source: AwaitSource::Poll {
                    prop: "0xdf01".into(),
                },
                until: leaf_eq("0xdf01", 0),
                on_each: vec![],
                timeout_ms: 5000,
                interval_ms: 0,
            }),
            ..Default::default()
        }];
        let out = walk_ptpip(&mut e, &steps, &BTreeMap::new()).expect("walk ok");
        assert_eq!(out.await_iterations, vec![1]);
        assert_eq!(out.observed.get(0xdf01), Some(0));
    }

    /// #42 keystone round-trip: tap-to-AF (`0x9026`) then poll `0xd209`
    /// `S1_LOCK_COLOR` until it flips to locked (1). The camera-side flip is
    /// modeled as an op-effect with a 2-poll settle delay, so the poll-until
    /// loop genuinely iterates before satisfying — proving executor +
    /// op-effects-in-data + awaitUntil round-trip sim-side end to end.
    #[test]
    fn af_lock_round_trips_via_op_effect_and_poll_until() {
        const AF_MANIFEST: &str = r#"
schema: camera-config/v1
camera: { manufacturer: FUJIFILM, model: GFX100 II, firmware: "2.30" }
operations:
  "0x9026":
    name: LockS1Lock
    effects:
      - { setProp: "0xd209", value: 1, settleAfterPolls: 2 }
properties:
  "0xd209": { name: s1LockColor, type: u16, access: readOnly }
"#;
        let mut e = engine(AF_MANIFEST);
        let steps = vec![
            // Tap-to-AF: encoded AF-area param (asp_w, asp_h, col, row).
            Step {
                send_op: Some("0x9026".into()),
                params: vec![StepParam::Literal(0x0906_0403)],
                ..Default::default()
            },
            // Poll S1_LOCK_COLOR until the box turns green (locked).
            Step {
                await_until: Some(AwaitUntil {
                    source: AwaitSource::Poll {
                        prop: "0xd209".into(),
                    },
                    until: leaf_eq("0xd209", 1),
                    on_each: vec![],
                    timeout_ms: 5000,
                    interval_ms: 250,
                }),
                ..Default::default()
            },
        ];
        let out = walk_ptpip(&mut e, &steps, &BTreeMap::new()).expect("AF flow round-trips");
        // The loop iterated (settle delay) then satisfied on the 2nd poll.
        assert_eq!(out.await_iterations, vec![2]);
        assert!(out.await_iterations[0] > 1, "poll-until loop exercised");
        // The dispatcher observed the locked color.
        assert_eq!(out.observed.get(0xd209), Some(1));
    }

    #[test]
    fn await_until_times_out_when_never_satisfied() {
        // Nothing flips 0xdf01 off its default; `until eq 1` can never hold.
        let mut e = engine(MANIFEST);
        let steps = vec![Step {
            await_until: Some(AwaitUntil {
                source: AwaitSource::Poll {
                    prop: "0xdf01".into(),
                },
                until: leaf_eq("0xdf01", 1),
                on_each: vec![],
                timeout_ms: 5000,
                interval_ms: 0,
            }),
            ..Default::default()
        }];
        let e_err = walk_ptpip(&mut e, &steps, &BTreeMap::new()).unwrap_err();
        assert!(e_err.message.contains("not satisfied"), "{e_err}");
    }

    /// #54 hybrid round-trip, #185 re-poll: tap-to-AF (`0x9026`) emits the
    /// `0xC005` AFCAPTUER completion AND arms a `0xd209` → 1 transition that
    /// settles AFTER the event (fw02.30 fires the event before the status
    /// latches, client application#157). The event-source `awaitUntil` takes the event,
    /// then re-polls `then_poll` until the predicate holds — the first read
    /// sees pre-settle, the second resolves it.
    #[test]
    fn af_capture_round_trips_via_event_source_then_repoll() {
        const AF_EVENT_MANIFEST: &str = r#"
schema: camera-config/v1
camera: { manufacturer: FUJIFILM, model: GFX100 II, firmware: "2.30" }
operations:
  "0x9026":
    name: LockS1Lock
    effects:
      - { setProp: "0xd209", value: 1, settleAfterPolls: 2 }
    emits: ["0xc005"]
properties:
  "0xd209": { name: s1LockColor, type: u16, access: readOnly }
"#;
        let mut e = engine(AF_EVENT_MANIFEST);
        let steps = vec![
            Step {
                send_op: Some("0x9026".into()),
                params: vec![StepParam::Literal(0x0906_0403)],
                ..Default::default()
            },
            Step {
                await_until: Some(AwaitUntil {
                    source: AwaitSource::Event {
                        code: "0xc005".into(),
                        then_poll: Some("0xd209".into()),
                    },
                    until: leaf_eq("0xd209", 1),
                    on_each: vec![],
                    timeout_ms: 5000,
                    interval_ms: 100,
                }),
                ..Default::default()
            },
        ];
        let out = walk_ptpip(&mut e, &steps, &BTreeMap::new()).expect("AF capture round-trips");
        // settle=2: the first post-event read sees pre-settle 0, the re-poll
        // resolves the transition — two observations, not a single-shot.
        assert_eq!(out.await_iterations, vec![2]);
        assert_eq!(out.observed.get(0xd209), Some(1));
    }

    /// #54: an event that is never emitted fails a strict event source, and
    /// passes a tolerant one with a distinguishable `await_iterations` of 0.
    #[test]
    fn event_source_handles_missing_event() {
        // 0x9026 has NO `emits`, so 0xc005 never arrives.
        const NO_EMIT_MANIFEST: &str = r#"
schema: camera-config/v1
camera: { manufacturer: FUJIFILM, model: GFX100 II, firmware: "2.30" }
operations:
  "0x9026": { name: LockS1Lock }
properties:
  "0xd209": { name: s1LockColor, type: u16, access: readOnly }
"#;
        let event_step = |tolerant| Step {
            await_until: Some(AwaitUntil {
                source: AwaitSource::Event {
                    code: "0xc005".into(),
                    then_poll: Some("0xd209".into()),
                },
                until: leaf_eq("0xd209", 1),
                on_each: vec![],
                timeout_ms: 5000,
                interval_ms: 0,
            }),
            tolerant,
            ..Default::default()
        };
        // Strict: missing event is a hard error.
        let mut e = engine(NO_EMIT_MANIFEST);
        let steps = vec![
            Step {
                send_op: Some("0x9026".into()),
                ..Default::default()
            },
            event_step(false),
        ];
        let err = walk_ptpip(&mut e, &steps, &BTreeMap::new()).unwrap_err();
        assert!(err.message.contains("not emitted"), "{err}");
        // Tolerant: bails with await_iterations == [0] (≠ a poll loop's ≥1).
        let mut e = engine(NO_EMIT_MANIFEST);
        let steps = vec![
            Step {
                send_op: Some("0x9026".into()),
                ..Default::default()
            },
            event_step(true),
        ];
        let out = walk_ptpip(&mut e, &steps, &BTreeMap::new()).expect("tolerant bail");
        assert_eq!(out.await_iterations, vec![0]);
    }

    /// #54: `thenPoll: None` — event arrival alone satisfies `until` over the
    /// scope a prior `getProp` seeded (no post-event read).
    #[test]
    fn event_source_then_poll_none_uses_existing_scope() {
        const MANIFEST_EVENT: &str = r#"
schema: camera-config/v1
camera: { manufacturer: FUJIFILM, model: GFX100 II, firmware: "2.30" }
operations:
  "0x9026":
    name: LockS1Lock
    emits: ["0xc001"]
properties:
  "0xd400": { name: probe, type: u16, access: readOnly, descriptor: { form: enum, values: [7] } }
"#;
        let mut e = engine(MANIFEST_EVENT);
        let steps = vec![
            // Seed scope: 0xd400 reads its descriptor default (7).
            Step {
                get_prop: Some("0xd400".into()),
                ..Default::default()
            },
            Step {
                send_op: Some("0x9026".into()),
                ..Default::default()
            },
            Step {
                await_until: Some(AwaitUntil {
                    source: AwaitSource::Event {
                        code: "0xc001".into(),
                        then_poll: None,
                    },
                    until: leaf_eq("0xd400", 7),
                    on_each: vec![],
                    timeout_ms: 5000,
                    interval_ms: 0,
                }),
                ..Default::default()
            },
        ];
        let out = walk_ptpip(&mut e, &steps, &BTreeMap::new()).expect("event arrival satisfies");
        assert_eq!(out.await_iterations, vec![1]);
        assert_eq!(out.observed.get(0xd400), Some(7));
    }
}
