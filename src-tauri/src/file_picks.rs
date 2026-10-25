//! The files the UI may have the backend read or write outside exports: only paths the
//! user picked in a dialog the BACKEND opened (`file_pick_open` / `file_pick_save`), or
//! the `.sql` file of a query tab as stored in the app's own database (re-admitted by
//! `tabs_load`). `file_read` / `file_write` and the connection import / export commands
//! refuse any other path the UI names, so a compromised webview cannot read or
//! overwrite arbitrary files through them.
//!
//! Each path is admitted for one [`FileAccess`]: a `.sql` file picked to open may also
//! be saved (Ctrl+S writes back to it), a connections file picked to import is only
//! read, one picked to export is only written; a cell value file picked to load is only
//! read, one picked to save is only written. Admissions last for the app session
//! (the `.sql` file of an open tab is saved again much later); the oldest are dropped
//! past [`MAX_ADMITTED`] per access.

use dnt_core::error::DbError;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// Paths kept per access (oldest dropped first).
pub const MAX_ADMITTED: usize = 4096;

/// What the UI asks a dialog for (`file_pick_open` / `file_pick_save`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, specta::Type)]
#[serde(rename_all = "snake_case")]
pub enum FileKind {
    /// A `.sql` file of the query editor.
    Sql,
    /// A connections file: `.dntconn` / Navicat `.ncx` to import, `.dntconn` to export.
    Connections,
    /// Any file holding one cell's value (the value editor's Load / Save).
    CellValue,
}

/// What an admitted path may be used for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FileAccess {
    /// `file_read` and `file_write` (a query tab's `.sql` file).
    Sql,
    /// `import_needs_passphrase`, `preview_import`, `apply_import`.
    ConnectionsImport,
    /// `export_connections`.
    ConnectionsExport,
    /// `data_cell_load_file`.
    CellValueRead,
    /// `data_cell_save_file`.
    CellValueWrite,
    /// A data file to import (`import_pick_file`): `import_inspect`, the dry run, the plan.
    ImportRead,
    /// A dump file to write.
    DumpWrite,
    /// A dump file to restore.
    RestoreRead,
}

impl FileAccess {
    /// The access a dialog of `kind` grants: `save` for a save dialog.
    pub fn of(kind: FileKind, save: bool) -> FileAccess {
        match (kind, save) {
            (FileKind::Sql, _) => FileAccess::Sql,
            (FileKind::Connections, false) => FileAccess::ConnectionsImport,
            (FileKind::Connections, true) => FileAccess::ConnectionsExport,
            (FileKind::CellValue, false) => FileAccess::CellValueRead,
            (FileKind::CellValue, true) => FileAccess::CellValueWrite,
        }
    }
}

fn refused() -> DbError {
    DbError::Config {
        field: Some("path".into()),
        message: "Choose the file in the file dialog.".into(),
    }
}

#[derive(Default)]
pub struct FilePicks {
    admitted: Mutex<HashMap<FileAccess, VecDeque<PathBuf>>>,
}

impl FilePicks {
    /// Admits `path` for `access` and returns the string the UI passes back. Only an
    /// absolute path with a file name qualifies.
    pub fn admit(&self, access: FileAccess, path: PathBuf) -> Result<String, DbError> {
        if !path.is_absolute() || path.file_name().is_none() {
            return Err(refused());
        }
        let shown = path.to_string_lossy().into_owned();
        let mut admitted = self.admitted.lock().unwrap_or_else(|p| p.into_inner());
        let list = admitted.entry(access).or_default();
        list.retain(|p| *p != path);
        if list.len() >= MAX_ADMITTED {
            list.pop_front();
        }
        list.push_back(path);
        Ok(shown)
    }

    /// Whether `path` was admitted for `access`; refused (`config`, field `path`)
    /// otherwise.
    pub fn check(&self, access: FileAccess, path: &str) -> Result<(), DbError> {
        let wanted = Path::new(path);
        let admitted = self.admitted.lock().unwrap_or_else(|p| p.into_inner());
        let found = admitted
            .get(&access)
            .is_some_and(|list| list.iter().any(|p| p.as_path() == wanted));
        if found && wanted.is_absolute() {
            Ok(())
        } else {
            Err(refused())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn abs(name: &str) -> PathBuf {
        std::env::temp_dir().join(name)
    }

    #[test]
    fn only_admitted_paths_pass_for_their_access() {
        let picks = FilePicks::default();
        let sql = picks.admit(FileAccess::Sql, abs("q.sql")).unwrap();
        assert!(picks.check(FileAccess::Sql, &sql).is_ok());
        // Not for another access, and nothing else the UI names.
        assert!(picks.check(FileAccess::ConnectionsExport, &sql).is_err());
        assert!(picks.check(FileAccess::ConnectionsImport, &sql).is_err());
        for other in [
            abs("other.sql").to_string_lossy().into_owned(),
            "C:\\Windows\\System32\\drivers\\etc\\hosts".into(),
            format!("{sql}.bat"),
            "q.sql".into(),
            String::new(),
        ] {
            let err = picks.check(FileAccess::Sql, &other).unwrap_err();
            assert!(
                matches!(&err, DbError::Config { field: Some(f), .. } if f == "path"),
                "{other}: {err:?}"
            );
        }
        assert!(picks
            .admit(FileAccess::Sql, PathBuf::from("relative.sql"))
            .is_err());

        let import = picks
            .admit(FileAccess::ConnectionsImport, abs("in.ncx"))
            .unwrap();
        assert!(picks.check(FileAccess::ConnectionsImport, &import).is_ok());
        assert!(picks.check(FileAccess::ConnectionsExport, &import).is_err());
        // Admissions stay (a tab's file is saved again later).
        assert!(picks.check(FileAccess::Sql, &sql).is_ok());
    }

    #[test]
    fn admissions_are_bounded() {
        let picks = FilePicks::default();
        let first = picks.admit(FileAccess::Sql, abs("p0.sql")).unwrap();
        for i in 1..=MAX_ADMITTED {
            picks
                .admit(FileAccess::Sql, abs(&format!("p{i}.sql")))
                .unwrap();
        }
        assert!(picks.check(FileAccess::Sql, &first).is_err());
        assert!(picks
            .check(FileAccess::Sql, &abs("p1.sql").to_string_lossy())
            .is_ok());
    }

    #[test]
    fn dialogs_grant_their_access() {
        assert_eq!(FileAccess::of(FileKind::Sql, false), FileAccess::Sql);
        assert_eq!(FileAccess::of(FileKind::Sql, true), FileAccess::Sql);
        assert_eq!(
            FileAccess::of(FileKind::Connections, false),
            FileAccess::ConnectionsImport
        );
        assert_eq!(
            FileAccess::of(FileKind::Connections, true),
            FileAccess::ConnectionsExport
        );
        assert_eq!(
            FileAccess::of(FileKind::CellValue, false),
            FileAccess::CellValueRead
        );
        assert_eq!(
            FileAccess::of(FileKind::CellValue, true),
            FileAccess::CellValueWrite
        );
    }
}
