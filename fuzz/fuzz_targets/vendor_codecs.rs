#![no_main]

use libfuzzer_sys::fuzz_target;

use protocol_primitives::{fuji_framing, fuji_init, legacy_app_init, liveview, quirk, usb_ptp};

fuzz_target!(|data: &[u8]| {
    if let Ok(packet) = fuji_framing::decode(data) {
        let encoded = match &packet {
            ptp_core::PtpIpPacket::Data(block) => {
                fuji_framing::encode_data(0, block.transaction_id, &block.payload)
            }
            _ => fuji_framing::encode(&packet).unwrap(),
        };
        assert_eq!(fuji_framing::decode(&encoded).unwrap(), packet);
    }
    if let Ok(packet) = usb_ptp::decode(data) {
        let encoded = match &packet {
            ptp_core::PtpIpPacket::Data(block) => {
                usb_ptp::encode_data(0, block.transaction_id, &block.payload)
            }
            _ => usb_ptp::encode(&packet).unwrap(),
        };
        assert_eq!(usb_ptp::decode(&encoded).unwrap(), packet);
    }
    let _ = fuji_init::parse_app_init(data);
    let _ = fuji_init::validate_init_ack(data);
    let _ = legacy_app_init::parse_legacy_app_init(data);
    let _ = liveview::parse_frame(data);
    for width in [1, 2, 4] {
        let layout = quirk::RecordStreamLayout::new(width, 2, width).unwrap();
        let _ = quirk::parse_record_stream(data, &layout);
        let descriptor = quirk::RecordStreamDescriptor::new(
            layout,
            [
                (1, quirk::RecordValueEncoding::Fixed { width }),
                (2, quirk::RecordValueEncoding::Signed { width }),
                (3, quirk::RecordValueEncoding::PtpString),
            ],
        )
        .unwrap();
        let _ = quirk::parse_typed_record_stream(data, &descriptor);
    }
});
