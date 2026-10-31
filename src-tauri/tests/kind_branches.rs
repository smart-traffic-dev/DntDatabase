//! Ratchet against two-way `DbKind` / `SqlKind` branches.
//!
//! A branch such as `kind == DbKind::MySql` or `if let SqlKind::Postgres = kind`
//! silently routes every other kind (including future non-SQL kinds such as
//! MongoDB or Redis) into the "else" arm. Exhaustive `match` expressions are
//! required instead; no source file may contain such a branch.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

const KINDS: [&str; 2] = ["DbKind::", "SqlKind::"];

fn is_ident_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

fn starts_with_cmp(s: &str) -> bool {
    s.starts_with("==") || s.starts_with("!=")
}

fn ends_with_cmp(s: &str) -> bool {
    s.ends_with("==") || s.ends_with("!=")
}

/// Returns true when `line` contains a two-way `DbKind` / `SqlKind` branch.
fn offends(line: &str) -> bool {
    let trimmed = line.trim();
    if trimmed.starts_with("//") {
        return false;
    }
    for kind in KINDS {
        if !trimmed.contains(kind) {
            continue;
        }
        if trimmed.contains(&format!("if let {kind}")) || trimmed.contains("matches!(") {
            return true;
        }
        for (pos, _) in trimmed.match_indices(kind) {
            // `== K::` / `!= K::`
            if ends_with_cmp(trimmed[..pos].trim_end()) {
                return true;
            }
            // `K::<Variant> ==` / `K::<Variant> !=`
            let rest = &trimmed[pos + kind.len()..];
            let after_variant = rest.trim_start_matches(is_ident_char);
            if starts_with_cmp(after_variant.trim_start()) {
                return true;
            }
        }
    }
    false
}

fn workspace_root() -> PathBuf {
    PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/.."))
}

fn collect_rs_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let entries = fs::read_dir(dir).unwrap_or_else(|err| panic!("cannot read {dir:?}: {err}"));
    for entry in entries {
        let entry = entry.unwrap_or_else(|err| panic!("cannot read entry in {dir:?}: {err}"));
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        if path.is_dir() {
            if name != "tests" {
                collect_rs_files(&path, out);
            }
        } else if name.ends_with(".rs") && !name.contains("tests") {
            out.push(path);
        }
    }
}

fn relative(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

/// True when the `#[cfg(test)]` at `lines[at]` gates a module (possibly after
/// further attributes), which means the rest of the file is test code.
fn gates_test_module(lines: &[&str], at: usize) -> bool {
    lines[at + 1..]
        .iter()
        .map(|line| line.trim())
        .find(|line| !line.starts_with("#["))
        .is_some_and(|line| {
            ["mod ", "pub mod ", "pub(crate) mod "]
                .iter()
                .any(|prefix| line.starts_with(prefix))
        })
}

/// Offending lines in a file's text as `line_number: line`, stopping at the
/// first `#[cfg(test)]` that gates a module.
fn offending_lines(text: &str) -> Vec<String> {
    let lines: Vec<&str> = text.lines().collect();
    let mut found = Vec::new();
    for (i, line) in lines.iter().enumerate() {
        if line.trim() == "#[cfg(test)]" && gates_test_module(&lines, i) {
            break;
        }
        if offends(line) {
            found.push(format!("{}: {}", i + 1, line.trim()));
        }
    }
    found
}

/// Workspace-relative path (with `/` separators) of every file containing a
/// two-way kind branch, mapped to its offending lines.
fn scan() -> BTreeMap<String, Vec<String>> {
    let root = workspace_root();
    let mut src_dirs = vec![root.join("src-tauri").join("src")];
    let crates_dir = root.join("crates");
    let crates =
        fs::read_dir(&crates_dir).unwrap_or_else(|err| panic!("cannot read {crates_dir:?}: {err}"));
    for entry in crates {
        let entry =
            entry.unwrap_or_else(|err| panic!("cannot read entry in {crates_dir:?}: {err}"));
        let src = entry.path().join("src");
        if src.is_dir() {
            src_dirs.push(src);
        }
    }
    let mut files = Vec::new();
    for dir in &src_dirs {
        collect_rs_files(dir, &mut files);
    }
    assert!(!files.is_empty(), "no source files found under {root:?}");
    files
        .iter()
        .filter_map(|path| {
            let text = fs::read_to_string(path)
                .unwrap_or_else(|err| panic!("cannot read {path:?}: {err}"));
            let lines = offending_lines(&text);
            (!lines.is_empty()).then(|| (relative(&root, path), lines))
        })
        .collect()
}

#[test]
fn offends_detects_two_way_branches() {
    for line in [
        "if kind == DbKind::MySql {",
        "    kind != SqlKind::Postgres",
        "DbKind::MySql == kind",
        "matches!(kind, DbKind::MySql)",
        "if let SqlKind::MySql = kind {",
    ] {
        assert!(offends(line), "{line}");
    }
    for line in [
        "match kind { DbKind::MySql => 1, DbKind::Postgres => 2 }",
        "let k = DbKind::MySql;",
        "// if kind == DbKind::MySql",
    ] {
        assert!(!offends(line), "{line}");
    }
}

#[test]
fn single_item_cfg_test_does_not_stop_scan() {
    let text = "#[cfg(test)] use x;\nif kind == DbKind::MySql {\n}\n";
    assert_eq!(offending_lines(text), ["2: if kind == DbKind::MySql {"]);
    let text = "#[cfg(test)]\nuse x;\nif kind == DbKind::MySql {\n}\n";
    assert_eq!(offending_lines(text), ["3: if kind == DbKind::MySql {"]);
}

#[test]
fn cfg_test_module_stops_scan() {
    let text = "#[cfg(test)]\nmod tests {\n    if kind == DbKind::MySql {}\n}\n";
    assert!(offending_lines(text).is_empty());
    let text = "#[cfg(test)]\n#[path = \"x.rs\"]\nmod t;\nif kind == DbKind::MySql {\n";
    assert!(offending_lines(text).is_empty());
    let text = "if kind == DbKind::MySql {}\n#[cfg(test)]\npub(crate) mod t {\n    a == DbKind::MySql\n}\n";
    assert_eq!(offending_lines(text), ["1: if kind == DbKind::MySql {}"]);
}

#[test]
fn no_binary_kind_branches() {
    let offending = scan();
    assert!(
        offending.is_empty(),
        "two-way DbKind/SqlKind branches must become exhaustive matches\n\
         offenders: {offending:#?}"
    );
}
