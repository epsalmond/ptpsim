use std::collections::BTreeMap;
use std::path::PathBuf;

use camera_config::index::{
    FamilyBleBlock, ModelView, ResolvedManufacturerIndex, RetryFailureKind,
};
use camera_sim::{walk_establishment, BleEvent, BleResponder, WalkError};

fn data(rel: &str) -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../packages/camera-config-data")
        .join(rel);
    std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("read {}: {error}", path.display()))
}

fn xa7() -> ModelView {
    ResolvedManufacturerIndex::from_yaml(&data("fuji/index.yaml"))
        .expect("Fuji index loads")
        .models
        .into_iter()
        .find(|model| model.id == "xa7")
        .expect("X-A7 model")
}

fn uuid(ble: &FamilyBleBlock, name: &str) -> String {
    ble.gatt
        .get(name)
        .unwrap_or_else(|| panic!("missing GATT key {name}"))
        .clone()
}

fn responder(ble: &FamilyBleBlock, ap_states: Vec<Vec<u8>>) -> BleResponder {
    let launch = uuid(ble, "functionLaunchRequest");
    let ap_state = uuid(ble, "apState");
    let ssid = uuid(ble, "cameraSSIDNameString");
    BleResponder::new(vec![launch, ap_state.clone(), ssid.clone()])
        .serve_read_sequence(&ap_state, ap_states)
        .serve_read(&ssid, b"FUJIFILM-X-A7-1361\0")
}

fn walk(
    steps: &[camera_config::index::Step],
    responder: &mut BleResponder,
) -> Result<camera_sim::WalkOutcome, WalkError> {
    let params = BTreeMap::from([("launchMode".to_string(), "1".to_string())]);
    walk_establishment(
        responder,
        steps,
        &BTreeMap::new(),
        &BTreeMap::new(),
        &params,
    )
}

#[test]
fn ap_start_polls_reads_until_state_one_and_binds_ssid() {
    let view = xa7();
    let ble = view.ble.as_ref().expect("X-A7 BLE block");
    let steps = &ble
        .establishment("legacy-app-establish-wifi-ap")
        .expect("legacy app AP plan")
        .steps;
    let mut responder = responder(ble, vec![vec![0, 0], vec![1, 0]]);
    let outcome = walk(steps, &mut responder).expect("AP read polling succeeds");
    assert_eq!(outcome.scope.get("apState").map(String::as_str), Some("1"));
    assert_eq!(
        outcome.scope.get("ssid").map(String::as_str),
        Some("FUJIFILM-X-A7-1361")
    );
    let ap_state = uuid(ble, "apState");
    assert_eq!(
        responder
            .log()
            .iter()
            .filter(|event| matches!(event, BleEvent::Read { uuid } if uuid == &ap_state))
            .count(),
        2
    );
}

#[test]
fn ap_state_zero_remains_pending_instead_of_rejecting() {
    let view = xa7();
    let ble = view.ble.as_ref().unwrap();
    let steps = &ble
        .establishment("legacy-app-establish-wifi-ap")
        .unwrap()
        .steps;
    let mut responder = responder(ble, vec![vec![0, 0]]);
    let Err(error) = walk(steps, &mut responder) else {
        panic!("sticky zero must reach the deadline");
    };
    assert_eq!(error.kind, RetryFailureKind::DeadlineExceeded);
}

#[test]
fn ap_state_three_is_terminal_failure() {
    let view = xa7();
    let ble = view.ble.as_ref().unwrap();
    let steps = &ble
        .establishment("legacy-app-establish-wifi-ap")
        .unwrap()
        .steps;
    let mut responder = responder(ble, vec![vec![3, 0]]);
    let Err(error) = walk(steps, &mut responder) else {
        panic!("state three must reject the launch");
    };
    assert_eq!(error.kind, RetryFailureKind::ConditionRejected);
}
