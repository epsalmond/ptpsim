#![no_main]

use libfuzzer_sys::fuzz_target;

use ptp_core::{PtpCodec, PtpIpPacket};

fuzz_target!(|data: &[u8]| {
    if let Ok(packet) = PtpIpPacket::decode(data) {
        let encoded = ptp_core::encode(&packet).unwrap();
        assert_eq!(PtpIpPacket::decode(&encoded).unwrap(), packet);
    }
});
