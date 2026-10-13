#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let _ = camera_sim_tui::http::parse_request_if_complete(data);
    // Cover header/body fragmentation without quadratic prefix scans.
    for end in [0, data.len() / 2, data.len().saturating_sub(1)] {
        let _ = camera_sim_tui::http::parse_request_if_complete(&data[..end]);
    }
});
