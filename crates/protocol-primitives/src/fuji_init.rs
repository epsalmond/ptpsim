//! Fuji reference app `InitCommandRequest` (id `fuji-app-init`) — the 82-byte PTP/IP init
//! the GFX expects before the compressed channel opens.
//!
//! Fixed layout for the scoped reference app-compatible manifest shape:
//! ```text
//! u32 length (= total, 82)   u32 type (1 = Init_Command_Request)
//! payload: GUID[16]  u32(0)  friendlyNameField[54]
//! ```
//! The friendly-name field contains UTF-16LE text followed by one NUL unit and
//! deterministic zero-fill. Identity comes from manifest value policy; this
//! code owns only the fixed wire framing.

use crate::error::FramingError;
use ptp_core::Writer;

const INIT_COMMAND_REQUEST: u32 = 1;
const INIT_COMMAND_ACK: u32 = 2;
const NAME_FIELD_BYTES: usize = 54;
const MAX_NAME_UNITS: usize = NAME_FIELD_BYTES / 2 - 1;
const APP_INIT_BYTES: usize = 82;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppInit {
    pub initiator_guid: [u8; 16],
    pub friendly_name: String,
}

/// Build the fixed InitCommandRequest packet. `guid` must be 16 bytes and the
/// friendly name must leave room for its terminating NUL unit.
pub fn build_app_init(guid: &[u8], friendly_name: &str) -> Result<Vec<u8>, FramingError> {
    if guid.len() != 16 {
        return Err(FramingError::GuidLength(guid.len()));
    }

    let mut payload = Writer::new();
    payload.bytes(guid);
    payload.u32(0);
    payload.bytes(&fixed_name_field(friendly_name)?);
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
    let units = packet[28..]
        .chunks_exact(2)
        .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
        .collect::<Vec<_>>();
    let text_end = units
        .iter()
        .position(|unit| *unit == 0)
        .unwrap_or(units.len());
    let friendly_name = String::from_utf16(&units[..text_end])
        .map_err(|_| FramingError::InitRequest("friendly name is not UTF-16LE".into()))?;
    Ok(AppInit {
        initiator_guid,
        friendly_name,
    })
}

/// The 54-byte name field: UTF-16LE + NUL, then zero-fill.
fn fixed_name_field(name: &str) -> Result<[u8; NAME_FIELD_BYTES], FramingError> {
    let units = name.encode_utf16().collect::<Vec<_>>();
    if name.contains('\0') || units.len() > MAX_NAME_UNITS {
        return Err(FramingError::InitRequest(format!(
            "reference app friendly name exceeds {MAX_NAME_UNITS} UTF-16 units or contains NUL"
        )));
    }
    let mut field = [0u8; NAME_FIELD_BYTES];
    for (index, unit) in units.into_iter().enumerate() {
        let offset = index * 2;
        field[offset..offset + 2].copy_from_slice(&unit.to_le_bytes());
    }
    Ok(field)
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

    const GUID: [u8; 16] = [
        0xf2, 0xe4, 0x53, 0x8f, 0xad, 0xa5, 0x48, 0x5d, 0x87, 0xb2, 0x7f, 0x0b, 0xd3, 0xd5, 0xde,
        0xd0,
    ];
    #[test]
    fn builds_the_82_byte_init_with_correct_structure() {
        let pkt = build_app_init(&GUID, "Pixel-6-4976").unwrap();
        assert_eq!(pkt.len(), 82, "the fixed reference app init shape is 82 bytes");
        // Header: length == total, type == 1.
        assert_eq!(u32::from_le_bytes(pkt[0..4].try_into().unwrap()), 82);
        assert_eq!(
            u32::from_le_bytes(pkt[4..8].try_into().unwrap()),
            INIT_COMMAND_REQUEST
        );
        // GUID, then u32(0).
        assert_eq!(&pkt[8..24], &GUID);
        assert_eq!(u32::from_le_bytes(pkt[24..28].try_into().unwrap()), 0);
        // The canonical short-name request remains byte-identical: name, NUL,
        // then zeros through the end of the fixed packet.
        assert_eq!(
            &pkt[28..52],
            &"Pixel-6-4976"
                .encode_utf16()
                .flat_map(u16::to_le_bytes)
                .collect::<Vec<_>>()[..]
        );
        assert_eq!(&pkt[52..54], &[0, 0]); // NUL terminator
        assert!(pkt[54..82].iter().all(|byte| *byte == 0));
        assert_eq!(
            parse_app_init(&pkt).unwrap(),
            AppInit {
                initiator_guid: GUID,
                friendly_name: "Pixel-6-4976".into(),
            }
        );
    }

    #[test]
    fn parser_requires_the_fixed_82_byte_shape() {
        let mut packet = build_app_init(&GUID, "probe").unwrap();
        packet.pop();
        assert!(matches!(
            parse_app_init(&packet),
            Err(FramingError::InitRequest(message)) if message.contains("length 81 != 82")
        ));
    }

    #[test]
    fn name_field_pads_short_and_reaches_the_full_field() {
        let short = build_app_init(&GUID, "Hi").unwrap();
        let field = &short[28..82];
        assert_eq!(
            &field[0..4],
            &"Hi"
                .encode_utf16()
                .flat_map(u16::to_le_bytes)
                .collect::<Vec<_>>()[..]
        );
        assert!(field[6..].iter().all(|&b| b == 0));

        let name = "abcdefghijklmnopqr";
        let long = build_app_init(&GUID, name).unwrap();
        assert_eq!(long.len(), 82);
        assert_eq!(
            &long[28..64],
            &name
                .encode_utf16()
                .flat_map(u16::to_le_bytes)
                .collect::<Vec<_>>()
        );
        assert_eq!(parse_app_init(&long).unwrap().friendly_name, name);
    }

    #[test]
    fn builder_requires_room_for_a_terminator() {
        let max = "a".repeat(MAX_NAME_UNITS);
        let packet = build_app_init(&GUID, &max).unwrap();
        assert_eq!(&packet[80..82], &[0, 0]);

        let too_long = "a".repeat(MAX_NAME_UNITS + 1);
        assert!(matches!(
            build_app_init(&GUID, &too_long),
            Err(FramingError::InitRequest(message)) if message.contains("exceeds 26 UTF-16 units")
        ));
        assert!(matches!(
            build_app_init(&GUID, "before\0after"),
            Err(FramingError::InitRequest(message)) if message.contains("contains NUL")
        ));
    }

    #[test]
    fn parser_treats_post_nul_and_unterminated_units_as_name_field_data() {
        let mut post_nul = build_app_init(&GUID, "short").unwrap();
        post_nul[60..64].copy_from_slice(&[0xa5, 0xa5, 0x5a, 0x5a]);
        assert_eq!(parse_app_init(&post_nul).unwrap().friendly_name, "short");

        let mut unterminated = build_app_init(&GUID, "a").unwrap();
        for pair in unterminated[28..82].chunks_exact_mut(2) {
            pair.copy_from_slice(&u16::from(b'z').to_le_bytes());
        }
        assert_eq!(
            parse_app_init(&unterminated).unwrap().friendly_name,
            "z".repeat(27)
        );
    }

    #[test]
    fn rejects_wrong_guid_length() {
        assert!(matches!(
            build_app_init(&[0; 8], "x"),
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
