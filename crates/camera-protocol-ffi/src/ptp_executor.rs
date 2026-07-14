//! Rust-owned executor for the PTP mode-entry/action grammar (schema §11.24).

use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use camera_config as cc;
use futures_util::future::{select, Either};
use ptp_core::codes::{op, resp};

use crate::executor::ActiveActivity;
use crate::{
    frame_decode, frame_encode, ActionVerb, CaptureInfo, CaptureSourceInfo, ConfigStore,
    ConnectionActivityBinding, ConnectionActivityDescriptor, ConnectionActivityFailure,
    ConnectionActivityObserver, ConnectionActivityRetry, EntryParam, EntryStep,
    ExecutorStepFailureKind, FfiAwaitSource, FfiChunkSize, FfiLoopKind, FfiPredicate, KeyValue,
    PtpFraming, StepObserver, StepOutcome, StepReport, ValueWidth,
};

const DEFAULT_OP_TIMEOUT_MS: u32 = 10_000;
const DEFAULT_STEP_TIMEOUT_MS: u32 = 60_000;
const DEFAULT_POLL_INTERVAL_MS: u32 = 200;
const MAX_COMMAND_FRAMES: usize = 65_536;
const MAX_FOREACH_ITERS: usize = 100_000;
const MAX_CHUNK_ITERS: usize = 4096;

#[derive(Debug, thiserror::Error, uniffi::Error)]
pub enum PtpTransportError {
    #[error("command session is not connected")]
    NotConnected,
    #[error("transport operation timed out: {detail}")]
    Timeout { detail: String },
    #[error("transport failure: {detail}")]
    Failed { detail: String },
}

#[derive(Debug, Clone, uniffi::Record)]
pub struct PtpSessionOpenResult {
    pub transaction_id: u32,
    pub response_code: u16,
    pub response_params: Vec<u32>,
}

/// Raw host-owned PTP/IP I/O. Rust supplies framing, sequencing and policy.
#[uniffi::export(with_foreign)]
#[async_trait::async_trait]
pub trait PtpExecutorTransport: Send + Sync {
    async fn reserve_transaction_id(&self) -> Result<u32, PtpTransportError>;
    async fn send_command_frame(&self, frame: Vec<u8>) -> Result<(), PtpTransportError>;
    async fn next_command_frame(&self) -> Result<Vec<u8>, PtpTransportError>;
    /// Return the next frame for `event_code`, retaining unrelated events for
    /// their normal consumers instead of draining them from the host queue.
    async fn next_event_frame(&self, event_code: u16) -> Result<Vec<u8>, PtpTransportError>;
    /// Close the command channel after flushing the optional resolved sentinel.
    async fn close_command_channel(
        &self,
        transport_close_frame: Option<Vec<u8>>,
    ) -> Result<(), PtpTransportError>;
    /// Recreate command transport, replay cached init identity and OpenSession.
    async fn reopen_command_session(&self) -> Result<PtpSessionOpenResult, PtpTransportError>;
    async fn sleep(&self, ms: u32) -> Result<(), PtpTransportError>;
}

#[derive(Debug, Clone, uniffi::Record)]
pub struct PtpRuntimeValue {
    pub key: String,
    pub value: u64,
}

#[derive(Debug, Clone, uniffi::Record)]
pub struct PtpCollectionValue {
    pub key: String,
    pub values: Vec<u64>,
}

#[derive(Debug, Clone, uniffi::Record)]
pub struct PtpDataOutput {
    pub step_path: String,
    pub operation: u16,
    pub transaction_id: u32,
    pub payload: Vec<u8>,
    pub response_params: Vec<u32>,
}

#[derive(Debug, uniffi::Record)]
pub struct PtpExecutionOutcome {
    pub scope: Vec<PtpRuntimeValue>,
    pub collections: Vec<PtpCollectionValue>,
    pub outputs: Vec<PtpDataOutput>,
    pub steps_run: u32,
}

#[derive(Debug, thiserror::Error, uniffi::Error)]
pub enum PtpExecutorError {
    #[error("unknown plan: {detail}")]
    UnknownPlan { detail: String },
    #[error("unsupported plan: {detail}")]
    UnsupportedPlan { detail: String },
    #[error("{step}: {detail}")]
    StepFailed {
        step: String,
        kind: ExecutorStepFailureKind,
        detail: String,
        context: Vec<KeyValue>,
    },
}

#[uniffi::export]
#[allow(clippy::too_many_arguments)]
pub async fn run_mode_entry(
    store: Arc<ConfigStore>,
    connection: String,
    from: Option<String>,
    to: String,
    transport: Arc<dyn PtpExecutorTransport>,
    observer: Arc<dyn StepObserver>,
    activity_observer: Arc<dyn ConnectionActivityObserver>,
    runtime_params: Vec<PtpRuntimeValue>,
) -> Result<PtpExecutionOutcome, PtpExecutorError> {
    let plan = store
        .mode_entry(connection.clone(), from, to)
        .ok_or_else(|| PtpExecutorError::UnknownPlan {
            detail: format!("{connection}: mode entry not found"),
        })?;
    let crate::ModeEntryExecution::Ptp { steps } = plan.execution else {
        return Err(PtpExecutorError::UnsupportedPlan {
            detail: format!("{connection}: mode entry requires host orchestration"),
        });
    };
    run_steps(
        store,
        connection,
        steps,
        plan.activities,
        transport,
        observer,
        activity_observer,
        runtime_params,
    )
    .await
}

#[uniffi::export]
#[allow(clippy::too_many_arguments)]
pub async fn run_mode_reestablishment_exit(
    store: Arc<ConfigStore>,
    connection: String,
    from: Option<String>,
    to: String,
    transport: Arc<dyn PtpExecutorTransport>,
    observer: Arc<dyn StepObserver>,
    activity_observer: Arc<dyn ConnectionActivityObserver>,
    runtime_params: Vec<PtpRuntimeValue>,
) -> Result<PtpExecutionOutcome, PtpExecutorError> {
    let plan = store
        .mode_entry(connection.clone(), from, to)
        .ok_or_else(|| PtpExecutorError::UnknownPlan {
            detail: format!("{connection}: mode entry not found"),
        })?;
    let crate::ModeEntryExecution::ReestablishConnection { exit_steps, .. } = plan.execution else {
        return Err(PtpExecutorError::UnsupportedPlan {
            detail: format!("{connection}: mode entry is not a re-establishment"),
        });
    };
    run_steps(
        store,
        connection,
        exit_steps,
        plan.activities,
        transport,
        observer,
        activity_observer,
        runtime_params,
    )
    .await
}

#[uniffi::export]
#[allow(clippy::too_many_arguments)]
pub async fn run_action(
    store: Arc<ConfigStore>,
    connection: String,
    action: ActionVerb,
    transport: Arc<dyn PtpExecutorTransport>,
    observer: Arc<dyn StepObserver>,
    activity_observer: Arc<dyn ConnectionActivityObserver>,
    runtime_params: Vec<PtpRuntimeValue>,
) -> Result<PtpExecutionOutcome, PtpExecutorError> {
    let action =
        store
            .action(connection.clone(), action)
            .ok_or_else(|| PtpExecutorError::UnknownPlan {
                detail: format!("{connection}: action not found"),
            })?;
    run_steps(
        store,
        connection,
        action.steps,
        action.activities,
        transport,
        observer,
        activity_observer,
        runtime_params,
    )
    .await
}

#[uniffi::export]
#[allow(clippy::too_many_arguments)]
pub async fn run_selected_object_preparation(
    store: Arc<ConfigStore>,
    connection: String,
    transport: Arc<dyn PtpExecutorTransport>,
    observer: Arc<dyn StepObserver>,
    activity_observer: Arc<dyn ConnectionActivityObserver>,
    runtime_params: Vec<PtpRuntimeValue>,
) -> Result<PtpExecutionOutcome, PtpExecutorError> {
    let plan = store
        .selected_object_transfer(connection.clone())
        .map_err(|error| PtpExecutorError::UnknownPlan {
            detail: error.to_string(),
        })?
        .ok_or_else(|| PtpExecutorError::UnknownPlan {
            detail: format!("{connection}: selected-object preparation not found"),
        })?;
    run_steps(
        store,
        connection,
        plan.preparation_steps,
        Vec::new(),
        transport,
        observer,
        activity_observer,
        runtime_params,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn run_steps(
    store: Arc<ConfigStore>,
    connection: String,
    steps: Vec<EntryStep>,
    activities: Vec<ConnectionActivityDescriptor>,
    transport: Arc<dyn PtpExecutorTransport>,
    observer: Arc<dyn StepObserver>,
    activity_observer: Arc<dyn ConnectionActivityObserver>,
    runtime_params: Vec<PtpRuntimeValue>,
) -> Result<PtpExecutionOutcome, PtpExecutorError> {
    let connection_config = store
        .inner
        .manifest
        .connections
        .get(&connection)
        .ok_or_else(|| PtpExecutorError::UnknownPlan {
            detail: format!("unknown connection {connection}"),
        })?;
    let command_framing: PtpFraming = connection_config
        .command_framing
        .ok_or_else(|| PtpExecutorError::UnsupportedPlan {
            detail: format!("connection {connection} has no command framing"),
        })?
        .into();
    let event_framing = connection_config
        .event_framing
        .map(Into::into)
        .unwrap_or(command_framing);
    let runtime_params: BTreeMap<String, u64> = runtime_params
        .into_iter()
        .map(|value| (value.key, value.value))
        .collect();
    let mut ctx = PtpCtx {
        store,
        connection,
        command_framing,
        event_framing,
        transport,
        observer,
        activity_observer,
        activities,
        active_activity: None,
        observed: cc::PropView::new(),
        bindings: runtime_params.clone(),
        runtime_params,
        collections: BTreeMap::new(),
        outputs: Vec::new(),
        steps_run: 0,
        control_attempts: None,
        deferred_tolerance: None,
    };
    ctx.walk_steps(&steps, "steps", true)
        .await
        .map_err(PtpExecutorError::from)?;
    ctx.finish_activity_success();
    Ok(ctx.outcome())
}

#[derive(Debug, Clone)]
struct TxMeta {
    operation: u16,
    property: Option<u16>,
    response_code: u16,
    transaction_id: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FailureClass {
    Response,
    Deadline,
    Transport,
    Other,
}

#[derive(Debug)]
struct StepError {
    step: String,
    detail: String,
    class: FailureClass,
    meta: Option<TxMeta>,
}

fn ptp_activity_failure(error: &StepError) -> ConnectionActivityFailure {
    let kind = if error.class == FailureClass::Deadline {
        ExecutorStepFailureKind::DeadlineExceeded
    } else {
        ExecutorStepFailureKind::Other
    };
    ConnectionActivityFailure::without_context(kind)
}

impl From<StepError> for PtpExecutorError {
    fn from(error: StepError) -> Self {
        let mut context = Vec::new();
        if let Some(meta) = &error.meta {
            context.push(KeyValue {
                key: "operation".into(),
                value: format!("0x{:04x}", meta.operation),
            });
            context.push(KeyValue {
                key: "response".into(),
                value: format!("0x{:04x}", meta.response_code),
            });
            context.push(KeyValue {
                key: "transactionId".into(),
                value: meta.transaction_id.to_string(),
            });
        }
        Self::StepFailed {
            step: error.step,
            kind: match error.class {
                FailureClass::Deadline => ExecutorStepFailureKind::DeadlineExceeded,
                FailureClass::Response | FailureClass::Transport | FailureClass::Other => {
                    ExecutorStepFailureKind::Other
                }
            },
            detail: error.detail,
            context,
        }
    }
}

struct WireReply {
    meta: TxMeta,
    payload: Vec<u8>,
}

struct PtpActiveActivity {
    descriptor: ConnectionActivityDescriptor,
    lifecycle: ActiveActivity,
}

struct PtpCtx {
    store: Arc<ConfigStore>,
    connection: String,
    command_framing: PtpFraming,
    event_framing: PtpFraming,
    transport: Arc<dyn PtpExecutorTransport>,
    observer: Arc<dyn StepObserver>,
    activity_observer: Arc<dyn ConnectionActivityObserver>,
    activities: Vec<ConnectionActivityDescriptor>,
    active_activity: Option<PtpActiveActivity>,
    observed: cc::PropView,
    bindings: BTreeMap<String, u64>,
    runtime_params: BTreeMap<String, u64>,
    collections: BTreeMap<String, Vec<u64>>,
    outputs: Vec<PtpDataOutput>,
    steps_run: u32,
    control_attempts: Option<u32>,
    deferred_tolerance: Option<StepError>,
}

impl PtpCtx {
    fn outcome(&mut self) -> PtpExecutionOutcome {
        PtpExecutionOutcome {
            scope: std::mem::take(&mut self.bindings)
                .into_iter()
                .map(|(key, value)| PtpRuntimeValue { key, value })
                .collect(),
            collections: std::mem::take(&mut self.collections)
                .into_iter()
                .map(|(key, values)| PtpCollectionValue { key, values })
                .collect(),
            outputs: std::mem::take(&mut self.outputs),
            steps_run: self.steps_run,
        }
    }

    fn activity_for_start(&self, index: u32) -> Option<ConnectionActivityDescriptor> {
        self.activities
            .iter()
            .find_map(|activity| match &activity.binding {
                ConnectionActivityBinding::ExecutorSpan { start_step, .. }
                    if *start_step == index =>
                {
                    Some(activity.clone())
                }
                _ => None,
            })
    }

    fn activity_ends_after(&self, index: u32) -> bool {
        self.active_activity.as_ref().is_some_and(|active| {
            matches!(
                active.descriptor.binding,
                ConnectionActivityBinding::ExecutorSpan {
                    end_step_exclusive,
                    ..
                } if end_step_exclusive == index + 1
            )
        })
    }

    fn start_activity(&mut self, activity: ConnectionActivityDescriptor) {
        self.finish_activity_success();
        let lifecycle = ActiveActivity::new(
            Arc::clone(&self.activity_observer),
            activity.id.clone(),
            activity.version,
        );
        self.active_activity = Some(PtpActiveActivity {
            descriptor: activity,
            lifecycle,
        });
    }

    fn finish_activity_success(&mut self) {
        if let Some(activity) = self.active_activity.take() {
            activity.lifecycle.succeed();
        }
    }

    fn finish_activity_failure(&mut self, error: &StepError) {
        if let Some(activity) = self.active_activity.take() {
            activity.lifecycle.fail(ptp_activity_failure(error));
        }
    }

    fn retry_activity(&mut self, ordinal: u32, limit: u32, error: &StepError) {
        if let Some(activity) = &mut self.active_activity {
            activity.lifecycle.retry(ConnectionActivityRetry {
                ordinal,
                limit,
                failure: ptp_activity_failure(error),
            });
        }
    }

    fn activity_correlation(&self) -> (Option<String>, Option<u32>) {
        self.active_activity
            .as_ref()
            .map(|activity| {
                (
                    Some(activity.lifecycle.id().to_string()),
                    Some(activity.lifecycle.version()),
                )
            })
            .unwrap_or((None, None))
    }

    fn walk_steps<'a>(
        &'a mut self,
        steps: &'a [EntryStep],
        path: &'a str,
        top_level: bool,
    ) -> Pin<Box<dyn Future<Output = Result<Option<TxMeta>, StepError>> + Send + 'a>> {
        Box::pin(async move {
            let mut last = None;
            for (index, step) in steps.iter().enumerate() {
                if top_level {
                    if let Some(activity) = self.activity_for_start(index as u32) {
                        self.start_activity(activity);
                    }
                }
                let here = format!("{path}[{index}].{}", step_verb(step));
                match self.run_step(step, &here).await {
                    Ok(meta) => {
                        last = meta;
                        if top_level && self.activity_ends_after(index as u32) {
                            self.finish_activity_success();
                        }
                    }
                    Err(error) => {
                        if top_level {
                            self.finish_activity_failure(&error);
                        }
                        return Err(error);
                    }
                }
            }
            Ok(last)
        })
    }

    fn run_step<'a>(
        &'a mut self,
        step: &'a EntryStep,
        here: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<Option<TxMeta>, StepError>> + Send + 'a>> {
        Box::pin(async move {
            let tolerant = step_tolerant(step);
            let (operation, property) = step_codes(step);
            let (activity_id, activity_version) = self.activity_correlation();
            self.observer.on_step(StepReport {
                step_path: here.to_string(),
                verb: step_verb(step).to_string(),
                characteristic: None,
                operation,
                property,
                response_code: None,
                transaction_id: None,
                tolerant,
                outcome: StepOutcome::Started,
                error: None,
                attempts: 0,
                activity_id: activity_id.clone(),
                activity_version,
            });

            self.control_attempts = None;
            self.deferred_tolerance = None;
            let result = if matches!(step, EntryStep::AwaitUntil { .. }) {
                self.run_step_once(step, here).await
            } else {
                let transport = self.transport.clone();
                let selected = select(
                    Box::pin(self.run_step_once(step, here)),
                    Box::pin(transport.sleep(DEFAULT_STEP_TIMEOUT_MS)),
                )
                .await;
                match selected {
                    Either::Left((result, pending_clock)) => {
                        drop(pending_clock);
                        result
                    }
                    Either::Right((clock, pending_step)) => {
                        drop(pending_step);
                        match clock {
                            Ok(()) => Err(StepError {
                                step: here.to_string(),
                                detail: format!(
                                    "step deadline exceeded after {DEFAULT_STEP_TIMEOUT_MS}ms"
                                ),
                                class: FailureClass::Deadline,
                                meta: None,
                            }),
                            Err(error) => Err(transport_step_error(here, error)),
                        }
                    }
                }
            };
            let deferred_tolerance = self.deferred_tolerance.take();
            let attempts = if matches!(step, EntryStep::Retry { .. }) {
                self.control_attempts.take().unwrap_or(0)
            } else {
                0
            };
            match result {
                Ok(meta) if deferred_tolerance.is_none() => {
                    self.steps_run += 1;
                    self.report_terminal(
                        here,
                        step,
                        StepOutcome::Succeeded,
                        None,
                        attempts,
                        meta.as_ref(),
                        activity_id,
                        activity_version,
                    );
                    Ok(meta)
                }
                Ok(meta) => {
                    let error = deferred_tolerance.expect("checked above");
                    let tolerated_meta = error.meta.clone();
                    self.steps_run += 1;
                    self.report_terminal(
                        here,
                        step,
                        StepOutcome::Tolerated,
                        Some(error.detail),
                        attempts,
                        tolerated_meta.as_ref().or(meta.as_ref()),
                        activity_id,
                        activity_version,
                    );
                    Ok(tolerated_meta.or(meta))
                }
                Err(error) if tolerant && error.class == FailureClass::Response => {
                    self.steps_run += 1;
                    self.report_terminal(
                        here,
                        step,
                        StepOutcome::Tolerated,
                        Some(error.detail.clone()),
                        attempts,
                        error.meta.as_ref(),
                        activity_id,
                        activity_version,
                    );
                    Ok(error.meta)
                }
                Err(error) => {
                    self.report_terminal(
                        here,
                        step,
                        StepOutcome::Failed,
                        Some(error.detail.clone()),
                        attempts,
                        error.meta.as_ref(),
                        activity_id,
                        activity_version,
                    );
                    Err(error)
                }
            }
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn report_terminal(
        &self,
        here: &str,
        step: &EntryStep,
        outcome: StepOutcome,
        error: Option<String>,
        attempts: u32,
        meta: Option<&TxMeta>,
        activity_id: Option<String>,
        activity_version: Option<u32>,
    ) {
        let (declared_operation, declared_property) = step_codes(step);
        self.observer.on_step(StepReport {
            step_path: here.to_string(),
            verb: step_verb(step).to_string(),
            characteristic: None,
            operation: meta.map(|value| value.operation).or(declared_operation),
            property: meta.and_then(|value| value.property).or(declared_property),
            response_code: meta.map(|value| value.response_code),
            transaction_id: meta.map(|value| value.transaction_id),
            tolerant: step_tolerant(step),
            outcome,
            error,
            attempts,
            activity_id,
            activity_version,
        });
    }

    fn run_step_once<'a>(
        &'a mut self,
        step: &'a EntryStep,
        here: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<Option<TxMeta>, StepError>> + Send + 'a>> {
        Box::pin(async move {
            match step {
                EntryStep::SetProp { prop, value, .. } => {
                    let width = self.store.property_value_width(*prop).ok_or_else(|| {
                        self.other(here, format!("property {prop:#06x} has no scalar width"))
                    })?;
                    let payload = crate::encode_value(*value, width)
                        .map_err(|error| self.other(here, error.to_string()))?;
                    let reply = self
                        .issue(
                            here,
                            op::SET_DEVICE_PROP_VALUE,
                            vec![*prop as u32],
                            Some(payload),
                            Some(*prop),
                        )
                        .await?;
                    self.require_ok(here, reply).map(Some)
                }
                EntryStep::GetProp { prop, captures, .. } => {
                    let reply = self
                        .issue(
                            here,
                            op::GET_DEVICE_PROP_VALUE,
                            vec![*prop as u32],
                            None,
                            Some(*prop),
                        )
                        .await?;
                    let reply = self.require_ok_reply(here, reply)?;
                    self.capture_property(here, *prop, captures, &reply.payload)?;
                    Ok(Some(reply.meta))
                }
                EntryStep::ReadEcho { prop, captures, .. } => {
                    let read = self
                        .issue(
                            here,
                            op::GET_DEVICE_PROP_VALUE,
                            vec![*prop as u32],
                            None,
                            Some(*prop),
                        )
                        .await?;
                    let read = self.require_ok_reply(here, read)?;
                    self.capture_property(here, *prop, captures, &read.payload)?;
                    let write = self
                        .issue(
                            here,
                            op::SET_DEVICE_PROP_VALUE,
                            vec![*prop as u32],
                            Some(read.payload),
                            Some(*prop),
                        )
                        .await?;
                    self.require_ok(here, write).map(Some)
                }
                EntryStep::SendOp {
                    op,
                    params,
                    captures,
                    repeat,
                    ..
                } => {
                    let params = self.resolve_params(params, here)?;
                    let mut last = None;
                    for _ in 0..(*repeat).max(1) {
                        let reply = self.issue(here, *op, params.clone(), None, None).await?;
                        if reply.meta.response_code == resp::OK {
                            self.apply_captures(here, *op, captures, &reply.payload)?;
                            last = Some(reply.meta);
                        } else {
                            last = Some(self.require_ok_or_tolerate(
                                here,
                                reply,
                                step_tolerant(step),
                            )?);
                        }
                    }
                    Ok(last)
                }
                EntryStep::CloseSession {
                    transport_close, ..
                } => {
                    let reply = self
                        .issue(here, op::CLOSE_SESSION, Vec::new(), None, None)
                        .await?;
                    let meta = self.require_ok_or_tolerate(here, reply, step_tolerant(step))?;
                    let close_frame = if *transport_close {
                        self.store
                            .transport_close(self.connection.clone())
                            .map_err(|error| self.other(here, error.to_string()))?
                            .map(|info| info.packet)
                    } else {
                        None
                    };
                    self.transport_deadline(
                        self.transport.close_command_channel(close_frame),
                        DEFAULT_OP_TIMEOUT_MS,
                        here,
                    )
                    .await?;
                    Ok(Some(meta))
                }
                EntryStep::ReopenSession { .. } => {
                    let reply = self
                        .issue(here, op::CLOSE_SESSION, Vec::new(), None, None)
                        .await?;
                    self.require_ok_or_tolerate(here, reply, step_tolerant(step))?;
                    let close_frame = self
                        .store
                        .transport_close(self.connection.clone())
                        .map_err(|error| self.other(here, error.to_string()))?
                        .map(|info| info.packet);
                    self.transport_deadline(
                        self.transport.close_command_channel(close_frame),
                        DEFAULT_OP_TIMEOUT_MS,
                        here,
                    )
                    .await?;
                    let opened = self
                        .transport_deadline(
                            self.transport.reopen_command_session(),
                            DEFAULT_OP_TIMEOUT_MS,
                            here,
                        )
                        .await?;
                    let meta = TxMeta {
                        operation: op::OPEN_SESSION,
                        property: None,
                        response_code: opened.response_code,
                        transaction_id: opened.transaction_id,
                    };
                    let reply = WireReply {
                        meta,
                        payload: Vec::new(),
                    };
                    self.require_ok_or_tolerate(here, reply, step_tolerant(step))
                        .map(Some)
                }
                EntryStep::AwaitUntil {
                    source,
                    until,
                    on_each,
                    timeout_ms,
                    interval_ms,
                    ..
                } => {
                    let transport = self.transport.clone();
                    let budget = (*timeout_ms).max(1);
                    let walk = self.run_await_body(source, until, on_each, *interval_ms, here);
                    let selected = select(Box::pin(walk), Box::pin(transport.sleep(budget))).await;
                    match selected {
                        Either::Left((result, pending_clock)) => {
                            drop(pending_clock);
                            result
                        }
                        Either::Right((clock, pending_walk)) => {
                            drop(pending_walk);
                            match clock {
                                Ok(()) => Err(StepError {
                                    step: here.to_string(),
                                    detail: format!("await deadline exceeded after {budget}ms"),
                                    class: FailureClass::Deadline,
                                    meta: None,
                                }),
                                Err(error) => Err(transport_step_error(here, error)),
                            }
                        }
                    }
                }
                EntryStep::Retry {
                    steps,
                    when_response_codes,
                    max_attempts,
                    retry_delay_ms,
                    ..
                } => {
                    let limit = (*max_attempts).max(1);
                    for attempt in 1..=limit {
                        match self
                            .walk_steps(steps, &format!("{here}.steps"), false)
                            .await
                        {
                            Ok(meta) => {
                                self.control_attempts = Some(attempt - 1);
                                return Ok(meta);
                            }
                            Err(error)
                                if error.class == FailureClass::Response
                                    && error.meta.as_ref().is_some_and(|meta| {
                                        when_response_codes.contains(&meta.response_code)
                                    })
                                    && attempt < limit =>
                            {
                                self.retry_activity(attempt + 1, limit, &error);
                                if *retry_delay_ms > 0 {
                                    self.transport_deadline(
                                        self.transport.sleep(*retry_delay_ms),
                                        retry_delay_ms.saturating_add(DEFAULT_OP_TIMEOUT_MS),
                                        here,
                                    )
                                    .await?;
                                }
                            }
                            Err(error) => {
                                self.control_attempts = Some(attempt - 1);
                                return Err(error);
                            }
                        }
                    }
                    unreachable!("retry attempt limit is at least one")
                }
                EntryStep::Loop { kind, .. } => self.run_loop(kind, here).await,
                EntryStep::If {
                    slot,
                    equals,
                    then_steps,
                    ..
                } => {
                    let actual =
                        self.bindings.get(slot).copied().ok_or_else(|| {
                            self.other(here, format!("if slot {slot:?} is unbound"))
                        })?;
                    if actual == *equals {
                        self.walk_steps(then_steps, &format!("{here}.then"), false)
                            .await
                    } else {
                        Ok(None)
                    }
                }
            }
        })
    }

    fn run_await_body<'a>(
        &'a mut self,
        source: &'a FfiAwaitSource,
        until: &'a FfiPredicate,
        on_each: &'a [EntryStep],
        interval_ms: u32,
        here: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<Option<TxMeta>, StepError>> + Send + 'a>> {
        Box::pin(async move {
            let predicate = cc::Predicate::from(until);
            let mut last = None;
            if let FfiAwaitSource::Event { code, .. } = source {
                let frame = self
                    .transport_deadline(
                        self.transport.next_event_frame(*code),
                        DEFAULT_OP_TIMEOUT_MS,
                        here,
                    )
                    .await?;
                let packet = frame_decode(self.event_framing, &frame)
                    .map_err(|error| self.other(here, error.to_string()))?;
                if !matches!(packet, ptp_core::PtpIpPacket::Event(event) if event.code == *code) {
                    return Err(self.other(
                        here,
                        format!("event transport returned a frame other than {code:#06x}"),
                    ));
                }
            }
            let poll = match source {
                FfiAwaitSource::Poll { prop } => Some(*prop),
                FfiAwaitSource::Event { then_poll, .. } => *then_poll,
            };
            loop {
                if let Some(prop) = poll {
                    let reply = self
                        .issue(
                            here,
                            op::GET_DEVICE_PROP_VALUE,
                            vec![prop as u32],
                            None,
                            Some(prop),
                        )
                        .await?;
                    let reply = self.require_ok_reply(here, reply)?;
                    self.observe_property(here, prop, &reply.payload)?;
                    last = Some(reply.meta);
                }
                if predicate.eval(&self.observed) {
                    return Ok(last);
                }
                if poll.is_none() {
                    return Err(self.other(here, "await predicate rejected event".into()));
                }
                self.walk_steps(on_each, &format!("{here}.onEach"), false)
                    .await?;
                let cadence = if interval_ms == 0 {
                    DEFAULT_POLL_INTERVAL_MS
                } else {
                    interval_ms
                };
                self.transport_deadline(
                    self.transport.sleep(cadence),
                    cadence.saturating_add(DEFAULT_OP_TIMEOUT_MS),
                    here,
                )
                .await?;
            }
        })
    }

    fn run_loop<'a>(
        &'a mut self,
        kind: &'a FfiLoopKind,
        here: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<Option<TxMeta>, StepError>> + Send + 'a>> {
        Box::pin(async move {
            match kind {
                FfiLoopKind::ForEach {
                    collection,
                    bind,
                    body,
                } => {
                    let values = self.collections.get(collection).cloned().ok_or_else(|| {
                        self.other(here, format!("collection {collection:?} is unbound"))
                    })?;
                    if values.len() > MAX_FOREACH_ITERS {
                        return Err(self.other(
                            here,
                            format!("collection exceeds {MAX_FOREACH_ITERS} elements"),
                        ));
                    }
                    let mut last = None;
                    for (index, value) in values.into_iter().enumerate() {
                        let previous = self.bindings.insert(bind.clone(), value);
                        let result = self
                            .walk_steps(body, &format!("{here}.forEach[{index}]"), false)
                            .await;
                        restore(&mut self.bindings, bind, previous);
                        last = result?;
                    }
                    Ok(last)
                }
                FfiLoopKind::Chunk {
                    total,
                    size,
                    offset_bind,
                    length_bind,
                    body,
                } => {
                    let total =
                        self.bindings.get(total).copied().ok_or_else(|| {
                            self.other(here, "chunk total slot is unbound".into())
                        })?;
                    let window = match size {
                        FfiChunkSize::Literal { value } => *value as u64,
                        FfiChunkSize::Runtime { slot } => {
                            self.bindings.get(slot).copied().ok_or_else(|| {
                                self.other(here, format!("chunk size slot {slot:?} is unbound"))
                            })?
                        }
                    };
                    if window == 0 {
                        return Err(self.other(here, "chunk size must be non-zero".into()));
                    }
                    let mut offset = 0_u64;
                    let mut index = 0_usize;
                    let mut last = None;
                    while offset < total {
                        if index == MAX_CHUNK_ITERS {
                            return Err(self.other(
                                here,
                                format!("chunk loop exceeds {MAX_CHUNK_ITERS} windows"),
                            ));
                        }
                        let length = (total - offset).min(window);
                        let old_offset = self.bindings.insert(offset_bind.clone(), offset);
                        let old_length = self.bindings.insert(length_bind.clone(), length);
                        let result = self
                            .walk_steps(body, &format!("{here}.chunk[{index}]"), false)
                            .await;
                        restore(&mut self.bindings, offset_bind, old_offset);
                        restore(&mut self.bindings, length_bind, old_length);
                        last = result?;
                        offset += length;
                        index += 1;
                    }
                    Ok(last)
                }
            }
        })
    }

    async fn issue(
        &mut self,
        here: &str,
        operation: u16,
        params: Vec<u32>,
        data_out: Option<Vec<u8>>,
        property: Option<u16>,
    ) -> Result<WireReply, StepError> {
        let transaction_id = self
            .transport_deadline(
                self.transport.reserve_transaction_id(),
                DEFAULT_OP_TIMEOUT_MS,
                here,
            )
            .await?;
        let command = ptp_core::PtpIpPacket::OperationRequest(ptp_core::OperationRequest {
            data_phase_info: if data_out.is_some() { 2 } else { 1 },
            code: operation,
            transaction_id,
            params,
        });
        let command = frame_encode(self.command_framing, &command)
            .map_err(|error| self.other(here, error.to_string()))?;
        self.transport_deadline(
            self.transport.send_command_frame(command),
            DEFAULT_OP_TIMEOUT_MS,
            here,
        )
        .await?;
        if let Some(payload) = data_out {
            match self.command_framing {
                PtpFraming::Standard => {
                    let start = ptp_core::PtpIpPacket::StartData(ptp_core::StartData {
                        transaction_id,
                        total_length: payload.len() as u64,
                    });
                    let end = ptp_core::PtpIpPacket::EndData(ptp_core::DataBlock {
                        transaction_id,
                        payload,
                    });
                    for packet in [start, end] {
                        let frame = frame_encode(self.command_framing, &packet)
                            .map_err(|error| self.other(here, error.to_string()))?;
                        self.transport_deadline(
                            self.transport.send_command_frame(frame),
                            DEFAULT_OP_TIMEOUT_MS,
                            here,
                        )
                        .await?;
                    }
                }
                PtpFraming::Compressed | PtpFraming::Usb => {
                    let data =
                        crate::build_data(self.command_framing, operation, transaction_id, payload)
                            .map_err(|error| self.other(here, error.to_string()))?;
                    self.transport_deadline(
                        self.transport.send_command_frame(data),
                        DEFAULT_OP_TIMEOUT_MS,
                        here,
                    )
                    .await?;
                }
            }
        }

        let mut payload = Vec::new();
        let mut had_data = false;
        let mut standard_total = None;
        let mut standard_ended = false;
        for _ in 0..MAX_COMMAND_FRAMES {
            let frame = self
                .transport_deadline(
                    self.transport.next_command_frame(),
                    DEFAULT_OP_TIMEOUT_MS,
                    here,
                )
                .await?;
            let packet = frame_decode(self.command_framing, &frame)
                .map_err(|error| self.other(here, error.to_string()))?;
            match packet {
                ptp_core::PtpIpPacket::StartData(start)
                    if start.transaction_id == transaction_id =>
                {
                    if !matches!(self.command_framing, PtpFraming::Standard)
                        || standard_total.is_some()
                    {
                        return Err(self.other(here, "unexpected duplicate data start".into()));
                    }
                    had_data = true;
                    standard_total = Some(start.total_length);
                    if let Ok(capacity) = usize::try_from(start.total_length) {
                        payload.reserve(capacity.min(16 * 1024 * 1024));
                    }
                }
                ptp_core::PtpIpPacket::Data(data) if data.transaction_id == transaction_id => {
                    if matches!(self.command_framing, PtpFraming::Standard)
                        && (standard_total.is_none() || standard_ended)
                    {
                        return Err(
                            self.other(here, "data block outside standard data phase".into())
                        );
                    }
                    had_data = true;
                    payload.extend_from_slice(&data.payload);
                }
                ptp_core::PtpIpPacket::EndData(data) if data.transaction_id == transaction_id => {
                    if matches!(self.command_framing, PtpFraming::Standard)
                        && (standard_total.is_none() || standard_ended)
                    {
                        return Err(self.other(here, "data end outside standard data phase".into()));
                    }
                    had_data = true;
                    standard_ended = true;
                    payload.extend_from_slice(&data.payload);
                }
                ptp_core::PtpIpPacket::OperationResponse(response)
                    if response.transaction_id == transaction_id =>
                {
                    if let Some(total) = standard_total {
                        if !standard_ended || payload.len() as u64 != total {
                            return Err(self.other(
                                here,
                                format!(
                                    "incomplete standard data phase: declared {total} bytes, received {}",
                                    payload.len()
                                ),
                            ));
                        }
                    }
                    let meta = TxMeta {
                        operation,
                        property,
                        response_code: response.code,
                        transaction_id,
                    };
                    if had_data || !response.params.is_empty() {
                        self.outputs.push(PtpDataOutput {
                            step_path: here.to_string(),
                            operation,
                            transaction_id,
                            payload: payload.clone(),
                            response_params: response.params.clone(),
                        });
                    }
                    return Ok(WireReply { meta, payload });
                }
                _ => {
                    return Err(self.other(
                        here,
                        format!("unexpected or mismatched command frame for transaction {transaction_id}"),
                    ));
                }
            }
        }
        Err(self.other(here, "command frame limit exceeded".into()))
    }

    fn require_ok(&self, here: &str, reply: WireReply) -> Result<TxMeta, StepError> {
        self.require_ok_reply(here, reply).map(|reply| reply.meta)
    }

    fn require_ok_reply(&self, here: &str, reply: WireReply) -> Result<WireReply, StepError> {
        if reply.meta.response_code == resp::OK {
            Ok(reply)
        } else {
            Err(self.response_error(here, reply.meta))
        }
    }

    fn require_ok_or_tolerate(
        &mut self,
        here: &str,
        reply: WireReply,
        tolerant: bool,
    ) -> Result<TxMeta, StepError> {
        if reply.meta.response_code == resp::OK {
            Ok(reply.meta)
        } else if tolerant {
            let error = self.response_error(here, reply.meta.clone());
            self.deferred_tolerance = Some(error);
            Ok(reply.meta)
        } else {
            Err(self.response_error(here, reply.meta))
        }
    }

    fn response_error(&self, here: &str, meta: TxMeta) -> StepError {
        StepError {
            step: here.to_string(),
            detail: format!(
                "operation 0x{:04x} returned 0x{:04x}",
                meta.operation, meta.response_code
            ),
            class: FailureClass::Response,
            meta: Some(meta),
        }
    }

    fn capture_property(
        &mut self,
        here: &str,
        prop: u16,
        captures: &[CaptureInfo],
        payload: &[u8],
    ) -> Result<(), StepError> {
        if captures
            .iter()
            .any(|capture| matches!(capture.source, CaptureSourceInfo::PtpU32Array))
        {
            let values = decode_u32_array(payload)
                .map_err(|detail| self.other(here, format!("property {prop:#06x}: {detail}")))?;
            for capture in captures {
                if !matches!(capture.source, CaptureSourceInfo::PtpU32Array) {
                    return Err(self.other(here, "mixed scalar and collection captures".into()));
                }
                self.collections
                    .insert(capture.bind.clone(), values.clone());
            }
            return Ok(());
        }
        if self.store.property_payload(prop).is_some() {
            if !captures.is_empty() {
                return Err(self.other(
                    here,
                    "record-stream property cannot bind a scalar capture".into(),
                ));
            }
            return self.observe_property(here, prop, payload);
        }
        if captures.is_empty() && self.store.property_value_width(prop).is_none() {
            return Ok(());
        }
        let value = self.decode_property(here, prop, payload)?;
        self.observed.set(prop, value);
        for capture in captures {
            if !matches!(capture.source, CaptureSourceInfo::PropValue) {
                return Err(self.other(here, "getProp capture must use propValue".into()));
            }
            self.bindings.insert(capture.bind.clone(), value as u64);
        }
        Ok(())
    }

    fn observe_property(&mut self, here: &str, prop: u16, payload: &[u8]) -> Result<(), StepError> {
        if let Some(info) = self.store.property_payload(prop) {
            let members = info.members.clone();
            let status = crate::parse_record_stream(payload.to_vec(), info)
                .map_err(|error| self.other(here, error.to_string()))?;
            for observation in status
                .records
                .into_iter()
                .filter(|observation| members.contains(&observation.code))
            {
                self.observed.set(observation.code, observation.value);
            }
            Ok(())
        } else {
            let value = self.decode_property(here, prop, payload)?;
            self.observed.set(prop, value);
            Ok(())
        }
    }

    fn decode_property(&self, here: &str, prop: u16, payload: &[u8]) -> Result<i64, StepError> {
        let width = self
            .store
            .property_value_width(prop)
            .ok_or_else(|| self.other(here, format!("property {prop:#06x} has no scalar width")))?;
        decode_scalar(width, payload)
            .map_err(|detail| self.other(here, format!("property {prop:#06x}: {detail}")))
    }

    fn apply_captures(
        &mut self,
        here: &str,
        operation: u16,
        captures: &[CaptureInfo],
        payload: &[u8],
    ) -> Result<(), StepError> {
        let mut values = Vec::with_capacity(captures.len());
        for capture in captures {
            let value = match capture.source {
                CaptureSourceInfo::ObjectInfoCompressedSize => {
                    if operation != op::GET_OBJECT_INFO {
                        return Err(self.other(
                            here,
                            "objectInfoCompressedSize requires GetObjectInfo".into(),
                        ));
                    }
                    ptp_core::ObjectInfo::decode(payload)
                        .map_err(|error| self.other(here, format!("decode ObjectInfo: {error:?}")))?
                        .object_compressed_size as u64
                }
                CaptureSourceInfo::U32Le => {
                    read_u32(payload).map_err(|detail| self.other(here, detail))? as u64
                }
                CaptureSourceInfo::U64Le => {
                    read_u64(payload).map_err(|detail| self.other(here, detail))?
                }
                CaptureSourceInfo::PropValue => {
                    return Err(self.other(here, "propValue requires getProp".into()))
                }
                CaptureSourceInfo::PtpU32Array => {
                    let collection =
                        decode_u32_array(payload).map_err(|detail| self.other(here, detail))?;
                    self.collections.insert(capture.bind.clone(), collection);
                    continue;
                }
            };
            values.push((capture.bind.clone(), value));
        }
        for (bind, value) in values {
            self.bindings.insert(bind, value);
        }
        Ok(())
    }

    fn resolve_params(&self, params: &[EntryParam], here: &str) -> Result<Vec<u32>, StepError> {
        params
            .iter()
            .map(|param| match param {
                EntryParam::Literal { value } => Ok(*value),
                EntryParam::Runtime { slot, shift, mask } => {
                    let raw = self
                        .bindings
                        .get(slot)
                        .or_else(|| self.runtime_params.get(slot))
                        .copied()
                        .ok_or_else(|| {
                            self.other(here, format!("runtime slot {slot:?} is unbound"))
                        })?;
                    let shifted = raw.checked_shr(*shift).unwrap_or(0);
                    let value = mask.map_or(shifted, |mask| shifted & mask);
                    u32::try_from(value)
                        .map_err(|_| self.other(here, format!("runtime slot {slot:?} exceeds u32")))
                }
            })
            .collect()
    }

    async fn transport_deadline<T, F>(
        &self,
        future: F,
        timeout_ms: u32,
        here: &str,
    ) -> Result<T, StepError>
    where
        F: Future<Output = Result<T, PtpTransportError>> + Send,
        T: Send,
    {
        let transport = self.transport.clone();
        let selected = select(Box::pin(future), Box::pin(transport.sleep(timeout_ms))).await;
        match selected {
            Either::Left((result, pending_clock)) => {
                drop(pending_clock);
                result.map_err(|error| self.transport_error(here, error))
            }
            Either::Right((clock, pending_operation)) => {
                drop(pending_operation);
                match clock {
                    Ok(()) => Err(StepError {
                        step: here.to_string(),
                        detail: format!("transport deadline exceeded after {timeout_ms}ms"),
                        class: FailureClass::Deadline,
                        meta: None,
                    }),
                    Err(error) => Err(self.transport_error(here, error)),
                }
            }
        }
    }

    fn transport_error(&self, here: &str, error: PtpTransportError) -> StepError {
        transport_step_error(here, error)
    }

    fn other(&self, here: &str, detail: String) -> StepError {
        StepError {
            step: here.to_string(),
            detail,
            class: FailureClass::Other,
            meta: None,
        }
    }
}

fn transport_step_error(here: &str, error: PtpTransportError) -> StepError {
    let class = if matches!(error, PtpTransportError::Timeout { .. }) {
        FailureClass::Deadline
    } else {
        FailureClass::Transport
    };
    StepError {
        step: here.to_string(),
        detail: error.to_string(),
        class,
        meta: None,
    }
}

fn step_verb(step: &EntryStep) -> &'static str {
    match step {
        EntryStep::SetProp { .. } => "setProp",
        EntryStep::GetProp { .. } => "getProp",
        EntryStep::ReadEcho { .. } => "readEcho",
        EntryStep::SendOp { .. } => "sendOp",
        EntryStep::ReopenSession { .. } => "reopenSession",
        EntryStep::CloseSession { .. } => "closeSession",
        EntryStep::AwaitUntil { .. } => "awaitUntil",
        EntryStep::Retry { .. } => "retry",
        EntryStep::Loop { .. } => "loop",
        EntryStep::If { .. } => "if",
    }
}

fn step_tolerant(step: &EntryStep) -> bool {
    match step {
        EntryStep::SetProp { tolerant, .. }
        | EntryStep::GetProp { tolerant, .. }
        | EntryStep::ReadEcho { tolerant, .. }
        | EntryStep::SendOp { tolerant, .. }
        | EntryStep::ReopenSession { tolerant }
        | EntryStep::CloseSession { tolerant, .. }
        | EntryStep::AwaitUntil { tolerant, .. }
        | EntryStep::Retry { tolerant, .. }
        | EntryStep::Loop { tolerant, .. } => *tolerant,
        EntryStep::If { .. } => false,
    }
}

fn step_codes(step: &EntryStep) -> (Option<u16>, Option<u16>) {
    match step {
        EntryStep::SetProp { prop, .. } => (Some(op::SET_DEVICE_PROP_VALUE), Some(*prop)),
        EntryStep::GetProp { prop, .. } | EntryStep::ReadEcho { prop, .. } => {
            (Some(op::GET_DEVICE_PROP_VALUE), Some(*prop))
        }
        EntryStep::SendOp { op, .. } => (Some(*op), None),
        EntryStep::ReopenSession { .. } => (Some(op::OPEN_SESSION), None),
        EntryStep::CloseSession { .. } => (Some(op::CLOSE_SESSION), None),
        EntryStep::AwaitUntil { source, .. } => match source {
            FfiAwaitSource::Poll { prop } => (Some(op::GET_DEVICE_PROP_VALUE), Some(*prop)),
            FfiAwaitSource::Event { then_poll, .. } => {
                (then_poll.map(|_| op::GET_DEVICE_PROP_VALUE), *then_poll)
            }
        },
        EntryStep::Retry { .. } | EntryStep::Loop { .. } | EntryStep::If { .. } => (None, None),
    }
}

fn restore(bindings: &mut BTreeMap<String, u64>, slot: &str, previous: Option<u64>) {
    match previous {
        Some(value) => {
            bindings.insert(slot.to_string(), value);
        }
        None => {
            bindings.remove(slot);
        }
    }
}

fn decode_scalar(width: ValueWidth, payload: &[u8]) -> Result<i64, String> {
    let mut reader = ptp_core::Reader::new(payload);
    match width {
        ValueWidth::U8 => reader.u8().map(i64::from),
        ValueWidth::U16 => reader.u16().map(i64::from),
        ValueWidth::U32 => reader.u32().map(i64::from),
        ValueWidth::I16 => reader
            .u16()
            .map(|value| i16::from_le_bytes(value.to_le_bytes()) as i64),
        ValueWidth::I32 => reader
            .u32()
            .map(|value| i32::from_le_bytes(value.to_le_bytes()) as i64),
    }
    .map_err(|error| format!("decode scalar: {error:?}"))
}

fn decode_u32_array(payload: &[u8]) -> Result<Vec<u64>, String> {
    let count_bytes: [u8; 4] = payload
        .get(..4)
        .ok_or_else(|| "decode u32 array: count needs 4 bytes".to_string())?
        .try_into()
        .expect("four-byte count");
    let count = u32::from_le_bytes(count_bytes);
    if count as usize > MAX_FOREACH_ITERS {
        return Err(format!(
            "decode u32 array: count {count} exceeds {MAX_FOREACH_ITERS}"
        ));
    }
    let expected = 4_usize
        .checked_add(
            (count as usize)
                .checked_mul(4)
                .ok_or_else(|| "decode u32 array: payload length overflow".to_string())?,
        )
        .ok_or_else(|| "decode u32 array: payload length overflow".to_string())?;
    if payload.len() > expected {
        return Err(format!(
            "decode u32 array: {} trailing bytes",
            payload.len() - expected
        ));
    }
    if payload.len() != expected {
        return Err(format!(
            "decode u32 array: expected {expected} bytes for {count} values, got {}",
            payload.len()
        ));
    }
    Ok(payload[4..]
        .chunks_exact(4)
        .map(|bytes| {
            u64::from(u32::from_le_bytes(
                bytes.try_into().expect("four-byte value"),
            ))
        })
        .collect())
}

fn read_u32(payload: &[u8]) -> Result<u32, String> {
    ptp_core::Reader::new(payload)
        .u32()
        .map_err(|error| format!("decode u32Le: {error:?}"))
}

fn read_u64(payload: &[u8]) -> Result<u64, String> {
    ptp_core::Reader::new(payload)
        .u64()
        .map_err(|error| format!("decode u64Le: {error:?}"))
}
