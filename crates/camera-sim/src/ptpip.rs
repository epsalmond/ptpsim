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
    AwaitSource, AwaitUntil, Capture, CaptureSource, ChunkSize, Loop, Step, StepParam,
};
use camera_config::{parse_hex_code, PropView};
use ptp_core::codes::{op, resp};
use ptp_core::dataset::PropValue;
use ptp_core::{ObjectInfo, OperationRequest, Reader, Writer};

use crate::engine::{Engine, Reply};
use crate::state::{datatype_of, typed, Phase};

/// Reference-executor bound on an `awaitUntil` loop: the deterministic analogue
/// of the dispatcher's wall-clock `timeout_ms` (§11.15). A condition that never
/// holds hits this and fails like a real timeout rather than spinning forever.
/// Mirrors `crate::ble::MAX_AWAIT_ITERS`.
const MAX_AWAIT_ITERS: usize = 256;

/// Defensive runaway guard on a `forEach` loop (#46). The handle list is a finite
/// `Vec` the engine returns, so this is never hit in practice — it bounds a
/// corrupt array count, not real iteration. Set high enough for any real card.
const MAX_FOREACH_ITERS: usize = 100_000;

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
}

/// Walk failure: which step (by verb + position) and why. Tolerant steps never
/// produce one — their non-OK responses are skipped like a real dispatcher.
#[derive(Debug)]
pub struct PtpIpError {
    pub step: String,
    pub message: String,
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
        runtime_params: runtime_params.clone(),
        tid: 1,
        steps_run: 0,
        await_iterations: Vec::new(),
        loop_iterations: Vec::new(),
        bindings: BTreeMap::new(),
        command_listener_volatile,
    };
    // Session bring-up (idempotent): the responder rejects most ops before it.
    if !session_already_open {
        ctx.simple_op(op::OPEN_SESSION, vec![1], false)
            .map_err(|message| PtpIpError {
                step: "openSession".into(),
                message,
            })?;
    }
    ctx.walk_steps(steps, "steps")?;
    Ok(PtpIpOutcome {
        observed: ctx.observed,
        steps_run: ctx.steps_run,
        await_iterations: ctx.await_iterations,
        loop_iterations: ctx.loop_iterations,
    })
}

struct Ctx<'a> {
    engine: &'a mut Engine,
    /// The PTP-IP scope: observed property values accumulated from polls. A
    /// `getProp`/`readEcho`/`awaitUntil` poll lands the typed value here keyed
    /// by prop code; `until` predicates evaluate over it.
    observed: PropView,
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
                self.run_step(step, &here)?;
            }
            self.steps_run += 1;
        }
        Ok(())
    }

    fn run_step(&mut self, step: &Step, here: &str) -> Result<(), PtpIpError> {
        let err = |message: String| PtpIpError {
            step: here.to_string(),
            message,
        };
        if let Some(p) = &step.set_prop {
            let code = parse_hex_code(p).ok_or_else(|| err(format!("bad prop code {p:?}")))?;
            let value = step.value.unwrap_or(0);
            self.set_prop(code, value, step.tolerant).map_err(err)
        } else if let Some(p) = &step.get_prop {
            let code = parse_hex_code(p).ok_or_else(|| err(format!("bad prop code {p:?}")))?;
            let v = self.poll_prop(code).map_err(err)?;
            self.observed.set(code, v);
            self.capture_prop_value(&step.captures, v).map_err(err)?;
            Ok(())
        } else if let Some(p) = &step.read_echo {
            // Read, then write the same value back (the live-view 0xdf2a echo).
            let code = parse_hex_code(p).ok_or_else(|| err(format!("bad prop code {p:?}")))?;
            let v = self.poll_prop(code).map_err(err)?;
            self.observed.set(code, v);
            self.capture_prop_value(&step.captures, v).map_err(err)?;
            self.set_prop(code, v, step.tolerant).map_err(err)
        } else if let Some(o) = &step.send_op {
            let code = parse_hex_code(o).ok_or_else(|| err(format!("bad op code {o:?}")))?;
            let params = self.resolve_params(&step.params).map_err(err)?;
            let reply = self.issue_op(code, params);
            if reply_ok(&reply) {
                self.apply_captures(&step.captures, code, &reply)
                    .map_err(err)?;
                if code == op::GET_OBJECT_INFO && step.captures.is_empty() {
                    self.capture_object_info(OBJECT_SIZE_SLOT, &reply)
                        .map_err(err)?;
                }
            }
            check_ok(&reply, code, step.tolerant).map_err(err)
        } else if step.reopen_session.is_some() {
            if self.command_listener_volatile
                && matches!(self.engine.phase(), Phase::LiveView | Phase::Streaming)
            {
                // The camera tore down the command-port listener on the transport-
                // close while live-view was active, so the reconnect is refused —
                // switch live-view → image-transfer in-session (#103).
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
                Ok(())
            }
        } else {
            Err(err("step sets no action verb".into()))
        }
    }

    /// A closed declarative loop (#46): `forEach` over a captured collection or a
    /// `chunk`-by-size walk. The executor owns all cursor advancement; the body
    /// reuses the ordinary step grammar with the bound slots resolvable as runtime
    /// params. Each runs under a deterministic cap (the `awaitUntil` analogue).
    fn run_loop(&mut self, lp: &Loop, tolerant: bool, here: &str) -> Result<(), PtpIpError> {
        let err = |message: String| PtpIpError {
            step: here.to_string(),
            message,
        };
        match lp {
            Loop::ForEach {
                in_prop,
                bind,
                body,
            } => {
                let code = parse_hex_code(in_prop)
                    .ok_or_else(|| err(format!("bad forEach prop {in_prop:?}")))?;
                let items = self.poll_collection(code).map_err(err)?;
                if items.len() > MAX_FOREACH_ITERS && !tolerant {
                    return Err(err(format!(
                        "forEach over {code:#06x} has {} elements, exceeds cap {MAX_FOREACH_ITERS}",
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
    /// list) → decode the count-prefixed `u32` array → `Vec<i64>`. The forEach
    /// source. A scalar/non-array reply is a hard error (tolerant-aware at the loop).
    fn poll_collection(&mut self, code: u16) -> Result<Vec<u64>, String> {
        let tid = self.next_tid();
        let req = OperationRequest {
            data_phase_info: 1,
            code: op::GET_DEVICE_PROP_VALUE,
            transaction_id: tid,
            params: vec![code as u32],
        };
        let reply = self.engine.on_operation(&req, None);
        match reply {
            Reply::Data { data, response } if response.code == resp::OK => {
                let mut r = Reader::new(&data);
                let items = r
                    .ptp_array(|r| r.u32())
                    .map_err(|e| format!("decode array prop {code:#06x}: {e:?}"))?;
                Ok(items.into_iter().map(|v| v as u64).collect())
            }
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
        };
        match &aw.source {
            AwaitSource::Poll { prop } => {
                let code =
                    parse_hex_code(prop).ok_or_else(|| err(format!("bad source prop {prop:?}")))?;
                for iter in 1..=MAX_AWAIT_ITERS {
                    let v = self.poll_prop(code).map_err(err)?;
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
                        let v = self.poll_prop(pc).map_err(err)?;
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

    /// SetDevicePropValue: encode `value` at the property's datatype width and
    /// drive the data-out phase.
    fn set_prop(&mut self, code: u16, value: i64, tolerant: bool) -> Result<(), String> {
        let dt = self.datatype(code);
        let mut w = Writer::new();
        typed(dt, value)
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
        check_ok(&reply, op::SET_DEVICE_PROP_VALUE, tolerant)
    }

    /// GetDevicePropValue → decode at the property's datatype width → i64.
    fn poll_prop(&mut self, code: u16) -> Result<i64, String> {
        let dt = self.datatype(code);
        let tid = self.next_tid();
        let req = OperationRequest {
            data_phase_info: 1,
            code: op::GET_DEVICE_PROP_VALUE,
            transaction_id: tid,
            params: vec![code as u32],
        };
        let reply = self.engine.on_operation(&req, None);
        match reply {
            Reply::Data { data, response } if response.code == resp::OK => {
                let mut r = Reader::new(&data);
                let v = PropValue::decode(&mut r, dt)
                    .map_err(|e| format!("decode prop {code:#06x}: {e:?}"))?;
                prop_value_to_i64(&v)
                    .ok_or_else(|| format!("prop {code:#06x} is non-numeric (string)"))
            }
            other => Err(format!(
                "GetDevicePropValue({code:#06x}) -> {}",
                describe_reply(&other)
            )),
        }
    }

    /// A no-data operation (send_op / session ops). `repeat` is applied by the
    /// caller; this fires exactly one request.
    fn simple_op(&mut self, code: u16, params: Vec<u32>, tolerant: bool) -> Result<(), String> {
        let reply = self.issue_op(code, params);
        check_ok(&reply, code, tolerant)
    }

    fn issue_op(&mut self, code: u16, params: Vec<u32>) -> Reply {
        let tid = self.next_tid();
        let req = OperationRequest {
            data_phase_info: 1,
            code,
            transaction_id: tid,
            params,
        };
        self.engine.on_operation(&req, None)
    }

    fn apply_captures(
        &mut self,
        captures: &[Capture],
        code: u16,
        reply: &Reply,
    ) -> Result<(), String> {
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
                CaptureSource::U32Le => {
                    let Reply::Data { data, .. } = reply else {
                        return Err("u32Le capture requires a data reply".into());
                    };
                    let mut r = Reader::new(data);
                    r.u32()
                        .map_err(|e| format!("decode captured u32Le: {e:?}"))?
                        as u64
                }
                CaptureSource::U64Le => {
                    let Reply::Data { data, .. } = reply else {
                        return Err("u64Le capture requires a data reply".into());
                    };
                    let mut r = Reader::new(data);
                    r.u64()
                        .map_err(|e| format!("decode captured u64Le: {e:?}"))?
                }
            };
            self.bindings.insert(capture.bind.clone(), value);
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
    } else if s.reopen_session.is_some() {
        "reopenSession"
    } else if s.await_until.is_some() {
        "awaitUntil"
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
    match reply {
        Reply::Response(r)
        | Reply::Data { response: r, .. }
        | Reply::DataStream { response: r, .. } => {
            if r.code == resp::OK || tolerant {
                Ok(())
            } else {
                Err(format!("op {code:#06x} -> response {:#06x}", r.code))
            }
        }
        Reply::NoResponse => Err(format!("op {code:#06x} timed out with no response")),
        Reply::Close => Err(format!("op {code:#06x} closed the connection")),
    }
}

fn reply_ok(reply: &Reply) -> bool {
    match reply {
        Reply::Response(r)
        | Reply::Data { response: r, .. }
        | Reply::DataStream { response: r, .. } => r.code == resp::OK,
        Reply::NoResponse | Reply::Close => false,
    }
}

fn describe_reply(reply: &Reply) -> String {
    match reply {
        Reply::Response(r) => format!("response {:#06x}", r.code),
        Reply::Data { response, .. } => format!("data + response {:#06x}", response.code),
        Reply::DataStream { response, .. } => format!("stream + response {:#06x}", response.code),
        Reply::NoResponse => "no response".into(),
        Reply::Close => "connection closed".into(),
    }
}

fn prop_value_to_i64(v: &PropValue) -> Option<i64> {
    Some(match v {
        PropValue::U8(x) => *x as i64,
        PropValue::U16(x) => *x as i64,
        PropValue::U32(x) => *x as i64,
        PropValue::U64(x) => *x as i64,
        PropValue::Str(_) => return None,
    })
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
    use camera_config::{CameraManifest, Leaf, Predicate};
    use camera_media_store::MediaStore;

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
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("ptpsim-ptpip-{nanos}"));
        std::fs::create_dir_all(&root).unwrap();
        MediaStore::open(&root).unwrap()
    }

    fn engine(yaml: &str) -> Engine {
        let manifest = CameraManifest::from_yaml(yaml).expect("manifest loads");
        Engine::new(manifest, empty_store())
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

    #[test]
    fn plain_set_get_sendop_sequence_round_trips() {
        let mut e = engine(MANIFEST);
        let steps = vec![
            Step {
                set_prop: Some("0xdf00".into()),
                value: Some(6),
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
