//! `blePeripheralName` reference-walker behavior (#403). CoreBluetooth filters
//! the GAP service (0x1800) from discovery, so the Device Name characteristic
//! (0x2A00) is unreachable as a GATT read on iOS; the step captures the
//! platform peripheral name instead. These pin the walker arms: a served name
//! binds scope as UTF-8, an unserved name fails the step, and `tolerant: true`
//! skips and records the path.

use std::collections::BTreeMap;

use camera_config::index::{ResolvedManufacturerIndex, Step};
use camera_sim::{walk_establishment, BleResponder};

/// A synthetic single-family index whose establishment is `bleConnect` +
/// the caller's `steps`. Mirrors the `ble_request_mtu_tolerant.rs` harness.
fn index_with_steps(steps: &str) -> ResolvedManufacturerIndex {
    let steps = steps
        .lines()
        .map(|l| {
            if l.trim().is_empty() {
                String::new()
            } else {
                format!("  {l}")
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    let yaml = format!(
        r#"
manufacturer: TESTCO
families:
  test:
    ble:
      gatt: {{}}
      advert: {{ manufacturerCompanyId: 1 }}
      establishments:
        test:
          mechanism: test
          activities:
            - {{ id: camera.test.walk, version: 1, displayRole: preparingConnection, defaultExpectedDurationMs: 1, interactionRequired: false, executorSpan: {{ sequence: steps, startStep: 0, endStepExclusive: 2 }} }}
          steps:
            - bleConnect: {{}}
{steps}
models:
  - id: tm1
    displayName: "Test"
    inherits: [test]
    manifest: tm1.yaml
"#
    );
    ResolvedManufacturerIndex::from_yaml(&yaml).expect("synthetic index loads")
}

fn steps_of(idx: &ResolvedManufacturerIndex) -> Vec<Step> {
    idx.models[0]
        .ble
        .as_ref()
        .unwrap()
        .establishment("test")
        .unwrap()
        .steps
        .clone()
}

#[test]
fn served_peripheral_name_binds_scope_as_utf8() {
    let idx = index_with_steps(
        r#"          - blePeripheralName: { captureAs: cameraName }
"#,
    );
    let mut responder =
        BleResponder::new(Vec::<String>::new()).with_peripheral_name("1361X-A7-1361");

    let outcome = walk_establishment(
        &mut responder,
        &steps_of(&idx),
        &BTreeMap::new(),
        &BTreeMap::new(),
        &BTreeMap::new(),
    )
    .expect("a served peripheral name must bind");

    assert_eq!(
        outcome.scope.get("cameraName").map(String::as_str),
        Some("1361X-A7-1361")
    );
}

#[test]
fn unserved_peripheral_name_fails_the_walk() {
    let idx = index_with_steps(
        r#"          - blePeripheralName: { captureAs: cameraName }
"#,
    );
    let mut responder = BleResponder::new(Vec::<String>::new());

    let error = match walk_establishment(
        &mut responder,
        &steps_of(&idx),
        &BTreeMap::new(),
        &BTreeMap::new(),
        &BTreeMap::new(),
    ) {
        Ok(_) => panic!("an unserved peripheral name must fail the walk"),
        Err(e) => e,
    };

    assert!(
        error.to_string().contains("peripheral name"),
        "the failure names the missing source: {error}"
    );
}

#[test]
fn served_peripheral_name_strips_the_nul_terminator() {
    // GAP-exposing hosts satisfy the step with the raw 0x2A00 read, which is
    // NUL-terminated; the bound value must not carry it (#444 review).
    let idx = index_with_steps(
        r#"          - blePeripheralName: { captureAs: cameraName }
"#,
    );
    let mut responder =
        BleResponder::new(Vec::<String>::new()).with_peripheral_name("1361X-A7-1361\0");

    let outcome = walk_establishment(
        &mut responder,
        &steps_of(&idx),
        &BTreeMap::new(),
        &BTreeMap::new(),
        &BTreeMap::new(),
    )
    .expect("a NUL-terminated name binds trimmed");

    assert_eq!(
        outcome.scope.get("cameraName").map(String::as_str),
        Some("1361X-A7-1361")
    );
}

#[test]
fn empty_peripheral_name_fails_like_an_unserved_one() {
    // CBPeripheral.name is optional; an unavailable name is a step failure,
    // never a silently empty capture (#444 review).
    let idx = index_with_steps(
        r#"          - blePeripheralName: { captureAs: cameraName }
"#,
    );
    let mut responder = BleResponder::new(Vec::<String>::new()).with_peripheral_name("");

    let error = match walk_establishment(
        &mut responder,
        &steps_of(&idx),
        &BTreeMap::new(),
        &BTreeMap::new(),
        &BTreeMap::new(),
    ) {
        Ok(_) => panic!("an empty peripheral name must fail the walk"),
        Err(e) => e,
    };

    assert!(
        error.to_string().contains("peripheral name unavailable"),
        "the failure names the unavailable name: {error}"
    );
}

#[test]
fn tolerant_unserved_peripheral_name_is_skipped_and_recorded() {
    let idx = index_with_steps(
        r#"          - blePeripheralName: { captureAs: cameraName, tolerant: true }
"#,
    );
    let mut responder = BleResponder::new(Vec::<String>::new());

    let outcome = walk_establishment(
        &mut responder,
        &steps_of(&idx),
        &BTreeMap::new(),
        &BTreeMap::new(),
        &BTreeMap::new(),
    )
    .expect("a tolerant peripheral-name step must not fail the walk");

    assert_eq!(
        outcome.summary.tolerated_step_paths,
        vec!["steps[1].blePeripheralName".to_string()],
        "the tolerated step is recorded at its step path"
    );
    assert!(
        !outcome.scope.contains_key("cameraName"),
        "a skipped capture binds nothing"
    );
}
