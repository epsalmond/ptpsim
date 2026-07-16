//! Fuji reference app `InitCommandRequest` (id `fuji-app-init`) — the 82-byte PTP/IP init
//! the GFX expects before the compressed channel opens.
//!
//! Layout (from `client application FujiPTPIP.swift`, pinned by the app's init golden):
//! ```text
//! u32 length (= total, 82)   u32 type (1 = Init_Command_Request)
//! payload: GUID[16]  u32(0)  nameField[26]  tail[28]
//! ```
//! `nameField` = the friendly name as UTF-16LE + a NUL u16, then truncated or
//! zero-padded to exactly 26 bytes. Identity (GUID/name) + `tail` come from the
//! manifest (value-policy + `connections.*.init`) — this code only frames them.

use crate::error::FramingError;
use ptp_core::Writer;

const INIT_COMMAND_REQUEST: u32 = 1;
const INIT_COMMAND_ACK: u32 = 2;
const NAME_FIELD_BYTES: usize = 26;
const APP_INIT_BYTES: usize = 82;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppInit {
    pub initiator_guid: [u8; 16],
    pub friendly_name: String,
    pub tail: Vec<u8>,
}

/// Build the InitCommandRequest packet. `guid` must be 16 bytes; `tail` is the
/// manifest-supplied trailer (28 bytes for the GFX, but length is not enforced —
/// it's data).
pub fn build_app_init(
    guid: &[u8],
    friendly_name: &str,
    tail: &[u8],
) -> Result<Vec<u8>, FramingError> {
    if guid.len() != 16 {
        return Err(FramingError::GuidLength(guid.len()));
    }

    let mut payload = Writer::new();
    payload.bytes(guid);
    payload.u32(0);
    payload.bytes(&fixed_name_field(friendly_name));
    payload.bytes(tail);
    let payload = payload.into_vec();

    let mut pkt = Writer::new();
    pkt.u32((payload.len() + 8) as u32); // total length incl. the 8-byte header
    pkt.u32(INIT_COMMAND_REQUEST);
    pkt.bytes(&payload);
    Ok(pkt.into_vec())
}

/// Parse the fixed-field reference app request without treating it as the variable-length
/// standard PTP/IP InitCommandRequest.
pub fn parse_app_init(packet: &[u8]) -> Result<AppInit, FramingError> {
    if packet.len() != APP_INIT_BYTES {
        return Err(FramingError::InitRequest(format!(
            "length {} != {APP_INIT_BYTES}",
            packet.len()
        )));
    }
    let declared = u32::from_le_bytes(packet[0..4].try_into().unwrap()) as usize;
    let typ = u32::from_le_bytes(packet[4..8].try_into().unwrap());
    if declared != packet.len() {
        return Err(FramingError::InitRequest(format!(
            "declared length {declared} != actual {}",
            packet.len()
        )));
    }
    if typ != INIT_COMMAND_REQUEST {
        return Err(FramingError::InitRequest(format!(
            "type {typ} is not Init_Command_Request ({INIT_COMMAND_REQUEST})"
        )));
    }
    if packet[24..28] != [0; 4] {
        return Err(FramingError::InitRequest(
            "reserved identity word is non-zero".into(),
        ));
    }
    let mut initiator_guid = [0u8; 16];
    initiator_guid.copy_from_slice(&packet[8..24]);
    let mut units = Vec::new();
    for pair in packet[28..54].chunks_exact(2) {
        let unit = u16::from_le_bytes([pair[0], pair[1]]);
        if unit == 0 {
            break;
        }
        units.push(unit);
    }
    let friendly_name = String::from_utf16(&units)
        .map_err(|_| FramingError::InitRequest("friendly name is not UTF-16LE".into()))?;
    Ok(AppInit {
        initiator_guid,
        friendly_name,
        tail: packet[54..].to_vec(),
    })
}

/// The 26-byte name field: UTF-16LE + NUL, truncated or zero-padded to fit.
fn fixed_name_field(name: &str) -> Vec<u8> {
    let mut w = Writer::new();
    for unit in name.encode_utf16() {
        w.u16(unit);
    }
    w.u16(0); // NUL terminator
    let mut v = w.into_vec();
    v.resize(NAME_FIELD_BYTES, 0); // truncate if longer, zero-pad if shorter
    v
}

/// Validate an InitCommandAck: declared length must match, and the packet type
/// must be `Init_Command_Ack` (2). Mirrors the app's `validateInitCommandAck`.
pub fn validate_init_ack(packet: &[u8]) -> Result<(), FramingError> {
    if packet.len() < 8 {
        return Err(FramingError::InitAck(format!(
            "too short: {} bytes",
            packet.len()
        )));
    }
    let declared = u32::from_le_bytes(packet[0..4].try_into().unwrap()) as usize;
    let typ = u32::from_le_bytes(packet[4..8].try_into().unwrap());
    if declared != packet.len() {
        return Err(FramingError::InitAck(format!(
            "declared length {declared} != actual {}",
            packet.len()
        )));
    }
    if typ != INIT_COMMAND_ACK {
        return Err(FramingError::InitAck(format!(
            "type {typ} is not Init_Command_Ack ({INIT_COMMAND_ACK})"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // The known-accepted initiator identity (FujiPTPIP.swift) + liveViewInitTail.
    const GUID: [u8; 16] = [
        0xf2, 0xe4, 0x53, 0x8f, 0xad, 0xa5, 0x48, 0x5d, 0x87, 0xb2, 0x7f, 0x0b, 0xd3, 0xd5, 0xde,
        0xd0,
    ];
    const TAIL: [u8; 28] = [
        0xcc, 0x00, 0x4f, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x57,
        0x00, 0x4d, 0x00, 0x42, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    ];

    #[test]
    fn builds_the_82_byte_init_with_correct_structure() {
        let pkt = build_app_init(&GUID, "Pixel-6-4976", &TAIL).unwrap();
        assert_eq!(pkt.len(), 82, "the canonical reference app init is 82 bytes");
        // Header: length == total, type == 1.
        assert_eq!(u32::from_le_bytes(pkt[0..4].try_into().unwrap()), 82);
        assert_eq!(
            u32::from_le_bytes(pkt[4..8].try_into().unwrap()),
            INIT_COMMAND_REQUEST
        );
        // GUID, then u32(0).
        assert_eq!(&pkt[8..24], &GUID);
        assert_eq!(u32::from_le_bytes(pkt[24..28].try_into().unwrap()), 0);
        // Name field (26): "Pixel-6-4976" is 12 chars → 24 bytes + NUL = 26 exactly.
        assert_eq!(
            &pkt[28..52],
            &"Pixel-6-4976"
                .encode_utf16()
                .flat_map(u16::to_le_bytes)
                .collect::<Vec<_>>()[..]
        );
        assert_eq!(&pkt[52..54], &[0, 0]); // NUL terminator
                                           // Tail (28).
        assert_eq!(&pkt[54..82], &TAIL);
        assert_eq!(
            parse_app_init(&pkt).unwrap(),
            AppInit {
                initiator_guid: GUID,
                friendly_name: "Pixel-6-4976".into(),
                tail: TAIL.to_vec(),
            }
        );
    }

    #[test]
    fn parser_requires_the_fixed_82_byte_shape() {
        let mut packet = build_app_init(&GUID, "probe", &TAIL).unwrap();
        packet.pop();
        assert!(matches!(
            parse_app_init(&packet),
            Err(FramingError::InitRequest(message)) if message.contains("length 81 != 82")
        ));
    }

    #[test]
    fn name_field_pads_short_and_truncates_long() {
        // Short name → zero-padded to 26 within the field.
        let short = build_app_init(&GUID, "Hi", &TAIL).unwrap();
        let field = &short[28..54];
        assert_eq!(
            &field[0..4],
            &"Hi"
                .encode_utf16()
                .flat_map(u16::to_le_bytes)
                .collect::<Vec<_>>()[..]
        );
        assert!(field[6..].iter().all(|&b| b == 0));
        // Over-long name → field stays exactly 26 bytes, total stays 82.
        let long = build_app_init(&GUID, "ThisNameIsWayTooLongForTheField", &TAIL).unwrap();
        assert_eq!(long.len(), 82);
    }

    #[test]
    fn rejects_wrong_guid_length() {
        assert!(matches!(
            build_app_init(&[0; 8], "x", &TAIL),
            Err(FramingError::GuidLength(8))
        ));
    }

    #[test]
    fn validates_init_ack() {
        // type 2, declared length matches.
        let ack = [8u8, 0, 0, 0, 2, 0, 0, 0];
        assert!(validate_init_ack(&ack).is_ok());
        // wrong type.
        let bad_type = [8u8, 0, 0, 0, 1, 0, 0, 0];
        assert!(validate_init_ack(&bad_type).is_err());
        // length mismatch.
        let bad_len = [99u8, 0, 0, 0, 2, 0, 0, 0];
        assert!(validate_init_ack(&bad_len).is_err());
    }
}
