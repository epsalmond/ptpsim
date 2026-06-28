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
