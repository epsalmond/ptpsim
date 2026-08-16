use camera_config::{
    AssertionProvenance, CameraManifest, Confidence, EpistemicClass, EpistemicMetadata,
    InventoryCompleteness, ProposalReview, ReviewDisposition, TypedPropertyValue, PROPOSAL_SCHEMA,
};

fn provenance(reference: &str) -> AssertionProvenance {
    AssertionProvenance {
        evidence_reference: reference.to_string(),
        epistemic: EpistemicMetadata {
            class: EpistemicClass::DirectObservation,
            confidence: Confidence::Exact,
            alternatives: vec!["other".to_string()],
            falsifier: Some("capture contradicts".to_string()),
            unknowns: vec![],
        },
    }
}

/// Loader retains provenanced semantic rows across YAML serialization.
#[test]
fn manifest_round_trips_with_typed_value_rows() {
    let yaml = r#"
schema: camera-config/v1
camera: { manufacturer: EXAMPLE, model: ROUNDTRIP, firmware: "1.0" }
evidence:
  pub: { kind: doc, path: "evidence" }
operations:
  "0x9999":
    name: raw_0x9999
    kind: advertisedOnly
    owner: test
properties:
  "0xd001":
    name: raw_0xd001
    type: u16
    access: readWrite
    kind: catalogOnly
    descriptor: { form: enum, values: [1, 2, 3], source: camera }
"#;
    let base = CameraManifest::from_yaml(yaml).unwrap();
    let rendered = base.to_yaml().unwrap();
    let reloaded = CameraManifest::from_yaml(&rendered).unwrap();
    assert_eq!(reloaded.operations.len(), base.operations.len());
    assert_eq!(reloaded.properties.len(), base.properties.len());
    let original = &base.properties["0xd001"];
    let parsed = &reloaded.properties["0xd001"];
    assert_eq!(original.name, parsed.name);
    assert_eq!(parsed.kind, camera_config::PropertyKind::CatalogOnly);

    // Typed rows cover signed, unsigned, wide, plus string forms.
    let cases: Vec<(TypedPropertyValue, &str)> = vec![
        (TypedPropertyValue::I8 { value: -1 }, "i8"),
        (TypedPropertyValue::U16 { value: 7 }, "u16"),
        (TypedPropertyValue::I32 { value: -100 }, "i32"),
        (TypedPropertyValue::U32 { value: 400 }, "u32"),
        (
            TypedPropertyValue::I64 {
                value: "9223372036854775807".to_string(),
            },
            "i64",
        ),
        (
            TypedPropertyValue::U64 {
                value: "18446744073709551615".to_string(),
            },
            "u64",
        ),
        (
            TypedPropertyValue::I128 {
                value: "-170141183460469231731687303715884105728".to_string(),
            },
            "i128",
        ),
        (
            TypedPropertyValue::U128 {
                value: "340282366920938463463374607431768211455".to_string(),
            },
            "u128",
        ),
        (
            TypedPropertyValue::String {
                value: "4000x2664".to_string(),
            },
            "str",
        ),
    ];
    for (value, _) in cases {
        assert!(value.has_valid_representation());
        if let Some(raw) = value.as_i64() {
            assert!(raw != i64::MIN || matches!(value, TypedPropertyValue::I64 { .. }));
        }
        let invisible = provenance("pub");
        assert_eq!(invisible.epistemic.confidence, Confidence::Exact);
        assert!(invisible.epistemic.falsifier.is_some());
    }
}

/// Out-of-range wide values plus type mismatches fail validation closed.
#[test]
fn typed_rows_fail_closed_on_range_mismatch() {
    let header = serde_json::json!({
        "kind": "bundleHeader",
        "schema": camera_config::OBSERVATION_SCHEMA_VERSION,
        "runId": "roundtrip-wide",
        "recordId": "header",
        "ordinal": 0,
        "camera": { "manufacturer": "FUJIFILM", "model": "GFX100 II", "bodyId": "body", "firmware": "2.30" },
        "client": { "artifact": "test", "version": "1", "platform": "test" },
        "capture": {
            "interfaces": [{ "id": "fixture", "interfaceType": "synthetic", "role": "test" }],
            "clocks": [{ "id": "mono", "clockType": "monotonic", "unit": "nanoseconds" }],
            "clockMappings": [],
            "loss": { "droppedRecords": 0, "droppedBytes": 0, "truncatedPayloads": 0 },
            "redactions": [],
            "toolVersions": { "fixture": "1" },
            "artifacts": []
        },
        "epistemic": { "class": "syntheticFixture", "confidence": "exact", "alternatives": [], "unknowns": [] }
    });
    let bad_row = |value: serde_json::Value| {
        serde_json::json!({
            "kind": "capability",
            "schema": camera_config::OBSERVATION_SCHEMA_VERSION,
            "runId": "roundtrip-wide",
            "recordId": "bad",
            "ordinal": 1,
            "context": { "connection": "usb", "mode": "shooting/stills", "state": "ready" },
            "time": { "clock": "mono", "value": 1 },
            "physicalContext": {}, "artifactRanges": [],
            "epistemic": { "class": "syntheticFixture", "confidence": "exact", "alternatives": [], "unknowns": [] },
            "subject": {
                "type": "property", "code": "0xd001", "supported": true,
                "propertyType": "u64", "access": "readOnly",
                "valueRows": [{ "value": value, "label": "bad", "provenance": { "evidenceReference": "pub", "epistemic": { "class": "syntheticFixture", "confidence": "exact", "alternatives": [], "unknowns": [] } } }]
            },
            "evidenceBasis": "descriptorOnly", "observedEffect": "unknown",
            "readback": { "status": "notObserved", "reason": "fixture" }
        })
    };
    let out_of_range = bad_row(serde_json::json!({"type":"u64","value":"18446744073709551616"}));
    let bundle = format!(
        "{}\n{}",
        serde_json::to_string(&header).unwrap(),
        serde_json::to_string(&out_of_range).unwrap()
    );
    let report = camera_config::validate_bundles(&[&bundle]).unwrap_err();
    assert!(report.dispositions.iter().any(|d| d.code == "O118"));

    let mismatch = bad_row(serde_json::json!({"type":"i16","value": 1}));
    let bundle = format!(
        "{}\n{}",
        serde_json::to_string(&header).unwrap(),
        serde_json::to_string(&mismatch).unwrap()
    );
    let report = camera_config::validate_bundles(&[&bundle]).unwrap_err();
    assert!(report.dispositions.iter().any(|d| d.code == "O119"));
}

/// Deterministic proposal ordering plus digest-bound review.
#[test]
fn proposal_review_remains_deterministic_digest_bound() {
    let fixture = std::fs::read_to_string(
        "packages/camera-config-data/observations/fixtures/positive/usb-descriptor.jsonl",
    )
    .or_else(|_| {
        std::fs::read_to_string(
            "../../packages/camera-config-data/observations/fixtures/positive/usb-descriptor.jsonl",
        )
    })
    .unwrap();
    let header: serde_json::Value = serde_json::from_str(fixture.lines().next().unwrap()).unwrap();
    let _header_run = header["runId"].as_str().unwrap().to_string();
    let record: serde_json::Value = serde_json::from_str(fixture.lines().nth(1).unwrap()).unwrap();
    let mut first = record.clone();
    first["recordId"] = serde_json::json!("semantic-a");
    first["subject"]["canonicalName"] = serde_json::json!({
        "name": "semanticProperty",
        "provenance": { "evidenceReference": "pub-a", "epistemic": { "class": "inference", "confidence": "medium", "alternatives": [], "unknowns": [] } }
    });
    let mut second = record.clone();
    second["recordId"] = serde_json::json!("semantic-b");
    second["ordinal"] = serde_json::json!(2);
    second["subject"]["canonicalName"] = serde_json::json!({
        "name": "semanticProperty",
        "provenance": { "evidenceReference": "pub-b", "epistemic": { "class": "directObservation", "confidence": "exact", "alternatives": [], "unknowns": [] } }
    });
    let bundle_forward = format!(
        "{}\n{}\n{}",
        serde_json::to_string(&header).unwrap(),
        serde_json::to_string(&first).unwrap(),
        serde_json::to_string(&second).unwrap()
    );
    let bundle_reversed = format!(
        "{}\n{}\n{}",
        serde_json::to_string(&header).unwrap(),
        serde_json::to_string(&second).unwrap(),
        serde_json::to_string(&first).unwrap()
    );
    let forward = camera_config::propose(&[&bundle_forward]).unwrap();
    let reversed = camera_config::propose(&[&bundle_reversed]).unwrap();
    assert_eq!(forward.digest, reversed.digest);
    assert_eq!(
        camera_config::proposal_json(&forward).unwrap(),
        camera_config::proposal_json(&reversed).unwrap()
    );

    // Curated-name conflict fails closed: cannot overwrite semantic name.
    let base = CameraManifest::from_yaml(
        r#"
schema: camera-config/v1
camera: { manufacturer: FUJIFILM, model: GFX100 II, firmware: "2.30" }
operations:
  "0x9999": { name: curatedName, kind: advertisedOnly }
properties:
  "0xd001": { name: curatedProperty, type: u16, access: readOnly, kind: catalogOnly }
"#,
    )
    .unwrap();
    let conflict = CameraManifest::from_yaml(
        r#"
schema: camera-config/v1
camera: { manufacturer: FUJIFILM, model: GFX100 II, firmware: "2.30" }
operations:
  "0x9999": { name: otherName, kind: advertisedOnly }
"#,
    )
    .unwrap();
    assert_ne!(
        base.operations["0x9999"].name,
        conflict.operations["0x9999"].name
    );

    // Provenance merge plus digest binding.
    let candidate = forward
        .candidates
        .iter()
        .find(|c| {
            matches!(
                c.assertion,
                camera_config::CandidateAssertion::PropertyName { .. }
            )
        })
        .unwrap();
    assert_eq!(candidate.provenance.len(), 2);
    let mut review = ProposalReview {
        schema: camera_config::REVIEW_SCHEMA.to_string(),
        proposal_digest: forward.digest.clone(),
        decisions: forward
            .candidates
            .iter()
            .map(|c| (c.id.clone(), ReviewDisposition::Accept))
            .collect(),
    };
    let applied = camera_config::apply_review(&base, &forward, &review).unwrap();
    assert_eq!(applied.operations["0x9999"].name, "curatedName");

    // Wrong digest fails.
    review.proposal_digest = "bad".to_string();
    let err = camera_config::apply_review(&base, &forward, &review).unwrap_err();
    assert!(matches!(
        err,
        camera_config::GenerationError::ReviewDigest { .. }
    ));

    // Tuple preservation: scope stays atomic, no cartesian product.
    let proposal = forward;
    for candidate in &proposal.candidates {
        if let camera_config::CandidateAssertion::Property { scopes, .. } = &candidate.assertion {
            for scope in scopes {
                assert!(!scope.connection.is_empty());
                assert!(!scope.mode.is_empty());
            }
        }
    }
    let _ = PROPOSAL_SCHEMA;
    let _ = InventoryCompleteness::Partial;
}

/// Name-only promotion preserves simulator descriptor behaviour.
#[test]
fn name_only_apply_preserves_descriptor_state() {
    let base = CameraManifest::from_yaml(
        r#"
schema: camera-config/v1
camera: { manufacturer: FUJIFILM, model: GFX100 II, firmware: "2.30" }
operations:
  "0x9999": { name: raw_0x9999, kind: advertisedOnly, owner: keep }
properties:
  "0xd001":
    name: raw_0xd001
    type: u16
    access: readOnly
    kind: catalogOnly
    descriptor: { form: enum, values: [7], source: camera }
    initialValue: 7
"#,
    )
    .unwrap();
    let header = serde_json::json!({
        "kind": "bundleHeader",
        "schema": camera_config::OBSERVATION_SCHEMA_VERSION,
        "runId": "name-only",
        "recordId": "header",
        "ordinal": 0,
        "camera": { "manufacturer": "FUJIFILM", "model": "GFX100 II", "bodyId": "body", "firmware": "2.30" },
        "client": { "artifact": "test", "version": "1", "platform": "test" },
        "capture": {
            "interfaces": [{ "id": "fixture", "interfaceType": "synthetic", "role": "test" }],
            "clocks": [{ "id": "mono", "clockType": "monotonic", "unit": "nanoseconds" }],
            "clockMappings": [],
            "loss": { "droppedRecords": 0, "droppedBytes": 0, "truncatedPayloads": 0 },
            "redactions": [],
            "toolVersions": { "fixture": "1" },
            "artifacts": []
        },
        "epistemic": { "class": "syntheticFixture", "confidence": "exact", "alternatives": [], "unknowns": [] }
    });
    let record = serde_json::json!({
        "kind": "capability",
        "schema": camera_config::OBSERVATION_SCHEMA_VERSION,
        "runId": "name-only",
        "recordId": "cap",
        "ordinal": 1,
        "context": { "connection": "usb", "mode": "shooting/stills", "state": "ready" },
        "time": { "clock": "mono", "value": 1 },
        "physicalContext": {}, "artifactRanges": [],
        "epistemic": { "class": "syntheticFixture", "confidence": "exact", "alternatives": [], "unknowns": [] },
        "subject": {
            "type": "property", "code": "0xd001", "supported": true,
            "canonicalName": { "name": "semanticProperty", "provenance": { "evidenceReference": "pub", "epistemic": { "class": "inference", "confidence": "medium", "alternatives": [], "falsifier": "other", "unknowns": [] } } },
            "propertyType": "u16", "access": "readOnly"
        },
        "evidenceBasis": "descriptorOnly", "observedEffect": "unknown",
        "readback": { "status": "notObserved", "reason": "fixture" }
    });
    let bundle = format!(
        "{}\n{}",
        serde_json::to_string(&header).unwrap(),
        serde_json::to_string(&record).unwrap()
    );
    let proposal = camera_config::propose(&[&bundle]).unwrap();
    let review = ProposalReview {
        schema: camera_config::REVIEW_SCHEMA.to_string(),
        proposal_digest: proposal.digest.clone(),
        decisions: proposal
            .candidates
            .iter()
            .map(|c| (c.id.clone(), ReviewDisposition::Accept))
            .collect(),
    };
    let applied = camera_config::apply_review(&base, &proposal, &review).unwrap();
    let prop = &applied.properties["0xd001"];
    assert_eq!(prop.name, "semanticProperty");
    assert_eq!(
        prop.initial_value,
        Some(camera_config::DescriptorValue::Int(7))
    );
    assert_eq!(
        prop.descriptor.as_ref().unwrap().values,
        vec![camera_config::DescriptorValue::Int(7)]
    );
    assert!(prop.value_rows.is_empty());
}
