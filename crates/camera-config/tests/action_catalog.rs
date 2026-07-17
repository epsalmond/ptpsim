use camera_config::{
    ActionArgument, ActionInvocationRequest, ActionResolutionError, ActionRole, CameraManifest,
    ResponderMutation,
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
    assert_eq!(resolved.parameters["objectCount"], 1);
    assert_eq!(
        resolved.responder_mutation,
        Some(ResponderMutation::EnqueueObjects {
            count_param: "objectCount".into(),
        })
    );
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
                request.mode = "image-transfer".into();
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
                        value: 1,
                    },
                    ActionArgument {
                        name: "objectCount".into(),
                        value: 2,
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
                    value: 1,
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
                    value: 4,
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
