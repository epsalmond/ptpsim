//! #91 acceptance: BLE-native control actions runnable from the resting
//! BLE-connected link without Wi-Fi — `remote-shutter`, `write-time`,
//! `write-gps`. The plans live in the family BLE `actions:` registry and reuse
//! the establishment step grammar. Source: client application FujiBLERegistration.swift.

use std::collections::BTreeMap;
use std::path::PathBuf;

use camera_config::index::{FamilyBleBlock, ModelView, ResolvedManufacturerIndex};
use camera_sim::{walk_establishment, BleEvent, BleResponder};

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

#[test]
fn remote_shutter_writes_the_s1_s2_release_sequence() {
    let view = gfx100ii();
    let ble = view.ble.as_ref().unwrap();
    let mut r = BleResponder::new(ble.gatt.values().cloned());
    walk_establishment(
        &mut r,
        &ble.action("remote-shutter").unwrap().steps,
        &BTreeMap::new(),
        &BTreeMap::new(),
        &BTreeMap::new(),
    )
    .expect("the remote-shutter action completes");

    let sr = uuid(ble, "shootingRequest");
    // The exact S1 → S2 → S1 → S0 press sequence on SHOOTING_REQUEST.
    assert_eq!(
        r.written(&sr),
        vec![
            vec![0x01, 0x00],
            vec![0x02, 0x00],
            vec![0x01, 0x00],
            vec![0x00, 0x00],
        ],
    );
    assert_eq!(
        r.log(),
        &[
            BleEvent::Connect,
            BleEvent::Write {
                uuid: sr.clone(),
                value: vec![0x01, 0x00]
            },
            BleEvent::Write {
                uuid: sr.clone(),
                value: vec![0x02, 0x00]
            },
            BleEvent::Write {
                uuid: sr.clone(),
                value: vec![0x01, 0x00]
            },
            BleEvent::Write {
                uuid: sr.clone(),
                value: vec![0x00, 0x00]
            },
        ],
    );
}

#[test]
fn write_gps_writes_the_host_packed_payload() {
    let view = gfx100ii();
    let ble = view.ble.as_ref().unwrap();
    let mut r = BleResponder::new(ble.gatt.values().cloned());

    // The host packs the 23-byte location/speed payload (its own GPS data) and
    // supplies it as a hex string; the plan just writes it verbatim.
    let payload: Vec<u8> = (1u8..=23).collect();
    let payload_hex: String = payload.iter().map(|b| format!("{b:02x}")).collect();
    let params = BTreeMap::from([("locationSpeedPayload".to_string(), payload_hex)]);

    walk_establishment(
        &mut r,
        &ble.action("write-gps").unwrap().steps,
        &BTreeMap::new(),
        &BTreeMap::new(),
        &params,
    )
    .expect("the write-gps action completes");

    assert_eq!(r.written(&uuid(ble, "locationAndSpeed")), vec![payload]);
}

#[test]
fn write_time_writes_the_host_packed_payload() {
    let view = gfx100ii();
    let ble = view.ble.as_ref().unwrap();
    let mut r = BleResponder::new(ble.gatt.values().cloned());

    let payload: Vec<u8> = (1u8..=12).collect();
    let payload_hex: String = payload.iter().map(|b| format!("{b:02x}")).collect();
    let params = BTreeMap::from([("utcTimezonePayload".to_string(), payload_hex)]);

    walk_establishment(
        &mut r,
        &ble.action("write-time").unwrap().steps,
        &BTreeMap::new(),
        &BTreeMap::new(),
        &params,
    )
    .expect("the write-time action completes");

    assert_eq!(r.written(&uuid(ble, "utcAndTimezone")), vec![payload]);
}

fn reads_of(r: &BleResponder, target: &str) -> usize {
    r.log()
        .iter()
        .filter(|e| matches!(e, BleEvent::Read { uuid } if uuid == target))
        .count()
}

#[test]
fn settings_backup_reads_each_chunk_until_the_complete_state() {
    // #112 BACKUP: the camera streams fileTransactionState notifications
    // [opcode u16-le][idx u16-le] — 0x0001/0x0002 announce the next chunk,
    // 0x0003 = complete — and the plan reads filePartialData on each
    // announcement until complete. Source: operators OTA doc backup phase.
    let view = gfx100ii();
    let ble = view.ble.as_ref().unwrap();
    let tx = uuid(ble, "fileTransactionState");
    let partial = uuid(ble, "filePartialData");

    let mut r = BleResponder::new(ble.gatt.values().cloned())
        .queue_notification(&tx, &[0x01, 0x00, 0x00, 0x00]) // start, chunk 0 ready
        .queue_notification(&tx, &[0x02, 0x00, 0x01, 0x00]) // chunk 1 ready
        .queue_notification(&tx, &[0x02, 0x00, 0xff, 0xff]) // sentinel chunk ready
        .queue_notification(&tx, &[0x03, 0x00, 0xff, 0xff]) // complete
        .serve_read_sequence(
            &partial,
            vec![
                vec![0x00, 0x00, 0x02, 0x00, 0x00, 0x00, 0xAA, 0xBB],
                vec![0x01, 0x00, 0x02, 0x00, 0x00, 0x00, 0xCC, 0xDD],
                vec![0xff, 0xff, 0x01, 0x00, 0x00, 0x00, 0xEE],
            ],
        );

    walk_establishment(
        &mut r,
        &ble.action("settings-backup").unwrap().steps,
        &BTreeMap::new(),
        &BTreeMap::new(),
        &BTreeMap::new(),
    )
    .expect("the settings-backup action completes on the 0x0003 state");

    // One read of filePartialData per announced chunk (0x0001 + 2× 0x0002); the
    // 0x0003 notification satisfied `until` and ended the loop with no extra read.
    assert_eq!(
        reads_of(&r, &partial),
        3,
        "read each announced chunk, stopped on complete",
    );
    // The arm write happened; the keep-alive channel was subscribed.
    assert_eq!(
        r.written(&uuid(ble, "backupRequest")),
        vec![&[0x01u8, 0x00][..]],
    );
    let keep_alive = uuid(ble, "settingsKeepAlive");
    assert!(r.subscribed().contains(&keep_alive.as_str()));
}

#[test]
fn settings_restore_writes_each_framed_chunk_the_camera_requests() {
    // #112 RESTORE: symmetric to backup — the camera announces the next index on
    // fileTransactionState and the plan WRITES the framed window of the host blob
    // for that index. A 250-byte blob → two full 120-byte windows (idx 0, 1) + a
    // 10-byte remainder window (idx 0xffff). Source: operators OTA restore phase.
    let view = gfx100ii();
    let ble = view.ble.as_ref().unwrap();
    let tx = uuid(ble, "fileTransactionState");
    let partial = uuid(ble, "filePartialData");

    let blob: Vec<u8> = (0..250u32).map(|i| i as u8).collect();
    let blob_hex: String = blob.iter().map(|b| format!("{b:02x}")).collect();
    let params = BTreeMap::from([("settingsBlob".to_string(), blob_hex)]);

    let mut r = BleResponder::new(ble.gatt.values().cloned())
        .queue_notification(&tx, &[0x01, 0x00, 0x00, 0x00]) // write chunk 0
        .queue_notification(&tx, &[0x02, 0x00, 0x01, 0x00]) // write chunk 1
        .queue_notification(&tx, &[0x02, 0x00, 0xff, 0xff]) // write sentinel
        .queue_notification(&tx, &[0x03, 0x00, 0xff, 0xff]); // complete

    walk_establishment(
        &mut r,
        &ble.action("settings-restore").unwrap().steps,
        &BTreeMap::new(),
        &BTreeMap::new(),
        &params,
    )
    .expect("the settings-restore action completes");

    // Each framed window: [idx u16-le][len u32-le][payload].
    let frame = |idx: u16, lo: usize, hi: usize| {
        let mut f = idx.to_le_bytes().to_vec();
        f.extend_from_slice(&((hi - lo) as u32).to_le_bytes());
        f.extend_from_slice(&blob[lo..hi]);
        f
    };
    let written: Vec<Vec<u8>> = r.written(&partial).iter().map(|s| s.to_vec()).collect();
    assert_eq!(
        written,
        vec![
            frame(0, 0, 120),
            frame(1, 120, 240),
            frame(0xffff, 240, 250), // short remainder window, indexed 0xffff
        ],
    );
}
