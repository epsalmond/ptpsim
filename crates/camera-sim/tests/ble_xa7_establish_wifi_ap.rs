//! Issue #412 acceptance: walk the REAL `legacy-app-establish-wifi-ap`
//! plan from `packages/camera-config-data/fuji/index.yaml` against the
//! in-memory responder. The X-A7 (fw 02.30-era) starts its AP on the
//! function-launch write but confirms state only by read: no `apState`
//! indication arrived within 20s on hardware, twice (2026-07-24). The plan
//! therefore polls the readable characteristic with a `read`-source
//! `bleAwaitUntil` instead of awaiting an indication.
//!
//! A pre-launch read of 0 is the normal not-yet state, not a refusal, so the
//! await declares no `failWhen`: a body that never launches fails as a
//! deadline, not a rejection.

use std::collections::BTreeMap;
use std::path::PathBuf;

use camera_config::index::{
    FamilyBleBlock, ModelView, ResolvedManufacturerIndex, RetryFailureKind,
};
use camera_sim::{walk_establishment, BleEvent, BleResponder, WalkError};

fn data(rel: &str) -> String {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../packages/camera-config-data")
        .join(rel);
    std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("read {}: {e}", p.display()))
}

fn xa7() -> ModelView {
    ResolvedManufacturerIndex::from_yaml(&data("fuji/index.yaml"))
        .expect("fuji/index.yaml loads")
        .models
        .into_iter()
        .find(|m| m.id == "xa7")
        .expect("xa7 present")
}

fn uuid(ble: &FamilyBleBlock, name: &str) -> String {
    ble.gatt
        .get(name)
        .unwrap_or_else(|| panic!("gatt name {name} in catalog"))
        .clone()
}

fn responder(ble: &FamilyBleBlock, ap_state_reads: Vec<Vec<u8>>) -> BleResponder {
    let launch = uuid(ble, "functionLaunchRequest");
    let ap_state = uuid(ble, "apState");
    let ssid = uuid(ble, "cameraSSIDNameString");
    BleResponder::new(vec![launch, ap_state.clone(), ssid.clone()])
        .serve_read_sequence(&ap_state, ap_state_reads)
        .serve_read(&ssid, b"FUJIFILM-X-A7-1361\0")
}

fn walk(
    steps: &[camera_config::index::Step],
    responder: &mut BleResponder,
) -> Result<camera_sim::WalkOutcome, WalkError> {
    let runtime_params: BTreeMap<String, String> = [("launchMode".to_string(), "1".to_string())]
        .into_iter()
        .collect();
    walk_establishment(
        responder,
        steps,
        &BTreeMap::new(),
        &BTreeMap::new(),
        &runtime_params,
    )
}

#[test]
fn ap_start_confirms_by_read_poll_and_binds_ssid() {
    let view = xa7();
    let ble = view.ble.as_ref().expect("xa7 inherits the fuji ble block");
    let steps = &ble
        .establishment("legacy-app-establish-wifi-ap")
        .expect("legacy-app-establish-wifi-ap plan registered")
        .steps;

    // The hardware observation: the first poll after the launch write still
    // reads 0 (AP not yet up), a later poll reads 1 (launched). Wire bytes
    // are little-endian.
    let mut responder = responder(ble, vec![vec![0x00, 0x00], vec![0x01, 0x00]]);
    let outcome = walk(steps, &mut responder).expect("read-poll confirm walks to completion");

    assert_eq!(outcome.scope.get("apState").map(String::as_str), Some("1"));
    assert_eq!(
        outcome.scope.get("apStateRaw").map(String::as_str),
        Some("0100"),
        "the raw payload stays bound for app-side fallback parsing"
    );
    assert_eq!(
        outcome.scope.get("ssid").map(String::as_str),
        Some("FUJIFILM-X-A7-1361")
    );

    let launch = uuid(ble, "functionLaunchRequest");
    assert_eq!(
        responder.written(&launch),
        vec![&[0x01, 0x00][..]],
        "launch request written exactly once (launchMode 1, u16-le)"
    );

    // The await consumed reads, never the notification stream.
    let ap_state = uuid(ble, "apState");
    let events = responder.log();
    let ap_state_reads = events
        .iter()
        .filter(|e| matches!(e, BleEvent::Read { uuid } if *uuid == ap_state))
        .count();
    assert_eq!(ap_state_reads, 2, "two polls: not-yet, then launched");
    assert!(
        events
            .iter()
            .any(|e| matches!(e, BleEvent::Subscribe { uuid, .. } if *uuid == ap_state)),
        "the reference-app subscribe still runs"
    );
}

#[test]
fn ap_that_never_launches_fails_as_deadline_not_rejection() {
    let view = xa7();
    let ble = view.ble.as_ref().expect("xa7 inherits the fuji ble block");
    let steps = &ble
        .establishment("legacy-app-establish-wifi-ap")
        .expect("legacy-app-establish-wifi-ap plan registered")
        .steps;

    // Sticky 0: the AP never comes up. With no failWhen this is a deadline
    // (source-exhaustion analogue), not a condition rejection.
    let mut responder = responder(ble, vec![vec![0x00, 0x00]]);
    let error = match walk(steps, &mut responder) {
        Ok(_) => panic!("a never-launching AP must fail the walk"),
        Err(e) => e,
    };
    assert_eq!(error.kind, RetryFailureKind::DeadlineExceeded);
    assert!(
        error.to_string().contains("apState"),
        "the failure names the awaited field: {error}"
    );
}
