//! Versioned dynamic connection lifecycle contract.
//!
//! The layers are deliberately orthogonal. A transition changes only the
//! layers for which that observation is evidence. In particular, transport
//! teardown does not claim that a listener or network persona ended.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

pub const CONNECTION_LIFECYCLE_SCHEMA_VERSION: &str = "ptpsim-connection-lifecycle/v1";
const MAX_CONNECTION_LIFECYCLE_TRACE_EVENTS: usize = 512;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub enum ConnectionLifecycleSchema {
    #[default]
    #[serde(rename = "ptpsim-connection-lifecycle/v1")]
    V1,
}

/// Evidence carried by one lifecycle layer.
///
/// `Unconfirmed(value)` records the last observed or requested value without
/// claiming that the camera currently has that value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(
    tag = "evidence",
    content = "value",
    rename_all = "camelCase",
    deny_unknown_fields
)]
pub enum LifecycleEvidence<T> {
    Unknown,
    Unconfirmed(T),
    Confirmed(T),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum NetworkPersonaState {
    Ready,
    Exited,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum CommandListenerState {
    Ready,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum ProxyTransportState {
    Accepted,
    CommandInitialized,
    Lost,
    LocallyClosed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum PtpSessionState {
    Open,
    Closed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum FeatureMode {
    LiveView,
    ImageTransfer,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "state", rename_all = "camelCase", deny_unknown_fields)]
pub enum FeatureState {
    Inactive,
    Active { mode: FeatureMode },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ConnectionLifecycleSnapshot {
    pub schema: ConnectionLifecycleSchema,
    #[schemars(range(max = u64::MAX))]
    pub sequence: u64,
    pub network_persona: LifecycleEvidence<NetworkPersonaState>,
    pub command_listener: LifecycleEvidence<CommandListenerState>,
    pub proxy_transport: LifecycleEvidence<ProxyTransportState>,
    pub ptp_session: LifecycleEvidence<PtpSessionState>,
    pub feature: LifecycleEvidence<FeatureState>,
}

impl Default for ConnectionLifecycleSnapshot {
    fn default() -> Self {
        Self {
            schema: ConnectionLifecycleSchema::V1,
            sequence: 0,
            network_persona: LifecycleEvidence::Unknown,
            command_listener: LifecycleEvidence::Unknown,
            proxy_transport: LifecycleEvidence::Unknown,
            ptp_session: LifecycleEvidence::Unknown,
            feature: LifecycleEvidence::Unknown,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "camelCase", deny_unknown_fields)]
pub enum ConnectionLifecycleTransition {
    NetworkPersonaReady,
    NetworkPersonaExitRequested,
    NetworkPersonaExitConfirmed,
    CommandListenerReady,
    ProxyTransportAccepted,
    CommandInitialized,
    ProxyTransportLost,
    ProxyTransportClosedLocally,
    PtpSessionOpened,
    PtpSessionClosed,
    FeatureEntered { mode: FeatureMode },
    FeatureExited,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ConnectionLifecycleTraceEvent {
    pub schema: ConnectionLifecycleSchema,
    #[schemars(range(max = u64::MAX))]
    pub sequence: u64,
    pub transition: ConnectionLifecycleTransition,
    pub before: ConnectionLifecycleSnapshot,
    pub after: ConnectionLifecycleSnapshot,
}

/// JSON records accepted by the versioned schema.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(
    tag = "kind",
    content = "record",
    rename_all = "camelCase",
    deny_unknown_fields
)]
pub enum ConnectionLifecycleRecord {
    Snapshot(ConnectionLifecycleSnapshot),
    TraceEvent(ConnectionLifecycleTraceEvent),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ConnectionLifecycleError {
    #[error("connection lifecycle sequence overflow")]
    SequenceOverflow,
}

/// Apply one observed transition without consulting simulator state.
pub fn apply_connection_lifecycle_transition(
    before: &ConnectionLifecycleSnapshot,
    transition: ConnectionLifecycleTransition,
) -> Result<ConnectionLifecycleTraceEvent, ConnectionLifecycleError> {
    let sequence = before
        .sequence
        .checked_add(1)
        .ok_or(ConnectionLifecycleError::SequenceOverflow)?;
    let mut after = before.clone();
    after.sequence = sequence;

    match transition {
        ConnectionLifecycleTransition::NetworkPersonaReady => {
            after.network_persona = LifecycleEvidence::Confirmed(NetworkPersonaState::Ready);
        }
        ConnectionLifecycleTransition::NetworkPersonaExitRequested => {
            after.network_persona = LifecycleEvidence::Unconfirmed(NetworkPersonaState::Exited);
        }
        ConnectionLifecycleTransition::NetworkPersonaExitConfirmed => {
            after.network_persona = LifecycleEvidence::Confirmed(NetworkPersonaState::Exited);
        }
        ConnectionLifecycleTransition::CommandListenerReady => {
            after.command_listener = LifecycleEvidence::Confirmed(CommandListenerState::Ready);
        }
        ConnectionLifecycleTransition::ProxyTransportAccepted => {
            after.proxy_transport = LifecycleEvidence::Confirmed(ProxyTransportState::Accepted);
        }
        ConnectionLifecycleTransition::CommandInitialized => {
            // A successful command init is also direct evidence that the
            // command listener accepted traffic.
            after.command_listener = LifecycleEvidence::Confirmed(CommandListenerState::Ready);
            after.proxy_transport =
                LifecycleEvidence::Confirmed(ProxyTransportState::CommandInitialized);
        }
        ConnectionLifecycleTransition::ProxyTransportLost => {
            after.proxy_transport = LifecycleEvidence::Confirmed(ProxyTransportState::Lost);
            mark_open_layers_unconfirmed(&mut after);
        }
        ConnectionLifecycleTransition::ProxyTransportClosedLocally => {
            after.proxy_transport =
                LifecycleEvidence::Confirmed(ProxyTransportState::LocallyClosed);
            mark_open_layers_unconfirmed(&mut after);
        }
        ConnectionLifecycleTransition::PtpSessionOpened => {
            after.ptp_session = LifecycleEvidence::Confirmed(PtpSessionState::Open);
        }
        ConnectionLifecycleTransition::PtpSessionClosed => {
            after.ptp_session = LifecycleEvidence::Confirmed(PtpSessionState::Closed);
            after.feature = LifecycleEvidence::Confirmed(FeatureState::Inactive);
        }
        ConnectionLifecycleTransition::FeatureEntered { mode } => {
            after.feature = LifecycleEvidence::Confirmed(FeatureState::Active { mode });
        }
        ConnectionLifecycleTransition::FeatureExited => {
            after.feature = LifecycleEvidence::Confirmed(FeatureState::Inactive);
        }
    }

    Ok(ConnectionLifecycleTraceEvent {
        schema: ConnectionLifecycleSchema::V1,
        sequence,
        transition,
        before: before.clone(),
        after,
    })
}

fn mark_open_layers_unconfirmed(snapshot: &mut ConnectionLifecycleSnapshot) {
    if matches!(
        snapshot.ptp_session,
        LifecycleEvidence::Confirmed(PtpSessionState::Open)
    ) {
        snapshot.ptp_session = LifecycleEvidence::Unconfirmed(PtpSessionState::Open);
    }
    if let LifecycleEvidence::Confirmed(FeatureState::Active { mode }) = snapshot.feature {
        snapshot.feature = LifecycleEvidence::Unconfirmed(FeatureState::Active { mode });
    }
}

#[derive(Debug, Clone, Default)]
pub struct ConnectionLifecycleTracker {
    snapshot: ConnectionLifecycleSnapshot,
    trace: Vec<ConnectionLifecycleTraceEvent>,
}

impl ConnectionLifecycleTracker {
    pub fn snapshot(&self) -> &ConnectionLifecycleSnapshot {
        &self.snapshot
    }

    pub fn trace(&self) -> &[ConnectionLifecycleTraceEvent] {
        &self.trace
    }

    pub fn apply(
        &mut self,
        transition: ConnectionLifecycleTransition,
    ) -> Result<&ConnectionLifecycleTraceEvent, ConnectionLifecycleError> {
        let event = apply_connection_lifecycle_transition(&self.snapshot, transition)?;
        self.snapshot = event.after.clone();
        self.trace.push(event);
        if self.trace.len() > MAX_CONNECTION_LIFECYCLE_TRACE_EVENTS {
            self.trace.remove(0);
        }
        Ok(self.trace.last().expect("event was just pushed"))
    }
}

pub fn generated_connection_lifecycle_json_schema() -> Result<String, serde_json::Error> {
    let schema = schemars::schema_for!(ConnectionLifecycleRecord);
    let mut text = serde_json::to_string_pretty(&schema)?;
    text.push('\n');
    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn applied(
        snapshot: &ConnectionLifecycleSnapshot,
        transition: ConnectionLifecycleTransition,
    ) -> ConnectionLifecycleSnapshot {
        apply_connection_lifecycle_transition(snapshot, transition)
            .unwrap()
            .after
    }

    fn connected_feature(mode: FeatureMode) -> ConnectionLifecycleSnapshot {
        let mut snapshot = ConnectionLifecycleSnapshot::default();
        for transition in [
            ConnectionLifecycleTransition::NetworkPersonaReady,
            ConnectionLifecycleTransition::CommandListenerReady,
            ConnectionLifecycleTransition::ProxyTransportAccepted,
            ConnectionLifecycleTransition::CommandInitialized,
            ConnectionLifecycleTransition::PtpSessionOpened,
            ConnectionLifecycleTransition::FeatureEntered { mode },
        ] {
            snapshot = applied(&snapshot, transition);
        }
        snapshot
    }

    #[test]
    fn close_session_changes_only_ptp_and_feature_layers() {
        let before = connected_feature(FeatureMode::LiveView);
        let after = applied(&before, ConnectionLifecycleTransition::PtpSessionClosed);

        assert_eq!(after.network_persona, before.network_persona);
        assert_eq!(after.command_listener, before.command_listener);
        assert_eq!(after.proxy_transport, before.proxy_transport);
        assert_eq!(
            after.ptp_session,
            LifecycleEvidence::Confirmed(PtpSessionState::Closed)
        );
        assert_eq!(
            after.feature,
            LifecycleEvidence::Confirmed(FeatureState::Inactive)
        );
    }

    #[test]
    fn transport_loss_preserves_lower_layers_and_does_not_claim_camera_cleanup() {
        let before = connected_feature(FeatureMode::LiveView);
        let after = applied(&before, ConnectionLifecycleTransition::ProxyTransportLost);

        assert_eq!(after.network_persona, before.network_persona);
        assert_eq!(after.command_listener, before.command_listener);
        assert_eq!(
            after.proxy_transport,
            LifecycleEvidence::Confirmed(ProxyTransportState::Lost)
        );
        assert_eq!(
            after.ptp_session,
            LifecycleEvidence::Unconfirmed(PtpSessionState::Open)
        );
        assert_eq!(
            after.feature,
            LifecycleEvidence::Unconfirmed(FeatureState::Active {
                mode: FeatureMode::LiveView
            })
        );
    }

    #[test]
    fn local_close_has_the_same_remote_uncertainty_as_transport_loss() {
        let before = connected_feature(FeatureMode::ImageTransfer);
        let after = applied(
            &before,
            ConnectionLifecycleTransition::ProxyTransportClosedLocally,
        );

        assert_eq!(after.network_persona, before.network_persona);
        assert_eq!(after.command_listener, before.command_listener);
        assert_eq!(
            after.proxy_transport,
            LifecycleEvidence::Confirmed(ProxyTransportState::LocallyClosed)
        );
        assert_eq!(
            after.ptp_session,
            LifecycleEvidence::Unconfirmed(PtpSessionState::Open)
        );
        assert_eq!(
            after.feature,
            LifecycleEvidence::Unconfirmed(FeatureState::Active {
                mode: FeatureMode::ImageTransfer
            })
        );
    }

    #[test]
    fn requested_network_exit_remains_unconfirmed_until_separate_evidence() {
        let ready = applied(
            &ConnectionLifecycleSnapshot::default(),
            ConnectionLifecycleTransition::NetworkPersonaReady,
        );
        let requested = applied(
            &ready,
            ConnectionLifecycleTransition::NetworkPersonaExitRequested,
        );
        assert_eq!(
            requested.network_persona,
            LifecycleEvidence::Unconfirmed(NetworkPersonaState::Exited)
        );

        let confirmed = applied(
            &requested,
            ConnectionLifecycleTransition::NetworkPersonaExitConfirmed,
        );
        assert_eq!(
            confirmed.network_persona,
            LifecycleEvidence::Confirmed(NetworkPersonaState::Exited)
        );
    }

    #[test]
    fn schema_rejects_unknown_fields_and_unknown_versions() {
        let mut value = serde_json::to_value(ConnectionLifecycleRecord::Snapshot(
            ConnectionLifecycleSnapshot::default(),
        ))
        .unwrap();
        value["record"]["firewall"] = serde_json::json!("open");
        assert!(serde_json::from_value::<ConnectionLifecycleRecord>(value).is_err());

        let mut value = serde_json::to_value(ConnectionLifecycleRecord::Snapshot(
            ConnectionLifecycleSnapshot::default(),
        ))
        .unwrap();
        value["record"]["schema"] = serde_json::json!("ptpsim-connection-lifecycle/v2");
        assert!(serde_json::from_value::<ConnectionLifecycleRecord>(value).is_err());
    }

    #[test]
    fn schema_rejects_missing_fields_unknown_states_and_overflow() {
        let mut missing = serde_json::to_value(ConnectionLifecycleRecord::Snapshot(
            ConnectionLifecycleSnapshot::default(),
        ))
        .unwrap();
        missing["record"].as_object_mut().unwrap().remove("feature");
        assert!(serde_json::from_value::<ConnectionLifecycleRecord>(missing).is_err());

        let mut unknown_state = serde_json::to_value(ConnectionLifecycleRecord::Snapshot(
            ConnectionLifecycleSnapshot::default(),
        ))
        .unwrap();
        unknown_state["record"]["proxyTransport"] =
            serde_json::json!({ "evidence": "confirmed", "value": "firewalled" });
        assert!(serde_json::from_value::<ConnectionLifecycleRecord>(unknown_state).is_err());

        let mut overflow = serde_json::to_value(ConnectionLifecycleRecord::Snapshot(
            ConnectionLifecycleSnapshot::default(),
        ))
        .unwrap();
        overflow["record"]["sequence"] = serde_json::from_str("18446744073709551616").unwrap();
        assert!(serde_json::from_value::<ConnectionLifecycleRecord>(overflow).is_err());
    }

    #[test]
    fn trace_sequence_and_snapshots_are_deterministic() {
        let mut tracker = ConnectionLifecycleTracker::default();
        tracker
            .apply(ConnectionLifecycleTransition::ProxyTransportAccepted)
            .unwrap();
        tracker
            .apply(ConnectionLifecycleTransition::CommandInitialized)
            .unwrap();

        assert_eq!(tracker.snapshot().sequence, 2);
        assert_eq!(tracker.trace()[0].before.sequence, 0);
        assert_eq!(tracker.trace()[0].after.sequence, 1);
        assert_eq!(tracker.trace()[1].before, tracker.trace()[0].after);
        assert_eq!(tracker.trace()[1].after, *tracker.snapshot());
    }

    #[test]
    fn trace_retains_a_bounded_deterministic_tail() {
        let mut tracker = ConnectionLifecycleTracker::default();
        for _ in 0..(MAX_CONNECTION_LIFECYCLE_TRACE_EVENTS + 3) {
            tracker
                .apply(ConnectionLifecycleTransition::ProxyTransportAccepted)
                .unwrap();
        }

        assert_eq!(tracker.trace().len(), MAX_CONNECTION_LIFECYCLE_TRACE_EVENTS);
        assert_eq!(tracker.snapshot().sequence, 515);
        assert_eq!(tracker.trace().first().unwrap().sequence, 4);
        assert_eq!(tracker.trace().last().unwrap().sequence, 515);
        assert!(tracker
            .trace()
            .windows(2)
            .all(|events| events[0].after == events[1].before));
    }
}
