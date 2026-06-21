//! `acquire` reference-walker coverage (#44). `acquire` aliases its delegate's
//! *explicit* capture under a new name. These pin the two cases the old scope
//! set-diff mis-handled — a delegate overwriting a pre-existing (recognize-
//! seeded) key, and a multi-capture delegate — plus the delegate honoring its
//! OWN `tolerant` flag rather than the acquire's. No shipped manifest uses
//! `acquire` yet; this file is the conformance oracle a platform dispatcher
//! follows, so the semantics are pinned before the first acquire-using vendor.

use std::collections::BTreeMap;

use camera_config::index::{ResolvedManufacturerIndex, Step};
use camera_sim::{walk_establishment, BleResponder};

const ID: &str = "0000AA01-0000-1000-8000-00805F9B34FB";
const STATUS: &str = "0000AA02-0000-1000-8000-00805F9B34FB";

/// A synthetic single-family index whose establishment is `bleConnect` + the
/// caller's `steps`. `idChar`/`statusChar` resolve to the UUIDs the responder
/// is keyed on. Mirrors the `ble_await_until.rs` harness (steps bumped two
/// spaces to land under `establishments.<m>.steps`).
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
        idChar: "{ID}"
        statusChar: "{STATUS}"
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
fn acquire_aliases_delegate_capture_by_name() {
    // The base case: acquire a bleRead's capture under a new name.
    let idx = index_with_steps(
        r#"          - acquire:
              name: deviceId
              from:
                bleRead: { gatt: idChar, encoding: ascii, captureAs: rawId }
"#,
    );
    let mut responder = BleResponder::new([ID.to_string()]).serve_read(ID, b"ABCDE");

    let outcome = walk_establishment(
        &mut responder,
        &steps_of(&idx),
        &BTreeMap::new(),
        &BTreeMap::new(),
        &BTreeMap::new(),
    )
    .expect("acquire over a bleRead succeeds");

    assert_eq!(outcome.scope.get("rawId").map(String::as_str), Some("ABCDE"));
    assert_eq!(
        outcome.scope.get("deviceId").map(String::as_str),
        Some("ABCDE"),
        "acquire aliases the delegate's explicit capture under its name"
    );
}

#[test]
fn acquire_rebinds_when_delegate_overwrites_seeded_key() {
    // Finding 1, overwrite case: the delegate captures into a key already in
    // scope (here a recognize-seeded `rawId`). The old set-diff saw no NEW key
    // and aliased nothing; binding by the delegate's declared name picks up the
    // freshly-read value.
    let idx = index_with_steps(
        r#"          - acquire:
              name: deviceId
              from:
                bleRead: { gatt: idChar, encoding: ascii, captureAs: rawId }
"#,
    );
    let mut responder = BleResponder::new([ID.to_string()]).serve_read(ID, b"NEW");
    let seeded = BTreeMap::from([("rawId".to_string(), "OLD".to_string())]);

    let outcome = walk_establishment(
        &mut responder,
        &steps_of(&idx),
        &seeded,
        &BTreeMap::new(),
        &BTreeMap::new(),
    )
    .expect("acquire over an overwriting delegate succeeds");

    assert_eq!(
        outcome.scope.get("rawId").map(String::as_str),
        Some("NEW"),
        "the delegate overwrote the seeded key"
    );
    assert_eq!(
        outcome.scope.get("deviceId").map(String::as_str),
        Some("NEW"),
        "acquire aliases the fresh value, not nothing (the old set-diff bug)"
    );
}

#[test]
fn acquire_binds_declared_target_not_smallest_new_key() {
    // Finding 1, multi-capture case: the delegate binds both its `captureAs`
    // (whole payload) and an extra field `capture`. The old set-diff picked the
    // lexicographically-smallest new key (`aByte`); binding by name picks the
    // declared `captureAs` (`zWhole`).
    let idx = index_with_steps(
        r#"          - acquire:
              name: result
              from:
                bleNotify:
                  gatt: statusChar
                  until: any
                  captureAs: zWhole
                  capture: [{ at: 0, length: 1, encoding: u8, name: aByte }]
                  timeoutMs: 3000
"#,
    );
    let mut responder =
        BleResponder::new([STATUS.to_string()]).queue_notification(STATUS, &[0x07]);

    let outcome = walk_establishment(
        &mut responder,
        &steps_of(&idx),
        &BTreeMap::new(),
        &BTreeMap::new(),
        &BTreeMap::new(),
    )
    .expect("acquire over a multi-capture bleNotify succeeds");

    // zWhole = hex of the whole payload; aByte = u8 of byte 0 — distinct values.
    assert_eq!(outcome.scope.get("zWhole").map(String::as_str), Some("07"));
    assert_eq!(outcome.scope.get("aByte").map(String::as_str), Some("7"));
    assert_eq!(
        outcome.scope.get("result").map(String::as_str),
        Some("07"),
        "acquire binds the declared captureAs, not the smallest new key"
    );
}

#[test]
fn acquire_honors_delegate_tolerant_flag() {
    // Finding 2: the delegate runs through `walk_steps`, so its OWN `tolerant`
    // flag governs its failure. A failing read (no value served) under a
    // tolerant delegate is swallowed; acquire then has nothing to bind, and
    // that is not an error.
    let tolerant = index_with_steps(
        r#"          - acquire:
              name: deviceId
              from:
                bleRead: { gatt: idChar, encoding: ascii, captureAs: rawId, tolerant: true }
"#,
    );
    let mut responder = BleResponder::new([ID.to_string()]); // idChar in catalog, no value served

    let outcome = walk_establishment(
        &mut responder,
        &steps_of(&tolerant),
        &BTreeMap::new(),
        &BTreeMap::new(),
        &BTreeMap::new(),
    )
    .expect("a tolerant delegate's read failure is swallowed, not propagated");
    assert_eq!(
        outcome.scope.get("deviceId"),
        None,
        "nothing was captured, so nothing is aliased"
    );
    assert_eq!(outcome.scope.get("rawId"), None);

    // The non-tolerant twin over the same failing read is a hard error — the
    // delegate's failure propagates (acquire's own absence of tolerance).
    let strict = index_with_steps(
        r#"          - acquire:
              name: deviceId
              from:
                bleRead: { gatt: idChar, encoding: ascii, captureAs: rawId }
"#,
    );
    let mut responder = BleResponder::new([ID.to_string()]);
    let err = match walk_establishment(
        &mut responder,
        &steps_of(&strict),
        &BTreeMap::new(),
        &BTreeMap::new(),
        &BTreeMap::new(),
    ) {
        Err(e) => e,
        Ok(_) => panic!("a non-tolerant delegate's read failure must abort the walk"),
    };
    assert!(
        err.step.contains("acquire"),
        "the failure is attributed to the acquire's delegate: {err}"
    );
}
