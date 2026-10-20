//! The files an export may write: only paths the user picked in a save dialog the
//! BACKEND opened (`export_pick_path`), for a short while. The UI cannot name another
//! path for `export_rows` / `export_table` / `export_query`, and `reveal_export` only
//! shows files an export wrote.
//!
//! A pick stays approved for [`PICK_TTL`] (an export refused before it wrote anything,
//! e.g. because the tab was busy, can be started again without a new dialog) and is
//! used up by an export that wrote it. The temp file an export writes first
//! (`<file>.dntpart`) must sit in the picked file's directory.

use dnt_core::error::DbError;
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// How long a picked path may be exported to.
pub const PICK_TTL: Duration = Duration::from_secs(10 * 60);
/// Picks kept (oldest dropped first).
const MAX_PICKED: usize = 16;
/// Written files `reveal_export` may show (oldest dropped first).
const MAX_WRITTEN: usize = 64;

fn path_refused() -> DbError {
    DbError::Config {
        field: Some("path".into()),
        message: "Choose the file to export to in the save dialog.".into(),
    }
}

#[derive(Default)]
struct Inner {
    picked: VecDeque<(PathBuf, Instant)>,
    written: VecDeque<PathBuf>,
}

#[derive(Default)]
pub struct ExportPaths {
    inner: Mutex<Inner>,
    ttl: Option<Duration>,
}

impl ExportPaths {
    /// With another pick lifetime (tests).
    pub fn with_ttl(ttl: Duration) -> ExportPaths {
        ExportPaths {
            ttl: Some(ttl),
            ..ExportPaths::default()
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn ttl(&self) -> Duration {
        self.ttl.unwrap_or(PICK_TTL)
    }

    /// Records a path the user picked in the backend's save dialog; the string the UI
    /// passes back to export to it. Only an absolute path with a file name qualifies.
    pub fn approve(&self, path: PathBuf) -> Result<String, DbError> {
        if !path.is_absolute() || path.file_name().is_none() {
            return Err(path_refused());
        }
        let shown = path.to_string_lossy().into_owned();
        let mut inner = self.lock();
        let ttl = self.ttl();
        inner
            .picked
            .retain(|(p, at)| *p != path && at.elapsed() < ttl);
        if inner.picked.len() >= MAX_PICKED {
            inner.picked.pop_front();
        }
        inner.picked.push_back((path, Instant::now()));
        Ok(shown)
    }

    /// Whether `path` is a live pick, and its temp file (`<path>.dntpart`) would sit in
    /// the same directory.
    pub fn check(&self, path: &str) -> Result<(), DbError> {
        let wanted = Path::new(path);
        let mut part = wanted.as_os_str().to_owned();
        part.push(".dntpart");
        if Path::new(&part).parent() != wanted.parent() {
            return Err(path_refused());
        }
        let mut inner = self.lock();
        let ttl = self.ttl();
        inner.picked.retain(|(_, at)| at.elapsed() < ttl);
        if inner.picked.iter().any(|(p, _)| p.as_path() == wanted) {
            Ok(())
        } else {
            Err(path_refused())
        }
    }

    /// An export wrote `path`: its pick is used up, and `reveal_export` may show it.
    pub fn written(&self, path: &str) {
        let wanted = PathBuf::from(path);
        let mut inner = self.lock();
        inner.picked.retain(|(p, _)| *p != wanted);
        inner.written.retain(|p| *p != wanted);
        if inner.written.len() >= MAX_WRITTEN {
            inner.written.pop_front();
        }
        inner.written.push_back(wanted);
    }

    /// Whether an export wrote `path`.
    pub fn check_written(&self, path: &str) -> Result<(), DbError> {
        if self
            .lock()
            .written
            .iter()
            .any(|p| p.as_path() == Path::new(path))
        {
            Ok(())
        } else {
            Err(DbError::Config {
                field: Some("path".into()),
                message: "Only an exported file can be shown.".into(),
            })
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
    fn only_picked_paths_are_exported_to() {
        let paths = ExportPaths::default();
        let picked = paths.approve(abs("out.csv")).unwrap();
        assert!(paths.check(&picked).is_ok());
        // Anything else the UI names is refused.
        for other in [
            abs("other.csv").to_string_lossy().into_owned(),
            "C:\\Windows\\System32\\drivers\\etc\\hosts".into(),
            format!("{picked}.bat"),
            "relative.csv".into(),
        ] {
            assert!(paths.check(&other).is_err(), "{other}");
        }
        assert!(paths.approve(PathBuf::from("relative.csv")).is_err());
        // Used up once written; then only revealable.
        assert!(paths.check_written(&picked).is_err());
        paths.written(&picked);
        assert!(paths.check(&picked).is_err());
        assert!(paths.check_written(&picked).is_ok());
    }

    #[test]
    fn picks_expire_and_are_bounded() {
        let paths = ExportPaths::with_ttl(Duration::from_millis(0));
        let picked = paths.approve(abs("late.csv")).unwrap();
        std::thread::sleep(Duration::from_millis(2));
        assert!(paths.check(&picked).is_err());

        let paths = ExportPaths::default();
        let first = paths.approve(abs("p0.csv")).unwrap();
        for i in 1..=MAX_PICKED {
            paths.approve(abs(&format!("p{i}.csv"))).unwrap();
        }
        assert!(paths.check(&first).is_err());
        assert!(paths.check(&abs("p1.csv").to_string_lossy()).is_ok());
    }
}
