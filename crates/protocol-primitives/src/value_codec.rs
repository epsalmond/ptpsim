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

/// The width a property's value occupies on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValueWidth {
    U16,
    U32,
}

impl ValueWidth {
    pub fn bytes(self) -> u8 {
        match self {
            ValueWidth::U16 => 2,
            ValueWidth::U32 => 4,
        }
    }
}

/// Encode a resolved raw value at `width`, little-endian. Errors if `raw` exceeds
/// a `U16` width (matches the app's `valueTooLarge`). The raw value itself carries
/// all semantics (sentinels, auto-ceiling, ×100) — those are manifest data.
pub fn encode_value(raw: u32, width: ValueWidth) -> Result<Vec<u8>, FramingError> {
    let mut w = Writer::new();
    match width {
        ValueWidth::U16 => {
            if raw > u32::from(u16::MAX) {
                return Err(FramingError::ValueTooWide {
                    value: raw,
                    width: 2,
                });
            }
            w.u16(raw as u16);
        }
        ValueWidth::U32 => w.u32(raw),
    }
    Ok(w.into_vec())
}

#[cfg(test)]
mod tests {
    use super::*;

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
                width: 2
            })
        ));
    }
}
