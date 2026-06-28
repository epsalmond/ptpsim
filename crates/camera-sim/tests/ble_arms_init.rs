//! #102 acceptance: the BLE `IMAGE_TRANSFER_SETTING` write arms the PTP/IP engine
//! at runtime. A Wi-Fi-AP handoff that function-launches WITHOUT the prep write
//! leaves the engine unarmed, so it drops `InitCommandRequest` — modeling the real
//! GFX100 II (device 2026-06-28). The BLE responder and the engine share one
//! `CameraLink`; the responder's writes mutate it, the engine reads it.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use camera_config::index::{FamilyBleBlock, ModelView, ResolvedManufacturerIndex};
use camera_config::CameraManifest;
use camera_media_store::MediaStore;
use camera_sim::{walk_establishment, BleResponder, Engine};

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

/// A minimal engine (its manifest is irrelevant here — only the arming link is) and
/// a clone of its arming link to hand to a BLE responder.
fn engine_and_link() -> (Engine, camera_sim::SharedLink) {
    let manifest = CameraManifest::from_yaml(
        "schema: camera-config/v1\ncamera: { manufacturer: FUJIFILM, model: GFX100 II, firmware: \"2.30\" }\n",
    )
    .expect("minimal manifest loads");
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let root = std::env::temp_dir().join(format!("ptpsim-arm-{nanos}"));
    std::fs::create_dir_all(root.join("DCIM")).unwrap();
    let mut store = MediaStore::open(&root).unwrap();
    store.scan().unwrap();
    let engine = Engine::new(manifest, store);
    let link = engine.link();
    (engine, link)
}

#[test]
fn function_launch_without_the_prep_write_leaves_the_engine_unarmed() {
    let (engine, link) = engine_and_link();
    let view = gfx100ii();
    let ble = view.ble.as_ref().expect("ble block");
    let arm = uuid(ble, "imageTransferSetting");
    let launch = uuid(ble, "functionLaunchRequest");

    // A standalone camera is armed by default (the service's smoke path).
    assert!(engine.accepts_init(), "default (standalone) is armed");

    // An AP handoff that function-launches WITHOUT the prep write → unarmed.
    let mut responder = BleResponder::new([]).link_arming(Arc::clone(&link), &arm, &launch);
    responder.connect();
    responder.write(&launch, &[0x04, 0x00]).unwrap();
    assert!(
        !engine.accepts_init(),
        "launch without the IMAGE_TRANSFER_SETTING prep write must leave the engine unarmed (#102)"
    );

    // The prep write BEFORE the next launch re-arms it.
    responder.write(&arm, &[0x01]).unwrap();
    responder.write(&launch, &[0x04, 0x00]).unwrap();
    assert!(
        engine.accepts_init(),
        "the prep write before function-launch arms the session"
    );
}

#[test]
fn the_real_establish_plan_arms_the_engine() {
    let (engine, link) = engine_and_link();
    let view = gfx100ii();
    let ble = view.ble.as_ref().expect("ble block");
    let steps = &ble
        .establishment("ble-establish-wifi-ap")
        .expect("ble-establish-wifi-ap plan registered")
        .steps;

    let arm = uuid(ble, "imageTransferSetting");
    let launch = uuid(ble, "functionLaunchRequest");
    let ap_state = uuid(ble, "apState");
    let ssid = uuid(ble, "cameraSSIDNameString");
    let pass = uuid(ble, "cameraWiFiPassphraseString");

    let mut responder = BleResponder::new([ap_state.clone(), ssid.clone(), pass.clone()])
        .link_arming(Arc::clone(&link), &arm, &launch)
        .queue_notification(&ap_state, &[0x02, 0x80]) // Launching (transitional)
        .queue_notification(&ap_state, &[0x01, 0x80]) // Launched (0x8001)
        .serve_read(&ssid, b"GFX100II-1234")
        .serve_read(&pass, b"hunter2pass");

    let runtime_params: BTreeMap<String, String> = [("launchMode".to_string(), "4".to_string())]
        .into_iter()
        .collect();
    walk_establishment(
        &mut responder,
        steps,
        &BTreeMap::new(),
        &BTreeMap::new(),
        &runtime_params,
    )
    .expect("the establish-wifi-ap plan walks to completion");

    // The plan's IMAGE_TRANSFER_SETTING prep write (before function-launch) armed
    // the engine — so it will answer InitCommandRequest (#102 fix).
    assert!(
        engine.accepts_init(),
        "the canonical ble-establish-wifi-ap plan must arm the engine"
    );
}
