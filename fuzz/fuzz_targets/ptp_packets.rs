#![no_main]

use libfuzzer_sys::fuzz_target;

use ptp_core::{PtpCodec, PtpIpPacket};

fuzz_target!(|data: &[u8]| {
    if let Ok(packet) = PtpIpPacket::decode(data) {
        if let Ok(encoded) = ptp_core::encode(&packet) {
            assert_eq!(PtpIpPacket::decode(&encoded).unwrap(), packet);
        }
    }
});
