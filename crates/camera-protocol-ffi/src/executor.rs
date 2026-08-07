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
    BleWriteChunkStep, Encoding, EstablishmentBlock, NotifyCapture, Predicate, PredicateOp, Step,
    StepValue, UsbInterfaceTriple,
};
use camera_config::{
    ConnectionActivityBinding as ConfigActivityBinding,
    ConnectionActivityDescriptor as ConfigActivityDescriptor,
    ConnectionActivitySequence as ConfigActivitySequence,
};
use futures_util::future::{select, Either, FutureExt};
use protocol_primitives::{NikonConnectionConfiguration, NikonLssClient, NikonLssSession};

use crate::usb_executor::{UsbExecutorTransport, UsbTransportError};
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
/// ordinary step failure to the BLE executor — retried per `StepOptions`, then
/// tolerated or fatal without error-class discrimination (parity with the
/// shipping dispatcher). The PCSS executor separately uses the connection and
/// timeout variants to select its manifest-authorized endpoint recovery; its
/// foreign trait documents the required socket-error mapping.
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
    /// The bound peripheral's platform name (§11.4b): `CBPeripheral.name` on
    /// stacks that filter the GAP service from discovery (CoreBluetooth),
    /// a GATT 0x2A00 read elsewhere. Host-side, never dispatched by the
    /// executor as a characteristic read. Return the UTF-8 name with any NUL
    /// terminator removed; report an unavailable name as a transport error,
    /// never an empty string.
    async fn peripheral_name(&self) -> Result<String, TransportError>;
    async fn write(&self, characteristic: String, value: Vec<u8>) -> Result<(), TransportError>;
    /// Atomically fence the already-buffered prefix for
    /// `notification_characteristic` immediately before issuing the write.
    /// Notifications caused by this write must remain buffered and visible to
    /// [`Self::next_notification`].
    async fn write_with_notification_fence(
        &self,
        characteristic: String,
        value: Vec<u8>,
        notification_characteristic: String,
    ) -> Result<(), TransportError>;
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
    /// The walk continued after a declared tolerant failure or a recoverable
    /// decode condition. `error` carries the diagnostic detail.
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
    /// PTP operation/property correlation; unset for BLE reports.
    pub operation: Option<u16>,
    pub property: Option<u16>,
    pub response_code: Option<u16>,
    pub transaction_id: Option<u32>,
    /// The step's declared response tolerance.
    pub tolerant: bool,
    pub outcome: StepOutcome,
    /// Failure or recoverable diagnostic detail on `Tolerated`/`Failed`.
    pub error: Option<String>,
    /// Retries consumed so far (0 on first-try success).
    pub attempts: u32,
    /// Semantic activity correlation for this raw report, when the top-level
    /// step belongs to an executor span.
    pub activity_id: Option<String>,
    pub activity_version: Option<u32>,
}

/// Completes the raw step-report pair if a parent deadline drops a nested
/// `run_step` future before it can emit its normal terminal outcome.
struct StepTerminalGuard {
    observer: Arc<dyn StepObserver>,
    cancelled: Option<StepReport>,
}

impl StepTerminalGuard {
    fn new(observer: Arc<dyn StepObserver>, started: &StepReport) -> Self {
        let mut cancelled = started.clone();
        cancelled.outcome = StepOutcome::Failed;
        cancelled.error = Some("step cancelled before terminal outcome".to_string());
        Self {
            observer,
            cancelled: Some(cancelled),
        }
    }

    fn set_attempts(&mut self, attempts: u32) {
        if let Some(report) = &mut self.cancelled {
            report.attempts = attempts;
        }
    }

    fn finish(&mut self) {
        self.cancelled = None;
    }
}

impl Drop for StepTerminalGuard {
    fn drop(&mut self) {
        if let Some(report) = self.cancelled.take() {
            self.observer.on_step(report);
        }
    }
}

/// Foreign observer for the step outcome stream. Fire-and-forget from the
/// executor's perspective; the app maps reports onto its telemetry bus.
#[uniffi::export(with_foreign)]
pub trait StepObserver: Send + Sync {
    fn on_step(&self, report: StepReport);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum ExecutorStepFailureKind {
    /// An executor-owned wall-clock budget or transport timeout elapsed.
    DeadlineExceeded,
    /// A manifest-declared terminal condition matched an observation.
    ConditionRejected,
    /// Any non-timeout transport, validation, transform, or plan-step failure.
    Other,
}

/// Typed cause of an activity retry or terminal failure. `context` contains
/// only manifest-selected, decoded scope values; raw diagnostic text stays on
/// the executor error and step-report surfaces.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct ConnectionActivityFailure {
    pub kind: ExecutorStepFailureKind,
    pub context: Vec<KeyValue>,
}

impl ConnectionActivityFailure {
    pub(crate) fn without_context(kind: ExecutorStepFailureKind) -> Self {
        Self {
            kind,
            context: Vec::new(),
        }
    }
}

/// One replay transition. `ordinal`/`limit` belong to the local retry
/// primitive and may reset within a longer activity.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct ConnectionActivityRetry {
    pub ordinal: u32,
    pub limit: u32,
    pub failure: ConnectionActivityFailure,
}

/// Queryable terminal rollup for one activity lifecycle.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct ConnectionActivityTerminalSummary {
    /// Every replay transition across the activity, independent of local
    /// retry ordinals.
    pub retry_count: u32,
    /// The most recent failure that authorized a replay.
    pub last_retry: Option<ConnectionActivityRetry>,
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Enum)]
pub enum ConnectionActivityEvent {
    Started {
        id: String,
        version: u32,
    },
    Retrying {
        id: String,
        version: u32,
        retry: ConnectionActivityRetry,
    },
    Succeeded {
        id: String,
        version: u32,
        summary: ConnectionActivityTerminalSummary,
    },
    Failed {
        id: String,
        version: u32,
        summary: ConnectionActivityTerminalSummary,
        failure: ConnectionActivityFailure,
    },
    Cancelled {
        id: String,
        version: u32,
        summary: ConnectionActivityTerminalSummary,
    },
}

#[uniffi::export(with_foreign)]
pub trait ConnectionActivityObserver: Send + Sync {
    fn on_activity(&self, event: ConnectionActivityEvent);
}

// ---------------------------------------------------------------------------
// Entry points
// ---------------------------------------------------------------------------

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
    pub summary: EstablishmentWalkSummary,
}

/// Whether a completed establishment walk observed its declared registration
/// confirmation signal (plan §11.6).
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum EstablishmentConfirmOutcome {
    Satisfied,
    Unsatisfied,
    NotDeclared,
}

/// Per-walk confirmation verdict plus the terminal tolerated-step aggregate.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct EstablishmentWalkSummary {
    pub confirm_outcome: EstablishmentConfirmOutcome,
    pub tolerated_step_count: u32,
    pub tolerated_step_paths: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NativeEstablishmentConfirmOutcome {
    Satisfied,
    Unsatisfied,
    NotDeclared,
}

#[derive(Debug)]
pub(crate) struct NativeEstablishmentWalkSummary {
    confirm_outcome: NativeEstablishmentConfirmOutcome,
    tolerated_step_paths: Vec<String>,
}

impl Default for NativeEstablishmentWalkSummary {
    fn default() -> Self {
        Self {
            confirm_outcome: NativeEstablishmentConfirmOutcome::NotDeclared,
            tolerated_step_paths: Vec::new(),
        }
    }
}

impl NativeEstablishmentWalkSummary {
    pub(crate) fn for_steps(steps: &[Step]) -> Self {
        let mut summary = Self::default();
        summary.declare_for_steps(steps);
        summary
    }

    fn declare_for_steps(&mut self, steps: &[Step]) {
        if steps.iter().any(step_declares_confirmation)
            && self.confirm_outcome == NativeEstablishmentConfirmOutcome::NotDeclared
        {
            self.confirm_outcome = NativeEstablishmentConfirmOutcome::Unsatisfied;
        }
    }

    fn satisfy(&mut self) {
        self.confirm_outcome = NativeEstablishmentConfirmOutcome::Satisfied;
    }

    fn tolerated(&mut self, step_path: &str) {
        self.tolerated_step_paths.push(step_path.to_string());
    }
}

impl From<NativeEstablishmentConfirmOutcome> for EstablishmentConfirmOutcome {
    fn from(value: NativeEstablishmentConfirmOutcome) -> Self {
        match value {
            NativeEstablishmentConfirmOutcome::Satisfied => Self::Satisfied,
            NativeEstablishmentConfirmOutcome::Unsatisfied => Self::Unsatisfied,
            NativeEstablishmentConfirmOutcome::NotDeclared => Self::NotDeclared,
        }
    }
}

impl From<NativeEstablishmentWalkSummary> for EstablishmentWalkSummary {
    fn from(value: NativeEstablishmentWalkSummary) -> Self {
        Self {
            confirm_outcome: value.confirm_outcome.into(),
            tolerated_step_count: value.tolerated_step_paths.len() as u32,
            tolerated_step_paths: value.tolerated_step_paths,
        }
    }
}

/// Execute the establishment plan behind `plan_handle` (`model:selector`,
/// returned by establishment/reconnect decisions) against a foreign transport.
/// `initial_scope` is the recognition `runtime_scope` and `initial_encodings`
/// its `runtime_scope_encodings` — threading the real capture encodings from
/// the matched signature so a `{ captured: … }` write-back re-encodes
/// correctly without the app-side hex-string heuristic (#43). Unknown
/// encoding tokens are ignored (the scope value then decodes by fallback).
#[uniffi::export]
#[allow(clippy::too_many_arguments)] // Flat, explicit FFI call contract.
pub async fn run_establishment(
    store: Arc<ConfigStore>,
    plan_handle: String,
    transport: Arc<dyn BleExecutorTransport>,
    observer: Arc<dyn StepObserver>,
    activity_observer: Arc<dyn ConnectionActivityObserver>,
    initial_scope: Vec<KeyValue>,
    initial_encodings: Vec<KeyValue>,
    runtime_params: Vec<KeyValue>,
) -> Result<ExecutionOutcome, ExecutorError> {
    let block = resolve_establishment(&store, &plan_handle)?;
    let summary = NativeEstablishmentWalkSummary::for_steps(&block.steps);
    let encodings = initial_encodings
        .into_iter()
        .filter_map(|kv| Encoding::from_token(&kv.value).map(|enc| (kv.key, enc)))
        .collect();

    let mut ctx = ExecCtx {
        transport: ExecTransport::Ble(&transport),
        observer: &observer,
        activity_observer: Some(&activity_observer),
        active_activity: None,
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
        nikon_lss_session: None,
        steps_run: 0,
        summary,
        refine: Some(RefineCtx {
            source: RefinementSource::Store(&store),
            plan_handle: plan_handle.clone(),
        }),
        usb_interfaces: BTreeMap::new(),
        usb_interface_claimed: false,
    };
    walk_plan_with_activities(
        &mut ctx,
        block.steps,
        block.activities,
        ConfigActivitySequence::Steps,
    )
    .await?;
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
#[allow(clippy::too_many_arguments)] // Mirrors run_establishment at the FFI seam.
pub async fn run_post_exit_readiness(
    store: Arc<ConfigStore>,
    plan_handle: String,
    transport: Arc<dyn BleExecutorTransport>,
    observer: Arc<dyn StepObserver>,
    activity_observer: Arc<dyn ConnectionActivityObserver>,
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
        transport: ExecTransport::Ble(&transport),
        observer: &observer,
        activity_observer: Some(&activity_observer),
        active_activity: None,
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
        nikon_lss_session: None,
        steps_run: 0,
        summary: NativeEstablishmentWalkSummary::default(),
        refine: None,
        usb_interfaces: BTreeMap::new(),
        usb_interface_claimed: false,
    };
    walk_plan_with_activities(
        &mut ctx,
        block.post_exit_readiness,
        block.activities,
        ConfigActivitySequence::PostExitReadiness,
    )
    .await?;
    Ok(outcome(ctx))
}

/// Shared plan-handle resolution: parse `model:selector` and resolve the
/// establishment mechanism connection-first — a selector naming a declared
/// body connection must declare an establishment; any other selector is
/// itself the mechanism key (reconnect handles). The caller picks the family
/// registry (BLE, or USB per §11.29) the mechanism then resolves in.
pub(crate) struct ResolvedPlanRef<'a> {
    pub(crate) view: &'a camera_config::index::ModelView,
    pub(crate) mechanism: String,
}

pub(crate) fn resolve_plan_ref<'a, E>(
    store: &'a ConfigStore,
    plan_handle: &'a str,
    unknown: impl Fn(String) -> E,
) -> Result<ResolvedPlanRef<'a>, E> {
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
    Ok(ResolvedPlanRef { view, mechanism })
}

/// Resolve `plan_handle` (`model:selector`) to its establishment block,
/// looking the mechanism up in the family BLE registry. Raw USB
/// establishments resolve through [`crate::usb_executor::run_usb_establishment`].
fn resolve_establishment(
    store: &ConfigStore,
    plan_handle: &str,
) -> Result<EstablishmentBlock, ExecutorError> {
    let unknown = |detail: String| ExecutorError::UnknownPlan { detail };
    let resolved = resolve_plan_ref(store, plan_handle, unknown)?;
    resolved
        .view
        .ble
        .as_ref()
        .and_then(|ble| ble.establishment(&resolved.mechanism))
        .cloned()
        .ok_or_else(|| {
            unknown(format!(
                "{plan_handle}: missing mechanism {}",
                resolved.mechanism
            ))
        })
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
        transport: ExecTransport::Ble(&transport),
        observer: &observer,
        activity_observer: None,
        active_activity: None,
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
        nikon_lss_session: None,
        steps_run: 0,
        summary: NativeEstablishmentWalkSummary::default(),
        refine: None,
        usb_interfaces: BTreeMap::new(),
        usb_interface_claimed: false,
    };
    walk_plan_with_activities(
        &mut ctx,
        block.steps.clone(),
        Vec::new(),
        ConfigActivitySequence::Steps,
    )
    .await?;
    Ok(outcome(ctx))
}

pub(crate) fn outcome(ctx: ExecCtx<'_>) -> ExecutionOutcome {
    ExecutionOutcome {
        scope: ctx
            .scope
            .into_iter()
            .map(|(key, value)| KeyValue { key, value })
            .collect(),
        steps_run: ctx.steps_run,
        summary: ctx.summary.into(),
    }
}

// ---------------------------------------------------------------------------
// Walk state
// ---------------------------------------------------------------------------

pub(crate) struct RefineCtx<'a> {
    pub(crate) source: RefinementSource<'a>,
    pub(crate) plan_handle: String,
}

pub(crate) enum RefinementSource<'a> {
    Store(&'a ConfigStore),
    #[cfg(test)]
    #[allow(dead_code)]
    Resolver(&'a dyn NativeRefinementResolver),
}

#[cfg(test)]
pub(crate) trait NativeRefinementResolver: Send + Sync {
    fn refine(
        &self,
        plan_handle: String,
        firmware: String,
        scope: Vec<KeyValue>,
        next_step_index: u32,
    ) -> Result<crate::NativeEstablishmentRefinement, crate::EstablishmentError>;
}

impl RefineCtx<'_> {
    fn refine(
        &self,
        firmware: String,
        scope: Vec<KeyValue>,
        next_step_index: u32,
    ) -> Result<crate::NativeEstablishmentRefinement, crate::EstablishmentError> {
        match self.source {
            RefinementSource::Store(store) => crate::refine_establishment_native(
                store,
                self.plan_handle.clone(),
                firmware,
                scope,
                next_step_index,
            ),
            #[cfg(test)]
            RefinementSource::Resolver(resolver) => {
                resolver.refine(self.plan_handle.clone(), firmware, scope, next_step_index)
            }
        }
    }
}

/// The transport behind a walk: BLE for `run_establishment` /
/// `run_post_exit_readiness` / `run_ble_action`, raw USB for
/// `run_usb_establishment` (§11.29). A verb addressed at the other transport
/// fails in `run_step_once`; the loader's plan scoping keeps such plans from
/// loading at all, so the mismatch arms are defensive.
pub(crate) enum ExecTransport<'a> {
    Ble(&'a Arc<dyn BleExecutorTransport>),
    Usb(&'a Arc<dyn UsbExecutorTransport>),
}

impl<'a> ExecTransport<'a> {
    /// The BLE transport, when this walk runs over BLE.
    fn ble(&self) -> Option<&'a Arc<dyn BleExecutorTransport>> {
        match self {
            Self::Ble(transport) => Some(*transport),
            Self::Usb(_) => None,
        }
    }

    /// The USB transport, when this walk runs over raw USB.
    fn usb(&self) -> Option<&'a Arc<dyn UsbExecutorTransport>> {
        match self {
            Self::Usb(transport) => Some(*transport),
            Self::Ble(_) => None,
        }
    }

    /// Host wall clock for retry backoff and poll cadence: both transports
    /// expose `sleep`, so the shared retry ladder runs unchanged on either.
    async fn sleep(&self, ms: u32) -> Result<(), TransportError> {
        match self {
            Self::Ble(transport) => transport.sleep(ms).await,
            Self::Usb(transport) => transport.sleep(ms).await.map_err(Into::into),
        }
    }
}

pub(crate) struct ExecCtx<'a> {
    pub(crate) transport: ExecTransport<'a>,
    pub(crate) observer: &'a Arc<dyn StepObserver>,
    pub(crate) activity_observer: Option<&'a Arc<dyn ConnectionActivityObserver>>,
    pub(crate) active_activity: Option<ActiveActivity>,
    pub(crate) scope: BTreeMap<String, String>,
    /// Encoding each scope key was captured with — `{ captured: … }` writes
    /// re-encode by this instead of guessing from the scope string.
    pub(crate) encodings: BTreeMap<String, Encoding>,
    pub(crate) runtime_params: BTreeMap<String, String>,
    /// Successful CCCD enables in this walk. A retry reuses transport state.
    pub(crate) subscriptions: BTreeSet<(String, bool)>,
    /// Opaque authenticated Nikon LSS cipher state. It deliberately has no
    /// scope/FFI/log representation and lives only for this executor walk.
    pub(crate) nikon_lss_session: Option<NikonLssSession>,
    pub(crate) steps_run: u32,
    pub(crate) summary: NativeEstablishmentWalkSummary,
    /// Present for establishment walks; `acquireFirmware` re-resolves the
    /// tail through it (§11.5). `None` for BLE actions.
    pub(crate) refine: Option<RefineCtx<'a>>,
    /// The family `usb.interfaces` map a `usbClaim` step's symbolic interface
    /// name resolves against (§11.29). Empty on BLE walks.
    pub(crate) usb_interfaces: BTreeMap<String, UsbInterfaceTriple>,
    /// Set once a `usbClaim` succeeds, so a failed raw USB walk can release
    /// the claimed interface. Unused on BLE walks.
    pub(crate) usb_interface_claimed: bool,
}

pub(crate) struct ActiveActivity {
    observer: Arc<dyn ConnectionActivityObserver>,
    id: String,
    version: u32,
    retry_count: u32,
    last_retry: Option<ConnectionActivityRetry>,
    terminal: bool,
}

impl ActiveActivity {
    pub(crate) fn new(
        observer: Arc<dyn ConnectionActivityObserver>,
        id: String,
        version: u32,
    ) -> Self {
        observer.on_activity(ConnectionActivityEvent::Started {
            id: id.clone(),
            version,
        });
        Self {
            observer,
            id,
            version,
            retry_count: 0,
            last_retry: None,
            terminal: false,
        }
    }

    pub(crate) fn id(&self) -> &str {
        &self.id
    }

    pub(crate) fn version(&self) -> u32 {
        self.version
    }

    fn summary(&self) -> ConnectionActivityTerminalSummary {
        ConnectionActivityTerminalSummary {
            retry_count: self.retry_count,
            last_retry: self.last_retry.clone(),
        }
    }

    fn emit(&mut self, event: ConnectionActivityEvent) {
        self.observer.on_activity(event);
    }

    pub(crate) fn retry(&mut self, retry: ConnectionActivityRetry) {
        self.retry_count = self.retry_count.saturating_add(1);
        self.last_retry = Some(retry.clone());
        self.emit(ConnectionActivityEvent::Retrying {
            id: self.id.clone(),
            version: self.version,
            retry,
        });
    }

    pub(crate) fn succeed(mut self) {
        self.emit(ConnectionActivityEvent::Succeeded {
            id: self.id.clone(),
            version: self.version,
            summary: self.summary(),
        });
        self.terminal = true;
    }

    pub(crate) fn fail(mut self, failure: ConnectionActivityFailure) {
        self.emit(ConnectionActivityEvent::Failed {
            id: self.id.clone(),
            version: self.version,
            summary: self.summary(),
            failure,
        });
        self.terminal = true;
    }
}

impl Drop for ActiveActivity {
    fn drop(&mut self) {
        if !self.terminal {
            self.observer
                .on_activity(ConnectionActivityEvent::Cancelled {
                    id: self.id.clone(),
                    version: self.version,
                    summary: self.summary(),
                });
            self.terminal = true;
        }
    }
}

impl ExecCtx<'_> {
    fn begin_activity(&mut self, descriptor: &ConfigActivityDescriptor) {
        debug_assert!(self.active_activity.is_none());
        if let Some(observer) = self.activity_observer {
            self.active_activity = Some(ActiveActivity::new(
                Arc::clone(observer),
                descriptor.id.clone(),
                descriptor.version,
            ));
        }
    }

    fn succeed_activity(&mut self) {
        if let Some(activity) = self.active_activity.take() {
            activity.succeed();
        }
    }

    fn fail_activity(&mut self, failure: ConnectionActivityFailure) {
        if let Some(activity) = self.active_activity.take() {
            activity.fail(failure);
        }
    }

    fn retry_activity(&mut self, ordinal: u32, limit: u32, failure: ConnectionActivityFailure) {
        if let Some(activity) = &mut self.active_activity {
            activity.retry(ConnectionActivityRetry {
                ordinal,
                limit,
                failure,
            });
        }
    }

    fn activity_correlation(&self) -> (Option<String>, Option<u32>) {
        self.active_activity
            .as_ref()
            .map(|activity| (Some(activity.id().to_string()), Some(activity.version())))
            .unwrap_or((None, None))
    }
}

/// Step failure: which step (verb + position path) and why.
#[derive(Debug)]
pub(crate) struct StepError {
    pub(crate) step: String,
    pub(crate) kind: ExecutorStepFailureKind,
    pub(crate) message: String,
    pub(crate) context: Vec<KeyValue>,
    /// The step's verb does not run on this walk's transport (§11.29). The
    /// BLE executor surfaces it as an ordinary step failure; the USB
    /// executor maps it to its typed `UnsupportedVerb` variant.
    pub(crate) unsupported_verb: bool,
}

impl StepError {
    fn other(step: &str, message: String) -> Self {
        Self {
            step: step.to_string(),
            kind: ExecutorStepFailureKind::Other,
            message,
            context: Vec::new(),
            unsupported_verb: false,
        }
    }

    /// A verb reached through a walk over the wrong transport (§11.29). The
    /// loader's plan scoping makes this unreachable from manifest data.
    fn unsupported_verb(step: &str, verb: &str, transport: &str) -> Self {
        Self {
            step: step.to_string(),
            kind: ExecutorStepFailureKind::Other,
            message: format!("verb `{verb}` is not supported by the {transport} transport"),
            context: Vec::new(),
            unsupported_verb: true,
        }
    }

    fn deadline(step: &str, message: String) -> Self {
        Self {
            step: step.to_string(),
            kind: ExecutorStepFailureKind::DeadlineExceeded,
            message,
            context: Vec::new(),
            unsupported_verb: false,
        }
    }

    fn condition_rejected(step: &str, message: String) -> Self {
        Self {
            step: step.to_string(),
            kind: ExecutorStepFailureKind::ConditionRejected,
            message,
            context: Vec::new(),
            unsupported_verb: false,
        }
    }

    fn operation(step: &str, failure: OperationFailure) -> Self {
        Self {
            step: step.to_string(),
            kind: failure.kind,
            message: failure.message,
            context: Vec::new(),
            unsupported_verb: false,
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
struct NativeRefinedTail {
    steps: Vec<Step>,
    activities: Vec<ConfigActivityDescriptor>,
}

type RefinedTail = Option<NativeRefinedTail>;

// ---------------------------------------------------------------------------
// The walk
// ---------------------------------------------------------------------------

/// Top-level walk with §11.5 tail splicing: a step that returns a refined
/// tail replaces everything after itself, and the walk continues into the
/// spliced steps.
pub(crate) async fn walk_plan_with_activities(
    ctx: &mut ExecCtx<'_>,
    mut steps: Vec<Step>,
    mut activities: Vec<ConfigActivityDescriptor>,
    sequence: ConfigActivitySequence,
) -> Result<(), StepError> {
    let mut i = 0;
    while i < steps.len() {
        let activity = activities
            .iter()
            .find(|activity| {
                matches!(
                    &activity.binding,
                    ConfigActivityBinding::ExecutorSpan(binding)
                        if binding.executor_span.sequence == sequence
                            && binding.executor_span.start_step <= i as u32
                            && (i as u32) < binding.executor_span.end_step_exclusive
                )
            })
            .cloned();
        if ctx.active_activity.is_none() {
            if let Some(activity) = &activity {
                ctx.begin_activity(activity);
            }
        }
        let step = steps[i].clone();
        let here = format!("steps[{i}].{}", step.verb_name());
        let mut refined_activity_continues = false;
        match run_step(ctx, &step, &here, (i + 1) as u32).await {
            Ok(Some(tail)) => {
                ctx.summary.declare_for_steps(&tail.steps);
                steps.truncate(i + 1);
                steps.extend(tail.steps);
                refined_activity_continues = splice_refined_activities(
                    &mut activities,
                    &sequence,
                    (i + 1) as u32,
                    tail.activities,
                );
                if !refined_activity_continues {
                    ctx.succeed_activity();
                }
            }
            Ok(None) => {}
            Err(error) => {
                ctx.fail_activity(ConnectionActivityFailure {
                    kind: error.kind,
                    context: error.context.clone(),
                });
                return Err(error);
            }
        }
        let activity_ends = !refined_activity_continues
            && ctx.active_activity.is_some()
            && activity.is_some_and(|activity| {
                matches!(
                    &activity.binding,
                    ConfigActivityBinding::ExecutorSpan(binding)
                        if binding.executor_span.end_step_exclusive == (i + 1) as u32
                )
            });
        if activity_ends {
            ctx.succeed_activity();
        }
        i += 1;
    }
    Ok(())
}

fn splice_refined_activities(
    activities: &mut Vec<ConfigActivityDescriptor>,
    sequence: &ConfigActivitySequence,
    next_step: u32,
    replacements: Vec<ConfigActivityDescriptor>,
) -> bool {
    let continuation = activities
        .iter()
        .enumerate()
        .find(|(_, activity)| {
            matches!(
                &activity.binding,
                ConfigActivityBinding::ExecutorSpan(binding)
                    if &binding.executor_span.sequence == sequence
                        && binding.executor_span.start_step < next_step
                        && next_step <= binding.executor_span.end_step_exclusive
            )
        })
        .and_then(|(retained_index, retained)| {
            replacements
                .iter()
                .position(|replacement| {
                    matches!(
                        &replacement.binding,
                        ConfigActivityBinding::ExecutorSpan(binding)
                            if &binding.executor_span.sequence == sequence
                                && binding.executor_span.start_step == 0
                    ) && same_activity_metadata(retained, replacement)
                })
                .map(|replacement_index| (retained_index, replacement_index))
        });

    activities.retain_mut(|activity| match &mut activity.binding {
        ConfigActivityBinding::ExecutorSpan(binding)
            if &binding.executor_span.sequence == sequence =>
        {
            if binding.executor_span.start_step >= next_step {
                false
            } else {
                binding.executor_span.end_step_exclusive =
                    binding.executor_span.end_step_exclusive.min(next_step);
                true
            }
        }
        _ => true,
    });
    let mut replacements = replacements;
    if let Some((retained_index, replacement_index)) = continuation {
        let replacement = replacements.remove(replacement_index);
        let ConfigActivityBinding::ExecutorSpan(replacement_binding) = replacement.binding else {
            unreachable!("continuation selection requires an executor span");
        };
        let ConfigActivityBinding::ExecutorSpan(retained_binding) =
            &mut activities[retained_index].binding
        else {
            unreachable!("continuation selection requires an executor span");
        };
        retained_binding.executor_span.end_step_exclusive =
            next_step + replacement_binding.executor_span.end_step_exclusive;
    }
    activities.extend(replacements.into_iter().map(|mut activity| {
        if let ConfigActivityBinding::ExecutorSpan(binding) = &mut activity.binding {
            binding.executor_span.start_step += next_step;
            binding.executor_span.end_step_exclusive += next_step;
        }
        activity
    }));
    continuation.is_some()
}

fn same_activity_metadata(
    left: &ConfigActivityDescriptor,
    right: &ConfigActivityDescriptor,
) -> bool {
    left.id == right.id
        && left.version == right.version
        && left.display_role == right.display_role
        && left.default_expected_duration_ms == right.default_expected_duration_ms
        && left.interaction_required == right.interaction_required
}

#[cfg(test)]
async fn walk_plan(ctx: &mut ExecCtx<'_>, steps: Vec<Step>) -> Result<(), StepError> {
    walk_plan_with_activities(ctx, steps, Vec::new(), ConfigActivitySequence::Steps).await
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
        let confirms = opts.confirms.is_some();
        if confirms && ctx.summary.confirm_outcome == NativeEstablishmentConfirmOutcome::NotDeclared
        {
            ctx.summary.confirm_outcome = NativeEstablishmentConfirmOutcome::Unsatisfied;
        }
        let verb = step.verb_name();
        let characteristic = step_characteristic(step);
        let (activity_id, activity_version) = ctx.activity_correlation();
        let tolerant = match step {
            // §11.6: If's tolerant gates predicate fields, not body errors.
            Step::If(_) => false,
            other => other.options().tolerant,
        };
        let report = |outcome: StepOutcome, error: Option<String>, attempts: u32| StepReport {
            step_path: here.to_string(),
            verb: verb.to_string(),
            characteristic: characteristic.clone(),
            operation: None,
            property: None,
            response_code: None,
            transaction_id: None,
            tolerant,
            outcome,
            error,
            attempts,
            activity_id: activity_id.clone(),
            activity_version,
        };
        let started = report(StepOutcome::Started, None, 0);
        ctx.observer.on_step(started.clone());
        let mut terminal_guard = StepTerminalGuard::new(Arc::clone(ctx.observer), &started);

        if let Step::Retry(retry) = step {
            return match run_retry_control(ctx, retry, here, top_next, &mut terminal_guard).await {
                Ok((tail, retries_consumed)) => {
                    ctx.steps_run += 1;
                    if confirms {
                        ctx.summary.satisfy();
                    }
                    terminal_guard.finish();
                    ctx.observer
                        .on_step(report(StepOutcome::Succeeded, None, retries_consumed));
                    Ok(tail)
                }
                Err((error, retries_consumed)) if tolerant => {
                    ctx.steps_run += 1;
                    ctx.summary.tolerated(here);
                    terminal_guard.finish();
                    ctx.observer.on_step(report(
                        StepOutcome::Tolerated,
                        Some(error.message),
                        retries_consumed,
                    ));
                    Ok(None)
                }
                Err((error, retries_consumed)) => {
                    terminal_guard.finish();
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
            terminal_guard.set_attempts(attempt);
            match run_step_once(ctx, step, here, top_next).await {
                Ok(tail) => {
                    ctx.steps_run += 1;
                    if confirms {
                        ctx.summary.satisfy();
                    }
                    terminal_guard.finish();
                    ctx.observer
                        .on_step(report(StepOutcome::Succeeded, None, attempt));
                    return Ok(tail);
                }
                Err(e) if attempt < opts.retries => {
                    ctx.retry_activity(
                        attempt + 2,
                        opts.retries + 1,
                        ConnectionActivityFailure::without_context(e.kind),
                    );
                    attempt += 1;
                    terminal_guard.set_attempts(attempt);
                    if opts.retry_delay_ms > 0 {
                        let _ = ctx.transport.sleep(opts.retry_delay_ms).await;
                    }
                }
                Err(e) if tolerant => {
                    ctx.steps_run += 1;
                    ctx.summary.tolerated(here);
                    terminal_guard.finish();
                    ctx.observer
                        .on_step(report(StepOutcome::Tolerated, Some(e.message), attempt));
                    return Ok(None);
                }
                Err(e) => {
                    terminal_guard.finish();
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

fn step_declares_confirmation(step: &Step) -> bool {
    if step.options().confirms.is_some() {
        return true;
    }
    match step {
        Step::Acquire(step) => step_declares_confirmation(&step.from),
        Step::If(step) => step
            .then
            .iter()
            .chain(&step.else_branch)
            .any(step_declares_confirmation),
        Step::BleAwaitUntil(step) => step
            .failure_evidence
            .iter()
            .flat_map(|evidence| &evidence.steps)
            .chain(&step.on_each)
            .any(step_declares_confirmation),
        Step::Retry(step) => step
            .steps
            .iter()
            .chain(&step.on_failure)
            .any(step_declares_confirmation),
        _ => false,
    }
}

async fn run_retry_control(
    ctx: &mut ExecCtx<'_>,
    retry: &camera_config::index::RetryStep,
    here: &str,
    top_next: u32,
    terminal_guard: &mut StepTerminalGuard,
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

                let failure = ConnectionActivityFailure {
                    kind: body_error.kind,
                    context: retry
                        .failure_context
                        .iter()
                        .filter_map(|key| {
                            ctx.scope.get(key).map(|value| KeyValue {
                                key: key.clone(),
                                value: value.clone(),
                            })
                        })
                        .collect(),
                };
                let attempts_used = retries_consumed + 1;
                if !should_retry || attempts_used >= retry.max_attempts {
                    body_error.context = failure.context;
                    return Err((body_error, retries_consumed));
                }

                retries_consumed += 1;
                terminal_guard.set_attempts(retries_consumed);
                ctx.retry_activity(retries_consumed + 1, retry.max_attempts, failure);
                if retry.retry_delay_ms > 0 {
                    if let Err(error) = ctx.transport.sleep(retry.retry_delay_ms).await {
                        return Err((StepError::transport(here, error), retries_consumed));
                    }
                }
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
        Step::NikonLssAuthenticate(s) => Some(s.gatt.clone()),
        Step::NikonLssReadConnectionConfiguration(s) => Some(s.gatt.clone()),
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
    let verb = step.verb_name();
    match step {
        Step::BleConnect(_) => {
            let transport = ble_transport(ctx, here, verb)?;
            deadline(transport, CONNECT_TIMEOUT_MS, "connect", async {
                transport.connect().await
            })
            .await
            .map_err(op_err)?;
            Ok(None)
        }
        Step::BleDelay(s) => {
            ctx.transport
                .sleep(s.duration_ms)
                .await
                .map_err(|error| StepError::transport(here, error))?;
            Ok(None)
        }
        Step::BleAwaitDisconnect(s) => {
            let transport = ble_transport(ctx, here, verb)?;
            deadline(transport, s.timeout_ms, "awaitDisconnect", async {
                transport.await_disconnect().await
            })
            .await
            .map_err(op_err)?;
            Ok(None)
        }
        Step::BleRequestMtu(s) => {
            let transport = ble_transport(ctx, here, verb)?;
            let negotiated = deadline(transport, DEFAULT_OP_TIMEOUT_MS, "requestMtu", async {
                transport.request_mtu(s.requested_mtu).await
            })
            .await
            .map_err(op_err)?;
            // §11.4a: the checkpoint is the evidenced floor, not the request
            // target. No floor means any negotiated MTU succeeds.
            if let Some(minimum) = s.minimum_mtu {
                if negotiated < minimum {
                    return Err(err(format!(
                        "negotiated MTU {negotiated} < required {minimum}"
                    )));
                }
            }
            Ok(None)
        }
        Step::BleDiscoverServices(_) => {
            let transport = ble_transport(ctx, here, verb)?;
            deadline(
                transport,
                DEFAULT_OP_TIMEOUT_MS,
                "discoverServices",
                async { transport.ensure_services_discovered().await },
            )
            .await
            .map_err(op_err)?;
            Ok(None)
        }
        Step::BleRead(s) => {
            let transport = ble_transport(ctx, here, verb)?;
            let wire = deadline(transport, DEFAULT_OP_TIMEOUT_MS, "read", async {
                transport.read(s.gatt.clone()).await
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
        Step::BlePeripheralName(s) => {
            let transport = ble_transport(ctx, here, verb)?;
            let raw = deadline(transport, DEFAULT_OP_TIMEOUT_MS, "peripheralName", async {
                transport.peripheral_name().await
            })
            .await
            .map_err(op_err)?;
            // §11.4b: UTF-8 with any NUL terminator removed; a name that is
            // empty after the trim is unavailable and must fail the step, so
            // a host never silently binds an empty capture.
            let name = eval::decode_bytes(raw.as_bytes(), Encoding::Utf8Cstring)
                .ok_or_else(|| err("peripheral name is not valid UTF-8".into()))?;
            if name.is_empty() {
                return Err(err("peripheral name unavailable".into()));
            }
            ctx.scope.insert(s.capture_as.clone(), name);
            ctx.encodings.insert(s.capture_as.clone(), Encoding::Utf8);
            Ok(None)
        }
        Step::BleWrite(s) => {
            let transport = ble_transport(ctx, here, verb)?;
            let bytes = resolve_value(ctx, &s.value).map_err(err)?;
            deadline(transport, DEFAULT_OP_TIMEOUT_MS, "write", async {
                match &s.notification_fence {
                    Some(notification_characteristic) => {
                        transport
                            .write_with_notification_fence(
                                s.gatt.clone(),
                                bytes,
                                notification_characteristic.clone(),
                            )
                            .await
                    }
                    None => transport.write(s.gatt.clone(), bytes).await,
                }
            })
            .await
            .map_err(op_err)?;
            Ok(None)
        }
        Step::BleSubscribe(s) => {
            let transport = ble_transport(ctx, here, verb)?;
            let budget = if s.timeout_ms > 0 {
                s.timeout_ms
            } else {
                DEFAULT_OP_TIMEOUT_MS
            };
            ensure_subscribed(ctx, transport, &s.gatt, s.mode, budget)
                .await
                .map_err(op_err)?;
            Ok(None)
        }
        Step::BleNotify(s) => {
            let transport = ble_transport(ctx, here, verb)?;
            run_notify(ctx, transport, s, here).await?;
            Ok(None)
        }
        Step::BleAwaitUntil(s) => {
            let transport = ble_transport(ctx, here, verb)?;
            run_await_until(ctx, transport, s, here, top_next).await
        }
        Step::BleWriteChunk(s) => {
            let transport = ble_transport(ctx, here, verb)?;
            run_write_chunk(ctx, transport, s, here).await?;
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
                    let transport = ble_transport(ctx, here, verb)?;
                    let wire = deadline(transport, DEFAULT_OP_TIMEOUT_MS, "read", async {
                        transport.read(gatt.clone()).await
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
            match refine.refine(firmware, scope_kvs, top_next) {
                Ok(crate::NativeEstablishmentRefinement::NoChange) => Ok(None),
                Ok(crate::NativeEstablishmentRefinement::ReplaceTail { steps, activities }) => {
                    Ok(Some(NativeRefinedTail { steps, activities }))
                }
                Err(e) => Err(err(format!("refinement failed: {e}"))),
            }
        }
        Step::NikonLssAuthenticate(s) => {
            let transport = ble_transport(ctx, here, verb)?;
            // Re-authentication must fail closed: a failed exchange must never
            // leave a prior session available to later encrypted reads.
            ctx.nikon_lss_session = None;
            let client_device_id: [u8; 8] = resolve_value(ctx, &s.client_device_id)
                .map_err(&err)?
                .try_into()
                .map_err(|bytes: Vec<u8>| {
                    err(format!(
                        "clientDeviceId must resolve to exactly 8 bytes (got {})",
                        bytes.len()
                    ))
                })?;
            let nonce: [u8; 8] = resolve_value(ctx, &s.nonce)
                .map_err(&err)?
                .try_into()
                .map_err(|bytes: Vec<u8>| {
                    err(format!(
                        "nonce must resolve to exactly 8 bytes (got {})",
                        bytes.len()
                    ))
                })?;
            ensure_subscribed(
                ctx,
                transport,
                &s.gatt,
                camera_config::index::CccdMode::Indicate,
                s.timeout_ms,
            )
            .await
            .map_err(op_err)?;

            let mut client = NikonLssClient::new(client_device_id, nonce);
            let stage1 = client
                .stage1_record()
                .map_err(|e| err(format!("LSS stage 1 failed: {e}")))?;
            deadline(transport, s.timeout_ms, "LSS stage 1 write", async {
                transport
                    .write_with_notification_fence(s.gatt.clone(), stage1.to_vec(), s.gatt.clone())
                    .await
            })
            .await
            .map_err(op_err)?;
            let stage2 = deadline(transport, s.timeout_ms, "LSS stage 2 indication", async {
                transport.next_notification(s.gatt.clone()).await
            })
            .await
            .map_err(op_err)?;
            let stage3 = client
                .handle_stage2(&stage2)
                .map_err(|e| err(format!("LSS stage 2 failed: {e}")))?;
            deadline(transport, s.timeout_ms, "LSS stage 3 write", async {
                transport
                    .write_with_notification_fence(s.gatt.clone(), stage3.to_vec(), s.gatt.clone())
                    .await
            })
            .await
            .map_err(op_err)?;
            let stage4 = deadline(transport, s.timeout_ms, "LSS stage 4 indication", async {
                transport.next_notification(s.gatt.clone()).await
            })
            .await
            .map_err(op_err)?;
            ctx.nikon_lss_session = Some(
                client
                    .finish_stage4(&stage4)
                    .map_err(|e| err(format!("LSS stage 4 failed: {e}")))?,
            );
            Ok(None)
        }
        Step::NikonLssReadConnectionConfiguration(s) => {
            let transport = ble_transport(ctx, here, verb)?;
            let session = ctx
                .nikon_lss_session
                .as_ref()
                .ok_or_else(|| err("Nikon LSS session is not authenticated".into()))?;
            let wire = deadline(transport, DEFAULT_OP_TIMEOUT_MS, "LSS config read", async {
                transport.read(s.gatt.clone()).await
            })
            .await
            .map_err(op_err)?;
            let config = session
                .decode_connection_configuration(&wire)
                .map_err(|e| err(format!("LSS connection configuration failed: {e}")))?;
            bind_nikon_connection_configuration(ctx, s, config);
            Ok(None)
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
        Step::UsbClaim(s) => {
            let transport = usb_transport(ctx, here)?;
            let triple = ctx
                .usb_interfaces
                .get(&s.interface)
                .copied()
                .ok_or_else(|| {
                    err(format!(
                        "usbClaim interface '{}' is not declared in the family usb.interfaces map",
                        s.interface
                    ))
                })?;
            usb_deadline(transport, DEFAULT_OP_TIMEOUT_MS, "usbClaim", async {
                transport
                    .claim_interface(triple.class, triple.subclass, triple.protocol)
                    .await
            })
            .await
            .map_err(op_err)?;
            ctx.usb_interface_claimed = true;
            Ok(None)
        }
        Step::UsbBulkOut(s) => {
            let transport = usb_transport(ctx, here)?;
            let bytes = resolve_value(ctx, &s.data).map_err(err)?;
            usb_deadline(transport, DEFAULT_OP_TIMEOUT_MS, "usbBulkOut", async {
                transport.bulk_out(bytes).await
            })
            .await
            .map_err(op_err)?;
            Ok(None)
        }
        Step::UsbBulkIn(s) => {
            let transport = usb_transport(ctx, here)?;
            let wire = usb_deadline(transport, DEFAULT_OP_TIMEOUT_MS, "usbBulkIn", async {
                transport.bulk_in(s.max_length).await
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
        Step::UsbAwaitInterrupt(s) => {
            let transport = usb_transport(ctx, here)?;
            let frame = usb_deadline(
                transport,
                s.timeout_ms.unwrap_or(DEFAULT_OP_TIMEOUT_MS),
                "usbAwaitInterrupt",
                async { transport.next_interrupt_event().await },
            )
            .await
            .map_err(op_err)?;
            // §11.13 capture pipeline, same as `usbBulkIn`.
            let bytes = eval::apply_transforms(&frame, &s.transform)
                .ok_or_else(|| err("transform chain failed".into()))?;
            let value = eval::decode_bytes(&bytes, s.encoding)
                .ok_or_else(|| err(format!("decode as {} failed", s.encoding.as_token())))?;
            ctx.scope.insert(s.capture_as.clone(), value);
            ctx.encodings.insert(s.capture_as.clone(), s.encoding);
            Ok(None)
        }
    }
}

// ---------------------------------------------------------------------------
// Notify + await loops
// ---------------------------------------------------------------------------

async fn ensure_subscribed(
    ctx: &mut ExecCtx<'_>,
    transport: &Arc<dyn BleExecutorTransport>,
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
    deadline(transport, timeout_ms, "subscribe", async {
        transport.subscribe(gatt.to_string(), mode.into()).await
    })
    .await?;
    ctx.subscriptions.insert(key);
    Ok(())
}

/// `bleNotify` (§11.8): CCCD-enable, then consume the notification stream
/// until a payload satisfies `until` or the wall-clock budget lapses. A
/// non-matching payload keeps the wait alive (the app dispatcher's semantics;
/// the deterministic walker fails on it instead).
async fn run_notify(
    ctx: &mut ExecCtx<'_>,
    transport: &Arc<dyn BleExecutorTransport>,
    s: &BleNotifyStep,
    here: &str,
) -> Result<(), StepError> {
    let err = |message: String| StepError::other(here, message);
    ensure_subscribed(ctx, transport, &s.gatt, s.mode, DEFAULT_OP_TIMEOUT_MS)
        .await
        .map_err(|failure| StepError::operation(here, failure))?;

    let mut budget = transport.sleep(s.timeout_ms).fuse();
    let mut observations: Vec<String> = Vec::new();
    for _ in 0..MAX_LOOP_ITERS {
        let payload = next_or_budget(transport, &s.gatt, &mut budget)
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
    transport: &Arc<dyn BleExecutorTransport>,
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
            ensure_subscribed(ctx, transport, gatt, *mode, DEFAULT_OP_TIMEOUT_MS)
                .await
                .map_err(|failure| StepError::operation(here, failure))?;

            let mut budget = transport.sleep(s.timeout_ms).fuse();
            let mut observations: Vec<String> = Vec::new();
            let mut seed_pending = *seed_read;
            let mut tail = None;
            for _ in 0..MAX_LOOP_ITERS {
                let value = if seed_pending {
                    // One fresh read routed through the same acceptance path,
                    // so an already-satisfied state resolves immediately.
                    seed_pending = false;
                    let read = transport.read(gatt.clone());
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
                    match next_or_budget(transport, gatt, &mut budget).await {
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
                    return Ok(tail);
                }
                if fail_when_holds(&ctx.scope, s) {
                    let confirmed = match &s.failure_evidence {
                        None => true,
                        Some(evidence) => {
                            ctx.scope.remove(&evidence.when.field);
                            ctx.encodings.remove(&evidence.when.field);
                            let evidence_path = format!("{here}.failureEvidence.steps");
                            let evidence_result = {
                                let evidence_walk =
                                    walk_steps(ctx, &evidence.steps, &evidence_path, top_next);
                                futures_util::pin_mut!(evidence_walk);
                                match select(evidence_walk, &mut budget).await {
                                    Either::Left((result, _)) => result,
                                    Either::Right((Ok(()), _)) => {
                                        return Err(await_timeout_err(here, s, &observations));
                                    }
                                    Either::Right((Err(error), _)) => {
                                        return Err(StepError::transport(here, error));
                                    }
                                }
                            };
                            if let Some(t) = evidence_result? {
                                tail = Some(t);
                            }
                            predicate_matches(&ctx.scope, &evidence.when)
                        }
                    };
                    if confirmed {
                        return Err(condition_rejected_err(here, s));
                    }
                }
                {
                    let on_each_path = format!("{here}.onEach");
                    let on_each = walk_steps(ctx, &s.on_each, &on_each_path, top_next);
                    futures_util::pin_mut!(on_each);
                    match select(on_each, &mut budget).await {
                        Either::Left((result, _)) => {
                            if let Some(t) = result? {
                                tail = Some(t);
                            }
                        }
                        Either::Right((Ok(()), _)) => {
                            return Err(await_timeout_err(here, s, &observations));
                        }
                        Either::Right((Err(error), _)) => {
                            return Err(StepError::transport(here, error));
                        }
                    }
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
            let mut budget = transport.sleep(s.timeout_ms).fuse();
            let mut tail = None;
            for _ in 0..MAX_LOOP_ITERS {
                let read = transport.read(gatt.clone());
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
                    let confirmed = match &s.failure_evidence {
                        None => true,
                        Some(evidence) => {
                            ctx.scope.remove(&evidence.when.field);
                            ctx.encodings.remove(&evidence.when.field);
                            let evidence_path = format!("{here}.failureEvidence.steps");
                            let evidence_result = {
                                let evidence_walk =
                                    walk_steps(ctx, &evidence.steps, &evidence_path, top_next);
                                futures_util::pin_mut!(evidence_walk);
                                match select(evidence_walk, &mut budget).await {
                                    Either::Left((result, _)) => result,
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
                            };
                            if let Some(t) = evidence_result? {
                                tail = Some(t);
                            }
                            predicate_matches(&ctx.scope, &evidence.when)
                        }
                    };
                    if confirmed {
                        return Err(condition_rejected_err(here, s));
                    }
                }
                // Not yet: act, then observe again after the poll interval.
                {
                    let on_each_path = format!("{here}.onEach");
                    let on_each = walk_steps(ctx, &s.on_each, &on_each_path, top_next);
                    futures_util::pin_mut!(on_each);
                    match select(on_each, &mut budget).await {
                        Either::Left((result, _)) => {
                            if let Some(t) = result? {
                                tail = Some(t);
                            }
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
                let pause = transport.sleep(interval);
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
    s.fail_when
        .as_ref()
        .is_some_and(|predicate| predicate_matches(scope, predicate))
}

fn predicate_matches(scope: &BTreeMap<String, String>, predicate: &Predicate) -> bool {
    scope
        .get(&predicate.field)
        .is_some_and(|actual| predicate_holds(actual, predicate.op, &predicate.value))
}

fn condition_rejected_err(here: &str, s: &BleAwaitUntilStep) -> StepError {
    let predicate = s
        .fail_when
        .as_ref()
        .expect("called only after failWhen matched");
    let evidence = s
        .failure_evidence
        .as_ref()
        .map_or(String::new(), |evidence| {
            format!(
                "; `failureEvidence.when` matched ({} {} {})",
                evidence.when.field,
                evidence.when.op.as_token(),
                evidence.when.value
            )
        });
    StepError::condition_rejected(
        here,
        format!(
            "`failWhen` matched ({} {} {}){}",
            predicate.field,
            predicate.op.as_token(),
            predicate.value,
            evidence
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
    transport: &Arc<dyn BleExecutorTransport>,
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

    deadline(transport, DEFAULT_OP_TIMEOUT_MS, "write", async {
        transport.write(s.gatt.clone(), frame).await
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

/// USB analog of [`deadline`] (§11.29): race one USB transfer against the
/// host clock. `UsbTransportError` folds into the shared vocabulary via its
/// `From` impl, which keeps the timeout → deadline-exceeded classification.
async fn usb_deadline<T>(
    transport: &Arc<dyn UsbExecutorTransport>,
    ms: u32,
    what: &str,
    io: impl std::future::Future<Output = Result<T, UsbTransportError>>,
) -> Result<T, OperationFailure> {
    futures_util::pin_mut!(io);
    let clock = async { transport.sleep(ms).await.map_err(TransportError::from) };
    futures_util::pin_mut!(clock);
    match select(io, clock).await {
        Either::Left((res, _)) => res.map_err(|error| OperationFailure::transport(error.into())),
        Either::Right((Ok(()), _)) => Err(OperationFailure::deadline(what, ms)),
        Either::Right((Err(error), _)) => Err(OperationFailure::transport(error)),
    }
}

/// The BLE transport for a BLE-only verb, or the typed transport-mismatch
/// failure the verb raises on a raw USB walk (§11.29).
fn ble_transport<'a>(
    ctx: &ExecCtx<'a>,
    here: &str,
    verb: &'static str,
) -> Result<&'a Arc<dyn BleExecutorTransport>, StepError> {
    ctx.transport
        .ble()
        .ok_or_else(|| StepError::unsupported_verb(here, verb, "USB"))
}

/// The USB transport for a USB verb (§11.29). USB verbs never load into BLE
/// plans, so a BLE walk reaching one is a loader escape, not a plan shape to
/// support.
fn usb_transport<'a>(
    ctx: &ExecCtx<'a>,
    here: &str,
) -> Result<&'a Arc<dyn UsbExecutorTransport>, StepError> {
    ctx.transport.usb().ok_or_else(|| {
        StepError::other(
            here,
            "USB establishment verbs do not run on the BLE executor".into(),
        )
    })
}

/// The scope slot an `acquire` delegate binds its result to — the delegate's
/// own explicit `capture_as` (#44).
fn primary_capture_name(step: &Step) -> Option<&str> {
    match step {
        Step::BleRead(s) => Some(&s.capture_as),
        Step::BlePeripheralName(s) => Some(&s.capture_as),
        Step::BleNotify(s) => s.capture_as.as_deref(),
        Step::BleAwaitUntil(s) => s.capture_as.as_deref(),
        _ => None,
    }
}

fn bind_nikon_connection_configuration(
    ctx: &mut ExecCtx<'_>,
    step: &camera_config::index::NikonLssReadConnectionConfigurationStep,
    config: NikonConnectionConfiguration,
) {
    for name in [
        &step.ssid_capture_as,
        &step.password_capture_as,
        &step.security_mode_capture_as,
    ] {
        ctx.scope.remove(name);
        ctx.encodings.remove(name);
    }
    if let Some(name) = &step.spp_max_length_capture_as {
        ctx.scope.remove(name);
        ctx.encodings.remove(name);
    }
    ctx.scope
        .insert(step.flags_capture_as.clone(), config.flags.to_string());
    ctx.encodings
        .insert(step.flags_capture_as.clone(), Encoding::U8);
    if let Some(wifi) = config.wifi {
        ctx.scope.insert(step.ssid_capture_as.clone(), wifi.ssid);
        ctx.encodings
            .insert(step.ssid_capture_as.clone(), Encoding::Utf8);
        ctx.scope
            .insert(step.password_capture_as.clone(), wifi.password);
        ctx.encodings
            .insert(step.password_capture_as.clone(), Encoding::Utf8);
        ctx.scope.insert(
            step.security_mode_capture_as.clone(),
            wifi.security.as_token().to_string(),
        );
        ctx.encodings
            .insert(step.security_mode_capture_as.clone(), Encoding::Utf8);
    }
    if let (Some(name), Some(length)) = (&step.spp_max_length_capture_as, config.spp_maximum_length)
    {
        ctx.scope.insert(name.clone(), length.to_string());
        ctx.encodings.insert(name.clone(), Encoding::U32Le);
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
        AcquireFirmwareStep, AwaitSource, BleAwaitDisconnectStep, BleAwaitUntilStep, BleDelayStep,
        BlePeripheralNameStep, BleReadStep, BleRequestMtuStep, BleWriteStep, CccdMode, IfStep,
        NotifyCapture, Predicate, RetryFailureKind, RetryStep, StepConfirmation, StepOptions,
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
        /// Fire only the clock with this exact duration. This lets tests keep
        /// nested operation deadlines frozen while the enclosing await lapses.
        sleep_fire_ms: Option<u32>,
        /// The platform peripheral name served to `blePeripheralName` (§11.4b).
        /// Empty models a stack that cannot supply one.
        peripheral_name: String,
        /// Fail the MTU request itself (a GATT error or timeout), as distinct
        /// from negotiating below a manifest floor (#449).
        request_mtu_fails: bool,
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
            if self.request_mtu_fails {
                return Err(TransportError::Failed {
                    detail: "requestMtu rejected by the GATT stack".to_string(),
                });
            }
            Ok(158)
        }
        async fn ensure_services_discovered(&self) -> Result<(), TransportError> {
            Ok(())
        }
        async fn read(&self, _characteristic: String) -> Result<Vec<u8>, TransportError> {
            let io = self.reads.lock().unwrap().pop_front();
            self.play(io).await
        }
        async fn peripheral_name(&self) -> Result<String, TransportError> {
            Ok(self.peripheral_name.clone())
        }
        async fn write(
            &self,
            _characteristic: String,
            _value: Vec<u8>,
        ) -> Result<(), TransportError> {
            Ok(())
        }
        async fn write_with_notification_fence(
            &self,
            _characteristic: String,
            _value: Vec<u8>,
            _notification_characteristic: String,
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
            if self.sleeps_fire || self.sleep_fire_ms == Some(ms) {
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

    #[derive(Default)]
    struct ActivityRecorder(Mutex<Vec<ConnectionActivityEvent>>);
    impl ConnectionActivityObserver for ActivityRecorder {
        fn on_activity(&self, event: ConnectionActivityEvent) {
            self.0.lock().unwrap().push(event);
        }
    }

    fn activity_descriptor(id: &str, end_step_exclusive: u32) -> ConfigActivityDescriptor {
        ConfigActivityDescriptor {
            id: id.into(),
            version: 1,
            display_role: camera_config::ConnectionActivityDisplayRole::Connecting,
            default_expected_duration_ms: 1000,
            interaction_required: false,
            optional: false,
            binding: ConfigActivityBinding::ExecutorSpan(camera_config::ExecutorSpanBinding {
                executor_span: camera_config::ConnectionActivityExecutorSpan {
                    sequence: ConfigActivitySequence::Steps,
                    start_step: 0,
                    end_step_exclusive,
                },
            }),
        }
    }

    fn no_retry_summary() -> ConnectionActivityTerminalSummary {
        ConnectionActivityTerminalSummary {
            retry_count: 0,
            last_retry: None,
        }
    }

    fn context_free_retry(ordinal: u32, limit: u32) -> ConnectionActivityRetry {
        ConnectionActivityRetry {
            ordinal,
            limit,
            failure: ConnectionActivityFailure::without_context(ExecutorStepFailureKind::Other),
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
            transport: ExecTransport::Ble(transport),
            observer,
            activity_observer: None,
            active_activity: None,
            scope: BTreeMap::new(),
            encodings: BTreeMap::new(),
            runtime_params: BTreeMap::new(),
            subscriptions: BTreeSet::new(),
            nikon_lss_session: None,
            steps_run: 0,
            summary: NativeEstablishmentWalkSummary::default(),
            refine: None,
            usb_interfaces: BTreeMap::new(),
            usb_interface_claimed: false,
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
                seed_read: false,
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
            failure_evidence: None,
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
    fn manifest_delay_uses_transport_clock_once() {
        let sleep_log = Arc::new(Mutex::new(Vec::new()));
        let (transport, _recorder, observer) = harness(MockTransport {
            sleeps_fire: true,
            sleep_log: sleep_log.clone(),
            ..Default::default()
        });
        let mut context = ctx(&transport, &observer);
        block_on(walk_plan(
            &mut context,
            vec![Step::BleDelay(BleDelayStep {
                duration_ms: 600,
                opts: StepOptions::default(),
            })],
        ))
        .unwrap();
        assert_eq!(*sleep_log.lock().unwrap(), vec![600]);
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
        let activity_recorder = Arc::new(ActivityRecorder::default());
        let activity_observer: Arc<dyn ConnectionActivityObserver> = activity_recorder.clone();
        let mut ctx = ctx(&transport, &observer);
        ctx.activity_observer = Some(&activity_observer);
        let steps = vec![read_step(
            "v",
            StepOptions {
                tolerant: false,
                retries: 2,
                retry_delay_ms: 7,
                confirms: None,
            },
        )];
        block_on(walk_plan_with_activities(
            &mut ctx,
            steps,
            vec![activity_descriptor("camera.test.retry", 1)],
            ConfigActivitySequence::Steps,
        ))
        .expect("third attempt succeeds");
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
        assert_eq!(
            *activity_recorder.0.lock().unwrap(),
            vec![
                ConnectionActivityEvent::Started {
                    id: "camera.test.retry".into(),
                    version: 1,
                },
                ConnectionActivityEvent::Retrying {
                    id: "camera.test.retry".into(),
                    version: 1,
                    retry: context_free_retry(2, 3),
                },
                ConnectionActivityEvent::Retrying {
                    id: "camera.test.retry".into(),
                    version: 1,
                    retry: context_free_retry(3, 3),
                },
                ConnectionActivityEvent::Succeeded {
                    id: "camera.test.retry".into(),
                    version: 1,
                    summary: ConnectionActivityTerminalSummary {
                        retry_count: 2,
                        last_retry: Some(context_free_retry(3, 3)),
                    },
                },
            ]
        );
    }

    #[test]
    fn terminal_retry_count_spans_local_ordinal_resets() {
        let (transport, _recorder, observer) = harness(MockTransport {
            reads: Mutex::new(VecDeque::from([
                Io::Fail("first retry"),
                Io::Value(b"one".to_vec()),
                Io::Fail("second retry"),
                Io::Value(b"two".to_vec()),
            ])),
            sleeps_fire: true,
            ..Default::default()
        });
        let activity_recorder = Arc::new(ActivityRecorder::default());
        let activity_observer: Arc<dyn ConnectionActivityObserver> = activity_recorder.clone();
        let mut ctx = ctx(&transport, &observer);
        ctx.activity_observer = Some(&activity_observer);
        let retry_once = StepOptions {
            tolerant: false,
            retries: 1,
            retry_delay_ms: 0,
            confirms: None,
        };

        block_on(walk_plan_with_activities(
            &mut ctx,
            vec![
                read_step("first", retry_once.clone()),
                read_step("second", retry_once),
            ],
            vec![activity_descriptor("camera.test.two-retries", 2)],
            ConfigActivitySequence::Steps,
        ))
        .expect("both local retry ladders recover");

        assert_eq!(
            *activity_recorder.0.lock().unwrap(),
            vec![
                ConnectionActivityEvent::Started {
                    id: "camera.test.two-retries".into(),
                    version: 1,
                },
                ConnectionActivityEvent::Retrying {
                    id: "camera.test.two-retries".into(),
                    version: 1,
                    retry: context_free_retry(2, 2),
                },
                ConnectionActivityEvent::Retrying {
                    id: "camera.test.two-retries".into(),
                    version: 1,
                    retry: context_free_retry(2, 2),
                },
                ConnectionActivityEvent::Succeeded {
                    id: "camera.test.two-retries".into(),
                    version: 1,
                    summary: ConnectionActivityTerminalSummary {
                        retry_count: 2,
                        last_retry: Some(context_free_retry(2, 2)),
                    },
                },
            ]
        );
    }

    struct SyntheticTailResolver;

    impl NativeRefinementResolver for SyntheticTailResolver {
        fn refine(
            &self,
            _plan_handle: String,
            _firmware: String,
            _scope: Vec<KeyValue>,
            next_step_index: u32,
        ) -> Result<crate::NativeEstablishmentRefinement, crate::EstablishmentError> {
            assert_eq!(next_step_index, 1);
            Ok(crate::NativeEstablishmentRefinement::ReplaceTail {
                steps: vec![
                    read_step("refinedFirst", StepOptions::default()),
                    read_step("refinedSecond", StepOptions::default()),
                ],
                activities: vec![activity_descriptor("camera.test.refined", 2)],
            })
        }
    }

    struct ContinuingTailResolver;

    impl NativeRefinementResolver for ContinuingTailResolver {
        fn refine(
            &self,
            _plan_handle: String,
            _firmware: String,
            _scope: Vec<KeyValue>,
            next_step_index: u32,
        ) -> Result<crate::NativeEstablishmentRefinement, crate::EstablishmentError> {
            assert_eq!(next_step_index, 1);
            Ok(crate::NativeEstablishmentRefinement::ReplaceTail {
                steps: vec![
                    read_step("refinedFirst", StepOptions::default()),
                    read_step("refinedSecond", StepOptions::default()),
                ],
                activities: vec![activity_descriptor("camera.test.continuing", 2)],
            })
        }
    }

    #[test]
    fn refinement_splices_native_steps_and_relative_activity_spans() {
        let (transport, recorder, observer) = harness(MockTransport {
            reads: Mutex::new(VecDeque::from([
                Io::Value(b"2.40".to_vec()),
                Io::Value(b"first".to_vec()),
                Io::Value(b"second".to_vec()),
            ])),
            sleeps_fire: true,
            ..Default::default()
        });
        let activity_recorder = Arc::new(ActivityRecorder::default());
        let activity_observer: Arc<dyn ConnectionActivityObserver> = activity_recorder.clone();
        let resolver = SyntheticTailResolver;
        let mut ctx = ctx(&transport, &observer);
        ctx.activity_observer = Some(&activity_observer);
        ctx.refine = Some(RefineCtx {
            source: RefinementSource::Resolver(&resolver),
            plan_handle: "tm1:test".into(),
        });
        let original = vec![
            Step::AcquireFirmware(AcquireFirmwareStep {
                from: AcquireSource::BleRead {
                    gatt: "FIRMWARE".into(),
                    encoding: Encoding::Utf8,
                },
                opts: StepOptions::default(),
            }),
            read_step("oldFirst", StepOptions::default()),
            read_step("oldSecond", StepOptions::default()),
        ];
        let activities = vec![
            activity_descriptor("camera.test.acquire", 1),
            ConfigActivityDescriptor {
                id: "camera.test.old-tail".into(),
                binding: ConfigActivityBinding::ExecutorSpan(camera_config::ExecutorSpanBinding {
                    executor_span: camera_config::ConnectionActivityExecutorSpan {
                        sequence: ConfigActivitySequence::Steps,
                        start_step: 1,
                        end_step_exclusive: 3,
                    },
                }),
                ..activity_descriptor("camera.test.old-tail", 3)
            },
        ];

        block_on(walk_plan_with_activities(
            &mut ctx,
            original,
            activities,
            ConfigActivitySequence::Steps,
        ))
        .expect("the native replacement tail runs");

        assert_eq!(
            ctx.scope.get("refinedFirst").map(String::as_str),
            Some("first")
        );
        assert_eq!(
            ctx.scope.get("refinedSecond").map(String::as_str),
            Some("second")
        );
        assert!(!ctx.scope.contains_key("oldFirst"));
        assert_eq!(recorder.0.lock().unwrap().len(), 6);
        assert_eq!(
            *activity_recorder.0.lock().unwrap(),
            vec![
                ConnectionActivityEvent::Started {
                    id: "camera.test.acquire".into(),
                    version: 1,
                },
                ConnectionActivityEvent::Succeeded {
                    id: "camera.test.acquire".into(),
                    version: 1,
                    summary: no_retry_summary(),
                },
                ConnectionActivityEvent::Started {
                    id: "camera.test.refined".into(),
                    version: 1,
                },
                ConnectionActivityEvent::Succeeded {
                    id: "camera.test.refined".into(),
                    version: 1,
                    summary: no_retry_summary(),
                },
            ]
        );
    }

    #[test]
    fn refinement_keeps_a_matching_activity_alive_across_the_splice() {
        let (transport, recorder, observer) = harness(MockTransport {
            reads: Mutex::new(VecDeque::from([
                Io::Value(b"2.40".to_vec()),
                Io::Value(b"first".to_vec()),
                Io::Value(b"second".to_vec()),
            ])),
            sleeps_fire: true,
            ..Default::default()
        });
        let activity_recorder = Arc::new(ActivityRecorder::default());
        let activity_observer: Arc<dyn ConnectionActivityObserver> = activity_recorder.clone();
        let resolver = ContinuingTailResolver;
        let mut ctx = ctx(&transport, &observer);
        ctx.activity_observer = Some(&activity_observer);
        ctx.refine = Some(RefineCtx {
            source: RefinementSource::Resolver(&resolver),
            plan_handle: "tm1:test".into(),
        });
        let original = vec![
            Step::AcquireFirmware(AcquireFirmwareStep {
                from: AcquireSource::BleRead {
                    gatt: "FIRMWARE".into(),
                    encoding: Encoding::Utf8,
                },
                opts: StepOptions::default(),
            }),
            read_step("oldFirst", StepOptions::default()),
            read_step("oldSecond", StepOptions::default()),
        ];

        block_on(walk_plan_with_activities(
            &mut ctx,
            original,
            vec![activity_descriptor("camera.test.continuing", 3)],
            ConfigActivitySequence::Steps,
        ))
        .expect("the continued replacement tail runs");

        assert_eq!(recorder.0.lock().unwrap().len(), 6);
        assert!(recorder
            .0
            .lock()
            .unwrap()
            .iter()
            .all(|report| report.activity_id.as_deref() == Some("camera.test.continuing")));
        assert_eq!(
            *activity_recorder.0.lock().unwrap(),
            vec![
                ConnectionActivityEvent::Started {
                    id: "camera.test.continuing".into(),
                    version: 1,
                },
                ConnectionActivityEvent::Succeeded {
                    id: "camera.test.continuing".into(),
                    version: 1,
                    summary: no_retry_summary(),
                },
            ]
        );
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
                    confirms: None,
                },
            ),
            Step::BleWrite(BleWriteStep {
                gatt: "BBBB".into(),
                value: StepValue::Literal {
                    literal: serde_yaml::Value::String("01".into()),
                },
                notification_fence: None,
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
    fn establishment_summary_reproduces_withheld_anchor_and_all_outcomes() {
        let (transport, _recorder, observer) = harness(MockTransport {
            reads: Mutex::new(VecDeque::from([
                Io::Fail("optional characteristic absent"),
                Io::Fail("anchor characteristic absent"),
            ])),
            sleeps_fire: true,
            ..Default::default()
        });
        let mut context = ctx(&transport, &observer);
        let withheld = vec![
            read_step(
                "optional",
                StepOptions {
                    tolerant: true,
                    ..Default::default()
                },
            ),
            read_step(
                "anchor",
                StepOptions {
                    tolerant: true,
                    confirms: Some(StepConfirmation::Registration),
                    ..Default::default()
                },
            ),
        ];
        context.summary = NativeEstablishmentWalkSummary::for_steps(&withheld);
        block_on(walk_plan(&mut context, withheld))
            .expect("withholding a tolerant anchor does not abort the walk");
        assert_eq!(
            context.summary.confirm_outcome,
            NativeEstablishmentConfirmOutcome::Unsatisfied
        );
        assert_eq!(
            context.summary.tolerated_step_paths,
            vec!["steps[0].bleRead", "steps[1].bleRead"]
        );

        let (transport, _recorder, observer) = harness(MockTransport {
            reads: Mutex::new(VecDeque::from([Io::Value(b"confirmed".to_vec())])),
            sleeps_fire: true,
            ..Default::default()
        });
        let mut context = ctx(&transport, &observer);
        let satisfied = vec![read_step(
            "anchor",
            StepOptions {
                confirms: Some(StepConfirmation::Registration),
                ..Default::default()
            },
        )];
        context.summary = NativeEstablishmentWalkSummary::for_steps(&satisfied);
        block_on(walk_plan(&mut context, satisfied)).expect("anchor succeeds");
        assert_eq!(
            context.summary.confirm_outcome,
            NativeEstablishmentConfirmOutcome::Satisfied
        );
        assert!(context.summary.tolerated_step_paths.is_empty());

        let (transport, _recorder, observer) = harness(MockTransport {
            sleeps_fire: true,
            ..Default::default()
        });
        let mut context = ctx(&transport, &observer);
        context.scope.insert("style".into(), "legacy".into());
        let skipped = vec![Step::If(IfStep {
            condition: Predicate {
                field: "style".into(),
                op: PredicateOp::Eq,
                value: "red".into(),
            },
            then: vec![read_step(
                "anchor",
                StepOptions {
                    confirms: Some(StepConfirmation::Registration),
                    ..Default::default()
                },
            )],
            else_branch: vec![],
            tolerant: false,
        })];
        context.summary = NativeEstablishmentWalkSummary::for_steps(&skipped);
        block_on(walk_plan(&mut context, skipped)).expect("untaken anchor branch is a valid walk");
        assert_eq!(
            context.summary.confirm_outcome,
            NativeEstablishmentConfirmOutcome::Unsatisfied
        );

        let (transport, _recorder, observer) = harness(MockTransport {
            reads: Mutex::new(VecDeque::from([Io::Value(b"ordinary".to_vec())])),
            sleeps_fire: true,
            ..Default::default()
        });
        let mut context = ctx(&transport, &observer);
        let unmarked = vec![read_step("ordinary", StepOptions::default())];
        context.summary = NativeEstablishmentWalkSummary::for_steps(&unmarked);
        block_on(walk_plan(&mut context, unmarked)).expect("ordinary walk succeeds");
        assert_eq!(
            context.summary.confirm_outcome,
            NativeEstablishmentConfirmOutcome::NotDeclared
        );
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
        let (transport, recorder, observer) = harness(MockTransport {
            reads: Mutex::new(VecDeque::from([Io::Stall])),
            // Frozen clock: nothing resolves; the walk parks on the read.
            sleeps_fire: false,
            dropped_inflight: flag.clone(),
            ..Default::default()
        });
        let activity_recorder = Arc::new(ActivityRecorder::default());
        let activity_observer: Arc<dyn ConnectionActivityObserver> = activity_recorder.clone();
        let mut ctx = ctx(&transport, &observer);
        ctx.activity_observer = Some(&activity_observer);
        let steps = vec![read_step("v", StepOptions::default())];
        {
            let mut fut = Box::pin(walk_plan_with_activities(
                &mut ctx,
                steps,
                vec![activity_descriptor("camera.test.cancel", 1)],
                ConfigActivitySequence::Steps,
            ));
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
        drop(ctx);
        assert_eq!(
            *activity_recorder.0.lock().unwrap(),
            vec![
                ConnectionActivityEvent::Started {
                    id: "camera.test.cancel".into(),
                    version: 1,
                },
                ConnectionActivityEvent::Cancelled {
                    id: "camera.test.cancel".into(),
                    version: 1,
                    summary: no_retry_summary(),
                },
            ],
            "cancellation emits one terminal event"
        );
        let reports = recorder.0.lock().unwrap();
        assert_eq!(reports.len(), 2, "raw Started also receives one terminal");
        assert_eq!(reports[1].outcome, StepOutcome::Failed);
        assert_eq!(reports[1].attempts, 0);
        assert_eq!(
            reports[1].error.as_deref(),
            Some("step cancelled before terminal outcome")
        );
    }

    #[test]
    fn cancellation_preserves_a_retry_already_in_progress() {
        let (transport, recorder, observer) = harness(MockTransport {
            reads: Mutex::new(VecDeque::from([Io::Fail("retry me")])),
            // The retry transition is emitted before this pending backoff.
            sleeps_fire: false,
            ..Default::default()
        });
        let activity_recorder = Arc::new(ActivityRecorder::default());
        let activity_observer: Arc<dyn ConnectionActivityObserver> = activity_recorder.clone();
        let mut ctx = ctx(&transport, &observer);
        ctx.activity_observer = Some(&activity_observer);
        {
            let mut future = Box::pin(walk_plan_with_activities(
                &mut ctx,
                vec![read_step(
                    "v",
                    StepOptions {
                        tolerant: false,
                        retries: 1,
                        retry_delay_ms: 7,
                        confirms: None,
                    },
                )],
                vec![activity_descriptor("camera.test.cancel-retry", 1)],
                ConfigActivitySequence::Steps,
            ));
            let waker = futures::task::noop_waker();
            let mut poll_ctx = Context::from_waker(&waker);
            assert!(matches!(future.as_mut().poll(&mut poll_ctx), Poll::Pending));
        }
        drop(ctx);

        assert_eq!(
            *activity_recorder.0.lock().unwrap(),
            vec![
                ConnectionActivityEvent::Started {
                    id: "camera.test.cancel-retry".into(),
                    version: 1,
                },
                ConnectionActivityEvent::Retrying {
                    id: "camera.test.cancel-retry".into(),
                    version: 1,
                    retry: context_free_retry(2, 2),
                },
                ConnectionActivityEvent::Cancelled {
                    id: "camera.test.cancel-retry".into(),
                    version: 1,
                    summary: ConnectionActivityTerminalSummary {
                        retry_count: 1,
                        last_retry: Some(context_free_retry(2, 2)),
                    },
                },
            ]
        );
        let reports = recorder.0.lock().unwrap();
        assert_eq!(reports.len(), 2, "raw Started also receives one terminal");
        assert_eq!(reports[1].outcome, StepOutcome::Failed);
        assert_eq!(reports[1].attempts, 1);
        assert_eq!(
            reports[1].error.as_deref(),
            Some("step cancelled before terminal outcome")
        );
    }

    #[test]
    fn control_retry_cancellation_reports_the_committed_retry() {
        let (transport, recorder, observer) = harness(MockTransport {
            reads: Mutex::new(VecDeque::from([Io::Value(vec![0x02, 0x00])])),
            notifications: Mutex::new(VecDeque::from([Io::Value(vec![0x00, 0x80])])),
            // The manifest retry transition is emitted before this pending backoff.
            sleeps_fire: false,
            ..Default::default()
        });
        let activity_recorder = Arc::new(ActivityRecorder::default());
        let activity_observer: Arc<dyn ConnectionActivityObserver> = activity_recorder.clone();
        let mut ctx = ctx(&transport, &observer);
        ctx.activity_observer = Some(&activity_observer);
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
        {
            let mut future = Box::pin(walk_plan_with_activities(
                &mut ctx,
                vec![retry],
                vec![activity_descriptor("camera.test.control-retry", 1)],
                ConfigActivitySequence::Steps,
            ));
            let waker = futures::task::noop_waker();
            let mut poll_ctx = Context::from_waker(&waker);
            assert!(matches!(future.as_mut().poll(&mut poll_ctx), Poll::Pending));
        }
        drop(ctx);

        let reports = recorder.0.lock().unwrap();
        let retry_reports: Vec<&StepReport> = reports
            .iter()
            .filter(|report| report.verb == "retry")
            .collect();
        assert_eq!(retry_reports.len(), 2);
        assert_eq!(retry_reports[0].outcome, StepOutcome::Started);
        assert_eq!(retry_reports[1].outcome, StepOutcome::Failed);
        assert_eq!(retry_reports[1].attempts, 1);
        assert_eq!(
            retry_reports[1].error.as_deref(),
            Some("step cancelled before terminal outcome")
        );
        assert!(matches!(
            activity_recorder.0.lock().unwrap().as_slice(),
            [
                ConnectionActivityEvent::Started { .. },
                ConnectionActivityEvent::Retrying { .. },
                ConnectionActivityEvent::Cancelled {
                    summary: ConnectionActivityTerminalSummary { retry_count: 1, .. },
                    ..
                }
            ]
        ));
    }

    #[test]
    fn mtu_below_requirement_fails_the_checkpoint() {
        let (transport, _recorder, observer) = harness(MockTransport {
            sleeps_fire: true,
            ..Default::default()
        });
        let mut ctx = ctx(&transport, &observer);
        let steps = vec![Step::BleRequestMtu(BleRequestMtuStep {
            requested_mtu: 517,
            minimum_mtu: Some(517),
            opts: StepOptions::default(),
        })];
        let err = block_on(walk_plan(&mut ctx, steps)).expect_err("negotiated 158 < 517");
        assert_eq!(err.kind, ExecutorStepFailureKind::Other);
        assert!(err.message.contains("negotiated MTU 158"));
    }

    #[test]
    fn mtu_without_floor_succeeds_below_the_request_target() {
        // §11.4a: with no evidenced floor, any negotiated MTU passes the
        // checkpoint (the X-A7 pairs at 185 against a 515 target).
        let (transport, _recorder, observer) = harness(MockTransport {
            sleeps_fire: true,
            ..Default::default()
        });
        let mut ctx = ctx(&transport, &observer);
        let steps = vec![Step::BleRequestMtu(BleRequestMtuStep {
            requested_mtu: 517,
            minimum_mtu: None,
            opts: StepOptions::default(),
        })];
        block_on(walk_plan(&mut ctx, steps)).expect("negotiated 158 with no floor must succeed");
    }

    #[test]
    fn tolerant_mtu_step_absorbs_a_failed_request_call() {
        // legacy manufacturer app's onMtuChanged ignores the callback status, so a
        // failed requestMtu call must not block registration (#449):
        // tolerance absorbs the call error itself, not just an unmet floor.
        let (transport, recorder, observer) = harness(MockTransport {
            sleeps_fire: true,
            request_mtu_fails: true,
            ..Default::default()
        });
        let mut ctx = ctx(&transport, &observer);
        let steps = vec![Step::BleRequestMtu(BleRequestMtuStep {
            requested_mtu: 517,
            minimum_mtu: None,
            opts: StepOptions {
                tolerant: true,
                ..StepOptions::default()
            },
        })];
        block_on(walk_plan(&mut ctx, steps)).expect("tolerance absorbs the call failure");

        let reports = recorder.0.lock().unwrap();
        let outcomes: Vec<StepOutcome> = reports.iter().map(|r| r.outcome).collect();
        assert_eq!(outcomes, vec![StepOutcome::Started, StepOutcome::Tolerated]);
        // The tolerated error is the scripted call failure, provably not the
        // executor's own deadline firing (a pend would report a timeout).
        assert!(reports[1]
            .error
            .as_deref()
            .expect("tolerated report carries the error")
            .contains("requestMtu rejected by the GATT stack"));
    }

    #[test]
    fn strict_mtu_step_fails_on_a_failed_request_call() {
        let (transport, _recorder, observer) = harness(MockTransport {
            sleeps_fire: true,
            request_mtu_fails: true,
            ..Default::default()
        });
        let mut ctx = ctx(&transport, &observer);
        let steps = vec![Step::BleRequestMtu(BleRequestMtuStep {
            requested_mtu: 517,
            minimum_mtu: None,
            opts: StepOptions::default(),
        })];
        let err = block_on(walk_plan(&mut ctx, steps))
            .expect_err("a failed request call fails a strict step");
        assert_eq!(err.kind, ExecutorStepFailureKind::Other);
        assert!(err
            .message
            .contains("requestMtu rejected by the GATT stack"));
    }

    #[test]
    fn peripheral_name_binds_scope_from_the_platform_surface() {
        let (transport, _recorder, observer) = harness(MockTransport {
            sleeps_fire: true,
            peripheral_name: "TEST-PERIPHERAL".into(),
            ..Default::default()
        });
        let mut ctx = ctx(&transport, &observer);
        let steps = vec![Step::BlePeripheralName(BlePeripheralNameStep {
            capture_as: "cameraName".into(),
            opts: StepOptions::default(),
        })];
        block_on(walk_plan(&mut ctx, steps)).expect("mock transport serves a name");
        assert_eq!(
            ctx.scope.get("cameraName").map(String::as_str),
            Some("TEST-PERIPHERAL")
        );
    }

    #[test]
    fn peripheral_name_strips_the_nul_terminator() {
        // GAP-exposing hosts may satisfy the step with the raw 0x2A00 read,
        // which is NUL-terminated; the bound value must not carry it (#444
        // review).
        let (transport, _recorder, observer) = harness(MockTransport {
            sleeps_fire: true,
            peripheral_name: "TEST-PERIPHERAL\0".into(),
            ..Default::default()
        });
        let mut ctx = ctx(&transport, &observer);
        let steps = vec![Step::BlePeripheralName(BlePeripheralNameStep {
            capture_as: "cameraName".into(),
            opts: StepOptions::default(),
        })];
        block_on(walk_plan(&mut ctx, steps)).expect("a NUL-terminated name binds trimmed");
        assert_eq!(
            ctx.scope.get("cameraName").map(String::as_str),
            Some("TEST-PERIPHERAL")
        );
    }

    #[test]
    fn empty_peripheral_name_fails_instead_of_binding_empty() {
        // CBPeripheral.name is optional; an unavailable name is a step
        // failure, never a silently empty capture (#444 review).
        let (transport, _recorder, observer) = harness(MockTransport {
            sleeps_fire: true,
            peripheral_name: String::new(),
            ..Default::default()
        });
        let mut ctx = ctx(&transport, &observer);
        let steps = vec![Step::BlePeripheralName(BlePeripheralNameStep {
            capture_as: "cameraName".into(),
            opts: StepOptions::default(),
        })];
        let err = block_on(walk_plan(&mut ctx, steps)).expect_err("empty name must fail");
        assert!(err.message.contains("peripheral name unavailable"));
        assert!(!ctx.scope.contains_key("cameraName"));
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
            reads: Mutex::new(VecDeque::from([Io::Value(vec![0x02, 0x00])])),
            notifications: Mutex::new(VecDeque::from([
                Io::Value(vec![0x00, 0x80]),
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

        block_on(walk_plan(&mut ctx, vec![retry]))
            .expect("the second attempt's notification succeeds");
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
    fn seeded_notify_runs_on_each_before_waiting_for_a_notification() {
        let mut step = match rejection_await() {
            Step::BleAwaitUntil(step) => step,
            _ => unreachable!(),
        };
        let AwaitSource::Notify { seed_read, .. } = &mut step.source else {
            unreachable!()
        };
        *seed_read = true;
        step.fail_when = None;
        step.on_each = vec![read_step("onEachResult", StepOptions::default())];
        let transport = MockTransport {
            reads: Mutex::new(VecDeque::from([
                Io::Value(vec![0x02, 0x80]),
                Io::Value(b"ran".to_vec()),
            ])),
            notifications: Mutex::new(VecDeque::from([Io::Value(vec![0x01, 0x80])])),
            ..Default::default()
        };
        let (transport, _recorder, observer) = harness(transport);
        let mut ctx = ctx(&transport, &observer);

        block_on(walk_plan(&mut ctx, vec![Step::BleAwaitUntil(step)]))
            .expect("the seed runs onEach before the launched notification");
        assert_eq!(ctx.scope.get("apState").map(String::as_str), Some("32769"));
        assert_eq!(
            ctx.scope.get("onEachResult").map(String::as_str),
            Some("ran")
        );
    }

    #[test]
    fn notify_on_each_cannot_outlive_the_await_budget() {
        let mut step = match rejection_await() {
            Step::BleAwaitUntil(step) => step,
            _ => unreachable!(),
        };
        step.fail_when = None;
        step.on_each = vec![read_step("onEachResult", StepOptions::default())];
        let transport = MockTransport {
            reads: Mutex::new(VecDeque::from([Io::Stall])),
            notifications: Mutex::new(VecDeque::from([Io::Value(vec![0x02, 0x80])])),
            sleep_fire_ms: Some(step.timeout_ms),
            ..Default::default()
        };
        let dropped = transport.dropped_inflight.clone();
        let (transport, recorder, observer) = harness(transport);
        let mut ctx = ctx(&transport, &observer);

        let error = block_on(walk_plan(&mut ctx, vec![Step::BleAwaitUntil(step)]))
            .expect_err("the enclosing await deadline cancels stalled onEach work");
        assert_eq!(error.kind, ExecutorStepFailureKind::DeadlineExceeded);
        assert!(
            dropped.load(Ordering::SeqCst),
            "stalled onEach was cancelled"
        );
        let reports = recorder.0.lock().unwrap();
        let nested: Vec<StepOutcome> = reports
            .iter()
            .filter(|report| report.step_path.contains(".onEach[0]."))
            .map(|report| report.outcome)
            .collect();
        assert_eq!(nested, [StepOutcome::Started, StepOutcome::Failed]);
    }

    #[test]
    fn read_on_each_cannot_outlive_the_await_budget() {
        let mut step = match rejection_await() {
            Step::BleAwaitUntil(step) => step,
            _ => unreachable!(),
        };
        step.source = AwaitSource::Read {
            gatt: "APSTATE".into(),
        };
        step.fail_when = None;
        step.on_each = vec![read_step("onEachResult", StepOptions::default())];
        let transport = MockTransport {
            reads: Mutex::new(VecDeque::from([Io::Value(vec![0x02, 0x80]), Io::Stall])),
            sleep_fire_ms: Some(step.timeout_ms),
            ..Default::default()
        };
        let dropped = transport.dropped_inflight.clone();
        let (transport, recorder, observer) = harness(transport);
        let mut ctx = ctx(&transport, &observer);

        let error = block_on(walk_plan(&mut ctx, vec![Step::BleAwaitUntil(step)]))
            .expect_err("the enclosing await deadline cancels stalled onEach work");
        assert_eq!(error.kind, ExecutorStepFailureKind::DeadlineExceeded);
        assert!(
            dropped.load(Ordering::SeqCst),
            "stalled onEach was cancelled"
        );
        let reports = recorder.0.lock().unwrap();
        let nested: Vec<StepOutcome> = reports
            .iter()
            .filter(|report| report.step_path.contains(".onEach[0]."))
            .map(|report| report.outcome)
            .collect();
        assert_eq!(nested, [StepOutcome::Started, StepOutcome::Failed]);
    }

    #[test]
    fn read_source_remains_eligible_for_fail_when() {
        let mut step = match rejection_await() {
            Step::BleAwaitUntil(step) => step,
            _ => unreachable!(),
        };
        step.source = AwaitSource::Read {
            gatt: "APSTATE".into(),
        };
        let transport = MockTransport {
            reads: Mutex::new(VecDeque::from([Io::Value(vec![0x00, 0x80])])),
            ..Default::default()
        };
        let (transport, _recorder, observer) = harness(transport);
        let mut ctx = ctx(&transport, &observer);

        let error = block_on(walk_plan(&mut ctx, vec![Step::BleAwaitUntil(step)]))
            .expect_err("a read observation can still match failWhen");
        assert_eq!(error.kind, ExecutorStepFailureKind::ConditionRejected);
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
            reads: Mutex::new(VecDeque::from([Io::Fail("details unavailable")])),
            notifications: Mutex::new(VecDeque::from([Io::Value(vec![0x00, 0x80])])),
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
