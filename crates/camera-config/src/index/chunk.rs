use protocol_primitives::chunk_frame::{self, ChunkError};

use super::{eval, BleWriteChunkStep, ChunkChecksum, ChunkField, ChunkFrameField, Encoding};

impl BleWriteChunkStep {
    /// Validate the fixed layout selected by the optional checksum policy.
    /// Legacy layouts retain their existing behavior.
    pub fn validate_checksum_layout(&self) -> Result<(), ChunkError> {
        if self.checksum.is_some()
            && (self.size < 2
                || self.size > u32::MAX - 2
                || self.sentinel_index != chunk_frame::SENTINEL as u32
                || self.frame
                    != [
                        ChunkFrameField {
                            field: ChunkField::Index,
                            encoding: Encoding::U16Le,
                        },
                        ChunkFrameField {
                            field: ChunkField::Length,
                            encoding: Encoding::U32Le,
                        },
                    ])
        {
            return Err(ChunkError::Layout);
        }
        Ok(())
    }

    /// Shared framing entrypoint for the simulator and foreign executor.
    pub fn encode_chunk(&self, source: &[u8], index: u64) -> Result<Vec<u8>, ChunkError> {
        self.validate_checksum_layout()?;
        chunk_frame::encode_window(
            source,
            self.size as usize,
            index,
            u64::from(self.sentinel_index),
            self.checksum.map(|checksum| match checksum {
                ChunkChecksum::SourceSum16Le => chunk_frame::ChunkChecksum::SourceSum16Le,
            }),
            |index, length| {
                let mut header = Vec::new();
                for field in &self.frame {
                    let value = match field.field {
                        ChunkField::Index => index,
                        ChunkField::Length => length,
                    };
                    header.extend(eval::encode_uint(value, field.encoding)?);
                }
                Some(header)
            },
        )
    }
}
