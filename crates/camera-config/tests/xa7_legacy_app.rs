use std::collections::BTreeSet;
use std::path::PathBuf;

use camera_config::{CameraManifest, ModeEntryExecution, WireFraming};

fn body_text() -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../packages/camera-config-data/fuji/xa7/xa7.yaml");
    std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("read {}: {error}", path.display()))
}

#[test]
fn xa7_is_legacy_app_not_app_and_maps_every_static_feature_mode() {
    let yaml = body_text();
    assert!(!yaml.to_ascii_lowercase().contains("ptpip-app"));
    let manifest = CameraManifest::from_yaml(&yaml).expect("X-A7 manifest loads");
    let connection = manifest
        .connections
        .get("legacy-app")
        .expect("legacy manufacturer app connection");
    assert_eq!(connection.init_shape.as_deref(), Some("legacyApp82"));
    assert_eq!(connection.command_framing, Some(WireFraming::Usb));
    assert_eq!(connection.event_framing, Some(WireFraming::Usb));
    let bindings = connection.bindings.as_ref().expect("three socket roles");
    assert_eq!(bindings.command, 55740);
    assert_eq!(bindings.event, Some(55741));
    assert_eq!(bindings.live_view, Some(55742));

    let targets = connection
        .entries
        .iter()
        .map(|entry| entry.to.as_str())
        .collect::<BTreeSet<_>>();
    assert_eq!(
        targets,
        BTreeSet::from([
            "firmware-update",
            "gps-assist",
            "mode-mismatch-cleanup",
            "photo-receiver",
            "photo-viewer",
            "remote-photo-view",
            "remote-shooting",
            "reserved-photo-receive",
        ])
    );
    assert!(connection.entries.iter().all(|entry| matches!(
        entry.execution,
        ModeEntryExecution::Ptp { ref steps } if !steps.is_empty()
    )));
}

#[test]
fn xa7_viewer_and_remote_paths_keep_the_apk_backed_operations() {
    let manifest = CameraManifest::from_yaml(&body_text()).expect("X-A7 manifest loads");
    let connection = manifest.connections.get("legacy-app").unwrap();
    let viewer = connection
        .entries
        .iter()
        .find(|entry| entry.to == "photo-viewer")
        .unwrap();
    let steps = viewer.ptp_steps().unwrap();
    assert_eq!(steps[0].get_prop.as_deref(), Some("0xdf00"));
    let neutral_four = steps[1].if_step.as_ref().expect("DF00=4 branch");
    assert_eq!(neutral_four.equals, 4);
    assert_eq!(neutral_four.then_steps[0].value, Some(9.into()));
    let neutral_six = neutral_four.else_steps[0]
        .if_step
        .as_ref()
        .expect("DF00=6 branch");
    assert_eq!(neutral_six.equals, 6);
    assert_eq!(neutral_six.then_steps[0].value, Some(9.into()));
    assert_eq!(neutral_six.else_steps[0].value, Some(2.into()));
    assert_eq!(steps[2].get_prop.as_deref(), Some("0xdf22"));
    assert_eq!(steps[3].value, Some(5.into()));

    for code in ["0x1007", "0x1008", "0x1009", "0x100a", "0x1018", "0x101c"] {
        assert!(manifest.operations.contains_key(code), "missing {code}");
    }
    assert_eq!(
        manifest
            .properties
            .get("0xdf24")
            .and_then(|property| property.initial_value),
        Some(0)
    );
}

#[test]
fn legacy_app_retry_policy_is_validated_without_a_pcss_knock() {
    let invalid = body_text().replacen(
        "      whenReasons: [\"0x2019\"]",
        "      whenReasons: []",
        1,
    );
    let error = CameraManifest::from_yaml(&invalid).expect_err("empty retry reasons must fail");
    assert!(error.to_string().contains("initRetries"), "{error}");
}

#[test]
fn legacy_app_init_shape_fails_closed_on_missing_or_wrong_fields() {
    let missing_ip = body_text().replacen("        clientIpv4: legacyAppClientIpv4\n", "", 1);
    assert!(CameraManifest::from_yaml(&missing_ip)
        .unwrap_err()
        .to_string()
        .contains("clientIpv4"));

    let wrong_width = body_text().replacen(
        "      nameFieldByteCount: 54",
        "      nameFieldByteCount: 26",
        1,
    );
    assert!(CameraManifest::from_yaml(&wrong_width)
        .unwrap_err()
        .to_string()
        .contains("nameFieldByteCount"));

    let unexpected_tail = body_text().replacen(
        "      expectedResponderGuid: legacyAppResponderGuid",
        "      tail: \"0000\"\n      expectedResponderGuid: legacyAppResponderGuid",
        1,
    );
    assert!(CameraManifest::from_yaml(&unexpected_tail)
        .unwrap_err()
        .to_string()
        .contains(".tail"));
}

#[test]
fn transition_params_require_a_mechanism_backed_edge() {
    let invalid = body_text().replacen(
        "        mechanism: legacy-app-establish-wifi-ap\n        mode: photo-receiver",
        "        userInstruction: launch manually\n        mode: photo-receiver",
        1,
    );
    let error = CameraManifest::from_yaml(&invalid).expect_err("params without mechanism fail");
    assert!(error.to_string().contains("params require"), "{error}");
}
