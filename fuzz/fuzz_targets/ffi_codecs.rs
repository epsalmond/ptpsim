#![no_main]

use libfuzzer_sys::fuzz_target;

use camera_protocol_ffi as ffi;

fuzz_target!(|data: &[u8]| {
    for framing in [
        ffi::PtpFraming::Standard,
        ffi::PtpFraming::Compressed,
        ffi::PtpFraming::Usb,
    ] {
        let _ = ffi::parse_response(framing, data.to_vec());
        let _ = ffi::parse_data_payload(framing, data.to_vec());
        let _ = ffi::parse_data_phase(framing, data.to_vec());
        let _ = ffi::parse_event(framing, data.to_vec());
    }
    let _ = ffi::decode_init_command_response(data.to_vec());
    let _ = ffi::validate_init_ack(data.to_vec());
    let _ = ffi::parse_device_info(data.to_vec());
    let _ = ffi::parse_object_info(data.to_vec());
    let _ = ffi::parse_device_prop_desc(data.to_vec());
    let _ = ffi::parse_object_handle_list(data.to_vec());
});
