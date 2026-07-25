//! Exercises the FFI seam against the REAL camera-config-data files — the proof
//! that the `(connection, mode)` surface works end-to-end before Swift bindings.

use camera_protocol_ffi::*;
use std::path::PathBuf;

fn data(rel: &str) -> String {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../packages/camera-config-data")
        .join(rel);
    std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("read {}: {e}", p.display()))
}

fn store() -> std::sync::Arc<ConfigStore> {
    ConfigStore::from_bundle(
        data("fuji/gfx100ii/gfx100ii.yaml"),
        Some(data("fuji/fuji.yaml")),
    )
    .expect("bundle loads")
}

fn consolidated_store() -> std::sync::Arc<ConfigStore> {
    ConfigStore::from_bundle(
        data("fuji/gfx100ii/gfx100ii.consolidated.yaml"),
        Some(data("fuji/fuji.yaml")),
    )
    .expect("consolidated bundle loads")
}

fn ids(cs: &[ConnectionInfo]) -> Vec<&str> {
    cs.iter().map(|c| c.id.as_str()).collect()
}

#[test]
fn action_catalog_and_resolution_cross_the_hand_written_ffi_seam() {
    let store = consolidated_store();
    let catalog = store.action_catalog();
    assert_eq!(catalog.revision.len(), 64);
    let shutter = catalog
        .actions
        .iter()
        .find(|entry| entry.connection == "wireless-tether" && entry.action_id == "shutter")
        .expect("wireless shutter catalog entry");
    assert_eq!(
        shutter.supported_roles,
        [ActionRole::Initiator, ActionRole::Responder]
    );

    let resolved = store
        .resolve_action_invocation(ActionInvocationRequest {
            catalog_revision: catalog.revision,
            action_id: "shutter".into(),
            connection: "wireless-tether".into(),
            mode: "shooting/stills".into(),
            role: ActionRole::Responder,
            parameters: Vec::new(),
        })
        .unwrap();
    assert_eq!(resolved.role, ActionRole::Responder);
    assert_eq!(resolved.parameters[0].name, "objectCount");
    assert_eq!(resolved.parameters[0].value, 1_u64);
    assert!(matches!(
        resolved.responder_mutation,
        Some(ResponderMutation::EnqueueObjects { ref count_param }) if count_param == "objectCount"
    ));
}

#[test]
fn every_property_transition_variant_crosses_the_hand_written_ffi_seam() {
    let store = ConfigStore::from_bundle(
        r#"
schema: camera-config/v1
camera: { manufacturer: Test, model: Test, firmware: "1" }
properties:
  "0xd001": { name: result, type: u16, access: readOnly }
connections:
  app:
    actions:
      autofocusLock:
        mode: shooting/stills
        responder:
          params:
            - { name: result, kind: u32, min: 2, max: 3 }
          mutation:
            kind: propertyTransition
            target: "0xd001"
            initial: 1
            terminal: { kind: parameter, parameter: result }
            settleAfterPolls: 2
        triggers: []
      autofocusRelease:
        mode: shooting/stills
        responder:
          params: []
          mutation:
            kind: propertyTransition
            target: "0xd001"
            terminal: { kind: fixed, value: 4 }
        triggers: []
"#
        .into(),
        None,
    )
    .expect("synthetic transition manifest loads");

    let lock = store
        .action("app".into(), ActionVerb::AutofocusLock)
        .expect("lock action");
    assert!(matches!(
        lock.responder.expect("responder").mutation,
        ResponderMutation::PropertyTransition {
            target: 0xd001,
            initial: Some(1),
            terminal: PropertyTransitionTerminal::Parameter { ref parameter },
            settle_after_polls: 2,
        } if parameter == "result"
    ));

    let release = store
        .action("app".into(), ActionVerb::AutofocusRelease)
        .expect("release action");
    assert!(matches!(
        release.responder.expect("responder").mutation,
        ResponderMutation::PropertyTransition {
            target: 0xd001,
            initial: None,
            terminal: PropertyTransitionTerminal::Fixed { value: 4 },
            settle_after_polls: 0,
        }
    ));

    let catalog = store.action_catalog();
    let resolved = store
        .resolve_action_invocation(ActionInvocationRequest {
            catalog_revision: catalog.revision,
            action_id: "autofocusLock".into(),
            connection: "app".into(),
            mode: "shooting/stills".into(),
            role: ActionRole::Responder,
            parameters: vec![ActionArgument {
                name: "result".into(),
                value: ActionValue::U64 { value: 3 },
            }],
        })
        .expect("responder invocation resolves");
    assert!(matches!(
        resolved.responder_mutation,
        Some(ResponderMutation::PropertyTransition {
            target: 0xd001,
            terminal: PropertyTransitionTerminal::Parameter { ref parameter },
            ..
        }) if parameter == "result"
    ));
}

fn assert_observation_mirror_matches_kind(record: &ObservationRecord) {
    assert!(matches!(
        (&record.kind, &record.value),
        (
            ObservationKind::BundleHeader,
            ObservationValue::BundleHeader { .. }
        ) | (
            ObservationKind::Lifecycle,
            ObservationValue::Lifecycle { .. }
        ) | (ObservationKind::BleGatt, ObservationValue::BleGatt { .. })
            | (
                ObservationKind::PtpTransaction,
                ObservationValue::PtpTransaction { .. }
            )
            | (ObservationKind::PtpEvent, ObservationValue::PtpEvent { .. })
            | (
                ObservationKind::HttpExchange,
                ObservationValue::HttpExchange { .. }
            )
            | (
                ObservationKind::Capability,
                ObservationValue::Capability { .. }
            )
            | (
                ObservationKind::ActionInvocation,
                ObservationValue::ActionInvocation { .. }
            )
    ));
}

#[test]
fn every_observation_variant_has_a_complete_hand_written_ffi_mirror() {
    let mut kinds = Vec::new();
    for fixture in [
        "observations/fixtures/positive/ptpip-lifecycle-retry.jsonl",
        "observations/fixtures/positive/pcss-500d-write-readback.jsonl",
        "observations/fixtures/positive/usb-descriptor.jsonl",
        "observations/fixtures/positive/shared-action-roles.jsonl",
    ] {
        for line in data(fixture).lines().filter(|line| !line.is_empty()) {
            let mapped = parse_observation_record(line.to_string()).unwrap();
            let round_trip: serde_json::Value =
                serde_json::from_str(&mapped.canonical_json).unwrap();
            assert_eq!(round_trip["schema"], "camera-observation/v1");
            assert_observation_mirror_matches_kind(&mapped);
            kinds.push(mapped.kind);
        }
    }

    let common = serde_json::json!({
        "schema": "camera-observation/v1",
        "runId": "ffi-run",
        "recordId": "record",
        "ordinal": 1,
        "context": {"connection":"test","mode":"test","state":"test"},
        "time": {"clock":"mono","value":1},
        "epistemic": {"class":"directObservation","confidence":"exact"}
    });
    let mut ble = common.clone();
    let object = ble.as_object_mut().unwrap();
    object.insert("kind".into(), serde_json::json!("bleGatt"));
    object.insert("connectionInstance".into(), serde_json::json!("ble"));
    object.insert("operation".into(), serde_json::json!("read"));
    object.insert("service".into(), serde_json::json!("service"));
    object.insert("characteristic".into(), serde_json::json!("characteristic"));
    object.insert("outcome".into(), serde_json::json!("ok"));
    kinds.push(parse_observation_record(ble.to_string()).unwrap().kind);

    let mut event = common.clone();
    let object = event.as_object_mut().unwrap();
    object.insert("kind".into(), serde_json::json!("ptpEvent"));
    object.insert("connectionInstance".into(), serde_json::json!("event"));
    object.insert("session".into(), serde_json::json!("session"));
    object.insert("endpointSet".into(), serde_json::json!("event"));
    object.insert("transactionId".into(), serde_json::json!(0));
    object.insert(
        "transactionRecordId".into(),
        serde_json::json!("transaction"),
    );
    object.insert("event".into(), serde_json::json!("0xc001"));
    let event = parse_observation_record(event.to_string()).unwrap();
    let ObservationValue::PtpEvent { value } = &event.value else {
        panic!("expected typed PTP event")
    };
    assert_eq!(value.transaction_id, 0);
    assert_eq!(value.transaction_record_id.as_deref(), Some("transaction"));
    kinds.push(event.kind);

    let mut http = common;
    let object = http.as_object_mut().unwrap();
    object.insert("kind".into(), serde_json::json!("httpExchange"));
    object.insert("connectionInstance".into(), serde_json::json!("http"));
    object.insert(
        "request".into(),
        serde_json::json!({"method":"GET","target":"/","headers":{}}),
    );
    object.insert(
        "response".into(),
        serde_json::json!({"status":200,"headers":{}}),
    );
    object.insert("outcome".into(), serde_json::json!("ok"));
    kinds.push(parse_observation_record(http.to_string()).unwrap().kind);

    let pcss = data("observations/fixtures/positive/pcss-500d-write-readback.jsonl");
    let mapped = parse_observation_record(pcss.lines().nth(1).unwrap().into()).unwrap();
    let ObservationValue::PtpTransaction { value } = mapped.value else {
        panic!("expected typed PTP transaction")
    };
    assert!(matches!(
        value.evidence_basis,
        Some(ControlEvidenceBasis::WriteProbe)
    ));
    assert!(matches!(
        value.observed_effect,
        Some(ControlObservedEffect::Confirmed)
    ));
    let Some(ObservationReadback::Observed { baseline, .. }) = value.readback else {
        panic!("expected typed observed readback")
    };
    assert_eq!(baseline.canonical_json, "100");

    for expected in [
        ObservationKind::BundleHeader,
        ObservationKind::Lifecycle,
        ObservationKind::BleGatt,
        ObservationKind::PtpTransaction,
        ObservationKind::PtpEvent,
        ObservationKind::HttpExchange,
        ObservationKind::Capability,
        ObservationKind::ActionInvocation,
    ] {
        assert!(kinds.contains(&expected), "missing {expected:?}");
    }
}

#[test]
fn semantic_observation_assertions_cross_the_hand_written_ffi_seam() {
    let record = serde_json::json!({
        "kind": "capability",
        "schema": "camera-observation/v1",
        "runId": "ffi-semantic",
        "recordId": "property",
        "ordinal": 1,
        "context": {"connection":"synthetic","mode":"test","state":"inventory"},
        "time": {"clock":"ordinal","value":1},
        "epistemic": {"class":"syntheticFixture","confidence":"exact"},
        "subject": {
            "type": "property",
            "code": "0xd001",
            "supported": true,
            "canonicalName": {
                "name": "exposureMode",
                "provenance": {
                    "evidenceReference": "publicName",
                    "epistemic": {"class":"inference","confidence":"medium"}
                }
            },
            "sourceNativeName": {
                "name": "ExposureProgramMode",
                "provenance": {
                    "evidenceReference": "publicNativeName",
                    "epistemic": {"class":"directObservation","confidence":"high"}
                }
            },
            "propertyType": "u128",
            "valueRows": [{
                "value": {"type":"u128","value":"340282366920938463463374607431768211455"},
                "label": "maximum",
                "provenance": {
                    "evidenceReference": "publicValue",
                    "epistemic": {
                        "class":"deterministicReduction",
                        "confidence":"exact",
                        "alternatives":["reserved"],
                        "falsifier":"a capture assigns a different label"
                    }
                }
            }]
        },
        "evidenceBasis": "descriptorOnly",
        "observedEffect": "unknown",
        "readback": {"status":"notObserved","reason":"semantic fixture"}
    });
    let mapped = parse_observation_record(record.to_string()).unwrap();
    let ObservationValue::Capability { value } = mapped.value else {
        panic!("expected capability")
    };
    let ObservationCapabilitySubject::Property {
        canonical_name,
        source_native_name,
        value_rows,
        ..
    } = value.subject
    else {
        panic!("expected property")
    };
    assert_eq!(canonical_name.unwrap().name, "exposureMode");
    assert_eq!(
        source_native_name.unwrap().provenance.evidence_reference,
        "publicNativeName"
    );
    assert_eq!(value_rows.len(), 1);
    assert!(matches!(
        &value_rows[0].value,
        ObservationTypedPropertyValue::U128 { value }
            if value == "340282366920938463463374607431768211455"
    ));
    assert_eq!(
        value_rows[0].provenance.epistemic.alternatives,
        ["reserved"]
    );
}

#[test]
fn durable_semantic_provenance_crosses_the_catalog_ffi_seam() {
    let store = ConfigStore::from_bundle(
        r#"
schema: camera-config/v1
camera: { manufacturer: Test, model: Test, firmware: "1" }
evidence:
  publicName: { kind: semantic-assertion }
  publicValue: { kind: semantic-assertion }
semanticAssertions:
  operations:
    "0x9999":
      canonicalName:
        name: semanticOperation
        provenance:
          - evidenceReference: publicName
            epistemic:
              class: inference
              confidence: medium
              alternatives: [candidateOperation]
              falsifier: a capture contradicts the name
  properties:
    "0xd001":
      canonicalName:
        name: semanticProperty
        provenance:
          - evidenceReference: publicName
            epistemic: { class: deterministicReduction, confidence: high }
      sourceNativeName:
        name: NativeProperty
        provenance:
          - evidenceReference: publicName
            epistemic: { class: directObservation, confidence: exact }
      valueRows:
        - value: { type: u128, value: "340282366920938463463374607431768211455" }
          label: maximum
          provenance:
            - evidenceReference: publicValue
              epistemic:
                class: inference
                confidence: low
                alternatives: [reserved]
                falsifier: a public value table assigns another label
operations:
  "0x9999": { name: semanticOperation, kind: advertisedOnly }
properties:
  "0xd001":
    name: semanticProperty
    ptpName: NativeProperty
    type: u128
    access: readOnly
    kind: catalogOnly
"#
        .into(),
        None,
    )
    .expect("semantic ledger manifest loads");

    let operation = store
        .operations()
        .into_iter()
        .find(|operation| operation.code == 0x9999)
        .unwrap();
    assert_eq!(operation.canonical_name_provenance.len(), 1);
    assert_eq!(
        operation.canonical_name_provenance[0].evidence_reference,
        "publicName"
    );

    let property = store
        .properties()
        .into_iter()
        .find(|property| property.code == 0xd001)
        .unwrap();
    assert_eq!(property.canonical_name_provenance.len(), 1);
    assert_eq!(property.source_native_name_provenance.len(), 1);
    assert_eq!(property.semantic_value_rows.len(), 1);
    assert!(matches!(
        &property.semantic_value_rows[0].value,
        ObservationTypedPropertyValue::U128 { value }
            if value == "340282366920938463463374607431768211455"
    ));
    assert_eq!(
        property.semantic_value_rows[0].provenance[0]
            .epistemic
            .alternatives,
        ["reserved"]
    );
}

#[test]
fn init_command_response_decoder_distinguishes_ack_and_fail() {
    let ack = ptp_core::encode(&ptp_core::PtpIpPacket::InitCommandAck(
        ptp_core::InitCommandAck {
            connection_number: 0,
            responder_guid: [0x5a; 16],
            friendly_name: "CAMERA".into(),
            protocol_version: 0x0001_0000,
        },
    ))
    .unwrap();
    match decode_init_command_response(ack).unwrap() {
        InitCommandResponse::Acknowledged {
            connection_number,
            responder_guid,
            friendly_name,
            protocol_version,
        } => {
            assert_eq!(connection_number, 0);
            assert_eq!(responder_guid, vec![0x5a; 16]);
            assert_eq!(friendly_name, "CAMERA");
            assert_eq!(protocol_version, Some(0x0001_0000));
        }
        other => panic!("expected acknowledged response, got {other:?}"),
    }

    let fail = ptp_core::encode(&ptp_core::PtpIpPacket::InitFail(ptp_core::InitFail {
        reason: 0x2019,
    }))
    .unwrap();
    match decode_init_command_response(fail).unwrap() {
        InitCommandResponse::Failed { reason } => assert_eq!(reason, 0x2019),
        other => panic!("expected failed response, got {other:?}"),
    }
}

#[test]
fn init_command_response_decoder_accepts_fixed_pcss_ack() {
    let ack = protocol_primitives::pcss_init_ack_message(0, [0x5a; 16], "GFX100 II").unwrap();
    match decode_init_command_response(ack).unwrap() {
        InitCommandResponse::Acknowledged {
            connection_number,
            responder_guid,
            friendly_name,
            protocol_version,
        } => {
            assert_eq!(connection_number, 0);
            assert_eq!(responder_guid, vec![0x5a; 16]);
            assert_eq!(friendly_name, "GFX100 II");
            assert_eq!(protocol_version, None);
        }
        other => panic!("expected acknowledged response, got {other:?}"),
    }
}

#[test]
fn legacy_app_init_ack_validator_accepts_opaque_tail_and_checks_guid() {
    let expected = vec![0x5a; 16];
    let mut ack = vec![0xa5; 68];
    ack[0..4].copy_from_slice(&68u32.to_le_bytes());
    ack[4..8].copy_from_slice(&2u32.to_le_bytes());
    ack[8..12].copy_from_slice(&1u32.to_le_bytes());
    ack[12..28].copy_from_slice(&expected);

    validate_legacy_app_init_ack(ack.clone(), expected.clone())
        .expect("legacy manufacturer app tail is opaque");
    assert!(decode_init_command_response(ack.clone()).is_err());

    let mut wrong_guid = expected.clone();
    wrong_guid[0] ^= 0xff;
    assert!(validate_legacy_app_init_ack(ack.clone(), wrong_guid).is_err());
    assert!(validate_legacy_app_init_ack(ack[..67].to_vec(), expected).is_err());
}

#[test]
fn init_command_response_decoder_rejects_malformed_fixed_pcss_acks() {
    let valid = protocol_primitives::pcss_init_ack_message(0, [0x5a; 16], "GFX100 II").unwrap();

    let mut nonzero_padding = valid.clone();
    nonzero_padding[67] = 1;
    assert!(decode_init_command_response(nonzero_padding).is_err());

    let mut missing_terminator = valid.clone();
    for byte in &mut missing_terminator[28..] {
        *byte = 0x41;
    }
    assert!(decode_init_command_response(missing_terminator).is_err());

    let mut invalid_utf16 = valid;
    invalid_utf16[28..32].copy_from_slice(&[0x00, 0xd8, 0x00, 0x00]);
    assert!(decode_init_command_response(invalid_utf16).is_err());
}

#[test]
fn init_command_response_decoder_rejects_trailing_standard_bytes() {
    let mut ack = ptp_core::encode(&ptp_core::PtpIpPacket::InitCommandAck(
        ptp_core::InitCommandAck {
            connection_number: 0,
            responder_guid: [0x5a; 16],
            friendly_name: "CAMERA".into(),
            protocol_version: 0x0001_0000,
        },
    ))
    .unwrap();
    ack.extend_from_slice(&[0, 0, 0, 0]);
    let length = ack.len() as u32;
    ack[0..4].copy_from_slice(&length.to_le_bytes());

    assert!(decode_init_command_response(ack).is_err());
}

#[test]
fn init_command_response_decoder_does_not_format_large_wrong_payloads() {
    let packet = ptp_core::encode(&ptp_core::PtpIpPacket::Data(ptp_core::DataBlock {
        transaction_id: 1,
        payload: vec![0xab; 4 * 1024 * 1024],
    }))
    .unwrap();

    let error = decode_init_command_response(packet).unwrap_err();
    assert_eq!(
        error.to_string(),
        "expected InitCommandAck or InitFail, got Data"
    );
}

fn ptp_steps(plan: &ModeEntryPlan) -> &[EntryStep] {
    match &plan.execution {
        ModeEntryExecution::Ptp { steps } => steps,
        other => panic!("expected PTP mode entry, got {other:?}"),
    }
}

fn assert_bootstrap_tail_surfaces(steps: &[EntryStep]) {
    assert_no_gate_metadata_surfaces(steps);
    let d22b = steps
        .iter()
        .position(|st| matches!(st, EntryStep::GetProp { prop: 0xd22b, .. }))
        .expect("D22B bootstrap read crosses FFI");
    let page = steps
        .iter()
        .position(|st| matches!(st, EntryStep::SendOp { op: 0x9053, .. }))
        .expect("0x9053 page op crosses FFI");
    let final_d212 = steps
        .iter()
        .enumerate()
        .skip(page + 1)
        .find_map(|(i, st)| matches!(st, EntryStep::GetProp { prop: 0xd212, .. }).then_some(i))
        .expect("final D212 read crosses FFI after 0x9053");
    assert!(d22b < page && page < final_d212);
    match &steps[page] {
        EntryStep::SendOp { params, .. } => {
            assert!(matches!(
                params.as_slice(),
                [
                    EntryParam::Literal { value: 0 },
                    EntryParam::Literal { value: 0x7530 }
                ]
            ));
        }
        other => panic!("expected 0x9053 SendOp, got {other:?}"),
    }
}

fn assert_no_gate_metadata_surfaces(steps: &[EntryStep]) {
    for step in steps {
        match step {
            EntryStep::SetProp {
                prop: _,
                value: _,
                tolerant: _,
            }
            | EntryStep::SetPropRuntime {
                prop: _,
                slot: _,
                if_missing: _,
                tolerant: _,
            }
            | EntryStep::GetProp {
                prop: _,
                captures: _,
                tolerant: _,
            }
            | EntryStep::ReadEcho {
                prop: _,
                captures: _,
                tolerant: _,
            }
            | EntryStep::SendOp {
                op: _,
                params: _,
                captures: _,
                repeat: _,
                tolerant: _,
            }
            | EntryStep::OpenChannel {
                role: _,
                tolerant: _,
            }
            | EntryStep::ReopenSession { tolerant: _ }
            | EntryStep::CloseSession {
                transport_close: _,
                tolerant: _,
            } => {}
            EntryStep::AwaitUntil {
                source: _,
                until: _,
                on_each,
                timeout_ms: _,
                interval_ms: _,
                tolerant: _,
                captures: _,
            } => assert_no_gate_metadata_surfaces(on_each),
            EntryStep::Retry { steps, .. } => assert_no_gate_metadata_surfaces(steps),
            EntryStep::Loop { kind, tolerant: _ } => match kind {
                FfiLoopKind::ForEach {
                    collection: _,
                    bind: _,
                    body,
                }
                | FfiLoopKind::Chunk {
                    total: _,
                    size: _,
                    offset_bind: _,
                    length_bind: _,
                    body,
                } => assert_no_gate_metadata_surfaces(body),
            },
            EntryStep::If {
                slot: _,
                equals: _,
                then_steps,
                tolerant: _,
            } => assert_no_gate_metadata_surfaces(then_steps),
            EntryStep::IfElse {
                then_steps,
                else_steps,
                ..
            } => {
                assert_no_gate_metadata_surfaces(then_steps);
                assert_no_gate_metadata_surfaces(else_steps);
            }
        }
    }
}

#[test]
fn platform_filters_connections_macos_vs_ios() {
    let s = store();
    let mac = s.connections(Platform::Macos);
    let ios = s.connections(Platform::Ios);
    // macOS sees USB + the wired tether; iOS does not (platforms: excludes it).
    assert!(ids(&mac).contains(&"usb"), "macOS has USB");
    assert!(ids(&mac).contains(&"wireless-tether"));
    assert!(
        !ids(&ios).contains(&"usb"),
        "iOS hides USB — same build, data-driven"
    );
    // App + XLV available to both.
    assert!(ids(&ios).contains(&"app"));
    assert!(ids(&mac).contains(&"xlv"));
}

#[test]
fn operation_gating_is_connection_and_mode_keyed() {
    let s = store();
    // 0x9018 (tether live-view) is available over the tether, wrong-connection over app.
    assert!(matches!(
        s.operation_available(
            "wireless-tether".into(),
            "shooting/stills".into(),
            0x9018,
            vec![]
        ),
        Availability::Available
    ));
    assert!(matches!(
        s.operation_available("app".into(), "shooting/stills".into(), 0x9018, vec![]),
        Availability::WrongConnection
    ));
    for op in [0x101cu16, 0x1018] {
        assert!(matches!(
            s.operation_available(
                "wireless-tether".into(),
                "shooting/stills".into(),
                op,
                vec![]
            ),
            Availability::Available
        ));
    }
    assert!(matches!(
        s.operation_available(
            "wireless-tether".into(),
            "shooting/stills".into(),
            0x1007,
            vec![]
        ),
        Availability::Available
    ));
    // Backup op (0x100c) is available in ANY mode over usb (modes: []).
    assert!(matches!(
        s.operation_available("usb".into(), "shooting/stills".into(), 0x100c, vec![]),
        Availability::Available
    ));
    // A genuine WrongMode: the raw-conv op (0x900c) is mode-specific.
    assert!(matches!(
        s.operation_available("usb".into(), "shooting/stills".into(), 0x900c, vec![]),
        Availability::WrongMode
    ));
}

#[test]
fn control_mechanism_varies_by_connection() {
    let s = store();
    let ctl = s
        .control_for("wireless-tether".into(), "shooting/stills".into(), 0x5007)
        .expect("aperture control over tether");
    assert_eq!(ctl.set_method.as_deref(), Some("absolute"));
    assert_eq!(ctl.operation, Some(0x1016));

    let app_aperture = s
        .control_for("app".into(), "shooting/stills".into(), 0x5007)
        .expect("aperture control over app");
    assert_eq!(app_aperture.set_method.as_deref(), Some("vendorStep"));
    assert_eq!(app_aperture.operation, Some(0x902d));
    assert_eq!(app_aperture.readback, Some(0xd212));

    let app_iso = s
        .control_for("app".into(), "shooting/stills".into(), 0xd02a)
        .expect("ISO control over app");
    assert_eq!(app_iso.set_method.as_deref(), Some("absolute"));
    assert_eq!(app_iso.operation, Some(0x1016));
    assert_eq!(app_iso.readback, Some(0xd212));
}

#[test]
fn app_current_behavior_ops_gate_through_ffi() {
    let s = store();
    assert!(matches!(
        s.operation_available("app".into(), "shooting/stills".into(), 0x9026, vec![]),
        Availability::Available
    ));
    assert!(matches!(
        s.operation_available("app".into(), "shooting/stills".into(), 0x100e, vec![]),
        Availability::Available
    ));
    assert!(matches!(
        s.operation_available("app".into(), "image-transfer".into(), 0x1008, vec![]),
        Availability::Available
    ));
    assert!(matches!(
        s.operation_available("app".into(), "image-transfer".into(), 0x101b, vec![]),
        Availability::Available
    ));
}

#[test]
fn mode_entry_returns_the_ground_truth_wire_steps() {
    let s = store();
    let plan = s
        .mode_entry("app".into(), None, "shooting/stills".into())
        .expect("live-view entry");
    let steps = ptp_steps(&plan);
    // First step: SetProp 0xdf00 = 6 (the real live-view startup constant).
    match &steps[0] {
        EntryStep::SetProp { prop, value, .. } => {
            assert_eq!(*prop, 0xdf00);
            assert_eq!(*value, 6);
        }
        other => panic!("expected SetProp, got {other:?}"),
    }
    // The 902B repeat survives the round-trip.
    assert!(steps.iter().any(|st| matches!(
        st,
        EntryStep::SendOp {
            op: 0x902b,
            repeat: 4,
            ..
        }
    )));

    // A USB sub-mode entry is a userInstruction (camera menu), no steps.
    let usb = s
        .mode_entry("usb".into(), None, "raw-conv-backup-restore".into())
        .unwrap();
    assert!(matches!(
        usb.execution,
        ModeEntryExecution::UserInstruction { .. }
    ));
}

#[test]
fn malformed_predicate_prop_is_a_load_error() {
    let body = r#"
schema: camera-config/v1
camera: { manufacturer: Test, model: Test, firmware: "1" }
connections:
  app:
    kind: ptpip-app
    entries:
      - to: shooting
        steps:
          - awaitUntil:
              source: { poll: "0xd209" }
              until: { prop: "0xzz", eq: 1 }
              timeoutMs: 1000
"#;
    let error = match ConfigStore::from_bundle(body.into(), None) {
        Ok(_) => panic!("malformed awaitUntil predicate must fail store construction"),
        Err(error) => error,
    };
    assert!(matches!(
        error,
        ConfigError::Contract(message)
            if message.contains("predicate leaf prop `0xzz` is not a hex property code")
    ));
}

#[test]
fn mode_entry_requires_crosses_the_mirror() {
    let body = r#"
schema: camera-config/v1
camera: { manufacturer: Test, model: Test, firmware: "1" }
connections:
  app:
    kind: ptpip-app
    entries:
      - to: shooting
        steps: []
        requires: { prop: "0xd209", mask: 255, eq: 1 }
"#;
    let store = ConfigStore::from_bundle(body.into(), None).expect("manifest loads");
    let plan = store
        .mode_entry("app".into(), None, "shooting".into())
        .expect("mode entry");
    assert!(matches!(
        plan.requires,
        Some(FfiPredicate::Leaf {
            prop: 0xd209,
            mask: Some(255),
            eq: Some(1),
            ..
        })
    ));
}

#[test]
fn connection_transition_requires_crosses_the_mirror() {
    let body = r#"
schema: camera-config/v1
camera: { manufacturer: Test, model: Test, firmware: "1" }
connections:
  ble:
    kind: ble
    enables:
      - to: app
        mechanism: test
        requires: { prop: "0xd212", ne: 0 }
  app: { kind: ptpip-app, establishment: test }
"#;
    let store = ConfigStore::from_bundle(body.into(), None).expect("manifest loads");
    let transition = store
        .connection_transition("ble".into(), "app".into(), None)
        .expect("connection transition");
    assert!(matches!(
        transition.requires,
        Some(FfiPredicate::Leaf {
            prop: 0xd212,
            ne: Some(0),
            ..
        })
    ));
}

#[test]
fn malformed_requires_predicate_is_a_load_error() {
    let body = r#"
schema: camera-config/v1
camera: { manufacturer: Test, model: Test, firmware: "1" }
connections:
  ble:
    kind: ble
    enables:
      - to: app
        mechanism: test
        requires: { prop: "0xzz", eq: 1 }
  app: { kind: ptpip-app, establishment: test }
"#;
    let error = match ConfigStore::from_bundle(body.into(), None) {
        Ok(_) => panic!("malformed requires predicate must fail store construction"),
        Err(error) => error,
    };
    assert!(matches!(
        error,
        ConfigError::Contract(message)
            if message.contains("predicate leaf prop `0xzz` is not a hex property code")
    ));
}

#[test]
fn non_scalar_fixed_value_is_a_load_error() {
    let body = r#"
schema: camera-config/v1
camera: { manufacturer: Test, model: Test, firmware: "1" }
values:
  invalid:
    type: fixed
    value: [1, 2]
"#;
    let error = match ConfigStore::from_bundle(body.into(), None) {
        Ok(_) => panic!("non-scalar fixed value must fail store construction"),
        Err(error) => error,
    };
    assert!(matches!(
        error,
        ConfigError::Contract(message)
            if message.contains("values.invalid: fixed value is not a scalar")
    ));
}

#[test]
fn connection_establishment_is_returned_as_data() {
    let s = store();
    // wireless-tether: PCSS knock params surfaced for the app to drive.
    let wt = s
        .connection_establishment("wireless-tether".into())
        .unwrap();
    assert_eq!(wt.mechanism.as_deref(), Some("pcss-knock"));
    assert!(wt
        .params
        .iter()
        .any(|kv| kv.key == "knockPort" && kv.value == "51562"));
    assert!(wt
        .params
        .iter()
        .any(|kv| kv.key == "initRetriesMax" && kv.value == "3"));
    assert!(matches!(
        wt.activities.as_slice(),
        [ConnectionActivityDescriptor {
            id,
            version: 2,
            display_role: ConnectionActivityDisplayRole::OpeningSession,
            optional: false,
            binding: ConnectionActivityBinding::HostEstablishment {
                action: HostEstablishment::RetainedSessionOpen {
                    socket_role: SocketRole::Command,
                },
            },
            ..
        }] if id == "camera.session.open.direct"
    ));
    // app is brought up via the BLE→WiFi handover.
    let app = s.connection_establishment("app".into()).unwrap();
    assert_eq!(app.mechanism.as_deref(), Some("ble-establish-wifi-ap"));
    assert_eq!(app.activities.len(), 3);
}

#[test]
fn pcss_rendezvous_is_typed_and_codecs_are_manifest_driven() {
    let s = store();
    let connection = s
        .connections(Platform::Macos)
        .into_iter()
        .find(|candidate| candidate.id == "wireless-tether")
        .expect("wireless-tether connection info");
    assert!(connection.auto_discoverable);
    let rendezvous = s
        .pcss_rendezvous("wireless-tether".into())
        .expect("wireless-tether PCSS rendezvous");
    assert_eq!(rendezvous.callback_port, 51560);
    assert_eq!(rendezvous.knock_port, 51562);
    assert_eq!(rendezvous.protocol, "PCSS/1.0");
    assert_eq!(rendezvous.camera_name.as_deref(), Some("GFX100 II"));
    assert_eq!(
        rendezvous.default_discovery_target,
        PcssDiscoveryTarget::SubnetBroadcast
    );
    assert_eq!(
        rendezvous.supported_discovery_targets,
        [
            PcssDiscoveryTarget::SubnetBroadcast,
            PcssDiscoveryTarget::ExplicitUnicast,
        ]
    );
    assert!(rendezvous.retry_discovered_unicast);
    assert_eq!(
        rendezvous.callback_message_terminator,
        b"SERVICE: PCSS/1.0\r\n"
    );
    assert_eq!(rendezvous.retry_interval_ms, 1_000);
    assert_eq!(rendezvous.max_attempts, 15);
    assert_eq!(rendezvous.connect_timeout_ms, 5_000);
    assert_eq!(rendezvous.init_retries_max, 3);
    assert_eq!(rendezvous.init_retries_backoff_ms, 500);
    assert_eq!(rendezvous.init_retry_reasons, [0x2019]);
    let retry = s
        .connection_init_retry_policy("wireless-tether".into())
        .expect("typed PCSS init retry policy");
    assert_eq!(retry.max_retries, 3);
    assert_eq!(retry.backoff_ms, 500);
    assert_eq!(retry.when_reasons, vec![0x2019]);

    let discovery = s
        .build_pcss_discovery("wireless-tether".into(), "192.0.2.49".into())
        .expect("discovery builds");
    assert_eq!(
        discovery,
        b"DISCOVERY * HTTP/1.1\r\nHOST: 192.0.2.49\r\nMX: 5\r\nSERVICE: PCSS/1.0\r\n\0"
    );
    let guid = vec![
        0xf2, 0xe4, 0x53, 0x8f, 0xad, 0xa5, 0x48, 0x5d, 0x87, 0xb2, 0x7f, 0x0b, 0xd3, 0xd5, 0xde,
        0xd0,
    ];
    let init = s
        .build_pcss_init(
            "wireless-tether".into(),
            guid,
            "192.0.2.49".into(),
            "mbp".into(),
        )
        .expect("PCSS init builds");
    assert_eq!(init.len(), 82);
    assert_eq!(&init[0..8], &[82, 0, 0, 0, 1, 0, 0, 0]);
    assert_eq!(&init[24..28], &[0x31, 0x02, 0x00, 0xc0]);
    assert_eq!(&init[28..36], &[b'm', 0, b'b', 0, b'p', 0, 0, 0]);
    assert!(s
        .build_pcss_init(
            "wireless-tether".into(),
            vec![0; 15],
            "192.0.2.49".into(),
            "mbp".into(),
        )
        .is_err());
    assert!(s
        .build_pcss_init(
            "wireless-tether".into(),
            vec![0; 16],
            "192.0.2.49".into(),
            "mbp\0other".into(),
        )
        .is_err());
    assert!(s
        .build_pcss_init(
            "wireless-tether".into(),
            vec![0; 16],
            "not-ipv4".into(),
            "mbp".into(),
        )
        .is_err());
    assert!(s
        .build_pcss_init(
            "wireless-tether".into(),
            vec![0; 16],
            "192.0.2.49".into(),
            "thirteen-units".into(),
        )
        .is_err());
    let notify = s
        .parse_pcss_notify(
            "wireless-tether".into(),
            b"NOTIFY * HTTP/1.1\r\nDSC: 198.51.100.50\r\nCAMERANAME: CAMERA\r\nDSCPORT: 15740\r\nMX: 7\r\nSERVICE: PCSS/1.0\r\n"
                .to_vec(),
        )
        .expect("notify parses");
    assert_eq!(notify.camera_ipv4, "198.51.100.50");
    assert_eq!(notify.camera_name, "CAMERA");
    assert_eq!(notify.command_port, 15740);
    assert_eq!(notify.service, "PCSS/1.0");
    assert_eq!(
        s.build_pcss_callback_ack("wireless-tether".into())
            .expect("ack builds"),
        b"HTTP/1.1 200 OK\r\n\0"
    );
    assert!(s
        .build_pcss_discovery("wireless-tether".into(), "not-ipv4".into())
        .is_err());
    assert!(s.pcss_rendezvous("app".into()).is_none());

    let manifest_without_camera_name =
        data("fuji/gfx100ii/gfx100ii.yaml").replace("      cameraName: \"GFX100 II\"\n", "");
    let without_camera_name =
        ConfigStore::from_bundle(manifest_without_camera_name, Some(data("fuji/fuji.yaml")))
            .expect("fixture without PCSS cameraName loads");
    assert_eq!(
        without_camera_name
            .pcss_rendezvous("wireless-tether".into())
            .expect("fixture has PCSS rendezvous")
            .camera_name,
        None
    );
}

#[test]
fn pcss_retry_queries_preserve_full_u32_init_fail_reasons() {
    let manifest = data("fuji/gfx100ii/gfx100ii.yaml")
        .replace("whenReasons: [\"0x2019\"]", "whenReasons: [\"0x00012019\"]");
    let s = ConfigStore::from_bundle(manifest, Some(data("fuji/fuji.yaml")))
        .expect("bundle accepts a full-width InitFail reason");

    let rendezvous = s
        .pcss_rendezvous("wireless-tether".into())
        .expect("wireless-tether PCSS rendezvous");
    assert_eq!(rendezvous.init_retry_reasons, [0x0001_2019]);

    let retry = s
        .connection_init_retry_policy("wireless-tether".into())
        .expect("typed PCSS init retry policy");
    assert_eq!(retry.when_reasons, [0x0001_2019]);
}

#[test]
fn pcss_transfer_and_semantic_controls_surface_evidence_state() {
    let s = store();
    let transfer = s
        .object_transfer_contract("wireless-tether".into())
        .expect("PCSS object-transfer contract");
    assert!(matches!(
        transfer.strategy,
        ObjectTransferStrategy::WholeObject
    ));
    assert!(matches!(
        transfer.resume_policy,
        ObjectTransferResumePolicy::RestartFromZero
    ));
    assert!(matches!(transfer.read_action, ActionVerb::GetObject));
    assert!(matches!(
        transfer.completion_action,
        Some(ActionVerb::DeleteObject)
    ));
    assert!(matches!(
        transfer.completion_after,
        Some(ObjectTransferCompletionTiming::LocalCommit)
    ));
    assert_eq!(transfer.formats.len(), 4);
    assert!(transfer.formats.iter().any(|format| {
        format.code == 0xb105 && matches!(format.support, ObjectTransferFormatSupport::Confirmed)
    }));
    assert!(transfer.formats.iter().any(|format| {
        format.code == 0x3801 && matches!(format.support, ObjectTransferFormatSupport::Experimental)
    }));

    let controls = s.control_surface("wireless-tether".into(), "shooting/stills".into());
    assert_eq!(controls.len(), 5);
    let focus = controls
        .iter()
        .find(|control| matches!(control.role, ControlRole::FocusArea))
        .expect("focus-area semantic control");
    assert_eq!(focus.property, 0xd395);
    let exposure_bias = controls
        .iter()
        .find(|control| matches!(control.role, ControlRole::ExposureBias))
        .expect("exposure-bias semantic control");
    assert_eq!(exposure_bias.property, 0x5010);
    assert!(matches!(
        exposure_bias.read_source,
        ControlReadSource::DirectProperty
    ));
    assert!(matches!(
        exposure_bias.evidence_basis,
        ControlEvidenceBasis::DescriptorOnly
    ));
    assert!(matches!(
        exposure_bias.observed_effect,
        ControlObservedEffect::Unknown
    ));
    assert_eq!(exposure_bias.control.operation, Some(0x1016));
}

#[test]
fn value_policy_resolves_initiator_identity_from_manufacturer_tier() {
    let s = store();
    match s.value("initiatorGuid".into()) {
        Some(ResolvedValue::Fixed { value }) => {
            assert_eq!(value, "f2e4538fada5485d87b27f0bd3d5ded0");
        }
        other => panic!("expected fixed initiator GUID, got {other:?}"),
    }
}

#[test]
fn explained_gate_traces_real_data_decisions() {
    let s = store();
    // 0x900c is a (usb, raw-conv-backup-restore) op. Over the app connection → WrongConnection,
    // and the trace says why (what telemetry captures) — no predicate eval needed.
    let wc = s.operation_available_explained(
        "app".into(),
        "raw-conv-backup-restore".into(),
        0x900c,
        vec![],
    );
    assert!(matches!(wc.availability, Availability::WrongConnection));
    assert!(!wc.trace.connection_ok);
    assert!(wc.trace.requires.is_none()); // this op declares no prerequisite
    assert!(wc.trace.reason.contains("usb"));
    // Over its own connection/mode → Available, both axes ok.
    let ok = s.operation_available_explained(
        "usb".into(),
        "raw-conv-backup-restore".into(),
        0x900c,
        vec![],
    );
    assert!(matches!(ok.availability, Availability::Available));
    assert!(ok.trace.connection_ok && ok.trace.mode_ok);
    // Unknown op → Unavailable with an explanatory reason.
    let un =
        s.operation_available_explained("app".into(), "shooting/stills".into(), 0x9999, vec![]);
    assert!(matches!(un.availability, Availability::Unavailable));
    assert!(un.trace.reason.contains("not defined"));
}

#[test]
fn explained_gate_carries_predicate_leaf_detail_through_ffi() {
    // Synthetic manifest (FFI plumbing test, NOT camera facts): an op with a
    // `requires` prerequisite so the leaf-eval detail flows through the boundary.
    let yaml = r#"
schema: camera-config/v1
camera: { manufacturer: FUJIFILM, model: GFX100 II, firmware: "2.30" }
operations:
  "0x101c": { name: OpenCap, modes: [shooting], connections: [app], requires: { prop: "0xd212", mask: 0x00ff, ne: 0 } }
connections: { app: { kind: ptpip-app } }
modes: { "shooting/stills": {} }
"#;
    let s = ConfigStore::from_bundle(yaml.to_string(), None).expect("loads");
    // Low byte masks to 0 → `ne 0` fails → Blocked, and the leaf shows exactly why.
    let g = s.operation_available_explained(
        "app".into(),
        "shooting/stills".into(),
        0x101c,
        vec![PropObservation {
            code: 0xd212,
            value: 0xab00,
        }],
    );
    assert!(matches!(g.availability, Availability::Blocked));
    let req = g.trace.requires.expect("requires evaluated");
    assert!(!req.passed);
    let leaf = &req.leaves[0];
    assert_eq!(leaf.prop, "0xd212");
    assert_eq!(leaf.observed, Some(0xab00));
    assert_eq!(leaf.effective, Some(0x00));
    assert!(!leaf.passed);
    // Satisfy it → Available.
    let ok = s.operation_available_explained(
        "app".into(),
        "shooting/stills".into(),
        0x101c,
        vec![PropObservation {
            code: 0xd212,
            value: 0xab01,
        }],
    );
    assert!(matches!(ok.availability, Availability::Available));
    assert!(ok.trace.requires.unwrap().passed);
}

#[test]
fn property_value_width_resolves_from_manifest_type() {
    let s = store();
    // Scalar manifest types map to encoder widths; u8a (rawSettings) and unknown props → None.
    assert!(matches!(
        s.property_value_width(0xd246),
        Some(ValueWidth::U8)
    )); // stills/video selector u8
    assert_eq!(
        encode_value(1, ValueWidth::U8).expect("u8 encodes across the FFI seam"),
        vec![0x01]
    );
    assert!(matches!(
        s.property_value_width(0x5007),
        Some(ValueWidth::U16)
    )); // aperture u16
    assert!(matches!(
        s.property_value_width(0xdf28),
        Some(ValueWidth::U32)
    )); // featureVersion u32
    assert!(matches!(
        s.property_value_width(0xd02a),
        Some(ValueWidth::U32)
    )); // App still ISO u32 — literal/auto, degenerate u16 stub overridden (#100)
    assert!(matches!(
        s.property_value_width(0xd240),
        Some(ValueWidth::U32)
    )); // shutter u32 — 0x80000000|denom*1000, stub overridden (#100)
    assert!(matches!(
        s.property_value_width(0x5010),
        Some(ValueWidth::I16)
    )); // exposure bias i16 — signed (#88)
    assert!(matches!(
        s.property_value_width(0x500f),
        Some(ValueWidth::I32)
    )); // standard ISO (ExposureIndex) i32 — signed, auto sentinels (#88)
    assert!(matches!(
        s.property_value_width(0xd226),
        Some(ValueWidth::U16)
    )); // imageImportFilter u16
    assert!(matches!(
        s.property_value_width(0xd227),
        Some(ValueWidth::U16)
    )); // imageImportSort u16
    assert!(s.property_value_width(0xd185).is_none()); // rawSettings u8a → unsupported
    assert!(s.property_value_width(0x9999).is_none()); // unknown property
}

#[test]
fn property_value_codec_crosses_the_ffi_seam() {
    let s = consolidated_store();
    let decoded = s
        .decode_property(0xd02a, 0x8000_1900)
        .expect("auto ISO decodes through the manifest");
    assert_eq!(decoded.raw, 0x8000_1900);
    assert_eq!(decoded.label, "AUTO 6400");
    assert_eq!(
        s.encode_property(0xd02a, "AUTO 6400".into())
            .expect("auto ISO encodes"),
        vec![0x00, 0x19, 0x00, 0x80]
    );

    let iso = s
        .properties()
        .into_iter()
        .find(|p| p.code == 0xd02a)
        .expect("still ISO in catalog");
    assert!(iso.value_rows.iter().any(|row| row.label == "6400"));
    let sentinel = iso
        .value_encoding
        .as_ref()
        .and_then(|enc| enc.sentinel.as_ref())
        .expect("sentinel metadata crosses the seam");
    assert_eq!(sentinel.mask, 0x8000_0000);
    assert_eq!(sentinel.meaning.as_deref(), Some("autoCeiling"));
    assert_eq!(sentinel.label_prefix, "AUTO");
    let encoding = iso
        .value_encoding
        .expect("encoding metadata crosses the seam");
    assert!(
        encoding.masks.iter().any(|mask| mask.mask == 0x4000_0000
            && mask.meaning.as_deref() == Some("extendedSensitivity")),
        "extended-sensitivity mask must cross the FFI seam"
    );
    let profile = iso
        .value_profiles
        .iter()
        .find(|profile| profile.connection.as_deref() == Some("app"))
        .expect("still ISO app value profile crosses the seam");
    assert!(profile.rows.iter().any(|row| row.raw == 80 && row.legal));
    assert!(profile
        .rows
        .iter()
        .any(|row| row.raw == 50 && !row.legal && row.write_store_raw == Some(80)));
}

#[test]
fn property_sentinel_codec_does_not_require_legacy_label_rows() {
    let yaml = r#"
schema: camera-config/v1
camera: { manufacturer: Test, model: Body, firmware: "1.0" }
properties:
  "0xd02a":
    name: stillIso
    type: u32
    access: readWrite
    valueRows:
      - { label: "6400", raw: 6400 }
    valueEncoding:
      sentinel: { mask: 2147483648, meaning: autoCeiling, labelPrefix: AUTO }
"#;
    let s = ConfigStore::from_bundle(yaml.to_string(), None).expect("bundle loads");
    assert_eq!(
        s.decode_property(0xd02a, 0x8000_1900)
            .expect("sentinel decode")
            .label,
        "AUTO 6400"
    );
    assert_eq!(
        s.encode_property(0xd02a, "AUTO 6400".into())
            .expect("sentinel encode"),
        vec![0x00, 0x19, 0x00, 0x80]
    );
}

#[test]
fn property_payload_surfaces_d212_record_stream() {
    let s = store();
    // 0xD212 live-status is a record stream the app walks; the descriptor + its
    // member allowlist must survive the FFI boundary intact (a dropped `members`
    // would silently lose the poll set the consumer keys on).
    let p = s
        .property_payload(0xd212)
        .expect("0xD212 carries a payload descriptor");
    assert!(matches!(p.form, PayloadForm::RecordStream));
    assert_eq!(p.count_width, Some(2));
    let rec = p.record.expect("record layout present");
    assert_eq!((rec.code_width, rec.value_width), (2, 4));
    assert!(p.members.iter().any(|member| member.code == 0xd17c)); // s1Lock
    assert!(p.members.iter().any(|member| member.code == 0xd209)); // s1LockColor
    assert!(p.members.iter().any(|member| member.code == 0x5007)); // aperture
    assert_eq!(
        p.members
            .iter()
            .find(|member| member.code == 0xd22f)
            .map(|member| member.encoding),
        Some(RecordValueEncodingInfo::PtpString)
    );
    assert!(s.property_payload(0x5007).is_none()); // a scalar property → no payload
}

#[test]
fn take_to_get_entry_reestablishes_with_image_import_launch() {
    let s = store();
    let plan = s
        .mode_entry(
            "app".into(),
            Some("shooting/stills".into()),
            "image-transfer".into(),
        )
        .expect("from-Stills image-import entry");
    let ModeEntryExecution::ReestablishConnection {
        connection,
        exit_steps,
        establishment_params,
    } = &plan.execution
    else {
        panic!("expected outer re-establishment, got {:?}", plan.execution);
    };
    assert_eq!(connection, "app");
    assert_eq!(
        establishment_params
            .iter()
            .find(|param| param.key == "launchMode")
            .map(|param| param.value.as_str()),
        Some("3")
    );
    assert!(matches!(
        exit_steps[0],
        EntryStep::SendOp { op: 0x1018, .. }
    ));
    assert!(matches!(
        exit_steps[1],
        EntryStep::CloseSession {
            transport_close: true,
            tolerant: false,
        }
    ));
    assert_eq!(exit_steps.len(), 2);

    let cold = s
        .mode_entry("app".into(), None, "image-transfer".into())
        .expect("cold image-transfer entry");
    assert!(ptp_steps(&cold).iter().all(|step| !matches!(
        step,
        EntryStep::SendOp {
            op: 0x9050 | 0x9053,
            ..
        } | EntryStep::Retry { .. }
    )));
}

#[test]
fn store_rejects_an_unmappable_reestablishment_exit_step() {
    let body = r#"
schema: camera-config/v1
camera: { manufacturer: Test, model: Test, firmware: "1" }
connections:
  app:
    establishment: test
    entries:
      - { to: image-transfer, steps: [] }
      - to: image-transfer
        from: shooting/stills
        reestablishConnection:
          params: { launchMode: "3" }
          exitSteps:
            - { sendOp: not-a-hex-code }
"#;
    let error = match ConfigStore::from_bundle(body.into(), None) {
        Ok(_) => panic!("unmappable exit step must fail store construction"),
        Err(error) => error,
    };
    assert!(matches!(
        error,
        ConfigError::Contract(message)
            if message.contains("mode entry app[1] exitSteps contains an unmappable step")
    ));
}

#[test]
fn get_to_take_entry_reopens_then_starts_live_view() {
    let s = store();
    let plan = s
        .mode_entry(
            "app".into(),
            Some("image-transfer".into()),
            "shooting/stills".into(),
        )
        .expect("from-image-transfer live-view entry");
    let steps = ptp_steps(&plan);
    assert!(matches!(
        steps[0],
        EntryStep::ReopenSession { tolerant: false }
    ));
    assert!(steps.iter().any(|st| matches!(
        st,
        EntryStep::SetProp {
            prop: 0xdf01,
            value: 0x16,
            ..
        }
    )));
    assert!(steps.iter().any(|st| matches!(
        st,
        EntryStep::SetProp {
            prop: 0xdf2a,
            value: 2,
            ..
        }
    )));
    assert!(steps.iter().any(|st| matches!(
        st,
        EntryStep::SendOp {
            op: 0x902b,
            repeat: 4,
            ..
        }
    )));
    assert!(matches!(
        &steps[steps.len() - 3..],
        [
            EntryStep::OpenChannel {
                role: SocketRole::Event,
                ..
            },
            EntryStep::OpenChannel {
                role: SocketRole::LiveView,
                ..
            },
            EntryStep::SendOp { op: 0x101c, .. }
        ]
    ));
    assert!(
        !steps
            .iter()
            .any(|st| matches!(st, EntryStep::SendOp { op: 0x1018, .. })),
        "Get→Take must not terminate a non-existent live-view stream"
    );
}

#[test]
fn d246_stills_video_edges_surface_through_ffi() {
    let s = store();
    let to_video = s
        .mode_entry(
            "app".into(),
            Some("shooting/stills".into()),
            "shooting/video".into(),
        )
        .expect("stills→video selector");
    let to_video_steps = ptp_steps(&to_video);
    assert_eq!(to_video_steps.len(), 1);
    assert!(matches!(
        to_video_steps[0],
        EntryStep::SetProp {
            prop: 0xd246,
            value: 1,
            tolerant: false,
        }
    ));

    let to_stills = s
        .mode_entry(
            "app".into(),
            Some("shooting/video".into()),
            "shooting/stills".into(),
        )
        .expect("video→stills selector");
    let to_stills_steps = ptp_steps(&to_stills);
    assert_eq!(to_stills_steps.len(), 1);
    assert!(matches!(
        to_stills_steps[0],
        EntryStep::SetProp {
            prop: 0xd246,
            value: 0,
            tolerant: false,
        }
    ));
}

#[test]
fn read_device_info_action_pairs_with_the_device_info_codec() {
    // The #173 seam: opcode from the manifest action, layout from the FFI
    // codec — the app spells neither.
    let s = store();
    let read = s
        .action("wireless-tether".into(), ActionVerb::ReadDeviceInfo)
        .expect("wireless-tether.actions.readDeviceInfo");
    assert_eq!(read.mode, "", "identity read is not mode-gated");
    assert!(matches!(
        read.initiator.as_ref().unwrap().steps[0],
        EntryStep::SendOp { op: 0x1001, .. }
    ));
    // NOT authored on `app`: the reference app never sends 0x1001 on the reference app
    // channel (v6 wire-level run: zero 0x1001 frames) — evidence-deferred.
    assert!(s.action("app".into(), ActionVerb::ReadDeviceInfo).is_none());

    // Codec half: a real DeviceInfo dataset round-trips, serial included.
    let di = ptp_core::DeviceInfo {
        standard_version: 100,
        manufacturer: "FUJIFILM".into(),
        model: "GFX100 II".into(),
        device_version: "2.30".into(),
        serial_number: "PTPSIM-GFX100II-0001".into(),
        operations_supported: vec![0x1001, 0x1002],
        ..Default::default()
    };
    let mut w = Writer::new();
    di.encode(&mut w).unwrap();
    let parsed = parse_device_info(w.into_vec()).unwrap();
    assert_eq!(parsed.serial_number, "PTPSIM-GFX100II-0001");
    assert_eq!(parsed.model, "GFX100 II");
    assert_eq!(parsed.operations_supported, vec![0x1001, 0x1002]);
}

#[test]
fn pcss_live_view_verbs_are_exact_and_preserve_connection_specific_shapes() {
    let s = store();
    assert_eq!(
        parse_action_verb("startLiveView".into()),
        Some(ActionVerb::StartLiveView)
    );
    assert_eq!(
        parse_action_verb("pollLiveView".into()),
        Some(ActionVerb::PollLiveView)
    );
    assert_eq!(
        parse_action_verb("stopLiveView".into()),
        Some(ActionVerb::StopLiveView)
    );
    assert_eq!(parse_action_verb("start-live-view".into()), None);
    assert_eq!(parse_action_verb("poll-live-view".into()), None);
    assert_eq!(parse_action_verb("stop-live-view".into()), None);
    assert_eq!(parse_action_verb("StartLiveView".into()), None);
    assert_eq!(parse_action_verb("PollLiveView".into()), None);
    assert_eq!(parse_action_verb("StopLiveView".into()), None);

    let start = s
        .action("wireless-tether".into(), ActionVerb::StartLiveView)
        .expect("wireless-tether.actions.startLiveView");
    assert_eq!(start.mode, "shooting/stills");
    assert!(start.initiator.as_ref().unwrap().params.is_empty());
    assert!(matches!(
        start.initiator.as_ref().unwrap().steps.as_slice(),
        [
            EntryStep::Retry {
                steps: terminate_steps,
                when_response_codes: terminate_response_codes,
                when_failure_classes: terminate_failure_classes,
                max_attempts: 10,
                retry_delay_ms: 300,
                tolerant: true,
            },
            EntryStep::SetProp {
                prop: 0xd1bc,
                value: 2,
                tolerant: false,
            },
            EntryStep::SendOp {
                op: 0x101c,
                params,
                captures,
                repeat: 1,
                tolerant: false,
            }
        ] if terminate_response_codes.as_slice() == [0x2019]
            && terminate_failure_classes.is_empty()
            && matches!(
                terminate_steps.as_slice(),
                [EntryStep::SendOp {
                    op: 0x1018,
                    params: terminate_params,
                    captures: terminate_captures,
                    repeat: 1,
                    tolerant: false,
                }] if matches!(
                    terminate_params.as_slice(),
                    [EntryParam::Literal { value: 1 }]
                ) && terminate_captures.is_empty()
            )
            && matches!(
            params.as_slice(),
            [
                EntryParam::Literal { value: 0 },
                EntryParam::Literal { value: 0 }
            ]
        ) && captures.is_empty()
    ));

    let poll = s
        .action("wireless-tether".into(), ActionVerb::PollLiveView)
        .expect("wireless-tether.actions.pollLiveView");
    assert_eq!(poll.mode, "shooting/stills");
    assert!(poll.initiator.as_ref().unwrap().params.is_empty());
    assert!(matches!(
        poll.initiator.as_ref().unwrap().steps.as_slice(),
        [EntryStep::Retry {
            steps,
            when_response_codes,
            when_failure_classes,
            max_attempts: 10,
            retry_delay_ms: 100,
            tolerant: false,
        }] if when_response_codes.as_slice() == [0x2002]
            && when_failure_classes.is_empty()
            && matches!(
                steps.as_slice(),
                [EntryStep::SendOp {
                    op: 0x9018,
                    params,
                    captures,
                    repeat: 1,
                    tolerant: false,
                }] if params.is_empty() && captures.is_empty()
            )
    ));

    let stop = s
        .action("wireless-tether".into(), ActionVerb::StopLiveView)
        .expect("wireless-tether.actions.stopLiveView");
    assert_eq!(stop.mode, "shooting/stills");
    assert!(stop.initiator.as_ref().unwrap().params.is_empty());
    assert!(matches!(
        stop.initiator.as_ref().unwrap().steps.as_slice(),
        [EntryStep::SendOp {
            op: 0x1018,
            params,
            captures,
            repeat: 1,
            tolerant: false,
        }] if matches!(
            params.as_slice(),
            [EntryParam::Literal { value: 1 }]
        ) && captures.is_empty()
    ));

    let enumerate = s
        .action("wireless-tether".into(), ActionVerb::EnumerateObjects)
        .expect("wireless-tether.actions.enumerateObjects");
    assert_eq!(enumerate.mode, "");
    assert!(matches!(
        enumerate.initiator.as_ref().unwrap().steps.as_slice(),
        [EntryStep::SendOp {
            op: 0x1007,
            params,
            captures,
            repeat: 1,
            tolerant: false,
        }] if matches!(
            params.as_slice(),
            [
                EntryParam::Literal { value: 0xffff_ffff },
                EntryParam::Literal { value: 0 },
            ]
        ) && matches!(
            captures.as_slice(),
            [CaptureInfo {
                bind,
                source: CaptureSourceInfo::PtpU32Array,
            }] if bind == "objectHandles"
        )
    ));

    for verb in [
        ActionVerb::StartLiveView,
        ActionVerb::PollLiveView,
        ActionVerb::StopLiveView,
    ] {
        assert!(s.action("app".into(), verb).is_none());
    }
}

#[test]
fn action_returns_pcss_shutter_with_objects_available_trigger() {
    // wireless-tether shutter — the wire-confirmed 3-beat virtual-shutter
    // (setProp 0xD039 phases + sendOp 0x100E). triggers: [ObjectsAvailable{1,3}]
    // because PCSS exposes 1-3 queued objects depending on user's JPEG/HEIF/RAW.
    let s = store();
    let shutter = s
        .action("wireless-tether".into(), ActionVerb::Shutter)
        .expect("wireless-tether.actions.shutter");
    assert_eq!(shutter.mode, "shooting/stills");
    assert!(shutter.initiator.as_ref().unwrap().params.is_empty());
    assert_eq!(shutter.initiator.as_ref().unwrap().steps.len(), 6); // 3 beats × 2 ops each
                                                                    // Trigger surfaces as a tagged enum with min/max payload.
    assert_eq!(shutter.triggers.len(), 1);
    assert!(
        matches!(
            shutter.triggers[0],
            ActionEffect::ObjectsAvailable { min: 1, max: 3 }
        ),
        "expected ObjectsAvailable{{1,3}}, got {:?}",
        shutter.triggers[0]
    );
}

#[test]
fn action_returns_pcss_keepalive_recipe() {
    let s = store();
    let keepalive = s
        .action("wireless-tether".into(), ActionVerb::Keepalive)
        .expect("wireless-tether.actions.keepalive");
    assert_eq!(keepalive.mode, "");
    assert!(keepalive.initiator.as_ref().unwrap().params.is_empty());
    assert!(keepalive.triggers.is_empty());
    assert_eq!(keepalive.initiator.as_ref().unwrap().steps.len(), 1);
    assert!(matches!(
        keepalive.initiator.as_ref().unwrap().steps[0],
        EntryStep::SetProp {
            prop: 0xd21c,
            value: 0,
            ..
        }
    ));
}

#[test]
fn action_returns_app_shutter_with_postview_event_trigger() {
    // Same verb, different connection — the reference app shutter take cycle (#29):
    // 0x100E → awaitUntil the 0xC001 PostviewComplete event → 0x9022 read.
    let s = store();
    let shutter = s
        .action("app".into(), ActionVerb::Shutter)
        .expect("app.actions.shutter");
    assert_eq!(shutter.initiator.as_ref().unwrap().steps.len(), 3);
    assert!(matches!(
        shutter.initiator.as_ref().unwrap().steps[0],
        EntryStep::SendOp { op: 0x100e, .. }
    ));
    // The postview await surfaces as an event-source AwaitUntil; a dropped step
    // would silently break the manifest-scripted take cycle.
    assert!(matches!(
        &shutter.initiator.as_ref().unwrap().steps[1],
        EntryStep::AwaitUntil {
            source: FfiAwaitSource::Event {
                code: 0xc001,
                then_poll: None
            },
            ..
        }
    ));
    assert!(matches!(
        shutter.initiator.as_ref().unwrap().steps[2],
        EntryStep::SendOp { op: 0x9022, .. }
    ));
    assert!(matches!(shutter.triggers[0], ActionEffect::PostviewEvent));
}

#[test]
fn action_getobject_params_differ_per_connection_same_verb() {
    // PCSS getObject: whole-object 0x1009 with params: [handle].
    // reference app getObject: chunked 0x101B with params: [handle, offset, length].
    // Consumer reads .params at the call site to know what to bind.
    let s = store();
    let pcss = s
        .action("wireless-tether".into(), ActionVerb::GetObject)
        .expect("wireless-tether.actions.getObject");
    let app = s
        .action("app".into(), ActionVerb::GetObject)
        .expect("app.actions.getObject");
    assert_eq!(
        pcss.initiator.as_ref().unwrap().params,
        vec!["handle".to_string()]
    );
    assert_eq!(
        app.initiator.as_ref().unwrap().params,
        vec![
            "handle".to_string(),
            "offset".to_string(),
            "length".to_string()
        ]
    );
    assert!(matches!(
        pcss.initiator.as_ref().unwrap().steps[0],
        EntryStep::SendOp {
            op: 0x1009,
            ref params,
            ..
        } if params.len() == 1
    ));
    assert!(matches!(
        app.initiator.as_ref().unwrap().steps[0],
        EntryStep::SendOp {
            op: 0x101b,
            ref params,
            ..
        } if matches!(
            params.as_slice(),
            [
                EntryParam::Runtime { slot: h, shift: 0, mask: None },
                EntryParam::Runtime { slot: o1, shift: 0, mask: Some(0xffff_ffff) },
                EntryParam::Runtime { slot: l, shift: 0, mask: None },
                EntryParam::Runtime { slot: o2, shift: 32, mask: None },
            ] if h == "handle" && o1 == "offset" && l == "length" && o2 == "offset"
        )
    ));
}

#[test]
fn action_import_objects_surfaces_the_nested_transfer_loop() {
    // #46/#187: the full image-transfer choreography surfaces as ONE action whose
    // steps nest a forEach(handle) loop over true-size capture + chunk download.
    // The hand-written FFI mirror must not silently drop captures, conditionals,
    // runtime chunk size, or shifted params.
    let s = store();
    let plan = s
        .action("app".into(), ActionVerb::ImportObjects)
        .expect("app.actions.importObjects");
    assert_eq!(plan.mode, "image-transfer");
    let bootstrap = plan
        .initiator
        .as_ref()
        .unwrap()
        .steps
        .iter()
        .find_map(|step| match step {
            EntryStep::Retry { steps, .. }
                if steps
                    .iter()
                    .any(|nested| matches!(nested, EntryStep::GetProp { prop: 0xd22b, .. })) =>
            {
                Some(steps.as_slice())
            }
            _ => None,
        })
        .expect("importObjects reuses the enumeration-prime retry");
    assert_bootstrap_tail_surfaces(bootstrap);

    assert!(plan.initiator.as_ref().unwrap().steps.iter().any(|step| matches!(
        step,
        EntryStep::Retry { steps, .. }
            if matches!(steps.as_slice(), [EntryStep::GetProp { prop: 0xd621, captures, .. }]
                if matches!(captures.as_slice(), [CaptureInfo { bind, source: CaptureSourceInfo::PtpU32Array }]
                    if bind == "objectHandles"))
    )));

    // The forEach iterates the captured handle list, binding `handle`.
    let for_each = plan
        .initiator
        .as_ref()
        .unwrap()
        .steps
        .iter()
        .find_map(|st| match st {
            EntryStep::Loop {
                kind:
                    FfiLoopKind::ForEach {
                        collection,
                        bind,
                        body,
                    },
                ..
            } => Some((collection.as_str(), bind.as_str(), body)),
            _ => None,
        })
        .expect("importObjects nests a forEach over the handle list");
    assert_eq!(for_each.0, "objectHandles");
    assert_eq!(for_each.1, "handle");

    assert!(for_each.2.iter().any(|st| matches!(
        st,
        EntryStep::SendOp {
            op: 0x1008,
            captures,
            ..
        } if captures.iter().any(|c| c.bind == "objectReportedSize"
            && matches!(c.source, CaptureSourceInfo::ObjectInfoCompressedSize))
            && captures.iter().any(|c| c.bind == "objectTransferSize"
            && matches!(c.source, CaptureSourceInfo::ObjectInfoCompressedSize))
    )));
    assert!(for_each.2.iter().any(|st| matches!(
        st,
        EntryStep::If {
            slot,
            equals: 0xffff_ffff,
            then_steps,
            ..
        } if slot == "objectReportedSize"
            && matches!(then_steps.as_slice(), [EntryStep::SendOp { op: 0x9803, captures, .. }]
                if captures.iter().any(|c| c.bind == "objectTransferSize"
                    && matches!(c.source, CaptureSourceInfo::U64Le)))
    )));
    assert!(for_each.2.iter().any(|st| matches!(
        st,
        EntryStep::GetProp {
            prop: 0xd235,
            captures,
            ..
        } if captures.iter().any(|c| c.bind == "chunkSize"
            && matches!(c.source, CaptureSourceInfo::PropValue))
    )));
    let chunk = for_each
        .2
        .iter()
        .find_map(|st| match st {
            EntryStep::Loop {
                kind:
                    FfiLoopKind::Chunk {
                        total, size, body, ..
                    },
                ..
            } => Some((total.as_str(), size, body)),
            _ => None,
        })
        .expect("forEach body nests a chunk loop");
    assert_eq!(
        chunk.0, "objectTransferSize",
        "chunk total names the true transfer size slot"
    );
    assert!(
        matches!(chunk.1, FfiChunkSize::Runtime { slot } if slot == "chunkSize"),
        "chunk size comes from the captured 0xd235 property",
    );
    assert!(
        matches!(chunk.2.as_slice(), [EntryStep::SendOp { op: 0x101b, params, .. }]
            if matches!(params.as_slice(), [
                EntryParam::Runtime { slot: h, shift: 0, mask: None },
                EntryParam::Runtime { slot: o1, shift: 0, mask: Some(0xffff_ffff) },
                EntryParam::Runtime { slot: l, shift: 0, mask: None },
                EntryParam::Runtime { slot: o2, shift: 32, mask: None },
            ] if h == "handle" && o1 == "offset" && l == "length" && o2 == "offset")),
        "chunk body is the GetPartialObject download",
    );
}

#[test]
fn enumerate_objects_surfaces_response_selected_retries() {
    let plan = store()
        .action("app".into(), ActionVerb::EnumerateObjects)
        .expect("app.actions.enumerateObjects");
    assert_eq!(plan.initiator.as_ref().unwrap().steps.len(), 3);
    let EntryStep::Retry {
        steps,
        when_response_codes,
        when_failure_classes,
        max_attempts,
        retry_delay_ms,
        tolerant,
    } = &plan.initiator.as_ref().unwrap().steps[0]
    else {
        panic!("expected enumeration-prime retry")
    };
    assert_eq!(when_response_codes, &[0x2013, 0x2019]);
    assert_eq!(when_failure_classes, &[FfiRetryFailureClass::Decode]);
    assert_eq!(*max_attempts, 5);
    assert_eq!(*retry_delay_ms, 100);
    assert!(!tolerant);
    assert_bootstrap_tail_surfaces(steps);

    for (step, prop) in plan.initiator.as_ref().unwrap().steps[1..]
        .iter()
        .zip([0xd620, 0xd621])
    {
        let EntryStep::Retry {
            steps,
            when_response_codes,
            when_failure_classes,
            max_attempts,
            retry_delay_ms,
            ..
        } = step
        else {
            panic!("expected property retry")
        };
        assert_eq!(when_response_codes, &[0x2002, 0x2013, 0x2019]);
        assert_eq!(when_failure_classes, &[FfiRetryFailureClass::Decode]);
        assert_eq!(*max_attempts, 3);
        assert_eq!(*retry_delay_ms, 1000);
        assert!(
            matches!(steps.as_slice(), [EntryStep::GetProp { prop: actual, .. }] if *actual == prop)
        );
    }
}

#[test]
fn selected_object_transfer_projects_the_canonical_per_handle_contract() {
    let s = store();
    let selected = s
        .selected_object_transfer("app".into())
        .expect("valid selected-object transfer contract")
        .expect("app selected-object transfer contract");

    assert_eq!(selected.params, ["handle"]);
    assert_eq!(selected.object_info_step_index, 0);
    assert_eq!(selected.transfer_size_slot, "objectTransferSize");
    assert_eq!(selected.chunk_size_slot, "chunkSize");
    assert!(selected.preparation_steps.iter().any(|step| matches!(
        step,
        EntryStep::SendOp { captures, .. }
            if captures.iter().any(|capture|
                capture.bind == selected.transfer_size_slot
                    && matches!(capture.source, CaptureSourceInfo::ObjectInfoCompressedSize))
    )));
    assert!(selected.preparation_steps.iter().any(|step| matches!(
        step,
        EntryStep::If { equals: 0xffff_ffff, then_steps, .. }
            if matches!(then_steps.as_slice(), [EntryStep::SendOp { captures, .. }]
                if captures.iter().any(|capture|
                    capture.bind == selected.transfer_size_slot
                        && matches!(capture.source, CaptureSourceInfo::U64Le)))
    )));
    assert!(selected.preparation_steps.iter().any(|step| matches!(
        step,
        EntryStep::GetProp { captures, .. }
            if captures.iter().any(|capture|
                capture.bind == selected.chunk_size_slot
                    && matches!(capture.source, CaptureSourceInfo::PropValue))
    )));
    assert!(selected.preparation_steps.iter().all(|step| !matches!(
        step,
        EntryStep::Loop {
            kind: FfiLoopKind::Chunk { .. },
            ..
        }
    )));

    assert_eq!(
        selected.read.initiator.as_ref().unwrap().params,
        ["handle", "offset", "length"]
    );
    assert!(matches!(
        selected.read.initiator.as_ref().unwrap().steps.as_slice(),
        [EntryStep::SendOp { params, .. }]
            if matches!(params.as_slice(), [
                EntryParam::Runtime { slot: h, shift: 0, mask: None },
                EntryParam::Runtime { slot: lo, shift: 0, mask: Some(0xffff_ffff) },
                EntryParam::Runtime { slot: len, shift: 0, mask: None },
                EntryParam::Runtime { slot: hi, shift: 32, mask: None },
            ] if h == "handle" && lo == "offset" && len == "length" && hi == "offset")
    ));
}

#[test]
fn selected_object_transfer_is_absent_without_the_canonical_actions() {
    let s = store();
    assert!(s
        .selected_object_transfer("ble".into())
        .expect("ble lookup")
        .is_none());
    assert!(s
        .selected_object_transfer("wireless-tether".into())
        .expect("wireless-tether lookup")
        .is_none());
    assert!(s
        .selected_object_transfer("missing".into())
        .expect("missing lookup")
        .is_none());
}

#[test]
fn action_misses_when_connection_does_not_declare_the_verb() {
    let s = store();
    // ble has no transfer actions.
    assert!(s.action("ble".into(), ActionVerb::GetObject).is_none());
    // app does NOT model DeleteObject (no wire-truth for reference app).
    assert!(s.action("app".into(), ActionVerb::DeleteObject).is_none());
    // Unknown connection.
    assert!(s
        .action("nonexistent".into(), ActionVerb::Shutter)
        .is_none());
}

#[test]
fn runtime_param_slot_surfaces_through_ffi() {
    let s = store();
    // The from-live-view image-transfer entry: 0x1018 carries a runtime slot the app binds.
    let plan = s
        .mode_entry(
            "app".into(),
            Some("shooting/stills".into()),
            "image-transfer".into(),
        )
        .expect("from-Stills image-import entry");
    let ModeEntryExecution::ReestablishConnection { exit_steps, .. } = &plan.execution else {
        panic!("expected re-establishment, got {:?}", plan.execution);
    };
    match &exit_steps[0] {
        EntryStep::SendOp { op, params, .. } => {
            assert_eq!(*op, 0x1018);
            assert!(matches!(
                &params[0],
                EntryParam::Runtime {
                    slot,
                    shift: 0,
                    mask: None,
                } if slot == "openCaptureTxId"
            ));
        }
        other => panic!("expected SendOp 0x1018, got {other:?}"),
    }
    assert!(exit_steps.iter().all(|step| !matches!(
        step,
        EntryStep::SendOp {
            op: 0x9050 | 0x9053,
            ..
        }
    )));
}

#[test]
fn from_tiers_applies_fw_overlay_through_ffi() {
    let s = ConfigStore::from_tiers(
        data("fuji/gfx100ii/gfx100ii.yaml"),
        Some(data("fuji/fuji.yaml")),
        vec![data("fuji/gfx100ii/fw2.40.yaml")],
    )
    .expect("tiered bundle loads");
    // The fw2.40 overlay applied: XLV connection still present (HTTPS is in extra),
    // and the rest of the seam still answers. Smoke that the merged store works.
    let xlv = &s.connections(Platform::Macos);
    assert!(xlv.iter().any(|c| c.id == "xlv"));
    // Manufacturer tier still resolves through the tiered constructor.
    assert!(matches!(
        s.value("initiatorGuid".into()),
        Some(ResolvedValue::Fixed { .. })
    ));
}

#[test]
fn detect_mode_from_observed_function_mode() {
    let s = store();
    let obs = vec![PropObservation {
        code: 0xdf01,
        value: 0x16,
    }];
    assert_eq!(
        s.detect_mode("app".into(), obs).as_deref(),
        Some("shooting/stills")
    );
}

#[test]
fn client_derived_friendly_name_defers_the_init_packet() {
    // #109: the PTP/IP friendly name is the host's OWN device name — client-derived
    // from the `terminalName` slot, identical to the BLE deviceNameString — NOT a
    // manifest literal. So the manifest exposes the policy (not a value), and
    // connection_init declines to bake the 82-byte packet: the consumer must fill
    // the name from session state (the adoption is #29). Packet-shape coverage lives
    // in protocol_primitives::build_app_init's own tests.
    let s = store();

    // The friendly name resolves to a client-derived runtime slot, not a literal.
    match s.value("initFriendlyName".into()) {
        Some(ResolvedValue::ClientDerived { runtime }) => assert_eq!(runtime, "terminalName"),
        other => panic!("expected client-derived initFriendlyName, got {other:?}"),
    }
    // The GUID is still a fixed manufacturer constant (correctly modeled).
    assert!(matches!(
        s.value("initiatorGuid".into()),
        Some(ResolvedValue::Fixed { .. })
    ));

    // Without a host-supplied name, the app init can't be assembled → None (was the
    // 82-byte packet before #109). usb declares no init shape → None.
    assert!(s.connection_init("app".into()).is_none());
    assert!(s.connection_init("usb".into()).is_none());

    let init = s
        .connection_init_with_runtime(
            "app".into(),
            vec![KeyValue {
                key: "terminalName".into(),
                value: "abcdefghijklmnopqr".into(),
            }],
        )
        .expect("runtime terminalName assembles app init packet");
    assert_eq!(init.friendly_name, "abcdefghijklmnopqr");
    assert_eq!(init.name_field_byte_count, 54);
    assert_eq!(
        init.guid,
        vec![
            0xf2, 0xe4, 0x53, 0x8f, 0xad, 0xa5, 0x48, 0x5d, 0x87, 0xb2, 0x7f, 0x0b, 0xd3, 0xd5,
            0xde, 0xd0
        ]
    );
    assert_eq!(init.packet.len(), 82);
    let encoded_name = "abcdefghijklmnopqr"
        .encode_utf16()
        .flat_map(u16::to_le_bytes)
        .collect::<Vec<_>>();
    assert_eq!(&init.packet[28..64], encoded_name);
    assert_eq!(&init.packet[64..82], &[0; 18]);
    assert!(s
        .connection_init_with_runtime("app".into(), vec![])
        .is_none());
    assert!(s
        .connection_init_with_runtime(
            "wireless-tether".into(),
            vec![KeyValue {
                key: "terminalName".into(),
                value: "iphone".into(),
            }],
        )
        .is_none());
    let pcss_identity = s
        .connection_init_identity_with_runtime(
            "wireless-tether".into(),
            vec![KeyValue {
                key: "terminalName".into(),
                value: "iphone".into(),
            }],
        )
        .expect("PCSS identity resolves through the common policy");
    assert_eq!(pcss_identity.guid, init.guid);
    assert_eq!(pcss_identity.friendly_name, "iphone");
}

#[test]
fn normalize_client_name_produces_the_terminal_name_for_the_init() {
    // #139: the host normalizes its raw device name once; the result is the single
    // `terminalName` value driving BOTH the BLE deviceNameString and the PTP/IP
    // friendly name, so the two channels agree (#109) with no name logic in Swift.
    let name = normalize_client_name(" Eric's iPad Pro ".into());
    assert_eq!(name, "eric-s-ipad-pro");
    // Fits the 54-byte UTF-16LE name field (chars * 2 + 2-byte NUL).
    assert!(name.chars().count() * 2 + 2 <= 54);

    let s = store();
    let init = s
        .connection_init_with_runtime(
            "app".into(),
            vec![KeyValue {
                key: "terminalName".into(),
                value: name.clone(),
            }],
        )
        .expect("the normalized name assembles the app init packet");
    assert_eq!(init.friendly_name, name);
    assert_eq!(init.packet.len(), 82);
}

#[test]
fn connection_info_carries_per_connection_traits() {
    let s = store();
    let conns = s.connections(Platform::Macos); // app + wireless-tether both visible
    let app = conns.iter().find(|c| c.id == "app").expect("app present");
    assert!(app.command_listener_volatile);
    assert_eq!(app.init_shape.as_deref(), Some("app82"));
    assert!(matches!(
        app.shutter_recipe,
        Some(ShutterRecipe::AppPostview)
    ));
    assert!(matches!(
        app.live_view_delivery.as_ref().map(|d| &d.kind),
        Some(LiveViewDeliveryKind::Stream)
    ));

    let wt = conns
        .iter()
        .find(|c| c.id == "wireless-tether")
        .expect("tether present");
    let lv = wt
        .live_view_delivery
        .as_ref()
        .expect("tether polls live view");
    assert!(matches!(lv.kind, LiveViewDeliveryKind::Poll));
    assert_eq!(lv.poll_op, Some(0x9018)); // hex string → u16 across the FFI
    assert!(matches!(
        wt.shutter_recipe,
        Some(ShutterRecipe::WirelessTether3Beat)
    ));
    assert!(!wt.command_listener_volatile);

    // usb omits the listener trait, proving the schema default remains false.
    let usb = conns.iter().find(|c| c.id == "usb").expect("usb on macOS");
    assert!(!usb.command_listener_volatile);
    assert!(usb.shutter_recipe.is_none());
    assert!(usb.live_view_delivery.is_none());
}

#[test]
fn autofocus_actions_cross_the_hand_written_ffi_seam() {
    let s = store();
    let lock = s
        .action("app".into(), ActionVerb::AutofocusLock)
        .expect("app autofocusLock action");
    assert_eq!(
        lock.initiator.as_ref().unwrap().params,
        vec!["afArea".to_string()]
    );
    assert!(matches!(
        lock.initiator.as_ref().unwrap().steps[0],
        EntryStep::SendOp { op: 0x9026, .. }
    ));
    // The AF await surfaces as an event-source AwaitUntil (event 0xC005 → read 0xD209).
    assert!(matches!(
        &lock.initiator.as_ref().unwrap().steps[1],
        EntryStep::AwaitUntil {
            source: FfiAwaitSource::Event {
                code: 0xc005,
                then_poll: Some(0xd209)
            },
            ..
        }
    ));
    let release = s
        .action("app".into(), ActionVerb::AutofocusRelease)
        .expect("app autofocusRelease action");
    assert!(matches!(
        release.initiator.as_ref().unwrap().steps[0],
        EntryStep::SendOp { op: 0x9027, .. }
    ));
    let pcss_lock = s
        .action("wireless-tether".into(), ActionVerb::AutofocusLock)
        .expect("wireless-tether autofocusLock action");
    let pcss_initiator = pcss_lock.initiator.as_ref().expect("PCSS lock initiator");
    assert_eq!(pcss_initiator.params.len(), 1);
    assert_eq!(pcss_initiator.params[0].name, "focusArea");
    assert!(matches!(
        pcss_initiator.params[0].kind,
        ActionCatalogParameterKind::String
    ));
    assert!(!pcss_initiator.params[0].required);
    assert!(matches!(
        &pcss_initiator.steps[0],
        EntryStep::SetPropRuntime {
            prop: 0xd395,
            slot,
            if_missing: FfiMissingRuntimeValue::Skip,
            ..
        } if slot == "focusArea"
    ));
    assert!(matches!(
        &pcss_initiator.steps[4],
        EntryStep::AwaitUntil {
            source: FfiAwaitSource::Poll { prop: 0xd209 },
            captures,
            timeout_ms: 3000,
            interval_ms: 25,
            ..
        } if captures.len() == 1
            && captures[0].bind == "autofocusResult"
            && matches!(captures[0].source, CaptureSourceInfo::PropValue)
    ));
    assert!(matches!(
        pcss_lock.responder.expect("PCSS lock responder").mutation,
        ResponderMutation::PropertyTransition {
            target: 0xd209,
            initial: Some(1),
            terminal: PropertyTransitionTerminal::Parameter { ref parameter },
            settle_after_polls: 2,
        } if parameter == "result"
    ));

    let pcss_release = s
        .action("wireless-tether".into(), ActionVerb::AutofocusRelease)
        .expect("wireless-tether autofocusRelease action");
    assert!(matches!(
        pcss_release
            .responder
            .expect("PCSS release responder")
            .mutation,
        ResponderMutation::PropertyTransition {
            target: 0xd209,
            initial: None,
            terminal: PropertyTransitionTerminal::Fixed { value: 4 },
            settle_after_polls: 0,
        }
    ));

    let properties = s.properties();
    let d208 = properties
        .iter()
        .find(|property| property.code == 0xd208)
        .expect("D208 crosses FFI");
    assert_eq!(d208.name, "pcssCaptureFunction");
    assert_eq!(d208.kind, PropertyKind::Scaffold);
    assert!(d208
        .value_rows
        .iter()
        .any(|row| row.raw == 0xa000 && row.label == "instantAf"));
    let d230 = properties
        .iter()
        .find(|property| property.code == 0xd230)
        .expect("D230 crosses FFI");
    assert_eq!(d230.name, "pcssForceMode");
    assert_eq!(d230.kind, PropertyKind::Scaffold);
    assert_eq!(d230.value_rows.len(), 1);
    assert_eq!(d230.value_rows[0].raw, 1);
}

#[test]
fn camera_identity_surfaces_manifest_identity() {
    let s = store();
    let identity = s.camera_identity();
    assert_eq!(identity.manufacturer, "FUJIFILM");
    assert_eq!(identity.model, "GFX100 II");
    assert_eq!(identity.firmware, "2.30");
    assert!(identity
        .identities
        .iter()
        .any(|kv| kv.key == "ptpDeviceName" && kv.value == "GFX100 II"));
}

#[test]
fn property_catalog_enumerates_through_ffi() {
    let s = store();
    let cat = s.properties();
    assert!(
        cat.len() > 20,
        "the catalog enumerates many props: {}",
        cat.len()
    );
    // A representative scalar property surfaces with its metadata.
    let aperture = cat
        .iter()
        .find(|p| p.code == 0x5007)
        .expect("aperture in the catalog");
    assert_eq!(aperture.name, "aperture");
    assert_eq!(aperture.ptp_name.as_deref(), Some("FNumber"));
    assert_eq!(aperture.ptype.as_deref(), Some("u16"));
    assert_eq!(aperture.access.as_deref(), Some("readWrite"));
    assert_eq!(aperture.kind, PropertyKind::Setting);
    assert!(aperture
        .evidence
        .iter()
        .any(|evidence| evidence == "iosLiveControls"));
    for code in [0xd039, 0xd1bc, 0xd208, 0xd21c, 0xd230, 0xd207] {
        let property = cat
            .iter()
            .find(|property| property.code == code)
            .unwrap_or_else(|| panic!("scaffold property 0x{code:04x} in the catalog"));
        assert_eq!(property.kind, PropertyKind::Scaffold);
    }
    let pcss_live_view_selector = cat
        .iter()
        .find(|property| property.code == 0xd1bc)
        .expect("PCSS live-view selector in the catalog");
    assert_eq!(pcss_live_view_selector.name, "pcssLiveViewSelector");
    assert_eq!(pcss_live_view_selector.ptype.as_deref(), Some("u16"));
    assert_eq!(pcss_live_view_selector.access.as_deref(), Some("readWrite"));
    // The newly-declared 0xD212 member is enumerable.
    assert!(cat
        .iter()
        .any(|p| p.code == 0xd028 && p.name == "depthOfField"));
    let chunk_size = cat
        .iter()
        .find(|p| p.code == 0xd235)
        .expect("reference app import chunk-size property in the catalog");
    assert_eq!(chunk_size.ptype.as_deref(), Some("u32"));
    assert_eq!(chunk_size.initial_value, Some(0x00bf_ffe0));
}

#[test]
fn operation_and_property_catalog_safety_crosses_the_ffi_seam() {
    let store = consolidated_store();
    let operations = store.operations();
    assert!(operations.len() >= 20);

    let generated = operations
        .iter()
        .find(|operation| operation.code == 0x1001)
        .expect("generated GetDeviceInfo inventory row");
    assert_eq!(generated.name, "GetDeviceInfo");
    assert_eq!(generated.kind, OperationKind::AdvertisedOnly);
    assert!(generated
        .observed_scopes
        .iter()
        .any(|scope| scope.connection == "usb"
            && scope.mode == "shooting/stills"
            && scope.state == "descriptor-enumeration"));
    assert_eq!(generated.evidence, ["canonicalObservation"]);
    assert!(matches!(
        store.operation_available("usb".into(), "shooting/stills".into(), 0x1001, vec![]),
        Availability::Unavailable
    ));

    let authored = operations
        .iter()
        .find(|operation| operation.code == 0x1002)
        .expect("authored OpenSession row");
    assert_eq!(authored.kind, OperationKind::Executable);

    let properties = store.properties();
    let catalog_only = properties
        .iter()
        .find(|property| property.code == 0x5001)
        .expect("generated BatteryLevel property");
    assert_eq!(catalog_only.kind, PropertyKind::CatalogOnly);
    assert_eq!(catalog_only.name, "BatteryLevel");
    assert_eq!(catalog_only.ptp_name, None);
    assert_eq!(catalog_only.descriptor_form.as_deref(), Some("range"));
    assert_eq!(
        catalog_only.descriptor_source,
        Some(DescriptorSource::Camera)
    );
    assert!(catalog_only
        .observed_scopes
        .iter()
        .any(|scope| scope.connection == "usb"));
    assert_eq!(catalog_only.evidence, ["canonicalObservation"]);
}

#[test]
fn focus_grid_and_af_packing_are_data_driven() {
    let s = store();
    // #135: the AF grid is manifest data, not a Swift constant.
    let grid = s.focus_grid().expect("gfx100ii declares a focus grid");
    assert_eq!((grid.columns, grid.rows), (9, 6));
    // The app packs a live-view tap into the 0x9026 param using the manifest grid.
    // A tap in cell (5,4) with default 4:3 aspect is the wire-confirmed 0x04030504.
    assert_eq!(
        pack_af_area(0.45, 0.5, grid.columns, grid.rows, None),
        0x0403_0504
    );
    // A prior 0xD17C lock carries its aspect forward into the next pack.
    assert_eq!(
        pack_af_area(0.0, 0.0, grid.columns, grid.rows, Some(0x1009_0101)) >> 16,
        0x1009
    );
}

#[test]
fn media_format_table_classifies_objects_through_ffi() {
    let s = store();
    let raf = s.media_format(0xb103).expect("RAF in the format table");
    assert_eq!(raf.name, "raf");
    assert_eq!(raf.vendor.as_deref(), Some("fuji"));
    assert!(raf.is_raw && !raf.is_movie);
    // #101: the embedded-JPEG locator reaches the app through the FFI, so it can
    // GetPartialObject the embedded JPG (magic + big-endian offset/length @ 0x54/0x58).
    let ej = raf
        .embedded_jpeg
        .as_ref()
        .expect("RAF carries an embedded-JPEG locator");
    assert_eq!(ej.magic, "FUJIFILMCCD-RAW");
    assert_eq!(ej.offset_at, 0x54);
    assert_eq!(ej.length_at, 0x58);
    assert!(ej.big_endian, "RAF header fields are big-endian");

    let mov = s.media_format(0x300d).expect("MOV in the table");
    assert!(mov.is_movie && !mov.is_raw);

    let jpeg = s.media_format(0x3801).expect("JPEG in the table");
    assert!(!jpeg.is_raw && !jpeg.is_movie);
    // A non-RAW format carries no embedded-JPEG locator.
    assert!(jpeg.embedded_jpeg.is_none());

    // #136: photos-compatibility is data — JPEG/RAF/MOV are all full assets the
    // app may hand to the OS photo library, without its own still/movie tables.
    assert!(jpeg.is_photos_compatible);
    assert!(raf.is_photos_compatible);
    assert!(mov.is_photos_compatible);
    // The reported-size sentinel is data, not a hardcoded 0xFFFFFFFF in Swift.
    assert_eq!(s.object_info_size_sentinel(), Some(0xffff_ffff));
    assert_eq!(s.wireless_transfer_ceiling(), Some(0xffff_ffff));

    // An unknown / unlisted format code → None.
    assert!(s.media_format(0x9999).is_none());
}

// ---------------------------------------------------------------------------
// G3 codec seam (#133) — PTP/IP framing + dataset codecs. Fixtures are built
// with the same ptp-core / protocol-primitives primitives the FFI wraps, so
// each test proves the exposed parser is the inverse of the real encoder.
// ---------------------------------------------------------------------------

use ptp_core::{
    DevicePropDesc, EventPacket, ObjectInfo, OperationResponse, PropForm, PropValue, PtpIpPacket,
    Writer,
};

#[test]
fn build_command_is_byte_exact_per_framing() {
    // Compressed OpenSession(0x1002), tid 1, param 1 — no DataPhaseInfo field.
    assert_eq!(
        build_command(PtpFraming::Compressed, 0x1002, 1, vec![1]).unwrap(),
        vec![0x10, 0, 0, 0, 0x01, 0x00, 0x02, 0x10, 0x01, 0, 0, 0, 0x01, 0, 0, 0],
    );
    // Standard framing carries DataPhaseInfo (=1) before the opcode.
    assert_eq!(
        build_command(PtpFraming::Standard, 0x1002, 1, vec![1]).unwrap(),
        vec![
            0x16, 0, 0, 0, // length 22
            0x06, 0, 0, 0, // type OperationRequest
            0x01, 0, 0, 0, // data phase info
            0x02, 0x10, // op 0x1002
            0x01, 0, 0, 0, // tid
            0x01, 0, 0, 0, // param
        ],
    );
    // USB container: GetDeviceInfo(0x1001), tid 0, no params.
    assert_eq!(
        build_command(PtpFraming::Usb, 0x1001, 0, vec![]).unwrap(),
        vec![0x0c, 0, 0, 0, 0x01, 0x00, 0x01, 0x10, 0x00, 0x00, 0x00, 0x00],
    );
}

#[test]
fn build_data_round_trips_through_parse_data_payload() {
    let payload = vec![0xde, 0xad, 0xbe, 0xef, 0x01];
    for framing in [
        PtpFraming::Standard,
        PtpFraming::Compressed,
        PtpFraming::Usb,
    ] {
        let frame = build_data(framing, 0x1009, 9, payload.clone()).unwrap();
        assert_eq!(parse_data_payload(framing, frame).unwrap(), payload);
    }
    // USB retains the operation code in its type-2 container header.
    assert_eq!(
        build_data(PtpFraming::Usb, 0x1009, 7, vec![0xde, 0xad, 0xbe, 0xef]).unwrap(),
        vec![0x10, 0, 0, 0, 0x02, 0x00, 0x09, 0x10, 0x07, 0, 0, 0, 0xde, 0xad, 0xbe, 0xef,]
    );
    let usb_data = vec![
        0x10, 0, 0, 0, 0x02, 0x00, 0x09, 0x10, 0x07, 0, 0, 0, 0xde, 0xad, 0xbe, 0xef,
    ];
    assert_eq!(
        parse_data_payload(PtpFraming::Usb, usb_data).unwrap(),
        vec![0xde, 0xad, 0xbe, 0xef],
    );
}

#[test]
fn parse_response_reads_a_compressed_operation_response() {
    let frame = protocol_primitives::fuji_framing::encode(&PtpIpPacket::OperationResponse(
        OperationResponse {
            code: 0x2001,
            transaction_id: 7,
            params: vec![0x2a],
        },
    ))
    .unwrap();
    let r = parse_response(PtpFraming::Compressed, frame).unwrap();
    assert_eq!(r.response_code, 0x2001);
    assert_eq!(r.txn, 7);
    assert_eq!(r.params, vec![0x2a]);

    // Truncated bytes → a decode error, not a panic.
    assert!(matches!(
        parse_response(PtpFraming::Standard, vec![0, 0]),
        Err(CodecError::Decode(_))
    ));
}

#[test]
fn parse_event_reads_the_event_container_and_ignores_non_events() {
    // USB/PIMA event container (type 4).
    let frame = protocol_primitives::usb_ptp::encode(&PtpIpPacket::Event(EventPacket {
        code: 0x4002,
        transaction_id: 0,
        params: vec![5],
    }))
    .unwrap();
    let got = parse_event(PtpFraming::Usb, frame)
        .unwrap()
        .expect("an event");
    assert_eq!(got.code, 0x4002);
    assert_eq!(got.params, vec![5]);

    // Standard PTP/IP event (packet-type 8) round-trips too.
    let std_frame = ptp_core::encode(&PtpIpPacket::Event(EventPacket {
        code: 0x4002,
        transaction_id: 3,
        params: vec![9],
    }))
    .unwrap();
    assert_eq!(
        parse_event(PtpFraming::Standard, std_frame)
            .unwrap()
            .unwrap()
            .code,
        0x4002,
    );

    // A response frame is not an event → None (not an error).
    let resp =
        protocol_primitives::usb_ptp::encode(&PtpIpPacket::OperationResponse(OperationResponse {
            code: 0x2001,
            transaction_id: 1,
            params: vec![],
        }))
        .unwrap();
    assert!(parse_event(PtpFraming::Usb, resp).unwrap().is_none());
}

#[test]
fn parse_object_info_decodes_the_generic_fields() {
    let oi = ObjectInfo {
        storage_id: 0x0001_0001,
        object_format: 0xb103, // RAF
        object_compressed_size: 4_289_912,
        image_pix_width: 8256,
        image_pix_height: 6192,
        filename: "DSCF1494.RAF".into(),
        ..Default::default()
    };
    let mut w = Writer::new();
    oi.encode(&mut w).unwrap();
    let got = parse_object_info(w.into_vec()).unwrap();
    assert_eq!(got.object_format, 0xb103);
    assert_eq!(got.object_compressed_size, 4_289_912);
    assert_eq!(got.image_pix_width, 8256);
    assert_eq!(got.filename, "DSCF1494.RAF");
}

#[test]
fn parse_device_prop_desc_decodes_value_and_enum_form() {
    let desc = DevicePropDesc {
        code: 0x5007,
        datatype: 0x0004, // UINT16
        get_set: 1,
        factory_default: PropValue::U16(400),
        current: PropValue::U16(560),
        form: PropForm::Enum(vec![PropValue::U16(280), PropValue::U16(560)]),
    };
    let mut w = Writer::new();
    desc.encode(&mut w).unwrap();
    let got = parse_device_prop_desc(w.into_vec()).unwrap();
    assert_eq!(got.code, 0x5007);
    assert_eq!(got.datatype, 0x0004);
    assert!(matches!(got.current, PtpValue::U16 { value: 560 }));
    match got.form {
        PtpPropForm::Enum { values } => {
            assert_eq!(values.len(), 2);
            assert!(matches!(values[0], PtpValue::U16 { value: 280 }));
        }
        other => panic!("expected an enum form, got {other:?}"),
    }
}

#[test]
fn parse_record_stream_preserves_member_types_and_absence() {
    let info = store()
        .property_payload(0xd212)
        .expect("D212 payload descriptor");

    let one_numeric =
        parse_record_stream(vec![0x01, 0x00, 0x41, 0xdf, 0x01, 0x00, 0x00, 0x00], info).unwrap();
    assert_eq!(
        one_numeric.records,
        vec![RecordStreamRecord {
            code: 0xdf41,
            value: PtpValue::U32 { value: 1 },
        }]
    );
    assert_eq!(record_stream_value(one_numeric, 0xd22f), None);

    let present_zero = parse_record_stream(
        vec![0x01, 0x00, 0x41, 0xdf, 0x00, 0x00, 0x00, 0x00],
        store()
            .property_payload(0xd212)
            .expect("D212 payload descriptor"),
    )
    .unwrap();
    assert_eq!(
        record_stream_value(present_zero, 0xdf41),
        Some(PtpValue::U32 { value: 0 })
    );

    let mixed = parse_record_stream(
        vec![
            0x04, 0x00, 0x00, 0xdf, 0x12, 0x00, 0x00, 0x00, 0x20, 0xd2, 0x01, 0x00, 0x00, 0x00,
            0x41, 0xdf, 0x01, 0x00, 0x00, 0x00, 0x2f, 0xd2, 0x01, 0x00, 0x00,
        ],
        store()
            .property_payload(0xd212)
            .expect("D212 payload descriptor"),
    )
    .unwrap();
    assert_eq!(
        record_stream_value(mixed.clone(), 0xdf41),
        Some(PtpValue::U32 { value: 1 })
    );
    assert_eq!(
        record_stream_value(mixed, 0xd22f),
        Some(PtpValue::Str {
            value: String::new()
        })
    );
}

#[test]
fn parse_record_stream_honors_declared_widths_with_d212_defaults() {
    // Omitted widths use the schema defaults from
    // camera_config::Payload::record_widths (#161).
    let defaults = PayloadInfo {
        form: PayloadForm::RecordStream,
        count_width: None,
        record: None,
        members: vec![RecordMemberInfo {
            code: 0x5007,
            encoding: RecordValueEncodingInfo::Fixed { width: 4 },
        }],
    };
    let cc_defaults = camera_config::Payload {
        form: camera_config::PayloadForm::RecordStream,
        count_width: None,
        record: None,
        members: vec![],
    }
    .record_widths();
    assert_eq!(cc_defaults, (2, 2, 4), "schema defaults are the D212 shape");
    let d212 = protocol_primitives::quirk::record_stream(
        &[(0x5007, 280)],
        &protocol_primitives::quirk::RecordStreamLayout::D212,
    )
    .unwrap();
    let via_defaults = parse_record_stream(d212, defaults).unwrap();
    assert_eq!(via_defaults.records.len(), 1);
    assert_eq!(
        (
            via_defaults.records[0].code,
            via_defaults.records[0].value.clone()
        ),
        (0x5007, PtpValue::U32 { value: 280 })
    );

    // Declared non-default widths reframe the parse: u8 count, u8 code, u16 value.
    let tight = PayloadInfo {
        form: PayloadForm::RecordStream,
        count_width: Some(1),
        record: Some(RecordLayoutInfo {
            code_width: 1,
            value_width: 2,
        }),
        members: vec![
            RecordMemberInfo {
                code: 0x07,
                encoding: RecordValueEncodingInfo::Fixed { width: 2 },
            },
            RecordMemberInfo {
                code: 0x09,
                encoding: RecordValueEncodingInfo::Fixed { width: 2 },
            },
        ],
    };
    let layout = protocol_primitives::quirk::RecordStreamLayout::new(1, 1, 2).unwrap();
    let bytes =
        protocol_primitives::quirk::record_stream(&[(0x07, 280), (0x09, 1)], &layout).unwrap();
    let ls = parse_record_stream(bytes, tight).unwrap();
    assert_eq!(ls.records.len(), 2);
    assert_eq!(
        (ls.records[0].code, ls.records[0].value.clone()),
        (0x07, PtpValue::U32 { value: 280 })
    );

    // Widths the codec can't honor are a loud decode error, not a misread.
    let bad = PayloadInfo {
        form: PayloadForm::RecordStream,
        count_width: Some(3),
        record: None,
        members: vec![],
    };
    assert!(parse_record_stream(vec![0, 0], bad).is_err());
}

#[test]
fn parse_object_handle_list_decodes_a_u32_array() {
    let mut w = Writer::new();
    w.ptp_array(&[7u32, 9, 11], |w, v| w.u32(*v));
    assert_eq!(
        parse_object_handle_list(w.into_vec()).unwrap(),
        vec![7, 9, 11]
    );
}

#[test]
fn socket_bindings_and_transport_close_surface_through_ffi() {
    let s = store();
    // Resolved ports keyed by role — the app binds by role, not Fuji offsets (#140).
    assert_eq!(
        s.port_for_role("app".into(), SocketRole::Command),
        Some(55740)
    );
    assert_eq!(
        s.port_for_role("app".into(), SocketRole::Event),
        Some(55741)
    );
    assert_eq!(
        s.port_for_role("app".into(), SocketRole::LiveView),
        Some(55742)
    );
    // socket_bindings lists the three roles in command → event → live-view order.
    let binds = s.socket_bindings("app".into());
    assert_eq!(binds.len(), 3);
    assert_eq!(binds[0].role, SocketRole::Command);
    assert_eq!(binds[0].port, 55740);
    assert_eq!(binds[2].role, SocketRole::LiveView);

    // wireless-tether binds only a command socket (PCSS DSPORT 15740); poll-based
    // delivery means no event/live-view socket.
    assert_eq!(
        s.port_for_role("wireless-tether".into(), SocketRole::Command),
        Some(15740)
    );
    assert_eq!(
        s.port_for_role("wireless-tether".into(), SocketRole::Event),
        None
    );
    assert_eq!(s.socket_bindings("wireless-tether".into()).len(), 1);

    // Transport-close resolves the named sentinel through manifest data.
    let tc = s
        .transport_close("app".into())
        .expect("transport-close query succeeds")
        .expect("app declares a transport-close");
    assert_eq!(tc.packet, vec![0x08, 0, 0, 0, 0xff, 0xff, 0xff, 0xff]);
    assert_eq!(
        tc.when.as_deref(),
        Some("before-image-transfer-reestablishment")
    );
    // A connection without one → Ok(None).
    assert!(s
        .transport_close("wireless-tether".into())
        .expect("transport-close query succeeds")
        .is_none());
}

#[test]
fn transport_close_reports_bad_sentinel_data() {
    let missing = ConfigStore::from_bundle(
        r#"
schema: camera-config/v1
camera: { manufacturer: TESTCO, model: TM1 }
connections:
  app:
    transportClose: { sentinel: missing }
"#
        .into(),
        None,
    )
    .expect("manifest loads");
    assert!(matches!(
        missing.transport_close("app".into()),
        Err(TransportCloseError::UnknownSentinel(_))
    ));

    let malformed = ConfigStore::from_bundle(
        r#"
schema: camera-config/v1
camera: { manufacturer: TESTCO, model: TM1 }
sentinels:
  bad: { bytes: "0xz" }
connections:
  app:
    transportClose: { sentinel: bad }
"#
        .into(),
        None,
    )
    .expect("manifest loads");
    assert!(matches!(
        malformed.transport_close("app".into()),
        Err(TransportCloseError::InvalidSentinelBytes(_))
    ));
}

#[test]
fn parse_data_phase_decodes_a_compressed_single_frame() {
    // The wireless-tether/reference app compressed channel delivers a whole data phase in
    // one type-2 frame — no StartData/Data/EndData. Byte-exact golden from
    // 2026-06-02-pcss-ptpip-fuji-original.pcapng: Data(0x1015) tid 2, value 5.
    let golden = vec![
        0x0e, 0, 0, 0, // length 14
        0x02, 0x00, // type 2 = Data
        0x15, 0x10, // opcode 0x1015 echoed in the code field
        0x02, 0, 0, 0, // tid 2
        0x05, 0x00, // payload: u16 value 5
    ];
    // build_data reproduces the captured frame byte-for-byte.
    assert_eq!(
        build_data(PtpFraming::Compressed, 0x1015, 2, vec![0x05, 0x00]).unwrap(),
        golden,
    );
    // parse_data_phase yields the entire payload in one Data frame — the app used
    // to crash waiting for an EndData / type-12 that never arrives.
    let d = parse_data_phase(PtpFraming::Compressed, golden).unwrap();
    assert_eq!(d.kind, DataPhaseKind::Data);
    assert_eq!(d.txn, 2);
    assert_eq!(d.payload, vec![0x05, 0x00]);
    assert_eq!(d.total_length, None);

    // A large transfer is still a single frame: on the wire a full GetObject(0x1009)
    // arrives as one 14.5 MB type-2 Data frame. A stand-in payload round-trips whole.
    let big = vec![0xabu8; 4096];
    let frame = build_data(PtpFraming::Compressed, 0x1009, 99, big.clone()).unwrap();
    let bd = parse_data_phase(PtpFraming::Compressed, frame).unwrap();
    assert_eq!(bd.kind, DataPhaseKind::Data);
    assert_eq!(bd.payload, big);
}

#[test]
fn connection_wire_framing_is_declared_in_the_manifest() {
    let s = store();
    let app = s
        .connections(Platform::Macos)
        .into_iter()
        .find(|c| c.id == "app")
        .expect("app connection");
    // The app reads framing from data — it never maps kind→framing itself (#133).
    assert!(matches!(app.command_framing, Some(PtpFraming::Compressed)));
    // Command vs event framing differ: the event socket is a PIMA type-4 container.
    assert!(matches!(app.event_framing, Some(PtpFraming::Usb)));
    // The manifest-declared command framing drives the codec: byte-exact compressed frame.
    assert_eq!(
        build_command(app.command_framing.unwrap(), 0x1002, 1, vec![1]).unwrap(),
        vec![0x10, 0, 0, 0, 0x01, 0x00, 0x02, 0x10, 0x01, 0, 0, 0, 0x01, 0, 0, 0],
    );

    // wireless-tether's command/data channel is compressed too (single type-2 data
    // frame); its poll-based delivery means no event socket, so no event framing.
    let wt = s
        .connections(Platform::Macos)
        .into_iter()
        .find(|c| c.id == "wireless-tether")
        .expect("wireless-tether connection");
    assert!(matches!(wt.command_framing, Some(PtpFraming::Compressed)));
    assert!(wt.event_framing.is_none());
}
