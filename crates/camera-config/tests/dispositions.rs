use camera_config::{
    CameraManifest, OperationDisposition, PropView, PropertyDisposition, Severity,
};

fn manifest_with_scope(scope: &str) -> CameraManifest {
    // These records are synthetic validation fixtures.
    CameraManifest::from_yaml(&format!(
        r#"
schema: camera-config/v1
camera: {{ manufacturer: Test, model: Dispositions, firmware: "1" }}
evidence:
  fixture: {{ kind: doc, path: "synthetic-fixture" }}
connections:
  usb: {{}}
modes:
  shooting/stills/manual: {{}}
properties:
  "0x500A": {{ name: focusMode, type: u16 }}
  "0x5010":
    name: exposureBias
    type: i16
    dispositions:
      - disposition: valueless
      - disposition: valueless
        {scope}
operations:
  "0x9026":
    name: afLock
    dispositions:
      - disposition: silentRefusal
      - disposition: silentRefusal
        {scope}
"#
    ))
    .expect("disposition lints do not prevent loading")
}

fn messages_for_both_owners(manifest: &CameraManifest, expected: &[&str]) {
    let lints = manifest.validate();
    assert_eq!(lints.len(), expected.len() * 2, "{lints:?}");
    assert!(lints.iter().all(|lint| lint.severity == Severity::Warning));
    for owner in ["property 0x5010", "operation 0x9026"] {
        for message in expected {
            let expected = format!("{owner} disposition 1 {message}");
            assert!(
                lints.iter().any(|lint| lint.message == expected),
                "missing {expected:?} in {lints:?}"
            );
        }
    }
}

#[test]
fn disposition_scope_warnings_identify_owner_and_index() {
    let manifest = manifest_with_scope(
        "connections: [usb, usb-ptp]\n        modes: [shooting/stills, shooting/still, shooting/stills/manual/child]",
    );
    messages_for_both_owners(
        &manifest,
        &[
            "references unknown connection 'usb-ptp'",
            "mode scope 'shooting/still' matches no declared mode",
            "mode scope 'shooting/stills/manual/child' matches no declared mode",
        ],
    );
}

#[test]
fn disposition_predicate_warnings_visit_every_nested_leaf() {
    let manifest = manifest_with_scope(
        r#"when:
          all:
            - { prop: "0xzz", eq: 1 }
            - any:
                - { prop: "0x500a", eq: 1 }
                - { prop: "0xffff", eq: 1 }
                - not: { prop: "0xfffe", eq: 1 }"#,
    );
    messages_for_both_owners(
        &manifest,
        &[
            "when property '0xzz' is not a hex property code",
            "when references unknown property '0xffff'",
            "when references unknown property '0xfffe'",
        ],
    );
}

#[test]
fn disposition_evidence_uses_the_manifest_evidence_lint() {
    let manifest = manifest_with_scope("evidence: [fixture, missing]");
    messages_for_both_owners(
        &manifest,
        &["references evidence id 'missing' which is not defined in this manifest (provenance may live in a private repo)"],
    );
}

#[test]
fn valid_disposition_scopes_match_runtime_semantics() {
    for mode in ["shooting", "shooting/stills", "shooting/stills/manual"] {
        for property in ["0x500a", "0X500A", "500a"] {
            let mut manifest = manifest_with_scope(&format!(
                "connections: [usb]\n        modes: [{mode}]\n        evidence: [fixture]\n        when: {{ not: {{ not: {{ prop: \"{property}\", eq: 1 }} }} }}"
            ));
            assert!(manifest.validate().is_empty(), "{mode}, {property}");
            manifest
                .properties
                .get_mut("0x5010")
                .unwrap()
                .dispositions
                .remove(0);
            manifest
                .operations
                .get_mut("0x9026")
                .unwrap()
                .dispositions
                .remove(0);
            let observed = PropView::new().with(0x500a, 1);
            assert!(manifest.properties["0x5010"].is_valueless(
                "usb",
                Some("shooting/stills/manual"),
                &observed
            ));
            assert!(manifest.operations["0x9026"].is_silent_refusal(
                "usb",
                Some("shooting/stills/manual"),
                &observed
            ));
            assert!(!manifest.properties["0x5010"].is_valueless(
                "usb",
                Some("shootingExtra/stills/manual"),
                &observed
            ));
            assert!(!manifest.operations["0x9026"].is_silent_refusal("usb", None, &observed));
        }
    }
    // Unscoped legacy entries also remain valid with no declared scopes.
    let mut unscoped = manifest_with_scope("evidence: []");
    unscoped.connections.clear();
    unscoped.modes.clear();
    assert!(unscoped.validate().is_empty());
}

#[test]
fn disposition_evidence_defaults_empty_and_is_omitted_when_empty() {
    let property: PropertyDisposition = serde_yaml::from_str("disposition: valueless").unwrap();
    let operation: OperationDisposition =
        serde_yaml::from_str("disposition: silentRefusal").unwrap();
    assert!(property.evidence.is_empty());
    assert!(operation.evidence.is_empty());
    assert!(!serde_yaml::to_string(&property)
        .unwrap()
        .contains("evidence:"));
    assert!(!serde_yaml::to_string(&operation)
        .unwrap()
        .contains("evidence:"));
}

#[test]
fn disposition_evidence_round_trips_through_manifest_yaml() {
    let manifest = manifest_with_scope("evidence: [fixture, second]");
    let reloaded = CameraManifest::from_yaml(&manifest.to_yaml().unwrap()).unwrap();
    let original_property = &manifest.properties["0x5010"].dispositions;
    let original_operation = &manifest.operations["0x9026"].dispositions;
    assert_eq!(
        reloaded.properties["0x5010"].dispositions,
        *original_property
    );
    assert_eq!(
        reloaded.operations["0x9026"].dispositions,
        *original_operation
    );
    assert_eq!(original_property[1].evidence, ["fixture", "second"]);
    assert_eq!(original_operation[1].evidence, ["fixture", "second"]);
}
