use camera_protocol_ffi::ConfigStore;
use ptp_core::{PtpCodec, PtpIpPacket};

fn manifest(guid: &str, command_framing: &str) -> String {
    format!(
        r#"
schema: camera-config/v1
camera: {{ manufacturer: TEST, model: Standard Camera }}
values:
  initiatorGuid: {{ type: fixed, value: "{guid}" }}
  initiatorName: {{ type: fixed, value: SnapBridge }}
connections:
  app:
    kind: ptpip
    initShape: standardPtpIp
    init:
      identity: {{ guid: initiatorGuid, friendlyName: initiatorName }}
    commandFraming: {command_framing}
    eventFraming: standard
    bindings: {{ command: 15740, event: 15740 }}
operations:
  "0x1001": {{ name: GetDeviceInfo, connections: [app] }}
  "0x1002": {{ name: OpenSession, connections: [app] }}
properties: {{}}
"#
    )
}

#[test]
fn standard_init_shape_builds_canonical_init_command_request() {
    let store = ConfigStore::from_bundle(
        manifest("00112233445566778899aabbccddeeff", "standard"),
        None,
    )
    .expect("standard PTP/IP manifest loads");
    let init = store
        .connection_init_with_runtime("app".into(), vec![])
        .expect("standard init resolves");
    assert_eq!(
        init.guid,
        vec![
            0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd,
            0xee, 0xff,
        ]
    );
    assert_eq!(init.friendly_name, "SnapBridge");
    assert!(init.tail.is_empty());
    assert!(matches!(
        PtpIpPacket::decode(&init.packet),
        Ok(PtpIpPacket::InitCommandRequest(request))
            if request.initiator_guid == <[u8; 16]>::try_from(init.guid).unwrap()
                && request.friendly_name == "SnapBridge"
                && request.protocol_version == 0x0001_0000
    ));
}

#[test]
fn standard_init_shape_rejects_malformed_guid_and_nonstandard_framing() {
    assert!(ConfigStore::from_bundle(manifest("0011", "standard"), None).is_err());
    assert!(ConfigStore::from_bundle(
        manifest("00112233445566778899aabbccddeeff", "compressed"),
        None,
    )
    .is_err());
}
