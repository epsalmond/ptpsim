//! Evaluation of the closed schema vocabularies (plan §11.2 + §11.13).
//!
//! The engine owns the semantics of transforms and encodings; the FFI layer
//! is a thin mirror that delegates here. A dispatcher implementing the same
//! grammar on another platform must match this module byte-for-byte — the
//! unit tests at the bottom are the executable spec.

use super::types::{Encoding, Transform};

/// Apply a transform chain in order (§11.13). `None` when any link fails
/// (out-of-range window, integer op on > 8 bytes, wrong width for
/// `uuidFromBytes`) — the caller surfaces that as a step/capture failure.
pub fn apply_transforms(input: &[u8], chain: &[Transform]) -> Option<Vec<u8>> {
    let mut cur = input.to_vec();
    for t in chain {
        cur = apply_one(&cur, t)?;
    }
    Some(cur)
}

fn apply_one(input: &[u8], t: &Transform) -> Option<Vec<u8>> {
    match t {
        Transform::BitOr(operand) => int_op(input, |v| v | operand),
        Transform::BitAnd(operand) => int_op(input, |v| v & operand),
        Transform::Slice { at, length } => window(input, *at, *length),
        Transform::DropPrefix(n) => window(input, *n, None),
        Transform::ReverseBytes => {
            let mut out = input.to_vec();
            out.reverse();
            Some(out)
        }
        Transform::UuidFromBytes => {
            if input.len() != 16 {
                return None;
            }
            let hex: String = input.iter().fold(String::with_capacity(32), |mut s, b| {
                use std::fmt::Write;
                let _ = write!(s, "{b:02X}");
                s
            });
            let uuid = format!(
                "{}-{}-{}-{}-{}",
                &hex[0..8],
                &hex[8..12],
                &hex[12..16],
                &hex[16..20],
                &hex[20..32]
            );
            Some(uuid.into_bytes())
        }
        Transform::Bits { mask, shift } => int_op(input, |v| (v & mask) >> shift),
    }
}

/// Integer transforms read the input as a ≤ 8-byte LE integer and re-emit the
/// result at the input width (overflow bits beyond the width are dropped,
/// inherent to the fixed re-emit width).
fn int_op(input: &[u8], f: impl Fn(u64) -> u64) -> Option<Vec<u8>> {
    if input.is_empty() || input.len() > 8 {
        return None;
    }
    let mut le = [0u8; 8];
    le[..input.len()].copy_from_slice(input);
    let out = f(u64::from_le_bytes(le));
    Some(out.to_le_bytes()[..input.len()].to_vec())
}

fn window(input: &[u8], at: usize, length: Option<usize>) -> Option<Vec<u8>> {
    if at > input.len() {
        return None;
    }
    let end = match length {
        Some(l) => at.checked_add(l)?,
        None => input.len(),
    };
    if end > input.len() {
        return None;
    }
    Some(input[at..end].to_vec())
}

/// Decode bytes to the string form scope carries (§11.2 — scope is always
/// strings). `None` on width/charset mismatch — capture-skip / step-failure
/// at the caller.
pub fn decode_bytes(bytes: &[u8], encoding: Encoding) -> Option<String> {
    match encoding {
        Encoding::Utf8 => std::str::from_utf8(bytes).ok().map(String::from),
        Encoding::Ascii => {
            if bytes.iter().all(u8::is_ascii) {
                std::str::from_utf8(bytes).ok().map(String::from)
            } else {
                None
            }
        }
        Encoding::Bytes | Encoding::BytesRaw | Encoding::BytesLe | Encoding::BytesBe => {
            Some(hex_lower(bytes))
        }
        Encoding::U8 => (bytes.len() == 1).then(|| (bytes[0] as u64).to_string()),
        Encoding::U16Le => (bytes.len() == 2)
            .then(|| (u16::from_le_bytes([bytes[0], bytes[1]]) as u64).to_string()),
        Encoding::U16Be => (bytes.len() == 2)
            .then(|| (u16::from_be_bytes([bytes[0], bytes[1]]) as u64).to_string()),
        Encoding::U32 | Encoding::U32Le => (bytes.len() == 4).then(|| {
            (u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as u64).to_string()
        }),
        Encoding::U32Be => (bytes.len() == 4).then(|| {
            (u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as u64).to_string()
        }),
    }
}

pub fn hex_lower(b: &[u8]) -> String {
    let mut s = String::with_capacity(b.len() * 2);
    for byte in b {
        use std::fmt::Write;
        let _ = write!(s, "{byte:02x}");
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bit_or_reads_le_and_reemits_at_input_width() {
        // The RED F557D96B echo: 4 bytes | 0x20000000.
        let input = 0x0000_1234u32.to_le_bytes();
        let out = apply_transforms(&input, &[Transform::BitOr(0x2000_0000)]).unwrap();
        assert_eq!(out, 0x2000_1234u32.to_le_bytes());
    }

    #[test]
    fn bit_ops_fail_beyond_8_bytes() {
        assert!(apply_transforms(&[0u8; 9], &[Transform::BitAnd(0xFF)]).is_none());
        assert!(apply_transforms(&[], &[Transform::BitOr(1)]).is_none());
    }

    #[test]
    fn slice_and_drop_prefix_window() {
        let input = [0xAA, 0xBB, 0xCC, 0xDD];
        assert_eq!(
            apply_transforms(
                &input,
                &[Transform::Slice {
                    at: 1,
                    length: Some(2)
                }]
            ),
            Some(vec![0xBB, 0xCC])
        );
        assert_eq!(
            apply_transforms(&input, &[Transform::DropPrefix(3)]),
            Some(vec![0xDD])
        );
        // To-end slice.
        assert_eq!(
            apply_transforms(
                &input,
                &[Transform::Slice {
                    at: 2,
                    length: None
                }]
            ),
            Some(vec![0xCC, 0xDD])
        );
        // Out-of-range fails the chain, never clamps.
        assert!(apply_transforms(
            &input,
            &[Transform::Slice {
                at: 3,
                length: Some(2)
            }]
        )
        .is_none());
        assert!(apply_transforms(&input, &[Transform::DropPrefix(5)]).is_none());
        // Empty window at the very end is legal (length 0 is rejected at
        // load; an exhausted to-end window is not).
        assert_eq!(
            apply_transforms(&input, &[Transform::DropPrefix(4)]),
            Some(vec![])
        );
    }

    #[test]
    fn reverse_bytes() {
        assert_eq!(
            apply_transforms(&[1, 2, 3], &[Transform::ReverseBytes]),
            Some(vec![3, 2, 1])
        );
    }

    #[test]
    fn uuid_from_bytes_canonical_uppercase() {
        let input: [u8; 16] = [
            0xAF, 0x85, 0x4C, 0x2E, 0xB2, 0x14, 0x45, 0x8E, 0x97, 0xE2, 0x91, 0x2C, 0x4E, 0xCF,
            0x2C, 0xB8,
        ];
        let out = apply_transforms(&input, &[Transform::UuidFromBytes]).unwrap();
        assert_eq!(
            std::str::from_utf8(&out).unwrap(),
            "AF854C2E-B214-458E-97E2-912C4ECF2CB8"
        );
        assert!(apply_transforms(&[0u8; 15], &[Transform::UuidFromBytes]).is_none());
    }

    #[test]
    fn bits_mask_and_shift() {
        // Single byte 0b1010_1100, take bits 2-3 → 0b11.
        let out = apply_transforms(
            &[0b1010_1100],
            &[Transform::Bits {
                mask: 0b0000_1100,
                shift: 2,
            }],
        )
        .unwrap();
        assert_eq!(out, vec![0b11]);
    }

    #[test]
    fn chain_applies_in_order() {
        // Nikon-style: reverse advert UUID bytes then slice — order matters.
        let input = [0x01, 0x02, 0x03, 0x04];
        let out = apply_transforms(
            &input,
            &[
                Transform::ReverseBytes,
                Transform::Slice {
                    at: 0,
                    length: Some(2),
                },
            ],
        )
        .unwrap();
        assert_eq!(out, vec![0x04, 0x03]);
    }

    #[test]
    fn empty_chain_is_identity() {
        assert_eq!(apply_transforms(&[7, 8], &[]), Some(vec![7, 8]));
    }
}
