//! Bounded chunk windows and source-wide checksum framing. No transport or
//! transfer choreography is implied by this codec.

use thiserror::Error;

pub const MAX_CHUNK_WINDOWS: usize = 4096;
pub const SENTINEL: u64 = 65535;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChunkChecksum {
    SourceSum16Le,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Backup,
    Restore,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum ChunkError {
    #[error("empty source unsupported by checksum policy")]
    EmptySource,
    #[error("invalid checksum frame layout")]
    Layout,
    #[error("chunk index out of range")]
    Index,
    #[error("chunk transfer exceeds resource limit")]
    Limit,
    #[error("frame field cannot be encoded")]
    HeaderEncoding,
    #[error("short frame")]
    ShortFrame,
    #[error("short declared body")]
    ShortBody,
    #[error("truncated declared body")]
    TruncatedBody,
    #[error("invalid window length")]
    WindowLength,
    #[error("ordinary chunk sequence mismatch")]
    Sequence,
    #[error("source checksum mismatch")]
    Checksum,
    #[error("restore trailer mismatch")]
    Trailer,
    #[error("frame after sentinel")]
    AfterSentinel,
    #[error("missing sentinel")]
    MissingSentinel,
    #[error("source size mismatch")]
    Size,
}

/// Wrap on every addition, so arbitrarily long sources cannot overflow an
/// intermediate accumulator. Bytes are unsigned and framing is excluded.
pub fn source_sum16(source: &[u8]) -> u16 {
    source
        .iter()
        .fold(0u16, |sum, byte| sum.wrapping_add(u16::from(*byte)))
}

/// Select and encode one source window. The caller supplies the declared
/// header encoder, preserving legacy field layouts when checksum is absent.
/// Checksum callers must use the fixed u16/u32 little-endian layout.
pub fn encode_window(
    source: &[u8],
    size: usize,
    index: u64,
    sentinel: u64,
    checksum: Option<ChunkChecksum>,
    header: impl FnOnce(u64, u64) -> Option<Vec<u8>>,
) -> Result<Vec<u8>, ChunkError> {
    if checksum.is_some() && (!(2..=u32::MAX as usize - 2).contains(&size) || sentinel != SENTINEL)
    {
        return Err(ChunkError::Layout);
    }
    if source.is_empty() && checksum.is_some() {
        return Err(ChunkError::EmptySource);
    }
    let size = size.max(1);
    let full = source.len().saturating_sub(1) / size;
    if full >= MAX_CHUNK_WINDOWS {
        return Err(ChunkError::Limit);
    }
    let final_window = index == sentinel;
    let ordinal = if final_window {
        full
    } else {
        let ordinal = usize::try_from(index).map_err(|_| ChunkError::Index)?;
        if ordinal >= full {
            return Err(ChunkError::Index);
        }
        ordinal
    };
    let offset = ordinal * size;
    let end = offset.saturating_add(size).min(source.len());
    let payload = &source[offset..end];
    let sum = if checksum.is_some() && final_window {
        source_sum16(source)
    } else {
        0
    };
    let declared = payload.len() as u64 + u64::from(checksum.is_some() && final_window) * 2;
    let mut frame = header(index, declared).ok_or(ChunkError::HeaderEncoding)?;
    if checksum.is_some() {
        let expected = [
            (index as u16).to_le_bytes().as_slice(),
            (declared as u32).to_le_bytes().as_slice(),
        ]
        .concat();
        if index > u16::MAX as u64 || declared > u32::MAX as u64 || frame != expected {
            return Err(ChunkError::Layout);
        }
    }
    frame.extend_from_slice(payload);
    if checksum.is_some() {
        if final_window {
            frame.extend_from_slice(&sum.to_le_bytes());
        }
        frame.extend_from_slice(&sum.to_le_bytes());
    }
    Ok(frame)
}

/// Incremental validator for the sourceSum16Le layout. Allocation is bounded
/// by `max_source_size`; the transfer is also capped at 4096 frames. An error
/// leaves the previously accepted state unchanged.
pub struct SourceSum16Decoder {
    direction: Direction,
    window: usize,
    max_source_size: usize,
    expected_size: Option<usize>,
    source: Vec<u8>,
    sum: u16,
    windows: usize,
    final_seen: bool,
}

impl SourceSum16Decoder {
    pub fn new(
        direction: Direction,
        window: usize,
        max_source_size: usize,
        expected_size: Option<usize>,
    ) -> Result<Self, ChunkError> {
        if !(2..=u32::MAX as usize - 2).contains(&window) {
            return Err(ChunkError::Layout);
        }
        if expected_size.is_some_and(|size| size > max_source_size) {
            return Err(ChunkError::Limit);
        }
        Ok(Self {
            direction,
            window,
            max_source_size,
            expected_size,
            source: Vec::new(),
            sum: 0,
            windows: 0,
            final_seen: false,
        })
    }

    pub fn push(&mut self, raw: &[u8]) -> Result<(), ChunkError> {
        if self.final_seen {
            return Err(ChunkError::AfterSentinel);
        }
        if self.windows >= MAX_CHUNK_WINDOWS {
            return Err(ChunkError::Limit);
        }
        if raw.len() < 8 {
            return Err(ChunkError::ShortFrame);
        }
        let index = u16::from_le_bytes([raw[0], raw[1]]);
        let declared = u32::from_le_bytes(raw[2..6].try_into().unwrap()) as usize;
        if declared < 2 {
            return Err(ChunkError::ShortBody);
        }
        let end = declared.checked_add(6).ok_or(ChunkError::TruncatedBody)?;
        let body = raw.get(6..end).ok_or(ChunkError::TruncatedBody)?;
        let final_window = u64::from(index) == SENTINEL;
        if !final_window && usize::from(index) != self.windows {
            return Err(ChunkError::Sequence);
        }
        let (payload, observed) = if final_window {
            let (payload, sum) = body.split_at(body.len() - 2);
            (payload, u16::from_le_bytes(sum.try_into().unwrap()))
        } else {
            (body, 0)
        };
        if payload.len() > self.window || (!final_window && payload.len() != self.window) {
            return Err(ChunkError::WindowLength);
        }
        let size = self
            .source
            .len()
            .checked_add(payload.len())
            .ok_or(ChunkError::Limit)?;
        if size > self.max_source_size {
            return Err(ChunkError::Limit);
        }
        let sum = self.sum.wrapping_add(source_sum16(payload));
        if final_window && sum != observed {
            return Err(ChunkError::Checksum);
        }
        if self.direction == Direction::Restore && raw[end..] != observed.to_le_bytes() {
            return Err(ChunkError::Trailer);
        }
        if payload.is_empty() {
            return Err(ChunkError::EmptySource);
        }
        if self
            .expected_size
            .is_some_and(|expected| size > expected || (final_window && size != expected))
        {
            return Err(ChunkError::Size);
        }
        if size > self.source.capacity() {
            let capacity = self
                .source
                .capacity()
                .saturating_mul(2)
                .max(size)
                .min(self.max_source_size);
            self.source
                .try_reserve_exact(capacity - self.source.len())
                .map_err(|_| ChunkError::Limit)?;
        }
        self.source.extend_from_slice(payload);
        self.sum = sum;
        self.windows += 1;
        self.final_seen = final_window;
        Ok(())
    }

    pub fn finish(self) -> Result<Vec<u8>, ChunkError> {
        if !self.final_seen {
            return Err(ChunkError::MissingSentinel);
        }
        Ok(self.source)
    }
}
