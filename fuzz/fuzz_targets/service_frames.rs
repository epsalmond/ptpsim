#![no_main]

use libfuzzer_sys::fuzz_target;

use camera_sim_service::inbound::{FrameReadPolicy, FrameReader, HANDSHAKE_BYTES, OPERATION_BYTES};

fuzz_target!(|data: &[u8]| {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .unwrap();
    runtime.block_on(async {
        for maximum in [HANDSHAKE_BYTES, OPERATION_BYTES] {
            let mut input = data;
            let mut reader = FrameReader::default();
            // Read coalesced frames as well as malformed or truncated input.
            for _ in 0..32 {
                match reader
                    .read_frame(&mut input, FrameReadPolicy::established(maximum))
                    .await
                {
                    Ok(Some(frame)) => {
                        assert!(frame.len() <= maximum);
                        assert_eq!(
                            frame.len(),
                            u32::from_le_bytes(frame[..4].try_into().unwrap()) as usize
                        );
                    }
                    _ => break,
                }
            }
        }
    });
});
