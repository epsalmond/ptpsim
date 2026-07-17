//! Evaluation of the closed schema vocabularies (plan §11.2 + §11.13).
//!
//! The engine owns the semantics of transforms and encodings; the FFI layer
//! is a thin mirror that delegates here. A dispatcher implementing the same
//! grammar on another platform must match this module byte-for-byte — the
//! unit tests at the bottom are the executable spec.

use super::types::{
    AdvertByteSource, AdvertPredicate, BleAdvertSignature, Encoding, PayloadPredicate, Transform,
    MAX_PAD_RIGHT_LENGTH,
};

/// Transport-neutral facts about one observed BLE advertisement — what the
/// predicate model (§11.14) evaluates against. The FFI converts its
/// `Observation::BleAdvert` into this; a platform that can't supply a field
/// leaves it `None`/empty and predicates over it evaluate false
/// (absent-field rule).
#[derive(Debug, Clone, Default)]
pub struct BleAdvertFacts {
    pub service_uuids: Vec<String>,
    /// `(company_id, post-company-id payload)`.
    pub manufacturer_data: Option<(u16, Vec<u8>)>,
    /// `(service UUID, payload)` pairs.
    pub service_data: Vec<(String, Vec<u8>)>,
    pub local_name: Option<String>,
    pub tx_power: Option<i8>,
    /// `(AD type, as-on-air payload)` — for AD type 0xFF the payload
    /// INCLUDES the 2-byte LE company id.
    pub ad_records: Vec<(u8, Vec<u8>)>,
}

/// Match one BLE-advert signature against observed advert facts (§11.7 —
/// the caller iterates signatures in file-declaration order).
pub fn advert_matches(sig: &BleAdvertSignature, facts: &BleAdvertFacts) -> bool {
    eval_predicate(&sig.require, facts)
}

fn eval_predicate(p: &AdvertPredicate, facts: &BleAdvertFacts) -> bool {
    use AdvertPredicate as P;
    match p {
        P::All(children) => children.iter().all(|c| eval_predicate(c, facts)),
        P::Any(children) => children.iter().any(|c| eval_predicate(c, facts)),
        P::Not(inner) => !eval_predicate(inner, facts),
        P::ManufacturerData(m) => match &facts.manufacturer_data {
            None => false, // absent-field rule
            Some((company_id, payload)) => {
                if let Some(want) = m.company_id {
                    if want != *company_id {
                        return false;
                    }
                }
                payload_holds(&m.payload, payload)
            }
        },
        P::ServiceUuids { contains } => facts
            .service_uuids
            .iter()
            .any(|u| u.eq_ignore_ascii_case(contains)),
        P::ServiceData { uuid, payload } => facts
            .service_data
            .iter()
            .filter(|(u, _)| u.eq_ignore_ascii_case(uuid))
            .any(|(_, bytes)| payload_holds(payload, bytes)),
        P::LocalName(n) => match &facts.local_name {
            None => false,
            Some(name) => {
                if let Some(want) = &n.equals {
                    return name == want;
                }
                if let Some(want) = &n.prefix {
                    return name.starts_with(want);
                }
                if let Some(want) = &n.contains {
                    return name.contains(want);
                }
                false // unreachable post-validation (exactly-one-of)
            }
        },
        P::TxPower { min, max } => match facts.tx_power {
            None => false,
            Some(p) => min.is_none_or(|lo| p >= lo) && max.is_none_or(|hi| p <= hi),
        },
        P::RawAdRecord { ad_type, payload } => facts
            .ad_records
            .iter()
            .filter(|(t, _)| t == ad_type)
            .any(|(_, bytes)| payload_holds(payload, bytes)),
    }
}

fn payload_holds(p: &PayloadPredicate, bytes: &[u8]) -> bool {
    if let Some(len) = p.length {
        if bytes.len() != len {
            return false;
        }
    }
    if let Some(min) = p.min_length {
        if bytes.len() < min {
            return false;
        }
    }
    for asrt in &p.assert_byte {
        if bytes.get(asrt.index) != Some(&asrt.equals) {
            return false;
        }
    }
    for bits in &p.assert_bits {
        // Read the minimum LE width covering the mask, starting at offset.
        let width = (bits.mask.bit_width() as usize).div_ceil(8);
        let width = width.max(1);
        // checked_add: a huge `offset` would overflow `offset + width` and
        // panic under debug overflow-checks — but §11.14 requires a payload
        // too short for the read to evaluate false, never error.
        let Some(end) = bits.offset.checked_add(width) else {
            return false;
        };
        let Some(slice) = bytes.get(bits.offset..end) else {
            return false; // payload too short — absent-field rule
        };
        let mut le = [0u8; 8];
        le[..slice.len()].copy_from_slice(slice);
        if (u64::from_le_bytes(le) & bits.mask) != bits.equals {
            return false;
        }
    }
    true
}

/// Derive a matched signature's runtime-scope facts: literal `scope:`
/// entries first, then captures run through the §11.13 pipeline
/// (`source bytes → window → transform chain → encoding → string`).
/// A capture that fails anywhere is skipped (never an error).
pub fn advert_scope(sig: &BleAdvertSignature, facts: &BleAdvertFacts) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::with_capacity(sig.scope.len() + sig.capture.len());
    for (k, v) in &sig.scope {
        out.push((k.clone(), v.clone()));
    }
    for cap in &sig.capture {
        let Some(source) = capture_source_bytes(&cap.source, facts) else {
            continue;
        };
        let end = match cap.length {
            Some(l) => match cap.at.checked_add(l) {
                Some(e) => e,
                None => continue,
            },
            None => source.len(),
        };
        if cap.at > source.len() || end > source.len() {
            continue;
        }
        let Some(bytes) = apply_transforms(&source[cap.at..end], &cap.transform) else {
            continue;
        };
        if let Some(value) = decode_bytes(&bytes, cap.encoding) {
            out.push((cap.name.clone(), value));
        }
    }
    out
}

/// The encoding each advert *capture* decoded with, keyed by capture name —
/// parallel to [`advert_scope`]'s values. A later `{ captured: … }` write-back
/// re-encodes by this real encoding (§11.13) instead of guessing from the
/// scope string, which silently hex-decodes an even-length all-hex-digit ASCII
/// value. Static `scope` entries are literals with no capture encoding and are
/// omitted; a capture whose source/window/decode fails never lands in scope, so
/// seeding its encoding here is harmless (the key is never read back).
pub fn advert_capture_encodings(sig: &BleAdvertSignature) -> Vec<(String, Encoding)> {
    sig.capture
        .iter()
        .map(|cap| (cap.name.clone(), cap.encoding))
        .collect()
}

fn capture_source_bytes<'a>(
    source: &AdvertByteSource,
    facts: &'a BleAdvertFacts,
) -> Option<&'a [u8]> {
    match source {
        AdvertByteSource::ManufacturerData => facts
            .manufacturer_data
            .as_ref()
            .map(|(_, payload)| payload.as_slice()),
        AdvertByteSource::RawAdRecord { ad_type } => facts
            .ad_records
            .iter()
            .find(|(t, _)| t == ad_type)
            .map(|(_, bytes)| bytes.as_slice()),
        AdvertByteSource::ServiceData { uuid } => facts
            .service_data
            .iter()
            .find(|(u, _)| u.eq_ignore_ascii_case(uuid))
            .map(|(_, bytes)| bytes.as_slice()),
        AdvertByteSource::LocalName => facts.local_name.as_deref().map(str::as_bytes),
    }
}

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
        Transform::AppendNul => {
            let mut out = input.to_vec();
            out.push(0);
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
        Transform::PadRight { length, byte } => {
            if input.len() > *length || *length > MAX_PAD_RIGHT_LENGTH {
                return None;
            }
            let mut out = Vec::with_capacity(*length);
            out.extend_from_slice(input);
            out.resize(*length, *byte);
            Some(out)
        }
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
        Encoding::Utf8Cstring => {
            // C-string semantics: the live value ends at the first NUL; the
            // rest is fixed-width field padding the consumer must not see.
            let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
            std::str::from_utf8(&bytes[..end]).ok().map(String::from)
        }
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

/// Best-effort decode of a YAML literal value into bytes — the engine-side
/// twin of the dispatcher's `StepValue::Literal` / `BleNotifyUntil::Equals`
/// handling (the FFI delegates here):
/// * String of hex digits (optionally `0x`-prefixed, even length) → hex bytes.
/// * Other string + `utf8` encoding hint → UTF-8 bytes.
/// * Sequence of u8 numbers → bytes verbatim.
/// * Integer + a width-bearing encoding → fixed-width bytes per §11.2.
///
/// `None` for shapes outside that coverage; callers surface a tolerant-aware
/// error.
pub fn yaml_literal_to_bytes(v: &serde_yaml::Value, encoding: Option<Encoding>) -> Option<Vec<u8>> {
    use Encoding::*;
    match v {
        serde_yaml::Value::String(s) => {
            let trimmed = s.trim();
            let payload = trimmed.strip_prefix("0x").unwrap_or(trimmed);
            if payload.chars().all(|c| c.is_ascii_hexdigit()) && payload.len().is_multiple_of(2) {
                let mut out = Vec::with_capacity(payload.len() / 2);
                let bytes = payload.as_bytes();
                for chunk in bytes.chunks(2) {
                    let hi = (chunk[0] as char).to_digit(16)? as u8;
                    let lo = (chunk[1] as char).to_digit(16)? as u8;
                    out.push((hi << 4) | lo);
                }
                return Some(out);
            }
            if matches!(encoding, Some(Utf8) | Some(Utf8Cstring)) {
                return Some(s.as_bytes().to_vec());
            }
            None
        }
        serde_yaml::Value::Number(n) => {
            let n_u = n.as_u64()?;
            match encoding {
                Some(U8) => Some(vec![n_u as u8]),
                Some(U16Le) => Some((n_u as u16).to_le_bytes().to_vec()),
                Some(U16Be) => Some((n_u as u16).to_be_bytes().to_vec()),
                Some(U32) | Some(U32Le) => Some((n_u as u32).to_le_bytes().to_vec()),
                Some(U32Be) => Some((n_u as u32).to_be_bytes().to_vec()),
                _ => None,
            }
        }
        serde_yaml::Value::Sequence(seq) => {
            let mut out = Vec::with_capacity(seq.len());
            for v in seq {
                let b = v.as_u64()?;
                if b > 255 {
                    return None;
                }
                out.push(b as u8);
            }
            Some(out)
        }
        _ => None,
    }
}

/// Convert a runtime_scope string back into wire bytes for a
/// `{ captured: <name> }` write — the inverse of [`decode_bytes`]'s
/// byte-flavoured encodings. Scope carries strings (§11.2): byte captures
/// land as even-length lowercase hex, integer captures as decimal. The
/// heuristic mirrors [`yaml_literal_to_bytes`]: even-length all-hex strings
/// hex-decode (this also round-trips decimal u32 captures like `idNumber`
/// via the caller passing `encoding`), everything else is UTF-8 bytes.
/// Ambiguity caveat: a 5-char ASCII capture like `ABCDE` is odd-length so
/// it stays text, but an even-length all-hex ASCII value would hex-decode —
/// data authors should capture text with non-hex alphabets or odd lengths
/// in mind (true for every current Fuji capture).
pub fn scope_string_to_bytes(value: &str, encoding: Option<Encoding>) -> Option<Vec<u8>> {
    if let Some(enc) = encoding {
        match enc {
            Encoding::Utf8 | Encoding::Utf8Cstring | Encoding::Ascii => {
                // The host emits the bare UTF-8 bytes; the camera owns any
                // fixed-width NUL padding on the wire.
                return Some(value.as_bytes().to_vec());
            }
            Encoding::U8 => return value.parse::<u8>().ok().map(|v| vec![v]),
            Encoding::U16Le => return value.parse::<u16>().ok().map(|v| v.to_le_bytes().to_vec()),
            Encoding::U16Be => return value.parse::<u16>().ok().map(|v| v.to_be_bytes().to_vec()),
            Encoding::U32 | Encoding::U32Le => {
                return value.parse::<u32>().ok().map(|v| v.to_le_bytes().to_vec())
            }
            Encoding::U32Be => return value.parse::<u32>().ok().map(|v| v.to_be_bytes().to_vec()),
            Encoding::Bytes | Encoding::BytesRaw | Encoding::BytesLe | Encoding::BytesBe => {}
        }
    }
    let payload = value.strip_prefix("0x").unwrap_or(value);
    if !payload.is_empty()
        && payload.chars().all(|c| c.is_ascii_hexdigit())
        && payload.len().is_multiple_of(2)
    {
        let mut out = Vec::with_capacity(payload.len() / 2);
        for chunk in payload.as_bytes().chunks(2) {
            let hi = (chunk[0] as char).to_digit(16)? as u8;
            let lo = (chunk[1] as char).to_digit(16)? as u8;
            out.push((hi << 4) | lo);
        }
        return Some(out);
    }
    Some(value.as_bytes().to_vec())
}

/// Encode an unsigned integer to wire bytes at the width/order of an integer
/// `Encoding` — the spec-owned counterpart of [`decode_bytes`]'s integer arms,
/// so a value emitted here round-trips through the matching decode. `None` for a
/// non-integer encoding (text/byte-string forms have no fixed integer width).
/// Used to assemble declared chunk-frame headers (#112).
pub fn encode_uint(value: u64, encoding: Encoding) -> Option<Vec<u8>> {
    use Encoding::*;
    match encoding {
        U8 => Some(vec![value as u8]),
        U16Le => Some((value as u16).to_le_bytes().to_vec()),
        U16Be => Some((value as u16).to_be_bytes().to_vec()),
        U32 | U32Le => Some((value as u32).to_le_bytes().to_vec()),
        U32Be => Some((value as u32).to_be_bytes().to_vec()),
        Utf8 | Utf8Cstring | Ascii | Bytes | BytesRaw | BytesLe | BytesBe => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::types::{
        AdvertCapture, BitsAssertion, ByteAssertion, LocalNamePredicate, MfgDataPredicate,
    };
    use std::collections::BTreeMap;

    fn sig(require: AdvertPredicate, capture: Vec<AdvertCapture>) -> BleAdvertSignature {
        BleAdvertSignature {
            require,
            capture,
            scope: BTreeMap::new(),
            suggests: crate::index::types::SuggestsBlock {
                connection: "ble".into(),
                confidence: crate::index::types::Confidence::High,
            },
            discoverable: true,
            reconnect: None,
        }
    }

    fn mfg(company_id: Option<u16>, payload: PayloadPredicate) -> AdvertPredicate {
        AdvertPredicate::ManufacturerData(MfgDataPredicate {
            company_id,
            payload,
        })
    }

    #[test]
    fn advert_capture_encodings_keys_each_capture_by_its_encoding() {
        let cap = |name: &str, encoding| AdvertCapture {
            source: AdvertByteSource::ManufacturerData,
            at: 1,
            length: Some(4),
            transform: vec![],
            encoding,
            name: name.into(),
        };
        let s = sig(
            mfg(Some(1), PayloadPredicate::default()),
            vec![
                cap("pairingKeyBytes", Encoding::Ascii),
                cap("idNumber", Encoding::U32),
            ],
        );
        let enc: BTreeMap<_, _> = advert_capture_encodings(&s).into_iter().collect();
        assert_eq!(enc.get("pairingKeyBytes"), Some(&Encoding::Ascii));
        assert_eq!(enc.get("idNumber"), Some(&Encoding::U32));
        assert_eq!(enc.len(), 2, "only captures carry an encoding");
    }

    #[test]
    fn absent_fields_evaluate_false_and_not_inverts() {
        let facts = BleAdvertFacts::default(); // nothing observed
        let name_pred = AdvertPredicate::LocalName(LocalNamePredicate {
            prefix: Some("GFX".into()),
            ..Default::default()
        });
        assert!(!advert_matches(&sig(name_pred.clone(), vec![]), &facts));
        assert!(advert_matches(
            &sig(AdvertPredicate::Not(Box::new(name_pred)), vec![]),
            &facts
        ));
        // No mfg data observed → mfg predicate false even with no constraints.
        assert!(!advert_matches(
            &sig(mfg(None, PayloadPredicate::default()), vec![]),
            &facts
        ));
        assert!(!advert_matches(
            &sig(
                AdvertPredicate::TxPower {
                    min: Some(-100),
                    max: None
                },
                vec![]
            ),
            &facts
        ));
    }

    #[test]
    fn combinators_and_bits_assertions() {
        let facts = BleAdvertFacts {
            service_uuids: vec!["0000de00-3dd4-4255-8d62-6dc7b9bd5561".into()],
            manufacturer_data: Some((0x012D, vec![0x21, 0b0000_0110, 0x07])),
            local_name: Some("ILCE-7M4".into()),
            tx_power: Some(-59),
            ..Default::default()
        };
        // Case-insensitive service-UUID compare.
        let svc = AdvertPredicate::ServiceUuids {
            contains: "0000DE00-3DD4-4255-8D62-6DC7B9BD5561".into(),
        };
        assert!(advert_matches(&sig(svc.clone(), vec![]), &facts));
        // Bits: byte 1, mask 0x06 == 0x06 (two feature flags set).
        let bits = mfg(
            Some(0x012D),
            PayloadPredicate {
                assert_bits: vec![BitsAssertion {
                    offset: 1,
                    mask: 0x06,
                    equals: 0x06,
                }],
                ..Default::default()
            },
        );
        assert!(advert_matches(&sig(bits.clone(), vec![]), &facts));
        // any-of with a failing branch still matches; all-of with the same fails.
        let wrong_byte = mfg(
            Some(0x012D),
            PayloadPredicate {
                assert_byte: vec![ByteAssertion {
                    index: 0,
                    equals: 0x99,
                }],
                ..Default::default()
            },
        );
        assert!(advert_matches(
            &sig(
                AdvertPredicate::Any(vec![wrong_byte.clone(), bits.clone()]),
                vec![]
            ),
            &facts
        ));
        assert!(!advert_matches(
            &sig(AdvertPredicate::All(vec![wrong_byte, bits]), vec![]),
            &facts
        ));
        // Bits read past the payload end → false, not error.
        let oob = mfg(
            None,
            PayloadPredicate {
                assert_bits: vec![BitsAssertion {
                    offset: 2,
                    mask: 0xFFFF,
                    equals: 0,
                }],
                ..Default::default()
            },
        );
        assert!(!advert_matches(&sig(oob, vec![]), &facts));
        // TxPower bounds.
        assert!(advert_matches(
            &sig(
                AdvertPredicate::TxPower {
                    min: Some(-70),
                    max: Some(-50)
                },
                vec![]
            ),
            &facts
        ));
        // LocalName prefix.
        assert!(advert_matches(
            &sig(
                AdvertPredicate::LocalName(LocalNamePredicate {
                    prefix: Some("ILCE".into()),
                    ..Default::default()
                }),
                vec![]
            ),
            &facts
        ));
    }

    #[test]
    fn captures_pull_from_sources_through_transform_chains() {
        let facts = BleAdvertFacts {
            manufacturer_data: Some((0x01A9, vec![0x01, 0x34, 0x12, 0xAA])),
            local_name: Some("Canon EOS R5".into()),
            ad_records: vec![(0x21, vec![0x10, 0x20, 0x30])],
            ..Default::default()
        };
        let s = sig(
            mfg(Some(0x01A9), PayloadPredicate::default()),
            vec![
                // Canon-style: reverse 2 bytes then decode as u16-le == 0x1234 byte-swapped.
                AdvertCapture {
                    source: AdvertByteSource::ManufacturerData,
                    at: 1,
                    length: Some(2),
                    transform: vec![Transform::ReverseBytes],
                    encoding: Encoding::U16Le,
                    name: "usbId".into(),
                },
                AdvertCapture {
                    source: AdvertByteSource::RawAdRecord { ad_type: 0x21 },
                    at: 1,
                    length: None,
                    transform: vec![],
                    encoding: Encoding::Bytes,
                    name: "rawTail".into(),
                },
                AdvertCapture {
                    source: AdvertByteSource::LocalName,
                    at: 0,
                    length: Some(5),
                    transform: vec![],
                    encoding: Encoding::Ascii,
                    name: "brand".into(),
                },
                // Out-of-range window → skipped, not an error.
                AdvertCapture {
                    source: AdvertByteSource::ManufacturerData,
                    at: 10,
                    length: Some(1),
                    transform: vec![],
                    encoding: Encoding::U8,
                    name: "missing".into(),
                },
                // Absent source → skipped.
                AdvertCapture {
                    source: AdvertByteSource::ServiceData {
                        uuid: "FE2C".into(),
                    },
                    at: 0,
                    length: None,
                    transform: vec![],
                    encoding: Encoding::Bytes,
                    name: "absent".into(),
                },
            ],
        );
        let scope: BTreeMap<String, String> = advert_scope(&s, &facts).into_iter().collect();
        // [0x34, 0x12] reversed → [0x12, 0x34] → u16-le = 0x3412 = 13330.
        assert_eq!(scope.get("usbId").map(String::as_str), Some("13330"));
        assert_eq!(scope.get("rawTail").map(String::as_str), Some("2030"));
        assert_eq!(scope.get("brand").map(String::as_str), Some("Canon"));
        assert!(!scope.contains_key("missing"));
        assert!(!scope.contains_key("absent"));
    }

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
    fn append_nul_emits_one_explicit_c_string_terminator() {
        assert_eq!(
            apply_transforms(b"Pixel 8", &[Transform::AppendNul]),
            Some(b"Pixel 8\0".to_vec())
        );
    }

    #[test]
    fn pad_right_extends_to_exact_width_without_truncation() {
        let mut expected = b"SnapBridge".to_vec();
        expected.resize(32, 0);
        assert_eq!(
            apply_transforms(
                b"SnapBridge",
                &[Transform::PadRight {
                    length: 32,
                    byte: 0,
                }]
            ),
            Some(expected)
        );
        assert_eq!(
            apply_transforms(
                &[1, 2, 3],
                &[Transform::PadRight {
                    length: 3,
                    byte: 0xff,
                }]
            ),
            Some(vec![1, 2, 3])
        );
        assert!(
            apply_transforms(&[1, 2, 3, 4], &[Transform::PadRight { length: 3, byte: 0 }])
                .is_none()
        );
        assert!(
            apply_transforms(
                &[],
                &[Transform::PadRight {
                    length: usize::MAX,
                    byte: 0,
                }]
            )
            .is_none(),
            "programmatically constructed transforms must keep the same allocation ceiling"
        );
    }

    #[test]
    fn encode_uint_matches_the_declared_width_and_order() {
        // #112: chunk-frame headers are built with encode_uint; the bytes must
        // match decode_bytes' integer arms so a frame round-trips.
        assert_eq!(encode_uint(0x78, Encoding::U8), Some(vec![0x78]));
        assert_eq!(encode_uint(0xffff, Encoding::U16Le), Some(vec![0xff, 0xff]));
        assert_eq!(encode_uint(1, Encoding::U16Le), Some(vec![0x01, 0x00]));
        assert_eq!(encode_uint(1, Encoding::U16Be), Some(vec![0x00, 0x01]));
        assert_eq!(encode_uint(120, Encoding::U32Le), Some(vec![0x78, 0, 0, 0]));
        assert_eq!(encode_uint(120, Encoding::U32Be), Some(vec![0, 0, 0, 0x78]));
        // round-trips through decode_bytes.
        let bytes = encode_uint(258, Encoding::U16Le).unwrap();
        assert_eq!(
            decode_bytes(&bytes, Encoding::U16Le).as_deref(),
            Some("258")
        );
        // A non-integer encoding has no fixed width → None.
        assert_eq!(encode_uint(1, Encoding::Utf8), None);
        assert_eq!(encode_uint(1, Encoding::BytesRaw), None);
    }

    #[test]
    fn utf8_cstring_decode_stops_at_first_nul() {
        // #87: a fixed-width, NUL-padded SSID field — the live name ends at
        // the first \0; the trailing padding must not reach scope.
        let padded = b"FUJIFILM-GFX100II-0C3E\0\0\0\0\0\0\0\0\0\0";
        assert_eq!(
            decode_bytes(padded, Encoding::Utf8Cstring).as_deref(),
            Some("FUJIFILM-GFX100II-0C3E"),
        );
        // Plain utf8 leaks the padding — the exact bug #87 reports.
        assert_eq!(
            decode_bytes(padded, Encoding::Utf8).as_deref(),
            Some("FUJIFILM-GFX100II-0C3E\0\0\0\0\0\0\0\0\0\0"),
        );
        // No NUL: the whole buffer decodes, same as plain utf8.
        assert_eq!(
            decode_bytes(b"open", Encoding::Utf8Cstring).as_deref(),
            Some("open"),
        );
        // Invalid UTF-8 in the live prefix fails the round-trip (tolerant-aware).
        assert_eq!(
            decode_bytes(&[0xff, 0x00, 0x41], Encoding::Utf8Cstring),
            None
        );
        // The stripped value re-encodes to its bare UTF-8 bytes (no padding).
        assert_eq!(
            scope_string_to_bytes("FUJIFILM-GFX100II-0C3E", Some(Encoding::Utf8Cstring)).as_deref(),
            Some(&b"FUJIFILM-GFX100II-0C3E"[..]),
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
    fn assert_bits_huge_offset_is_false_not_panic() {
        // §11.14: a payload too short for the bits read evaluates false,
        // never errors. A near-usize::MAX offset must not overflow
        // offset+width (panics under debug overflow-checks before the fix).
        let pred = PayloadPredicate {
            assert_bits: vec![BitsAssertion {
                offset: usize::MAX,
                mask: 0xFF,
                equals: 0,
            }],
            ..Default::default()
        };
        assert!(!payload_holds(&pred, &[0x01, 0x02, 0x03]));
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
