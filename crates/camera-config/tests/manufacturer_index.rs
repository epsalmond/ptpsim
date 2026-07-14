//! Manufacturer-index loader contract tests (plan §2.3 + §11).
//!
//! Exercises the schema additions in `crates/camera-config` against the
//! real `packages/camera-config-data/fuji/index.yaml` plus synthetic fixtures
//! for each validation rule. Order of tests follows the contract list in
//! §11 so a reviewer can pair-read.

use std::collections::BTreeMap;
use std::path::PathBuf;

use camera_config::error::ConfigError;
use camera_config::index::eval::{advert_matches, BleAdvertFacts};
use camera_config::index::{
    AdvertByteSource, AdvertPredicate, AwaitSource, BleNotifyUntil, CccdMode, Confidence, Encoding,
    PredicateOp, ReconnectDisposition, ResolvedManufacturerIndex, Signature, Step, StepValue,
    Transform,
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

fn store_for(vendor: &str, model_id: &str) -> std::sync::Arc<ConfigStore> {
    let mut bodies = BTreeMap::new();
    bodies.insert(
        model_id.to_string(),
        data(&format!("{vendor}/{model_id}/{model_id}.yaml")),
    );
    ConfigStore::from_manufacturer_index(&data(&format!("{vendor}/index.yaml")), bodies)
        .unwrap_or_else(|e| panic!("{vendor}/{model_id} loads: {e:?}"))
}

#[test]
fn reestablishment_bindings_must_match_the_resolved_plan() {
    let original = data("fuji/gfx100ii/gfx100ii.yaml");
    let body = original.replace(
        "params: { launchMode: \"3\" }",
        "params: { wrongLaunchMode: \"3\" }",
    );
    assert_ne!(body, original, "fixture replacement must find the binding");
    let mut bodies = BTreeMap::new();
    bodies.insert("gfx100ii".to_string(), body);
    let error = ConfigStore::from_manufacturer_index(&data("fuji/index.yaml"), bodies)
        .expect_err("mismatched establishment parameters must fail store loading");
    assert!(
        matches!(error, ConfigError::Validation { ref message, .. } if message.contains("do not exactly match")),
        "got: {error}"
    );
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
    assert_eq!(ble.advert.manufacturer_company_id, Some(0x04D8));
    assert_eq!(
        ble.advert
            .service_uuids
            .get("fileTransfer")
            .map(String::as_str),
        Some("AF854C2E-B214-458E-97E2-912C4ECF2CB8"),
    );
    // Establishment plan inherited.
    assert_eq!(ble.establishment("ble-pair").unwrap().mechanism, "ble-pair");
    assert!(
        ble.establishment("ble-pair").unwrap().steps.len() >= 4,
        "establishment carries the multi-step pair flow"
    );
}

#[test]
fn reconnect_routes_fail_closed_at_index_load() {
    let original = data("fuji/index.yaml");

    let zero_timeout = original.replacen("scanTimeoutMs: 60000", "scanTimeoutMs: 0", 1);
    let error = ResolvedManufacturerIndex::from_yaml(&zero_timeout)
        .expect_err("a zero reconnect scan window must fail");
    assert!(error.to_string().contains("must be greater than zero"));

    let unknown_plan = original.replacen(
        "mechanism: ble-wake\n          identity:",
        "mechanism: missing-wake\n          identity:",
        1,
    );
    let error = ResolvedManufacturerIndex::from_yaml(&unknown_plan)
        .expect_err("an unknown reconnect plan must fail");
    assert!(error
        .to_string()
        .contains("unknown establishment 'missing-wake'"));

    let unknown_identity =
        original.replacen("identity: [pairingKeyBytes]", "identity: [notCaptured]", 1);
    let error = ResolvedManufacturerIndex::from_yaml(&unknown_identity)
        .expect_err("an identity absent from signature scope must fail");
    assert!(error
        .to_string()
        .contains("identity key 'notCaptured' is not captured or scoped"));
}

// ---------------------------------------------------------------------------
// §11.3 GATT-name → UUID at index-build
// ---------------------------------------------------------------------------

#[test]
fn gatt_symbolic_names_in_steps_become_uuids() {
    let idx = real_index();
    let gfx = idx.models.iter().find(|m| m.id == "gfx100ii").unwrap();
    let steps = &gfx
        .ble
        .as_ref()
        .unwrap()
        .establishment("ble-pair")
        .unwrap()
        .steps;

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
    let steps = &gfx
        .ble
        .as_ref()
        .unwrap()
        .establishment("ble-pair")
        .unwrap()
        .steps;
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
        manufacturerCompanyId: 0x1234
      establishments:
        test:
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
    let (name, sig) = gfx
        .signatures
        .iter()
        .find(|(name, _)| name == "bleLegacyAdvert")
        .unwrap();
    assert_eq!(name, "bleLegacyAdvert");
    let Signature::BleAdvert(sig) = sig;
    // The legacy require is all-of [manufacturerData, serviceUuids] with
    // both template refs resolved to literals.
    let AdvertPredicate::All(children) = &sig.require else {
        panic!("expected all-of predicate, got {:?}", sig.require);
    };
    assert_eq!(children.len(), 2);
    // "{ble.advert.manufacturerCompanyId}" resolved to 0x04D8 = 1240.
    let AdvertPredicate::ManufacturerData(m) = &children[0] else {
        panic!("expected manufacturerData first, got {:?}", children[0]);
    };
    assert_eq!(m.company_id, Some(0x04D8));
    assert_eq!(m.payload.min_length, Some(5));
    // "{ble.advert.serviceUuids.fileTransfer}" resolved to the real UUID.
    let AdvertPredicate::ServiceUuids { contains } = &children[1] else {
        panic!("expected serviceUuids second, got {:?}", children[1]);
    };
    assert_eq!(contains, "AF854C2E-B214-458E-97E2-912C4ECF2CB8");
}

#[test]
fn awake_legacy_signature_uses_local_name_identity_without_fuji_mfg_data() {
    let idx = real_index();
    let gfx = idx.models.iter().find(|m| m.id == "gfx100ii").unwrap();
    let Signature::BleAdvert(sig) = &gfx
        .signatures
        .iter()
        .find(|(name, _)| name == "bleAwakeLegacyAdvert")
        .unwrap()
        .1;
    let AdvertPredicate::All(children) = &sig.require else {
        panic!("expected all-of predicate, got {:?}", sig.require);
    };
    assert_eq!(children.len(), 3);
    assert!(matches!(
        &children[0],
        AdvertPredicate::ServiceUuids { contains }
            if contains == "AF854C2E-B214-458E-97E2-912C4ECF2CB8"
    ));
    assert!(matches!(
        &children[1],
        AdvertPredicate::LocalName(name)
            if name.contains.as_deref() == Some("GFX100II-")
    ));
    let AdvertPredicate::Not(no_fuji_mfg) = &children[2] else {
        panic!("expected negated manufacturer-data predicate");
    };
    assert!(matches!(
        &**no_fuji_mfg,
        AdvertPredicate::ManufacturerData(mfg) if mfg.company_id == Some(0x04D8)
    ));

    assert!(!sig.discoverable);
    assert_eq!(sig.capture.len(), 1);
    let capture = &sig.capture[0];
    assert_eq!(capture.source, AdvertByteSource::LocalName);
    assert_eq!(capture.at, 0);
    assert_eq!(capture.length, Some(4));
    assert_eq!(capture.encoding, Encoding::Ascii);
    assert_eq!(capture.name, "shortSerial");
    assert_eq!(sig.scope.get("style").map(String::as_str), Some("legacy"));

    let reconnect = sig.reconnect.as_ref().expect("awake route reconnects");
    assert!(matches!(reconnect.disposition, ReconnectDisposition::Ready));
    assert_eq!(reconnect.mechanism, "ble-reconnect");
    assert_eq!(reconnect.identity, ["shortSerial"]);

    let Signature::BleAdvert(pairing) = &gfx
        .signatures
        .iter()
        .find(|(name, _)| name == "bleLegacyAdvert")
        .unwrap()
        .1;
    assert!(pairing.discoverable);
    assert!(pairing.reconnect.is_none());
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
        manufacturerCompanyId: 0x1234
      establishments: { test: { mechanism: test, steps: [] } }
models:
  - id: tm1
    displayName: "Test"
    inherits: [test]
    manifest: tm1.yaml
    signatures:
      bad:
        kind: bleAdvert
        require:
          manufacturerData:
            companyId: "{ble.advert.doesNotExist}"
            minLength: 1
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
        manufacturerCompanyId: 0x1234
      establishments: { test: { mechanism: test, steps: [] } }
models:
  - id: tm1
    displayName: "Test"
    inherits: [test]
    manifest: tm1.yaml
    signatures:
      bad:
        kind: bleAdvert
        require:
          serviceUuids:
            contains: "prefix-{ble.advert.manufacturerCompanyId}-suffix"
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
        vec![
            "bleStartupLegacyAdvert",
            "bleStartupRedAdvert",
            "bleAwakeRedAdvert",
            "bleAwakeLegacyAdvert",
            "bleLegacyAdvert",
            "bleRedAdvert",
        ],
        "startup/awake reconnect routes must precede broad pairing signatures (§11.7)",
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
      advert: { manufacturerCompanyId: 1 }
      establishments:
        test:
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
      advert: { manufacturerCompanyId: 1 }
      establishments:
        test:
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
    for s in &gfx
        .ble
        .as_ref()
        .unwrap()
        .establishment("ble-pair")
        .unwrap()
        .steps
    {
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
      advert: { manufacturerCompanyId: 1 }
      establishments:
        test:
          mechanism: test
          activities:
            - { id: camera.test.captures, version: 1, displayRole: preparingConnection, defaultExpectedDurationMs: 1, interactionRequired: false, executorSpan: { sequence: steps, startStep: 0, endStepExclusive: 3 } }
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
    let steps = &idx.models[0]
        .ble
        .as_ref()
        .unwrap()
        .establishment("test")
        .unwrap()
        .steps;
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
      advert: { manufacturerCompanyId: 1 }
      establishments:
        test:
          mechanism: test
          activities:
            - { id: camera.test.subscribe, version: 1, displayRole: preparingConnection, defaultExpectedDurationMs: 1, interactionRequired: false, executorSpan: { sequence: steps, startStep: 0, endStepExclusive: 2 } }
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
    let steps = &idx.models[0]
        .ble
        .as_ref()
        .unwrap()
        .establishment("test")
        .unwrap()
        .steps;
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
    let Signature::BleAdvert(legacy) = &gfx
        .signatures
        .iter()
        .find(|(name, _)| name == "bleLegacyAdvert")
        .unwrap()
        .1;
    assert_eq!(
        legacy.scope.get("style").map(String::as_str),
        Some("legacy")
    );
    assert!(matches!(legacy.suggests.confidence, Confidence::High));
    assert_eq!(legacy.suggests.connection, "ble");

    let Signature::BleAdvert(red) = &gfx
        .signatures
        .iter()
        .find(|(name, _)| name == "bleRedAdvert")
        .unwrap()
        .1;
    assert_eq!(red.scope.get("style").map(String::as_str), Some("red"));
    // RED captures 5 ASCII bytes into pairingKeyBytes AND shortSerial,
    // both from the manufacturer-data payload.
    assert_eq!(red.capture.len(), 2);
    let names: Vec<&str> = red.capture.iter().map(|c| c.name.as_str()).collect();
    assert!(names.contains(&"pairingKeyBytes"));
    assert!(names.contains(&"shortSerial"));
    assert!(red
        .capture
        .iter()
        .all(|c| c.source == AdvertByteSource::ManufacturerData));
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
    let steps = &gfx
        .ble
        .as_ref()
        .unwrap()
        .establishment("ble-pair")
        .unwrap()
        .steps;
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
      advert: { manufacturerCompanyId: 1 }
      establishments:
        test:
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
    let steps = &gfx
        .ble
        .as_ref()
        .unwrap()
        .establishment("ble-pair")
        .unwrap()
        .steps;
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
      advert: { manufacturerCompanyId: 1 }
      establishments:
        test:
          mechanism: test
          activities:
            - { id: camera.test.transform, version: 1, displayRole: preparingConnection, defaultExpectedDurationMs: 1, interactionRequired: false, executorSpan: { sequence: steps, startStep: 0, endStepExclusive: 2 } }
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
    let steps = &idx.models[0]
        .ble
        .as_ref()
        .unwrap()
        .establishment("test")
        .unwrap()
        .steps;
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
      advert: {{ manufacturerCompanyId: 1 }}
      establishments:
        test:
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
      advert: { manufacturerCompanyId: 1 }
      establishments:
        test:
          mechanism: test
          activities:
            - { id: camera.test.captures, version: 1, displayRole: preparingConnection, defaultExpectedDurationMs: 1, interactionRequired: false, executorSpan: { sequence: steps, startStep: 0, endStepExclusive: 3 } }
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
    let steps = &idx.models[0]
        .ble
        .as_ref()
        .unwrap()
        .establishment("test")
        .unwrap()
        .steps;
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

// ---------------------------------------------------------------------------
// bleRequestMtu + bleDiscoverServices setup verbs (multivendor pass)
// ---------------------------------------------------------------------------

#[test]
fn mtu_and_discover_services_verbs_parse() {
    let yaml = r#"
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
            - { id: camera.test.setup, version: 1, displayRole: preparingConnection, defaultExpectedDurationMs: 1, interactionRequired: false, executorSpan: { sequence: steps, startStep: 0, endStepExclusive: 4 } }
          steps:
            - bleConnect: {}
            - bleRequestMtu: { mtu: 158 }
            - bleDiscoverServices: { tolerant: true, retries: 3, retryDelayMs: 250 }
            - bleRead: { gatt: c, encoding: bytes, captureAs: x }
models:
  - id: tm1
    displayName: "Test"
    inherits: [test]
    manifest: tm1.yaml
"#;
    let idx = ResolvedManufacturerIndex::from_yaml(yaml).expect("setup verbs load");
    let steps = &idx.models[0]
        .ble
        .as_ref()
        .unwrap()
        .establishment("test")
        .unwrap()
        .steps;
    match &steps[1] {
        Step::BleRequestMtu(s) => {
            assert_eq!(s.mtu, 158);
            assert!(!s.opts.tolerant);
        }
        other => panic!("expected bleRequestMtu, got {other:?}"),
    }
    assert!(matches!(&steps[2], Step::BleDiscoverServices(_)));
    match &steps[2] {
        Step::BleDiscoverServices(s) => {
            assert!(s.opts.tolerant);
            assert_eq!(s.opts.retries, 3);
            assert_eq!(s.opts.retry_delay_ms, 250);
        }
        other => panic!("expected bleDiscoverServices, got {other:?}"),
    }
    assert_eq!(steps[1].verb_name(), "bleRequestMtu");
    assert_eq!(steps[2].verb_name(), "bleDiscoverServices");
}

// ---------------------------------------------------------------------------
// §11.14 advert predicate model (multivendor pass)
// ---------------------------------------------------------------------------

fn predicate_fixture(require_block: &str) -> String {
    format!(
        r#"
manufacturer: TESTCO
families:
  test:
    ble:
      gatt: {{}}
      advert: {{ manufacturerCompanyId: 0x012D }}
      establishments: {{ test: {{ mechanism: test, steps: [] }} }}
models:
  - id: tm1
    displayName: "Test"
    inherits: [test]
    manifest: tm1.yaml
    signatures:
      s1:
        kind: bleAdvert
        require:
{require_block}
        suggests: {{ connection: ble, confidence: low }}
"#
    )
}

#[test]
fn nikon_style_signature_without_company_id_parses() {
    // The Nikon shape from the risk pass: recognition by LSS service UUID +
    // local-name prefix, no manufacturer data at all.
    let yaml = predicate_fixture(
        r#"          all:
            - serviceUuids: { contains: "0000DE00-3DD4-4255-8D62-6DC7B9BD5561" }
            - localName: { prefix: "Z " }
            - not: { txPower: { min: 0 } }"#,
    );
    let idx = ResolvedManufacturerIndex::from_yaml(&yaml).expect("nikon-style shape loads");
    let Signature::BleAdvert(sig) = &idx.models[0].signatures[0].1;
    let AdvertPredicate::All(children) = &sig.require else {
        panic!("expected all-of");
    };
    assert_eq!(children.len(), 3);
    assert!(matches!(&children[2], AdvertPredicate::Not(_)));
}

#[test]
fn manufacturer_payload_constraint_without_company_id_matches() {
    let yaml = predicate_fixture(
        r#"          manufacturerData:
            minLength: 2
            assertByte: { index: 0, equals: 0x42 }"#,
    );
    let idx = ResolvedManufacturerIndex::from_yaml(&yaml)
        .expect("manufacturerData without companyId but with payload constraint loads");
    let Signature::BleAdvert(sig) = &idx.models[0].signatures[0].1;
    let facts = BleAdvertFacts {
        manufacturer_data: Some((0x9999, vec![0x42, 0x10])),
        ..Default::default()
    };
    assert!(
        advert_matches(sig, &facts),
        "payload constraint should match regardless of company id when omitted"
    );
    let wrong = BleAdvertFacts {
        manufacturer_data: Some((0x9999, vec![0x41, 0x10])),
        ..Default::default()
    };
    assert!(!advert_matches(sig, &wrong));
}

#[test]
fn service_data_and_raw_ad_record_predicates_evaluate() {
    let yaml = predicate_fixture(
        r#"          all:
            - serviceData:
                uuid: "FE2C"
                minLength: 4
                assertByte: { index: 0, equals: 0xaa }
            - rawAdRecord:
                adType: 0xff
                minLength: 4
                assertByte:
                  - { index: 0, equals: 0x2d }
                  - { index: 1, equals: 0x01 }"#,
    );
    let idx = ResolvedManufacturerIndex::from_yaml(&yaml)
        .expect("serviceData/rawAdRecord predicate loads");
    let Signature::BleAdvert(sig) = &idx.models[0].signatures[0].1;
    let facts = BleAdvertFacts {
        service_data: vec![("fe2c".into(), vec![0xaa, 0xbb, 0xcc, 0xdd])],
        // Raw AD manufacturer payload as seen on air: company id included.
        ad_records: vec![(0xff, vec![0x2d, 0x01, 0x03, 0x00])],
        ..Default::default()
    };
    assert!(advert_matches(sig, &facts));
    let missing_raw = BleAdvertFacts {
        service_data: vec![("fe2c".into(), vec![0xaa, 0xbb, 0xcc, 0xdd])],
        ..Default::default()
    };
    assert!(!advert_matches(sig, &missing_raw));
}

#[test]
fn statically_invalid_predicates_are_load_errors() {
    for (require, needle) in [
        ("          all: []", "empty predicate list"),
        (
            "          localName: { prefix: \"A\", equals: \"B\" }",
            "exactly one of",
        ),
        ("          txPower: {}", "at least one of min/max"),
        ("          manufacturerData: {}", "vacuous"),
        (
            "          manufacturerData: { length: 6, minLength: 5 }",
            "mutually exclusive",
        ),
        (
            "          manufacturerData: { assertBits: { offset: 0, mask: 0, equals: 0 } }",
            "mask 0",
        ),
        (
            "          windSpeed: { knots: 12 }",
            "unknown advert predicate",
        ),
    ] {
        let err = ResolvedManufacturerIndex::from_yaml(&predicate_fixture(require)).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains(needle), "for `{require}`: got {msg}");
    }
}

#[test]
fn invalid_cccd_modes_are_load_errors() {
    let template = |verb: &str| {
        format!(
            r#"
manufacturer: TESTCO
families:
  test:
    ble:
      gatt: {{ c: "00002A25-0000-1000-8000-00805F9B34FB" }}
      advert: {{ manufacturerCompanyId: 1 }}
      establishments:
        test:
          mechanism: test
          steps:
            - {verb}
models:
  - id: tm1
    displayName: "Test"
    inherits: [test]
    manifest: tm1.yaml
"#
        )
    };
    for step in [
        "bleSubscribe: { gatt: c, timeoutMs: 3000, mode: confirm }",
        "bleNotify: { gatt: c, until: any, timeoutMs: 3000, mode: confirm }",
    ] {
        let err = ResolvedManufacturerIndex::from_yaml(&template(step)).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("confirm") || msg.contains("unknown variant"),
            "for `{step}`: got {msg}"
        );
    }
}

#[test]
fn capture_sources_parse_in_both_authoring_forms() {
    let yaml = r#"
manufacturer: TESTCO
families:
  test:
    ble:
      gatt: {}
      advert: { manufacturerCompanyId: 0x012D }
      establishments: { test: { mechanism: test, steps: [] } }
models:
  - id: tm1
    displayName: "Test"
    inherits: [test]
    manifest: tm1.yaml
    signatures:
      s1:
        kind: bleAdvert
        require:
          manufacturerData: { companyId: "{ble.advert.manufacturerCompanyId}" }
        capture:
          - { source: manufacturerData, at: 1, length: 2, encoding: u16-le, name: model }
          - { source: localName, encoding: utf8, name: name }
          - { source: { rawAdRecord: 0x21 }, at: 1, encoding: bytes, name: raw21 }
          - source: { serviceData: "FE2C" }
            transform: { dropPrefix: 3 }
            encoding: ascii
            name: ssid
        suggests: { connection: ble, confidence: low }
"#;
    let idx = ResolvedManufacturerIndex::from_yaml(yaml).expect("capture sources load");
    let Signature::BleAdvert(sig) = &idx.models[0].signatures[0].1;
    assert_eq!(sig.capture.len(), 4);
    assert_eq!(sig.capture[0].source, AdvertByteSource::ManufacturerData);
    assert_eq!(sig.capture[1].source, AdvertByteSource::LocalName);
    assert_eq!(
        sig.capture[2].source,
        AdvertByteSource::RawAdRecord { ad_type: 0x21 }
    );
    assert_eq!(
        sig.capture[3].source,
        AdvertByteSource::ServiceData {
            uuid: "FE2C".into()
        }
    );
    assert_eq!(sig.capture[3].transform, vec![Transform::DropPrefix(3)]);
}

#[test]
fn preliminary_vendor_indexes_load_in_camera_config() {
    for (vendor, model_id) in [
        ("sony", "sony-camera"),
        ("canon", "canon-camera"),
        ("nikon", "nikon-camera"),
    ] {
        let store = store_for(vendor, model_id);
        assert!(
            store.index.is_some(),
            "{vendor}/{model_id} exposes a resolved manufacturer index"
        );
        let expected_mfr = vendor.to_ascii_uppercase();
        assert_eq!(
            store.body(model_id).map(|b| b.camera.manufacturer.as_str()),
            Some(expected_mfr.as_str()),
            "{vendor}/{model_id} body manifest is available"
        );
    }
}

// ---------------------------------------------------------------------------
// §11.15 bleAwaitUntil — deserialize, validate, gatt-resolution
// ---------------------------------------------------------------------------

#[test]
fn fuji_condition_retry_parses_with_resolved_diagnostic_gatt() {
    let index = real_index();
    let plan = index.models[0]
        .ble
        .as_ref()
        .unwrap()
        .establishment("ble-establish-wifi-ap")
        .unwrap();
    let retry = plan
        .steps
        .iter()
        .find_map(|step| match step {
            Step::Retry(retry) => Some(retry),
            _ => None,
        })
        .expect("launch retry parses");
    assert_eq!(retry.max_attempts, 2);
    assert_eq!(retry.retry_delay_ms, 200);
    assert!(matches!(
        &retry.on_failure[..],
        [Step::BleRead(read)]
            if read.gatt == "1587B102-0B6D-4B63-9226-66FCC6D17387"
    ));
    assert!(matches!(
        &plan.steps[2],
        Step::BleAwaitUntil(await_step)
            if matches!(await_step.source, camera_config::index::AwaitSource::Read { .. })
                && await_step.fail_when.is_none()
                && await_step.until.field == "apStateBaseline"
                && await_step.capture.iter().any(|capture| capture.name == "apStateBaseline")
    ));
    assert!(matches!(&plan.steps[3], Step::BleSubscribe(_)));
    assert!(matches!(
        &retry.steps[1],
        Step::BleAwaitUntil(await_step)
            if matches!(
                await_step.source,
                camera_config::index::AwaitSource::Notify { seed_read: false, .. }
            ) && await_step
                .fail_when
                .as_ref()
                .is_some_and(|p| p.field == "apStateRaw" && p.value == "0080")
                && await_step.until.field == "apStateRaw"
                && await_step.until.value == "0180"
    ));
    assert!(matches!(
        &retry.steps[0],
        Step::BleWrite(write)
            if write.notification_fence.as_deref()
                == Some("A68E3F66-0FCC-4395-8D4C-AA980B5877FA")
    ));

    // Step's serializer is intentionally not the authored one-entry mapping,
    // so isolate the retry record fields from its nested step payloads here.
    let mut retry_shape = retry.clone();
    retry_shape.steps.clear();
    retry_shape.on_failure.clear();
    let yaml = serde_yaml::to_string(&retry_shape).expect("retry serializes");
    assert_eq!(
        yaml.lines()
            .filter(|line| line.starts_with("retryDelayMs:"))
            .count(),
        1,
        "retry policy has one unambiguous delay field: {yaml}",
    );
}

#[test]
fn notification_fence_requires_a_declared_gatt_characteristic() {
    let yaml = data("fuji/index.yaml").replacen(
        "notificationFence: apState",
        "notificationFence: missingFenceCharacteristic",
        1,
    );
    let error = ResolvedManufacturerIndex::from_yaml(&yaml).unwrap_err();
    assert!(
        error.to_string().contains("notificationFence")
            && error.to_string().contains("missingFenceCharacteristic"),
        "got: {error}"
    );
}

#[test]
fn retry_validation_rejects_zero_max_attempts() {
    let yaml = data("fuji/index.yaml").replacen("maxAttempts: 2", "maxAttempts: 0", 1);
    let error = ResolvedManufacturerIndex::from_yaml(&yaml).unwrap_err();
    assert!(
        error.to_string().contains("maxAttempts must be > 0"),
        "got: {error}",
    );
}

#[test]
fn seeded_notify_rejects_fail_when_at_validation() {
    let yaml = await_fixture(
        r#"                source: { notify: { gatt: statusChar, seedRead: true } }
                capture: { at: 0, length: 1, encoding: u8, name: status }
                until: { status: { eq: 1 } }
                failWhen: { status: { eq: 0 } }
                timeoutMs: 5000"#,
    );
    let error = ResolvedManufacturerIndex::from_yaml(&yaml).unwrap_err();
    assert!(
        error
            .to_string()
            .contains("notify seedRead cannot be combined with failWhen"),
        "got: {error}",
    );
}

#[test]
fn seeded_notify_rejection_is_validated_inside_ble_actions() {
    let yaml = r#"
manufacturer: TESTCO
families:
  test:
    ble:
      gatt:
        statusChar: "0000CC09-0000-1000-8000-00805F9B34FB"
      advert: { manufacturerCompanyId: 1 }
      actions:
        test-action:
          steps:
            - bleAwaitUntil:
                source: { notify: { gatt: statusChar, seedRead: true } }
                capture: { at: 0, length: 1, encoding: u8, name: status }
                until: { status: { eq: 1 } }
                failWhen: { status: { eq: 0 } }
                timeoutMs: 5000
models:
  - id: tm1
    displayName: "Test"
    inherits: [test]
    manifest: tm1.yaml
"#;
    let error = ResolvedManufacturerIndex::from_yaml(yaml).unwrap_err();
    assert!(
        error.to_string().contains("actions.test-action.steps[0]")
            && error
                .to_string()
                .contains("notify seedRead cannot be combined with failWhen"),
        "got: {error}",
    );
}

fn await_fixture(step_body: &str) -> String {
    format!(
        r#"
manufacturer: TESTCO
families:
  test:
    ble:
      gatt:
        statusChar: "0000CC09-0000-1000-8000-00805F9B34FB"
        requestChar: "0000CC08-0000-1000-8000-00805F9B34FB"
      advert: {{ manufacturerCompanyId: 1 }}
      establishments:
        test:
          mechanism: test
          activities:
            - {{ id: camera.test.await, version: 1, displayRole: waitingForCamera, defaultExpectedDurationMs: 1, interactionRequired: false, executorSpan: {{ sequence: steps, startStep: 0, endStepExclusive: 2 }} }}
          steps:
            - bleConnect: {{}}
            - bleAwaitUntil:
{step_body}
models:
  - id: tm1
    displayName: "Test"
    inherits: [test]
    manifest: tm1.yaml
"#
    )
}

#[test]
fn ble_await_until_both_source_forms_parse_and_resolve_gatt() {
    // notify-source with onEach; gatt names in source + onEach resolve to UUIDs.
    let yaml = await_fixture(
        r#"                source: { notify: { gatt: statusChar, mode: indicate, seedRead: true } }
                capture: { at: 0, length: 1, encoding: u8, name: status }
                until: { status: { eq: 1 } }
                onEach:
                  - bleWrite: { gatt: requestChar, value: { literal: "01" } }
                timeoutMs: 5000
                intervalMs: 250"#,
    );
    let idx = ResolvedManufacturerIndex::from_yaml(&yaml).expect("notify form loads");
    let steps = &idx.models[0]
        .ble
        .as_ref()
        .unwrap()
        .establishment("test")
        .unwrap()
        .steps;
    match &steps[1] {
        Step::BleAwaitUntil(s) => {
            match &s.source {
                camera_config::index::AwaitSource::Notify {
                    gatt,
                    mode,
                    seed_read,
                } => {
                    assert_eq!(
                        gatt, "0000CC09-0000-1000-8000-00805F9B34FB",
                        "source gatt resolved"
                    );
                    assert_eq!(*mode, CccdMode::Indicate);
                    assert!(*seed_read);
                }
                other => panic!("expected notify source, got {other:?}"),
            }
            assert_eq!(s.until.field, "status");
            assert_eq!(s.timeout_ms, 5000);
            assert_eq!(s.interval_ms, 250);
            // onEach's gatt resolved too.
            match &s.on_each[0] {
                Step::BleWrite(w) => {
                    assert_eq!(w.gatt, "0000CC08-0000-1000-8000-00805F9B34FB")
                }
                other => panic!("expected bleWrite in onEach, got {other:?}"),
            }
        }
        other => panic!("expected bleAwaitUntil, got {other:?}"),
    }

    // read-source (bare-string gatt), no onEach.
    let yaml = await_fixture(
        r#"                source: { read: statusChar }
                capture: { at: 0, length: 1, encoding: u8, name: color }
                until: { color: { eq: 1 } }
                timeoutMs: 3000"#,
    );
    let idx = ResolvedManufacturerIndex::from_yaml(&yaml).expect("read form loads");
    let steps = &idx.models[0]
        .ble
        .as_ref()
        .unwrap()
        .establishment("test")
        .unwrap()
        .steps;
    match &steps[1] {
        Step::BleAwaitUntil(s) => match &s.source {
            camera_config::index::AwaitSource::Read { gatt } => {
                assert_eq!(
                    gatt, "0000CC09-0000-1000-8000-00805F9B34FB",
                    "read gatt resolved"
                );
                assert!(s.on_each.is_empty());
            }
            other => panic!("expected read source, got {other:?}"),
        },
        other => panic!("expected bleAwaitUntil, got {other:?}"),
    }
}

#[test]
fn ble_await_until_notify_seed_read_defaults_and_round_trips() {
    let ordinary: AwaitSource =
        serde_yaml::from_str("notify: { gatt: statusChar }").expect("ordinary notify parses");
    assert!(matches!(
        ordinary,
        AwaitSource::Notify {
            seed_read: false,
            ..
        }
    ));
    let ordinary_yaml = serde_yaml::to_string(&ordinary).expect("ordinary notify serializes");
    assert!(
        !ordinary_yaml.contains("seedRead"),
        "false stays omitted: {ordinary_yaml}"
    );

    let seeded: AwaitSource =
        serde_yaml::from_str("notify: { gatt: statusChar, mode: indicate, seedRead: true }")
            .expect("seeded notify parses");
    let seeded_yaml = serde_yaml::to_string(&seeded).expect("seeded notify serializes");
    assert!(seeded_yaml.contains("seedRead: true"), "got: {seeded_yaml}");
    assert_eq!(
        serde_yaml::from_str::<AwaitSource>(&seeded_yaml).expect("seeded notify reparses"),
        seeded
    );
}

#[test]
fn ble_await_until_validation_rejects_bad_forms() {
    // timeoutMs: 0 — an await needs a budget.
    let yaml = await_fixture(
        r#"                source: { read: statusChar }
                until: { x: { eq: 1 } }
                timeoutMs: 0"#,
    );
    let err = ResolvedManufacturerIndex::from_yaml(&yaml).unwrap_err();
    assert!(
        err.to_string().contains("timeoutMs must be > 0"),
        "got: {err}"
    );

    // unknown source key.
    let yaml = await_fixture(
        r#"                source: { poll: statusChar }
                until: { x: { eq: 1 } }
                timeoutMs: 1000"#,
    );
    let err = ResolvedManufacturerIndex::from_yaml(&yaml).unwrap_err();
    assert!(
        err.to_string().contains("unknown awaitSource 'poll'"),
        "got: {err}"
    );

    // undefined gatt symbolic name in the source.
    let yaml = await_fixture(
        r#"                source: { read: notDeclared }
                until: { x: { eq: 1 } }
                timeoutMs: 1000"#,
    );
    let err = ResolvedManufacturerIndex::from_yaml(&yaml).unwrap_err();
    assert!(
        err.to_string()
            .contains("undefined gatt symbolic name 'notDeclared'"),
        "got: {err}"
    );
}

#[test]
fn post_exit_readiness_rejects_acquire_firmware() {
    // The gate is a fixed sequence: §11.5 firmware tiering applies to `steps`
    // only, and executors walk the gate without a refinement context — reject
    // at parse time rather than silently skipping refinement at run time.
    // Nested inside `if.then` to prove the guard recurses into branches.
    let yaml = r#"
manufacturer: TESTCO
families:
  test:
    ble:
      gatt:
        statusChar: "0000CC09-0000-1000-8000-00805F9B34FB"
      advert: { manufacturerCompanyId: 1 }
      establishments:
        test:
          mechanism: test
          activities:
            - { id: camera.test.gate, version: 1, displayRole: waitingForCamera, defaultExpectedDurationMs: 1, interactionRequired: false, executorSpan: { sequence: postExitReadiness, startStep: 0, endStepExclusive: 1 } }
            - { id: camera.test.connect, version: 1, displayRole: connecting, defaultExpectedDurationMs: 1, interactionRequired: false, executorSpan: { sequence: steps, startStep: 0, endStepExclusive: 1 } }
          postExitReadiness:
            - if:
                condition: { style: { eq: legacy } }
                then:
                  - acquireFirmware:
                      from: { bleAdvert: { offset: 0, length: 1, encoding: u8 } }
          steps:
            - bleConnect: {}
models:
  - id: tm1
    displayName: "Test"
    inherits: [test]
    manifest: tm1.yaml
"#;
    let err = ResolvedManufacturerIndex::from_yaml(yaml).unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("acquireFirmware is not allowed in postExitReadiness"),
        "got: {msg}"
    );
    assert!(
        msg.contains("postExitReadiness[0].then[0]"),
        "path names the nested step: {msg}"
    );
}

fn activity_index(activities: &str, steps: &str) -> String {
    format!(
        r#"
manufacturer: TESTCO
families:
  test:
    ble:
      gatt: {{}}
      advert: {{}}
      establishments:
        test:
          mechanism: test
          activities:
{activities}
          steps:
{steps}
models:
  - id: tm1
    displayName: Test
    inherits: [test]
    manifest: tm1.yaml
"#
    )
}

#[test]
fn connection_activity_spans_reject_invalid_coverage_and_metadata() {
    let cases = [
        (
            "requires 1",
            "            - { id: camera.test.first, version: 1, displayRole: connecting, defaultExpectedDurationMs: 1, interactionRequired: false, executorSpan: { sequence: steps, startStep: 0, endStepExclusive: 1 } }\n            - { id: camera.test.second, version: 1, displayRole: connecting, defaultExpectedDurationMs: 1, interactionRequired: false, executorSpan: { sequence: steps, startStep: 2, endStepExclusive: 3 } }",
        ),
        (
            "requires 2",
            "            - { id: camera.test.first, version: 1, displayRole: connecting, defaultExpectedDurationMs: 1, interactionRequired: false, executorSpan: { sequence: steps, startStep: 0, endStepExclusive: 2 } }\n            - { id: camera.test.second, version: 1, displayRole: connecting, defaultExpectedDurationMs: 1, interactionRequired: false, executorSpan: { sequence: steps, startStep: 1, endStepExclusive: 3 } }",
        ),
        (
            "outside sequence length",
            "            - { id: camera.test.first, version: 1, displayRole: connecting, defaultExpectedDurationMs: 1, interactionRequired: false, executorSpan: { sequence: steps, startStep: 0, endStepExclusive: 4 } }",
        ),
        (
            "duplicate activity id",
            "            - { id: camera.test.same, version: 1, displayRole: connecting, defaultExpectedDurationMs: 1, interactionRequired: false, executorSpan: { sequence: steps, startStep: 0, endStepExclusive: 1 } }\n            - { id: camera.test.same, version: 1, displayRole: connecting, defaultExpectedDurationMs: 1, interactionRequired: false, executorSpan: { sequence: steps, startStep: 1, endStepExclusive: 3 } }",
        ),
        (
            "version",
            "            - { id: camera.test.zero, version: 0, displayRole: connecting, defaultExpectedDurationMs: 1, interactionRequired: false, executorSpan: { sequence: steps, startStep: 0, endStepExclusive: 3 } }",
        ),
        (
            "defaultExpectedDurationMs",
            "            - { id: camera.test.zero, version: 1, displayRole: connecting, defaultExpectedDurationMs: 0, interactionRequired: false, executorSpan: { sequence: steps, startStep: 0, endStepExclusive: 3 } }",
        ),
        (
            "must use executorSpan",
            "            - { id: camera.test.host, version: 1, displayRole: connecting, defaultExpectedDurationMs: 1, interactionRequired: false, hostCheckpoint: { name: host } }",
        ),
    ];
    let steps =
        "            - bleConnect: {}\n            - bleConnect: {}\n            - bleConnect: {}";
    for (needle, activities) in cases {
        let error = ResolvedManufacturerIndex::from_yaml(&activity_index(activities, steps))
            .expect_err(needle);
        assert!(error.to_string().contains(needle), "{needle}: {error}");
    }
}

#[test]
fn connection_activity_span_covers_nested_steps_and_unknown_roles() {
    let activities = "            - { id: camera.test.future, version: 1, displayRole: futureRole, defaultExpectedDurationMs: 1, interactionRequired: false, executorSpan: { sequence: steps, startStep: 0, endStepExclusive: 1 } }";
    let steps = "            - if:\n                condition: { style: { eq: red } }\n                then:\n                  - bleConnect: {}\n                  - retry:\n                      steps: [{ bleConnect: {} }]\n                      whenFailure: other\n                      retryWhen: { style: { eq: red } }\n                      maxAttempts: 1";
    let index = ResolvedManufacturerIndex::from_yaml(&activity_index(activities, steps))
        .expect("a top-level span covers every nested child");
    assert!(matches!(
        index.models[0].ble.as_ref().unwrap().establishments["test"].activities[0]
            .display_role,
        camera_config::ConnectionActivityDisplayRole::Unknown(ref raw) if raw == "futureRole"
    ));

    ResolvedManufacturerIndex::from_yaml(&activity_index("            []", "            []"))
        .expect("an empty preliminary plan needs no activities");
}

#[test]
fn host_activity_checkpoints_are_unique_and_metadata_is_consistent() {
    let invalid_body = r#"
schema: camera-config/v1
camera: { manufacturer: TESTCO, model: TM1 }
connections:
  ble:
    activities:
      - { id: camera.test.first, version: 1, displayRole: connecting, defaultExpectedDurationMs: 1, interactionRequired: false, hostCheckpoint: { name: same } }
      - { id: camera.test.second, version: 1, displayRole: connecting, defaultExpectedDurationMs: 1, interactionRequired: false, hostCheckpoint: { name: same } }
"#;
    let error = camera_config::CameraManifest::from_yaml(invalid_body)
        .expect_err("duplicate host checkpoints fail");
    assert!(error.to_string().contains("duplicates checkpoint"));

    let index = data("fuji/index.yaml").replacen(
        "defaultExpectedDurationMs: 4000",
        "defaultExpectedDurationMs: 4001",
        1,
    );
    let mut bodies = BTreeMap::new();
    bodies.insert("gfx100ii".into(), data("fuji/gfx100ii/gfx100ii.yaml"));
    let error = ConfigStore::from_manufacturer_index(&index, bodies)
        .expect_err("repeated id/version metadata must agree");
    assert!(error.to_string().contains("metadata differs"));
}
