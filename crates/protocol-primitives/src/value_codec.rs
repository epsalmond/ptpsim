//! Property value encoding (concern: turning a resolved raw value into wire bytes).
//!
//! The Fuji app's encoder is **generic and table-driven** — it writes a `rawValue`
//! at the property's declared width (u16/u32, little-endian); the per-value
//! semantics (f/2.8 → 280, ISO auto-ceiling → `0x80001900`, …) are precomputed
//! `rawValue`s in a per-property options table. That table is pure manifest data
//! (`descriptor.values` + `labels`), so the only *code* needed is this width
//! encoder. Byte-exact parity target: `FujiCameraPropertyValueWidth.encode`.

use crate::error::FramingError;
use ptp_core::Writer;

/// The width a property's value occupies on the wire, including signedness. The
/// camera's GetDevicePropDesc declares the datatype (e.g. exposure-bias `i16`,
/// standard ISO `i32`); signed values are written two's-complement, little-endian.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValueWidth {
    U8,
    U16,
    U32,
    I16,
    I32,
}

impl ValueWidth {
    pub fn bytes(self) -> u8 {
        match self {
            ValueWidth::U8 => 1,
            ValueWidth::U16 | ValueWidth::I16 => 2,
            ValueWidth::U32 | ValueWidth::I32 => 4,
        }
    }

    pub fn is_signed(self) -> bool {
        matches!(self, ValueWidth::I16 | ValueWidth::I32)
    }
}

/// Encode a resolved value at `width`, little-endian. Signed widths write the
/// two's-complement bit pattern (negative exposure-bias, ISO auto sentinels like
/// `-1`). Errors if the value is out of range for the width (matches the app's
/// `valueTooLarge`). The value itself carries all semantics (sentinels,
/// auto-ceiling, ×100) — those are manifest data.
pub fn encode_value(value: i64, width: ValueWidth) -> Result<Vec<u8>, FramingError> {
    let too_wide = || FramingError::ValueTooWide {
        value,
        width: width.bytes(),
        signed: width.is_signed(),
    };
    let mut w = Writer::new();
    match width {
        ValueWidth::U8 => {
            let v: u8 = u8::try_from(value).map_err(|_| too_wide())?;
            w.u8(v);
        }
        ValueWidth::U16 => {
            let v: u16 = u16::try_from(value).map_err(|_| too_wide())?;
            w.u16(v);
        }
        ValueWidth::U32 => {
            let v: u32 = u32::try_from(value).map_err(|_| too_wide())?;
            w.u32(v);
        }
        ValueWidth::I16 => {
            let v: i16 = i16::try_from(value).map_err(|_| too_wide())?;
            w.u16(v as u16);
        }
        ValueWidth::I32 => {
            let v: i32 = i32::try_from(value).map_err(|_| too_wide())?;
            w.u32(v as u32);
        }
    }
    Ok(w.into_vec())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn u8_scalar_and_range_are_byte_exact() {
        assert_eq!(encode_value(1, ValueWidth::U8).unwrap(), vec![0x01]);
        assert!(matches!(
            encode_value(0x100, ValueWidth::U8),
            Err(FramingError::ValueTooWide {
                value: 0x100,
                width: 1,
                signed: false,
            })
        ));
    }

    #[test]
    fn u16_le_aperture() {
        // f/2.8 = rawValue 280 = 0x0118 → LE [0x18, 0x01].
        assert_eq!(
            encode_value(280, ValueWidth::U16).unwrap(),
            vec![0x18, 0x01]
        );
    }

    #[test]
    fn u32_le_iso_including_auto_ceiling_sentinel() {
        // ISO 400 = 0x190 → LE; Auto-ceiling 6400 = 0x80001900 → LE. Both are just
        // rawValues written at u32 width; the sentinel lives in the value, not code.
        assert_eq!(
            encode_value(0x190, ValueWidth::U32).unwrap(),
            vec![0x90, 0x01, 0x00, 0x00]
        );
        assert_eq!(
            encode_value(0x8000_1900, ValueWidth::U32).unwrap(),
            vec![0x00, 0x19, 0x00, 0x80]
        );
    }

    #[test]
    fn u16_overflow_is_an_error_not_a_truncation() {
        assert!(matches!(
            encode_value(0x1_0000, ValueWidth::U16),
            Err(FramingError::ValueTooWide {
                value: 0x1_0000,
                width: 2,
                signed: false,
            })
        ));
    }

    #[test]
    fn i16_exposure_bias_writes_twos_complement() {
        // 0x5010 exposure-bias is i16 (probe). -333 milliEV (-0.3 EV) → 0xFEB3 →
        // LE [0xB3, 0xFE] — exactly the bytes in the wire capture
        // (evidence/probe …0x5010 raw contains `b3fe`).
        assert_eq!(
            encode_value(-333, ValueWidth::I16).unwrap(),
            vec![0xB3, 0xFE]
        );
        // 0 EV and a positive third both round-trip.
        assert_eq!(encode_value(0, ValueWidth::I16).unwrap(), vec![0x00, 0x00]);
        assert_eq!(
            encode_value(333, ValueWidth::I16).unwrap(),
            vec![0x4D, 0x01]
        );
    }

    #[test]
    fn i32_iso_sentinels_write_twos_complement() {
        // 0x500f standard ISO is i32 (probe). The auto sentinels -1/-2/-3 encode
        // as 0xffffffff / 0xfffffffe / 0xfffffffd — the tail of the wire capture.
        assert_eq!(
            encode_value(-1, ValueWidth::I32).unwrap(),
            vec![0xFF, 0xFF, 0xFF, 0xFF]
        );
        assert_eq!(
            encode_value(-3, ValueWidth::I32).unwrap(),
            vec![0xFD, 0xFF, 0xFF, 0xFF]
        );
        // A normal positive ISO is just the little-endian value.
        assert_eq!(
            encode_value(102400, ValueWidth::I32).unwrap(),
            vec![0x00, 0x90, 0x01, 0x00]
        );
    }

    #[test]
    fn signed_range_is_enforced() {
        // i16 rejects a value that only fits unsigned u16.
        assert!(matches!(
            encode_value(40000, ValueWidth::I16),
            Err(FramingError::ValueTooWide {
                width: 2,
                signed: true,
                ..
            })
        ));
        // unsigned u16 rejects a negative value.
        assert!(matches!(
            encode_value(-1, ValueWidth::U16),
            Err(FramingError::ValueTooWide {
                width: 2,
                signed: false,
                ..
            })
        ));
    }
}
