//! Parse surface snapshots and classify the differences between two of them.
//!
//! A snapshot is a list of items. An item is a header line at column zero
//! followed by member lines indented by two spaces. The first word of every
//! line is its kind and the second word is its name; together they form the
//! key that matches a baseline line to a current line. The rest of the line is
//! the signature. A key present in both snapshots with a different line is a
//! change.

use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Item {
    pub header: String,
    pub members: Vec<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Severity {
    Breaking,
    Addition,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Finding {
    pub severity: Severity,
    pub summary: String,
    pub detail: Vec<String>,
}

/// Parse snapshot text. Blank lines and `#` comments are ignored.
pub fn parse(text: &str) -> Result<Vec<Item>, String> {
    let mut items: Vec<Item> = Vec::new();
    let mut item_keys = BTreeSet::new();
    let mut member_keys = BTreeSet::new();
    for (index, line) in text.lines().enumerate() {
        let number = index + 1;
        if line.trim().is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(member) = line.strip_prefix("  ") {
            if member.starts_with(' ') {
                return Err(format!("line {number}: members are indented by two spaces"));
            }
            let item = items
                .last_mut()
                .ok_or_else(|| format!("line {number}: member line before any item"))?;
            if !member_keys.insert(key(member)) {
                return Err(format!("line {number}: duplicate member `{}`", key(member)));
            }
            item.members.push(member.to_string());
        } else if line.starts_with(' ') {
            return Err(format!("line {number}: members are indented by two spaces"));
        } else {
            if !item_keys.insert(key(line)) {
                return Err(format!("line {number}: duplicate item `{}`", key(line)));
            }
            member_keys.clear();
            items.push(Item {
                header: line.to_string(),
                members: Vec::new(),
            });
        }
    }
    Ok(items)
}

/// The identity of a header or member line: its kind and name.
pub fn key(line: &str) -> String {
    let line = line.trim_start();
    let (kind, rest) = line.split_once(' ').unwrap_or((line, ""));
    let name: String = rest
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.'))
        .collect();
    format!("{kind} {name}")
}

fn kind(line: &str) -> &str {
    line.split_whitespace().next().unwrap_or("")
}

/// Member kinds whose additions break consumers of the item.
fn closed_member_kinds(header: &str) -> &'static [&'static str] {
    let qualifiers: Vec<&str> = header.split_whitespace().skip(2).collect();
    match kind(header) {
        "record" => &["field"],
        "enum" if !qualifiers.contains(&"non_exhaustive") => &["case"],
        "callback_interface" => &["method"],
        "object" if qualifiers.contains(&"callback_trait") => &["method"],
        _ => &[],
    }
}

/// Member kinds whose relative order is part of the consumer contract:
/// positional record initializers and enum ordinals.
const ORDERED_MEMBER_KINDS: [&str; 2] = ["field", "case"];

fn closed_reason(member_kind: &str) -> &'static str {
    match member_kind {
        "field" => "Swift and Kotlin record initializers take every field, so existing initializer calls stop compiling.",
        "case" => "Exhaustive Swift `switch` and Kotlin `when` over this enum must handle the new case.",
        "method" => "Foreign implementations of this interface must add the method.",
        _ => "Consumers of this item must change.",
    }
}

/// Compare a baseline snapshot with the current snapshot.
pub fn compare(baseline: &[Item], current: &[Item]) -> Vec<Finding> {
    let current_by_key: BTreeMap<String, &Item> = current
        .iter()
        .map(|item| (key(&item.header), item))
        .collect();
    let baseline_keys: BTreeSet<String> = baseline.iter().map(|item| key(&item.header)).collect();
    let mut findings = Vec::new();

    for old in baseline {
        let item_key = key(&old.header);
        let Some(new) = current_by_key.get(&item_key) else {
            findings.push(Finding {
                severity: Severity::Breaking,
                summary: format!("removed `{}`", old.header),
                detail: old.members.iter().map(|m| format!("- {m}")).collect(),
            });
            continue;
        };
        if old.header != new.header {
            findings.push(Finding {
                severity: Severity::Breaking,
                summary: format!("changed `{item_key}`"),
                detail: vec![format!("- {}", old.header), format!("+ {}", new.header)],
            });
        }
        compare_members(&item_key, old, new, &mut findings);
    }

    for new in current {
        if !baseline_keys.contains(&key(&new.header)) {
            findings.push(Finding {
                severity: Severity::Addition,
                summary: format!("added `{}`", new.header),
                detail: new.members.iter().map(|m| format!("+ {m}")).collect(),
            });
        }
    }
    findings
}

fn compare_members(item_key: &str, old: &Item, new: &Item, findings: &mut Vec<Finding>) {
    let new_by_key: BTreeMap<String, &String> = new.members.iter().map(|m| (key(m), m)).collect();
    let old_keys: BTreeSet<String> = old.members.iter().map(|m| key(m)).collect();
    let closed = closed_member_kinds(&new.header);

    for member in &old.members {
        match new_by_key.get(&key(member)) {
            None => findings.push(Finding {
                severity: Severity::Breaking,
                summary: format!("`{item_key}`: removed `{member}`"),
                detail: Vec::new(),
            }),
            Some(updated) if *updated != member => findings.push(Finding {
                severity: Severity::Breaking,
                summary: format!("`{item_key}`: changed `{}`", key(member)),
                detail: vec![format!("- {member}"), format!("+ {updated}")],
            }),
            Some(_) => {}
        }
    }

    for member in &new.members {
        if old_keys.contains(&key(member)) {
            continue;
        }
        let member_kind = kind(member);
        if closed.contains(&member_kind) {
            findings.push(Finding {
                severity: Severity::Breaking,
                summary: format!("`{item_key}`: added `{member}`"),
                detail: vec![closed_reason(member_kind).to_string()],
            });
        } else {
            findings.push(Finding {
                severity: Severity::Addition,
                summary: format!("`{item_key}`: added `{member}`"),
                detail: Vec::new(),
            });
        }
    }

    for ordered_kind in ORDERED_MEMBER_KINDS {
        let common = |members: &[String], other: &dyn Fn(&str) -> bool| -> Vec<String> {
            members
                .iter()
                .filter(|m| kind(m) == ordered_kind && other(&key(m)))
                .map(|m| key(m))
                .collect()
        };
        let old_order = common(&old.members, &|k| new_by_key.contains_key(k));
        let new_order = common(&new.members, &|k| old_keys.contains(k));
        if old_order != new_order {
            findings.push(Finding {
                severity: Severity::Breaking,
                summary: format!("`{item_key}`: {ordered_kind} order changed"),
                detail: vec![
                    format!("- {}", old_order.join(", ")),
                    format!("+ {}", new_order.join(", ")),
                ],
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASELINE: &str = "\
# comment
fn connect(host: String, port: u16) throws(FfiError) -> Session

record Descriptor
  field id: String
  field title: Option<String>

enum Step
  case Read(uuid: String)
  case Write(uuid: String, value: Bytes)

object Store
  constructor new(path: String) throws(FfiError)
  method lookup(model: String) -> Option<bool>

object Transport callback_trait
  method read(uuid: String) async throws(TransportError) -> Bytes
";

    fn run(current: &str) -> Vec<Finding> {
        compare(&parse(BASELINE).unwrap(), &parse(current).unwrap())
    }

    fn breaking(findings: &[Finding]) -> Vec<&str> {
        findings
            .iter()
            .filter(|f| f.severity == Severity::Breaking)
            .map(|f| f.summary.as_str())
            .collect()
    }

    fn additions(findings: &[Finding]) -> Vec<&str> {
        findings
            .iter()
            .filter(|f| f.severity == Severity::Addition)
            .map(|f| f.summary.as_str())
            .collect()
    }

    #[test]
    fn identical_snapshot_has_no_findings() {
        assert!(run(BASELINE).is_empty());
    }

    #[test]
    fn removed_function_is_breaking() {
        let current = BASELINE.replace(
            "fn connect(host: String, port: u16) throws(FfiError) -> Session\n",
            "",
        );
        assert_eq!(
            breaking(&run(&current)),
            ["removed `fn connect(host: String, port: u16) throws(FfiError) -> Session`"]
        );
    }

    #[test]
    fn renamed_function_is_a_removal_and_an_addition() {
        let current = BASELINE.replace("fn connect(", "fn open_session(");
        let findings = run(&current);
        assert_eq!(breaking(&findings).len(), 1);
        assert!(breaking(&findings)[0].starts_with("removed `fn connect("));
        assert_eq!(additions(&findings).len(), 1);
    }

    #[test]
    fn changed_function_signature_is_breaking() {
        let current = BASELINE.replace("port: u16", "port: u32");
        let findings = run(&current);
        assert_eq!(breaking(&findings), ["changed `fn connect`"]);
        assert_eq!(
            findings[0].detail,
            [
                "- fn connect(host: String, port: u16) throws(FfiError) -> Session",
                "+ fn connect(host: String, port: u32) throws(FfiError) -> Session",
            ]
        );
    }

    #[test]
    fn dropped_throws_is_breaking() {
        let current = BASELINE.replace(") throws(FfiError) -> Session", ") -> Session");
        assert_eq!(breaking(&run(&current)), ["changed `fn connect`"]);
    }

    #[test]
    fn enum_case_arity_change_is_breaking() {
        let current = BASELINE.replace(
            "case Read(uuid: String)",
            "case Read(uuid: String, timeout_ms: u32)",
        );
        assert_eq!(
            breaking(&run(&current)),
            ["`enum Step`: changed `case Read`"]
        );
    }

    #[test]
    fn new_enum_case_is_breaking() {
        let current = BASELINE.replace(
            "  case Write(uuid: String, value: Bytes)\n",
            "  case Write(uuid: String, value: Bytes)\n  case Subscribe(uuid: String)\n",
        );
        let findings = run(&current);
        assert_eq!(
            breaking(&findings),
            ["`enum Step`: added `case Subscribe(uuid: String)`"]
        );
        assert!(findings[0].detail[0].contains("switch"));
    }

    #[test]
    fn new_case_in_non_exhaustive_enum_is_an_addition() {
        let baseline = "enum Kind non_exhaustive\n  case A\n";
        let current = "enum Kind non_exhaustive\n  case A\n  case B\n";
        let findings = compare(&parse(baseline).unwrap(), &parse(current).unwrap());
        assert_eq!(additions(&findings), ["`enum Kind`: added `case B`"]);
        assert!(breaking(&findings).is_empty());
    }

    #[test]
    fn new_record_field_is_breaking() {
        let current = BASELINE.replace(
            "  field title: Option<String>\n",
            "  field title: Option<String>\n  field role: String\n",
        );
        assert_eq!(
            breaking(&run(&current)),
            ["`record Descriptor`: added `field role: String`"]
        );
    }

    #[test]
    fn record_field_reorder_is_breaking() {
        let current = BASELINE.replace(
            "  field id: String\n  field title: Option<String>\n",
            "  field title: Option<String>\n  field id: String\n",
        );
        assert_eq!(
            breaking(&run(&current)),
            ["`record Descriptor`: field order changed"]
        );
    }

    #[test]
    fn new_callback_method_is_breaking() {
        let current = format!("{BASELINE}  method write(uuid: String, value: Bytes) async\n");
        assert_eq!(
            breaking(&run(&current)),
            ["`object Transport`: added `method write(uuid: String, value: Bytes) async`"]
        );
    }

    #[test]
    fn new_method_on_rust_object_is_an_addition() {
        let current = BASELINE.replace(
            "  method lookup(model: String) -> Option<bool>\n",
            "  method lookup(model: String) -> Option<bool>\n  method models() -> Vec<String>\n",
        );
        let findings = run(&current);
        assert!(breaking(&findings).is_empty());
        assert_eq!(
            additions(&findings),
            ["`object Store`: added `method models() -> Vec<String>`"]
        );
    }

    #[test]
    fn removed_method_is_breaking() {
        let current = BASELINE.replace("  method lookup(model: String) -> Option<bool>\n", "");
        assert_eq!(
            breaking(&run(&current)),
            ["`object Store`: removed `method lookup(model: String) -> Option<bool>`"]
        );
    }

    #[test]
    fn new_top_level_item_is_an_addition() {
        let current = format!("{BASELINE}\nfn version() -> String\n");
        let findings = run(&current);
        assert!(breaking(&findings).is_empty());
        assert_eq!(additions(&findings), ["added `fn version() -> String`"]);
    }

    #[test]
    fn object_kind_change_is_breaking() {
        let current = BASELINE.replace("object Store\n", "object Store trait\n");
        assert_eq!(breaking(&run(&current)), ["changed `object Store`"]);
    }

    #[test]
    fn parse_rejects_duplicate_items_and_bad_indentation() {
        assert!(parse("fn a()\nfn a(x: u8)\n").is_err());
        assert!(parse("  field x: u8\n").is_err());
        assert!(parse("record R\n   field x: u8\n").is_err());
        assert!(parse("record R\n  field x: u8\n  field x: u16\n").is_err());
    }

    #[test]
    fn key_uses_kind_and_name() {
        assert_eq!(key("fn connect(host: String)"), "fn connect");
        assert_eq!(
            key("config bindings.swift.module_name = \"M\""),
            "config bindings.swift.module_name"
        );
        assert_eq!(key("  case Read(uuid: String)"), "case Read");
        assert_eq!(
            key("uniffi_trait Store.Display"),
            "uniffi_trait Store.Display"
        );
    }
}
