use std::collections::BTreeMap;
use std::path::PathBuf;

use camera_config::index::{
    FamilyBleBlock, ModelView, ResolvedManufacturerIndex, RetryFailureKind,
};
use camera_config::CameraManifest;
use camera_media_store::MediaStore;
use camera_sim::{walk_establishment, BleEvent, BleResponder, Engine, Reply, WalkError};
use ptp_core::{DeviceInfo, OperationRequest};
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

/// Returns the engine with the guard for its media root. `tempfile` creates the
/// root exclusively under a random name and removes it when the guard drops.
fn engine() -> (Engine, tempfile::TempDir) {
    let manifest =
        CameraManifest::from_yaml(&data("fuji/xa7/xa7.yaml")).expect("X-A7 manifest loads");
    let root = tempfile::Builder::new()
        .prefix("ptpsim-xa7-device-info-")
        .tempdir()
        .expect("create X-A7 media root");
    let mut store = MediaStore::open(root.path()).expect("open X-A7 media store");
    store.scan().expect("scan X-A7 media store");
    (Engine::new(manifest, store), root)
}

fn request(code: u16, transaction_id: u32, params: Vec<u32>) -> OperationRequest {
    OperationRequest {
        data_phase_info: 1,
        code,
        transaction_id,
        params,
    }
}

fn data_reply(reply: Reply) -> Vec<u8> {
    match reply {
        Reply::Data { data, response } => {
            assert_eq!(response.code, 0x2001, "OK expected");
            data
        }
        other => panic!("expected data reply, got {other:?}"),
    }
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
fn device_info_advertises_property_operations_used_by_the_manifest() {
    let (mut engine, _media_root) = engine();
    engine.bind_connection("legacy-app");
    let open = engine.on_operation(&request(0x1002, 1, vec![1]), None);
    assert!(matches!(open, Reply::Response(response) if response.code == 0x2001));
    let device_info = DeviceInfo::decode(&data_reply(
        engine.on_operation(&request(0x1001, 2, vec![]), None),
    ))
    .expect("decode X-A7 DeviceInfo");
    for operation in [0x1014, 0x1015, 0x1016] {
        assert!(
            device_info.operations_supported.contains(&operation),
            "DeviceInfo omits 0x{operation:04x}"
        );
    }
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
fn ap_state_two_keeps_the_poll_waiting() {
    let view = xa7();
    let ble = view.ble.as_ref().unwrap();
    let steps = &ble
        .establishment("legacy-app-establish-wifi-ap")
        .unwrap()
        .steps;
    let mut responder = responder(ble, vec![vec![2, 0], vec![2, 0], vec![1, 0]]);
    let outcome = walk(steps, &mut responder).expect("launching then launched succeeds");
    assert_eq!(outcome.scope.get("apState").map(String::as_str), Some("1"));
}

/// State 3 means reserved or automatic transfer in the legacy vendor mobile
/// app, which ends the launch as not connectable. The walk surfaces it as
/// `ConditionRejected`; consumers present it as the camera being in its
/// reserved (automatic) transfer state, not as a refusal.
#[test]
fn ap_state_three_ends_the_launch_as_not_connectable() {
    let view = xa7();
    let ble = view.ble.as_ref().unwrap();
    let steps = &ble
        .establishment("legacy-app-establish-wifi-ap")
        .unwrap()
        .steps;
    let mut responder = responder(ble, vec![vec![3, 0]]);
    let Err(error) = walk(steps, &mut responder) else {
        panic!("state three must end the launch");
    };
    assert_eq!(error.kind, RetryFailureKind::ConditionRejected);
    assert_eq!(error.step, "steps[4].bleAwaitUntil");
}

/// Characteristics the X-A7 registration plan touches, by GATT key, with the
/// bytes this simulated camera serves on read. `None` marks a characteristic
/// that is written or subscribed but never read.
const REGISTRATION_CHARACTERISTICS: &[(&str, Option<&[u8]>)] = &[
    ("pairingKey", None),
    ("deviceNameString", None),
    ("connectedDeviceBleProtocolVersion", None),
    ("protectedSerialString", Some(b"9AB12345\0")),
    ("cameraSSIDNameString", Some(b"FUJIFILM-X-A7-1361\0")),
    ("reservationAfterShooting", Some(&[0x00])),
    ("pairedSmartDeviceCount", Some(&[0x01])),
    ("firmwareRevisionString", Some(b"2.30\0")),
    ("lensProductName", Some(b"XC15-45mm\0")),
    ("lensFirmwareVersion", Some(b"1.00\0")),
    ("wakeupMode", Some(&[0x00])),
    ("startupValue", Some(&[0x00, 0x00, 0x00, 0x00])),
    ("disconnectedReason", Some(&[0x00, 0x00])),
    ("cardState", Some(&[0x01])),
    ("movieButton", Some(&[0x00])),
    ("cameraBleProtocolVersion", Some(&[0x02, 0x00])),
    ("apState", Some(&[0x00, 0x00])),
    ("transferState", Some(&[0x00, 0x00])),
    ("dateSyncState", None),
    ("locationSyncState", None),
    ("firmwareUpdateState", None),
];

/// A simulated X-A7 serving the whole registration surface except `omit`.
/// An omitted key is left out of the catalog, as on a body that does not
/// expose it, so reads, writes and subscribes to it all fail.
fn registration_responder(ble: &FamilyBleBlock, omit: &[&str]) -> BleResponder {
    registration_responder_with(ble, omit, &[], &[])
}

/// As [`registration_responder`]. An `unserved` key stays in the catalog, so
/// writes and subscribes to it succeed, but its reads fail. `extra` adds
/// writable characteristics given by UUID.
fn registration_responder_with(
    ble: &FamilyBleBlock,
    omit: &[&str],
    unserved: &[&str],
    extra: &[&str],
) -> BleResponder {
    let mut catalog: Vec<String> = extra.iter().map(|uuid| uuid.to_string()).collect();
    let mut served = Vec::new();
    for (key, bytes) in REGISTRATION_CHARACTERISTICS {
        if omit.contains(key) {
            continue;
        }
        catalog.push(uuid(ble, key));
        if unserved.contains(key) {
            continue;
        }
        if let Some(bytes) = bytes {
            served.push((uuid(ble, key), *bytes));
        }
    }
    let mut responder = BleResponder::new(catalog).with_peripheral_name("1361X-A7-1361");
    for (uuid, bytes) in served {
        responder = responder.serve_read(&uuid, bytes);
    }
    responder
}

fn walk_registration(
    steps: &[camera_config::index::Step],
    responder: &mut BleResponder,
) -> Result<camera_sim::WalkOutcome, WalkError> {
    let scope = BTreeMap::from([("pairingKeyBytes".to_string(), "095ee904".to_string())]);
    let params = BTreeMap::from([("terminalName".to_string(), "host".to_string())]);
    walk_establishment(responder, steps, &scope, &BTreeMap::new(), &params)
}

fn registration_steps(ble: &FamilyBleBlock) -> Vec<camera_config::index::Step> {
    ble.establishment("legacy-app-pair")
        .expect("legacy app registration plan")
        .steps
        .clone()
}

#[test]
fn registration_tolerates_missing_optional_reads_like_the_legacy_app() {
    let view = xa7();
    let ble = view.ble.as_ref().unwrap();
    // AP state stays subscribable (the subscribe is strict) but its read
    // fails, so only the tail read is tolerated.
    let mut responder = registration_responder_with(
        ble,
        &[
            "reservationAfterShooting",
            "pairedSmartDeviceCount",
            "firmwareRevisionString",
        ],
        &["apState"],
        &[],
    );
    let outcome = walk_registration(&registration_steps(ble), &mut responder)
        .expect("registration completes past the tolerated reads");
    assert_eq!(
        outcome.summary.tolerated_step_paths,
        [
            "steps[11].bleRead",
            "steps[12].bleRead",
            "steps[13].bleRead",
            "steps[28].bleRead",
        ]
    );
    for missing in [
        "reservationAfterShooting",
        "pairedSmartDeviceCount",
        "firmware",
        "apState",
    ] {
        assert!(!outcome.scope.contains_key(missing), "{missing} is unbound");
    }
    assert_eq!(
        outcome.scope.get("cameraSerial").map(String::as_str),
        Some("9AB12345")
    );
    assert_eq!(
        outcome.scope.get("ssid").map(String::as_str),
        Some("FUJIFILM-X-A7-1361")
    );
}

#[test]
fn registration_fails_without_the_serial_or_ssid() {
    let view = xa7();
    let ble = view.ble.as_ref().unwrap();
    for (omit, step) in [
        ("protectedSerialString", "steps[9].bleRead"),
        ("cameraSSIDNameString", "steps[10].bleRead"),
    ] {
        let mut responder = registration_responder(ble, &[omit]);
        let Err(error) = walk_registration(&registration_steps(ble), &mut responder) else {
            panic!("a missing {omit} must fail registration");
        };
        assert_eq!(error.step, step, "{omit}");
    }
}

/// The legacy vendor mobile app never marks the link ready when its remaining
/// CCCD writes all fail, so a link that drops during registration must fail
/// the walk rather than let every later step fail tolerantly. The responder
/// has no link-drop switch, so the drop is modelled by exposing only the
/// characteristics steps 0 to 10 use: every later read and every later
/// subscribe except the SSID one then fails, as on a dropped link.
#[test]
fn registration_fails_when_the_link_drops_after_the_ssid_read() {
    let view = xa7();
    let ble = view.ble.as_ref().unwrap();
    let kept = [
        "pairingKey",
        "deviceNameString",
        "connectedDeviceBleProtocolVersion",
        "protectedSerialString",
        "cameraSSIDNameString",
    ];
    let dropped: Vec<&str> = REGISTRATION_CHARACTERISTICS
        .iter()
        .map(|(key, _)| *key)
        .filter(|key| !kept.contains(key))
        .collect();
    let mut responder = registration_responder(ble, &dropped);
    let Err(error) = walk_registration(&registration_steps(ble), &mut responder) else {
        panic!("a dropped link must not complete registration");
    };
    assert_eq!(error.step, "steps[23].bleSubscribe");
}

/// The legacy vendor mobile app sends its second name write when the GAP name
/// read completes, racing the serial read. The manifest produces one of the
/// app's two orders: name, second name write, then serial.
#[test]
fn registration_sends_the_second_name_write_between_the_name_and_serial() {
    let view = xa7();
    let ble = view.ble.as_ref().unwrap();
    let mut responder = registration_responder(ble, &[]);
    walk_registration(&registration_steps(ble), &mut responder).expect("registration completes");
    let name = uuid(ble, "deviceNameString");
    let serial = uuid(ble, "protectedSerialString");
    let log = responder.log();
    let position = |matches: &dyn Fn(&BleEvent) -> bool, nth: usize| {
        log.iter()
            .enumerate()
            .filter(|(_, event)| matches(event))
            .nth(nth)
            .map(|(index, _)| index)
            .expect("event present")
    };
    let peripheral_name = position(&|event| matches!(event, BleEvent::PeripheralName), 0);
    let second_name_write = position(
        &|event| matches!(event, BleEvent::Write { uuid, .. } if uuid == &name),
        1,
    );
    let serial_read = position(
        &|event| matches!(event, BleEvent::Read { uuid } if uuid == &serial),
        0,
    );
    assert!(peripheral_name < second_name_write);
    assert!(second_name_write < serial_read);
    assert_eq!(
        responder.written(&name),
        [b"host\0".as_slice(), b"host\0".as_slice()],
        "the two name writes are byte-identical"
    );
}

/// The simulator models no pairing screen or registration state for the
/// X-A7, because the event that ends the camera's pairing screen is unknown
/// (#355). Client traffic (the name writes, a local-time write, the tail reads,
/// repeated serial reads, a reconnect walk) must not change what the simulated
/// camera serves or make it send anything. The generic responder, with no
/// linked engine, must keep its reads unchanged and send nothing under this
/// client traffic, so no X-A7 test can rely on a write-triggered registration
/// cue that no evidence supports.
#[test]
fn no_client_traffic_changes_simulated_registration_state() {
    // The legacy vendor mobile app's local-time characteristic. The manifest
    // declares no GATT key for it because it performs no time write.
    const LOCAL_TIME: &str = "B9BFD37F-CCAD-4D36-A1EE-018E792B3EDF";
    let view = xa7();
    let ble = view.ble.as_ref().unwrap();
    let mut responder = registration_responder_with(ble, &[], &[], &[LOCAL_TIME]);

    let first = walk_registration(&registration_steps(ble), &mut responder)
        .expect("registration completes");
    assert_eq!(
        first.summary.confirm_outcome,
        camera_sim::EstablishmentConfirmOutcome::NotDeclared
    );
    assert!(first.summary.tolerated_step_paths.is_empty());

    let name = uuid(ble, "deviceNameString");
    let serial = uuid(ble, "protectedSerialString");
    responder
        .write(LOCAL_TIME, &[0xEA, 0x07, 11, 24, 12, 0, 0])
        .expect("time write accepted");
    responder
        .write(&name, b"host\0")
        .expect("extra name write accepted");
    for _ in 0..3 {
        responder.read(&serial).expect("serial stays readable");
    }

    for (key, bytes) in REGISTRATION_CHARACTERISTICS {
        let uuid = uuid(ble, key);
        if let Some(bytes) = bytes {
            assert_eq!(
                responder.read(&uuid).expect("still served"),
                *bytes,
                "{key} is unchanged by client traffic"
            );
        }
        assert_eq!(
            responder.take_notification(&uuid),
            None,
            "{key} sends nothing in response to client traffic"
        );
    }

    let reconnect = ble
        .establishment("legacy-app-reconnect")
        .expect("legacy app reconnect plan")
        .steps
        .clone();
    let again = walk_registration(&reconnect, &mut responder).expect("reconnect completes");
    assert_eq!(again.scope, first.scope, "the camera answers the same way");
    assert_eq!(
        again.summary.confirm_outcome,
        camera_sim::EstablishmentConfirmOutcome::NotDeclared
    );
    for (key, _) in REGISTRATION_CHARACTERISTICS {
        assert_eq!(
            responder.take_notification(&uuid(ble, key)),
            None,
            "{key} sends nothing after the reconnect walk"
        );
    }
}
