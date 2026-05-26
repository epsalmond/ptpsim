//! The closed predicate grammar (`camera-config.md` decision #8, §3).
//!
//! One total, side-effect-free boolean language shared by `requires` (gating
//! prerequisites) and `detect` (mode determination). It is **data, not a
//! program**: a leaf compares one property value; connectives are `all`/`any`/
//! `not`. No loops, no sequencing, no property *writes*. The engine evaluates it
//! over property values the I/O-owning client has already read (sans-io).
//!
//! Deliberately *not* an embedded script engine — making a signed, wire-driving
//! artifact Turing-complete would be unauditable, the wrong trade at this scale.
//! If a future camera needs an operator this grammar can't express, add a named
//! leaf comparator (e.g. `inRange`), never a script.

use crate::model::{parse_hex_code, HexCode};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Observed property values supplied by the client: PTP code → value. Built by
/// whoever owns I/O (the app or the simulator); the engine only reads it.
#[derive(Debug, Clone, Default)]
pub struct PropView {
    values: BTreeMap<u16, i64>,
}

impl PropView {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record an observed value for a property code.
    pub fn with(mut self, code: u16, value: i64) -> Self {
        self.values.insert(code, value);
        self
    }

    pub fn set(&mut self, code: u16, value: i64) {
        self.values.insert(code, value);
    }

    pub fn get(&self, code: u16) -> Option<i64> {
        self.values.get(&code).copied()
    }

    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }
}

impl FromIterator<(u16, i64)> for PropView {
    fn from_iter<T: IntoIterator<Item = (u16, i64)>>(iter: T) -> Self {
        PropView {
            values: iter.into_iter().collect(),
        }
    }
}

/// A node in the predicate tree. Deserializes from the YAML shapes in the plan:
/// `{prop, eq|ne|lt|gt, mask?}` (leaf), `{all: [...]}`, `{any: [...]}`,
/// `{not: ...}`. Connective variants are tried before the leaf, so their
/// distinctive keys win.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Predicate {
    All { all: Vec<Predicate> },
    Any { any: Vec<Predicate> },
    Not { not: Box<Predicate> },
    Leaf(Leaf),
}

/// A leaf: compare one property's (optionally masked) value. Each present
/// comparator must hold (conjunction); the grammar expects exactly one, but
/// evaluating several as AND keeps the operation total.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Leaf {
    /// Property code as a hex string, e.g. `"0xd212"`.
    pub prop: HexCode,
    /// Applied to the observed value (`v & mask`) before comparison.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mask: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub eq: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ne: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lt: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gt: Option<i64>,
}

impl Predicate {
    /// Evaluate against observed values. Total: a leaf over a property that was
    /// not observed is `false` (an unmet condition), never an error.
    pub fn eval(&self, view: &PropView) -> bool {
        match self {
            Predicate::All { all } => all.iter().all(|p| p.eval(view)),
            Predicate::Any { any } => any.iter().any(|p| p.eval(view)),
            Predicate::Not { not } => !not.eval(view),
            Predicate::Leaf(leaf) => leaf.eval(view),
        }
    }
}

impl Leaf {
    fn eval(&self, view: &PropView) -> bool {
        let Some(code) = parse_hex_code(&self.prop) else {
            return false; // malformed prop code can't match anything
        };
        let Some(mut v) = view.get(code) else {
            return false; // property not observed → condition unmet
        };
        if let Some(m) = self.mask {
            v &= m;
        }
        // Every present comparator must hold.
        if let Some(x) = self.eq {
            if v != x {
                return false;
            }
        }
        if let Some(x) = self.ne {
            if v == x {
                return false;
            }
        }
        if let Some(x) = self.lt {
            if v >= x {
                return false;
            }
        }
        if let Some(x) = self.gt {
            if v <= x {
                return false;
            }
        }
        // An empty leaf (no comparator) is vacuously true once the prop exists.
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(yaml: &str) -> Predicate {
        serde_yaml::from_str(yaml).expect("predicate parses")
    }

    #[test]
    fn leaf_eq_and_ne() {
        let p = parse(r#"{ prop: "0xdf01", eq: 0x1600 }"#);
        assert!(p.eval(&PropView::new().with(0xdf01, 0x1600)));
        assert!(!p.eval(&PropView::new().with(0xdf01, 0x1400)));

        let ne = parse(r#"{ prop: "0xdf01", ne: 0 }"#);
        assert!(ne.eval(&PropView::new().with(0xdf01, 0x1600)));
        assert!(!ne.eval(&PropView::new().with(0xdf01, 0)));
    }

    #[test]
    fn unobserved_property_is_unmet_not_error() {
        let p = parse(r#"{ prop: "0xd212", ne: 0 }"#);
        assert!(!p.eval(&PropView::new())); // nothing observed → false
    }

    #[test]
    fn mask_applies_before_compare() {
        // The card-inserted-style prereq from the plan: (v & 0x00ff) != 0.
        let p = parse(r#"{ prop: "0xd212", mask: 0x00ff, ne: 0x00 }"#);
        assert!(p.eval(&PropView::new().with(0xd212, 0xab01))); // low byte 0x01 != 0
        assert!(!p.eval(&PropView::new().with(0xd212, 0xab00))); // low byte 0 → false
    }

    #[test]
    fn lt_gt_bounds() {
        let p = parse(r#"{ prop: "0x5007", lt: 800, gt: 200 }"#);
        assert!(p.eval(&PropView::new().with(0x5007, 400)));
        assert!(!p.eval(&PropView::new().with(0x5007, 800))); // lt is exclusive
        assert!(!p.eval(&PropView::new().with(0x5007, 200))); // gt is exclusive
    }

    #[test]
    fn connectives_all_any_not() {
        let all =
            parse(r#"{ all: [ { prop: "0xdf01", eq: 0x1600 }, { prop: "0xd212", ne: 0 } ] }"#);
        let view = PropView::new().with(0xdf01, 0x1600).with(0xd212, 5);
        assert!(all.eval(&view));
        assert!(!all.eval(&PropView::new().with(0xdf01, 0x1600))); // second leaf unmet

        let any =
            parse(r#"{ any: [ { prop: "0xdf01", eq: 0x1600 }, { prop: "0xdf01", eq: 0x1400 } ] }"#);
        assert!(any.eval(&PropView::new().with(0xdf01, 0x1400)));
        assert!(!any.eval(&PropView::new().with(0xdf01, 0x9999)));

        let not = parse(r#"{ not: { prop: "0xdf01", eq: 0x1400 } }"#);
        assert!(not.eval(&PropView::new().with(0xdf01, 0x1600)));
        assert!(!not.eval(&PropView::new().with(0xdf01, 0x1400)));
    }

    #[test]
    fn nested_tree() {
        // all[ any[A,B], not C ]
        let p = parse(
            r#"
            all:
              - any:
                  - { prop: "0xdf01", eq: 0x1600 }
                  - { prop: "0xdf01", eq: 0x1400 }
              - not: { prop: "0xd17f", eq: 1 }
            "#,
        );
        let ok = PropView::new().with(0xdf01, 0x1400).with(0xd17f, 0);
        assert!(p.eval(&ok));
        let blocked = PropView::new().with(0xdf01, 0x1400).with(0xd17f, 1);
        assert!(!p.eval(&blocked));
    }

    #[test]
    fn round_trips_through_serde() {
        let p = parse(r#"{ prop: "0xd212", mask: 255, ne: 0 }"#);
        let yaml = serde_yaml::to_string(&p).unwrap();
        let back: Predicate = serde_yaml::from_str(&yaml).unwrap();
        assert_eq!(p, back);
    }
}
