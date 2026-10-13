#![no_main]

use std::collections::VecDeque;
use std::future::pending;
use std::sync::{Arc, Mutex, OnceLock};

use camera_protocol_ffi::{
    run_initiator_action, ActionInvocationRequest, ActionRole, ConfigStore,
    ConnectionActivityEvent, ConnectionActivityObserver, PtpExecutorTransport, PtpFraming,
    PtpSessionOpenResult, PtpTransportError, SocketRole, StepObserver, StepReport,
};
use libfuzzer_sys::fuzz_target;
use ptp_core::{DataBlock, OperationResponse, PtpIpPacket, StartData};

const OPERATION: u16 = 0x1015;
const TRANSACTION_ID: u32 = 1;
const MAX_SYNTHETIC_PAYLOAD: usize = 4096;

struct NullSteps;

impl StepObserver for NullSteps {
    fn on_step(&self, _report: StepReport) {}
}

struct NullActivities;

impl ConnectionActivityObserver for NullActivities {
    fn on_activity(&self, _event: ConnectionActivityEvent) {}
}

struct InputTransport {
    frames: Mutex<VecDeque<Vec<u8>>>,
}

impl InputTransport {
    fn new(frames: VecDeque<Vec<u8>>) -> Self {
        Self {
            frames: Mutex::new(frames),
        }
    }
}

#[async_trait::async_trait]
impl PtpExecutorTransport for InputTransport {
    async fn reserve_transaction_id(&self) -> Result<u32, PtpTransportError> {
        Ok(TRANSACTION_ID)
    }

    async fn send_command_frame(&self, _frame: Vec<u8>) -> Result<(), PtpTransportError> {
        Ok(())
    }

    async fn next_command_frame(&self) -> Result<Vec<u8>, PtpTransportError> {
        self.frames
            .lock()
            .expect("frame queue")
            .pop_front()
            .ok_or_else(|| PtpTransportError::Failed {
                detail: "synthetic input exhausted".into(),
            })
    }

    async fn next_event_frame(&self, _event_code: u16) -> Result<Vec<u8>, PtpTransportError> {
        Err(PtpTransportError::Failed {
            detail: "synthetic input has no events".into(),
        })
    }

    async fn open_channel(&self, _role: SocketRole) -> Result<(), PtpTransportError> {
        Ok(())
    }

    async fn close_command_channel(
        &self,
        _transport_close_frame: Option<Vec<u8>>,
    ) -> Result<(), PtpTransportError> {
        Ok(())
    }

    async fn reopen_command_session(&self) -> Result<PtpSessionOpenResult, PtpTransportError> {
        Ok(PtpSessionOpenResult {
            transaction_id: TRANSACTION_ID,
            response_code: 0x2001,
            response_params: Vec::new(),
        })
    }

    async fn sleep(&self, _ms: u32) -> Result<(), PtpTransportError> {
        pending().await
    }
}

fn stores() -> &'static [(PtpFraming, Arc<ConfigStore>)] {
    static STORES: OnceLock<Vec<(PtpFraming, Arc<ConfigStore>)>> = OnceLock::new();
    STORES.get_or_init(|| {
        [
            (PtpFraming::Standard, "standard"),
            (PtpFraming::Compressed, "compressed"),
            (PtpFraming::Usb, "usb"),
        ]
        .into_iter()
        .map(|(framing, name)| {
            let yaml = format!(
                r#"schema: camera-config/v1
camera: {{ manufacturer: Test, model: Fuzz, firmware: "1" }}
properties:
  "0xd209": {{ name: autofocusResult, type: u16, access: readWrite }}
connections:
  app:
    commandFraming: {name}
    actions:
      autofocusLock:
        mode: ""
        initiator:
          steps:
            - getProp: "0xd209"
"#
            );
            let store = ConfigStore::from_bundle(yaml, None).expect("synthetic store");
            (framing, store)
        })
        .collect()
    })
}

fn request(store: &ConfigStore) -> ActionInvocationRequest {
    ActionInvocationRequest {
        catalog_revision: store.action_catalog().revision,
        action_id: "autofocusLock".into(),
        connection: "app".into(),
        mode: String::new(),
        role: ActionRole::Initiator,
        parameters: Vec::new(),
    }
}

fn split_frames(data: &[u8]) -> VecDeque<Vec<u8>> {
    let mut remaining = data;
    let mut frames = VecDeque::new();
    while !remaining.is_empty() && frames.len() < 64 {
        if remaining.len() < 4 {
            frames.push_back(remaining.to_vec());
            break;
        }
        let length = u32::from_le_bytes(remaining[..4].try_into().unwrap()) as usize;
        if length < 4 || length > remaining.len() {
            frames.push_back(remaining.to_vec());
            break;
        }
        let (frame, tail) = remaining.split_at(length);
        frames.push_back(frame.to_vec());
        remaining = tail;
    }
    frames
}

fn encode(framing: PtpFraming, packet: &PtpIpPacket) -> Vec<u8> {
    match framing {
        PtpFraming::Standard => ptp_core::encode(packet).unwrap(),
        PtpFraming::Compressed => protocol_primitives::fuji_framing::encode(packet).unwrap(),
        PtpFraming::Usb => protocol_primitives::usb_ptp::encode(packet).unwrap(),
    }
}

fn canonical_frames(framing: PtpFraming, data: &[u8]) -> VecDeque<Vec<u8>> {
    let payload = data[..data.len().min(MAX_SYNTHETIC_PAYLOAD)].to_vec();
    let mut frames = VecDeque::new();
    match framing {
        PtpFraming::Standard => {
            frames.push_back(encode(
                framing,
                &PtpIpPacket::StartData(StartData {
                    transaction_id: TRANSACTION_ID,
                    total_length: payload.len() as u64,
                }),
            ));
            frames.push_back(encode(
                framing,
                &PtpIpPacket::EndData(DataBlock {
                    transaction_id: TRANSACTION_ID,
                    payload,
                }),
            ));
        }
        PtpFraming::Compressed | PtpFraming::Usb => {
            frames.push_back(
                camera_protocol_ffi::build_data(framing, OPERATION, TRANSACTION_ID, payload)
                    .unwrap(),
            );
        }
    }
    frames.push_back(encode(
        framing,
        &PtpIpPacket::OperationResponse(OperationResponse {
            code: 0x2001,
            transaction_id: TRANSACTION_ID,
            params: data
                .chunks_exact(4)
                .take(5)
                .map(|bytes| u32::from_le_bytes(bytes.try_into().unwrap()))
                .collect(),
        }),
    ));
    frames
}

async fn run_case(store: Arc<ConfigStore>, frames: VecDeque<Vec<u8>>) {
    let _ = run_initiator_action(
        Arc::clone(&store),
        request(&store),
        Arc::new(InputTransport::new(frames)),
        Arc::new(NullSteps),
        Arc::new(NullActivities),
    )
    .await;
}

fuzz_target!(|data: &[u8]| {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .unwrap();
    runtime.block_on(async {
        for (framing, store) in stores() {
            run_case(Arc::clone(store), split_frames(data)).await;
            run_case(Arc::clone(store), canonical_frames(*framing, data)).await;
        }
    });
});
