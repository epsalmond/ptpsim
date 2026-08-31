//! USB step verbs and the family `usb` block (§11.29).
//!
//! Exercises the index-build path (`ResolvedManufacturerIndex::from_yaml`)
//! for the USB establishment vocabulary: verb parsing, symbolic interface
//! validation, and the two-directional verb scoping between BLE and USB
//! establishment plans.

use std::collections::BTreeMap;

use camera_config::index::{
    Encoding, FamilyBlock, ResolvedManufacturerIndex, Step, StepValue, Transform,
    UsbInterfaceTriple,
};
use camera_config::ConfigStore;

/// Synthetic index with one family carrying a `usb` block. `steps` is spliced
/// into the single `usb-claim-session` establishment plan.
fn usb_index(steps: &str, top_level_step_count: usize) -> String {
    format!(
        r#"
manufacturer: TESTCO
families:
  test:
    usb:
      interfaces:
        stillImage: {{ class: 6, subclass: 1, protocol: 1 }}
        vendor: {{ class: 255, subclass: 255, protocol: 0 }}
      establishments:
        usb-claim-session:
          mechanism: usb-claim-session
          activities:
            - {{ id: camera.test.usbClaim, version: 1, displayRole: connecting, defaultExpectedDurationMs: 1, interactionRequired: false, executorSpan: {{ sequence: steps, startStep: 0, endStepExclusive: {top_level_step_count} }} }}
          steps:
{steps}
models:
  - id: tm1
    displayName: Test
    inherits: [test]
    manifest: tm1.yaml
"#
    )
}

/// Synthetic index with one family carrying a `ble` block, for the
/// USB-verb-in-BLE-plan scoping direction.
fn ble_index(steps: &str) -> String {
    format!(
        r#"
manufacturer: TESTCO
families:
  test:
    ble:
      gatt:
        status: "00002A25-0000-1000-8000-00805F9B34FB"
      establishments:
        pair:
          mechanism: pair
          steps:
{steps}
models:
  - id: tm1
    displayName: Test
    inherits: [test]
    manifest: tm1.yaml
"#
    )
}

fn ble_action_index(steps: &str) -> String {
    format!(
        r#"
manufacturer: TESTCO
families:
  test:
    ble:
      gatt:
        status: "00002A25-0000-1000-8000-00805F9B34FB"
      actions:
        test:
          steps:
{steps}
models:
  - id: tm1
    displayName: Test
    inherits: [test]
    manifest: tm1.yaml
"#
    )
}

fn usb_post_exit_index(steps: &str, count: usize) -> String {
    format!(
        r#"
manufacturer: TESTCO
families:
  test:
    usb:
      interfaces:
        stillImage: {{ class: 6, subclass: 1, protocol: 1 }}
      establishments:
        usb-claim-session:
          mechanism: usb-claim-session
          activities:
            - {{ id: camera.test.readiness, version: 1, displayRole: waitingForCamera, defaultExpectedDurationMs: 1, interactionRequired: false, executorSpan: {{ sequence: postExitReadiness, startStep: 0, endStepExclusive: {count} }} }}
          postExitReadiness:
{steps}
          steps: []
models:
  - id: tm1
    displayName: Test
    inherits: [test]
    manifest: tm1.yaml
"#
    )
}

#[test]
fn usb_verbs_parse_from_one_entry_mappings() {
    let index = ResolvedManufacturerIndex::from_yaml(&usb_index(
        "            - usbClaim: { interface: stillImage }\n            - usbBulkOut: { data: { captured: openSessionContainer } }\n            - usbBulkIn: { maxLength: 512, encoding: bytes-raw, captureAs: openSessionResponse }\n            - usbAwaitInterrupt: { encoding: bytes, captureAs: eventFrame }",
        4,
    ))
    .expect("USB establishment verbs parse");

    let usb = index.models[0]
        .usb
        .as_ref()
        .expect("family usb block merges into the model view");
    let plan = usb
        .establishment("usb-claim-session")
        .expect("plan resolves by mechanism");
    assert!(usb.establishment("notDeclared").is_none());

    let verbs: Vec<&str> = plan.steps.iter().map(Step::verb_name).collect();
    assert_eq!(
        verbs,
        ["usbClaim", "usbBulkOut", "usbBulkIn", "usbAwaitInterrupt"]
    );

    let Step::UsbClaim(claim) = &plan.steps[0] else {
        panic!("steps[0] is usbClaim, got {:?}", plan.steps[0]);
    };
    assert_eq!(claim.interface, "stillImage");

    let Step::UsbBulkOut(out) = &plan.steps[1] else {
        panic!("steps[1] is usbBulkOut, got {:?}", plan.steps[1]);
    };
    assert!(
        matches!(&out.data, StepValue::Captured { captured, .. } if captured == "openSessionContainer"),
        "usbBulkOut data keeps its StepValue form, got {:?}",
        out.data,
    );

    let Step::UsbBulkIn(bulk_in) = &plan.steps[2] else {
        panic!("steps[2] is usbBulkIn, got {:?}", plan.steps[2]);
    };
    assert_eq!(bulk_in.max_length, 512);
    assert_eq!(bulk_in.encoding, Encoding::BytesRaw);
    assert_eq!(bulk_in.capture_as, "openSessionResponse");

    let Step::UsbAwaitInterrupt(interrupt) = &plan.steps[3] else {
        panic!("steps[3] is usbAwaitInterrupt, got {:?}", plan.steps[3]);
    };
    assert_eq!(interrupt.encoding, Encoding::Bytes);
    assert_eq!(interrupt.capture_as, "eventFrame");
}

#[test]
fn usb_bulk_in_parses_capture_pipeline() {
    let index = ResolvedManufacturerIndex::from_yaml(&usb_index(
        "            - usbBulkIn: { maxLength: 512, encoding: bytes, captureAs: response, transform: { dropPrefix: 4 } }\n            - usbAwaitInterrupt: { encoding: bytes, captureAs: eventFrame, transform: [ { dropPrefix: 4 }, { bitOr: 4 } ] }",
        2,
    ))
    .expect("capture pipeline fields parse");

    let plan = index.models[0]
        .usb
        .as_ref()
        .unwrap()
        .establishment("usb-claim-session")
        .unwrap();

    let Step::UsbBulkIn(bulk_in) = &plan.steps[0] else {
        panic!("steps[0] is usbBulkIn, got {:?}", plan.steps[0]);
    };
    assert_eq!(bulk_in.max_length, 512);
    assert_eq!(bulk_in.encoding, Encoding::Bytes);
    assert_eq!(bulk_in.capture_as, "response");
    assert_eq!(
        bulk_in.transform,
        vec![Transform::DropPrefix(4)],
        "a single transform mapping normalizes to a one-element chain (§11.13)",
    );

    let Step::UsbAwaitInterrupt(interrupt) = &plan.steps[1] else {
        panic!("steps[1] is usbAwaitInterrupt, got {:?}", plan.steps[1]);
    };
    assert_eq!(
        interrupt.transform,
        vec![Transform::DropPrefix(4), Transform::BitOr(4)],
        "a transform list keeps its order",
    );
}

#[test]
fn usb_claim_keeps_symbolic_name_with_family_triple_declared() {
    let index = ResolvedManufacturerIndex::from_yaml(&usb_index(
        "            - usbClaim: { interface: stillImage }",
        1,
    ))
    .expect("a declared interface name passes index build");

    let usb = index.models[0].usb.as_ref().unwrap();
    // camera-config keeps the symbolic name on the step; the triple stays in
    // the family interfaces map for the FFI mirror to resolve (§11.29).
    assert_eq!(
        usb.interfaces.get("stillImage"),
        Some(&UsbInterfaceTriple {
            class: 6,
            subclass: 1,
            protocol: 1,
        }),
    );
    let plan = usb.establishment("usb-claim-session").unwrap();
    let Step::UsbClaim(claim) = &plan.steps[0] else {
        panic!("steps[0] is usbClaim, got {:?}", plan.steps[0]);
    };
    assert_eq!(claim.interface, "stillImage");
}

#[test]
fn usb_claim_unknown_interface_fails_at_index_build() {
    let error = ResolvedManufacturerIndex::from_yaml(&usb_index(
        "            - usbClaim: { interface: notDeclared }",
        1,
    ))
    .expect_err("a claim naming an undeclared interface is a load-time error");
    assert!(
        error
            .to_string()
            .contains("undefined usb interface name 'notDeclared'"),
        "got: {error}",
    );
}

#[test]
fn unknown_usb_verb_fails_with_the_allowlist_message() {
    let error = ResolvedManufacturerIndex::from_yaml(&usb_index("            - usbFormat: {}", 1))
        .expect_err("usbFormat is not a verb");
    let message = error.to_string();
    assert!(
        message.contains("unknown step verb 'usbFormat'"),
        "got: {message}",
    );
    assert!(
        message.contains("usbClaim"),
        "the allowlist names the valid USB verbs, got: {message}",
    );
}

#[test]
fn ble_verb_rejected_inside_usb_plan() {
    let error = ResolvedManufacturerIndex::from_yaml(&usb_index(
        "            - bleRead: { gatt: status, encoding: bytes, captureAs: x }",
        1,
    ))
    .expect_err("BLE verbs do not run in a USB establishment plan");
    assert!(
        error
            .to_string()
            .contains("not valid in a USB establishment plan"),
        "got: {error}",
    );

    let nested = ResolvedManufacturerIndex::from_yaml(&usb_index(
        "            - if: { condition: { style: { eq: red } }, then: [ { bleConnect: {} } ] }",
        1,
    ))
    .expect_err("scoping walks into control-flow branches");
    assert!(
        nested
            .to_string()
            .contains("not valid in a USB establishment plan"),
        "got: {nested}",
    );
}

#[test]
fn ble_verb_rejected_inside_usb_acquire_source() {
    let error = ResolvedManufacturerIndex::from_yaml(&usb_index(
        "            - acquire: { name: status, from: { bleRead: { gatt: status, encoding: bytes, captureAs: status } } }",
        1,
    ))
    .expect_err("BLE verbs do not run inside a USB acquire delegate");
    assert!(
        error
            .to_string()
            .contains("not valid in a USB establishment plan"),
        "got: {error}",
    );
}

#[test]
fn ble_firmware_sources_are_rejected_inside_usb_plans() {
    for source in [
        "{ bleRead: { gatt: status, encoding: utf8 } }",
        "{ bleAdvert: { offset: 0, length: 4, encoding: utf8 } }",
    ] {
        let error = ResolvedManufacturerIndex::from_yaml(&usb_index(
            &format!("            - acquireFirmware: {{ from: {source} }}"),
            1,
        ))
        .expect_err("BLE-derived firmware sources do not run in raw USB plans");
        let message = error.to_string();
        assert!(message.contains("acquireFirmware"), "got: {message}");
        assert!(
            message.contains("not valid in a USB establishment plan"),
            "got: {message}"
        );
    }
}

#[test]
fn usb_verb_rejected_inside_ble_plan() {
    let error = ResolvedManufacturerIndex::from_yaml(&ble_index(
        "            - usbClaim: { interface: stillImage }",
    ))
    .expect_err("USB verbs do not run in a BLE establishment plan");
    assert!(
        error
            .to_string()
            .contains("not valid in a BLE establishment plan"),
        "got: {error}",
    );

    let nested = ResolvedManufacturerIndex::from_yaml(&ble_index(
        "            - if: { condition: { style: { eq: red } }, then: [ { usbBulkOut: { data: { captured: x } } } ] }",
    ))
    .expect_err("scoping walks into control-flow branches");
    assert!(
        nested
            .to_string()
            .contains("not valid in a BLE establishment plan"),
        "got: {nested}",
    );
}

#[test]
fn usb_verb_rejected_inside_ble_action() {
    let error = ResolvedManufacturerIndex::from_yaml(&ble_action_index(
        "            - usbBulkOut: { data: { captured: payload } }",
    ))
    .expect_err("USB verbs do not run in BLE actions");
    assert!(
        error
            .to_string()
            .contains("not valid in a BLE establishment plan"),
        "got: {error}",
    );
}

#[test]
fn ble_verb_rejected_inside_usb_post_exit_readiness() {
    let error = ResolvedManufacturerIndex::from_yaml(&usb_post_exit_index(
        "            - bleRead: { gatt: status, encoding: bytes, captureAs: status }",
        1,
    ))
    .expect_err("postExitReadiness keeps USB verb scoping");
    assert!(
        error
            .to_string()
            .contains("not valid in a USB establishment plan"),
        "got: {error}",
    );
}

#[test]
fn control_flow_verbs_stay_valid_in_usb_plans() {
    ResolvedManufacturerIndex::from_yaml(&usb_index(
        "            - if: { condition: { style: { eq: red } }, then: [ { usbClaim: { interface: stillImage } } ] }\n            - retry: { steps: [ { usbBulkOut: { data: { captured: openSessionContainer } } } ], whenFailure: other, retryWhen: { style: { eq: red } }, maxAttempts: 2 }\n            - acquire: { name: session, from: { usbBulkIn: { maxLength: 64, encoding: bytes, captureAs: session } } }\n            - acquireFirmware: { from: { userPrompt: { text: \"enter the pairing code\" } } }",
        4,
    ))
    .expect("if, retry, acquire, and acquireFirmware remain valid in USB plans");
}

#[test]
fn none_event_channel_rejects_usb_await_interrupt_in_the_establishment_plan() {
    let index = usb_index(
        "            - usbClaim: { interface: stillImage }\n            - usbAwaitInterrupt: { encoding: bytes, captureAs: eventFrame }",
        2,
    );
    let store_with_delivery = |delivery: &str| {
        let body = format!(
            r#"
schema: camera-config/v1
camera: {{ manufacturer: TESTCO, model: Test, firmware: "1" }}
connections:
  usbTether:
    kind: usb
    establishment: usb-claim-session
    events: {{ delivery: {delivery} }}
"#
        );
        ConfigStore::from_manufacturer_index(&index, BTreeMap::from([("tm1".to_string(), body)]))
    };

    let error = store_with_delivery("none")
        .expect_err("a none event channel forbids an interrupt await in the plan");
    let message = error.to_string();
    assert!(
        message.contains("usbTether"),
        "names the connection, got: {message}",
    );
    assert!(
        message.contains("usbAwaitInterrupt"),
        "names the plan step, got: {message}",
    );

    store_with_delivery("reliable")
        .expect("the raw kind owns the interrupt pipe, so reliable loads");
    store_with_delivery("bestEffort")
        .expect("the thenPoll rule scopes to the EntryStep awaitUntil grammar, not USB verbs");
}

#[test]
fn family_block_with_a_usb_block_loads() {
    let yaml = r#"
ble:
  gatt:
    status: "00002A25-0000-1000-8000-00805F9B34FB"
usb:
  interfaces:
    stillImage: { class: 6, subclass: 1, protocol: 1 }
    vendor: { class: 255, subclass: 255, protocol: 0 }
  establishments:
    usb-claim-session:
      mechanism: usb-claim-session
      steps:
        - usbClaim: { interface: stillImage }
"#;
    let block: FamilyBlock =
        serde_yaml::from_str(yaml).expect("FamilyBlock with a usb block loads");
    assert!(block.ble.is_some());
    let usb = block.usb.expect("usb block is present");
    assert_eq!(usb.interfaces.len(), 2);
    assert_eq!(
        usb.interfaces.get("vendor"),
        Some(&UsbInterfaceTriple {
            class: 255,
            subclass: 255,
            protocol: 0,
        }),
    );
    assert!(usb.establishment("usb-claim-session").is_some());
}
