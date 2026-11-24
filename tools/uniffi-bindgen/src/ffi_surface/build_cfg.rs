//! Early-warning scan for build-dependent `cfg` in the FFI crate's source.
//!
//! The PR-time surface snapshot is read from a host dev-profile build. Swift
//! bindings are generated from the iOS device release build. An export,
//! field, or case gated on the target or profile can differ between the two.
//! The comparisons of the iOS staticlib (the `ffi-surface` CI job and
//! `ci/build-xcframework.sh`) are the guarantee; this scan only points at the
//! likely cause earlier and with a source location.
//!
//! It is a heuristic over source text. It flags build-dependent names in
//! `#[cfg(...)]`, `#![cfg(...)]`, the predicate of `cfg_attr`, `cfg!(...)` and
//! `cfg_select!` arms, `#[path]` and `include!` that leave `src/`, and a
//! build script. It does not see predicates forwarded through `macro_rules!`
//! fragments, custom `--cfg` names set by cargo config or `RUSTFLAGS`, or
//! macros from dependencies.
//!
//! A justified exception is written as a comment on the flagged line or the
//! line above: `// ffi-surface: allow-build-cfg <reason>`.

use std::path::{Component, Path, PathBuf};

/// The comment that accepts one flagged line. A reason must follow it.
pub const ALLOW_MARKER: &str = "ffi-surface: allow-build-cfg";

/// `cfg` names whose value differs between the host dev build and the
/// shipped target or release builds.
fn build_dependent(name: &str) -> bool {
    name.starts_with("target_")
        || matches!(
            name,
            "unix" | "windows" | "debug_assertions" | "panic" | "overflow_checks" | "miri"
        )
}

/// One flagged location: 1-based line and a description.
#[derive(Debug, PartialEq, Eq)]
pub struct Violation {
    pub line: usize,
    pub message: String,
}

/// Scan a crate directory: every `.rs` file under `src/`, plus a build script,
/// which could set build-dependent `cfg` values.
pub fn scan_crate(crate_dir: &Path) -> Result<Vec<String>, String> {
    let mut findings = Vec::new();
    let manifest_path = crate_dir.join("Cargo.toml");
    let manifest: toml::Table = std::fs::read_to_string(&manifest_path)
        .map_err(|e| format!("{}: {e}", manifest_path.display()))
        .and_then(|text| {
            toml::from_str(&text).map_err(|e| format!("{}: {e}", manifest_path.display()))
        })?;
    let build = manifest
        .get("package")
        .and_then(|package| package.get("build"));
    let has_build_script = match build {
        Some(toml::Value::Boolean(false)) => false,
        Some(_) => true,
        None => crate_dir.join("build.rs").exists(),
    };
    if has_build_script {
        findings.push(format!(
            "{}: the FFI crate has a build script, which can set build-dependent cfg values",
            crate_dir.display()
        ));
    }
    let src = crate_dir.join("src");
    let mut files = Vec::new();
    collect_rust_files(&src, &mut files)?;
    files.sort();
    for file in files {
        let text =
            std::fs::read_to_string(&file).map_err(|e| format!("{}: {e}", file.display()))?;
        let file_dir = file.parent().unwrap_or(&src);
        for violation in scan_text(&text, file_dir, &src) {
            findings.push(format!(
                "{}:{}: {}",
                file.display(),
                violation.line,
                violation.message
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

/// Scan one source file located in `file_dir`, whose crate sources live in
/// `src_root`.
pub fn scan_text(text: &str, file_dir: &Path, src_root: &Path) -> Vec<Violation> {
    let original: Vec<char> = text.chars().collect();
    let code = code_only(&original);
    let mut violations = Vec::new();
    let line_of = |pos: usize| code[..pos].iter().filter(|c| **c == '\n').count() + 1;
    let flag_predicate = |pos: usize, predicate: &[char], violations: &mut Vec<Violation>| {
        for name in identifiers(predicate) {
            if build_dependent(&name) {
                violations.push(Violation {
                    line: line_of(pos),
                    message: format!("cfg uses build-dependent `{name}`"),
                });
            }
        }
    };

    for (start, word) in words(&code) {
        let end = start + word.len();
        let attribute = after_attribute_open(&code, start);
        match word.as_str() {
            "cfg" if attribute => {
                if let Some((open, close)) = group_after(&code, end) {
                    flag_predicate(start, &code[open + 1..close], &mut violations);
                }
            }
            "cfg" => {
                let bang = skip_space(&code, end);
                if code.get(bang) == Some(&'!') {
                    if let Some((open, close)) = group_after(&code, bang + 1) {
                        flag_predicate(start, &code[open + 1..close], &mut violations);
                    }
                }
            }
            "cfg_attr" if attribute => {
                if let Some((open, close)) = group_after(&code, end) {
                    let predicate = first_argument(&code[open + 1..close]);
                    flag_predicate(start, predicate, &mut violations);
                }
            }
            "cfg_select" => {
                let bang = skip_space(&code, end);
                if code.get(bang) == Some(&'!') {
                    if let Some((open, close)) = group_after(&code, bang + 1) {
                        for predicate in select_predicates(&code[open + 1..close]) {
                            flag_predicate(start, predicate, &mut violations);
                        }
                    }
                }
            }
            "path" if attribute => {
                let eq = skip_space(&code, end);
                let target = (code.get(eq) == Some(&'='))
                    .then(|| string_literal(&original, &code, skip_space(&code, eq + 1)))
                    .flatten();
                if let Some(message) = outside_src("#[path]", target, file_dir, src_root) {
                    violations.push(Violation {
                        line: line_of(start),
                        message,
                    });
                }
            }
            "include" => {
                let bang = skip_space(&code, end);
                if code.get(bang) == Some(&'!') {
                    if let Some((open, _)) = group_after(&code, bang + 1) {
                        let target = string_literal(&original, &code, skip_space(&code, open + 1));
                        if let Some(message) = outside_src("include!", target, file_dir, src_root) {
                            violations.push(Violation {
                                line: line_of(start),
                                message,
                            });
                        }
                    }
                }
            }
            _ => {}
        }
    }

    let lines: Vec<&str> = text.lines().collect();
    let allowed = |line: usize| {
        [line, line.saturating_sub(1)].iter().any(|&n| {
            n >= 1
                && lines.get(n - 1).is_some_and(|text| {
                    text.find(ALLOW_MARKER)
                        .is_some_and(|at| !text[at + ALLOW_MARKER.len()..].trim().is_empty())
                })
        })
    };
    violations.retain(|v| !allowed(v.line));
    violations
}

/// A message when `target` (relative to `file_dir`) is outside `src_root` or
/// cannot be resolved from the source text.
fn outside_src(
    what: &str,
    target: Option<String>,
    file_dir: &Path,
    src_root: &Path,
) -> Option<String> {
    let Some(target) = target else {
        return Some(format!(
            "{what} target is not a string literal, so the scan cannot follow it"
        ));
    };
    let resolved = normalize(&file_dir.join(&target));
    (!resolved.starts_with(normalize(src_root)))
        .then(|| format!("{what} reads `{target}`, outside the crate's src/ directory"))
}

fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other),
        }
    }
    out
}

fn is_ident(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// Every identifier-like word with its start position.
fn words(code: &[char]) -> Vec<(usize, String)> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < code.len() {
        if is_ident(code[i]) && (i == 0 || !is_ident(code[i - 1])) {
            let start = i;
            while i < code.len() && is_ident(code[i]) {
                i += 1;
            }
            out.push((start, code[start..i].iter().collect()));
        } else {
            i += 1;
        }
    }
    out
}

/// True when the word at `start` directly follows `#[` or `#![`.
fn after_attribute_open(code: &[char], start: usize) -> bool {
    let mut i = start;
    let back = |i: &mut usize| -> Option<char> {
        while *i > 0 {
            *i -= 1;
            if !code[*i].is_whitespace() {
                return Some(code[*i]);
            }
        }
        None
    };
    if back(&mut i) != Some('[') {
        return false;
    }
    match back(&mut i) {
        Some('#') => true,
        Some('!') => back(&mut i) == Some('#'),
        _ => false,
    }
}

fn skip_space(code: &[char], mut cursor: usize) -> usize {
    while code.get(cursor).is_some_and(|c| c.is_whitespace()) {
        cursor += 1;
    }
    cursor
}

/// The bracketed group (`(...)`, `[...]` or `{...}`) starting at the first
/// non-space character from `cursor`, as open and close positions.
fn group_after(code: &[char], cursor: usize) -> Option<(usize, usize)> {
    let open = skip_space(code, cursor);
    if !matches!(code.get(open), Some('(' | '[' | '{')) {
        return None;
    }
    let mut depth = 0usize;
    for (offset, c) in code[open..].iter().enumerate() {
        match c {
            '(' | '[' | '{' => depth += 1,
            ')' | ']' | '}' => {
                depth -= 1;
                if depth == 0 {
                    return Some((open, open + offset));
                }
            }
            _ => {}
        }
    }
    None
}

/// The first comma-separated argument at nesting depth zero.
fn first_argument(code: &[char]) -> &[char] {
    let mut depth = 0usize;
    for (index, c) in code.iter().enumerate() {
        match c {
            '(' | '[' | '{' => depth += 1,
            ')' | ']' | '}' => depth = depth.saturating_sub(1),
            ',' if depth == 0 => return &code[..index],
            _ => {}
        }
    }
    code
}

/// The predicates of `cfg_select! { pred => { ... } pred => expr, _ => ... }`.
fn select_predicates(body: &[char]) -> Vec<&[char]> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < body.len() {
        let start = skip_space(body, i);
        let mut depth = 0usize;
        let mut arrow = None;
        let mut j = start;
        while j + 1 < body.len() {
            match body[j] {
                '(' | '[' | '{' => depth += 1,
                ')' | ']' | '}' => depth = depth.saturating_sub(1),
                '=' if depth == 0 && body[j + 1] == '>' => {
                    arrow = Some(j);
                    break;
                }
                _ => {}
            }
            j += 1;
        }
        let Some(arrow) = arrow else {
            break;
        };
        out.push(&body[start..arrow]);
        // Skip the arm: a braced block, or an expression up to a top-level comma.
        let arm = skip_space(body, arrow + 2);
        i = if body.get(arm) == Some(&'{') {
            group_after(body, arm).map_or(body.len(), |(_, close)| close + 1)
        } else {
            let mut depth = 0usize;
            let mut k = arm;
            while k < body.len() {
                match body[k] {
                    '(' | '[' | '{' => depth += 1,
                    ')' | ']' | '}' => depth = depth.saturating_sub(1),
                    ',' if depth == 0 => break,
                    _ => {}
                }
                k += 1;
            }
            k
        };
        if body.get(skip_space(body, i)) == Some(&',') {
            i = skip_space(body, i) + 1;
        }
    }
    out
}

fn identifiers(code: &[char]) -> Vec<String> {
    words(code).into_iter().map(|(_, word)| word).collect()
}

/// The contents of the plain string literal whose opening quote is at `at`.
fn string_literal(original: &[char], code: &[char], at: usize) -> Option<String> {
    if code.get(at) != Some(&'"') {
        return None;
    }
    let mut out = String::new();
    let mut i = at + 1;
    while i < original.len() {
        match original[i] {
            '\\' => {
                out.push(*original.get(i + 1)?);
                i += 2;
            }
            '"' => return Some(out),
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    None
}

/// The source with comments and the contents of string and character
/// literals replaced by spaces. Each input character maps to one output
/// character, and newlines are kept, so positions and line numbers hold.
fn code_only(chars: &[char]) -> Vec<char> {
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
                    out.extend([' ', ' ']);
                    i += 2;
                } else if chars[i] == '*' && chars.get(i + 1) == Some(&'/') {
                    depth -= 1;
                    out.extend([' ', ' ']);
                    i += 2;
                    if depth == 0 {
                        break;
                    }
                } else {
                    out.push(blank(chars[i]));
                    i += 1;
                }
            }
        } else if let Some((prefix, hashes)) = raw_string_start(chars, i) {
            // r"..", r#".."#, br#".."#, cr#".."#: blank the prefix and body.
            out.extend(std::iter::repeat_n(' ', prefix));
            i += prefix;
            while i < chars.len() {
                let closes = chars[i] == '"'
                    && chars[i + 1..]
                        .iter()
                        .take(hashes)
                        .filter(|c| **c == '#')
                        .count()
                        == hashes;
                if closes {
                    out.extend(std::iter::repeat_n(' ', hashes + 1));
                    i += hashes + 1;
                    break;
                }
                out.push(blank(chars[i]));
                i += 1;
            }
        } else if c == '"' {
            // Plain, byte (b"..") and C (c"..") strings: the prefix letter
            // has already been copied as code.
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
        } else if let Some(len) = (c == '\'').then(|| char_literal_len(chars, i)).flatten() {
            out.extend(std::iter::repeat_n(' ', len));
            i += len;
        } else {
            out.push(c);
            i += 1;
        }
    }
    out.truncate(chars.len());
    out
}

/// For a raw string literal starting at `i` (`r`, `br` or `cr` prefix), the
/// length up to and including the opening quote, and the number of `#`.
fn raw_string_start(chars: &[char], i: usize) -> Option<(usize, usize)> {
    if i > 0 && is_ident(chars[i - 1]) {
        return None;
    }
    let mut j = i;
    if matches!(chars.get(j), Some('b' | 'c')) {
        j += 1;
    }
    if chars.get(j) != Some(&'r') {
        return None;
    }
    j += 1;
    let hashes = chars[j..].iter().take_while(|c| **c == '#').count();
    (chars.get(j + hashes) == Some(&'"')).then_some((j + hashes + 1 - i, hashes))
}

/// Length of a character literal starting at `i` (`'x'`, `'\n'`, `'\''`,
/// `'\u{1F600}'`), or `None` for a lifetime or label.
fn char_literal_len(chars: &[char], i: usize) -> Option<usize> {
    match chars.get(i + 1) {
        Some('\\') => {
            let close = chars.get(i + 3..)?.iter().position(|c| *c == '\'')?;
            Some(close + 4)
        }
        Some(_) if chars.get(i + 2) == Some(&'\'') => Some(3),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(text: &str) -> Vec<(usize, String)> {
        let src = Path::new("/crate/src");
        scan_text(text, src, src)
            .into_iter()
            .map(|v| (v.line, v.message))
            .collect()
    }

    fn uses(line: usize, name: &str) -> (usize, String) {
        (line, format!("cfg uses build-dependent `{name}`"))
    }

    #[test]
    fn target_gated_export_is_rejected() {
        let text =
            "#[cfg(target_os = \"ios\")]\n#[uniffi::export]\npub fn ios_only() -> u8 { 1 }\n";
        assert_eq!(names(text), [uses(1, "target_os")]);
    }

    #[test]
    fn nested_and_multi_line_predicates_are_rejected() {
        let text = "fn a() {}\n#[cfg(\n    any(unix, all(windows, target_pointer_width = \"64\"))\n)]\nfn b() {}\n#![cfg(miri)]\n";
        assert_eq!(
            names(text),
            [
                uses(2, "unix"),
                uses(2, "windows"),
                uses(2, "target_pointer_width"),
                uses(6, "miri"),
            ]
        );
    }

    #[test]
    fn cfg_attr_predicate_and_cfg_macro_are_rejected() {
        let text = "#[cfg_attr(target_arch = \"aarch64\", uniffi::export)]\nfn a() {}\nfn b() -> bool { cfg!(debug_assertions) }\n";
        assert_eq!(
            names(text),
            [uses(1, "target_arch"), uses(3, "debug_assertions")]
        );
    }

    #[test]
    fn cfg_select_arms_are_rejected() {
        let text = concat!(
            "cfg_select! {\n",
            "    target_os = \"macos\" => { pub fn f() { panic!(\"x\") } }\n",
            "    _ => {}\n",
            "}\n",
            "const X: u8 = core::cfg_select! { unix => 1, _ => 2 };\n",
            "const Y: u8 = std::cfg_select!(windows => { 1 } _ => { 2 });\n",
        );
        assert_eq!(
            names(text),
            [uses(1, "target_os"), uses(5, "unix"), uses(6, "windows")]
        );
    }

    #[test]
    fn build_independent_uses_are_allowed() {
        let text = concat!(
            "#[cfg(test)]\nmod tests {}\n",
            "#[cfg(feature = \"unix\")]\nfn a() {}\n",
            "#[cfg(not(test))]\nfn b() {}\n",
            "#[cfg_attr(test, allow(clippy::panic))]\nfn c() {}\n",
            "fn cfg(windows: u8) -> u8 { windows }\n",
            "#[doc(cfg(target_os = \"ios\"))]\nfn d() {}\n",
        );
        assert_eq!(names(text), []);
    }

    #[test]
    fn comments_strings_and_literals_are_ignored() {
        let text = concat!(
            "// #[cfg(target_os = \"ios\")]\n",
            "/* #[cfg(unix)] /* nested */ #[cfg(windows)] */\n",
            "const A: &str = \"#[cfg(target_os)]\";\n",
            "const B: &str = r#\"#[cfg(target_arch)] \"quoted\" \"#;\n",
            "const C: char = '\"';\n",
            "const D: char = '\\'';\n",
            "fn e<'a>(x: &'a str) -> &'a str { x }\n",
            "#[cfg(target_family = \"wasm\")]\n",
            "fn f() {}\n",
        );
        assert_eq!(names(text), [uses(8, "target_family")]);
    }

    #[test]
    fn c_string_literals_keep_the_scan_in_step() {
        let text = concat!(
            "const C: &core::ffi::CStr = cr#\"a\"b\"#;\n",
            "#[cfg(target_os = \"ios\")]\npub fn f() {}\n",
            "const D: &core::ffi::CStr = c\"x\\\"y\";\n",
            "#[cfg(unix)]\npub fn g() {}\n",
            "const E: &[u8] = br##\"\"#\"##;\n",
            "#[cfg(windows)]\npub fn h() {}\n",
        );
        assert_eq!(
            names(text),
            [uses(2, "target_os"), uses(5, "unix"), uses(8, "windows")]
        );
    }

    #[test]
    fn path_and_include_outside_src_are_rejected() {
        let text = concat!(
            "#[path = \"../outside/ios.rs\"]\nmod ios;\n",
            "#[path = \"nested/inner.rs\"]\nmod inner;\n",
            "include!(\"../gen/ios.rs\");\n",
            "include!(\"generated.rs\");\n",
            "include!(concat!(env!(\"OUT_DIR\"), \"/x.rs\"));\n",
            "const S: &str = include_str!(\"../README.md\");\n",
        );
        let found: Vec<usize> = names(text).into_iter().map(|(line, _)| line).collect();
        assert_eq!(found, [1, 5, 7]);
    }

    #[test]
    fn allow_marker_with_reason_accepts_one_line() {
        let text = concat!(
            "// ffi-surface: allow-build-cfg logging only, no exported item\n",
            "#[cfg(debug_assertions)]\nfn log() {}\n",
            "#[cfg(unix)] // ffi-surface: allow-build-cfg host-only test helper\n",
            "fn helper() {}\n",
            "// ffi-surface: allow-build-cfg\n",
            "#[cfg(windows)]\nfn no_reason() {}\n",
            "#[cfg(target_os = \"ios\")]\nfn not_covered() {}\n",
        );
        assert_eq!(names(text), [uses(7, "windows"), uses(9, "target_os")]);
    }

    #[test]
    fn crate_scan_reports_files_and_build_scripts() {
        let dir = std::env::temp_dir().join(format!("ffi-surface-cfg-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("src/nested")).unwrap();
        std::fs::write(
            dir.join("Cargo.toml"),
            "[package]\nname = \"x\"\nbuild = false\n",
        )
        .unwrap();
        std::fs::write(dir.join("build.rs"), "fn main() {}\n").unwrap();
        std::fs::write(dir.join("src/lib.rs"), "#[cfg(test)]\nmod tests {}\n").unwrap();
        // `build = false` disables the build script even when build.rs exists.
        assert_eq!(scan_crate(&dir).unwrap(), Vec::<String>::new());

        std::fs::write(dir.join("Cargo.toml"), "[package]\nname = \"x\"\n").unwrap();
        std::fs::write(
            dir.join("src/nested/ios.rs"),
            "\n#[cfg(target_os = \"ios\")]\nfn a() {}\n#[path = \"../../../elsewhere.rs\"]\nmod b;\n",
        )
        .unwrap();
        let findings = scan_crate(&dir).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(findings.len(), 3, "{findings:?}");
        assert!(findings[0].contains("build script"));
        assert!(findings[1].ends_with("ios.rs:2: cfg uses build-dependent `target_os`"));
        assert!(findings[2].contains("ios.rs:4: #[path] reads"));
    }
}
