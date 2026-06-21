//! Issue #25 Phase 1 acceptance: round-trip the REAL Fuji BLE establishment
//! plan from `packages/camera-config-data/fuji/index.yaml` against the
//! in-memory responder — recognition seeds scope, the reference walker plays
//! the app dispatcher, and the responder's interaction log must match the
//! reference app wire order (pair key → name → [RED id exchange] → first-round CCCDs
//! → transferState read → second-round CCCDs).
//!

//! `APP_BLE_PAIRING_HANDSHAKE_2026-05-13.md` §6 + §6a.

use std::collections::BTreeMap;
use std::path::PathBuf;

use camera_config::index::eval::{self, BleAdvertFacts};
use camera_config::index::{
    CccdMode, FamilyBleBlock, ModelView, ResolvedManufacturerIndex, Signature,
};
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

/// Recognition exactly as the FFI does it: first matching signature in file
/// order (§11.7) seeds the scope.
fn recognize(view: &ModelView, facts: &BleAdvertFacts) -> BTreeMap<String, String> {
    for (_name, sig) in &view.signatures {
        let Signature::BleAdvert(sig) = sig;
        if eval::advert_matches(sig, facts) {
            return eval::advert_scope(sig, facts).into_iter().collect();
        }
    }
    panic!("no signature matched the synthetic advert");
}

fn legacy_facts(ble: &FamilyBleBlock) -> BleAdvertFacts {
    BleAdvertFacts {
        service_uuids: vec![ble.advert.service_uuids["fileTransfer"].clone()],
        manufacturer_data: Some((
            ble.advert.manufacturer_company_id.expect("company id"),
            vec![0x02, 0x44, 0x73, 0x2a, 0x80],
        )),
        ..Default::default()
    }
}

fn red_facts(ble: &FamilyBleBlock) -> BleAdvertFacts {
    BleAdvertFacts {
        // RED bodies advertise CONNECTED_DEVICE_INFORMATION_RED, not the
        // legacy file-transfer service.
        service_uuids: vec!["123D8F06-62A1-4935-9322-833C531EE225".to_string()],
        manufacturer_data: Some((
            ble.advert.manufacturer_company_id.expect("company id"),
            vec![0x01, b'A', b'B', b'C', b'D', b'E'],
        )),
        ..Default::default()
    }
}

/// The CCCD rounds in reference app order, by symbolic name.
const FIRST_ROUND: [&str; 4] = [
    "apState",
    "transferState",
    "dateSyncState",
    "locationSyncState",
];
const SECOND_ROUND: [&str; 9] = [
    "remoteBootSetting",
    "cameraSSIDNameString",
    "loggingSetting",
    "cameraVitalState",
    "locationSyncSetting",
    "imageResizeRate",
    "imageResizeSetting",
    "iptcSetting",
    "locationSyncCycle",
];

fn responder_for(ble: &FamilyBleBlock) -> BleResponder {
    BleResponder::new(ble.gatt.values().cloned())
        .serve_read(&uuid(ble, "protectedSerialString"), b"FF123456")
        .serve_read(&uuid(ble, "transferState"), &[0x00])
}

fn runtime_params() -> BTreeMap<String, String> {
    BTreeMap::from([("terminalName".to_string(), "iphone".to_string())])
}

/// Assert the responder saw exactly the reference app pair sequence after the writes
/// begin: pairing key, device name, optional RED id exchange, first-round
/// CCCDs, transferState read, second-round CCCDs.
fn assert_app_order(ble: &FamilyBleBlock, log: &[BleEvent], red_exchange: bool) {
    let mut expected: Vec<BleEvent> = vec![
        BleEvent::Connect,
        BleEvent::Read {
            uuid: uuid(ble, "protectedSerialString"),
        },
        BleEvent::Write {
            uuid: uuid(ble, "pairingKey"),
            value: if red_exchange {
                b"ABCDE".to_vec()
            } else {
                vec![0x44, 0x73, 0x2a, 0x80]
            },
        },
        BleEvent::Write {
            uuid: uuid(ble, "deviceNameString"),
            value: b"iphone".to_vec(),
        },
    ];
    if red_exchange {
        expected.push(BleEvent::Read {
            uuid: uuid(ble, "deviceIdentificationNumber"),
        });
        // Echo = camera id | 0x20000000 (the APP_IDENTIFIER OR), LE bytes.
        expected.push(BleEvent::Write {
            uuid: uuid(ble, "deviceIdentificationNumber"),
            value: 0x3234_5678u32.to_le_bytes().to_vec(),
        });
    }
    for name in FIRST_ROUND {
        expected.push(BleEvent::Subscribe {
            uuid: uuid(ble, name),
            mode: CccdMode::Notify,
        });
    }
    expected.push(BleEvent::Read {
        uuid: uuid(ble, "transferState"),
    });
    for name in SECOND_ROUND {
        expected.push(BleEvent::Subscribe {
            uuid: uuid(ble, name),
            mode: CccdMode::Notify,
        });
    }
    assert_eq!(
        log, expected,
        "responder log must match the reference app wire order"
    );
}

#[test]
fn legacy_pair_plan_round_trips_against_the_responder() {
    let view = gfx100ii();
    let ble = view.ble.as_ref().unwrap();
    let scope = recognize(&view, &legacy_facts(ble));
    assert_eq!(scope.get("style").map(String::as_str), Some("legacy"));

    let mut responder = responder_for(ble);
    let outcome = walk_establishment(
        &mut responder,
        &ble.establishment("ble-pair").unwrap().steps,
        &scope,
        &runtime_params(),
    )
    .expect("every step of the legacy plan completes");

    assert_app_order(ble, responder.log(), false);
    // Captures landed in scope: serial from the bond-trigger read, the
    // transferState handshake read; the RED idNumber never bound.
    assert_eq!(
        outcome.scope.get("cameraSerial").map(String::as_str),
        Some("4646313233343536"), // hex of b"FF123456" (encoding: bytes)
    );
    assert_eq!(
        outcome.scope.get("transferState").map(String::as_str),
        Some("00")
    );
    assert!(!outcome.scope.contains_key("idNumber"));
}

#[test]
fn red_pair_plan_round_trips_with_id_number_echo() {
    let view = gfx100ii();
    let ble = view.ble.as_ref().unwrap();
    let scope = recognize(&view, &red_facts(ble));
    assert_eq!(scope.get("style").map(String::as_str), Some("red"));
    assert_eq!(scope.get("shortSerial").map(String::as_str), Some("ABCDE"));

    // RED bodies expose deviceIdentificationNumber; camera-chosen id.
    let mut responder = responder_for(ble).serve_read(
        &uuid(ble, "deviceIdentificationNumber"),
        &0x1234_5678u32.to_le_bytes(),
    );
    let outcome = walk_establishment(
        &mut responder,
        &ble.establishment("ble-pair").unwrap().steps,
        &scope,
        &runtime_params(),
    )
    .expect("every step of the RED plan completes");

    assert_app_order(ble, responder.log(), true);
    // idNumber decoded per `encoding: u32` (decimal string in scope), and
    // the echo write carried (id | 0x20000000) — asserted in the log above.
    assert_eq!(
        outcome.scope.get("idNumber").map(String::as_str),
        Some("305419896"), // 0x12345678
    );
}

#[test]
fn legacy_body_without_id_characteristic_still_completes() {
    // A LEGACY-styled body never enters the RED branch, so the absent
    // deviceIdentificationNumber read policy must not matter; and the
    // tolerant bond-trigger read failing (no serve_read) is skipped, not
    // fatal — the reference app behavior the data's tolerant flags encode.
    let view = gfx100ii();
    let ble = view.ble.as_ref().unwrap();
    let scope = recognize(&view, &legacy_facts(ble));

    let mut responder = BleResponder::new(ble.gatt.values().cloned())
        .serve_read(&uuid(ble, "transferState"), &[0x01]);
    let outcome = walk_establishment(
        &mut responder,
        &ble.establishment("ble-pair").unwrap().steps,
        &scope,
        &runtime_params(),
    )
    .expect("tolerant reads skip; the plan still completes");
    assert!(!outcome.scope.contains_key("cameraSerial"));
    assert_eq!(
        outcome.scope.get("transferState").map(String::as_str),
        Some("01")
    );
}

#[test]
fn legacy_tolerant_bond_read_skips_when_characteristic_absent_from_catalog() {
    // Distinct from `legacy_body_without_id_characteristic_still_completes`,
    // where the bond-trigger characteristic is catalogued but unserved (the
    // read logs, then fails `NotExposed`). Here it is ABSENT from the catalog
    // entirely, so `require_char` rejects it before the read is even logged —
    // and the tolerant bond read must still skip, not abort. This covers the
    // char-absent tolerant path on the legacy side (the RED side is #38).
    let view = gfx100ii();
    let ble = view.ble.as_ref().unwrap();
    let scope = recognize(&view, &legacy_facts(ble));

    let serial_uuid = uuid(ble, "protectedSerialString");
    let mut responder =
        BleResponder::new(ble.gatt.values().filter(|u| **u != serial_uuid).cloned())
            .serve_read(&uuid(ble, "transferState"), &[0x00]);

    let outcome = walk_establishment(
        &mut responder,
        &ble.establishment("ble-pair").unwrap().steps,
        &scope,
        &runtime_params(),
    )
    .expect("the tolerant bond read skips an absent characteristic; plan completes");

    assert!(!outcome.scope.contains_key("cameraSerial"));
    // The absent characteristic was never even read (rejected pre-log).
    assert!(
        !responder
            .log()
            .iter()
            .any(|e| matches!(e, BleEvent::Read { uuid } if *uuid == serial_uuid)),
        "no read was logged for the absent bond characteristic"
    );
    assert_eq!(
        outcome.scope.get("transferState").map(String::as_str),
        Some("00")
    );
    // The CCCD rounds still ran in full.
    assert_eq!(responder.subscribed().len(), 13);
}

#[test]
fn red_body_without_id_characteristic_skips_the_echo_and_completes() {
    // #38: a red-styled body that doesn't expose deviceIdentificationNumber
    // must complete the plan with BOTH id steps skipped — the read is
    // tolerant (gatt-not-found) and the echo write is tolerant (idNumber
    // unbound). Before the fix the echo write hard-failed the walk.
    let view = gfx100ii();
    let ble = view.ble.as_ref().unwrap();
    let scope = recognize(&view, &red_facts(ble));
    assert_eq!(scope.get("style").map(String::as_str), Some("red"));

    // Catalog WITHOUT deviceIdentificationNumber at all.
    let id_uuid = uuid(ble, "deviceIdentificationNumber");
    let mut responder = BleResponder::new(ble.gatt.values().filter(|u| **u != id_uuid).cloned())
        .serve_read(&uuid(ble, "protectedSerialString"), b"FF123456")
        .serve_read(&uuid(ble, "transferState"), &[0x00]);

    let outcome = walk_establishment(
        &mut responder,
        &ble.establishment("ble-pair").unwrap().steps,
        &scope,
        &runtime_params(),
    )
    .expect("plan completes despite the absent characteristic");

    assert!(!outcome.scope.contains_key("idNumber"));
    assert!(
        responder.written(&id_uuid).is_empty(),
        "no echo write reached the absent characteristic"
    );
    // The CCCD rounds still ran in full after the skipped exchange.
    assert_eq!(responder.subscribed().len(), 13);
}
