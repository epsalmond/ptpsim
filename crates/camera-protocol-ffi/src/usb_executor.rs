//! Raw USB establishment-plan executor behind a foreign async trait (§11.29).
//!
//! The USB counterpart to the BLE executor (`executor.rs`): the plan-walker,
//! retry/tolerance ladder, capture pipeline, deadlines, and telemetry are
//! shared; the host supplies only raw USB I/O through
//! [`UsbExecutorTransport`], a `with_foreign` async trait. Deadline
//! discipline mirrors BLE: the executor owns every deadline by racing
//! transport calls against [`UsbExecutorTransport::sleep`], and dropping the
//! losing future propagates over the FFI as task/coroutine cancellation on
//! the foreign side, so every transport method must be cancellation-safe.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use camera_config::index::{Encoding, EstablishmentBlock, UsbInterfaceTriple};
use camera_config::ConnectionActivitySequence as ConfigActivitySequence;

use crate::executor::{
    outcome, resolve_plan_ref, usb_deadline, walk_plan_with_activities, ExecCtx, ExecTransport,
    NativeEstablishmentWalkSummary, RefineCtx, RefinementSource, StepError, DEFAULT_OP_TIMEOUT_MS,
};
use crate::{
    ConfigStore, ConnectionActivityObserver, ExecutionOutcome, ExecutorStepFailureKind, KeyValue,
    StepObserver, TransportError,
};

/// Failure surface a USB transport implementation may raise (§11.29). Every
/// variant is an ordinary step failure to the executor — retried per
/// `StepOptions`, then tolerated or fatal without error-class discrimination,
/// matching the BLE executor.
#[derive(Debug, thiserror::Error, uniffi::Error)]
pub enum UsbTransportError {
    /// No matching device is attached.
    #[error("no matching device is attached")]
    NotConnected,
    /// The device detached mid-operation.
    #[error("device detached mid-operation")]
    DeviceGone,
    /// An endpoint answered STALL.
    #[error("endpoint stalled: {detail}")]
    Stall { detail: String },
    /// A transfer exceeded its deadline.
    #[error("USB transfer timed out: {detail}")]
    Timeout { detail: String },
    /// The platform denied USB access.
    #[error("platform denied USB access: {detail}")]
    NotAuthorized { detail: String },
    /// Another driver holds the interface; `owner` names it when the platform
    /// reports one.
    #[error(
        "interface is claimed by another driver{}",
        .owner.as_ref().map(|owner| format!(" (owner: {owner})")).unwrap_or_default()
    )]
    ClaimFailed { owner: Option<String> },
    /// The device could not be opened.
    #[error("device could not be opened: {detail}")]
    OpenFailed { detail: String },
    /// Any remaining failure.
    #[error("USB transport failure: {detail}")]
    Failed { detail: String },
}

/// How a USB transport failure reads in the shared executor vocabulary.
/// `Timeout` keeps its identity so the executor still classifies it as a
/// deadline; the USB-only variants fold into `Failed` with their display
/// text preserved.
impl From<UsbTransportError> for TransportError {
    fn from(error: UsbTransportError) -> Self {
        match error {
            UsbTransportError::NotConnected => TransportError::NotConnected,
            UsbTransportError::Timeout { detail } => TransportError::Timeout { detail },
            other => TransportError::Failed {
                detail: other.to_string(),
            },
        }
    }
}

/// Raw USB I/O the host supplies for a raw `usb` establishment (§11.29); the
/// executor owns everything else (plan walking, retries, deadlines, captures,
/// telemetry). The trait is raw I/O only: step sequencing,
/// capture/transform/encoding evaluation, and deadline policy stay in Rust.
///
/// `sleep` is the host clock: the executor races pending I/O against it to
/// enforce deadlines and uses it for retry backoff. A dropped in-flight call
/// (deadline lost the race, or the whole run future was cancelled) surfaces
/// on the foreign side as task/coroutine cancellation, so every method must
/// be cancellation-safe.
#[uniffi::export(with_foreign)]
#[async_trait::async_trait]
pub trait UsbExecutorTransport: Send + Sync {
    /// Claim the interface matching the resolved class/subclass/protocol
    /// triple on the bound device.
    async fn claim_interface(
        &self,
        class: u8,
        subclass: u8,
        protocol: u8,
    ) -> Result<(), UsbTransportError>;
    /// Write one bulk OUT transfer.
    async fn bulk_out(&self, data: Vec<u8>) -> Result<(), UsbTransportError>;
    /// Read one bulk IN transfer of at most `max_length` bytes.
    async fn bulk_in(&self, max_length: u32) -> Result<Vec<u8>, UsbTransportError>;
    /// Await one interrupt IN event frame. May stay pending indefinitely —
    /// the executor owns every deadline.
    async fn next_interrupt_event(&self) -> Result<Vec<u8>, UsbTransportError>;
    /// Release the claimed interface and close the device handle.
    async fn release_and_close(&self) -> Result<(), UsbTransportError>;
    /// Resolve after `ms` milliseconds of wall-clock time.
    async fn sleep(&self, ms: u32) -> Result<(), UsbTransportError>;
}

/// The `run_usb_establishment` failure surface, mirroring the PCSS executor's
/// shape: plan resolution, a step failure with its retry-ladder
/// classification, the typed transport-mismatch error, and a catch-all
/// transport variant for failures outside a step.
#[derive(Debug, thiserror::Error, uniffi::Error)]
pub enum UsbExecutorError {
    #[error("unknown USB establishment plan: {detail}")]
    UnknownPlan { detail: String },
    #[error("{step}: {detail}")]
    StepFailed {
        step: String,
        kind: ExecutorStepFailureKind,
        detail: String,
        context: Vec<KeyValue>,
    },
    /// A verb the raw USB transport cannot run reached the walk. The loader
    /// scopes BLE verbs out of USB plans, so this is a loader escape, not a
    /// plan shape to support.
    #[error("verb not supported by the USB transport: {detail}")]
    UnsupportedVerb { detail: String },
    #[error("USB transport failure: {detail}")]
    Transport { detail: String },
}

impl From<UsbTransportError> for UsbExecutorError {
    fn from(error: UsbTransportError) -> Self {
        Self::Transport {
            detail: error.to_string(),
        }
    }
}

impl From<StepError> for UsbExecutorError {
    fn from(error: StepError) -> Self {
        if error.unsupported_verb {
            UsbExecutorError::UnsupportedVerb {
                detail: format!("{}: {}", error.step, error.message),
            }
        } else {
            UsbExecutorError::StepFailed {
                step: error.step,
                kind: error.kind,
                detail: error.message,
                context: error.context,
            }
        }
    }
}

/// Resolve `plan_handle` (`model:selector`) to its raw USB establishment
/// block plus the family interface-triple map `usbClaim` names resolve
/// against (§11.29). The USB analog of the BLE-only registry lookup behind
/// `run_establishment`.
fn resolve_usb_establishment(
    store: &ConfigStore,
    plan_handle: &str,
) -> Result<(EstablishmentBlock, BTreeMap<String, UsbInterfaceTriple>), UsbExecutorError> {
    let unknown = |detail: String| UsbExecutorError::UnknownPlan { detail };
    let resolved = resolve_plan_ref(store, plan_handle, unknown)?;
    let Some(usb) = resolved.view.usb.as_ref() else {
        return Err(unknown(format!(
            "{plan_handle}: missing mechanism {}",
            resolved.mechanism
        )));
    };
    let block = usb
        .establishment(&resolved.mechanism)
        .cloned()
        .ok_or_else(|| {
            unknown(format!(
                "{plan_handle}: missing mechanism {}",
                resolved.mechanism
            ))
        })?;
    Ok((block, usb.interfaces.clone()))
}

/// Execute the raw USB establishment plan behind `plan_handle`
/// (`model:selector`) against a foreign USB transport (§11.29). `initial_scope`
/// and `initial_encodings` thread exactly as in [`crate::run_establishment`]
/// so `{ captured: … }` writes re-encode with the capture's true encoding. A
/// walk that fails after a successful `usbClaim` releases the claimed
/// interface best-effort, so the camera is not stranded with it held.
#[uniffi::export]
#[allow(clippy::too_many_arguments)] // Mirrors run_establishment at the FFI seam.
pub async fn run_usb_establishment(
    store: Arc<ConfigStore>,
    plan_handle: String,
    transport: Arc<dyn UsbExecutorTransport>,
    observer: Arc<dyn StepObserver>,
    activity_observer: Arc<dyn ConnectionActivityObserver>,
    initial_scope: Vec<KeyValue>,
    initial_encodings: Vec<KeyValue>,
    runtime_params: Vec<KeyValue>,
) -> Result<ExecutionOutcome, UsbExecutorError> {
    let (block, usb_interfaces) = resolve_usb_establishment(&store, &plan_handle)?;
    let summary = NativeEstablishmentWalkSummary::for_steps(&block.steps);
    let encodings = initial_encodings
        .into_iter()
        .filter_map(|kv| Encoding::from_token(&kv.value).map(|enc| (kv.key, enc)))
        .collect();

    let mut ctx = ExecCtx {
        transport: ExecTransport::Usb(&transport),
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
        usb_interfaces,
        usb_interface_claimed: false,
    };
    match walk_plan_with_activities(
        &mut ctx,
        block.steps,
        block.activities,
        ConfigActivitySequence::Steps,
    )
    .await
    {
        Ok(()) => Ok(outcome(ctx)),
        Err(error) => {
            // A failed walk must not strand the camera with the interface
            // claimed; the release is best-effort cleanup, never the reported
            // error.
            if ctx.usb_interface_claimed {
                let _ = usb_deadline(
                    &transport,
                    DEFAULT_OP_TIMEOUT_MS,
                    "usbRelease",
                    transport.release_and_close(),
                )
                .await;
            }
            Err(error.into())
        }
    }
}

#[cfg(test)]
mod tests {
    use camera_config::index::{BleReadStep, Step as IxStep, StepOptions as IxStepOptions};

    use super::*;
    use crate::StepReport;

    struct StubUsbTransport;

    #[async_trait::async_trait]
    impl UsbExecutorTransport for StubUsbTransport {
        async fn claim_interface(
            &self,
            _class: u8,
            _subclass: u8,
            _protocol: u8,
        ) -> Result<(), UsbTransportError> {
            Ok(())
        }
        async fn bulk_out(&self, _data: Vec<u8>) -> Result<(), UsbTransportError> {
            Ok(())
        }
        async fn bulk_in(&self, _max_length: u32) -> Result<Vec<u8>, UsbTransportError> {
            Ok(Vec::new())
        }
        async fn next_interrupt_event(&self) -> Result<Vec<u8>, UsbTransportError> {
            Ok(Vec::new())
        }
        async fn release_and_close(&self) -> Result<(), UsbTransportError> {
            Ok(())
        }
        async fn sleep(&self, _ms: u32) -> Result<(), UsbTransportError> {
            Ok(())
        }
    }

    struct NoObserver;
    impl StepObserver for NoObserver {
        fn on_step(&self, _report: StepReport) {}
    }

    /// A BLE verb on a raw USB walk fails with the typed transport-mismatch
    /// error (§11.29). The loader scopes BLE verbs out of USB plans, so the
    /// walk-level error is exercised here by constructing the walk directly.
    #[test]
    fn ble_verb_on_usb_walk_is_a_typed_unsupported_verb() {
        let transport: Arc<dyn UsbExecutorTransport> = Arc::new(StubUsbTransport);
        let observer: Arc<dyn StepObserver> = Arc::new(NoObserver);
        let mut ctx = ExecCtx {
            transport: ExecTransport::Usb(&transport),
            observer: &observer,
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
        };
        let step = IxStep::BleRead(BleReadStep {
            gatt: "AAAA".into(),
            encoding: Encoding::Utf8,
            capture_as: "value".into(),
            transform: vec![],
            opts: IxStepOptions::default(),
        });
        let error = futures::executor::block_on(walk_plan_with_activities(
            &mut ctx,
            vec![step],
            vec![],
            ConfigActivitySequence::Steps,
        ))
        .expect_err("a BLE verb cannot run over the USB transport");
        assert!(error.unsupported_verb);
        assert!(matches!(
            UsbExecutorError::from(error),
            UsbExecutorError::UnsupportedVerb { .. }
        ));
    }
}
