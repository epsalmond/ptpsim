//! Issue #47 acceptance: round-trip the REAL `ble-establish-wifi-ap` plan from
//! `packages/camera-config-data/fuji/index.yaml` against the in-memory
//! responder. The reference walker plays the app dispatcher: it writes the
//! function-launch request (launchMode bound at runtime), awaits the camera AP
//! reaching a launched `apState`, then reads SSID + passphrase.
//!
//! The consumer contract (client application #5) is that a completed walk leaves
//! `scope["ssid"]` and `scope["passphrase"]` bound — that is what the app
//! joins the camera AP with before opening PTP/IP.

use std::collections::BTreeMap;
use std::path::PathBuf;

use camera_config::index::{FamilyBleBlock, ModelView, ResolvedManufacturerIndex};
use camera_sim::{walk_establishment, BleResponder};

fn data(rel: &str) -> String {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../packages/camera-config-data")
        .join(rel);
    std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("read {}: {e}", p.display()))
}

fn gfx100ii() -> ModelView {
    ResolvedManufacturerIndex::from_yaml(&data("fuji/index.yaml"))
        .expect("fuji/index.yaml loads")
        .models
        .into_iter()
        .find(|m| m.id == "gfx100ii")
        .expect("gfx100ii present")
}

fn uuid(ble: &FamilyBleBlock, name: &str) -> String {
    ble.gatt
        .get(name)
        .unwrap_or_else(|| panic!("gatt name {name} in catalog"))
        .clone()
}

/// Walk `ble-establish-wifi-ap` with `launch_mode` bound; return the final
/// scope and the (single) bytes written to functionLaunchRequest.
fn run(launch_mode: &str) -> (BTreeMap<String, String>, Vec<u8>) {
    let view = gfx100ii();
    let ble = view
        .ble
        .as_ref()
        .expect("gfx100ii inherits the fuji ble block");
    let steps = &ble
        .establishment("ble-establish-wifi-ap")
        .expect("ble-establish-wifi-ap plan registered")
        .steps;

    let launch = uuid(ble, "functionLaunchRequest");
    let ap_state = uuid(ble, "apState");
    let ssid_uuid = uuid(ble, "cameraSSIDNameString");
    let pass_uuid = uuid(ble, "cameraWiFiPassphraseString");

    let mut responder = BleResponder::new([
        launch.clone(),
        ap_state.clone(),
        ssid_uuid.clone(),
        pass_uuid.clone(),
    ])
    // apState notify source: not-launched (0x0000) then launched
    // (0x0180 little-endian = 384, in the `until` set).
    .queue_notification(&ap_state, &[0x00, 0x00])
    .queue_notification(&ap_state, &[0x80, 0x01])
    .serve_read(&ssid_uuid, b"GFX100II-1234")
    .serve_read(&pass_uuid, b"hunter2pass");

    let runtime_params: BTreeMap<String, String> =
        [("launchMode".to_string(), launch_mode.to_string())]
            .into_iter()
            .collect();

    let outcome = walk_establishment(&mut responder, steps, &BTreeMap::new(), &runtime_params)
        .expect("the establish-wifi-ap plan walks to completion");

    let writes = responder.written(&launch);
    assert_eq!(writes.len(), 1, "launch request written exactly once");
    (outcome.scope, writes[0].to_vec())
}

#[test]
fn establish_wifi_ap_binds_credentials_and_writes_launch_mode_4() {
    let (scope, launch_write) = run("4");
    // launchMode 4 (RemoteShooting) → u16-le [04 00].
    assert_eq!(launch_write, vec![0x04, 0x00]);
    // apState observed until launched (0x0180 → 384).
    assert_eq!(scope.get("apState").map(String::as_str), Some("384"));
    // Raw apState bytes preserved for the app's fallback parsing.
    assert!(scope.contains_key("apStateRaw"));
    // Consumer contract: SSID + passphrase bound so the app can join the AP.
    assert_eq!(scope.get("ssid").map(String::as_str), Some("GFX100II-1234"));
    assert_eq!(
        scope.get("passphrase").map(String::as_str),
        Some("hunter2pass")
    );
}

#[test]
fn establish_wifi_ap_launch_mode_3_image_transfer() {
    let (_scope, launch_write) = run("3");
    // launchMode 3 (InCameraViewIng / image-transfer) → u16-le [03 00].
    assert_eq!(launch_write, vec![0x03, 0x00]);
}
