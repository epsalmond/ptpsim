use std::collections::BTreeMap;
use std::path::PathBuf;

use camera_config::index::{FamilyBleBlock, ModelView, ResolvedManufacturerIndex};
use camera_sim::{walk_establishment, BleEvent, BleResponder};

fn data(rel: &str) -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../packages/camera-config-data")
        .join(rel);
    std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("read {}: {error}", path.display()))
}

fn gfx100ii() -> ModelView {
    ResolvedManufacturerIndex::from_yaml(&data("fuji/index.yaml"))
        .expect("Fuji index loads")
        .models
        .into_iter()
        .find(|model| model.id == "gfx100ii")
        .expect("GFX100 II model")
}

fn uuid(ble: &FamilyBleBlock, name: &str) -> String {
    ble.gatt
        .get(name)
        .unwrap_or_else(|| panic!("missing GATT key {name}"))
        .clone()
}

fn run(
    ble: &FamilyBleBlock,
    action: &str,
    responder: &mut BleResponder,
    params: &BTreeMap<String, String>,
) {
    walk_establishment(
        responder,
        &ble.action(action).unwrap().steps,
        &BTreeMap::new(),
        &BTreeMap::new(),
        params,
    )
    .unwrap_or_else(|error| panic!("{action} completes: {error}"));
}

#[test]
fn remote_shutter_writes_s1_s2_s1_s0() {
    let view = gfx100ii();
    let ble = view.ble.as_ref().unwrap();
    let mut responder = BleResponder::new(ble.gatt.values().cloned());
    run(ble, "remote-shutter", &mut responder, &BTreeMap::new());
    assert_eq!(
        responder.written(&uuid(ble, "shootingRequest")),
        vec![&[1, 0][..], &[2, 0][..], &[1, 0][..], &[0, 0][..],]
    );
}

#[test]
fn auto_transfer_size_actions_keep_the_declared_values() {
    let view = gfx100ii();
    let ble = view.ble.as_ref().unwrap();
    let setting = uuid(ble, "imageResizeSetting");
    let rate = uuid(ble, "imageResizeRate");
    let cases = [
        (
            "auto-transfer-size-original",
            vec![(setting.clone(), vec![0])],
        ),
        (
            "auto-transfer-size-s",
            vec![(rate.clone(), vec![1, 0]), (setting.clone(), vec![1])],
        ),
        (
            "auto-transfer-size-xs",
            vec![(rate.clone(), vec![0, 0]), (setting.clone(), vec![1])],
        ),
    ];
    for (action, expected) in cases {
        let mut responder = BleResponder::new(ble.gatt.values().cloned());
        run(ble, action, &mut responder, &BTreeMap::new());
        let writes = responder
            .log()
            .iter()
            .filter_map(|event| match event {
                BleEvent::Write { uuid, value } => Some((uuid.clone(), value.clone())),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(writes, expected, "{action}");
    }
}

#[test]
fn write_time_and_gps_forward_binary_payloads() {
    let view = gfx100ii();
    let ble = view.ble.as_ref().unwrap();
    for (action, slot, gatt, length) in [
        ("write-time", "utcTimezonePayload", "utcAndTimezone", 12u8),
        (
            "write-gps",
            "locationSpeedPayload",
            "locationAndSpeed",
            23u8,
        ),
    ] {
        let payload = (1..=length).collect::<Vec<_>>();
        let payload_hex = payload
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        let params = BTreeMap::from([(slot.to_string(), payload_hex)]);
        let mut responder = BleResponder::new(ble.gatt.values().cloned());
        run(ble, action, &mut responder, &params);
        assert_eq!(responder.written(&uuid(ble, gatt)), vec![payload]);
    }
}

#[test]
fn legacy_app_movie_record_uses_its_dedicated_characteristic() {
    let view = gfx100ii();
    let ble = view.ble.as_ref().unwrap();
    let mut responder = BleResponder::new(ble.gatt.values().cloned());
    run(
        ble,
        "legacy-app-movie-record",
        &mut responder,
        &BTreeMap::new(),
    );
    assert_eq!(
        responder.written(&uuid(ble, "movieRecordRequest")),
        vec![&[1, 0][..]]
    );
}

#[test]
fn settings_backup_reads_each_announced_chunk() {
    let view = gfx100ii();
    let ble = view.ble.as_ref().unwrap();
    let state = uuid(ble, "fileTransactionState");
    let partial = uuid(ble, "filePartialData");
    let mut responder = BleResponder::new(ble.gatt.values().cloned())
        .queue_notification(&state, &[1, 0, 0, 0])
        .queue_notification(&state, &[2, 0, 1, 0])
        .queue_notification(&state, &[3, 0, 0xff, 0xff])
        .serve_read_sequence(
            &partial,
            vec![vec![0, 0, 1, 0, 0, 0, 0xaa], vec![1, 0, 1, 0, 0, 0, 0xbb]],
        );
    run(ble, "settings-backup", &mut responder, &BTreeMap::new());
    assert_eq!(
        responder
            .log()
            .iter()
            .filter(|event| matches!(event, BleEvent::Read { uuid } if uuid == &partial))
            .count(),
        2
    );
    assert_eq!(
        responder.written(&uuid(ble, "backupRequest")),
        vec![&[1, 0][..]]
    );
}

#[test]
fn settings_restore_writes_each_requested_frame() {
    let view = gfx100ii();
    let ble = view.ble.as_ref().unwrap();
    let state = uuid(ble, "fileTransactionState");
    let partial = uuid(ble, "filePartialData");
    let blob = (0..130u32).map(|value| value as u8).collect::<Vec<_>>();
    let blob_hex = blob
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    let params = BTreeMap::from([("settingsBlob".to_string(), blob_hex)]);
    let mut responder = BleResponder::new(ble.gatt.values().cloned())
        .queue_notification(&state, &[1, 0, 0, 0])
        .queue_notification(&state, &[2, 0, 0xff, 0xff])
        .queue_notification(&state, &[3, 0, 0xff, 0xff]);
    run(ble, "settings-restore", &mut responder, &params);

    let frame = |index: u16, bytes: &[u8]| {
        let mut frame = index.to_le_bytes().to_vec();
        frame.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
        frame.extend_from_slice(bytes);
        frame
    };
    assert_eq!(
        responder
            .written(&partial)
            .into_iter()
            .map(|bytes| bytes.to_vec())
            .collect::<Vec<_>>(),
        vec![frame(0, &blob[..120]), frame(0xffff, &blob[120..])]
    );
}
