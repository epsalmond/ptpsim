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

fn consolidated_store() -> std::sync::Arc<ConfigStore> {
    ConfigStore::from_bundle(
        data("fuji/gfx100ii/gfx100ii.consolidated.yaml"),
        Some(data("fuji/fuji.yaml")),
    )
    .expect("consolidated bundle loads")
}

fn ids(cs: &[ConnectionInfo]) -> Vec<&str> {
    cs.iter().map(|c| c.id.as_str()).collect()
}

fn assert_bootstrap_tail_surfaces(steps: &[EntryStep]) {
    let d22b = steps
        .iter()
        .position(|st| matches!(st, EntryStep::GetProp { prop: 0xd22b, .. }))
        .expect("D22B bootstrap read crosses FFI");
    let page = steps
        .iter()
        .position(|st| matches!(st, EntryStep::SendOp { op: 0x9053, .. }))
        .expect("0x9053 page op crosses FFI");
    let final_d212 = steps
        .iter()
        .enumerate()
        .skip(page + 1)
        .find_map(|(i, st)| matches!(st, EntryStep::GetProp { prop: 0xd212, .. }).then_some(i))
        .expect("final D212 read crosses FFI after 0x9053");
    assert!(d22b < page && page < final_d212);
    match &steps[page] {
        EntryStep::SendOp { params, .. } => {
            assert!(matches!(
                params.as_slice(),
                [
                    EntryParam::Literal { value: 0 },
                    EntryParam::Literal { value: 0x7530 }
                ]
            ));
        }
        other => panic!("expected 0x9053 SendOp, got {other:?}"),
    }
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
fn property_value_codec_crosses_the_ffi_seam() {
    let s = consolidated_store();
    let decoded = s
        .decode_property(0xd02a, 0x8000_1900)
        .expect("auto ISO decodes through the manifest");
    assert_eq!(decoded.raw, 0x8000_1900);
    assert_eq!(decoded.label, "AUTO 6400");
    assert_eq!(
        s.encode_property(0xd02a, "AUTO 6400".into())
            .expect("auto ISO encodes"),
        vec![0x00, 0x19, 0x00, 0x80]
    );

    let iso = s
        .properties()
        .into_iter()
        .find(|p| p.code == 0xd02a)
        .expect("still ISO in catalog");
    assert!(iso.value_rows.iter().any(|row| row.label == "6400"));
    let sentinel = iso
        .value_encoding
        .as_ref()
        .and_then(|enc| enc.sentinel.as_ref())
        .expect("sentinel metadata crosses the seam");
    assert_eq!(sentinel.mask, 0x8000_0000);
    assert_eq!(sentinel.meaning.as_deref(), Some("autoCeiling"));
    assert_eq!(sentinel.label_prefix, "AUTO");
    let encoding = iso
        .value_encoding
        .expect("encoding metadata crosses the seam");
    assert!(
        encoding.masks.iter().any(|mask| mask.mask == 0x4000_0000
            && mask.meaning.as_deref() == Some("extendedSensitivity")),
        "extended-sensitivity mask must cross the FFI seam"
    );
    let profile = iso
        .value_profiles
        .iter()
        .find(|profile| profile.connection.as_deref() == Some("app"))
        .expect("still ISO app value profile crosses the seam");
    assert!(profile.rows.iter().any(|row| row.raw == 80 && row.legal));
    assert!(profile
        .rows
        .iter()
        .any(|row| row.raw == 50 && !row.legal && row.write_store_raw == Some(80)));
}

#[test]
fn property_sentinel_codec_does_not_require_legacy_label_rows() {
    let yaml = r#"
schema: camera-config/v1
camera: { manufacturer: Test, model: Body, firmware: "1.0" }
properties:
  "0xd02a":
    name: stillIso
    type: u32
    access: readWrite
    valueRows:
      - { label: "6400", raw: 6400 }
    valueEncoding:
      sentinel: { mask: 2147483648, meaning: autoCeiling, labelPrefix: AUTO }
"#;
    let s = ConfigStore::from_bundle(yaml.to_string(), None).expect("bundle loads");
    assert_eq!(
        s.decode_property(0xd02a, 0x8000_1900)
            .expect("sentinel decode")
            .label,
        "AUTO 6400"
    );
    assert_eq!(
        s.encode_property(0xd02a, "AUTO 6400".into())
            .expect("sentinel encode"),
        vec![0x00, 0x19, 0x00, 0x80]
    );
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
    assert_bootstrap_tail_surfaces(&plan.steps);
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
fn get_to_take_entry_reopens_then_starts_live_view() {
    let s = store();
    let plan = s
        .mode_entry(
            "app".into(),
            Some("image-transfer".into()),
            "shooting/stills".into(),
        )
        .expect("from-image-transfer live-view entry");
    assert!(matches!(
        plan.steps[0],
        EntryStep::ReopenSession { tolerant: false }
    ));
    assert!(plan.steps.iter().any(|st| matches!(
        st,
        EntryStep::SetProp {
            prop: 0xdf01,
            value: 0x16,
            ..
        }
    )));
    assert!(plan.steps.iter().any(|st| matches!(
        st,
        EntryStep::SetProp {
            prop: 0xdf2a,
            value: 2,
            ..
        }
    )));
    assert!(plan.steps.iter().any(|st| matches!(
        st,
        EntryStep::SendOp {
            op: 0x902b,
            repeat: 4,
            ..
        }
    )));
    assert!(matches!(
        plan.steps.last(),
        Some(EntryStep::SendOp { op: 0x101c, .. })
    ));
    assert!(
        !plan
            .steps
            .iter()
            .any(|st| matches!(st, EntryStep::SendOp { op: 0x1018, .. })),
        "Get→Take must not terminate a non-existent live-view stream"
    );
}

#[test]
fn d246_stills_video_edges_surface_through_ffi() {
    let s = store();
    let to_video = s
        .mode_entry(
            "app".into(),
            Some("shooting/stills".into()),
            "shooting/video".into(),
        )
        .expect("stills→video selector");
    assert_eq!(to_video.steps.len(), 1);
    assert!(matches!(
        to_video.steps[0],
        EntryStep::SetProp {
            prop: 0xd246,
            value: 1,
            tolerant: false,
        }
    ));

    let to_stills = s
        .mode_entry(
            "app".into(),
            Some("shooting/video".into()),
            "shooting/stills".into(),
        )
        .expect("video→stills selector");
    assert_eq!(to_stills.steps.len(), 1);
    assert!(matches!(
        to_stills.steps[0],
        EntryStep::SetProp {
            prop: 0xd246,
            value: 0,
            tolerant: false,
        }
    ));
}

#[test]
fn read_device_info_action_pairs_with_the_device_info_codec() {
    // The #173 seam: opcode from the manifest action, layout from the FFI
    // codec — the app spells neither.
    let s = store();
    let read = s
        .action("wireless-tether".into(), ActionVerb::ReadDeviceInfo)
        .expect("wireless-tether.actions.readDeviceInfo");
    assert_eq!(read.mode, "", "identity read is not mode-gated");
    assert!(matches!(
        read.steps[0],
        EntryStep::SendOp { op: 0x1001, .. }
    ));
    // NOT authored on `app`: the reference app never sends 0x1001 on the reference app
    // channel (v6 wire-level run: zero 0x1001 frames) — evidence-deferred.
    assert!(s.action("app".into(), ActionVerb::ReadDeviceInfo).is_none());

    // Codec half: a real DeviceInfo dataset round-trips, serial included.
    let di = ptp_core::DeviceInfo {
        standard_version: 100,
        manufacturer: "FUJIFILM".into(),
        model: "GFX100 II".into(),
        device_version: "2.30".into(),
        serial_number: "PTPSIM-GFX100II-0001".into(),
        operations_supported: vec![0x1001, 0x1002],
        ..Default::default()
    };
    let mut w = Writer::new();
    di.encode(&mut w).unwrap();
    let parsed = parse_device_info(w.into_vec()).unwrap();
    assert_eq!(parsed.serial_number, "PTPSIM-GFX100II-0001");
    assert_eq!(parsed.model, "GFX100 II");
    assert_eq!(parsed.operations_supported, vec![0x1001, 0x1002]);
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
        EntryStep::SendOp {
            op: 0x1009,
            ref params,
            ..
        } if params.len() == 1
    ));
    assert!(matches!(
        app.steps[0],
        EntryStep::SendOp {
            op: 0x101b,
            ref params,
            ..
        } if matches!(
            params.as_slice(),
            [
                EntryParam::Runtime { slot: h, shift: 0, mask: None },
                EntryParam::Runtime { slot: o1, shift: 0, mask: Some(0xffff_ffff) },
                EntryParam::Runtime { slot: l, shift: 0, mask: None },
                EntryParam::Runtime { slot: o2, shift: 32, mask: None },
            ] if h == "handle" && o1 == "offset" && l == "length" && o2 == "offset"
        )
    ));
}

#[test]
fn action_import_objects_surfaces_the_nested_transfer_loop() {
    // #46/#187: the full image-transfer choreography surfaces as ONE action whose
    // steps nest a forEach(handle) loop over true-size capture + chunk download.
    // The hand-written FFI mirror must not silently drop captures, conditionals,
    // runtime chunk size, or shifted params.
    let s = store();
    let plan = s
        .action("app".into(), ActionVerb::ImportObjects)
        .expect("app.actions.importObjects");
    assert_eq!(plan.mode, "image-transfer");
    assert_bootstrap_tail_surfaces(&plan.steps);

    // The forEach iterates the 0xd621 handle list, binding `handle`.
    let for_each = plan
        .steps
        .iter()
        .find_map(|st| match st {
            EntryStep::Loop {
                kind:
                    FfiLoopKind::ForEach {
                        in_prop,
                        bind,
                        body,
                    },
                ..
            } => Some((*in_prop, bind.as_str(), body)),
            _ => None,
        })
        .expect("importObjects nests a forEach over the handle list");
    assert_eq!(for_each.0, 0xd621);
    assert_eq!(for_each.1, "handle");

    assert!(for_each.2.iter().any(|st| matches!(
        st,
        EntryStep::SendOp {
            op: 0x1008,
            captures,
            ..
        } if captures.iter().any(|c| c.bind == "objectReportedSize"
            && matches!(c.source, CaptureSourceInfo::ObjectInfoCompressedSize))
            && captures.iter().any(|c| c.bind == "objectTransferSize"
            && matches!(c.source, CaptureSourceInfo::ObjectInfoCompressedSize))
    )));
    assert!(for_each.2.iter().any(|st| matches!(
        st,
        EntryStep::If {
            slot,
            equals: 0xffff_ffff,
            then_steps,
            ..
        } if slot == "objectReportedSize"
            && matches!(then_steps.as_slice(), [EntryStep::SendOp { op: 0x9803, captures, .. }]
                if captures.iter().any(|c| c.bind == "objectTransferSize"
                    && matches!(c.source, CaptureSourceInfo::U64Le)))
    )));
    assert!(for_each.2.iter().any(|st| matches!(
        st,
        EntryStep::GetProp {
            prop: 0xd235,
            captures,
            ..
        } if captures.iter().any(|c| c.bind == "chunkSize"
            && matches!(c.source, CaptureSourceInfo::PropValue))
    )));
    let chunk = for_each
        .2
        .iter()
        .find_map(|st| match st {
            EntryStep::Loop {
                kind:
                    FfiLoopKind::Chunk {
                        total, size, body, ..
                    },
                ..
            } => Some((total.as_str(), size, body)),
            _ => None,
        })
        .expect("forEach body nests a chunk loop");
    assert_eq!(
        chunk.0, "objectTransferSize",
        "chunk total names the true transfer size slot"
    );
    assert!(
        matches!(chunk.1, FfiChunkSize::Runtime { slot } if slot == "chunkSize"),
        "chunk size comes from the captured 0xd235 property",
    );
    assert!(
        matches!(chunk.2.as_slice(), [EntryStep::SendOp { op: 0x101b, params, .. }]
            if matches!(params.as_slice(), [
                EntryParam::Runtime { slot: h, shift: 0, mask: None },
                EntryParam::Runtime { slot: o1, shift: 0, mask: Some(0xffff_ffff) },
                EntryParam::Runtime { slot: l, shift: 0, mask: None },
                EntryParam::Runtime { slot: o2, shift: 32, mask: None },
            ] if h == "handle" && o1 == "offset" && l == "length" && o2 == "offset")),
        "chunk body is the GetPartialObject download",
    );
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
        EntryStep::SendOp { op, params, .. } => {
            assert_eq!(*op, 0x1018);
            assert!(matches!(
                &params[0],
                EntryParam::Runtime {
                    slot,
                    shift: 0,
                    mask: None,
                } if slot == "openCaptureTxId"
            ));
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
fn client_derived_friendly_name_defers_the_init_packet() {
    // #109: the PTP/IP friendly name is the host's OWN device name — client-derived
    // from the `terminalName` slot, identical to the BLE deviceNameString — NOT a
    // manifest literal. So the manifest exposes the policy (not a value), and
    // connection_init declines to bake the 82-byte packet: the consumer must fill
    // the name from session state (the adoption is #29). Packet-shape coverage lives
    // in protocol_primitives::build_app_init's own tests.
    let s = store();

    // The friendly name resolves to a client-derived runtime slot, not a literal.
    match s.value("initFriendlyName".into()) {
        Some(ResolvedValue::ClientDerived { runtime }) => assert_eq!(runtime, "terminalName"),
        other => panic!("expected client-derived initFriendlyName, got {other:?}"),
    }
    // The GUID is still a fixed manufacturer constant (correctly modeled).
    assert!(matches!(
        s.value("initiatorGuid".into()),
        Some(ResolvedValue::Fixed { .. })
    ));

    // Without a host-supplied name, the app init can't be assembled → None (was the
    // 82-byte packet before #109). usb declares no init shape → None.
    assert!(s.connection_init("app".into()).is_none());
    assert!(s.connection_init("usb".into()).is_none());

    let init = s
        .connection_init_with_runtime(
            "app".into(),
            vec![KeyValue {
                key: "terminalName".into(),
                value: "iphone".into(),
            }],
        )
        .expect("runtime terminalName assembles app init packet");
    assert_eq!(init.friendly_name, "iphone");
    assert_eq!(init.name_field_byte_count, 26);
    assert_eq!(
        init.guid,
        vec![
            0xf2, 0xe4, 0x53, 0x8f, 0xad, 0xa5, 0x48, 0x5d, 0x87, 0xb2, 0x7f, 0x0b, 0xd3, 0xd5,
            0xde, 0xd0
        ]
    );
    assert_eq!(init.tail.len(), 28);
    assert_eq!(init.packet.len(), 82);
    assert!(s
        .connection_init_with_runtime("app".into(), vec![])
        .is_none());
}

#[test]
fn normalize_client_name_produces_the_terminal_name_for_the_init() {
    // #139: the host normalizes its raw device name once; the result is the single
    // `terminalName` value driving BOTH the BLE deviceNameString and the PTP/IP
    // friendly name, so the two channels agree (#109) with no name logic in Swift.
    let name = normalize_client_name(" Eric's iPad Pro ".into());
    assert_eq!(name, "eric-s-ipad");
    // Fits the 26-byte UTF-16LE name field (chars * 2 + 2-byte NUL).
    assert!(name.chars().count() * 2 + 2 <= 26);

    let s = store();
    let init = s
        .connection_init_with_runtime(
            "app".into(),
            vec![KeyValue {
                key: "terminalName".into(),
                value: name.clone(),
            }],
        )
        .expect("the normalized name assembles the app init packet");
    assert_eq!(init.friendly_name, name);
    assert_eq!(init.packet.len(), 82);
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
fn camera_identity_surfaces_manifest_identity() {
    let s = store();
    let identity = s.camera_identity();
    assert_eq!(identity.manufacturer, "FUJIFILM");
    assert_eq!(identity.model, "GFX100 II");
    assert_eq!(identity.firmware, "2.30");
    assert!(identity
        .identities
        .iter()
        .any(|kv| kv.key == "ptpDeviceName" && kv.value == "GFX100 II"));
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
    let chunk_size = cat
        .iter()
        .find(|p| p.code == 0xd235)
        .expect("reference app import chunk-size property in the catalog");
    assert_eq!(chunk_size.ptype.as_deref(), Some("u32"));
    assert_eq!(chunk_size.initial_value, Some(0x00bf_ffe0));
}

#[test]
fn focus_grid_and_af_packing_are_data_driven() {
    let s = store();
    // #135: the AF grid is manifest data, not a Swift constant.
    let grid = s.focus_grid().expect("gfx100ii declares a focus grid");
    assert_eq!((grid.columns, grid.rows), (9, 6));
    // The app packs a live-view tap into the 0x9026 param using the manifest grid.
    // A tap in cell (5,4) with default 4:3 aspect is the wire-confirmed 0x04030504.
    assert_eq!(
        pack_af_area(0.45, 0.5, grid.columns, grid.rows, None),
        0x0403_0504
    );
    // A prior 0xD17C lock carries its aspect forward into the next pack.
    assert_eq!(
        pack_af_area(0.0, 0.0, grid.columns, grid.rows, Some(0x1009_0101)) >> 16,
        0x1009
    );
}

#[test]
fn media_format_table_classifies_objects_through_ffi() {
    let s = store();
    let raf = s.media_format(0xb103).expect("RAF in the format table");
    assert_eq!(raf.name, "raf");
    assert_eq!(raf.vendor.as_deref(), Some("fuji"));
    assert!(raf.is_raw && !raf.is_movie);
    // #101: the embedded-JPEG locator reaches the app through the FFI, so it can
    // GetPartialObject the embedded JPG (magic + big-endian offset/length @ 0x54/0x58).
    let ej = raf
        .embedded_jpeg
        .as_ref()
        .expect("RAF carries an embedded-JPEG locator");
    assert_eq!(ej.magic, "FUJIFILMCCD-RAW");
    assert_eq!(ej.offset_at, 0x54);
    assert_eq!(ej.length_at, 0x58);
    assert!(ej.big_endian, "RAF header fields are big-endian");

    let mov = s.media_format(0x300d).expect("MOV in the table");
    assert!(mov.is_movie && !mov.is_raw);

    let jpeg = s.media_format(0x3801).expect("JPEG in the table");
    assert!(!jpeg.is_raw && !jpeg.is_movie);
    // A non-RAW format carries no embedded-JPEG locator.
    assert!(jpeg.embedded_jpeg.is_none());

    // #136: photos-compatibility is data — JPEG/RAF/MOV are all full assets the
    // app may hand to the OS photo library, without its own still/movie tables.
    assert!(jpeg.is_photos_compatible);
    assert!(raf.is_photos_compatible);
    assert!(mov.is_photos_compatible);
    // The reported-size sentinel is data, not a hardcoded 0xFFFFFFFF in Swift.
    assert_eq!(s.object_info_size_sentinel(), Some(0xffff_ffff));
    assert_eq!(s.wireless_transfer_ceiling(), Some(0xffff_ffff));

    // An unknown / unlisted format code → None.
    assert!(s.media_format(0x9999).is_none());
}

// ---------------------------------------------------------------------------
// G3 codec seam (#133) — PTP/IP framing + dataset codecs. Fixtures are built
// with the same ptp-core / protocol-primitives primitives the FFI wraps, so
// each test proves the exposed parser is the inverse of the real encoder.
// ---------------------------------------------------------------------------

use ptp_core::{
    DevicePropDesc, EventPacket, ObjectInfo, OperationResponse, PropForm, PropValue, PtpIpPacket,
    Writer,
};

#[test]
fn build_command_is_byte_exact_per_framing() {
    // Compressed OpenSession(0x1002), tid 1, param 1 — no DataPhaseInfo field.
    assert_eq!(
        build_command(PtpFraming::Compressed, 0x1002, 1, vec![1]).unwrap(),
        vec![0x10, 0, 0, 0, 0x01, 0x00, 0x02, 0x10, 0x01, 0, 0, 0, 0x01, 0, 0, 0],
    );
    // Standard framing carries DataPhaseInfo (=1) before the opcode.
    assert_eq!(
        build_command(PtpFraming::Standard, 0x1002, 1, vec![1]).unwrap(),
        vec![
            0x16, 0, 0, 0, // length 22
            0x06, 0, 0, 0, // type OperationRequest
            0x01, 0, 0, 0, // data phase info
            0x02, 0x10, // op 0x1002
            0x01, 0, 0, 0, // tid
            0x01, 0, 0, 0, // param
        ],
    );
    // USB container: GetDeviceInfo(0x1001), tid 0, no params.
    assert_eq!(
        build_command(PtpFraming::Usb, 0x1001, 0, vec![]).unwrap(),
        vec![0x0c, 0, 0, 0, 0x01, 0x00, 0x01, 0x10, 0x00, 0x00, 0x00, 0x00],
    );
}

#[test]
fn build_data_round_trips_through_parse_data_payload() {
    let payload = vec![0xde, 0xad, 0xbe, 0xef, 0x01];
    // USB data phases are a bulk-transfer concern, not a re-emittable container,
    // so build_data covers the two framings that model a Data block.
    for framing in [PtpFraming::Standard, PtpFraming::Compressed] {
        let frame = build_data(framing, 0x1009, 9, payload.clone()).unwrap();
        assert_eq!(parse_data_payload(framing, frame).unwrap(), payload);
    }
    // parse_data_payload still decodes a USB type-2 data container.
    let usb_data = vec![
        0x10, 0, 0, 0, 0x02, 0x00, 0x09, 0x10, 0x07, 0, 0, 0, 0xde, 0xad, 0xbe, 0xef,
    ];
    assert_eq!(
        parse_data_payload(PtpFraming::Usb, usb_data).unwrap(),
        vec![0xde, 0xad, 0xbe, 0xef],
    );
}

#[test]
fn parse_response_reads_a_compressed_operation_response() {
    let frame = protocol_primitives::fuji_framing::encode(&PtpIpPacket::OperationResponse(
        OperationResponse {
            code: 0x2001,
            transaction_id: 7,
            params: vec![0x2a],
        },
    ))
    .unwrap();
    let r = parse_response(PtpFraming::Compressed, frame).unwrap();
    assert_eq!(r.response_code, 0x2001);
    assert_eq!(r.txn, 7);
    assert_eq!(r.params, vec![0x2a]);

    // Truncated bytes → a decode error, not a panic.
    assert!(matches!(
        parse_response(PtpFraming::Standard, vec![0, 0]),
        Err(CodecError::Decode(_))
    ));
}

#[test]
fn parse_event_reads_the_event_container_and_ignores_non_events() {
    // USB/PIMA event container (type 4).
    let frame = protocol_primitives::usb_ptp::encode(&PtpIpPacket::Event(EventPacket {
        code: 0x4002,
        transaction_id: 0,
        params: vec![5],
    }))
    .unwrap();
    let got = parse_event(PtpFraming::Usb, frame)
        .unwrap()
        .expect("an event");
    assert_eq!(got.code, 0x4002);
    assert_eq!(got.params, vec![5]);

    // Standard PTP/IP event (packet-type 8) round-trips too.
    let std_frame = ptp_core::encode(&PtpIpPacket::Event(EventPacket {
        code: 0x4002,
        transaction_id: 3,
        params: vec![9],
    }))
    .unwrap();
    assert_eq!(
        parse_event(PtpFraming::Standard, std_frame)
            .unwrap()
            .unwrap()
            .code,
        0x4002,
    );

    // A response frame is not an event → None (not an error).
    let resp =
        protocol_primitives::usb_ptp::encode(&PtpIpPacket::OperationResponse(OperationResponse {
            code: 0x2001,
            transaction_id: 1,
            params: vec![],
        }))
        .unwrap();
    assert!(parse_event(PtpFraming::Usb, resp).unwrap().is_none());
}

#[test]
fn parse_object_info_decodes_the_generic_fields() {
    let oi = ObjectInfo {
        storage_id: 0x0001_0001,
        object_format: 0xb103, // RAF
        object_compressed_size: 4_289_912,
        image_pix_width: 8256,
        image_pix_height: 6192,
        filename: "DSCF1494.RAF".into(),
        ..Default::default()
    };
    let mut w = Writer::new();
    oi.encode(&mut w).unwrap();
    let got = parse_object_info(w.into_vec()).unwrap();
    assert_eq!(got.object_format, 0xb103);
    assert_eq!(got.object_compressed_size, 4_289_912);
    assert_eq!(got.image_pix_width, 8256);
    assert_eq!(got.filename, "DSCF1494.RAF");
}

#[test]
fn parse_device_prop_desc_decodes_value_and_enum_form() {
    let desc = DevicePropDesc {
        code: 0x5007,
        datatype: 0x0004, // UINT16
        get_set: 1,
        factory_default: PropValue::U16(400),
        current: PropValue::U16(560),
        form: PropForm::Enum(vec![PropValue::U16(280), PropValue::U16(560)]),
    };
    let mut w = Writer::new();
    desc.encode(&mut w).unwrap();
    let got = parse_device_prop_desc(w.into_vec()).unwrap();
    assert_eq!(got.code, 0x5007);
    assert_eq!(got.datatype, 0x0004);
    assert!(matches!(got.current, PtpValue::U16 { value: 560 }));
    match got.form {
        PtpPropForm::Enum { values } => {
            assert_eq!(values.len(), 2);
            assert!(matches!(values[0], PtpValue::U16 { value: 280 }));
        }
        other => panic!("expected an enum form, got {other:?}"),
    }
}

#[test]
fn parse_live_status_is_the_inverse_of_the_record_stream_encoder() {
    let bytes = protocol_primitives::quirk::record_stream(
        &[(0x5007, 280), (0xd212, 1)],
        &protocol_primitives::quirk::RecordStreamLayout::D212,
    )
    .unwrap();
    let ls = parse_live_status(bytes).unwrap();
    assert_eq!(ls.records.len(), 2);
    assert_eq!((ls.records[0].code, ls.records[0].value), (0x5007, 280));
    assert_eq!((ls.records[1].code, ls.records[1].value), (0xd212, 1));
}

#[test]
fn parse_record_stream_honors_declared_widths_with_d212_defaults() {
    // Omitted widths must mean exactly what parse_live_status assumes — the FFI
    // defaults mirror camera_config::Payload::record_widths (#161).
    let defaults = PayloadInfo {
        form: PayloadForm::RecordStream,
        count_width: None,
        record: None,
        members: vec![],
    };
    let cc_defaults = camera_config::Payload {
        form: camera_config::PayloadForm::RecordStream,
        count_width: None,
        record: None,
        members: vec![],
    }
    .record_widths();
    assert_eq!(cc_defaults, (2, 2, 4), "schema defaults are the D212 shape");
    let d212 = protocol_primitives::quirk::record_stream(
        &[(0x5007, 280)],
        &protocol_primitives::quirk::RecordStreamLayout::D212,
    )
    .unwrap();
    let via_defaults = parse_record_stream(d212.clone(), defaults).unwrap();
    let via_live_status = parse_live_status(d212).unwrap();
    assert_eq!(via_defaults.records.len(), 1);
    assert_eq!(
        (via_defaults.records[0].code, via_defaults.records[0].value),
        (
            via_live_status.records[0].code,
            via_live_status.records[0].value
        )
    );

    // Declared non-default widths reframe the parse: u8 count, u8 code, u16 value.
    let tight = PayloadInfo {
        form: PayloadForm::RecordStream,
        count_width: Some(1),
        record: Some(RecordLayoutInfo {
            code_width: 1,
            value_width: 2,
        }),
        members: vec![],
    };
    let layout = protocol_primitives::quirk::RecordStreamLayout::new(1, 1, 2).unwrap();
    let bytes =
        protocol_primitives::quirk::record_stream(&[(0x07, 280), (0x09, 1)], &layout).unwrap();
    let ls = parse_record_stream(bytes, tight).unwrap();
    assert_eq!(ls.records.len(), 2);
    assert_eq!((ls.records[0].code, ls.records[0].value), (0x07, 280));

    // Widths the codec can't honor are a loud decode error, not a misread.
    let bad = PayloadInfo {
        form: PayloadForm::RecordStream,
        count_width: Some(3),
        record: None,
        members: vec![],
    };
    assert!(parse_record_stream(vec![0, 0], bad).is_err());
}

#[test]
fn parse_object_handle_list_decodes_a_u32_array() {
    let mut w = Writer::new();
    w.ptp_array(&[7u32, 9, 11], |w, v| w.u32(*v));
    assert_eq!(
        parse_object_handle_list(w.into_vec()).unwrap(),
        vec![7, 9, 11]
    );
}

#[test]
fn socket_bindings_and_transport_close_surface_through_ffi() {
    let s = store();
    // Resolved ports keyed by role — the app binds by role, not Fuji offsets (#140).
    assert_eq!(
        s.port_for_role("app".into(), SocketRole::Command),
        Some(55740)
    );
    assert_eq!(
        s.port_for_role("app".into(), SocketRole::Event),
        Some(55741)
    );
    assert_eq!(
        s.port_for_role("app".into(), SocketRole::LiveView),
        Some(55742)
    );
    // socket_bindings lists the three roles in command → event → live-view order.
    let binds = s.socket_bindings("app".into());
    assert_eq!(binds.len(), 3);
    assert_eq!(binds[0].role, SocketRole::Command);
    assert_eq!(binds[0].port, 55740);
    assert_eq!(binds[2].role, SocketRole::LiveView);

    // wireless-tether binds only a command socket (PCSS DSPORT 15740); poll-based
    // delivery means no event/live-view socket.
    assert_eq!(
        s.port_for_role("wireless-tether".into(), SocketRole::Command),
        Some(15740)
    );
    assert_eq!(
        s.port_for_role("wireless-tether".into(), SocketRole::Event),
        None
    );
    assert_eq!(s.socket_bindings("wireless-tether".into()).len(), 1);

    // Transport-close resolves the named sentinel through manifest data.
    let tc = s
        .transport_close("app".into())
        .expect("transport-close query succeeds")
        .expect("app declares a transport-close");
    assert_eq!(tc.packet, vec![0x08, 0, 0, 0, 0xff, 0xff, 0xff, 0xff]);
    assert_eq!(tc.when.as_deref(), Some("before-image-transfer-reopen"));
    // A connection without one → Ok(None).
    assert!(s
        .transport_close("wireless-tether".into())
        .expect("transport-close query succeeds")
        .is_none());
}

#[test]
fn transport_close_reports_bad_sentinel_data() {
    let missing = ConfigStore::from_bundle(
        r#"
schema: camera-config/v1
camera: { manufacturer: TESTCO, model: TM1 }
connections:
  app:
    transportClose: { sentinel: missing }
"#
        .into(),
        None,
    )
    .expect("manifest loads");
    assert!(matches!(
        missing.transport_close("app".into()),
        Err(TransportCloseError::UnknownSentinel(_))
    ));

    let malformed = ConfigStore::from_bundle(
        r#"
schema: camera-config/v1
camera: { manufacturer: TESTCO, model: TM1 }
sentinels:
  bad: { bytes: "0xz" }
connections:
  app:
    transportClose: { sentinel: bad }
"#
        .into(),
        None,
    )
    .expect("manifest loads");
    assert!(matches!(
        malformed.transport_close("app".into()),
        Err(TransportCloseError::InvalidSentinelBytes(_))
    ));
}

#[test]
fn parse_data_phase_decodes_a_compressed_single_frame() {
    // The wireless-tether/reference app compressed channel delivers a whole data phase in
    // one type-2 frame — no StartData/Data/EndData. Byte-exact golden from
    // 2026-06-02-pcss-ptpip-fuji-original.pcapng: Data(0x1015) tid 2, value 5.
    let golden = vec![
        0x0e, 0, 0, 0, // length 14
        0x02, 0x00, // type 2 = Data
        0x15, 0x10, // opcode 0x1015 echoed in the code field
        0x02, 0, 0, 0, // tid 2
        0x05, 0x00, // payload: u16 value 5
    ];
    // build_data reproduces the captured frame byte-for-byte.
    assert_eq!(
        build_data(PtpFraming::Compressed, 0x1015, 2, vec![0x05, 0x00]).unwrap(),
        golden,
    );
    // parse_data_phase yields the entire payload in one Data frame — the app used
    // to crash waiting for an EndData / type-12 that never arrives.
    let d = parse_data_phase(PtpFraming::Compressed, golden).unwrap();
    assert_eq!(d.kind, DataPhaseKind::Data);
    assert_eq!(d.txn, 2);
    assert_eq!(d.payload, vec![0x05, 0x00]);
    assert_eq!(d.total_length, None);

    // A large transfer is still a single frame: on the wire a full GetObject(0x1009)
    // arrives as one 14.5 MB type-2 Data frame. A stand-in payload round-trips whole.
    let big = vec![0xabu8; 4096];
    let frame = build_data(PtpFraming::Compressed, 0x1009, 99, big.clone()).unwrap();
    let bd = parse_data_phase(PtpFraming::Compressed, frame).unwrap();
    assert_eq!(bd.kind, DataPhaseKind::Data);
    assert_eq!(bd.payload, big);
}

#[test]
fn connection_wire_framing_is_declared_in_the_manifest() {
    let s = store();
    let app = s
        .connections(Platform::Macos)
        .into_iter()
        .find(|c| c.id == "app")
        .expect("app connection");
    // The app reads framing from data — it never maps kind→framing itself (#133).
    assert!(matches!(app.command_framing, Some(PtpFraming::Compressed)));
    // Command vs event framing differ: the event socket is a PIMA type-4 container.
    assert!(matches!(app.event_framing, Some(PtpFraming::Usb)));
    // The manifest-declared command framing drives the codec: byte-exact compressed frame.
    assert_eq!(
        build_command(app.command_framing.unwrap(), 0x1002, 1, vec![1]).unwrap(),
        vec![0x10, 0, 0, 0, 0x01, 0x00, 0x02, 0x10, 0x01, 0, 0, 0, 0x01, 0, 0, 0],
    );

    // wireless-tether's command/data channel is compressed too (single type-2 data
    // frame); its poll-based delivery means no event socket, so no event framing.
    let wt = s
        .connections(Platform::Macos)
        .into_iter()
        .find(|c| c.id == "wireless-tether")
        .expect("wireless-tether connection");
    assert!(matches!(wt.command_framing, Some(PtpFraming::Compressed)));
    assert!(wt.event_framing.is_none());
}
