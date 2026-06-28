//! Exercises the FFI seam against the REAL camera-config-data files — the proof
//! that the `(connection, mode)` surface works end-to-end before Swift bindings.

use camera_protocol_ffi::*;
use std::path::PathBuf;

fn data(rel: &str) -> String {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../packages/camera-config-data")
        .join(rel);
    std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("read {}: {e}", p.display()))
}

fn store() -> std::sync::Arc<ConfigStore> {
    ConfigStore::from_bundle(
        data("fuji/gfx100ii/gfx100ii.yaml"),
        Some(data("fuji/fuji.yaml")),
    )
    .expect("bundle loads")
}

fn ids(cs: &[ConnectionInfo]) -> Vec<&str> {
    cs.iter().map(|c| c.id.as_str()).collect()
}

#[test]
fn platform_filters_connections_macos_vs_ios() {
    let s = store();
    let mac = s.connections(Platform::Macos);
    let ios = s.connections(Platform::Ios);
    // macOS sees USB + the wired tether; iOS does not (platforms: excludes it).
    assert!(ids(&mac).contains(&"usb"), "macOS has USB");
    assert!(ids(&mac).contains(&"wireless-tether"));
    assert!(
        !ids(&ios).contains(&"usb"),
        "iOS hides USB — same build, data-driven"
    );
    // App + XLV available to both.
    assert!(ids(&ios).contains(&"app"));
    assert!(ids(&mac).contains(&"xlv"));
}

#[test]
fn operation_gating_is_connection_and_mode_keyed() {
    let s = store();
    // 0x9018 (tether live-view) is available over the tether, wrong-connection over app.
    assert!(matches!(
        s.operation_available(
            "wireless-tether".into(),
            "shooting/stills".into(),
            0x9018,
            vec![]
        ),
        Availability::Available
    ));
    assert!(matches!(
        s.operation_available("app".into(), "shooting/stills".into(), 0x9018, vec![]),
        Availability::WrongConnection
    ));
    // Backup op (0x100c) is available in ANY mode over usb (modes: []).
    assert!(matches!(
        s.operation_available("usb".into(), "shooting/stills".into(), 0x100c, vec![]),
        Availability::Available
    ));
    // A genuine WrongMode: the raw-conv op (0x900c) is mode-specific.
    assert!(matches!(
        s.operation_available("usb".into(), "shooting/stills".into(), 0x900c, vec![]),
        Availability::WrongMode
    ));
}

#[test]
fn control_mechanism_varies_by_connection() {
    let s = store();
    let ctl = s
        .control_for("wireless-tether".into(), "shooting/stills".into(), 0x5007)
        .expect("aperture control over tether");
    assert_eq!(ctl.set_method.as_deref(), Some("absolute"));
    assert_eq!(ctl.operation, Some(0x1016));

    let app_aperture = s
        .control_for("app".into(), "shooting/stills".into(), 0x5007)
        .expect("aperture control over app");
    assert_eq!(app_aperture.set_method.as_deref(), Some("vendorStep"));
    assert_eq!(app_aperture.operation, Some(0x902d));
    assert_eq!(app_aperture.readback, Some(0xd212));

    let app_iso = s
        .control_for("app".into(), "shooting/stills".into(), 0xd02a)
        .expect("ISO control over app");
    assert_eq!(app_iso.set_method.as_deref(), Some("absolute"));
    assert_eq!(app_iso.operation, Some(0x1016));
    assert_eq!(app_iso.readback, Some(0xd212));
}

#[test]
fn app_current_behavior_ops_gate_through_ffi() {
    let s = store();
    assert!(matches!(
        s.operation_available("app".into(), "shooting/stills".into(), 0x9026, vec![]),
        Availability::Available
    ));
    assert!(matches!(
        s.operation_available("app".into(), "shooting/stills".into(), 0x100e, vec![]),
        Availability::Available
    ));
    assert!(matches!(
        s.operation_available("app".into(), "image-transfer".into(), 0x1008, vec![]),
        Availability::Available
    ));
    assert!(matches!(
        s.operation_available("app".into(), "image-transfer".into(), 0x101b, vec![]),
        Availability::Available
    ));
}

#[test]
fn mode_entry_returns_the_ground_truth_wire_steps() {
    let s = store();
    let plan = s
        .mode_entry("app".into(), None, "shooting/stills".into())
        .expect("live-view entry");
    assert!(plan.user_instruction.is_none());
    // First step: SetProp 0xdf00 = 6 (the real live-view startup constant).
    match &plan.steps[0] {
        EntryStep::SetProp { prop, value, .. } => {
            assert_eq!(*prop, 0xdf00);
            assert_eq!(*value, 6);
        }
        other => panic!("expected SetProp, got {other:?}"),
    }
    // The 902B repeat survives the round-trip.
    assert!(plan.steps.iter().any(|st| matches!(
        st,
        EntryStep::SendOp {
            op: 0x902b,
            repeat: 4,
            ..
        }
    )));

    // A USB sub-mode entry is a userInstruction (camera menu), no steps.
    let usb = s
        .mode_entry("usb".into(), None, "raw-conv-backup-restore".into())
        .unwrap();
    assert!(usb.user_instruction.is_some());
    assert!(usb.steps.is_empty());
}

#[test]
fn connection_establishment_is_returned_as_data() {
    let s = store();
    // wireless-tether: PCSS knock params surfaced for the app to drive.
    let wt = s
        .connection_establishment("wireless-tether".into())
        .unwrap();
    assert_eq!(wt.mechanism.as_deref(), Some("pcss-knock"));
    assert!(wt
        .params
        .iter()
        .any(|kv| kv.key == "knockPort" && kv.value == "51562"));
    // app is brought up via the BLE→WiFi handover.
    let app = s.connection_establishment("app".into()).unwrap();
    assert_eq!(app.mechanism.as_deref(), Some("ble-establish-wifi-ap"));
}

#[test]
fn value_policy_resolves_initiator_identity_from_manufacturer_tier() {
    let s = store();
    match s.value("initiatorGuid".into()) {
        Some(ResolvedValue::Fixed { value }) => {
            assert_eq!(value, "f2e4538fada5485d87b27f0bd3d5ded0");
        }
        other => panic!("expected fixed initiator GUID, got {other:?}"),
    }
}

#[test]
fn explained_gate_traces_real_data_decisions() {
    let s = store();
    // 0x900c is a (usb, raw-conv-backup-restore) op. Over the app connection → WrongConnection,
    // and the trace says why (what telemetry captures) — no predicate eval needed.
    let wc = s.operation_available_explained(
        "app".into(),
        "raw-conv-backup-restore".into(),
        0x900c,
        vec![],
    );
    assert!(matches!(wc.availability, Availability::WrongConnection));
    assert!(!wc.trace.connection_ok);
    assert!(wc.trace.requires.is_none()); // this op declares no prerequisite
    assert!(wc.trace.reason.contains("usb"));
    // Over its own connection/mode → Available, both axes ok.
    let ok = s.operation_available_explained(
        "usb".into(),
        "raw-conv-backup-restore".into(),
        0x900c,
        vec![],
    );
    assert!(matches!(ok.availability, Availability::Available));
    assert!(ok.trace.connection_ok && ok.trace.mode_ok);
    // Unknown op → Unavailable with an explanatory reason.
    let un =
        s.operation_available_explained("app".into(), "shooting/stills".into(), 0x9999, vec![]);
    assert!(matches!(un.availability, Availability::Unavailable));
    assert!(un.trace.reason.contains("not defined"));
}

#[test]
fn explained_gate_carries_predicate_leaf_detail_through_ffi() {
    // Synthetic manifest (FFI plumbing test, NOT camera facts): an op with a
    // `requires` prerequisite so the leaf-eval detail flows through the boundary.
    let yaml = r#"
schema: camera-config/v1
camera: { manufacturer: FUJIFILM, model: GFX100 II, firmware: "2.30" }
operations:
  "0x101c": { name: OpenCap, modes: [shooting], connections: [app], requires: { prop: "0xd212", mask: 0x00ff, ne: 0 } }
connections: { app: { kind: ptpip-app } }
modes: { "shooting/stills": {} }
"#;
    let s = ConfigStore::from_bundle(yaml.to_string(), None).expect("loads");
    // Low byte masks to 0 → `ne 0` fails → Blocked, and the leaf shows exactly why.
    let g = s.operation_available_explained(
        "app".into(),
        "shooting/stills".into(),
        0x101c,
        vec![PropObservation {
            code: 0xd212,
            value: 0xab00,
        }],
    );
    assert!(matches!(g.availability, Availability::Blocked));
    let req = g.trace.requires.expect("requires evaluated");
    assert!(!req.passed);
    let leaf = &req.leaves[0];
    assert_eq!(leaf.prop, "0xd212");
    assert_eq!(leaf.observed, Some(0xab00));
    assert_eq!(leaf.effective, Some(0x00));
    assert!(!leaf.passed);
    // Satisfy it → Available.
    let ok = s.operation_available_explained(
        "app".into(),
        "shooting/stills".into(),
        0x101c,
        vec![PropObservation {
            code: 0xd212,
            value: 0xab01,
        }],
    );
    assert!(matches!(ok.availability, Availability::Available));
    assert!(ok.trace.requires.unwrap().passed);
}

#[test]
fn property_value_width_resolves_from_manifest_type() {
    let s = store();
    // u16/u32/i16/i32 map to encoder widths; u8a (rawSettings) and unknown props → None.
    assert!(matches!(
        s.property_value_width(0x5007),
        Some(ValueWidth::U16)
    )); // aperture u16
    assert!(matches!(
        s.property_value_width(0xdf28),
        Some(ValueWidth::U32)
    )); // featureVersion u32
    assert!(matches!(
        s.property_value_width(0xd02a),
        Some(ValueWidth::U32)
    )); // App still ISO u32 — literal/auto, degenerate u16 stub overridden (#100)
    assert!(matches!(
        s.property_value_width(0xd240),
        Some(ValueWidth::U32)
    )); // shutter u32 — 0x80000000|denom*1000, stub overridden (#100)
    assert!(matches!(
        s.property_value_width(0x5010),
        Some(ValueWidth::I16)
    )); // exposure bias i16 — signed (#88)
    assert!(matches!(
        s.property_value_width(0x500f),
        Some(ValueWidth::I32)
    )); // standard ISO (ExposureIndex) i32 — signed, auto sentinels (#88)
    assert!(matches!(
        s.property_value_width(0xd226),
        Some(ValueWidth::U16)
    )); // imageImportFilter u16
    assert!(matches!(
        s.property_value_width(0xd227),
        Some(ValueWidth::U16)
    )); // imageImportSort u16
    assert!(s.property_value_width(0xd185).is_none()); // rawSettings u8a → unsupported
    assert!(s.property_value_width(0x9999).is_none()); // unknown property
}

#[test]
fn property_payload_surfaces_d212_record_stream() {
    let s = store();
    // 0xD212 live-status is a record stream the app walks; the descriptor + its
    // member allowlist must survive the FFI boundary intact (a dropped `members`
    // would silently lose the poll set the consumer keys on).
    let p = s
        .property_payload(0xd212)
        .expect("0xD212 carries a payload descriptor");
    assert!(matches!(p.form, PayloadForm::RecordStream));
    assert_eq!(p.count_width, Some(2));
    let rec = p.record.expect("record layout present");
    assert_eq!((rec.code_width, rec.value_width), (2, 4));
    assert!(p.members.contains(&0xd17c)); // s1Lock
    assert!(p.members.contains(&0xd209)); // s1LockColor
    assert!(p.members.contains(&0x5007)); // aperture
    assert!(s.property_payload(0x5007).is_none()); // a scalar property → no payload
}

#[test]
fn take_to_get_entry_switches_in_session_without_reopen() {
    // #103: the from-live-view image-transfer entry switches functionMode IN-SESSION
    // — no reopenSession. The real GFX100 II refuses the reconnect after the
    // transport-close (the `app` connection's commandListenerVolatile trait), so
    // 0x1018 (TerminateOpenCapture) is followed by the 0xd212 read on the existing
    // socket, then 0xDF01=0x14 (FunctionMode=Image-Import). The earlier reopen here
    // was the misdiagnosed "image transfer downloads 0 files" bug.
    let s = store();
    let plan = s
        .mode_entry(
            "app".into(),
            Some("shooting/stills".into()),
            "image-transfer".into(),
        )
        .expect("from-Stills image-import entry");
    assert!(matches!(
        plan.steps[0],
        EntryStep::SendOp { op: 0x1018, .. }
    ));
    assert!(
        matches!(plan.steps[1], EntryStep::GetProp { prop: 0xd212, .. }),
        "0x1018 is followed by the in-session 0xd212 read, not a reopen: {:?}",
        plan.steps[1]
    );
    assert!(
        !plan
            .steps
            .iter()
            .any(|st| matches!(st, EntryStep::ReopenSession { .. })),
        "the take→get switch must stay in-session (#103)"
    );
    // The DF01=0x14 follow-up is still present (the prefix didn't get truncated).
    assert!(plan.steps.iter().any(|st| matches!(
        st,
        EntryStep::SetProp {
            prop: 0xdf01,
            value: 0x14,
            ..
        }
    )));
}

#[test]
fn action_returns_pcss_shutter_with_images_pushed_trigger() {
    // wireless-tether shutter — the wire-confirmed 3-beat virtual-shutter
    // (setProp 0xD039 phases + sendOp 0x100E). triggers: [ImagesPushed{1,3}]
    // because PCSS auto-pushes 1-3 images depending on user's JPEG/HEIF/RAW.
    let s = store();
    let shutter = s
        .action("wireless-tether".into(), ActionVerb::Shutter)
        .expect("wireless-tether.actions.shutter");
    assert_eq!(shutter.mode, "shooting/stills");
    assert!(shutter.params.is_empty());
    assert_eq!(shutter.steps.len(), 6); // 3 beats × 2 ops each
                                        // Trigger surfaces as a tagged enum with min/max payload.
    assert_eq!(shutter.triggers.len(), 1);
    assert!(
        matches!(
            shutter.triggers[0],
            ActionEffect::ImagesPushed { min: 1, max: 3 }
        ),
        "expected ImagesPushed{{1,3}}, got {:?}",
        shutter.triggers[0]
    );
}

#[test]
fn action_returns_app_shutter_with_postview_event_trigger() {
    // Same verb, different connection — the reference app shutter take cycle (#29):
    // 0x100E → awaitUntil the 0xC001 PostviewComplete event → 0x9022 read.
    let s = store();
    let shutter = s
        .action("app".into(), ActionVerb::Shutter)
        .expect("app.actions.shutter");
    assert_eq!(shutter.steps.len(), 3);
    assert!(matches!(
        shutter.steps[0],
        EntryStep::SendOp { op: 0x100e, .. }
    ));
    // The postview await surfaces as an event-source AwaitUntil; a dropped step
    // would silently break the manifest-scripted take cycle.
    assert!(matches!(
        &shutter.steps[1],
        EntryStep::AwaitUntil {
            source: FfiAwaitSource::Event {
                code: 0xc001,
                then_poll: None
            },
            ..
        }
    ));
    assert!(matches!(
        shutter.steps[2],
        EntryStep::SendOp { op: 0x9022, .. }
    ));
    assert!(matches!(shutter.triggers[0], ActionEffect::PostviewEvent));
}

#[test]
fn action_getobject_params_differ_per_connection_same_verb() {
    // PCSS getObject: whole-object 0x1009 with params: [handle].
    // reference app getObject: chunked 0x101B with params: [handle, offset, length].
    // Consumer reads .params at the call site to know what to bind.
    let s = store();
    let pcss = s
        .action("wireless-tether".into(), ActionVerb::GetObject)
        .expect("wireless-tether.actions.getObject");
    let app = s
        .action("app".into(), ActionVerb::GetObject)
        .expect("app.actions.getObject");
    assert_eq!(pcss.params, vec!["handle".to_string()]);
    assert_eq!(
        app.params,
        vec![
            "handle".to_string(),
            "offset".to_string(),
            "length".to_string()
        ]
    );
    assert!(matches!(
        pcss.steps[0],
        EntryStep::SendOp { op: 0x1009, .. }
    ));
    assert!(matches!(
        app.steps[0],
        EntryStep::SendOp { op: 0x101b, .. }
    ));
}

#[test]
fn action_misses_when_connection_does_not_declare_the_verb() {
    let s = store();
    // ble has no transfer actions.
    assert!(s.action("ble".into(), ActionVerb::GetObject).is_none());
    // app does NOT model DeleteObject (no wire-truth for reference app).
    assert!(s.action("app".into(), ActionVerb::DeleteObject).is_none());
    // Unknown connection.
    assert!(s
        .action("nonexistent".into(), ActionVerb::Shutter)
        .is_none());
}

#[test]
fn runtime_param_slot_surfaces_through_ffi() {
    let s = store();
    // The from-live-view image-transfer entry: 0x1018 carries a runtime slot the app binds.
    let plan = s
        .mode_entry(
            "app".into(),
            Some("shooting/stills".into()),
            "image-transfer".into(),
        )
        .expect("from-Stills image-import entry");
    match &plan.steps[0] {
        EntryStep::SendOp {
            op,
            params,
            tolerant: _,
            repeat: _,
        } => {
            assert_eq!(*op, 0x1018);
            assert!(
                matches!(&params[0], EntryParam::Runtime { slot } if slot == "openCaptureTxId")
            );
        }
        other => panic!("expected SendOp 0x1018, got {other:?}"),
    }
    // A tolerant vendor-prime op with literal params also round-trips.
    assert!(plan.steps.iter().any(|st| matches!(
        st,
        EntryStep::SendOp {
            op: 0x9053,
            tolerant: true,
            ..
        }
    )));
}

#[test]
fn from_tiers_applies_fw_overlay_through_ffi() {
    let s = ConfigStore::from_tiers(
        data("fuji/gfx100ii/gfx100ii.yaml"),
        Some(data("fuji/fuji.yaml")),
        vec![data("fuji/gfx100ii/fw2.40.yaml")],
    )
    .expect("tiered bundle loads");
    // The fw2.40 overlay applied: XLV connection still present (HTTPS is in extra),
    // and the rest of the seam still answers. Smoke that the merged store works.
    let xlv = &s.connections(Platform::Macos);
    assert!(xlv.iter().any(|c| c.id == "xlv"));
    // Manufacturer tier still resolves through the tiered constructor.
    assert!(matches!(
        s.value("initiatorGuid".into()),
        Some(ResolvedValue::Fixed { .. })
    ));
}

#[test]
fn detect_mode_from_observed_function_mode() {
    let s = store();
    let obs = vec![PropObservation {
        code: 0xdf01,
        value: 0x16,
    }];
    assert_eq!(
        s.detect_mode("app".into(), obs).as_deref(),
        Some("shooting/stills")
    );
}

#[test]
fn connection_init_assembles_the_82_byte_app_packet_from_manifest_data() {
    let s = store();
    let init = s
        .connection_init("app".into())
        .expect("the app connection declares an init shape");

    // Identity resolved from `values:`, tail decoded from the manifest — the
    // packet is assembled with zero client-side literals (#82).
    assert_eq!(init.guid.len(), 16, "GUID is 16 bytes");
    assert_eq!(init.tail.len(), 28, "vendor tail is 28 bytes");
    assert_eq!(init.name_field_byte_count, 26);
    assert_eq!(init.packet.len(), 82, "the canonical reference app init is 82 bytes");
    // Structure: u32 length == 82, GUID at 8..24, tail at 54..82.
    assert_eq!(
        u32::from_le_bytes(init.packet[0..4].try_into().unwrap()),
        82
    );
    assert_eq!(&init.packet[8..24], &init.guid[..]);
    assert_eq!(&init.packet[54..82], &init.tail[..]);

    // A connection with no init shape returns None.
    assert!(s.connection_init("usb".into()).is_none());
}

#[test]
fn connection_info_carries_per_connection_traits() {
    let s = store();
    let conns = s.connections(Platform::Macos); // app + wireless-tether both visible
    let app = conns.iter().find(|c| c.id == "app").expect("app present");
    assert_eq!(app.init_shape.as_deref(), Some("app82"));
    assert!(matches!(
        app.shutter_recipe,
        Some(ShutterRecipe::AppPostview)
    ));
    assert!(matches!(
        app.live_view_delivery.as_ref().map(|d| &d.kind),
        Some(LiveViewDeliveryKind::Stream)
    ));

    let wt = conns
        .iter()
        .find(|c| c.id == "wireless-tether")
        .expect("tether present");
    let lv = wt
        .live_view_delivery
        .as_ref()
        .expect("tether polls live view");
    assert!(matches!(lv.kind, LiveViewDeliveryKind::Poll));
    assert_eq!(lv.poll_op, Some(0x9018)); // hex string → u16 across the FFI
    assert!(matches!(
        wt.shutter_recipe,
        Some(ShutterRecipe::WirelessTether3Beat)
    ));

    // usb declares no traits → None (the app falls back, no negative list).
    let usb = conns.iter().find(|c| c.id == "usb").expect("usb on macOS");
    assert!(usb.shutter_recipe.is_none());
    assert!(usb.live_view_delivery.is_none());
}

#[test]
fn autofocus_lock_action_surfaces_the_event_source_recipe() {
    let s = store();
    let lock = s
        .action("app".into(), ActionVerb::AutofocusLock)
        .expect("app autofocusLock action");
    assert_eq!(lock.params, vec!["afArea".to_string()]);
    assert!(matches!(
        lock.steps[0],
        EntryStep::SendOp { op: 0x9026, .. }
    ));
    // The AF await surfaces as an event-source AwaitUntil (event 0xC005 → read 0xD209).
    assert!(matches!(
        &lock.steps[1],
        EntryStep::AwaitUntil {
            source: FfiAwaitSource::Event {
                code: 0xc005,
                then_poll: Some(0xd209)
            },
            ..
        }
    ));
    let release = s
        .action("app".into(), ActionVerb::AutofocusRelease)
        .expect("app autofocusRelease action");
    assert!(matches!(
        release.steps[0],
        EntryStep::SendOp { op: 0x9027, .. }
    ));
    // A connection without the verb returns None.
    assert!(s
        .action("wireless-tether".into(), ActionVerb::AutofocusLock)
        .is_none());
}

#[test]
fn property_catalog_enumerates_through_ffi() {
    let s = store();
    let cat = s.properties();
    assert!(
        cat.len() > 20,
        "the catalog enumerates many props: {}",
        cat.len()
    );
    // A representative scalar property surfaces with its metadata.
    let aperture = cat
        .iter()
        .find(|p| p.code == 0x5007)
        .expect("aperture in the catalog");
    assert_eq!(aperture.name, "aperture");
    assert_eq!(aperture.ptype.as_deref(), Some("u16"));
    assert_eq!(aperture.access.as_deref(), Some("readWrite"));
    // The newly-declared 0xD212 member is enumerable.
    assert!(cat
        .iter()
        .any(|p| p.code == 0xd028 && p.name == "depthOfField"));
}

#[test]
fn media_format_table_classifies_objects_through_ffi() {
    let s = store();
    let raf = s.media_format(0xb103).expect("RAF in the format table");
    assert_eq!(raf.name, "raf");
    assert_eq!(raf.vendor.as_deref(), Some("fuji"));
    assert!(raf.is_raw && !raf.is_movie);

    let mov = s.media_format(0x300d).expect("MOV in the table");
    assert!(mov.is_movie && !mov.is_raw);

    let jpeg = s.media_format(0x3801).expect("JPEG in the table");
    assert!(!jpeg.is_raw && !jpeg.is_movie);

    // An unknown / unlisted format code → None.
    assert!(s.media_format(0x9999).is_none());
}
