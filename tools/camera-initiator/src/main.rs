use std::collections::BTreeMap;
use std::fs::File;
use std::io::{self, Write};
use std::net::IpAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use camera_initiator::{NativePtpTransport, TraceFormat, TraceWriter, TransportConfig};
use camera_protocol_ffi::{
    parse_action_verb, run_action, run_action_to_sink, run_mode_entry,
    run_mode_reestablishment_exit, run_streaming_action, ActionVerb, ConfigStore,
    ConnectionActivityEvent, ConnectionActivityObserver, ModeEntryExecution,
    ObjectTransferStrategy, PtpDataOutput, PtpDataOutputSink, PtpDataOutputSinkError,
    PtpExecutionOutcome, PtpExecutorTransport, PtpRuntimeValue, PtpStreamingSink,
    PtpStreamingSinkError, PtpStreamingTransport, StepObserver, StepReport,
};
use clap::{Parser, Subcommand};
use serde_json::{json, Value};
use tokio::sync::Mutex;

#[derive(Parser)]
#[command(
    name = "camera-initiator",
    about = "Drive a real camera through ptpsim's shipping PTP/IP engine"
)]
struct Cli {
    /// Camera IPv4/IPv6 address. Omit only when the manifest defaults to broadcast discovery.
    #[arg(long)]
    camera: Option<IpAddr>,
    /// IPv4 interface for subnet-directed broadcast discovery. Defaults to the default route.
    #[arg(long)]
    interface: Option<String>,
    /// Body manifest YAML.
    #[arg(long)]
    manifest: PathBuf,
    /// Optional manufacturer-tier YAML.
    #[arg(long)]
    manufacturer: Option<PathBuf>,
    /// Firmware overlays, applied in command-line order.
    #[arg(long)]
    overlay: Vec<PathBuf>,
    #[arg(long, default_value = "app")]
    connection: String,
    /// Runtime binding as KEY=VALUE. Numeric executor values accept decimal or 0x hex.
    #[arg(long = "param")]
    params: Vec<String>,
    /// Trace path, or '-' for stdout.
    #[arg(long, default_value = "-")]
    trace: String,
    #[arg(long, value_enum, default_value_t = TraceFormat::Jsonl)]
    trace_format: TraceFormat,
    #[arg(long, default_value_t = 10_000)]
    connect_timeout_ms: u64,
    #[arg(long, default_value_t = 120_000)]
    handoff_timeout_ms: u64,
    #[arg(long, default_value_t = 64 * 1024 * 1024)]
    max_frame_bytes: usize,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Execute a current-session mode entry.
    Entry {
        #[arg(long)]
        to: String,
        #[arg(long)]
        from: Option<String>,
    },
    /// Enter a cold source mode, exit it, wait for external establishment, and enter the target.
    Switch {
        #[arg(long)]
        from: String,
        #[arg(long)]
        to: String,
    },
    /// Execute an exact camelCase manifest action.
    Action {
        action: String,
        /// Execute another ordinary action before closing the same PTP session.
        #[arg(long = "then")]
        then: Vec<String>,
        #[arg(long, conflicts_with = "payload_dir")]
        payload_out: Option<PathBuf>,
        #[arg(long, conflicts_with = "payload_out")]
        payload_dir: Option<PathBuf>,
        #[arg(long)]
        expected_bytes: Option<u64>,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let trace = Arc::new(TraceWriter::new(
        cli.trace_format,
        trace_output(&cli.trace)?,
    ));
    let store = load_store(&cli)?;
    let params = RuntimeParams::parse(&cli.params)?;
    let transport = NativePtpTransport::new(
        Arc::clone(&store),
        TransportConfig {
            camera: cli.camera,
            interface: cli.interface.clone(),
            connection: cli.connection.clone(),
            runtime_scope: params.raw_pairs(),
            connect_timeout: Duration::from_millis(cli.connect_timeout_ms),
            max_frame_bytes: cli.max_frame_bytes,
        },
        Arc::clone(&trace),
    )?;
    let observer: Arc<dyn StepObserver> = Arc::new(TraceStepObserver {
        trace: Arc::clone(&trace),
    });
    let activity_observer: Arc<dyn ConnectionActivityObserver> = Arc::new(TraceActivityObserver {
        trace: Arc::clone(&trace),
    });

    let result = match cli.command {
        Command::Entry { to, from } => {
            run_entry(
                &cli.connection,
                store,
                Arc::clone(&transport),
                observer,
                activity_observer,
                &params,
                from,
                to,
            )
            .await
        }
        Command::Switch { from, to } => {
            run_switch(
                &cli.connection,
                store,
                Arc::clone(&transport),
                observer,
                activity_observer,
                &params,
                from,
                to,
                Duration::from_millis(cli.handoff_timeout_ms),
                Arc::clone(&trace),
            )
            .await
        }
        Command::Action {
            action,
            then,
            payload_out,
            payload_dir,
            expected_bytes,
        } => {
            run_named_action(
                &cli.connection,
                store,
                Arc::clone(&transport),
                observer,
                activity_observer,
                &params,
                &action,
                &then,
                payload_out,
                payload_dir,
                expected_bytes,
            )
            .await
        }
    };

    let cleanup_warning = transport
        .close_session_if_open()
        .await
        .err()
        .map(|error| error.to_string());
    if let Some(error) = &cleanup_warning {
        eprintln!("cleanup warning: {error}");
    }
    match result {
        Ok(mut detail) => {
            if let (Some(warning), Value::Object(object)) = (cleanup_warning, &mut detail) {
                object.insert("cleanupWarning".into(), Value::String(warning));
            }
            trace.outcome("succeeded", detail)?;
            Ok(())
        }
        Err(error) => {
            let detail = json!({
                "error": error.to_string(),
                "cleanupWarning": cleanup_warning,
            });
            if let Err(trace_error) = trace.outcome("failed", detail) {
                eprintln!("trace warning: {trace_error}");
            }
            Err(error)
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_entry(
    connection: &str,
    store: Arc<ConfigStore>,
    transport: Arc<NativePtpTransport>,
    observer: Arc<dyn StepObserver>,
    activity_observer: Arc<dyn ConnectionActivityObserver>,
    params: &RuntimeParams,
    from: Option<String>,
    to: String,
) -> Result<Value> {
    let plan = store
        .mode_entry(connection.to_string(), from.clone(), to.clone())
        .with_context(|| format!("mode entry {from:?} -> {to} is not declared"))?;
    match plan.execution {
        ModeEntryExecution::Ptp { .. } => {}
        ModeEntryExecution::UserInstruction { instruction } => {
            bail!("mode entry requires user instruction: {instruction}")
        }
        ModeEntryExecution::ReestablishConnection { .. } => {
            bail!("mode entry requires outer re-establishment; use 'switch'")
        }
    }
    transport.open_command_session().await?;
    let raw: Arc<dyn PtpExecutorTransport> = transport;
    let outcome = run_mode_entry(
        store,
        connection.to_string(),
        from,
        to,
        raw,
        observer,
        activity_observer,
        params.numeric_values(),
    )
    .await?;
    Ok(execution_outcome_detail(&outcome))
}

#[allow(clippy::too_many_arguments)]
async fn run_switch(
    connection: &str,
    store: Arc<ConfigStore>,
    transport: Arc<NativePtpTransport>,
    observer: Arc<dyn StepObserver>,
    activity_observer: Arc<dyn ConnectionActivityObserver>,
    params: &RuntimeParams,
    from: String,
    to: String,
    handoff_timeout: Duration,
    trace: Arc<TraceWriter>,
) -> Result<Value> {
    let source = store
        .mode_entry(connection.to_string(), None, from.clone())
        .with_context(|| format!("cold source entry '{from}' is not declared"))?;
    if !matches!(source.execution, ModeEntryExecution::Ptp { .. }) {
        bail!("cold source entry '{from}' is not a PTP plan")
    }
    let edge = store
        .mode_entry(connection.to_string(), Some(from.clone()), to.clone())
        .with_context(|| format!("switch edge '{from}' -> '{to}' is not declared"))?;
    let establishment_params = match edge.execution {
        ModeEntryExecution::ReestablishConnection {
            establishment_params,
            ..
        } => establishment_params,
        _ => bail!("switch edge '{from}' -> '{to}' is not a re-establishment"),
    };
    let target = store
        .mode_entry(connection.to_string(), None, to.clone())
        .with_context(|| format!("cold target entry '{to}' is not declared"))?;
    if !matches!(target.execution, ModeEntryExecution::Ptp { .. }) {
        bail!("cold target entry '{to}' is not a PTP plan")
    }

    transport.open_command_session().await?;
    let raw: Arc<dyn PtpExecutorTransport> = transport.clone();
    let source_outcome = run_mode_entry(
        Arc::clone(&store),
        connection.to_string(),
        None,
        from.clone(),
        raw.clone(),
        Arc::clone(&observer),
        Arc::clone(&activity_observer),
        params.numeric_values(),
    )
    .await?;
    let frame_bytes = transport.confirm_live_view_frame().await?;
    eprintln!("live view ready ({frame_bytes} byte frame)");

    run_mode_reestablishment_exit(
        Arc::clone(&store),
        connection.to_string(),
        Some(from.clone()),
        to.clone(),
        raw,
        observer,
        activity_observer,
        source_outcome.scope,
    )
    .await?;

    let checkpoint: BTreeMap<_, _> = establishment_params
        .into_iter()
        .map(|param| (param.key, param.value))
        .collect();
    trace.checkpoint("externalEstablishment", json!(checkpoint))?;
    eprintln!(
        "external BLE/Wi-Fi establishment required; params={} (waiting up to {}s)",
        serde_json::to_string(&checkpoint)?,
        handoff_timeout.as_secs()
    );

    let deadline = Instant::now() + handoff_timeout;
    while Instant::now() < deadline && transport.endpoint_accepts_tcp().await {
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    if Instant::now() >= deadline {
        bail!("handoff timed out before the old command endpoint became unavailable")
    }
    transport
        .open_command_session_after_handoff(deadline)
        .await?;
    let raw: Arc<dyn PtpExecutorTransport> = transport;
    let outcome = run_mode_entry(
        store,
        connection.to_string(),
        None,
        to,
        raw,
        Arc::new(TraceStepObserver {
            trace: Arc::clone(&trace),
        }),
        Arc::new(TraceActivityObserver {
            trace: Arc::clone(&trace),
        }),
        params.numeric_values(),
    )
    .await?;
    Ok(execution_outcome_detail(&outcome))
}

#[allow(clippy::too_many_arguments)]
async fn run_named_action(
    connection: &str,
    store: Arc<ConfigStore>,
    transport: Arc<NativePtpTransport>,
    observer: Arc<dyn StepObserver>,
    activity_observer: Arc<dyn ConnectionActivityObserver>,
    params: &RuntimeParams,
    action_name: &str,
    then: &[String],
    payload_out: Option<PathBuf>,
    payload_dir: Option<PathBuf>,
    expected_bytes: Option<u64>,
) -> Result<Value> {
    if !then.is_empty() {
        return run_action_sequence(
            connection,
            store,
            transport,
            observer,
            activity_observer,
            params,
            action_name,
            then,
            payload_out,
            payload_dir,
            expected_bytes,
        )
        .await;
    }
    let action = parse_action_verb(action_name.to_string())
        .with_context(|| format!("unknown action '{action_name}'; use exact camelCase"))?;
    let plan = store
        .action(connection.to_string(), action)
        .with_context(|| format!("connection '{connection}' has no action '{action_name}'"))?;
    params.require_numeric(&plan.params)?;
    if action == ActionVerb::ImportObjects && payload_dir.is_none() {
        bail!("importObjects requires --payload-dir")
    }
    let contract = store.object_transfer_contract(connection.to_string());
    let streaming = contract.as_ref().is_some_and(|contract| {
        contract.strategy == ObjectTransferStrategy::WholeObject && contract.read_action == action
    });
    if expected_bytes.is_some() && !streaming {
        bail!("--expected-bytes is valid only for a whole-object streaming action")
    }

    transport.open_command_session().await?;
    if streaming {
        let destination = payload_out.context("whole-object action requires --payload-out")?;
        if payload_dir.is_some() {
            bail!("whole-object action does not accept --payload-dir")
        }
        let sink = Arc::new(StreamingFileSink::new(destination));
        let raw: Arc<dyn PtpStreamingTransport> = transport;
        let outcome = run_streaming_action(
            store,
            connection.to_string(),
            action,
            raw,
            sink.clone(),
            params.numeric_values(),
            expected_bytes,
        )
        .await?;
        sink.commit().await?;
        return Ok(json!({
            "transactionId": outcome.transaction_id,
            "operation": outcome.operation,
            "payloadBytes": outcome.total_bytes,
            "responseParams": outcome.response_params,
        }));
    }

    let raw: Arc<dyn PtpExecutorTransport> = transport;
    let outcome = if payload_out.is_some() || payload_dir.is_some() {
        let sink: Arc<dyn PtpDataOutputSink> = Arc::new(
            OrdinaryFileSink::new(payload_out, payload_dir)
                .await
                .context("prepare payload destination")?,
        );
        run_action_to_sink(
            store,
            connection.to_string(),
            action,
            raw,
            observer,
            activity_observer,
            sink,
            params.numeric_values(),
        )
        .await?
    } else {
        run_action(
            store,
            connection.to_string(),
            action,
            raw,
            observer,
            activity_observer,
            params.numeric_values(),
        )
        .await?
    };
    Ok(execution_outcome_detail(&outcome))
}

#[allow(clippy::too_many_arguments)]
async fn run_action_sequence(
    connection: &str,
    store: Arc<ConfigStore>,
    transport: Arc<NativePtpTransport>,
    observer: Arc<dyn StepObserver>,
    activity_observer: Arc<dyn ConnectionActivityObserver>,
    params: &RuntimeParams,
    first: &str,
    then: &[String],
    payload_out: Option<PathBuf>,
    payload_dir: Option<PathBuf>,
    expected_bytes: Option<u64>,
) -> Result<Value> {
    if payload_out.is_some() {
        bail!("a multi-action sequence requires --payload-dir instead of --payload-out")
    }
    if expected_bytes.is_some() {
        bail!("--expected-bytes is not supported for a multi-action sequence")
    }

    let names = std::iter::once(first)
        .chain(then.iter().map(String::as_str))
        .collect::<Vec<_>>();
    let contract = store.object_transfer_contract(connection.to_string());
    let mut actions = Vec::with_capacity(names.len());
    for name in &names {
        let action = parse_action_verb((*name).to_string())
            .with_context(|| format!("unknown action '{name}'; use exact camelCase"))?;
        let plan = store
            .action(connection.to_string(), action)
            .with_context(|| format!("connection '{connection}' has no action '{name}'"))?;
        params.require_numeric(&plan.params)?;
        let streaming = contract.as_ref().is_some_and(|contract| {
            contract.strategy == ObjectTransferStrategy::WholeObject
                && contract.read_action == action
        });
        if streaming {
            bail!("whole-object action '{name}' cannot run inside --then sequence")
        }
        if action == ActionVerb::ImportObjects && payload_dir.is_none() {
            bail!("importObjects requires --payload-dir")
        }
        actions.push(action);
    }

    let sink = if payload_dir.is_some() {
        Some(Arc::new(
            OrdinaryFileSink::new(None, payload_dir)
                .await
                .context("prepare payload directory")?,
        ) as Arc<dyn PtpDataOutputSink>)
    } else {
        None
    };

    transport.open_command_session().await?;
    let mut scope = params.numeric_values();
    let mut outcomes = Vec::with_capacity(actions.len());
    let mut live_view_started = false;
    for (index, (name, action)) in names.into_iter().zip(actions.iter().copied()).enumerate() {
        let raw: Arc<dyn PtpExecutorTransport> = transport.clone();
        let result = if let Some(sink) = &sink {
            run_action_to_sink(
                Arc::clone(&store),
                connection.to_string(),
                action,
                raw,
                Arc::clone(&observer),
                Arc::clone(&activity_observer),
                Arc::clone(sink),
                scope.clone(),
            )
            .await
        } else {
            run_action(
                Arc::clone(&store),
                connection.to_string(),
                action,
                raw,
                Arc::clone(&observer),
                Arc::clone(&activity_observer),
                scope.clone(),
            )
            .await
        };
        let outcome = match result {
            Ok(outcome) => outcome,
            Err(primary) => {
                if should_run_requested_stop_cleanup(
                    live_view_started,
                    action,
                    &actions[index + 1..],
                ) {
                    eprintln!("action '{name}' failed; attempting requested stopLiveView cleanup");
                    let raw: Arc<dyn PtpExecutorTransport> = transport.clone();
                    match run_action(
                        Arc::clone(&store),
                        connection.to_string(),
                        ActionVerb::StopLiveView,
                        raw,
                        Arc::clone(&observer),
                        Arc::clone(&activity_observer),
                        scope.clone(),
                    )
                    .await
                    {
                        Ok(_) => eprintln!("stopLiveView cleanup succeeded"),
                        Err(cleanup) => {
                            return Err(anyhow::anyhow!(
                                "action '{name}' failed: {primary}; best-effort stopLiveView cleanup failed: {cleanup}"
                            ))
                        }
                    }
                }
                return Err(primary.into());
            }
        };
        match action {
            ActionVerb::StartLiveView => live_view_started = true,
            ActionVerb::StopLiveView => live_view_started = false,
            _ => {}
        }
        scope = outcome.scope.clone();
        outcomes.push(json!({
            "action": name,
            "outcome": execution_outcome_detail(&outcome),
        }));
    }

    Ok(json!({ "actions": outcomes }))
}

fn should_run_requested_stop_cleanup(
    live_view_started: bool,
    failed_action: ActionVerb,
    remaining_actions: &[ActionVerb],
) -> bool {
    live_view_started
        && failed_action != ActionVerb::StopLiveView
        && remaining_actions.contains(&ActionVerb::StopLiveView)
}

fn load_store(cli: &Cli) -> Result<Arc<ConfigStore>> {
    let body = std::fs::read_to_string(&cli.manifest)
        .with_context(|| format!("read {}", cli.manifest.display()))?;
    let manufacturer = cli
        .manufacturer
        .as_ref()
        .map(|path| {
            std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))
        })
        .transpose()?;
    let overlays = cli
        .overlay
        .iter()
        .map(|path| {
            std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))
        })
        .collect::<Result<Vec<_>>>()?;
    ConfigStore::from_tiers(body, manufacturer, overlays).map_err(Into::into)
}

fn trace_output(path: &str) -> Result<Box<dyn Write + Send>> {
    if path == "-" {
        Ok(Box::new(io::stdout()))
    } else {
        Ok(Box::new(
            File::create(path).with_context(|| format!("create trace {path}"))?,
        ))
    }
}

fn execution_outcome_detail(outcome: &PtpExecutionOutcome) -> Value {
    json!({
        "stepsRun": outcome.steps_run,
        "scope": outcome.scope.iter().map(|value| json!({"key": value.key, "value": value.value})).collect::<Vec<_>>(),
        "collections": outcome.collections.iter().map(|value| json!({"key": value.key, "values": value.values})).collect::<Vec<_>>(),
        "outputs": outcome.outputs.iter().map(|value| json!({
            "stepPath": value.step_path,
            "operation": value.operation,
            "transactionId": value.transaction_id,
            "payloadBytes": value.payload.len(),
            "responseParams": value.response_params,
        })).collect::<Vec<_>>(),
    })
}

#[derive(Debug)]
struct RuntimeParams {
    raw: BTreeMap<String, String>,
}

impl RuntimeParams {
    fn parse(values: &[String]) -> Result<Self> {
        let mut raw = BTreeMap::new();
        for value in values {
            let (key, value) = value
                .split_once('=')
                .with_context(|| format!("runtime parameter '{value}' must be KEY=VALUE"))?;
            if key.trim().is_empty() {
                bail!("runtime parameter key must not be empty")
            }
            if raw.insert(key.to_string(), value.to_string()).is_some() {
                bail!("duplicate runtime parameter '{key}'")
            }
        }
        Ok(Self { raw })
    }

    fn raw_pairs(&self) -> Vec<(String, String)> {
        self.raw
            .iter()
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect()
    }

    fn numeric_values(&self) -> Vec<PtpRuntimeValue> {
        self.raw
            .iter()
            .filter_map(|(key, value)| {
                parse_u64(value).map(|value| PtpRuntimeValue {
                    key: key.clone(),
                    value,
                })
            })
            .collect()
    }

    fn require_numeric(&self, required: &[String]) -> Result<()> {
        for key in required {
            let value = self
                .raw
                .get(key)
                .with_context(|| format!("action requires --param {key}=VALUE"))?;
            parse_u64(value)
                .with_context(|| format!("action parameter '{key}' must be decimal or 0x hex"))?;
        }
        Ok(())
    }
}

fn parse_u64(value: &str) -> Option<u64> {
    value
        .strip_prefix("0x")
        .or_else(|| value.strip_prefix("0X"))
        .map_or_else(
            || value.parse().ok(),
            |hex| u64::from_str_radix(hex, 16).ok(),
        )
}

struct TraceStepObserver {
    trace: Arc<TraceWriter>,
}

impl StepObserver for TraceStepObserver {
    fn on_step(&self, report: StepReport) {
        let _ = self.trace.step(&report);
    }
}

struct TraceActivityObserver {
    trace: Arc<TraceWriter>,
}

impl ConnectionActivityObserver for TraceActivityObserver {
    fn on_activity(&self, event: ConnectionActivityEvent) {
        let _ = self.trace.activity(&event);
    }
}

enum OrdinaryDestination {
    Single(PathBuf),
    Directory(PathBuf),
}

struct OrdinaryFileSink {
    destination: OrdinaryDestination,
    seen: AtomicU64,
}

impl OrdinaryFileSink {
    async fn new(single: Option<PathBuf>, directory: Option<PathBuf>) -> Result<Self> {
        let destination = match (single, directory) {
            (Some(path), None) => OrdinaryDestination::Single(path),
            (None, Some(path)) => {
                tokio::fs::create_dir_all(&path).await?;
                OrdinaryDestination::Directory(path)
            }
            _ => bail!("one payload destination is required"),
        };
        Ok(Self {
            destination,
            seen: AtomicU64::new(0),
        })
    }
}

#[async_trait]
impl PtpDataOutputSink for OrdinaryFileSink {
    async fn write(&self, output: PtpDataOutput) -> Result<(), PtpDataOutputSinkError> {
        if output.payload.is_empty() {
            return Ok(());
        }
        let ordinal = self.seen.fetch_add(1, Ordering::AcqRel);
        let path = match &self.destination {
            OrdinaryDestination::Single(path) if ordinal == 0 => path.clone(),
            OrdinaryDestination::Single(_) => {
                return Err(PtpDataOutputSinkError::Failed {
                    detail: "action produced more than one payload; use --payload-dir".into(),
                })
            }
            OrdinaryDestination::Directory(directory) => directory.join(format!(
                "{ordinal:04}_{}_tid{:08x}.bin",
                sanitize(&output.step_path),
                output.transaction_id
            )),
        };
        tokio::fs::write(&path, output.payload)
            .await
            .map_err(|error| PtpDataOutputSinkError::Failed {
                detail: format!("write {}: {error}", path.display()),
            })
    }
}

struct StreamingFileSink {
    final_path: PathBuf,
    partial_path: PathBuf,
    file: Mutex<Option<tokio::fs::File>>,
}

impl StreamingFileSink {
    fn new(final_path: PathBuf) -> Self {
        let mut partial_name = final_path.as_os_str().to_os_string();
        partial_name.push(".partial");
        let partial_path = PathBuf::from(partial_name);
        Self {
            final_path,
            partial_path,
            file: Mutex::new(None),
        }
    }

    async fn commit(&self) -> Result<()> {
        let mut guard = self.file.lock().await;
        let file = guard
            .take()
            .context("stream sink did not open its partial file")?;
        file.sync_all().await?;
        drop(file);
        tokio::fs::rename(&self.partial_path, &self.final_path)
            .await
            .with_context(|| {
                format!(
                    "rename {} to {}",
                    self.partial_path.display(),
                    self.final_path.display()
                )
            })
    }
}

#[async_trait]
impl PtpStreamingSink for StreamingFileSink {
    async fn begin(&self, _total_bytes: u64) -> Result<(), PtpStreamingSinkError> {
        let file = tokio::fs::File::create(&self.partial_path)
            .await
            .map_err(|error| PtpStreamingSinkError::Failed {
                detail: format!("create {}: {error}", self.partial_path.display()),
            })?;
        *self.file.lock().await = Some(file);
        Ok(())
    }

    async fn write(&self, chunk: Vec<u8>) -> Result<(), PtpStreamingSinkError> {
        use tokio::io::AsyncWriteExt;
        let mut guard = self.file.lock().await;
        let file = guard
            .as_mut()
            .ok_or_else(|| PtpStreamingSinkError::Failed {
                detail: "stream sink write arrived before begin".into(),
            })?;
        file.write_all(&chunk)
            .await
            .map_err(|error| PtpStreamingSinkError::Failed {
                detail: format!("write {}: {error}", self.partial_path.display()),
            })
    }
}

fn sanitize(value: &str) -> String {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_numeric_runtime_values_exactly() {
        assert_eq!(parse_u64("42"), Some(42));
        assert_eq!(parse_u64("0x2a"), Some(42));
        assert_eq!(parse_u64(" 42"), None);
        assert_eq!(parse_u64("probe-host"), None);
    }

    #[test]
    fn rejects_duplicate_runtime_keys() {
        let error = RuntimeParams::parse(&["handle=1".into(), "handle=2".into()]).unwrap_err();
        assert!(error.to_string().contains("duplicate"));
    }

    #[test]
    fn sanitizes_step_paths_for_output_files() {
        assert_eq!(
            sanitize("steps[1].loop.body[2].sendOp"),
            "steps_1__loop_body_2__sendOp"
        );
    }

    #[test]
    fn exact_action_parser_rejects_case_and_separators() {
        assert_eq!(
            parse_action_verb("getObject".into()),
            Some(ActionVerb::GetObject)
        );
        assert!(parse_action_verb("GetObject".into()).is_none());
        assert!(parse_action_verb("get-object".into()).is_none());
    }

    #[test]
    fn parses_address_free_multi_action_probe() {
        let cli = Cli::try_parse_from([
            "camera-initiator",
            "--interface",
            "eth0",
            "--manifest",
            "camera.yaml",
            "--connection",
            "wireless-tether",
            "action",
            "startLiveView",
            "--then",
            "pollLiveView",
            "--then",
            "enumerateObjects",
            "--then",
            "stopLiveView",
            "--payload-dir",
            "payloads",
        ])
        .expect("address-free PCSS CLI parses");

        assert_eq!(cli.camera, None);
        assert_eq!(cli.interface.as_deref(), Some("eth0"));
        match cli.command {
            Command::Action {
                action,
                then,
                payload_dir,
                ..
            } => {
                assert_eq!(action, "startLiveView");
                assert_eq!(then, ["pollLiveView", "enumerateObjects", "stopLiveView"]);
                assert_eq!(payload_dir, Some(PathBuf::from("payloads")));
            }
            _ => panic!("unexpected command variant"),
        }
    }

    #[test]
    fn sequence_cleanup_only_runs_for_a_requested_trailing_stop() {
        assert!(should_run_requested_stop_cleanup(
            true,
            ActionVerb::PollLiveView,
            &[ActionVerb::EnumerateObjects, ActionVerb::StopLiveView],
        ));
        assert!(!should_run_requested_stop_cleanup(
            false,
            ActionVerb::PollLiveView,
            &[ActionVerb::StopLiveView],
        ));
        assert!(!should_run_requested_stop_cleanup(
            true,
            ActionVerb::StopLiveView,
            &[],
        ));
        assert!(!should_run_requested_stop_cleanup(
            true,
            ActionVerb::PollLiveView,
            &[ActionVerb::EnumerateObjects],
        ));
    }
}
