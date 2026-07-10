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
use camera_sim::{walk_establishment, BleEvent, BleResponder};

const AP_STATE_UUID: &str = "A68E3F66-0FCC-4395-8D4C-AA980B5877FA";

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

/// Walk `ble-establish-wifi-ap` with `launch_mode` bound, serving `ssid` and —
/// when the body exposes it — `passphrase`. When `passphrase` is `None` the
/// passphrase characteristic is absent from the catalog entirely, so a read
/// fails as `NotExposed` exactly as on an open-AP / legacy-fw body (#85).
/// Returns the final scope and the (single) bytes written to functionLaunchRequest.
fn walk_wifi_ap(
    launch_mode: &str,
    ssid: &[u8],
    passphrase: Option<&[u8]>,
) -> (BTreeMap<String, String>, Vec<u8>) {
    let (scope, launch_write, _events) = walk_wifi_ap_with_ap_state(
        launch_mode,
        ssid,
        passphrase,
        vec![0x02, 0x80],
        vec![vec![0x01, 0x80]],
    );
    (scope, launch_write)
}

fn walk_wifi_ap_with_ap_state(
    launch_mode: &str,
    ssid: &[u8],
    passphrase: Option<&[u8]>,
    ap_state_seed: Vec<u8>,
    ap_state_notifications: Vec<Vec<u8>>,
) -> (BTreeMap<String, String>, Vec<u8>, Vec<BleEvent>) {
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

    // The passphrase characteristic is catalogued only when the body exposes
    // it; an open AP omits it, and the tolerant read then skips.
    let mut catalog = vec![launch.clone(), ap_state.clone(), ssid_uuid.clone()];
    if passphrase.is_some() {
        catalog.push(pass_uuid.clone());
    }

    let mut responder = BleResponder::new(catalog)
        // apState seeded-notify source: one Launching (0x8002) read, then the
        // terminal Launched (0x8001) notification. Wire bytes are little-endian,
        // so [01 80] = 0x8001 = 32769 (#84, #202).
        .serve_read(&ap_state, &ap_state_seed)
        .serve_read(&ssid_uuid, ssid);
    for payload in ap_state_notifications {
        responder = responder.queue_notification(&ap_state, &payload);
    }
    if let Some(pass) = passphrase {
        responder = responder.serve_read(&pass_uuid, pass);
    }

    let runtime_params: BTreeMap<String, String> =
        [("launchMode".to_string(), launch_mode.to_string())]
            .into_iter()
            .collect();

    let outcome = walk_establishment(
        &mut responder,
        steps,
        &BTreeMap::new(),
        &BTreeMap::new(),
        &runtime_params,
    )
    .expect("the establish-wifi-ap plan walks to completion");

    let writes = responder.written(&launch);
    assert_eq!(writes.len(), 1, "launch request written exactly once");
    (outcome.scope, writes[0].to_vec(), responder.log().to_vec())
}

/// The happy path: a body that exposes both credentials, no padding.
fn run(launch_mode: &str) -> (BTreeMap<String, String>, Vec<u8>) {
    walk_wifi_ap(launch_mode, b"GFX100II-1234", Some(b"hunter2pass"))
}

#[test]
fn establish_wifi_ap_binds_credentials_and_writes_launch_mode_4() {
    let (scope, launch_write) = run("4");
    // launchMode 4 (RemoteShooting) → u16-le [04 00].
    assert_eq!(launch_write, vec![0x04, 0x00]);
    // The transitional Launching seed (0x8002) did not satisfy `until`; the
    // terminal Launched notification (0x8001 → 32769) did.
    assert_eq!(scope.get("apState").map(String::as_str), Some("32769"));
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
fn establish_wifi_ap_seeds_already_launched_apstate_without_notify() {
    // #179: if the AP was already up before this walk started, the camera will
    // not re-notify `Launched`. The one AP_STATE seed read must recover the
    // current state and let the handoff continue.
    let (scope, _, events) = walk_wifi_ap_with_ap_state(
        "3",
        b"FUJIFILM-GFX100II-0C3E",
        Some(b"secret-pass"),
        vec![0x01, 0x80],
        vec![],
    );
    assert_eq!(scope.get("apState").map(String::as_str), Some("32769"));
    assert_eq!(
        scope.get("ssid").map(String::as_str),
        Some("FUJIFILM-GFX100II-0C3E"),
    );

    let ap_state_reads = events
        .iter()
        .filter(|e| matches!(e, BleEvent::Read { uuid } if uuid == AP_STATE_UUID))
        .count();
    assert_eq!(ap_state_reads, 1, "first read was already satisfying");
    let ap_state_subscriptions = events
        .iter()
        .filter(|e| matches!(e, BleEvent::Subscribe { uuid, .. } if uuid == AP_STATE_UUID))
        .count();
    assert_eq!(ap_state_subscriptions, 1, "subscribe before the seed read");
}

#[test]
fn establish_wifi_ap_reads_once_then_accepts_the_terminal_notify() {
    let (scope, _, events) = walk_wifi_ap_with_ap_state(
        "4",
        b"FUJIFILM-GFX100II-0C3E",
        Some(b"secret-pass"),
        vec![0x02, 0x80],
        vec![vec![0x01, 0x80]],
    );
    assert_eq!(scope.get("apState").map(String::as_str), Some("32769"));

    let observations: Vec<&BleEvent> = events
        .iter()
        .filter(|event| {
            matches!(
                event,
                BleEvent::Subscribe { uuid, .. } | BleEvent::Read { uuid }
                    if uuid == AP_STATE_UUID
            )
        })
        .collect();
    assert_eq!(observations.len(), 2, "one subscription and one seed read");
    assert!(matches!(observations[0], BleEvent::Subscribe { .. }));
    assert!(matches!(observations[1], BleEvent::Read { .. }));
}

#[test]
fn establish_wifi_ap_launch_mode_3_image_transfer() {
    let (_scope, launch_write) = run("3");
    // launchMode 3 (InCameraViewIng / image-transfer) → u16-le [03 00].
    assert_eq!(launch_write, vec![0x03, 0x00]);
}

#[test]
fn establish_wifi_ap_trims_nul_padding_from_the_ssid() {
    // #87: the SSID is a fixed-width field padded with trailing NULs. The
    // `utf8-cstring` read stops at the first \0, so the join targets the bare
    // name. A plain `utf8` read would surface the padded form and iOS would
    // look for "FUJIFILM-GFX100II-0C3E\0\0…" and never associate (device 2026-06-22).
    let padded = b"FUJIFILM-GFX100II-0C3E\0\0\0\0\0\0\0\0\0\0";
    let (scope, _) = walk_wifi_ap("4", padded, Some(b"secret-pass"));
    assert_eq!(
        scope.get("ssid").map(String::as_str),
        Some("FUJIFILM-GFX100II-0C3E"),
        "trailing NUL padding must not reach scope",
    );
    // The passphrase (no padding here) still binds normally.
    assert_eq!(
        scope.get("passphrase").map(String::as_str),
        Some("secret-pass")
    );
}

#[test]
fn establish_wifi_ap_tolerates_a_missing_passphrase_on_an_open_ap() {
    // #85: an OPEN AP (or legacy fw 2.30) doesn't expose the passphrase
    // characteristic. The tolerant read skips after a clean launch + SSID read;
    // the handoff completes and the consumer joins an open network rather than
    // aborting (operators APP_AP_HANDOVER_HANDSHAKE_2026-05-13).
    let (scope, _) = walk_wifi_ap("4", b"FUJIFILM-GFX100II-0C3E", None);
    assert_eq!(
        scope.get("ssid").map(String::as_str),
        Some("FUJIFILM-GFX100II-0C3E"),
        "SSID stays required and bound",
    );
    assert!(
        !scope.contains_key("passphrase"),
        "an absent passphrase characteristic leaves scope unbound, not aborted",
    );
}
