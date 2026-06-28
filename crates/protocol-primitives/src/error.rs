use thiserror::Error;

#[derive(Debug, Error)]
pub enum FramingError {
    #[error("packet type is not valid on the Fuji compressed command channel")]
    NotOnCompressedChannel,
    #[error("packet type is not encodable as a single container in this framing")]
    UnsupportedPacket,
    #[error(transparent)]
    Encode(#[from] ptp_core::EncodeError),
    #[error("GUID must be 16 bytes, got {0}")]
    GuidLength(usize),
    #[error("value {value} does not fit in a {width}-byte property (signed={signed})")]
    ValueTooWide { value: i64, width: u8, signed: bool },
    #[error("InitCommandAck malformed: {0}")]
    InitAck(String),
}
