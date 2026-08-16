//! Issue #452: executor walk for legacy-app-establish-wifi-ap launch await semantics.
//! Synthetic plan mirroring the X-A7 (legacy manufacturer app) flow: subscribe AP state
//! (indicate), write launchMode, then await indications. State 1 is launched,
//! state 0 is failure, state 2 is transitional and ignored.

use std::collections::BTreeMap;

use camera_config::index::{ResolvedManufacturerIndex, RetryFailureKind, Step};
use camera_sim::{walk_establishment, BleEvent, BleResponder};

fn index_with_legacy_steps() -> (ResolvedManufacturerIndex, String, String, String) {
    let ap_state = "A68E3F66-0FCC-4395-8D4C-AA980B5877FA";
    let launch = "600655E6-3637-42F1-8FB2-44EFC5C63B13";
    let ssid = "BF6DC9CF-3606-4EC9-A4C8-D77576E93EA4";
    let transfer = "BD17BA04-B76B-4892-A545-B73BA1F74DAE";
    let yaml = format!(
        r#"
manufacturer: FUJIFILM
families:
  fuji:
    ble:
      gatt:
        apState: "{ap_state}"
        functionLaunchRequest: "{launch}"
        cameraSSIDNameString: "{ssid}"
        transferState: "{transfer}"
      advert: {{}}
      establishments:
        legacy-app-pair:
          mechanism: legacy-app-pair
          activities:
            - {{ id: camera.remote.registration, version: 1, displayRole: confirmingPairing, defaultExpectedDurationMs: 1, interactionRequired: false, executorSpan: {{ sequence: steps, startStep: 0, endStepExclusive: 2 }} }}
          steps:
            - bleConnect: {{}}
            - bleDiscoverServices: {{}}
        legacy-app-establish-wifi-ap:
          mechanism: legacy-app-establish-wifi-ap
          prerequisite: legacy-app-pair
          onDemand: true
          params: [launchMode]
          persist: [ssid]
          activities:
            - {{ id: camera.remote.ap-launch, version: 1, displayRole: startingNetwork, defaultExpectedDurationMs: 1, interactionRequired: false, executorSpan: {{ sequence: steps, startStep: 0, endStepExclusive: 6 }} }}
          steps:
            - bleConnect: {{}}
            - bleDiscoverServices: {{}}
            - bleSubscribe: {{ gatt: apState, timeoutMs: 3000, mode: indicate }}
            - bleWrite:
                gatt: functionLaunchRequest
                value: {{ runtime: launchMode, encoding: u16-le }}
                notificationFence: apState
            - bleAwaitUntil:
                source: {{ notify: {{ gatt: apState, mode: indicate }} }}
                capture: {{ at: 0, length: 2, encoding: u16-le, name: apState }}
                captureAs: apStateRaw
                until: {{ apState: {{ eq: "1" }} }}
                failWhen: {{ apState: {{ eq: "0" }} }}
                timeoutMs: 20000
            - bleRead: {{ gatt: cameraSSIDNameString, encoding: utf8-cstring, captureAs: ssid }}
models:
  - id: xa7
    displayName: "X-A7"
    inherits: [fuji]
    manifest: xa7.yaml
"#,
    );
    let _body = r#"
schema: camera-config/v1
camera: {{ manufacturer: FUJIFILM, model: X-A7 }}
"#;
    // Use from_manufacturer_index to resolve inheritance; we need a ConfigStore helper
    // but for the walker we can just use ResolvedManufacturerIndex directly with a dummy body.
    // ResolvedManufacturerIndex::from_yaml merges families but does not validate body; we
    // embed the establishment in the index itself, so we can walk it directly.
    let idx = ResolvedManufacturerIndex::from_yaml(&yaml).expect("legacy index loads");
    (
        idx,
        ap_state.to_string(),
        launch.to_string(),
        ssid.to_string(),
    )
}

fn steps_of(idx: &ResolvedManufacturerIndex) -> Vec<Step> {
    idx.models
        .iter()
        .find(|m| m.id == "xa7")
        .expect("xa7 present")
        .ble
        .as_ref()
        .expect("xa7 ble")
        .establishment("legacy-app-establish-wifi-ap")
        .expect("legacy-app-establish-wifi-ap")
        .steps
        .clone()
}

#[test]
fn legacy_app_establish_awaits_indications_and_completes_on_state_1() {
    let (idx, ap_state, launch, ssid) = index_with_legacy_steps();
    let steps = steps_of(&idx);
    // Responder scripted with 02 then 01 after the fenced write
    let mut responder = BleResponder::new([ap_state.clone(), launch.clone(), ssid.clone()])
        .queue_notification_after_fenced_write(&ap_state, &launch, 1, &[0x02, 0x00])
        .queue_notification_after_fenced_write(&ap_state, &launch, 1, &[0x01, 0x00])
        .serve_read(&ssid, b"MY-AP");
    let mut runtime = BTreeMap::new();
    // launchMode 4 -> u16-le 04 00
    runtime.insert("launchMode".to_string(), "4".to_string());
    // walk_establishment expects runtime_params as strings; value will be encoded as u16-le
    // via resolve_value which parses the string as integer and encodes.
    let outcome = walk_establishment(
        &mut responder,
        &steps,
        &BTreeMap::new(),
        &BTreeMap::new(),
        &runtime,
    )
    .expect("walk completes on 01 00");
    // No pre-launch read of apState
    let reads: Vec<_> = responder
        .log()
        .iter()
        .filter(|e| matches!(e, BleEvent::Read { uuid } if uuid == &ap_state))
        .collect();
    assert!(
        reads.is_empty(),
        "no pre-launch AP-state read, got {reads:?}"
    );
    // Launch write carries u16-le launch mode
    let writes = responder.written(&launch);
    assert_eq!(writes.len(), 1);
    assert_eq!(writes[0], &[0x04, 0x00], "launch mode 4 as u16-le");
    // Walk binds ssid
    assert_eq!(outcome.scope.get("ssid").map(String::as_str), Some("MY-AP"));
    assert_eq!(outcome.scope.get("apState").map(String::as_str), Some("1"));
}

#[test]
fn legacy_app_establish_ignores_state_2_and_does_not_satisfy() {
    let (idx, ap_state, launch, ssid) = index_with_legacy_steps();
    let steps = steps_of(&idx);
    let mut responder = BleResponder::new([ap_state.clone(), launch.clone(), ssid.clone()])
        .queue_notification_after_fenced_write(&ap_state, &launch, 1, &[0x02, 0x00])
        .serve_read(&ssid, b"MY-AP");
    let mut runtime = BTreeMap::new();
    runtime.insert("launchMode".to_string(), "4".to_string());
    let err = match walk_establishment(
        &mut responder,
        &steps,
        &BTreeMap::new(),
        &BTreeMap::new(),
        &runtime,
    ) {
        Ok(_) => panic!("02 alone must deadline"),
        Err(e) => e,
    };
    // Timeout / deadline, not conditionRejected
    assert_eq!(err.kind, RetryFailureKind::DeadlineExceeded);
}

#[test]
fn legacy_app_establish_state_0_yields_typed_failure() {
    let (idx, ap_state, launch, ssid) = index_with_legacy_steps();
    let steps = steps_of(&idx);
    let mut responder = BleResponder::new([ap_state.clone(), launch.clone(), ssid.clone()])
        .queue_notification_after_fenced_write(&ap_state, &launch, 1, &[0x00, 0x00])
        .serve_read(&ssid, b"MY-AP");
    let mut runtime = BTreeMap::new();
    runtime.insert("launchMode".to_string(), "4".to_string());
    let err = match walk_establishment(
        &mut responder,
        &steps,
        &BTreeMap::new(),
        &BTreeMap::new(),
        &runtime,
    ) {
        Ok(_) => panic!("00 must trip failWhen"),
        Err(e) => e,
    };
    assert_eq!(err.kind, RetryFailureKind::ConditionRejected);
    // Registration tail order is not exercised here, but ensure the legacy-app-pair
    // shape asserts transferState after apState (see manufacturer_index test).
}

#[test]
fn legacy_app_registration_tail_reads_ap_then_transfer() {
    // Shape assertion for legacy-app-pair registration tail: apState then transferState
    let yaml = r#"
manufacturer: FUJIFILM
families:
  fuji:
    ble:
      gatt:
        apState: "A68E3F66-0FCC-4395-8D4C-AA980B5877FA"
        transferState: "BD17BA04-B76B-4892-A545-B73BA1F74DAE"
        cameraSSIDNameString: "BF6DC9CF-3606-4EC9-A4C8-D77576E93EA4"
        gapDeviceName: "00002A00-0000-1000-8000-00805F9B34FB"
        protectedSerialString: "00002A25-0000-1000-8000-00805F9B34FB"
        pairingKey: "ABA356EB-9633-4E60-B73F-F52516DBD671"
        deviceNameString: "85B9163E-62D1-49FF-A6F5-054B4630D4A1"
      advert: {}
      establishments:
        legacy-app-pair:
          mechanism: legacy-app-pair
          activities:
            - { id: camera.remote.registration, version: 1, displayRole: confirmingPairing, defaultExpectedDurationMs: 1, interactionRequired: false, executorSpan: { sequence: steps, startStep: 0, endStepExclusive: 5 } }
          steps:
            - bleConnect: {}
            - bleSubscribe: { gatt: apState, timeoutMs: 3000, mode: indicate }
            - bleSubscribe: { gatt: transferState, timeoutMs: 3000, mode: indicate }
            - bleRead: { gatt: apState, encoding: u16-le, captureAs: apState }
            - bleRead: { gatt: transferState, encoding: u16-le, captureAs: transferState }
models:
  - id: xa7
    displayName: "X-A7"
    inherits: [fuji]
    manifest: xa7.yaml
"#;
    let idx = ResolvedManufacturerIndex::from_yaml(yaml).expect("legacy pair index loads");
    let steps = idx
        .models
        .iter()
        .find(|m| m.id == "xa7")
        .unwrap()
        .ble
        .as_ref()
        .unwrap()
        .establishment("legacy-app-pair")
        .unwrap()
        .steps
        .clone();
    // Last two steps must be apState then transferState reads
    assert!(steps.len() >= 2);
    let last = &steps[steps.len() - 1];
    let prev = &steps[steps.len() - 2];
    match (prev, last) {
        (Step::BleRead(a), Step::BleRead(b)) => {
            assert_eq!(a.capture_as, "apState");
            assert_eq!(b.capture_as, "transferState");
        }
        _ => panic!("expected tail reads apState then transferState, got {prev:?} {last:?}"),
    }
}
