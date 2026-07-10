//! Loads the REAL `camera-config-data` files (fuji.yaml + gfx100ii.yaml App slice)
//! and exercises the engine against them — the first validation of the schema on
//! actual derived data rather than in-crate fixtures.

use camera_config::{
    ActionVerb, CameraManifest, ConfigStore, ManufacturerDefaults, ObjectsAvailable, Predicate,
    PropView, PropertyKind, StepParam, ValuePolicy, VersionScheme,
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

fn assert_image_import_bootstrap_gate(steps: &[camera_config::Step]) {
    let start = steps
        .iter()
        .position(|s| s.starts_gate.as_deref() == Some("imageImportBootstrap"))
        .expect("bootstrap gate starts");
    let complete = steps
        .iter()
        .position(|s| s.completes_gate.as_deref() == Some("imageImportBootstrap"))
        .expect("bootstrap gate completes");
    assert!(start < complete, "gate start precedes completion");
    assert_eq!(steps[start].get_prop.as_deref(), Some("0xd212"));
    assert_eq!(steps[complete].get_prop.as_deref(), Some("0xd212"));
    let d22b = steps[start..=complete]
        .iter()
        .position(|s| s.get_prop.as_deref() == Some("0xd22b"))
        .map(|i| start + i)
        .expect("bootstrap reads D22B");
    let page = steps[start..=complete]
        .iter()
        .position(|s| s.send_op.as_deref() == Some("0x9053"))
        .map(|i| start + i)
        .expect("bootstrap sends 0x9053");
    assert!(d22b < page, "D22B read precedes 0x9053 page op");
    assert_eq!(
        steps[page].params,
        vec![StepParam::Literal(0), StepParam::Literal(0x7530)]
    );
}

#[test]
fn app_slice_loads_and_schema_is_supported() {
    let m = gfx();
    m.require_supported_schema().unwrap();
    assert_eq!(m.camera.model, "GFX100 II");
    // Evidence may cite paths outside ptpsim → warnings, never load errors.
    assert!(m
        .validate()
        .iter()
        .all(|l| l.severity == camera_config::Severity::Warning));
}

#[test]
fn port_roles_match_the_shipping_app() {
    use camera_config::SocketRole;
    let m = gfx();
    let app = &m.connections["app"];
    let b = app
        .bindings
        .as_ref()
        .expect("app declares typed socket bindings");
    assert_eq!(b.command, 55740);
    assert_eq!(b.event, Some(55741), "event = command+1 per iOS source");
    assert_eq!(b.live_view, Some(55742), "live-view stream = command+2");
    // Resolve by role — the accessor the FFI `port_for_role` calls.
    assert_eq!(b.port_for(SocketRole::Command), Some(55740));
    assert_eq!(b.port_for(SocketRole::Event), Some(55741));
    assert_eq!(b.port_for(SocketRole::LiveView), Some(55742));
    // The transport-close frame names a manifest-owned byte sentinel.
    assert_eq!(
        camera_config::parse_hex_bytes(&m.sentinels["keepApSentinel"].bytes),
        Some(vec![0x08, 0, 0, 0, 0xff, 0xff, 0xff, 0xff])
    );
    let tc = app
        .transport_close
        .as_ref()
        .expect("app declares a transport-close");
    assert_eq!(tc.sentinel, "keepApSentinel");
    assert_eq!(tc.when.as_deref(), Some("before-image-transfer-reopen"));
}

#[test]
fn camera_initiated_transfer_references_are_complete() {
    let manifest = gfx();
    let transfer = manifest
        .camera_initiated_transfer
        .as_ref()
        .expect("camera declares its reserved transfer queue");
    assert_eq!(transfer.handoff.connection, "app");
    assert_eq!(transfer.receive.mode, "reserved-photo-receive");
    assert_eq!(transfer.receive.head_index, 1);
    assert_eq!(transfer.receive.count.property, "0xd212");
    assert_eq!(transfer.receive.count.member, "0xdf41");
    assert_eq!(
        transfer.receive.metadata.phases,
        vec![
            camera_config::CameraInitiatedMetadataPhase::AfterCountBeforeModeEntry,
            camera_config::CameraInitiatedMetadataPhase::AfterModeEntry,
        ]
    );
    assert_eq!(transfer.receive.metadata.operation, "0x1008");
    assert_eq!(transfer.receive.data.operation, "0x101b");
    assert_eq!(transfer.receive.data.chunk_limit_property, "0xd235");
    assert_eq!(
        manifest.connections["app"]
            .bindings
            .as_ref()
            .and_then(|bindings| bindings.host.as_deref()),
        Some("192.168.0.1")
    );
    assert!(
        manifest
            .validate()
            .iter()
            .all(|lint| !lint.message.contains("cameraInitiatedTransfer")),
        "camera-initiated transfer has no structural lints"
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
    // Device-validated (#39): the GFX100 II rejects this advisory write
    // with 0x201d, so the step MUST stay tolerant or mode entry dies on
    // real hardware. This flag regressed silently once (client application #4) —
    // hence the explicit assert.
    assert!(steps[0].tolerant, "0xdf00 write must be tolerant");
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
    // App slice's `establishment: ble-establish-wifi-ap` dangles on).
    let edge = ble
        .enables
        .iter()
        .find(|e| e.to == "app")
        .expect("BLE enables app");
    assert_eq!(edge.mechanism.as_deref(), Some("ble-establish-wifi-ap"));
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
    assert_eq!(wt.establishment.as_deref(), Some("pcss-knock"));
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
    assert_eq!(
        m.operation_available("wireless-tether", "shooting/stills", 0x1015, &any),
        camera_config::Availability::Available,
        "wireless-tether uses GetDevicePropValue for PCSS readbacks"
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
    // by 0x100E(0, 0). triggers: [ObjectsAvailable] tells the app how many
    // queued objects to expect after invoking the action.
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
    assert_eq!(
        t.objects_available,
        Some(ObjectsAvailable { min: 1, max: 3 })
    );
    assert!(t.postview_event.is_none());
    assert!(t.live_view_stream.is_none());
}

#[test]
fn wireless_tether_keepalive_action_is_session_scaffold_not_settings() {
    let m = gfx();
    let keepalive = m
        .action("wireless-tether", ActionVerb::Keepalive)
        .expect("wireless-tether.actions.keepalive must exist");
    assert_eq!(keepalive.mode, "");
    assert!(keepalive.params.is_empty());
    assert!(keepalive.triggers.is_empty());
    assert_eq!(keepalive.steps.len(), 2);
    assert_eq!(keepalive.steps[0].set_prop.as_deref(), Some("0xd21c"));
    assert_eq!(keepalive.steps[0].value, Some(0));
    assert_eq!(keepalive.steps[1].set_prop.as_deref(), Some("0xd207"));
    assert_eq!(keepalive.steps[1].value, Some(1));
    assert_eq!(m.properties["0xd21c"].kind, PropertyKind::Scaffold);
    assert_eq!(m.properties["0xd207"].kind, PropertyKind::Scaffold);
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
                runtime: "handle".into(),
                shift: 0,
                mask: None,
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
    // ble has no transfer actions.
    assert!(m.action("ble", ActionVerb::GetObject).is_none());
    // Unknown connection name returns None too.
    assert!(m.action("nonexistent", ActionVerb::Shutter).is_none());
    // reference app `app` does NOT model DeleteObject (no wire-truth) — verb-level
    // miss without polluting the negative list with explicit entries.
    assert!(m.action("app", ActionVerb::DeleteObject).is_none());
}

#[test]
fn app_shutter_action_scripts_the_postview_await_take_cycle() {
    // reference app shutter cycle (#29, MODE_CHANGES.md §6b + FUJI_PTP_PROP_REFERENCE §6):
    // 0x100E InitiateCapture(0,0) → awaitUntil the PostviewComplete event (0xC001,
    // JPEG saved to card) → 0x9022 cleanup/postview read. The await makes the take
    // cycle manifest-scripted instead of client responsibility.
    let m = gfx();
    let shutter = m
        .action("app", ActionVerb::Shutter)
        .expect("app.actions.shutter must exist");
    assert_eq!(shutter.mode, "shooting/stills");
    assert!(shutter.params.is_empty());
    assert_eq!(shutter.steps.len(), 3);
    assert_eq!(shutter.steps[0].send_op.as_deref(), Some("0x100e"));
    assert_eq!(
        shutter.steps[0].params,
        vec![StepParam::Literal(0), StepParam::Literal(0)]
    );
    // The middle step waits for the camera's postview event (arrival alone gates
    // the read — the 0x9022 below is the data read).
    let aw = shutter.steps[1]
        .await_until
        .as_ref()
        .expect("postview await step between capture and cleanup");
    match &aw.source {
        camera_config::AwaitSource::Event { code, then_poll } => {
            assert_eq!(code, "0xc001");
            assert!(then_poll.is_none());
        }
        other => panic!("expected an event source, got {other:?}"),
    }
    assert_eq!(shutter.steps[2].send_op.as_deref(), Some("0x9022"));
    assert!(shutter
        .steps
        .iter()
        .all(camera_config::Step::is_well_formed));
    assert!(shutter.triggers[0].postview_event.is_some());
    // 0x100E emits 0xC001 so the simulator pushes the event the await consumes.
    assert!(m.operations["0x100e"].emits.iter().any(|e| e == "0xc001"));
}

#[test]
fn app_transfer_actions_use_app_specific_wire_shape() {
    // reference app differs from PCSS on the transfer path (IMAGE_TRANSFER_FW230.md):
    // (1) enumeration reads 0xD620/0xD621 PROPERTIES (camera rejects 0x1007),
    // (2) getObject is CHUNKED via 0x101B with handle/offset/length params
    //     (not whole-object 0x1009 like PCSS).
    let m = gfx();

    // Enumeration: two getProps, no sendOp, no runtime params.
    let enumerate = m
        .action("app", ActionVerb::EnumerateObjects)
        .expect("app.actions.enumerateObjects");
    assert_eq!(enumerate.mode, "image-transfer");
    assert!(enumerate.params.is_empty());
    assert_eq!(enumerate.steps.len(), 2);
    assert_eq!(enumerate.steps[0].get_prop.as_deref(), Some("0xd620"));
    assert_eq!(enumerate.steps[1].get_prop.as_deref(), Some("0xd621"));
    assert!(enumerate.triggers.is_empty());

    // Per-handle metadata + thumbnail: standard PTP, same wire shape as PCSS.
    for verb in [ActionVerb::GetObjectInfo, ActionVerb::GetThumb] {
        let a = m
            .action("app", verb)
            .unwrap_or_else(|| panic!("missing action {verb:?}"));
        assert_eq!(a.mode, "image-transfer");
        assert_eq!(a.params, vec!["handle".to_string()]);
        assert_eq!(a.steps.len(), 1);
        assert_eq!(
            a.steps[0].params,
            vec![StepParam::Runtime {
                runtime: "handle".into(),
                shift: 0,
                mask: None,
            }]
        );
    }

    // Chunked download — three runtime slots (handle / offset / length).
    let get = m
        .action("app", ActionVerb::GetObject)
        .expect("app.actions.getObject");
    assert_eq!(get.mode, "image-transfer");
    assert_eq!(
        get.params,
        vec![
            "handle".to_string(),
            "offset".to_string(),
            "length".to_string()
        ],
        "reference app getObject is chunked — caller binds offset+length per iteration"
    );
    assert_eq!(get.steps.len(), 1);
    assert_eq!(get.steps[0].send_op.as_deref(), Some("0x101b"));
    assert_eq!(
        get.steps[0].params,
        vec![
            StepParam::Runtime {
                runtime: "handle".into(),
                shift: 0,
                mask: None,
            },
            StepParam::Runtime {
                runtime: "offset".into(),
                shift: 0,
                mask: Some(0xffff_ffff),
            },
            StepParam::Runtime {
                runtime: "length".into(),
                shift: 0,
                mask: None,
            },
            StepParam::Runtime {
                runtime: "offset".into(),
                shift: 32,
                mask: None,
            },
        ]
    );
}

#[test]
fn getobject_params_differ_per_connection_same_verb() {
    // The closed ActionVerb vocabulary supports same-verb-different-shape
    // across transports: PCSS getObject is whole-object (`[handle]`),
    // reference app getObject is chunked (`[handle, offset, length]`) and emits a
    // four-param GetPartialObject with low/high offset words. Clients
    // introspect `.params` to know what to bind at the call site.
    let m = gfx();
    let pcss = m
        .action("wireless-tether", ActionVerb::GetObject)
        .expect("wireless-tether.actions.getObject");
    let app = m
        .action("app", ActionVerb::GetObject)
        .expect("app.actions.getObject");
    assert_eq!(pcss.params.len(), 1, "PCSS getObject is whole-object");
    assert_eq!(app.params.len(), 3, "reference app getObject is chunked");
    assert_eq!(pcss.steps[0].send_op.as_deref(), Some("0x1009"));
    assert_eq!(pcss.steps[0].params.len(), 1);
    assert_eq!(app.steps[0].send_op.as_deref(), Some("0x101b"));
    assert_eq!(
        app.steps[0].params.len(),
        4,
        "reference app derives offset_high from the logical offset slot for the wire call"
    );
    assert_eq!(
        app.steps[0].params[1],
        StepParam::Runtime {
            runtime: "offset".into(),
            shift: 0,
            mask: Some(0xffff_ffff),
        }
    );
    assert_eq!(
        app.steps[0].params[3],
        StepParam::Runtime {
            runtime: "offset".into(),
            shift: 32,
            mask: None,
        }
    );
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
            p.kind,
            PropertyKind::Scaffold,
            "{code} must carry kind: scaffold"
        );
    }
    assert_eq!(m.properties["0x5007"].kind, PropertyKind::Setting);
    assert_eq!(m.properties["0x500f"].kind, PropertyKind::Setting);
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
    let focus = &m.properties["0x500a"];
    assert_eq!(focus.ptype.as_deref(), Some("u16"));
    assert_eq!(focus.access.as_deref(), Some("readWrite"));

    assert!(m.connections["app"]
        .modes
        .iter()
        .any(|mode| mode == "shooting/video"));
    assert_eq!(m.properties["0xdf2a"].ptype.as_deref(), Some("u32"));
    let d246 = &m.properties["0xd246"];
    assert_eq!(d246.ptype.as_deref(), Some("u8"));
    assert_eq!(d246.access.as_deref(), Some("readWrite"));
    assert_eq!(d246.initial_value, Some(0));
    let d246_desc = d246.descriptor.as_ref().expect("D246 descriptor");
    assert_eq!(d246_desc.form, "enum");
    assert_eq!(d246_desc.values, vec![0, 1]);
    assert_eq!(d246.labels["0"], "stills");
    assert_eq!(d246.labels["1"], "video");

    // Existing app operations are available over the app connection in their
    // current modes, and the live snapshot remains available after D246 enters
    // shooting/video.
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
    assert_eq!(
        m.operation_available("app", "shooting/video", 0x902b, &any),
        camera_config::Availability::Available
    );
}

#[test]
fn af_tap_ops_and_props_are_ingested_from_the_wire_doc() {
    // Issue #35: the AF tap / S1-lock surface from PTP_PROPERTIES_REFERENCE.md §5.
    // DATA-only ingestion — names/access/owner/gating must match the wire doc; the
    // camera-side AF color flip is curated as an op-effect so the simulator can
    // round-trip the poll-until flow against the real manifest.
    let m = gfx();
    let any = PropView::new();

    // 0x9026 LockS1Lock (tap-to-AF) — fuji-vendor, matching its 0x90xx siblings.
    let lock = &m.operations["0x9026"];
    assert_eq!(lock.name, "LockS1Lock");
    assert_eq!(lock.owner, "fuji-vendor");
    assert_eq!(
        m.operations["0x902c"].owner, lock.owner,
        "owner matches siblings"
    );
    // 0x9027 UnlockS1Lock — companion release op (name PRELIMINARY per §5.1).
    let unlock = &m.operations["0x9027"];
    assert_eq!(unlock.name, "UnlockS1Lock");
    assert_eq!(unlock.owner, "fuji-vendor");
    assert_eq!(lock.effects.len(), 2);
    assert_eq!(lock.effects[0].set_prop, "0xd209");
    assert_eq!(lock.effects[0].value, 1);
    // #185: settle 2 models the measured fw02.30 latency — 0xC005 fires before
    // 0xD209 latches, and the event-source await re-polls to absorb it (the
    // former #42 "settle ≤1 event-coupling invariant" is retired; client application#157).
    assert_eq!(lock.effects[0].settle_after_polls, 2);
    // #96: the second effect mirrors the packed AF-area request param into 0xD17C
    // (§5.5) — a param-derived value (fromParam index 0, identity copy, immediate).
    assert_eq!(lock.effects[1].set_prop, "0xd17c");
    let src = lock.effects[1]
        .from_param
        .as_ref()
        .expect("0xd17c effect derives its value from the request param");
    assert_eq!(src.index, 0);
    assert_eq!(src.shift, 0);
    assert!(src.mask.is_none(), "identity copy — no bit-slice");
    assert_eq!(
        lock.effects[1].settle_after_polls, 0,
        "0xD17C updates immediately on the tap"
    );
    assert!(
        lock.emits.iter().any(|e| e == "0xc005"),
        "0x9026 emits AFCAPTUER"
    );
    assert_eq!(
        m.events["0xc005"].name, "AFCAPTUER",
        "the AF completion event is declared"
    );
    assert_eq!(unlock.effects.len(), 1);
    assert_eq!(unlock.effects[0].set_prop, "0xd209");
    assert_eq!(unlock.effects[0].value, 0);
    assert_eq!(unlock.effects[0].settle_after_polls, 0);
    // Both gate to the app connection in shooting/stills, and only there.
    for op in [0x9026u16, 0x9027] {
        assert_eq!(
            m.operation_available("app", "shooting/stills", op, &any),
            camera_config::Availability::Available,
            "op 0x{op:04x} available on app/shooting-stills"
        );
        assert_eq!(
            m.operation_available("wireless-tether", "shooting/stills", op, &any),
            camera_config::Availability::WrongConnection,
            "op 0x{op:04x} is app-only (reference app tap-to-AF path)"
        );
    }

    // 0xD17C S1_LOCK — AF area state; high bytes also encode aspect ratio (§5.1).
    let s1 = &m.properties["0xd17c"];
    assert_eq!(s1.name, "s1Lock");
    assert_eq!(s1.access.as_deref(), Some("readOnly"));
    // 0xD209 S1_LOCK_COLOR — 0=white/none, 1=green/locked, 2=red/failed (§5.3).
    let color = &m.properties["0xd209"];
    assert_eq!(color.name, "s1LockColor");
    assert_eq!(color.ptype.as_deref(), Some("u16"));
    assert_eq!(color.access.as_deref(), Some("readOnly"));

    // All four cite the in-repo wire doc (docLiveControls → PTP_PROPERTIES_REFERENCE.md).
    assert_eq!(m.evidence["docLiveControls"].kind, "doc");
    for ev in [&lock.evidence, &unlock.evidence] {
        assert!(ev.contains(&"docLiveControls".to_string()));
    }
    for p in [s1, color] {
        assert!(p.evidence.contains(&"docLiveControls".to_string()));
    }
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
    assert_image_import_bootstrap_gate(&cold.steps);
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
    assert_image_import_bootstrap_gate(&from.steps);
    assert_eq!(from.steps[0].send_op.as_deref(), Some("0x1018"));
    assert_eq!(
        from.steps[0].params,
        vec![StepParam::Runtime {
            runtime: "openCaptureTxId".into(),
            shift: 0,
            mask: None,
        }]
    );
    // #103: the Take→Get switch stays IN-SESSION — no reopenSession. The camera
    // refuses the reconnect after the transport-close (see `commandListenerVolatile`
    // on `app`), so after 0x1018 it reads 0xd212 then sets DF01=0x14 on the existing
    // socket, matching main's working flow.
    assert!(
        from.steps.iter().all(|s| s.reopen_session.is_none()),
        "the from-LV image-transfer entry must switch in-session, not reopen (#103)"
    );
    assert_eq!(
        from.steps[1].get_prop.as_deref(),
        Some("0xd212"),
        "0x1018 is followed by the in-session 0xd212 read, not a reopen"
    );
    assert!(from
        .steps
        .iter()
        .any(|s| { s.set_prop.as_deref() == Some("0xd226") && s.value == Some(0) && s.tolerant }));

    // Get→Take is the reverse edge: reopen from image-import, select
    // FunctionMode=Take, negotiate the live-view function version as u32, and
    // restart open capture. It must not terminate an open-capture stream because
    // image-transfer has none active.
    let reverse = entries
        .iter()
        .find(|e| e.to == "shooting/stills" && e.from.as_deref() == Some("image-transfer"))
        .expect("image-transfer → shooting/stills entry");
    assert!(
        reverse.steps[0].reopen_session.is_some(),
        "Get→Take re-establishes PTP/IP from image-import"
    );
    assert!(reverse
        .steps
        .iter()
        .any(|s| { s.set_prop.as_deref() == Some("0xdf01") && s.value == Some(0x16) }));
    assert!(reverse
        .steps
        .iter()
        .any(|s| { s.set_prop.as_deref() == Some("0xdf2a") && s.value == Some(2) }));
    assert!(reverse
        .steps
        .iter()
        .any(|s| s.send_op.as_deref() == Some("0x902b") && s.repeat == 4));
    assert_eq!(
        reverse.steps.last().and_then(|s| s.send_op.as_deref()),
        Some("0x101c")
    );
    assert!(
        reverse
            .steps
            .iter()
            .all(|s| s.send_op.as_deref() != Some("0x1018")),
        "Get→Take must not terminate live-view before reopening it"
    );
}

#[test]
fn image_import_bootstrap_gate_covers_import_action_and_enumeration_props() {
    let m = gfx();
    assert!(m.sequence_gates.contains_key("imageImportBootstrap"));
    let d620 = m.properties["0xd620"]
        .requires_gate
        .as_ref()
        .expect("D620 is gated");
    assert_eq!(d620.name, "imageImportBootstrap");
    assert_eq!(d620.failure, camera_config::GateFailure::NoResponse);
    let d621 = m.properties["0xd621"]
        .requires_gate
        .as_ref()
        .expect("D621 is gated");
    assert_eq!(d621.name, "imageImportBootstrap");
    assert_eq!(d621.failure, camera_config::GateFailure::NoResponse);

    let import = m
        .action("app", ActionVerb::ImportObjects)
        .expect("app importObjects action");
    assert_image_import_bootstrap_gate(&import.steps);
    let d620_pos = import
        .steps
        .iter()
        .position(|s| s.get_prop.as_deref() == Some("0xd620"))
        .expect("import action reads D620");
    let complete = import
        .steps
        .iter()
        .position(|s| s.completes_gate.as_deref() == Some("imageImportBootstrap"))
        .expect("import action completes bootstrap");
    assert!(
        complete < d620_pos,
        "gate completes before D620 enumeration"
    );
}

#[test]
fn app_stills_video_mode_edges_are_lightweight_d246_writes() {
    let m = gfx();
    let entries = &m.connections["app"].entries;

    let to_video = entries
        .iter()
        .find(|e| e.to == "shooting/video" && e.from.as_deref() == Some("shooting/stills"))
        .expect("shooting/stills → shooting/video entry");
    assert_eq!(to_video.steps.len(), 1);
    assert_eq!(to_video.steps[0].set_prop.as_deref(), Some("0xd246"));
    assert_eq!(to_video.steps[0].value, Some(1));

    let to_stills = entries
        .iter()
        .find(|e| e.to == "shooting/stills" && e.from.as_deref() == Some("shooting/video"))
        .expect("shooting/video → shooting/stills entry");
    assert_eq!(to_stills.steps.len(), 1);
    assert_eq!(to_stills.steps[0].set_prop.as_deref(), Some("0xd246"));
    assert_eq!(to_stills.steps[0].value, Some(0));

    for entry in [to_video, to_stills] {
        assert!(
            entry.steps.iter().all(|s| {
                s.reopen_session.is_none()
                    && s.send_op.as_deref() != Some("0x100e")
                    && s.send_op.as_deref() != Some("0x1018")
                    && s.send_op.as_deref() != Some("0x9020")
                    && s.send_op.as_deref() != Some("0x9021")
                    && s.send_op.as_deref() != Some("0x9050")
                    && s.send_op.as_deref() != Some("0x9053")
                    && s.send_op.as_deref() != Some("0x9054")
                    && s.send_op.as_deref() != Some("0x9055")
            }),
            "D246 mode edges must not import, capture, record, reconnect, or tear down"
        );
    }
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

#[test]
fn app_init_shape_is_typed_and_carries_the_vendor_tail() {
    // #82: the init shape is a typed field (promoted out of `extra`), with the
    // literal 28-byte tail in data so the app replays bytes, not Swift literals.
    let m = gfx();
    let init = m.connections["app"]
        .init
        .as_ref()
        .expect("app declares an init shape");
    assert_eq!(init.identity.guid, "initiatorGuid");
    assert_eq!(init.identity.friendly_name, "initFriendlyName");
    assert_eq!(init.name_field_byte_count, 26);
    assert_eq!(
        init.tail.as_deref(),
        Some("cc004f000000000000000000000057004d0042000000000000000000")
    );
}

#[test]
fn close_session_step_parses_and_is_well_formed() {
    // #82: the graceful-close step kind, with the keep-AP flag, is expressible.
    let step: camera_config::Step =
        serde_yaml::from_str("closeSession: { keepAp: true }").expect("closeSession parses");
    assert_eq!(
        step.close_session,
        Some(camera_config::CloseSession { keep_ap: true })
    );
    assert!(step.is_well_formed(), "exactly one action field set");
}

#[test]
fn per_connection_traits_parse() {
    use camera_config::{LiveViewDeliveryKind, ShutterRecipe};
    let m = gfx();

    let app = &m.connections["app"];
    assert_eq!(app.init_shape.as_deref(), Some("app82"));
    assert_eq!(
        app.live_view_delivery.as_ref().map(|d| d.kind),
        Some(LiveViewDeliveryKind::Stream)
    );
    assert_eq!(app.shutter_recipe, Some(ShutterRecipe::AppPostview));

    let wt = &m.connections["wireless-tether"];
    let lv = wt
        .live_view_delivery
        .as_ref()
        .expect("tether polls live view");
    assert_eq!(lv.kind, LiveViewDeliveryKind::Poll);
    assert_eq!(lv.poll_op.as_deref(), Some("0x9018"));
    assert_eq!(wt.shutter_recipe, Some(ShutterRecipe::WirelessTether3Beat));

    // usb declares none → the app falls back (no negative list needed).
    assert!(m.connections["usb"].shutter_recipe.is_none());
    assert!(m.connections["usb"].live_view_delivery.is_none());
}

#[test]
fn unknown_shutter_recipe_fails_to_load() {
    // Closed vocabulary: an unknown value needs a schema PR, not silent acceptance.
    let r = serde_yaml::from_str::<camera_config::Connection>(
        "kind: ptpip-app\nshutterRecipe: teleport",
    );
    assert!(r.is_err(), "unknown shutterRecipe must fail to parse");
}

#[test]
fn app_autofocus_actions_lock_await_and_release() {
    use camera_config::AwaitSource;
    let m = gfx();

    // Tap-to-AF lock recipe (#35): 0x9026(packed area) → await 0xC005 + read 0xD209.
    let lock = m
        .action("app", ActionVerb::AutofocusLock)
        .expect("app.actions.autofocusLock");
    assert_eq!(lock.mode, "shooting/stills");
    assert_eq!(lock.params, vec!["afArea".to_string()]);
    assert_eq!(lock.steps[0].send_op.as_deref(), Some("0x9026"));
    assert_eq!(
        lock.steps[0].params,
        vec![StepParam::Runtime {
            runtime: "afArea".into(),
            shift: 0,
            mask: None,
        }]
    );
    let aw = lock.steps[1].await_until.as_ref().expect("AF await step");
    match &aw.source {
        AwaitSource::Event { code, then_poll } => {
            assert_eq!(code, "0xc005");
            assert_eq!(then_poll.as_deref(), Some("0xd209")); // hybrid: event then one read
        }
        other => panic!("expected an event source, got {other:?}"),
    }
    match &aw.until {
        Predicate::Any { any } => {
            assert!(
                any.iter().any(
                    |p| matches!(p, Predicate::Leaf(l) if l.prop == "0xd209" && l.eq == Some(1))
                ),
                "AF lock treats 0xd209=1 as terminal locked"
            );
            assert!(
                any.iter().any(
                    |p| matches!(p, Predicate::Leaf(l) if l.prop == "0xd209" && l.eq == Some(2))
                ),
                "AF lock treats 0xd209=2 as terminal failed"
            );
        }
        other => panic!("expected AF terminal predicate to be any(locked, failed), got {other:?}"),
    }
    assert!(lock.steps.iter().all(camera_config::Step::is_well_formed));

    // Release recipe: single 0x9027.
    let release = m
        .action("app", ActionVerb::AutofocusRelease)
        .expect("app.actions.autofocusRelease");
    assert_eq!(release.steps.len(), 1);
    assert_eq!(release.steps[0].send_op.as_deref(), Some("0x9027"));
}

/// #101: the RAF embedded-JPEG locator parses from the media table — magic +
/// big-endian offset/length at 0x54/0x58 (exiftool ProcessRAF / dcraw parse_fuji).
/// The app reads this to GetPartialObject the embedded JPG; the sim only serves
/// bytes and describes the layout here.
#[test]
fn raf_embedded_jpeg_locator_parses_from_the_media_table() {
    use camera_config::model::Endian;
    let m = gfx();
    let media = m.media.as_ref().expect("media table present");
    let raf = media.formats.get("0xb103").expect("RAF format row");
    assert!(raf.is_raw);
    let ej = raf
        .embedded_jpeg
        .as_ref()
        .expect("RAF carries an embedded-JPEG locator");
    assert_eq!(ej.magic, "FUJIFILMCCD-RAW");
    assert_eq!(ej.offset_at, 0x54);
    assert_eq!(ej.length_at, 0x58);
    assert_eq!(ej.endian, Endian::Big);
    // A non-RAW format has no locator.
    assert!(media.formats["0x3801"].embedded_jpeg.is_none());
}
