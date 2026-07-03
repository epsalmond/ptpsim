//! Computed quirks (id-referenced from manifests) — the cases where a property
//! value is *assembled or derived*, not looked up in a table, so it needs code.
//! Kept here as named, shared functions rather than baked into a per-brand
//! emulator.

use ptp_core::{DecodeError, Reader};

/// A record-stream layout the carrier types can't represent, or data that
/// doesn't fit the declared field widths.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RecordStreamError {
    #[error(
        "unsupported record-stream widths count={count} code={code} value={value} \
         (supported: count 1/2/4, code 1/2, value 1/2/4)"
    )]
    UnsupportedWidths { count: u8, code: u8, value: u8 },
    #[error("{field} {value:#x} does not fit a {width}-byte field")]
    Overflow {
        field: &'static str,
        value: u64,
        width: u8,
    },
}

/// Field widths of a record-stream payload, from the manifest's payload
/// descriptor (`countWidth` / `record.codeWidth` / `record.valueWidth`).
/// Construction is fallible so a manifest declaring widths this code can't
/// honor fails loudly instead of being read at the wrong width (#161).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RecordStreamLayout {
    count_width: u8,
    code_width: u8,
    value_width: u8,
}

impl RecordStreamLayout {
    /// The Fuji `0xD212` tight format: u16 count, u16 code, u32 value.
    /// Also the schema's default when a payload descriptor omits the widths.
    pub const D212: RecordStreamLayout = RecordStreamLayout {
        count_width: 2,
        code_width: 2,
        value_width: 4,
    };

    /// Widths are bounded by the carrier types (`u16` prop code, `u32` value).
    pub fn new(
        count_width: u8,
        code_width: u8,
        value_width: u8,
    ) -> Result<Self, RecordStreamError> {
        if !matches!(count_width, 1 | 2 | 4)
            || !matches!(code_width, 1 | 2)
            || !matches!(value_width, 1 | 2 | 4)
        {
            return Err(RecordStreamError::UnsupportedWidths {
                count: count_width,
                code: code_width,
                value: value_width,
            });
        }
        Ok(RecordStreamLayout {
            count_width,
            code_width,
            value_width,
        })
    }
}

/// Largest value a `width`-byte LE field can carry.
fn max_for(width: u8) -> u64 {
    (1u64 << (width * 8)) - 1
}

fn write_le(out: &mut Vec<u8>, value: u64, width: u8) {
    out.extend_from_slice(&value.to_le_bytes()[..width as usize]);
}

fn read_le(r: &mut Reader, width: u8) -> Result<u64, DecodeError> {
    Ok(match width {
        1 => r.u8()? as u64,
        2 => r.u16()? as u64,
        _ => r.u32()? as u64, // layout construction admits only 1/2/4
    })
}

/// Assemble a live-status record stream (e.g. Fuji `0xD212`): an LE element
/// count, then one record per `(prop code, value)`, each field at the
/// manifest-declared width. The member set and current values come from the
/// payload descriptor and engine state; this primitive only frames them, so
/// no per-brand layout is baked in. Wire format: operators `D212_TIGHT_FORMAT`.
/// Errors when the count, a code, or a value doesn't fit its declared field.
pub fn record_stream(
    records: &[(u16, u32)],
    layout: &RecordStreamLayout,
) -> Result<Vec<u8>, RecordStreamError> {
    let count = records.len() as u64;
    if count > max_for(layout.count_width) {
        return Err(RecordStreamError::Overflow {
            field: "record count",
            value: count,
            width: layout.count_width,
        });
    }
    let mut v = Vec::with_capacity(
        layout.count_width as usize
            + records.len() * (layout.code_width + layout.value_width) as usize,
    );
    write_le(&mut v, count, layout.count_width);
    for &(code, value) in records {
        if u64::from(code) > max_for(layout.code_width) {
            return Err(RecordStreamError::Overflow {
                field: "prop code",
                value: code.into(),
                width: layout.code_width,
            });
        }
        if u64::from(value) > max_for(layout.value_width) {
            return Err(RecordStreamError::Overflow {
                field: "value",
                value: value.into(),
                width: layout.value_width,
            });
        }
        write_le(&mut v, code.into(), layout.code_width);
        write_le(&mut v, value.into(), layout.value_width);
    }
    Ok(v)
}

/// Parse a record stream back into its `(prop code, value)` pairs — the exact
/// inverse of [`record_stream`] at the same layout. A stream that ends before
/// the declared count is a [`DecodeError::UnexpectedEof`]; trailing bytes past
/// the last record are ignored (the count is authoritative).
pub fn parse_record_stream(
    bytes: &[u8],
    layout: &RecordStreamLayout,
) -> Result<Vec<(u16, u32)>, DecodeError> {
    let mut r = Reader::new(bytes);
    let count = read_le(&mut r, layout.count_width)? as usize;
    let mut out = Vec::with_capacity(count.min(bytes.len()));
    for _ in 0..count {
        let code = read_le(&mut r, layout.code_width)? as u16;
        let value = read_le(&mut r, layout.value_width)? as u32;
        out.push((code, value));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_count_and_six_byte_records_at_d212_widths() {
        let s = record_stream(&[(0x5007, 280), (0xd209, 1)], &RecordStreamLayout::D212).unwrap();
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
        assert_eq!(
            record_stream(&[], &RecordStreamLayout::D212).unwrap(),
            0u16.to_le_bytes()
        );
    }

    #[test]
    fn parse_is_the_inverse_of_record_stream() {
        let records = vec![(0x5007u16, 280u32), (0xd209, 1), (0xd17c, 0x0403_0504)];
        let bytes = record_stream(&records, &RecordStreamLayout::D212).unwrap();
        assert_eq!(
            parse_record_stream(&bytes, &RecordStreamLayout::D212).unwrap(),
            records
        );
    }

    #[test]
    fn declared_widths_change_the_framing() {
        // A hypothetical tight vendor layout: u8 count, u8 code, u16 value.
        let layout = RecordStreamLayout::new(1, 1, 2).unwrap();
        let records = vec![(0x07u16, 280u32), (0x09, 1)];
        let bytes = record_stream(&records, &layout).unwrap();
        assert_eq!(bytes.len(), 1 + 2 * 3);
        assert_eq!(bytes[0], 2);
        assert_eq!(parse_record_stream(&bytes, &layout).unwrap(), records);
        // The same bytes read at D212 widths would mean something else entirely —
        // that divergence is exactly what #161 makes explicit.
        assert_ne!(
            parse_record_stream(&bytes, &RecordStreamLayout::D212).ok(),
            Some(records)
        );
    }

    #[test]
    fn unsupported_widths_are_a_construction_error() {
        assert!(matches!(
            RecordStreamLayout::new(3, 2, 4),
            Err(RecordStreamError::UnsupportedWidths { count: 3, .. })
        ));
        // A 4-byte code field exceeds the u16 prop-code carrier.
        assert!(matches!(
            RecordStreamLayout::new(2, 4, 4),
            Err(RecordStreamError::UnsupportedWidths { .. })
        ));
    }

    #[test]
    fn data_wider_than_a_declared_field_is_an_overflow_error() {
        let layout = RecordStreamLayout::new(1, 1, 2).unwrap();
        // Code 0x5007 doesn't fit a 1-byte code field.
        assert!(matches!(
            record_stream(&[(0x5007, 1)], &layout),
            Err(RecordStreamError::Overflow {
                field: "prop code",
                ..
            })
        ));
        // Value 0x10000 doesn't fit a 2-byte value field.
        assert!(matches!(
            record_stream(&[(0x07, 0x1_0000)], &layout),
            Err(RecordStreamError::Overflow { field: "value", .. })
        ));
    }

    #[test]
    fn parse_rejects_a_truncated_record() {
        // Count says 1 record but only 3 of the 6 record bytes are present.
        let bytes = [0x01, 0x00, 0x07, 0x50, 0x18];
        assert!(matches!(
            parse_record_stream(&bytes, &RecordStreamLayout::D212),
            Err(DecodeError::UnexpectedEof { .. })
        ));
    }
}
