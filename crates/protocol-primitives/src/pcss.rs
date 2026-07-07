//! PCSS wireless-tether establishment helpers.
//!
//! These primitives cover shared wire syntax: the PCSS-flavored PTP/IP init
//! packet and the LAN discovery/callback text frames. Camera behavior still
//! lives in manifest data and the simulator service.

use std::fmt;
use std::net::Ipv4Addr;

const INIT_LEN: usize = 82;
const INIT_PACKET_TYPE: u32 = 1;
const IP_OFFSET: usize = 0x18;
const NAME_OFFSET: usize = 0x1c;
const ZERO_TAIL_OFFSET: usize = 0x36;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PcssInit {
    pub initiator_guid: [u8; 16],
    pub client_ip: Ipv4Addr,
    pub hostname: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PcssInitError {
    WrongLength(usize),
    WrongPacketType(u32),
    MissingHostnameTerminator,
    InvalidHostname,
    NonZeroTail,
}

impl fmt::Display for PcssInitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::WrongLength(n) => write!(f, "PCSS init length {n} != {INIT_LEN}"),
            Self::WrongPacketType(t) => {
                write!(f, "PCSS init packet type {t} != {INIT_PACKET_TYPE}")
            }
            Self::MissingHostnameTerminator => {
                write!(f, "PCSS init hostname is not NUL-terminated")
            }
            Self::InvalidHostname => write!(f, "PCSS init hostname is not valid UTF-16LE"),
            Self::NonZeroTail => write!(f, "PCSS init zero-tail contains nonzero bytes"),
        }
    }
}

impl std::error::Error for PcssInitError {}

pub fn parse_pcss_init(bytes: &[u8]) -> Result<PcssInit, PcssInitError> {
    if bytes.len() != INIT_LEN {
        return Err(PcssInitError::WrongLength(bytes.len()));
    }
    let declared = u32::from_le_bytes(bytes[0..4].try_into().unwrap()) as usize;
    if declared != INIT_LEN {
        return Err(PcssInitError::WrongLength(declared));
    }
    let packet_type = u32::from_le_bytes(bytes[4..8].try_into().unwrap());
    if packet_type != INIT_PACKET_TYPE {
        return Err(PcssInitError::WrongPacketType(packet_type));
    }
    if bytes[ZERO_TAIL_OFFSET..].iter().any(|b| *b != 0) {
        return Err(PcssInitError::NonZeroTail);
    }
    let mut initiator_guid = [0u8; 16];
    initiator_guid.copy_from_slice(&bytes[8..24]);
    let ip_raw = u32::from_le_bytes(bytes[IP_OFFSET..IP_OFFSET + 4].try_into().unwrap());
    let client_ip = Ipv4Addr::from(ip_raw);
    let hostname = decode_fixed_utf16_name(&bytes[NAME_OFFSET..ZERO_TAIL_OFFSET])?;
    Ok(PcssInit {
        initiator_guid,
        client_ip,
        hostname,
    })
}

fn decode_fixed_utf16_name(bytes: &[u8]) -> Result<String, PcssInitError> {
    let mut units = Vec::new();
    for chunk in bytes.chunks_exact(2) {
        let unit = u16::from_le_bytes([chunk[0], chunk[1]]);
        if unit == 0 {
            return String::from_utf16(&units).map_err(|_| PcssInitError::InvalidHostname);
        }
        units.push(unit);
    }
    Err(PcssInitError::MissingHostnameTerminator)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PcssDiscovery {
    pub host: String,
}

pub fn parse_discovery(datagram: &[u8], protocol: &str) -> Option<PcssDiscovery> {
    let text = std::str::from_utf8(datagram.strip_suffix(&[0]).unwrap_or(datagram)).ok()?;
    let mut lines = text.split("\r\n");
    (lines.next()? == "DISCOVERY * HTTP/1.1").then_some(())?;
    let mut host = None;
    let mut service_ok = false;
    for line in lines {
        let Some((k, v)) = line.split_once(':') else {
            continue;
        };
        match k.trim().to_ascii_lowercase().as_str() {
            "host" => host = Some(v.trim().to_string()),
            "service" => service_ok = v.trim() == protocol,
            _ => {}
        }
    }
    service_ok.then_some(PcssDiscovery { host: host? })
}

pub fn notify_message(camera_name: &str, command_port: u16, protocol: &str) -> Vec<u8> {
    format!(
        "NOTIFY * HTTP/1.1\r\nCAMERANAME: {camera_name}\r\nDSCPORT:{command_port}\r\nSERVICE: {protocol}\r\n\r\n\0"
    )
    .into_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid_init() -> Vec<u8> {
        let mut bytes = vec![0u8; INIT_LEN];
        bytes[0..4].copy_from_slice(&(INIT_LEN as u32).to_le_bytes());
        bytes[4..8].copy_from_slice(&INIT_PACKET_TYPE.to_le_bytes());
        bytes[8..24].copy_from_slice(&[
            0xf2, 0xe4, 0x53, 0x8f, 0xad, 0xa5, 0x48, 0x5d, 0x87, 0xb2, 0x7f, 0x0b, 0xd3, 0xd5,
            0xde, 0xd0,
        ]);
        bytes[IP_OFFSET..IP_OFFSET + 4].copy_from_slice(&[0x31, 0x07, 0xa8, 0xc0]);
        bytes[NAME_OFFSET..NAME_OFFSET + 8].copy_from_slice(&[b'm', 0, b'b', 0, b'p', 0, 0, 0]);
        bytes
    }

    #[test]
    fn parses_wire_backed_pcss_init_shape() {
        let parsed = parse_pcss_init(&valid_init()).unwrap();
        assert_eq!(parsed.client_ip, Ipv4Addr::new(192, 168, 7, 49));
        assert_eq!(parsed.hostname, "mbp");
    }

    #[test]
    fn rejects_app_tail_on_pcss_init() {
        let mut bytes = valid_init();
        bytes[0x36] = 0xcc;
        assert!(matches!(
            parse_pcss_init(&bytes),
            Err(PcssInitError::NonZeroTail)
        ));
    }

    #[test]
    fn parses_discovery_and_builds_notify() {
        let discovery =
            b"DISCOVERY * HTTP/1.1\r\nHOST: 127.0.0.1\r\nMX: 5\r\nSERVICE: PCSS/1.0\r\n\0";
        assert_eq!(
            parse_discovery(discovery, "PCSS/1.0"),
            Some(PcssDiscovery {
                host: "127.0.0.1".into()
            })
        );
        let notify = notify_message("GFX100 II", 15740, "PCSS/1.0");
        let text = std::str::from_utf8(&notify).unwrap();
        assert!(text.starts_with("NOTIFY * HTTP/1.1\r\n"));
        assert!(text.contains("DSCPORT:15740\r\n"));
        assert!(notify.ends_with(&[0]));
    }
}
