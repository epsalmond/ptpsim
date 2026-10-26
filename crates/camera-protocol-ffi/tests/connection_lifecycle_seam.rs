use camera_protocol_ffi::{
    connection_lifecycle_apply_transition, connection_lifecycle_initial_snapshot,
    connection_lifecycle_schema_version, ConnectionLifecycleError, ConnectionLifecycleTransition,
    LifecycleCommandListenerEvidence, LifecycleFeatureEvidence, LifecycleFeatureMode,
    LifecycleFeatureState, LifecycleNetworkPersonaEvidence, LifecycleNetworkPersonaState,
    LifecycleProxyTransportEvidence, LifecycleProxyTransportState, LifecyclePtpSessionEvidence,
    LifecyclePtpSessionState,
};

fn apply(
    snapshot: camera_protocol_ffi::ConnectionLifecycleSnapshot,
    transition: ConnectionLifecycleTransition,
) -> camera_protocol_ffi::ConnectionLifecycleSnapshot {
    connection_lifecycle_apply_transition(snapshot, transition)
        .unwrap()
        .after
}

#[test]
fn ffi_initial_snapshot_is_versioned_and_unknown() {
    let snapshot = connection_lifecycle_initial_snapshot();
    assert_eq!(snapshot.schema, connection_lifecycle_schema_version());
    assert_eq!(snapshot.sequence, 0);
    assert_eq!(
        snapshot.network_persona,
        LifecycleNetworkPersonaEvidence::Unknown
    );
    assert_eq!(
        snapshot.command_listener,
        LifecycleCommandListenerEvidence::Unknown
    );
    assert_eq!(
        snapshot.proxy_transport,
        LifecycleProxyTransportEvidence::Unknown
    );
    assert_eq!(snapshot.ptp_session, LifecyclePtpSessionEvidence::Unknown);
    assert_eq!(snapshot.feature, LifecycleFeatureEvidence::Unknown);
}

#[test]
fn ffi_close_session_does_not_collapse_lower_layers() {
    let mut snapshot = connection_lifecycle_initial_snapshot();
    for transition in [
        ConnectionLifecycleTransition::NetworkPersonaReady,
        ConnectionLifecycleTransition::CommandListenerReady,
        ConnectionLifecycleTransition::ProxyTransportAccepted,
        ConnectionLifecycleTransition::CommandInitialized,
        ConnectionLifecycleTransition::PtpSessionOpened,
        ConnectionLifecycleTransition::FeatureEntered {
            mode: LifecycleFeatureMode::LiveView,
        },
    ] {
        snapshot = apply(snapshot, transition);
    }
    let network = snapshot.network_persona;
    let listener = snapshot.command_listener;
    let transport = snapshot.proxy_transport;

    snapshot = apply(snapshot, ConnectionLifecycleTransition::PtpSessionClosed);
    assert_eq!(snapshot.network_persona, network);
    assert_eq!(snapshot.command_listener, listener);
    assert_eq!(snapshot.proxy_transport, transport);
    assert_eq!(
        snapshot.ptp_session,
        LifecyclePtpSessionEvidence::Confirmed {
            state: LifecyclePtpSessionState::Closed
        }
    );
    assert_eq!(
        snapshot.feature,
        LifecycleFeatureEvidence::Confirmed {
            state: LifecycleFeatureState::Inactive
        }
    );
}

#[test]
fn ffi_issue_921_loss_and_user_exit_remain_evidence_aware() {
    let mut snapshot = connection_lifecycle_initial_snapshot();
    for transition in [
        ConnectionLifecycleTransition::NetworkPersonaReady,
        ConnectionLifecycleTransition::CommandInitialized,
        ConnectionLifecycleTransition::PtpSessionOpened,
        ConnectionLifecycleTransition::FeatureEntered {
            mode: LifecycleFeatureMode::ImageTransfer,
        },
        ConnectionLifecycleTransition::ProxyTransportLost,
        ConnectionLifecycleTransition::NetworkPersonaExitRequested,
    ] {
        snapshot = apply(snapshot, transition);
    }

    assert_eq!(
        snapshot.network_persona,
        LifecycleNetworkPersonaEvidence::Unconfirmed {
            state: LifecycleNetworkPersonaState::Exited
        }
    );
    assert_eq!(
        snapshot.command_listener,
        LifecycleCommandListenerEvidence::Confirmed {
            state: camera_protocol_ffi::LifecycleCommandListenerState::Ready
        }
    );
    assert_eq!(
        snapshot.proxy_transport,
        LifecycleProxyTransportEvidence::Confirmed {
            state: LifecycleProxyTransportState::Lost
        }
    );
    assert_eq!(
        snapshot.ptp_session,
        LifecyclePtpSessionEvidence::Unconfirmed {
            state: LifecyclePtpSessionState::Open
        }
    );
    assert_eq!(
        snapshot.feature,
        LifecycleFeatureEvidence::Unconfirmed {
            state: LifecycleFeatureState::Active {
                mode: LifecycleFeatureMode::ImageTransfer
            }
        }
    );

    snapshot = apply(
        snapshot,
        ConnectionLifecycleTransition::NetworkPersonaExitConfirmed,
    );
    assert_eq!(
        snapshot.network_persona,
        LifecycleNetworkPersonaEvidence::Confirmed {
            state: LifecycleNetworkPersonaState::Exited
        }
    );
    assert_eq!(
        snapshot.ptp_session,
        LifecyclePtpSessionEvidence::Unconfirmed {
            state: LifecyclePtpSessionState::Open
        }
    );
}

#[test]
fn ffi_transition_rejects_a_future_snapshot_version() {
    let mut snapshot = connection_lifecycle_initial_snapshot();
    snapshot.schema = "ptpsim-connection-lifecycle/v2".into();
    let error = connection_lifecycle_apply_transition(
        snapshot,
        ConnectionLifecycleTransition::ProxyTransportAccepted,
    )
    .unwrap_err();
    assert!(matches!(
        error,
        ConnectionLifecycleError::UnsupportedSchema { .. }
    ));
}
