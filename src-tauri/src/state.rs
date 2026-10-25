//! App-wide state: data paths, connection repository, secret store and sessions.

use crate::export_paths::ExportPaths;
use crate::file_picks::FilePicks;
use crate::session_manager::SessionManager;
use dnt_core::driver::Driver;
use dnt_core::error::DbError;
use dnt_core::secret::SecretStore;
use dnt_core::server::DbKind;
use dnt_driver_mysql::MySqlDriver;
use dnt_driver_pg::PgDriver;
use dnt_store::repo::ConnectionRepo;
use dnt_tunnel::KnownHosts;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

/// Overrides the data directory (used by E2E tests).
pub const DATA_DIR_ENV: &str = "DNT_DATA_DIR";

/// Directory name under the OS data directory (`%APPDATA%` on Windows). Debug builds use
/// their own, so a dev build never shares the app database, result files or logs with an
/// installed release build running at the same time.
#[cfg(not(debug_assertions))]
pub const APP_DIR: &str = "DntDatabase";
#[cfg(debug_assertions)]
pub const APP_DIR: &str = "DntDatabase-dev";

/// Directory of the jobs' temporary files, below the data directory.
pub const JOB_TMP_DIR: &str = "job-tmp";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppPaths {
    pub data_dir: PathBuf,
    pub db_file: PathBuf,
    pub known_hosts: PathBuf,
    pub logs_dir: PathBuf,
}

impl AppPaths {
    /// `$DNT_DATA_DIR` when set, otherwise `<OS data dir>/<APP_DIR>`.
    pub fn resolve() -> Result<AppPaths, DbError> {
        if let Some(dir) = std::env::var_os(DATA_DIR_ENV).filter(|d| !d.is_empty()) {
            return Ok(AppPaths::in_dir(PathBuf::from(dir)));
        }
        let base = dirs::data_dir().ok_or_else(|| DbError::Internal {
            message: "cannot locate the user data directory".into(),
        })?;
        Ok(AppPaths::in_dir(base.join(APP_DIR)))
    }

    /// The fixed layout below `data_dir`.
    pub fn in_dir(data_dir: PathBuf) -> AppPaths {
        AppPaths {
            db_file: data_dir.join("app.db"),
            known_hosts: data_dir.join("known_hosts"),
            logs_dir: data_dir.join("logs"),
            data_dir,
        }
    }
}

/// The drivers of every supported database kind.
pub fn default_drivers() -> HashMap<DbKind, Arc<dyn Driver>> {
    HashMap::from([
        (DbKind::MySql, Arc::new(MySqlDriver) as Arc<dyn Driver>),
        (DbKind::Postgres, Arc::new(PgDriver) as Arc<dyn Driver>),
    ])
}

pub struct AppState {
    pub repo: Arc<ConnectionRepo>,
    pub secrets: Arc<dyn SecretStore>,
    pub sessions: SessionManager,
    pub paths: AppPaths,
    /// The files exports may write (picked in the backend's save dialog).
    pub export_paths: ExportPaths,
    /// The `.sql` and connections files the UI may have read or written (picked in a
    /// dialog the backend opened, or a query tab's stored file).
    pub file_picks: FilePicks,
}

impl AppState {
    /// Creates the data and log directories, opens (and migrates) the app database.
    pub fn open(paths: AppPaths, secrets: Arc<dyn SecretStore>) -> Result<AppState, DbError> {
        for dir in [&paths.data_dir, &paths.logs_dir] {
            std::fs::create_dir_all(dir).map_err(|e| DbError::Internal {
                message: format!("cannot create {}: {e}", dir.display()),
            })?;
        }
        let conn = dnt_store::db::open(&paths.db_file)?;
        let repo = Arc::new(ConnectionRepo::new(conn, Arc::clone(&secrets)));
        let sessions = SessionManager::new(
            Arc::clone(&repo),
            Arc::clone(&secrets),
            KnownHosts::new(paths.known_hosts.clone()),
            default_drivers(),
        )
        .with_result_root(paths.data_dir.join("query-results"))?;
        // Jobs: temporary files under the data directory; jobs the last exit interrupted
        // become failed with an unknown outcome and their leftover files go (R7).
        sessions.jobs.set_tmp_root(paths.data_dir.join(JOB_TMP_DIR));
        if let Err(e) = sessions.jobs.recover() {
            tracing::warn!(error = %e, "could not recover the jobs of the last run");
        }
        Ok(AppState {
            repo,
            secrets,
            sessions,
            paths,
            export_paths: ExportPaths::default(),
            file_picks: FilePicks::default(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dnt_core::secret::MemorySecretStore;

    /// The only test in this binary touching `DNT_DATA_DIR`.
    #[test]
    fn data_dir_env_override() {
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var(DATA_DIR_ENV, dir.path());
        let paths = AppPaths::resolve();
        std::env::remove_var(DATA_DIR_ENV);
        let paths = paths.unwrap();
        assert_eq!(paths.data_dir, dir.path());
        assert_eq!(paths.db_file, dir.path().join("app.db"));
        assert_eq!(paths.known_hosts, dir.path().join("known_hosts"));
        assert_eq!(paths.logs_dir, dir.path().join("logs"));

        let default = AppPaths::resolve().unwrap();
        assert_eq!(default.data_dir, dirs::data_dir().unwrap().join(APP_DIR));
        // A debug build must never resolve to the release data directory.
        #[cfg(debug_assertions)]
        assert_eq!(APP_DIR, "DntDatabase-dev");
    }

    #[test]
    fn app_state_opens_in_data_dir() {
        let dir = tempfile::tempdir().unwrap();
        let paths = AppPaths::in_dir(dir.path().join("data"));
        let state = AppState::open(paths.clone(), Arc::new(MemorySecretStore::default())).unwrap();
        assert!(paths.db_file.is_file());
        assert!(paths.logs_dir.is_dir());
        assert!(state.repo.tree().unwrap().connections.is_empty());
        assert!(state.sessions.open_connections().is_empty());
    }
}
