use camera_config::index::ResolvedManufacturerIndex;
use std::collections::BTreeMap;

use camera_sim::{walk_establishment, BleEvent, BleResponder};
use protocol_primitives::{NikonLssAuthenticationSelection, NikonLssClient, NikonLssServer};

const AUTH: &str = "00002000-3DD4-4255-8D62-6DC7B9BD5561";
const CONFIG: &str = "00002004-3DD4-4255-8D62-6DC7B9BD5561";

fn steps() -> Vec<camera_config::index::Step> {
    let index = ResolvedManufacturerIndex::from_yaml(&format!(
        r#"
manufacturer: NIKON
families:
  lss:
    ble:
      gatt: {{ auth: "{AUTH}", config: "{CONFIG}" }}
      establishments:
        pair:
          mechanism: pair
          params: [clientDeviceId, clientNonce]
          activities:
            - {{ id: camera.test.lss, version: 1, displayRole: connecting, defaultExpectedDurationMs: 1, interactionRequired: false, executorSpan: {{ sequence: steps, startStep: 0, endStepExclusive: 4 }} }}
          steps:
            - bleConnect: {{}}
            - bleDiscoverServices: {{}}
            - nikonLssAuthenticate:
                gatt: auth
                clientDeviceId: {{ runtime: clientDeviceId, encoding: bytes-raw }}
                nonce: {{ runtime: clientNonce, encoding: bytes-raw }}
                timeoutMs: 1000
            - nikonLssReadConnectionConfiguration:
                gatt: config
                flagsCaptureAs: flags
                ssidCaptureAs: ssid
                passwordCaptureAs: password
                securityModeCaptureAs: security
                sppMaxLengthCaptureAs: sppMaximumLength
models:
  - id: d850
    displayName: "D850"
    inherits: [lss]
    manifest: d850.yaml
"#
    ))
    .expect("LSS fixture loads");
    index.models[0].ble.as_ref().unwrap().establishments["pair"]
        .steps
        .clone()
}

#[test]
fn reference_walker_runs_exact_lss_exchange_and_decrypts_configuration() {
    let device_id = [0x10; 8];
    let client_nonce = [0x20; 8];
    let mut oracle_client = NikonLssClient::new(device_id, client_nonce);
    let stage1 = oracle_client.stage1_record().unwrap();
    let mut oracle_server = NikonLssServer::new(
        NikonLssAuthenticationSelection::new(7).unwrap(),
        [0x30; 8],
        [0x40; 8],
    );
    let stage2 = oracle_server.handle_stage1(&stage1).unwrap();
    let stage3 = oracle_client.handle_stage2(&stage2).unwrap();
    let (stage4, server_session) = oracle_server.finish_stage3(&stage3).unwrap();

    let mut ssid = b"D850_TEST_AP".to_vec();
    ssid.resize(32, 0);
    let mut password = b"snapbridge-password".to_vec();
    password.resize(64, 0);
    let mut config = vec![0x03];
    config.extend(server_session.encrypt(&ssid).unwrap());
    config.extend(server_session.encrypt(&password).unwrap());
    config.push(1);
    config.extend(512_u32.to_le_bytes());

    let mut responder = BleResponder::new([AUTH.into(), CONFIG.into()])
        .queue_notification(AUTH, &[0xff; 17])
        .expect_exact_write(AUTH, &stage1)
        .queue_ordered_indication(AUTH, &stage2)
        .expect_exact_write(AUTH, &stage3)
        .queue_ordered_indication(AUTH, &stage4)
        .serve_read(CONFIG, &config);
    let runtime = BTreeMap::from([
        ("clientDeviceId".into(), "1010101010101010".into()),
        ("clientNonce".into(), "2020202020202020".into()),
    ]);
    let outcome = walk_establishment(
        &mut responder,
        &steps(),
        &BTreeMap::new(),
        &BTreeMap::new(),
        &runtime,
    )
    .expect("reference walker completes");

    assert_eq!(outcome.scope.get("flags").map(String::as_str), Some("3"));
    assert_eq!(
        outcome.scope.get("ssid").map(String::as_str),
        Some("D850_TEST_AP")
    );
    assert_eq!(
        outcome.scope.get("password").map(String::as_str),
        Some("snapbridge-password")
    );
    assert_eq!(
        outcome.scope.get("security").map(String::as_str),
        Some("wpa2")
    );
    assert_eq!(
        outcome.scope.get("sppMaximumLength").map(String::as_str),
        Some("512")
    );
    assert_eq!(
        outcome.scope.len(),
        5,
        "cipher/key/runtime values stay private"
    );
    assert!(matches!(
        &responder.log()[2],
        BleEvent::Subscribe {
            mode: camera_config::index::CccdMode::Indicate,
            ..
        }
    ));
    assert_eq!(
        responder.written(AUTH),
        vec![stage1.as_slice(), stage3.as_slice()]
    );
}

#[test]
fn reference_walker_rejects_wrong_device_id_length_before_gatt_exchange() {
    let mut responder = BleResponder::new([AUTH.into(), CONFIG.into()]);
    let runtime = BTreeMap::from([
        ("clientDeviceId".into(), "1010".into()),
        ("clientNonce".into(), "2020202020202020".into()),
    ]);
    let error = match walk_establishment(
        &mut responder,
        &steps(),
        &BTreeMap::new(),
        &BTreeMap::new(),
        &runtime,
    ) {
        Ok(_) => panic!("short persistent id must fail"),
        Err(error) => error,
    };
    assert!(error.message.contains("exactly 8 bytes"), "got: {error}");
    assert!(responder.written(AUTH).is_empty());
}

#[test]
fn reference_walker_clears_optional_configuration_slots_when_flags_are_absent() {
    let device_id = [0x10; 8];
    let client_nonce = [0x20; 8];
    let mut oracle_client = NikonLssClient::new(device_id, client_nonce);
    let stage1 = oracle_client.stage1_record().unwrap();
    let mut oracle_server = NikonLssServer::new(
        NikonLssAuthenticationSelection::new(7).unwrap(),
        [0x30; 8],
        [0x40; 8],
    );
    let stage2 = oracle_server.handle_stage1(&stage1).unwrap();
    let stage3 = oracle_client.handle_stage2(&stage2).unwrap();
    let (stage4, _) = oracle_server.finish_stage3(&stage3).unwrap();
    let mut responder = BleResponder::new([AUTH.into(), CONFIG.into()])
        .expect_exact_write(AUTH, &stage1)
        .queue_ordered_indication(AUTH, &stage2)
        .expect_exact_write(AUTH, &stage3)
        .queue_ordered_indication(AUTH, &stage4)
        .serve_read(CONFIG, &[0]);
    let initial_scope: BTreeMap<String, String> = BTreeMap::from([
        ("ssid".into(), "stale-ap".into()),
        ("password".into(), "stale-password".into()),
        ("security".into(), "wpa2".into()),
        ("sppMaximumLength".into(), "512".into()),
    ]);
    let initial_encodings = initial_scope
        .keys()
        .map(|key| (key.clone(), camera_config::index::Encoding::Utf8))
        .collect();
    let runtime = BTreeMap::from([
        ("clientDeviceId".into(), "1010101010101010".into()),
        ("clientNonce".into(), "2020202020202020".into()),
    ]);

    let outcome = walk_establishment(
        &mut responder,
        &steps(),
        &initial_scope,
        &initial_encodings,
        &runtime,
    )
    .expect("configuration without optional blocks completes");

    assert_eq!(
        outcome.scope,
        BTreeMap::from([("flags".into(), "0".into())])
    );
}
