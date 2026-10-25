//! Dumps through an external tool (spec §7.2, R25–R29): `mysqldump` / `mariadb-dump`
//! for MySQL-family connections, `pg_dump` for PostgreSQL. A read: no confirmation,
//! allowed on read-only connections, no History entry (R10).
//!
//! `tool_dump_start` checks the request, finds and probes the tool and builds the
//! invocation once (so a refused name or TLS mode fails before a job exists). The job
//! then resolves where the tool connects now (the tunnel's local port through SSH),
//! builds the invocation again with the secret file in its temporary folder, logs the
//! command line (never a secret) and runs the tool with stdout into `<file>.dntpart`
//! (gzip for `.sql.gz`), renamed onto the file when the tool exits with 0; otherwise the
//! part file is removed (R29).
use super::ext_tools::{
    locate, run_tool, tool_error, OsSpawner, Spawner, StdoutSink, ToolRun, MYSQL_SECRET_FILE,
    PGPASS_FILE,
};
use crate::commands::settings::saved_external_tools_dir;
use crate::ddl_exec::stopped;
use crate::export_format::Target;
use crate::jobs::{
    JobCtx, JobDone, JobFailure, JobKind, JobLogLevel, JobParams, JobRunner, JobSpec,
};
use crate::session_manager::{config_error, ConnectionRuntime, SessionManager};
use async_trait::async_trait;
use dnt_core::error::DbError;
use dnt_core::server::{DbKind, ServerInfo};
use dnt_transfer::tools::{
    mysql_dump_args, pg_dump_args, shown_command, Invocation, MysqlDumpOptions, PgDumpFormat,
    PgDumpOptions, Tool, ToolConn, ToolInfo, ToolOutput,
};
use serde::{Deserialize, Serialize};
use specta::Type;
use std::io::BufWriter;
use std::path::Path;
use std::sync::Arc;
use uuid::Uuid;

/// A dump through an external tool (also the stored parameters of "Run again").
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct ToolDumpRequest {
    pub connection_id: Uuid,
    pub database: String,
    /// `mysqldump` / `mariadb-dump` (MySQL family) or `pg_dump` (PostgreSQL).
    pub tool: Tool,
    /// The options of `mysqldump` / `mariadb-dump`.
    pub mysql: Option<MysqlDumpOptions>,
    /// The options of `pg_dump`.
    pub pg: Option<PgDumpOptions>,
    /// Write `.sql.gz` (the tool's output, gzipped by the app).
    pub compress: bool,
    /// A path admitted by `dump_pick_path`.
    pub path: String,
}

/// The request's own checks: a database and a path, a dump tool of the connection's
/// engine and its options (the other engine's options dropped).
pub(crate) fn normalise_tool_request(
    mut req: ToolDumpRequest,
    kind: DbKind,
) -> Result<ToolDumpRequest, DbError> {
    if req.database.trim().is_empty() {
        return Err(config_error("database", "Choose a database to dump."));
    }
    if req.path.trim().is_empty() {
        return Err(config_error("path", "Choose a file to dump to."));
    }
    match (kind, req.tool) {
        (DbKind::MySql, Tool::MysqlDump | Tool::MariadbDump) => {
            req.pg = None;
            if req.mysql.is_none() {
                return Err(config_error("mysql", "The mysqldump options are missing."));
            }
        }
        (DbKind::Postgres, Tool::PgDump) => {
            req.mysql = None;
            let Some(pg) = &req.pg else {
                return Err(config_error("pg", "The pg_dump options are missing."));
            };
            // A custom or tar archive is never gzipped: pg_restore reads the archive
            // itself, so a gzip-wrapped one could not be restored (R30).
            if pg.format != PgDumpFormat::Plain {
                req.compress = false;
            }
        }
        (_, tool) => {
            return Err(config_error(
                "tool",
                &format!("{} cannot dump this connection.", tool.name()),
            ))
        }
    }
    Ok(req)
}

/// The invocation of `req` for `conn` with its secret file at `secret`.
pub(crate) fn tool_dump_invocation(
    info: &ToolInfo,
    conn: &ToolConn,
    server: &ServerInfo,
    req: &ToolDumpRequest,
    secret: &Path,
) -> Result<Invocation, DbError> {
    let built = match (&req.mysql, &req.pg) {
        (Some(o), _) if req.tool != Tool::PgDump => mysql_dump_args(info, conn, server, o, secret),
        (_, Some(o)) if req.tool == Tool::PgDump => pg_dump_args(info, conn, o, secret),
        _ => {
            return Err(config_error(
                "tool",
                "The options do not match the chosen tool.",
            ))
        }
    };
    built.map_err(tool_error)
}

fn secret_name(kind: DbKind) -> &'static str {
    match kind {
        DbKind::MySql => MYSQL_SECRET_FILE,
        DbKind::Postgres => PGPASS_FILE,
    }
}

fn file_name(path: &str) -> String {
    Path::new(path)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string())
}

/// `1.5 MB`-style size for the summary.
fn size_label(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["KB", "MB", "GB", "TB"];
    if bytes < 1024 {
        return format!("{bytes} bytes");
    }
    let mut v = bytes as f64 / 1024.0;
    let mut unit = 0;
    while v >= 1024.0 && unit < UNITS.len() - 1 {
        v /= 1024.0;
        unit += 1;
    }
    format!("{v:.1} {}", UNITS[unit])
}

impl SessionManager {
    /// Starts a dump through an external tool (a read: no confirmation, allowed on
    /// read-only connections); returns its id. The path must have been admitted by
    /// `dump_pick_path` (checked by the command).
    pub async fn tool_dump_start(&self, request: ToolDumpRequest) -> Result<Uuid, DbError> {
        let cfg = self.repo.get(request.connection_id)?;
        let request = normalise_tool_request(request, cfg.kind)?;
        let rt = self.runtime(request.connection_id)?;
        let configured = saved_external_tools_dir(&self.repo);
        let path_env = std::env::var_os("PATH");
        let info = locate(request.tool, configured.as_deref(), path_env.as_deref()).await?;
        // Checked now so a refused name or TLS mode fails before a job exists.
        let (resolved, tunnelled) = rt.tool_endpoint(Some(request.database.clone())).await?;
        tool_dump_invocation(
            &info,
            &ToolConn::from_resolved(&resolved, tunnelled),
            rt.server_info(),
            &request,
            Path::new(secret_name(cfg.kind)),
        )?;
        drop(resolved);
        let spec = JobSpec {
            kind: JobKind::ToolDump,
            connection_id: Some(request.connection_id),
            title: format!(
                "Dump {} with {} to {}",
                request.database,
                request.tool.name(),
                file_name(&request.path)
            ),
            params: serde_json::to_value(&request).ok().map(JobParams::ToolDump),
            rerunnable: true,
        };
        let job = ToolDumpJob {
            request,
            rt,
            info,
            spawner: Arc::new(OsSpawner),
        };
        self.jobs.submit(spec, Box::new(job))
    }
}

/// The tool dump job.
pub(crate) struct ToolDumpJob {
    pub(crate) request: ToolDumpRequest,
    pub(crate) rt: Arc<ConnectionRuntime>,
    pub(crate) info: ToolInfo,
    pub(crate) spawner: Arc<dyn Spawner>,
}

/// Creates the temp file; returns its writer and a handle to sync it before the rename.
fn create_part(
    part: &Path,
    shown: &str,
    compress: bool,
) -> Result<(ToolOutput, std::fs::File), DbError> {
    let file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(part)
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::AlreadyExists {
                config_error(
                    "path",
                    &format!(
                        "The temporary file {} already exists next to the file (another dump \
                         may be writing it). Remove it, or choose another file.",
                        file_name(&part.to_string_lossy())
                    ),
                )
            } else {
                config_error("path", &format!("Could not write {shown}: {e}."))
            }
        })?;
    let sync = file.try_clone().map_err(|e| {
        let _ = std::fs::remove_file(part);
        config_error("path", &format!("Could not write {shown}: {e}."))
    })?;
    Ok((
        ToolOutput::new(
            Box::new(BufWriter::with_capacity(256 * 1024, file)),
            compress,
        ),
        sync,
    ))
}

#[async_trait]
impl JobRunner for ToolDumpJob {
    async fn run(self: Box<Self>, ctx: JobCtx) -> Result<JobDone, JobFailure> {
        let req = &self.request;
        let name = req.tool.name();
        let target = Target::new(&req.path).map_err(JobFailure::from)?;
        let (resolved, tunnelled) = self
            .rt
            .tool_endpoint(Some(req.database.clone()))
            .await
            .map_err(JobFailure::from)?;
        let tmp = ctx.tmp_dir().map_err(JobFailure::from)?;
        let secret = tmp.join(secret_name(self.rt.cfg.kind));
        let inv = tool_dump_invocation(
            &self.info,
            &ToolConn::from_resolved(&resolved, tunnelled),
            self.rt.server_info(),
            req,
            &secret,
        )
        .map_err(JobFailure::from)?;
        ctx.log(JobLogLevel::Info, format!("Tool: {}", self.info.version));
        ctx.log(
            JobLogLevel::Info,
            format!("Command: {}", shown_command(&inv)),
        );
        let part = target.part.clone();
        let shown = req.path.clone();
        let compress = req.compress;
        let (out, sync) = tokio::task::spawn_blocking(move || create_part(&part, &shown, compress))
            .await
            .map_err(|e| {
                JobFailure::from(DbError::Internal {
                    message: format!("creating the dump file failed: {e}"),
                })
            })?
            .map_err(JobFailure::from)?;
        // The temp file is ours from here: removed on any way out but the commit.
        let guard = target.guard();
        let ran = run_tool(
            self.spawner.as_ref(),
            &ctx,
            ToolRun {
                inv: &inv,
                password: resolved.password.as_ref(),
                stdin: None,
                stdout: StdoutSink::File(Box::new(out)),
            },
        )
        .await;
        drop(resolved);
        // Every early return below drops the guard, which removes the temp file.
        let report = ran.map_err(JobFailure::from)?;
        if let Some(e) = &report.wait_error {
            ctx.log(JobLogLevel::Error, e.clone());
        }
        if report.killed || ctx.is_stopped() {
            return Err(JobFailure::from(stopped()));
        }
        if let Some(e) = &report.out_error {
            return Err(JobFailure::from(config_error(
                "path",
                &format!("Could not write {}: {e}.", req.path),
            )));
        }
        if report.code != Some(0) {
            let code = report
                .code
                .map(|c| format!("exit code {c}"))
                .unwrap_or_else(|| "no exit code".into());
            let message = format!("{name} failed ({code}). The job log has its messages.");
            return Err(JobFailure {
                error: config_error("tool", &message),
                outcome_unknown: false,
                summary: message,
            });
        }
        // Synced first: the rename must not publish unwritten data.
        let synced = tokio::task::spawn_blocking(move || sync.sync_all())
            .await
            .map_err(|e| std::io::Error::other(e.to_string()))
            .and_then(|r| r);
        if let Err(e) = synced {
            return Err(JobFailure::from(config_error(
                "path",
                &format!("Could not write {}: {e}.", req.path),
            )));
        }
        guard.commit().map_err(JobFailure::from)?;
        Ok(JobDone {
            summary: format!("{name} wrote {}.", size_label(report.out_bytes)),
            result_path: Some(req.path.clone()),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(tool: Tool) -> ToolDumpRequest {
        ToolDumpRequest {
            connection_id: Uuid::nil(),
            database: "shop".into(),
            tool,
            mysql: Some(MysqlDumpOptions {
                tables: vec![],
                structure: true,
                data: true,
                routines: false,
                events: false,
                triggers: true,
                single_transaction: true,
            }),
            pg: Some(PgDumpOptions {
                schemas: vec![],
                tables: vec![],
                structure: true,
                data: true,
                format: dnt_transfer::tools::PgDumpFormat::Plain,
                no_owner: false,
                no_privileges: false,
            }),
            compress: false,
            path: r"C:\out\shop.sql".into(),
        }
    }

    #[test]
    fn tool_request_matches_the_engine() {
        let m = normalise_tool_request(req(Tool::MysqlDump), DbKind::MySql).unwrap();
        assert!(m.pg.is_none() && m.mysql.is_some());
        let m = normalise_tool_request(req(Tool::MariadbDump), DbKind::MySql).unwrap();
        assert!(m.pg.is_none());
        let p = normalise_tool_request(req(Tool::PgDump), DbKind::Postgres).unwrap();
        assert!(p.mysql.is_none() && p.pg.is_some());
        for (tool, kind) in [
            (Tool::PgDump, DbKind::MySql),
            (Tool::MysqlDump, DbKind::Postgres),
            (Tool::Psql, DbKind::Postgres),
            (Tool::PgRestore, DbKind::Postgres),
        ] {
            assert!(normalise_tool_request(req(tool), kind).is_err(), "{tool:?}");
        }
        let mut no_opts = req(Tool::MysqlDump);
        no_opts.mysql = None;
        assert!(normalise_tool_request(no_opts, DbKind::MySql).is_err());
        let mut no_db = req(Tool::MysqlDump);
        no_db.database = " ".into();
        assert!(normalise_tool_request(no_db, DbKind::MySql).is_err());
        let mut no_path = req(Tool::MysqlDump);
        no_path.path = String::new();
        assert!(normalise_tool_request(no_path, DbKind::MySql).is_err());
    }

    #[test]
    fn pg_archives_are_never_gzipped() {
        for (format, kept) in [
            (PgDumpFormat::Plain, true),
            (PgDumpFormat::Custom, false),
            (PgDumpFormat::Tar, false),
        ] {
            let mut r = req(Tool::PgDump);
            r.compress = true;
            r.pg.as_mut().unwrap().format = format;
            let r = normalise_tool_request(r, DbKind::Postgres).unwrap();
            assert_eq!(r.compress, kept, "{format:?}");
        }
    }

    #[test]
    fn sizes() {
        assert_eq!(size_label(10), "10 bytes");
        assert_eq!(size_label(1536), "1.5 KB");
        assert_eq!(size_label(5 * 1024 * 1024), "5.0 MB");
    }
}
