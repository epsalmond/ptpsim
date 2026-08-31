use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use camera_config::{ActionOutcome, PayloadMetadataBuilder};
use camera_protocol_ffi::{
    parse_action_verb, run_initiator_action_to_sink, run_mode_entry, run_mode_reestablishment_exit,
    run_streaming_action, ActionArgument, ActionCatalogParameterKind, ActionInvocationRequest,
    ActionRole, ActionValue, ConfigStore, ConnectionActivityEvent, ConnectionActivityObserver,
    EntryParam, EntryStep, ModeEntryExecution, ObjectTransferStrategy, PtpDataOutput,
    PtpDataOutputSink, PtpDataOutputSinkError, PtpExecutionOutcome, PtpExecutorTransport,
    PtpRuntimeValue, PtpSessionOpenResult, PtpStreamingTransport, PtpTransportError, StepObserver,
    StepReport,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::io::AsyncWriteExt;

use crate::{NativePtpTransport, StreamingFileSink, TraceWriter};

pub const SESSION_PLAN_SCHEMA: &str = "camera-initiator-session/v1";
pub const SESSION_REPORT_SCHEMA: &str = "camera-initiator-session-report/v1";

pub fn validate_ptp_entry(
    store: &ConfigStore,
    connection: &str,
    from: Option<String>,
    to: String,
) -> Result<()> {
    let entry = store
        .mode_entry(connection.to_string(), from.clone(), to.clone())
        .with_context(|| format!("mode entry {from:?} -> '{to}' is not declared"))?;
    match entry.execution {
        ModeEntryExecution::Ptp { .. } => Ok(()),
        ModeEntryExecution::UserInstruction { instruction } => {
            bail!("mode entry requires user instruction: {instruction}")
        }
        ModeEntryExecution::ReestablishConnection { .. } => {
            bail!("mode entry requires outer re-establishment; use 'switch'")
        }
    }
}

pub fn resolve_reestablishment_checkpoint(
    store: &ConfigStore,
    connection: &str,
    from: &str,
    to: &str,
) -> Result<BTreeMap<String, String>> {
    let edge = store
        .mode_entry(
            connection.to_string(),
            Some(from.to_string()),
            to.to_string(),
        )
        .with_context(|| format!("switch edge '{from}' -> '{to}' is not declared"))?;
    match edge.execution {
        ModeEntryExecution::ReestablishConnection {
            establishment_params,
            ..
        } => Ok(establishment_params
            .into_iter()
            .map(|value| (value.key, value.value))
            .collect()),
        _ => bail!("switch edge '{from}' -> '{to}' is not a re-establishment"),
    }
}

pub fn prepare_action_request(
    store: &ConfigStore,
    connection: &str,
    action: camera_protocol_ffi::ActionVerb,
    mode: &str,
    parameters: Vec<ActionArgument>,
) -> Result<ActionInvocationRequest> {
    let catalog = store.action_catalog();
    let action_id = catalog
        .actions
        .iter()
        .find(|entry| {
            entry.connection == connection
                && parse_action_verb(entry.action_id.clone()) == Some(action)
        })
        .map(|entry| entry.action_id.clone())
        .with_context(|| format!("connection '{connection}' has no cataloged action"))?;
    let request = ActionInvocationRequest {
        catalog_revision: catalog.revision,
        action_id,
        connection: connection.to_string(),
        mode: mode.to_string(),
        role: ActionRole::Initiator,
        parameters,
    };
    store
        .resolve_action_invocation(request.clone())
        .map_err(|error| anyhow::anyhow!(error.to_string()))?;
    Ok(request)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionPlan {
    pub schema: String,
    pub steps: Vec<SessionStep>,
}

impl SessionPlan {
    pub fn from_yaml(yaml: &str) -> Result<Self> {
        let plan: Self = serde_yaml::from_str(yaml).context("parse session plan YAML")?;
        if plan.schema != SESSION_PLAN_SCHEMA {
            bail!(
                "unsupported session plan schema '{}'; expected {SESSION_PLAN_SCHEMA}",
                plan.schema
            )
        }
        if plan.steps.is_empty() {
            bail!("session plan steps must not be empty")
        }
        let mut ids = BTreeSet::new();
        for step in &plan.steps {
            let id = step.id();
            if !safe_step_id(id) {
                bail!("session step id '{id}' must match [A-Za-z0-9][A-Za-z0-9_-]*")
            }
            if !ids.insert(id) {
                bail!("duplicate session step id '{id}'")
            }
            validate_expectation_shape(step)?;
        }
        Ok(plan)
    }
}

fn safe_step_id(value: &str) -> bool {
    let mut characters = value.chars();
    characters
        .next()
        .is_some_and(|character| character.is_ascii_alphanumeric())
        && characters.all(|character| {
            character.is_ascii_alphanumeric() || character == '_' || character == '-'
        })
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase", deny_unknown_fields)]
pub enum SessionStep {
    Entry {
        id: String,
        from: Option<String>,
        to: String,
        expect: Option<OutcomeExpectation>,
    },
    Switch {
        id: String,
        from: String,
        to: String,
        expect: Option<SwitchExpectation>,
    },
    Action {
        id: String,
        action: String,
        #[serde(default)]
        parameters: BTreeMap<String, SessionValue>,
        #[serde(rename = "expectedBytes")]
        expected_bytes: Option<u64>,
        expect: Option<OutcomeExpectation>,
    },
}

impl SessionStep {
    pub fn id(&self) -> &str {
        match self {
            Self::Entry { id, .. } | Self::Switch { id, .. } | Self::Action { id, .. } => id,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum SessionValue {
    U64(u64),
    String(String),
}

impl SessionValue {
    fn action_value(&self) -> ActionValue {
        match self {
            Self::U64(value) => ActionValue::U64 { value: *value },
            Self::String(value) => ActionValue::String {
                value: value.clone(),
            },
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct OutcomeExpectation {
    pub steps_run: Option<u32>,
    #[serde(default)]
    pub scope: BTreeMap<String, SessionValue>,
    #[serde(default)]
    pub collections: BTreeMap<String, Vec<u64>>,
    pub output_count: Option<usize>,
    #[serde(default)]
    pub outputs: Vec<OutputExpectation>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct OutputExpectation {
    pub index: usize,
    pub payload_bytes: Option<u64>,
    pub response_params: Option<Vec<u64>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SwitchExpectation {
    pub source_entry: Option<OutcomeExpectation>,
    pub exit: Option<OutcomeExpectation>,
    pub target_entry: Option<OutcomeExpectation>,
}

fn validate_expectation_shape(step: &SessionStep) -> Result<()> {
    let expectations: Vec<&OutcomeExpectation> = match step {
        SessionStep::Entry { expect, .. } | SessionStep::Action { expect, .. } => {
            expect.iter().collect()
        }
        SessionStep::Switch { expect, .. } => expect
            .iter()
            .flat_map(|expect| {
                [
                    expect.source_entry.as_ref(),
                    expect.exit.as_ref(),
                    expect.target_entry.as_ref(),
                ]
                .into_iter()
                .flatten()
            })
            .collect(),
    };
    for expect in expectations {
        let mut indexes = BTreeSet::new();
        for output in &expect.outputs {
            if !indexes.insert(output.index) {
                bail!(
                    "session step '{}' repeats output expectation index {}",
                    step.id(),
                    output.index
                )
            }
        }
        if let Some(count) = expect.output_count {
            if let Some(index) = expect.outputs.iter().map(|output| output.index).max() {
                if index >= count {
                    bail!(
                        "session step '{}' expects output index {index} outside outputCount {count}",
                        step.id()
                    )
                }
            }
        }
    }
    Ok(())
}

#[derive(Debug, Clone)]
pub struct PreparedSessionPlan {
    pub plan: SessionPlan,
    steps: Vec<PreparedStep>,
}

#[derive(Debug, Clone)]
enum PreparedStep {
    Entry(PreparedEntry),
    Switch(PreparedSwitch),
    Action(PreparedAction),
}

#[derive(Debug, Clone)]
struct PreparedEntry {
    id: String,
    from: Option<String>,
    to: String,
    expect: Option<OutcomeExpectation>,
}

#[derive(Debug, Clone)]
struct PreparedSwitch {
    id: String,
    from: String,
    to: String,
    cold_source: bool,
    checkpoint: BTreeMap<String, String>,
    expect: Option<SwitchExpectation>,
}

#[derive(Debug, Clone)]
struct PreparedAction {
    id: String,
    name: String,
    verb: camera_protocol_ffi::ActionVerb,
    request: ActionInvocationRequest,
    parameters: BTreeMap<String, SessionValue>,
    streaming: bool,
    expected_bytes: Option<u64>,
    expect: Option<OutcomeExpectation>,
}

pub fn prepare_session_plan(
    plan: SessionPlan,
    store: &ConfigStore,
    connection: &str,
) -> Result<PreparedSessionPlan> {
    let mut tracked_mode: Option<String> = None;
    let mut prepared = Vec::with_capacity(plan.steps.len());
    for step in &plan.steps {
        match step {
            SessionStep::Entry {
                id,
                from,
                to,
                expect,
            } => {
                match (&tracked_mode, from) {
                    (None, None) => {}
                    (None, Some(source)) => {
                        bail!("entry step '{id}' names from '{source}' without a retained mode")
                    }
                    (Some(active), None) => {
                        bail!("entry step '{id}' must name retained from mode '{active}'")
                    }
                    (Some(active), Some(source)) if active != source => bail!(
                        "entry step '{id}' names from '{source}', retained mode is '{active}'"
                    ),
                    (Some(_), Some(_)) => {}
                }
                validate_ptp_entry(store, connection, from.clone(), to.clone())
                    .with_context(|| format!("entry step '{id}' is invalid"))?;
                prepared.push(PreparedStep::Entry(PreparedEntry {
                    id: id.clone(),
                    from: from.clone(),
                    to: to.clone(),
                    expect: expect.clone(),
                }));
                tracked_mode = Some(to.clone());
            }
            SessionStep::Switch {
                id,
                from,
                to,
                expect,
            } => {
                let cold_source = match &tracked_mode {
                    None => true,
                    Some(active) if active == from => false,
                    Some(active) => bail!(
                        "switch step '{id}' names source '{from}', retained mode is '{active}'"
                    ),
                };
                if cold_source {
                    validate_ptp_entry(store, connection, None, from.clone())
                        .with_context(|| format!("switch step '{id}' cold source is invalid"))?;
                } else if expect
                    .as_ref()
                    .is_some_and(|expect| expect.source_entry.is_some())
                {
                    bail!(
                        "switch step '{id}' sourceEntry expectation is valid only for a cold source"
                    )
                }
                let checkpoint = resolve_reestablishment_checkpoint(store, connection, from, to)
                    .with_context(|| format!("switch step '{id}' is invalid"))?;
                validate_ptp_entry(store, connection, None, to.clone())
                    .with_context(|| format!("switch step '{id}' cold target is invalid"))?;
                prepared.push(PreparedStep::Switch(PreparedSwitch {
                    id: id.clone(),
                    from: from.clone(),
                    to: to.clone(),
                    cold_source,
                    checkpoint,
                    expect: expect.clone(),
                }));
                tracked_mode = Some(to.clone());
            }
            SessionStep::Action {
                id,
                action,
                parameters,
                expected_bytes,
                expect,
            } => {
                let verb = parse_action_verb(action.clone()).with_context(|| {
                    format!("action step '{id}' has unknown action '{action}'; use exact camelCase")
                })?;
                let action_plan =
                    store
                        .action(connection.to_string(), verb)
                        .with_context(|| {
                            format!(
                                "action step '{id}' is not declared for connection '{connection}'"
                            )
                        })?;
                if !action_plan.mode.is_empty() {
                    let active = tracked_mode.as_deref().with_context(|| {
                        format!(
                            "action step '{id}' requires mode '{}' before execution",
                            action_plan.mode
                        )
                    })?;
                    if active != action_plan.mode {
                        bail!(
                            "action step '{id}' requires mode '{}', retained mode is '{active}'",
                            action_plan.mode
                        )
                    }
                }
                let declarations = &action_plan
                    .initiator
                    .as_ref()
                    .with_context(|| format!("action step '{id}' has no initiator binding"))?
                    .params;
                let declared: BTreeMap<_, _> = declarations
                    .iter()
                    .map(|parameter| (parameter.name.as_str(), parameter))
                    .collect();
                if let Some(extra) = parameters
                    .keys()
                    .find(|name| !declared.contains_key(name.as_str()))
                {
                    bail!("action step '{id}' has undeclared parameter '{extra}'")
                }
                for parameter in declarations {
                    let Some(value) = parameters.get(&parameter.name) else {
                        if parameter.required {
                            bail!(
                                "action step '{id}' is missing required parameter '{}'",
                                parameter.name
                            )
                        }
                        continue;
                    };
                    match (parameter.kind, value) {
                        (ActionCatalogParameterKind::U32, SessionValue::U64(value))
                            if *value <= u32::MAX as u64 => {}
                        (ActionCatalogParameterKind::U64, SessionValue::U64(_))
                        | (ActionCatalogParameterKind::String, SessionValue::String(_)) => {}
                        (ActionCatalogParameterKind::U32, SessionValue::U64(_)) => bail!(
                            "action step '{id}' parameter '{}' exceeds u32",
                            parameter.name
                        ),
                        (_, _) => bail!(
                            "action step '{id}' parameter '{}' has the wrong type",
                            parameter.name
                        ),
                    }
                }
                let arguments = parameters
                    .iter()
                    .map(|(name, value)| ActionArgument {
                        name: name.clone(),
                        value: value.action_value(),
                    })
                    .collect();
                let request =
                    prepare_action_request(store, connection, verb, &action_plan.mode, arguments)
                        .with_context(|| format!("action step '{id}' is invalid"))?;
                let transfer = store.object_transfer_contract(connection.to_string());
                let streaming = transfer.as_ref().is_some_and(|contract| {
                    contract.strategy == ObjectTransferStrategy::WholeObject
                        && contract.read_action == verb
                });
                if streaming {
                    let initiator = action_plan.initiator.as_ref().expect("checked above");
                    let [EntryStep::SendOp {
                        params: step_params,
                        repeat: 1,
                        ..
                    }] = initiator.steps.as_slice()
                    else {
                        bail!(
                            "action step '{id}' whole-object action must contain one unrepeated sendOp"
                        )
                    };
                    for parameter in step_params {
                        if let EntryParam::Runtime { slot, .. } = parameter {
                            match parameters.get(slot) {
                                Some(SessionValue::U64(value)) if *value <= u32::MAX as u64 => {}
                                Some(SessionValue::U64(_)) => bail!(
                                    "action step '{id}' streaming parameter '{slot}' exceeds u32"
                                ),
                                Some(SessionValue::String(_)) => bail!(
                                    "action step '{id}' streaming parameter '{slot}' must be u64"
                                ),
                                None => bail!(
                                    "action step '{id}' streaming parameter '{slot}' is unbound"
                                ),
                            }
                        }
                    }
                }
                if expected_bytes.is_some() && !streaming {
                    bail!(
                        "action step '{id}' expectedBytes is valid only for a whole-object streaming action"
                    )
                }
                prepared.push(PreparedStep::Action(PreparedAction {
                    id: id.clone(),
                    name: action.clone(),
                    verb,
                    request,
                    parameters: parameters.clone(),
                    streaming,
                    expected_bytes: *expected_bytes,
                    expect: expect.clone(),
                }));
            }
        }
    }
    Ok(PreparedSessionPlan {
        plan,
        steps: prepared,
    })
}

pub fn validate_session_artifact_paths(
    plan_path: &Path,
    output_dir: &Path,
    trace_path: &str,
    observation_path: &Path,
) -> Result<()> {
    let report = output_dir.join("session-report.json");
    let payloads = output_dir.join("payloads");
    let plan = absolute_clean(plan_path)?;
    let report = absolute_clean(&report)?;
    let payloads = absolute_clean(&payloads)?;
    let output_dir = absolute_clean(output_dir)?;
    let observation = absolute_clean(observation_path)?;
    if plan.starts_with(&output_dir) || observation.starts_with(&output_dir) || plan == observation
    {
        bail!("session paths collide with reserved report or payload paths")
    }
    if trace_path != "-" {
        let trace = absolute_clean(Path::new(trace_path))?;
        if trace.starts_with(&output_dir) || trace == plan || trace == observation {
            bail!("session trace path collides with a reserved report or payload path")
        }
    }
    debug_assert!(report.starts_with(&output_dir));
    debug_assert!(payloads.starts_with(&output_dir));
    Ok(())
}

fn absolute_clean(path: &Path) -> Result<PathBuf> {
    let path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    let mut clean = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                clean.pop();
            }
            other => clean.push(other.as_os_str()),
        }
    }
    Ok(clean)
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionReport {
    pub schema: String,
    pub plan_schema: String,
    pub run_id: String,
    pub connection: String,
    pub status: RunStatus,
    pub steps: Vec<SessionStepReport>,
    pub terminal_error: Option<String>,
    pub cleanup_warning: Option<String>,
    pub artifacts: SessionArtifacts,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum RunStatus {
    Succeeded,
    Failed,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionStepReport {
    pub id: String,
    pub kind: SessionStepKind,
    pub status: RunStatus,
    pub session_index: Option<u32>,
    pub transaction_ids: Vec<u32>,
    pub outcome: Option<NormalizedOutcome>,
    pub payloads: Vec<PayloadReport>,
    pub cleanup_attempt: Option<CleanupAttemptReport>,
    pub switch: Option<SwitchReport>,
    pub expectation_mismatch: Option<ExpectationMismatch>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CleanupAttemptReport {
    pub action: String,
    pub status: RunStatus,
    pub transaction_ids: Vec<u32>,
    pub outcome: Option<NormalizedOutcome>,
    pub payloads: Vec<PayloadReport>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum SessionStepKind {
    Entry,
    Switch,
    Action,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NormalizedOutcome {
    pub steps_run: u32,
    pub scope: BTreeMap<String, SessionValue>,
    pub collections: BTreeMap<String, Vec<u64>>,
    pub output_count: usize,
    pub outputs: Vec<NormalizedOutput>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NormalizedOutput {
    pub step_path: String,
    pub transaction_id: u32,
    pub payload_bytes: u64,
    pub response_params: Vec<u32>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PayloadReport {
    pub path: String,
    pub length: u64,
    pub sha256: String,
    pub step_path: String,
    pub transaction_id: u32,
    pub response_params: Vec<u32>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SwitchReport {
    pub source_entry: Option<NormalizedOutcome>,
    pub exit: Option<NormalizedOutcome>,
    pub checkpoint: Option<BTreeMap<String, String>>,
    pub target_entry: Option<NormalizedOutcome>,
    pub before_session_index: u32,
    pub after_session_index: Option<u32>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ExpectationMismatch {
    pub path: String,
    pub expected: Value,
    pub actual: Value,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionArtifacts {
    pub report: String,
    pub trace: String,
    pub observation: String,
    pub payloads: String,
}

pub struct SessionRunOptions {
    pub output_dir: PathBuf,
    pub run_id: String,
    pub connection: String,
    pub trace_path: String,
    pub observation_path: PathBuf,
    pub runtime_params: Vec<PtpRuntimeValue>,
    pub handoff_timeout: Duration,
}

#[async_trait]
pub trait SessionLifecycle: Send + Sync {
    fn executor_transport(&self) -> Arc<dyn PtpExecutorTransport>;
    fn streaming_transport(&self) -> Arc<dyn PtpStreamingTransport>;
    async fn open_command_session(&self) -> Result<PtpSessionOpenResult, PtpTransportError>;
    async fn confirm_live_view_frame(&self) -> Result<usize, PtpTransportError>;
    async fn endpoint_accepts_tcp(&self) -> bool;
    async fn open_command_session_after_handoff(
        &self,
        deadline: Instant,
    ) -> Result<PtpSessionOpenResult, PtpTransportError>;
    async fn close_session_if_open(&self) -> Result<(), PtpTransportError>;
}

pub struct NativeSessionLifecycle {
    transport: Arc<NativePtpTransport>,
}

impl NativeSessionLifecycle {
    pub fn new(transport: Arc<NativePtpTransport>) -> Arc<Self> {
        Arc::new(Self { transport })
    }
}

#[async_trait]
impl SessionLifecycle for NativeSessionLifecycle {
    fn executor_transport(&self) -> Arc<dyn PtpExecutorTransport> {
        self.transport.clone()
    }

    fn streaming_transport(&self) -> Arc<dyn PtpStreamingTransport> {
        self.transport.clone()
    }

    async fn open_command_session(&self) -> Result<PtpSessionOpenResult, PtpTransportError> {
        self.transport.open_command_session().await
    }

    async fn confirm_live_view_frame(&self) -> Result<usize, PtpTransportError> {
        self.transport.confirm_live_view_frame().await
    }

    async fn endpoint_accepts_tcp(&self) -> bool {
        self.transport.endpoint_accepts_tcp().await
    }

    async fn open_command_session_after_handoff(
        &self,
        deadline: Instant,
    ) -> Result<PtpSessionOpenResult, PtpTransportError> {
        self.transport
            .open_command_session_after_handoff(deadline)
            .await
    }

    async fn close_session_if_open(&self) -> Result<(), PtpTransportError> {
        self.transport.close_session_if_open().await
    }
}

struct SessionStepObserver {
    trace: Arc<TraceWriter>,
    transaction_ids: Mutex<BTreeSet<u32>>,
}

impl SessionStepObserver {
    fn new(trace: Arc<TraceWriter>) -> Arc<Self> {
        Arc::new(Self {
            trace,
            transaction_ids: Mutex::new(BTreeSet::new()),
        })
    }

    fn reset(&self) {
        self.transaction_ids
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clear();
    }

    fn transaction_ids(&self) -> Vec<u32> {
        self.transaction_ids
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .iter()
            .copied()
            .collect()
    }
}

impl StepObserver for SessionStepObserver {
    fn on_step(&self, report: StepReport) {
        if let Some(transaction_id) = report.transaction_id {
            self.transaction_ids
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .insert(transaction_id);
        }
        let _ = self.trace.step(&report);
    }
}

struct SessionActivityObserver {
    trace: Arc<TraceWriter>,
}

impl ConnectionActivityObserver for SessionActivityObserver {
    fn on_activity(&self, event: ConnectionActivityEvent) {
        let _ = self.trace.activity(&event);
    }
}

struct OrdinarySessionSink {
    root: PathBuf,
    report_root: PathBuf,
    seen: std::sync::atomic::AtomicU64,
    payloads: Mutex<Vec<PayloadReport>>,
}

impl OrdinarySessionSink {
    fn new(root: PathBuf, report_root: PathBuf) -> Arc<Self> {
        Arc::new(Self {
            root,
            report_root,
            seen: std::sync::atomic::AtomicU64::new(0),
            payloads: Mutex::new(Vec::new()),
        })
    }

    fn payloads(&self) -> Vec<PayloadReport> {
        self.payloads
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }
}

#[async_trait]
impl PtpDataOutputSink for OrdinarySessionSink {
    async fn write(&self, output: PtpDataOutput) -> Result<(), PtpDataOutputSinkError> {
        let ordinal = self.seen.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        let filename = format!(
            "{ordinal:04}_{}_tid{:08x}.bin",
            sanitize_path(&output.step_path),
            output.transaction_id
        );
        let path = self.root.join(filename);
        let mut file = tokio::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .await
            .map_err(|error| PtpDataOutputSinkError::Failed {
                detail: format!("create new {}: {error}", path.display()),
            })?;
        file.write_all(&output.payload)
            .await
            .map_err(|error| PtpDataOutputSinkError::Failed {
                detail: format!("write {}: {error}", path.display()),
            })?;
        file.flush()
            .await
            .map_err(|error| PtpDataOutputSinkError::Failed {
                detail: format!("flush {}: {error}", path.display()),
            })?;
        file.sync_all()
            .await
            .map_err(|error| PtpDataOutputSinkError::Failed {
                detail: format!("sync {}: {error}", path.display()),
            })?;
        let mut metadata = PayloadMetadataBuilder::new();
        metadata.update(&output.payload);
        let metadata = metadata.metadata();
        let relative = path
            .strip_prefix(&self.report_root)
            .expect("session payload remains under output directory")
            .to_string_lossy()
            .into_owned();
        self.payloads
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(PayloadReport {
                path: relative,
                length: metadata.length,
                sha256: metadata.sha256,
                step_path: output.step_path,
                transaction_id: output.transaction_id,
                response_params: output.response_params,
            });
        Ok(())
    }
}

fn sanitize_path(value: &str) -> String {
    value
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || character == '-' || character == '_' {
                character
            } else {
                '_'
            }
        })
        .collect()
}

pub async fn run_session_plan(
    prepared: &PreparedSessionPlan,
    store: Arc<ConfigStore>,
    lifecycle: Arc<dyn SessionLifecycle>,
    trace: Arc<TraceWriter>,
    options: SessionRunOptions,
) -> Result<SessionReport> {
    tokio::fs::create_dir(&options.output_dir)
        .await
        .with_context(|| {
            format!(
                "create new session output directory {}",
                options.output_dir.display()
            )
        })?;
    let payload_root = options.output_dir.join("payloads");
    tokio::fs::create_dir(&payload_root)
        .await
        .with_context(|| format!("create payload directory {}", payload_root.display()))?;
    for (step_index, step) in prepared.steps.iter().enumerate() {
        if let PreparedStep::Action(action) = step {
            let step_dir = payload_root.join(format!("{step_index:04}-{}", action.id));
            tokio::fs::create_dir(&step_dir).await.with_context(|| {
                format!("create action payload directory {}", step_dir.display())
            })?;
        }
    }

    let artifacts = SessionArtifacts {
        report: "session-report.json".into(),
        trace: options.trace_path.clone(),
        observation: options.observation_path.to_string_lossy().into_owned(),
        payloads: "payloads".into(),
    };
    let mut report = SessionReport {
        schema: SESSION_REPORT_SCHEMA.into(),
        plan_schema: prepared.plan.schema.clone(),
        run_id: options.run_id.clone(),
        connection: options.connection.clone(),
        status: RunStatus::Succeeded,
        steps: Vec::new(),
        terminal_error: None,
        cleanup_warning: None,
        artifacts,
    };
    let observer = SessionStepObserver::new(Arc::clone(&trace));
    let activity: Arc<dyn ConnectionActivityObserver> = Arc::new(SessionActivityObserver {
        trace: Arc::clone(&trace),
    });
    let mut session_index = 0_u32;
    let mut retained_scope = BTreeMap::<String, SessionValue>::new();
    let mut live_view_started = false;

    for (step_index, step) in prepared.steps.iter().enumerate() {
        observer.reset();
        let mut failed_payloads = Vec::new();
        let mut failed_transaction_ids = None;
        let mut cleanup_attempt = None;
        let result = match step {
            PreparedStep::Entry(entry) => {
                async {
                    trace.set_mode(entry.to.clone());
                    if session_index == 0 {
                        lifecycle.open_command_session().await?;
                        session_index = 1;
                    }
                    let raw = lifecycle.executor_transport();
                    let outcome = run_mode_entry(
                        Arc::clone(&store),
                        options.connection.clone(),
                        entry.from.clone(),
                        entry.to.clone(),
                        raw,
                        observer.clone(),
                        Arc::clone(&activity),
                        entry_runtime_values(&options.runtime_params, &retained_scope)?,
                    )
                    .await
                    .map_err(|error| anyhow::anyhow!(error.to_string()))
                    .map(|outcome| {
                        merge_retained_scope(&mut retained_scope, &outcome);
                        let normalized = normalize_outcome(outcome, &[]);
                        let mismatch = entry
                            .expect
                            .as_ref()
                            .and_then(|expect| compare_expectation(expect, &normalized, "expect"));
                        SessionStepReport {
                            id: entry.id.clone(),
                            kind: SessionStepKind::Entry,
                            status: if mismatch.is_some() {
                                RunStatus::Failed
                            } else {
                                RunStatus::Succeeded
                            },
                            session_index: Some(session_index),
                            transaction_ids: observer.transaction_ids(),
                            outcome: Some(normalized),
                            payloads: Vec::new(),
                            cleanup_attempt: None,
                            switch: None,
                            expectation_mismatch: mismatch,
                            error: None,
                        }
                    });
                    outcome
                }
                .await
            }
            PreparedStep::Action(action) => {
                async {
                    trace.set_mode(action.request.mode.clone());
                    if session_index == 0 {
                        lifecycle.open_command_session().await?;
                        session_index = 1;
                    }
                    let step_dir_name = format!("{step_index:04}-{}", action.id);
                    let step_dir = payload_root.join(&step_dir_name);
                    let execution = execute_action(
                        action,
                        Arc::clone(&store),
                        Arc::clone(&lifecycle),
                        observer.clone(),
                        Arc::clone(&activity),
                        Arc::clone(&trace),
                        &options.output_dir,
                        &step_dir,
                    )
                    .await;
                    let mut transaction_ids = execution.transaction_ids;
                    transaction_ids.extend(observer.transaction_ids());
                    dedupe_adjacent(&mut transaction_ids);
                    if execution.outcome.is_err() {
                        failed_transaction_ids = Some(transaction_ids.clone());
                        failed_payloads = execution.payloads.clone();
                    }
                    let action_parameters = action
                        .parameters
                        .iter()
                        .map(|(name, value)| {
                            let value = match value {
                                SessionValue::U64(value) => json!(value),
                                SessionValue::String(value) => json!(value),
                            };
                            (name.clone(), value)
                        })
                        .collect();
                    trace.action(
                        action.request.catalog_revision.clone(),
                        action.name.clone(),
                        action_parameters,
                        if execution.outcome.is_ok() {
                            ActionOutcome::Succeeded
                        } else {
                            ActionOutcome::Failed
                        },
                    )?;
                    let result = execution.outcome.map(|outcome| {
                        match action.verb {
                            camera_protocol_ffi::ActionVerb::StartLiveView => {
                                live_view_started = true
                            }
                            camera_protocol_ffi::ActionVerb::StopLiveView => {
                                live_view_started = false
                            }
                            _ => {}
                        }
                        let mismatch = action
                            .expect
                            .as_ref()
                            .and_then(|expect| compare_expectation(expect, &outcome, "expect"));
                        SessionStepReport {
                            id: action.id.clone(),
                            kind: SessionStepKind::Action,
                            status: if mismatch.is_some() {
                                RunStatus::Failed
                            } else {
                                RunStatus::Succeeded
                            },
                            session_index: Some(session_index),
                            transaction_ids,
                            outcome: Some(outcome),
                            payloads: execution.payloads,
                            cleanup_attempt: None,
                            switch: None,
                            expectation_mismatch: mismatch,
                            error: None,
                        }
                    });
                    if result.is_err()
                        && should_run_stop_cleanup(
                            live_view_started,
                            action,
                            &prepared.steps[step_index + 1..],
                        )
                    {
                        if let Some(cleanup) = find_stop_cleanup(&prepared.steps[step_index + 1..])
                        {
                            trace.set_mode(cleanup.request.mode.clone());
                            observer.reset();
                            let cleanup_sink = OrdinarySessionSink::new(
                                step_dir.clone(),
                                options.output_dir.clone(),
                            );
                            let cleanup_result = run_initiator_action_to_sink(
                                Arc::clone(&store),
                                cleanup.request.clone(),
                                lifecycle.executor_transport(),
                                observer.clone(),
                                Arc::clone(&activity),
                                cleanup_sink.clone(),
                            )
                            .await;
                            let cleanup_transaction_ids = observer.transaction_ids();
                            let cleanup_payloads = cleanup_sink.payloads();
                            let cleanup_succeeded = cleanup_result.is_ok();
                            let (cleanup_outcome, cleanup_error) = match cleanup_result {
                                Ok(outcome) => {
                                    (Some(normalize_outcome(outcome, &cleanup_payloads)), None)
                                }
                                Err(error) => (None, Some(error.to_string())),
                            };
                            cleanup_attempt = Some(CleanupAttemptReport {
                                action: cleanup.name.clone(),
                                status: if cleanup_succeeded {
                                    RunStatus::Succeeded
                                } else {
                                    RunStatus::Failed
                                },
                                transaction_ids: cleanup_transaction_ids,
                                outcome: cleanup_outcome,
                                payloads: cleanup_payloads,
                                error: cleanup_error,
                            });
                            let cleanup_parameters = cleanup
                                .parameters
                                .iter()
                                .map(|(name, value)| {
                                    let value = match value {
                                        SessionValue::U64(value) => json!(value),
                                        SessionValue::String(value) => json!(value),
                                    };
                                    (name.clone(), value)
                                })
                                .collect();
                            trace.action(
                                cleanup.request.catalog_revision.clone(),
                                cleanup.name.clone(),
                                cleanup_parameters,
                                if cleanup_succeeded {
                                    ActionOutcome::Succeeded
                                } else {
                                    ActionOutcome::Failed
                                },
                            )?;
                        }
                    }
                    result
                }
                .await
            }
            PreparedStep::Switch(switch) => {
                run_switch_step(
                    switch,
                    Arc::clone(&store),
                    Arc::clone(&lifecycle),
                    observer.clone(),
                    Arc::clone(&activity),
                    Arc::clone(&trace),
                    &options,
                    &mut session_index,
                    &mut retained_scope,
                    &mut live_view_started,
                )
                .await
            }
        };

        match result {
            Ok(step_report) => {
                let failed = step_report.status == RunStatus::Failed;
                if failed {
                    report.status = RunStatus::Failed;
                    report.terminal_error = Some(step_report.error.clone().unwrap_or_else(|| {
                        format!("session step '{}' expectation failed", step_report.id)
                    }));
                }
                report.steps.push(step_report);
                if failed {
                    break;
                }
            }
            Err(error) => {
                report.status = RunStatus::Failed;
                report.terminal_error = Some(error.to_string());
                report.steps.push(failed_step_report(
                    step,
                    session_index,
                    failed_transaction_ids.unwrap_or_else(|| observer.transaction_ids()),
                    failed_payloads,
                    cleanup_attempt,
                    &error,
                ));
                break;
            }
        }
    }

    report.cleanup_warning = lifecycle
        .close_session_if_open()
        .await
        .err()
        .map(|error| error.to_string());
    publish_report(&options.output_dir.join("session-report.json"), &report).await?;
    if report.status == RunStatus::Failed {
        bail!(
            "{}",
            report
                .terminal_error
                .clone()
                .unwrap_or_else(|| "session failed".into())
        )
    }
    Ok(report)
}

fn failed_step_report(
    step: &PreparedStep,
    session_index: u32,
    transaction_ids: Vec<u32>,
    payloads: Vec<PayloadReport>,
    cleanup_attempt: Option<CleanupAttemptReport>,
    error: &anyhow::Error,
) -> SessionStepReport {
    let (id, kind) = match step {
        PreparedStep::Entry(step) => (step.id.clone(), SessionStepKind::Entry),
        PreparedStep::Switch(step) => (step.id.clone(), SessionStepKind::Switch),
        PreparedStep::Action(step) => (step.id.clone(), SessionStepKind::Action),
    };
    SessionStepReport {
        id,
        kind,
        status: RunStatus::Failed,
        session_index: (session_index != 0).then_some(session_index),
        transaction_ids,
        outcome: None,
        payloads,
        cleanup_attempt,
        switch: None,
        expectation_mismatch: None,
        error: Some(error.to_string()),
    }
}

fn entry_runtime_values(
    global: &[PtpRuntimeValue],
    retained: &BTreeMap<String, SessionValue>,
) -> Result<Vec<PtpRuntimeValue>> {
    let mut values: BTreeMap<String, u64> = global
        .iter()
        .map(|value| (value.key.clone(), value.value))
        .collect();
    for (key, value) in retained {
        let SessionValue::U64(value) = value else {
            bail!("mode-entry scope '{key}' is not numeric")
        };
        values.insert(key.clone(), *value);
    }
    Ok(values
        .into_iter()
        .map(|(key, value)| PtpRuntimeValue { key, value })
        .collect())
}

fn merge_retained_scope(
    retained: &mut BTreeMap<String, SessionValue>,
    outcome: &PtpExecutionOutcome,
) {
    for value in &outcome.scope {
        retained.insert(
            value.key.clone(),
            match &value.value {
                ActionValue::U64 { value } => SessionValue::U64(*value),
                ActionValue::String { value } => SessionValue::String(value.clone()),
            },
        );
    }
}

struct ActionExecution {
    outcome: Result<NormalizedOutcome>,
    payloads: Vec<PayloadReport>,
    transaction_ids: Vec<u32>,
}

#[allow(clippy::too_many_arguments)]
async fn execute_action(
    action: &PreparedAction,
    store: Arc<ConfigStore>,
    lifecycle: Arc<dyn SessionLifecycle>,
    observer: Arc<SessionStepObserver>,
    activity: Arc<dyn ConnectionActivityObserver>,
    trace: Arc<TraceWriter>,
    output_dir: &Path,
    step_dir: &Path,
) -> ActionExecution {
    if action.streaming {
        let result: Result<(NormalizedOutcome, PayloadReport, u32)> = async {
            let destination = step_dir.join("0000_stream.bin");
            let sink = Arc::new(StreamingFileSink::new(destination.clone())?);
            let outcome = run_streaming_action(
                Arc::clone(&store),
                action.request.clone(),
                lifecycle.streaming_transport(),
                sink.clone(),
                action.expected_bytes,
            )
            .await
            .map_err(|error| anyhow::anyhow!(error.to_string()))?;
            let metadata = sink.payload_metadata().await;
            trace.complete_streaming_transaction(
                outcome.transaction_id,
                outcome.response_params.clone(),
                metadata.clone(),
            )?;
            sink.commit().await?;
            let relative = destination
                .strip_prefix(output_dir)
                .expect("streaming payload remains under output directory")
                .to_string_lossy()
                .into_owned();
            let payload = PayloadReport {
                path: relative,
                length: metadata.length,
                sha256: metadata.sha256,
                step_path: "streamingAction".into(),
                transaction_id: outcome.transaction_id,
                response_params: outcome.response_params.clone(),
            };
            let normalized = NormalizedOutcome {
                steps_run: 1,
                scope: BTreeMap::new(),
                collections: BTreeMap::new(),
                output_count: 1,
                outputs: vec![NormalizedOutput {
                    step_path: payload.step_path.clone(),
                    transaction_id: payload.transaction_id,
                    payload_bytes: payload.length,
                    response_params: payload.response_params.clone(),
                }],
            };
            Ok((normalized, payload, outcome.transaction_id))
        }
        .await;
        return match result {
            Ok((outcome, payload, transaction_id)) => ActionExecution {
                outcome: Ok(outcome),
                payloads: vec![payload],
                transaction_ids: vec![transaction_id],
            },
            Err(error) => ActionExecution {
                outcome: Err(error),
                payloads: Vec::new(),
                transaction_ids: Vec::new(),
            },
        };
    }

    let sink = OrdinarySessionSink::new(step_dir.to_path_buf(), output_dir.to_path_buf());
    let outcome = run_initiator_action_to_sink(
        store,
        action.request.clone(),
        lifecycle.executor_transport(),
        observer,
        activity,
        sink.clone(),
    )
    .await
    .map_err(|error| anyhow::anyhow!(error.to_string()));
    let payloads = sink.payloads();
    ActionExecution {
        outcome: outcome.map(|outcome| normalize_outcome(outcome, &payloads)),
        payloads,
        transaction_ids: Vec::new(),
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_switch_step(
    switch: &PreparedSwitch,
    store: Arc<ConfigStore>,
    lifecycle: Arc<dyn SessionLifecycle>,
    observer: Arc<SessionStepObserver>,
    activity: Arc<dyn ConnectionActivityObserver>,
    trace: Arc<TraceWriter>,
    options: &SessionRunOptions,
    session_index: &mut u32,
    retained_scope: &mut BTreeMap<String, SessionValue>,
    live_view_started: &mut bool,
) -> Result<SessionStepReport> {
    trace.set_mode(switch.from.clone());
    if *session_index == 0 {
        lifecycle.open_command_session().await?;
        *session_index = 1;
    }
    let before_session_index = *session_index;
    let mut transaction_ids = Vec::new();
    let source_entry = if switch.cold_source {
        trace.set_mode(switch.from.clone());
        observer.reset();
        let runtime_values = match entry_runtime_values(&options.runtime_params, retained_scope) {
            Ok(values) => values,
            Err(error) => {
                return Ok(failed_switch_report(
                    switch,
                    before_session_index,
                    None,
                    None,
                    None,
                    None,
                    transaction_ids,
                    error.to_string(),
                ));
            }
        };
        let outcome = run_mode_entry(
            Arc::clone(&store),
            options.connection.clone(),
            None,
            switch.from.clone(),
            lifecycle.executor_transport(),
            observer.clone(),
            Arc::clone(&activity),
            runtime_values,
        )
        .await;
        let outcome = match outcome {
            Ok(outcome) => outcome,
            Err(error) => {
                transaction_ids.extend(observer.transaction_ids());
                return Ok(failed_switch_report(
                    switch,
                    before_session_index,
                    None,
                    None,
                    None,
                    None,
                    transaction_ids,
                    error.to_string(),
                ));
            }
        };
        transaction_ids.extend(observer.transaction_ids());
        merge_retained_scope(retained_scope, &outcome);
        Some(normalize_outcome(outcome, &[]))
    } else {
        None
    };

    if let Err(error) = lifecycle.confirm_live_view_frame().await {
        return Ok(failed_switch_report(
            switch,
            before_session_index,
            source_entry,
            None,
            None,
            None,
            transaction_ids,
            error.to_string(),
        ));
    }
    trace.set_mode(switch.from.clone());
    observer.reset();
    let runtime_values = match entry_runtime_values(&[], retained_scope) {
        Ok(values) => values,
        Err(error) => {
            return Ok(failed_switch_report(
                switch,
                before_session_index,
                source_entry,
                None,
                None,
                None,
                transaction_ids,
                error.to_string(),
            ));
        }
    };
    let exit = run_mode_reestablishment_exit(
        Arc::clone(&store),
        options.connection.clone(),
        Some(switch.from.clone()),
        switch.to.clone(),
        lifecycle.executor_transport(),
        observer.clone(),
        Arc::clone(&activity),
        runtime_values,
    )
    .await;
    transaction_ids.extend(observer.transaction_ids());
    let exit = match exit {
        Ok(exit) => exit,
        Err(error) => {
            return Ok(failed_switch_report(
                switch,
                before_session_index,
                source_entry,
                None,
                None,
                None,
                transaction_ids,
                error.to_string(),
            ));
        }
    };
    let exit = normalize_outcome(exit, &[]);
    if let Err(error) = trace.checkpoint("externalEstablishment", json!(switch.checkpoint)) {
        return Ok(failed_switch_report(
            switch,
            before_session_index,
            source_entry,
            Some(exit),
            None,
            None,
            transaction_ids,
            error.to_string(),
        ));
    }

    retained_scope.clear();
    let deadline = Instant::now() + options.handoff_timeout;
    while Instant::now() < deadline && lifecycle.endpoint_accepts_tcp().await {
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    if Instant::now() >= deadline {
        return Ok(failed_switch_report(
            switch,
            before_session_index,
            source_entry,
            Some(exit),
            Some(switch.checkpoint.clone()),
            None,
            transaction_ids,
            "handoff timed out before the old command endpoint became unavailable".into(),
        ));
    }
    trace.set_mode(switch.to.clone());
    if let Err(error) = lifecycle.open_command_session_after_handoff(deadline).await {
        return Ok(failed_switch_report(
            switch,
            before_session_index,
            source_entry,
            Some(exit),
            Some(switch.checkpoint.clone()),
            None,
            transaction_ids,
            error.to_string(),
        ));
    }
    *live_view_started = false;
    *session_index += 1;

    observer.reset();
    let target = run_mode_entry(
        store,
        options.connection.clone(),
        None,
        switch.to.clone(),
        lifecycle.executor_transport(),
        observer.clone(),
        activity,
        options.runtime_params.clone(),
    )
    .await;
    transaction_ids.extend(observer.transaction_ids());
    let target = match target {
        Ok(target) => target,
        Err(error) => {
            return Ok(failed_switch_report(
                switch,
                before_session_index,
                source_entry,
                Some(exit),
                Some(switch.checkpoint.clone()),
                Some(*session_index),
                transaction_ids,
                error.to_string(),
            ));
        }
    };
    merge_retained_scope(retained_scope, &target);
    let target = normalize_outcome(target, &[]);

    let mismatch = switch.expect.as_ref().and_then(|expect| {
        expect
            .source_entry
            .as_ref()
            .and_then(|expected| {
                compare_expectation(
                    expected,
                    source_entry
                        .as_ref()
                        .expect("sourceEntry expectation was statically validated"),
                    "expect.sourceEntry",
                )
            })
            .or_else(|| {
                expect
                    .exit
                    .as_ref()
                    .and_then(|expected| compare_expectation(expected, &exit, "expect.exit"))
            })
            .or_else(|| {
                expect.target_entry.as_ref().and_then(|expected| {
                    compare_expectation(expected, &target, "expect.targetEntry")
                })
            })
    });
    Ok(SessionStepReport {
        id: switch.id.clone(),
        kind: SessionStepKind::Switch,
        status: if mismatch.is_some() {
            RunStatus::Failed
        } else {
            RunStatus::Succeeded
        },
        session_index: Some(before_session_index),
        transaction_ids,
        outcome: None,
        payloads: Vec::new(),
        cleanup_attempt: None,
        switch: Some(SwitchReport {
            source_entry,
            exit: Some(exit),
            checkpoint: Some(switch.checkpoint.clone()),
            target_entry: Some(target),
            before_session_index,
            after_session_index: Some(*session_index),
        }),
        expectation_mismatch: mismatch,
        error: None,
    })
}

#[allow(clippy::too_many_arguments)]
fn failed_switch_report(
    switch: &PreparedSwitch,
    before_session_index: u32,
    source_entry: Option<NormalizedOutcome>,
    exit: Option<NormalizedOutcome>,
    checkpoint: Option<BTreeMap<String, String>>,
    after_session_index: Option<u32>,
    transaction_ids: Vec<u32>,
    error: String,
) -> SessionStepReport {
    SessionStepReport {
        id: switch.id.clone(),
        kind: SessionStepKind::Switch,
        status: RunStatus::Failed,
        session_index: Some(before_session_index),
        transaction_ids,
        outcome: None,
        payloads: Vec::new(),
        cleanup_attempt: None,
        switch: Some(SwitchReport {
            source_entry,
            exit,
            checkpoint,
            target_entry: None,
            before_session_index,
            after_session_index,
        }),
        expectation_mismatch: None,
        error: Some(error),
    }
}

fn normalize_outcome(
    outcome: PtpExecutionOutcome,
    payloads: &[PayloadReport],
) -> NormalizedOutcome {
    let scope = outcome
        .scope
        .into_iter()
        .map(|entry| {
            let value = match entry.value {
                ActionValue::U64 { value } => SessionValue::U64(value),
                ActionValue::String { value } => SessionValue::String(value),
            };
            (entry.key, value)
        })
        .collect();
    let collections = outcome
        .collections
        .into_iter()
        .map(|value| (value.key, value.values))
        .collect();
    let mut outputs: Vec<_> = outcome
        .outputs
        .into_iter()
        .map(|output| NormalizedOutput {
            step_path: output.step_path,
            transaction_id: output.transaction_id,
            payload_bytes: output.payload.len() as u64,
            response_params: output.response_params,
        })
        .collect();
    if outputs.is_empty() {
        outputs.extend(payloads.iter().map(|payload| NormalizedOutput {
            step_path: payload.step_path.clone(),
            transaction_id: payload.transaction_id,
            payload_bytes: payload.length,
            response_params: payload.response_params.clone(),
        }));
    }
    NormalizedOutcome {
        steps_run: outcome.steps_run,
        scope,
        collections,
        output_count: outputs.len(),
        outputs,
    }
}

fn compare_expectation(
    expected: &OutcomeExpectation,
    actual: &NormalizedOutcome,
    prefix: &str,
) -> Option<ExpectationMismatch> {
    if let Some(value) = expected.steps_run {
        if value != actual.steps_run {
            return Some(mismatch(
                format!("{prefix}.stepsRun"),
                json!(value),
                json!(actual.steps_run),
            ));
        }
    }
    for (key, value) in &expected.scope {
        let actual_value = actual.scope.get(key);
        if actual_value != Some(value) {
            return Some(mismatch(
                format!("{prefix}.scope.{key}"),
                serde_json::to_value(value).expect("session value serializes"),
                actual_value
                    .map(|value| serde_json::to_value(value).expect("session value serializes"))
                    .unwrap_or(Value::Null),
            ));
        }
    }
    for (key, values) in &expected.collections {
        let actual_values = actual.collections.get(key);
        if actual_values != Some(values) {
            return Some(mismatch(
                format!("{prefix}.collections.{key}"),
                json!(values),
                actual_values.map_or(Value::Null, |values| json!(values)),
            ));
        }
    }
    if let Some(value) = expected.output_count {
        if value != actual.output_count {
            return Some(mismatch(
                format!("{prefix}.outputCount"),
                json!(value),
                json!(actual.output_count),
            ));
        }
    }
    for output in &expected.outputs {
        let Some(actual_output) = actual.outputs.get(output.index) else {
            return Some(mismatch(
                format!("{prefix}.outputs[{}]", output.index),
                json!(output),
                Value::Null,
            ));
        };
        if let Some(value) = output.payload_bytes {
            if value != actual_output.payload_bytes {
                return Some(mismatch(
                    format!("{prefix}.outputs[{}].payloadBytes", output.index),
                    json!(value),
                    json!(actual_output.payload_bytes),
                ));
            }
        }
        if let Some(values) = &output.response_params {
            let actual_values: Vec<u64> = actual_output
                .response_params
                .iter()
                .copied()
                .map(u64::from)
                .collect();
            if values != &actual_values {
                return Some(mismatch(
                    format!("{prefix}.outputs[{}].responseParams", output.index),
                    json!(values),
                    json!(actual_values),
                ));
            }
        }
    }
    None
}

fn mismatch(path: String, expected: Value, actual: Value) -> ExpectationMismatch {
    ExpectationMismatch {
        path,
        expected,
        actual,
    }
}

fn dedupe_adjacent(values: &mut Vec<u32>) {
    values.dedup();
}

fn should_run_stop_cleanup(
    live_view_started: bool,
    failed: &PreparedAction,
    remaining: &[PreparedStep],
) -> bool {
    live_view_started
        && failed.verb != camera_protocol_ffi::ActionVerb::StopLiveView
        && find_stop_cleanup(remaining).is_some()
}

fn find_stop_cleanup(remaining: &[PreparedStep]) -> Option<&PreparedAction> {
    remaining
        .iter()
        .take_while(|step| matches!(step, PreparedStep::Action(_)))
        .find_map(|step| match step {
            PreparedStep::Action(action)
                if action.verb == camera_protocol_ffi::ActionVerb::StopLiveView =>
            {
                Some(action)
            }
            _ => None,
        })
}

#[async_trait]
trait ReportFileSystem: Send + Sync {
    async fn rename_noreplace(&self, source: &Path, destination: &Path) -> std::io::Result<()>;
    async fn hard_link(&self, source: &Path, destination: &Path) -> std::io::Result<()>;
    async fn create_new_synced_copy(
        &self,
        source: &Path,
        destination: &Path,
    ) -> std::io::Result<()>;
    async fn sync_parent(&self, path: &Path) -> std::io::Result<()>;
    async fn remove_file(&self, path: &Path) -> std::io::Result<()>;
}

struct TokioReportFileSystem;

#[async_trait]
impl ReportFileSystem for TokioReportFileSystem {
    async fn rename_noreplace(&self, source: &Path, destination: &Path) -> std::io::Result<()> {
        rename_noreplace(source, destination)
    }

    async fn hard_link(&self, source: &Path, destination: &Path) -> std::io::Result<()> {
        tokio::fs::hard_link(source, destination).await
    }

    async fn create_new_synced_copy(
        &self,
        source: &Path,
        destination: &Path,
    ) -> std::io::Result<()> {
        copy_new_report_file(source, destination).await
    }

    async fn sync_parent(&self, path: &Path) -> std::io::Result<()> {
        let parent = path.parent().unwrap_or_else(|| Path::new("."));
        tokio::fs::File::open(parent).await?.sync_all().await
    }

    async fn remove_file(&self, path: &Path) -> std::io::Result<()> {
        tokio::fs::remove_file(path).await
    }
}

async fn publish_report(path: &Path, report: &SessionReport) -> Result<()> {
    publish_report_with_fs(path, report, &TokioReportFileSystem).await
}

async fn publish_report_with_fs(
    path: &Path,
    report: &SessionReport,
    file_system: &dyn ReportFileSystem,
) -> Result<()> {
    let mut temporary = path.as_os_str().to_os_string();
    temporary.push(format!(".partial-{}", std::process::id()));
    let temporary = PathBuf::from(temporary);
    let mut bytes = serde_json::to_vec_pretty(report)?;
    bytes.push(b'\n');
    write_new_report_file(&temporary, &bytes)
        .await
        .with_context(|| format!("create new report staging file {}", temporary.display()))?;
    let staging_remains = match file_system.rename_noreplace(&temporary, path).await {
        Ok(()) => false,
        Err(error) if rename_noreplace_is_unsupported(&error) => {
            match file_system.hard_link(&temporary, path).await {
                Ok(()) => true,
                Err(error) if hard_link_is_unsupported(&error) => {
                    file_system
                        .create_new_synced_copy(&temporary, path)
                        .await
                        .with_context(|| format!("copy new session report {}", path.display()))?;
                    true
                }
                Err(error) => {
                    return Err(error)
                        .with_context(|| format!("link new session report {}", path.display()));
                }
            }
        }
        Err(error) => {
            return Err(error)
                .with_context(|| format!("publish new session report {}", path.display()));
        }
    };
    file_system
        .sync_parent(path)
        .await
        .with_context(|| format!("sync session report directory for {}", path.display()))?;
    if staging_remains {
        let _ = file_system.remove_file(&temporary).await;
    }
    Ok(())
}

async fn write_new_report_file(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let mut file = tokio::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .await?;
    file.write_all(bytes).await?;
    file.sync_all().await?;
    drop(file);
    Ok(())
}

async fn copy_new_report_file(source: &Path, destination: &Path) -> std::io::Result<()> {
    let mut source = tokio::fs::File::open(source).await?;
    let mut destination_file = tokio::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(destination)
        .await?;
    let result = async {
        tokio::io::copy(&mut source, &mut destination_file).await?;
        destination_file.sync_all().await
    }
    .await;
    drop(destination_file);
    if let Err(error) = result {
        let _ = tokio::fs::remove_file(destination).await;
        return Err(error);
    }
    Ok(())
}

#[cfg(any(
    target_os = "android",
    target_os = "linux",
    target_os = "macos",
    target_os = "ios",
    target_os = "tvos",
    target_os = "visionos",
    target_os = "watchos",
    target_os = "redox",
))]
fn rename_noreplace(source: &Path, destination: &Path) -> std::io::Result<()> {
    use rustix::fs::{renameat_with, RenameFlags, CWD};

    renameat_with(CWD, source, CWD, destination, RenameFlags::NOREPLACE)
        .map_err(std::io::Error::from)
}

#[cfg(not(any(
    target_os = "android",
    target_os = "linux",
    target_os = "macos",
    target_os = "ios",
    target_os = "tvos",
    target_os = "visionos",
    target_os = "watchos",
    target_os = "redox",
)))]
fn rename_noreplace(_source: &Path, _destination: &Path) -> std::io::Result<()> {
    Err(std::io::ErrorKind::Unsupported.into())
}

fn rename_noreplace_is_unsupported(error: &std::io::Error) -> bool {
    matches!(
        error.kind(),
        std::io::ErrorKind::InvalidInput | std::io::ErrorKind::Unsupported
    ) || matches!(error.raw_os_error(), Some(22 | 38 | 45 | 95))
}

fn hard_link_is_unsupported(error: &std::io::Error) -> bool {
    matches!(
        error.kind(),
        std::io::ErrorKind::PermissionDenied | std::io::ErrorKind::Unsupported
    ) || matches!(error.raw_os_error(), Some(45 | 50 | 95 | 1314))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::io::{self, Write};
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    use camera_config::{
        direct_epistemic, no_loss, validate_bundles, BundleHeader, CameraContext, CaptureClock,
        CaptureContext, CaptureInterface, CaptureInterfaceType, ClientContext, ClockType,
        ClockUnit, ObservationLine, ObservationRecorder,
    };
    use camera_protocol_ffi::SocketRole;
    use ptp_core::{OperationRequest, OperationResponse, PtpIpPacket};

    fn store() -> Arc<ConfigStore> {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let body = std::fs::read_to_string(
            root.join("packages/camera-config-data/fuji/gfx100ii/gfx100ii.yaml"),
        )
        .unwrap();
        let manufacturer =
            std::fs::read_to_string(root.join("packages/camera-config-data/fuji/fuji.yaml"))
                .unwrap();
        ConfigStore::from_tiers(body, Some(manufacturer), Vec::new()).unwrap()
    }

    fn prepare(yaml: &str) -> Result<PreparedSessionPlan> {
        prepare_session_plan(SessionPlan::from_yaml(yaml)?, store().as_ref(), "app")
    }

    #[test]
    fn parses_and_prepares_retained_action_plan() {
        let plan = prepare(
            r#"
schema: camera-initiator-session/v1
steps:
  - { id: enter, kind: entry, to: shooting/stills }
  - id: lock
    kind: action
    action: autofocusLock
    parameters: { afArea: 7 }
    expect:
      stepsRun: 2
      scope: { focusResult: 1 }
      collections: { handles: [1, 2] }
      outputCount: 1
      outputs: [{ index: 0, payloadBytes: 4, responseParams: [1] }]
  - { id: release, kind: action, action: autofocusRelease }
"#,
        )
        .unwrap();
        assert_eq!(plan.steps.len(), 3);
    }

    #[test]
    fn strict_parser_rejects_schema_kinds_fields_ids_and_assertion_fields() {
        let rejected = [
            "schema: wrong\nsteps: [{ id: one, kind: entry, to: shooting/stills }]\n",
            "schema: camera-initiator-session/v1\nsteps: []\n",
            "schema: camera-initiator-session/v1\nsteps: [{ id: one, kind: assert }]\n",
            "schema: camera-initiator-session/v1\nsteps: [{ id: one, kind: entry, to: shooting/stills, extra: true }]\n",
            "schema: camera-initiator-session/v1\nsteps: [{ id: ../bad, kind: entry, to: shooting/stills }]\n",
            "schema: camera-initiator-session/v1\nsteps: [{ id: same, kind: entry, to: shooting/stills }, { id: same, kind: action, action: autofocusRelease }]\n",
            "schema: camera-initiator-session/v1\nsteps: [{ id: one, kind: entry, to: shooting/stills, expect: { operation: 1 } }]\n",
            "schema: camera-initiator-session/v1\nsteps: [{ id: one, kind: entry, to: shooting/stills, expect: { outputs: [{ index: 0, operation: 1 }] } }]\n",
        ];
        for yaml in rejected {
            assert!(SessionPlan::from_yaml(yaml).is_err(), "accepted:\n{yaml}");
        }
    }

    #[test]
    fn preparation_rejects_modes_variants_and_progression() {
        let rejected = [
            r#"schema: camera-initiator-session/v1
steps: [{ id: bad, kind: entry, to: unknown }]
"#,
            r#"schema: camera-initiator-session/v1
steps:
  - { id: enter, kind: entry, to: shooting/stills }
  - { id: wrong, kind: entry, from: shooting/video, to: shooting/stills }
"#,
            r#"schema: camera-initiator-session/v1
steps:
  - { id: enter, kind: entry, to: shooting/stills }
  - { id: wrong-kind, kind: entry, from: shooting/stills, to: image-transfer }
"#,
            r#"schema: camera-initiator-session/v1
steps:
  - { id: enter, kind: entry, to: shooting/stills }
  - { id: wrong-source, kind: switch, from: shooting/video, to: image-transfer }
"#,
            r#"schema: camera-initiator-session/v1
steps:
  - { id: enter, kind: entry, to: shooting/stills }
  - id: switch
    kind: switch
    from: shooting/stills
    to: image-transfer
    expect: { sourceEntry: { stepsRun: 1 } }
"#,
        ];
        for yaml in rejected {
            assert!(prepare(yaml).is_err(), "accepted:\n{yaml}");
        }
    }

    #[test]
    fn preparation_rejects_action_names_modes_parameters_and_expected_bytes() {
        let rejected = [
            r#"schema: camera-initiator-session/v1
steps: [{ id: action, kind: action, action: autofocusRelease }]
"#,
            r#"schema: camera-initiator-session/v1
steps:
  - { id: enter, kind: entry, to: shooting/stills }
  - { id: action, kind: action, action: AutofocusRelease }
"#,
            r#"schema: camera-initiator-session/v1
steps:
  - { id: enter, kind: entry, to: shooting/stills }
  - { id: action, kind: action, action: enumerateObjects }
"#,
            r#"schema: camera-initiator-session/v1
steps:
  - { id: enter, kind: entry, to: shooting/stills }
  - { id: action, kind: action, action: autofocusLock }
"#,
            r#"schema: camera-initiator-session/v1
steps:
  - { id: enter, kind: entry, to: shooting/stills }
  - { id: action, kind: action, action: autofocusLock, parameters: { afArea: text } }
"#,
            r#"schema: camera-initiator-session/v1
steps:
  - { id: enter, kind: entry, to: shooting/stills }
  - { id: action, kind: action, action: autofocusLock, parameters: { afArea: 1, extra: 2 } }
"#,
            r#"schema: camera-initiator-session/v1
steps:
  - { id: enter, kind: entry, to: shooting/stills }
  - { id: action, kind: action, action: autofocusRelease, expectedBytes: 4 }
"#,
        ];
        for yaml in rejected {
            assert!(prepare(yaml).is_err(), "accepted:\n{yaml}");
        }
    }

    #[test]
    fn expectation_indexes_are_checked_before_execution() {
        let duplicate = r#"schema: camera-initiator-session/v1
steps:
  - id: enter
    kind: entry
    to: shooting/stills
    expect:
      outputCount: 1
      outputs: [{ index: 0 }, { index: 0 }]
"#;
        assert!(SessionPlan::from_yaml(duplicate).is_err());
        let outside = duplicate.replace("{ index: 0 }, { index: 0 }", "{ index: 1 }");
        assert!(SessionPlan::from_yaml(&outside).is_err());
    }

    #[derive(Clone, Default)]
    struct TestTraceBuffer(Arc<Mutex<Vec<u8>>>);

    impl TestTraceBuffer {
        fn records(&self) -> Vec<Value> {
            String::from_utf8(self.0.lock().unwrap().clone())
                .unwrap()
                .lines()
                .map(|line| serde_json::from_str(line).unwrap())
                .collect()
        }
    }

    impl Write for TestTraceBuffer {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    struct FakeTransport {
        next_tid: AtomicU32,
        replies: Mutex<VecDeque<Vec<u8>>>,
        response_codes: Mutex<BTreeMap<u16, u16>>,
        data_replies: Mutex<BTreeMap<u16, Vec<u8>>>,
        calls: Arc<Mutex<Vec<String>>>,
        trace: Arc<TraceWriter>,
    }

    impl FakeTransport {
        fn new(calls: Arc<Mutex<Vec<String>>>, trace: Arc<TraceWriter>) -> Arc<Self> {
            Arc::new(Self {
                next_tid: AtomicU32::new(2),
                replies: Mutex::new(VecDeque::new()),
                response_codes: Mutex::new(BTreeMap::new()),
                data_replies: Mutex::new(BTreeMap::new()),
                calls,
                trace,
            })
        }

        fn respond_with(&self, operation: u16, response_code: u16) {
            self.response_codes
                .lock()
                .unwrap()
                .insert(operation, response_code);
        }

        fn respond_with_data(&self, operation: u16, payload: Vec<u8>) {
            self.data_replies.lock().unwrap().insert(operation, payload);
        }

        fn reset(&self) {
            self.next_tid.store(2, Ordering::SeqCst);
        }

        fn encode(packet: &PtpIpPacket) -> Result<Vec<u8>, PtpTransportError> {
            protocol_primitives::fuji_framing::encode(packet).map_err(|error| {
                PtpTransportError::Failed {
                    detail: error.to_string(),
                }
            })
        }
    }

    #[async_trait]
    impl PtpExecutorTransport for FakeTransport {
        async fn reserve_transaction_id(&self) -> Result<u32, PtpTransportError> {
            Ok(self.next_tid.fetch_add(1, Ordering::SeqCst))
        }

        async fn send_command_frame(&self, frame: Vec<u8>) -> Result<(), PtpTransportError> {
            self.trace
                .ptp_frame("tx", camera_protocol_ffi::PtpFraming::Compressed, &frame)
                .map_err(|error| PtpTransportError::Failed {
                    detail: error.to_string(),
                })?;
            let packet = protocol_primitives::fuji_framing::decode(&frame).map_err(|error| {
                PtpTransportError::Failed {
                    detail: error.to_string(),
                }
            })?;
            let PtpIpPacket::OperationRequest(request) = packet else {
                return Err(PtpTransportError::Failed {
                    detail: "fake expected operation request".into(),
                });
            };
            self.calls.lock().unwrap().push(format!(
                "op:{:04x}:{}:{:?}",
                request.code, request.transaction_id, request.params
            ));
            if let Some(payload) = self
                .data_replies
                .lock()
                .unwrap()
                .get(&request.code)
                .cloned()
            {
                self.replies.lock().unwrap().push_back(
                    protocol_primitives::fuji_framing::encode_data(
                        request.code,
                        request.transaction_id,
                        &payload,
                    ),
                );
            }
            let response_code = self
                .response_codes
                .lock()
                .unwrap()
                .get(&request.code)
                .copied()
                .unwrap_or(0x2001);
            let response = Self::encode(&PtpIpPacket::OperationResponse(OperationResponse {
                code: response_code,
                transaction_id: request.transaction_id,
                params: Vec::new(),
            }))?;
            self.replies.lock().unwrap().push_back(response);
            Ok(())
        }

        async fn next_command_frame(&self) -> Result<Vec<u8>, PtpTransportError> {
            let frame = self.replies.lock().unwrap().pop_front().ok_or_else(|| {
                PtpTransportError::Failed {
                    detail: "fake reply queue is empty".into(),
                }
            })?;
            self.trace
                .ptp_frame("rx", camera_protocol_ffi::PtpFraming::Compressed, &frame)
                .map_err(|error| PtpTransportError::Failed {
                    detail: error.to_string(),
                })?;
            Ok(frame)
        }

        async fn next_event_frame(&self, _event_code: u16) -> Result<Vec<u8>, PtpTransportError> {
            Err(PtpTransportError::Failed {
                detail: "fake has no event channel".into(),
            })
        }

        async fn open_channel(&self, role: SocketRole) -> Result<(), PtpTransportError> {
            self.calls.lock().unwrap().push(format!("channel:{role:?}"));
            Ok(())
        }

        async fn close_command_channel(
            &self,
            _transport_close_frame: Option<Vec<u8>>,
        ) -> Result<(), PtpTransportError> {
            self.calls.lock().unwrap().push("transport-close".into());
            self.trace
                .session("fakeClosed", json!({}))
                .map_err(|error| PtpTransportError::Failed {
                    detail: error.to_string(),
                })?;
            Ok(())
        }

        async fn reopen_command_session(&self) -> Result<PtpSessionOpenResult, PtpTransportError> {
            Err(PtpTransportError::Failed {
                detail: "fake lifecycle owns replacement".into(),
            })
        }

        async fn sleep(&self, ms: u32) -> Result<(), PtpTransportError> {
            tokio::time::sleep(Duration::from_millis(u64::from(ms))).await;
            Ok(())
        }
    }

    #[async_trait]
    impl PtpStreamingTransport for FakeTransport {
        async fn reserve_transaction_id(&self) -> Result<u32, PtpTransportError> {
            PtpExecutorTransport::reserve_transaction_id(self).await
        }

        async fn send_command_frame(&self, frame: Vec<u8>) -> Result<(), PtpTransportError> {
            PtpExecutorTransport::send_command_frame(self, frame).await
        }

        async fn receive_command_bytes(
            &self,
            _max_bytes: u32,
        ) -> Result<Vec<u8>, PtpTransportError> {
            Err(PtpTransportError::Failed {
                detail: "fake has no streaming data".into(),
            })
        }

        async fn sleep(&self, ms: u32) -> Result<(), PtpTransportError> {
            tokio::time::sleep(Duration::from_millis(u64::from(ms))).await;
            Ok(())
        }

        fn invalidate_command_session(&self, reason: String) {
            self.calls
                .lock()
                .unwrap()
                .push(format!("invalidate:{reason}"));
        }
    }

    struct FakeLifecycle {
        transport: Arc<FakeTransport>,
        trace: Arc<TraceWriter>,
        calls: Arc<Mutex<Vec<String>>>,
        generation: AtomicU32,
    }

    impl FakeLifecycle {
        fn record_open(&self, generation: u32) -> Result<PtpSessionOpenResult, PtpTransportError> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("open:{generation}"));
            self.trace
                .begin_ptp_session()
                .map_err(|error| PtpTransportError::Failed {
                    detail: error.to_string(),
                })?;
            let request =
                FakeTransport::encode(&PtpIpPacket::OperationRequest(OperationRequest {
                    data_phase_info: 1,
                    code: 0x1002,
                    transaction_id: 1,
                    params: vec![1],
                }))?;
            let response =
                FakeTransport::encode(&PtpIpPacket::OperationResponse(OperationResponse {
                    code: 0x2001,
                    transaction_id: 1,
                    params: Vec::new(),
                }))?;
            self.trace
                .ptp_frame("tx", camera_protocol_ffi::PtpFraming::Compressed, &request)
                .and_then(|_| {
                    self.trace.ptp_frame(
                        "rx",
                        camera_protocol_ffi::PtpFraming::Compressed,
                        &response,
                    )
                })
                .map_err(|error| PtpTransportError::Failed {
                    detail: error.to_string(),
                })?;
            self.trace
                .session("fakeOpened", json!({ "generation": generation }))
                .map_err(|error| PtpTransportError::Failed {
                    detail: error.to_string(),
                })?;
            self.transport.reset();
            Ok(PtpSessionOpenResult {
                transaction_id: 1,
                response_code: 0x2001,
                response_params: Vec::new(),
            })
        }
    }

    #[async_trait]
    impl SessionLifecycle for FakeLifecycle {
        fn executor_transport(&self) -> Arc<dyn PtpExecutorTransport> {
            self.transport.clone()
        }

        fn streaming_transport(&self) -> Arc<dyn PtpStreamingTransport> {
            self.transport.clone()
        }

        async fn open_command_session(&self) -> Result<PtpSessionOpenResult, PtpTransportError> {
            self.generation.store(1, Ordering::SeqCst);
            self.record_open(1)
        }

        async fn confirm_live_view_frame(&self) -> Result<usize, PtpTransportError> {
            self.calls.lock().unwrap().push("confirm-source".into());
            Ok(10)
        }

        async fn endpoint_accepts_tcp(&self) -> bool {
            self.calls.lock().unwrap().push("endpoint-gone".into());
            false
        }

        async fn open_command_session_after_handoff(
            &self,
            _deadline: Instant,
        ) -> Result<PtpSessionOpenResult, PtpTransportError> {
            let generation = self.generation.fetch_add(1, Ordering::SeqCst) + 1;
            self.record_open(generation)
        }

        async fn close_session_if_open(&self) -> Result<(), PtpTransportError> {
            self.calls.lock().unwrap().push("cleanup".into());
            Ok(())
        }
    }

    fn fake_switch_store() -> Arc<ConfigStore> {
        ConfigStore::from_bundle(
            r#"
schema: camera-config/v1
camera: { manufacturer: Test, model: Session, firmware: "1" }
connections:
  app:
    commandFraming: compressed
    establishment: fake-handoff
    modes: [source, target]
    entries:
      - to: source
        steps:
          - { sendOp: "0x9001", captures: [{ bind: sourceTid, as: transactionId }] }
      - from: source
        to: target
        reestablishConnection:
          params: { launchMode: "3" }
          exitSteps:
            - { sendOp: "0x9002", params: [{ runtime: sourceTid }] }
            - { closeSession: { transportClose: false } }
      - to: target
        steps: [{ sendOp: "0x9003" }]
    actions:
      startLiveView:
        mode: ""
        initiator:
          steps: [{ sendOp: "0x9100" }]
      shutter:
        mode: ""
        initiator:
          steps:
            - { sendOp: "0x9101" }
            - { sendOp: "0x9102" }
      stopLiveView:
        mode: ""
        initiator:
          steps: [{ sendOp: "0x9103" }]
"#
            .into(),
            None,
        )
        .unwrap()
    }

    fn fake_string_switch_store() -> Arc<ConfigStore> {
        ConfigStore::from_bundle(
            r#"
schema: camera-config/v1
camera: { manufacturer: Test, model: Session, firmware: "1" }
properties:
  "0xd001": { name: sourceLabel, type: str, access: readOnly }
connections:
  app:
    commandFraming: compressed
    establishment: fake-handoff
    modes: [source, target]
    entries:
      - to: source
        steps:
          - { sendOp: "0x9001", captures: [{ bind: sourceTid, as: transactionId }] }
          - { getProp: "0xd001", captures: [{ bind: sourceLabel, as: propValue }] }
      - from: source
        to: target
        reestablishConnection:
          params: { launchMode: "3" }
          exitSteps:
            - { sendOp: "0x9002", params: [{ runtime: sourceTid }] }
            - { closeSession: { transportClose: false } }
      - to: target
        steps: [{ sendOp: "0x9003" }]
    actions:
      shutter:
        mode: ""
        initiator:
          steps: [{ sendOp: "0x9101" }]
"#
            .into(),
            None,
        )
        .unwrap()
    }

    fn observation_recorder(path: PathBuf) -> ObservationRecorder {
        ObservationRecorder::open(
            Some(path),
            BundleHeader {
                schema: camera_config::OBSERVATION_SCHEMA_VERSION.into(),
                run_id: "fake-switch".into(),
                record_id: "header".into(),
                ordinal: 0,
                camera: CameraContext {
                    manufacturer: "Test".into(),
                    model: "Session".into(),
                    body_id: "fake".into(),
                    firmware: "1".into(),
                },
                client: ClientContext {
                    artifact: "camera-initiator-test".into(),
                    version: "test".into(),
                    platform: "test".into(),
                },
                capture: CaptureContext {
                    interfaces: vec![CaptureInterface {
                        id: "fake".into(),
                        interface_type: CaptureInterfaceType::Tcp,
                        role: "initiator".into(),
                    }],
                    clocks: vec![CaptureClock {
                        id: "process-monotonic".into(),
                        clock_type: ClockType::Monotonic,
                        unit: ClockUnit::Milliseconds,
                    }],
                    clock_mappings: Vec::new(),
                    loss: no_loss(),
                    redactions: Vec::new(),
                    tool_versions: BTreeMap::from([(
                        "camera-initiator-test".into(),
                        "test".into(),
                    )]),
                    artifacts: Vec::new(),
                },
                epistemic: direct_epistemic(),
            },
        )
        .unwrap()
    }

    async fn run_fake_session(
        yaml: &str,
        response_codes: &[(u16, u16)],
        data_replies: &[(u16, &[u8])],
    ) -> (
        Result<SessionReport>,
        PathBuf,
        PathBuf,
        Arc<Mutex<Vec<String>>>,
    ) {
        run_fake_session_with_store(fake_switch_store(), yaml, response_codes, data_replies).await
    }

    async fn run_fake_session_with_store(
        store: Arc<ConfigStore>,
        yaml: &str,
        response_codes: &[(u16, u16)],
        data_replies: &[(u16, &[u8])],
    ) -> (
        Result<SessionReport>,
        PathBuf,
        PathBuf,
        Arc<Mutex<Vec<String>>>,
    ) {
        let plan = SessionPlan::from_yaml(yaml).unwrap();
        let prepared = prepare_session_plan(plan, &store, "app").unwrap();
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "camera-initiator-fake-session-{}-{nonce}",
            std::process::id()
        ));
        let observation = root.with_extension("jsonl");
        let trace = Arc::new(TraceWriter::with_observations(
            crate::TraceFormat::Jsonl,
            Box::new(TestTraceBuffer::default()),
            observation_recorder(observation.clone()),
            "app".into(),
            "session".into(),
        ));
        let calls = Arc::new(Mutex::new(Vec::new()));
        let transport = FakeTransport::new(Arc::clone(&calls), Arc::clone(&trace));
        for (operation, response_code) in response_codes {
            transport.respond_with(*operation, *response_code);
        }
        for (operation, payload) in data_replies {
            transport.respond_with_data(*operation, payload.to_vec());
        }
        let lifecycle: Arc<dyn SessionLifecycle> = Arc::new(FakeLifecycle {
            transport,
            trace: Arc::clone(&trace),
            calls: Arc::clone(&calls),
            generation: AtomicU32::new(0),
        });
        let result = run_session_plan(
            &prepared,
            store,
            lifecycle,
            trace,
            SessionRunOptions {
                output_dir: root.clone(),
                run_id: "fake-session".into(),
                connection: "app".into(),
                trace_path: "-".into(),
                observation_path: observation.clone(),
                runtime_params: Vec::new(),
                handoff_timeout: Duration::from_secs(1),
            },
        )
        .await;
        (result, root, observation, calls)
    }

    #[tokio::test]
    async fn cold_source_runtime_failure_preserves_completed_switch_phase() {
        let source_label = [
            7, b'o', 0, b'p', 0, b'a', 0, b'q', 0, b'u', 0, b'e', 0, 0, 0,
        ];
        let (result, root, observation, calls) = run_fake_session_with_store(
            fake_string_switch_store(),
            r#"
schema: camera-initiator-session/v1
steps:
  - { id: switch, kind: switch, from: source, to: target }
  - { id: later, kind: action, action: shutter }
"#,
            &[],
            &[(0x1015, &source_label)],
        )
        .await;
        result.expect_err("retained string scope must fail the switch");

        let report: Value =
            serde_json::from_slice(&std::fs::read(root.join("session-report.json")).unwrap())
                .unwrap();
        assert_eq!(report["steps"].as_array().unwrap().len(), 1);
        let failed = &report["steps"][0];
        assert!(!failed["transactionIds"].as_array().unwrap().is_empty());
        assert!(!failed["switch"]["sourceEntry"].is_null());
        assert_eq!(
            failed["switch"]["sourceEntry"]["scope"]["sourceLabel"],
            "opaque"
        );
        assert_eq!(failed["sessionIndex"], 1);
        assert_eq!(failed["switch"]["beforeSessionIndex"], 1);
        assert!(failed["switch"]["afterSessionIndex"].is_null());
        assert!(failed["error"].as_str().unwrap().contains("sourceLabel"));
        assert!(failed["switch"]["exit"].is_null());
        assert!(failed["switch"]["checkpoint"].is_null());
        assert!(failed["switch"]["targetEntry"].is_null());
        assert!(!calls
            .lock()
            .unwrap()
            .iter()
            .any(|call| call.starts_with("op:9002:") || call.starts_with("op:9101:")));

        let _ = std::fs::remove_dir_all(root);
        let _ = std::fs::remove_file(observation);
    }

    #[tokio::test]
    async fn failed_action_reports_payloads_and_rejected_cleanup_separately() {
        let (result, root, observation, _) = run_fake_session(
            r#"
schema: camera-initiator-session/v1
steps:
  - { id: start, kind: action, action: startLiveView }
  - { id: fail, kind: action, action: shutter }
  - { id: stop, kind: action, action: stopLiveView }
"#,
            &[(0x9102, 0x2002), (0x9103, 0x2002)],
            &[(0x9101, b"action-payload"), (0x9103, b"cleanup-payload")],
        )
        .await;
        result.expect_err("action failure must fail the session");

        let report: Value =
            serde_json::from_slice(&std::fs::read(root.join("session-report.json")).unwrap())
                .unwrap();
        let failed = &report["steps"][1];
        let cleanup = &failed["cleanupAttempt"];
        let payloads: Vec<_> = failed["payloads"]
            .as_array()
            .unwrap()
            .iter()
            .chain(cleanup["payloads"].as_array().unwrap())
            .collect();
        let files: Vec<_> = std::fs::read_dir(root.join("payloads/0001-fail"))
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect();
        assert_eq!(payloads.len(), files.len());
        for payload in payloads {
            let bytes = std::fs::read(root.join(payload["path"].as_str().unwrap())).unwrap();
            let mut metadata = PayloadMetadataBuilder::new();
            metadata.update(&bytes);
            let metadata = metadata.metadata();
            assert_eq!(payload["length"], metadata.length);
            assert_eq!(payload["sha256"], metadata.sha256);
        }
        let action_ids = failed["transactionIds"].as_array().unwrap();
        assert_eq!(cleanup["action"], "stopLiveView");
        assert_eq!(cleanup["status"], "failed");
        assert!(cleanup["error"].as_str().unwrap().contains("0x2002"));
        let cleanup_ids = cleanup["transactionIds"].as_array().unwrap();
        assert_eq!(action_ids.len(), 2);
        assert_eq!(cleanup_ids.len(), 1);
        assert!(action_ids
            .iter()
            .all(|transaction_id| !cleanup_ids.contains(transaction_id)));
        assert_eq!(cleanup["payloads"].as_array().unwrap().len(), 1);

        let _ = std::fs::remove_dir_all(root);
        let _ = std::fs::remove_file(observation);
    }

    #[tokio::test]
    async fn replacement_session_clears_live_view_cleanup_state() {
        let (result, root, observation, calls) = run_fake_session(
            r#"
schema: camera-initiator-session/v1
steps:
  - { id: start, kind: action, action: startLiveView }
  - { id: switch, kind: switch, from: source, to: target }
  - { id: fail, kind: action, action: shutter }
  - { id: stop, kind: action, action: stopLiveView }
"#,
            &[(0x9102, 0x2002)],
            &[],
        )
        .await;
        result.expect_err("action failure must fail the session");
        let calls = calls.lock().unwrap();
        let replacement = calls.iter().position(|call| call == "open:2").unwrap();
        assert!(!calls[replacement..]
            .iter()
            .any(|call| call.starts_with("op:9103:")));

        let _ = std::fs::remove_dir_all(root);
        let _ = std::fs::remove_file(observation);
    }

    #[test]
    fn entry_runtime_values_rejects_retained_string_scope() {
        let retained =
            BTreeMap::from([("sessionToken".into(), SessionValue::String("opaque".into()))]);
        let error = entry_runtime_values(&[], &retained).unwrap_err();
        assert!(error.to_string().contains("sessionToken"));
    }

    struct FaultInjectingReportFileSystem {
        rename_error: Option<io::ErrorKind>,
        hard_link_error: Option<io::ErrorKind>,
        concurrent_destination: Option<Vec<u8>>,
        sync_parent_error: Option<io::ErrorKind>,
        remove_error: Option<io::ErrorKind>,
        calls: Arc<Mutex<Vec<&'static str>>>,
    }

    #[async_trait]
    impl ReportFileSystem for FaultInjectingReportFileSystem {
        async fn rename_noreplace(&self, source: &Path, destination: &Path) -> io::Result<()> {
            self.calls.lock().unwrap().push("rename");
            if let Some(kind) = self.rename_error {
                return Err(io::Error::new(kind, "injected no-replace rename failure"));
            }
            rename_noreplace(source, destination)
        }

        async fn hard_link(&self, source: &Path, destination: &Path) -> io::Result<()> {
            self.calls.lock().unwrap().push("link");
            if let Some(kind) = self.hard_link_error {
                return Err(io::Error::new(kind, "injected hard-link failure"));
            }
            tokio::fs::hard_link(source, destination).await
        }

        async fn create_new_synced_copy(
            &self,
            source: &Path,
            destination: &Path,
        ) -> io::Result<()> {
            self.calls.lock().unwrap().push("copy");
            if let Some(bytes) = &self.concurrent_destination {
                write_new_report_file(destination, bytes).await?;
            }
            copy_new_report_file(source, destination).await
        }

        async fn sync_parent(&self, path: &Path) -> io::Result<()> {
            self.calls.lock().unwrap().push("sync-parent");
            if let Some(kind) = self.sync_parent_error {
                return Err(io::Error::new(kind, "injected parent sync failure"));
            }
            let parent = path.parent().unwrap_or_else(|| Path::new("."));
            tokio::fs::File::open(parent).await?.sync_all().await
        }

        async fn remove_file(&self, path: &Path) -> io::Result<()> {
            self.calls.lock().unwrap().push("remove");
            if let Some(kind) = self.remove_error {
                return Err(io::Error::new(kind, "injected removal failure"));
            }
            tokio::fs::remove_file(path).await
        }
    }

    fn publication_report(run_id: &str) -> SessionReport {
        SessionReport {
            schema: SESSION_REPORT_SCHEMA.into(),
            plan_schema: SESSION_PLAN_SCHEMA.into(),
            run_id: run_id.into(),
            connection: "app".into(),
            status: RunStatus::Succeeded,
            steps: Vec::new(),
            terminal_error: None,
            cleanup_warning: None,
            artifacts: SessionArtifacts {
                report: "session-report.json".into(),
                trace: "-".into(),
                observation: "observation.jsonl".into(),
                payloads: "payloads".into(),
            },
        }
    }

    fn publication_root(label: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "camera-initiator-report-{label}-{}-{nonce}",
            std::process::id()
        ));
        std::fs::create_dir(&root).unwrap();
        root
    }

    #[tokio::test]
    async fn report_publication_copies_complete_bytes_when_links_are_unsupported() {
        let root = publication_root("fallback");
        let path = root.join("session-report.json");
        let calls = Arc::new(Mutex::new(Vec::new()));
        let file_system = FaultInjectingReportFileSystem {
            rename_error: Some(io::ErrorKind::Unsupported),
            hard_link_error: Some(io::ErrorKind::Unsupported),
            concurrent_destination: None,
            sync_parent_error: None,
            remove_error: None,
            calls: Arc::clone(&calls),
        };
        publish_report_with_fs(&path, &publication_report("first"), &file_system)
            .await
            .unwrap();
        let first = std::fs::read(&path).unwrap();
        assert_eq!(first.last(), Some(&b'\n'));
        let parsed: Value = serde_json::from_slice(&first).unwrap();
        assert_eq!(parsed["runId"], "first");
        assert_eq!(
            calls.lock().unwrap().as_slice(),
            ["rename", "link", "copy", "sync-parent", "remove"]
        );

        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn report_publication_refuses_an_existing_destination() {
        let root = publication_root("existing");
        let path = root.join("session-report.json");
        publish_report(&path, &publication_report("first"))
            .await
            .unwrap();
        let first = std::fs::read(&path).unwrap();

        publish_report(&path, &publication_report("second"))
            .await
            .expect_err("existing report must not be overwritten");
        assert_eq!(std::fs::read(&path).unwrap(), first);

        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn report_publication_does_not_overwrite_a_racing_creator() {
        let root = publication_root("race");
        let path = root.join("session-report.json");
        let competing = b"competing report\n".to_vec();
        let calls = Arc::new(Mutex::new(Vec::new()));
        let file_system = FaultInjectingReportFileSystem {
            rename_error: Some(io::ErrorKind::Unsupported),
            hard_link_error: Some(io::ErrorKind::Unsupported),
            concurrent_destination: Some(competing.clone()),
            sync_parent_error: None,
            remove_error: None,
            calls: Arc::clone(&calls),
        };

        publish_report_with_fs(&path, &publication_report("candidate"), &file_system)
            .await
            .expect_err("racing report must not be overwritten");
        assert_eq!(std::fs::read(&path).unwrap(), competing);
        assert_eq!(calls.lock().unwrap().as_slice(), ["rename", "link", "copy"]);
        let staging = PathBuf::from(format!("{}.partial-{}", path.display(), std::process::id()));
        assert!(staging.is_file());

        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn published_report_ignores_staging_cleanup_failure() {
        let root = publication_root("cleanup");
        let path = root.join("session-report.json");
        let calls = Arc::new(Mutex::new(Vec::new()));
        let file_system = FaultInjectingReportFileSystem {
            rename_error: Some(io::ErrorKind::Unsupported),
            hard_link_error: None,
            concurrent_destination: None,
            sync_parent_error: None,
            remove_error: Some(io::ErrorKind::PermissionDenied),
            calls: Arc::clone(&calls),
        };
        publish_report_with_fs(&path, &publication_report("published"), &file_system)
            .await
            .unwrap();
        assert!(path.is_file());
        let staging = PathBuf::from(format!("{}.partial-{}", path.display(), std::process::id()));
        assert!(staging.is_file());
        assert_eq!(
            calls.lock().unwrap().as_slice(),
            ["rename", "link", "sync-parent", "remove"]
        );

        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn parent_sync_failure_preserves_the_final_and_staging_files() {
        let root = publication_root("sync-failure");
        let path = root.join("session-report.json");
        let calls = Arc::new(Mutex::new(Vec::new()));
        let file_system = FaultInjectingReportFileSystem {
            rename_error: Some(io::ErrorKind::Unsupported),
            hard_link_error: None,
            concurrent_destination: None,
            sync_parent_error: Some(io::ErrorKind::Other),
            remove_error: None,
            calls: Arc::clone(&calls),
        };
        publish_report_with_fs(&path, &publication_report("published"), &file_system)
            .await
            .expect_err("parent sync failure must fail publication");
        let staging = PathBuf::from(format!("{}.partial-{}", path.display(), std::process::id()));
        assert_eq!(
            std::fs::read(&path).unwrap(),
            std::fs::read(&staging).unwrap()
        );
        assert_eq!(
            calls.lock().unwrap().as_slice(),
            ["rename", "link", "sync-parent"]
        );

        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn fake_lifecycle_reestablishes_with_scope_and_fresh_transaction_ids() {
        let store = fake_switch_store();
        let plan = SessionPlan::from_yaml(
            r#"
schema: camera-initiator-session/v1
steps:
  - { id: switch, kind: switch, from: source, to: target }
"#,
        )
        .unwrap();
        let prepared = prepare_session_plan(plan, &store, "app").unwrap();
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "camera-initiator-fake-switch-{}-{nonce}",
            std::process::id()
        ));
        let observation = root.with_extension("jsonl");
        let trace_buffer = TestTraceBuffer::default();
        let trace = Arc::new(TraceWriter::with_observations(
            crate::TraceFormat::Jsonl,
            Box::new(trace_buffer.clone()),
            observation_recorder(observation.clone()),
            "app".into(),
            "session".into(),
        ));
        let calls = Arc::new(Mutex::new(Vec::new()));
        let transport = FakeTransport::new(Arc::clone(&calls), Arc::clone(&trace));
        let lifecycle: Arc<dyn SessionLifecycle> = Arc::new(FakeLifecycle {
            transport,
            trace: Arc::clone(&trace),
            calls: Arc::clone(&calls),
            generation: AtomicU32::new(0),
        });
        let report = run_session_plan(
            &prepared,
            store,
            lifecycle,
            trace,
            SessionRunOptions {
                output_dir: root.clone(),
                run_id: "fake-switch".into(),
                connection: "app".into(),
                trace_path: "-".into(),
                observation_path: observation.clone(),
                runtime_params: Vec::new(),
                handoff_timeout: Duration::from_secs(1),
            },
        )
        .await
        .unwrap();

        let calls = calls.lock().unwrap().clone();
        assert_eq!(
            calls,
            [
                "open:1",
                "op:9001:2:[]",
                "confirm-source",
                "op:9002:3:[2]",
                "op:1003:4:[]",
                "transport-close",
                "endpoint-gone",
                "open:2",
                "op:9003:2:[]",
                "cleanup",
            ]
        );
        assert_eq!(report.steps[0].transaction_ids, [2, 3, 4, 2]);
        let switch = report.steps[0].switch.as_ref().unwrap();
        assert!(switch.source_entry.is_some());
        assert_eq!(switch.before_session_index, 1);
        assert_eq!(switch.after_session_index, Some(2));

        let trace_records = trace_buffer.records();
        let close = trace_records
            .iter()
            .position(|record| record.get("state") == Some(&json!("fakeClosed")))
            .unwrap();
        let checkpoint = trace_records
            .iter()
            .position(|record| record.get("name") == Some(&json!("externalEstablishment")))
            .unwrap();
        let replacement = trace_records
            .iter()
            .rposition(|record| record.get("state") == Some(&json!("fakeOpened")))
            .unwrap();
        assert!(close < checkpoint && checkpoint < replacement);

        let bundle = std::fs::read_to_string(&observation).unwrap();
        let bundle = validate_bundles(&[&bundle]).unwrap();
        let transactions: Vec<_> = bundle
            .records
            .iter()
            .filter_map(|record| match record {
                ObservationLine::PtpTransaction(transaction) => Some(transaction.as_ref()),
                _ => None,
            })
            .collect();
        let sessions: BTreeSet<_> = transactions
            .iter()
            .map(|transaction| transaction.session.as_str())
            .collect();
        assert_eq!(sessions.len(), 2);
        let replacement_session = transactions.last().unwrap().session.clone();
        let replacement_tids: Vec<_> = transactions
            .iter()
            .filter(|transaction| transaction.session == replacement_session)
            .map(|transaction| transaction.transaction_id)
            .collect();
        assert_eq!(replacement_tids, [1, 2]);
        assert!(transactions
            .iter()
            .filter(|transaction| transaction.session == replacement_session)
            .all(|transaction| transaction.common.context.mode == "target"));
        assert!(transactions
            .iter()
            .filter(|transaction| transaction.session != replacement_session)
            .all(|transaction| transaction.common.context.mode == "source"));

        let _ = std::fs::remove_dir_all(root);
        let _ = std::fs::remove_file(observation);
    }
}
