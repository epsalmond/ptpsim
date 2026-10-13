#![no_main]

use libfuzzer_sys::fuzz_target;

use ptp_core::{DeviceInfo, DevicePropDesc, ObjectInfo, PropValue, Reader, StorageInfo};

fuzz_target!(|data: &[u8]| {
    let _ = DeviceInfo::decode(data);
    let _ = StorageInfo::decode(data);
    let _ = ObjectInfo::decode(data);
    let _ = DevicePropDesc::decode(data);
    let _ = Reader::new(data).ptp_string();
    let _ = Reader::new(data).ptp_array(|reader| reader.u32());
    for datatype in [
        0x0001, 0x0002, 0x0003, 0x0004, 0x0005, 0x0006, 0x0007, 0x0008, 0xffff,
    ] {
        let _ = PropValue::decode(&mut Reader::new(data), datatype);
    }
});
