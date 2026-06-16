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

use camera_config::model::{AwaitSource, AwaitUntil, Step, StepParam};
use camera_config::{parse_hex_code, PropView};
use ptp_core::codes::{op, resp};
use ptp_core::dataset::PropValue;
use ptp_core::{OperationRequest, Reader, Writer};

use crate::engine::{Engine, Reply};
use crate::state::{datatype_of, typed};

/// Reference-executor bound on an `awaitUntil` loop: the deterministic analogue
/// of the dispatcher's wall-clock `timeout_ms` (§11.15). A condition that never
/// holds hits this and fails like a real timeout rather than spinning forever.
/// Mirrors `crate::ble::MAX_AWAIT_ITERS`.
const MAX_AWAIT_ITERS: usize = 256;

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
    let mut ctx = Ctx {
        engine,
        observed: PropView::new(),
        runtime_params: runtime_params.clone(),
        tid: 1,
        steps_run: 0,
        await_iterations: Vec::new(),
    };
    // Session bring-up (idempotent): the responder rejects most ops before it.
    ctx.simple_op(op::OPEN_SESSION, vec![1], false)
        .map_err(|message| PtpIpError {
            step: "openSession".into(),
            message,
        })?;
    ctx.walk_steps(steps, "steps")?;
    Ok(PtpIpOutcome {
        observed: ctx.observed,
        steps_run: ctx.steps_run,
        await_iterations: ctx.await_iterations,
    })
}

struct Ctx<'a> {
    engine: &'a mut Engine,
    /// The PTP-IP scope: observed property values accumulated from polls. A
    /// `getProp`/`readEcho`/`awaitUntil` poll lands the typed value here keyed
    /// by prop code; `until` predicates evaluate over it.
    observed: PropView,
    runtime_params: BTreeMap<String, String>,
    tid: u32,
    steps_run: usize,
    await_iterations: Vec<usize>,
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
            Ok(())
        } else if let Some(p) = &step.read_echo {
            // Read, then write the same value back (the live-view 0xdf2a echo).
            let code = parse_hex_code(p).ok_or_else(|| err(format!("bad prop code {p:?}")))?;
            let v = self.poll_prop(code).map_err(err)?;
            self.observed.set(code, v);
            self.set_prop(code, v, step.tolerant).map_err(err)
        } else if let Some(o) = &step.send_op {
            let code = parse_hex_code(o).ok_or_else(|| err(format!("bad op code {o:?}")))?;
            let params = self.resolve_params(&step.params).map_err(err)?;
            self.simple_op(code, params, step.tolerant).map_err(err)
        } else if step.reopen_session.is_some() {
            // Deterministic analogue of the TCP teardown/reconnect: close then
            // re-open the session in place.
            self.simple_op(op::CLOSE_SESSION, vec![], step.tolerant)
                .map_err(err)?;
            self.simple_op(op::OPEN_SESSION, vec![1], step.tolerant)
                .map_err(err)
        } else if let Some(aw) = &step.await_until {
            self.run_await_until(aw, step.tolerant, here)
        } else {
            Err(err("step sets no action verb".into()))
        }
    }

    /// `awaitUntil` (§11.16): observe until `until` holds. The `poll` source loops
    /// (`source` polled each iteration, `on_each` run when unsatisfied,
    /// deterministic timeout [`MAX_AWAIT_ITERS`]); the `event` source is
    /// single-shot — take the completion event off the engine queue, then one
    /// post-event read of `then_poll` and a single `until` eval (the hybrid
    /// push-then-read). A non-numeric/unsupported source poll is a hard error.
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
                // Single-shot: the push channel IS the loop. The event is either
                // already queued (the triggering op's `emits` fired) or it never
                // will be — the analogue of BLE notify source-exhaustion.
                if !self.engine.take_event(ev) {
                    if tolerant {
                        self.await_iterations.push(0);
                        return Ok(());
                    }
                    return Err(err(format!("awaited event {ev:#06x} was not emitted")));
                }
                // The event is the readiness signal: ONE post-event value read.
                if let Some(tp) = then_poll {
                    let pc = parse_hex_code(tp)
                        .ok_or_else(|| err(format!("bad thenPoll prop {tp:?}")))?;
                    let v = self.poll_prop(pc).map_err(err)?;
                    self.observed.set(pc, v);
                }
                if aw.until.eval(&self.observed) || tolerant {
                    self.await_iterations.push(1);
                    Ok(())
                } else {
                    Err(err(format!(
                        "`until` not satisfied after event {ev:#06x} + single read"
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
        let tid = self.next_tid();
        let req = OperationRequest {
            data_phase_info: 1,
            code,
            transaction_id: tid,
            params,
        };
        let reply = self.engine.on_operation(&req, None);
        check_ok(&reply, code, tolerant)
    }

    fn resolve_params(&self, params: &[StepParam]) -> Result<Vec<u32>, String> {
        params
            .iter()
            .map(|p| match p {
                StepParam::Literal(v) => Ok(*v),
                StepParam::Runtime { runtime } => {
                    let raw = self
                        .runtime_params
                        .get(runtime)
                        .ok_or_else(|| format!("runtime slot '{runtime}' unbound"))?;
                    parse_u32(raw).ok_or_else(|| {
                        format!("runtime slot '{runtime}' value {raw:?} is not a u32")
                    })
                }
            })
            .collect()
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
    } else {
        "?"
    }
}

/// OK unless a non-OK response (tolerated → skipped) or a transport-level
/// failure (Close → always aborts, like a dropped socket).
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
        Reply::Close => Err(format!("op {code:#06x} closed the connection")),
    }
}

fn describe_reply(reply: &Reply) -> String {
    match reply {
        Reply::Response(r) => format!("response {:#06x}", r.code),
        Reply::Data { response, .. } => format!("data + response {:#06x}", response.code),
        Reply::DataStream { response, .. } => format!("stream + response {:#06x}", response.code),
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

fn parse_u32(s: &str) -> Option<u32> {
    let t = s.trim();
    if let Some(hex) = t.strip_prefix("0x").or_else(|| t.strip_prefix("0X")) {
        u32::from_str_radix(hex, 16).ok()
    } else {
        t.parse::<u32>().ok()
    }
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

    /// #54 hybrid round-trip: tap-to-AF (`0x9026`) emits the `0xC005` AFCAPTUER
    /// completion AND arms a `0xd209` → 1 transition that settles in one poll.
    /// The event-source `awaitUntil` takes the event, then does ONE post-event
    /// read which resolves the pending value — proving push-then-read end to end.
    #[test]
    fn af_capture_round_trips_via_event_source_then_single_read() {
        const AF_EVENT_MANIFEST: &str = r#"
schema: camera-config/v1
camera: { manufacturer: FUJIFILM, model: GFX100 II, firmware: "2.30" }
operations:
  "0x9026":
    name: LockS1Lock
    effects:
      - { setProp: "0xd209", value: 1, settleAfterPolls: 1 }
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
                    interval_ms: 0,
                }),
                ..Default::default()
            },
        ];
        let out = walk_ptpip(&mut e, &steps, &BTreeMap::new()).expect("AF capture round-trips");
        // Single-shot: one post-event read (not a poll loop).
        assert_eq!(out.await_iterations, vec![1]);
        // The single post-event read resolved the settle=1 transition.
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
