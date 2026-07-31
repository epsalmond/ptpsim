//! Computed quirks (id-referenced from manifests) — the cases where a property
//! value is *assembled or derived*, not looked up in a table, so it needs code.
//! Kept here as named, shared functions rather than baked into a per-brand
//! emulator.

use std::collections::BTreeMap;

use ptp_core::{DecodeError, EncodeError, PropValue, Reader, Writer};

/// A record-stream layout the carrier types can't represent, or data that
/// doesn't fit the declared field widths.
#[derive(Debug, thiserror::Error)]
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
    #[error(
        "record-stream member {code:#06x} negative value {value} cannot use unsigned \
         {width}-byte fixed encoding"
    )]
    NegativeFixedValue { code: u16, value: i64, width: u8 },
    #[error(
        "record-stream member {code:#06x} signed value {value} does not fit a {width}-byte field"
    )]
    SignedOverflow { code: u16, value: i128, width: u8 },
    #[error("record-stream member {code:#06x} is not declared")]
    UndeclaredMember { code: u16 },
    #[error("record-stream member {code:#06x} is declared more than once")]
    DuplicateMember { code: u16 },
    #[error("record-stream member {code:#06x} value does not match {encoding:?}")]
    ValueTypeMismatch {
        code: u16,
        encoding: RecordValueEncoding,
    },
    #[error(transparent)]
    Decode(#[from] DecodeError),
    #[error(transparent)]
    Encode(#[from] EncodeError),
}

/// Payload-local wire encoding for a record member. `Fixed` is raw unsigned
/// little-endian. `Signed` preserves the declared signed value at its payload
/// width, including sign extension when the source type is narrower.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecordValueEncoding {
    Fixed { width: u8 },
    Signed { width: u8 },
    PtpString,
}

impl RecordValueEncoding {
    /// Whether a value has the right kind and sign for this encoding. This does
    /// not check whether a nonnegative magnitude fits a fixed field's width;
    /// positive overflow remains a codec error rather than a type mismatch.
    pub fn accepts_value(self, value: &PropValue) -> bool {
        match self {
            Self::Fixed { .. } => matches!(numeric_value(value), NumericValue::Unsigned(_)),
            Self::Signed { .. } => !matches!(numeric_value(value), NumericValue::NotNumeric),
            Self::PtpString => matches!(value, PropValue::Str(_)),
        }
    }
}

/// A heterogeneous record-stream descriptor resolved from manifest data.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordStreamDescriptor {
    layout: RecordStreamLayout,
    members: BTreeMap<u16, RecordValueEncoding>,
}

impl RecordStreamDescriptor {
    pub fn new(
        layout: RecordStreamLayout,
        members: impl IntoIterator<Item = (u16, RecordValueEncoding)>,
    ) -> Result<Self, RecordStreamError> {
        let mut resolved = BTreeMap::new();
        for (code, encoding) in members {
            if layout.code_width == 1 && code > u8::MAX as u16 {
                return Err(RecordStreamError::Overflow {
                    field: "prop code",
                    value: code.into(),
                    width: layout.code_width,
                });
            }
            if let RecordValueEncoding::Fixed { width } | RecordValueEncoding::Signed { width } =
                encoding
            {
                if !matches!(width, 1 | 2 | 4) {
                    return Err(RecordStreamError::UnsupportedWidths {
                        count: layout.count_width,
                        code: layout.code_width,
                        value: width,
                    });
                }
            }
            if resolved.insert(code, encoding).is_some() {
                return Err(RecordStreamError::DuplicateMember { code });
            }
        }
        Ok(Self {
            layout,
            members: resolved,
        })
    }

    fn encoding(&self, code: u16) -> Result<RecordValueEncoding, RecordStreamError> {
        self.members
            .get(&code)
            .copied()
            .ok_or(RecordStreamError::UndeclaredMember { code })
    }
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

enum NumericValue {
    Unsigned(u64),
    Negative(i64),
    NotNumeric,
}

fn signed_numeric_value(value: i64) -> NumericValue {
    if value < 0 {
        NumericValue::Negative(value)
    } else {
        NumericValue::Unsigned(value as u64)
    }
}

fn numeric_value(value: &PropValue) -> NumericValue {
    match value {
        PropValue::I8(value) => signed_numeric_value(i64::from(*value)),
        PropValue::U8(value) => NumericValue::Unsigned((*value).into()),
        PropValue::I16(value) => signed_numeric_value(i64::from(*value)),
        PropValue::U16(value) => NumericValue::Unsigned((*value).into()),
        PropValue::I32(value) => signed_numeric_value(i64::from(*value)),
        PropValue::U32(value) => NumericValue::Unsigned((*value).into()),
        PropValue::I64(value) => signed_numeric_value(*value),
        PropValue::U64(value) => NumericValue::Unsigned(*value),
        PropValue::Str(_) => NumericValue::NotNumeric,
    }
}

fn write_ptp_string(out: &mut Vec<u8>, value: &str) -> Result<(), EncodeError> {
    if value.is_empty() {
        out.extend_from_slice(&[1, 0, 0]);
        return Ok(());
    }
    let mut writer = Writer::new();
    writer.ptp_string(value)?;
    out.extend_from_slice(writer.as_slice());
    Ok(())
}

/// Assemble a manifest-resolved heterogeneous record stream. Records must be
/// declared and their values must match the member-local encoding. Fixed
/// values are raw unsigned magnitudes; negative signed values are never
/// reinterpreted as two's-complement bit patterns.
pub fn typed_record_stream(
    records: &[(u16, PropValue)],
    descriptor: &RecordStreamDescriptor,
) -> Result<Vec<u8>, RecordStreamError> {
    let count = records.len() as u64;
    if count > max_for(descriptor.layout.count_width) {
        return Err(RecordStreamError::Overflow {
            field: "record count",
            value: count,
            width: descriptor.layout.count_width,
        });
    }
    let mut out = Vec::new();
    write_le(&mut out, count, descriptor.layout.count_width);
    for (code, value) in records {
        let encoding = descriptor.encoding(*code)?;
        write_le(&mut out, (*code).into(), descriptor.layout.code_width);
        match encoding {
            RecordValueEncoding::Fixed { width } => {
                let value = match numeric_value(value) {
                    NumericValue::Unsigned(value) => value,
                    NumericValue::Negative(value) => {
                        return Err(RecordStreamError::NegativeFixedValue {
                            code: *code,
                            value,
                            width,
                        });
                    }
                    NumericValue::NotNumeric => {
                        return Err(RecordStreamError::ValueTypeMismatch {
                            code: *code,
                            encoding,
                        });
                    }
                };
                if value > max_for(width) {
                    return Err(RecordStreamError::Overflow {
                        field: "value",
                        value,
                        width,
                    });
                }
                write_le(&mut out, value, width);
            }
            RecordValueEncoding::Signed { width } => {
                let value = match numeric_value(value) {
                    NumericValue::Unsigned(value) => i128::from(value),
                    NumericValue::Negative(value) => i128::from(value),
                    NumericValue::NotNumeric => {
                        return Err(RecordStreamError::ValueTypeMismatch {
                            code: *code,
                            encoding,
                        });
                    }
                };
                let bits = u32::from(width) * 8;
                let min = -(1_i128 << (bits - 1));
                let max = (1_i128 << (bits - 1)) - 1;
                if !(min..=max).contains(&value) {
                    return Err(RecordStreamError::SignedOverflow {
                        code: *code,
                        value,
                        width,
                    });
                }
                write_le(&mut out, value as i64 as u64, width);
            }
            RecordValueEncoding::PtpString => {
                let PropValue::Str(value) = value else {
                    return Err(RecordStreamError::ValueTypeMismatch {
                        code: *code,
                        encoding,
                    });
                };
                write_ptp_string(&mut out, value)?;
            }
        }
    }
    Ok(out)
}

/// One recoverable record-stream decode condition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecordStreamDiagnostic {
    SkippedUndeclaredMember { code: u16, value: u32 },
}

/// Records and recoverable diagnostics from one record-stream decode.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodedRecordStream {
    pub records: Vec<(u16, PropValue)>,
    pub diagnostics: Vec<RecordStreamDiagnostic>,
}

/// Decode a manifest-resolved heterogeneous record stream. The count remains
/// authoritative. Declared members consume their declared encoding. An
/// undeclared member tentatively consumes the payload's default fixed width.
/// That tolerant result is accepted only when exactly `count` records consume
/// the complete payload. Otherwise the first undeclared member remains a hard
/// error. Payloads without undeclared members retain count-authoritative
/// handling of trailing bytes.
pub fn parse_typed_record_stream(
    bytes: &[u8],
    descriptor: &RecordStreamDescriptor,
) -> Result<DecodedRecordStream, RecordStreamError> {
    let mut reader = Reader::new(bytes);
    let count = read_le(&mut reader, descriptor.layout.count_width)? as usize;
    let mut out = Vec::with_capacity(count.min(bytes.len()));
    let mut diagnostics = Vec::new();
    let mut first_undeclared = None;
    for _ in 0..count {
        let code = match read_le(&mut reader, descriptor.layout.code_width) {
            Ok(code) => code as u16,
            Err(error) => {
                return Err(first_undeclared
                    .map(|code| RecordStreamError::UndeclaredMember { code })
                    .unwrap_or_else(|| error.into()));
            }
        };
        let Some(encoding) = descriptor.members.get(&code).copied() else {
            let original = *first_undeclared.get_or_insert(code);
            let value = match read_le(&mut reader, descriptor.layout.value_width) {
                Ok(value) => value as u32,
                Err(_) => return Err(RecordStreamError::UndeclaredMember { code: original }),
            };
            diagnostics.push(RecordStreamDiagnostic::SkippedUndeclaredMember { code, value });
            continue;
        };
        let value = match encoding {
            RecordValueEncoding::Fixed { width } => {
                read_le(&mut reader, width).map(|value| PropValue::U32(value as u32))
            }
            RecordValueEncoding::Signed { width } => match width {
                1 => reader.i8().map(PropValue::I8),
                2 => reader.i16().map(PropValue::I16),
                _ => reader.i32().map(PropValue::I32),
            },
            RecordValueEncoding::PtpString => reader.ptp_string().map(PropValue::Str),
        };
        match value {
            Ok(value) => out.push((code, value)),
            Err(error) => {
                if let Some(code) = first_undeclared {
                    return Err(RecordStreamError::UndeclaredMember { code });
                }
                return Err(error.into());
            }
        }
    }
    if let Some(code) = first_undeclared {
        if reader.remaining() != 0 {
            return Err(RecordStreamError::UndeclaredMember { code });
        }
    }
    Ok(DecodedRecordStream {
        records: out,
        diagnostics,
    })
}

/// Assemble a live-status record stream (e.g. Fuji `0xD212`): an LE element
/// count, then one record per `(prop code, value)`, each field at the
/// manifest-declared width. The member set and current values come from the
/// payload descriptor and engine state; this primitive only frames them, so
/// no per-brand layout is baked in.
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

    fn heterogeneous_descriptor() -> RecordStreamDescriptor {
        RecordStreamDescriptor::new(
            RecordStreamLayout::D212,
            [
                (0xdf00, RecordValueEncoding::Fixed { width: 4 }),
                (0xd220, RecordValueEncoding::Fixed { width: 4 }),
                (0xdf41, RecordValueEncoding::Fixed { width: 4 }),
                (0xd22f, RecordValueEncoding::PtpString),
            ],
        )
        .unwrap()
    }

    fn fixed_descriptor(width: u8) -> RecordStreamDescriptor {
        RecordStreamDescriptor::new(
            RecordStreamLayout::new(2, 2, width).unwrap(),
            [(0xd100, RecordValueEncoding::Fixed { width })],
        )
        .unwrap()
    }

    #[test]
    fn decodes_complete_empty_ptp_string_record() {
        let bytes = [0x01, 0x00, 0x2f, 0xd2, 0x01, 0x00, 0x00];
        assert_eq!(
            parse_typed_record_stream(&bytes, &heterogeneous_descriptor())
                .unwrap()
                .records,
            vec![(0xd22f, PropValue::Str(String::new()))]
        );
    }

    #[test]
    fn decodes_complete_mixed_record_stream() {
        let bytes = [
            0x04, 0x00, 0x00, 0xdf, 0x12, 0x00, 0x00, 0x00, 0x20, 0xd2, 0x01, 0x00, 0x00, 0x00,
            0x41, 0xdf, 0x01, 0x00, 0x00, 0x00, 0x2f, 0xd2, 0x01, 0x00, 0x00,
        ];
        assert_eq!(
            parse_typed_record_stream(&bytes, &heterogeneous_descriptor())
                .unwrap()
                .records,
            vec![
                (0xdf00, PropValue::U32(0x12)),
                (0xd220, PropValue::U32(1)),
                (0xdf41, PropValue::U32(1)),
                (0xd22f, PropValue::Str(String::new())),
            ]
        );
    }

    #[test]
    fn string_member_does_not_hide_a_later_numeric_member() {
        let records = vec![
            (0xd22f, PropValue::Str(String::new())),
            (0xdf41, PropValue::U32(7)),
        ];
        let descriptor = heterogeneous_descriptor();
        let bytes = typed_record_stream(&records, &descriptor).unwrap();
        assert_eq!(&bytes[4..7], &[1, 0, 0]);
        assert_eq!(
            parse_typed_record_stream(&bytes, &descriptor)
                .unwrap()
                .records,
            records
        );
    }

    #[test]
    fn signed_member_sign_extends_a_narrow_negative_value() {
        let descriptor = RecordStreamDescriptor::new(
            RecordStreamLayout::D212,
            [(0x5010, RecordValueEncoding::Signed { width: 4 })],
        )
        .unwrap();
        let bytes = typed_record_stream(&[(0x5010, PropValue::I16(-333))], &descriptor).unwrap();

        assert_eq!(bytes, [0x01, 0x00, 0x10, 0x50, 0xb3, 0xfe, 0xff, 0xff]);
        assert_eq!(
            parse_typed_record_stream(&bytes, &descriptor)
                .unwrap()
                .records,
            vec![(0x5010, PropValue::I32(-333))]
        );
    }

    #[test]
    fn typed_stream_rejects_every_negative_signed_source_width() {
        let values = [
            (PropValue::I8(-1), -1),
            (PropValue::I8(i8::MIN), i64::from(i8::MIN)),
            (PropValue::I16(-1), -1),
            (PropValue::I16(i16::MIN), i64::from(i16::MIN)),
            (PropValue::I32(-1), -1),
            (PropValue::I32(i32::MIN), i64::from(i32::MIN)),
            (PropValue::I64(-1), -1),
            (PropValue::I64(i64::MIN), i64::MIN),
        ];

        for width in [1, 2, 4] {
            let descriptor = fixed_descriptor(width);
            let encoding = RecordValueEncoding::Fixed { width };
            for (value, expected) in &values {
                assert!(!encoding.accepts_value(value));
                match typed_record_stream(&[(0xd100, value.clone())], &descriptor) {
                    Err(RecordStreamError::NegativeFixedValue {
                        code,
                        value,
                        width: actual_width,
                    }) => assert_eq!((code, value, actual_width), (0xd100, *expected, width)),
                    other => panic!("expected negative fixed-value error, got {other:?}"),
                }
            }
        }
    }

    #[test]
    fn typed_stream_accepts_nonnegative_signed_values_independent_of_source_width() {
        let zero_values = [
            PropValue::I8(0),
            PropValue::I16(0),
            PropValue::I32(0),
            PropValue::I64(0),
        ];
        for width in [1, 2, 4] {
            let descriptor = fixed_descriptor(width);
            let encoding = RecordValueEncoding::Fixed { width };
            for value in &zero_values {
                assert!(encoding.accepts_value(value));
                let bytes = typed_record_stream(&[(0xd100, value.clone())], &descriptor).unwrap();
                assert_eq!(
                    parse_typed_record_stream(&bytes, &descriptor)
                        .unwrap()
                        .records,
                    vec![(0xd100, PropValue::U32(0))]
                );
            }
        }

        for (width, value, expected) in [
            (1, PropValue::I16(i16::from(u8::MAX)), u32::from(u8::MAX)),
            (2, PropValue::I32(i32::from(u16::MAX)), u32::from(u16::MAX)),
            (4, PropValue::I64(i64::from(u32::MAX)), u32::MAX),
        ] {
            let descriptor = fixed_descriptor(width);
            let bytes = typed_record_stream(&[(0xd100, value)], &descriptor).unwrap();
            assert_eq!(
                parse_typed_record_stream(&bytes, &descriptor)
                    .unwrap()
                    .records,
                vec![(0xd100, PropValue::U32(expected))]
            );
        }
    }

    #[test]
    fn typed_stream_rejects_positive_values_above_each_declared_width() {
        for (width, value, expected) in [
            (1, PropValue::I16(0x100), 0x100),
            (2, PropValue::I32(0x1_0000), 0x1_0000),
            (4, PropValue::I64(0x1_0000_0000), 0x1_0000_0000),
        ] {
            let descriptor = fixed_descriptor(width);
            assert!(RecordValueEncoding::Fixed { width }.accepts_value(&value));
            assert!(matches!(
                typed_record_stream(&[(0xd100, value)], &descriptor),
                Err(RecordStreamError::Overflow {
                    field: "value",
                    value,
                    width: actual_width,
                }) if value == expected && actual_width == width
            ));
        }
    }

    #[test]
    fn typed_stream_rejects_full_width_signed_and_unsigned_overflow() {
        let descriptor = fixed_descriptor(4);
        let too_large = 0x1_0000_0005;

        for value in [PropValue::I64(too_large as i64), PropValue::U64(too_large)] {
            assert!(matches!(
                typed_record_stream(&[(0xd100, value)], &descriptor),
                Err(RecordStreamError::Overflow {
                    field: "value",
                    value: 0x1_0000_0005,
                    width: 4,
                })
            ));
        }
    }

    #[test]
    fn typed_stream_skips_undeclared_member_in_fixed_framing() {
        let descriptor = heterogeneous_descriptor();
        let bytes = [
            0x03, 0x00, // count
            0x00, 0xdf, 0x12, 0x00, 0x00, 0x00, // declared fixed member
            0x34, 0x12, 0xef, 0xbe, 0xad, 0xde, // undeclared fixed member
            0x41, 0xdf, 0x01, 0x00, 0x00, 0x00, // declared fixed member
        ];

        let decoded = parse_typed_record_stream(&bytes, &descriptor).unwrap();
        assert_eq!(
            decoded.records,
            vec![(0xdf00, PropValue::U32(0x12)), (0xdf41, PropValue::U32(1)),]
        );
        assert_eq!(
            decoded.diagnostics,
            vec![RecordStreamDiagnostic::SkippedUndeclaredMember {
                code: 0x1234,
                value: 0xdead_beef,
            }]
        );
    }

    #[test]
    fn typed_stream_rejects_undeclared_member_before_inconsistent_string_tail() {
        let descriptor = heterogeneous_descriptor();
        let bytes = [
            0x02, 0x00, // count
            0x34, 0x12, 0xef, 0xbe, 0xad, 0xde, // undeclared fixed member
            0x2f, 0xd2, 0x02, 0x00, 0x00, // declared PTP string missing one code unit
        ];

        assert!(matches!(
            parse_typed_record_stream(&bytes, &descriptor),
            Err(RecordStreamError::UndeclaredMember { code: 0x1234 })
        ));
    }

    #[test]
    fn typed_stream_rejects_truncated_member() {
        let descriptor = heterogeneous_descriptor();
        let truncated = [1, 0, 0x41, 0xdf, 1, 0, 0];
        assert!(matches!(
            parse_typed_record_stream(&truncated, &descriptor),
            Err(RecordStreamError::Decode(DecodeError::UnexpectedEof { .. }))
        ));
    }
}
