//! Loads the REAL `camera-config-data` files (fuji.yaml + gfx100ii.yaml App slice)
//! and exercises the engine against them — the first validation of the schema on
//! actual derived data rather than in-crate fixtures.

use camera_config::{
    CameraManifest, ConfigStore, ManufacturerDefaults, PropView, StepParam, ValuePolicy,
    VersionScheme,
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
fn remote_trigger_is_reachable_over_both_ble_and_wireless_tether() {
    // The transport-independence payoff: ONE mode node, two connections.
    let m = gfx();
    assert!(m.connections["ble"]
        .modes
        .contains(&"RemoteTrigger".to_string()));
    assert!(m.connections["wireless-tether"]
        .modes
        .contains(&"RemoteTrigger".to_string()));
    // RemoteTrigger is defined once (not duplicated per connection).
    assert!(m.modes.contains_key("RemoteTrigger"));
}

#[test]
fn wireless_tether_is_wire_confirmed_and_uses_absolute_big3() {
    let m = gfx();
    let wt = &m.connections["wireless-tether"];
    assert_eq!(wt.kind.as_deref(), Some("ptpip-direct"));
    assert_eq!(wt.establishment.as_deref(), Some("pcss-knock-v1"));
    assert_eq!(m.evidence["wireTether"].kind, "wire-capture");
    // Big-3 control mechanism is per-connection: absolute over the tether.
    let ap = m.control_for(0x5007, "wireless-tether").unwrap();
    assert_eq!(ap.set_method.as_deref(), Some("absolute"));
    assert_eq!(ap.operation.as_deref(), Some("0x1016"));
    // 0x9018 live-view gates to the tether; wrong-connection over app.
    let any = PropView::new();
    assert_eq!(
        m.operation_available("wireless-tether", "Shooting/Stills", 0x9018, &any),
        camera_config::Availability::Available
    );
    assert_eq!(
        m.operation_available("app", "Shooting/Stills", 0x9018, &any),
        camera_config::Availability::WrongConnection
    );
}

#[test]
fn app_current_behavior_ops_and_controls_are_modeled() {
    let m = gfx();
    let any = PropView::new();

    // Existing app live-view controls: ISO is direct SetDevicePropValue, the
    // ring controls are vendor-step ops with 0xd212 readback.
    let iso = m.control_for(0xd02a, "app").unwrap();
    assert_eq!(iso.set_method.as_deref(), Some("absolute"));
    assert_eq!(iso.operation.as_deref(), Some("0x1016"));
    assert_eq!(iso.readback.as_deref(), Some("0xd212"));
    let shutter = m.control_for(0xd240, "app").unwrap();
    assert_eq!(shutter.set_method.as_deref(), Some("vendorStep"));
    assert_eq!(shutter.operation.as_deref(), Some("0x902c"));
    let aperture = m.control_for(0x5007, "app").unwrap();
    assert_eq!(aperture.operation.as_deref(), Some("0x902d"));
    let ev = m.control_for(0x5010, "app").unwrap();
    assert_eq!(ev.operation.as_deref(), Some("0x902e"));

    // Existing app operations are available over the app connection in their
    // current modes, and do not imply the new video/transfer-back flows.
    assert_eq!(
        m.operation_available("app", "Shooting/Stills", 0x9026, &any),
        camera_config::Availability::Available
    );
    assert_eq!(
        m.operation_available("app", "Shooting/Stills", 0x100e, &any),
        camera_config::Availability::Available
    );
    assert_eq!(
        m.operation_available("app", "ImageTransfer", 0x1008, &any),
        camera_config::Availability::Available
    );
    assert_eq!(
        m.operation_available("app", "ImageTransfer", 0x101b, &any),
        camera_config::Availability::Available
    );
    assert!(m.connections["app"]
        .entries
        .iter()
        .all(|e| e.to != "Shooting/Video"));
    assert!(m.connections["app"]
        .entries
        .iter()
        .all(|e| !(e.from.as_deref() == Some("ImageTransfer") && e.to == "Shooting/Stills")));
}

#[test]
fn xlv_models_protocol_shape_with_access_gate_kept_private() {
    let m = gfx();
    let xlv = &m.connections["xlv"];
    assert_eq!(xlv.kind.as_deref(), Some("http-xlv"));
    // Wire-confirmed routes present.
    let routes = xlv.extra.get("routes").expect("routes");
    assert!(routes.get("GET /camera/functions/{code}/get").is_some());
    // Bearer auth EXISTS (public shape) but the token source is a private overlay —
    // the JWT forging must never land in the public data repo.
    let auth = xlv.extra.get("auth").expect("auth");
    assert_eq!(auth["scheme"].as_str(), Some("bearer"));
    assert_eq!(auth["tokenSource"].as_str(), Some("private-overlay"));
    // The public file carries no secret/forging material.
    let raw = data("fuji/gfx100ii/gfx100ii.yaml").to_lowercase();
    assert!(!raw.contains("forge") && !raw.contains("jwt") && !raw.contains("secret"));
}

#[test]
fn image_import_entry_uses_tolerant_params_and_runtime_slot() {
    let m = gfx();
    let entries = &m.connections["app"].entries;
    // Cold entry: tolerant preamble + vendor-prime op with literal params.
    let cold = entries
        .iter()
        .find(|e| e.to == "ImageTransfer" && e.from.is_none())
        .unwrap();
    assert!(cold
        .steps
        .iter()
        .any(|s| s.get_prop.as_deref() == Some("0xd212") && s.tolerant));
    assert!(cold
        .steps
        .iter()
        .any(|s| { s.set_prop.as_deref() == Some("0xdf28") && s.value == Some(3) && s.tolerant }));
    assert!(cold
        .steps
        .iter()
        .any(|s| { s.set_prop.as_deref() == Some("0xd226") && s.value == Some(0) && s.tolerant }));
    assert!(cold
        .steps
        .iter()
        .any(|s| { s.set_prop.as_deref() == Some("0xd227") && s.value == Some(0) && s.tolerant }));
    assert!(cold
        .steps
        .iter()
        .any(|s| s.get_prop.as_deref() == Some("0xd244") && s.tolerant));
    let prime = cold
        .steps
        .iter()
        .find(|s| s.send_op.as_deref() == Some("0x9053"))
        .unwrap();
    assert_eq!(
        prime.params,
        vec![StepParam::Literal(0), StepParam::Literal(0x7530)]
    );
    assert!(prime.tolerant);
    // from-live-view entry binds the runtime open-capture txid into 0x1018.
    let from = entries
        .iter()
        .find(|e| e.to == "ImageTransfer" && e.from.as_deref() == Some("Shooting/Stills"))
        .unwrap();
    assert_eq!(from.steps[0].send_op.as_deref(), Some("0x1018"));
    assert_eq!(
        from.steps[0].params,
        vec![StepParam::Runtime {
            runtime: "openCaptureTxId".into()
        }]
    );
    assert!(from
        .steps
        .iter()
        .any(|s| { s.set_prop.as_deref() == Some("0xd226") && s.value == Some(0) && s.tolerant }));
}

#[test]
fn fw_overlay_flips_xlv_to_https_field_level() {
    // Baseline body: XLV over plain HTTP:80.
    let base = gfx();
    let xlv = &base.connections["xlv"];
    let t = xlv.extra.get("transport").unwrap();
    assert_eq!(t["scheme"].as_str(), Some("http"));
    assert_eq!(t["port"].as_u64(), Some(80));

    // Merge the fw2.40 overlay → only transport flips; routes/auth/modes inherited.
    let merged = CameraManifest::from_tiers(
        &data("fuji/gfx100ii/gfx100ii.yaml"),
        &[&data("fuji/gfx100ii/fw2.40.yaml")],
    )
    .expect("tiers merge");
    assert_eq!(merged.camera.firmware, "2.40");
    let xlv2 = &merged.connections["xlv"];
    let t2 = xlv2.extra.get("transport").unwrap();
    assert_eq!(t2["scheme"].as_str(), Some("https"), "fw2.40 → HTTPS");
    assert_eq!(t2["port"].as_u64(), Some(443));
    assert_eq!(
        xlv2.extra.get("tls").unwrap()["mode"].as_str(),
        Some("self-signed")
    );
    // Inherited (not restated in the overlay): the routes + bearer auth survive.
    assert!(xlv2.extra.contains_key("routes"));
    assert!(xlv2.extra.contains_key("auth"));
    // Other connections untouched by the fw overlay.
    assert!(merged.connections.contains_key("app"));
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
