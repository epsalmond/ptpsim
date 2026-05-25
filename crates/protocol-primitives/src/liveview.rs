//! Live-view through-picture framing (id `fuji-liveview-v1`): each JPEG frame is
//! emitted on the stream socket as a length-prefixed packet. The exact Fuji
//! header is provisional and flagged for capture reconciliation; the shape
//! (u32 length prefix + JPEG payload) is what the engine needs to pace frames.

/// Wrap one JPEG frame for the live-view socket.
pub fn frame_packet(jpeg: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(jpeg.len() + 4);
    out.extend_from_slice(&((jpeg.len() as u32).to_le_bytes()));
    out.extend_from_slice(jpeg);
    out
}

/// Parse a length-prefixed frame back out (used by the smoke client/tests).
pub fn parse_frame(bytes: &[u8]) -> Option<&[u8]> {
    if bytes.len() < 4 {
        return None;
    }
    let len = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as usize;
    bytes.get(4..4 + len)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_round_trips() {
        let jpeg = [0xFF, 0xD8, 1, 2, 3, 0xFF, 0xD9];
        let pkt = frame_packet(&jpeg);
        assert_eq!(pkt[0..4], [7, 0, 0, 0]);
        assert_eq!(parse_frame(&pkt).unwrap(), &jpeg);
    }
}
