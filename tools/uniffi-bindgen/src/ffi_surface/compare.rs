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
    /// Existing consumer code keeps compiling, but callers that rely on a
    /// default value may see different runtime behaviour.
    SourceCompatible,
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

const DEFAULT_REASON: &str = "Only default values or trailing defaulted parameters changed. Existing Swift and Kotlin call sites keep compiling, and callers that omit the argument get the new default. Code that refers to the function as a value (a typed Swift function reference or a Kotlin `::name` reference) may need updating.";

const METHOD_DEFAULT_REASON: &str = "Only default values of existing parameters changed. The Swift class and Kotlin interface declare the defaults; the Swift protocol and Kotlin overrides do not, so call sites and implementers keep compiling. Callers that omit the argument get the new default.";

const FIELD_DEFAULT_REASON: &str = "Only the field default changed. Existing Swift initializer and Kotlin constructor calls keep compiling, and callers that omit the field get the new default.";

/// The conformances UniFFI 0.31.1 derives for records and enums from
/// whether they reach an object (`contains_object`): Swift drops `Equatable`
/// unless the type exports `Eq`, drops `Hashable` unless it exports `Hash`,
/// and drops `Codable` (when enabled) regardless; Kotlin adds `Disposable`.
struct Conformance {
    traits: BTreeSet<String>,
    codable: bool,
}

impl Conformance {
    fn of(items: &[Item]) -> Self {
        Self {
            traits: items
                .iter()
                .map(|item| key(&item.header))
                .filter(|k| k.starts_with("uniffi_trait "))
                .collect(),
            codable: items.iter().any(|item| {
                item.header == "config bindings.swift.generate_codable_conformance = true"
            }),
        }
    }

    fn derived(&self, header: &str) -> BTreeSet<&'static str> {
        let name = header.split_whitespace().nth(1).unwrap_or("");
        let contains = header
            .split_whitespace()
            .skip(2)
            .any(|q| q == "contains_object");
        let exports = |t: &str| self.traits.contains(&format!("uniffi_trait {name}.{t}"));
        let mut out = BTreeSet::new();
        if !contains || exports("Eq") {
            out.insert("Swift `Equatable`");
        }
        if !contains || exports("Hash") {
            out.insert("Swift `Hashable`");
        }
        if !contains && self.codable {
            out.insert("Swift `Codable`");
        }
        if contains {
            out.insert("Kotlin `Disposable`");
        }
        out
    }
}

fn without_object_qualifier(header: &str) -> String {
    header
        .split_whitespace()
        .filter(|word| *word != "contains_object")
        .collect::<Vec<_>>()
        .join(" ")
}

/// What changing an item header means for consumers.
fn changed_header(
    old: &str,
    new: &str,
    old_conformance: &Conformance,
    new_conformance: &Conformance,
) -> (Severity, Option<String>) {
    if !matches!(kind(new), "record" | "enum") {
        let (severity, reason) = changed_line(old, new);
        return (severity, reason.map(str::to_string));
    }
    if without_object_qualifier(old) != without_object_qualifier(new) {
        return (Severity::Breaking, None);
    }
    let before = old_conformance.derived(old);
    let after = new_conformance.derived(new);
    let lost: Vec<&str> = before.difference(&after).copied().collect();
    let gained: Vec<&str> = after.difference(&before).copied().collect();
    let mut reason = String::from("Whether this type reaches an object changed.");
    if !lost.is_empty() {
        reason.push_str(&format!(
            " It loses {}, so consumer code that relies on them stops compiling.",
            lost.join(", ")
        ));
    }
    if !gained.is_empty() {
        reason.push_str(&format!(" It gains {}.", gained.join(", ")));
    }
    let severity = if lost.is_empty() {
        Severity::Additive
    } else {
        Severity::Breaking
    };
    (severity, Some(reason))
}

/// What changing a member line with the same key means for consumers.
fn changed_line(old: &str, new: &str) -> (Severity, Option<&'static str>) {
    match kind(new) {
        "fn" | "constructor" if defaults_only_change(old, new, true) => {
            (Severity::SourceCompatible, Some(DEFAULT_REASON))
        }
        "method" if defaults_only_change(old, new, false) => {
            (Severity::SourceCompatible, Some(METHOD_DEFAULT_REASON))
        }
        "field" if field_default_only_change(old, new) => {
            (Severity::SourceCompatible, Some(FIELD_DEFAULT_REASON))
        }
        _ => (Severity::Breaking, None),
    }
}

/// `field name: Type` or `field name: Type = default`.
fn split_default(text: &str) -> (&str, Option<&str>) {
    match text.split_once(" = ") {
        Some((declaration, default)) => (declaration, Some(default)),
        None => (text, None),
    }
}

fn field_default_only_change(old: &str, new: &str) -> bool {
    let (old_decl, _) = split_default(old);
    let (new_decl, new_default) = split_default(new);
    old_decl == new_decl && new_default.is_some()
}

/// A rendered callable: `kind name(params) suffix`.
struct Callable<'a> {
    params: Vec<(&'a str, Option<&'a str>)>,
    suffix: &'a str,
}

/// Split a callable line into its parameters and the text after them,
/// honouring string literals and nested brackets inside defaults and types.
fn parse_callable(line: &str) -> Option<Callable<'_>> {
    let open = line.find('(')?;
    let (mut depth, mut quoted, mut escaped) = (0usize, false, false);
    let mut params = Vec::new();
    let mut start = open + 1;
    for (index, c) in line.char_indices().skip_while(|(i, _)| *i < open) {
        if quoted {
            match c {
                _ if escaped => escaped = false,
                '\\' => escaped = true,
                '"' => quoted = false,
                _ => {}
            }
            continue;
        }
        match c {
            '"' => quoted = true,
            '(' | '[' | '{' | '<' => depth += 1,
            ')' | ']' | '}' | '>' if depth > 1 => depth -= 1,
            ')' if depth == 1 => {
                let last = line[start..index].trim();
                if !last.is_empty() {
                    params.push(split_default(last));
                }
                return Some(Callable {
                    params,
                    suffix: &line[index + 1..],
                });
            }
            ',' if depth == 1 => {
                params.push(split_default(line[start..index].trim()));
                start = index + 1;
            }
            _ => {}
        }
    }
    None
}

/// True when two callable lines differ only in parameter defaults, in
/// parameters that gained a default, or (when `allow_trailing`) in new
/// trailing parameters that all have defaults.
fn defaults_only_change(old: &str, new: &str, allow_trailing: bool) -> bool {
    let (Some(old), Some(new)) = (parse_callable(old), parse_callable(new)) else {
        return false;
    };
    old.suffix == new.suffix
        && (new.params.len() == old.params.len()
            || (allow_trailing && new.params.len() > old.params.len()))
        && old.params.iter().zip(&new.params).all(
            |((old_decl, old_default), (new_decl, new_default))| {
                old_decl == new_decl && (old_default.is_none() || new_default.is_some())
            },
        )
        && new.params[old.params.len()..]
            .iter()
            .all(|(_, default)| default.is_some())
}

/// Compare a baseline snapshot with the current snapshot.
pub fn compare(baseline: &[Item], current: &[Item]) -> Vec<Finding> {
    let current_by_key: BTreeMap<String, &Item> = current
        .iter()
        .map(|item| (key(&item.header), item))
        .collect();
    let baseline_keys: BTreeSet<String> = baseline.iter().map(|item| key(&item.header)).collect();
    let (old_conformance, new_conformance) = (Conformance::of(baseline), Conformance::of(current));
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
            let (severity, reason) =
                changed_header(&old.header, &new.header, &old_conformance, &new_conformance);
            findings.push(Finding {
                severity,
                summary: format!("changed `{item_key}`"),
                detail: [format!("- {}", old.header), format!("+ {}", new.header)]
                    .into_iter()
                    .chain(reason)
                    .collect(),
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
            Some(updated) if *updated != member => {
                let (severity, reason) = changed_line(member, updated);
                findings.push(Finding {
                    severity,
                    summary: format!("`{item_key}`: changed `{}`", key(member)),
                    detail: [format!("- {member}"), format!("+ {updated}")]
                        .into_iter()
                        .chain(reason.map(str::to_string))
                        .collect(),
                });
            }
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

    fn source_compatible(findings: &[Finding]) -> Vec<&str> {
        with(findings, Severity::SourceCompatible)
    }

    fn pair(baseline: &str, current: &str) -> Vec<Finding> {
        compare(&parse(baseline).unwrap(), &parse(current).unwrap())
    }

    #[test]
    fn record_gaining_an_object_reference_is_breaking() {
        let findings = pair(
            "record Descriptor\n  field id: String\n",
            "record Descriptor contains_object\n  field id: String\n  field store: Option<Store> = None\n",
        );
        assert_eq!(breaking(&findings), ["changed `record Descriptor`"]);
        assert!(findings[0].detail.iter().any(|d| d.contains("Equatable")));
    }

    #[test]
    fn enum_losing_an_object_reference_is_breaking() {
        let findings = pair(
            "enum Event error contains_object\n  case A\n",
            "enum Event error\n  case A\n",
        );
        assert_eq!(breaking(&findings), ["changed `enum Event`"]);
        assert!(findings[0].detail.last().unwrap().contains("Disposable"));
    }

    #[test]
    fn changed_parameter_default_is_source_compatible() {
        let findings = pair(
            "fn scan(timeout_ms: u32 = 100, name: String = \"a, b)\")\n",
            "fn scan(timeout_ms: u32 = 200, name: String = \"c, d)\")\n",
        );
        assert!(breaking(&findings).is_empty());
        assert_eq!(source_compatible(&findings), ["changed `fn scan`"]);
    }

    #[test]
    fn parameter_gaining_a_default_is_source_compatible() {
        let findings = pair(
            "fn scan(timeout_ms: u32)\n",
            "fn scan(timeout_ms: u32 = 100)\n",
        );
        assert_eq!(source_compatible(&findings), ["changed `fn scan`"]);
    }

    #[test]
    fn parameter_losing_a_default_is_breaking() {
        let findings = pair(
            "fn scan(timeout_ms: u32 = 100)\n",
            "fn scan(timeout_ms: u32)\n",
        );
        assert_eq!(breaking(&findings), ["changed `fn scan`"]);
    }

    #[test]
    fn trailing_defaulted_parameter_is_source_compatible() {
        let findings = pair(
            "fn scan(timeout_ms: u32) -> Map<String, u8>\n",
            "fn scan(timeout_ms: u32, retries: Option<u8> = None) -> Map<String, u8>\n",
        );
        assert_eq!(source_compatible(&findings), ["changed `fn scan`"]);

        let findings = pair(
            "object Store\n  constructor new(path: String)\n",
            "object Store\n  constructor new(path: String, mode: Mode = Mode.Read)\n",
        );
        assert_eq!(
            source_compatible(&findings),
            ["`object Store`: changed `constructor new`"]
        );
    }

    #[test]
    fn trailing_parameter_without_default_or_new_return_is_breaking() {
        let findings = pair("fn scan(a: u32)\n", "fn scan(a: u32, b: u8)\n");
        assert_eq!(breaking(&findings), ["changed `fn scan`"]);
        let findings = pair("fn scan(a: u32 = 1)\n", "fn scan(a: u32 = 2) -> u8\n");
        assert_eq!(breaking(&findings), ["changed `fn scan`"]);
        let findings = pair("fn scan(a: u32 = 1)\n", "fn scan(a: u64 = 1)\n");
        assert_eq!(breaking(&findings), ["changed `fn scan`"]);
    }

    #[test]
    fn method_default_change_is_source_compatible() {
        // UniFFI 0.31.1 writes method defaults in the Swift class and the
        // Kotlin interface; the Swift protocol and Kotlin overrides omit them,
        // so neither implementers nor call sites change.
        let findings = pair(
            "object Store\n  method lookup(model: String = \"a\", limit: u8)\n",
            "object Store\n  method lookup(model: String = \"b\", limit: u8 = 1)\n",
        );
        assert!(breaking(&findings).is_empty());
        assert_eq!(
            source_compatible(&findings),
            ["`object Store`: changed `method lookup`"]
        );
    }

    #[test]
    fn method_trailing_defaulted_parameter_is_breaking() {
        // A new parameter changes the Swift protocol requirement and the
        // Kotlin interface method that implementers override.
        let findings = pair(
            "object Store\n  method lookup(model: String)\n",
            "object Store\n  method lookup(model: String, limit: u8 = 1)\n",
        );
        assert_eq!(
            breaking(&findings),
            ["`object Store`: changed `method lookup`"]
        );
    }

    #[test]
    fn object_reference_keeps_conformances_the_type_exports() {
        let with_traits = |header: &str| {
            format!("{header}\n  field id: String\n\nuniffi_trait Descriptor.Eq\n\nuniffi_trait Descriptor.Hash\n")
        };
        let findings = pair(
            &with_traits("record Descriptor"),
            &with_traits("record Descriptor contains_object"),
        );
        assert!(breaking(&findings).is_empty());
        assert_eq!(additive(&findings), ["changed `record Descriptor`"]);
        assert!(findings[0].detail.iter().any(|d| d.contains("Disposable")));
    }

    #[test]
    fn object_reference_loses_each_unexported_conformance_separately() {
        let eq_only =
            |header: &str| format!("{header}\n  field id: String\n\nuniffi_trait Descriptor.Eq\n");
        let findings = pair(
            &eq_only("record Descriptor"),
            &eq_only("record Descriptor contains_object"),
        );
        assert_eq!(breaking(&findings), ["changed `record Descriptor`"]);
        let reason = findings[0].detail.last().unwrap();
        assert!(reason.contains("`Hashable`"), "{reason}");
        assert!(!reason.contains("`Equatable`"), "{reason}");
    }

    #[test]
    fn object_reference_loses_codable_when_generation_is_on() {
        let both = |header: &str| {
            format!(
                "config bindings.swift.generate_codable_conformance = true\n\n{header}\n  field id: String\n\n\
                 uniffi_trait Descriptor.Eq\n\nuniffi_trait Descriptor.Hash\n"
            )
        };
        let findings = pair(
            &both("record Descriptor"),
            &both("record Descriptor contains_object"),
        );
        assert_eq!(breaking(&findings), ["changed `record Descriptor`"]);
        assert!(findings[0].detail.last().unwrap().contains("`Codable`"));
    }

    #[test]
    fn field_default_change_is_source_compatible() {
        let findings = pair(
            "record R\n  field a: u8 = 1\n  field b: u8\n",
            "record R\n  field a: u8 = 2\n  field b: u8 = 0\n",
        );
        assert!(breaking(&findings).is_empty());
        assert_eq!(
            source_compatible(&findings),
            [
                "`record R`: changed `field a`",
                "`record R`: changed `field b`"
            ]
        );
        let findings = pair("record R\n  field a: u8 = 1\n", "record R\n  field a: u8\n");
        assert_eq!(breaking(&findings), ["`record R`: changed `field a`"]);
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
