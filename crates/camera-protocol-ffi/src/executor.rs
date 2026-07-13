//! Async establishment-plan executor behind foreign async traits (issue #246).
//!
//! Inverts the historical split: instead of the app hand-writing a dispatcher
//! per platform (`docs/INTEGRATION.md` "you build a small dispatcher"), the
//! plan-walker lives here and the app supplies only raw I/O through
//! [`BleExecutorTransport`], a `with_foreign` async trait (Swift async
//! protocol / Kotlin suspend interface), plus a [`StepObserver`] for the
//! per-step outcome stream.
//!
//! Semantics are the reference walker's (`camera_sim::ble::walk_establishment`,
//! the executable spec) layered with the timing behavior the deterministic
//! walker deliberately skips: `StepOptions { retries, retry_delay_ms }` as a
//! real retry ladder, verb `timeout_ms` as a wall-clock budget, and a default
//! deadline on every transport call so a silently-stalled transport can never
//! hang a walk. Timeout enforcement is Rust-owned but clock-delegated: every
//! deadline races the pending I/O future against [`BleExecutorTransport::sleep`],
//! and dropping the losing future propagates over the FFI as task/coroutine
//! cancellation on the foreign side.
//!
//! Notifications arrive by pull — [`BleExecutorTransport::next_notification`]
//! resolves with the next payload for a subscribed characteristic. The
//! transport must buffer payloads from `subscribe` time until they are
//! consumed, so acceptance logic (which lives here, not in the app) can never
//! lose a notification that raced a seed read.

use std::collections::{BTreeMap, BTreeSet};
use std::pin::Pin;
use std::sync::Arc;

use camera_config::index::eval;
use camera_config::index::{
    AcquireSource, AwaitSource, BleAwaitUntilStep, BleNotifyStep, BleNotifyUntil,
    BleWriteChunkStep, Encoding, EstablishmentBlock, NotifyCapture, PredicateOp, Step, StepValue,
};
use futures_util::future::{select, Either, FutureExt};

use crate::{ConfigStore, KeyValue};

/// Default deadline on a single transport call (read/write/subscribe/MTU/
/// discover) — the backstop that converts a silently-stalled transport into a
/// step failure the retry ladder can act on. Verbs with an explicit
/// `timeout_ms` (notify/awaitUntil/subscribe) use that instead.
const DEFAULT_OP_TIMEOUT_MS: u32 = 10_000;

/// Deadline on `bleConnect` — connects legitimately take longer than GATT ops.
const CONNECT_TIMEOUT_MS: u32 = 30_000;

/// Poll cadence for a `bleAwaitUntil` read source when the step declares
/// `interval_ms: 0` — mirrors the app dispatcher's 200ms default.
const DEFAULT_POLL_INTERVAL_MS: u32 = 200;

/// Runaway backstop on await/notify acceptance loops. The wall-clock budget is
/// the real bound; this only stops a spin against a transport whose clock and
/// notification stream both misbehave.
const MAX_LOOP_ITERS: usize = 65_536;

// ---------------------------------------------------------------------------
// Foreign-implemented traits
// ---------------------------------------------------------------------------

/// Failure surface a transport implementation may raise. Every variant is an
/// ordinary step failure to the executor — retried per `StepOptions`, then
/// tolerated or fatal. There is deliberately no error-class discrimination in
/// the retry ladder (parity with the shipping dispatcher).
#[derive(Debug, thiserror::Error, uniffi::Error)]
pub enum TransportError {
    #[error("peripheral not connected")]
    NotConnected,
    #[error("characteristic {uuid} not exposed")]
    NotExposed { uuid: String },
    #[error("transport operation timed out: {detail}")]
    Timeout { detail: String },
    #[error("connect failed: {detail}")]
    ConnectFailed { detail: String },
    #[error("transport failure: {detail}")]
    Failed { detail: String },
}

/// Raw BLE I/O the app supplies; the executor owns everything else (plan
/// walking, retries, deadlines, captures, predicates, telemetry).
///
/// `sleep` is the host clock: the executor races pending I/O against it to
/// enforce deadlines and uses it for retry backoff and poll cadence. A
/// dropped in-flight call (deadline lost the race, or the whole run future
/// was cancelled) surfaces on the foreign side as task/coroutine cancellation.
#[uniffi::export(with_foreign)]
#[async_trait::async_trait]
pub trait BleExecutorTransport: Send + Sync {
    /// Connect the pre-bound peripheral (binding happened at recognition).
    async fn connect(&self) -> Result<(), TransportError>;
    /// Resolve when the connected peer disconnects — a wake plan expects the
    /// camera to drop the link while it boots. May stay pending indefinitely;
    /// the executor races it against the step's manifest `timeoutMs`.
    async fn await_disconnect(&self) -> Result<(), TransportError>;
    /// Request an ATT MTU and return the negotiated value. Platforms without
    /// a request API (CoreBluetooth) return the already-negotiated MTU; the
    /// executor treats the verb as a checkpoint either way.
    async fn request_mtu(&self, mtu: u16) -> Result<u16, TransportError>;
    /// Complete GATT service discovery (a checkpoint on stacks that
    /// auto-discover during connect).
    async fn ensure_services_discovered(&self) -> Result<(), TransportError>;
    async fn read(&self, characteristic: String) -> Result<Vec<u8>, TransportError>;
    async fn write(&self, characteristic: String, value: Vec<u8>) -> Result<(), TransportError>;
    /// CCCD-enable with `mode`; success is the descriptor-write ack. From this
    /// point the transport buffers notifications on the characteristic until
    /// they are consumed via [`Self::next_notification`].
    async fn subscribe(
        &self,
        characteristic: String,
        mode: crate::CccdMode,
    ) -> Result<(), TransportError>;
    /// Resolve with the next (buffered or live) notification payload for a
    /// subscribed characteristic. May stay pending indefinitely — the
    /// executor owns the deadline.
    async fn next_notification(&self, characteristic: String) -> Result<Vec<u8>, TransportError>;
    /// Resolve after `ms` milliseconds of wall-clock time.
    async fn sleep(&self, ms: u32) -> Result<(), TransportError>;
}

/// Terminal state of one step dispatch (plus the `Started` marker). Exactly
/// one `Started` and one terminal report fire per step at every nesting level.
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum StepOutcome {
    Started,
    Succeeded,
    /// The step failed after its retries but was `tolerant: true` — swallowed,
    /// walk continues. The diagnostically critical case.
    Tolerated,
    Failed,
}

/// One entry of the per-step outcome stream — the same shape the app
/// dispatcher's `StepReport` feeds the telemetry bus today.
#[derive(Debug, Clone, uniffi::Record)]
pub struct StepReport {
    /// Position path, e.g. `steps[3].bleWrite` or `steps[5].if.then[0].bleRead`.
    pub step_path: String,
    pub verb: String,
    /// The GATT UUID the verb addressed, when it addressed one.
    pub characteristic: Option<String>,
    pub outcome: StepOutcome,
    /// `Display` of the failure on `Tolerated`/`Failed`, else `None`.
    pub error: Option<String>,
    /// Retries consumed so far (0 on first-try success).
    pub attempts: u32,
}

/// Foreign observer for the step outcome stream. Fire-and-forget from the
/// executor's perspective; the app maps reports onto its telemetry bus.
#[uniffi::export(with_foreign)]
pub trait StepObserver: Send + Sync {
    fn on_step(&self, report: StepReport);
}

// ---------------------------------------------------------------------------
// Entry points
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum ExecutorStepFailureKind {
    /// An executor-owned wall-clock budget or transport timeout elapsed.
    DeadlineExceeded,
    /// A manifest-declared terminal condition matched an observation.
    ConditionRejected,
    /// Any non-timeout transport, validation, transform, or plan-step failure.
    Other,
}

#[derive(Debug, thiserror::Error, uniffi::Error)]
pub enum ExecutorError {
    #[error("unknown plan: {detail}")]
    UnknownPlan { detail: String },
    #[error("{step}: {detail}")]
    StepFailed {
        step: String,
        kind: ExecutorStepFailureKind,
        detail: String,
        context: Vec<KeyValue>,
    },
}

/// A completed walk: the final scope (recognition seed + step captures, §11.2
/// string form) and how many steps ran (branch steps counted inside-out).
#[derive(Debug, uniffi::Record)]
pub struct ExecutionOutcome {
    pub scope: Vec<KeyValue>,
    pub steps_run: u32,
}

/// Execute the establishment plan behind `plan_handle` (`model:selector`,
/// returned by establishment/reconnect decisions) against a foreign transport.
/// `initial_scope` is the recognition `runtime_scope` and `initial_encodings`
/// its `runtime_scope_encodings` — threading the real capture encodings from
/// the matched signature so a `{ captured: … }` write-back re-encodes
/// correctly without the app-side hex-string heuristic (#43). Unknown
/// encoding tokens are ignored (the scope value then decodes by fallback).
#[uniffi::export]
pub async fn run_establishment(
    store: Arc<ConfigStore>,
    plan_handle: String,
    transport: Arc<dyn BleExecutorTransport>,
    observer: Arc<dyn StepObserver>,
    initial_scope: Vec<KeyValue>,
    initial_encodings: Vec<KeyValue>,
    runtime_params: Vec<KeyValue>,
) -> Result<ExecutionOutcome, ExecutorError> {
    let block = resolve_establishment(&store, &plan_handle)?;
    let encodings = initial_encodings
        .into_iter()
        .filter_map(|kv| Encoding::from_token(&kv.value).map(|enc| (kv.key, enc)))
        .collect();

    let mut ctx = ExecCtx {
        transport: &transport,
        observer: &observer,
        scope: initial_scope
            .into_iter()
            .map(|kv| (kv.key, kv.value))
            .collect(),
        encodings,
        runtime_params: runtime_params
            .into_iter()
            .map(|kv| (kv.key, kv.value))
            .collect(),
        subscriptions: BTreeSet::new(),
        steps_run: 0,
        refine: Some(RefineCtx {
            store: &store,
            plan_handle: plan_handle.clone(),
        }),
    };
    walk_plan(&mut ctx, block.steps).await?;
    Ok(outcome(ctx))
}

/// Walk the `postExitReadiness` gate of the plan behind `plan_handle` — the
/// manifest-authored sequence proving the camera has returned to a replayable
/// state after an orderly feature exit, run before re-running
/// [`run_establishment`]. A connection declaring no gate resolves `Ok`
/// immediately with `steps_run == 0` and touches no I/O. Same walker and
/// telemetry contract as establishment; no firmware refinement — the gate is a
/// fixed sequence, §11.5 tiering applies to `steps` only, and the manifest
/// parser rejects `acquireFirmware` inside `postExitReadiness`.
#[uniffi::export]
pub async fn run_post_exit_readiness(
    store: Arc<ConfigStore>,
    plan_handle: String,
    transport: Arc<dyn BleExecutorTransport>,
    observer: Arc<dyn StepObserver>,
    initial_scope: Vec<KeyValue>,
    initial_encodings: Vec<KeyValue>,
    runtime_params: Vec<KeyValue>,
) -> Result<ExecutionOutcome, ExecutorError> {
    let block = resolve_establishment(&store, &plan_handle)?;
    let encodings = initial_encodings
        .into_iter()
        .filter_map(|kv| Encoding::from_token(&kv.value).map(|enc| (kv.key, enc)))
        .collect();

    let mut ctx = ExecCtx {
        transport: &transport,
        observer: &observer,
        scope: initial_scope
            .into_iter()
            .map(|kv| (kv.key, kv.value))
            .collect(),
        encodings,
        runtime_params: runtime_params
            .into_iter()
            .map(|kv| (kv.key, kv.value))
            .collect(),
        subscriptions: BTreeSet::new(),
        steps_run: 0,
        refine: None,
    };
    walk_plan(&mut ctx, block.post_exit_readiness).await?;
    Ok(outcome(ctx))
}

/// Resolve `plan_handle` (`model:selector`) to its establishment block.
/// Declared connections take precedence; a non-connection selector falls back
/// to a direct establishment-mechanism key for reconnect plans.
fn resolve_establishment(
    store: &ConfigStore,
    plan_handle: &str,
) -> Result<EstablishmentBlock, ExecutorError> {
    let unknown = |detail: String| ExecutorError::UnknownPlan { detail };
    let (model, selector) = plan_handle
        .split_once(':')
        .filter(|(m, c)| !m.is_empty() && !c.is_empty() && !c.contains(':'))
        .ok_or_else(|| unknown(format!("bad plan handle {plan_handle:?}")))?;
    let inner = &store.inner;
    let index = inner
        .index
        .as_ref()
        .ok_or_else(|| unknown("store has no manufacturer index".into()))?;
    let body = inner
        .body(model)
        .ok_or_else(|| unknown(format!("unknown model {model}")))?;
    let mechanism = match body.connections.get(selector) {
        Some(connection) => connection
            .establishment
            .clone()
            .ok_or_else(|| unknown(format!("{plan_handle}: connection has no establishment")))?,
        None => selector.to_string(),
    };
    let view = index
        .models
        .iter()
        .find(|m| m.id == model)
        .ok_or_else(|| unknown(format!("model {model} not in index")))?;
    view.ble
        .as_ref()
        .and_then(|ble| ble.establishment(&mechanism))
        .cloned()
        .ok_or_else(|| unknown(format!("{plan_handle}: missing mechanism {mechanism}")))
}

/// Execute the BLE-native control action `action` for `model` (#91) over an
/// already-established link. Same walker, no refinement (actions are not
/// firmware-tiered).
#[uniffi::export]
pub async fn run_ble_action(
    store: Arc<ConfigStore>,
    model: String,
    action: String,
    transport: Arc<dyn BleExecutorTransport>,
    observer: Arc<dyn StepObserver>,
    initial_scope: Vec<KeyValue>,
    runtime_params: Vec<KeyValue>,
) -> Result<ExecutionOutcome, ExecutorError> {
    let unknown = |detail: String| ExecutorError::UnknownPlan { detail };
    let inner = &store.inner;
    let index = inner
        .index
        .as_ref()
        .ok_or_else(|| unknown("store has no manufacturer index".into()))?;
    let block = index
        .models
        .iter()
        .find(|m| m.id == model)
        .and_then(|m| m.ble.as_ref())
        .and_then(|ble| ble.action(&action))
        .ok_or_else(|| unknown(format!("{model}: unknown action {action}")))?;

    let mut ctx = ExecCtx {
        transport: &transport,
        observer: &observer,
        scope: initial_scope
            .into_iter()
            .map(|kv| (kv.key, kv.value))
            .collect(),
        encodings: BTreeMap::new(),
        runtime_params: runtime_params
            .into_iter()
            .map(|kv| (kv.key, kv.value))
            .collect(),
        subscriptions: BTreeSet::new(),
        steps_run: 0,
        refine: None,
    };
    walk_plan(&mut ctx, block.steps.clone()).await?;
    Ok(outcome(ctx))
}

fn outcome(ctx: ExecCtx<'_>) -> ExecutionOutcome {
    ExecutionOutcome {
        scope: ctx
            .scope
            .into_iter()
            .map(|(key, value)| KeyValue { key, value })
            .collect(),
        steps_run: ctx.steps_run,
    }
}

// ---------------------------------------------------------------------------
// Walk state
// ---------------------------------------------------------------------------

struct RefineCtx<'a> {
    store: &'a ConfigStore,
    plan_handle: String,
}

struct ExecCtx<'a> {
    transport: &'a Arc<dyn BleExecutorTransport>,
    observer: &'a Arc<dyn StepObserver>,
    scope: BTreeMap<String, String>,
    /// Encoding each scope key was captured with — `{ captured: … }` writes
    /// re-encode by this instead of guessing from the scope string.
    encodings: BTreeMap<String, Encoding>,
    runtime_params: BTreeMap<String, String>,
    /// Successful CCCD enables in this walk. A retry reuses transport state.
    subscriptions: BTreeSet<(String, bool)>,
    steps_run: u32,
    /// Present for establishment walks; `acquireFirmware` re-resolves the
    /// tail through it (§11.5). `None` for BLE actions.
    refine: Option<RefineCtx<'a>>,
}

/// Step failure: which step (verb + position path) and why.
#[derive(Debug)]
struct StepError {
    step: String,
    kind: ExecutorStepFailureKind,
    message: String,
    context: Vec<KeyValue>,
}

impl StepError {
    fn other(step: &str, message: String) -> Self {
        Self {
            step: step.to_string(),
            kind: ExecutorStepFailureKind::Other,
            message,
            context: Vec::new(),
        }
    }

    fn deadline(step: &str, message: String) -> Self {
        Self {
            step: step.to_string(),
            kind: ExecutorStepFailureKind::DeadlineExceeded,
            message,
            context: Vec::new(),
        }
    }

    fn condition_rejected(step: &str, message: String) -> Self {
        Self {
            step: step.to_string(),
            kind: ExecutorStepFailureKind::ConditionRejected,
            message,
            context: Vec::new(),
        }
    }

    fn operation(step: &str, failure: OperationFailure) -> Self {
        Self {
            step: step.to_string(),
            kind: failure.kind,
            message: failure.message,
            context: Vec::new(),
        }
    }

    fn transport(step: &str, error: TransportError) -> Self {
        Self::operation(step, OperationFailure::transport(error))
    }
}

impl From<StepError> for ExecutorError {
    fn from(e: StepError) -> Self {
        ExecutorError::StepFailed {
            step: e.step,
            kind: e.kind,
            detail: e.message,
            context: e.context,
        }
    }
}

struct OperationFailure {
    kind: ExecutorStepFailureKind,
    message: String,
}

impl OperationFailure {
    fn transport(error: TransportError) -> Self {
        let kind = if matches!(error, TransportError::Timeout { .. }) {
            ExecutorStepFailureKind::DeadlineExceeded
        } else {
            ExecutorStepFailureKind::Other
        };
        Self {
            kind,
            message: error.to_string(),
        }
    }

    fn deadline(what: &str, ms: u32) -> Self {
        Self {
            kind: ExecutorStepFailureKind::DeadlineExceeded,
            message: format!("{what} timed out after {ms}ms"),
        }
    }
}

/// A refined tail bubbling up from `acquireFirmware` (§11.5) — the top-level
/// walk splices it over the steps after the current one.
type RefinedTail = Option<Vec<Step>>;

// ---------------------------------------------------------------------------
// The walk
// ---------------------------------------------------------------------------

/// Top-level walk with §11.5 tail splicing: a step that returns a refined
/// tail replaces everything after itself, and the walk continues into the
/// spliced steps.
async fn walk_plan(ctx: &mut ExecCtx<'_>, mut steps: Vec<Step>) -> Result<(), StepError> {
    let mut i = 0;
    while i < steps.len() {
        let step = steps[i].clone();
        let here = format!("steps[{i}].{}", step.verb_name());
        if let Some(tail) = run_step(ctx, &step, &here, (i + 1) as u32).await? {
            steps.truncate(i + 1);
            steps.extend(tail);
        }
        i += 1;
    }
    Ok(())
}

/// Nested walk (if-branches, `on_each`, acquire delegates). A refined tail
/// from a nested `acquireFirmware` propagates up to the top-level splice.
fn walk_steps<'a>(
    ctx: &'a mut ExecCtx<'_>,
    steps: &'a [Step],
    path: &'a str,
    top_next: u32,
) -> Pin<Box<dyn std::future::Future<Output = Result<RefinedTail, StepError>> + Send + 'a>> {
    Box::pin(async move {
        let mut tail = None;
        for (i, step) in steps.iter().enumerate() {
            let here = format!("{path}[{i}].{}", step.verb_name());
            if let Some(t) = run_step(ctx, step, &here, top_next).await? {
                tail = Some(t);
            }
        }
        Ok(tail)
    })
}

/// Dispatch one step through the retry/tolerance ladder (§11.6), emitting the
/// `Started` + terminal [`StepReport`] pair. `retries` is the count of
/// ADDITIONAL attempts; backoff is a fixed `retry_delay_ms` between attempts;
/// every failure class retries alike. `If` carries its own `tolerant` with
/// predicate-only semantics, so it dispatches with default (single-attempt,
/// strict) options.
fn run_step<'a>(
    ctx: &'a mut ExecCtx<'_>,
    step: &'a Step,
    here: &'a str,
    top_next: u32,
) -> Pin<Box<dyn std::future::Future<Output = Result<RefinedTail, StepError>> + Send + 'a>> {
    Box::pin(async move {
        let opts = step.options();
        let verb = step.verb_name();
        let characteristic = step_characteristic(step);
        let report = |outcome: StepOutcome, error: Option<String>, attempts: u32| StepReport {
            step_path: here.to_string(),
            verb: verb.to_string(),
            characteristic: characteristic.clone(),
            outcome,
            error,
            attempts,
        };
        ctx.observer.on_step(report(StepOutcome::Started, None, 0));

        let tolerant = match step {
            // §11.6: If's tolerant gates predicate fields, not body errors.
            Step::If(_) => false,
            other => other.options().tolerant,
        };

        if let Step::Retry(retry) = step {
            return match run_retry_control(ctx, retry, here, top_next).await {
                Ok((tail, retries_consumed)) => {
                    ctx.steps_run += 1;
                    ctx.observer
                        .on_step(report(StepOutcome::Succeeded, None, retries_consumed));
                    Ok(tail)
                }
                Err((error, retries_consumed)) if tolerant => {
                    ctx.steps_run += 1;
                    ctx.observer.on_step(report(
                        StepOutcome::Tolerated,
                        Some(error.message),
                        retries_consumed,
                    ));
                    Ok(None)
                }
                Err((error, retries_consumed)) => {
                    ctx.observer.on_step(report(
                        StepOutcome::Failed,
                        Some(error.message.clone()),
                        retries_consumed,
                    ));
                    Err(error)
                }
            };
        }

        let mut attempt: u32 = 0;
        loop {
            match run_step_once(ctx, step, here, top_next).await {
                Ok(tail) => {
                    ctx.steps_run += 1;
                    ctx.observer
                        .on_step(report(StepOutcome::Succeeded, None, attempt));
                    return Ok(tail);
                }
                Err(e) if attempt < opts.retries => {
                    attempt += 1;
                    let _ = e;
                    if opts.retry_delay_ms > 0 {
                        let _ = ctx.transport.sleep(opts.retry_delay_ms).await;
                    }
                }
                Err(e) if tolerant => {
                    ctx.steps_run += 1;
                    ctx.observer
                        .on_step(report(StepOutcome::Tolerated, Some(e.message), attempt));
                    return Ok(None);
                }
                Err(e) => {
                    ctx.observer.on_step(report(
                        StepOutcome::Failed,
                        Some(e.message.clone()),
                        attempt,
                    ));
                    return Err(e);
                }
            }
        }
    })
}

async fn run_retry_control(
    ctx: &mut ExecCtx<'_>,
    retry: &camera_config::index::RetryStep,
    here: &str,
    top_next: u32,
) -> Result<(RefinedTail, u32), (StepError, u32)> {
    let mut retries_consumed = 0;
    loop {
        match walk_steps(ctx, &retry.steps, &format!("{here}.steps"), top_next).await {
            Ok(tail) => return Ok((tail, retries_consumed)),
            Err(mut body_error) => {
                let selected = match retry.when_failure {
                    camera_config::index::RetryFailureKind::DeadlineExceeded => {
                        ExecutorStepFailureKind::DeadlineExceeded
                    }
                    camera_config::index::RetryFailureKind::ConditionRejected => {
                        ExecutorStepFailureKind::ConditionRejected
                    }
                    camera_config::index::RetryFailureKind::Other => ExecutorStepFailureKind::Other,
                };
                if body_error.kind != selected {
                    return Err((body_error, retries_consumed));
                }

                if let Err(error) = walk_steps(
                    ctx,
                    &retry.on_failure,
                    &format!("{here}.onFailure"),
                    top_next,
                )
                .await
                {
                    return Err((error, retries_consumed));
                }

                let should_retry = match ctx.scope.get(&retry.retry_when.field) {
                    Some(actual) => {
                        predicate_holds(actual, retry.retry_when.op, &retry.retry_when.value)
                    }
                    None => {
                        return Err((
                            StepError::other(
                                here,
                                format!(
                                    "retryWhen field '{}' unbound in scope",
                                    retry.retry_when.field
                                ),
                            ),
                            retries_consumed,
                        ));
                    }
                };

                let attempts_used = retries_consumed + 1;
                if !should_retry || attempts_used >= retry.max_attempts {
                    body_error.context = retry
                        .failure_context
                        .iter()
                        .filter_map(|key| {
                            ctx.scope.get(key).map(|value| KeyValue {
                                key: key.clone(),
                                value: value.clone(),
                            })
                        })
                        .collect();
                    return Err((body_error, retries_consumed));
                }

                if retry.retry_delay_ms > 0 {
                    if let Err(error) = ctx.transport.sleep(retry.retry_delay_ms).await {
                        return Err((StepError::transport(here, error), retries_consumed));
                    }
                }
                retries_consumed += 1;
            }
        }
    }
}

/// The GATT UUID a verb addresses, for telemetry.
fn step_characteristic(step: &Step) -> Option<String> {
    match step {
        Step::BleRead(s) => Some(s.gatt.clone()),
        Step::BleWrite(s) => Some(s.gatt.clone()),
        Step::BleSubscribe(s) => Some(s.gatt.clone()),
        Step::BleNotify(s) => Some(s.gatt.clone()),
        Step::BleWriteChunk(s) => Some(s.gatt.clone()),
        Step::BleAwaitUntil(s) => Some(match &s.source {
            AwaitSource::Read { gatt } => gatt.clone(),
            AwaitSource::Notify { gatt, .. } => gatt.clone(),
        }),
        Step::AcquireFirmware(s) => match &s.from {
            AcquireSource::BleRead { gatt, .. } => Some(gatt.clone()),
            _ => None,
        },
        _ => None,
    }
}

/// One attempt at one step — the per-verb semantics, ported from the
/// reference walker with real deadlines in place of its determinism bounds.
async fn run_step_once(
    ctx: &mut ExecCtx<'_>,
    step: &Step,
    here: &str,
    top_next: u32,
) -> Result<RefinedTail, StepError> {
    let err = |message: String| StepError::other(here, message);
    let op_err = |failure: OperationFailure| StepError::operation(here, failure);
    match step {
        Step::BleConnect(_) => {
            deadline(ctx.transport, CONNECT_TIMEOUT_MS, "connect", async {
                ctx.transport.connect().await
            })
            .await
            .map_err(op_err)?;
            Ok(None)
        }
        Step::BleAwaitDisconnect(s) => {
            deadline(ctx.transport, s.timeout_ms, "awaitDisconnect", async {
                ctx.transport.await_disconnect().await
            })
            .await
            .map_err(op_err)?;
            Ok(None)
        }
        Step::BleRequestMtu(s) => {
            let negotiated = deadline(ctx.transport, DEFAULT_OP_TIMEOUT_MS, "requestMtu", async {
                ctx.transport.request_mtu(s.mtu).await
            })
            .await
            .map_err(op_err)?;
            if negotiated < s.mtu {
                return Err(err(format!(
                    "negotiated MTU {negotiated} < required {}",
                    s.mtu
                )));
            }
            Ok(None)
        }
        Step::BleDiscoverServices(_) => {
            deadline(
                ctx.transport,
                DEFAULT_OP_TIMEOUT_MS,
                "discoverServices",
                async { ctx.transport.ensure_services_discovered().await },
            )
            .await
            .map_err(op_err)?;
            Ok(None)
        }
        Step::BleRead(s) => {
            let wire = deadline(ctx.transport, DEFAULT_OP_TIMEOUT_MS, "read", async {
                ctx.transport.read(s.gatt.clone()).await
            })
            .await
            .map_err(op_err)?;
            // §11.13 capture pipeline: bytes → transform chain → encoding.
            let bytes = eval::apply_transforms(&wire, &s.transform)
                .ok_or_else(|| err("transform chain failed".into()))?;
            let value = eval::decode_bytes(&bytes, s.encoding)
                .ok_or_else(|| err(format!("decode as {} failed", s.encoding.as_token())))?;
            ctx.scope.insert(s.capture_as.clone(), value);
            ctx.encodings.insert(s.capture_as.clone(), s.encoding);
            Ok(None)
        }
        Step::BleWrite(s) => {
            let bytes = resolve_value(ctx, &s.value).map_err(err)?;
            deadline(ctx.transport, DEFAULT_OP_TIMEOUT_MS, "write", async {
                ctx.transport.write(s.gatt.clone(), bytes).await
            })
            .await
            .map_err(op_err)?;
            Ok(None)
        }
        Step::BleSubscribe(s) => {
            let budget = if s.timeout_ms > 0 {
                s.timeout_ms
            } else {
                DEFAULT_OP_TIMEOUT_MS
            };
            ensure_subscribed(ctx, &s.gatt, s.mode, budget)
                .await
                .map_err(op_err)?;
            Ok(None)
        }
        Step::BleNotify(s) => {
            run_notify(ctx, s, here).await?;
            Ok(None)
        }
        Step::BleAwaitUntil(s) => run_await_until(ctx, s, here, top_next).await,
        Step::BleWriteChunk(s) => {
            run_write_chunk(ctx, s, here).await?;
            Ok(None)
        }
        Step::Acquire(s) => {
            // Run the delegate through the nested walk (a one-element slice)
            // so its OWN tolerant/retry options apply at its level, then alias
            // the slot the delegate explicitly declared (#44).
            let tail = walk_steps(
                ctx,
                std::slice::from_ref(s.from.as_ref()),
                &format!("{here}.from"),
                top_next,
            )
            .await?;
            let target = primary_capture_name(&s.from).ok_or_else(|| {
                err(format!(
                    "acquire delegate `{}` declares no capture_as to bind",
                    s.from.verb_name()
                ))
            })?;
            // A tolerant delegate that failed bound nothing — nothing to
            // alias, and its own tolerance already decided to continue.
            if let Some(v) = ctx.scope.get(target).cloned() {
                if let Some(enc) = ctx.encodings.get(target).copied() {
                    ctx.encodings.insert(s.name.clone(), enc);
                }
                ctx.scope.insert(s.name.clone(), v);
            }
            Ok(tail)
        }
        Step::AcquireFirmware(s) => {
            let firmware = match &s.from {
                AcquireSource::BleRead { gatt, encoding } => {
                    let wire = deadline(ctx.transport, DEFAULT_OP_TIMEOUT_MS, "read", async {
                        ctx.transport.read(gatt.clone()).await
                    })
                    .await
                    .map_err(op_err)?;
                    let value = eval::decode_bytes(&wire, *encoding)
                        .ok_or_else(|| err(format!("decode as {} failed", encoding.as_token())))?;
                    ctx.encodings.insert("firmware".to_string(), *encoding);
                    value
                }
                AcquireSource::BleAdvert {
                    offset,
                    length,
                    encoding,
                } => {
                    let advert = ctx
                        .scope
                        .get("advertisement")
                        .ok_or_else(|| err("no advertisement bytes in scope".into()))?;
                    let bytes = eval::scope_string_to_bytes(
                        advert,
                        ctx.encodings.get("advertisement").copied(),
                    )
                    .ok_or_else(|| err("advertisement bytes undecodable".into()))?;
                    let (at, len) = (*offset as usize, *length as usize);
                    if at.saturating_add(len) > bytes.len() {
                        return Err(err(format!(
                            "advert window {at}+{len} out of range ({} bytes)",
                            bytes.len()
                        )));
                    }
                    let value = eval::decode_bytes(&bytes[at..at + len], *encoding)
                        .ok_or_else(|| err(format!("decode as {} failed", encoding.as_token())))?;
                    ctx.encodings.insert("firmware".to_string(), *encoding);
                    value
                }
                AcquireSource::UserPrompt { .. } => {
                    return Err(err("acquireFirmware userPrompt is unsupported".into()));
                }
            };
            ctx.scope.insert("firmware".to_string(), firmware.clone());

            // §11.5: re-resolve the plan under the acquired firmware and hand
            // the refined tail up for splicing.
            let Some(refine) = &ctx.refine else {
                return Ok(None);
            };
            let scope_kvs: Vec<KeyValue> = ctx
                .scope
                .iter()
                .map(|(k, v)| KeyValue {
                    key: k.clone(),
                    value: v.clone(),
                })
                .collect();
            match refine.store.refine_establishment(
                refine.plan_handle.clone(),
                firmware,
                scope_kvs,
                top_next,
            ) {
                Ok(crate::EstablishmentRefinement::NoChange) => Ok(None),
                Ok(crate::EstablishmentRefinement::ReplaceTail { .. }) => {
                    // The FFI refinement carries FFI Steps; the internal
                    // walker needs ix Steps. Current manifests never branch on
                    // firmware, so a live ReplaceTail here means the manifest
                    // grammar outgrew the executor — fail loud, don't guess.
                    Err(err("refinement returned replaceTail — executor-side tail \
                         conversion is not implemented yet"
                        .into()))
                }
                Err(e) => Err(err(format!("refinement failed: {e}"))),
            }
        }
        Step::If(s) => {
            let holds = match ctx.scope.get(&s.condition.field) {
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
            walk_steps(ctx, branch, &branch_path, top_next).await
        }
        Step::Retry(_) => unreachable!("retry is handled by run_step"),
    }
}

// ---------------------------------------------------------------------------
// Notify + await loops
// ---------------------------------------------------------------------------

async fn ensure_subscribed(
    ctx: &mut ExecCtx<'_>,
    gatt: &str,
    mode: camera_config::index::CccdMode,
    timeout_ms: u32,
) -> Result<(), OperationFailure> {
    let key = (
        gatt.to_string(),
        matches!(mode, camera_config::index::CccdMode::Indicate),
    );
    if ctx.subscriptions.contains(&key) {
        return Ok(());
    }
    deadline(ctx.transport, timeout_ms, "subscribe", async {
        ctx.transport.subscribe(gatt.to_string(), mode.into()).await
    })
    .await?;
    ctx.subscriptions.insert(key);
    Ok(())
}

/// `bleNotify` (§11.8): CCCD-enable, then consume the notification stream
/// until a payload satisfies `until` or the wall-clock budget lapses. A
/// non-matching payload keeps the wait alive (the app dispatcher's semantics;
/// the deterministic walker fails on it instead).
async fn run_notify(ctx: &mut ExecCtx<'_>, s: &BleNotifyStep, here: &str) -> Result<(), StepError> {
    let err = |message: String| StepError::other(here, message);
    ensure_subscribed(ctx, &s.gatt, s.mode, DEFAULT_OP_TIMEOUT_MS)
        .await
        .map_err(|failure| StepError::operation(here, failure))?;

    let mut budget = ctx.transport.sleep(s.timeout_ms).fuse();
    let mut observations: Vec<String> = Vec::new();
    for _ in 0..MAX_LOOP_ITERS {
        let payload = next_or_budget(ctx.transport, &s.gatt, &mut budget)
            .await
            .map_err(|failure| match failure {
                BudgetFailure::Lapsed => StepError::deadline(
                    here,
                    format!(
                        "no accepted notification within {}ms (observed: {})",
                        s.timeout_ms,
                        summarize_observations(&observations)
                    ),
                ),
                BudgetFailure::Clock(error) => StepError::transport(here, error),
            })?
            .map_err(|error| StepError::transport(here, error))?;
        observations.push(eval::hex_lower(&payload));
        let accepted = match &s.until {
            BleNotifyUntil::Any => true,
            BleNotifyUntil::Equals { value, encoding } => {
                let want = eval::yaml_literal_to_bytes(value, *encoding)
                    .ok_or_else(|| err("until.equals value undecodable".into()))?;
                payload == want
            }
            BleNotifyUntil::Matches { pattern } => {
                regex_matches(pattern, &payload).map_err(|e| err(format!("until.matches: {e}")))?
            }
        };
        if accepted {
            apply_value_captures(ctx, &payload, &s.capture_as, &s.capture);
            return Ok(());
        }
    }
    Err(err(format!(
        "notification acceptance loop exceeded {MAX_LOOP_ITERS} iterations"
    )))
}

/// `bleAwaitUntil` (§11.15): observe the source until `until` holds over
/// scope, running `on_each` between unsatisfied iterations, inside one
/// wall-clock budget.
async fn run_await_until(
    ctx: &mut ExecCtx<'_>,
    s: &BleAwaitUntilStep,
    here: &str,
    top_next: u32,
) -> Result<RefinedTail, StepError> {
    let err = |message: String| StepError::other(here, message);
    match &s.source {
        AwaitSource::Notify {
            gatt,
            mode,
            seed_read,
        } => {
            if !s.on_each.is_empty() {
                return Err(err("onEach is unsupported for a notify source".into()));
            }
            ensure_subscribed(ctx, gatt, *mode, DEFAULT_OP_TIMEOUT_MS)
                .await
                .map_err(|failure| StepError::operation(here, failure))?;

            let mut budget = ctx.transport.sleep(s.timeout_ms).fuse();
            let mut observations: Vec<String> = Vec::new();
            let mut seed_pending = *seed_read;
            for _ in 0..MAX_LOOP_ITERS {
                let value = if seed_pending {
                    // One fresh read routed through the same acceptance path,
                    // so an already-satisfied state resolves immediately.
                    seed_pending = false;
                    let read = ctx.transport.read(gatt.clone());
                    futures_util::pin_mut!(read);
                    match select(read, &mut budget).await {
                        Either::Left((res, _)) => {
                            res.map_err(|error| StepError::transport(here, error))?
                        }
                        Either::Right((clock, _)) => match clock {
                            Ok(()) => return Err(await_timeout_err(here, s, &observations)),
                            Err(error) => return Err(StepError::transport(here, error)),
                        },
                    }
                } else {
                    match next_or_budget(ctx.transport, gatt, &mut budget).await {
                        Ok(res) => res.map_err(|error| StepError::transport(here, error))?,
                        Err(BudgetFailure::Lapsed) => {
                            return Err(await_timeout_err(here, s, &observations));
                        }
                        Err(BudgetFailure::Clock(error)) => {
                            return Err(StepError::transport(here, error));
                        }
                    }
                };
                observations.push(eval::hex_lower(&value));
                apply_value_captures(ctx, &value, &s.capture_as, &s.capture);
                let satisfied = match ctx.scope.get(&s.until.field) {
                    Some(actual) => predicate_holds(actual, s.until.op, &s.until.value),
                    None => false,
                };
                if satisfied {
                    return Ok(None);
                }
                if fail_when_holds(&ctx.scope, s) {
                    return Err(condition_rejected_err(here, s));
                }
            }
            Err(err(format!(
                "await loop exceeded {MAX_LOOP_ITERS} iterations"
            )))
        }
        AwaitSource::Read { gatt } => {
            let interval = if s.interval_ms > 0 {
                s.interval_ms
            } else {
                DEFAULT_POLL_INTERVAL_MS
            };
            let mut budget = ctx.transport.sleep(s.timeout_ms).fuse();
            let mut tail = None;
            for _ in 0..MAX_LOOP_ITERS {
                let read = ctx.transport.read(gatt.clone());
                futures_util::pin_mut!(read);
                let value = match select(read, &mut budget).await {
                    Either::Left((res, _)) => {
                        res.map_err(|error| StepError::transport(here, error))?
                    }
                    Either::Right((clock, _)) => match clock {
                        Ok(()) => {
                            return Err(StepError::deadline(
                                here,
                                format!(
                                    "`until` ({} {} {}) not satisfied within {}ms",
                                    s.until.field,
                                    s.until.op.as_token(),
                                    s.until.value,
                                    s.timeout_ms
                                ),
                            ));
                        }
                        Err(error) => return Err(StepError::transport(here, error)),
                    },
                };
                apply_value_captures(ctx, &value, &s.capture_as, &s.capture);
                let satisfied = match ctx.scope.get(&s.until.field) {
                    Some(actual) => predicate_holds(actual, s.until.op, &s.until.value),
                    None => false,
                };
                if satisfied {
                    return Ok(tail);
                }
                if fail_when_holds(&ctx.scope, s) {
                    return Err(condition_rejected_err(here, s));
                }
                // Not yet: act, then observe again after the poll interval.
                if let Some(t) =
                    walk_steps(ctx, &s.on_each, &format!("{here}.onEach"), top_next).await?
                {
                    tail = Some(t);
                }
                let pause = ctx.transport.sleep(interval);
                futures_util::pin_mut!(pause);
                match select(pause, &mut budget).await {
                    Either::Left((Ok(()), _)) => {}
                    Either::Left((Err(error), _)) => {
                        return Err(StepError::transport(here, error));
                    }
                    Either::Right((Ok(()), _)) => {
                        return Err(StepError::deadline(
                            here,
                            format!(
                                "`until` ({} {} {}) not satisfied within {}ms",
                                s.until.field,
                                s.until.op.as_token(),
                                s.until.value,
                                s.timeout_ms
                            ),
                        ));
                    }
                    Either::Right((Err(error), _)) => {
                        return Err(StepError::transport(here, error));
                    }
                }
            }
            Err(err(format!(
                "await loop exceeded {MAX_LOOP_ITERS} iterations"
            )))
        }
    }
}

fn fail_when_holds(scope: &BTreeMap<String, String>, s: &BleAwaitUntilStep) -> bool {
    s.fail_when.as_ref().is_some_and(|predicate| {
        scope
            .get(&predicate.field)
            .is_some_and(|actual| predicate_holds(actual, predicate.op, &predicate.value))
    })
}

fn condition_rejected_err(here: &str, s: &BleAwaitUntilStep) -> StepError {
    let predicate = s
        .fail_when
        .as_ref()
        .expect("called only after failWhen matched");
    StepError::condition_rejected(
        here,
        format!(
            "`failWhen` matched ({} {} {})",
            predicate.field,
            predicate.op.as_token(),
            predicate.value
        ),
    )
}

fn await_timeout_err(here: &str, s: &BleAwaitUntilStep, observations: &[String]) -> StepError {
    StepError::deadline(
        here,
        format!(
            "awaited notification did not satisfy `until` ({} {} {}) within {}ms (observed: {})",
            s.until.field,
            s.until.op.as_token(),
            s.until.value,
            s.timeout_ms,
            summarize_observations(observations)
        ),
    )
}

/// Why the shared wall-clock budget completed before the I/O resolved.
enum BudgetFailure {
    Lapsed,
    Clock(TransportError),
}

/// Race the next notification against the (shared, fused) budget sleep.
async fn next_or_budget<B>(
    transport: &Arc<dyn BleExecutorTransport>,
    gatt: &str,
    budget: &mut B,
) -> Result<Result<Vec<u8>, TransportError>, BudgetFailure>
where
    B: std::future::Future<Output = Result<(), TransportError>> + Unpin,
{
    let next = transport.next_notification(gatt.to_string());
    futures_util::pin_mut!(next);
    match select(next, budget).await {
        Either::Left((res, _)) => Ok(res),
        Either::Right((Ok(()), _)) => Err(BudgetFailure::Lapsed),
        Either::Right((Err(error), _)) => Err(BudgetFailure::Clock(error)),
    }
}

// ---------------------------------------------------------------------------
// Chunked write
// ---------------------------------------------------------------------------

/// Reference-walker guard on a `bleWriteChunk` upload (#112): a blob needing
/// more windows than this fails rather than letting a corrupt size spin.
const MAX_CHUNK_WINDOWS: usize = 4096;

/// `bleWriteChunk` (#112): frame + write ONE window of the host blob,
/// selected by the captured chunk index. Slice math and frame assembly live
/// here; the manifest declares only policy.
async fn run_write_chunk(
    ctx: &mut ExecCtx<'_>,
    s: &BleWriteChunkStep,
    here: &str,
) -> Result<(), StepError> {
    let err = |message: String| StepError::other(here, message);

    // The host supplies the whole blob once as a bytes-raw hex param (#114);
    // a captured scope slot is accepted as the fallback source.
    let raw = ctx
        .runtime_params
        .get(&s.source)
        .or_else(|| ctx.scope.get(&s.source))
        .ok_or_else(|| err(format!("source slot '{}' unbound", s.source)))?;
    let blob = eval::scope_string_to_bytes(raw, Some(Encoding::BytesRaw))
        .ok_or_else(|| err(format!("source '{}' undecodable", s.source)))?;

    let size = s.size.max(1) as usize;
    let total = blob.len();
    let full = if total == 0 { 0 } else { (total - 1) / size };
    if full + 1 > MAX_CHUNK_WINDOWS {
        return Err(err(format!(
            "blob of {total} bytes needs {} windows, exceeds cap {MAX_CHUNK_WINDOWS}",
            full + 1
        )));
    }

    let captured = ctx
        .scope
        .get(&s.index)
        .or_else(|| ctx.runtime_params.get(&s.index))
        .ok_or_else(|| err(format!("index slot '{}' unbound in scope", s.index)))?;
    let idx: u64 = captured.parse().map_err(|_| {
        err(format!(
            "index '{}' = {captured:?} is not an integer",
            s.index
        ))
    })?;

    let (offset, len) = if idx == s.sentinel_index as u64 {
        (full * size, total - full * size)
    } else if (idx as usize) < full {
        (idx as usize * size, size)
    } else {
        return Err(err(format!(
            "chunk index {idx} out of range (full windows 0..{full}, sentinel {})",
            s.sentinel_index
        )));
    };

    let mut frame = Vec::new();
    for f in &s.frame {
        let value = match f.field {
            camera_config::index::ChunkField::Index => idx,
            camera_config::index::ChunkField::Length => len as u64,
        };
        let encoded = eval::encode_uint(value, f.encoding).ok_or_else(|| {
            err(format!(
                "frame field {:?} needs an integer encoding (got {})",
                f.field,
                f.encoding.as_token()
            ))
        })?;
        frame.extend_from_slice(&encoded);
    }
    frame.extend_from_slice(&blob[offset..offset + len]);

    deadline(ctx.transport, DEFAULT_OP_TIMEOUT_MS, "write", async {
        ctx.transport.write(s.gatt.clone(), frame).await
    })
    .await
    .map_err(|failure| StepError::operation(here, failure))?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Shared helpers (ported from the reference walker)
// ---------------------------------------------------------------------------

/// Race `io` against the host clock; dropping the losing future cancels the
/// foreign task behind it.
async fn deadline<T>(
    transport: &Arc<dyn BleExecutorTransport>,
    ms: u32,
    what: &str,
    io: impl std::future::Future<Output = Result<T, TransportError>>,
) -> Result<T, OperationFailure> {
    futures_util::pin_mut!(io);
    let clock = transport.sleep(ms);
    futures_util::pin_mut!(clock);
    match select(io, clock).await {
        Either::Left((res, _)) => res.map_err(OperationFailure::transport),
        Either::Right((Ok(()), _)) => Err(OperationFailure::deadline(what, ms)),
        Either::Right((Err(error), _)) => Err(OperationFailure::transport(error)),
    }
}

/// The scope slot an `acquire` delegate binds its result to — the delegate's
/// own explicit `capture_as` (#44).
fn primary_capture_name(step: &Step) -> Option<&str> {
    match step {
        Step::BleRead(s) => Some(&s.capture_as),
        Step::BleNotify(s) => s.capture_as.as_deref(),
        Step::BleAwaitUntil(s) => s.capture_as.as_deref(),
        _ => None,
    }
}

/// Bind a value (read result / notification payload) into scope: the whole
/// value under `capture_as` (hex), then each field capture through the §11.13
/// pipeline (window → transform → encoding). Fail-soft: a capture whose
/// window/transform/decode fails is skipped, never an error.
fn apply_value_captures(
    ctx: &mut ExecCtx<'_>,
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

fn resolve_value(ctx: &ExecCtx<'_>, value: &StepValue) -> Result<Vec<u8>, String> {
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

/// Regex acceptance over the UTF-8 decoding of a payload; a payload that
/// isn't UTF-8 never matches. The pattern comes from manifest data.
fn regex_matches(pattern: &str, payload: &[u8]) -> Result<bool, String> {
    let re = regex_lite::Regex::new(pattern).map_err(|e| e.to_string())?;
    Ok(std::str::from_utf8(payload)
        .map(|s| re.is_match(s))
        .unwrap_or(false))
}

/// Distinct observed payloads with counts, first-seen order: `"0140×2,0100×1"`
/// or `"none"` — the diagnostic tail of an await/notify timeout.
fn summarize_observations(observations: &[String]) -> String {
    if observations.is_empty() {
        return "none".to_string();
    }
    let mut order: Vec<&String> = Vec::new();
    let mut counts: BTreeMap<&String, usize> = BTreeMap::new();
    for o in observations {
        if !counts.contains_key(o) {
            order.push(o);
        }
        *counts.entry(o).or_insert(0) += 1;
    }
    order
        .iter()
        .map(|o| format!("{o}×{}", counts[*o]))
        .collect::<Vec<_>>()
        .join(",")
}

// ---------------------------------------------------------------------------
// Timing-layer tests: the semantics the deterministic reference walker can't
// express — retry ladders, wall-clock deadlines, cancellation propagation —
// exercised over a scripted transport with an instant (or frozen) clock.
// Wire-order parity with the reference walker lives in
// `tests/executor_seam.rs` against the real manifest data.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use camera_config::index::{
        AwaitSource, BleAwaitDisconnectStep, BleAwaitUntilStep, BleReadStep, BleRequestMtuStep,
        BleWriteStep, CccdMode, IfStep, NotifyCapture, Predicate, RetryFailureKind, RetryStep,
        StepOptions,
    };
    use std::collections::VecDeque;
    use std::future::Future;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Mutex;
    use std::task::{Context, Poll};

    enum Io {
        Value(Vec<u8>),
        Fail(&'static str),
        Timeout(&'static str),
        /// Never resolves; sets the shared drop flag when the executor drops
        /// the in-flight future (deadline lost the race / walk cancelled).
        Stall,
    }

    struct SetOnDrop(Arc<AtomicBool>);
    impl Drop for SetOnDrop {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    #[derive(Default)]
    struct MockTransport {
        reads: Mutex<VecDeque<Io>>,
        notifications: Mutex<VecDeque<Io>>,
        /// Empty queue = a peer that never disconnects (the call pends).
        disconnects: Mutex<VecDeque<Io>>,
        /// `true` = the clock fires instantly (deadlines lapse as soon as the
        /// raced I/O pends); `false` = the clock is frozen (deadlines never
        /// lapse). Both are the degenerate ends a real clock interpolates.
        sleeps_fire: bool,
        sleep_log: Arc<Mutex<Vec<u32>>>,
        subscribe_log: Arc<Mutex<Vec<String>>>,
        dropped_inflight: Arc<AtomicBool>,
    }

    impl MockTransport {
        async fn play(&self, io: Option<Io>) -> Result<Vec<u8>, TransportError> {
            match io {
                Some(Io::Value(v)) => Ok(v),
                Some(Io::Fail(m)) => Err(TransportError::Failed { detail: m.into() }),
                Some(Io::Timeout(m)) => Err(TransportError::Timeout { detail: m.into() }),
                Some(Io::Stall) | None => {
                    let _guard = SetOnDrop(self.dropped_inflight.clone());
                    std::future::pending().await
                }
            }
        }
    }

    #[async_trait::async_trait]
    impl BleExecutorTransport for MockTransport {
        async fn connect(&self) -> Result<(), TransportError> {
            Ok(())
        }
        async fn await_disconnect(&self) -> Result<(), TransportError> {
            let io = self.disconnects.lock().unwrap().pop_front();
            self.play(io).await.map(|_| ())
        }
        async fn request_mtu(&self, _mtu: u16) -> Result<u16, TransportError> {
            Ok(158)
        }
        async fn ensure_services_discovered(&self) -> Result<(), TransportError> {
            Ok(())
        }
        async fn read(&self, _characteristic: String) -> Result<Vec<u8>, TransportError> {
            let io = self.reads.lock().unwrap().pop_front();
            self.play(io).await
        }
        async fn write(
            &self,
            _characteristic: String,
            _value: Vec<u8>,
        ) -> Result<(), TransportError> {
            Ok(())
        }
        async fn subscribe(
            &self,
            characteristic: String,
            _mode: crate::CccdMode,
        ) -> Result<(), TransportError> {
            self.subscribe_log.lock().unwrap().push(characteristic);
            Ok(())
        }
        async fn next_notification(
            &self,
            _characteristic: String,
        ) -> Result<Vec<u8>, TransportError> {
            let io = self.notifications.lock().unwrap().pop_front();
            self.play(io).await
        }
        async fn sleep(&self, ms: u32) -> Result<(), TransportError> {
            self.sleep_log.lock().unwrap().push(ms);
            if self.sleeps_fire {
                Ok(())
            } else {
                std::future::pending().await
            }
        }
    }

    #[derive(Default)]
    struct Recorder(Mutex<Vec<StepReport>>);
    impl StepObserver for Recorder {
        fn on_step(&self, report: StepReport) {
            self.0.lock().unwrap().push(report);
        }
    }

    fn harness(
        transport: MockTransport,
    ) -> (
        Arc<dyn BleExecutorTransport>,
        Arc<Recorder>,
        Arc<dyn StepObserver>,
    ) {
        let transport: Arc<dyn BleExecutorTransport> = Arc::new(transport);
        let recorder = Arc::new(Recorder::default());
        let observer: Arc<dyn StepObserver> = recorder.clone();
        (transport, recorder, observer)
    }

    fn ctx<'a>(
        transport: &'a Arc<dyn BleExecutorTransport>,
        observer: &'a Arc<dyn StepObserver>,
    ) -> ExecCtx<'a> {
        ExecCtx {
            transport,
            observer,
            scope: BTreeMap::new(),
            encodings: BTreeMap::new(),
            runtime_params: BTreeMap::new(),
            subscriptions: BTreeSet::new(),
            steps_run: 0,
            refine: None,
        }
    }

    fn read_step(capture_as: &str, opts: StepOptions) -> Step {
        Step::BleRead(BleReadStep {
            gatt: "AAAA".into(),
            encoding: Encoding::Utf8,
            capture_as: capture_as.into(),
            transform: vec![],
            opts,
        })
    }

    fn rejection_await() -> Step {
        Step::BleAwaitUntil(BleAwaitUntilStep {
            source: AwaitSource::Notify {
                gatt: "APSTATE".into(),
                mode: CccdMode::Notify,
                seed_read: true,
            },
            capture: vec![NotifyCapture {
                at: 0,
                length: Some(2),
                transform: vec![],
                encoding: Encoding::U16Le,
                name: "apState".into(),
            }],
            capture_as: None,
            until: Predicate {
                field: "apState".into(),
                op: PredicateOp::Eq,
                value: "32769".into(),
            },
            fail_when: Some(Predicate {
                field: "apState".into(),
                op: PredicateOp::Eq,
                value: "32768".into(),
            }),
            on_each: vec![],
            timeout_ms: 20_000,
            interval_ms: 0,
            opts: StepOptions::default(),
        })
    }

    fn condition_retry(steps: Vec<Step>, on_failure: Vec<Step>) -> Step {
        Step::Retry(RetryStep {
            steps,
            when_failure: RetryFailureKind::ConditionRejected,
            on_failure,
            retry_when: Predicate {
                field: "stateErrorDetails".into(),
                op: PredicateOp::Eq,
                value: "2".into(),
            },
            max_attempts: 2,
            retry_delay_ms: 200,
            failure_context: vec!["apState".into(), "stateErrorDetails".into()],
        })
    }

    fn block_on<T>(fut: impl Future<Output = T>) -> T {
        futures::executor::block_on(fut)
    }

    #[test]
    fn retry_ladder_retries_with_backoff_then_succeeds() {
        let sleep_log = Arc::new(Mutex::new(Vec::new()));
        let (transport, recorder, observer) = harness(MockTransport {
            reads: Mutex::new(VecDeque::from([
                Io::Fail("gatt error"),
                Io::Fail("gatt error"),
                Io::Value(b"ok".to_vec()),
            ])),
            sleeps_fire: true,
            sleep_log: sleep_log.clone(),
            ..Default::default()
        });
        let mut ctx = ctx(&transport, &observer);
        let steps = vec![read_step(
            "v",
            StepOptions {
                tolerant: false,
                retries: 2,
                retry_delay_ms: 7,
            },
        )];
        block_on(walk_plan(&mut ctx, steps)).expect("third attempt succeeds");
        assert_eq!(ctx.scope.get("v").map(String::as_str), Some("ok"));

        let reports = recorder.0.lock().unwrap();
        assert_eq!(reports.len(), 2, "Started + one terminal");
        assert_eq!(reports[1].outcome, StepOutcome::Succeeded);
        assert_eq!(reports[1].attempts, 2, "two retries consumed");
        // Fixed backoff between attempts, from the host clock.
        let backoffs = sleep_log
            .lock()
            .unwrap()
            .iter()
            .filter(|m| **m == 7)
            .count();
        assert_eq!(backoffs, 2);
    }

    #[test]
    fn stalled_transport_hits_the_deadline_and_cancels_the_inflight_call() {
        let flag = Arc::new(AtomicBool::new(false));
        let (transport, recorder, observer) = harness(MockTransport {
            reads: Mutex::new(VecDeque::from([Io::Stall])),
            sleeps_fire: true,
            dropped_inflight: flag.clone(),
            ..Default::default()
        });
        let mut ctx = ctx(&transport, &observer);
        let steps = vec![read_step("v", StepOptions::default())];
        let err = block_on(walk_plan(&mut ctx, steps)).expect_err("deadline fires");
        assert!(
            err.message.contains("timed out after 10000ms"),
            "got: {}",
            err.message
        );
        assert_eq!(err.kind, ExecutorStepFailureKind::DeadlineExceeded);
        assert!(
            flag.load(Ordering::SeqCst),
            "the in-flight read future must be dropped (foreign cancellation)"
        );
        let reports = recorder.0.lock().unwrap();
        assert_eq!(reports[1].outcome, StepOutcome::Failed);
    }

    #[test]
    fn await_disconnect_succeeds_when_the_peer_drops_the_link() {
        let (transport, recorder, observer) = harness(MockTransport {
            disconnects: Mutex::new(VecDeque::from([Io::Value(vec![])])),
            sleeps_fire: true,
            ..Default::default()
        });
        let mut ctx = ctx(&transport, &observer);
        let steps = vec![Step::BleAwaitDisconnect(BleAwaitDisconnectStep {
            timeout_ms: 60_000,
            opts: StepOptions::default(),
        })];
        block_on(walk_plan(&mut ctx, steps)).expect("disconnect observed");
        assert_eq!(
            recorder.0.lock().unwrap()[1].outcome,
            StepOutcome::Succeeded
        );
    }

    #[test]
    fn await_disconnect_expiry_is_a_step_failure() {
        // Empty disconnect queue = a peer that never drops the link; the
        // step's own manifest timeoutMs is the deadline, not the backstop.
        let (transport, recorder, observer) = harness(MockTransport {
            sleeps_fire: true,
            ..Default::default()
        });
        let mut ctx = ctx(&transport, &observer);
        let steps = vec![Step::BleAwaitDisconnect(BleAwaitDisconnectStep {
            timeout_ms: 60_000,
            opts: StepOptions::default(),
        })];
        let err = block_on(walk_plan(&mut ctx, steps)).expect_err("deadline fires");
        assert!(
            err.message
                .contains("awaitDisconnect timed out after 60000ms"),
            "got: {}",
            err.message
        );
        assert_eq!(err.kind, ExecutorStepFailureKind::DeadlineExceeded);
        assert_eq!(recorder.0.lock().unwrap()[1].outcome, StepOutcome::Failed);
    }

    #[test]
    fn tolerant_step_failure_is_swallowed_and_reported() {
        let (transport, recorder, observer) = harness(MockTransport {
            reads: Mutex::new(VecDeque::from([Io::Fail("not exposed")])),
            sleeps_fire: true,
            ..Default::default()
        });
        let mut ctx = ctx(&transport, &observer);
        let steps = vec![
            read_step(
                "serial",
                StepOptions {
                    tolerant: true,
                    retries: 0,
                    retry_delay_ms: 0,
                },
            ),
            Step::BleWrite(BleWriteStep {
                gatt: "BBBB".into(),
                value: StepValue::Literal {
                    literal: serde_yaml::Value::String("01".into()),
                },
                opts: StepOptions::default(),
            }),
        ];
        block_on(walk_plan(&mut ctx, steps)).expect("tolerant failure continues the walk");
        assert!(!ctx.scope.contains_key("serial"));
        assert_eq!(ctx.steps_run, 2);

        let reports = recorder.0.lock().unwrap();
        let outcomes: Vec<StepOutcome> = reports.iter().map(|r| r.outcome).collect();
        assert_eq!(
            outcomes,
            vec![
                StepOutcome::Started,
                StepOutcome::Tolerated,
                StepOutcome::Started,
                StepOutcome::Succeeded,
            ]
        );
        assert!(reports[1].error.as_deref().unwrap().contains("not exposed"));
    }

    #[test]
    fn notify_keeps_waiting_past_unaccepted_payloads() {
        let (transport, _recorder, observer) = harness(MockTransport {
            notifications: Mutex::new(VecDeque::from([
                Io::Value(vec![0x02, 0x80]),
                Io::Value(vec![0x01, 0x80]),
            ])),
            // Frozen clock: the budget never lapses; acceptance alone decides.
            sleeps_fire: false,
            ..Default::default()
        });
        let mut ctx = ctx(&transport, &observer);
        let steps = vec![Step::BleNotify(BleNotifyStep {
            gatt: "CCCC".into(),
            until: BleNotifyUntil::Equals {
                value: serde_yaml::Value::String("0180".into()),
                encoding: Some(Encoding::Bytes),
            },
            capture_as: Some("payload".into()),
            capture: vec![],
            mode: CccdMode::Notify,
            timeout_ms: 5000,
            opts: StepOptions::default(),
        })];
        block_on(walk_plan(&mut ctx, steps)).expect("second payload is accepted");
        assert_eq!(ctx.scope.get("payload").map(String::as_str), Some("0180"));
    }

    #[test]
    fn notify_budget_lapse_reports_the_observed_payloads() {
        let (transport, _recorder, observer) = harness(MockTransport {
            notifications: Mutex::new(VecDeque::from([Io::Value(vec![0x02, 0x80]), Io::Stall])),
            sleeps_fire: true,
            ..Default::default()
        });
        let mut ctx = ctx(&transport, &observer);
        let steps = vec![Step::BleNotify(BleNotifyStep {
            gatt: "CCCC".into(),
            until: BleNotifyUntil::Equals {
                value: serde_yaml::Value::String("0180".into()),
                encoding: Some(Encoding::Bytes),
            },
            capture_as: None,
            capture: vec![],
            mode: CccdMode::Notify,
            timeout_ms: 5000,
            opts: StepOptions::default(),
        })];
        let err = block_on(walk_plan(&mut ctx, steps)).expect_err("budget lapses");
        assert!(
            err.message.contains("within 5000ms") && err.message.contains("0280×1"),
            "got: {}",
            err.message
        );
        assert_eq!(err.kind, ExecutorStepFailureKind::DeadlineExceeded);
    }

    #[test]
    fn transport_timeout_retains_deadline_kind() {
        let (transport, _recorder, observer) = harness(MockTransport {
            reads: Mutex::new(VecDeque::from([Io::Timeout("platform read timeout")])),
            sleeps_fire: false,
            ..Default::default()
        });
        let mut ctx = ctx(&transport, &observer);
        let err = block_on(walk_plan(
            &mut ctx,
            vec![read_step("v", StepOptions::default())],
        ))
        .expect_err("transport timeout is fatal");
        assert_eq!(err.kind, ExecutorStepFailureKind::DeadlineExceeded);
        assert!(err.message.contains("platform read timeout"));
    }

    #[test]
    fn cancelling_the_walk_drops_the_inflight_transport_call() {
        let flag = Arc::new(AtomicBool::new(false));
        let (transport, _recorder, observer) = harness(MockTransport {
            reads: Mutex::new(VecDeque::from([Io::Stall])),
            // Frozen clock: nothing resolves; the walk parks on the read.
            sleeps_fire: false,
            dropped_inflight: flag.clone(),
            ..Default::default()
        });
        let mut ctx = ctx(&transport, &observer);
        let steps = vec![read_step("v", StepOptions::default())];
        {
            let mut fut = Box::pin(walk_plan(&mut ctx, steps));
            let waker = futures::task::noop_waker();
            let mut poll_ctx = Context::from_waker(&waker);
            assert!(matches!(fut.as_mut().poll(&mut poll_ctx), Poll::Pending));
            assert!(!flag.load(Ordering::SeqCst));
            // Dropping the walk future — what `rust_future_cancel` does when
            // the foreign task/coroutine is cancelled.
        }
        assert!(
            flag.load(Ordering::SeqCst),
            "cancelling the walk must drop the in-flight transport call"
        );
    }

    #[test]
    fn mtu_below_requirement_fails_the_checkpoint() {
        let (transport, _recorder, observer) = harness(MockTransport {
            sleeps_fire: true,
            ..Default::default()
        });
        let mut ctx = ctx(&transport, &observer);
        let steps = vec![Step::BleRequestMtu(BleRequestMtuStep {
            mtu: 517,
            opts: StepOptions::default(),
        })];
        let err = block_on(walk_plan(&mut ctx, steps)).expect_err("negotiated 158 < 517");
        assert_eq!(err.kind, ExecutorStepFailureKind::Other);
        assert!(err.message.contains("negotiated MTU 158"));
    }

    #[test]
    fn if_unbound_predicate_field_is_strict_unless_tolerant() {
        let (transport, _recorder, observer) = harness(MockTransport {
            sleeps_fire: true,
            ..Default::default()
        });
        let step = |tolerant| {
            Step::If(IfStep {
                condition: Predicate {
                    field: "style".into(),
                    op: PredicateOp::Eq,
                    value: "red".into(),
                },
                then: vec![],
                else_branch: vec![],
                tolerant,
            })
        };
        let mut strict_ctx = ctx(&transport, &observer);
        let err = block_on(walk_plan(&mut strict_ctx, vec![step(false)]))
            .expect_err("strict if on unbound field");
        assert!(err.message.contains("unbound in scope"));

        let mut tolerant_ctx = ctx(&transport, &observer);
        block_on(walk_plan(&mut tolerant_ctx, vec![step(true)]))
            .expect("tolerant if treats unbound as false");
    }

    #[test]
    fn condition_retry_sleeps_once_reuses_subscription_and_reports_retry() {
        let transport = MockTransport {
            reads: Mutex::new(VecDeque::from([
                Io::Value(vec![0x00, 0x80]),
                Io::Value(vec![0x02, 0x00]),
                Io::Value(vec![0x01, 0x80]),
            ])),
            sleeps_fire: true,
            ..Default::default()
        };
        let sleep_log = transport.sleep_log.clone();
        let subscribe_log = transport.subscribe_log.clone();
        let (transport, recorder, observer) = harness(transport);
        let mut ctx = ctx(&transport, &observer);
        let retry = condition_retry(
            vec![rejection_await()],
            vec![Step::BleRead(BleReadStep {
                gatt: "DETAILS".into(),
                encoding: Encoding::U16Le,
                capture_as: "stateErrorDetails".into(),
                transform: vec![],
                opts: StepOptions::default(),
            })],
        );

        block_on(walk_plan(&mut ctx, vec![retry])).expect("second seed succeeds");
        assert_eq!(subscribe_log.lock().unwrap().as_slice(), ["APSTATE"]);
        assert_eq!(
            sleep_log
                .lock()
                .unwrap()
                .iter()
                .filter(|ms| **ms == 200)
                .count(),
            1,
        );
        let reports = recorder.0.lock().unwrap();
        let outer = reports
            .iter()
            .rev()
            .find(|report| report.verb == "retry")
            .expect("outer retry report");
        assert_eq!(outer.outcome, StepOutcome::Succeeded);
        assert_eq!(outer.attempts, 1, "one retry was consumed");
    }

    #[test]
    fn condition_retry_does_not_select_transport_or_timeout_failures() {
        for (io, expected_kind) in [
            (Io::Fail("read failed"), ExecutorStepFailureKind::Other),
            (
                Io::Timeout("read timed out"),
                ExecutorStepFailureKind::DeadlineExceeded,
            ),
        ] {
            let transport = MockTransport {
                reads: Mutex::new(VecDeque::from([io])),
                sleeps_fire: false,
                ..Default::default()
            };
            let sleep_log = transport.sleep_log.clone();
            let (transport, _recorder, observer) = harness(transport);
            let mut ctx = ctx(&transport, &observer);
            let error = block_on(walk_plan(
                &mut ctx,
                vec![condition_retry(
                    vec![read_step("value", StepOptions::default())],
                    vec![read_step("diagnostic", StepOptions::default())],
                )],
            ))
            .expect_err("unselected failure escapes");
            assert_eq!(error.kind, expected_kind);
            assert!(!ctx.scope.contains_key("diagnostic"));
            assert_eq!(
                sleep_log
                    .lock()
                    .unwrap()
                    .iter()
                    .filter(|ms| **ms == 200)
                    .count(),
                0,
            );
        }
    }

    #[test]
    fn condition_retry_does_not_mask_diagnostic_read_failure() {
        let transport = MockTransport {
            reads: Mutex::new(VecDeque::from([
                Io::Value(vec![0x00, 0x80]),
                Io::Fail("details unavailable"),
            ])),
            sleeps_fire: false,
            ..Default::default()
        };
        let sleep_log = transport.sleep_log.clone();
        let (transport, _recorder, observer) = harness(transport);
        let mut ctx = ctx(&transport, &observer);
        let error = block_on(walk_plan(
            &mut ctx,
            vec![condition_retry(
                vec![rejection_await()],
                vec![read_step("stateErrorDetails", StepOptions::default())],
            )],
        ))
        .expect_err("diagnostic failure escapes");
        assert_eq!(error.kind, ExecutorStepFailureKind::Other);
        assert!(error.message.contains("details unavailable"));
        assert_eq!(
            sleep_log
                .lock()
                .unwrap()
                .iter()
                .filter(|ms| **ms == 200)
                .count(),
            0,
        );
    }
}
