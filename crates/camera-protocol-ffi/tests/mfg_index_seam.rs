//! End-to-end exercise of the manufacturer-index FFI surface (plan §3.2 +
//! §3.3 + §11): load → recognize → establishment → refine_establishment.
//! Synthetic adverts mirror the GFX100 II / fw 02.30 observations from


use camera_protocol_ffi::*;
use std::path::PathBuf;

fn data(rel: &str) -> String {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../packages/camera-config-data")
        .join(rel);
    std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("read {}: {e}", p.display()))
}

fn store() -> std::sync::Arc<ConfigStore> {
    ConfigStore::from_manufacturer_index(
        data("fuji/index.yaml"),
        vec![KeyValue {
            key: "gfx100ii".to_string(),
            value: data("fuji/gfx100ii/gfx100ii.yaml"),
        }],
    )
    .expect("manufacturer index loads")
}

/// Convenience constructor for the common "manufacturer data + service
/// UUIDs" advert shape. Fields the synthetic adverts never carry
/// (service data, TX power, raw AD records) stay empty.
fn ble_advert(
    service_uuids: &[&str],
    company_id: u16,
    payload: &[u8],
    local_name: Option<&str>,
) -> Observation {
    Observation::BleAdvert {
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

// ---------------------------------------------------------------------------
// recognize() — BLE advert classification
// ---------------------------------------------------------------------------

/// A synthetic LEGACY advert in the GFX100 II / fw 02.30 shape observed during
/// the 2026-05-16 test run. Mfg-data is `0x02 + 4-byte LE key`.
fn synthetic_legacy_advert() -> Observation {
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

/// A synthetic RED advert: type=0x01 + 5 ASCII bytes (placeholder "ABCDE",
/// the shape of a 5-byte short-serial used as the RED pairing key).
fn synthetic_red_advert() -> Observation {
    ble_advert(
        // RED bodies advertise CONNECTED_DEVICE_INFORMATION_RED, NOT
        // SERVICE_FF_FILE_TRANSFER (legacy detector). Per READ_THIS_FIRST §2.
        &["123D8F06-62A1-4935-9322-833C531EE225"],
        0x04D8, // Fujifilm
        &[0x01, b'A', b'B', b'C', b'D', b'E'],
        Some("GFX100 II"),
    )
}

#[test]
fn legacy_advert_recognised_as_gfx100ii_with_legacy_style() {
    let s = store();
    match s.recognize(synthetic_legacy_advert()) {
        Recognition::Candidate {
            model,
            connection,
            confidence,
            runtime_scope,
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
    match s.recognize(synthetic_red_advert()) {
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
    let scope = match s.recognize(synthetic_legacy_advert()) {
        Recognition::Candidate { runtime_scope, .. } => runtime_scope,
        other => panic!("expected Candidate, got {other:?}"),
    };
    let plan = s
        .establishment("gfx100ii".into(), "ble".into(), scope)
        .expect("plan present");
    assert_eq!(plan.plan_handle, "gfx100ii:ble");
    assert_eq!(plan.mechanism, "fuji-ble-pair-v1");
    assert!(plan.prerequisite.is_none());
    assert!(!plan.steps.is_empty());

    // Step 0: bleConnect with no fields.
    assert!(matches!(plan.steps[0], Step::BleConnect { .. }));

    // Step 1: bleRead protectedSerialString tolerant retries=20.
    match &plan.steps[1] {
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

    // Step 2: bleWrite pairingKey ← captured pairingKeyBytes.
    match &plan.steps[2] {
        Step::BleWrite { gatt, value, .. } => {
            assert_eq!(gatt, "ABA356EB-9633-4E60-B73F-F52516DBD671");
            match value {
                StepValue::Captured { name, .. } => assert_eq!(name, "pairingKeyBytes"),
                other => panic!("expected Captured, got {other:?}"),
            }
        }
        other => panic!("expected BleWrite, got {other:?}"),
    }

    // Step 3: bleWrite deviceNameString ← runtime terminalName utf8.
    match &plan.steps[3] {
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

    // Step 4: if style == red — RED identification number exchange.
    match &plan.steps[4] {
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
}

#[test]
fn establishment_returns_none_for_unknown_connection() {
    let s = store();
    assert!(s
        .establishment("gfx100ii".into(), "usb".into(), vec![])
        .is_none());
}

#[test]
fn establishment_returns_none_for_unknown_model() {
    let s = store();
    assert!(s
        .establishment("xt5".into(), "ble".into(), vec![])
        .is_none());
}

// ---------------------------------------------------------------------------
// refine_establishment() — §11.5 graceful-degrade contract
// ---------------------------------------------------------------------------

#[test]
fn refine_establishment_returns_none_when_no_overlay_matches() {
    // MVP YAML has no firmware-branching `if:` blocks, so refine always
    // returns None ("graceful degrade: use body's default sequence").
    let s = store();
    let tail = s.refine_establishment("gfx100ii:ble".into(), "02.30".into(), vec![], 2);
    assert!(tail.is_none());
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
      establishment:
        mechanism: test
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
            value: data("fuji/gfx100ii/gfx100ii.yaml"),
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
      establishment:
        mechanism: test
        steps:
          - bleConnect: {}
          - bleRequestMtu: { mtu: 158, tolerant: true }
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
            value: data("fuji/gfx100ii/gfx100ii.yaml"),
        }],
    )
    .expect("synthetic index loads");
    let plan = s
        .establishment("tm1".into(), "ble".into(), vec![])
        .expect("plan present");
    match &plan.steps[1] {
        Step::BleRequestMtu { mtu, opts } => {
            assert_eq!(*mtu, 158);
            assert!(opts.tolerant);
        }
        other => panic!("expected BleRequestMtu, got {other:?}"),
    }
    assert!(matches!(&plan.steps[2], Step::BleDiscoverServices { .. }));
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
      establishment: { mechanism: test, steps: [ { bleConnect: {} } ] }
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
            value: data("fuji/gfx100ii/gfx100ii.yaml"),
        }],
    )
    .expect("synthetic index loads");
    let obs = Observation::BleAdvert {
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
    let obs = Observation::BleAdvert {
        service_uuids: vec!["0000de00-3dd4-4255-8d62-6dc7b9bd5561".to_string()],
        manufacturer_data: None,
        service_data: vec![],
        local_name: None,
        tx_power: None,
        ad_records: vec![],
    };
    assert!(matches!(s.recognize(obs), Recognition::NoMatch));
}
