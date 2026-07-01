//! Computed quirks (id-referenced from manifests) — the cases where a property
//! value is *assembled or derived*, not looked up in a table, so it needs code.
//! Kept here as named, shared functions rather than baked into a per-brand
//! emulator.

use ptp_core::{DecodeError, Reader};

/// Assemble a Fuji `0xD212`-style live-status record stream: a `u16` LE element
/// count, then one 6-byte record per `(prop code, value)` — `<code u16 LE>`
/// `<value u32 LE>`, the value zero-padded from its native width. The member
/// set and the current values come from the manifest's payload descriptor and
/// engine state; this primitive only frames them, so no per-brand layout is
/// baked in. Wire format: operators `D212_TIGHT_FORMAT`.
pub fn record_stream(records: &[(u16, u32)]) -> Vec<u8> {
    let count = records.len() as u16;
    let mut v = Vec::with_capacity(2 + records.len() * 6);
    v.extend_from_slice(&count.to_le_bytes());
    for (code, value) in records {
        v.extend_from_slice(&code.to_le_bytes());
        v.extend_from_slice(&value.to_le_bytes());
    }
    v
}

/// Parse a Fuji `0xD212`-style live-status record stream back into its
/// `(prop code, value)` pairs — the exact inverse of [`record_stream`]. Reads a
/// `u16` LE element count, then that many 6-byte records (`<code u16 LE>`
/// `<value u32 LE>`). A stream that ends before the declared count is a
/// [`DecodeError::UnexpectedEof`]; trailing bytes past the last record are
/// ignored (the count is authoritative).
pub fn parse_record_stream(bytes: &[u8]) -> Result<Vec<(u16, u32)>, DecodeError> {
    let mut r = Reader::new(bytes);
    let count = r.u16()? as usize;
    let mut out = Vec::with_capacity(count);
    for _ in 0..count {
        let code = r.u16()?;
        let value = r.u32()?;
        out.push((code, value));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_count_and_six_byte_records() {
        let s = record_stream(&[(0x5007, 280), (0xd209, 1)]);
        // u16 count, then 6 bytes per record.
        assert_eq!(s.len(), 2 + 2 * 6);
        assert_eq!(&s[0..2], &2u16.to_le_bytes());
        assert_eq!(&s[2..4], &0x5007u16.to_le_bytes());
        assert_eq!(&s[4..8], &280u32.to_le_bytes());
        assert_eq!(&s[8..10], &0xd209u16.to_le_bytes());
        assert_eq!(&s[10..14], &1u32.to_le_bytes());
    }

    #[test]
    fn empty_is_just_a_zero_count() {
        assert_eq!(record_stream(&[]), 0u16.to_le_bytes());
    }

    #[test]
    fn parse_is_the_inverse_of_record_stream() {
        let records = vec![(0x5007u16, 280u32), (0xd209, 1), (0xd17c, 0x0403_0504)];
        let bytes = record_stream(&records);
        assert_eq!(parse_record_stream(&bytes).unwrap(), records);
        // Empty stream round-trips to no records.
        assert_eq!(parse_record_stream(&record_stream(&[])).unwrap(), vec![]);
    }

    #[test]
    fn parse_rejects_a_truncated_record() {
        // Count says 1 record but only 3 of the 6 record bytes are present.
        let bytes = [0x01, 0x00, 0x07, 0x50, 0x18];
        assert!(matches!(
            parse_record_stream(&bytes),
            Err(DecodeError::UnexpectedEof { .. })
        ));
    }
}
