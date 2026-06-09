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
    Observation::BleAdvert {
        service_uuids: vec![
            "AF854C2E-B214-458E-97E2-912C4ECF2CB8".to_string(), // SERVICE_FF_FILE_TRANSFER
            "6514EB81-4E8F-458D-AA2A-E691336CDFAC".to_string(), // CAMERA_CONTROL — harmless
        ],
        // type=0x02 + key bytes (synthetic placeholder values).
        manufacturer_data: vec![0x02, 0x44, 0x73, 0x2a, 0x80],
        local_name: Some("GFX100 II".to_string()),
    }
}

/// A synthetic RED advert: type=0x01 + 5 ASCII bytes (placeholder "ABCDE",
/// the shape of a 5-byte short-serial used as the RED pairing key).
fn synthetic_red_advert() -> Observation {
    Observation::BleAdvert {
        service_uuids: vec![
            // RED bodies advertise CONNECTED_DEVICE_INFORMATION_RED, NOT
            // SERVICE_FF_FILE_TRANSFER (legacy detector). Per READ_THIS_FIRST §2.
            "123D8F06-62A1-4935-9322-833C531EE225".to_string(),
        ],
        manufacturer_data: vec![0x01, b'A', b'B', b'C', b'D', b'E'],
        local_name: Some("GFX100 II".to_string()),
    }
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
    // No matching service UUID + arbitrary mfg-data → NoMatch.
    let obs = Observation::BleAdvert {
        service_uuids: vec!["DEADBEEF-0000-1000-8000-00805F9B34FB".to_string()],
        manufacturer_data: vec![0xFF, 0xFF, 0xFF],
        local_name: None,
    };
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
    let obs = Observation::BleAdvert {
        service_uuids: vec!["AF854C2E-B214-458E-97E2-912C4ECF2CB8".to_string()],
        manufacturer_data: vec![0x02, 0x11, 0x22, 0x33, 0x44],
        local_name: None,
    };
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
            opts,
        } => {
            assert_eq!(gatt, "00002A25-0000-1000-8000-00805F9B34FB");
            assert_eq!(encoding, "bytes");
            assert_eq!(capture_as, "cameraSerial");
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
                    opts,
                } => {
                    assert_eq!(gatt, "F557D96B-8284-4667-8793-B971C1DECA2A");
                    assert_eq!(encoding, "u32");
                    assert_eq!(capture_as, "idNumber");
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
                matches!(transform, Some(ValueTransform::BitOr { operand }) if *operand == 0x20000000),
                "got: {transform:?}"
            );
        }
        other => panic!("expected Captured with transform, got {other:?}"),
    }
}
