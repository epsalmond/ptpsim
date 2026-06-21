//! `If.tolerant` reference-walker coverage (#45). The §11.6 unbound-predicate-
//! field path — `tolerant: true` makes an unbound `if` field evaluate false
//! (else/skip) instead of erroring — is the headline reason the `If`
//! special-case exists, yet the pair-roundtrip tests only ever exercise a
//! BOUND `style` field. These pin both arms of that path directly: a mutation
//! turning `None if tolerant => false` into `=> true`, or dropping the tolerant
//! arm, would fail here.

use std::collections::BTreeMap;

use camera_config::index::{ResolvedManufacturerIndex, Step};
use camera_sim::{walk_establishment, BleResponder};

const THEN: &str = "0000BB01-0000-1000-8000-00805F9B34FB";
const ELSE: &str = "0000BB02-0000-1000-8000-00805F9B34FB";

/// A synthetic single-family index whose establishment is `bleConnect` + the
/// caller's `steps`. `thenChar`/`elseChar` resolve to the UUIDs the responder
/// is keyed on, so an `if`'s branches write observably. Mirrors the
/// `ble_await_until.rs` / `ble_acquire.rs` harness.
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
      gatt:
        thenChar: "{THEN}"
        elseChar: "{ELSE}"
      advert: {{ manufacturerCompanyId: 1 }}
      establishments:
        test:
          mechanism: test
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
fn if_tolerant_unbound_field_runs_else_branch() {
    // `missingStyle` is never seeded into scope; `tolerant: true` makes the
    // unbound field evaluate false → the else branch runs and the walk does
    // not error (§11.6).
    let idx = index_with_steps(
        r#"          - if:
              condition: { missingStyle: { eq: "red" } }
              tolerant: true
              then:
                - bleWrite: { gatt: thenChar, value: { literal: "01" } }
              else:
                - bleWrite: { gatt: elseChar, value: { literal: "02" } }
"#,
    );
    let mut responder = BleResponder::new([THEN.to_string(), ELSE.to_string()]);

    walk_establishment(
        &mut responder,
        &steps_of(&idx),
        &BTreeMap::new(),
        &BTreeMap::new(),
        &BTreeMap::new(),
    )
    .expect("an unbound field under tolerant: true takes the else path, no error");

    assert!(
        responder.written(THEN).is_empty(),
        "the then branch must not run for an unbound field"
    );
    let else_writes = responder.written(ELSE);
    assert_eq!(else_writes.len(), 1, "the else branch ran exactly once");
    assert_eq!(else_writes[0], &[0x02]);
}

#[test]
fn if_nontolerant_unbound_field_is_a_hard_error() {
    // The same unbound field WITHOUT `tolerant` (defaults false) aborts the
    // walk rather than silently skipping — the strict §11.6 path.
    let idx = index_with_steps(
        r#"          - if:
              condition: { missingStyle: { eq: "red" } }
              then:
                - bleWrite: { gatt: thenChar, value: { literal: "01" } }
"#,
    );
    let mut responder = BleResponder::new([THEN.to_string(), ELSE.to_string()]);

    let err = match walk_establishment(
        &mut responder,
        &steps_of(&idx),
        &BTreeMap::new(),
        &BTreeMap::new(),
        &BTreeMap::new(),
    ) {
        Err(e) => e,
        Ok(_) => panic!("an unbound predicate field without tolerant must error"),
    };

    assert!(
        err.message.contains("missingStyle") && err.message.contains("unbound"),
        "the error names the unbound predicate field: {err}"
    );
    assert!(
        responder.written(THEN).is_empty(),
        "the then branch never ran"
    );
}
