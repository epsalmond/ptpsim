//! Standard PTP (ISO 15740) operation + property code names. These are **spec
//! facts** — the same on every PTP camera — so the generator can name standard
//! codes (`0x5007` → `FNumber`) with zero per-camera curation. Vendor codes
//! (`0xd…`, `0x9…`) are NOT here; they're named from RE/gphoto2 evidence or stay
//! `raw_0x…`. Kept local to camera-config so the engine stays standalone (it does
//! not depend on ptpsim's `ptp-core`).

/// Canonical name for a standard PTP operation code, if known.
pub fn standard_operation_name(code: u16) -> Option<&'static str> {
    Some(match code {
        0x1001 => "GetDeviceInfo",
        0x1002 => "OpenSession",
        0x1003 => "CloseSession",
        0x1004 => "GetStorageIDs",
        0x1005 => "GetStorageInfo",
        0x1006 => "GetNumObjects",
        0x1007 => "GetObjectHandles",
        0x1008 => "GetObjectInfo",
        0x1009 => "GetObject",
        0x100a => "GetThumb",
        0x100b => "DeleteObject",
        0x100c => "SendObjectInfo",
        0x100d => "SendObject",
        0x100e => "InitiateCapture",
        0x100f => "FormatStore",
        0x1010 => "ResetDevice",
        0x1011 => "SelfTest",
        0x1012 => "SetObjectProtection",
        0x1013 => "PowerDown",
        0x1014 => "GetDevicePropDesc",
        0x1015 => "GetDevicePropValue",
        0x1016 => "SetDevicePropValue",
        0x1017 => "ResetDevicePropValue",
        0x1018 => "TerminateOpenCapture",
        0x1019 => "MoveObject",
        0x101a => "CopyObject",
        0x101b => "GetPartialObject",
        0x101c => "InitiateOpenCapture",
        _ => return None,
    })
}

/// Canonical name for a standard PTP device-property code, if known (PTP 1.1
/// `0x50xx` range).
pub fn standard_property_name(code: u16) -> Option<&'static str> {
    Some(match code {
        0x5001 => "BatteryLevel",
        0x5002 => "FunctionalMode",
        0x5003 => "ImageSize",
        0x5004 => "CompressionSetting",
        0x5005 => "WhiteBalance",
        0x5006 => "RGBGain",
        0x5007 => "FNumber",
        0x5008 => "FocalLength",
        0x5009 => "FocusDistance",
        0x500a => "FocusMode",
        0x500b => "ExposureMeteringMode",
        0x500c => "FlashMode",
        0x500d => "ExposureTime",
        0x500e => "ExposureProgramMode",
        0x500f => "ExposureIndex",
        0x5010 => "ExposureBiasCompensation",
        0x5011 => "DateTime",
        0x5012 => "CaptureDelay",
        0x5013 => "StillCaptureMode",
        0x5014 => "Contrast",
        0x5015 => "Sharpness",
        0x5016 => "DigitalZoom",
        0x5017 => "EffectMode",
        0x5018 => "BurstNumber",
        0x5019 => "BurstInterval",
        0x501a => "TimelapseNumber",
        0x501b => "TimelapseInterval",
        0x501c => "FocusMeteringMode",
        0x501d => "UploadURL",
        0x501e => "Artist",
        0x501f => "CopyrightInfo",
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_standard_codes_only() {
        assert_eq!(standard_property_name(0x5007), Some("FNumber"));
        assert_eq!(standard_property_name(0x500d), Some("ExposureTime"));
        assert_eq!(standard_operation_name(0x1014), Some("GetDevicePropDesc"));
        // Vendor codes are not named here.
        assert_eq!(standard_property_name(0xd02a), None);
        assert_eq!(standard_operation_name(0x9054), None);
    }
}
