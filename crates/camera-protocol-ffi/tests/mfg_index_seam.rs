//! End-to-end exercise of the manufacturer-index FFI surface (plan §3.2 +
//! §3.3 + §11): load → recognize → establishment → refine_establishment.
//! Synthetic adverts mirror the GFX100 II / fw 2.30 observations from

use camera_protocol_ffi::*;
use std::path::PathBuf;

mod common;

fn data(rel: &str) -> String {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../packages/camera-config-data")
        .join(rel);
    std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("read {}: {e}", p.display()))
}

fn store() -> std::sync::Arc<ConfigStore> {
    common::real_fuji_store()
}

/// Minimal body manifest for the synthetic `tm1` model used by the inline-index
/// seam tests. Its `ble` connection declares `establishment: test`, matching the
/// mechanism the synthetic indexes register their plan under — so
/// `ConfigStore::establishment("tm1", "ble", …)` resolves to that plan.
fn tm1_body() -> String {
    r#"
schema: camera-config/v1
camera:
  manufacturer: TESTCO
  model: TM1
connections:
  ble:
    kind: ble
    establishment: test
"#
    .to_string()
}

fn tm1_store_with_step(step: &str) -> std::sync::Arc<ConfigStore> {
    let index_yaml = format!(
        r#"
manufacturer: TESTCO
families:
  test:
    ble:
      gatt:
        c: "00002A25-0000-1000-8000-00805F9B34FB"
      establishments:
        test:
          mechanism: test
          activities:
            - id: camera.test.step
              version: 1
              displayRole: preparingConnection
              defaultExpectedDurationMs: 1
              interactionRequired: false
              executorSpan:
                sequence: steps
                startStep: 0
                endStepExclusive: 1
          steps:
{step}
models:
  - id: tm1
    displayName: "Test"
    inherits: [test]
    manifest: tm1.yaml
"#
    );
    ConfigStore::from_manufacturer_index(
        index_yaml,
        vec![KeyValue {
            key: "tm1".into(),
            value: tm1_body(),
        }],
    )
    .expect("synthetic single-step index loads")
}

/// Convenience constructor for the common "manufacturer data + service
/// UUIDs" advert shape. Fields the synthetic adverts never carry
/// (service data, TX power, raw AD records) stay empty.
fn ble_advert(
    service_uuids: &[&str],
    company_id: u16,
    payload: &[u8],
    local_name: Option<&str>,
) -> ScanObservation {
    ScanObservation::BleAdvert {
        service_uuids: service_uuids.iter().map(|s| s.to_string()).collect(),
        manufacturer_data: Some(BleManufacturerData {
            company_id,
            payload: payload.to_vec(),
        }),
        service_data: vec![],
        local_name: local_name.map(String::from),
        tx_power: None,
        ad_records: vec![],
    }
}

#[test]
fn pcss_notify_recognition_carries_dynamic_endpoint_scope() {
    let result = store().recognize(ScanObservation::PcssNotify {
        camera_ipv4: "192.0.2.44".into(),
        camera_name: "GFX100 II".into(),
        command_port: 17555,
        service: "PCSS/1.0".into(),
    });
    let Recognition::Candidate {
        model,
        connection,
        runtime_scope,
        ..
    } = result
    else {
        panic!("expected PCSS candidate");
    };
    assert_eq!(model, "gfx100ii");
    assert_eq!(connection, "wireless-tether");
    assert!(runtime_scope
        .iter()
        .any(|entry| { entry.key == "cameraIpv4" && entry.value == "192.0.2.44" }));
    assert!(runtime_scope
        .iter()
        .any(|entry| entry.key == "commandPort" && entry.value == "17555"));
}

#[test]
fn usb_attachment_recognition_selects_the_manifest_platform_route() {
    let store = store();
    for (platform, expected_connection) in [
        (Platform::Ios, "usb-passthrough"),
        (Platform::Macos, "usb-passthrough"),
        (Platform::Android, "usb"),
        (Platform::Linux, "usb"),
    ] {
        let recognition = store.recognize(ScanObservation::UsbAttachment {
            platform,
            vendor_id: 0x04cb,
            product_id: 0x4321,
        });
        let Recognition::Candidate {
            model,
            connection,
            confidence,
            runtime_scope,
            ..
        } = recognition
        else {
            panic!("expected USB candidate for {platform:?}");
        };
        assert_eq!(model, "gfx100ii");
        assert_eq!(connection, expected_connection);
        assert!(matches!(confidence, Confidence::Medium));
        assert!(runtime_scope
            .iter()
            .any(|entry| entry.key == "usbVendorId" && entry.value == "1227"));
        assert!(runtime_scope
            .iter()
            .any(|entry| entry.key == "usbProductId" && entry.value == "17185"));
    }
}

#[test]
fn unmatched_usb_attachment_returns_no_match() {
    assert!(matches!(
        store().recognize(ScanObservation::UsbAttachment {
            platform: Platform::Ios,
            vendor_id: 0xffff,
            product_id: 0x4321,
        }),
        Recognition::NoMatch
    ));
}

#[test]
fn authored_usb_product_id_is_matched_exactly() {
    let body = data("fuji/gfx100ii/gfx100ii.yaml").replace(
        "      vid: 0x04cb\n",
        "      vid: 0x04cb\n      pid: 0x3105\n",
    );
    let store = ConfigStore::from_manufacturer_index_with_defaults(
        data("fuji/index.yaml"),
        data("fuji/fuji.yaml"),
        common::real_fuji_bodies_with("gfx100ii", body),
    )
    .expect("manifest with an evidenced product ID loads");

    assert!(matches!(
        store.recognize(ScanObservation::UsbAttachment {
            platform: Platform::Ios,
            vendor_id: 0x04cb,
            product_id: 0x3105,
        }),
        Recognition::Candidate {
            confidence: Confidence::High,
            ..
        }
    ));
    assert!(matches!(
        store.recognize(ScanObservation::UsbAttachment {
            platform: Platform::Ios,
            vendor_id: 0x04cb,
            product_id: 0x9999,
        }),
        Recognition::NoMatch
    ));
}

fn device_info(manufacturer: &str, model: &str) -> PtpDeviceInfo {
    PtpDeviceInfo {
        standard_version: 100,
        vendor_extension_id: 0,
        vendor_extension_version: 0,
        vendor_extension_desc: String::new(),
        functional_mode: 0,
        operations_supported: Vec::new(),
        events_supported: Vec::new(),
        device_properties_supported: Vec::new(),
        capture_formats: Vec::new(),
        image_formats: Vec::new(),
        manufacturer: manufacturer.into(),
        model: model.into(),
        device_version: "2.30".into(),
        serial_number: "TEST-SERIAL".into(),
    }
}

#[test]
fn parsed_device_info_confirms_the_recognized_model_inside_the_engine() {
    let store = store();
    assert!(store.confirm_device_info(
        "gfx100ii".into(),
        device_info("  fujifilm\0", "GFX100   II"),
    ));
    assert!(!store.confirm_device_info("gfx100ii".into(), device_info("FUJIFILM", "X-T5"),));
    assert!(!store.confirm_device_info("not-a-model".into(), device_info("FUJIFILM", "GFX100 II"),));
}

// ---------------------------------------------------------------------------
// from_manufacturer_index loader
// ---------------------------------------------------------------------------

#[test]
fn loader_requires_every_declared_model_body() {
    let result = ConfigStore::from_manufacturer_index(data("fuji/index.yaml"), vec![]);
    let Err(ConfigError::Parse(msg)) = result else {
        panic!("expected Parse error, got Ok");
    };
    assert!(msg.contains("gfx100ii"), "got: {msg}");
}

#[test]
fn non_hex_property_key_in_secondary_body_is_a_load_error() {
    let index_yaml = r#"
manufacturer: TESTCO
families:
  test:
    ble:
      gatt: {}
      establishments:
        test: { mechanism: test, steps: [] }
models:
  - id: primary
    displayName: Primary
    inherits: [test]
    manifest: primary.yaml
  - id: secondary
    displayName: Secondary
    inherits: [test]
    manifest: secondary.yaml
"#;
    let secondary_body = format!(
        "{}properties: {{ \"0xzz\": {{ name: bogus }} }}\n",
        tm1_body()
    );
    let error = match ConfigStore::from_manufacturer_index(
        index_yaml.to_string(),
        vec![
            KeyValue {
                key: "primary".into(),
                value: tm1_body(),
            },
            KeyValue {
                key: "secondary".into(),
                value: secondary_body,
            },
        ],
    ) {
        Ok(_) => panic!("non-hex property key in secondary body must fail store construction"),
        Err(error) => error,
    };
    assert!(matches!(
        error,
        ConfigError::Parse(message)
            if message.contains("properties map key '0xzz' is not a hex property code")
    ));
}

#[test]
fn establishment_uses_the_requested_models_host_activities() {
    let index_yaml = r#"
manufacturer: TESTCO
families:
  test:
    ble:
      gatt: {}
      advert: {}
      establishments:
        test:
          mechanism: test
          activities:
            - id: camera.test.executor.optional
              version: 1
              displayRole: connecting
              defaultExpectedDurationMs: 1
              interactionRequired: false
              optional: true
              executorSpan: { sequence: steps, startStep: 0, endStepExclusive: 1 }
            - id: camera.test.executor.default
              version: 1
              displayRole: preparingConnection
              defaultExpectedDurationMs: 1
              interactionRequired: false
              executorSpan: { sequence: steps, startStep: 1, endStepExclusive: 2 }
          steps:
            - bleConnect: {}
            - bleConnect: {}
models:
  - id: primary
    displayName: Primary
    inherits: [test]
    manifest: primary.yaml
  - id: secondary
    displayName: Secondary
    inherits: [test]
    manifest: secondary.yaml
"#;
    let body = |model: &str, activity_namespace: &str| {
        format!(
            r#"
schema: camera-config/v1
camera: {{ manufacturer: TESTCO, model: {model} }}
connections:
  ble:
    kind: ble
    establishment: test
    activities:
      - id: camera.{activity_namespace}.host.first
        version: 1
        displayRole: openingSession
        defaultExpectedDurationMs: 1
        interactionRequired: false
        hostCheckpoint: {{ name: networkReady }}
      - id: camera.{activity_namespace}.host.second
        version: 1
        displayRole: openingSession
        defaultExpectedDurationMs: 1
        interactionRequired: false
        hostCheckpoint: {{ name: sessionOpen }}
"#
        )
    };
    let store = ConfigStore::from_manufacturer_index(
        index_yaml.to_string(),
        vec![
            KeyValue {
                key: "primary".into(),
                value: body("Primary", "primary"),
            },
            KeyValue {
                key: "secondary".into(),
                value: body("Secondary", "secondary"),
            },
        ],
    )
    .expect("multi-model store loads");

    let secondary = store
        .model_store("secondary".into())
        .expect("secondary direct-query store");
    assert_eq!(secondary.camera_identity().model, "Secondary");
    assert!(store.model_store("missing".into()).is_none());

    let plan = store
        .establishment("secondary".into(), "ble".into(), vec![])
        .expect("secondary establishment resolves");
    assert_eq!(
        plan.activities
            .iter()
            .map(|activity| activity.id.as_str())
            .collect::<Vec<_>>(),
        [
            "camera.test.executor.optional",
            "camera.test.executor.default",
            "camera.secondary.host.first",
            "camera.secondary.host.second",
        ],
        "establishment spans precede connection checkpoints and both preserve declared order"
    );
    assert!(plan.activities[0].optional);
    assert!(!plan.activities[1].optional);
    assert!(!plan.activities[2].optional);
}

#[test]
fn real_manifest_exposes_resolved_camera_initiated_transfer() {
    let transfer = store()
        .camera_initiated_transfer("gfx100ii".into())
        .expect("GFX100 II declares a camera-initiated transfer");
    assert!(matches!(
        transfer.trigger.match_mode,
        CameraInitiatedTriggerMatch::All
    ));
    assert_eq!(transfer.trigger.states.len(), 2);
    assert_eq!(
        transfer.trigger.states[0].gatt_uuid,
        "A68E3F66-0FCC-4395-8D4C-AA980B5877FA"
    );
    assert_eq!(
        transfer.trigger.states[0].trigger_values,
        vec![vec![0x03, 0x80]]
    );
    assert_eq!(
        transfer.trigger.states[0].baseline_values,
        vec![vec![0x00, 0x80]]
    );
    assert_eq!(
        transfer.trigger.states[1].gatt_uuid,
        "BD17BA04-B76B-4892-A545-B73BA1F74DAE"
    );
    assert_eq!(
        transfer.trigger.states[1].trigger_values,
        vec![vec![0x01, 0x80]]
    );

    assert_eq!(transfer.handoff.connection, "app");
    assert_eq!(transfer.handoff.socket_role, SocketRole::Command);
    assert_eq!(
        transfer.handoff.endpoint_host.as_deref(),
        Some("192.168.0.1")
    );
    assert_eq!(transfer.handoff.endpoint_port, 55740);
    assert!(transfer.handoff.cached_credentials_allowed);
    assert!(matches!(
        transfer.monitor_recovery,
        Some(CameraInitiatedMonitorRecovery::SavedCameraReconnect)
    ));
    let launch = transfer.handoff.function_launch.as_ref().unwrap();
    assert_eq!(launch.gatt_uuid, "600655E6-3637-42F1-8FB2-44EFC5C63B13");
    assert_eq!(launch.value, vec![0x03, 0x00]);
    assert!(!launch.required);

    assert_eq!(transfer.receive.mode, "reserved-photo-receive");
    assert_eq!(transfer.receive.count_property, 0xd212);
    assert_eq!(transfer.receive.count_member, 0xdf41);
    assert_eq!(transfer.receive.head_index, 1);
    assert_eq!(transfer.receive.metadata_operation, 0x1008);
    assert!(matches!(
        transfer.receive.metadata_phases.as_slice(),
        [
            CameraInitiatedMetadataPhase::AfterCountBeforeModeEntry,
            CameraInitiatedMetadataPhase::AfterModeEntry
        ]
    ));
    assert_eq!(transfer.receive.data_operation, 0x101b);
    assert_eq!(transfer.receive.chunk_limit_property, 0xd235);
    assert!(matches!(
        transfer.receive.completion,
        CameraInitiatedCompletion::ReadToEof
    ));
}

#[test]
fn single_body_store_has_no_resolved_camera_initiated_transfer() {
    let store = ConfigStore::from_bundle(data("fuji/gfx100ii/gfx100ii.yaml"), None).unwrap();
    assert!(store.camera_initiated_transfer("gfx100ii".into()).is_none());
}

// ---------------------------------------------------------------------------
// recognize() — BLE advert classification
// ---------------------------------------------------------------------------

/// Pairing-mode LEGACY advert. Mfg-data is `0x02 + 4-byte LE key`.
fn synthetic_legacy_pairing_advert() -> ScanObservation {
    ble_advert(
        &[
            "AF854C2E-B214-458E-97E2-912C4ECF2CB8", // SERVICE_FF_FILE_TRANSFER
            "6514EB81-4E8F-458D-AA2A-E691336CDFAC", // CAMERA_CONTROL — harmless
        ],
        0x04D8, // Fujifilm
        // type=0x02 + key bytes (synthetic placeholder values).
        &[0x02, 0x44, 0x73, 0x2a, 0x80],
        Some("GFX100 II"),
    )
}

/// Idle bonded GFX100 II / fw 2.30 advert observed for issue #264: the
/// file-transfer UUID and serial-bearing local name, with no mfg-data.
fn synthetic_legacy_awake_advert() -> ScanObservation {
    ScanObservation::BleAdvert {
        service_uuids: vec!["AF854C2E-B214-458E-97E2-912C4ECF2CB8".into()],
        manufacturer_data: None,
        service_data: vec![],
        local_name: Some("0C3EGFX100II-0C3E".into()),
        tx_power: None,
        ad_records: vec![],
    }
}

/// A synthetic RED advert: type=0x01 + 5 ASCII bytes (placeholder "ABCDE",
/// the shape of a 5-byte short-serial used as the RED pairing key).
fn synthetic_red_advert() -> ScanObservation {
    ble_advert(
        // RED bodies advertise CONNECTED_DEVICE_INFORMATION_RED, NOT
        // SERVICE_FF_FILE_TRANSFER (legacy detector). Per READ_THIS_FIRST §2.
        &["123D8F06-62A1-4935-9322-833C531EE225"],
        0x04D8, // Fujifilm
        &[0x01, b'A', b'B', b'C', b'D', b'E'],
        Some("GFX100 II"),
    )
}

fn synthetic_red_pairing_advert() -> ScanObservation {
    ble_advert(
        &[],
        0x04D8,
        &[0x01, b'A', b'B', b'C', b'D', b'E'],
        Some("GFX100 II"),
    )
}

fn synthetic_legacy_startup_advert() -> ScanObservation {
    ble_advert(
        &["731893F9-744E-4899-B7E3-174106FF2B82"],
        0x04D8,
        &[0x02, 0x44, 0x73, 0x2a, 0x80, 0x00],
        Some("GFX100 II"),
    )
}

fn synthetic_red_startup_advert() -> ScanObservation {
    ble_advert(
        &["804DAA8E-FFEB-4AB3-8E75-6EDD7303208D"],
        0x04D8,
        &[0x01, b'A', b'B', b'C', b'D', b'E', 0x00],
        Some("GFX100 II"),
    )
}

#[test]
fn saved_reconnect_classifies_startup_and_awake_adverts_from_persisted_identity() {
    let s = store();
    assert_eq!(
        s.reconnect_policy("gfx100ii".into())
            .unwrap()
            .scan_timeout_ms,
        60_000
    );

    let legacy_scope = vec![
        KeyValue {
            key: "pairingKeyBytes".into(),
            value: "44732a80".into(),
        },
        KeyValue {
            key: "style".into(),
            value: "legacy".into(),
        },
        KeyValue {
            key: "shortSerial".into(),
            value: "0C3E".into(),
        },
    ];
    match s.reconnect_decision(
        "gfx100ii".into(),
        synthetic_legacy_startup_advert(),
        legacy_scope.clone(),
    ) {
        ReconnectDecision::Wake { plan, .. } => {
            assert_eq!(plan.plan_handle, "gfx100ii:ble-wake");
            assert_eq!(plan.mechanism, "ble-wake");
            assert!(matches!(
                plan.steps.as_slice(),
                [
                    Step::BleConnect { .. },
                    Step::BleAwaitDisconnect {
                        timeout_ms: 60_000,
                        ..
                    }
                ]
            ));
        }
        other => panic!("expected wake, got {other:?}"),
    }
    match s.reconnect_decision(
        "gfx100ii".into(),
        synthetic_legacy_awake_advert(),
        legacy_scope,
    ) {
        ReconnectDecision::Ready {
            plan,
            runtime_scope,
        } => {
            assert_eq!(plan.plan_handle, "gfx100ii:ble-reconnect");
            assert_eq!(plan.mechanism, "ble-reconnect");
            assert!(runtime_scope
                .iter()
                .any(|kv| kv.key == "style" && kv.value == "legacy"));
            assert!(runtime_scope
                .iter()
                .any(|kv| kv.key == "shortSerial" && kv.value == "0C3E"));
            assert!(!runtime_scope.iter().any(|kv| kv.key == "pairingKeyBytes"));
        }
        other => panic!("expected ready, got {other:?}"),
    }

    let red_scope = vec![KeyValue {
        key: "shortSerial".into(),
        value: "ABCDE".into(),
    }];
    assert!(matches!(
        s.reconnect_decision(
            "gfx100ii".into(),
            synthetic_red_startup_advert(),
            red_scope.clone()
        ),
        ReconnectDecision::Wake { .. }
    ));
    assert!(matches!(
        s.reconnect_decision("gfx100ii".into(), synthetic_red_advert(), red_scope),
        ReconnectDecision::Ready { .. }
    ));
}

#[test]
fn saved_reconnect_rejects_wrong_identity_and_startup_is_not_discoverable() {
    let s = store();
    assert!(matches!(
        s.recognize(synthetic_legacy_startup_advert()),
        Recognition::NoMatch
    ));
    assert!(matches!(
        s.recognize(synthetic_red_startup_advert()),
        Recognition::NoMatch
    ));
    assert!(matches!(
        s.recognize(synthetic_red_advert()),
        Recognition::NoMatch
    ));
    assert!(matches!(
        s.recognize(synthetic_legacy_awake_advert()),
        Recognition::NoMatch
    ));
    let wrong = vec![KeyValue {
        key: "pairingKeyBytes".into(),
        value: "00000000".into(),
    }];
    assert!(matches!(
        s.reconnect_decision("gfx100ii".into(), synthetic_legacy_startup_advert(), wrong),
        ReconnectDecision::NoMatch
    ));

    let persisted_pairing_key = vec![KeyValue {
        key: "pairingKeyBytes".into(),
        value: "44732a80".into(),
    }];
    assert!(matches!(
        s.reconnect_decision(
            "gfx100ii".into(),
            synthetic_legacy_pairing_advert(),
            persisted_pairing_key,
        ),
        ReconnectDecision::NoMatch
    ));

    for persisted_scope in [
        vec![],
        vec![KeyValue {
            key: "shortSerial".into(),
            value: "FFFF".into(),
        }],
    ] {
        assert!(matches!(
            s.reconnect_decision(
                "gfx100ii".into(),
                synthetic_legacy_awake_advert(),
                persisted_scope,
            ),
            ReconnectDecision::NoMatch
        ));
    }

    let malformed_name = ScanObservation::BleAdvert {
        service_uuids: vec!["AF854C2E-B214-458E-97E2-912C4ECF2CB8".into()],
        manufacturer_data: None,
        service_data: vec![],
        local_name: Some("GFX100 II".into()),
        tx_power: None,
        ad_records: vec![],
    };
    assert!(matches!(
        s.reconnect_decision(
            "gfx100ii".into(),
            malformed_name,
            vec![KeyValue {
                key: "shortSerial".into(),
                value: "GFX1".into(),
            }],
        ),
        ReconnectDecision::NoMatch
    ));
}

#[test]
fn legacy_advert_recognised_as_gfx100ii_with_legacy_style() {
    let s = store();
    match s.recognize(synthetic_legacy_pairing_advert()) {
        Recognition::Candidate {
            model,
            connection,
            confidence,
            runtime_scope,
            ..
        } => {
            assert_eq!(model, "gfx100ii");
            assert_eq!(connection, "ble");
            assert!(matches!(confidence, Confidence::High));
            assert!(
                runtime_scope
                    .iter()
                    .any(|kv| kv.key == "style" && kv.value == "legacy"),
                "scope: {runtime_scope:?}"
            );
            assert!(
                runtime_scope
                    .iter()
                    .any(|kv| kv.key == "pairingKeyBytes" && kv.value == "44732a80"),
                "scope: {runtime_scope:?}"
            );
        }
        other => panic!("expected Candidate, got {other:?}"),
    }
}

#[test]
fn red_advert_recognised_as_gfx100ii_with_red_style_and_short_serial() {
    let s = store();
    match s.recognize(synthetic_red_pairing_advert()) {
        Recognition::Candidate {
            model,
            connection,
            runtime_scope,
            ..
        } => {
            assert_eq!(model, "gfx100ii");
            assert_eq!(connection, "ble");
            assert!(runtime_scope
                .iter()
                .any(|kv| kv.key == "style" && kv.value == "red"));
            // The 5 ASCII pairing-key bytes are bound to BOTH slots so iOS
            // can persist the saved entry by short serial without re-decoding.
            assert!(runtime_scope
                .iter()
                .any(|kv| kv.key == "pairingKeyBytes" && kv.value == "ABCDE"));
            assert!(runtime_scope
                .iter()
                .any(|kv| kv.key == "shortSerial" && kv.value == "ABCDE"));
        }
        other => panic!("expected Candidate, got {other:?}"),
    }
}

#[test]
fn unknown_legacy_body_with_file_transfer_service_stays_a_single_candidate() {
    let s = store();
    // A never-seen legacy body advertising SERVICE_FF_FILE_TRANSFER matches
    // gfx100ii's name-guard-free legacy pairing signature and the baseline's.
    // Closest-match ranking suppresses the baseline, so the result is a single
    // Candidate rather than a disambiguation.
    let obs = ble_advert(
        &["AF854C2E-B214-458E-97E2-912C4ECF2CB8"], // SERVICE_FF_FILE_TRANSFER
        0x04D8,
        &[0x02, 0x11, 0x22, 0x33, 0x44],
        Some("1234GFX100S-1234"),
    );
    match s.recognize(obs) {
        Recognition::Candidate { model, .. } => assert_eq!(model, "gfx100ii"),
        other => panic!("expected a single Candidate, got {other:?}"),
    }
}

#[test]
fn specific_model_suppresses_the_baseline_on_a_legacy_pairing_advert() {
    let s = store();
    // The GFX100 II legacy pairing advert (file-transfer service + "GFX100 II"
    // name) matches BOTH gfx100ii's bleLegacyAdvert AND the baseline's
    // bleLegacyPairingAdvert. Closest-match ranking must drop the baseline and
    // leave a single Candidate{gfx100ii} — NOT a Disambiguate (which would
    // break the app's Candidate-only discovery path).
    match s.recognize(synthetic_legacy_pairing_advert()) {
        Recognition::Candidate { model, .. } => assert_eq!(model, "gfx100ii"),
        other => panic!("expected Candidate{{gfx100ii}}, got {other:?}"),
    }
}

#[test]
fn specific_model_suppresses_the_baseline_on_a_red_pairing_advert() {
    let s = store();
    // A RED pairing advert (no service uuids, mfg 0x01 + 5 ASCII) matches
    // gfx100ii's bleRedAdvert AND the baseline's bleRedPairingAdvert (both
    // deliberately service/name-free). Suppression keeps it a single
    // Candidate{gfx100ii}, not a Disambiguate.
    match s.recognize(synthetic_red_pairing_advert()) {
        Recognition::Candidate { model, .. } => assert_eq!(model, "gfx100ii"),
        other => panic!("expected Candidate{{gfx100ii}}, got {other:?}"),
    }
}

#[test]
fn baseline_reconnect_routes_wake_and_ready() {
    let s = store();
    // The family-baseline model carries the same four reconnect signatures as a
    // specific model, body-agnostically. reconnect_decision is model-keyed, so
    // a saved body promoted to `fuji-generic` reconnects through them.
    assert_eq!(
        s.reconnect_policy("fuji-generic".into())
            .unwrap()
            .scan_timeout_ms,
        60_000
    );

    // Legacy startup advert → wake (mechanism ble-wake).
    let legacy_scope = vec![KeyValue {
        key: "pairingKeyBytes".into(),
        value: "44732a80".into(),
    }];
    match s.reconnect_decision(
        "fuji-generic".into(),
        synthetic_legacy_startup_advert(),
        legacy_scope,
    ) {
        ReconnectDecision::Wake { plan, .. } => {
            assert_eq!(plan.plan_handle, "fuji-generic:ble-wake");
            assert_eq!(plan.mechanism, "ble-wake");
        }
        other => panic!("expected Wake, got {other:?}"),
    }

    // Awake red advert → ready (mechanism ble-reconnect).
    let red_scope = vec![KeyValue {
        key: "shortSerial".into(),
        value: "ABCDE".into(),
    }];
    match s.reconnect_decision("fuji-generic".into(), synthetic_red_advert(), red_scope) {
        ReconnectDecision::Ready { plan, .. } => {
            assert_eq!(plan.plan_handle, "fuji-generic:ble-reconnect");
            assert_eq!(plan.mechanism, "ble-reconnect");
        }
        other => panic!("expected Ready, got {other:?}"),
    }
}

#[test]
fn non_fuji_advert_returns_nomatch() {
    let s = store();
    // Wrong company ID + no matching service UUID → NoMatch.
    let obs = ble_advert(
        &["DEADBEEF-0000-1000-8000-00805F9B34FB"],
        0x004C, // Apple
        &[0xFF, 0xFF, 0xFF],
        None,
    );
    assert!(matches!(s.recognize(obs), Recognition::NoMatch));
}

#[test]
fn wrong_company_id_with_payload_shape_that_would_otherwise_match_returns_nomatch() {
    // Guards against the false-positive window the old skip created: an advert
    // whose payload happens to look like a Fuji RED mfg-data (6 bytes,
    // type=0x01 + 5 ASCII) must NOT recognize as a Fuji body if the BT-SIG
    // company-ID isn't 0x04D8.
    let s = store();
    let obs = ble_advert(
        &["123D8F06-62A1-4935-9322-833C531EE225"],
        0x004C, // Apple, not Fuji
        &[0x01, b'A', b'B', b'C', b'D', b'E'],
        Some("GFX100 II"),
    );
    assert!(matches!(s.recognize(obs), Recognition::NoMatch));
}

#[test]
fn legacy_signature_wins_over_red_when_both_could_match_per_file_order() {
    // An advert with SERVICE_FF_FILE_TRANSFER + 6-byte mfg-data could
    // structurally satisfy the RED signature (it doesn't require absence
    // of the legacy UUID — that's implicit via §11.7 file-order
    // precedence). The legacy signature is declared first, so it should
    // be the one that fires.
    //
    // We construct a synthetic advert where both signatures would
    // structurally pass except for one detail: RED expects length==6 but
    // legacy expects min_length>=5. A 5-byte mfg-data with legacy type byte
    // passes legacy and FAILS red (length mismatch).
    let s = store();
    let obs = ble_advert(
        &["AF854C2E-B214-458E-97E2-912C4ECF2CB8"],
        0x04D8, // Fujifilm
        &[0x02, 0x11, 0x22, 0x33, 0x44],
        None,
    );
    let Recognition::Candidate { runtime_scope, .. } = s.recognize(obs) else {
        panic!("expected Candidate");
    };
    assert!(
        runtime_scope
            .iter()
            .any(|kv| kv.key == "style" && kv.value == "legacy"),
        "legacy wins per file-declaration order (§11.7)"
    );
}

// ---------------------------------------------------------------------------
// establishment() — plan returned with structured step values
// ---------------------------------------------------------------------------

#[test]
fn establishment_returns_walkable_ble_plan() {
    let s = store();
    let scope = match s.recognize(synthetic_legacy_pairing_advert()) {
        Recognition::Candidate { runtime_scope, .. } => runtime_scope,
        other => panic!("expected Candidate, got {other:?}"),
    };
    let plan = s
        .establishment("gfx100ii".into(), "ble".into(), scope)
        .expect("plan present");
    assert_eq!(plan.plan_handle, "gfx100ii:ble");
    assert_eq!(plan.mechanism, "ble-pair");
    assert!(plan.prerequisite.is_none());
    assert!(!plan.steps.is_empty());
    // #91: ble-pair is the initial establishment (not on-demand) and declares the
    // identity material the host caches for a later ble-reconnect.
    assert!(!plan.on_demand);
    assert_eq!(
        plan.persist,
        vec![
            "pairingKeyBytes".to_string(),
            "style".to_string(),
            "cameraSerial".to_string(),
        ],
    );
    assert!(matches!(
        &plan.activities[0],
        ConnectionActivityDescriptor {
            id,
            version: 1,
            display_role: ConnectionActivityDisplayRole::Connecting,
            binding: ConnectionActivityBinding::ExecutorSpan {
                sequence: ConnectionActivitySequence::Steps,
                start_step: 0,
                end_step_exclusive: 2,
            },
            ..
        } if id == "camera.link.connect"
    ));

    // Step 0: bleConnect with no fields.
    assert!(matches!(plan.steps[0], Step::BleConnect { .. }));

    // Step 1: explicit service discovery; bleConnect is connection-only.
    assert!(matches!(plan.steps[1], Step::BleDiscoverServices { .. }));

    // Step 2: bleRead protectedSerialString tolerant retries=20.
    match &plan.steps[2] {
        Step::BleRead {
            gatt,
            encoding,
            capture_as,
            transform,
            opts,
        } => {
            assert_eq!(gatt, "00002A25-0000-1000-8000-00805F9B34FB");
            assert_eq!(encoding, "bytes");
            assert_eq!(capture_as, "cameraSerial");
            assert!(transform.is_empty(), "raw read, no transform chain");
            assert!(opts.tolerant);
            assert_eq!(opts.retries, 20);
            assert_eq!(opts.retry_delay_ms, 1000);
        }
        other => panic!("expected BleRead, got {other:?}"),
    }

    // Step 3: bleWrite pairingKey ← captured pairingKeyBytes.
    match &plan.steps[3] {
        Step::BleWrite { gatt, value, .. } => {
            assert_eq!(gatt, "ABA356EB-9633-4E60-B73F-F52516DBD671");
            match value {
                StepValue::Captured { name, .. } => assert_eq!(name, "pairingKeyBytes"),
                other => panic!("expected Captured, got {other:?}"),
            }
        }
        other => panic!("expected BleWrite, got {other:?}"),
    }

    // Step 4: bleWrite deviceNameString ← runtime terminalName utf8.
    match &plan.steps[4] {
        Step::BleWrite { gatt, value, .. } => {
            assert_eq!(gatt, "85B9163E-62D1-49FF-A6F5-054B4630D4A1");
            match value {
                StepValue::Runtime { slot, encoding, .. } => {
                    assert_eq!(slot, "terminalName");
                    assert_eq!(encoding.as_deref(), Some("utf8"));
                }
                other => panic!("expected Runtime, got {other:?}"),
            }
        }
        other => panic!("expected BleWrite, got {other:?}"),
    }

    // Step 5: if style == red — RED identification number exchange.
    match &plan.steps[5] {
        Step::If {
            condition,
            then_branch,
            else_branch,
            tolerant,
        } => {
            assert_eq!(condition.field, "style");
            assert!(matches!(condition.op, PredicateOp::Eq));
            assert_eq!(condition.value, "red");
            assert!(*tolerant);
            assert_eq!(then_branch.len(), 2, "read + echo write");
            assert!(else_branch.is_empty());
            // then[0] = bleRead deviceIdentificationNumber u32 retries 20.
            match &then_branch[0] {
                Step::BleRead {
                    gatt,
                    encoding,
                    capture_as,
                    transform,
                    opts,
                } => {
                    assert_eq!(gatt, "F557D96B-8284-4667-8793-B971C1DECA2A");
                    assert_eq!(encoding, "u32");
                    assert_eq!(capture_as, "idNumber");
                    assert!(transform.is_empty(), "raw read, no transform chain");
                    assert!(opts.tolerant);
                    assert_eq!(opts.retries, 20);
                }
                other => panic!("expected BleRead inside then, got {other:?}"),
            }
        }
        other => panic!("expected If, got {other:?}"),
    }

    let confirmation = plan.steps.iter().find_map(|step| match step {
        Step::BleRead {
            capture_as, opts, ..
        } if capture_as == "transferState" => opts.confirms,
        _ => None,
    });
    assert_eq!(
        confirmation,
        Some(StepConfirmation::Registration),
        "the loader-visible StepOptions mirror carries the registration anchor"
    );
}

#[test]
fn establishment_returns_none_for_connection_without_plan() {
    // The daemon-attached pass-through connection owns no establishment plan:
    // the platform daemon already opened the session it attaches to.
    let s = store();
    assert!(s
        .establishment("gfx100ii".into(), "usb-passthrough".into(), vec![])
        .is_none());
}

#[test]
fn establishment_returns_none_for_unknown_model() {
    let s = store();
    assert!(s
        .establishment("xt5".into(), "ble".into(), vec![])
        .is_none());
}

#[test]
fn establishment_app_connection_returns_wifi_ap_plan() {
    // Issue #47: the `app` (BLE-initiated WiFi-AP) connection now resolves to
    // the ble-establish-wifi-ap plan — previously nil, so a paired camera had
    // nowhere to go. The mechanism is read from the body manifest
    // (connections.app.establishment) then looked up in the index registry.
    let s = store();
    let plan = s
        .establishment("gfx100ii".into(), "app".into(), vec![])
        .expect("app connection resolves to the ble-establish-wifi-ap plan");
    assert_eq!(plan.plan_handle, "gfx100ii:app");
    assert_eq!(plan.mechanism, "ble-establish-wifi-ap");
    assert_eq!(plan.prerequisite.as_deref(), Some("ble-pair"));
    assert_eq!(plan.params, vec!["launchMode".to_string()]);
    // #91: user-initiated from the resting BLE link (NOT auto-chained after
    // ble-pair), and the Wi-Fi creds it reads are flagged for the host to cache.
    assert!(plan.on_demand, "the AP launch is on-demand");
    assert_eq!(
        plan.persist,
        vec!["ssid".to_string(), "passphrase".to_string()]
    );
    assert_eq!(plan.post_exit_readiness.len(), 3);
    assert_eq!(
        plan.activities.len(),
        7,
        "four executor spans plus one checkpoint and two typed host actions"
    );
    assert!(matches!(
        plan.activities.first(),
        Some(ConnectionActivityDescriptor { id, optional: true, .. })
            if id == "camera.ap.reset"
    ));
    assert!(matches!(
        plan.activities.get(5),
        Some(ConnectionActivityDescriptor {
            id,
            version: 2,
            binding: ConnectionActivityBinding::HostEstablishment {
                action: HostEstablishment::NetworkIdentityExact { expected_scope },
            },
            ..
        }) if id == "camera.network.associate" && expected_scope == "ssid"
    ));
    assert!(matches!(
        plan.activities.last(),
        Some(ConnectionActivityDescriptor {
            id,
            version: 2,
            binding: ConnectionActivityBinding::HostEstablishment {
                action: HostEstablishment::RetainedSessionOpen { socket_role: SocketRole::Command },
            },
            ..
        }) if id == "camera.session.open.ap"
    ));
    assert!(matches!(
        plan.post_exit_readiness[0],
        Step::BleConnect { .. }
    ));
    assert!(matches!(
        plan.post_exit_readiness[1],
        Step::BleDiscoverServices { .. }
    ));
    match &plan.post_exit_readiness[2] {
        Step::BleAwaitUntil {
            source: AwaitSource::Notify {
                gatt, seed_read, ..
            },
            until,
            timeout_ms,
            ..
        } => {
            assert_eq!(gatt, "A68E3F66-0FCC-4395-8D4C-AA980B5877FA");
            assert!(*seed_read);
            assert_eq!(until.field, "apState");
            assert_eq!(until.value, "32768");
            assert_eq!(*timeout_ms, 20_000);
        }
        other => panic!("expected post-exit BleAwaitUntil, got {other:?}"),
    }

    // Opens with bleConnect (the BLE link carried over from ble-pair), then arms the
    // AP handoff with the IMAGE_TRANSFER_SETTING prep write (#102) BEFORE the
    // FUNCTION_LAUNCH_REQUEST write (runtime launchMode, u16-le).
    assert!(matches!(plan.steps[0], Step::BleConnect { .. }));
    let write_gatts: Vec<&str> = plan
        .steps
        .iter()
        .filter_map(|s| match s {
            Step::BleWrite { gatt, .. } => Some(gatt.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(
        write_gatts.first(),
        Some(&"98934B2C-756C-4632-AA2F-DCBA1BFEC824"),
        "the IMAGE_TRANSFER_SETTING prep write must come first (#102)"
    );
    let Step::BleAwaitUntil {
        source,
        fail_when,
        capture,
        ..
    } = &plan.steps[2]
    else {
        panic!("plan reads AP state before the command")
    };
    assert!(matches!(source, AwaitSource::Read { .. }));
    assert!(fail_when.is_none());
    assert!(capture
        .iter()
        .any(|capture| capture.name == "apStateBaseline"));
    assert!(matches!(plan.steps[3], Step::BleSubscribe { .. }));

    let retry = plan
        .steps
        .iter()
        .find(|step| matches!(step, Step::Retry { .. }))
        .expect("AP launch is guarded by typed retry control flow");
    let Step::Retry {
        steps,
        when_failure,
        on_failure,
        retry_when,
        max_attempts,
        retry_delay_ms,
        failure_context,
        ..
    } = retry
    else {
        unreachable!()
    };
    assert_eq!(*when_failure, ExecutorStepFailureKind::ConditionRejected);
    assert_eq!(*max_attempts, 2);
    assert_eq!(*retry_delay_ms, 200);
    assert_eq!(retry_when.field, "stateErrorDetails");
    assert_eq!(retry_when.value, "2");
    assert_eq!(failure_context, &["apState", "stateErrorDetails"]);
    assert!(on_failure.is_empty());

    let (gatt, value, notification_fence) = steps
        .iter()
        .find_map(|step| match step {
            Step::BleWrite {
                gatt,
                value,
                notification_fence,
                ..
            } => Some((gatt, value, notification_fence)),
            _ => None,
        })
        .expect("retry body writes the function-launch request");
    assert_eq!(gatt, "600655E6-3637-42F1-8FB2-44EFC5C63B13");
    assert_eq!(
        notification_fence.as_deref(),
        Some("A68E3F66-0FCC-4395-8D4C-AA980B5877FA")
    );
    match value {
        StepValue::Runtime { slot, encoding, .. } => {
            assert_eq!(slot, "launchMode");
            assert_eq!(encoding.as_deref(), Some("u16-le"));
        }
        other => panic!("expected Runtime launch value, got {other:?}"),
    }

    // The post-command await consumes notifications only, so callback transports
    // cannot misclassify a racing notification as the baseline read response.
    let until = steps
        .iter()
        .find_map(|s| match s {
            Step::BleAwaitUntil {
                source,
                capture,
                until,
                fail_when,
                failure_evidence,
                interval_ms,
                ..
            } => {
                match source {
                    AwaitSource::Notify {
                        gatt, seed_read, ..
                    } => {
                        assert_eq!(gatt, "A68E3F66-0FCC-4395-8D4C-AA980B5877FA");
                        assert!(!*seed_read, "post-command await is notification-only");
                    }
                    other => panic!("expected apState notify source, got {other:?}"),
                }
                assert_eq!(*interval_ms, 0, "notify sources do not carry poll cadence");
                assert!(capture.iter().any(|c| c.name == "apState"));
                let fail_when = fail_when.as_ref().expect("NotLaunched probes details");
                assert_eq!(fail_when.field, "apStateRaw");
                assert_eq!(fail_when.value, "0080");
                let evidence = failure_evidence
                    .as_ref()
                    .expect("NotLaunched requires nonzero detail evidence");
                assert_eq!(evidence.when.field, "stateErrorDetails");
                assert_eq!(evidence.when.value, "0");
                assert_eq!(evidence.steps.len(), 1);
                Some(until.clone())
            }
            _ => None,
        })
        .expect("ble-establish-wifi-ap awaits apState");
    assert_eq!(until.field, "apStateRaw");
    assert_eq!(until.value, "0180");

    // The credential reads bind ssid + passphrase — the consumer contract.
    // Both decode as utf8-cstring so trailing NUL padding never reaches the
    // consumer (#87); the passphrase read is tolerant so an open/legacy-fw AP
    // that omits the characteristic doesn't abort the handoff (#85), while the
    // SSID stays required.
    let reads: Vec<(&str, &str, bool)> = plan
        .steps
        .iter()
        .filter_map(|s| match s {
            Step::BleRead {
                capture_as,
                encoding,
                opts,
                ..
            } => Some((capture_as.as_str(), encoding.as_str(), opts.tolerant)),
            _ => None,
        })
        .collect();
    assert!(
        reads.contains(&("ssid", "utf8-cstring", false)),
        "ssid: required utf8-cstring read; got {reads:?}",
    );
    assert!(
        reads.contains(&("passphrase", "utf8-cstring", true)),
        "passphrase: tolerant utf8-cstring read; got {reads:?}",
    );
}

// ---------------------------------------------------------------------------
// refine_establishment() — §11.5 explicit no-change / error contract
// ---------------------------------------------------------------------------

#[test]
fn refine_establishment_returns_no_change_when_no_overlay_matches() {
    // Current YAML has no firmware-branching establishment overlays, so a valid
    // refinement request keeps the existing tail instead of returning a silent
    // optional None.
    let s = store();
    let tail = s.refine_establishment("gfx100ii:ble".into(), "2.30".into(), vec![], 2);
    assert!(matches!(tail, Ok(EstablishmentRefinement::NoChange)));

    for handle in ["gfx100ii:ble-wake", "gfx100ii:ble-reconnect"] {
        let result = s.refine_establishment(handle.into(), "2.30".into(), vec![], 0);
        assert!(
            matches!(result, Ok(EstablishmentRefinement::NoChange)),
            "mechanism-backed handle {handle} resolves"
        );
    }
}

#[test]
fn refine_establishment_rejects_bad_handles_and_indices() {
    let s = store();
    let malformed = s.refine_establishment("gfx100ii".into(), "2.30".into(), vec![], 0);
    assert!(matches!(
        malformed,
        Err(EstablishmentError::InvalidPlanHandle(_))
    ));

    let unknown = s.refine_establishment("gfx100ii:missing".into(), "2.30".into(), vec![], 0);
    assert!(matches!(unknown, Err(EstablishmentError::UnknownPlan(_))));

    let connection_without_plan =
        s.refine_establishment("gfx100ii:usb-passthrough".into(), "2.30".into(), vec![], 0);
    assert!(matches!(
        connection_without_plan,
        Err(EstablishmentError::UnknownPlan(_))
    ));

    let bad_index = s.refine_establishment("gfx100ii:ble".into(), "2.30".into(), vec![], 999);
    assert!(matches!(
        bad_index,
        Err(EstablishmentError::InvalidNextStepIndex(_))
    ));
}

// ---------------------------------------------------------------------------
// transform: surfaces to the FFI on the RED echo write
// ---------------------------------------------------------------------------

#[test]
fn red_echo_write_value_carries_bit_or_transform_through_ffi() {
    let s = store();
    let plan = s
        .establishment("gfx100ii".into(), "ble".into(), vec![])
        .expect("plan present");
    let red_if = plan
        .steps
        .iter()
        .find_map(|s| match s {
            Step::If {
                condition,
                then_branch,
                ..
            } if condition.value == "red" => Some(then_branch),
            _ => None,
        })
        .expect("red if-block in FFI plan");
    let echo_write = red_if
        .iter()
        .find_map(|s| match s {
            Step::BleWrite { value, .. } => Some(value),
            _ => None,
        })
        .expect("echo bleWrite");
    match echo_write {
        StepValue::Captured { name, transform } => {
            assert_eq!(name, "idNumber");
            assert!(
                matches!(transform.as_slice(), [Transform::BitOr { operand }] if *operand == 0x20000000),
                "got: {transform:?}"
            );
        }
        other => panic!("expected Captured with transform, got {other:?}"),
    }
}

#[test]
fn acquire_step_variants_cross_the_mirror() {
    for (case, step_yaml) in [
        (
            "acquire",
            r#"            - acquire:
                name: serial
                from:
                  bleRead:
                    gatt: c
                    encoding: ascii
                    captureAs: serialBytes"#,
        ),
        (
            "bleAdvert",
            r#"            - acquireFirmware:
                from:
                  bleAdvert:
                    offset: 3
                    length: 2
                    encoding: u16-le"#,
        ),
        (
            "bleRead",
            r#"            - acquireFirmware:
                from:
                  bleRead:
                    gatt: c
                    encoding: utf8-cstring"#,
        ),
        (
            "userPrompt",
            r#"            - acquireFirmware:
                from:
                  userPrompt:
                    text: "Enter camera firmware""#,
        ),
    ] {
        let store = tm1_store_with_step(step_yaml);
        let plan = store
            .establishment("tm1".into(), "ble".into(), vec![])
            .unwrap_or_else(|| panic!("{case} plan resolves"));

        match (case, &plan.steps[0]) {
            ("acquire", Step::Acquire { name, from, .. }) => {
                assert_eq!(name, "serial");
                assert_eq!(from.len(), 1);
                match &from[0] {
                    Step::BleRead {
                        gatt,
                        encoding,
                        capture_as,
                        ..
                    } => {
                        assert_eq!(gatt, "00002A25-0000-1000-8000-00805F9B34FB");
                        assert_eq!(encoding, "ascii");
                        assert_eq!(capture_as, "serialBytes");
                    }
                    other => panic!("expected nested BleRead, got {other:?}"),
                }
            }
            (
                "bleAdvert",
                Step::AcquireFirmware {
                    from:
                        AcquireSource::BleAdvert {
                            offset,
                            length,
                            encoding,
                        },
                    ..
                },
            ) => {
                assert_eq!(*offset, 3);
                assert_eq!(*length, 2);
                assert_eq!(encoding, "u16-le");
            }
            (
                "bleRead",
                Step::AcquireFirmware {
                    from: AcquireSource::BleRead { gatt, encoding },
                    ..
                },
            ) => {
                assert_eq!(gatt, "00002A25-0000-1000-8000-00805F9B34FB");
                assert_eq!(encoding, "utf8-cstring");
            }
            (
                "userPrompt",
                Step::AcquireFirmware {
                    from: AcquireSource::UserPrompt { text },
                    ..
                },
            ) => assert_eq!(text, "Enter camera firmware"),
            (_, other) => panic!("{case} crossed as the wrong FFI step: {other:?}"),
        }
    }
}

#[test]
fn step_value_template_crosses_the_mirror() {
    let store = tm1_store_with_step(
        r#"            - bleWrite:
                gatt: c
                value:
                  template: "camera-{model}-ready"
                  transform: { reverseBytes: {} }"#,
    );
    let plan = store
        .establishment("tm1".into(), "ble".into(), vec![])
        .expect("template plan resolves");

    match &plan.steps[0] {
        Step::BleWrite {
            gatt,
            value: StepValue::Template { value, transform },
            ..
        } => {
            assert_eq!(gatt, "00002A25-0000-1000-8000-00805F9B34FB");
            assert_eq!(value, "camera-{model}-ready");
            assert!(matches!(transform.as_slice(), [Transform::ReverseBytes]));
        }
        other => panic!("expected BleWrite with Template value, got {other:?}"),
    }
}

#[test]
fn ble_notify_until_variants_cross_the_mirror() {
    for (case, until_yaml) in [
        ("any", "any"),
        ("equals", r#"{ equals: "0x8001", encoding: bytes-raw }"#),
        ("matches", r#"{ matches: "^READY-[0-9]+$" }"#),
    ] {
        let step_yaml = format!(
            r#"            - bleNotify:
                gatt: c
                until: {until_yaml}
                timeoutMs: 5000"#
        );
        let store = tm1_store_with_step(&step_yaml);
        let plan = store
            .establishment("tm1".into(), "ble".into(), vec![])
            .unwrap_or_else(|| panic!("{case} plan resolves"));

        match (case, &plan.steps[0]) {
            (
                "any",
                Step::BleNotify {
                    until: BleNotifyUntil::Any,
                    timeout_ms,
                    ..
                },
            ) => assert_eq!(*timeout_ms, 5000),
            (
                "equals",
                Step::BleNotify {
                    until: BleNotifyUntil::Equals { value },
                    timeout_ms,
                    ..
                },
            ) => {
                assert_eq!(value, &[0x80, 0x01]);
                assert_eq!(*timeout_ms, 5000);
            }
            (
                "matches",
                Step::BleNotify {
                    until: BleNotifyUntil::Matches { pattern },
                    timeout_ms,
                    ..
                },
            ) => {
                assert_eq!(pattern, "^READY-[0-9]+$");
                assert_eq!(*timeout_ms, 5000);
            }
            (_, other) => panic!("{case} crossed as the wrong FFI notify: {other:?}"),
        }
    }
}

#[test]
fn transform_variants_cross_the_mirror() {
    for (case, transform_yaml) in [
        ("bitAnd", "{ bitAnd: 0xff00 }"),
        ("slice", "{ slice: { at: 2, length: 3 } }"),
        ("reverseBytes", "{ reverseBytes: {} }"),
        ("uuidFromBytes", "{ uuidFromBytes: {} }"),
        ("bits", "{ bits: { mask: 0xf0, shift: 4 } }"),
    ] {
        let step_yaml = format!(
            r#"            - bleWrite:
                gatt: c
                value:
                  captured: payload
                  transform: {transform_yaml}"#
        );
        let store = tm1_store_with_step(&step_yaml);
        let plan = store
            .establishment("tm1".into(), "ble".into(), vec![])
            .unwrap_or_else(|| panic!("{case} plan resolves"));
        let transform = match &plan.steps[0] {
            Step::BleWrite {
                value: StepValue::Captured { name, transform },
                ..
            } => {
                assert_eq!(name, "payload");
                assert_eq!(transform.len(), 1);
                &transform[0]
            }
            other => panic!("expected transformed Captured value, got {other:?}"),
        };

        match (case, transform) {
            ("bitAnd", Transform::BitAnd { operand }) => assert_eq!(*operand, 0xff00),
            (
                "slice",
                Transform::Slice {
                    at,
                    length: Some(length),
                },
            ) => {
                assert_eq!(*at, 2);
                assert_eq!(*length, 3);
            }
            ("reverseBytes", Transform::ReverseBytes)
            | ("uuidFromBytes", Transform::UuidFromBytes) => {}
            ("bits", Transform::Bits { mask, shift }) => {
                assert_eq!(*mask, 0xf0);
                assert_eq!(*shift, 4);
            }
            (_, other) => panic!("{case} crossed as the wrong FFI transform: {other:?}"),
        }
    }
}

#[test]
fn predicate_op_variants_cross_the_mirror() {
    for (operator, expected) in [
        ("ne", PredicateOp::Ne),
        ("gt", PredicateOp::Gt),
        ("gte", PredicateOp::Gte),
        ("lt", PredicateOp::Lt),
        ("lte", PredicateOp::Lte),
        ("in", PredicateOp::In),
    ] {
        let step_yaml = format!(
            r#"            - if:
                condition: {{ status: {{ {operator}: 7 }} }}
                then:
                  - bleConnect: {{}}"#
        );
        let store = tm1_store_with_step(&step_yaml);
        let plan = store
            .establishment("tm1".into(), "ble".into(), vec![])
            .unwrap_or_else(|| panic!("{operator} plan resolves"));

        match &plan.steps[0] {
            Step::If {
                condition,
                then_branch,
                else_branch,
                ..
            } => {
                assert_eq!(condition.field, "status");
                assert_eq!(
                    std::mem::discriminant(&condition.op),
                    std::mem::discriminant(&expected),
                    "{operator} crossed as the wrong FFI predicate op"
                );
                assert_eq!(condition.value, "7");
                assert!(matches!(then_branch.as_slice(), [Step::BleConnect { .. }]));
                assert!(else_branch.is_empty());
            }
            other => panic!("expected If for {operator}, got {other:?}"),
        }
    }
}

// ---------------------------------------------------------------------------
// CCCD mode + bleNotify field captures cross the seam (multivendor pass)
// ---------------------------------------------------------------------------

#[test]
fn cccd_mode_and_notify_captures_surface_through_ffi() {
    let index_yaml = r#"
manufacturer: TESTCO
families:
  test:
    ble:
      gatt: { c: "00002A25-0000-1000-8000-00805F9B34FB" }
      advert: { manufacturerCompanyId: 1 }
      establishments:
        test:
          mechanism: test
          activities:
            - id: camera.test.notify
              version: 1
              displayRole: preparingConnection
              defaultExpectedDurationMs: 1
              interactionRequired: false
              executorSpan: { sequence: steps, startStep: 0, endStepExclusive: 2 }
          steps:
            - bleSubscribe: { gatt: c, timeoutMs: 3000, mode: indicate }
            - bleNotify:
                gatt: c
                until: any
                capture:
                  - at: 3
                    transform: { dropPrefix: 1 }
                    encoding: ascii
                    name: ssid
                timeoutMs: 5000
models:
  - id: tm1
    displayName: "Test"
    inherits: [test]
    manifest: tm1.yaml
"#;
    let s = ConfigStore::from_manufacturer_index(
        index_yaml.to_string(),
        vec![KeyValue {
            key: "tm1".to_string(),
            value: tm1_body(),
        }],
    )
    .expect("synthetic index loads");
    let plan = s
        .establishment("tm1".into(), "ble".into(), vec![])
        .expect("plan present");
    match &plan.steps[0] {
        Step::BleSubscribe { mode, .. } => assert!(matches!(mode, CccdMode::Indicate)),
        other => panic!("expected BleSubscribe, got {other:?}"),
    }
    match &plan.steps[1] {
        Step::BleNotify {
            mode,
            capture,
            capture_as,
            ..
        } => {
            assert!(matches!(mode, CccdMode::Notify), "default mode is notify");
            assert!(capture_as.is_none());
            assert_eq!(capture.len(), 1);
            assert_eq!(capture[0].at, 3);
            assert_eq!(capture[0].length, None);
            assert_eq!(capture[0].encoding, "ascii");
            assert_eq!(capture[0].name, "ssid");
            assert!(
                matches!(
                    capture[0].transform.as_slice(),
                    [Transform::DropPrefix { count: 1 }]
                ),
                "got: {:?}",
                capture[0].transform
            );
        }
        other => panic!("expected BleNotify, got {other:?}"),
    }
}

#[test]
fn malformed_ble_write_literal_is_a_load_error() {
    let index_yaml = r#"
manufacturer: TESTCO
families:
  test:
    ble:
      gatt: { c: "00002A25-0000-1000-8000-00805F9B34FB" }
      establishments:
        test:
          mechanism: test
          activities:
            - id: camera.test.write
              version: 1
              displayRole: preparingConnection
              defaultExpectedDurationMs: 1
              interactionRequired: false
              executorSpan: { sequence: steps, startStep: 0, endStepExclusive: 1 }
          steps:
            - bleWrite: { gatt: c, value: { literal: "0xzz" } }
models:
  - id: tm1
    displayName: "Test"
    inherits: [test]
    manifest: tm1.yaml
"#;
    let error = match ConfigStore::from_manufacturer_index(
        index_yaml.to_string(),
        vec![KeyValue {
            key: "tm1".into(),
            value: tm1_body(),
        }],
    ) {
        Ok(_) => panic!("malformed BLE write literal must fail store construction"),
        Err(error) => error,
    };
    assert!(
        matches!(
        &error,
        ConfigError::Contract(message)
            if message.contains("step literal") && message.contains("0xzz")
        ),
        "unexpected error: {error}"
    );
}

#[test]
fn malformed_post_exit_readiness_literal_is_a_load_error() {
    let index_yaml = r#"
manufacturer: TESTCO
families:
  test:
    ble:
      gatt: { c: "00002A25-0000-1000-8000-00805F9B34FB" }
      establishments:
        test:
          mechanism: test
          activities:
            - id: camera.test.readiness
              version: 1
              displayRole: preparingConnection
              defaultExpectedDurationMs: 1
              interactionRequired: false
              executorSpan: { sequence: postExitReadiness, startStep: 0, endStepExclusive: 1 }
            - id: camera.test.write
              version: 1
              displayRole: preparingConnection
              defaultExpectedDurationMs: 1
              interactionRequired: false
              executorSpan: { sequence: steps, startStep: 0, endStepExclusive: 1 }
          steps:
            - bleConnect: {}
          postExitReadiness:
            - bleWrite: { gatt: c, value: { literal: "0xzz" } }
models:
  - id: tm1
    displayName: "Test"
    inherits: [test]
    manifest: tm1.yaml
"#;
    let error = match ConfigStore::from_manufacturer_index(
        index_yaml.to_string(),
        vec![KeyValue {
            key: "tm1".into(),
            value: tm1_body(),
        }],
    ) {
        Ok(_) => panic!("malformed postExitReadiness literal must fail store construction"),
        Err(error) => error,
    };
    assert!(
        matches!(
        &error,
        ConfigError::Contract(message)
            if message.contains("postExitReadiness") && message.contains("0xzz")
        ),
        "unexpected error: {error}"
    );
}

#[test]
fn malformed_ble_action_literal_is_a_load_error() {
    let index_yaml = r#"
manufacturer: TESTCO
families:
  test:
    ble:
      gatt: { c: "00002A25-0000-1000-8000-00805F9B34FB" }
      establishments:
        test:
          mechanism: test
          activities:
            - id: camera.test.write
              version: 1
              displayRole: preparingConnection
              defaultExpectedDurationMs: 1
              interactionRequired: false
              executorSpan: { sequence: steps, startStep: 0, endStepExclusive: 1 }
          steps:
            - bleConnect: {}
      actions:
        bad-action:
          steps:
            - bleWrite: { gatt: c, value: { literal: "0xzz" } }
models:
  - id: tm1
    displayName: "Test"
    inherits: [test]
    manifest: tm1.yaml
"#;
    let error = match ConfigStore::from_manufacturer_index(
        index_yaml.to_string(),
        vec![KeyValue {
            key: "tm1".into(),
            value: tm1_body(),
        }],
    ) {
        Ok(_) => panic!("malformed BLE action literal must fail store construction"),
        Err(error) => error,
    };
    assert!(
        matches!(
        &error,
        ConfigError::Contract(message)
            if message.contains("action `bad-action`") && message.contains("0xzz")
        ),
        "unexpected error: {error}"
    );
}

#[test]
fn overflowing_ble_notify_equals_is_a_load_error() {
    let index_yaml = r#"
manufacturer: TESTCO
families:
  test:
    ble:
      gatt: { c: "00002A25-0000-1000-8000-00805F9B34FB" }
      establishments:
        test:
          mechanism: test
          activities:
            - id: camera.test.notify
              version: 1
              displayRole: preparingConnection
              defaultExpectedDurationMs: 1
              interactionRequired: false
              executorSpan: { sequence: steps, startStep: 0, endStepExclusive: 1 }
          steps:
            - bleNotify:
                gatt: c
                until: { equals: 65536, encoding: u16-le }
                timeoutMs: 1000
models:
  - id: tm1
    displayName: "Test"
    inherits: [test]
    manifest: tm1.yaml
"#;
    let error = match ConfigStore::from_manufacturer_index(
        index_yaml.to_string(),
        vec![KeyValue {
            key: "tm1".into(),
            value: tm1_body(),
        }],
    ) {
        Ok(_) => panic!("overflowing BLE notify value must fail store construction"),
        Err(error) => error,
    };
    assert!(
        matches!(
        &error,
        ConfigError::Contract(message)
            if message.contains("BLE notify equals value")
                && message.contains("65536")
                && message.contains("U16Le")
        ),
        "unexpected error: {error}"
    );
}

#[test]
fn fuji_cccd_finalization_subscribes_default_to_notify_mode() {
    let s = store();
    let plan = s
        .establishment("gfx100ii".into(), "ble".into(), vec![])
        .expect("plan present");
    let modes: Vec<_> = plan
        .steps
        .iter()
        .filter_map(|s| match s {
            Step::BleSubscribe { mode, .. } => Some(mode),
            _ => None,
        })
        .collect();
    assert!(!modes.is_empty(), "fuji plan carries bleSubscribe steps");
    assert!(modes.iter().all(|m| matches!(m, CccdMode::Notify)));
}

// ---------------------------------------------------------------------------
// bleRequestMtu + bleDiscoverServices cross the seam (multivendor pass)
// ---------------------------------------------------------------------------

#[test]
fn mtu_and_discover_services_surface_through_ffi() {
    let index_yaml = r#"
manufacturer: TESTCO
families:
  test:
    ble:
      gatt: { c: "00002A25-0000-1000-8000-00805F9B34FB" }
      advert: { manufacturerCompanyId: 1 }
      establishments:
        test:
          mechanism: test
          activities:
            - id: camera.test.setup
              version: 1
              displayRole: preparingConnection
              defaultExpectedDurationMs: 1
              interactionRequired: false
              executorSpan: { sequence: steps, startStep: 0, endStepExclusive: 3 }
          steps:
            - bleConnect: {}
            - bleRequestMtu: { requestedMtu: 158, minimumMtu: 120, tolerant: true }
            - bleDiscoverServices: {}
models:
  - id: tm1
    displayName: "Test"
    inherits: [test]
    manifest: tm1.yaml
"#;
    let s = ConfigStore::from_manufacturer_index(
        index_yaml.to_string(),
        vec![KeyValue {
            key: "tm1".to_string(),
            value: tm1_body(),
        }],
    )
    .expect("synthetic index loads");
    let plan = s
        .establishment("tm1".into(), "ble".into(), vec![])
        .expect("plan present");
    match &plan.steps[1] {
        Step::BleRequestMtu {
            requested_mtu,
            minimum_mtu,
            opts,
        } => {
            assert_eq!(*requested_mtu, 158);
            assert_eq!(*minimum_mtu, Some(120));
            assert!(opts.tolerant);
        }
        other => panic!("expected BleRequestMtu, got {other:?}"),
    }
    assert!(matches!(&plan.steps[2], Step::BleDiscoverServices { .. }));
}

// ---------------------------------------------------------------------------
// blePeripheralName crosses the seam (#403)
// ---------------------------------------------------------------------------

#[test]
fn peripheral_name_surfaces_through_ffi() {
    let index_yaml = r#"
manufacturer: TESTCO
families:
  test:
    ble:
      gatt: { c: "00002A25-0000-1000-8000-00805F9B34FB" }
      advert: { manufacturerCompanyId: 1 }
      establishments:
        test:
          mechanism: test
          activities:
            - id: camera.test.setup
              version: 1
              displayRole: preparingConnection
              defaultExpectedDurationMs: 1
              interactionRequired: false
              executorSpan: { sequence: steps, startStep: 0, endStepExclusive: 2 }
          steps:
            - bleConnect: {}
            - blePeripheralName: { captureAs: cameraName }
models:
  - id: tm1
    displayName: "Test"
    inherits: [test]
    manifest: tm1.yaml
"#;
    let s = ConfigStore::from_manufacturer_index(
        index_yaml.to_string(),
        vec![KeyValue {
            key: "tm1".to_string(),
            value: tm1_body(),
        }],
    )
    .expect("synthetic index loads");
    let plan = s
        .establishment("tm1".into(), "ble".into(), vec![])
        .expect("plan present");
    match &plan.steps[1] {
        Step::BlePeripheralName { capture_as, .. } => {
            assert_eq!(capture_as, "cameraName");
        }
        other => panic!("expected BlePeripheralName, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// §11.14 predicate model crosses the seam (multivendor pass)
// ---------------------------------------------------------------------------

#[test]
fn service_uuid_plus_local_name_recognition_without_manufacturer_data() {
    // The Nikon shape: no manufacturer data anywhere, recognition by service
    // UUID + local-name prefix. Inexpressible pre-predicate-model.
    let index_yaml = r#"
manufacturer: TESTCO
families:
  test:
    ble:
      gatt: { c: "00002A25-0000-1000-8000-00805F9B34FB" }
      advert: {}
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
          steps: [ { bleConnect: {} } ]
models:
  - id: tm1
    displayName: "Test Z9"
    inherits: [test]
    manifest: tm1.yaml
    signatures:
      lss:
        kind: bleAdvert
        require:
          all:
            - serviceUuids: { contains: "0000DE00-3DD4-4255-8D62-6DC7B9BD5561" }
            - localName: { prefix: "Z " }
        capture:
          - { source: localName, encoding: utf8, name: bodyName }
        scope: { vendor: "testco" }
        suggests: { connection: ble, confidence: medium }
"#;
    let s = ConfigStore::from_manufacturer_index(
        index_yaml.to_string(),
        vec![KeyValue {
            key: "tm1".to_string(),
            value: tm1_body(),
        }],
    )
    .expect("synthetic index loads");
    let obs = ScanObservation::BleAdvert {
        service_uuids: vec!["0000de00-3dd4-4255-8d62-6dc7b9bd5561".to_string()],
        manufacturer_data: None,
        service_data: vec![],
        local_name: Some("Z 9".to_string()),
        tx_power: None,
        ad_records: vec![],
    };
    match s.recognize(obs) {
        Recognition::Candidate {
            model,
            runtime_scope,
            ..
        } => {
            assert_eq!(model, "tm1");
            assert!(runtime_scope
                .iter()
                .any(|kv| kv.key == "vendor" && kv.value == "testco"));
            assert!(runtime_scope
                .iter()
                .any(|kv| kv.key == "bodyName" && kv.value == "Z 9"));
        }
        other => panic!("expected Candidate, got {other:?}"),
    }
    // Same advert without the local name → NoMatch (absent-field rule).
    let obs = ScanObservation::BleAdvert {
        service_uuids: vec!["0000de00-3dd4-4255-8d62-6dc7b9bd5561".to_string()],
        manufacturer_data: None,
        service_data: vec![],
        local_name: None,
        tx_power: None,
        ad_records: vec![],
    };
    assert!(matches!(s.recognize(obs), Recognition::NoMatch));
}

// ---------------------------------------------------------------------------
// #311 family-baseline ranking, on a synthetic index (one specific + one
// baseline model) so the closest-match rule is exercised in isolation from the
// real Fuji data.
// ---------------------------------------------------------------------------

/// A synthetic index whose specific model `tm1` and baseline model `tm-generic`
/// (fallback: true, declared last) BOTH match the same advert shape (service
/// `DE00` + Fuji-style company id). The baseline's signature has no name guard.
fn baseline_ranking_index() -> std::sync::Arc<ConfigStore> {
    let index_yaml = r#"
manufacturer: TESTCO
families:
  test:
    ble:
      gatt: { c: "00002A25-0000-1000-8000-00805F9B34FB" }
      advert: { manufacturerCompanyId: 1 }
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
          steps: [ { bleConnect: {} } ]
models:
  - id: tm1
    displayName: "Test One"
    inherits: [test]
    manifest: tm1.yaml
    signatures:
      specific:
        kind: bleAdvert
        require:
          all:
            - serviceUuids: { contains: "0000DE00-3DD4-4255-8D62-6DC7B9BD5561" }
            - localName: { contains: "ONE" }
        scope: { model: "one" }
        suggests: { connection: ble, confidence: high }
  - id: tm-generic
    displayName: "Test camera"
    inherits: [test]
    manifest: tmg.yaml
    fallback: true
    signatures:
      baseline:
        kind: bleAdvert
        require:
          serviceUuids: { contains: "0000DE00-3DD4-4255-8D62-6DC7B9BD5561" }
        scope: { model: "generic" }
        suggests: { connection: ble, confidence: high }
"#;
    let body = |model: &str| {
        format!(
            "schema: camera-config/v1\ncamera:\n  manufacturer: TESTCO\n  model: {model}\nconnections:\n  ble:\n    kind: ble\n    establishment: test\n"
        )
    };
    ConfigStore::from_manufacturer_index(
        index_yaml.to_string(),
        vec![
            KeyValue {
                key: "tm1".to_string(),
                value: body("TM1"),
            },
            KeyValue {
                key: "tm-generic".to_string(),
                value: body("Test camera"),
            },
        ],
    )
    .expect("synthetic baseline index loads")
}

fn de00_advert(local_name: Option<&str>) -> ScanObservation {
    ScanObservation::BleAdvert {
        service_uuids: vec!["0000de00-3dd4-4255-8d62-6dc7b9bd5561".to_string()],
        manufacturer_data: None,
        service_data: vec![],
        local_name: local_name.map(String::from),
        tx_power: None,
        ad_records: vec![],
    }
}

#[test]
fn specific_match_suppresses_the_baseline_leaving_one_candidate() {
    let s = baseline_ranking_index();
    // Both models match this advert; the specific model must win as a single
    // Candidate, NEVER a Disambiguate.
    match s.recognize(de00_advert(Some("MODEL-ONE-42"))) {
        Recognition::Candidate {
            model,
            runtime_scope,
            ..
        } => {
            assert_eq!(model, "tm1");
            assert!(runtime_scope
                .iter()
                .any(|kv| kv.key == "model" && kv.value == "one"));
        }
        other => panic!("expected Candidate{{tm1}}, got {other:?}"),
    }
}

#[test]
fn baseline_only_match_is_a_plain_candidate() {
    let s = baseline_ranking_index();
    // No specific model matches (name lacks "ONE"): the baseline alone matches,
    // so it behaves exactly as a normal single-model match → Candidate.
    match s.recognize(de00_advert(Some("SOMEBODY-ELSE"))) {
        Recognition::Candidate {
            model,
            runtime_scope,
            ..
        } => {
            assert_eq!(model, "tm-generic");
            assert!(runtime_scope
                .iter()
                .any(|kv| kv.key == "model" && kv.value == "generic"));
        }
        other => panic!("expected Candidate{{tm-generic}}, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// bleAwaitUntil crosses the seam (§11.15)
// ---------------------------------------------------------------------------

#[test]
fn ble_await_until_surfaces_through_ffi() {
    let index_yaml = r#"
manufacturer: TESTCO
families:
  test:
    ble:
      gatt:
        statusChar: "0000CC09-0000-1000-8000-00805F9B34FB"
        requestChar: "0000CC08-0000-1000-8000-00805F9B34FB"
      advert: { manufacturerCompanyId: 1 }
      establishments:
        test:
          mechanism: test
          activities:
            - id: camera.test.await
              version: 1
              displayRole: waitingForCamera
              defaultExpectedDurationMs: 1
              interactionRequired: false
              executorSpan: { sequence: steps, startStep: 0, endStepExclusive: 2 }
          steps:
            - bleConnect: {}
            - bleAwaitUntil:
                source: { notify: { gatt: statusChar, mode: indicate, seedRead: true } }
                capture: { at: 0, length: 1, encoding: u8, name: status }
                until: { status: { eq: 1 } }
                onEach:
                  - bleWrite: { gatt: requestChar, value: { literal: "01" } }
                timeoutMs: 5000
                intervalMs: 250
models:
  - id: tm1
    displayName: "Test"
    inherits: [test]
    manifest: tm1.yaml
"#;
    let s = ConfigStore::from_manufacturer_index(
        index_yaml.to_string(),
        vec![KeyValue {
            key: "tm1".to_string(),
            value: tm1_body(),
        }],
    )
    .expect("synthetic index loads");
    let plan = s
        .establishment("tm1".into(), "ble".into(), vec![])
        .expect("plan present");
    match &plan.steps[1] {
        Step::BleAwaitUntil {
            source,
            capture,
            until,
            on_each,
            timeout_ms,
            interval_ms,
            ..
        } => {
            match source {
                AwaitSource::Notify {
                    gatt,
                    mode,
                    seed_read,
                } => {
                    assert_eq!(gatt, "0000CC09-0000-1000-8000-00805F9B34FB");
                    assert!(matches!(mode, CccdMode::Indicate));
                    assert!(*seed_read);
                }
                other => panic!("expected notify source, got {other:?}"),
            }
            assert_eq!(capture.len(), 1);
            assert_eq!(capture[0].name, "status");
            assert_eq!(until.field, "status");
            assert_eq!(*timeout_ms, 5000);
            assert_eq!(*interval_ms, 250);
            // onEach bleWrite, gatt resolved, crosses as a nested Step.
            assert_eq!(on_each.len(), 1);
            match &on_each[0] {
                Step::BleWrite { gatt, .. } => {
                    assert_eq!(gatt, "0000CC08-0000-1000-8000-00805F9B34FB")
                }
                other => panic!("expected bleWrite in onEach, got {other:?}"),
            }
        }
        other => panic!("expected BleAwaitUntil, got {other:?}"),
    }
}

#[test]
fn index_model_refs_enumerates_declared_models_in_order() {
    let refs = index_model_refs(common::data("fuji/index.yaml")).expect("index parses");
    let pairs: Vec<(String, String)> = refs.into_iter().map(|r| (r.id, r.manifest_path)).collect();
    assert_eq!(
        pairs,
        vec![
            ("gfx100ii".to_string(), "gfx100ii/gfx100ii.yaml".to_string()),
            (
                "fuji-generic".to_string(),
                "fuji-generic/fuji-generic.yaml".to_string(),
            ),
        ]
    );
}

#[test]
fn index_model_refs_rejects_malformed_yaml() {
    assert!(index_model_refs("models: [".to_string()).is_err());
}

#[test]
fn nikon_lss_steps_and_pad_right_cross_the_uniffi_mirror_whole() {
    let index_yaml = r#"
manufacturer: TESTCO
families:
  test:
    ble:
      gatt:
        auth: "00002000-3DD4-4255-8D62-6DC7B9BD5561"
        config: "00002004-3DD4-4255-8D62-6DC7B9BD5561"
        clientName: "00002005-3DD4-4255-8D62-6DC7B9BD5561"
      establishments:
        test:
          mechanism: test
          params: [clientDeviceId, clientNonce, clientName]
          activities:
            - { id: camera.test.lss, version: 1, displayRole: connecting, defaultExpectedDurationMs: 1, interactionRequired: false, executorSpan: { sequence: steps, startStep: 0, endStepExclusive: 3 } }
          steps:
            - bleWrite:
                gatt: clientName
                value:
                  runtime: clientName
                  encoding: utf8
                  transform: { padRight: { length: 32, byte: 0 } }
            - nikonLssAuthenticate:
                gatt: auth
                clientDeviceId: { runtime: clientDeviceId, encoding: bytes-raw }
                nonce: { runtime: clientNonce, encoding: bytes-raw }
                timeoutMs: 4321
            - nikonLssReadConnectionConfiguration:
                gatt: config
                flagsCaptureAs: flags
                ssidCaptureAs: ssid
                passwordCaptureAs: password
                securityModeCaptureAs: security
                sppMaxLengthCaptureAs: sppMax
models:
  - id: tm1
    displayName: "Test"
    inherits: [test]
    manifest: tm1.yaml
"#;
    let store = ConfigStore::from_manufacturer_index(
        index_yaml.to_string(),
        vec![KeyValue {
            key: "tm1".into(),
            value: tm1_body(),
        }],
    )
    .expect("synthetic LSS index loads");
    let plan = store
        .establishment("tm1".into(), "ble".into(), vec![])
        .expect("plan present");

    match &plan.steps[0] {
        Step::BleWrite {
            value:
                StepValue::Runtime {
                    transform,
                    slot,
                    encoding,
                },
            ..
        } => {
            assert_eq!(slot, "clientName");
            assert_eq!(encoding.as_deref(), Some("utf8"));
            assert!(matches!(
                transform.as_slice(),
                [Transform::PadRight {
                    length: 32,
                    byte: 0
                }]
            ));
        }
        other => panic!("expected padded client-name write, got {other:?}"),
    }
    match &plan.steps[1] {
        Step::NikonLssAuthenticate {
            gatt,
            client_device_id,
            nonce,
            timeout_ms,
            ..
        } => {
            assert_eq!(gatt, "00002000-3DD4-4255-8D62-6DC7B9BD5561");
            assert!(matches!(
                client_device_id,
                StepValue::Runtime { slot, .. } if slot == "clientDeviceId"
            ));
            assert!(matches!(
                nonce,
                StepValue::Runtime { slot, .. } if slot == "clientNonce"
            ));
            assert_eq!(*timeout_ms, 4321);
        }
        other => panic!("expected Nikon auth FFI step, got {other:?}"),
    }
    match &plan.steps[2] {
        Step::NikonLssReadConnectionConfiguration {
            gatt,
            flags_capture_as,
            ssid_capture_as,
            password_capture_as,
            security_mode_capture_as,
            spp_max_length_capture_as,
            ..
        } => {
            assert_eq!(gatt, "00002004-3DD4-4255-8D62-6DC7B9BD5561");
            assert_eq!(flags_capture_as, "flags");
            assert_eq!(ssid_capture_as, "ssid");
            assert_eq!(password_capture_as, "password");
            assert_eq!(security_mode_capture_as, "security");
            assert_eq!(spp_max_length_capture_as.as_deref(), Some("sppMax"));
        }
        other => panic!("expected Nikon config FFI step, got {other:?}"),
    }
}
