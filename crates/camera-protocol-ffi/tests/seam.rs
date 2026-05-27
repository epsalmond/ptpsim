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
