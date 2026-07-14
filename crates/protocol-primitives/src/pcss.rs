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
    HostnameTooLong,
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
            Self::HostnameTooLong => write!(f, "PCSS init hostname exceeds 12 UTF-16 code units"),
            Self::NonZeroTail => write!(f, "PCSS init zero-tail contains nonzero bytes"),
        }
    }
}

impl std::error::Error for PcssInitError {}

/// Build the fixed PCSS InitCommandRequest. Unlike ordinary PTP/IP init, this
/// layout carries the route-selected client IPv4 and a fixed-width host name.
pub fn pcss_init_message(
    initiator_guid: [u8; 16],
    client_ip: Ipv4Addr,
    hostname: &str,
) -> Result<Vec<u8>, PcssInitError> {
    let hostname: Vec<u16> = hostname.encode_utf16().collect();
    let max_units = (ZERO_TAIL_OFFSET - NAME_OFFSET) / 2 - 1;
    if hostname.contains(&0) {
        return Err(PcssInitError::InvalidHostname);
    }
    if hostname.len() > max_units {
        return Err(PcssInitError::HostnameTooLong);
    }

    let mut bytes = vec![0u8; INIT_LEN];
    bytes[0..4].copy_from_slice(&(INIT_LEN as u32).to_le_bytes());
    bytes[4..8].copy_from_slice(&INIT_PACKET_TYPE.to_le_bytes());
    bytes[8..24].copy_from_slice(&initiator_guid);
    bytes[IP_OFFSET..IP_OFFSET + 4].copy_from_slice(&u32::from(client_ip).to_le_bytes());
    for (index, unit) in hostname.into_iter().enumerate() {
        let offset = NAME_OFFSET + index * 2;
        bytes[offset..offset + 2].copy_from_slice(&unit.to_le_bytes());
    }
    Ok(bytes)
}

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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PcssNotify {
    pub camera_address: Ipv4Addr,
    pub camera_name: String,
    pub command_port: u16,
    pub service: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PcssMessageError {
    InvalidIpv4,
    InvalidUtf8,
    WrongStartLine,
    MissingField(&'static str),
    WrongProtocol,
    InvalidCommandPort,
}

impl fmt::Display for PcssMessageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidIpv4 => write!(f, "PCSS address field is not an IPv4 address"),
            Self::InvalidUtf8 => write!(f, "PCSS message is not valid UTF-8"),
            Self::WrongStartLine => write!(f, "PCSS message has the wrong start line"),
            Self::MissingField(field) => write!(f, "PCSS message is missing {field}"),
            Self::WrongProtocol => write!(f, "PCSS message SERVICE does not match the manifest"),
            Self::InvalidCommandPort => write!(f, "PCSS notification DSCPORT is invalid"),
        }
    }
}

impl std::error::Error for PcssMessageError {}

/// Build the discovery datagram sent to the camera's manifest-declared knock
/// port. `host` must be the route-selected local IPv4 address on which the
/// caller is already listening for the callback.
pub fn discovery_message(host: Ipv4Addr, protocol: &str) -> Vec<u8> {
    format!("DISCOVERY * HTTP/1.1\r\nHOST: {host}\r\nMX: 5\r\nSERVICE: {protocol}\r\n\0")
        .into_bytes()
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

pub fn notify_message(
    camera_address: Ipv4Addr,
    camera_name: &str,
    command_port: u16,
    protocol: &str,
) -> Vec<u8> {
    format!(
        "NOTIFY * HTTP/1.1\r\nDSC: {camera_address}\r\nCAMERANAME: {camera_name}\r\nDSCPORT: {command_port}\r\nMX: 7\r\nSERVICE: {protocol}\r\n"
    )
    .into_bytes()
}

/// Parse the camera's TCP callback and verify it names the protocol selected by
/// the manifest. Unknown headers are ignored for forward compatibility.
pub fn parse_notify(datagram: &[u8], protocol: &str) -> Result<PcssNotify, PcssMessageError> {
    let text = std::str::from_utf8(datagram.strip_suffix(&[0]).unwrap_or(datagram))
        .map_err(|_| PcssMessageError::InvalidUtf8)?;
    let mut lines = text.split("\r\n");
    if lines.next() != Some("NOTIFY * HTTP/1.1") {
        return Err(PcssMessageError::WrongStartLine);
    }
    let mut camera_address = None;
    let mut camera_name = None;
    let mut command_port = None;
    let mut service = None;
    for line in lines {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        match key.trim().to_ascii_lowercase().as_str() {
            "dsc" => {
                camera_address = Some(
                    value
                        .trim()
                        .parse::<Ipv4Addr>()
                        .map_err(|_| PcssMessageError::InvalidIpv4)?,
                )
            }
            "cameraname" => camera_name = Some(value.trim().to_string()),
            "dscport" => {
                let port = value
                    .trim()
                    .parse::<u16>()
                    .map_err(|_| PcssMessageError::InvalidCommandPort)?;
                if port == 0 {
                    return Err(PcssMessageError::InvalidCommandPort);
                }
                command_port = Some(port)
            }
            "service" => service = Some(value.trim()),
            _ => {}
        }
    }
    let service = service.ok_or(PcssMessageError::MissingField("SERVICE"))?;
    if service != protocol {
        return Err(PcssMessageError::WrongProtocol);
    }
    Ok(PcssNotify {
        camera_address: camera_address.ok_or(PcssMessageError::MissingField("DSC"))?,
        camera_name: camera_name.ok_or(PcssMessageError::MissingField("CAMERANAME"))?,
        command_port: command_port.ok_or(PcssMessageError::MissingField("DSCPORT"))?,
        service: service.to_string(),
    })
}

/// Acknowledge a valid PCSS callback. This is the byte-exact response observed
/// on the callback socket; the caller closes the socket after writing it.
pub fn callback_ack_message() -> Vec<u8> {
    b"HTTP/1.1 200 OK\r\n\0".to_vec()
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
    fn builds_wire_backed_pcss_init_shape() {
        let guid: [u8; 16] = valid_init()[8..24].try_into().unwrap();
        let bytes = pcss_init_message(guid, "192.168.7.49".parse().unwrap(), "mbp").unwrap();
        assert_eq!(bytes, valid_init());
        assert_eq!(parse_pcss_init(&bytes).unwrap().hostname, "mbp");
    }

    #[test]
    fn rejects_pcss_init_hostname_over_fixed_field() {
        assert_eq!(
            pcss_init_message([0; 16], Ipv4Addr::LOCALHOST, "thirteen-units").unwrap_err(),
            PcssInitError::HostnameTooLong
        );
    }

    #[test]
    fn rejects_pcss_init_hostname_with_embedded_nul() {
        assert_eq!(
            pcss_init_message([0; 16], Ipv4Addr::LOCALHOST, "mbp\0other").unwrap_err(),
            PcssInitError::InvalidHostname
        );
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
        let notify = notify_message(Ipv4Addr::new(192, 0, 2, 94), "GFX100 II", 15740, "PCSS/1.0");
        let text = std::str::from_utf8(&notify).unwrap();
        assert!(text.starts_with("NOTIFY * HTTP/1.1\r\n"));
        assert!(text.contains("DSC: 192.0.2.94\r\n"));
        assert!(text.contains("DSCPORT: 15740\r\n"));
        assert!(notify.ends_with(b"\r\n"));
    }

    #[test]
    fn builds_discovery_and_parses_notify_and_ack() {
        assert_eq!(
            discovery_message(Ipv4Addr::new(192, 168, 7, 49), "PCSS/1.0"),
            b"DISCOVERY * HTTP/1.1\r\nHOST: 192.168.7.49\r\nMX: 5\r\nSERVICE: PCSS/1.0\r\n\0"
        );
        let notify = notify_message(Ipv4Addr::new(192, 0, 2, 94), "CAMERA", 15740, "PCSS/1.0");
        assert_eq!(
            parse_notify(&notify, "PCSS/1.0"),
            Ok(PcssNotify {
                camera_address: Ipv4Addr::new(192, 0, 2, 94),
                camera_name: "CAMERA".into(),
                command_port: 15740,
                service: "PCSS/1.0".into(),
            })
        );
        assert_eq!(callback_ack_message(), b"HTTP/1.1 200 OK\r\n\0");
    }

    #[test]
    fn rejects_notify_for_a_different_protocol() {
        let notify = notify_message(Ipv4Addr::LOCALHOST, "CAMERA", 15740, "OTHER/1.0");
        assert_eq!(
            parse_notify(&notify, "PCSS/1.0"),
            Err(PcssMessageError::WrongProtocol)
        );
    }

    #[test]
    fn rejects_notify_with_a_zero_command_port() {
        let notify = notify_message(Ipv4Addr::LOCALHOST, "CAMERA", 0, "PCSS/1.0");
        assert_eq!(
            parse_notify(&notify, "PCSS/1.0"),
            Err(PcssMessageError::InvalidCommandPort)
        );
    }
}
