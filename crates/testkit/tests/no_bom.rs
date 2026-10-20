//! Repository hygiene: no tracked file starts with a UTF-8 byte order mark.

use std::path::PathBuf;
use std::process::Command;

#[test]
fn no_tracked_file_has_a_utf8_bom() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let Ok(out) = Command::new("git")
        .arg("ls-files")
        .arg("-z")
        .current_dir(&root)
        .output()
    else {
        eprintln!("skipped: git is not available");
        return;
    };
    if !out.status.success() {
        eprintln!("skipped: not a git checkout");
        return;
    }
    let with_bom: Vec<String> = out
        .stdout
        .split(|&b| b == 0)
        .filter(|p| !p.is_empty())
        .map(|p| String::from_utf8_lossy(p).into_owned())
        .filter(|p| {
            std::fs::read(root.join(p)).is_ok_and(|bytes| bytes.starts_with(b"\xEF\xBB\xBF"))
        })
        .collect();
    assert!(with_bom.is_empty(), "files with a UTF-8 BOM: {with_bom:?}");
}
