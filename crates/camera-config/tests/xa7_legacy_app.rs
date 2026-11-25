use std::collections::BTreeSet;
use std::path::PathBuf;

use camera_config::index::{ResolvedManufacturerIndex, Step};
use camera_config::{CameraManifest, ModeEntryExecution, SocketRole, WireFraming};

fn body_text() -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../packages/camera-config-data/fuji/xa7/xa7.yaml");
    std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("read {}: {error}", path.display()))
}

fn xa7_plan_steps(mechanism: &str) -> Vec<Step> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../packages/camera-config-data/fuji/index.yaml");
    let yaml = std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
    ResolvedManufacturerIndex::from_yaml(&yaml)
        .expect("Fuji index loads")
        .models
        .into_iter()
        .find(|model| model.id == "xa7")
        .expect("X-A7 model")
        .ble
        .expect("X-A7 BLE block")
        .establishment(mechanism)
        .unwrap_or_else(|| panic!("X-A7 {mechanism} plan"))
        .steps
        .clone()
}

/// The step paths of a plan's strict (non-tolerant) steps.
fn strict_paths(steps: &[Step]) -> Vec<String> {
    steps
        .iter()
        .enumerate()
        .filter(|(_, step)| !step.options().tolerant)
        .map(|(index, step)| format!("steps[{index}].{}", step.verb_name()))
        .collect()
}

#[test]
fn xa7_legacy_app_manifest_maps_the_feature_modes() {
    let yaml = body_text();
    let manifest = CameraManifest::from_yaml(&yaml).expect("X-A7 manifest loads");
    let connection = manifest
        .connections
        .get("legacy-app")
        .expect("legacy app connection");
    assert_eq!(connection.init_shape.as_deref(), Some("legacyApp82"));
    assert_eq!(connection.command_framing, Some(WireFraming::Usb));
    assert_eq!(connection.event_framing, Some(WireFraming::Usb));
    let bindings = connection.bindings.as_ref().expect("three socket roles");
    assert_eq!(bindings.port_for(SocketRole::Command), Some(55740));
    assert_eq!(bindings.port_for(SocketRole::Event), Some(55741));
    assert_eq!(bindings.port_for(SocketRole::LiveView), Some(55742));

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
fn xa7_viewer_and_remote_paths_keep_the_declared_operations() {
    let manifest = CameraManifest::from_yaml(&body_text()).expect("X-A7 manifest loads");
    let connection = &manifest.connections["legacy-app"];
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

    for code in [
        "0x1007", "0x1008", "0x1009", "0x100a", "0x1014", "0x1015", "0x1016", "0x1018", "0x101c",
    ] {
        assert!(manifest.operations.contains_key(code), "missing {code}");
    }
}

#[test]
fn xa7_init_contract_fails_closed_on_shape_drift() {
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
}

#[test]
fn xa7_manifest_contains_only_public_relative_evidence_paths() {
    let yaml = body_text();
    let manifest = CameraManifest::from_yaml(&yaml).expect("X-A7 manifest loads");
    let evidence_path = &manifest.evidence["legacyApp4102"].path;
    assert_eq!(evidence_path, "evidence/LEGACY_APP_4_10_2.md");
    assert!(PathBuf::from(evidence_path).is_relative());
    assert!(!evidence_path.starts_with('~'));
}

/// The legacy vendor mobile app continues past read and CCCD error callbacks,
/// so the registration plan tolerates them too. The strict steps are the
/// connection setup, the pairing-key and name writes, the peripheral-name
/// step, the serial and SSID reads, and the AP-state subscribe, each with a
/// comment in the index giving its reason.
#[test]
fn xa7_registration_tolerates_what_the_legacy_app_tolerates() {
    let pair = xa7_plan_steps("legacy-app-pair");
    assert_eq!(pair.len(), 30, "the step count is unchanged");
    assert_eq!(
        strict_paths(&pair),
        [
            "steps[0].bleConnect",
            "steps[1].bleDelay",
            "steps[3].bleDiscoverServices",
            "steps[4].bleWrite",
            "steps[5].bleWrite",
            "steps[7].blePeripheralName",
            "steps[8].bleWrite",
            "steps[9].bleRead",
            "steps[10].bleRead",
            "steps[23].bleSubscribe",
        ]
    );
    for (index, capture) in [
        (11, "reservationAfterShooting"),
        (12, "pairedSmartDeviceCount"),
        (13, "firmware"),
        (28, "apState"),
    ] {
        let Step::BleRead(read) = &pair[index] else {
            panic!("steps[{index}] is a bleRead");
        };
        assert!(read.opts.tolerant, "steps[{index}] is tolerant");
        assert_eq!(read.capture_as, capture, "steps[{index}] capture");
    }
    assert!(
        pair.iter().all(|step| step.options().confirms.is_none()),
        "no step claims to confirm registration (#355)"
    );

    let reconnect = xa7_plan_steps("legacy-app-reconnect");
    assert_eq!(
        format!("{reconnect:?}"),
        format!("{pair:?}"),
        "reconnect replays the registration steps"
    );
}
