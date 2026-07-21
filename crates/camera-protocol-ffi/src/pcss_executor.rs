//! Manifest-driven PCSS discovery and command-session establishment.
//!
//! A recognized subnet-broadcast callback is itself a complete rendezvous: its
//! validated endpoint is tried directly. A manifest may authorize one fresh
//! unicast rendezvous to the learned camera address only when that first
//! endpoint, or the first Init socket I/O on it, is unavailable.

use std::future::Future;
use std::net::Ipv4Addr;
use std::sync::Arc;

use futures_util::future::{select, Either};
use ptp_core::{PtpCodec, PtpIpPacket};

use crate::executor::ActiveActivity;
use crate::{
    ConfigStore, ConnectionActivityFailure, ConnectionActivityObserver, ConnectionActivityRetry,
    ExecutorStepFailureKind, Recognition, TransportError,
};

#[uniffi::export(with_foreign)]
#[async_trait::async_trait]
/// Raw PCSS socket I/O supplied by the foreign host.
///
/// For command connect and Init I/O, adapters map EOF, connection reset,
/// broken pipe, and write-zero failures to [`TransportError::NotConnected`],
/// connect refusal/unreachability to [`TransportError::ConnectFailed`], and
/// socket deadlines to [`TransportError::Timeout`]. Local framing, validation,
/// trace, and clock failures use [`TransportError::Failed`]. The executor uses
/// that distinction only to select the manifest's one learned-unicast recovery
/// after an unavailable broadcast-discovered endpoint or first Init attempt.
pub trait PcssExecutorTransport: Send + Sync {
    async fn bind_callback_listener(&self, port: u16) -> Result<(), TransportError>;
    async fn send_discovery(
        &self,
        destination_ipv4: String,
        destination_port: u16,
        payload: Vec<u8>,
    ) -> Result<(), TransportError>;
    async fn next_callback(&self) -> Result<PcssCallback, TransportError>;
    async fn send_callback_reply(&self, payload: Vec<u8>) -> Result<(), TransportError>;
    async fn close_callback_connection(&self) -> Result<(), TransportError>;
    async fn connect_command(&self, camera_ipv4: String, port: u16) -> Result<(), TransportError>;
    async fn send_command_frame(&self, frame: Vec<u8>) -> Result<(), TransportError>;
    async fn next_command_frame(&self) -> Result<Vec<u8>, TransportError>;
    async fn close_command_connection(&self) -> Result<(), TransportError>;
    async fn sleep(&self, ms: u32) -> Result<(), TransportError>;
}

/// One accepted callback connection and the bytes read from it. The executor
/// needs the TCP peer separately from the payload so it can enforce the PCSS
/// requirement that the peer, the advertised DSC, and (for explicit unicast)
/// the requested camera address agree.
#[derive(Debug, Clone, uniffi::Record)]
pub struct PcssCallback {
    pub peer_ipv4: String,
    pub payload: Vec<u8>,
}

#[derive(Debug, Clone, uniffi::Record)]
pub struct PcssEstablishmentOutcome {
    pub model: String,
    pub connection: String,
    pub camera_ipv4: String,
    pub camera_name: String,
    pub command_port: u16,
    pub service: String,
    pub connection_number: u32,
    pub responder_guid: Vec<u8>,
    pub responder_name: String,
}

#[derive(Debug, thiserror::Error, uniffi::Error)]
pub enum PcssExecutorError {
    #[error("unknown PCSS establishment plan: {detail}")]
    UnknownPlan { detail: String },
    #[error("invalid IPv4 address: {detail}")]
    InvalidAddress { detail: String },
    #[error("PCSS discovery exhausted its retry policy")]
    DiscoveryTimedOut,
    #[error("PCSS callback did not identify a supported camera")]
    UnrecognizedCamera,
    #[error("PCSS callback identity does not match model {model}")]
    IdentityMismatch { model: String },
    #[error("camera command endpoint was not ready after {attempts} attempts")]
    EndpointUnavailable { attempts: u32 },
    #[error("PTP/IP Init was rejected with reason 0x{reason:08x}")]
    InitRejected { reason: u32 },
    #[error("unexpected PTP/IP Init response: {detail}")]
    InvalidInitResponse { detail: String },
    #[error("PCSS transport failure: {detail}")]
    Transport { detail: String },
    #[error("PCSS deadline exceeded during {stage}")]
    DeadlineExceeded { stage: String },
}

impl From<TransportError> for PcssExecutorError {
    fn from(error: TransportError) -> Self {
        Self::Transport {
            detail: error.to_string(),
        }
    }
}

#[allow(clippy::too_many_arguments)]
#[uniffi::export]
pub async fn run_pcss_auto_establishment(
    store: Arc<ConfigStore>,
    broadcast_ipv4: String,
    callback_ipv4: String,
    initiator_guid: Vec<u8>,
    friendly_name: String,
    transport: Arc<dyn PcssExecutorTransport>,
    activity_observer: Option<Arc<dyn ConnectionActivityObserver>>,
) -> Result<PcssEstablishmentOutcome, PcssExecutorError> {
    let broadcast = parse_ipv4(&broadcast_ipv4)?;
    let callback = parse_ipv4(&callback_ipv4)?;
    let init = build_init(initiator_guid, callback, &friendly_name)?;
    let index = store
        .inner
        .index
        .as_ref()
        .ok_or_else(|| PcssExecutorError::UnknownPlan {
            detail: "manufacturer index is not loaded".into(),
        })?;
    let policy = select_auto_discovery_policy(index)?;

    transport
        .bind_callback_listener(policy.callback_port)
        .await?;
    let discovery = protocol_primitives::pcss_discovery_message(callback, &policy.protocol);
    for _attempt in 1..=policy.discovery.max_attempts {
        transport
            .send_discovery(broadcast.to_string(), policy.knock_port, discovery.clone())
            .await?;
        let payload = match with_deadline_source(
            &transport,
            policy.discovery.retry_interval_ms,
            transport.next_callback(),
        )
        .await
        {
            Ok(payload) => payload,
            Err(error) if error.callback_timed_out() => continue,
            Err(error) => return Err(error.into_executor("discovery callback")),
        };
        let notify =
            match protocol_primitives::parse_pcss_notify(&payload.payload, &policy.protocol) {
                Ok(notify) => notify,
                Err(_) => {
                    let _ = transport.close_callback_connection().await;
                    continue;
                }
            };
        let peer = match parse_ipv4(&payload.peer_ipv4) {
            Ok(peer) => peer,
            Err(error) => {
                let _ = transport.close_callback_connection().await;
                return Err(error);
            }
        };
        if peer != notify.camera_address {
            let _ = transport.close_callback_connection().await;
            continue;
        }
        match crate::mfg_index::recognize_pcss(
            index,
            &notify.camera_address.to_string(),
            &notify.camera_name,
            notify.command_port,
            &notify.service,
        ) {
            Recognition::Candidate {
                model, connection, ..
            } => {
                acknowledge_callback(&transport).await?;
                let plan = connection_plan(&store, &model, &connection)?;
                // Connection-level host activities are emitted by the
                // socket-owning host; this executor must never synthesize
                // their lifecycle events.
                let _ = activity_observer;
                let mut activity = None;

                match establish_endpoint(
                    model.clone(),
                    connection.clone(),
                    notify.clone(),
                    &init,
                    &plan.retries,
                    plan.knock.connect_timeout_ms,
                    &transport,
                    &mut activity,
                    true,
                )
                .await
                {
                    Ok(outcome) => return succeed_activity(activity, outcome),
                    Err(first)
                        if first.recovery_eligible
                            && plan.knock.discovery_targets.retry_discovered_unicast =>
                    {
                        record_retry(&mut activity, 2, 2, executor_failure_kind(&first.error));
                        let recovered = match acquire_explicit_callback(
                            &store,
                            &model,
                            &connection,
                            notify.camera_address,
                            &plan.knock,
                            callback,
                            &transport,
                            &mut activity,
                        )
                        .await
                        {
                            Ok(recovered) => recovered,
                            Err(error) => return fail_activity(activity, error),
                        };
                        return match establish_endpoint(
                            model,
                            connection,
                            recovered,
                            &init,
                            &plan.retries,
                            plan.knock.connect_timeout_ms,
                            &transport,
                            &mut activity,
                            false,
                        )
                        .await
                        {
                            Ok(outcome) => succeed_activity(activity, outcome),
                            Err(error) => fail_activity(activity, error.error),
                        };
                    }
                    Err(error) => return fail_activity(activity, error.error),
                }
            }
            Recognition::NoMatch | Recognition::Disambiguate { .. } => {
                let _ = transport.close_callback_connection().await;
                continue;
            }
        }
    }
    Err(PcssExecutorError::DiscoveryTimedOut)
}

#[allow(clippy::too_many_arguments)]
#[uniffi::export]
pub async fn run_pcss_known_address_establishment(
    store: Arc<ConfigStore>,
    model: String,
    connection: String,
    camera_ipv4: String,
    callback_ipv4: String,
    initiator_guid: Vec<u8>,
    friendly_name: String,
    transport: Arc<dyn PcssExecutorTransport>,
    activity_observer: Option<Arc<dyn ConnectionActivityObserver>>,
) -> Result<PcssEstablishmentOutcome, PcssExecutorError> {
    let camera = parse_ipv4(&camera_ipv4)?;
    let callback = parse_ipv4(&callback_ipv4)?;
    let init = build_init(initiator_guid, callback, &friendly_name)?;
    let plan = connection_plan(&store, &model, &connection)?;
    // Connection-level activities are host checkpoints owned by the foreign
    // socket/session lifecycle, not executor spans.
    let _ = activity_observer;
    let mut activity = None;
    if let Err(error) = transport
        .bind_callback_listener(plan.knock.callback_port)
        .await
    {
        return fail_activity(activity, error.into());
    }
    let notify = match acquire_explicit_callback(
        &store,
        &model,
        &connection,
        camera,
        &plan.knock,
        callback,
        &transport,
        &mut activity,
    )
    .await
    {
        Ok(notify) => notify,
        Err(error) => return fail_activity(activity, error),
    };
    match establish_endpoint(
        model,
        connection,
        notify,
        &init,
        &plan.retries,
        plan.knock.connect_timeout_ms,
        &transport,
        &mut activity,
        false,
    )
    .await
    {
        Ok(outcome) => succeed_activity(activity, outcome),
        Err(error) => fail_activity(activity, error.error),
    }
}

#[derive(Clone)]
struct PcssConnectionPlan {
    knock: camera_config::PcssKnock,
    retries: camera_config::InitRetries,
}

fn connection_plan(
    store: &ConfigStore,
    model: &str,
    connection: &str,
) -> Result<PcssConnectionPlan, PcssExecutorError> {
    let body = store
        .inner
        .body(model)
        .ok_or_else(|| PcssExecutorError::UnknownPlan {
            detail: format!("unknown model '{model}'"),
        })?;
    let connection_config =
        body.connections
            .get(connection)
            .ok_or_else(|| PcssExecutorError::UnknownPlan {
                detail: format!("model '{model}' has no connection '{connection}'"),
            })?;
    let knock = connection_config
        .knock
        .clone()
        .ok_or_else(|| PcssExecutorError::UnknownPlan {
            detail: format!("connection '{connection}' has no PCSS rendezvous"),
        })?;
    Ok(PcssConnectionPlan {
        knock,
        retries: connection_config.init_retries.clone().unwrap_or_default(),
    })
}

fn build_init(
    initiator_guid: Vec<u8>,
    callback: Ipv4Addr,
    friendly_name: &str,
) -> Result<Vec<u8>, PcssExecutorError> {
    let guid: [u8; 16] = initiator_guid.try_into().map_err(|value: Vec<u8>| {
        PcssExecutorError::InvalidInitResponse {
            detail: format!("initiator GUID is {} bytes, expected 16", value.len()),
        }
    })?;
    protocol_primitives::pcss_init_message(guid, callback, friendly_name).map_err(|error| {
        PcssExecutorError::InvalidInitResponse {
            detail: error.to_string(),
        }
    })
}

#[allow(clippy::too_many_arguments)]
async fn acquire_explicit_callback(
    store: &ConfigStore,
    model: &str,
    connection: &str,
    camera: Ipv4Addr,
    knock: &camera_config::PcssKnock,
    callback: Ipv4Addr,
    transport: &Arc<dyn PcssExecutorTransport>,
    activity: &mut Option<ActiveActivity>,
) -> Result<protocol_primitives::PcssNotify, PcssExecutorError> {
    let discovery = protocol_primitives::pcss_discovery_message(callback, &knock.protocol);
    for attempt in 1..=knock.max_attempts {
        transport
            .send_discovery(camera.to_string(), knock.knock_port, discovery.clone())
            .await
            .map_err(PcssExecutorError::from)?;
        let callback_payload = match with_deadline_source(
            transport,
            knock.retry_interval_ms,
            transport.next_callback(),
        )
        .await
        {
            Ok(payload) => payload,
            Err(error) if error.callback_timed_out() => {
                if attempt < knock.max_attempts {
                    record_retry(
                        activity,
                        attempt.saturating_add(1),
                        knock.max_attempts,
                        ExecutorStepFailureKind::DeadlineExceeded,
                    );
                }
                continue;
            }
            Err(error) => return Err(error.into_executor("unicast callback")),
        };
        let notify = match protocol_primitives::parse_pcss_notify(
            &callback_payload.payload,
            &knock.protocol,
        ) {
            Ok(notify) => notify,
            Err(_) => {
                let _ = transport.close_callback_connection().await;
                continue;
            }
        };
        let peer = match parse_ipv4(&callback_payload.peer_ipv4) {
            Ok(peer) => peer,
            Err(error) => {
                let _ = transport.close_callback_connection().await;
                return Err(error);
            }
        };
        if peer != camera || notify.camera_address != camera {
            let _ = transport.close_callback_connection().await;
            return Err(PcssExecutorError::IdentityMismatch {
                model: model.into(),
            });
        }
        if !matches_model(store, model, connection, &notify) {
            let _ = transport.close_callback_connection().await;
            return Err(PcssExecutorError::IdentityMismatch {
                model: model.into(),
            });
        }
        acknowledge_callback(transport).await?;
        return Ok(notify);
    }
    Err(PcssExecutorError::DiscoveryTimedOut)
}

struct EndpointAttemptError {
    error: PcssExecutorError,
    recovery_eligible: bool,
}

impl EndpointAttemptError {
    fn terminal(error: PcssExecutorError) -> Self {
        Self {
            error,
            recovery_eligible: false,
        }
    }

    fn endpoint(error: PcssExecutorError) -> Self {
        Self {
            error,
            recovery_eligible: true,
        }
    }
}

enum InitResponse {
    Ack(protocol_primitives::PcssInitAck),
    Fail(u32),
}

fn parse_init_response(frame: &[u8]) -> Result<InitResponse, PcssExecutorError> {
    match protocol_primitives::parse_pcss_init_ack(frame) {
        Ok(ack) => Ok(InitResponse::Ack(ack)),
        Err(ack_error) => match canonical_init_fail_reason(frame) {
            Some(reason) => Ok(InitResponse::Fail(reason)),
            None => match PtpIpPacket::decode(frame) {
                Ok(other) => Err(PcssExecutorError::InvalidInitResponse {
                    detail: format!("expected PCSS InitCommandAck or InitFail, got {other:?}"),
                }),
                Err(_) => Err(PcssExecutorError::InvalidInitResponse {
                    detail: ack_error.to_string(),
                }),
            },
        },
    }
}

fn canonical_init_fail_reason(frame: &[u8]) -> Option<u32> {
    if frame.len() != 12 {
        return None;
    }
    match PtpIpPacket::decode(frame).ok()? {
        PtpIpPacket::InitFail(failure) => Some(failure.reason),
        _ => None,
    }
}

async fn acknowledge_callback(
    transport: &Arc<dyn PcssExecutorTransport>,
) -> Result<(), PcssExecutorError> {
    if let Err(error) = transport
        .send_callback_reply(protocol_primitives::pcss_callback_ack_message())
        .await
    {
        let _ = transport.close_callback_connection().await;
        return Err(error.into());
    }
    transport
        .close_callback_connection()
        .await
        .map_err(Into::into)
}

#[allow(clippy::too_many_arguments)]
async fn establish_endpoint(
    model: String,
    connection: String,
    notify: protocol_primitives::PcssNotify,
    init: &[u8],
    retries: &camera_config::InitRetries,
    connect_timeout_ms: u32,
    transport: &Arc<dyn PcssExecutorTransport>,
    activity: &mut Option<ActiveActivity>,
    permit_recovery: bool,
) -> Result<PcssEstablishmentOutcome, EndpointAttemptError> {
    let connected = with_deadline_source(
        transport,
        connect_timeout_ms,
        transport.connect_command(notify.camera_address.to_string(), notify.command_port),
    )
    .await;
    if let Err(error) = connected {
        let recovery_eligible = permit_recovery && error.endpoint_eligible();
        let error = error.into_executor("command endpoint connect");
        return Err(finish_failed_endpoint(transport, error, recovery_eligible).await);
    }

    for init_attempt in 0..=retries.max {
        if let Err(error) = with_deadline_source(
            transport,
            connect_timeout_ms,
            transport.send_command_frame(init.to_vec()),
        )
        .await
        {
            let recovery_eligible =
                permit_recovery && init_attempt == 0 && error.endpoint_eligible();
            let error = error.into_executor("PTP/IP Init request");
            return Err(finish_failed_endpoint(transport, error, recovery_eligible).await);
        }
        let frame = match with_deadline_source(
            transport,
            connect_timeout_ms,
            transport.next_command_frame(),
        )
        .await
        {
            Ok(frame) => frame,
            Err(error) => {
                let recovery_eligible =
                    permit_recovery && init_attempt == 0 && error.endpoint_eligible();
                let error = error.into_executor("PTP/IP Init response");
                return Err(finish_failed_endpoint(transport, error, recovery_eligible).await);
            }
        };
        match parse_init_response(&frame) {
            Ok(InitResponse::Ack(ack)) => {
                return Ok(PcssEstablishmentOutcome {
                    model,
                    connection,
                    camera_ipv4: notify.camera_address.to_string(),
                    camera_name: notify.camera_name,
                    command_port: notify.command_port,
                    service: notify.service,
                    connection_number: ack.connection_number,
                    responder_guid: ack.responder_guid.to_vec(),
                    responder_name: ack.friendly_name,
                });
            }
            Ok(InitResponse::Fail(reason))
                if init_attempt < retries.max
                    && retries.when_reasons.iter().any(|configured| {
                        camera_config::parse_hex_u32(configured).is_some_and(|code| code == reason)
                    }) =>
            {
                record_retry(
                    activity,
                    init_attempt.saturating_add(2),
                    retries.max.saturating_add(1),
                    ExecutorStepFailureKind::ConditionRejected,
                );
                if let Err(error) = transport.sleep(retries.backoff_ms).await {
                    let _ = transport.close_command_connection().await;
                    return Err(EndpointAttemptError::terminal(error.into()));
                }
            }
            Ok(InitResponse::Fail(reason)) => {
                let _ = transport.close_command_connection().await;
                return Err(EndpointAttemptError::terminal(
                    PcssExecutorError::InitRejected { reason },
                ));
            }
            Err(error) => {
                let _ = transport.close_command_connection().await;
                return Err(EndpointAttemptError::terminal(error));
            }
        }
    }
    unreachable!("the inclusive Init retry loop always returns or advances")
}

async fn finish_failed_endpoint(
    transport: &Arc<dyn PcssExecutorTransport>,
    error: PcssExecutorError,
    recovery_eligible: bool,
) -> EndpointAttemptError {
    match transport.close_command_connection().await {
        Err(close_error) if recovery_eligible => EndpointAttemptError::terminal(close_error.into()),
        _ if recovery_eligible => EndpointAttemptError::endpoint(error),
        _ => EndpointAttemptError::terminal(error),
    }
}

fn matches_model(
    store: &ConfigStore,
    model: &str,
    connection: &str,
    notify: &protocol_primitives::PcssNotify,
) -> bool {
    let Some(index) = store.inner.index.as_ref() else {
        return false;
    };
    matches!(
        crate::mfg_index::recognize_pcss(
            index,
            &notify.camera_address.to_string(),
            &notify.camera_name,
            notify.command_port,
            &notify.service,
        ),
        Recognition::Candidate { model: matched_model, connection: matched_connection, .. }
            if matched_model == model && matched_connection == connection
    )
}

fn select_auto_discovery_policy(
    index: &camera_config::index::ResolvedManufacturerIndex,
) -> Result<camera_config::index::FamilyPcssBlock, PcssExecutorError> {
    let policy = index
        .models
        .iter()
        .find_map(|model| model.pcss.as_ref())
        .ok_or_else(|| PcssExecutorError::UnknownPlan {
            detail: "manufacturer index has no PCSS discovery policy".into(),
        })?;
    if index
        .models
        .iter()
        .filter_map(|model| model.pcss.as_ref())
        .any(|candidate| candidate != policy)
    {
        return Err(PcssExecutorError::UnknownPlan {
            detail: "manufacturer index has multiple PCSS discovery policies; automatic discovery requires an unambiguous family policy".into(),
        });
    }
    Ok(policy.clone())
}

fn record_retry(
    activity: &mut Option<ActiveActivity>,
    ordinal: u32,
    limit: u32,
    kind: ExecutorStepFailureKind,
) {
    if let Some(activity) = activity {
        activity.retry(ConnectionActivityRetry {
            ordinal,
            limit,
            failure: ConnectionActivityFailure::without_context(kind),
        });
    }
}

fn fail_activity<T>(
    activity: Option<ActiveActivity>,
    error: PcssExecutorError,
) -> Result<T, PcssExecutorError> {
    if let Some(activity) = activity {
        activity.fail(ConnectionActivityFailure::without_context(
            executor_failure_kind(&error),
        ));
    }
    Err(error)
}

fn executor_failure_kind(error: &PcssExecutorError) -> ExecutorStepFailureKind {
    if matches!(error, PcssExecutorError::DeadlineExceeded { .. }) {
        ExecutorStepFailureKind::DeadlineExceeded
    } else {
        ExecutorStepFailureKind::Other
    }
}

fn succeed_activity(
    activity: Option<ActiveActivity>,
    outcome: PcssEstablishmentOutcome,
) -> Result<PcssEstablishmentOutcome, PcssExecutorError> {
    if let Some(activity) = activity {
        activity.succeed();
    }
    Ok(outcome)
}

fn parse_ipv4(value: &str) -> Result<Ipv4Addr, PcssExecutorError> {
    value
        .parse()
        .map_err(|error| PcssExecutorError::InvalidAddress {
            detail: format!("'{value}': {error}"),
        })
}

enum TimedIoError {
    Operation(TransportError),
    Deadline,
    Clock(TransportError),
}

impl TimedIoError {
    fn callback_timed_out(&self) -> bool {
        matches!(
            self,
            Self::Deadline | Self::Operation(TransportError::Timeout { .. })
        )
    }

    fn endpoint_eligible(&self) -> bool {
        match self {
            Self::Operation(error) => transport_error_endpoint_eligible(error),
            Self::Deadline => true,
            Self::Clock(_) => false,
        }
    }

    fn into_executor(self, stage: &str) -> PcssExecutorError {
        match self {
            Self::Operation(error) | Self::Clock(error) => error.into(),
            Self::Deadline => PcssExecutorError::DeadlineExceeded {
                stage: stage.into(),
            },
        }
    }
}

fn transport_error_endpoint_eligible(error: &TransportError) -> bool {
    matches!(
        error,
        TransportError::NotConnected
            | TransportError::Timeout { .. }
            | TransportError::ConnectFailed { .. }
    )
}

async fn with_deadline_source<T, F>(
    transport: &Arc<dyn PcssExecutorTransport>,
    timeout_ms: u32,
    future: F,
) -> Result<T, TimedIoError>
where
    F: Future<Output = Result<T, TransportError>>,
{
    match select(Box::pin(future), Box::pin(transport.sleep(timeout_ms))).await {
        Either::Left((result, _)) => result.map_err(TimedIoError::Operation),
        Either::Right((Ok(()), _)) => Err(TimedIoError::Deadline),
        Either::Right((Err(error), _)) => Err(TimedIoError::Clock(error)),
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use camera_config::index::{
        FamilyPcssBlock, ModelView, PcssDiscoveryPolicy, ResolvedManufacturerIndex,
    };

    use super::*;

    fn model(id: &str, knock_port: u16) -> ModelView {
        ModelView {
            id: id.into(),
            display_name: id.into(),
            fallback: false,
            manifest_path: PathBuf::from(format!("{id}.yaml")),
            ble: None,
            pcss: Some(FamilyPcssBlock {
                callback_port: 51560,
                knock_port,
                protocol: "PCSS/1.0".into(),
                discovery: PcssDiscoveryPolicy {
                    retry_interval_ms: 1_000,
                    max_attempts: 10,
                },
            }),
            signatures: Vec::new(),
        }
    }

    #[test]
    fn auto_discovery_rejects_ambiguous_family_policies() {
        let index = ResolvedManufacturerIndex {
            manufacturer: "example".into(),
            models: vec![model("one", 51562), model("two", 51563)],
        };

        assert!(matches!(
            select_auto_discovery_policy(&index),
            Err(PcssExecutorError::UnknownPlan { .. })
        ));
    }
}
