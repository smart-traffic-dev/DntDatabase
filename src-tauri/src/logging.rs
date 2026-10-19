//! File logging: daily rolling `logs/dntdatabase.log.<date>`, kept for 7 days, with a level
//! that can be switched at runtime (`set_log_level`).
//!
//! Default level is `info`; nothing logged here may contain secrets or SQL text. `Debug`
//! additionally enables our own crates at `debug`, including the `dnt::sql` target that is
//! reserved for SQL text.

use dnt_core::error::DbError;
use serde::{Deserialize, Serialize};
use specta::Type;
use std::path::Path;
use std::time::{Duration, SystemTime};
use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{fmt, reload, EnvFilter, Registry};

/// File name prefix of the log files (the appender adds `.<yyyy-mm-dd>`).
pub const LOG_FILE_PREFIX: &str = "dntdatabase.log";

/// Log files whose last modification is older than this are removed at start-up.
pub const LOG_RETENTION: Duration = Duration::from_secs(7 * 24 * 60 * 60);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type)]
pub enum LogLevel {
    Info,
    Debug,
}

impl LogLevel {
    /// `EnvFilter` directives. Targets match by string prefix, so `dnt` covers `dnt::sql`
    /// and every crate of ours (`dnt_core`, `dnt_driver_*`, `dntdatabase`, ...).
    fn directives(self) -> &'static str {
        match self {
            LogLevel::Info => "info",
            LogLevel::Debug => "info,dnt=debug",
        }
    }

    fn filter(self) -> EnvFilter {
        EnvFilter::new(self.directives())
    }
}

/// Keeps the log writer alive and lets the level be changed.
pub struct LogHandle {
    filter: reload::Handle<EnvFilter, Registry>,
    _writer: WorkerGuard,
}

impl LogHandle {
    pub fn set_level(&self, level: LogLevel) -> Result<(), DbError> {
        self.filter
            .reload(level.filter())
            .map_err(|e| DbError::Internal {
                message: format!("cannot change the log level: {e}"),
            })?;
        tracing::info!(?level, "log level changed");
        Ok(())
    }
}

/// Removes old log files, then installs the global subscriber writing to `logs_dir` at
/// `info`. If a global subscriber is already set (tests), it is kept.
pub fn init(logs_dir: &Path) -> LogHandle {
    let removed = remove_old_logs(logs_dir, LOG_RETENTION, SystemTime::now());
    let appender = tracing_appender::rolling::daily(logs_dir, LOG_FILE_PREFIX);
    let (writer, guard) = tracing_appender::non_blocking(appender);
    let (filter, handle) = reload::Layer::new(LogLevel::Info.filter());
    let installed = tracing_subscriber::registry()
        .with(filter)
        .with(fmt::layer().with_writer(writer).with_ansi(false))
        .try_init();
    if let Err(e) = installed {
        eprintln!("logging was already initialised: {e}");
    }
    match removed {
        Ok(0) => {}
        Ok(n) => tracing::info!(removed = n, "removed old log files"),
        Err(e) => tracing::warn!(error = %e, "could not clean up old log files"),
    }
    LogHandle {
        filter: handle,
        _writer: guard,
    }
}

/// Deletes the log files in `dir` last modified more than `max_age` before `now`; returns
/// how many were removed. Other files are left alone.
pub fn remove_old_logs(dir: &Path, max_age: Duration, now: SystemTime) -> std::io::Result<usize> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(e) => return Err(e),
    };
    let mut removed = 0;
    for entry in entries {
        let entry = entry?;
        if !entry
            .file_name()
            .to_string_lossy()
            .starts_with(LOG_FILE_PREFIX)
        {
            continue;
        }
        let meta = entry.metadata()?;
        if !meta.is_file() {
            continue;
        }
        let age = now
            .duration_since(meta.modified()?)
            .unwrap_or(Duration::ZERO);
        if age > max_age {
            std::fs::remove_file(entry.path())?;
            removed += 1;
        }
    }
    Ok(removed)
}

#[cfg(test)]
mod tests {
    use super::*;

    const DAY: Duration = Duration::from_secs(24 * 60 * 60);

    fn file_aged(dir: &Path, name: &str, now: SystemTime, days: u32) {
        let path = dir.join(name);
        std::fs::write(&path, "log line\n").unwrap();
        let file = std::fs::File::options().write(true).open(&path).unwrap();
        file.set_modified(now - DAY * days).unwrap();
    }

    #[test]
    fn log_files_older_than_7_days_removed() {
        let dir = tempfile::tempdir().unwrap();
        let now = SystemTime::now();
        file_aged(dir.path(), "dntdatabase.log.2026-10-18", now, 1);
        file_aged(dir.path(), "dntdatabase.log.2026-10-13", now, 6);
        file_aged(dir.path(), "dntdatabase.log.2026-10-11", now, 8);

        assert_eq!(remove_old_logs(dir.path(), LOG_RETENTION, now).unwrap(), 1);

        let mut left: Vec<String> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        left.sort();
        assert_eq!(
            left,
            ["dntdatabase.log.2026-10-13", "dntdatabase.log.2026-10-18"]
        );
    }

    #[test]
    fn unrelated_files_and_missing_dir_are_left_alone() {
        let dir = tempfile::tempdir().unwrap();
        let now = SystemTime::now();
        file_aged(dir.path(), "notes.txt", now, 30);
        assert_eq!(remove_old_logs(dir.path(), LOG_RETENTION, now).unwrap(), 0);
        assert!(dir.path().join("notes.txt").exists());
        assert_eq!(
            remove_old_logs(&dir.path().join("missing"), LOG_RETENTION, now).unwrap(),
            0
        );
    }

    #[test]
    fn debug_level_enables_sql_target_only_for_our_crates() {
        use tracing::level_filters::LevelFilter;
        use tracing_subscriber::layer::SubscriberExt;

        let enabled = |level: LogLevel, target: &str| {
            let subscriber = tracing_subscriber::registry().with(level.filter());
            tracing::subscriber::with_default(subscriber, || match target {
                "dnt::sql" => tracing::enabled!(target: "dnt::sql", tracing::Level::DEBUG),
                "russh" => tracing::enabled!(target: "russh", tracing::Level::DEBUG),
                _ => unreachable!(),
            })
        };
        assert!(!enabled(LogLevel::Info, "dnt::sql"));
        assert!(enabled(LogLevel::Debug, "dnt::sql"));
        assert!(!enabled(LogLevel::Debug, "russh"));
        assert_eq!(
            LogLevel::Info.filter().max_level_hint(),
            Some(LevelFilter::INFO)
        );
    }
}
