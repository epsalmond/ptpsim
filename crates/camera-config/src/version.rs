//! Firmware version ordering — the fail-soft, data-selected comparator
//! (`camera-config.md` decision #11).
//!
//! Firmware *identity* is always the raw string (`"2.30"`, `"1.2.3"`); this
//! module is only consulted for *range* queries (the rare fw-divergent override,
//! e.g. a PIN-on-pair delta). A manufacturer-tier `versionOrder` names one of a
//! finite set of engine comparators. The default, [`VersionScheme::DottedInt`],
//! parses a dotted-integer string into a component vector and compares
//! component-wise within a manufacturer, padding the shorter operand with zeros.
//!
//! **Fail-soft is the whole point:** an unparseable version never panics and is
//! never dropped — [`compare`] returns `None` and callers fall back to exact
//! string match. Exact match is the common path and needs no parsing at all.
//!
//! **Wire-form caveat (GFX100 II, verified 2026-06):** the same camera reports its
//! firmware differently per transport — BLE GATT advertises `"02.30"`, PTP
//! `GetDeviceInfo.DeviceVersion` returns `"2.30"`. The dotted-int comparator
//! normalizes both to `[2, 30]`, so range queries are safe — but a *camera-reported*
//! firmware must be compared through this module (or otherwise normalized), never by
//! a raw `==` against a manifest string, or the BLE form would miss. Manifest
//! identity is canonically the human form (`"2.30"`).

use std::cmp::Ordering;

/// The finite set of version-ordering schemes the engine knows. Selected by data
/// (a manufacturer's `versionOrder` field); the engine implements them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum VersionScheme {
    /// `"2.30"` → `[2, 30]`, `"1.2.3"` → `[1, 2, 3]`. Dot-separated unsigned
    /// integers, variable arity, component-wise compare with zero-padding.
    #[default]
    DottedInt,
}

/// Parse a version string into a comparable component vector under `scheme`.
/// Returns `None` if any component fails to parse (→ caller uses exact match).
pub fn parse(s: &str, scheme: VersionScheme) -> Option<Vec<u64>> {
    match scheme {
        VersionScheme::DottedInt => {
            let s = s.trim();
            if s.is_empty() {
                return None;
            }
            s.split('.').map(|c| c.trim().parse::<u64>().ok()).collect()
        }
    }
}

/// Order two version strings under `scheme`. `None` means at least one operand
/// is unparseable for ordering — callers must fall back to exact-string equality
/// and must never treat `None` as a successful comparison.
pub fn compare(a: &str, b: &str, scheme: VersionScheme) -> Option<Ordering> {
    let (va, vb) = (parse(a, scheme)?, parse(b, scheme)?);
    let n = va.len().max(vb.len());
    for i in 0..n {
        let ai = va.get(i).copied().unwrap_or(0);
        let bi = vb.get(i).copied().unwrap_or(0);
        match ai.cmp(&bi) {
            Ordering::Equal => continue,
            non_eq => return Some(non_eq),
        }
    }
    Some(Ordering::Equal)
}

/// Is `v` within the half-open range `[min, max)` under `scheme`? Either bound
/// may be omitted (unbounded on that side). **Fail-soft:** if `v` is unparseable
/// for ordering, the range only matches when `v` string-equals a defined bound
/// (so an exotic version is never silently swept into a range it can't be
/// ordered against).
pub fn in_range(v: &str, min: Option<&str>, max: Option<&str>, scheme: VersionScheme) -> bool {
    let parsed_ok = parse(v, scheme).is_some();
    if !parsed_ok {
        // Exact-string fallback against whichever bounds are present.
        return min == Some(v) || max == Some(v);
    }
    let above_min = match min {
        None => true,
        Some(lo) => {
            matches!(
                compare(v, lo, scheme),
                Some(Ordering::Greater | Ordering::Equal)
            ) || lo == v
        }
    };
    let below_max = match max {
        None => true,
        Some(hi) => matches!(compare(v, hi, scheme), Some(Ordering::Less)),
    };
    above_min && below_max
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cmp::Ordering::*;

    #[test]
    fn dotted_int_parses_variable_arity() {
        assert_eq!(parse("2.30", VersionScheme::DottedInt), Some(vec![2, 30]));
        assert_eq!(
            parse("1.2.3", VersionScheme::DottedInt),
            Some(vec![1, 2, 3])
        );
        assert_eq!(parse("5", VersionScheme::DottedInt), Some(vec![5]));
    }

    #[test]
    fn component_wise_not_lexical() {
        // The classic trap: "2.30" must be > "2.9", not < (string compare fails).
        assert_eq!(compare("2.9", "2.30", VersionScheme::DottedInt), Some(Less));
        assert_eq!(
            compare("2.30", "2.9", VersionScheme::DottedInt),
            Some(Greater)
        );
        assert_eq!(
            compare("2.30", "2.30", VersionScheme::DottedInt),
            Some(Equal)
        );
    }

    #[test]
    fn zero_padding_for_differing_arity() {
        // "2.0" vs "2" -> equal (trailing zero pad); "2.0.1" > "2".
        assert_eq!(compare("2.0", "2", VersionScheme::DottedInt), Some(Equal));
        assert_eq!(
            compare("2.0.1", "2", VersionScheme::DottedInt),
            Some(Greater)
        );
        assert_eq!(
            compare("1.2.3", "1.2", VersionScheme::DottedInt),
            Some(Greater)
        );
    }

    #[test]
    fn unparseable_is_none_never_panics() {
        assert_eq!(parse("2.40a", VersionScheme::DottedInt), None);
        assert_eq!(parse("v2", VersionScheme::DottedInt), None);
        assert_eq!(parse("", VersionScheme::DottedInt), None);
        assert_eq!(compare("2.40a", "2.30", VersionScheme::DottedInt), None);
        assert_eq!(compare("2.30", "weird", VersionScheme::DottedInt), None);
    }

    #[test]
    fn range_half_open() {
        let s = VersionScheme::DottedInt;
        // [1.31, 2.0): the "fixed in 1.31, changed in 2.0" case from the plan.
        assert!(in_range("1.31", Some("1.31"), Some("2.0"), s)); // min inclusive
        assert!(in_range("1.40", Some("1.31"), Some("2.0"), s));
        assert!(!in_range("2.0", Some("1.31"), Some("2.0"), s)); // max exclusive
        assert!(!in_range("1.30", Some("1.31"), Some("2.0"), s));
    }

    #[test]
    fn range_unbounded_sides() {
        let s = VersionScheme::DottedInt;
        assert!(in_range("2.40", Some("2.40"), None, s)); // >= 2.40, no upper
        assert!(in_range("0.1", None, Some("1.0"), s)); // < 1.0, no lower
        assert!(!in_range("1.0", None, Some("1.0"), s));
    }

    #[test]
    fn range_failsoft_exact_match_for_unparseable() {
        let s = VersionScheme::DottedInt;
        // An unparseable version only matches when it equals a bound exactly.
        assert!(in_range("beta", Some("beta"), None, s));
        assert!(!in_range("beta", Some("1.0"), Some("2.0"), s));
    }
}
