//! Action failure conditions (#352): rules on an action initiator that
//! classify the PTP response failure ending the action. These tests pin the
//! manifest shape, the load-time contract, the lint, and the lookup the
//! executor uses. The executor behavior is pinned in camera-protocol-ffi.

use camera_config::{CameraManifest, FailureCondition, ManifestError};

const BASE: &str = r#"
schema: camera-config/v1
camera: { manufacturer: TESTCO, model: TEST }
evidence:
  docCondition: { kind: doc, path: "evidence/CONDITION.md" }
operations:
  "0x9001": { name: VendorBegin }
  "0x9002": { name: VendorPage }
properties:
  "0xd001": { name: selector, type: u16, access: readWrite }
connections:
  app:
    kind: ptpip-app
    commandFraming: compressed
    bindings: { command: 55740 }
    actions:
      enumerateObjects:
        mode: ""
        initiator:
          steps:
            - retry:
                whenResponseCodes: ["0x2019"]
                maxAttempts: 3
                retryDelayMs: 10
                steps:
                  - { sendOp: "0x9001" }
                  - { getProp: "0xd001" }
                fallback:
                  - { sendOp: "0x9002" }
"#;

const SLOT_RULE: &str = r#"
            - operation: "0x9001"
              whenResponseCodes: ["0x2019"]
              condition: mediaSlotUnavailable
              context: { slot: "1" }
              evidence: [docCondition]
"#;

fn with_rules(rules: &str) -> String {
    format!("{BASE}          failureConditions:{rules}")
}

fn load(yaml: &str) -> CameraManifest {
    CameraManifest::from_yaml(yaml).unwrap_or_else(|error| panic!("manifest loads: {error}"))
}

fn contract_error(yaml: &str) -> String {
    match CameraManifest::from_yaml(yaml) {
        Err(ManifestError::Contract(message)) => message,
        Err(other) => panic!("expected a contract error, got {other}"),
        Ok(_) => panic!("expected a contract error, the manifest loaded"),
    }
}

fn initiator(manifest: &CameraManifest) -> &camera_config::ActionInitiator {
    manifest.connections["app"]
        .actions
        .values()
        .next()
        .and_then(|action| action.initiator.as_ref())
        .expect("enumerateObjects initiator")
}

#[test]
fn declared_rule_parses_with_its_fields() {
    let manifest = load(&with_rules(SLOT_RULE));
    let rules = &initiator(&manifest).failure_conditions;
    assert_eq!(rules.len(), 1);
    let rule = &rules[0];
    assert_eq!(rule.operation, "0x9001");
    assert_eq!(rule.when_response_codes, ["0x2019"]);
    assert_eq!(rule.condition, FailureCondition::MediaSlotUnavailable);
    assert_eq!(rule.condition.name(), "mediaSlotUnavailable");
    assert_eq!(rule.context.len(), 1);
    assert_eq!(rule.context["slot"], "1");
    assert_eq!(rule.evidence, ["docCondition"]);
}

#[test]
fn lookup_matches_only_the_declared_operation_and_responses() {
    let manifest = load(&with_rules(SLOT_RULE));
    let initiator = initiator(&manifest);
    assert!(initiator.failure_condition(0x9001, 0x2019).is_some());
    assert!(initiator.failure_condition(0x9001, 0x2005).is_none());
    assert!(initiator.failure_condition(0x9002, 0x2019).is_none());
    assert!(initiator.failure_condition(0x1015, 0x2019).is_none());
}

#[test]
fn initiator_without_rules_has_none_and_serializes_without_the_key() {
    let manifest = load(BASE);
    assert!(initiator(&manifest).failure_conditions.is_empty());
    let yaml = serde_yaml::to_string(&manifest).expect("serialize");
    assert!(!yaml.contains("failureConditions"), "{yaml}");
}

#[test]
fn declared_rules_round_trip_through_serialization() {
    let manifest = load(&with_rules(SLOT_RULE));
    let yaml = serde_yaml::to_string(&manifest).expect("serialize");
    let reloaded = load(&yaml);
    assert_eq!(
        initiator(&reloaded).failure_conditions,
        initiator(&manifest).failure_conditions
    );
}

#[test]
fn operation_sent_inside_nested_control_flow_is_accepted() {
    // 0x9002 is sent only by the retry's fallback branch.
    let manifest = load(&with_rules(
        r#"
            - operation: "0x9002"
              whenResponseCodes: ["0x2019", "0xa001"]
              condition: mediaSlotUnavailable
              context: { slot: "2" }
              evidence: [docCondition]
"#,
    ));
    let initiator = initiator(&manifest);
    assert!(initiator.failure_condition(0x9002, 0xa001).is_some());

    for nested in [
        "            - if: { slot: page, equals: 1, then: [ { sendOp: \"0x9006\" } ] }\n",
        "            - awaitUntil:\n                source: { poll: \"0xd001\" }\n                until: { prop: \"0xd001\", eq: 1 }\n                timeoutMs: 100\n                onEach: [ { sendOp: \"0x9006\" } ]\n",
    ] {
        let yaml = BASE
            .replace(
                "  \"0x9002\": { name: VendorPage }\n",
                "  \"0x9002\": { name: VendorPage }\n  \"0x9006\": { name: VendorNested }\n",
            )
            .replace(
                "                  - { sendOp: \"0x9002\" }\n",
                &format!("                  - {{ sendOp: \"0x9002\" }}\n{nested}"),
            );
        let yaml = format!(
            "{yaml}          failureConditions:\n            - {{ operation: \"0x9006\", whenResponseCodes: [\"0x2019\"], condition: mediaSlotUnavailable, context: {{ slot: \"1\" }}, evidence: [docCondition] }}\n"
        );
        let manifest = CameraManifest::from_yaml(&yaml)
            .unwrap_or_else(|error| panic!("nested sendOp accepted: {error}\n{yaml}"));
        assert!(initiator_has_rule(&manifest, 0x9006));
    }
}

fn initiator_has_rule(manifest: &CameraManifest, operation: u16) -> bool {
    initiator(manifest)
        .failure_condition(operation, 0x2019)
        .is_some()
}

fn rule_with(field_overrides: &str) -> String {
    with_rules(&format!("\n            - {{ {field_overrides} }}\n"))
}

#[test]
fn contract_rejects_an_operation_that_is_not_hex_or_not_sent() {
    let error = contract_error(&rule_with(
        r#"operation: "begin", whenResponseCodes: ["0x2019"], condition: mediaSlotUnavailable, context: { slot: "1" }, evidence: [docCondition]"#,
    ));
    assert!(error.contains("is not a hex operation code"), "{error}");

    let error = contract_error(&rule_with(
        r#"operation: "0x9003", whenResponseCodes: ["0x2019"], condition: mediaSlotUnavailable, context: { slot: "1" }, evidence: [docCondition]"#,
    ));
    assert!(error.contains("failureConditions[0]"), "{error}");
    assert!(error.contains("is not sent by any sendOp step"), "{error}");

    // A getProp step's GetDevicePropValue is not a sendOp step.
    let error = contract_error(&rule_with(
        r#"operation: "0x1015", whenResponseCodes: ["0x2019"], condition: mediaSlotUnavailable, context: { slot: "1" }, evidence: [docCondition]"#,
    ));
    assert!(error.contains("is not sent by any sendOp step"), "{error}");
}

#[test]
fn contract_rejects_bad_response_lists() {
    for (codes, expected) in [
        ("[]", "whenResponseCodes must not be empty"),
        (r#"["busy"]"#, "is not a hex response code"),
        (r#"["0x9050"]"#, "is not a PTP response code"),
        (r#"["0x2001"]"#, "must not contain 0x2001"),
        (r#"["0x2019", "0x2019"]"#, "repeats"),
    ] {
        let error = contract_error(&rule_with(&format!(
            r#"operation: "0x9001", whenResponseCodes: {codes}, condition: mediaSlotUnavailable, context: {{ slot: "1" }}, evidence: [docCondition]"#
        )));
        assert!(error.contains(expected), "{codes}: {error}");
    }
}

#[test]
fn contract_requires_exactly_the_conditions_context() {
    let error = contract_error(&rule_with(
        r#"operation: "0x9001", whenResponseCodes: ["0x2019"], condition: mediaSlotUnavailable, evidence: [docCondition]"#,
    ));
    assert!(error.contains("requires context key 'slot'"), "{error}");

    // Extra keys are rejected, so declared context can never replace the
    // executor's own entries.
    for extra in [
        "operation",
        "response",
        "transactionId",
        "condition",
        "note",
    ] {
        let error = contract_error(&rule_with(&format!(
            r#"operation: "0x9001", whenResponseCodes: ["0x2019"], condition: mediaSlotUnavailable, context: {{ slot: "1", {extra}: "x" }}, evidence: [docCondition]"#
        )));
        assert!(
            error.contains(&format!("context key '{extra}' is not used")),
            "{extra}: {error}"
        );
    }
}

#[test]
fn contract_rejects_a_malformed_slot_number() {
    for slot in ["", "0", "01", "-1", "+1", " 1", "1 ", "one", "1.0"] {
        let error = contract_error(&rule_with(&format!(
            r#"operation: "0x9001", whenResponseCodes: ["0x2019"], condition: mediaSlotUnavailable, context: {{ slot: "{slot}" }}, evidence: [docCondition]"#
        )));
        assert!(error.contains("is malformed"), "{slot:?}: {error}");
    }
    for slot in ["1", "2", "10"] {
        load(&rule_with(&format!(
            r#"operation: "0x9001", whenResponseCodes: ["0x2019"], condition: mediaSlotUnavailable, context: {{ slot: "{slot}" }}, evidence: [docCondition]"#
        )));
    }
}

#[test]
fn contract_requires_evidence() {
    let error = contract_error(&rule_with(
        r#"operation: "0x9001", whenResponseCodes: ["0x2019"], condition: mediaSlotUnavailable, context: { slot: "1" }"#,
    ));
    assert!(error.contains("evidence must not be empty"), "{error}");
}

#[test]
fn contract_rejects_two_rules_that_match_the_same_failure() {
    let error = contract_error(&with_rules(
        r#"
            - { operation: "0x9001", whenResponseCodes: ["0x2019", "0x2005"], condition: mediaSlotUnavailable, context: { slot: "1" }, evidence: [docCondition] }
            - { operation: "0x9001", whenResponseCodes: ["0x2005"], condition: mediaSlotUnavailable, context: { slot: "2" }, evidence: [docCondition] }
"#,
    ));
    assert!(
        error.contains("failureConditions[1] overlaps failureConditions[0]"),
        "{error}"
    );

    // Different responses, or different operations, do not overlap.
    load(&with_rules(
        r#"
            - { operation: "0x9001", whenResponseCodes: ["0x2019"], condition: mediaSlotUnavailable, context: { slot: "1" }, evidence: [docCondition] }
            - { operation: "0x9001", whenResponseCodes: ["0x2005"], condition: mediaSlotUnavailable, context: { slot: "2" }, evidence: [docCondition] }
            - { operation: "0x9002", whenResponseCodes: ["0x2019"], condition: mediaSlotUnavailable, context: { slot: "2" }, evidence: [docCondition] }
"#,
    ));
}

#[test]
fn unknown_condition_or_field_is_a_load_error() {
    let unknown_condition = rule_with(
        r#"operation: "0x9001", whenResponseCodes: ["0x2019"], condition: cardMissing, context: { slot: "1" }, evidence: [docCondition]"#,
    );
    assert!(CameraManifest::from_yaml(&unknown_condition).is_err());

    let unknown_field = rule_with(
        r#"operation: "0x9001", whenResponseCodes: ["0x2019"], condition: mediaSlotUnavailable, context: { slot: "1" }, evidence: [docCondition], summary: "x""#,
    );
    assert!(CameraManifest::from_yaml(&unknown_field).is_err());
}

#[test]
fn undefined_evidence_id_is_a_lint_naming_the_action() {
    let manifest = load(&rule_with(
        r#"operation: "0x9001", whenResponseCodes: ["0x2019"], condition: mediaSlotUnavailable, context: { slot: "1" }, evidence: [docMissing]"#,
    ));
    let lints = manifest.validate();
    assert!(
        lints.iter().any(|lint| {
            lint.message.contains("failureConditions[0]") && lint.message.contains("docMissing")
        }),
        "{lints:?}"
    );
}
