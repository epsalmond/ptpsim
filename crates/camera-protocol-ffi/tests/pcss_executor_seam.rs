use std::collections::{BTreeSet, VecDeque};
use std::sync::{Arc, Mutex};

use camera_protocol_ffi::{
    run_pcss_auto_establishment, run_pcss_known_address_establishment, ConfigStore,
    ConnectionActivityEvent, ConnectionActivityObserver, KeyValue, PcssExecutorError,
    PcssExecutorTransport, TransportError,
};
use futures::executor::block_on;
use ptp_core::{InitCommandAck, InitFail, PtpIpPacket};

const INDEX: &str = include_str!("../../../packages/camera-config-data/fuji/index.yaml");
const BODY: &str = include_str!("../../../packages/camera-config-data/fuji/gfx100ii/gfx100ii.yaml");
const XA7_BODY: &str = include_str!("../../../packages/camera-config-data/fuji/xa7/xa7.yaml");

#[derive(Default)]
struct State {
    callbacks: VecDeque<Vec<u8>>,
    command_replies: VecDeque<Vec<u8>>,
    discoveries: Vec<(String, u16, Vec<u8>)>,
    callback_replies: Vec<Vec<u8>>,
    command_connects: Vec<(String, u16)>,
    command_frames: Vec<Vec<u8>>,
    connect_failures: u32,
    command_closes: u32,
    listener_port: Option<u16>,
}

#[derive(Default)]
struct FakeTransport(Mutex<State>);

#[derive(Default)]
struct ActivityRecorder(Mutex<Vec<ConnectionActivityEvent>>);

impl ConnectionActivityObserver for ActivityRecorder {
    fn on_activity(&self, event: ConnectionActivityEvent) {
        self.0.lock().unwrap().push(event);
    }
}

#[async_trait::async_trait]
impl PcssExecutorTransport for FakeTransport {
    async fn bind_callback_listener(&self, port: u16) -> Result<(), TransportError> {
        self.0.lock().unwrap().listener_port = Some(port);
        Ok(())
    }

    async fn send_discovery(
        &self,
        destination_ipv4: String,
        destination_port: u16,
        payload: Vec<u8>,
    ) -> Result<(), TransportError> {
        self.0
            .lock()
            .unwrap()
            .discoveries
            .push((destination_ipv4, destination_port, payload));
        Ok(())
    }

    async fn next_callback(&self) -> Result<Vec<u8>, TransportError> {
        self.0
            .lock()
            .unwrap()
            .callbacks
            .pop_front()
            .ok_or_else(|| TransportError::Timeout {
                detail: "no callback".into(),
            })
    }

    async fn send_callback_reply(&self, payload: Vec<u8>) -> Result<(), TransportError> {
        self.0.lock().unwrap().callback_replies.push(payload);
        Ok(())
    }

    async fn close_callback_connection(&self) -> Result<(), TransportError> {
        Ok(())
    }

    async fn connect_command(&self, camera_ipv4: String, port: u16) -> Result<(), TransportError> {
        let mut state = self.0.lock().unwrap();
        state.command_connects.push((camera_ipv4, port));
        if state.connect_failures > 0 {
            state.connect_failures -= 1;
            Err(TransportError::ConnectFailed {
                detail: "service is still starting".into(),
            })
        } else {
            Ok(())
        }
    }

    async fn send_command_frame(&self, frame: Vec<u8>) -> Result<(), TransportError> {
        self.0.lock().unwrap().command_frames.push(frame);
        Ok(())
    }

    async fn next_command_frame(&self) -> Result<Vec<u8>, TransportError> {
        self.0
            .lock()
            .unwrap()
            .command_replies
            .pop_front()
            .ok_or_else(|| TransportError::Timeout {
                detail: "no command reply".into(),
            })
    }

    async fn close_command_connection(&self) -> Result<(), TransportError> {
        self.0.lock().unwrap().command_closes += 1;
        Ok(())
    }

    async fn sleep(&self, _ms: u32) -> Result<(), TransportError> {
        Ok(())
    }
}

fn store() -> Arc<ConfigStore> {
    ConfigStore::from_manufacturer_index(
        INDEX.into(),
        vec![
            KeyValue {
                key: "gfx100ii".into(),
                value: BODY.into(),
            },
            KeyValue {
                key: "xa7".into(),
                value: XA7_BODY.into(),
            },
        ],
    )
    .unwrap()
}

fn notify(address: &str, port: u16) -> Vec<u8> {
    protocol_primitives::pcss_notify_message(
        address.parse().unwrap(),
        "GFX100 II",
        port,
        "PCSS/1.0",
    )
}

fn init_ack() -> Vec<u8> {
    ptp_core::encode(&PtpIpPacket::InitCommandAck(InitCommandAck {
        connection_number: 7,
        responder_guid: [0x42; 16],
        friendly_name: "GFX100 II".into(),
        protocol_version: 0x0001_0000,
    }))
    .unwrap()
}

#[test]
fn auto_discovery_converges_on_unicast_and_reuses_socket_for_device_busy() {
    let transport = Arc::new(FakeTransport::default());
    {
        let mut state = transport.0.lock().unwrap();
        state.callbacks.push_back(notify("192.0.2.44", 17555));
        state.callbacks.push_back(notify("192.0.2.44", 17555));
        state.callbacks.push_back(notify("192.0.2.44", 17555));
        state.connect_failures = 1;
        state.command_replies.push_back(
            ptp_core::encode(&PtpIpPacket::InitFail(InitFail { reason: 0x2019 })).unwrap(),
        );
        state.command_replies.push_back(init_ack());
    }

    let outcome = block_on(run_pcss_auto_establishment(
        store(),
        "192.0.2.255".into(),
        "192.0.2.10".into(),
        vec![0x11; 16],
        "host".into(),
        transport.clone(),
        None,
    ))
    .unwrap();

    assert_eq!(outcome.model, "gfx100ii");
    assert_eq!(outcome.camera_ipv4, "192.0.2.44");
    assert_eq!(outcome.command_port, 17555);
    let state = transport.0.lock().unwrap();
    assert_eq!(state.listener_port, Some(51560));
    assert_eq!(state.discoveries[0].0, "192.0.2.255");
    assert_eq!(state.discoveries[1].0, "192.0.2.44");
    assert_eq!(state.discoveries[2].0, "192.0.2.44");
    assert!(state
        .command_connects
        .iter()
        .all(|endpoint| endpoint == &("192.0.2.44".into(), 17555)));
    assert_eq!(state.command_frames.len(), 2);
    assert_eq!(state.command_frames[0], state.command_frames[1]);
    assert_eq!(
        state.callback_replies,
        vec![protocol_primitives::pcss_callback_ack_message(); 3]
    );
}

#[test]
fn known_address_skips_broadcast_and_unknown_identity_fails_loud() {
    let transport = Arc::new(FakeTransport::default());
    let activity = Arc::new(ActivityRecorder::default());
    transport
        .0
        .lock()
        .unwrap()
        .callbacks
        .push_back(protocol_primitives::pcss_notify_message(
            "192.0.2.44".parse().unwrap(),
            "Different Camera",
            15740,
            "PCSS/1.0",
        ));

    let error = block_on(run_pcss_known_address_establishment(
        store(),
        "gfx100ii".into(),
        "wireless-tether".into(),
        "192.0.2.44".into(),
        "192.0.2.10".into(),
        vec![0x11; 16],
        "host".into(),
        transport.clone(),
        Some(activity.clone()),
    ))
    .unwrap_err();

    assert!(matches!(error, PcssExecutorError::IdentityMismatch { .. }));
    let state = transport.0.lock().unwrap();
    assert_eq!(state.discoveries.len(), 1);
    assert_eq!(state.discoveries[0].0, "192.0.2.44");
    let events = activity.0.lock().unwrap();
    assert!(matches!(
        events.first(),
        Some(ConnectionActivityEvent::Started { .. })
    ));
    assert!(matches!(
        events.last(),
        Some(ConnectionActivityEvent::Failed { .. })
    ));
    assert!(!events
        .iter()
        .any(|event| matches!(event, ConnectionActivityEvent::Cancelled { .. })));
}

#[test]
fn known_address_uses_the_endpoint_advertised_by_notify() {
    let transport = Arc::new(FakeTransport::default());
    {
        let mut state = transport.0.lock().unwrap();
        state.callbacks.push_back(notify("192.0.2.45", 17555));
        state.command_replies.push_back(init_ack());
    }

    let outcome = block_on(run_pcss_known_address_establishment(
        store(),
        "gfx100ii".into(),
        "wireless-tether".into(),
        "192.0.2.44".into(),
        "192.0.2.10".into(),
        vec![0x11; 16],
        "host".into(),
        transport.clone(),
        None,
    ))
    .unwrap();

    let state = transport.0.lock().unwrap();
    assert_eq!(state.discoveries[0].0, "192.0.2.44");
    assert_eq!(state.command_connects, [("192.0.2.45".into(), 17555)]);
    assert_eq!(outcome.camera_ipv4, "192.0.2.45");
}

#[test]
fn malformed_init_response_closes_the_command_connection() {
    let transport = Arc::new(FakeTransport::default());
    {
        let mut state = transport.0.lock().unwrap();
        state.callbacks.push_back(notify("192.0.2.44", 17555));
        state.command_replies.push_back(vec![1, 2, 3]);
    }

    let error = block_on(run_pcss_known_address_establishment(
        store(),
        "gfx100ii".into(),
        "wireless-tether".into(),
        "192.0.2.44".into(),
        "192.0.2.10".into(),
        vec![0x11; 16],
        "host".into(),
        transport.clone(),
        None,
    ))
    .unwrap_err();

    assert!(matches!(
        error,
        PcssExecutorError::InvalidInitResponse { .. }
    ));
    assert_eq!(transport.0.lock().unwrap().command_closes, 1);
}

#[test]
fn semantic_properties_preserve_row_provenance_and_encode_focus_text() {
    let store = store();
    let property = store
        .properties()
        .into_iter()
        .find(|property| property.code == 0xd20c)
        .unwrap();
    assert_eq!(property.name, "simultaneousCardRecording");
    assert_eq!(property.value_rows.len(), 4);
    assert_eq!(property.value_rows[0].evidence, ["wirePcssParity20260714"]);
    assert_eq!(property.value_rows[2].evidence, ["sdkPcssParity20260714"]);
    assert_eq!(
        store.decode_property(0xd20c, 1).unwrap().evidence,
        ["wirePcssParity20260714"]
    );
    assert_eq!(
        store
            .encode_structured_integer_property(0xd395, vec![-3, 2, 4])
            .unwrap(),
        vec![7, 45, 0, 51, 0, 44, 0, 50, 0, 44, 0, 52, 0, 0, 0]
    );
    assert!(store
        .encode_structured_integer_property(0xd395, vec![1, 2])
        .is_err());
    let focus = store
        .properties()
        .into_iter()
        .find(|property| property.code == 0xd395)
        .unwrap();
    let layout = focus.structured_text.unwrap();
    assert_eq!(layout.delimiter, ",");
    assert_eq!(
        layout
            .fields
            .iter()
            .map(|field| field.name.as_str())
            .collect::<Vec<_>>(),
        ["x", "y", "size"]
    );
}

#[test]
fn libgphoto2_parity_inventory_is_machine_reviewable() {
    let artifact: serde_yaml::Value = serde_yaml::from_str(include_str!(
        "../../../packages/camera-config-data/fuji/gfx100ii/evidence/pcss-libgphoto2-parity.yaml"
    ))
    .unwrap();
    let capture = include_str!(
        "../../../packages/camera-config-data/fuji/gfx100ii/evidence/probe/2026-05-27-ptp-evidence-wireless-stills.jsonl"
    );
    let captured: Vec<serde_json::Value> = capture
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let captured_operations: BTreeSet<String> = captured
        .iter()
        .filter(|row| row["kind"] == "operation" && row["supported"] == true)
        .map(|row| row["code"].as_str().unwrap().to_ascii_lowercase())
        .collect();
    let captured_properties: BTreeSet<String> = captured
        .iter()
        .filter(|row| row["kind"] == "property" && row["supported"] == true)
        .map(|row| row["code"].as_str().unwrap().to_ascii_lowercase())
        .collect();
    assert_eq!(captured_operations.len(), 24);
    assert_eq!(captured_properties.len(), 287);

    let compared_operations: BTreeSet<String> = artifact["operations"]
        .as_sequence()
        .unwrap()
        .iter()
        .map(|row| row["code"].as_str().unwrap().to_ascii_lowercase())
        .collect();
    assert_eq!(compared_operations, captured_operations);

    let consolidated: serde_yaml::Value = serde_yaml::from_str(include_str!(
        "../../../packages/camera-config-data/fuji/gfx100ii/gfx100ii.consolidated.yaml"
    ))
    .unwrap();
    let generated_properties: BTreeSet<String> = consolidated["properties"]
        .as_mapping()
        .unwrap()
        .keys()
        .map(|key| key.as_str().unwrap().to_ascii_lowercase())
        .collect();
    assert!(captured_properties.is_subset(&generated_properties));

    let allowed: BTreeSet<&str> = [
        "alreadyModeled",
        "safeToAdd",
        "modelOrFirmwareDependent",
        "conflictsWithCapture",
        "requiresFocusedCapture",
    ]
    .into_iter()
    .collect();
    for section in ["operations", "properties", "workflows", "transport"] {
        assert!(artifact[section].as_sequence().unwrap().iter().all(|row| {
            allowed.contains(
                row["classification"]
                    .as_str()
                    .expect("classification is text"),
            )
        }));
    }

    let d20c = artifact["properties"]
        .as_sequence()
        .unwrap()
        .iter()
        .find(|row| row["code"] == "0xd20c")
        .unwrap();
    assert_eq!(d20c["rows"].as_sequence().unwrap().len(), 4);
    assert_eq!(d20c["rows"][0]["provenance"], "wirePcssParity20260714");
    assert_eq!(d20c["rows"][2]["provenance"], "sdkPcssParity20260714");
}
