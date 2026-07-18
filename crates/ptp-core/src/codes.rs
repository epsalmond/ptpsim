//! Standard PTP code registries (operation, response, object-format) and the
//! PTP/IP packet-type tags. Vendor (Fuji/Nikon/Canon) codes are *not* here —
//! those live in manifest data, per the syntax-only rule.

/// PTP/IP packet types for the standard (ISO 15740) TCP framing. The Fuji
/// "compressed" framing uses a different, narrower tag space and lives in
/// `protocol-primitives`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum PacketType {
    InitCommandRequest = 1,
    InitCommandAck = 2,
    InitEventRequest = 3,
    InitEventAck = 4,
    InitFail = 5,
    OperationRequest = 6,
    OperationResponse = 7,
    Event = 8,
    StartData = 9,
    Data = 10,
    Cancel = 11,
    EndData = 12,
    ProbeRequest = 13,
    ProbeResponse = 14,
}

impl PacketType {
    pub fn from_u32(v: u32) -> Option<Self> {
        use PacketType::*;
        Some(match v {
            1 => InitCommandRequest,
            2 => InitCommandAck,
            3 => InitEventRequest,
            4 => InitEventAck,
            5 => InitFail,
            6 => OperationRequest,
            7 => OperationResponse,
            8 => Event,
            9 => StartData,
            10 => Data,
            11 => Cancel,
            12 => EndData,
            13 => ProbeRequest,
            14 => ProbeResponse,
            _ => return None,
        })
    }
}

/// Standard PTP operation codes used by image import and property access.
/// Vendor operations (e.g. `0x90xx`) are manifest data, not constants here.
pub mod op {
    pub const GET_DEVICE_INFO: u16 = 0x1001;
    pub const OPEN_SESSION: u16 = 0x1002;
    pub const CLOSE_SESSION: u16 = 0x1003;
    pub const GET_STORAGE_IDS: u16 = 0x1004;
    pub const GET_STORAGE_INFO: u16 = 0x1005;
    pub const GET_NUM_OBJECTS: u16 = 0x1006;
    pub const GET_OBJECT_HANDLES: u16 = 0x1007;
    pub const GET_OBJECT_INFO: u16 = 0x1008;
    pub const GET_OBJECT: u16 = 0x1009;
    pub const GET_THUMB: u16 = 0x100a;
    pub const DELETE_OBJECT: u16 = 0x100b;
    pub const SEND_OBJECT_INFO: u16 = 0x100c;
    pub const SEND_OBJECT: u16 = 0x100d;
    pub const INITIATE_CAPTURE: u16 = 0x100e;
    pub const GET_DEVICE_PROP_DESC: u16 = 0x1014;
    pub const GET_DEVICE_PROP_VALUE: u16 = 0x1015;
    pub const SET_DEVICE_PROP_VALUE: u16 = 0x1016;
    pub const GET_PARTIAL_OBJECT: u16 = 0x101b;
    pub const INITIATE_OPEN_CAPTURE: u16 = 0x101c;
    pub const TERMINATE_OPEN_CAPTURE: u16 = 0x1018;
}

/// Standard PTP response codes.
pub mod resp {
    pub const OK: u16 = 0x2001;
    pub const GENERAL_ERROR: u16 = 0x2002;
    pub const SESSION_NOT_OPEN: u16 = 0x2003;
    pub const OPERATION_NOT_SUPPORTED: u16 = 0x2005;
    pub const PARAMETER_NOT_SUPPORTED: u16 = 0x2006;
    pub const INVALID_STORAGE_ID: u16 = 0x2008;
    pub const INVALID_OBJECT_HANDLE: u16 = 0x2009;
    pub const DEVICE_PROP_NOT_SUPPORTED: u16 = 0x200a;
    pub const INVALID_PARAMETER: u16 = 0x201d;
    pub const SESSION_ALREADY_OPEN: u16 = 0x201e;
    pub const ACCESS_DENIED: u16 = 0x200f;
    pub const DEVICE_BUSY: u16 = 0x2019;
}

/// A subset of standard object-format codes; manifests can add vendor formats.
pub mod format {
    pub const UNDEFINED: u16 = 0x3000;
    pub const ASSOCIATION: u16 = 0x3001; // folder
    pub const EXIF_JPEG: u16 = 0x3801;
    pub const TIFF: u16 = 0x380d;
}

/// Standard PTP datatype codes used in `DevicePropDesc`.
pub mod datatype_code {
    pub const UINT8: u16 = 0x0002;
    pub const UINT16: u16 = 0x0004;
    pub const UINT32: u16 = 0x0006;
    pub const UINT64: u16 = 0x0008;
    pub const STR: u16 = 0xffff;
}
