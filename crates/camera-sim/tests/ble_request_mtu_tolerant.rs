//! `bleRequestMtu` checkpoint semantics (#400). `requestedMtu` is the
//! reference app's request target; `minimumMtu` is a separately evidenced
//! floor. With no floor declared the step succeeds at any negotiated MTU;
//! with one it fails (tolerant-aware) when the negotiated MTU is below the
//! floor, on every platform. The X-A7 negotiates 185 against a 515 request
//! target and the reference app enforces no floor, so its legacy-app
//! establishments declare no `minimumMtu`.

use std::collections::BTreeMap;

use camera_config::index::{ResolvedManufacturerIndex, Step};
use camera_sim::{walk_establishment, BleResponder};

/// A synthetic single-family index whose establishment is `bleConnect` +
/// `bleDiscoverServices` + the caller's `steps`. Mirrors the
/// `ble_if_tolerant.rs` harness.
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
            - {{ id: camera.test.walk, version: 1, displayRole: preparingConnection, defaultExpectedDurationMs: 1, interactionRequired: false, executorSpan: {{ sequence: steps, startStep: 0, endStepExclusive: 3 }} }}
          steps:
            - bleConnect: {{}}
            - bleDiscoverServices: {{}}
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
fn no_floor_checkpoint_accepts_any_negotiated_mtu() {
    // The X-A7 hardware observation: negotiated 185 against a 515 request
    // target. With no `minimumMtu` the walk continues, untolerated.
    let idx = index_with_steps(
        r#"          - bleRequestMtu: { requestedMtu: 515 }
"#,
    );
    let mut responder = BleResponder::new(Vec::<String>::new()).with_mtu_cap(185);

    let outcome = walk_establishment(
        &mut responder,
        &steps_of(&idx),
        &BTreeMap::new(),
        &BTreeMap::new(),
        &BTreeMap::new(),
    )
    .expect("a no-floor MTU checkpoint must succeed at any negotiated value");

    assert!(
        outcome.summary.tolerated_step_paths.is_empty(),
        "no tolerance annotation is involved"
    );
}

#[test]
fn unmet_floor_checkpoint_aborts_the_walk() {
    let idx = index_with_steps(
        r#"          - bleRequestMtu: { requestedMtu: 515, minimumMtu: 200 }
"#,
    );
    let mut responder = BleResponder::new(Vec::<String>::new()).with_mtu_cap(185);

    let error = match walk_establishment(
        &mut responder,
        &steps_of(&idx),
        &BTreeMap::new(),
        &BTreeMap::new(),
        &BTreeMap::new(),
    ) {
        Ok(_) => panic!("negotiated 185 below the 200 floor must fail the walk"),
        Err(e) => e,
    };

    assert!(
        error.to_string().contains("200"),
        "the failure names the unmet floor: {error}"
    );
}

#[test]
fn met_floor_checkpoint_succeeds() {
    let idx = index_with_steps(
        r#"          - bleRequestMtu: { requestedMtu: 515, minimumMtu: 185 }
"#,
    );
    let mut responder = BleResponder::new(Vec::<String>::new()).with_mtu_cap(185);

    walk_establishment(
        &mut responder,
        &steps_of(&idx),
        &BTreeMap::new(),
        &BTreeMap::new(),
        &BTreeMap::new(),
    )
    .expect("a negotiated value equal to the floor meets it");
}

#[test]
fn tolerant_unmet_floor_is_skipped_and_recorded() {
    let idx = index_with_steps(
        r#"          - bleRequestMtu: { requestedMtu: 515, minimumMtu: 200, tolerant: true }
"#,
    );
    let mut responder = BleResponder::new(Vec::<String>::new()).with_mtu_cap(185);

    let outcome = walk_establishment(
        &mut responder,
        &steps_of(&idx),
        &BTreeMap::new(),
        &BTreeMap::new(),
        &BTreeMap::new(),
    )
    .expect("a tolerant unmet-floor checkpoint must not fail the walk");

    assert_eq!(
        outcome.summary.tolerated_step_paths,
        vec!["steps[2].bleRequestMtu".to_string()],
        "the tolerated checkpoint is recorded at its step path"
    );
}

#[test]
fn tolerant_step_absorbs_a_failed_mtu_request() {
    // legacy manufacturer app's onMtuChanged ignores the callback status, so a failed
    // requestMtu call must not block registration (#449): tolerance absorbs
    // the call error itself, not just an unmet floor.
    let idx = index_with_steps(
        r#"          - bleRequestMtu: { requestedMtu: 515, tolerant: true }
"#,
    );
    let mut responder = BleResponder::new(Vec::<String>::new()).with_failing_mtu_request();

    let outcome = walk_establishment(
        &mut responder,
        &steps_of(&idx),
        &BTreeMap::new(),
        &BTreeMap::new(),
        &BTreeMap::new(),
    )
    .expect("a tolerant step must absorb a failed MTU request");

    assert_eq!(
        outcome.summary.tolerated_step_paths,
        vec!["steps[2].bleRequestMtu".to_string()],
        "the tolerated call failure is recorded at its step path"
    );
}

#[test]
fn strict_step_fails_on_a_failed_mtu_request() {
    let idx = index_with_steps(
        r#"          - bleRequestMtu: { requestedMtu: 515 }
"#,
    );
    let mut responder = BleResponder::new(Vec::<String>::new()).with_failing_mtu_request();

    let error = match walk_establishment(
        &mut responder,
        &steps_of(&idx),
        &BTreeMap::new(),
        &BTreeMap::new(),
        &BTreeMap::new(),
    ) {
        Ok(_) => panic!("a failed MTU request must fail a strict step"),
        Err(e) => e,
    };

    assert!(
        error.to_string().contains("MTU request failed"),
        "the failure reports the call error: {error}"
    );
}
