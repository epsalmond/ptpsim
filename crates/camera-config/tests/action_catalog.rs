use camera_config::{
    ActionArgument, ActionArgumentValue, ActionCatalogParameterKind, ActionInvocationRequest,
    ActionResolutionError, ActionRole, CameraManifest, ResponderMutation,
};

fn manifest() -> CameraManifest {
    CameraManifest::from_yaml(include_str!(
        "../../../packages/camera-config-data/fuji/gfx100ii/gfx100ii.consolidated.yaml"
    ))
    .expect("consolidated manifest")
}

fn request(manifest: &CameraManifest) -> ActionInvocationRequest {
    ActionInvocationRequest {
        catalog_revision: manifest.action_catalog().revision,
        action_id: "shutter".into(),
        connection: "wireless-tether".into(),
        mode: "shooting/stills".into(),
        role: ActionRole::Responder,
        parameters: Vec::new(),
    }
}

#[test]
fn catalog_is_deterministic_and_responder_defaults_to_one_object() {
    let manifest = manifest();
    let first = manifest.action_catalog();
    let second = manifest.action_catalog();
    assert_eq!(first, second);
    assert_eq!(first.revision.len(), 64);

    let resolved = manifest
        .resolve_action_invocation(&request(&manifest))
        .unwrap();
    assert_eq!(
        resolved.parameters["objectCount"],
        camera_config::ActionArgumentValue::U64(1)
    );
    assert_eq!(
        resolved.responder_mutation,
        Some(ResponderMutation::EnqueueObjects {
            count_param: "objectCount".into(),
        })
    );
}

#[test]
fn bundled_pcss_autofocus_accepts_optional_string_and_rejects_numeric_focus_area() {
    let manifest = manifest();
    let catalog = manifest.action_catalog();
    let entry = catalog
        .actions
        .iter()
        .find(|entry| entry.connection == "wireless-tether" && entry.action_id == "autofocusLock")
        .expect("bundled PCSS autofocusLock catalog entry");
    let initiator = entry
        .parameters
        .iter()
        .find(|parameters| parameters.role == ActionRole::Initiator)
        .expect("initiator parameters");
    assert_eq!(initiator.parameters.len(), 1);
    assert_eq!(initiator.parameters[0].name, "focusArea");
    assert_eq!(
        initiator.parameters[0].kind,
        ActionCatalogParameterKind::String
    );
    assert!(!initiator.parameters[0].required);

    let base_request = || ActionInvocationRequest {
        catalog_revision: catalog.revision.clone(),
        action_id: "autofocusLock".into(),
        connection: "wireless-tether".into(),
        mode: "shooting/stills".into(),
        role: ActionRole::Initiator,
        parameters: Vec::new(),
    };
    let omitted = manifest
        .resolve_action_invocation(&base_request())
        .expect("focusArea may be omitted");
    assert!(omitted.parameters.is_empty());

    let mut valid = base_request();
    valid.parameters.push(ActionArgument {
        name: "focusArea".into(),
        value: ActionArgumentValue::String("-12,34,5".into()),
    });
    let valid = manifest
        .resolve_action_invocation(&valid)
        .expect("valid string argument resolves");
    assert_eq!(
        valid.parameters["focusArea"],
        ActionArgumentValue::String("-12,34,5".into())
    );

    let mut numeric = base_request();
    numeric.parameters.push(ActionArgument {
        name: "focusArea".into(),
        value: 7.into(),
    });
    assert!(matches!(
        manifest.resolve_action_invocation(&numeric),
        Err(ActionResolutionError::WrongParameterType { .. })
    ));
}

#[test]
fn every_preflight_failure_has_a_stable_code() {
    let manifest = manifest();
    let cases = [
        (
            {
                let mut request = request(&manifest);
                request.catalog_revision = "stale".into();
                request
            },
            "staleCatalogRevision",
        ),
        (
            {
                let mut request = request(&manifest);
                request.connection = "missing".into();
                request
            },
            "unknownConnection",
        ),
        (
            {
                let mut request = request(&manifest);
                request.action_id = "getObject".into();
                request.connection = "app".into();
                request
            },
            "wrongMode",
        ),
        (
            {
                let mut request = request(&manifest);
                request.mode = "image-transfer".into();
                request
            },
            "wrongMode",
        ),
        (
            {
                let mut request = request(&manifest);
                request.action_id = "keepalive".into();
                request.mode.clear();
                request.role = ActionRole::Responder;
                request
            },
            "wrongRole",
        ),
        (
            {
                let mut request = request(&manifest);
                request.action_id = "getObject".into();
                request.mode.clear();
                request.role = ActionRole::Initiator;
                request
            },
            "missingParameter",
        ),
        (
            {
                let mut request = request(&manifest);
                request.parameters = vec![
                    ActionArgument {
                        name: "objectCount".into(),
                        value: 1.into(),
                    },
                    ActionArgument {
                        name: "objectCount".into(),
                        value: 2.into(),
                    },
                ];
                request
            },
            "duplicateParameter",
        ),
        (
            {
                let mut request = request(&manifest);
                request.parameters = vec![ActionArgument {
                    name: "extra".into(),
                    value: 1.into(),
                }];
                request
            },
            "extraParameter",
        ),
        (
            {
                let mut request = request(&manifest);
                request.parameters = vec![ActionArgument {
                    name: "objectCount".into(),
                    value: 4.into(),
                }];
                request
            },
            "invalidParameter",
        ),
    ];

    for (request, expected) in cases {
        let error = manifest.resolve_action_invocation(&request).unwrap_err();
        assert_eq!(error.code(), expected, "{error:?}");
    }
}

#[test]
fn unknown_action_is_distinct_from_unknown_connection() {
    let manifest = manifest();
    let mut request = request(&manifest);
    request.action_id = "notAnAction".into();
    assert!(matches!(
        manifest.resolve_action_invocation(&request),
        Err(ActionResolutionError::UnknownAction { .. })
    ));
}

fn action_manifest(step: &str, triggers: &str) -> String {
    format!(
        r#"schema: camera-config/v1
camera: {{ manufacturer: Test, model: Test, firmware: "1" }}
connections:
  app:
    establishment: test
    actions:
      shutter:
        mode: shooting/stills
        initiator:
          steps:
{step}
        triggers:
{triggers}
"#
    )
}

fn transition_manifest(property: &str, params: &str, mutation: &str) -> String {
    format!(
        r#"schema: camera-config/v1
camera: {{ manufacturer: Test, model: Test, firmware: "1" }}
properties:
  "0xd001": {property}
connections:
  app:
    actions:
      autofocusLock:
        mode: shooting/stills
        responder:
          params:
{params}
          mutation:
{mutation}
        triggers: []
"#
    )
}

#[test]
fn property_transition_schema_accepts_closed_terminal_sources() {
    let fixed = transition_manifest(
        "{ name: result, type: u16, access: readOnly }",
        "            []",
        r#"            kind: propertyTransition
            target: "0xd001"
            initial: 1
            terminal: { kind: fixed, value: 2 }
            settleAfterPolls: 2"#,
    );
    CameraManifest::from_yaml(&fixed).expect("fixed transition");

    let parameter = transition_manifest(
        "{ name: result, type: u8, access: readOnly }",
        r#"            - { name: result, kind: u32, min: 2, max: 3 }"#,
        r#"            kind: propertyTransition
            target: "0xd001"
            terminal: { kind: parameter, parameter: result }"#,
    );
    CameraManifest::from_yaml(&parameter).expect("parameter transition");
}

#[test]
fn property_transition_schema_rejects_invalid_targets_and_values() {
    let cases = [
        (
            "{ name: result, type: u16, access: readOnly }",
            "            []",
            r#"            kind: propertyTransition
            target: "0xd002"
            terminal: { kind: fixed, value: 2 }"#,
            "unknown target",
        ),
        (
            "{ name: result, type: str, access: readOnly }",
            "            []",
            r#"            kind: propertyTransition
            target: "0xd001"
            terminal: { kind: fixed, value: 2 }"#,
            "numeric scalar property",
        ),
        (
            "{ name: result, type: u8, access: readOnly }",
            "            []",
            r#"            kind: propertyTransition
            target: "0xd001"
            initial: 256
            terminal: { kind: fixed, value: 2 }"#,
            "initial value",
        ),
        (
            "{ name: result, type: u8, access: readOnly }",
            "            []",
            r#"            kind: propertyTransition
            target: "0xd001"
            terminal: { kind: fixed, value: 256 }"#,
            "terminal value",
        ),
        (
            "{ name: result, type: u16, access: readOnly }",
            "            []",
            r#"            kind: propertyTransition
            target: "0xd001"
            terminal: { kind: parameter, parameter: missing }"#,
            "unknown terminal parameter",
        ),
        (
            "{ name: result, type: u8, access: readOnly }",
            r#"            - { name: result, kind: u32, min: 0, max: 256 }"#,
            r#"            kind: propertyTransition
            target: "0xd001"
            terminal: { kind: parameter, parameter: result }"#,
            "parameter 'result' range",
        ),
    ];

    for (property, params, mutation, expected) in cases {
        let error = CameraManifest::from_yaml(&transition_manifest(property, params, mutation))
            .expect_err("invalid property transition must fail at load");
        assert!(error.to_string().contains(expected), "{error}");
    }
}

#[test]
fn malformed_action_steps_fail_closed_in_nested_else_branches() {
    let body = action_manifest(
        r#"            - if:
                slot: selection
                equals: 1
                then:
                  - { sendOp: "0x100e" }
                else:
                  - { sendOp: "0x100e", getProp: "0xd001" }"#,
        "          []",
    );
    let error = CameraManifest::from_yaml(&body).expect_err("ambiguous else step must fail");
    assert!(
        error
            .to_string()
            .contains("if.else.steps[0] must contain exactly one action"),
        "{error}"
    );
}

#[test]
fn malformed_and_open_ended_action_triggers_fail_closed() {
    for triggers in [
        r#"          - { postviewEvent: {}, liveViewStream: {} }"#,
        r#"          - { postviewEvent: { unexpected: true } }"#,
    ] {
        let body = action_manifest(r#"            - { sendOp: "0x100e" }"#, triggers);
        assert!(
            CameraManifest::from_yaml(&body).is_err(),
            "trigger must be rejected: {triggers}"
        );
    }
}

fn typed_action_manifest(step_value: &str, captures: &str) -> String {
    format!(
        r#"schema: camera-config/v1
camera: {{ manufacturer: Test, model: Test, firmware: "1" }}
properties:
  "0xd001": {{ name: numeric, type: u16, access: readWrite }}
  "0xd002":
    name: structured
    type: str
    access: readWrite
    structuredText:
      delimiter: ","
      fields:
        - {{ name: x, scalar: signedInteger }}
        - {{ name: y, scalar: signedInteger }}
        - {{ name: size, scalar: signedInteger }}
connections:
  app:
    commandFraming: standard
    actions:
      shutter:
        mode: shooting/stills
        initiator:
          params:
            - count
            - {{ name: focusArea, kind: string, required: false }}
          steps:
            - {{ setProp: "0xd001", value: {{ runtime: count }} }}
            - setProp: "0xd002"
              value: {step_value}
            {captures}
"#
    )
}

#[test]
fn legacy_and_expanded_initiator_parameters_normalize_in_catalog() {
    let expanded: camera_config::ActionInitiatorParameter =
        serde_yaml::from_str("name: expanded\nkind: u64\n").unwrap();
    assert!(expanded.normalized().required);

    let manifest = CameraManifest::from_yaml(&typed_action_manifest(
        "{ runtime: focusArea, ifMissing: skip }",
        "",
    ))
    .expect("typed action manifest");
    let catalog = manifest.action_catalog();
    let parameters = &catalog.actions[0].parameters[0].parameters;
    assert_eq!(parameters[0].name, "count");
    assert_eq!(parameters[0].kind, ActionCatalogParameterKind::U64);
    assert!(parameters[0].required);
    assert_eq!(parameters[1].name, "focusArea");
    assert_eq!(parameters[1].kind, ActionCatalogParameterKind::String);
    assert!(!parameters[1].required);

    let resolved = manifest
        .resolve_action_invocation(&ActionInvocationRequest {
            catalog_revision: catalog.revision.clone(),
            action_id: "shutter".into(),
            connection: "app".into(),
            mode: "shooting/stills".into(),
            role: ActionRole::Initiator,
            parameters: vec![ActionArgument {
                name: "count".into(),
                value: 7.into(),
            }],
        })
        .expect("optional string may be omitted");
    assert_eq!(resolved.parameters.len(), 1);
    assert_eq!(resolved.parameters["count"], ActionArgumentValue::U64(7));

    let error = manifest
        .resolve_action_invocation(&ActionInvocationRequest {
            catalog_revision: catalog.revision,
            action_id: "shutter".into(),
            connection: "app".into(),
            mode: "shooting/stills".into(),
            role: ActionRole::Initiator,
            parameters: vec![
                ActionArgument {
                    name: "count".into(),
                    value: 7.into(),
                },
                ActionArgument {
                    name: "focusArea".into(),
                    value: 9.into(),
                },
            ],
        })
        .expect_err("wrong argument kind");
    assert_eq!(error.code(), "wrongParameterType");
}

#[test]
fn action_argument_serde_is_untagged_number_or_string() {
    let numeric: ActionArgument = serde_json::from_str(r#"{"name":"count","value":7}"#).unwrap();
    assert_eq!(numeric.value, ActionArgumentValue::U64(7));
    let string: ActionArgument = serde_yaml::from_str("name: focusArea\nvalue: 1,2,-3\n").unwrap();
    assert_eq!(string.value, ActionArgumentValue::String("1,2,-3".into()));
    assert!(
        serde_json::from_str::<ActionArgument>(r#"{"name":"count","value":{"u64":7}}"#).is_err()
    );
}

#[test]
fn invalid_runtime_skip_and_await_capture_shapes_fail_schema_validation() {
    let required_skip = typed_action_manifest("{ runtime: count, ifMissing: skip }", "");
    assert!(CameraManifest::from_yaml(&required_skip)
        .unwrap_err()
        .to_string()
        .contains("ifMissing skip requires optional"));

    let event_capture = typed_action_manifest(
        "{ runtime: focusArea, ifMissing: skip }",
        r#"- awaitUntil:
                source: { event: { code: "0xc001" } }
                until: { all: [] }
                timeoutMs: 10
              captures: [{ bind: result, as: propValue }]"#,
    );
    assert!(CameraManifest::from_yaml(&event_capture)
        .unwrap_err()
        .to_string()
        .contains("require awaitUntil poll or event thenPoll"));

    let wrong_capture = typed_action_manifest(
        "{ runtime: focusArea, ifMissing: skip }",
        r#"- awaitUntil:
                source: { poll: "0xd001" }
                until: { prop: "0xd001", eq: 1 }
                timeoutMs: 10
              captures: [{ bind: result, as: u32Le }]"#,
    );
    assert!(CameraManifest::from_yaml(&wrong_capture)
        .unwrap_err()
        .to_string()
        .contains("support only propValue"));
}

#[test]
fn prop_value_response_fallback_is_closed_and_get_prop_only() {
    let valid = typed_action_manifest(
        "{ runtime: focusArea, ifMissing: skip }",
        r#"- getProp: "0xd001"
              captures:
                - bind: result
                  as: propValue
                  fallback:
                    value: 0x00200000
                    whenResponseCodes: ["0x200a"]"#,
    );
    CameraManifest::from_yaml(&valid).expect("response-selected propValue fallback loads");

    let wrong_verb = typed_action_manifest(
        "{ runtime: focusArea, ifMissing: skip }",
        r#"- sendOp: "0x1008"
              captures:
                - bind: result
                  as: u32Le
                  fallback:
                    value: 1
                    whenResponseCodes: ["0x200a"]"#,
    );
    assert!(CameraManifest::from_yaml(&wrong_verb)
        .unwrap_err()
        .to_string()
        .contains("fallback requires getProp propValue"));

    let empty_selection = typed_action_manifest(
        "{ runtime: focusArea, ifMissing: skip }",
        r#"- getProp: "0xd001"
              captures:
                - bind: result
                  as: propValue
                  fallback: { value: 1, whenResponseCodes: [] }"#,
    );
    assert!(CameraManifest::from_yaml(&empty_selection)
        .unwrap_err()
        .to_string()
        .contains("whenResponseCodes must not be empty"));

    for (fallback, expected) in [
        (
            r#"{ value: 1, whenResponseCodes: [not-a-code] }"#,
            "invalid response code",
        ),
        (
            r#"{ value: 1, whenResponseCodes: ["0x2001"] }"#,
            "cannot select the OK response",
        ),
        (
            r#"{ value: 1, whenResponseCodes: ["0x200a", "0x200a"] }"#,
            "repeats response code",
        ),
    ] {
        let invalid = typed_action_manifest(
            "{ runtime: focusArea, ifMissing: skip }",
            &format!(
                r#"- getProp: "0xd001"
              captures:
                - bind: result
                  as: propValue
                  fallback: {fallback}"#
            ),
        );
        assert!(
            CameraManifest::from_yaml(&invalid)
                .unwrap_err()
                .to_string()
                .contains(expected),
            "fallback {fallback} must fail with {expected:?}"
        );
    }

    let tolerant = typed_action_manifest(
        "{ runtime: focusArea, ifMissing: skip }",
        r#"- getProp: "0xd001"
              tolerant: true
              captures:
                - bind: result
                  as: propValue
                  fallback: { value: 1, whenResponseCodes: ["0x200a"] }"#,
    );
    assert!(CameraManifest::from_yaml(&tolerant)
        .unwrap_err()
        .to_string()
        .contains("fallback must not be tolerant"));
}
