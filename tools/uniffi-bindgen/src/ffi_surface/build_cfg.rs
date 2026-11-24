//! Reject build-dependent `cfg` predicates in the FFI crate's library source.
//!
//! The surface snapshot is read from a host dev-profile build. Swift bindings
//! are generated from the iOS device release build, and Android libraries are
//! release builds. An export, field, or case gated on the target or profile
//! could therefore differ between the snapshot and the shipped bindings, so
//! the crate must not use such predicates at all. The scan fails closed: a
//! predicate in a test module or an unexported item is rejected too.

use std::path::{Path, PathBuf};

/// `cfg` names whose value differs between the host dev build and the
/// shipped target or release builds.
fn build_dependent(name: &str) -> bool {
    name.starts_with("target_")
        || matches!(
            name,
            "unix" | "windows" | "debug_assertions" | "panic" | "overflow_checks" | "miri"
        )
}

/// One rejected predicate: 1-based line and the offending `cfg` name.
#[derive(Debug, PartialEq, Eq)]
pub struct Violation {
    pub line: usize,
    pub name: String,
}

/// Scan a crate directory: every `.rs` file under `src/`, plus a build script,
/// which could set target-dependent `cfg` values that the source scan cannot
/// see.
pub fn scan_crate(crate_dir: &Path) -> Result<Vec<String>, String> {
    let mut findings = Vec::new();
    let manifest = crate_dir.join("Cargo.toml");
    let manifest_text =
        std::fs::read_to_string(&manifest).map_err(|e| format!("{}: {e}", manifest.display()))?;
    let declares_build = manifest_text.lines().any(|line| {
        line.trim_start().starts_with("build ") || line.trim_start().starts_with("build=")
    });
    if crate_dir.join("build.rs").exists() || declares_build {
        findings.push(format!(
            "{}: the FFI crate has a build script, which can set build-dependent cfg values",
            crate_dir.display()
        ));
    }
    let mut files = Vec::new();
    collect_rust_files(&crate_dir.join("src"), &mut files)?;
    files.sort();
    for file in files {
        let text =
            std::fs::read_to_string(&file).map_err(|e| format!("{}: {e}", file.display()))?;
        for violation in scan_text(&text) {
            findings.push(format!(
                "{}:{}: cfg uses build-dependent `{}`",
                file.display(),
                violation.line,
                violation.name
            ));
        }
    }
    Ok(findings)
}

fn collect_rust_files(dir: &Path, out: &mut Vec<PathBuf>) -> Result<(), String> {
    let entries = std::fs::read_dir(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    for entry in entries {
        let path = entry.map_err(|e| format!("{}: {e}", dir.display()))?.path();
        if path.is_dir() {
            collect_rust_files(&path, out)?;
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            out.push(path);
        }
    }
    Ok(())
}

/// Find build-dependent names inside `cfg(...)`, `cfg!(...)` and
/// `cfg_attr(...)` in Rust source.
pub fn scan_text(text: &str) -> Vec<Violation> {
    let code = code_only(text);
    let mut violations = Vec::new();
    let mut index = 0;
    while let Some(offset) = find_word(&code[index..], "cfg") {
        let start = index + offset;
        index = start + 3;
        let mut cursor = index;
        if code[cursor..].starts_with(&['_', 'a', 't', 't', 'r'][..]) {
            cursor += 5;
        }
        cursor = skip_space(&code, cursor);
        if code.get(cursor) == Some(&'!') {
            cursor = skip_space(&code, cursor + 1);
        }
        if code.get(cursor) != Some(&'(') {
            continue;
        }
        let Some(end) = matching_paren(&code, cursor) else {
            continue;
        };
        let line = code[..start].iter().filter(|c| **c == '\n').count() + 1;
        for name in identifiers(&code[cursor + 1..end]) {
            if build_dependent(&name) {
                violations.push(Violation { line, name });
            }
        }
    }
    violations
}

fn is_ident(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

fn find_word(code: &[char], word: &str) -> Option<usize> {
    let word: Vec<char> = word.chars().collect();
    (0..code.len()).find(|&i| {
        code[i..].starts_with(&word)
            && (i == 0 || !is_ident(code[i - 1]))
            && code
                .get(i + word.len())
                .is_none_or(|c| !is_ident(*c) || *c == '_')
    })
}

fn skip_space(code: &[char], mut cursor: usize) -> usize {
    while code.get(cursor).is_some_and(|c| c.is_whitespace()) {
        cursor += 1;
    }
    cursor
}

fn matching_paren(code: &[char], open: usize) -> Option<usize> {
    let mut depth = 0usize;
    for (offset, c) in code[open..].iter().enumerate() {
        match c {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(open + offset);
                }
            }
            _ => {}
        }
    }
    None
}

fn identifiers(code: &[char]) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    for c in code.iter().copied().chain(std::iter::once(' ')) {
        if is_ident(c) {
            current.push(c);
        } else if !current.is_empty() {
            out.push(std::mem::take(&mut current));
        }
    }
    out
}

/// The source with comments and the contents of string and character
/// literals replaced by spaces. Newlines are kept so line numbers hold.
fn code_only(text: &str) -> Vec<char> {
    let chars: Vec<char> = text.chars().collect();
    let mut out = Vec::with_capacity(chars.len());
    let blank = |c: char| if c == '\n' { '\n' } else { ' ' };
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        let next = chars.get(i + 1).copied();
        if c == '/' && next == Some('/') {
            while i < chars.len() && chars[i] != '\n' {
                out.push(' ');
                i += 1;
            }
        } else if c == '/' && next == Some('*') {
            let mut depth = 0usize;
            while i < chars.len() {
                if chars[i] == '/' && chars.get(i + 1) == Some(&'*') {
                    depth += 1;
                    out.push(' ');
                    out.push(' ');
                    i += 2;
                } else if chars[i] == '*' && chars.get(i + 1) == Some(&'/') {
                    depth -= 1;
                    out.push(' ');
                    out.push(' ');
                    i += 2;
                    if depth == 0 {
                        break;
                    }
                } else {
                    out.push(blank(chars[i]));
                    i += 1;
                }
            }
        } else if let Some(hashes) = raw_string_start(&chars, i) {
            // r"..." / r#"..."# / br#"..."#: skip prefix, blank to the closing quote.
            let prefix = chars[i..].iter().position(|c| *c == '"').unwrap_or(0) + 1;
            out.extend(std::iter::repeat_n(' ', prefix));
            i += prefix;
            while i < chars.len() {
                if chars[i] == '"'
                    && chars[i + 1..]
                        .iter()
                        .take(hashes)
                        .filter(|c| **c == '#')
                        .count()
                        == hashes
                {
                    out.extend(std::iter::repeat_n(' ', hashes + 1));
                    i += hashes + 1;
                    break;
                }
                out.push(blank(chars[i]));
                i += 1;
            }
        } else if c == '"' {
            out.push('"');
            i += 1;
            while i < chars.len() {
                match chars[i] {
                    '\\' => {
                        out.push(' ');
                        if let Some(escaped) = chars.get(i + 1) {
                            out.push(blank(*escaped));
                        }
                        i += 2;
                    }
                    '"' => {
                        out.push('"');
                        i += 1;
                        break;
                    }
                    other => {
                        out.push(blank(other));
                        i += 1;
                    }
                }
            }
        } else if c == '\'' && char_literal_len(&chars, i).is_some() {
            let len = char_literal_len(&chars, i).unwrap_or(1);
            out.extend(std::iter::repeat_n(' ', len));
            i += len;
        } else {
            out.push(c);
            i += 1;
        }
    }
    out
}

/// The number of `#` in a raw string literal starting at `i`, if one does.
fn raw_string_start(chars: &[char], i: usize) -> Option<usize> {
    if i > 0 && is_ident(chars[i - 1]) {
        return None;
    }
    let mut j = i;
    if chars.get(j) == Some(&'b') {
        j += 1;
    }
    if chars.get(j) != Some(&'r') {
        return None;
    }
    j += 1;
    let hashes = chars[j..].iter().take_while(|c| **c == '#').count();
    (chars.get(j + hashes) == Some(&'"')).then_some(hashes)
}

/// Length of a character literal starting at `i` (`'x'`, `'\n'`,
/// `'\u{1F600}'`), or `None` for a lifetime.
fn char_literal_len(chars: &[char], i: usize) -> Option<usize> {
    match chars.get(i + 1) {
        Some('\\') => {
            let close = chars[i + 2..].iter().position(|c| *c == '\'')?;
            Some(close + 3)
        }
        Some(_) if chars.get(i + 2) == Some(&'\'') => Some(3),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(text: &str) -> Vec<(usize, String)> {
        scan_text(text)
            .into_iter()
            .map(|v| (v.line, v.name))
            .collect()
    }

    #[test]
    fn target_gated_export_is_rejected() {
        let text =
            "#[cfg(target_os = \"ios\")]\n#[uniffi::export]\npub fn ios_only() -> u8 { 1 }\n";
        assert_eq!(names(text), [(1, "target_os".to_string())]);
    }

    #[test]
    fn nested_and_multi_line_predicates_are_rejected() {
        let text = "fn a() {}\n#[cfg(\n    any(unix, all(windows, target_pointer_width = \"64\"))\n)]\nfn b() {}\n";
        assert_eq!(
            names(text),
            [
                (2, "unix".to_string()),
                (2, "windows".to_string()),
                (2, "target_pointer_width".to_string()),
            ]
        );
    }

    #[test]
    fn cfg_attr_and_cfg_macro_are_rejected() {
        let text = "#[cfg_attr(target_arch = \"aarch64\", uniffi::export)]\nfn a() {}\nfn b() -> bool { cfg!(debug_assertions) }\n";
        assert_eq!(
            names(text),
            [
                (1, "target_arch".to_string()),
                (3, "debug_assertions".to_string())
            ]
        );
    }

    #[test]
    fn build_independent_predicates_are_allowed() {
        let text = "#[cfg(test)]\nmod tests {}\n#[cfg(feature = \"unix\")]\nfn a() {}\n#[cfg(not(test))]\nfn b() {}\n";
        assert!(names(text).is_empty());
    }

    #[test]
    fn comments_strings_and_literals_are_ignored() {
        let text = concat!(
            "// #[cfg(target_os = \"ios\")]\n",
            "/* #[cfg(unix)] /* nested */ cfg(windows) */\n",
            "const A: &str = \"cfg(target_os)\";\n",
            "const B: &str = r#\"cfg(target_arch) \"quoted\" \"#;\n",
            "const C: char = '\"';\n",
            "fn d<'a>(x: &'a str) -> &'a str { x }\n",
            "#[cfg(target_family = \"wasm\")]\n",
            "fn e() {}\n",
            "fn mycfg(target_os: u8) {}\n",
        );
        assert_eq!(names(text), [(7, "target_family".to_string())]);
    }

    #[test]
    fn crate_scan_reports_files_and_build_scripts() {
        let dir = std::env::temp_dir().join(format!("ffi-surface-cfg-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("src/nested")).unwrap();
        std::fs::write(dir.join("Cargo.toml"), "[package]\nname = \"x\"\n").unwrap();
        std::fs::write(dir.join("src/lib.rs"), "#[cfg(test)]\nmod tests {}\n").unwrap();
        assert!(scan_crate(&dir).unwrap().is_empty());

        std::fs::write(
            dir.join("src/nested/ios.rs"),
            "\n#[cfg(target_os = \"ios\")]\nfn a() {}\n",
        )
        .unwrap();
        std::fs::write(dir.join("build.rs"), "fn main() {}\n").unwrap();
        let findings = scan_crate(&dir).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(findings.len(), 2, "{findings:?}");
        assert!(findings[0].contains("build script"));
        assert!(findings[1].ends_with("ios.rs:2: cfg uses build-dependent `target_os`"));
    }
}
