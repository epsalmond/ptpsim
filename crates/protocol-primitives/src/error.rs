use thiserror::Error;

#[derive(Debug, Error)]
pub enum FramingError {
    #[error("packet type is not valid on the Fuji compressed command channel")]
    NotOnCompressedChannel,
    #[error("packet type is not encodable as a single container in this framing")]
    UnsupportedPacket,
    #[error(transparent)]
    Encode(#[from] ptp_core::EncodeError),
}
