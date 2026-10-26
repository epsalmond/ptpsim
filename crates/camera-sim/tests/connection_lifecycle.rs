use camera_config::CameraManifest;
use camera_media_store::{MediaStore, ObjectQuery};
use camera_sim::{
    Engine, FeatureMode, FeatureState, LifecycleEvidence, NetworkPersonaState, ProxyTransportState,
    PtpSessionState, Reply,
};
use ptp_core::codes::{op, resp};
use ptp_core::OperationRequest;

fn engine() -> (Engine, tempfile::TempDir) {
    let manifest = CameraManifest::from_yaml(
        r#"
schema: camera-config/v1
camera: { manufacturer: TESTCO, model: TM1, firmware: "1" }
modes:
  shooting/stills:
    detect: { prop: "0xdf01", eq: 22 }
    phase: liveView
  image-transfer:
    detect: { prop: "0xdf01", eq: 20 }
    phase: imageImport
properties:
  "0xdf01": { name: functionMode, type: u16, access: readWrite }
"#,
    )
    .unwrap();
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("completed.mov"), b"completed").unwrap();
    let mut store = MediaStore::open(directory.path()).unwrap();
    store.scan().unwrap();
    (Engine::new(manifest, store), directory)
}

fn request(code: u16, transaction_id: u32, params: Vec<u32>) -> OperationRequest {
    OperationRequest {
        data_phase_info: 1,
        code,
        transaction_id,
        params,
    }
}

fn assert_ok(reply: Reply) {
    let response = match reply {
        Reply::Response(response)
        | Reply::Data { response, .. }
        | Reply::DataStream { response, .. } => response,
        Reply::NoResponse | Reply::Close => panic!("expected response"),
    };
    assert_eq!(response.code, resp::OK);
}

fn establish(engine: &mut Engine, mode: u16) {
    engine.network_persona_ready();
    engine.command_listener_ready();
    engine.proxy_transport_accepted();
    engine.command_initialized();
    assert_ok(engine.on_operation(&request(op::OPEN_SESSION, 1, vec![1]), None));
    assert_ok(engine.on_operation(
        &request(op::SET_DEVICE_PROP_VALUE, 2, vec![0xdf01]),
        Some(&mode.to_le_bytes()),
    ));
}

#[test]
fn close_session_preserves_transport_listener_and_persona() {
    let (mut engine, _directory) = engine();
    establish(&mut engine, 22);
    let before = engine.connection_lifecycle_snapshot();

    assert_ok(engine.on_operation(&request(op::CLOSE_SESSION, 3, vec![]), None));
    let after = engine.connection_lifecycle_snapshot();

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
    assert_eq!(
        after.network_persona,
        LifecycleEvidence::Confirmed(NetworkPersonaState::Ready)
    );
}

#[test]
fn live_view_loss_keeps_camera_side_cleanup_unconfirmed() {
    let (mut engine, _directory) = engine();
    establish(&mut engine, 22);

    engine.transport_lost();
    let snapshot = engine.connection_lifecycle_snapshot();

    assert_eq!(
        snapshot.network_persona,
        LifecycleEvidence::Confirmed(NetworkPersonaState::Ready)
    );
    assert!(matches!(
        snapshot.command_listener,
        LifecycleEvidence::Confirmed(_)
    ));
    assert_eq!(
        snapshot.proxy_transport,
        LifecycleEvidence::Confirmed(ProxyTransportState::Lost)
    );
    assert_eq!(
        snapshot.ptp_session,
        LifecycleEvidence::Unconfirmed(PtpSessionState::Open)
    );
    assert_eq!(
        snapshot.feature,
        LifecycleEvidence::Unconfirmed(FeatureState::Active {
            mode: FeatureMode::LiveView
        })
    );
}

#[test]
fn local_transport_close_records_local_fact_without_claiming_remote_exit() {
    let (mut engine, _directory) = engine();
    establish(&mut engine, 20);

    engine.proxy_transport_closed_locally();
    let snapshot = engine.connection_lifecycle_snapshot();

    assert_eq!(
        snapshot.network_persona,
        LifecycleEvidence::Confirmed(NetworkPersonaState::Ready)
    );
    assert!(matches!(
        snapshot.command_listener,
        LifecycleEvidence::Confirmed(_)
    ));
    assert_eq!(
        snapshot.proxy_transport,
        LifecycleEvidence::Confirmed(ProxyTransportState::LocallyClosed)
    );
    assert_eq!(
        snapshot.ptp_session,
        LifecycleEvidence::Unconfirmed(PtpSessionState::Open)
    );
    assert_eq!(
        snapshot.feature,
        LifecycleEvidence::Unconfirmed(FeatureState::Active {
            mode: FeatureMode::ImageTransfer
        })
    );
}

#[test]
fn completed_transfer_idle_loss_then_user_exit_preserves_media() {
    let (mut engine, _directory) = engine();
    establish(&mut engine, 20);
    let handles_before = engine.store().handles(ObjectQuery::default());
    assert_eq!(handles_before.len(), 1);

    engine.transport_lost();
    engine.network_persona_exit_requested();
    let requested = engine.connection_lifecycle_snapshot();
    assert_eq!(
        requested.network_persona,
        LifecycleEvidence::Unconfirmed(NetworkPersonaState::Exited)
    );
    assert_eq!(
        requested.feature,
        LifecycleEvidence::Unconfirmed(FeatureState::Active {
            mode: FeatureMode::ImageTransfer
        })
    );

    engine.network_persona_exit_confirmed();
    let exited = engine.connection_lifecycle_snapshot();
    assert_eq!(
        exited.network_persona,
        LifecycleEvidence::Confirmed(NetworkPersonaState::Exited)
    );
    assert_eq!(
        exited.proxy_transport,
        LifecycleEvidence::Confirmed(ProxyTransportState::Lost)
    );
    assert_eq!(
        exited.ptp_session,
        LifecycleEvidence::Unconfirmed(PtpSessionState::Open)
    );
    assert_eq!(
        engine.store().handles(ObjectQuery::default()),
        handles_before
    );

    let trace = engine.connection_lifecycle_trace();
    assert_eq!(trace.len() as u64, exited.sequence);
    assert!(trace
        .windows(2)
        .all(|events| events[0].after == events[1].before));
    let exit_requested = trace
        .iter()
        .position(|event| {
            matches!(
                event.transition,
                camera_sim::ConnectionLifecycleTransition::NetworkPersonaExitRequested
            )
        })
        .unwrap();
    assert!(!trace[exit_requested + 1..].iter().any(|event| matches!(
        event.transition,
        camera_sim::ConnectionLifecycleTransition::ProxyTransportAccepted
            | camera_sim::ConnectionLifecycleTransition::CommandInitialized
            | camera_sim::ConnectionLifecycleTransition::PtpSessionOpened
            | camera_sim::ConnectionLifecycleTransition::FeatureEntered { .. }
    )));
}
