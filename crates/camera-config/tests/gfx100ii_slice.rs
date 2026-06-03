//! Loads the REAL `camera-config-data` files (fuji.yaml + gfx100ii.yaml App slice)
//! and exercises the engine against them — the first validation of the schema on
//! actual derived data rather than in-crate fixtures.

use camera_config::{
    ActionVerb, CameraManifest, ConfigStore, ImagesPushed, ManufacturerDefaults, PropView,
    StepParam, ValuePolicy, VersionScheme,
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
        Some("shooting/stills")
    );
    assert_eq!(
        m.detect_mode(&PropView::new().with(0xdf01, 0x14)),
        Some("image-transfer")
    );
    assert_eq!(m.detect_mode(&PropView::new().with(0xdf01, 0x99)), None);
}

#[test]
fn live_view_entry_is_the_ground_truth_sequence() {
    let m = gfx();
    let entries = &m.connections["app"].entries;
    let lv = entries
        .iter()
        .find(|e| e.to == "shooting/stills" && e.from.is_none())
        .unwrap();
    let steps = &lv.steps;
    assert_eq!(steps[0].set_prop.as_deref(), Some("0xdf00"));
    assert_eq!(steps[0].value, Some(6));
    assert_eq!(steps[1].value, Some(0x16)); // functionMode 22
    assert_eq!(steps[2].read_echo.as_deref(), Some("0xdf2a"));
    assert_eq!(steps[3].repeat, 4); // 902B ×4
    assert_eq!(steps[4].send_op.as_deref(), Some("0x101c"));
    assert!(steps.iter().all(camera_config::Step::is_well_formed));
    // A from-qualified image-transfer edge exists (teardown-first switch).
    assert!(entries
        .iter()
        .any(|e| e.to == "image-transfer" && e.from.as_deref() == Some("shooting/stills")));
}

#[test]
fn capabilities_inherit_and_screen_takeover_is_modeled() {
    let m = gfx();
    let caps = m.capabilities("shooting/stills");
    assert!(caps.contains(&"exposureControl")); // inherited from shooting
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
    // BLE carries the remote-trigger mode.
    assert!(ble.modes.contains(&"remote-trigger".to_string()));
}

#[test]
fn remote_trigger_is_screen_on_and_transport_independent() {
    let m = gfx();
    let caps = m.capabilities("remote-trigger");
    assert!(caps.contains(&"shutterControl"));
    assert!(caps.contains(&"eepromTransfer"));
    assert!(caps.contains(&"screenOn")); // vs shooting/stills screenTakeover
                                         // No detect predicate: over BLE the mode is connection-implied, not PTP-detected.
    assert!(m.modes["remote-trigger"].detect.is_none());
}

#[test]
fn usb_mode_is_user_instruction_and_ops_gate_appropriately() {
    use camera_config::Availability::*;
    let m = gfx();
    let usb = &m.connections["usb"];
    assert_eq!(usb.kind.as_deref(), Some("usb-ptp"));
    // One on-camera USB mode (raw-conv + backup-restore), a userInstruction edge, no PTP steps.
    let entry = usb
        .entries
        .iter()
        .find(|e| e.to == "raw-conv-backup-restore")
        .unwrap();
    assert!(entry.user_instruction.is_some());
    assert!(entry.steps.is_empty());

    let any = PropView::new();
    // raw-conv op (0x900c) is MODE-specific: available in raw-conv-backup-restore,
    // wrong-mode elsewhere, wrong-connection off usb.
    assert_eq!(
        m.operation_available("usb", "raw-conv-backup-restore", 0x900c, &any),
        Available
    );
    assert_eq!(
        m.operation_available("usb", "shooting/stills", 0x900c, &any),
        WrongMode
    );
    assert_eq!(
        m.operation_available("app", "raw-conv-backup-restore", 0x900c, &any),
        WrongConnection
    );
    // backup op (0x100c) is available in ANY mode (modes: []), still usb-only.
    assert_eq!(
        m.operation_available("usb", "raw-conv-backup-restore", 0x100c, &any),
        Available
    );
    assert_eq!(
        m.operation_available("usb", "shooting/stills", 0x100c, &any),
        Available
    );
    assert_eq!(
        m.operation_available("app", "shooting/stills", 0x100c, &any),
        WrongConnection
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
        .contains(&"remote-trigger".to_string()));
    assert!(m.connections["wireless-tether"]
        .modes
        .contains(&"remote-trigger".to_string()));
    // remote-trigger is defined once (not duplicated per connection).
    assert!(m.modes.contains_key("remote-trigger"));
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
        m.operation_available("wireless-tether", "shooting/stills", 0x9018, &any),
        camera_config::Availability::Available
    );
    assert_eq!(
        m.operation_available("app", "shooting/stills", 0x9018, &any),
        camera_config::Availability::WrongConnection
    );
    // Image-transfer triad (wirePCSSShootDownload20260523): standard PTP ops
    // gated to the wireless-tether image-transfer mode. No 0x101B on PCSS.
    for op in [0x1007u16, 0x1008, 0x1009, 0x100a, 0x100b] {
        assert_eq!(
            m.operation_available("wireless-tether", "image-transfer", op, &any),
            camera_config::Availability::Available,
            "op 0x{op:04x} should be available on wireless-tether/image-transfer"
        );
    }
    assert_eq!(
        m.operation_available("wireless-tether", "image-transfer", 0x101b, &any),
        camera_config::Availability::WrongConnection,
        "0x101b GetPartialObject must NOT be authorized on wireless-tether"
    );
    // PCSS ISO is 0x500F, NOT 0xD02A (reference app path). Verify both controls land on
    // the right connection.
    assert!(
        m.control_for(0xd02a, "wireless-tether").is_none(),
        "0xD02A (reference app ISO) must not have a wireless-tether control"
    );
    let pcss_iso = m.control_for(0x500f, "wireless-tether").unwrap();
    assert_eq!(pcss_iso.set_method.as_deref(), Some("absolute"));
    assert_eq!(pcss_iso.operation.as_deref(), Some("0x1016"));
}

#[test]
fn wireless_tether_shutter_action_is_the_3_beat_pcss_sequence() {
    // Wire-confirmed by wirePCSSShootDownload20260523 (Hyper-Utility capture). The
    // 3 D039 phase values are 0x00010000, 0x00020000, 0x00000001 — each followed
    // by 0x100E(0, 0). triggers: [ImagePushed] tells the app to wire up the
    // receive handler BEFORE invoking the action.
    let m = gfx();
    let shutter = m
        .action("wireless-tether", ActionVerb::Shutter)
        .expect("wireless-tether.actions.shutter must exist");
    assert_eq!(shutter.mode, "shooting/stills");
    assert!(shutter.params.is_empty(), "shutter takes no runtime params");
    assert_eq!(shutter.steps.len(), 6, "3 beats × 2 ops each");
    let phase_values = [0x00010000_i64, 0x00020000, 0x00000001];
    for (beat, phase) in phase_values.iter().enumerate() {
        let setprop = &shutter.steps[beat * 2];
        assert_eq!(setprop.set_prop.as_deref(), Some("0xd039"));
        assert_eq!(setprop.value, Some(*phase), "beat {} phase value", beat + 1);
        let sendop = &shutter.steps[beat * 2 + 1];
        assert_eq!(sendop.send_op.as_deref(), Some("0x100e"));
        assert_eq!(
            sendop.params,
            vec![StepParam::Literal(0), StepParam::Literal(0)]
        );
    }
    // PCSS tether produces 1-3 images per press depending on the user's
    // JPEG / HEIF / RAW format selection — the manifest declares the bounded
    // range so the app sets receive timeouts + progress UI without knowing
    // which formats are selected.
    assert_eq!(shutter.triggers.len(), 1);
    let t = &shutter.triggers[0];
    assert!(t.is_well_formed());
    assert_eq!(t.images_pushed, Some(ImagesPushed { min: 1, max: 3 }));
    assert!(t.postview_event.is_none());
    assert!(t.live_view_stream.is_none());
}

#[test]
fn wireless_tether_transfer_actions_bind_runtime_handle() {
    // Per-handle ops are parameterized: caller binds `handle` to a slot the
    // engine plugs into the StepParam::Runtime reference at emit time.
    let m = gfx();
    for verb in [
        ActionVerb::GetObjectInfo,
        ActionVerb::GetThumb,
        ActionVerb::GetObject,
        ActionVerb::DeleteObject,
    ] {
        let a = m
            .action("wireless-tether", verb)
            .unwrap_or_else(|| panic!("missing action {verb:?}"));
        assert_eq!(a.mode, "image-transfer");
        assert_eq!(a.params, vec!["handle".to_string()]);
        assert_eq!(a.steps.len(), 1);
        assert_eq!(
            a.steps[0].params,
            vec![StepParam::Runtime {
                runtime: "handle".into()
            }],
            "{verb:?} must bind `handle`"
        );
        assert!(
            a.triggers.is_empty(),
            "{verb:?} has no declared side-effects"
        );
    }
    // enumerateObjects takes no params and seeds the handle list the others consume.
    let enumerate = m
        .action("wireless-tether", ActionVerb::EnumerateObjects)
        .unwrap();
    assert!(enumerate.params.is_empty());
    assert_eq!(enumerate.steps[0].send_op.as_deref(), Some("0x1007"));
    assert_eq!(
        enumerate.steps[0].params,
        vec![StepParam::Literal(0xffffffff), StepParam::Literal(0)]
    );
}

#[test]
fn action_query_misses_when_connection_does_not_declare_the_verb() {
    // The closed ActionVerb enum gates new verbs at the schema layer. A
    // connection that doesn't declare an action for a given verb returns None
    // — the client surfaces "not supported on this transport" without having
    // to encode the negative list itself.
    let m = gfx();
    // app connection has no actions block (yet) — Shutter resolves to None.
    assert!(m.action("app", ActionVerb::Shutter).is_none());
    // ble has no transfer actions.
    assert!(m.action("ble", ActionVerb::GetObject).is_none());
    // Unknown connection name returns None too.
    assert!(m.action("nonexistent", ActionVerb::Shutter).is_none());
}

#[test]
fn scaffold_props_are_tagged_so_clients_can_filter_them_out_of_settings_ui() {
    // 0xD039 / 0xD21C / 0xD207 LOOK settable on the wire but are protocol
    // scaffolding (virtual-shutter state machine + tethered keepalives) —
    // wirePCSSShootDownload20260523. `kind: scaffold` lets clients filter
    // them from settings UI without re-deriving the negative list each time.
    let m = gfx();
    for code in ["0xd039", "0xd21c", "0xd207"] {
        let p = m
            .properties
            .get(code)
            .unwrap_or_else(|| panic!("property {code} should exist"));
        assert_eq!(
            p.kind.as_deref(),
            Some("scaffold"),
            "{code} must carry kind: scaffold"
        );
    }
    // Real settings should NOT be tagged scaffold.
    assert!(
        m.properties["0x5007"].kind.is_none(),
        "aperture is a real setting"
    );
    assert!(
        m.properties["0x500f"].kind.is_none(),
        "PCSS ISO is a real setting"
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
        m.operation_available("app", "shooting/stills", 0x9026, &any),
        camera_config::Availability::Available
    );
    assert_eq!(
        m.operation_available("app", "shooting/stills", 0x100e, &any),
        camera_config::Availability::Available
    );
    assert_eq!(
        m.operation_available("app", "image-transfer", 0x1008, &any),
        camera_config::Availability::Available
    );
    assert_eq!(
        m.operation_available("app", "image-transfer", 0x101b, &any),
        camera_config::Availability::Available
    );
    assert!(m.connections["app"]
        .entries
        .iter()
        .all(|e| e.to != "Shooting/Video"));
    assert!(m.connections["app"]
        .entries
        .iter()
        .all(|e| !(e.from.as_deref() == Some("image-transfer") && e.to == "shooting/stills")));
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
fn generator_ingests_real_probe_evidence_into_a_proposal() {
    // Concatenate the committed camera-config-evidence/v1 probe files and run the generator.
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../packages/camera-config-data/fuji/gfx100ii/evidence/probe");
    let mut jsonl = String::new();
    let mut files = 0;
    for entry in std::fs::read_dir(&dir).expect("probe dir") {
        let p = entry.unwrap().path();
        if p.extension().is_some_and(|e| e == "jsonl") {
            jsonl.push_str(&std::fs::read_to_string(&p).unwrap());
            jsonl.push('\n');
            files += 1;
        }
    }
    assert!(files >= 8, "expected the 8 probe files, got {files}");

    let m = camera_config::generate_proposal(&jsonl);
    m.require_supported_schema()
        .expect("proposal uses the current schema");

    // Identity derived from the evidence.
    assert_eq!(m.camera.model, "GFX100 II");
    assert_eq!(m.camera.firmware, "2.30");
    // Both probed connections + the hierarchical modes show up as bare nodes.
    assert!(m.connections.contains_key("usb"));
    assert!(m.connections.contains_key("wireless-tether"));
    assert!(m.modes.contains_key("shooting/stills"));
    assert!(m.modes.contains_key("shooting/video"));
    // Substantial op/prop coverage from the enumeration.
    assert!(m.operations.len() >= 20, "ops: {}", m.operations.len());
    assert!(m.properties.len() >= 50, "props: {}", m.properties.len());
    // GetDevicePropDesc (0x1014) was exercised across scopes → multi-connection gating.
    let dpd = &m.operations["0x1014"];
    assert!(dpd.connections.contains(&"usb".to_string()));
    assert!(dpd.connections.contains(&"wireless-tether".to_string()));
    // The generator emits NO sequences (preludes/chords are curated, not probed).
    assert!(m.connections.values().all(|c| c.entries.is_empty()));
    // Properties are camera-sourced (GetDevicePropDesc).
    assert!(m
        .properties
        .values()
        .filter_map(|p| p.descriptor.as_ref())
        .all(|d| d.source == Some(camera_config::ValueSource::Camera)));
}

#[test]
fn image_import_entry_uses_tolerant_params_and_runtime_slot() {
    let m = gfx();
    let entries = &m.connections["app"].entries;
    // Cold entry: tolerant preamble + vendor-prime op with literal params.
    let cold = entries
        .iter()
        .find(|e| e.to == "image-transfer" && e.from.is_none())
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
        .find(|e| e.to == "image-transfer" && e.from.as_deref() == Some("shooting/stills"))
        .unwrap();
    assert_eq!(from.steps[0].send_op.as_deref(), Some("0x1018"));
    assert_eq!(
        from.steps[0].params,
        vec![StepParam::Runtime {
            runtime: "openCaptureTxId".into()
        }]
    );
    // reference app Take→Get switch re-establishes the PTP/IP session before DF01=0x14.
    // D3-wire 2026-06-02 confirmed parameterless verb; identity is reused.
    assert!(
        from.steps[1].reopen_session.is_some(),
        "reopenSession must come right after the 0x1018 in the from-LV image-transfer entry"
    );
    assert!(
        from.steps[1].is_well_formed(),
        "reopenSession step carries no other action fields"
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
