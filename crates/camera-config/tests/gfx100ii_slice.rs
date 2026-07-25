//! Loads the REAL `camera-config-data` files (fuji.yaml + gfx100ii.yaml App slice)
//! and exercises the engine against them — the first validation of the schema on
//! actual derived data rather than in-crate fixtures.

use camera_config::{
    parse_hex_code, ActionInitiatorParameterKind, ActionVerb, Availability, CameraManifest,
    CaptureSource, ConfigStore, InventoryCompleteness, ManufacturerDefaults, MissingRuntimeValue,
    ModeEntryExecution, ObjectTransferCompletionTiming, ObjectTransferResumePolicy,
    ObjectTransferStrategy, ObjectsAvailable, ObservationLine, OperationKind, PcssDiscoveryTarget,
    Predicate, PropView, PropertyKind, PropertyTransitionTerminal, RecordValueEncoding,
    RecordValueLiteral, ResponderMutation, SetPropValue, StepParam, ValuePolicy, VersionScheme,
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
    assert!(steps[start].is_sequence_gate_matchable());
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
fn d212_declares_heterogeneous_member_encoding() {
    let manifest = gfx();
    let payload = manifest.properties["0xd212"]
        .payload
        .as_ref()
        .expect("D212 payload descriptor");
    let member = payload
        .members
        .iter()
        .find(|member| member.code() == "0xd22f")
        .expect("D22F payload member");
    assert_eq!(member.encoding(4), RecordValueEncoding::PtpString);
    assert_eq!(
        member.simulator_value(),
        Some(&RecordValueLiteral::String(String::new()))
    );
}

#[test]
fn record_stream_member_contract_rejects_shape_drift() {
    let manifest = data("fuji/gfx100ii/gfx100ii.yaml");
    let malformed = [
        (
            "encoding: { kind: ptpString }, simulatorValue: \"\"",
            "encoding: { kind: fixed, width: 3 }, simulatorValue: 0",
            "unsupported fixed width 3",
        ),
        (
            "encoding: { kind: ptpString }, simulatorValue: \"\"",
            "encoding: { kind: ptpString }, simulatorValue: 0",
            "simulatorValue must be a string",
        ),
        (
            "encoding: { kind: ptpString }, simulatorValue: \"\"",
            "encoding: { kind: ptpString }",
            "ptpString requires simulatorValue",
        ),
        (
            "                \"0xdf41\"]",
            "                \"0xdf41\", \"0xdf41\"]",
            "repeats member code 0xdf41",
        ),
    ];

    for (old, new, expected) in malformed {
        let changed = manifest.replacen(old, new, 1);
        assert_ne!(changed, manifest, "fixture anchor must exist: {old}");
        let error = CameraManifest::from_yaml(&changed).expect_err("shape drift rejected");
        assert!(error.to_string().contains(expected), "{error}");
    }
}

#[test]
fn pcss_init_retry_policy_rejects_malformed_or_incoherent_values() {
    let manifest = data("fuji/gfx100ii/gfx100ii.yaml");
    let full_width = manifest.replace("whenReasons: [\"0x2019\"]", "whenReasons: [\"0x00012019\"]");
    let parsed = CameraManifest::from_yaml(&full_width).expect("u32 InitFail reason is valid");
    assert_eq!(
        parsed.connections["wireless-tether"]
            .init_retries
            .as_ref()
            .expect("wireless-tether has an InitFail retry policy")
            .when_reasons,
        ["0x00012019"]
    );

    let malformed = manifest.replace(
        "whenReasons: [\"0x2019\"]",
        "whenReasons: [not-a-response-code]",
    );
    assert!(CameraManifest::from_yaml(&malformed).is_err());

    let overflow = manifest.replace(
        "whenReasons: [\"0x2019\"]",
        "whenReasons: [\"0x100000000\"]",
    );
    let overflow_error = CameraManifest::from_yaml(&overflow).unwrap_err();
    assert!(
        overflow_error
            .to_string()
            .contains("whenReasons entries must be 32-bit hexadecimal codes"),
        "{overflow_error}"
    );

    let missing_backoff = manifest.replace(
        "max: 3, backoffMs: 500, whenReasons: [\"0x2019\"]",
        "max: 3, backoffMs: 0, whenReasons: [\"0x2019\"]",
    );
    assert!(CameraManifest::from_yaml(&missing_backoff).is_err());

    let duplicate = manifest.replace(
        "whenReasons: [\"0x2019\"]",
        "whenReasons: [\"0x2019\", \"0x2019\"]",
    );
    assert!(CameraManifest::from_yaml(&duplicate).is_err());

    let stray_backoff = manifest.replace(
        "max: 3, backoffMs: 500, whenReasons: [\"0x2019\"]",
        "max: 0, backoffMs: 500, whenReasons: []",
    );
    assert!(CameraManifest::from_yaml(&stray_backoff).is_err());

    let stray_reason = manifest.replace(
        "max: 3, backoffMs: 500, whenReasons: [\"0x2019\"]",
        "max: 0, backoffMs: 0, whenReasons: [\"0x2019\"]",
    );
    assert!(CameraManifest::from_yaml(&stray_reason).is_err());
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
        camera_config::parse_hex_bytes(&m.sentinels["ptpipCloseSentinel"].bytes),
        Some(vec![0x08, 0, 0, 0, 0xff, 0xff, 0xff, 0xff])
    );
    let tc = app
        .transport_close
        .as_ref()
        .expect("app declares a transport-close");
    assert_eq!(tc.sentinel, "ptpipCloseSentinel");
    assert_eq!(
        tc.when.as_deref(),
        Some("before-image-transfer-reestablishment")
    );
}

#[test]
fn camera_initiated_transfer_references_are_complete() {
    let manifest = gfx();
    let transfer = manifest
        .camera_initiated_transfer
        .as_ref()
        .expect("camera declares its reserved transfer queue");
    assert_eq!(transfer.handoff.connection, "app");
    assert_eq!(
        transfer.monitor_recovery,
        Some(camera_config::CameraInitiatedMonitorRecovery::SavedCameraReconnect)
    );
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
    let steps = lv.ptp_steps().expect("cold live-view PTP entry");
    assert_eq!(steps[0].set_prop.as_deref(), Some("0xdf00"));
    assert_eq!(steps[0].value, Some(6.into()));
    // Device-validated (#39): the GFX100 II rejects this advisory write
    // with 0x201d, so the step MUST stay tolerant or mode entry dies on
    // real hardware. This flag regressed silently once (client application #4) —
    // hence the explicit assert.
    assert!(steps[0].tolerant, "0xdf00 write must be tolerant");
    assert_eq!(steps[1].value, Some(0x16.into())); // functionMode 22
    assert_eq!(steps[2].read_echo.as_deref(), Some("0xdf2a"));
    assert_eq!(steps[3].repeat, 4); // 902B ×4
    assert_eq!(
        steps[4].open_channel,
        Some(camera_config::SocketRole::Event)
    );
    assert_eq!(
        steps[5].open_channel,
        Some(camera_config::SocketRole::LiveView)
    );
    assert_eq!(steps[6].send_op.as_deref(), Some("0x101c"));
    assert_eq!(steps[6].captures.len(), 1);
    assert_eq!(steps[6].captures[0].bind, "openCaptureTxId");
    assert_eq!(
        steps[6].captures[0].source,
        camera_config::CaptureSource::TransactionId
    );
    assert!(steps.iter().all(camera_config::Step::is_well_formed));
    let reverse = entries
        .iter()
        .find(|e| e.to == "shooting/stills" && e.from.as_deref() == Some("image-transfer"))
        .expect("image-transfer can return to live view");
    let reverse_steps = reverse.ptp_steps().expect("reverse live-view entry");
    let reverse_tail = &reverse_steps[reverse_steps.len() - 3..];
    assert_eq!(
        reverse_tail[0].open_channel,
        Some(camera_config::SocketRole::Event)
    );
    assert_eq!(
        reverse_tail[1].open_channel,
        Some(camera_config::SocketRole::LiveView)
    );
    let reverse_open = &reverse_tail[2];
    assert_eq!(reverse_open.send_op.as_deref(), Some("0x101c"));
    assert_eq!(reverse_open.captures.len(), 1);
    assert_eq!(reverse_open.captures[0].bind, "openCaptureTxId");
    assert_eq!(
        reverse_open.captures[0].source,
        camera_config::CaptureSource::TransactionId
    );
    assert_eq!(
        reverse_steps
            .iter()
            .filter(|step| step.send_op.as_deref() == Some("0x101c"))
            .count(),
        1
    );
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
    assert!(matches!(
        entry.execution,
        ModeEntryExecution::UserInstruction { .. }
    ));

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
    let discovery_targets = &wt.knock.as_ref().unwrap().discovery_targets;
    assert_eq!(
        discovery_targets.default,
        PcssDiscoveryTarget::SubnetBroadcast
    );
    assert_eq!(
        discovery_targets.supported,
        [
            PcssDiscoveryTarget::SubnetBroadcast,
            PcssDiscoveryTarget::ExplicitUnicast,
        ]
    );
    assert!(discovery_targets.retry_discovered_unicast);
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
    for op in [0x101cu16, 0x1018] {
        assert_eq!(
            m.operation_available("wireless-tether", "shooting/stills", op, &any),
            camera_config::Availability::Available,
            "PCSS live-view lifecycle op 0x{op:04x} should be available"
        );
    }
    // Image-transfer triad (wirePCSSShootDownload20260523): standard PTP ops
    // gated to the wireless-tether image-transfer mode. Enumeration is also
    // wire-confirmed while shooting/stills live view remains open. No 0x101B on PCSS.
    for op in [0x1007u16, 0x1008, 0x1009, 0x100a, 0x100b] {
        assert_eq!(
            m.operation_available("wireless-tether", "image-transfer", op, &any),
            camera_config::Availability::Available,
            "op 0x{op:04x} should be available on wireless-tether/image-transfer"
        );
    }
    assert_eq!(
        m.operation_available("wireless-tether", "shooting/stills", 0x1007, &any),
        camera_config::Availability::Available,
        "PCSS GetObjectHandles remains available while live view is open"
    );
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
    assert!(
        shutter.initiator().unwrap().params.is_empty(),
        "shutter takes no runtime params"
    );
    assert_eq!(
        shutter.initiator().unwrap().steps.len(),
        6,
        "3 beats × 2 ops each"
    );
    let phase_values = [0x00010000_i64, 0x00020000, 0x00000001];
    for (beat, phase) in phase_values.iter().enumerate() {
        let setprop = &shutter.initiator().unwrap().steps[beat * 2];
        assert_eq!(setprop.set_prop.as_deref(), Some("0xd039"));
        assert_eq!(
            setprop.value,
            Some((*phase).into()),
            "beat {} phase value",
            beat + 1
        );
        let retry_step = &shutter.initiator().unwrap().steps[beat * 2 + 1];
        assert!(!retry_step.tolerant);
        let retry = retry_step
            .retry
            .as_ref()
            .expect("each capture beat has a bounded Device Busy retry");
        assert_eq!(retry.when_response_codes, ["0x2019"]);
        assert!(retry.when_failure_classes.is_empty());
        assert_eq!(retry.max_attempts, 10);
        assert_eq!(retry.retry_delay_ms, 100);
        assert_eq!(retry.steps.len(), 1);
        let sendop = &retry.steps[0];
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
    assert!(keepalive.initiator().unwrap().params.is_empty());
    assert!(keepalive.triggers.is_empty());
    assert_eq!(keepalive.initiator().unwrap().steps.len(), 1);
    assert_eq!(
        keepalive.initiator().unwrap().steps[0].set_prop.as_deref(),
        Some("0xd21c")
    );
    assert_eq!(
        keepalive.initiator().unwrap().steps[0].value,
        Some(0.into())
    );
    assert_eq!(m.properties["0xd21c"].kind, PropertyKind::Scaffold);
    let priority_mode = &m.properties["0xd207"];
    assert_eq!(priority_mode.name, "priorityMode");
    assert_eq!(priority_mode.ptype.as_deref(), Some("u16"));
    assert_eq!(priority_mode.access.as_deref(), Some("readWrite"));
    assert_eq!(priority_mode.kind, PropertyKind::Scaffold);
    assert_eq!(priority_mode.labels["1"], "cameraPriority");
    assert_eq!(priority_mode.labels["2"], "pcPriority");
    assert!(priority_mode.controls.is_empty());
}

#[test]
fn standard_exposure_properties_have_display_labels() {
    let m = gfx();
    assert_eq!(m.value_label(0x500f, -1), Some("AUTO1"));
    assert_eq!(m.value_label(0x500f, 320), Some("320"));
    assert_eq!(m.value_label(0x500d, 244), Some("1/4000"));
    assert_eq!(m.value_label(0x500d, 64_000_030), Some("2m"));
    let generated = CameraManifest::from_yaml(&data("fuji/gfx100ii/gfx100ii.consolidated.yaml"))
        .expect("consolidated manifest loads");
    // Aperture and exposure-bias labels come from reviewed evidence at generation
    // time, so they exist only on the consolidated manifest.
    assert_eq!(generated.value_label(0x5007, 280), Some("F2.8"));
    assert_eq!(generated.value_label(0x5010, 333), Some("+0.3"));
    for code in [0x500f, 0x500d, 0x5007, 0x5010] {
        let property = generated.property(code).expect("exposure property exists");
        let descriptor = property
            .descriptor
            .as_ref()
            .expect("enum descriptor exists");
        for raw in &descriptor.values {
            assert!(
                generated.value_label(code, *raw).is_some(),
                "property 0x{code:04x} descriptor value {raw} needs a label"
            );
        }
    }

    let shutter = generated.property(0x500d).unwrap();
    let mut shutter_labels = std::collections::BTreeSet::new();
    for raw in &shutter.descriptor.as_ref().unwrap().values {
        let label = generated.value_label(0x500d, *raw).unwrap();
        assert!(
            shutter_labels.insert(label),
            "shutter descriptor label {label:?} must not be duplicated"
        );
    }
}

#[test]
fn wireless_tether_transfer_actions_bind_runtime_handle() {
    let m = gfx();
    for verb in [
        ActionVerb::EnumerateObjects,
        ActionVerb::GetObjectInfo,
        ActionVerb::GetThumb,
        ActionVerb::GetObject,
        ActionVerb::DeleteObject,
    ] {
        let action = m
            .action("wireless-tether", verb)
            .unwrap_or_else(|| panic!("missing action {verb:?}"));
        assert_eq!(action.mode, "", "{verb:?} must remain mode-neutral");
    }

    // Per-handle ops are parameterized: caller binds `handle` to a slot the
    // engine plugs into the StepParam::Runtime reference at emit time.
    for verb in [
        ActionVerb::GetObjectInfo,
        ActionVerb::GetThumb,
        ActionVerb::GetObject,
        ActionVerb::DeleteObject,
    ] {
        let a = m
            .action("wireless-tether", verb)
            .unwrap_or_else(|| panic!("missing action {verb:?}"));
        assert_eq!(a.initiator().unwrap().params, vec!["handle".to_string()]);
        assert_eq!(a.initiator().unwrap().steps.len(), 1);
        assert_eq!(
            a.initiator().unwrap().steps[0].params,
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
    assert!(enumerate.initiator().unwrap().params.is_empty());
    assert_eq!(
        enumerate.initiator().unwrap().steps[0].send_op.as_deref(),
        Some("0x1007")
    );
    assert_eq!(
        enumerate.initiator().unwrap().steps[0].params,
        vec![StepParam::Literal(0xffffffff), StepParam::Literal(0)]
    );

    let transfer = m.connections["wireless-tether"]
        .object_transfer
        .as_ref()
        .expect("wireless-tether objectTransfer contract");
    assert_eq!(transfer.strategy, ObjectTransferStrategy::WholeObject);
    assert_eq!(
        transfer.resume_policy,
        ObjectTransferResumePolicy::RestartFromZero
    );
    assert_eq!(transfer.read_action, ActionVerb::GetObject);
    let completion = transfer.completion.as_ref().expect("completion policy");
    assert_eq!(completion.action, ActionVerb::DeleteObject);
    assert_eq!(
        completion.after,
        ObjectTransferCompletionTiming::LocalCommit
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
fn wireless_tether_live_view_actions_keep_pcss_request_shapes_connection_scoped() {
    let m = gfx();
    let start = m
        .action("wireless-tether", ActionVerb::StartLiveView)
        .expect("wireless-tether startLiveView action");
    assert_eq!(start.mode, "shooting/stills");
    assert!(start.initiator().unwrap().params.is_empty());
    assert_eq!(start.initiator().unwrap().steps.len(), 3);
    let terminate_step = &start.initiator().unwrap().steps[0];
    assert!(terminate_step.tolerant);
    let terminate_retry = terminate_step
        .retry
        .as_ref()
        .expect("defensive terminate retries Device Busy before tolerance");
    assert_eq!(terminate_retry.when_response_codes, ["0x2019"]);
    assert!(terminate_retry.when_failure_classes.is_empty());
    assert_eq!(terminate_retry.max_attempts, 10);
    assert_eq!(terminate_retry.retry_delay_ms, 300);
    assert_eq!(terminate_retry.steps.len(), 1);
    assert_eq!(terminate_retry.steps[0].send_op.as_deref(), Some("0x1018"));
    assert_eq!(terminate_retry.steps[0].params, [StepParam::Literal(1)]);
    assert!(!terminate_retry.steps[0].tolerant);
    assert!(terminate_retry.steps[0].captures.is_empty());
    assert_eq!(
        start.initiator().unwrap().steps[1].set_prop.as_deref(),
        Some("0xd1bc")
    );
    assert_eq!(start.initiator().unwrap().steps[1].value, Some(2.into()));
    assert!(!start.initiator().unwrap().steps[1].tolerant);
    assert_eq!(
        start.initiator().unwrap().steps[2].send_op.as_deref(),
        Some("0x101c")
    );
    assert_eq!(
        start.initiator().unwrap().steps[2].params,
        [StepParam::Literal(0), StepParam::Literal(0)]
    );
    assert!(start.initiator().unwrap().steps[2].captures.is_empty());
    assert!(!start.initiator().unwrap().steps[2].tolerant);
    let selector = &m.properties["0xd1bc"];
    assert_eq!(selector.ptype.as_deref(), Some("u16"));
    assert_eq!(selector.access.as_deref(), Some("readWrite"));
    assert_eq!(selector.kind, PropertyKind::Scaffold);

    let poll = m
        .action("wireless-tether", ActionVerb::PollLiveView)
        .expect("wireless-tether pollLiveView action");
    assert_eq!(poll.mode, "shooting/stills");
    assert!(poll.initiator().unwrap().params.is_empty());
    assert_eq!(poll.initiator().unwrap().steps.len(), 1);
    let retry = poll.initiator().unwrap().steps[0]
        .retry
        .as_ref()
        .expect("pollLiveView has a bounded response-selected retry");
    assert_eq!(retry.when_response_codes, ["0x2002"]);
    assert!(retry.when_failure_classes.is_empty());
    assert_eq!(retry.max_attempts, 10);
    assert_eq!(retry.retry_delay_ms, 100);
    assert_eq!(retry.steps.len(), 1);
    assert_eq!(retry.steps[0].send_op.as_deref(), Some("0x9018"));
    assert!(retry.steps[0].params.is_empty());

    let stop = m
        .action("wireless-tether", ActionVerb::StopLiveView)
        .expect("wireless-tether stopLiveView action");
    assert_eq!(stop.mode, "shooting/stills");
    assert!(stop.initiator().unwrap().params.is_empty());
    assert_eq!(stop.initiator().unwrap().steps.len(), 1);
    assert_eq!(
        stop.initiator().unwrap().steps[0].send_op.as_deref(),
        Some("0x1018")
    );
    assert_eq!(
        stop.initiator().unwrap().steps[0].params,
        [StepParam::Literal(1)]
    );
    assert!(stop.initiator().unwrap().steps[0].captures.is_empty());

    for verb in [
        ActionVerb::StartLiveView,
        ActionVerb::PollLiveView,
        ActionVerb::StopLiveView,
    ] {
        assert!(m.action("app", verb).is_none());
    }
}

#[test]
fn wireless_tether_pcss_autofocus_actions_and_curation_are_exact() {
    use camera_config::AwaitSource;

    let m = gfx();
    let lock = m
        .action("wireless-tether", ActionVerb::AutofocusLock)
        .expect("wireless-tether autofocusLock action");
    assert_eq!(lock.mode, "shooting/stills");
    let initiator = lock.initiator().expect("lock initiator");
    assert_eq!(initiator.params.len(), 1);
    let focus_area = initiator.params[0].normalized();
    assert_eq!(focus_area.name, "focusArea");
    assert_eq!(focus_area.kind, ActionInitiatorParameterKind::String);
    assert!(!focus_area.required);
    assert_eq!(initiator.steps.len(), 5);

    assert_eq!(initiator.steps[0].set_prop.as_deref(), Some("0xd395"));
    assert!(matches!(
        initiator.steps[0].value.as_ref(),
        Some(SetPropValue::Runtime(value))
            if value.runtime == "focusArea" && value.if_missing == MissingRuntimeValue::Skip
    ));
    for (index, prop, value) in [(1, "0xd230", 1), (2, "0xd208", 0xa000)] {
        assert_eq!(initiator.steps[index].set_prop.as_deref(), Some(prop));
        assert_eq!(initiator.steps[index].value, Some(value.into()));
        assert!(!initiator.steps[index].tolerant);
    }
    assert_eq!(initiator.steps[3].send_op.as_deref(), Some("0x100e"));
    assert_eq!(
        initiator.steps[3].params,
        [StepParam::Literal(0), StepParam::Literal(0)]
    );

    let await_step = &initiator.steps[4];
    let await_until = await_step.await_until.as_ref().expect("D209 poll");
    assert!(matches!(
        &await_until.source,
        AwaitSource::Poll { prop } if prop == "0xd209"
    ));
    assert_eq!(await_until.interval_ms, 25);
    assert_eq!(await_until.timeout_ms, 3000);
    match &await_until.until {
        Predicate::Any { any } => {
            assert_eq!(any.len(), 2, "only success or failure is terminal");
            assert!(
                matches!(&any[0], Predicate::Leaf(leaf) if leaf.prop == "0xd209" && leaf.eq == Some(2))
            );
            assert!(
                matches!(&any[1], Predicate::Leaf(leaf) if leaf.prop == "0xd209" && leaf.eq == Some(3))
            );
        }
        other => panic!("expected success/failure predicate, got {other:?}"),
    }
    assert_eq!(await_step.captures.len(), 1);
    assert_eq!(await_step.captures[0].bind, "autofocusResult");
    assert_eq!(await_step.captures[0].source, CaptureSource::PropValue);

    let responder = lock.responder.as_ref().expect("lock responder");
    assert_eq!(responder.params.len(), 1);
    assert_eq!(responder.params[0].name, "result");
    assert_eq!(responder.params[0].default, Some(3));
    assert_eq!(responder.params[0].min, Some(2));
    assert_eq!(responder.params[0].max, Some(3));
    assert!(matches!(
        &responder.mutation,
        ResponderMutation::PropertyTransition {
            target,
            initial: Some(1),
            terminal: PropertyTransitionTerminal::Parameter { parameter },
            settle_after_polls: 2,
        } if target == "0xd209" && parameter == "result"
    ));

    let release = m
        .action("wireless-tether", ActionVerb::AutofocusRelease)
        .expect("wireless-tether autofocusRelease action");
    let release_steps = &release.initiator().expect("release initiator").steps;
    assert_eq!(release_steps.len(), 4);
    for (index, prop, value, tolerant) in [
        (0, "0xd230", 1, true),
        (1, "0xd21c", 0, false),
        (2, "0xd208", 6, false),
    ] {
        assert_eq!(release_steps[index].set_prop.as_deref(), Some(prop));
        assert_eq!(release_steps[index].value, Some(value.into()));
        assert_eq!(release_steps[index].tolerant, tolerant);
    }
    assert_eq!(release_steps[3].send_op.as_deref(), Some("0x100e"));
    assert_eq!(
        release_steps[3].params,
        [StepParam::Literal(0), StepParam::Literal(0)]
    );
    let release_responder = release.responder.as_ref().expect("release responder");
    assert!(release_responder.params.is_empty());
    assert!(matches!(
        &release_responder.mutation,
        ResponderMutation::PropertyTransition {
            target,
            initial: None,
            terminal: PropertyTransitionTerminal::Fixed { value: 4 },
            settle_after_polls: 0,
        } if target == "0xd209"
    ));

    for key in ["wirePcssAutofocus20260718", "sdkPcssAutofocus20260718"] {
        assert_eq!(m.evidence[key].path, "evidence/PCSS_PARITY_20260714.md");
    }
    assert_eq!(m.evidence["wirePcssAutofocus20260718"].kind, "wire-capture");
    assert_eq!(m.evidence["sdkPcssAutofocus20260718"].kind, "vendor-sdk");

    let magnification = &m.properties["0xd01b"];
    assert_eq!(magnification.name, "liveViewMagnification");
    assert_eq!(magnification.value_profiles.len(), 1);
    let magnification_profile = &magnification.value_profiles[0];
    assert_eq!(
        magnification_profile.connection.as_deref(),
        Some("wireless-tether")
    );
    assert_eq!(
        magnification_profile.mode.as_deref(),
        Some("shooting/stills")
    );
    assert_eq!(
        magnification_profile
            .rows
            .iter()
            .map(|row| (row.raw, row.label.as_str()))
            .collect::<Vec<_>>(),
        [(1, "x1.0"), (2, "x2.5"), (4, "x4.0")]
    );

    let d208 = &m.properties["0xd208"];
    assert_eq!(d208.name, "pcssCaptureFunction");
    assert_eq!(d208.kind, PropertyKind::Scaffold);
    assert!(
        d208.descriptor.is_none(),
        "source manifest leaves descriptor generation owned"
    );
    assert_eq!(
        d208.value_rows
            .iter()
            .map(|row| (row.raw, row.label.as_str()))
            .collect::<Vec<_>>(),
        [(0xa000, "instantAf"), (6, "aeOffS1Off")]
    );
    let d230 = &m.properties["0xd230"];
    assert_eq!(d230.name, "pcssForceMode");
    assert_eq!(d230.kind, PropertyKind::Scaffold);
    assert!(
        d230.descriptor.is_none(),
        "source manifest leaves descriptor generation owned"
    );
    assert_eq!(d230.value_rows.len(), 1);
    assert_eq!(
        (d230.value_rows[0].raw, d230.value_rows[0].label.as_str()),
        (1, "shootMode")
    );
    let generated = CameraManifest::from_yaml(&data("fuji/gfx100ii/gfx100ii.consolidated.yaml"))
        .expect("consolidated manifest");
    let generated_d230 = &generated.properties["0xd230"];
    assert_eq!(
        generated_d230
            .descriptor
            .as_ref()
            .expect("generated D230 descriptor")
            .values,
        [1, 2]
    );
    assert_eq!(
        generated_d230.value_rows.len(),
        1,
        "raw 2 remains unlabeled"
    );

    let d209 = &m.properties["0xd209"];
    assert_eq!(d209.name, "s1LockColor");
    assert_eq!(d209.value_profiles.len(), 1);
    assert_eq!(
        d209.value_profiles[0]
            .rows
            .iter()
            .map(|row| (row.raw, row.label.as_str()))
            .collect::<Vec<_>>(),
        [
            (1, "operating"),
            (2, "success"),
            (3, "failure"),
            (4, "noOperation")
        ]
    );
    assert!(m.properties["0xd395"]
        .evidence
        .contains(&"wirePcssAutofocus20260718".to_string()));
    assert!(!m.properties["0xd21c"]
        .name
        .to_ascii_lowercase()
        .contains("release"));
    assert_eq!(m.properties["0xd207"].name, "priorityMode");
    assert!(m.operations["0x100e"].effects.is_empty());
    assert!(m.connections["wireless-tether"]
        .bindings
        .as_ref()
        .is_some_and(|bindings| bindings.event.is_none()));
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
    assert!(shutter.initiator().unwrap().params.is_empty());
    assert_eq!(shutter.initiator().unwrap().steps.len(), 3);
    assert_eq!(
        shutter.initiator().unwrap().steps[0].send_op.as_deref(),
        Some("0x100e")
    );
    assert_eq!(
        shutter.initiator().unwrap().steps[0].params,
        vec![StepParam::Literal(0), StepParam::Literal(0)]
    );
    // The middle step waits for the camera's postview event (arrival alone gates
    // the read — the 0x9022 below is the data read).
    let aw = shutter.initiator().unwrap().steps[1]
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
    assert_eq!(
        shutter.initiator().unwrap().steps[2].send_op.as_deref(),
        Some("0x9022")
    );
    assert!(shutter
        .initiator()
        .unwrap()
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

    // Enumeration owns the page-prime operation and independently retries the
    // count/handle properties; it takes no runtime params.
    let enumerate = m
        .action("app", ActionVerb::EnumerateObjects)
        .expect("app.actions.enumerateObjects");
    assert_eq!(enumerate.mode, "image-transfer");
    assert!(enumerate.initiator().unwrap().params.is_empty());
    assert_eq!(enumerate.initiator().unwrap().steps.len(), 3);
    let prime = enumerate.initiator().unwrap().steps[0]
        .retry
        .as_ref()
        .expect("prime retry");
    assert_eq!(prime.when_response_codes, ["0x2013", "0x2019"]);
    assert_eq!(
        prime.when_failure_classes,
        [camera_config::RetryFailureClass::Decode]
    );
    assert_eq!(prime.max_attempts, 5);
    assert_eq!(prime.retry_delay_ms, 100);
    assert_image_import_bootstrap_gate(&prime.steps);
    for (step, prop) in enumerate.initiator().unwrap().steps[1..]
        .iter()
        .zip(["0xd620", "0xd621"])
    {
        let retry = step.retry.as_ref().expect("enumeration property retry");
        assert_eq!(retry.when_response_codes, ["0x2002", "0x2013", "0x2019"]);
        assert_eq!(
            retry.when_failure_classes,
            [camera_config::RetryFailureClass::Decode]
        );
        assert_eq!(retry.max_attempts, 3);
        assert_eq!(retry.retry_delay_ms, 1000);
        assert_eq!(retry.steps[0].get_prop.as_deref(), Some(prop));
    }
    assert!(matches!(
        enumerate.initiator().unwrap().steps[2].retry.as_ref().unwrap().steps[0].captures.as_slice(),
        [camera_config::model::Capture {
            bind,
            source: camera_config::CaptureSource::PtpU32Array,
        }] if bind == "objectHandles"
    ));
    assert!(enumerate.triggers.is_empty());

    // Per-handle metadata + thumbnail: standard PTP, same wire shape as PCSS.
    for verb in [ActionVerb::GetObjectInfo, ActionVerb::GetThumb] {
        let a = m
            .action("app", verb)
            .unwrap_or_else(|| panic!("missing action {verb:?}"));
        assert_eq!(a.mode, "image-transfer");
        assert_eq!(a.initiator().unwrap().params, vec!["handle".to_string()]);
        assert_eq!(a.initiator().unwrap().steps.len(), 1);
        assert_eq!(
            a.initiator().unwrap().steps[0].params,
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
        get.initiator().unwrap().params,
        vec![
            "handle".to_string(),
            "offset".to_string(),
            "length".to_string()
        ],
        "reference app getObject is chunked — caller binds offset+length per iteration"
    );
    assert_eq!(get.initiator().unwrap().steps.len(), 1);
    assert_eq!(
        get.initiator().unwrap().steps[0].send_op.as_deref(),
        Some("0x101b")
    );
    assert_eq!(
        get.initiator().unwrap().steps[0].params,
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
    assert_eq!(
        pcss.initiator().unwrap().params.len(),
        1,
        "PCSS getObject is whole-object"
    );
    assert_eq!(
        app.initiator().unwrap().params.len(),
        3,
        "reference app getObject is chunked"
    );
    assert_eq!(
        pcss.initiator().unwrap().steps[0].send_op.as_deref(),
        Some("0x1009")
    );
    assert_eq!(pcss.initiator().unwrap().steps[0].params.len(), 1);
    assert_eq!(
        app.initiator().unwrap().steps[0].send_op.as_deref(),
        Some("0x101b")
    );
    assert_eq!(
        app.initiator().unwrap().steps[0].params.len(),
        4,
        "reference app derives offset_high from the logical offset slot for the wire call"
    );
    assert_eq!(
        app.initiator().unwrap().steps[0].params[1],
        StepParam::Runtime {
            runtime: "offset".into(),
            shift: 0,
            mask: Some(0xffff_ffff),
        }
    );
    assert_eq!(
        app.initiator().unwrap().steps[0].params[3],
        StepParam::Runtime {
            runtime: "offset".into(),
            shift: 32,
            mask: None,
        }
    );
}

#[test]
fn scaffold_props_are_tagged_so_clients_can_filter_them_out_of_settings_ui() {
    // 0xD039 / 0xD1BC / 0xD21C / 0xD207 LOOK settable on the wire but are
    // protocol scaffolding (virtual shutter, live-view select, keepalive, and
    // SDK priority selection) — wirePCSSShootDownload20260523. `kind: scaffold`
    // lets clients filter them from settings UI without re-deriving the
    // negative list each time.
    let m = gfx();
    for code in ["0xd039", "0xd1bc", "0xd21c", "0xd207"] {
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
    // Load every committed camera-observation/v1 reduction bundle.
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../packages/camera-config-data/fuji/gfx100ii/evidence");
    let mut bundles = Vec::new();
    let mut files = 0;
    for directory in ["probe", "labels", "value-profiles"] {
        let mut paths = std::fs::read_dir(root.join(directory))
            .expect("evidence directory")
            .map(|entry| entry.unwrap().path())
            .filter(|path| {
                path.extension()
                    .is_some_and(|extension| extension == "jsonl")
            })
            .collect::<Vec<_>>();
        paths.sort();
        for path in paths {
            bundles.push(std::fs::read_to_string(path).unwrap());
            files += 1;
        }
    }
    assert_eq!(files, 10, "expected the migrated corpus");
    let refs = bundles.iter().map(String::as_str).collect::<Vec<_>>();

    let validated = camera_config::validate_bundles(&refs).expect("canonical corpus validates");
    assert!(
        validated.records.iter().all(|record| {
            !matches!(
                record,
                ObservationLine::Capability(capability)
                    if capability.inventory_completeness != InventoryCompleteness::Partial
            )
        }),
        "the current corpus has no evidence attesting a complete inventory"
    );

    let proposal = camera_config::propose(&refs).expect("canonical corpus validates");
    let committed_proposal: camera_config::Proposal = serde_json::from_str(
        &std::fs::read_to_string(root.join("camera-observation-v1.proposal.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(
        proposal, committed_proposal,
        "proposal regeneration drifted"
    );

    let review: camera_config::ProposalReview = serde_json::from_str(
        &std::fs::read_to_string(root.join("camera-observation-v1.review.json")).unwrap(),
    )
    .unwrap();
    let migration: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(root.join("camera-observation-v1-migration.json")).unwrap(),
    )
    .unwrap();
    let scoped_descriptor_codes = [
        "0xd037", "0xd039", "0xd1bc", "0xd201", "0xd208", "0xd228", "0xd23c", "0xd369",
    ];
    assert_eq!(migration["scopedDescriptorNormalizations"], 40);
    assert_eq!(
        migration["scopedDescriptorCodes"],
        serde_json::json!(scoped_descriptor_codes)
    );
    assert_eq!(
        review
            .decisions
            .values()
            .filter(|decision| **decision == camera_config::ReviewDisposition::Reject)
            .count(),
        13,
        "review rejects nine stale type claims and four incompatible descriptors"
    );
    let base = CameraManifest::from_yaml(&data("fuji/gfx100ii/gfx100ii.yaml")).unwrap();
    let m = camera_config::apply_review(&base, &proposal, &review).expect("review applies");

    m.require_supported_schema()
        .expect("applied manifest uses the current schema");

    // Identity and curated graph survive reviewed generation.
    assert_eq!(m.camera.model, "GFX100 II");
    assert_eq!(m.camera.firmware, "2.30");
    assert!(m.connections.contains_key("usb"));
    assert!(m.connections.contains_key("wireless-tether"));
    assert!(m.modes.contains_key("shooting/stills"));
    assert!(m.modes.contains_key("shooting/video"));
    // Substantial op/prop coverage from the enumeration.
    assert!(m.operations.len() >= 20, "ops: {}", m.operations.len());
    assert!(m.properties.len() >= 50, "props: {}", m.properties.len());
    assert!(base
        .operations
        .values()
        .all(|operation| operation.kind == OperationKind::Executable));
    let (generated_code, generated_operation) = m
        .operations
        .iter()
        .find(|(code, _)| !base.operations.contains_key(code.as_str()))
        .expect("generator adds inventory-only operations");
    assert_eq!(generated_operation.kind, OperationKind::AdvertisedOnly);
    assert_eq!(
        m.operation_available(
            "usb",
            "shooting/stills",
            parse_hex_code(generated_code).unwrap(),
            &PropView::new(),
        ),
        Availability::Unavailable
    );
    assert!(m
        .operations
        .iter()
        .filter(|(code, _)| !base.operations.contains_key(code.as_str()))
        .all(|(_, operation)| operation.kind == OperationKind::AdvertisedOnly));
    assert!(m
        .properties
        .iter()
        .filter(|(code, _)| !base.properties.contains_key(code.as_str()))
        .all(|(_, property)| property.kind == PropertyKind::CatalogOnly));
    for code in scoped_descriptor_codes {
        let property = &m.properties[code];
        assert!(
            property.descriptor.is_none(),
            "scoped values for {code} must not become a global descriptor"
        );
        assert_eq!(
            property.value_profiles.len(),
            5,
            "each exact connection/mode profile survives for {code}"
        );
    }
    // GetDevicePropDesc (0x1014) preserves exact evidence tuples.
    let dpd = &m.operations["0x1014"];
    assert!(dpd
        .observed_scopes
        .iter()
        .any(|scope| scope.connection == "usb" && scope.mode == "shooting/stills"));
    assert!(dpd
        .observed_scopes
        .iter()
        .any(|scope| { scope.connection == "wireless-tether" && scope.mode == "shooting/video" }));
    // Properties are camera-sourced (GetDevicePropDesc).
    for (code, descriptor) in m
        .properties
        .iter()
        .filter_map(|(code, property)| property.descriptor.as_ref().map(|value| (code, value)))
    {
        let expected = if code == "0xd246" {
            camera_config::ValueSource::Manifest
        } else {
            camera_config::ValueSource::Camera
        };
        assert_eq!(
            descriptor.source,
            Some(expected),
            "descriptor source {code}"
        );
    }
    assert_eq!(
        m.connections["wireless-tether"]
            .knock
            .as_ref()
            .and_then(|knock| knock.camera_name.as_deref()),
        Some("GFX100 II"),
        "reviewed generation preserves the curated callback identity"
    );
    let mut generated_projection = m.clone();
    // cameraName is curated callback identity, so exclude only that field from
    // the generator-owned regeneration comparison on both sides.
    generated_projection
        .connections
        .get_mut("wireless-tether")
        .expect("wireless tether connection")
        .knock
        .as_mut()
        .expect("wireless tether has a knock contract")
        .camera_name = None;
    let mut digest_bound =
        CameraManifest::from_yaml(&data("fuji/gfx100ii/gfx100ii.consolidated.yaml"))
            .expect("consolidated manifest loads");
    digest_bound
        .connections
        .get_mut("wireless-tether")
        .expect("wireless tether connection")
        .knock
        .as_mut()
        .expect("wireless tether has a knock contract")
        .camera_name = None;
    assert_eq!(
        digest_bound.to_yaml().unwrap(),
        generated_projection.to_yaml().unwrap(),
        "reviewed manifest regeneration drifted outside the curated callback identity"
    );
}

#[test]
fn image_import_entry_and_enumeration_keep_their_own_steps() {
    let m = gfx();
    let entries = &m.connections["app"].entries;
    // Cold entry: tolerant preamble + first-image initialization. Public
    // enumeration priming belongs to enumerateObjects, not mode entry.
    let cold = entries
        .iter()
        .find(|e| e.to == "image-transfer" && e.from.is_none())
        .unwrap();
    let cold_steps = cold.ptp_steps().expect("cold image-transfer PTP entry");
    assert!(cold_steps.iter().all(|step| step.retry.is_none()));
    assert!(cold_steps
        .iter()
        .any(|s| s.get_prop.as_deref() == Some("0xd212") && s.tolerant));
    assert!(cold_steps.iter().any(|s| {
        s.set_prop.as_deref() == Some("0xdf28") && s.value == Some(3.into()) && s.tolerant
    }));
    assert!(cold_steps.iter().any(|s| {
        s.set_prop.as_deref() == Some("0xd226") && s.value == Some(0.into()) && s.tolerant
    }));
    assert!(cold_steps.iter().any(|s| {
        s.set_prop.as_deref() == Some("0xd227") && s.value == Some(0.into()) && s.tolerant
    }));
    assert!(cold_steps
        .iter()
        .any(|s| s.get_prop.as_deref() == Some("0xd244") && s.tolerant));
    assert!(cold_steps
        .iter()
        .all(|step| !matches!(step.send_op.as_deref(), Some("0x9050" | "0x9053"))));
    let enumerate = m
        .action("app", ActionVerb::EnumerateObjects)
        .expect("enumeration action");
    assert_image_import_bootstrap_gate(
        &enumerate.initiator().unwrap().steps[0]
            .retry
            .as_ref()
            .unwrap()
            .steps,
    );
    // from-live-view entry binds the runtime open-capture txid into 0x1018.
    let from = entries
        .iter()
        .find(|e| e.to == "image-transfer" && e.from.as_deref() == Some("shooting/stills"))
        .unwrap();
    let reestablish = match &from.execution {
        ModeEntryExecution::ReestablishConnection(plan) => plan,
        other => panic!("expected re-establishment, got {other:?}"),
    };
    assert_eq!(
        reestablish.params.get("launchMode").map(String::as_str),
        Some("3")
    );
    assert_eq!(reestablish.exit_steps[0].send_op.as_deref(), Some("0x1018"));
    assert_eq!(
        reestablish.exit_steps[0].params,
        vec![StepParam::Runtime {
            runtime: "openCaptureTxId".into(),
            shift: 0,
            mask: None,
        }]
    );
    let close = reestablish.exit_steps[1]
        .close_session
        .as_ref()
        .expect("orderly CloseSession follows TerminateOpenCapture");
    assert!(close.transport_close);
    assert_eq!(reestablish.exit_steps.len(), 2);

    // Get→Take is the reverse edge: reopen from image-import, select
    // FunctionMode=Take, negotiate the live-view function version as u32, and
    // restart open capture. It must not terminate an open-capture stream because
    // image-transfer has none active.
    let reverse = entries
        .iter()
        .find(|e| e.to == "shooting/stills" && e.from.as_deref() == Some("image-transfer"))
        .expect("image-transfer → shooting/stills entry");
    let reverse_steps = reverse.ptp_steps().expect("Get→Take PTP entry");
    assert!(
        reverse_steps[0].reopen_session.is_some(),
        "Get→Take re-establishes PTP/IP from image-import"
    );
    assert!(reverse_steps
        .iter()
        .any(|s| { s.set_prop.as_deref() == Some("0xdf01") && s.value == Some(0x16.into()) }));
    assert!(reverse_steps
        .iter()
        .any(|s| { s.set_prop.as_deref() == Some("0xdf2a") && s.value == Some(2.into()) }));
    assert!(reverse_steps
        .iter()
        .any(|s| s.send_op.as_deref() == Some("0x902b") && s.repeat == 4));
    assert_eq!(
        reverse_steps[reverse_steps.len() - 3].open_channel,
        Some(camera_config::SocketRole::Event)
    );
    assert_eq!(
        reverse_steps[reverse_steps.len() - 2].open_channel,
        Some(camera_config::SocketRole::LiveView)
    );
    assert_eq!(
        reverse_steps.last().and_then(|s| s.send_op.as_deref()),
        Some("0x101c")
    );
    assert!(
        reverse_steps
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
    let prime_pos = import
        .initiator()
        .unwrap()
        .steps
        .iter()
        .position(|step| {
            step.retry.as_ref().is_some_and(|retry| {
                retry
                    .steps
                    .iter()
                    .any(|nested| nested.starts_gate.as_deref() == Some("imageImportBootstrap"))
            })
        })
        .expect("import action reuses the enumeration prime");
    let import_prime = import.initiator().unwrap().steps[prime_pos]
        .retry
        .as_ref()
        .unwrap();
    assert_image_import_bootstrap_gate(&import_prime.steps);
    let d620_pos = import
        .initiator()
        .unwrap()
        .steps
        .iter()
        .position(|step| {
            step.retry.as_ref().is_some_and(|retry| {
                retry
                    .steps
                    .iter()
                    .any(|nested| nested.get_prop.as_deref() == Some("0xd620"))
            })
        })
        .expect("import action reads D620");
    assert!(
        prime_pos < d620_pos,
        "gate completes before D620 enumeration"
    );

    let enumerate = m
        .action("app", ActionVerb::EnumerateObjects)
        .expect("enumerateObjects action");
    let prime = enumerate.initiator().unwrap().steps[0]
        .retry
        .as_ref()
        .expect("prime retry");
    assert_image_import_bootstrap_gate(&prime.steps);
    assert_eq!(import_prime.when_response_codes, prime.when_response_codes);
    assert_eq!(
        import_prime.when_failure_classes,
        prime.when_failure_classes
    );
    assert_eq!(import_prime.max_attempts, prime.max_attempts);
    assert_eq!(import_prime.retry_delay_ms, prime.retry_delay_ms);
}

#[test]
fn app_stills_video_mode_edges_are_lightweight_d246_writes() {
    let m = gfx();
    let entries = &m.connections["app"].entries;

    let to_video = entries
        .iter()
        .find(|e| e.to == "shooting/video" && e.from.as_deref() == Some("shooting/stills"))
        .expect("shooting/stills → shooting/video entry");
    let to_video_steps = to_video.ptp_steps().expect("stills→video PTP entry");
    assert_eq!(to_video_steps.len(), 1);
    assert_eq!(to_video_steps[0].set_prop.as_deref(), Some("0xd246"));
    assert_eq!(to_video_steps[0].value, Some(1.into()));

    let to_stills = entries
        .iter()
        .find(|e| e.to == "shooting/stills" && e.from.as_deref() == Some("shooting/video"))
        .expect("shooting/video → shooting/stills entry");
    let to_stills_steps = to_stills.ptp_steps().expect("video→stills PTP entry");
    assert_eq!(to_stills_steps.len(), 1);
    assert_eq!(to_stills_steps[0].set_prop.as_deref(), Some("0xd246"));
    assert_eq!(to_stills_steps[0].value, Some(0.into()));

    for entry in [to_video, to_stills] {
        assert!(
            entry.ptp_steps().unwrap().iter().all(|s| {
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
fn app_init_shape_declares_one_fixed_name_field() {
    // #365: the bytes after the first 26 are part of the same UTF-16LE field.
    let m = gfx();
    let init = m.connections["app"]
        .init
        .as_ref()
        .expect("app declares an init shape");
    assert_eq!(init.identity.guid, "initiatorGuid");
    assert_eq!(init.identity.friendly_name, "initFriendlyName");
    assert_eq!(init.name_field_byte_count, 54);
    assert_eq!(init.evidence, ["docLiveControls"]);
}

#[test]
fn close_session_step_parses_and_is_well_formed() {
    // #244: orderly transport close does not promise AP retention.
    let step: camera_config::Step = serde_yaml::from_str("closeSession: { transportClose: true }")
        .expect("closeSession parses");
    assert_eq!(
        step.close_session,
        Some(camera_config::CloseSession {
            transport_close: true
        })
    );
    assert!(step.is_well_formed(), "exactly one action field set");
    assert!(
        serde_yaml::from_str::<camera_config::Step>("closeSession: { keepAp: true }").is_err(),
        "retired keepAp spelling must fail closed"
    );
}

#[test]
fn open_channel_requires_a_top_level_bound_auxiliary_role() {
    let manifest = |bindings: &str, step: &str| {
        format!(
            "schema: camera-config/v1\ncamera: {{ manufacturer: Test, model: Test, firmware: \"1\" }}\nconnections:\n  app:\n    bindings: {{ {bindings} }}\n    entries:\n      - to: test\n        steps:\n{step}\n"
        )
    };
    let valid = manifest(
        "command: 55740, event: 55741",
        "          - { sendOp: \"0x101c\" }\n          - { openChannel: event }",
    );
    assert!(CameraManifest::from_yaml(&valid).is_ok());

    let command = CameraManifest::from_yaml(&manifest(
        "command: 55740, event: 55741",
        "          - { openChannel: command }",
    ))
    .expect_err("the already-established command channel cannot be opened by a plan");
    assert!(command
        .to_string()
        .contains("cannot open the command channel"));

    let unbound = CameraManifest::from_yaml(&manifest(
        "command: 55740",
        "          - { openChannel: event }",
    ))
    .expect_err("an auxiliary channel needs a socket binding");
    assert!(unbound.to_string().contains("has no socket binding"));

    let nested = CameraManifest::from_yaml(&manifest(
        "command: 55740, event: 55741",
        "          - retry:\n              whenResponseCodes: [\"0x2002\"]\n              maxAttempts: 2\n              steps: [{ openChannel: event }]",
    ))
    .expect_err("nested channel openings are not simulator-enforceable");
    assert!(nested.to_string().contains("only valid as a top-level"));

    let tolerant_tail = CameraManifest::from_yaml(&manifest(
        "command: 55740, event: 55741",
        "          - { sendOp: \"0x9999\", tolerant: true }\n          - { openChannel: event }",
    ))
    .expect_err("a tolerated tail cannot enforce the callback boundary");
    assert!(tolerant_tail
        .to_string()
        .contains("requires a preceding strict wire step"));
}

#[test]
fn response_retry_requires_a_finite_selected_body() {
    let manifest = |retry: &str| {
        format!(
            "schema: camera-config/v1\ncamera: {{ manufacturer: Test, model: Test, firmware: \"1\" }}\nconnections:\n  app:\n    entries:\n      - to: test\n        steps:\n          - retry:\n{retry}\n"
        )
    };
    let valid = manifest(
        "              whenResponseCodes: [\"0x2019\"]\n              maxAttempts: 2\n              retryDelayMs: 10\n              steps: [{ getProp: \"0xd620\" }]",
    );
    assert!(CameraManifest::from_yaml(&valid).is_ok());
    let classes_only = manifest(
        "              whenFailureClasses: [\"decode\"]\n              maxAttempts: 2\n              steps: [{ getProp: \"0xd620\" }]",
    );
    assert!(CameraManifest::from_yaml(&classes_only).is_ok());
    for invalid in [
        "              whenResponseCodes: []\n              maxAttempts: 2\n              steps: [{ getProp: \"0xd620\" }]",
        "              whenFailureClasses: [\"transport\"]\n              maxAttempts: 2\n              steps: [{ getProp: \"0xd620\" }]",
        "              whenResponseCodes: [\"not-hex\"]\n              maxAttempts: 2\n              steps: [{ getProp: \"0xd620\" }]",
        "              whenResponseCodes: [\"0x2019\"]\n              maxAttempts: 0\n              steps: [{ getProp: \"0xd620\" }]",
        "              whenResponseCodes: [\"0x2019\"]\n              maxAttempts: 2\n              steps: []",
        "              whenResponseCodes: [\"0x2019\"]\n              maxAttempts: 2\n              steps:\n                - loop:\n                    chunk:\n                      total: total\n                      size: { literal: 1 }\n                      offsetBind: offset\n                      lengthBind: length\n                      body: [{ sendOp: \"0x101b\" }]",
    ] {
        assert!(CameraManifest::from_yaml(&manifest(invalid)).is_err());
    }
}

#[test]
fn captured_collection_loop_requires_a_definite_nontolerant_data_step() {
    let manifest = |steps: &str| {
        format!(
            "schema: camera-config/v1\ncamera: {{ manufacturer: Test, model: Test, firmware: \"1\" }}\nconnections:\n  app:\n    entries:\n      - to: test\n        steps:\n{steps}\n"
        )
    };
    let capture = "          - getProp: \"0xd621\"\n            captures: [{ bind: handles, as: ptpU32Array }]";
    let loop_step = "          - loop:\n              forEach:\n                in: handles\n                bind: handle\n                body: [{ sendOp: \"0x1008\", params: [{ runtime: handle }] }]";
    let valid = CameraManifest::from_yaml(&manifest(&format!("{capture}\n{loop_step}")));
    assert!(valid.is_ok(), "valid collection loop: {:?}", valid.err());
    let send_capture = capture.replace("getProp: \"0xd621\"", "sendOp: \"0x1007\"");
    assert!(CameraManifest::from_yaml(&manifest(&format!("{send_capture}\n{loop_step}"))).is_ok());

    for invalid in [
        loop_step.to_string(),
        format!("{capture}\n{capture}\n{loop_step}").replace("bind: handles", "bind: ''"),
        format!("{capture}\n{loop_step}")
            .replace("captures:", "tolerant: true\n            captures:"),
    ] {
        assert!(CameraManifest::from_yaml(&manifest(&invalid)).is_err());
    }

    let tolerant_retry = "          - retry:\n              whenResponseCodes: [\"0x2002\"]\n              maxAttempts: 2\n              steps:\n                - getProp: \"0xd621\"\n                  captures: [{ bind: handles, as: ptpU32Array }]\n            tolerant: true\n";
    assert!(CameraManifest::from_yaml(&manifest(tolerant_retry)).is_err());
}

#[test]
fn object_transfer_contract_requires_reachable_matching_action_modes() {
    let manifest = |modes: &str, read_mode: &str, completion_mode: &str| {
        format!(
            r#"schema: camera-config/v1
camera: {{ manufacturer: Test, model: Test, firmware: "1" }}
media:
  formats:
    "0x3801": {{ name: jpeg }}
connections:
  app:
    commandFraming: compressed
    modes: [{modes}]
    objectTransfer:
      strategy: wholeObject
      resumePolicy: restartFromZero
      readAction: getObject
      completion: {{ action: deleteObject, after: localCommit }}
      formats: {{ "0x3801": confirmed }}
    actions:
      getObject:
        mode: {read_mode}
        initiator:
          params: [handle]
          steps: [{{ sendOp: "0x1009", params: [{{ runtime: handle }}] }}]
      deleteObject:
        mode: {completion_mode}
        initiator:
          params: [handle]
          steps: [{{ sendOp: "0x100b", params: [{{ runtime: handle }}] }}]
"#
        )
    };
    assert!(CameraManifest::from_yaml(&manifest(
        "image-transfer",
        "image-transfer",
        "image-transfer"
    ))
    .is_ok());
    assert!(CameraManifest::from_yaml(&manifest(
        "shooting/stills",
        "image-transfer",
        "image-transfer"
    ))
    .is_err());
    assert!(CameraManifest::from_yaml(&manifest(
        "image-transfer, shooting/stills",
        "image-transfer",
        "shooting/stills"
    ))
    .is_err());
}

#[test]
fn ptp_executor_activities_require_complete_ordered_coverage() {
    let manifest = r#"
schema: camera-config/v1
camera: { manufacturer: Test, model: Test, firmware: "1" }
connections:
  app:
    entries:
      - to: test
        steps:
          - { sendOp: "0x1001" }
          - { sendOp: "0x1002" }
        activities:
          - id: camera.test.prepare
            version: 1
            displayRole: preparingConnection
            defaultExpectedDurationMs: 10
            interactionRequired: false
            executorSpan: { sequence: steps, startStep: 0, endStepExclusive: 2 }
"#;
    let loaded = CameraManifest::from_yaml(manifest).expect("complete span loads");
    assert_eq!(loaded.connections["app"].entries[0].activities.len(), 1);

    let gap = manifest.replace("startStep: 0", "startStep: 1");
    let error = CameraManifest::from_yaml(&gap).expect_err("coverage gap rejected");
    assert!(error.to_string().contains("ordered coverage"));
}

#[test]
fn user_instruction_rejects_executor_activities() {
    let manifest = r#"
schema: camera-config/v1
camera: { manufacturer: Test, model: Test, firmware: "1" }
connections:
  usb:
    entries:
      - to: test
        userInstruction: choose a camera menu item
        activities:
          - id: camera.test.manual
            version: 1
            displayRole: preparingConnection
            defaultExpectedDurationMs: 10
            interactionRequired: true
            executorSpan: { sequence: steps, startStep: 0, endStepExclusive: 1 }
"#;
    let error = CameraManifest::from_yaml(manifest).expect_err("manual entry rejects spans");
    assert!(error.to_string().contains("userInstruction"));
}

#[test]
fn mode_entry_execution_is_mutually_exclusive() {
    let mixed = r#"
to: image-transfer
steps: []
reestablishConnection:
  exitSteps: []
  params: { launchMode: "3" }
"#;
    assert!(
        serde_yaml::from_str::<camera_config::ModeEntry>(mixed).is_err(),
        "a mode entry cannot combine execution variants"
    );

    let missing = "to: image-transfer\n";
    assert!(
        serde_yaml::from_str::<camera_config::ModeEntry>(missing).is_err(),
        "a mode entry requires one execution variant"
    );
}

#[test]
fn transaction_id_capture_rejects_ambiguous_steps() {
    let manifest = |step: &str| {
        format!(
            r#"
schema: camera-config/v1
camera: {{ manufacturer: Test, model: Test, firmware: "1" }}
connections:
  app:
    commandFraming: compressed
    entries:
      - to: test
        steps:
          - {{ {step} }}
"#
        )
    };
    for step in [
        r#"getProp: "0x5001", captures: [{ bind: tx, as: transactionId }]"#,
        r#"sendOp: "0x1001", repeat: 0, captures: [{ bind: tx, as: transactionId }]"#,
        r#"sendOp: "0x1001", repeat: 2, captures: [{ bind: tx, as: transactionId }]"#,
        r#"sendOp: "0x1001", tolerant: true, captures: [{ bind: tx, as: transactionId }]"#,
    ] {
        let error = CameraManifest::from_yaml(&manifest(step)).expect_err("invalid capture");
        assert!(error.to_string().contains("transactionId"), "{error}");
    }
}

#[test]
fn destructive_reestablishment_requires_a_runnable_cold_path() {
    let without_establishment = r#"
schema: camera-config/v1
camera: { manufacturer: Test, model: Test, firmware: "1" }
connections:
  app:
    entries:
      - { to: image-transfer, steps: [] }
      - to: image-transfer
        from: shooting/stills
        reestablishConnection: { exitSteps: [], params: { launchMode: "3" } }
"#;
    assert!(
        CameraManifest::from_yaml(without_establishment).is_err(),
        "re-establishment without a connection mechanism must not load"
    );

    let without_cold_entry = r#"
schema: camera-config/v1
camera: { manufacturer: Test, model: Test, firmware: "1" }
connections:
  app:
    establishment: test
    entries:
      - to: image-transfer
        from: shooting/stills
        reestablishConnection: { exitSteps: [], params: { launchMode: "3" } }
"#;
    assert!(
        CameraManifest::from_yaml(without_cold_entry).is_err(),
        "re-establishment without a cold PTP entry must not load"
    );
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
    assert_eq!(lock.initiator().unwrap().params, vec!["afArea".to_string()]);
    assert_eq!(
        lock.initiator().unwrap().steps[0].send_op.as_deref(),
        Some("0x9026")
    );
    assert_eq!(
        lock.initiator().unwrap().steps[0].params,
        vec![StepParam::Runtime {
            runtime: "afArea".into(),
            shift: 0,
            mask: None,
        }]
    );
    let aw = lock.initiator().unwrap().steps[1]
        .await_until
        .as_ref()
        .expect("AF await step");
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
    assert!(lock
        .initiator()
        .unwrap()
        .steps
        .iter()
        .all(camera_config::Step::is_well_formed));

    // Release recipe: single 0x9027.
    let release = m
        .action("app", ActionVerb::AutofocusRelease)
        .expect("app.actions.autofocusRelease");
    assert_eq!(release.initiator().unwrap().steps.len(), 1);
    assert_eq!(
        release.initiator().unwrap().steps[0].send_op.as_deref(),
        Some("0x9027")
    );
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
