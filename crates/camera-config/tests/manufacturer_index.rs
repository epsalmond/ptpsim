//! Manufacturer-index loader contract tests (plan §2.3 + §11).
//!
//! Exercises the schema additions in `crates/camera-config` against the
//! real `packages/camera-config-data/fuji/index.yaml` plus synthetic fixtures
//! for each validation rule. Order of tests follows the contract list in
//! §11 so a reviewer can pair-read.

use std::collections::BTreeMap;
use std::path::PathBuf;

use camera_config::error::ConfigError;
use camera_config::index::{
    BleNotifyUntil, CccdMode, Confidence, Encoding, PredicateOp, ResolvedManufacturerIndex,
    Signature, Step, StepValue, Transform,
};
use camera_config::ConfigStore;

fn data(rel: &str) -> String {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../packages/camera-config-data")
        .join(rel);
    std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("read {}: {e}", p.display()))
}

fn real_index() -> ResolvedManufacturerIndex {
    ResolvedManufacturerIndex::from_yaml(&data("fuji/index.yaml")).expect("fuji/index.yaml loads")
}

// ---------------------------------------------------------------------------
// §11.9 inheritance
// ---------------------------------------------------------------------------

#[test]
fn family_ble_block_merges_into_gfx100ii_view() {
    let idx = real_index();
    let gfx = idx
        .models
        .iter()
        .find(|m| m.id == "gfx100ii")
        .expect("gfx100ii is in the index");
    let ble = gfx
        .ble
        .as_ref()
        .expect("inherits the fuji family ble block");
    // GATT catalog inherited verbatim.
    assert_eq!(
        ble.gatt.get("pairingKey").map(String::as_str),
        Some("ABA356EB-9633-4E60-B73F-F52516DBD671"),
    );
    assert_eq!(
        ble.gatt.get("deviceNameString").map(String::as_str),
        Some("85B9163E-62D1-49FF-A6F5-054B4630D4A1"),
    );
    assert_eq!(
        ble.gatt
            .get("deviceIdentificationNumber")
            .map(String::as_str),
        Some("F557D96B-8284-4667-8793-B971C1DECA2A"),
    );
    // Advert constants inherited.
    assert_eq!(ble.advert.fuji_company_id, 0x04D8);
    assert_eq!(
        ble.advert.legacy_service_uuid.as_deref(),
        Some("AF854C2E-B214-458E-97E2-912C4ECF2CB8"),
    );
    // Establishment plan inherited.
    assert_eq!(ble.establishment.mechanism, "fuji-ble-pair-v1");
    assert!(
        ble.establishment.steps.len() >= 4,
        "establishment carries the multi-step pair flow"
    );
}

// ---------------------------------------------------------------------------
// §11.3 GATT-name → UUID at index-build
// ---------------------------------------------------------------------------

#[test]
fn gatt_symbolic_names_in_steps_become_uuids() {
    let idx = real_index();
    let gfx = idx.models.iter().find(|m| m.id == "gfx100ii").unwrap();
    let steps = &gfx.ble.as_ref().unwrap().establishment.steps;

    // Step 1 (after bleConnect): bleRead on protectedSerialString.
    let read_serial = steps
        .iter()
        .find_map(|s| match s {
            Step::BleRead(r) if r.capture_as == "cameraSerial" => Some(r),
            _ => None,
        })
        .expect("bleRead on protectedSerialString");
    assert_eq!(read_serial.gatt, "00002A25-0000-1000-8000-00805F9B34FB");
    assert_eq!(read_serial.encoding, Encoding::Bytes);
    assert!(read_serial.opts.tolerant);
    assert_eq!(read_serial.opts.retries, 20);
    assert_eq!(read_serial.opts.retry_delay_ms, 1000);

    // bleWrite of pairing key → resolved to ABA356EB-... .
    let write_pk = steps
        .iter()
        .find_map(|s| match s {
            Step::BleWrite(w) if matches!(&w.value, StepValue::Captured { captured, .. } if captured == "pairingKeyBytes") => Some(w),
            _ => None,
        })
        .expect("bleWrite of pairingKeyBytes");
    assert_eq!(write_pk.gatt, "ABA356EB-9633-4E60-B73F-F52516DBD671");
}

#[test]
fn gatt_symbolic_names_inside_if_branches_also_resolve() {
    let idx = real_index();
    let gfx = idx.models.iter().find(|m| m.id == "gfx100ii").unwrap();
    let steps = &gfx.ble.as_ref().unwrap().establishment.steps;
    let ifs: Vec<_> = steps
        .iter()
        .filter_map(|s| match s {
            Step::If(i) => Some(i),
            _ => None,
        })
        .collect();
    assert_eq!(ifs.len(), 1, "exactly one if: block (the RED branch)");
    let red = ifs[0];
    assert_eq!(red.condition.field, "style");
    assert_eq!(red.condition.op, PredicateOp::Eq);
    assert_eq!(red.condition.value, "red");
    assert!(red.tolerant, "the if-block tolerates absent fields (§11.6)");

    // Both then-branch steps name deviceIdentificationNumber by symbol;
    // both should resolve to F557D96B-... .
    let then_uuids: Vec<&str> = red
        .then
        .iter()
        .filter_map(|s| match s {
            Step::BleRead(r) => Some(r.gatt.as_str()),
            Step::BleWrite(w) => Some(w.gatt.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(then_uuids.len(), 2);
    assert!(
        then_uuids
            .iter()
            .all(|u| *u == "F557D96B-8284-4667-8793-B971C1DECA2A"),
        "then-branch gatt references resolve through nested steps (§11.3 walks into if:.then)",
    );
}

#[test]
fn undefined_gatt_name_is_a_load_error() {
    let yaml = r#"
manufacturer: TESTCO
families:
  test:
    ble:
      gatt:
        knownChar: "00002A25-0000-1000-8000-00805F9B34FB"
      advert:
        fujiCompanyId: 0x1234
      establishment:
        mechanism: test
        steps:
          - bleRead: { gatt: notDeclared, encoding: bytes, captureAs: x }
models:
  - id: tm1
    displayName: "Test"
    inherits: [test]
    manifest: tm1.yaml
"#;
    let err = ResolvedManufacturerIndex::from_yaml(yaml).unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("undefined gatt symbolic name 'notDeclared'"),
        "got: {msg}"
    );
}

// ---------------------------------------------------------------------------
// §11.1 static template refs
// ---------------------------------------------------------------------------

#[test]
fn static_path_refs_substitute_in_signatures() {
    let idx = real_index();
    let gfx = idx.models.iter().find(|m| m.id == "gfx100ii").unwrap();
    let (name, sig) = &gfx.signatures[0];
    assert_eq!(name, "bleLegacyAdvert");
    let Signature::BleAdvert(sig) = sig;
    // "{ble.advert.fujiCompanyId}" resolved to 0x04D8 = 1240.
    assert_eq!(sig.require.manufacturer_company_id, 0x04D8);
    // "{ble.advert.legacyServiceUuid}" resolved to the real UUID.
    assert_eq!(
        sig.require.advert_contains_service.as_deref(),
        Some("AF854C2E-B214-458E-97E2-912C4ECF2CB8"),
    );
}

#[test]
fn unresolved_template_ref_is_a_load_error() {
    let yaml = r#"
manufacturer: TESTCO
families:
  test:
    ble:
      gatt: {}
      advert:
        fujiCompanyId: 0x1234
      establishment: { mechanism: test, steps: [] }
models:
  - id: tm1
    displayName: "Test"
    inherits: [test]
    manifest: tm1.yaml
    signatures:
      bad:
        kind: bleAdvert
        require:
          manufacturerCompanyId: "{ble.advert.doesNotExist}"
        manufacturerData: { minLength: 1 }
        suggests: { connection: ble, confidence: high }
"#;
    let err = ResolvedManufacturerIndex::from_yaml(yaml).unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("unresolved static ref"), "got: {msg}");
}

#[test]
fn embedded_template_ref_is_rejected() {
    // Only whole-string `"{path}"` substitutions are supported (§11.1).
    // `"prefix{path}suffix"` is a YAML-author error.
    let yaml = r#"
manufacturer: TESTCO
families:
  test:
    ble:
      gatt: {}
      advert:
        fujiCompanyId: 0x1234
      establishment: { mechanism: test, steps: [] }
models:
  - id: tm1
    displayName: "Test"
    inherits: [test]
    manifest: tm1.yaml
    signatures:
      bad:
        kind: bleAdvert
        require:
          manufacturerCompanyId: 0x1234
          advertContainsService: "prefix-{ble.advert.fujiCompanyId}-suffix"
        manufacturerData: { minLength: 1 }
        suggests: { connection: ble, confidence: high }
"#;
    let err = ResolvedManufacturerIndex::from_yaml(yaml).unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("embedded '{...}'"), "got: {msg}");
}

// ---------------------------------------------------------------------------
// §11.7 signature precedence (file-declaration order)
// ---------------------------------------------------------------------------

#[test]
fn signatures_preserve_file_declaration_order() {
    let idx = real_index();
    let gfx = idx.models.iter().find(|m| m.id == "gfx100ii").unwrap();
    let names: Vec<&str> = gfx.signatures.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(
        names,
        vec!["bleLegacyAdvert", "bleRedAdvert"],
        "legacy is declared first and must be tried first (§11.7)",
    );
}

// ---------------------------------------------------------------------------
// §11.10 fail-fast loader contract
// ---------------------------------------------------------------------------

#[test]
fn unknown_family_is_a_load_error() {
    let yaml = r#"
manufacturer: TESTCO
families: {}
models:
  - id: tm1
    displayName: "Test"
    inherits: [doesNotExist]
    manifest: tm1.yaml
"#;
    match ResolvedManufacturerIndex::from_yaml(yaml).unwrap_err() {
        ConfigError::UnknownFamily {
            model_id,
            family_id,
        } => {
            assert_eq!(model_id, "tm1");
            assert_eq!(family_id, "doesNotExist");
        }
        other => panic!("expected UnknownFamily, got {other:?}"),
    }
}

#[test]
fn missing_model_body_is_a_load_error() {
    // Real index references gfx100ii; supply an empty bodies map.
    let err = ConfigStore::from_manufacturer_index(&data("fuji/index.yaml"), BTreeMap::new())
        .unwrap_err();
    match err {
        ConfigError::MissingModelBody { id } => assert_eq!(id, "gfx100ii"),
        other => panic!("expected MissingModelBody, got {other:?}"),
    }
}

#[test]
fn malformed_model_body_is_a_load_error() {
    let mut bodies = BTreeMap::new();
    bodies.insert(
        "gfx100ii".to_string(),
        "schema: [not, a, mapping]".to_string(),
    );
    let err = ConfigStore::from_manufacturer_index(&data("fuji/index.yaml"), bodies).unwrap_err();
    match err {
        ConfigError::BodyParse { id, .. } => assert_eq!(id, "gfx100ii"),
        other => panic!("expected BodyParse, got {other:?}"),
    }
}

#[test]
fn unknown_step_verb_is_a_load_error() {
    let yaml = r#"
manufacturer: TESTCO
families:
  test:
    ble:
      gatt: {}
      advert: { fujiCompanyId: 1 }
      establishment:
        mechanism: test
        steps:
          - usbEnumerate: {}
models:
  - id: tm1
    displayName: "Test"
    inherits: [test]
    manifest: tm1.yaml
"#;
    // The MVP step-verb allowlist rejects `usbEnumerate`. The enum-tagged
    // Step deserializer fails at typed-decode time.
    let err = ResolvedManufacturerIndex::from_yaml(yaml).unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("usbEnumerate") || msg.contains("variant"),
        "got: {msg}"
    );
}

#[test]
fn unknown_encoding_is_a_load_error() {
    let yaml = r#"
manufacturer: TESTCO
families:
  test:
    ble:
      gatt:
        c: "00002A25-0000-1000-8000-00805F9B34FB"
      advert: { fujiCompanyId: 1 }
      establishment:
        mechanism: test
        steps:
          - bleRead: { gatt: c, encoding: noSuchEncoding, captureAs: x }
models:
  - id: tm1
    displayName: "Test"
    inherits: [test]
    manifest: tm1.yaml
"#;
    let err = ResolvedManufacturerIndex::from_yaml(yaml).unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("noSuchEncoding") || msg.contains("variant"),
        "got: {msg}"
    );
}

// ---------------------------------------------------------------------------
// End-to-end load via ConfigStore (P0.2 happy path)
// ---------------------------------------------------------------------------

#[test]
fn config_store_loads_real_fuji_index_with_real_body() {
    let mut bodies = BTreeMap::new();
    bodies.insert("gfx100ii".to_string(), data("fuji/gfx100ii/gfx100ii.yaml"));
    let store =
        ConfigStore::from_manufacturer_index(&data("fuji/index.yaml"), bodies).expect("loads");
    let index = store.index.as_ref().expect("index populated");
    assert_eq!(index.manufacturer, "FUJIFILM");
    assert_eq!(index.models.len(), 1);
    assert_eq!(index.models[0].id, "gfx100ii");
    // Body lookup works.
    let body = store.body("gfx100ii").expect("body present");
    assert_eq!(body.camera.model, "GFX100 II");
    // Primary manifest is the first model's body.
    assert_eq!(store.manifest.camera.model, "GFX100 II");
}

// ---------------------------------------------------------------------------
// §11.2 encoding allowlist (positive cases)
// ---------------------------------------------------------------------------

#[test]
fn every_authored_encoding_is_in_the_allowlist() {
    // Real index walk: every encoding-field-bearing thing should
    // parse — proves the YAML uses only authorized tokens.
    let idx = real_index();
    let gfx = idx.models.iter().find(|m| m.id == "gfx100ii").unwrap();

    let mut seen: Vec<Encoding> = Vec::new();
    fn collect(step: &Step, out: &mut Vec<Encoding>) {
        match step {
            Step::BleRead(r) => out.push(r.encoding),
            Step::Acquire(a) => collect(&a.from, out),
            Step::If(i) => {
                for s in &i.then {
                    collect(s, out);
                }
                for s in &i.else_branch {
                    collect(s, out);
                }
            }
            _ => {}
        }
    }
    for s in &gfx.ble.as_ref().unwrap().establishment.steps {
        collect(s, &mut seen);
    }
    // bytes (protectedSerialString) + u32 (deviceIdentificationNumber).
    assert!(seen.contains(&Encoding::Bytes));
    assert!(seen.contains(&Encoding::U32));
}

// ---------------------------------------------------------------------------
// §11.8 bleNotify until: shape (parse-only — no notify step in MVP YAML)
// ---------------------------------------------------------------------------

#[test]
fn ble_notify_until_variants_parse() {
    let yaml = r#"
manufacturer: TESTCO
families:
  test:
    ble:
      gatt:
        c: "00002A25-0000-1000-8000-00805F9B34FB"
      advert: { fujiCompanyId: 1 }
      establishment:
        mechanism: test
        steps:
          - bleNotify:
              gatt: c
              until: any
              timeoutMs: 5000
          - bleNotify:
              gatt: c
              until: { equals: "0x8001", encoding: bytes-raw }
              timeoutMs: 5000
          - bleNotify:
              gatt: c
              until: { matches: "^OK" }
              timeoutMs: 5000
models:
  - id: tm1
    displayName: "Test"
    inherits: [test]
    manifest: tm1.yaml
"#;
    let idx = ResolvedManufacturerIndex::from_yaml(yaml).expect("parses");
    let steps = &idx.models[0].ble.as_ref().unwrap().establishment.steps;
    let untils: Vec<&BleNotifyUntil> = steps
        .iter()
        .filter_map(|s| match s {
            Step::BleNotify(n) => Some(&n.until),
            _ => None,
        })
        .collect();
    assert!(matches!(untils[0], BleNotifyUntil::Any));
    assert!(matches!(untils[1], BleNotifyUntil::Equals { .. }));
    assert!(matches!(untils[2], BleNotifyUntil::Matches { .. }));
}

// ---------------------------------------------------------------------------
// §11.8 bleSubscribe — CCCD-enable verb (success on descriptor-write ack)
// ---------------------------------------------------------------------------

#[test]
fn ble_subscribe_step_parses_with_gatt_resolution_and_opts() {
    let yaml = r#"
manufacturer: TESTCO
families:
  test:
    ble:
      gatt:
        a: "00002A25-0000-1000-8000-00805F9B34FB"
        b: "00002A26-0000-1000-8000-00805F9B34FB"
      advert: { fujiCompanyId: 1 }
      establishment:
        mechanism: test
        steps:
          - bleSubscribe:
              gatt: a
              timeoutMs: 1500
          - bleSubscribe:
              gatt: b
              timeoutMs: 1500
              tolerant: true
models:
  - id: tm1
    displayName: "Test"
    inherits: [test]
    manifest: tm1.yaml
"#;
    let idx = ResolvedManufacturerIndex::from_yaml(yaml).expect("parses");
    let steps = &idx.models[0].ble.as_ref().unwrap().establishment.steps;
    let subs: Vec<_> = steps
        .iter()
        .filter_map(|s| match s {
            Step::BleSubscribe(s) => Some(s),
            _ => None,
        })
        .collect();
    assert_eq!(subs.len(), 2);
    // gatt: names resolve to UUIDs at load time per §11.3.
    assert_eq!(subs[0].gatt, "00002A25-0000-1000-8000-00805F9B34FB");
    assert_eq!(subs[1].gatt, "00002A26-0000-1000-8000-00805F9B34FB");
    assert_eq!(subs[0].timeout_ms, 1500);
    assert!(!subs[0].opts.tolerant);
    assert!(subs[1].opts.tolerant);
}

// ---------------------------------------------------------------------------
// §3.2 signature scope (recognize-seed facts)
// ---------------------------------------------------------------------------

#[test]
fn signature_scope_carries_literal_facts() {
    let idx = real_index();
    let gfx = idx.models.iter().find(|m| m.id == "gfx100ii").unwrap();
    let Signature::BleAdvert(legacy) = &gfx.signatures[0].1;
    assert_eq!(
        legacy.scope.get("style").map(String::as_str),
        Some("legacy")
    );
    assert!(matches!(legacy.suggests.confidence, Confidence::High));
    assert_eq!(legacy.suggests.connection, "ble");

    let Signature::BleAdvert(red) = &gfx.signatures[1].1;
    assert_eq!(red.scope.get("style").map(String::as_str), Some("red"));
    // RED captures 5 ASCII bytes into pairingKeyBytes AND shortSerial.
    assert_eq!(red.manufacturer_data.capture_bytes.len(), 2);
    let names: Vec<&str> = red
        .manufacturer_data
        .capture_bytes
        .iter()
        .map(|c| c.name.as_str())
        .collect();
    assert!(names.contains(&"pairingKeyBytes"));
    assert!(names.contains(&"shortSerial"));
}

// ---------------------------------------------------------------------------
// transform: schema addition (post-resolution byte transforms)
// ---------------------------------------------------------------------------

#[test]
fn red_echo_write_carries_app_identifier_bit_or_transform() {
    // F557D96B echo is `value | 0x20000000`. The schema models that as
    // `transform: { bitOr: 0x20000000 }` on the Captured StepValue.
    let idx = real_index();
    let gfx = idx.models.iter().find(|m| m.id == "gfx100ii").unwrap();
    let steps = &gfx.ble.as_ref().unwrap().establishment.steps;
    let red_if = steps
        .iter()
        .find_map(|s| match s {
            Step::If(i) if i.condition.value == "red" => Some(i),
            _ => None,
        })
        .expect("red if-block");
    let echo_write = red_if
        .then
        .iter()
        .find_map(|s| match s {
            Step::BleWrite(w) => Some(w),
            _ => None,
        })
        .expect("echo bleWrite");
    match &echo_write.value {
        StepValue::Captured {
            captured,
            transform,
        } => {
            assert_eq!(captured, "idNumber");
            assert_eq!(*transform, vec![Transform::BitOr(0x20000000)]);
        }
        other => panic!("expected Captured with transform, got {other:?}"),
    }
}

#[test]
fn unknown_transform_primitive_is_a_load_error() {
    let yaml = r#"
manufacturer: TESTCO
families:
  test:
    ble:
      gatt: { c: "00002A25-0000-1000-8000-00805F9B34FB" }
      advert: { fujiCompanyId: 1 }
      establishment:
        mechanism: test
        steps:
          - bleWrite:
              gatt: c
              value: { captured: x, transform: { rotateLeft: 4 } }
models:
  - id: tm1
    displayName: "Test"
    inherits: [test]
    manifest: tm1.yaml
"#;
    let err = ResolvedManufacturerIndex::from_yaml(yaml).unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("unknown transform 'rotateLeft'"), "got: {msg}");
}

#[test]
fn captured_without_transform_still_parses() {
    let idx = real_index();
    let gfx = idx.models.iter().find(|m| m.id == "gfx100ii").unwrap();
    let steps = &gfx.ble.as_ref().unwrap().establishment.steps;
    // The pre-RED-branch bleWrite of pairingKey has no transform.
    let write_pk = steps
        .iter()
        .find_map(|s| match s {
            Step::BleWrite(w) => Some(w),
            _ => None,
        })
        .expect("pairingKey write");
    match &write_pk.value {
        StepValue::Captured { transform, .. } => {
            assert!(
                transform.is_empty(),
                "no transform on legacy pairing-key write"
            );
        }
        other => panic!("expected Captured, got {other:?}"),
    }
}

#[test]
fn transform_chain_list_form_parses_in_order() {
    let yaml = r#"
manufacturer: TESTCO
families:
  test:
    ble:
      gatt: { c: "00002A25-0000-1000-8000-00805F9B34FB" }
      advert: { fujiCompanyId: 1 }
      establishment:
        mechanism: test
        steps:
          - bleRead:
              gatt: c
              encoding: u8
              captureAs: flags
              transform:
                - slice: { at: 3, length: 1 }
                - bits: { mask: 0x0C, shift: 2 }
          - bleWrite:
              gatt: c
              value:
                captured: flags
                transform:
                  - reverseBytes: {}
                  - dropPrefix: 2
models:
  - id: tm1
    displayName: "Test"
    inherits: [test]
    manifest: tm1.yaml
"#;
    let idx = ResolvedManufacturerIndex::from_yaml(yaml).expect("chain forms load");
    let steps = &idx.models[0].ble.as_ref().unwrap().establishment.steps;
    match &steps[0] {
        Step::BleRead(r) => assert_eq!(
            r.transform,
            vec![
                Transform::Slice {
                    at: 3,
                    length: Some(1)
                },
                Transform::Bits {
                    mask: 0x0C,
                    shift: 2
                },
            ]
        ),
        other => panic!("expected bleRead, got {other:?}"),
    }
    match &steps[1] {
        Step::BleWrite(w) => match &w.value {
            StepValue::Captured { transform, .. } => assert_eq!(
                *transform,
                vec![Transform::ReverseBytes, Transform::DropPrefix(2)]
            ),
            other => panic!("expected Captured, got {other:?}"),
        },
        other => panic!("expected bleWrite, got {other:?}"),
    }
}

#[test]
fn statically_invalid_transform_operands_are_load_errors() {
    let template = |transform: &str| {
        format!(
            r#"
manufacturer: TESTCO
families:
  test:
    ble:
      gatt: {{ c: "00002A25-0000-1000-8000-00805F9B34FB" }}
      advert: {{ fujiCompanyId: 1 }}
      establishment:
        mechanism: test
        steps:
          - bleWrite:
              gatt: c
              value: {{ captured: x, transform: {transform} }}
models:
  - id: tm1
    displayName: "Test"
    inherits: [test]
    manifest: tm1.yaml
"#
        )
    };
    for (transform, needle) in [
        ("{ slice: { at: 0, length: 0 } }", "length 0"),
        ("{ bits: { mask: 0, shift: 1 } }", "mask 0"),
        ("{ reverseBytes: 7 }", "takes no operand"),
    ] {
        let err = ResolvedManufacturerIndex::from_yaml(&template(transform)).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains(needle), "for {transform}: got {msg}");
    }
}

// ---------------------------------------------------------------------------
// §11.8 CCCD mode + bleNotify field captures (multivendor pass)
// ---------------------------------------------------------------------------

#[test]
fn cccd_mode_and_notify_captures_parse_with_defaults() {
    let yaml = r#"
manufacturer: TESTCO
families:
  test:
    ble:
      gatt: { c: "00002A25-0000-1000-8000-00805F9B34FB" }
      advert: { fujiCompanyId: 1 }
      establishment:
        mechanism: test
        steps:
          - bleSubscribe: { gatt: c, timeoutMs: 3000 }
          - bleSubscribe: { gatt: c, timeoutMs: 3000, mode: indicate }
          - bleNotify:
              gatt: c
              until: any
              mode: indicate
              captureAs: wholePayload
              capture:
                - { at: 2, length: 1, encoding: u8, name: wifiStatus }
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
    let idx = ResolvedManufacturerIndex::from_yaml(yaml).expect("mode + captures load");
    let steps = &idx.models[0].ble.as_ref().unwrap().establishment.steps;
    match (&steps[0], &steps[1]) {
        (Step::BleSubscribe(default_sub), Step::BleSubscribe(indicate_sub)) => {
            assert_eq!(
                default_sub.mode,
                CccdMode::Notify,
                "mode defaults to notify"
            );
            assert_eq!(indicate_sub.mode, CccdMode::Indicate);
        }
        other => panic!("expected two bleSubscribe steps, got {other:?}"),
    }
    match &steps[2] {
        Step::BleNotify(n) => {
            assert_eq!(n.mode, CccdMode::Indicate);
            assert_eq!(n.capture_as.as_deref(), Some("wholePayload"));
            assert_eq!(n.capture.len(), 2);
            assert_eq!(n.capture[0].at, 2);
            assert_eq!(n.capture[0].length, Some(1));
            assert_eq!(n.capture[0].encoding, Encoding::U8);
            assert_eq!(n.capture[0].name, "wifiStatus");
            assert_eq!(n.capture[1].at, 3);
            assert_eq!(n.capture[1].length, None, "omitted length = to end");
            assert_eq!(n.capture[1].transform, vec![Transform::DropPrefix(1)]);
        }
        other => panic!("expected bleNotify, got {other:?}"),
    }
}
