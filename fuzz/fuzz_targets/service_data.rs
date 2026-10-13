#![no_main]

use libfuzzer_sys::fuzz_target;

use camera_config::WireFraming;

fuzz_target!(|data: &[u8]| {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .unwrap();
    runtime.block_on(async {
        for framing in [
            WireFraming::Standard,
            WireFraming::Compressed,
            WireFraming::Usb,
        ] {
            let mut input = data;
            if let Ok(Some(payload)) =
                camera_sim_service::collect_data_in(&mut input, 1, framing).await
            {
                assert!(payload.len() <= camera_sim_service::inbound::DATA_BYTES);
            }
        }
    });
});
