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

use camera_config::model::{AwaitUntil, Step, StepParam};
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

    /// `awaitUntil` (§11.15): poll `source` until `until` holds over the observed
    /// scope, running `on_each` each unsatisfied iteration. Deterministic timeout
    /// = [`MAX_AWAIT_ITERS`]. A non-numeric/unsupported source poll is a hard
    /// error (it can never satisfy a numeric predicate).
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
        let code = parse_hex_code(&aw.source)
            .ok_or_else(|| err(format!("bad source prop {:?}", aw.source)))?;
        for iter in 1..=MAX_AWAIT_ITERS {
            let v = self.poll_prop(code).map_err(err)?;
            self.observed.set(code, v);
            if aw.until.eval(&self.observed) {
                self.await_iterations.push(iter);
                return Ok(());
            }
            // Not yet: act, then poll again. interval_ms is dispatcher cadence —
            // the deterministic executor doesn't sleep.
            self.walk_steps(&aw.on_each, &format!("{here}.onEach"))?;
        }
        if tolerant {
            self.await_iterations.push(MAX_AWAIT_ITERS);
            return Ok(());
        }
        Err(err(format!(
            "`until` not satisfied polling {:#06x} within {MAX_AWAIT_ITERS} observations",
            code
        )))
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
                source: "0xdf01".into(),
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

    #[test]
    fn await_until_times_out_when_never_satisfied() {
        // Nothing flips 0xdf01 off its default; `until eq 1` can never hold.
        let mut e = engine(MANIFEST);
        let steps = vec![Step {
            await_until: Some(AwaitUntil {
                source: "0xdf01".into(),
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
}
