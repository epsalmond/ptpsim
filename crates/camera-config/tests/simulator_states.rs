//! Simulator states (#164, #352): named physical conditions whose declared
//! responses the simulator answers with while the state is active. These tests
//! pin the manifest shape, the load-time contract, the lints, and the scope
//! query the engine uses. The engine behavior is pinned in camera-sim.

use camera_config::{CameraManifest, ManifestError};
use std::collections::BTreeSet;

const BASE: &str = r#"
schema: camera-config/v1
camera: { manufacturer: TESTCO, model: TEST }
evidence:
  docCondition: { kind: doc, path: "evidence/CONDITION.md" }
modes:
  browse:
    detect: { prop: "0xd001", eq: 1 }
  browse/list:
    detect: { prop: "0xd001", eq: 2 }
  shoot:
    detect: { prop: "0xd001", eq: 3 }
connections:
  app:
    kind: ptpip-app
    bindings: { command: 55740 }
  tether:
    kind: ptpip
    bindings: { command: 15740 }
operations:
  "0x9001": { name: VendorBegin, modes: [browse], connections: [app] }
  "0x9002": { name: VendorPage, modes: [browse], connections: [app] }
  "0x9006": { name: VendorAnywhere }
properties:
  "0xd001": { name: selector, type: u16, access: readWrite }
"#;

fn with_states(states: &str) -> String {
    format!("{BASE}states:\n{states}")
}

const ONE_STATE: &str = r#"
  slotEmpty:
    summary: The card slot holds no card. Nothing else is modeled.
    responses:
      - { operation: "0x9001", response: "0x2019", connections: [app], modes: [browse] }
    evidence: [docCondition]
"#;

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

fn active(names: &[&str]) -> BTreeSet<String> {
    names.iter().map(|name| name.to_string()).collect()
}

#[test]
fn declared_state_parses_with_its_fields() {
    let manifest = load(&with_states(ONE_STATE));
    let state = &manifest.states["slotEmpty"];
    assert_eq!(
        state.summary,
        "The card slot holds no card. Nothing else is modeled."
    );
    assert_eq!(state.evidence, ["docCondition"]);
    assert_eq!(state.responses.len(), 1);
    let response = &state.responses[0];
    assert_eq!(response.operation, "0x9001");
    assert_eq!(response.response, "0x2019");
    assert_eq!(response.connections, ["app"]);
    assert_eq!(response.modes, ["browse"]);
}

#[test]
fn active_state_answers_only_in_its_declared_scope() {
    let manifest = load(&with_states(ONE_STATE));
    let on = active(&["slotEmpty"]);
    let respond = |active: &BTreeSet<String>, connection, mode, code| {
        manifest.state_response(active, connection, mode, code)
    };

    assert_eq!(respond(&on, "app", Some("browse"), 0x9001), Some(0x2019));
    // A descendant of the declared mode is in scope.
    assert_eq!(
        respond(&on, "app", Some("browse/list"), 0x9001),
        Some(0x2019)
    );

    assert_eq!(respond(&active(&[]), "app", Some("browse"), 0x9001), None);
    assert_eq!(respond(&on, "tether", Some("browse"), 0x9001), None);
    assert_eq!(respond(&on, "app", Some("shoot"), 0x9001), None);
    // Before mode detection an override that names modes does not apply.
    assert_eq!(respond(&on, "app", None, 0x9001), None);
    assert_eq!(respond(&on, "app", Some("browse"), 0x9002), None);
}

#[test]
fn unscoped_response_applies_on_every_connection_and_before_detection() {
    let manifest = load(&with_states(
        r#"
  anywhere:
    summary: Answers everywhere.
    responses:
      - { operation: "0x9002", response: "0x2019" }
    evidence: [docCondition]
"#,
    ));
    let on = active(&["anywhere"]);
    for (connection, mode) in [
        ("app", Some("browse")),
        ("tether", Some("shoot")),
        ("app", None),
    ] {
        assert_eq!(
            manifest.state_response(&on, connection, mode, 0x9002),
            Some(0x2019),
            "{connection} {mode:?}"
        );
    }
}

fn two_entries(first: &str, second: &str) -> String {
    with_states(&format!(
        "  slotEmpty:\n    summary: s\n    responses:\n      - {first}\n      - {second}\n    evidence: [docCondition]\n"
    ))
}

fn two_states(first: &str, second: &str) -> String {
    with_states(&format!(
        "  aState:\n    summary: s\n    responses:\n      - {first}\n    evidence: [docCondition]\n  bState:\n    summary: s\n    responses:\n      - {second}\n    evidence: [docCondition]\n"
    ))
}

const OVERLAPPING: [(&str, &str); 4] = [
    (
        r#"{ operation: "0x9006", response: "0x2019" }"#,
        r#"{ operation: "0x9006", response: "0x2002", connections: [app] }"#,
    ),
    (
        r#"{ operation: "0x9006", response: "0x2019", connections: [app, tether] }"#,
        r#"{ operation: "0x9006", response: "0x2002", connections: [tether] }"#,
    ),
    (
        r#"{ operation: "0x9006", response: "0x2019", modes: [browse] }"#,
        r#"{ operation: "0x9006", response: "0x2002", modes: [browse/list] }"#,
    ),
    (
        r#"{ operation: "0x9006", response: "0x2019", connections: [app], modes: [browse] }"#,
        r#"{ operation: "0x9006", response: "0x2019", connections: [app], modes: [browse] }"#,
    ),
];

const DISJOINT: [(&str, &str); 3] = [
    (
        r#"{ operation: "0x9006", response: "0x2019", connections: [app] }"#,
        r#"{ operation: "0x9006", response: "0x2002", connections: [tether] }"#,
    ),
    (
        r#"{ operation: "0x9006", response: "0x2019", modes: [browse] }"#,
        r#"{ operation: "0x9006", response: "0x2002", modes: [shoot] }"#,
    ),
    (
        r#"{ operation: "0x9006", response: "0x2019" }"#,
        r#"{ operation: "0x9001", response: "0x2019" }"#,
    ),
];

#[test]
fn overlapping_entries_in_one_state_fail_to_load() {
    for (first, second) in OVERLAPPING {
        let message = contract_error(&two_entries(first, second));
        assert!(
            message.contains(
                "states.slotEmpty.responses[1] overlaps states.slotEmpty.responses[0] for operation '0x9006'"
            ),
            "{first} / {second}: {message}"
        );
    }
    for (first, second) in DISJOINT {
        load(&two_entries(first, second));
    }
}

#[test]
fn overlapping_entries_in_two_states_fail_to_load() {
    for (first, second) in OVERLAPPING {
        let message = contract_error(&two_states(first, second));
        assert!(
            message.contains(
                "states.bState.responses[0] overlaps states.aState.responses[0] for operation '0x9006'"
            ),
            "{first} / {second}: {message}"
        );
    }
    // Disjoint scopes in two states can both be active: each request has at
    // most one answer.
    for (first, second) in DISJOINT {
        let manifest = load(&two_states(first, second));
        let both = active(&["aState", "bState"]);
        assert_eq!(
            manifest.state_response(&both, "app", Some("browse"), 0x9006),
            Some(0x2019)
        );
    }
}

#[test]
fn manifest_without_states_serializes_without_the_key() {
    let manifest = load(BASE);
    assert!(manifest.states.is_empty());
    let yaml = manifest.to_yaml().unwrap();
    assert!(!yaml.contains("states:"), "{yaml}");

    let declared = load(&with_states(ONE_STATE));
    let round_trip = load(&declared.to_yaml().unwrap());
    assert_eq!(round_trip.states, declared.states);
}

#[test]
fn firmware_tier_inherits_base_states() {
    let manifest = CameraManifest::from_tiers(
        &with_states(ONE_STATE),
        &["camera: { manufacturer: TESTCO, model: TEST, firmware: \"2\" }\n"],
    )
    .expect("tiers merge");
    assert!(manifest.states.contains_key("slotEmpty"));
}

#[test]
fn unknown_state_fields_are_load_errors() {
    let unknown_state_field = with_states(
        r#"
  slotEmpty:
    summary: s
    responses: [{ operation: "0x9001", response: "0x2019" }]
    evidence: [docCondition]
    when: { prop: "0xd001", eq: 1 }
"#,
    );
    assert!(CameraManifest::from_yaml(&unknown_state_field).is_err());

    let unknown_response_field = with_states(
        r#"
  slotEmpty:
    summary: s
    responses: [{ operation: "0x9001", response: "0x2019", params: [1] }]
    evidence: [docCondition]
"#,
    );
    assert!(CameraManifest::from_yaml(&unknown_response_field).is_err());
}

#[test]
fn incomplete_or_invalid_states_fail_to_load() {
    let cases = [
        (
            r#"
  slotEmpty:
    summary: ""
    responses: [{ operation: "0x9001", response: "0x2019" }]
    evidence: [docCondition]
"#,
            "states.slotEmpty summary must not be empty",
        ),
        (
            r#"
  slotEmpty:
    summary: s
    responses: []
    evidence: [docCondition]
"#,
            "states.slotEmpty responses must not be empty",
        ),
        (
            r#"
  slotEmpty:
    summary: s
    responses: [{ operation: "0x9001", response: "0x2019" }]
"#,
            "states.slotEmpty evidence must not be empty",
        ),
        (
            r#"
  slotEmpty:
    summary: s
    responses: [{ operation: "9001x", response: "0x2019" }]
    evidence: [docCondition]
"#,
            "states.slotEmpty.responses[0] operation '9001x' is not a hex operation code",
        ),
        (
            r#"
  slotEmpty:
    summary: s
    responses: [{ operation: "0x9001", response: "busy" }]
    evidence: [docCondition]
"#,
            "states.slotEmpty.responses[0] response 'busy' is not a hex response code",
        ),
        (
            r#"
  slotEmpty:
    summary: s
    responses: [{ operation: "0x9001", response: "0x2001" }]
    evidence: [docCondition]
"#,
            "states.slotEmpty.responses[0] response must not be 0x2001 (OK)",
        ),
        (
            r#"
  slotEmpty:
    summary: s
    responses: [{ operation: "0x9003", response: "0x2019" }]
    evidence: [docCondition]
"#,
            "states.slotEmpty.responses[0] operation '0x9003' is not declared under operations",
        ),
    ];
    for (states, expected) in cases {
        let message = contract_error(&with_states(states));
        assert!(
            message.contains(expected),
            "expected {expected:?} in {message:?}"
        );
    }
}

#[test]
fn undefined_state_evidence_is_a_lint() {
    let manifest = load(&with_states(
        r#"
  slotEmpty:
    summary: s
    responses:
      - { operation: "0x9001", response: "0x2019", connections: [app], modes: [browse] }
    evidence: [docMissing]
"#,
    ));
    let messages: Vec<String> = manifest
        .validate()
        .into_iter()
        .map(|lint| lint.message)
        .collect();
    assert!(
        messages
            .iter()
            .any(|message| message.contains("state slotEmpty references evidence id 'docMissing'")),
        "{messages:#?}"
    );

    let clean = load(&with_states(ONE_STATE));
    assert!(
        clean
            .validate()
            .iter()
            .all(|lint| !lint.message.contains("state slotEmpty")),
        "{:#?}",
        clean.validate()
    );
}

#[test]
fn misspelled_or_unreachable_scopes_fail_to_load() {
    let cases = [
        (
            r#"{ operation: "0x9001", response: "0x2019", connections: [ap], modes: [browse] }"#,
            "states.slotEmpty.responses[0] connection 'ap' is not a declared connection",
        ),
        (
            r#"{ operation: "0x9001", response: "0x2019", connections: [app], modes: [brows] }"#,
            "states.slotEmpty.responses[0] mode 'brows' matches no declared mode",
        ),
        (
            r#"{ operation: "0x9001", response: "0x2019", connections: [tether] }"#,
            "states.slotEmpty.responses[0] operation '0x9001' is unavailable on every connection and mode in its scope",
        ),
        (
            r#"{ operation: "0x9001", response: "0x2019", modes: [shoot] }"#,
            "states.slotEmpty.responses[0] operation '0x9001' is unavailable on every connection and mode in its scope",
        ),
    ];
    for (response, expected) in cases {
        let states = format!(
            "  slotEmpty:\n    summary: s\n    responses:\n      - {response}\n    evidence: [docCondition]\n"
        );
        let message = contract_error(&with_states(&states));
        assert!(
            message.contains(expected),
            "expected {expected:?} in {message:?}"
        );
    }

    // An advertised-only operation can never be dispatched.
    let advertised = BASE.replace(
        r#"  "0x9002": { name: VendorPage, modes: [browse], connections: [app] }"#,
        r#"  "0x9002": { name: VendorPage, modes: [browse], connections: [app] }
  "0x9005": { name: VendorAdvertised, kind: advertisedOnly }"#,
    );
    let message = contract_error(&format!(
        "{advertised}states:\n  slotEmpty:\n    summary: s\n    responses:\n      - {{ operation: \"0x9005\", response: \"0x2019\" }}\n    evidence: [docCondition]\n"
    ));
    assert!(
        message.contains("operation '0x9005' is unavailable"),
        "{message}"
    );
}

#[test]
fn state_responses_must_be_ptp_response_codes() {
    let state = |response: &str| {
        with_states(&format!(
            "  slotEmpty:\n    summary: s\n    responses:\n      - {{ operation: \"0x9001\", response: \"{response}\", connections: [app], modes: [browse] }}\n    evidence: [docCondition]\n"
        ))
    };
    for rejected in [
        "0x0019", "0x1001", "0x9050", "0xc001", "0xd212", "0xffff", "0x3000",
    ] {
        let message = contract_error(&state(rejected));
        assert!(
            message.contains(&format!(
                "states.slotEmpty.responses[0] response '{rejected}' is not a PTP response code"
            )),
            "{rejected}: {message}"
        );
    }
    for accepted in ["0x2000", "0x2019", "0x2fff", "0xa000", "0xA001", "0xafff"] {
        load(&state(accepted));
    }
}

#[test]
fn reachability_follows_the_dispatch_availability_rule() {
    // The catalog limits GetDevicePropValue to `shoot` and OpenSession to
    // `tether`, but the dispatcher ignores the mode axis for property access
    // and never gates session operations, so both states are reachable.
    let manifest = BASE.replace(
        r#"  "0x9006": { name: VendorAnywhere }"#,
        r#"  "0x9006": { name: VendorAnywhere }
  "0x1015": { name: GetDevicePropValue, modes: [shoot], connections: [app] }
  "0x1002": { name: OpenSession, connections: [tether] }"#,
    );
    let loaded = load(&format!(
        "{manifest}states:
  readRefused:
    summary: s
    responses:
      - {{ operation: \"0x1015\", response: \"0x2019\", connections: [app], modes: [browse] }}
    evidence: [docCondition]
  cameraOff:
    summary: s
    responses:
      - {{ operation: \"0x1002\", response: \"0x2019\", connections: [app] }}
    evidence: [docCondition]
"
    ));
    let none = camera_config::PropView::new();
    assert_eq!(
        loaded.dispatch_availability("app", Some("browse"), 0x1015, &none),
        camera_config::Availability::Available
    );
    assert_eq!(
        loaded.dispatch_availability("app", None, 0x1002, &none),
        camera_config::Availability::Available
    );
    assert_eq!(
        loaded.dispatch_availability("app", Some("shoot"), 0x9001, &none),
        camera_config::Availability::WrongMode
    );
    // The property-access exemption covers only the mode axis.
    let message = contract_error(&format!(
        "{manifest}states:
  readRefused:
    summary: s
    responses:
      - {{ operation: \"0x1015\", response: \"0x2019\", connections: [tether] }}
    evidence: [docCondition]
"
    ));
    assert!(
        message.contains("operation '0x1015' is unavailable"),
        "{message}"
    );
}
