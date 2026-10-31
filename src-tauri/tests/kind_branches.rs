//! Ratchet against two-way `DbKind` / `SqlKind` branches.
//!
//! A branch such as `kind == DbKind::MySql` or `if let SqlKind::Postgres = kind`
//! silently routes every other kind (including future non-SQL kinds such as
//! MongoDB or Redis) into the "else" arm. Exhaustive `match` expressions are
//! required instead. Files that still contain such branches are listed in
//! `ALLOWED`; the list may only shrink.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

/// Workspace-relative paths (with `/` separators) of files that still contain
/// two-way kind branches. Later tasks remove entries from this list.
const ALLOWED: &[&str] = &[
    "src-tauri/src/backup/dump.rs",
    "src-tauri/src/backup/restore.rs",
    "src-tauri/src/commands/data.rs",
    "src-tauri/src/ddl_edit/apply.rs",
    "src-tauri/src/ddl_edit/catalog.rs",
    "src-tauri/src/export_format.rs",
    "src-tauri/src/import_data/plan.rs",
    "src-tauri/src/import_data/run.rs",
    "src-tauri/src/object_actions.rs",
    "src-tauri/src/query_sessions.rs",
    "src-tauri/src/seed_data/plan.rs",
    "src-tauri/src/seed_data/run.rs",
    "src-tauri/src/seed_data/target.rs",
    "src-tauri/src/table_design/apply.rs",
];

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

/// Number of offending lines in a file's text, stopping at the first
/// `#[cfg(test)]` that gates a module.
fn offending_lines(text: &str) -> usize {
    let lines: Vec<&str> = text.lines().collect();
    let mut count = 0;
    for (i, line) in lines.iter().enumerate() {
        if line.trim() == "#[cfg(test)]" && gates_test_module(&lines, i) {
            break;
        }
        if offends(line) {
            count += 1;
        }
    }
    count
}

fn scan() -> BTreeSet<String> {
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
        .filter(|path| {
            let text = fs::read_to_string(path)
                .unwrap_or_else(|err| panic!("cannot read {path:?}: {err}"));
            offending_lines(&text) > 0
        })
        .map(|path| relative(&root, path))
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
    assert_eq!(offending_lines(text), 1);
    let text = "#[cfg(test)]\nuse x;\nif kind == DbKind::MySql {\n}\n";
    assert_eq!(offending_lines(text), 1);
}

#[test]
fn cfg_test_module_stops_scan() {
    let text = "#[cfg(test)]\nmod tests {\n    if kind == DbKind::MySql {}\n}\n";
    assert_eq!(offending_lines(text), 0);
    let text = "#[cfg(test)]\n#[path = \"x.rs\"]\nmod t;\nif kind == DbKind::MySql {\n";
    assert_eq!(offending_lines(text), 0);
    let text = "if kind == DbKind::MySql {}\n#[cfg(test)]\npub(crate) mod t {\n    a == DbKind::MySql\n}\n";
    assert_eq!(offending_lines(text), 1);
}

#[test]
fn no_binary_kind_branches() {
    let offending = scan();
    let allowed: BTreeSet<String> = ALLOWED.iter().map(|s| s.replace('\\', "/")).collect();
    let new: Vec<&String> = offending.difference(&allowed).collect();
    let stale: Vec<&String> = allowed.difference(&offending).collect();
    assert!(
        new.is_empty() && stale.is_empty(),
        "two-way DbKind/SqlKind branches must become exhaustive matches\n\
         new offenders: {new:#?}\n\
         stale ALLOWED entries: {stale:#?}"
    );
}
