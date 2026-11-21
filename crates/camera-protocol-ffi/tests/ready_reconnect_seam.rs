//! Ready-profile selection through the public FFI, using synthetic index data.
use camera_protocol_ffi::*;
use std::sync::Arc;

mod common;

fn body(model: &str) -> String {
    format!(
        "schema: camera-config/v1\ncamera:\n  manufacturer: TESTCO\n  model: {model}\nconnections:\n  ble:\n    kind: ble\n    establishment: test\n"
    )
}

/// Routes share an advert predicate so their declaration order is observable.
/// `pairing` has no reconnect route; all other routes are reconnect-only.
fn synthetic_store(models: &[(&str, bool, &[&str])]) -> Arc<ConfigStore> {
    let mut index = String::from(
        r#"
manufacturer: TESTCO
families:
  test:
    ble:
      gatt: {}
      reconnect: { scanTimeoutMs: 1000 }
      establishments:
        test:
          mechanism: test
          activities:
            - id: camera.test.connect
              version: 1
              displayRole: connecting
              defaultExpectedDurationMs: 1
              interactionRequired: false
              executorSpan: { sequence: steps, startStep: 0, endStepExclusive: 1 }
          steps: [{ bleConnect: {} }]
models:
"#,
    );
    let mut bodies = vec![];
    for (model, fallback, routes) in models {
        index.push_str(&format!(
            "  - id: {model}\n    displayName: {model}\n    inherits: [test]\n    manifest: {model}.yaml\n    fallback: {fallback}\n    signatures:\n"
        ));
        for (number, route) in routes.iter().enumerate() {
            index.push_str(&format!(
                r#"      route{number}:
        kind: bleAdvert
        require:
          serviceUuids: {{ contains: "0000DE00-3DD4-4255-8D62-6DC7B9BD5561" }}
        capture:
          - {{ source: localName, at: 0, length: 4, encoding: ascii, name: shortSerial }}
        scope: {{ style: test }}
        suggests: {{ connection: ble, confidence: high }}
        discoverable: {}
"#,
                *route == "pairing"
            ));
            if *route != "pairing" {
                index.push_str(&format!(
                    "        reconnect:\n          disposition: {route}\n          mechanism: test\n          identity: [style, shortSerial]\n"
                ));
            }
        }
        bodies.push(KeyValue {
            key: (*model).into(),
            value: body(model),
        });
    }
    ConfigStore::from_manufacturer_index(index, bodies).expect("synthetic reconnect index")
}

fn advert(local_name: Option<&str>) -> ScanObservation {
    ScanObservation::BleAdvert {
        service_uuids: vec!["0000DE00-3DD4-4255-8D62-6DC7B9BD5561".into()],
        manufacturer_data: None,
        service_data: vec![],
        local_name: local_name.map(String::from),
        tx_power: None,
        ad_records: vec![],
    }
}

fn saved_scope() -> Vec<KeyValue> {
    vec![
        KeyValue {
            key: "style".into(),
            value: "test".into(),
        },
        KeyValue {
            key: "shortSerial".into(),
            value: "ABCD".into(),
        },
    ]
}

fn candidate_ids(result: ReadyReconnectRecognition) -> Vec<(String, bool)> {
    let candidates = match result {
        ReadyReconnectRecognition::NoMatch => return vec![],
        ReadyReconnectRecognition::Candidate { candidate } => vec![candidate],
        ReadyReconnectRecognition::Disambiguate { candidates } => {
            assert!(candidates.len() > 1, "ambiguity requires multiple profiles");
            candidates
        }
    };
    candidates
        .into_iter()
        .map(|candidate| (candidate.model, candidate.is_fallback))
        .collect()
}

#[test]
fn unique_ready_profile_reports_its_fallback_marker() {
    for fallback in [false, true] {
        let s = synthetic_store(&[("only", fallback, &["ready"])]);
        let ReadyReconnectRecognition::Candidate { candidate } =
            s.recognize_ready_for_reconnect(advert(Some("ABCD")), saved_scope())
        else {
            panic!("one Ready profile must return Candidate");
        };
        assert_eq!(candidate.model, "only");
        assert_eq!(candidate.is_fallback, fallback);
    }
}

#[test]
fn ready_specifics_suppress_baselines_without_losing_order_or_ambiguity() {
    for (models, expected) in [
        (
            vec![
                ("base", true, &["ready"][..]),
                ("specific", false, &["ready"][..]),
            ],
            vec![("specific".into(), false)],
        ),
        (
            vec![
                ("z-specific", false, &["ready"][..]),
                ("base", true, &["ready"][..]),
                ("a-specific", false, &["ready"][..]),
            ],
            vec![("z-specific".into(), false), ("a-specific".into(), false)],
        ),
        (
            vec![
                ("z-base", true, &["ready"][..]),
                ("a-base", true, &["ready"][..]),
            ],
            vec![("z-base".into(), true), ("a-base".into(), true)],
        ),
    ] {
        let result = synthetic_store(&models)
            .recognize_ready_for_reconnect(advert(Some("ABCD")), saved_scope());
        assert_eq!(candidate_ids(result), expected);
    }
}

#[test]
fn selection_preserves_first_eligible_route_in_each_model() {
    let s = synthetic_store(&[
        ("wake-first", false, &["wake", "ready"]),
        ("ready-first", false, &["ready", "wake"]),
    ]);
    assert!(matches!(
        s.reconnect_decision("wake-first".into(), advert(Some("ABCD")), saved_scope()),
        ReconnectDecision::Wake { .. }
    ));
    assert_eq!(
        candidate_ids(s.recognize_ready_for_reconnect(advert(Some("ABCD")), saved_scope())),
        vec![("ready-first".into(), false)]
    );
    let only_wake = synthetic_store(&[("wake", false, &["wake", "ready"])]);
    assert!(matches!(
        only_wake.recognize_ready_for_reconnect(advert(Some("ABCD")), saved_scope()),
        ReadyReconnectRecognition::NoMatch
    ));
}

#[test]
fn waking_specific_does_not_suppress_ready_baseline() {
    let s = synthetic_store(&[
        ("specific", false, &["wake", "ready"]),
        ("base", true, &["ready"]),
    ]);
    assert_eq!(
        candidate_ids(s.recognize_ready_for_reconnect(advert(Some("ABCD")), saved_scope())),
        vec![("base".into(), true)]
    );
}

#[test]
fn pairing_and_ready_recognition_keep_separate_eligibility() {
    let s = synthetic_store(&[
        ("pairing", false, &["pairing"]),
        ("ready", true, &["ready"]),
    ]);
    assert!(matches!(
        s.recognize(advert(Some("ABCD"))),
        Recognition::Candidate { model, .. } if model == "pairing"
    ));
    assert_eq!(
        candidate_ids(s.recognize_ready_for_reconnect(advert(Some("ABCD")), saved_scope())),
        vec![("ready".into(), true)]
    );
    let reconnect_only = synthetic_store(&[("ready", false, &["ready"])]);
    assert!(matches!(
        reconnect_only.recognize(advert(Some("ABCD"))),
        Recognition::NoMatch
    ));
}

#[test]
fn every_identity_value_must_exist_and_equal_the_saved_value() {
    let s = synthetic_store(&[("specific", false, &["ready"])]);
    for scope in [
        vec![],
        vec![saved_scope()[0].clone()],
        vec![saved_scope()[1].clone()],
    ] {
        assert!(matches!(
            s.recognize_ready_for_reconnect(advert(Some("ABCD")), scope),
            ReadyReconnectRecognition::NoMatch
        ));
    }
    for index in 0..2 {
        let mut wrong = saved_scope();
        wrong[index].value = "wrong".into();
        assert!(matches!(
            s.recognize_ready_for_reconnect(advert(Some("ABCD")), wrong),
            ReadyReconnectRecognition::NoMatch
        ));
    }
    for name in [None, Some("ABC"), Some("éAB")] {
        for scope in [saved_scope(), vec![], vec![saved_scope()[0].clone()]] {
            assert!(matches!(
                s.recognize_ready_for_reconnect(advert(name), scope),
                ReadyReconnectRecognition::NoMatch
            ));
        }
    }
    let mut wrong_advert = advert(Some("ABCD"));
    if let ScanObservation::BleAdvert { service_uuids, .. } = &mut wrong_advert {
        service_uuids.clear();
    }
    assert!(matches!(
        s.recognize_ready_for_reconnect(wrong_advert, saved_scope()),
        ReadyReconnectRecognition::NoMatch
    ));
}

#[test]
fn absent_index_and_non_ble_observations_fail_closed() {
    let no_index = ConfigStore::from_bundle(body("test"), None).unwrap();
    assert!(matches!(
        no_index.recognize_ready_for_reconnect(advert(Some("ABCD")), saved_scope()),
        ReadyReconnectRecognition::NoMatch
    ));
    let s = synthetic_store(&[("specific", false, &["ready"])]);
    for observation in [
        ScanObservation::PcssNotify {
            camera_ipv4: "192.0.2.1".into(),
            camera_name: "test".into(),
            command_port: 15740,
            service: "PCSS/1.0".into(),
        },
        ScanObservation::UsbAttachment {
            platform: Platform::Linux,
            vendor_id: 1,
            product_id: 2,
        },
    ] {
        assert!(matches!(
            s.recognize_ready_for_reconnect(observation, saved_scope()),
            ReadyReconnectRecognition::NoMatch
        ));
    }
}

#[test]
fn overlapping_red_profiles_are_compatible_hints_without_name_identification() {
    let s = common::real_fuji_store();
    // Both indexed RED Ready routes accept this serial and service. Neither
    // route identifies the physical camera from the advertised local name.
    for name in [None, Some("unidentified camera")] {
        let observation = || ScanObservation::BleAdvert {
            service_uuids: vec!["123D8F06-62A1-4935-9322-833C531EE225".into()],
            manufacturer_data: Some(BleManufacturerData {
                company_id: 0x04D8,
                payload: vec![1, b'A', b'B', b'C', b'D', b'E'],
            }),
            service_data: vec![],
            local_name: name.map(String::from),
            tx_power: None,
            ad_records: vec![],
        };
        let saved = vec![KeyValue {
            key: "shortSerial".into(),
            value: "ABCDE".into(),
        }];
        assert!(matches!(
            s.reconnect_decision("fuji-generic".into(), observation(), saved.clone()),
            ReconnectDecision::Ready { .. }
        ));
        assert_eq!(
            candidate_ids(s.recognize_ready_for_reconnect(observation(), saved)),
            vec![("gfx100ii".into(), false)]
        );
    }
}
