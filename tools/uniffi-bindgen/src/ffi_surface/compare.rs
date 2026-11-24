//! Parse surface snapshots and label the differences between two of them.
//!
//! A snapshot is a list of items. An item is a header line at column zero
//! followed by member lines indented by two spaces. The first word of every
//! line is its kind and the second word is its name; together they form the
//! key that matches a baseline line to a current line. The rest of the line is
//! the signature. A key present in both snapshots with a different line is a
//! change.
//!
//! Every difference fails the check: the baseline must equal the current
//! surface. The severity only tells the author what the difference means for
//! code that vendors the generated bindings.

use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Item {
    pub header: String,
    pub members: Vec<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Severity {
    /// Existing consumer code that uses the item stops compiling.
    Breaking,
    /// Consumer types that conform to a generated Swift protocol or Kotlin
    /// interface (test doubles, for example) stop compiling.
    BreaksImplementers,
    /// Existing consumer code keeps compiling.
    Additive,
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
///
/// A `config` name is its whole dotted path, which may contain JSON-quoted
/// segments; it ends at the first ` = ` outside quotes. Other names are
/// Rust identifiers, optionally dotted (`uniffi_trait Store.Display`).
pub fn key(line: &str) -> String {
    let line = line.trim_start();
    let (kind, rest) = line.split_once(' ').unwrap_or((line, ""));
    if kind == "config" {
        return format!("{kind} {}", config_path(rest));
    }
    let name: String = rest
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.'))
        .collect();
    format!("{kind} {name}")
}

fn config_path(rest: &str) -> &str {
    let (mut quoted, mut escaped) = (false, false);
    for (index, c) in rest.char_indices() {
        if quoted {
            match c {
                _ if escaped => escaped = false,
                '\\' => escaped = true,
                '"' => quoted = false,
                _ => {}
            }
        } else if c == '"' {
            quoted = true;
        } else if rest[index..].starts_with(" = ") {
            return &rest[..index];
        }
    }
    rest
}

fn kind(line: &str) -> &str {
    line.split_whitespace().next().unwrap_or("")
}

/// Member kinds whose relative order is part of the consumer contract:
/// positional Kotlin record constructors and enum ordinals.
const ORDERED_MEMBER_KINDS: [&str; 2] = ["field", "case"];

const SETTING_REASON: &str =
    "Binding settings and the generator version change the generated code for every consumer.";

/// What adding a top-level item means for consumers.
fn added_item(header: &str) -> (Severity, Option<&'static str>) {
    match kind(header) {
        "config" | "generator" => (Severity::Breaking, Some(SETTING_REASON)),
        _ => (Severity::Additive, None),
    }
}

/// What adding a member to an existing item means for consumers.
/// `appended_default` is true for a record field that has a default value and
/// follows every field the baseline already has.
fn added_member(header: &str, member: &str, appended_default: bool) -> (Severity, &'static str) {
    let callback_trait = header
        .split_whitespace()
        .skip(2)
        .any(|q| q == "callback_trait");
    match (kind(header), kind(member)) {
        ("record", "field") if appended_default => (
            Severity::Additive,
            "Appended with a default: existing Swift initializer and Kotlin constructor calls keep compiling.",
        ),
        ("record", "field") => (
            Severity::Breaking,
            "Swift and Kotlin record initializers take every field without a default, and Kotlin passes fields by position.",
        ),
        ("enum", "case") => (
            Severity::Breaking,
            "Exhaustive Swift `switch` and Kotlin `when` over this enum must handle the new case.",
        ),
        ("callback_interface", "method") => (
            Severity::Breaking,
            "Foreign implementations of this interface must add the method.",
        ),
        ("object", "method") if callback_trait => (
            Severity::Breaking,
            "Foreign implementations of this trait must add the method.",
        ),
        ("object", "method") => (
            Severity::BreaksImplementers,
            "The generated Swift protocol and Kotlin interface gain this requirement; consumer types that conform to them must add it.",
        ),
        _ => (Severity::Additive, "Existing consumer code keeps compiling."),
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
            let (severity, reason) = added_item(&new.header);
            findings.push(Finding {
                severity,
                summary: format!("added `{}`", new.header),
                detail: reason
                    .map(str::to_string)
                    .into_iter()
                    .chain(new.members.iter().map(|m| format!("+ {m}")))
                    .collect(),
            });
        }
    }
    findings
}

fn compare_members(item_key: &str, old: &Item, new: &Item, findings: &mut Vec<Finding>) {
    let new_by_key: BTreeMap<String, &String> = new.members.iter().map(|m| (key(m), m)).collect();
    let old_keys: BTreeSet<String> = old.members.iter().map(|m| key(m)).collect();
    // Index in the current item of the last field the baseline already has.
    let last_kept_field = new
        .members
        .iter()
        .rposition(|m| kind(m) == "field" && old_keys.contains(&key(m)));

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

    for (index, member) in new.members.iter().enumerate() {
        if old_keys.contains(&key(member)) {
            continue;
        }
        let appended_default = kind(member) == "field"
            && member.contains(" = ")
            && last_kept_field.is_none_or(|last| index > last);
        let (severity, reason) = added_member(&new.header, member, appended_default);
        findings.push(Finding {
            severity,
            summary: format!("`{item_key}`: added `{member}`"),
            detail: vec![reason.to_string()],
        });
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
generator uniffi 0.31.1

config bindings.swift.module_name = \"CameraProtocolFFI\"

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

    fn with(findings: &[Finding], severity: Severity) -> Vec<&str> {
        findings
            .iter()
            .filter(|f| f.severity == severity)
            .map(|f| f.summary.as_str())
            .collect()
    }

    fn breaking(findings: &[Finding]) -> Vec<&str> {
        with(findings, Severity::Breaking)
    }

    fn additive(findings: &[Finding]) -> Vec<&str> {
        with(findings, Severity::Additive)
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
        assert_eq!(additive(&findings).len(), 1);
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
    fn new_case_in_non_exhaustive_enum_is_breaking() {
        // UniFFI 0.31 Swift and Kotlin templates ignore `#[non_exhaustive]`.
        let baseline = "enum Kind non_exhaustive\n  case A\n";
        let current = "enum Kind non_exhaustive\n  case A\n  case B\n";
        let findings = compare(&parse(baseline).unwrap(), &parse(current).unwrap());
        assert_eq!(breaking(&findings), ["`enum Kind`: added `case B`"]);
    }

    #[test]
    fn new_record_field_without_default_is_breaking() {
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
    fn appended_record_field_with_default_is_additive() {
        let current = BASELINE.replace(
            "  field title: Option<String>\n",
            "  field title: Option<String>\n  field role: Option<String> = None\n  field rank: u8 = 0\n",
        );
        let findings = run(&current);
        assert!(breaking(&findings).is_empty());
        assert_eq!(
            additive(&findings),
            [
                "`record Descriptor`: added `field role: Option<String> = None`",
                "`record Descriptor`: added `field rank: u8 = 0`",
            ]
        );
    }

    #[test]
    fn inserted_record_field_with_default_is_breaking() {
        let current = BASELINE.replace(
            "  field id: String\n",
            "  field role: Option<String> = None\n  field id: String\n",
        );
        assert_eq!(
            breaking(&run(&current)),
            ["`record Descriptor`: added `field role: Option<String> = None`"]
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
    fn new_method_on_rust_object_breaks_implementers() {
        let current = BASELINE.replace(
            "  method lookup(model: String) -> Option<bool>\n",
            "  method lookup(model: String) -> Option<bool>\n  method models() -> Vec<String>\n",
        );
        let findings = run(&current);
        assert!(breaking(&findings).is_empty());
        assert!(additive(&findings).is_empty());
        assert_eq!(
            with(&findings, Severity::BreaksImplementers),
            ["`object Store`: added `method models() -> Vec<String>`"]
        );
    }

    #[test]
    fn new_constructor_on_rust_object_is_additive() {
        let current = BASELINE.replace(
            "  constructor new(path: String) throws(FfiError)\n",
            "  constructor new(path: String) throws(FfiError)\n  constructor empty()\n",
        );
        assert_eq!(
            additive(&run(&current)),
            ["`object Store`: added `constructor empty()`"]
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
    fn new_top_level_item_is_additive() {
        let current = format!("{BASELINE}\nfn version() -> String\n");
        let findings = run(&current);
        assert!(breaking(&findings).is_empty());
        assert_eq!(additive(&findings), ["added `fn version() -> String`"]);
    }

    #[test]
    fn object_kind_change_is_breaking() {
        let current = BASELINE.replace("object Store\n", "object Store trait\n");
        assert_eq!(breaking(&run(&current)), ["changed `object Store`"]);
    }

    #[test]
    fn added_config_setting_is_breaking() {
        let current = BASELINE.replace(
            "config bindings.swift.module_name",
            "config bindings.swift.omit_argument_labels = true\n\nconfig bindings.swift.module_name",
        );
        let findings = run(&current);
        assert_eq!(
            breaking(&findings),
            ["added `config bindings.swift.omit_argument_labels = true`"]
        );
        assert!(additive(&findings).is_empty());
    }

    #[test]
    fn removed_config_setting_is_breaking() {
        let current = BASELINE.replace(
            "config bindings.swift.module_name = \"CameraProtocolFFI\"\n",
            "",
        );
        assert_eq!(
            breaking(&run(&current)),
            ["removed `config bindings.swift.module_name = \"CameraProtocolFFI\"`"]
        );
    }

    #[test]
    fn changed_config_setting_is_breaking() {
        let current = BASELINE.replace("\"CameraProtocolFFI\"", "\"Renamed\"");
        assert_eq!(
            breaking(&run(&current)),
            ["changed `config bindings.swift.module_name`"]
        );
    }

    #[test]
    fn generator_version_added_removed_or_changed_is_breaking() {
        let changed = BASELINE.replace("generator uniffi 0.31.1", "generator uniffi 0.32.0");
        assert_eq!(breaking(&run(&changed)), ["changed `generator uniffi`"]);

        let removed = BASELINE.replace("generator uniffi 0.31.1\n", "");
        assert_eq!(
            breaking(&run(&removed)),
            ["removed `generator uniffi 0.31.1`"]
        );

        let baseline = parse(&removed).unwrap();
        let findings = compare(&baseline, &parse(BASELINE).unwrap());
        assert_eq!(breaking(&findings), ["added `generator uniffi 0.31.1`"]);
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

    #[test]
    fn config_key_is_the_full_path_including_quoted_segments() {
        assert_eq!(
            key(r#"config bindings.swift.rename."a-b" = "x""#),
            r#"config bindings.swift.rename."a-b""#
        );
        assert_eq!(
            key(r#"config bindings.swift.rename."a = \"b\"" = "x""#),
            r#"config bindings.swift.rename."a = \"b\"""#
        );
        assert_ne!(
            key(r#"config bindings.swift.rename."a-b" = "x""#),
            key(r#"config bindings.swift.rename."a-c" = "x""#)
        );
    }
}
