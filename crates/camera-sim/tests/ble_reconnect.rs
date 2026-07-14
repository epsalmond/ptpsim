//! #91 acceptance: warm-resume reconnect of an already-paired camera WITHOUT
//! re-entering pairing mode. The host persisted what first-pair recognition
//! captured (`pairingKeyBytes`, `style`) and re-seeds it; the `ble-reconnect`
//! plan replays the cached key verbatim and re-enables notifications, but —
//! unlike `ble-pair` — does NOT read the protected-serial bond trigger and does
//! NOT run the RED id-number echo (the camera caches the bond + id).
//!


use std::collections::BTreeMap;
use std::path::PathBuf;

use camera_config::index::eval::{self, BleAdvertFacts};
use camera_config::index::{
    CccdMode, Encoding, FamilyBleBlock, ModelView, ResolvedManufacturerIndex, Signature,
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

/// First-pair recognition — the source of the identity the host persists and
/// later re-seeds on a warm resume.
fn recognize(
    view: &ModelView,
    facts: &BleAdvertFacts,
) -> (BTreeMap<String, String>, BTreeMap<String, Encoding>) {
    for (_name, sig) in &view.signatures {
        let Signature::BleAdvert(sig) = sig else {
            continue;
        };
        if sig.discoverable && eval::advert_matches(sig, facts) {
            return (
                eval::advert_scope(sig, facts).into_iter().collect(),
                eval::advert_capture_encodings(sig).into_iter().collect(),
            );
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
        service_uuids: vec!["123D8F06-62A1-4935-9322-833C531EE225".to_string()],
        manufacturer_data: Some((
            ble.advert.manufacturer_company_id.expect("company id"),
            vec![0x01, b'A', b'B', b'C', b'D', b'E'],
        )),
        ..Default::default()
    }
}

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

fn runtime_params() -> BTreeMap<String, String> {
    BTreeMap::from([("terminalName".to_string(), "iphone".to_string())])
}

/// The exact warm-resume order: connect, replay the cached pairing key, re-assert
/// the device name, then both CCCD rounds with the transferState read between.
/// The bond-trigger read and RED id echo are absent by construction (not in the
/// expected vec) — that absence IS the reconnect-vs-pair contract.
fn assert_reconnect_order(ble: &FamilyBleBlock, log: &[BleEvent], pairing_key: Vec<u8>) {
    let mut expected: Vec<BleEvent> = vec![
        BleEvent::Connect,
        BleEvent::DiscoverServices,
        BleEvent::Write {
            uuid: uuid(ble, "pairingKey"),
            value: pairing_key,
        },
        BleEvent::Write {
            uuid: uuid(ble, "deviceNameString"),
            value: b"iphone".to_vec(),
        },
    ];
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
        "reconnect must replay the key + re-subscribe, with NO bond-read and NO RED echo"
    );
}

#[test]
fn reconnect_replays_the_cached_key_and_skips_pairing_mode() {
    let view = gfx100ii();
    let ble = view.ble.as_ref().unwrap();
    // The host persisted first-pair's captured identity and re-seeds it.
    let (scope, encodings) = recognize(&view, &legacy_facts(ble));

    let mut responder = BleResponder::new(ble.gatt.values().cloned())
        .serve_read(&uuid(ble, "transferState"), &[0x00]);
    let outcome = walk_establishment(
        &mut responder,
        &ble.establishment("ble-reconnect").unwrap().steps,
        &scope,
        &encodings,
        &runtime_params(),
    )
    .expect("the reconnect plan completes");

    // Exact warm-resume order (replayed key, no bond-trigger read, no RED echo).
    assert_reconnect_order(ble, responder.log(), vec![0x44, 0x73, 0x2a, 0x80]);

    // The protected-serial bond trigger is never read on a warm resume.
    let serial = uuid(ble, "protectedSerialString");
    assert!(
        responder.written(&serial).is_empty(),
        "no bond-trigger interaction",
    );
    assert!(
        !responder
            .log()
            .iter()
            .any(|e| matches!(e, BleEvent::Read { uuid: u } if *u == serial)),
        "the protected-serial bond read is absent on reconnect",
    );
    // The RED id-number characteristic is never touched (camera caches it).
    assert!(
        responder
            .written(&uuid(ble, "deviceIdentificationNumber"))
            .is_empty(),
        "no RED id-number echo on reconnect",
    );
    // Both CCCD rounds re-ran in full.
    assert_eq!(responder.subscribed().len(), 13);
    assert_eq!(
        outcome.scope.get("transferState").map(String::as_str),
        Some("00")
    );
}

#[test]
fn reconnect_replays_a_red_cached_key_verbatim() {
    // A RED-paired camera persists a 5-byte ASCII key; reconnect replays it with
    // the same plan (no style-specific branch — the camera cached the id-number).
    let view = gfx100ii();
    let ble = view.ble.as_ref().unwrap();
    let (scope, encodings) = recognize(&view, &red_facts(ble));

    let mut responder = BleResponder::new(ble.gatt.values().cloned())
        .serve_read(&uuid(ble, "transferState"), &[0x00]);
    walk_establishment(
        &mut responder,
        &ble.establishment("ble-reconnect").unwrap().steps,
        &scope,
        &encodings,
        &runtime_params(),
    )
    .expect("the RED reconnect plan completes");

    assert_reconnect_order(ble, responder.log(), b"ABCDE".to_vec());
}
