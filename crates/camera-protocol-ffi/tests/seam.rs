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
            "Shooting/Stills".into(),
            0x9018,
            vec![]
        ),
        Availability::Available
    ));
    assert!(matches!(
        s.operation_available("app".into(), "Shooting/Stills".into(), 0x9018, vec![]),
        Availability::WrongConnection
    ));
    // Backup op gates to BackupRestore, not RawConversion.
    assert!(matches!(
        s.operation_available("usb".into(), "RawConversion".into(), 0x100c, vec![]),
        Availability::WrongMode
    ));
}

#[test]
fn control_mechanism_varies_by_connection() {
    let s = store();
    let ctl = s
        .control_for("wireless-tether".into(), "Shooting/Stills".into(), 0x5007)
        .expect("aperture control over tether");
    assert_eq!(ctl.set_method.as_deref(), Some("absolute"));
    assert_eq!(ctl.operation, Some(0x1016));
}

#[test]
fn mode_entry_returns_the_ground_truth_wire_steps() {
    let s = store();
    let plan = s
        .mode_entry("app".into(), None, "Shooting/Stills".into())
        .expect("live-view entry");
    assert!(plan.user_instruction.is_none());
    // First step: SetProp 0xdf00 = 6 (the real live-view startup constant).
    match &plan.steps[0] {
        EntryStep::SetProp { prop, value } => {
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
            repeat: 4
        }
    )));

    // A USB sub-mode entry is a userInstruction (camera menu), no steps.
    let usb = s
        .mode_entry("usb".into(), None, "RawConversion".into())
        .unwrap();
    assert!(usb.user_instruction.is_some());
    assert!(usb.steps.is_empty());
}

#[test]
fn establishment_is_returned_as_data() {
    let s = store();
    // wireless-tether: PCSS knock params surfaced for the app to drive.
    let wt = s.establishment("wireless-tether".into()).unwrap();
    assert_eq!(wt.mechanism.as_deref(), Some("pcss-knock-v1"));
    assert!(wt
        .params
        .iter()
        .any(|kv| kv.key == "knockPort" && kv.value == "51562"));
    // app is brought up via the BLE→WiFi handover.
    let app = s.establishment("app".into()).unwrap();
    assert_eq!(app.mechanism.as_deref(), Some("ble-to-wifi-ap-v1"));
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
    // 0x900c is a (usb, RawConversion) op. Over the app connection → WrongConnection,
    // and the trace says why (what telemetry captures) — no predicate eval needed.
    let wc = s.operation_available_explained("app".into(), "RawConversion".into(), 0x900c, vec![]);
    assert!(matches!(wc.availability, Availability::WrongConnection));
    assert!(!wc.trace.connection_ok);
    assert!(wc.trace.requires.is_none()); // this op declares no prerequisite
    assert!(wc.trace.reason.contains("usb"));
    // Over its own connection/mode → Available, both axes ok.
    let ok = s.operation_available_explained("usb".into(), "RawConversion".into(), 0x900c, vec![]);
    assert!(matches!(ok.availability, Availability::Available));
    assert!(ok.trace.connection_ok && ok.trace.mode_ok);
    // Unknown op → Unavailable with an explanatory reason.
    let un =
        s.operation_available_explained("app".into(), "Shooting/Stills".into(), 0x9999, vec![]);
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
  "0x101c": { name: OpenCap, modes: [Shooting], connections: [app], requires: { prop: "0xd212", mask: 0x00ff, ne: 0 } }
connections: { app: { kind: ptpip-app } }
modes: { "Shooting/Stills": {} }
"#;
    let s = ConfigStore::from_bundle(yaml.to_string(), None).expect("loads");
    // Low byte masks to 0 → `ne 0` fails → Blocked, and the leaf shows exactly why.
    let g = s.operation_available_explained(
        "app".into(),
        "Shooting/Stills".into(),
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
        "Shooting/Stills".into(),
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
        Some("Shooting/Stills")
    );
}
