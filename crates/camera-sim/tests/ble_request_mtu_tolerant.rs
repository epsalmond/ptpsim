//! `bleRequestMtu` checkpoint tolerance (#399). The reference app treats
//! `requestMtu(515)` as fire-and-forget — no negotiated-MTU floor — and
//! CoreBluetooth has no request API, so the X-A7 legacy-app establishments
//! mark the step `tolerant: true`. These pin both walker arms against a
//! responder whose ATT MTU cap sits below the request target: tolerant skips
//! and records the tolerated path; strict (the default) aborts the walk.

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
fn tolerant_mtu_checkpoint_below_target_is_skipped_and_recorded() {
    // The X-A7 hardware observation: negotiated 185 against a 515 request
    // target. With `tolerant: true` the walk continues past the checkpoint.
    let idx = index_with_steps(
        r#"          - bleRequestMtu: { mtu: 515, tolerant: true }
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
    .expect("a tolerant MTU checkpoint below the target must not fail the walk");

    assert_eq!(
        outcome.summary.tolerated_step_paths,
        vec!["steps[2].bleRequestMtu".to_string()],
        "the tolerated checkpoint is recorded at its step path"
    );
}

#[test]
fn strict_mtu_checkpoint_below_target_aborts_the_walk() {
    // Without `tolerant` the §11.4a checkpoint semantics stand: negotiated
    // below the manifest `mtu` is a step failure.
    let idx = index_with_steps(
        r#"          - bleRequestMtu: { mtu: 515 }
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
        Ok(_) => panic!("a strict MTU checkpoint below the target must fail the walk"),
        Err(e) => e,
    };

    assert!(
        error.to_string().contains("515"),
        "the failure names the unmet requirement: {error}"
    );
}
