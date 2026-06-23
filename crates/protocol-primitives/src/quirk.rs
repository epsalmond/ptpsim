//! Computed quirks (id-referenced from manifests) — the cases where a property
//! value is *assembled or derived*, not looked up in a table, so it needs code.
//! Kept here as named, shared functions rather than baked into a per-brand
//! emulator.

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
}
