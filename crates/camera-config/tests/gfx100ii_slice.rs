//! Loads the REAL `camera-config-data` files (fuji.yaml + gfx100ii.yaml App slice)
//! and exercises the engine against them — the first validation of the schema on
//! actual derived data rather than in-crate fixtures.

use camera_config::{
    CameraManifest, ConfigStore, ManufacturerDefaults, PropView, ValuePolicy, VersionScheme,
};
use std::path::PathBuf;

fn data(rel: &str) -> String {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../packages/camera-config-data")
        .join(rel);
    std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("read {}: {e}", p.display()))
}

fn gfx() -> CameraManifest {
    CameraManifest::from_yaml(&data("fuji/gfx100ii/gfx100ii.yaml")).expect("gfx100ii.yaml loads")
}

#[test]
fn app_slice_loads_and_schema_is_supported() {
    let m = gfx();
    m.require_supported_schema().unwrap();
    assert_eq!(m.camera.model, "GFX100 II");
    // Evidence cites private client application paths → warnings, never load errors.
    assert!(m
        .validate()
        .iter()
        .all(|l| l.severity == camera_config::Severity::Warning));
}

#[test]
fn port_roles_match_the_shipping_app() {
    let m = gfx();
    let bind = m.connections["app"]
        .extra
        .get("bind")
        .expect("bind present");
    assert_eq!(bind["command"].as_u64(), Some(55740));
    assert_eq!(
        bind["event"].as_u64(),
        Some(55741),
        "event = command+1 per iOS source"
    );
    assert_eq!(
        bind["liveview"].as_u64(),
        Some(55742),
        "live-view stream = command+2"
    );
}

#[test]
fn mode_detect_from_function_mode() {
    let m = gfx();
    assert_eq!(
        m.detect_mode(&PropView::new().with(0xdf01, 0x16)),
        Some("Shooting/Stills")
    );
    assert_eq!(
        m.detect_mode(&PropView::new().with(0xdf01, 0x14)),
        Some("ImageTransfer")
    );
    assert_eq!(m.detect_mode(&PropView::new().with(0xdf01, 0x99)), None);
}

#[test]
fn live_view_entry_is_the_ground_truth_sequence() {
    let m = gfx();
    let entries = &m.connections["app"].entries;
    let lv = entries
        .iter()
        .find(|e| e.to == "Shooting/Stills" && e.from.is_none())
        .unwrap();
    let steps = &lv.steps;
    assert_eq!(steps[0].set_prop.as_deref(), Some("0xdf00"));
    assert_eq!(steps[0].value, Some(6));
    assert_eq!(steps[1].value, Some(0x16)); // functionMode 22
    assert_eq!(steps[2].read_echo.as_deref(), Some("0xdf2a"));
    assert_eq!(steps[3].repeat, 4); // 902B ×4
    assert_eq!(steps[4].send_op.as_deref(), Some("0x101c"));
    assert!(steps.iter().all(camera_config::Step::is_well_formed));
    // A from-qualified ImageTransfer edge exists (teardown-first switch).
    assert!(entries
        .iter()
        .any(|e| e.to == "ImageTransfer" && e.from.as_deref() == Some("Shooting/Stills")));
}

#[test]
fn capabilities_inherit_and_screen_takeover_is_modeled() {
    let m = gfx();
    let caps = m.capabilities("Shooting/Stills");
    assert!(caps.contains(&"exposureControl")); // inherited from Shooting
    assert!(caps.contains(&"liveView"));
    assert!(caps.contains(&"screenTakeover")); // distinguishes from the screen-on remote-trigger mode
}

#[test]
fn ble_connection_enables_app_and_carries_remote_trigger() {
    let m = gfx();
    let ble = &m.connections["ble"];
    assert_eq!(ble.kind.as_deref(), Some("ble"));
    // BLE is the establishment root: it brings up the App connection (the edge the
    // App slice's `establishment: ble-to-wifi-ap-v1` dangles on).
    let edge = ble
        .enables
        .iter()
        .find(|e| e.to == "app")
        .expect("BLE enables app");
    assert_eq!(edge.mechanism.as_deref(), Some("ble-to-wifi-ap-v1"));
    // BLE carries the RemoteTrigger mode.
    assert!(ble.modes.contains(&"RemoteTrigger".to_string()));
}

#[test]
fn remote_trigger_is_screen_on_and_transport_independent() {
    let m = gfx();
    let caps = m.capabilities("RemoteTrigger");
    assert!(caps.contains(&"shutterControl"));
    assert!(caps.contains(&"eepromTransfer"));
    assert!(caps.contains(&"screenOn")); // vs Shooting/Stills screenTakeover
                                         // No detect predicate: over BLE the mode is connection-implied, not PTP-detected.
    assert!(m.modes["RemoteTrigger"].detect.is_none());
}

#[test]
fn usb_modes_are_user_instruction_entries_and_ops_gate_to_usb() {
    let m = gfx();
    let usb = &m.connections["usb"];
    assert_eq!(usb.kind.as_deref(), Some("usb-ptp"));
    // USB sub-modes are camera-menu-selected → userInstruction edges, no PTP steps.
    let raw = usb
        .entries
        .iter()
        .find(|e| e.to == "RawConversion")
        .unwrap();
    assert!(raw.user_instruction.is_some());
    assert!(raw.steps.is_empty());

    // Vendor ops gate to (RawConversion, usb): available there, wrong-connection over app.
    let any = PropView::new();
    assert_eq!(
        m.operation_available("usb", "RawConversion", 0x900c, &any),
        camera_config::Availability::Available
    );
    assert_eq!(
        m.operation_available("app", "RawConversion", 0x900c, &any),
        camera_config::Availability::WrongConnection
    );
    // Backup ops gate to BackupRestore, not RawConversion.
    assert_eq!(
        m.operation_available("usb", "BackupRestore", 0x100c, &any),
        camera_config::Availability::Available
    );
    assert_eq!(
        m.operation_available("usb", "RawConversion", 0x100c, &any),
        camera_config::Availability::WrongMode
    );
}

#[test]
fn usb_evidence_is_the_lower_confidence_static_tier() {
    let m = gfx();
    assert_eq!(m.evidence["iosBLEReg"].kind, "ios-source");
}

#[test]
fn manufacturer_tier_supplies_fixed_initiator_identity() {
    let store = ConfigStore::new(gfx())
        .with_manufacturer(ManufacturerDefaults::from_yaml(&data("fuji/fuji.yaml")).unwrap());
    assert_eq!(store.version_scheme(), VersionScheme::DottedInt);
    // Identity is NOT in the body — it resolves from the manufacturer tier.
    assert!(!gfx().values.contains_key("initiatorGuid"));
    match store.value("initiatorGuid") {
        Some(ValuePolicy::Fixed { value }) => {
            assert_eq!(value.as_str(), Some("f2e4538fada5485d87b27f0bd3d5ded0"));
        }
        other => panic!("expected fixed initiator GUID, got {other:?}"),
    }
}
