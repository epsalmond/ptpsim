//! `bleAwaitUntil` (§11.15) reference-walker coverage. Synthetic establishment
//! plans modeled on the Sony Wi-Fi-handoff V2 shape (observe a launch-status
//! characteristic until launched, writing the launch request each iteration it
//! isn't) — NOT shipped Sony data (that stays Phase 2, protocol-mapper-sourced).

use std::collections::BTreeMap;

use camera_config::index::{ResolvedManufacturerIndex, RetryFailureKind, Step};
use camera_sim::{walk_establishment, BleEvent, BleResponder};

/// A synthetic single-family index whose establishment is `steps`. `gatt` maps
/// the two symbolic names used below to UUIDs the responder is keyed on.
fn index_with_steps(steps: &str) -> ResolvedManufacturerIndex {
    // The plan now nests steps two levels deeper (establishments → <mechanism>
    // → steps), so bump the caller-supplied step block by two spaces to match.
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
        launchState: "0000CC09-0000-1000-8000-00805F9B34FB"
        launchRequest: "0000CC08-0000-1000-8000-00805F9B34FB"
        failureDetail: "0000CC08-0000-1000-8000-00805F9B34FB"
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

const CC09: &str = "0000CC09-0000-1000-8000-00805F9B34FB";
const CC08: &str = "0000CC08-0000-1000-8000-00805F9B34FB";

#[test]
fn notify_observe_until_runs_on_each_then_exits_satisfied() {
    // Sony V2 shape: observe CC09 until status byte == 1 (launched); each
    // not-yet-launched notification, write the launch request to CC08.
    let idx = index_with_steps(
        r#"          - bleAwaitUntil:
              source: { notify: { gatt: launchState } }
              capture: { at: 0, length: 1, encoding: u8, name: wifiStatus }
              until: { wifiStatus: { eq: 1 } }
              onEach:
                - bleWrite: { gatt: launchRequest, value: { literal: "01" } }
              timeoutMs: 5000
"#,
    );
    // Queue two notifications: not-launched, then launched.
    let mut responder = BleResponder::new([CC09.to_string(), CC08.to_string()])
        .queue_notification(CC09, &[0x00])
        .queue_notification(CC09, &[0x01]);

    let outcome = walk_establishment(
        &mut responder,
        &steps_of(&idx),
        &BTreeMap::new(),
        &BTreeMap::new(),
        &BTreeMap::new(),
    )
    .expect("await loop completes once launched");

    // wifiStatus ended satisfied (== "1").
    assert_eq!(
        outcome.scope.get("wifiStatus").map(String::as_str),
        Some("1")
    );
    // onEach ran exactly once — after the not-launched notification, before
    // the launched one — so CC08 was written exactly once.
    let writes: Vec<&[u8]> = responder.written(CC08);
    assert_eq!(writes.len(), 1, "launch request written once: {writes:?}");
    assert_eq!(writes[0], &[0x01]);
    // The subscribe happened once up front, then two notification observations.
    let subs = responder
        .log()
        .iter()
        .filter(|e| matches!(e, BleEvent::Subscribe { uuid, .. } if uuid == CC09))
        .count();
    assert_eq!(subs, 1, "CCCD enabled once for the notify source");
}

#[test]
fn seeded_notify_completes_from_one_already_satisfying_read() {
    let idx = index_with_steps(
        r#"          - bleAwaitUntil:
              source: { notify: { gatt: launchState, seedRead: true } }
              capture: { at: 0, length: 1, encoding: u8, name: wifiStatus }
              until: { wifiStatus: { eq: 1 } }
              timeoutMs: 5000
"#,
    );
    let mut responder = BleResponder::new([CC09.to_string()]).serve_read(CC09, &[0x01]);

    let outcome = walk_establishment(
        &mut responder,
        &steps_of(&idx),
        &BTreeMap::new(),
        &BTreeMap::new(),
        &BTreeMap::new(),
    )
    .expect("the seed read observes an already-launched state");

    assert_eq!(
        outcome.scope.get("wifiStatus").map(String::as_str),
        Some("1")
    );
    let observations: Vec<&BleEvent> = responder
        .log()
        .iter()
        .filter(|event| {
            matches!(
                event,
                BleEvent::Subscribe { uuid, .. } | BleEvent::Read { uuid } if uuid == CC09
            )
        })
        .collect();
    assert!(matches!(observations[0], BleEvent::Subscribe { .. }));
    assert!(matches!(observations[1], BleEvent::Read { .. }));
    assert_eq!(observations.len(), 2, "subscribe once, seed-read once");
}

#[test]
fn seeded_notify_reads_once_then_waits_for_a_notification() {
    let idx = index_with_steps(
        r#"          - bleAwaitUntil:
              source: { notify: { gatt: launchState, seedRead: true } }
              capture: { at: 0, length: 1, encoding: u8, name: wifiStatus }
              until: { wifiStatus: { eq: 1 } }
              onEach:
                - bleWrite: { gatt: launchRequest, value: { literal: "01" } }
              timeoutMs: 5000
"#,
    );
    let mut responder = BleResponder::new([CC09.to_string(), CC08.to_string()])
        .serve_read(CC09, &[0x02])
        .queue_notification(CC09, &[0x01]);

    let outcome = walk_establishment(
        &mut responder,
        &steps_of(&idx),
        &BTreeMap::new(),
        &BTreeMap::new(),
        &BTreeMap::new(),
    )
    .expect("the terminal notification follows an unsatisfying seed");

    assert_eq!(
        outcome.scope.get("wifiStatus").map(String::as_str),
        Some("1")
    );
    let reads = responder
        .log()
        .iter()
        .filter(|event| matches!(event, BleEvent::Read { uuid } if uuid == CC09))
        .count();
    assert_eq!(reads, 1, "a seeded notify never becomes a read poll");
    assert_eq!(
        responder.written(CC08),
        vec![&[0x01][..]],
        "onEach runs once after the unsatisfying seed"
    );
}

#[test]
fn read_source_still_rejects_from_fail_when() {
    let idx = index_with_steps(
        r#"          - bleAwaitUntil:
              source: { read: launchState }
              capture: { at: 0, length: 1, encoding: u8, name: wifiStatus }
              until: { wifiStatus: { eq: 1 } }
              failWhen: { wifiStatus: { eq: 0 } }
              timeoutMs: 5000
"#,
    );
    let mut responder = BleResponder::new([CC09.to_string()]).serve_read(CC09, &[0x00]);

    let error = match walk_establishment(
        &mut responder,
        &steps_of(&idx),
        &BTreeMap::new(),
        &BTreeMap::new(),
        &BTreeMap::new(),
    ) {
        Err(error) => error,
        Ok(_) => panic!("ordinary read observations must remain eligible for failWhen"),
    };

    assert_eq!(error.kind, RetryFailureKind::ConditionRejected);
}

#[test]
fn unconfirmed_rejection_state_can_transition_to_success() {
    let idx = index_with_steps(
        r#"          - bleAwaitUntil:
              source: { notify: { gatt: launchState } }
              capture: { at: 0, length: 1, encoding: u8, name: wifiStatus }
              until: { wifiStatus: { eq: 1 } }
              failWhen: { wifiStatus: { eq: 0 } }
              failureEvidence:
                steps:
                  - bleRead: { gatt: failureDetail, encoding: u8, captureAs: failureDetail }
                when: { failureDetail: { ne: 0 } }
              timeoutMs: 5000
"#,
    );
    let mut responder = BleResponder::new([CC09.to_string(), CC08.to_string()])
        .serve_read(CC08, &[0x00])
        .queue_notification(CC09, &[0x00])
        .queue_notification(CC09, &[0x01]);

    let outcome = walk_establishment(
        &mut responder,
        &steps_of(&idx),
        &BTreeMap::new(),
        &BTreeMap::new(),
        &BTreeMap::new(),
    )
    .expect("an intermediate rejection-shaped state does not end the await");

    assert_eq!(
        outcome.scope.get("wifiStatus").map(String::as_str),
        Some("1")
    );
}

#[test]
fn failed_evidence_probe_cannot_reuse_stale_detail() {
    let idx = index_with_steps(
        r#"          - bleAwaitUntil:
              source: { notify: { gatt: launchState } }
              capture: { at: 0, length: 1, encoding: u8, name: wifiStatus }
              until: { wifiStatus: { eq: 1 } }
              failWhen: { wifiStatus: { eq: 0 } }
              failureEvidence:
                steps:
                  - bleRead: { gatt: failureDetail, encoding: u8, captureAs: failureDetail, tolerant: true }
                when: { failureDetail: { ne: 0 } }
              timeoutMs: 5000
"#,
    );
    let mut responder = BleResponder::new([CC09.to_string()])
        .queue_notification(CC09, &[0x00])
        .queue_notification(CC09, &[0x01]);
    let initial_scope = BTreeMap::from([("failureDetail".to_string(), "2".to_string())]);

    let outcome = walk_establishment(
        &mut responder,
        &steps_of(&idx),
        &initial_scope,
        &BTreeMap::new(),
        &BTreeMap::new(),
    )
    .expect("a failed probe clears stale evidence and keeps observing");

    assert_eq!(
        outcome.scope.get("wifiStatus").map(String::as_str),
        Some("1")
    );
    assert!(!outcome.scope.contains_key("failureDetail"));
}

#[test]
fn confirmed_rejection_state_is_terminal() {
    let idx = index_with_steps(
        r#"          - bleAwaitUntil:
              source: { notify: { gatt: launchState } }
              capture: { at: 0, length: 1, encoding: u8, name: wifiStatus }
              until: { wifiStatus: { eq: 1 } }
              failWhen: { wifiStatus: { eq: 0 } }
              failureEvidence:
                steps:
                  - bleRead: { gatt: failureDetail, encoding: u8, captureAs: failureDetail }
                when: { failureDetail: { ne: 0 } }
              timeoutMs: 5000
"#,
    );
    let mut responder = BleResponder::new([CC09.to_string(), CC08.to_string()])
        .serve_read(CC08, &[0x02])
        .queue_notification(CC09, &[0x00]);

    let error = match walk_establishment(
        &mut responder,
        &steps_of(&idx),
        &BTreeMap::new(),
        &BTreeMap::new(),
        &BTreeMap::new(),
    ) {
        Err(error) => error,
        Ok(_) => panic!("nonzero detail must confirm the rejection-shaped state"),
    };

    assert_eq!(error.kind, RetryFailureKind::ConditionRejected);
    assert!(error.message.contains("failureEvidence.when"));
}

#[test]
fn read_poll_until_exits_on_the_satisfying_read() {
    // Pure poll: read a color property until it flips to 1 (locked). Evolving
    // sequence [focusing, focusing, locked]; no onEach.
    let idx = index_with_steps(
        r#"          - bleAwaitUntil:
              source: { read: launchState }
              capture: { at: 0, length: 1, encoding: u8, name: afColor }
              until: { afColor: { eq: 1 } }
              timeoutMs: 3000
"#,
    );
    let mut responder = BleResponder::new([CC09.to_string()])
        .serve_read_sequence(CC09, vec![vec![0x00], vec![0x00], vec![0x01]]);

    let outcome = walk_establishment(
        &mut responder,
        &steps_of(&idx),
        &BTreeMap::new(),
        &BTreeMap::new(),
        &BTreeMap::new(),
    )
    .expect("poll exits on the satisfying read");

    assert_eq!(outcome.scope.get("afColor").map(String::as_str), Some("1"));
    // Exactly three reads (two unsatisfied + the satisfying one).
    let reads = responder
        .log()
        .iter()
        .filter(|e| matches!(e, BleEvent::Read { uuid } if uuid == CC09))
        .count();
    assert_eq!(reads, 3, "polled until the third read satisfied `until`");
}

#[test]
fn source_exhaustion_is_a_tolerant_aware_timeout() {
    // Notifications drain without ever satisfying `until` → step fails.
    let plan = r#"          - bleAwaitUntil:
              source: { notify: { gatt: launchState } }
              capture: { at: 0, length: 1, encoding: u8, name: wifiStatus }
              until: { wifiStatus: { eq: 1 } }
              timeoutMs: 5000
{tol}"#;

    // Non-tolerant: hard error.
    let idx = index_with_steps(&plan.replace("{tol}", ""));
    let mut responder = BleResponder::new([CC09.to_string()]).queue_notification(CC09, &[0x00]); // never launched
    let err = match walk_establishment(
        &mut responder,
        &steps_of(&idx),
        &BTreeMap::new(),
        &BTreeMap::new(),
        &BTreeMap::new(),
    ) {
        Err(e) => e,
        Ok(_) => panic!("exhausted source without satisfying `until` must fail"),
    };
    assert!(err.message.contains("source exhausted"), "got: {err}");

    // tolerant: true — the timeout is swallowed and the walk continues.
    let idx = index_with_steps(&plan.replace("{tol}", "              tolerant: true\n"));
    let mut responder = BleResponder::new([CC09.to_string()]).queue_notification(CC09, &[0x00]);
    let outcome = walk_establishment(
        &mut responder,
        &steps_of(&idx),
        &BTreeMap::new(),
        &BTreeMap::new(),
        &BTreeMap::new(),
    )
    .expect("tolerant await swallows the timeout");
    // connect + discovery + the tolerant await all count as run.
    assert_eq!(outcome.steps_run, 3, "the tolerant await counts as run");
}

#[test]
fn already_satisfied_on_first_observation_skips_on_each() {
    // If the first notification already satisfies `until`, onEach never runs.
    let idx = index_with_steps(
        r#"          - bleAwaitUntil:
              source: { notify: { gatt: launchState } }
              capture: { at: 0, length: 1, encoding: u8, name: wifiStatus }
              until: { wifiStatus: { eq: 1 } }
              onEach:
                - bleWrite: { gatt: launchRequest, value: { literal: "01" } }
              timeoutMs: 5000
"#,
    );
    let mut responder =
        BleResponder::new([CC09.to_string(), CC08.to_string()]).queue_notification(CC09, &[0x01]); // launched immediately

    let _ = walk_establishment(
        &mut responder,
        &steps_of(&idx),
        &BTreeMap::new(),
        &BTreeMap::new(),
        &BTreeMap::new(),
    )
    .expect("completes on the first satisfying notification");
    assert!(
        responder.written(CC08).is_empty(),
        "onEach must not run when already satisfied"
    );
}
