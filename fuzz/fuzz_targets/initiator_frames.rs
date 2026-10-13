#![no_main]

use libfuzzer_sys::fuzz_target;

const MAX_FRAME_BYTES: usize = 1024 * 1024;
const MAX_SYNTHETIC_PAYLOAD: usize = 4096;

fuzz_target!(|data: &[u8]| {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    runtime.block_on(async {
        let _ = camera_initiator::transport::fuzz_frame_reader(data, MAX_FRAME_BYTES).await;

        let payload = &data[..data.len().min(MAX_SYNTHETIC_PAYLOAD)];
        let length = 8 + payload.len();
        let mut frame = Vec::with_capacity(length * 2);
        frame.extend_from_slice(&(length as u32).to_le_bytes());
        frame.extend_from_slice(&1u32.to_le_bytes());
        frame.extend_from_slice(payload);
        frame.extend_from_within(..length);
        let frames = camera_initiator::transport::fuzz_frame_reader(&frame, MAX_FRAME_BYTES).await;
        assert_eq!(frames, [frame[..length].to_vec(), frame[length..].to_vec()]);
    });
});
