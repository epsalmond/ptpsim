use std::collections::BTreeMap;

use camera_config::index::ResolvedManufacturerIndex;
use camera_sim::{walk_establishment, BleEvent, BleResponder};

fn data(rel: &str) -> String {
    std::fs::read_to_string(
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../packages/camera-config-data")
            .join(rel),
    )
    .unwrap()
}

#[test]
fn wake_plan_connects_and_requires_the_peer_boot_disconnect() {
    let index = ResolvedManufacturerIndex::from_yaml(&data("fuji/index.yaml")).unwrap();
    let ble = index.models[0].ble.as_ref().unwrap();
    let plan = ble.establishment("ble-wake").unwrap();
    let mut responder = BleResponder::new(ble.gatt.values().cloned()).queue_peer_disconnect();

    walk_establishment(
        &mut responder,
        &plan.steps,
        &BTreeMap::new(),
        &BTreeMap::new(),
        &BTreeMap::new(),
    )
    .unwrap();

    assert_eq!(
        responder.log(),
        &[BleEvent::Connect, BleEvent::PeerDisconnect]
    );
}

#[test]
fn wake_plan_fails_when_the_peer_never_disconnects() {
    let index = ResolvedManufacturerIndex::from_yaml(&data("fuji/index.yaml")).unwrap();
    let ble = index.models[0].ble.as_ref().unwrap();
    let plan = ble.establishment("ble-wake").unwrap();
    let mut responder = BleResponder::new(ble.gatt.values().cloned());

    let error = match walk_establishment(
        &mut responder,
        &plan.steps,
        &BTreeMap::new(),
        &BTreeMap::new(),
        &BTreeMap::new(),
    ) {
        Ok(_) => panic!("wake unexpectedly completed without a peer disconnect"),
        Err(error) => error,
    };

    assert!(error.to_string().contains("peer disconnect not observed"));
}
