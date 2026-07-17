use std::collections::BTreeMap;
use std::path::PathBuf;

use camera_config::index::{CccdMode, ResolvedManufacturerIndex};
use camera_sim::{walk_establishment, BleEvent, BleResponder};
use protocol_primitives::{
    NikonLssAuthenticationSelection, NikonLssClient, NikonLssServer, NikonLssSession,
};

const AUTH: &str = "00002000-3DD4-4255-8D62-6DC7B9BD5561";
const CLIENT_NAME: &str = "00002002-3DD4-4255-8D62-6DC7B9BD5561";
const SERVER_NAME: &str = "00002003-3DD4-4255-8D62-6DC7B9BD5561";
const CONFIG: &str = "00002004-3DD4-4255-8D62-6DC7B9BD5561";
const ESTABLISHMENT: &str = "00002005-3DD4-4255-8D62-6DC7B9BD5561";
const CONTROL_POINT: &str = "00002008-3DD4-4255-8D62-6DC7B9BD5561";
const FEATURE: &str = "00002009-3DD4-4255-8D62-6DC7B9BD5561";
const CABLE_ATTACHMENT: &str = "0000200A-3DD4-4255-8D62-6DC7B9BD5561";
const SERIAL: &str = "0000200B-3DD4-4255-8D62-6DC7B9BD5561";
const STATUS_FOR_CONTROL: &str = "00002020-3DD4-4255-8D62-6DC7B9BD5561";
const CONTROL_POINT_FOR_CONTROL: &str = "00002021-3DD4-4255-8D62-6DC7B9BD5561";
const STATUS_FOR_CAPTURE: &str = "00002081-3DD4-4255-8D62-6DC7B9BD5561";
const MODEL: &str = "00002A24-0000-1000-8000-00805F9B34FB";
const FIRMWARE: &str = "00002A26-0000-1000-8000-00805F9B34FB";

fn data(rel: &str) -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../packages/camera-config-data")
        .join(rel);
    std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("read {}: {error}", path.display()))
}

fn oracle_exchange(
    client_device_id: [u8; 8],
    client_nonce: [u8; 8],
    camera_nonce: [u8; 8],
    server_device_id: [u8; 8],
) -> ([u8; 17], [u8; 17], [u8; 17], [u8; 17], NikonLssSession) {
    let mut client = NikonLssClient::new(client_device_id, client_nonce);
    let stage1 = client.stage1_record().expect("oracle stage 1");
    let mut server = NikonLssServer::new(
        NikonLssAuthenticationSelection::new(7).expect("selection 7"),
        camera_nonce,
        server_device_id,
    );
    let stage2 = server.handle_stage1(&stage1).expect("oracle stage 2");
    let stage3 = client.handle_stage2(&stage2).expect("oracle stage 3");
    let (stage4, session) = server.finish_stage3(&stage3).expect("oracle stage 4");
    (stage1, stage2, stage3, stage4, session)
}

fn cccd_events(log: &[BleEvent]) -> Vec<(&str, CccdMode)> {
    log.iter()
        .filter_map(|event| match event {
            BleEvent::Subscribe { uuid, mode } => Some((uuid.as_str(), *mode)),
            _ => None,
        })
        .collect()
}

fn read_events(log: &[BleEvent]) -> Vec<&str> {
    log.iter()
        .filter_map(|event| match event {
            BleEvent::Read { uuid } => Some(uuid.as_str()),
            _ => None,
        })
        .collect()
}

fn assert_no_private_lss_state(scope: &BTreeMap<String, String>, log: &[BleEvent]) {
    for forbidden in [
        "clientDeviceId",
        "clientNonce",
        "sessionKey",
        "cipherContext",
        "expandedSchedule",
    ] {
        assert!(!scope.contains_key(forbidden), "scope exposed {forbidden}");
    }

    let trace = format!("{log:?}");
    for forbidden in [
        "sessionKey",
        "cipherContext",
        "expandedSchedule",
        "D850_TEST_AP",
        "snapbridge-password",
    ] {
        assert!(!trace.contains(forbidden), "trace exposed {forbidden}");
    }
}

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

#[test]
fn real_nikon_plans_pair_then_decrypt_wifi_configuration() {
    let index = ResolvedManufacturerIndex::from_yaml(&data("nikon/index.yaml"))
        .expect("real Nikon index loads");
    let d850 = index
        .models
        .iter()
        .find(|model| model.id == "d850")
        .expect("D850 model view");
    let ble = d850.ble.as_ref().expect("D850 inherits Nikon BLE");
    let pair = ble.establishment("ble-pair").expect("pair plan");
    let wifi = ble
        .establishment("ble-establish-wifi-ap")
        .expect("Wi-Fi handoff plan");
    let catalog: Vec<String> = ble.gatt.values().cloned().collect();
    let expected_cccd = vec![
        (CONTROL_POINT, CccdMode::Notify),
        (CABLE_ATTACHMENT, CccdMode::Notify),
        (STATUS_FOR_CAPTURE, CccdMode::Notify),
        (AUTH, CccdMode::Indicate),
        (STATUS_FOR_CONTROL, CccdMode::Indicate),
        (CONTROL_POINT_FOR_CONTROL, CccdMode::Indicate),
    ];

    let client_device_id = [0x10, 0x11, 0x12, 0x13, 0x14, 0x15, 0x16, 0x17];
    let pair_nonce = [0x20, 0x21, 0x22, 0x23, 0x24, 0x25, 0x26, 0x27];
    let (pair_stage1, pair_stage2, pair_stage3, pair_stage4, _pair_session) =
        oracle_exchange(client_device_id, pair_nonce, [0x30; 8], [0x40; 8]);
    let mut pair_responder = BleResponder::new(catalog.clone())
        .expect_exact_write(AUTH, &pair_stage1)
        .queue_ordered_indication(AUTH, &pair_stage2)
        .expect_exact_write(AUTH, &pair_stage3)
        .queue_ordered_indication(AUTH, &pair_stage4)
        .serve_read(SERVER_NAME, b"D850\0")
        .serve_read(FEATURE, &[0x01, 0x02, 0x03, 0x04])
        .serve_read(MODEL, b"D850\0")
        .serve_read(SERIAL, b"D850-SERIAL\0")
        .serve_read(FIRMWARE, b"1.30\0");
    let pair_runtime = BTreeMap::from([
        ("clientDeviceId".into(), "1011121314151617".into()),
        ("clientNonce".into(), "2021222324252627".into()),
        ("snapBridgeClientName".into(), "Android_ptpsim".into()),
    ]);
    let pair_outcome = walk_establishment(
        &mut pair_responder,
        &pair.steps,
        &BTreeMap::new(),
        &BTreeMap::new(),
        &pair_runtime,
    )
    .expect("real Nikon pair plan completes");

    assert_eq!(cccd_events(pair_responder.log()), expected_cccd);
    assert_eq!(
        pair_responder.written(AUTH),
        vec![pair_stage1.as_slice(), pair_stage3.as_slice()]
    );
    assert_eq!(pair_stage2.len(), 17);
    assert_eq!(pair_stage4.len(), 17);
    let mut padded_client_name = b"Android_ptpsim".to_vec();
    padded_client_name.resize(32, 0);
    assert_eq!(
        pair_responder.written(CLIENT_NAME),
        vec![padded_client_name.as_slice()]
    );
    assert_eq!(
        read_events(pair_responder.log()),
        vec![
            SERVER_NAME,
            FEATURE,
            MODEL,
            SERIAL,
            FIRMWARE,
            FEATURE,
            FEATURE,
            FEATURE,
        ]
    );
    assert_eq!(
        read_events(pair_responder.log())
            .into_iter()
            .filter(|uuid| *uuid == FEATURE)
            .count(),
        4
    );
    assert_eq!(
        pair_outcome
            .scope
            .get("serverDeviceName")
            .map(String::as_str),
        Some("D850")
    );
    assert_no_private_lss_state(&pair_outcome.scope, pair_responder.log());

    let wifi_nonce = [0x50, 0x51, 0x52, 0x53, 0x54, 0x55, 0x56, 0x57];
    let (wifi_stage1, wifi_stage2, wifi_stage3, wifi_stage4, wifi_session) =
        oracle_exchange(client_device_id, wifi_nonce, [0x60; 8], [0x70; 8]);
    let mut ssid = b"D850_TEST_AP".to_vec();
    ssid.resize(32, 0);
    let mut password = b"snapbridge-password".to_vec();
    password.resize(64, 0);
    let mut encrypted_configuration = vec![0x03];
    encrypted_configuration.extend(wifi_session.encrypt(&ssid).expect("encrypt SSID"));
    encrypted_configuration.extend(wifi_session.encrypt(&password).expect("encrypt password"));
    encrypted_configuration.push(1);
    encrypted_configuration.extend(1024_u32.to_le_bytes());

    let mut wifi_responder = BleResponder::new(catalog)
        .expect_exact_write(AUTH, &wifi_stage1)
        .queue_ordered_indication(AUTH, &wifi_stage2)
        .expect_exact_write(AUTH, &wifi_stage3)
        .queue_ordered_indication(AUTH, &wifi_stage4)
        .serve_read(CONFIG, &encrypted_configuration);
    let wifi_runtime = BTreeMap::from([
        ("clientDeviceId".into(), "1011121314151617".into()),
        ("clientNonce".into(), "5051525354555657".into()),
    ]);
    let wifi_outcome = walk_establishment(
        &mut wifi_responder,
        &wifi.steps,
        &BTreeMap::new(),
        &BTreeMap::new(),
        &wifi_runtime,
    )
    .expect("real Nikon Wi-Fi handoff completes");

    assert_eq!(cccd_events(wifi_responder.log()), expected_cccd);
    assert_eq!(
        wifi_responder.written(AUTH),
        vec![wifi_stage1.as_slice(), wifi_stage3.as_slice()]
    );
    assert_eq!(wifi_stage2.len(), 17);
    assert_eq!(wifi_stage4.len(), 17);
    assert_eq!(
        wifi_responder.written(ESTABLISHMENT),
        vec![&[0x02][..]],
        "0x02 starts Wi-Fi; 0x01 is the BTC/SPP path"
    );
    assert_eq!(read_events(wifi_responder.log()), vec![CONFIG]);
    assert_eq!(
        wifi_outcome
            .scope
            .get("connectionFlags")
            .map(String::as_str),
        Some("3")
    );
    assert_eq!(
        wifi_outcome.scope.get("ssid").map(String::as_str),
        Some("D850_TEST_AP")
    );
    assert_eq!(
        wifi_outcome.scope.get("password").map(String::as_str),
        Some("snapbridge-password")
    );
    assert_eq!(
        wifi_outcome.scope.get("securityMode").map(String::as_str),
        Some("wpa2")
    );
    assert_eq!(
        wifi_outcome
            .scope
            .get("sppMaximumLength")
            .map(String::as_str),
        Some("1024")
    );
    assert_no_private_lss_state(&wifi_outcome.scope, wifi_responder.log());
}
