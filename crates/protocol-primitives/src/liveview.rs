//! Live-view through-picture framing (id `fuji-liveview-v1`) from the capture-backed
//! stream contract: `u32` inclusive total length, 14 bytes of stream metadata, then
//! the JPEG payload. The frame counter resets when a new stream socket is accepted.
//! Captures expose a JPEG-body offset adjustment at `0x0c`; the simulator emits the
//! observed value zero, while the parser accepts nonzero adjustments.

/// Inclusive prefix plus Fuji stream metadata before the JPEG body.
pub const HEADER_LEN: usize = 18;

/// Wrap one JPEG frame for the live-view socket.
pub fn frame_packet(jpeg: &[u8], frame_counter: u32) -> Vec<u8> {
    let total_len =
        u32::try_from(jpeg.len() + HEADER_LEN).expect("live-view frame packet exceeds u32 length");
    let mut out = Vec::with_capacity(total_len as usize);
    out.extend_from_slice(&total_len.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&frame_counter.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(jpeg);
    out
}

/// Parse a length-prefixed frame back out (used by the smoke client/tests).
pub fn parse_frame(bytes: &[u8]) -> Option<&[u8]> {
    if bytes.len() < HEADER_LEN {
        return None;
    }
    let total_len = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as usize;
    if !(HEADER_LEN..=bytes.len()).contains(&total_len) {
        return None;
    }
    let offset_adjust = u32::from_le_bytes([bytes[12], bytes[13], bytes[14], bytes[15]]) as usize;
    let jpeg_start = HEADER_LEN.checked_add(offset_adjust)?;
    bytes.get(jpeg_start..total_len)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_round_trips() {
        let jpeg = [0xFF, 0xD8, 1, 2, 3, 0xFF, 0xD9];
        let pkt = frame_packet(&jpeg, 42);
        assert_eq!(pkt.len(), HEADER_LEN + jpeg.len());
        assert_eq!(pkt[0..4], [25, 0, 0, 0]);
        assert_eq!(pkt[4..8], [0, 0, 0, 0]);
        assert_eq!(pkt[8..12], [42, 0, 0, 0]);
        assert_eq!(pkt[12..18], [0, 0, 0, 0, 0, 0]);
        assert_eq!(parse_frame(&pkt).unwrap(), &jpeg);
    }

    #[test]
    fn parser_applies_jpeg_body_offset_adjustment() {
        let jpeg = [0xff, 0xd8, 0xff, 0xd9];
        let mut pkt = frame_packet(&jpeg, 0);
        let adjusted_len = pkt.len() as u32 + 2;
        pkt[0..4].copy_from_slice(&adjusted_len.to_le_bytes());
        pkt[12..16].copy_from_slice(&2u32.to_le_bytes());
        pkt.splice(HEADER_LEN..HEADER_LEN, [0xaa, 0xbb]);
        assert_eq!(parse_frame(&pkt).unwrap(), &jpeg);
    }
}
