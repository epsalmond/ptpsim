//! Pure UniFFI seam for `ptpsim-connection-lifecycle/v1`.
//!
//! The caller owns every snapshot and supplies every observed transition. This
//! module does not read simulator state or perform transport I/O.

use camera_config as cc;

#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum LifecycleNetworkPersonaState {
    Ready,
    Exited,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum LifecycleCommandListenerState {
    Ready,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum LifecycleProxyTransportState {
    Accepted,
    CommandInitialized,
    Lost,
    LocallyClosed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum LifecyclePtpSessionState {
    Open,
    Closed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum LifecycleFeatureMode {
    LiveView,
    ImageTransfer,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum LifecycleFeatureState {
    Inactive,
    Active { mode: LifecycleFeatureMode },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum LifecycleNetworkPersonaEvidence {
    Unknown,
    Unconfirmed { state: LifecycleNetworkPersonaState },
    Confirmed { state: LifecycleNetworkPersonaState },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum LifecycleCommandListenerEvidence {
    Unknown,
    Unconfirmed {
        state: LifecycleCommandListenerState,
    },
    Confirmed {
        state: LifecycleCommandListenerState,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum LifecycleProxyTransportEvidence {
    Unknown,
    Unconfirmed { state: LifecycleProxyTransportState },
    Confirmed { state: LifecycleProxyTransportState },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum LifecyclePtpSessionEvidence {
    Unknown,
    Unconfirmed { state: LifecyclePtpSessionState },
    Confirmed { state: LifecyclePtpSessionState },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum LifecycleFeatureEvidence {
    Unknown,
    Unconfirmed { state: LifecycleFeatureState },
    Confirmed { state: LifecycleFeatureState },
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct ConnectionLifecycleSnapshot {
    pub schema: String,
    pub sequence: u64,
    pub network_persona: LifecycleNetworkPersonaEvidence,
    pub command_listener: LifecycleCommandListenerEvidence,
    pub proxy_transport: LifecycleProxyTransportEvidence,
    pub ptp_session: LifecyclePtpSessionEvidence,
    pub feature: LifecycleFeatureEvidence,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
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
    FeatureEntered { mode: LifecycleFeatureMode },
    FeatureExited,
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct ConnectionLifecycleTraceEvent {
    pub schema: String,
    pub sequence: u64,
    pub transition: ConnectionLifecycleTransition,
    pub before: ConnectionLifecycleSnapshot,
    pub after: ConnectionLifecycleSnapshot,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error, uniffi::Error)]
pub enum ConnectionLifecycleError {
    #[error("unsupported connection lifecycle schema {actual:?}; expected {expected:?}")]
    UnsupportedSchema { actual: String, expected: String },
    #[error("connection lifecycle sequence overflow")]
    SequenceOverflow,
}

#[uniffi::export]
pub fn connection_lifecycle_schema_version() -> String {
    cc::CONNECTION_LIFECYCLE_SCHEMA_VERSION.to_string()
}

#[uniffi::export]
pub fn connection_lifecycle_initial_snapshot() -> ConnectionLifecycleSnapshot {
    cc::ConnectionLifecycleSnapshot::default().into()
}

/// Apply one caller-observed transition and return before/after evidence.
///
/// This is a pure reducer. It does not retain the returned snapshot or infer
/// transitions from simulator mutable state.
#[uniffi::export]
pub fn connection_lifecycle_apply_transition(
    snapshot: ConnectionLifecycleSnapshot,
    transition: ConnectionLifecycleTransition,
) -> Result<ConnectionLifecycleTraceEvent, ConnectionLifecycleError> {
    let snapshot = cc::ConnectionLifecycleSnapshot::try_from(snapshot)?;
    cc::apply_connection_lifecycle_transition(&snapshot, transition.into())
        .map(Into::into)
        .map_err(|error| match error {
            cc::ConnectionLifecycleError::SequenceOverflow => {
                ConnectionLifecycleError::SequenceOverflow
            }
        })
}

impl From<cc::NetworkPersonaState> for LifecycleNetworkPersonaState {
    fn from(value: cc::NetworkPersonaState) -> Self {
        match value {
            cc::NetworkPersonaState::Ready => Self::Ready,
            cc::NetworkPersonaState::Exited => Self::Exited,
        }
    }
}

impl From<LifecycleNetworkPersonaState> for cc::NetworkPersonaState {
    fn from(value: LifecycleNetworkPersonaState) -> Self {
        match value {
            LifecycleNetworkPersonaState::Ready => Self::Ready,
            LifecycleNetworkPersonaState::Exited => Self::Exited,
        }
    }
}

impl From<cc::CommandListenerState> for LifecycleCommandListenerState {
    fn from(value: cc::CommandListenerState) -> Self {
        match value {
            cc::CommandListenerState::Ready => Self::Ready,
        }
    }
}

impl From<LifecycleCommandListenerState> for cc::CommandListenerState {
    fn from(value: LifecycleCommandListenerState) -> Self {
        match value {
            LifecycleCommandListenerState::Ready => Self::Ready,
        }
    }
}

impl From<cc::ProxyTransportState> for LifecycleProxyTransportState {
    fn from(value: cc::ProxyTransportState) -> Self {
        match value {
            cc::ProxyTransportState::Accepted => Self::Accepted,
            cc::ProxyTransportState::CommandInitialized => Self::CommandInitialized,
            cc::ProxyTransportState::Lost => Self::Lost,
            cc::ProxyTransportState::LocallyClosed => Self::LocallyClosed,
        }
    }
}

impl From<LifecycleProxyTransportState> for cc::ProxyTransportState {
    fn from(value: LifecycleProxyTransportState) -> Self {
        match value {
            LifecycleProxyTransportState::Accepted => Self::Accepted,
            LifecycleProxyTransportState::CommandInitialized => Self::CommandInitialized,
            LifecycleProxyTransportState::Lost => Self::Lost,
            LifecycleProxyTransportState::LocallyClosed => Self::LocallyClosed,
        }
    }
}

impl From<cc::PtpSessionState> for LifecyclePtpSessionState {
    fn from(value: cc::PtpSessionState) -> Self {
        match value {
            cc::PtpSessionState::Open => Self::Open,
            cc::PtpSessionState::Closed => Self::Closed,
        }
    }
}

impl From<LifecyclePtpSessionState> for cc::PtpSessionState {
    fn from(value: LifecyclePtpSessionState) -> Self {
        match value {
            LifecyclePtpSessionState::Open => Self::Open,
            LifecyclePtpSessionState::Closed => Self::Closed,
        }
    }
}

impl From<cc::FeatureMode> for LifecycleFeatureMode {
    fn from(value: cc::FeatureMode) -> Self {
        match value {
            cc::FeatureMode::LiveView => Self::LiveView,
            cc::FeatureMode::ImageTransfer => Self::ImageTransfer,
        }
    }
}

impl From<LifecycleFeatureMode> for cc::FeatureMode {
    fn from(value: LifecycleFeatureMode) -> Self {
        match value {
            LifecycleFeatureMode::LiveView => Self::LiveView,
            LifecycleFeatureMode::ImageTransfer => Self::ImageTransfer,
        }
    }
}

impl From<cc::FeatureState> for LifecycleFeatureState {
    fn from(value: cc::FeatureState) -> Self {
        match value {
            cc::FeatureState::Inactive => Self::Inactive,
            cc::FeatureState::Active { mode } => Self::Active { mode: mode.into() },
        }
    }
}

impl From<LifecycleFeatureState> for cc::FeatureState {
    fn from(value: LifecycleFeatureState) -> Self {
        match value {
            LifecycleFeatureState::Inactive => Self::Inactive,
            LifecycleFeatureState::Active { mode } => Self::Active { mode: mode.into() },
        }
    }
}

macro_rules! evidence_conversions {
    ($native:ty, $ffi:ident) => {
        impl From<cc::LifecycleEvidence<$native>> for $ffi {
            fn from(value: cc::LifecycleEvidence<$native>) -> Self {
                match value {
                    cc::LifecycleEvidence::Unknown => Self::Unknown,
                    cc::LifecycleEvidence::Unconfirmed(state) => Self::Unconfirmed {
                        state: state.into(),
                    },
                    cc::LifecycleEvidence::Confirmed(state) => Self::Confirmed {
                        state: state.into(),
                    },
                }
            }
        }

        impl From<$ffi> for cc::LifecycleEvidence<$native> {
            fn from(value: $ffi) -> Self {
                match value {
                    $ffi::Unknown => Self::Unknown,
                    $ffi::Unconfirmed { state } => Self::Unconfirmed(state.into()),
                    $ffi::Confirmed { state } => Self::Confirmed(state.into()),
                }
            }
        }
    };
}

evidence_conversions!(cc::NetworkPersonaState, LifecycleNetworkPersonaEvidence);
evidence_conversions!(cc::CommandListenerState, LifecycleCommandListenerEvidence);
evidence_conversions!(cc::ProxyTransportState, LifecycleProxyTransportEvidence);
evidence_conversions!(cc::PtpSessionState, LifecyclePtpSessionEvidence);
evidence_conversions!(cc::FeatureState, LifecycleFeatureEvidence);

impl From<cc::ConnectionLifecycleSnapshot> for ConnectionLifecycleSnapshot {
    fn from(value: cc::ConnectionLifecycleSnapshot) -> Self {
        Self {
            schema: cc::CONNECTION_LIFECYCLE_SCHEMA_VERSION.to_string(),
            sequence: value.sequence,
            network_persona: value.network_persona.into(),
            command_listener: value.command_listener.into(),
            proxy_transport: value.proxy_transport.into(),
            ptp_session: value.ptp_session.into(),
            feature: value.feature.into(),
        }
    }
}

impl TryFrom<ConnectionLifecycleSnapshot> for cc::ConnectionLifecycleSnapshot {
    type Error = ConnectionLifecycleError;

    fn try_from(value: ConnectionLifecycleSnapshot) -> Result<Self, Self::Error> {
        if value.schema != cc::CONNECTION_LIFECYCLE_SCHEMA_VERSION {
            return Err(ConnectionLifecycleError::UnsupportedSchema {
                actual: value.schema,
                expected: cc::CONNECTION_LIFECYCLE_SCHEMA_VERSION.to_string(),
            });
        }
        Ok(Self {
            schema: cc::ConnectionLifecycleSchema::V1,
            sequence: value.sequence,
            network_persona: value.network_persona.into(),
            command_listener: value.command_listener.into(),
            proxy_transport: value.proxy_transport.into(),
            ptp_session: value.ptp_session.into(),
            feature: value.feature.into(),
        })
    }
}

impl From<ConnectionLifecycleTransition> for cc::ConnectionLifecycleTransition {
    fn from(value: ConnectionLifecycleTransition) -> Self {
        match value {
            ConnectionLifecycleTransition::NetworkPersonaReady => Self::NetworkPersonaReady,
            ConnectionLifecycleTransition::NetworkPersonaExitRequested => {
                Self::NetworkPersonaExitRequested
            }
            ConnectionLifecycleTransition::NetworkPersonaExitConfirmed => {
                Self::NetworkPersonaExitConfirmed
            }
            ConnectionLifecycleTransition::CommandListenerReady => Self::CommandListenerReady,
            ConnectionLifecycleTransition::ProxyTransportAccepted => Self::ProxyTransportAccepted,
            ConnectionLifecycleTransition::CommandInitialized => Self::CommandInitialized,
            ConnectionLifecycleTransition::ProxyTransportLost => Self::ProxyTransportLost,
            ConnectionLifecycleTransition::ProxyTransportClosedLocally => {
                Self::ProxyTransportClosedLocally
            }
            ConnectionLifecycleTransition::PtpSessionOpened => Self::PtpSessionOpened,
            ConnectionLifecycleTransition::PtpSessionClosed => Self::PtpSessionClosed,
            ConnectionLifecycleTransition::FeatureEntered { mode } => {
                Self::FeatureEntered { mode: mode.into() }
            }
            ConnectionLifecycleTransition::FeatureExited => Self::FeatureExited,
        }
    }
}

impl From<cc::ConnectionLifecycleTransition> for ConnectionLifecycleTransition {
    fn from(value: cc::ConnectionLifecycleTransition) -> Self {
        match value {
            cc::ConnectionLifecycleTransition::NetworkPersonaReady => Self::NetworkPersonaReady,
            cc::ConnectionLifecycleTransition::NetworkPersonaExitRequested => {
                Self::NetworkPersonaExitRequested
            }
            cc::ConnectionLifecycleTransition::NetworkPersonaExitConfirmed => {
                Self::NetworkPersonaExitConfirmed
            }
            cc::ConnectionLifecycleTransition::CommandListenerReady => Self::CommandListenerReady,
            cc::ConnectionLifecycleTransition::ProxyTransportAccepted => {
                Self::ProxyTransportAccepted
            }
            cc::ConnectionLifecycleTransition::CommandInitialized => Self::CommandInitialized,
            cc::ConnectionLifecycleTransition::ProxyTransportLost => Self::ProxyTransportLost,
            cc::ConnectionLifecycleTransition::ProxyTransportClosedLocally => {
                Self::ProxyTransportClosedLocally
            }
            cc::ConnectionLifecycleTransition::PtpSessionOpened => Self::PtpSessionOpened,
            cc::ConnectionLifecycleTransition::PtpSessionClosed => Self::PtpSessionClosed,
            cc::ConnectionLifecycleTransition::FeatureEntered { mode } => {
                Self::FeatureEntered { mode: mode.into() }
            }
            cc::ConnectionLifecycleTransition::FeatureExited => Self::FeatureExited,
        }
    }
}

impl From<cc::ConnectionLifecycleTraceEvent> for ConnectionLifecycleTraceEvent {
    fn from(value: cc::ConnectionLifecycleTraceEvent) -> Self {
        Self {
            schema: cc::CONNECTION_LIFECYCLE_SCHEMA_VERSION.to_string(),
            sequence: value.sequence,
            transition: value.transition.into(),
            before: value.before.into(),
            after: value.after.into(),
        }
    }
}
