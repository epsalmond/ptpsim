use std::collections::{BTreeSet, VecDeque};
use std::sync::{Arc, Mutex};

use camera_config::{CapabilitySubject, ObservationLine};
use camera_protocol_ffi::{
    run_pcss_auto_establishment, run_pcss_known_address_establishment, ConfigStore,
    ConnectionActivityEvent, ConnectionActivityObserver, KeyValue, PcssCallback, PcssExecutorError,
    PcssExecutorTransport, TransportError,
};
use futures::executor::block_on;
use ptp_core::{InitFail, PtpCodec, PtpIpPacket};

const INDEX: &str = include_str!("../../../packages/camera-config-data/fuji/index.yaml");
const MANUFACTURER: &str = include_str!("../../../packages/camera-config-data/fuji/fuji.yaml");
const BODY: &str = include_str!("../../../packages/camera-config-data/fuji/gfx100ii/gfx100ii.yaml");
const GENERIC_BODY: &str =
    include_str!("../../../packages/camera-config-data/fuji/fuji-generic/fuji-generic.yaml");
const XA7_BODY: &str = include_str!("../../../packages/camera-config-data/fuji/xa7/xa7.yaml");

#[derive(Default)]
struct State {
    callbacks: VecDeque<PcssCallback>,
    callback_errors: VecDeque<TransportError>,
    command_replies: VecDeque<Result<Vec<u8>, TransportError>>,
    discoveries: Vec<(String, u16, Vec<u8>)>,
    callback_replies: Vec<Vec<u8>>,
    callback_reply_errors: VecDeque<TransportError>,
    callback_closes: u32,
    command_connects: Vec<(String, u16)>,
    command_frames: Vec<Vec<u8>>,
    connect_errors: VecDeque<TransportError>,
    command_send_errors: VecDeque<TransportError>,
    pending_connects: u32,
    pending_command_sends: u32,
    sleep_errors: VecDeque<TransportError>,
    command_closes: u32,
    command_close_errors: VecDeque<TransportError>,
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

    async fn next_callback(&self) -> Result<PcssCallback, TransportError> {
        let mut state = self.0.lock().unwrap();
        if let Some(error) = state.callback_errors.pop_front() {
            return Err(error);
        }
        state
            .callbacks
            .pop_front()
            .ok_or_else(|| TransportError::Timeout {
                detail: "no callback".into(),
            })
    }

    async fn send_callback_reply(&self, payload: Vec<u8>) -> Result<(), TransportError> {
        let mut state = self.0.lock().unwrap();
        state.callback_replies.push(payload);
        state.callback_reply_errors.pop_front().map_or(Ok(()), Err)
    }

    async fn close_callback_connection(&self) -> Result<(), TransportError> {
        self.0.lock().unwrap().callback_closes += 1;
        Ok(())
    }

    async fn connect_command(&self, camera_ipv4: String, port: u16) -> Result<(), TransportError> {
        let (pending, error) = {
            let mut state = self.0.lock().unwrap();
            state.command_connects.push((camera_ipv4, port));
            let pending = state.pending_connects > 0;
            state.pending_connects = state.pending_connects.saturating_sub(1);
            (pending, state.connect_errors.pop_front())
        };
        if pending {
            return std::future::pending().await;
        }
        error.map_or(Ok(()), Err)
    }

    async fn send_command_frame(&self, frame: Vec<u8>) -> Result<(), TransportError> {
        let (pending, error) = {
            let mut state = self.0.lock().unwrap();
            state.command_frames.push(frame);
            let pending = state.pending_command_sends > 0;
            state.pending_command_sends = state.pending_command_sends.saturating_sub(1);
            (pending, state.command_send_errors.pop_front())
        };
        if pending {
            return std::future::pending().await;
        }
        error.map_or(Ok(()), Err)
    }

    async fn next_command_frame(&self) -> Result<Vec<u8>, TransportError> {
        self.0
            .lock()
            .unwrap()
            .command_replies
            .pop_front()
            .unwrap_or_else(|| {
                Err(TransportError::Timeout {
                    detail: "no command reply".into(),
                })
            })
    }

    async fn close_command_connection(&self) -> Result<(), TransportError> {
        let mut state = self.0.lock().unwrap();
        state.command_closes += 1;
        state.command_close_errors.pop_front().map_or(Ok(()), Err)
    }

    async fn sleep(&self, _ms: u32) -> Result<(), TransportError> {
        self.0
            .lock()
            .unwrap()
            .sleep_errors
            .pop_front()
            .map_or(Ok(()), Err)
    }
}

fn store_with_body(body: &str) -> Arc<ConfigStore> {
    ConfigStore::from_manufacturer_index_with_defaults(
        INDEX.into(),
        MANUFACTURER.into(),
        vec![
            KeyValue {
                key: "gfx100ii".into(),
                value: body.into(),
            },
            KeyValue {
                key: "xa7".into(),
                value: XA7_BODY.into(),
            },
            KeyValue {
                key: "fuji-generic".into(),
                value: GENERIC_BODY.into(),
            },
        ],
    )
    .unwrap()
}

fn store() -> Arc<ConfigStore> {
    store_with_body(BODY)
}

fn notify(address: &str, port: u16) -> PcssCallback {
    notify_from(address, address, "GFX100 II", port)
}

fn init_ack() -> Vec<u8> {
    protocol_primitives::pcss_init_ack_message(7, [0x42; 16], "GFX100 II").unwrap()
}

fn notify_from(peer: &str, address: &str, name: &str, port: u16) -> PcssCallback {
    PcssCallback {
        peer_ipv4: peer.into(),
        payload: protocol_primitives::pcss_notify_message(
            address.parse().unwrap(),
            name,
            port,
            "PCSS/1.0",
        ),
    }
}

#[test]
fn auto_discovery_uses_the_recognized_broadcast_callback_directly() {
    let transport = Arc::new(FakeTransport::default());
    {
        let mut state = transport.0.lock().unwrap();
        state.callbacks.push_back(notify("192.0.2.44", 17555));
        state.command_replies.push_back(Ok(init_ack()));
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
    assert_eq!(state.discoveries.len(), 1);
    assert_eq!(state.discoveries[0].0, "192.0.2.255");
    assert_eq!(state.command_connects, [("192.0.2.44".into(), 17555)]);
    assert_eq!(state.command_frames.len(), 1);
    assert_eq!(
        state.callback_replies,
        vec![protocol_primitives::pcss_callback_ack_message()]
    );
}

#[test]
fn auto_discovery_ignores_a_callback_whose_peer_does_not_match_dsc() {
    let transport = Arc::new(FakeTransport::default());
    {
        let mut state = transport.0.lock().unwrap();
        state
            .callbacks
            .push_back(notify_from("192.0.2.45", "192.0.2.44", "GFX100 II", 17555));
        state.callbacks.push_back(notify("192.0.2.44", 17555));
        state.command_replies.push_back(Ok(init_ack()));
    }

    block_on(run_pcss_auto_establishment(
        store(),
        "192.0.2.255".into(),
        "192.0.2.10".into(),
        vec![0x11; 16],
        "host".into(),
        transport.clone(),
        None,
    ))
    .unwrap();

    let state = transport.0.lock().unwrap();
    assert_eq!(state.discoveries.len(), 2);
    assert_eq!(state.callback_replies.len(), 1);
    assert_eq!(state.command_connects.len(), 1);
}

#[test]
fn callback_ack_failure_closes_the_accepted_connection() {
    let transport = Arc::new(FakeTransport::default());
    {
        let mut state = transport.0.lock().unwrap();
        state.callbacks.push_back(notify("192.0.2.44", 17555));
        state
            .callback_reply_errors
            .push_back(TransportError::Failed {
                detail: "callback write failed".into(),
            });
    }

    let error = block_on(run_pcss_auto_establishment(
        store(),
        "192.0.2.255".into(),
        "192.0.2.10".into(),
        vec![0x11; 16],
        "host".into(),
        transport.clone(),
        None,
    ))
    .unwrap_err();

    assert!(matches!(error, PcssExecutorError::Transport { .. }));
    let state = transport.0.lock().unwrap();
    assert_eq!(state.callback_closes, 1);
    assert!(state.command_connects.is_empty());
}

#[test]
fn auto_discovery_retries_a_transport_callback_timeout() {
    let transport = Arc::new(FakeTransport::default());
    {
        let mut state = transport.0.lock().unwrap();
        state.callback_errors.push_back(TransportError::Timeout {
            detail: "callback receive timed out".into(),
        });
        state.callbacks.push_back(notify("192.0.2.44", 17555));
        state.command_replies.push_back(Ok(init_ack()));
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
    .expect("raw callback timeout remains inside the broadcast discovery budget");

    assert_eq!(outcome.camera_ipv4, "192.0.2.44");
    assert_eq!(transport.0.lock().unwrap().discoveries.len(), 2);
}

#[test]
fn known_address_retries_a_transport_callback_timeout() {
    let transport = Arc::new(FakeTransport::default());
    {
        let mut state = transport.0.lock().unwrap();
        state.callback_errors.push_back(TransportError::Timeout {
            detail: "callback receive timed out".into(),
        });
        state.callbacks.push_back(notify("192.0.2.44", 17555));
        state.command_replies.push_back(Ok(init_ack()));
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
    .expect("raw callback timeout remains inside the unicast discovery budget");

    assert_eq!(outcome.camera_ipv4, "192.0.2.44");
    assert_eq!(transport.0.lock().unwrap().discoveries.len(), 2);
}

#[test]
fn auto_discovery_recovers_once_by_unicast_after_command_connect_failure() {
    let transport = Arc::new(FakeTransport::default());
    {
        let mut state = transport.0.lock().unwrap();
        state.callbacks.push_back(notify("192.0.2.44", 17555));
        state.callbacks.push_back(notify("192.0.2.44", 17556));
        state
            .connect_errors
            .push_back(TransportError::ConnectFailed {
                detail: "service is still starting".into(),
            });
        state.command_replies.push_back(Ok(init_ack()));
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
    .expect("one learned-unicast rendezvous reaches the fresh endpoint");

    assert_eq!(outcome.command_port, 17556);
    let state = transport.0.lock().unwrap();
    assert_eq!(
        state
            .discoveries
            .iter()
            .map(|(address, _, _)| address.as_str())
            .collect::<Vec<_>>(),
        ["192.0.2.255", "192.0.2.44"]
    );
    assert_eq!(
        state.command_connects,
        [("192.0.2.44".into(), 17555), ("192.0.2.44".into(), 17556),]
    );
    assert_eq!(state.callback_replies.len(), 2);
}

#[test]
fn failed_command_teardown_prevents_learned_unicast_recovery() {
    let transport = Arc::new(FakeTransport::default());
    {
        let mut state = transport.0.lock().unwrap();
        state.callbacks.push_back(notify("192.0.2.44", 17555));
        state.callbacks.push_back(notify("192.0.2.44", 17556));
        state
            .connect_errors
            .push_back(TransportError::ConnectFailed {
                detail: "first endpoint unavailable".into(),
            });
        state
            .command_close_errors
            .push_back(TransportError::Failed {
                detail: "old command socket did not close".into(),
            });
    }

    let error = block_on(run_pcss_auto_establishment(
        store(),
        "192.0.2.255".into(),
        "192.0.2.10".into(),
        vec![0x11; 16],
        "host".into(),
        transport.clone(),
        None,
    ))
    .unwrap_err();

    assert!(matches!(error, PcssExecutorError::Transport { .. }));
    let state = transport.0.lock().unwrap();
    assert_eq!(state.command_closes, 1);
    assert_eq!(state.discoveries.len(), 1);
    assert_eq!(state.command_connects.len(), 1);
}

#[test]
fn auto_discovery_does_not_recover_when_the_manifest_disables_it() {
    let body = BODY.replace(
        "retryDiscoveredUnicast: true",
        "retryDiscoveredUnicast: false",
    );
    let transport = Arc::new(FakeTransport::default());
    {
        let mut state = transport.0.lock().unwrap();
        state.callbacks.push_back(notify("192.0.2.44", 17555));
        state
            .connect_errors
            .push_back(TransportError::ConnectFailed {
                detail: "endpoint unavailable".into(),
            });
    }

    let error = block_on(run_pcss_auto_establishment(
        store_with_body(&body),
        "192.0.2.255".into(),
        "192.0.2.10".into(),
        vec![0x11; 16],
        "host".into(),
        transport.clone(),
        None,
    ))
    .unwrap_err();

    assert!(matches!(error, PcssExecutorError::Transport { .. }));
    assert_eq!(transport.0.lock().unwrap().discoveries.len(), 1);
}

#[test]
fn auto_discovery_recovers_once_after_first_init_read_failure() {
    let transport = Arc::new(FakeTransport::default());
    {
        let mut state = transport.0.lock().unwrap();
        state.callbacks.push_back(notify("192.0.2.44", 17555));
        state.callbacks.push_back(notify("192.0.2.44", 17556));
        state
            .command_replies
            .push_back(Err(TransportError::NotConnected));
        state.command_replies.push_back(Ok(init_ack()));
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
    .expect("first Init socket failure permits one fresh unicast rendezvous");

    assert_eq!(outcome.command_port, 17556);
    let state = transport.0.lock().unwrap();
    assert_eq!(state.discoveries.len(), 2);
    assert_eq!(state.command_frames.len(), 2);
    assert_eq!(state.command_closes, 1);
}

#[test]
fn retryable_init_fail_reuses_the_same_socket_without_unicast() {
    let transport = Arc::new(FakeTransport::default());
    {
        let mut state = transport.0.lock().unwrap();
        state.callbacks.push_back(notify("192.0.2.44", 17555));
        state
            .command_replies
            .push_back(Ok(ptp_core::encode(&PtpIpPacket::InitFail(InitFail {
                reason: 0x2019,
            }))
            .unwrap()));
        state.command_replies.push_back(Ok(init_ack()));
    }

    block_on(run_pcss_auto_establishment(
        store(),
        "192.0.2.255".into(),
        "192.0.2.10".into(),
        vec![0x11; 16],
        "host".into(),
        transport.clone(),
        None,
    ))
    .unwrap();

    let state = transport.0.lock().unwrap();
    assert_eq!(state.discoveries.len(), 1);
    assert_eq!(state.command_connects.len(), 1);
    assert_eq!(state.command_frames.len(), 2);
    assert_eq!(state.command_frames[0], state.command_frames[1]);
}

#[test]
fn terminal_first_init_transport_failure_does_not_start_unicast_recovery() {
    let transport = Arc::new(FakeTransport::default());
    {
        let mut state = transport.0.lock().unwrap();
        state.callbacks.push_back(notify("192.0.2.44", 17555));
        state.command_send_errors.push_back(TransportError::Failed {
            detail: "local framing failure".into(),
        });
    }

    let error = block_on(run_pcss_auto_establishment(
        store(),
        "192.0.2.255".into(),
        "192.0.2.10".into(),
        vec![0x11; 16],
        "host".into(),
        transport.clone(),
        None,
    ))
    .unwrap_err();

    assert!(matches!(error, PcssExecutorError::Transport { .. }));
    let state = transport.0.lock().unwrap();
    assert_eq!(state.discoveries.len(), 1);
    assert_eq!(state.command_connects.len(), 1);
}

#[test]
fn second_init_io_failure_after_retryable_rejection_is_terminal() {
    let transport = Arc::new(FakeTransport::default());
    {
        let mut state = transport.0.lock().unwrap();
        state.callbacks.push_back(notify("192.0.2.44", 17555));
        state
            .command_replies
            .push_back(Ok(ptp_core::encode(&PtpIpPacket::InitFail(InitFail {
                reason: 0x2019,
            }))
            .unwrap()));
        state
            .command_replies
            .push_back(Err(TransportError::NotConnected));
    }

    let error = block_on(run_pcss_auto_establishment(
        store(),
        "192.0.2.255".into(),
        "192.0.2.10".into(),
        vec![0x11; 16],
        "host".into(),
        transport.clone(),
        None,
    ))
    .unwrap_err();

    assert!(matches!(error, PcssExecutorError::Transport { .. }));
    let state = transport.0.lock().unwrap();
    assert_eq!(state.discoveries.len(), 1);
    assert_eq!(state.command_frames.len(), 2);
}

#[test]
fn recovery_endpoint_failure_does_not_start_a_third_rendezvous() {
    let transport = Arc::new(FakeTransport::default());
    {
        let mut state = transport.0.lock().unwrap();
        state.callbacks.push_back(notify("192.0.2.44", 17555));
        state.callbacks.push_back(notify("192.0.2.44", 17556));
        state
            .connect_errors
            .push_back(TransportError::ConnectFailed {
                detail: "first endpoint unavailable".into(),
            });
        state
            .connect_errors
            .push_back(TransportError::ConnectFailed {
                detail: "recovered endpoint unavailable".into(),
            });
    }

    let error = block_on(run_pcss_auto_establishment(
        store(),
        "192.0.2.255".into(),
        "192.0.2.10".into(),
        vec![0x11; 16],
        "host".into(),
        transport.clone(),
        None,
    ))
    .unwrap_err();

    assert!(matches!(error, PcssExecutorError::Transport { .. }));
    let state = transport.0.lock().unwrap();
    assert_eq!(state.discoveries.len(), 2);
    assert_eq!(state.command_connects.len(), 2);
}

#[test]
fn foreign_clock_failure_during_connect_is_terminal() {
    let transport = Arc::new(FakeTransport::default());
    {
        let mut state = transport.0.lock().unwrap();
        state.callbacks.push_back(notify("192.0.2.44", 17555));
        state.pending_connects = 1;
        state.sleep_errors.push_back(TransportError::Failed {
            detail: "clock unavailable".into(),
        });
    }

    let error = block_on(run_pcss_auto_establishment(
        store(),
        "192.0.2.255".into(),
        "192.0.2.10".into(),
        vec![0x11; 16],
        "host".into(),
        transport.clone(),
        None,
    ))
    .unwrap_err();

    assert!(matches!(error, PcssExecutorError::Transport { .. }));
    assert_eq!(transport.0.lock().unwrap().discoveries.len(), 1);
}

#[test]
fn hung_first_init_write_deadline_permits_one_unicast_recovery() {
    let transport = Arc::new(FakeTransport::default());
    {
        let mut state = transport.0.lock().unwrap();
        state.callbacks.push_back(notify("192.0.2.44", 17555));
        state.callbacks.push_back(notify("192.0.2.44", 17556));
        state.pending_command_sends = 1;
        state.command_replies.push_back(Ok(init_ack()));
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

    assert_eq!(outcome.command_port, 17556);
    let state = transport.0.lock().unwrap();
    assert_eq!(state.discoveries.len(), 2);
    assert_eq!(state.command_frames.len(), 2);
}

#[test]
fn invalid_init_identity_fails_before_any_network_io() {
    let transport = Arc::new(FakeTransport::default());

    let error = block_on(run_pcss_auto_establishment(
        store(),
        "192.0.2.255".into(),
        "192.0.2.10".into(),
        vec![0x11; 16],
        "friendly-name-is-too-long".into(),
        transport.clone(),
        None,
    ))
    .unwrap_err();

    assert!(matches!(
        error,
        PcssExecutorError::InvalidInitResponse { .. }
    ));
    let state = transport.0.lock().unwrap();
    assert_eq!(state.listener_port, None);
    assert!(state.discoveries.is_empty());
}

#[test]
fn init_fail_retry_matching_preserves_full_u32_reason() {
    let body = BODY.replace("whenReasons: [\"0x2019\"]", "whenReasons: [\"0x00012019\"]");
    let transport = Arc::new(FakeTransport::default());
    {
        let mut state = transport.0.lock().unwrap();
        state.callbacks.push_back(notify("192.0.2.44", 17555));
        state
            .command_replies
            .push_back(Ok(ptp_core::encode(&PtpIpPacket::InitFail(InitFail {
                reason: 0x0001_2019,
            }))
            .unwrap()));
        state.command_replies.push_back(Ok(init_ack()));
    }

    let outcome = block_on(run_pcss_known_address_establishment(
        store_with_body(&body),
        "gfx100ii".into(),
        "wireless-tether".into(),
        "192.0.2.44".into(),
        "192.0.2.10".into(),
        vec![0x11; 16],
        "host".into(),
        transport.clone(),
        None,
    ))
    .expect("full-width configured reason retries Init and reaches the ack");

    assert_eq!(outcome.connection_number, 7);
    assert_eq!(transport.0.lock().unwrap().command_frames.len(), 2);
}

#[test]
fn known_address_skips_broadcast_and_unknown_identity_fails_loud() {
    let transport = Arc::new(FakeTransport::default());
    let activity = Arc::new(ActivityRecorder::default());
    transport.0.lock().unwrap().callbacks.push_back(notify_from(
        "192.0.2.44",
        "192.0.2.44",
        "Different Camera",
        15740,
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
    assert!(
        activity.0.lock().unwrap().is_empty(),
        "the PCSS executor must not emit body hostCheckpoint activities"
    );
}

#[test]
fn known_address_rejects_a_callback_from_a_different_endpoint() {
    let transport = Arc::new(FakeTransport::default());
    {
        let mut state = transport.0.lock().unwrap();
        state
            .callbacks
            .push_back(notify_from("192.0.2.45", "192.0.2.45", "GFX100 II", 17555));
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

    let state = transport.0.lock().unwrap();
    assert!(matches!(error, PcssExecutorError::IdentityMismatch { .. }));
    assert_eq!(state.discoveries[0].0, "192.0.2.44");
    assert!(state.command_connects.is_empty());
}

#[test]
fn known_address_connect_failure_does_not_enter_auto_recovery() {
    let transport = Arc::new(FakeTransport::default());
    {
        let mut state = transport.0.lock().unwrap();
        state.callbacks.push_back(notify("192.0.2.44", 17555));
        state
            .connect_errors
            .push_back(TransportError::ConnectFailed {
                detail: "endpoint unavailable".into(),
            });
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

    assert!(matches!(error, PcssExecutorError::Transport { .. }));
    let state = transport.0.lock().unwrap();
    assert_eq!(state.discoveries.len(), 1);
    assert_eq!(state.discoveries[0].0, "192.0.2.44");
}

#[test]
fn header_only_init_ack_is_terminal_and_closes_the_command_connection() {
    let transport = Arc::new(FakeTransport::default());
    {
        let mut state = transport.0.lock().unwrap();
        state.callbacks.push_back(notify("192.0.2.44", 17555));
        state
            .command_replies
            .push_back(Ok(vec![8, 0, 0, 0, 2, 0, 0, 0]));
    }

    let error = block_on(run_pcss_auto_establishment(
        store(),
        "192.0.2.255".into(),
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
    let state = transport.0.lock().unwrap();
    assert_eq!(state.command_closes, 1);
    assert_eq!(state.discoveries.len(), 1);
}

#[test]
fn init_fail_with_trailing_bytes_is_terminal_without_recovery() {
    let transport = Arc::new(FakeTransport::default());
    let mut malformed = ptp_core::encode(&PtpIpPacket::InitFail(InitFail { reason: 0x2019 }))
        .expect("encode canonical InitFail");
    malformed.extend_from_slice(&[0, 0, 0, 0]);
    let malformed_len = malformed.len() as u32;
    malformed[0..4].copy_from_slice(&malformed_len.to_le_bytes());
    assert!(matches!(
        PtpIpPacket::decode(&malformed),
        Ok(PtpIpPacket::InitFail(_))
    ));
    {
        let mut state = transport.0.lock().unwrap();
        state.callbacks.push_back(notify("192.0.2.44", 17555));
        state.command_replies.push_back(Ok(malformed));
    }

    let error = block_on(run_pcss_auto_establishment(
        store(),
        "192.0.2.255".into(),
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
    let state = transport.0.lock().unwrap();
    assert_eq!(state.discoveries.len(), 1);
    assert_eq!(state.command_frames.len(), 1);
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
    let captured: Vec<ObservationLine> = capture
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let captured_operations: BTreeSet<String> = captured
        .iter()
        .filter_map(|row| match row {
            ObservationLine::Capability(capability) => match &capability.subject {
                CapabilitySubject::Operation {
                    code,
                    supported: true,
                    ..
                } => Some(code.to_ascii_lowercase()),
                _ => None,
            },
            _ => None,
        })
        .collect();
    let captured_properties: BTreeSet<String> = captured
        .iter()
        .filter_map(|row| match row {
            ObservationLine::Capability(capability) => match &capability.subject {
                CapabilitySubject::Property {
                    code,
                    supported: true,
                    ..
                } => Some(code.to_ascii_lowercase()),
                _ => None,
            },
            _ => None,
        })
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
