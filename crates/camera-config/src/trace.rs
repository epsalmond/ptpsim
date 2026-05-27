//! Resolution trace — the serializable "why" behind a decision (`camera-config.md`
//! §5b). The legibility primitive for fast config iteration: an app captures it into
//! telemetry, a dev/tool reads "what the manifest delivered." Pure: computed from the
//! same data, no I/O. Scoped to what the engine does *today* — the orthogonal
//! `(connection, mode)` gating + predicate evaluation; the multi-tier funnel trace
//! grows when the funnel does.

use crate::model::parse_hex_code;
use crate::predicate::{Predicate, PropView};

/// Evaluation of one predicate leaf, with the observed value that decided it.
#[derive(Debug, Clone, PartialEq)]
pub struct LeafEval {
    /// Property code as written (e.g. `"0xd212"`).
    pub prop: String,
    /// The value the client supplied for it, if any (`None` = not observed → unmet).
    pub observed: Option<i64>,
    /// `observed & mask` when a mask is present, else `observed`.
    pub effective: Option<i64>,
    /// The comparators applied, e.g. `"mask 0xff, ne 0x0"`.
    pub test: String,
    pub passed: bool,
}

/// Outcome of evaluating a whole predicate, with every leaf recorded (no
/// short-circuit — the trace shows all of them).
#[derive(Debug, Clone, PartialEq)]
pub struct PredicateOutcome {
    pub passed: bool,
    pub leaves: Vec<LeafEval>,
    /// Connective structure, e.g. `"all of 2"`, `"any of 3"`, `"leaf"`, `"not(...)"`.
    pub summary: String,
}

/// Why an `operation_available` query answered as it did.
#[derive(Debug, Clone, PartialEq)]
pub struct ResolutionTrace {
    pub query: String,
    pub connection: String,
    pub mode: String,
    pub op: u16,
    pub outcome: String,
    pub connection_ok: bool,
    pub mode_ok: bool,
    /// The `requires` prerequisite evaluation, if the op declared one.
    pub requires: Option<PredicateOutcome>,
    /// One-line human reason for the outcome.
    pub reason: String,
}

impl Predicate {
    /// Evaluate, recording every leaf (no short-circuit) — for tracing/telemetry.
    /// `eval()` remains the fast path; this is the explain path.
    pub fn explain(&self, view: &PropView) -> PredicateOutcome {
        let mut leaves = Vec::new();
        let passed = collect(self, view, &mut leaves);
        PredicateOutcome {
            passed,
            leaves,
            summary: summary(self),
        }
    }
}

fn collect(p: &Predicate, view: &PropView, leaves: &mut Vec<LeafEval>) -> bool {
    match p {
        Predicate::All { all } => {
            let mut r = true;
            for c in all {
                r = collect(c, view, leaves) && r;
            }
            r
        }
        Predicate::Any { any } => {
            let mut r = false;
            for c in any {
                r = collect(c, view, leaves) || r;
            }
            r
        }
        Predicate::Not { not } => !collect(not, view, leaves),
        Predicate::Leaf(l) => {
            let code = parse_hex_code(&l.prop);
            let observed = code.and_then(|c| view.get(c));
            let effective = observed.map(|v| l.mask.map_or(v, |m| v & m));
            let mut tests = Vec::new();
            if let Some(m) = l.mask {
                tests.push(format!("mask 0x{m:x}"));
            }
            if let Some(x) = l.eq {
                tests.push(format!("eq 0x{x:x}"));
            }
            if let Some(x) = l.ne {
                tests.push(format!("ne 0x{x:x}"));
            }
            if let Some(x) = l.lt {
                tests.push(format!("lt 0x{x:x}"));
            }
            if let Some(x) = l.gt {
                tests.push(format!("gt 0x{x:x}"));
            }
            let passed = match effective {
                None => false, // unobserved (or malformed code) → condition unmet
                Some(v) => {
                    l.eq.is_none_or(|x| v == x)
                        && l.ne.is_none_or(|x| v != x)
                        && l.lt.is_none_or(|x| v < x)
                        && l.gt.is_none_or(|x| v > x)
                }
            };
            leaves.push(LeafEval {
                prop: l.prop.clone(),
                observed,
                effective,
                test: tests.join(", "),
                passed,
            });
            passed
        }
    }
}

fn summary(p: &Predicate) -> String {
    match p {
        Predicate::All { all } => format!("all of {}", all.len()),
        Predicate::Any { any } => format!("any of {}", any.len()),
        Predicate::Not { .. } => "not(...)".to_string(),
        Predicate::Leaf(_) => "leaf".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pred(yaml: &str) -> Predicate {
        serde_yaml::from_str(yaml).unwrap()
    }

    #[test]
    fn leaf_trace_shows_observed_effective_and_test() {
        // The card-inserted-style prereq: (v & 0x00ff) != 0.
        let p = pred(r#"{ prop: "0xd212", mask: 0x00ff, ne: 0x00 }"#);
        let out = p.explain(&PropView::new().with(0xd212, 0xab00));
        assert!(!out.passed);
        let leaf = &out.leaves[0];
        assert_eq!(leaf.prop, "0xd212");
        assert_eq!(leaf.observed, Some(0xab00));
        assert_eq!(leaf.effective, Some(0x00)); // masked low byte
        assert!(leaf.test.contains("mask 0xff"));
        assert!(leaf.test.contains("ne 0x0"));
        assert!(!leaf.passed);

        let out_ok = p.explain(&PropView::new().with(0xd212, 0xab01));
        assert!(out_ok.passed);
        assert_eq!(out_ok.leaves[0].effective, Some(0x01));
    }

    #[test]
    fn unobserved_leaf_is_unmet_with_none_observed() {
        let p = pred(r#"{ prop: "0xd212", ne: 0 }"#);
        let out = p.explain(&PropView::new());
        assert!(!out.passed);
        assert_eq!(out.leaves[0].observed, None);
    }

    #[test]
    fn connective_records_all_leaves_no_short_circuit() {
        let p = pred(r#"{ all: [ { prop: "0xdf01", eq: 0x1600 }, { prop: "0xd212", ne: 0 } ] }"#);
        // First leaf fails; second must STILL be recorded (no short-circuit in trace).
        let out = p.explain(&PropView::new().with(0xdf01, 0x1400).with(0xd212, 5));
        assert_eq!(out.summary, "all of 2");
        assert_eq!(out.leaves.len(), 2);
        assert!(!out.leaves[0].passed);
        assert!(out.leaves[1].passed);
        assert!(!out.passed);
    }
}
