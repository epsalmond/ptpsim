//! Manifest-driven PCSS discovery and command-session establishment.
//!
//! Broadcast discovery is only an optional address-finding front end. Both it
//! and a caller-supplied address converge on the same unicast rendezvous and
//! PTP/IP Init path.

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
pub trait PcssExecutorTransport: Send + Sync {
    async fn bind_callback_listener(&self, port: u16) -> Result<(), TransportError>;
    async fn send_discovery(
        &self,
        destination_ipv4: String,
        destination_port: u16,
        payload: Vec<u8>,
    ) -> Result<(), TransportError>;
    async fn next_callback(&self) -> Result<Vec<u8>, TransportError>;
    async fn send_callback_reply(&self, payload: Vec<u8>) -> Result<(), TransportError>;
    async fn close_callback_connection(&self) -> Result<(), TransportError>;
    async fn connect_command(&self, camera_ipv4: String, port: u16) -> Result<(), TransportError>;
    async fn send_command_frame(&self, frame: Vec<u8>) -> Result<(), TransportError>;
    async fn next_command_frame(&self) -> Result<Vec<u8>, TransportError>;
    async fn close_command_connection(&self) -> Result<(), TransportError>;
    async fn sleep(&self, ms: u32) -> Result<(), TransportError>;
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
        let payload = match with_deadline(
            &transport,
            "discovery callback",
            policy.discovery.retry_interval_ms,
            transport.next_callback(),
        )
        .await
        {
            Ok(payload) => payload,
            Err(PcssExecutorError::DeadlineExceeded { .. }) => continue,
            Err(error) => return Err(error),
        };
        let Ok(notify) = protocol_primitives::parse_pcss_notify(&payload, &policy.protocol) else {
            let _ = transport.close_callback_connection().await;
            continue;
        };
        transport
            .send_callback_reply(protocol_primitives::pcss_callback_ack_message())
            .await?;
        transport.close_callback_connection().await?;
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
                return establish_known(
                    store,
                    model,
                    connection,
                    notify.camera_address,
                    callback,
                    initiator_guid,
                    friendly_name,
                    transport,
                    activity_observer,
                    true,
                )
                .await;
            }
            Recognition::NoMatch | Recognition::Disambiguate { .. } => continue,
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
    establish_known(
        store,
        model,
        connection,
        camera,
        callback,
        initiator_guid,
        friendly_name,
        transport,
        activity_observer,
        false,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn establish_known(
    store: Arc<ConfigStore>,
    model: String,
    connection: String,
    camera: Ipv4Addr,
    callback: Ipv4Addr,
    initiator_guid: Vec<u8>,
    friendly_name: String,
    transport: Arc<dyn PcssExecutorTransport>,
    activity_observer: Option<Arc<dyn ConnectionActivityObserver>>,
    listener_bound: bool,
) -> Result<PcssEstablishmentOutcome, PcssExecutorError> {
    let body = store
        .inner
        .body(&model)
        .ok_or_else(|| PcssExecutorError::UnknownPlan {
            detail: format!("unknown model '{model}'"),
        })?;
    let connection_config =
        body.connections
            .get(&connection)
            .ok_or_else(|| PcssExecutorError::UnknownPlan {
                detail: format!("model '{model}' has no connection '{connection}'"),
            })?;
    let knock = connection_config
        .knock
        .clone()
        .ok_or_else(|| PcssExecutorError::UnknownPlan {
            detail: format!("connection '{connection}' has no PCSS rendezvous"),
        })?;
    let retries = connection_config.init_retries.clone().unwrap_or_default();
    let discovery = protocol_primitives::pcss_discovery_message(callback, &knock.protocol);
    let guid: [u8; 16] = initiator_guid.try_into().map_err(|value: Vec<u8>| {
        PcssExecutorError::InvalidInitResponse {
            detail: format!("initiator GUID is {} bytes, expected 16", value.len()),
        }
    })?;
    let init = protocol_primitives::pcss_init_message(guid, callback, &friendly_name).map_err(
        |error| PcssExecutorError::InvalidInitResponse {
            detail: error.to_string(),
        },
    )?;
    let mut activity = activity_observer.and_then(|observer| {
        connection_config.activities.first().map(|descriptor| {
            ActiveActivity::new(observer, descriptor.id.clone(), descriptor.version)
        })
    });
    if !listener_bound {
        if let Err(error) = transport.bind_callback_listener(knock.callback_port).await {
            return fail_activity(activity, error.into());
        }
    }

    for attempt in 1..=knock.max_attempts {
        if let Err(error) = transport
            .send_discovery(camera.to_string(), knock.knock_port, discovery.clone())
            .await
        {
            return fail_activity(activity, error.into());
        }
        let callback_payload = match with_deadline(
            &transport,
            "unicast callback",
            knock.retry_interval_ms,
            transport.next_callback(),
        )
        .await
        {
            Ok(payload) => payload,
            Err(PcssExecutorError::DeadlineExceeded { .. }) => {
                record_retry(
                    &mut activity,
                    attempt,
                    knock.max_attempts,
                    ExecutorStepFailureKind::DeadlineExceeded,
                );
                continue;
            }
            Err(error) => return fail_activity(activity, error),
        };
        let notify =
            match protocol_primitives::parse_pcss_notify(&callback_payload, &knock.protocol) {
                Ok(notify) => notify,
                Err(_) => {
                    let _ = transport.close_callback_connection().await;
                    continue;
                }
            };
        if let Err(error) = transport
            .send_callback_reply(protocol_primitives::pcss_callback_ack_message())
            .await
        {
            return fail_activity(activity, error.into());
        }
        if let Err(error) = transport.close_callback_connection().await {
            return fail_activity(activity, error.into());
        }
        if !matches_model(&store, &model, &connection, &notify) {
            return fail_activity(activity, PcssExecutorError::IdentityMismatch { model });
        }

        let connected = with_deadline(
            &transport,
            "command endpoint connect",
            knock.connect_timeout_ms,
            transport.connect_command(notify.camera_address.to_string(), notify.command_port),
        )
        .await;
        if let Err(error) = connected {
            let _ = transport.close_command_connection().await;
            let kind = if matches!(error, PcssExecutorError::DeadlineExceeded { .. }) {
                ExecutorStepFailureKind::DeadlineExceeded
            } else {
                ExecutorStepFailureKind::Other
            };
            record_retry(&mut activity, attempt, knock.max_attempts, kind);
            if attempt < knock.max_attempts {
                if let Err(error) = transport.sleep(knock.retry_interval_ms).await {
                    return fail_activity(activity, error.into());
                }
                continue;
            }
            return fail_activity(
                activity,
                PcssExecutorError::EndpointUnavailable {
                    attempts: knock.max_attempts,
                },
            );
        }

        for init_attempt in 0..=retries.max {
            if let Err(error) = transport.send_command_frame(init.clone()).await {
                let _ = transport.close_command_connection().await;
                return fail_activity(activity, error.into());
            }
            let frame = match with_deadline(
                &transport,
                "PTP/IP Init response",
                knock.connect_timeout_ms,
                transport.next_command_frame(),
            )
            .await
            {
                Ok(frame) => frame,
                Err(error) => {
                    let _ = transport.close_command_connection().await;
                    return fail_activity(activity, error);
                }
            };
            let packet = match PtpIpPacket::decode(&frame) {
                Ok(packet) => packet,
                Err(error) => {
                    let _ = transport.close_command_connection().await;
                    return fail_activity(
                        activity,
                        PcssExecutorError::InvalidInitResponse {
                            detail: error.to_string(),
                        },
                    );
                }
            };
            match packet {
                PtpIpPacket::InitCommandAck(ack) => {
                    if let Some(activity) = activity.take() {
                        activity.succeed();
                    }
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
                PtpIpPacket::InitFail(failure)
                    if init_attempt < retries.max
                        && retries.when_reasons.iter().any(|reason| {
                            camera_config::parse_hex_code(reason)
                                .is_some_and(|code| u32::from(code) == failure.reason)
                        }) =>
                {
                    record_retry(
                        &mut activity,
                        init_attempt + 1,
                        retries.max,
                        ExecutorStepFailureKind::ConditionRejected,
                    );
                    if let Err(error) = transport.sleep(retries.backoff_ms).await {
                        let _ = transport.close_command_connection().await;
                        return fail_activity(activity, error.into());
                    }
                }
                PtpIpPacket::InitFail(failure) => {
                    let _ = transport.close_command_connection().await;
                    return fail_activity(
                        activity,
                        PcssExecutorError::InitRejected {
                            reason: failure.reason,
                        },
                    );
                }
                other => {
                    let _ = transport.close_command_connection().await;
                    return fail_activity(
                        activity,
                        PcssExecutorError::InvalidInitResponse {
                            detail: format!("expected InitCommandAck or InitFail, got {other:?}"),
                        },
                    );
                }
            }
        }
    }
    fail_activity(
        activity,
        PcssExecutorError::EndpointUnavailable {
            attempts: knock.max_attempts,
        },
    )
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
        let kind = if matches!(error, PcssExecutorError::DeadlineExceeded { .. }) {
            ExecutorStepFailureKind::DeadlineExceeded
        } else {
            ExecutorStepFailureKind::Other
        };
        activity.fail(ConnectionActivityFailure::without_context(kind));
    }
    Err(error)
}

fn parse_ipv4(value: &str) -> Result<Ipv4Addr, PcssExecutorError> {
    value
        .parse()
        .map_err(|error| PcssExecutorError::InvalidAddress {
            detail: format!("'{value}': {error}"),
        })
}

async fn with_deadline<T, F>(
    transport: &Arc<dyn PcssExecutorTransport>,
    stage: &str,
    timeout_ms: u32,
    future: F,
) -> Result<T, PcssExecutorError>
where
    F: Future<Output = Result<T, TransportError>>,
{
    match select(Box::pin(future), Box::pin(transport.sleep(timeout_ms))).await {
        Either::Left((result, _)) => result.map_err(Into::into),
        Either::Right((Ok(()), _)) => Err(PcssExecutorError::DeadlineExceeded {
            stage: stage.into(),
        }),
        Either::Right((Err(error), _)) => Err(error.into()),
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
