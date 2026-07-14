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

use camera_config::index::{
    FamilyBleBlock, ModelView, ResolvedManufacturerIndex, RetryFailureKind,
};
use camera_sim::{walk_establishment, BleEvent, BleResponder, WalkError};

const AP_STATE_UUID: &str = "A68E3F66-0FCC-4395-8D4C-AA980B5877FA";
const STATE_ERROR_DETAILS_UUID: &str = "1587B102-0B6D-4B63-9226-66FCC6D17387";

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
    let (scope, launch_writes, _events) = walk_wifi_ap_with_ap_state(
        launch_mode,
        ssid,
        passphrase,
        vec![0x02, 0x80],
        vec![],
        vec![vec![0x01, 0x80]],
    );
    assert_eq!(
        launch_writes.len(),
        1,
        "launch request written exactly once"
    );
    (scope, launch_writes[0].clone())
}

fn walk_wifi_ap_with_ap_state(
    launch_mode: &str,
    ssid: &[u8],
    passphrase: Option<&[u8]>,
    ap_state_baseline: Vec<u8>,
    stale_ap_state_notifications: Vec<Vec<u8>>,
    ap_state_notifications: Vec<Vec<u8>>,
) -> (BTreeMap<String, String>, Vec<Vec<u8>>, Vec<BleEvent>) {
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
    let image_setting = uuid(ble, "imageTransferSetting");
    let ssid_uuid = uuid(ble, "cameraSSIDNameString");
    let pass_uuid = uuid(ble, "cameraWiFiPassphraseString");

    // The passphrase characteristic is catalogued only when the body exposes
    // it; an open AP omits it, and the tolerant read then skips.
    let mut catalog = vec![
        launch.clone(),
        ap_state.clone(),
        image_setting.clone(),
        ssid_uuid.clone(),
    ];
    if passphrase.is_some() {
        catalog.push(pass_uuid.clone());
    }

    let mut responder = BleResponder::new(catalog)
        // apState is read before the command; the later await consumes only
        // notifications. Wire bytes are little-endian, so [01 80] = 0x8001.
        .serve_read(&ap_state, &ap_state_baseline)
        .serve_read(&ssid_uuid, ssid);
    for payload in stale_ap_state_notifications {
        responder = responder.queue_notification(&ap_state, &payload);
    }
    for payload in ap_state_notifications {
        responder =
            responder.queue_notification_after_fenced_write(&ap_state, &launch, 1, &payload);
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

    let writes = responder
        .written(&launch)
        .into_iter()
        .map(ToOwned::to_owned)
        .collect();
    assert_eq!(
        responder.written(&image_setting),
        vec![&[0x01][..]],
        "image-transfer setting written exactly once",
    );
    (outcome.scope, writes, responder.log().to_vec())
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
    // The preflight Launching value (0x8002) selects the launch branch; the
    // terminal Launched notification (0x8001 → 32769) completes it.
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
fn establish_wifi_ap_reads_once_then_accepts_the_terminal_notify() {
    let (scope, _, events) = walk_wifi_ap_with_ap_state(
        "4",
        b"FUJIFILM-GFX100II-0C3E",
        Some(b"secret-pass"),
        vec![0x02, 0x80],
        vec![],
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
    assert_eq!(
        observations.len(),
        2,
        "one preflight read and one subscription"
    );
    assert!(matches!(observations[0], BleEvent::Read { .. }));
    assert!(matches!(observations[1], BleEvent::Subscribe { .. }));
}

#[test]
fn establish_wifi_ap_treats_pre_command_not_launched_as_a_baseline() {
    let (scope, _, events) = walk_wifi_ap_with_ap_state(
        "4",
        b"FUJIFILM-GFX100II-0C3E",
        Some(b"secret-pass"),
        vec![0x00, 0x80],
        vec![],
        vec![vec![0x02, 0x80], vec![0x01, 0x80]],
    );
    assert_eq!(scope.get("apState").map(String::as_str), Some("32769"));
    assert_eq!(
        count_writes(&events, "600655E6-3637-42F1-8FB2-44EFC5C63B13"),
        1,
        "the pre-command baseline must not select the retry path",
    );
    assert_eq!(
        events
            .iter()
            .filter(
                |event| matches!(event, BleEvent::Read { uuid } if uuid == STATE_ERROR_DETAILS_UUID)
            )
            .count(),
        0,
        "a pre-command baseline is not a refusal and needs no diagnostic read",
    );
    let preflight_read = events
        .iter()
        .position(|event| matches!(event, BleEvent::Read { uuid } if uuid == AP_STATE_UUID))
        .expect("pre-command AP-state read");
    let subscription = events
        .iter()
        .position(
            |event| matches!(event, BleEvent::Subscribe { uuid, .. } if uuid == AP_STATE_UUID),
        )
        .expect("post-baseline AP-state subscription");
    let launch_write = events
        .iter()
        .position(|event| {
            matches!(
                event,
                BleEvent::Write { uuid, .. }
                    if uuid == "600655E6-3637-42F1-8FB2-44EFC5C63B13"
            )
        })
        .expect("function-launch write");
    assert!(
        preflight_read < subscription && subscription < launch_write,
        "baseline provenance is established before subscribe-then-write: {events:?}"
    );
}

#[test]
fn launch_fence_discards_stale_notification_before_first_attempt() {
    let (scope, _, events) = walk_wifi_ap_with_ap_state(
        "4",
        b"FUJIFILM-GFX100II-0C3E",
        Some(b"secret-pass"),
        vec![0x00, 0x80],
        vec![vec![0x00, 0x80]],
        vec![vec![0x01, 0x80]],
    );
    assert_eq!(scope.get("apState").map(String::as_str), Some("32769"));
    let launch_uuid = "600655E6-3637-42F1-8FB2-44EFC5C63B13";
    let fence = events
        .iter()
        .position(
            |event| matches!(event, BleEvent::NotificationFence { uuid } if uuid == AP_STATE_UUID),
        )
        .expect("AP-state notification fence");
    assert!(matches!(
        events.get(fence + 1),
        Some(BleEvent::Write { uuid, .. }) if uuid == launch_uuid
    ));
}

#[test]
fn fence_discards_buffered_notifications_from_a_different_prior_write() {
    let first_write = "00000000-0000-0000-0000-000000000001";
    let second_write = "00000000-0000-0000-0000-000000000002";
    let notification = "00000000-0000-0000-0000-000000000003";
    let mut responder = BleResponder::new([
        first_write.to_string(),
        second_write.to_string(),
        notification.to_string(),
    ])
    .queue_notification_after_fenced_write(notification, first_write, 1, &[0x01])
    .queue_notification_after_fenced_write(notification, second_write, 1, &[0x02]);
    responder.connect();
    responder.discover_services().expect("discover");
    responder
        .write_with_notification_fence(first_write, &[0x01], notification)
        .expect("first fenced write");
    responder
        .write_with_notification_fence(second_write, &[0x02], notification)
        .expect("second fenced write");
    assert_eq!(
        responder.take_notification(notification),
        Some(vec![0x02]),
        "the second fence discards the first write's buffered payload"
    );
}

#[test]
fn failed_fenced_write_does_not_mutate_the_notification_stream() {
    let missing_write = "00000000-0000-0000-0000-000000000001";
    let notification = "00000000-0000-0000-0000-000000000002";
    let mut responder =
        BleResponder::new([notification.to_string()]).queue_notification(notification, &[0x01]);
    responder.connect();
    responder.discover_services().expect("discover");
    assert!(responder
        .write_with_notification_fence(missing_write, &[0x01], notification)
        .is_err());
    assert!(responder
        .log()
        .iter()
        .all(|event| !matches!(event, BleEvent::NotificationFence { .. })));
    assert_eq!(responder.take_notification(notification), Some(vec![0x01]));
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

fn walk_refusal(
    detail: u16,
    ap_state_baselines: &[u16],
    ap_state_notifications: &[(u32, u16)],
) -> (Result<BTreeMap<String, String>, WalkError>, Vec<BleEvent>) {
    let view = gfx100ii();
    let ble = view.ble.as_ref().expect("fuji BLE block");
    let plan = ble
        .establishment("ble-establish-wifi-ap")
        .expect("Wi-Fi plan");
    let launch = uuid(ble, "functionLaunchRequest");
    let image_setting = uuid(ble, "imageTransferSetting");
    let ap_state = uuid(ble, "apState");
    let details = uuid(ble, "stateErrorDetails");
    assert_eq!(details, STATE_ERROR_DETAILS_UUID);
    let ssid = uuid(ble, "cameraSSIDNameString");
    let passphrase = uuid(ble, "cameraWiFiPassphraseString");
    let mut responder = BleResponder::new([
        launch.clone(),
        image_setting,
        ap_state.clone(),
        details.clone(),
        ssid.clone(),
        passphrase.clone(),
    ])
    .serve_read_sequence(
        &ap_state,
        ap_state_baselines
            .iter()
            .map(|value| value.to_le_bytes().to_vec())
            .collect(),
    )
    .serve_read(&details, &detail.to_le_bytes())
    .serve_read(&ssid, b"GFX100II-1234")
    .serve_read(&passphrase, b"secret-pass");
    for (attempt, value) in ap_state_notifications {
        responder = responder.queue_notification_after_fenced_write(
            &ap_state,
            &launch,
            *attempt,
            &value.to_le_bytes(),
        );
    }
    let runtime_params = [("launchMode".to_string(), "4".to_string())]
        .into_iter()
        .collect();
    let result = walk_establishment(
        &mut responder,
        &plan.steps,
        &BTreeMap::new(),
        &BTreeMap::new(),
        &runtime_params,
    )
    .map(|outcome| outcome.scope);
    (result, responder.log().to_vec())
}

fn count_writes(events: &[BleEvent], uuid: &str) -> usize {
    events
        .iter()
        .filter(|event| matches!(event, BleEvent::Write { uuid: actual, .. } if actual == uuid))
        .count()
}

#[test]
fn short_camera_action_resends_launch_once_then_recovers() {
    let (result, events) = walk_refusal(2, &[0x8000], &[(1, 0x8000), (2, 0x8001)]);
    let scope = result.expect("second launch notification reaches Launched");
    assert_eq!(scope.get("apState").map(String::as_str), Some("32769"));
    assert_eq!(
        count_writes(&events, "600655E6-3637-42F1-8FB2-44EFC5C63B13"),
        2,
    );
    assert_eq!(
        count_writes(&events, "98934B2C-756C-4632-AA2F-DCBA1BFEC824"),
        1,
    );
    assert_eq!(
        events
            .iter()
            .filter(
                |event| matches!(event, BleEvent::Subscribe { uuid, .. } if uuid == AP_STATE_UUID)
            )
            .count(),
        1,
        "the retry reuses the successful CCCD enable",
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, BleEvent::Read { uuid } if uuid == AP_STATE_UUID))
            .count(),
        1,
        "the baseline is read once before the command and its retry",
    );
}

#[test]
fn terminal_baseline_does_not_claim_the_requested_mode_is_ready() {
    let (result, events) = walk_refusal(2, &[0x8001], &[]);
    assert!(
        result.is_err(),
        "AP lifecycle state alone is not requested launch-mode provenance"
    );
    assert_eq!(
        count_writes(&events, "600655E6-3637-42F1-8FB2-44EFC5C63B13"),
        1,
        "the requested launch mode must not be silently skipped"
    );
}

#[test]
fn reserved_transfer_notification_does_not_complete_a_manual_launch() {
    let (scope, _, _) = walk_wifi_ap_with_ap_state(
        "3",
        b"FUJIFILM-GFX100II-0C3E",
        Some(b"secret-pass"),
        vec![0x00, 0x80],
        vec![],
        vec![vec![0x03, 0x80], vec![0x01, 0x80]],
    );
    assert_eq!(
        scope.get("apState").map(String::as_str),
        Some("32769"),
        "0x8003 is reserved auto-transfer; manual launch completes at 0x8001"
    );
}

#[test]
fn retry_fence_discards_notification_left_by_prior_attempt() {
    let (result, events) = walk_refusal(2, &[0x8000], &[(1, 0x8000), (1, 0x8001), (2, 0x8000)]);
    let error = result.expect_err("attempt-1 success cannot satisfy attempt 2");
    assert_eq!(error.kind, RetryFailureKind::ConditionRejected);
    assert_eq!(
        count_writes(&events, "600655E6-3637-42F1-8FB2-44EFC5C63B13"),
        2
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, BleEvent::NotificationFence { uuid } if uuid == AP_STATE_UUID))
            .count(),
        2
    );
}

#[test]
fn short_camera_action_exhausts_after_one_resend_with_curated_context() {
    let (result, events) = walk_refusal(2, &[0x8000], &[(1, 0x8000), (2, 0x8000)]);
    let error = result.expect_err("second refusal exhausts the retry");
    assert_eq!(
        error.kind,
        camera_config::index::RetryFailureKind::ConditionRejected
    );
    assert_eq!(
        error.context,
        BTreeMap::from([
            ("apState".to_string(), "32768".to_string()),
            ("stateErrorDetails".to_string(), "2".to_string()),
        ])
    );
    assert_eq!(
        count_writes(&events, "600655E6-3637-42F1-8FB2-44EFC5C63B13"),
        2,
    );
    assert_eq!(
        count_writes(&events, "98934B2C-756C-4632-AA2F-DCBA1BFEC824"),
        1,
    );
}

#[test]
fn permanent_and_unknown_refusal_details_never_retry() {
    for detail in (0..=14).filter(|detail| *detail != 2).chain([99]) {
        let (result, events) = walk_refusal(detail, &[0x8000], &[(1, 0x8000)]);
        let error = result.unwrap_err();
        assert_eq!(
            error.context.get("stateErrorDetails"),
            Some(&detail.to_string()),
            "detail {detail}",
        );
        assert_eq!(
            count_writes(&events, "600655E6-3637-42F1-8FB2-44EFC5C63B13"),
            1,
            "detail {detail} must not retry",
        );
    }
}
