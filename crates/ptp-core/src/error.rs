//! Codec error types. `ptp-core` is protocol *syntax* only, so these describe
//! malformed bytes and buffer problems — never anything model- or
//! Fuji-specific.

use thiserror::Error;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum DecodeError {
    #[error("unexpected end of input: needed {needed} more byte(s) at offset {offset}")]
    UnexpectedEof { offset: usize, needed: usize },
    #[error("invalid PTP string: {0}")]
    InvalidString(&'static str),
    #[error("declared length {declared} does not match buffer length {actual}")]
    LengthMismatch { declared: usize, actual: usize },
    #[error("unknown PTP/IP packet type {0}")]
    UnknownPacketType(u32),
    #[error("array element count {0} is implausibly large")]
    ArrayTooLong(u32),
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum EncodeError {
    #[error("PTP string too long ({0} code units; max 254 incl. terminator)")]
    StringTooLong(usize),
}
