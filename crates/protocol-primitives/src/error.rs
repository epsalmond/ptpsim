use thiserror::Error;

#[derive(Debug, Error)]
pub enum FramingError {
    #[error("packet type is not valid on the Fuji compressed command channel")]
    NotOnCompressedChannel,
    #[error(transparent)]
    Encode(#[from] ptp_core::EncodeError),
}
