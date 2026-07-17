//! Fixed PTP/IP initialization used by FUJIFILM legacy manufacturer app.
//!
//! The legacy legacy manufacturer app client predates reference app's vendor-tail request. Its
//! 82-byte request carries the route-selected local IPv4 followed by a 54-byte
//! UTF-16LE client-name field. The matching camera response is the fixed
//! 68-byte ack shape also used by PCSS, with a legacy manufacturer app-specific responder
//! GUID. These are wire shapes, not camera-model policy.

use std::net::Ipv4Addr;

use crate::FramingError;

const INIT_LEN: usize = 82;
const INIT_TYPE: u32 = 1;
const IP_OFFSET: usize = 24;
const NAME_OFFSET: usize = 28;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LegacyAppInit {
    pub initiator_guid: [u8; 16],
    pub client_ip: Ipv4Addr,
    pub friendly_name: String,
}

pub fn build_legacy_app_init(
    guid: &[u8],
    client_ip: Ipv4Addr,
    friendly_name: &str,
) -> Result<Vec<u8>, FramingError> {
    if guid.len() != 16 {
        return Err(FramingError::GuidLength(guid.len()));
    }
    let units = friendly_name.encode_utf16().collect::<Vec<_>>();
    if friendly_name.contains('\0') || units.len() > 26 {
        return Err(FramingError::InitRequest(
            "legacy manufacturer app friendly name exceeds 26 UTF-16 units or contains NUL".into(),
        ));
    }

    let mut packet = vec![0u8; INIT_LEN];
    packet[0..4].copy_from_slice(&(INIT_LEN as u32).to_le_bytes());
    packet[4..8].copy_from_slice(&INIT_TYPE.to_le_bytes());
    packet[8..24].copy_from_slice(guid);
    packet[IP_OFFSET..NAME_OFFSET].copy_from_slice(&u32::from(client_ip).to_le_bytes());
    for (index, unit) in units.into_iter().enumerate() {
        let offset = NAME_OFFSET + index * 2;
        packet[offset..offset + 2].copy_from_slice(&unit.to_le_bytes());
    }
    Ok(packet)
}

pub fn parse_legacy_app_init(packet: &[u8]) -> Result<LegacyAppInit, FramingError> {
    if packet.len() != INIT_LEN {
        return Err(FramingError::InitRequest(format!(
            "legacy manufacturer app length {} != {INIT_LEN}",
            packet.len()
        )));
    }
    let declared = u32::from_le_bytes(packet[0..4].try_into().unwrap()) as usize;
    let packet_type = u32::from_le_bytes(packet[4..8].try_into().unwrap());
    if declared != INIT_LEN {
        return Err(FramingError::InitRequest(format!(
            "declared length {declared} != {INIT_LEN}"
        )));
    }
    if packet_type != INIT_TYPE {
        return Err(FramingError::InitRequest(format!(
            "type {packet_type} is not Init_Command_Request ({INIT_TYPE})"
        )));
    }

    let mut initiator_guid = [0u8; 16];
    initiator_guid.copy_from_slice(&packet[8..24]);
    let client_ip = Ipv4Addr::from(u32::from_le_bytes(
        packet[IP_OFFSET..NAME_OFFSET].try_into().unwrap(),
    ));
    let name_bytes = &packet[NAME_OFFSET..];
    let terminator = name_bytes
        .chunks_exact(2)
        .position(|pair| pair == [0, 0])
        .ok_or_else(|| FramingError::InitRequest("friendly name is not NUL-terminated".into()))?;
    if name_bytes[(terminator + 1) * 2..]
        .iter()
        .any(|byte| *byte != 0)
    {
        return Err(FramingError::InitRequest(
            "friendly-name tail contains nonzero bytes".into(),
        ));
    }
    let units = name_bytes[..terminator * 2]
        .chunks_exact(2)
        .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
        .collect::<Vec<_>>();
    let friendly_name = String::from_utf16(&units)
        .map_err(|_| FramingError::InitRequest("friendly name is not UTF-16LE".into()))?;
    Ok(LegacyAppInit {
        initiator_guid,
        client_ip,
        friendly_name,
    })
}

pub fn validate_legacy_app_init_ack(
    packet: &[u8],
    expected_responder_guid: &[u8],
) -> Result<(), FramingError> {
    if expected_responder_guid.len() != 16 {
        return Err(FramingError::GuidLength(expected_responder_guid.len()));
    }
    if packet.len() != 68 {
        return Err(FramingError::InitAck(format!(
            "legacy manufacturer app ack length {} != 68",
            packet.len()
        )));
    }
    let declared = u32::from_le_bytes(packet[0..4].try_into().unwrap()) as usize;
    let packet_type = u32::from_le_bytes(packet[4..8].try_into().unwrap());
    if declared != 68 || packet_type != 2 {
        return Err(FramingError::InitAck(format!(
            "legacy manufacturer app ack header length={declared} type={packet_type}"
        )));
    }
    if &packet[12..28] != expected_responder_guid {
        return Err(FramingError::InitAck(format!(
            "unexpected responder GUID {:02x?}",
            &packet[12..28]
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pcss_init_ack_message;

    const INITIATOR: [u8; 16] = [
        0xf2, 0xe4, 0x53, 0x8f, 0xad, 0xa5, 0x48, 0x5d, 0x87, 0xb2, 0x7f, 0x0b, 0xd3, 0xd5, 0xde,
        0xd0,
    ];
    const RESPONDER: [u8; 16] = [
        0x08, 0x70, 0xb0, 0x61, 0x0a, 0x8b, 0x45, 0x93, 0xb2, 0xe7, 0x93, 0x57, 0xdd, 0x36, 0xe0,
        0x50,
    ];

    #[test]
    fn builds_and_parses_exact_legacy_app_request() {
        let packet =
            build_legacy_app_init(&INITIATOR, Ipv4Addr::new(192, 168, 0, 2), "Pixel 8").unwrap();
        assert_eq!(packet.len(), 82);
        assert_eq!(&packet[0..8], &[82, 0, 0, 0, 1, 0, 0, 0]);
        assert_eq!(&packet[8..24], &INITIATOR);
        assert_eq!(&packet[24..28], &[2, 0, 168, 192]);
        assert_eq!(
            parse_legacy_app_init(&packet).unwrap(),
            LegacyAppInit {
                initiator_guid: INITIATOR,
                client_ip: Ipv4Addr::new(192, 168, 0, 2),
                friendly_name: "Pixel 8".into(),
            }
        );
    }

    #[test]
    fn permits_26_utf16_units_and_rejects_more() {
        let max = "x".repeat(26);
        assert!(build_legacy_app_init(&INITIATOR, Ipv4Addr::LOCALHOST, &max).is_ok());
        assert!(
            build_legacy_app_init(&INITIATOR, Ipv4Addr::LOCALHOST, &format!("{max}x")).is_err()
        );
    }

    #[test]
    fn validates_fixed_ack_and_responder_guid() {
        let mut ack = pcss_init_ack_message(1, RESPONDER, "X-A7").unwrap();
        assert!(validate_legacy_app_init_ack(&ack, &RESPONDER).is_ok());
        assert!(validate_legacy_app_init_ack(&ack, &[0; 16]).is_err());
        // The native client proves no camera-name semantics beyond the fixed
        // header/GUID, so do not import PCSS's stricter zero-tail rule here.
        ack[40] = 0xff;
        assert!(validate_legacy_app_init_ack(&ack, &RESPONDER).is_ok());
    }
}
