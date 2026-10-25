//! Restore (spec §7.3, R30–R34, R20, R21, R10): exactly three guarded paths.
//!
//! - **Preview** (`restore_preview`): blocked on read-only connections; one blocking pass
//!   over the file detects the path by content ([`dnt_transfer::detect`]), scans text
//!   files with N6 and hashes the raw bytes (SHA-256). App-runner refusals (R31), a file of
//!   the other engine, and a needed tool that is not installed (`Unsupported`, nothing
//!   executed) end the preview. pg_dump archives get their TOC from `pg_restore --list`.
//!   The single-use plan binds the file (SHA-256 + size), the connection (runtime, tunnel
//!   generation, settings), the target database, the options and the session dialect;
//!   any change → `PlanStale`. Plans live `plan_ttl`.
//! - **Start** (`restore_start`): re-checks the plan, the confirmation (Production types
//!   the database name, Staging confirms, R32), read-only, and re-hashes the file
//!   (`PlanStale` before anything is sent, R21).
//! - **App runner** ([`AppRestoreJob`]): a session on the target database; PostgreSQL in
//!   one transaction unless the file controls its own or holds statements that cannot run
//!   in one (R31); the file (gzip decoded, BOM stripped) feeds the `StreamSplitter` (client
//!   detection on) in 256 KiB chunks; each statement is checked again
//!   ([`statement_refusal`]), judged by the query guard as a one-statement `ScriptPlan`
//!   (its text verbatim, never re-split) and sent through `dnt_query::run_with` (R33). A
//!   `Block` verdict stops the job before sending; `Confirm` verdicts are covered by the
//!   job's confirmation. After each statement the session dialect is fed back to the
//!   splitter. The bytes read are hashed again and compared before `COMMIT` (PostgreSQL)
//!   / at the end; inside the transaction a mismatch rolls back.
//! - **Tools** ([`ToolRestoreJob`]): `psql -f -` (stdin from the file, gzip decoded by the
//!   app, R34) or `pg_restore` through the N7 runner; outcome per
//!   `dnt_transfer::tools::restore_outcome`.
//!
//! One History entry per job (R10, origin `restore`), written when anything was sent.
//!
//! Statements sent outside `plan` + guard, and why they cannot touch other data: the
//! cleanup `ROLLBACK` after a failure or Stop (it only undoes the job's own
//! transaction), sent with a fresh stop signal.
//!
//! The job splits with the scan's prediction of the session dialect
//! (`dnt_transfer::detect::ModeTracker`) and stops before sending a statement when the
//! session's real dialect differs (`PlanStale`). Inside the job's PostgreSQL transaction a
//! transaction-control statement or one that cannot run in a transaction block is refused
//! before it is sent (a file swapped after the start's check).
//!
//! Known limits (documented): the splitter keeps events it had already decided when a
//! statement changes the dialect (it rescans only what it had not returned); every such
//! statement is still checked and judged under the new dialect. With an external tool,
//! the window between the start's hash check and the tool's read is accepted (R21).
//! Restoring a gzip-compressed pg_dump archive is refused (pg_restore reads the archive
//! file itself): decompress it first.
use super::ext_tools::{
    probe, run_tool, tool_error, OsSpawner, RunCtx, Spawner, StdinFeed, StdoutSink, ToolRun,
    MAX_LIST_BYTES, PGPASS_FILE,
};
use crate::commands::settings::saved_external_tools_dir;
use crate::ddl_exec::{execute, stopped, HISTORY_FAILED};
use crate::import_data::ConfirmLevel;
use crate::jobs::{
    JobCtx, JobDone, JobFailure, JobKind, JobLogLevel, JobParams, JobRunner, JobSpec, ProgressUnit,
};
use crate::object_actions::stale;
use crate::session_manager::{
    close_quietly, config_error, lock, ConnectionRuntime, SessionManager,
};
use async_trait::async_trait;
use dnt_core::config::{ConnectionConfig, EnvTag};
use dnt_core::driver::{
    Canceller, ExecOptions, ResultSummary, RowSink, Session, StmtOutcome, StopSignal,
};
use dnt_core::error::{AppError, ConnectStage, DbError};
use dnt_core::server::{DbKind, ServerInfo, ServerVariant};
use dnt_core::sql::{classify, Dialect, SplitEvent, SplitOptions, StatementKind, StreamSplitter};
use dnt_core::tx::TxState;
use dnt_core::value::{ColumnMeta, Value};
use dnt_query::{
    guard_with, run_with, GuardCtx, PlannedStmt, RunCancel, RunConfig, RunSink, ScriptPlan, Verdict,
};
use dnt_store::history::{HistoryOrigin, HistoryStatus, NewHistoryEntry};
use dnt_store::repo::ConnectionRepo;
use dnt_transfer::detect::{
    analyze_file, pg_outside_tx_block, sent_text, statement_refusal, Container, Content,
    ModeTracker, MySqlServer, RestorePath, ScanResult, ScanStats, ScanTarget, CHUNK,
};
use dnt_transfer::tools::{
    find_tool, pg_restore_args, pg_restore_list_args, psql_args, restore_outcome, shown_command,
    Invocation, PgRestoreOptions, PsqlOptions, Tool, ToolConn, ToolInfo, ToolOutcome,
};
use dnt_transfer::FileFacts;
use serde::{Deserialize, Serialize};
use specta::Type;
use std::collections::HashMap;
use std::io::{BufReader, Read};
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use uuid::Uuid;

/// Restore plans kept at once.
pub(crate) const MAX_RESTORE_PLANS: usize = 16;
/// TOC lines of `pg_restore --list` kept for the preview.
pub const MAX_TOC_LINES: usize = 2000;
/// Bound of a cleanup `ROLLBACK`.
const CLEANUP_TIMEOUT_S: u32 = 30;
/// Chunks in flight between the file reader and the job.
const CHANNEL_DEPTH: usize = 8;

/// The preview's message when psql is needed and missing (spec §7.3).
pub const PSQL_MISSING: &str = "This file needs psql (it uses COPY … FROM stdin or psql \
     commands) and psql was not found. Install the PostgreSQL client tools or set the External \
     tools folder.";
/// The preview's message when pg_restore is needed and missing.
pub const PG_RESTORE_MISSING: &str = "This file is a pg_dump archive (custom or tar format) and \
     needs pg_restore, which was not found. Install the PostgreSQL client tools or set the \
     External tools folder.";

/// What the user asked to restore (also the stored parameters of "Run again").
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct RestoreRequest {
    pub connection_id: Uuid,
    /// The target database.
    pub database: String,
    /// A path admitted by `restore_pick_file`.
    pub path: String,
    /// psql / pg_restore `--single-transaction` (the app runner decides from the file,
    /// R31).
    pub single_transaction: bool,
    /// pg_restore `--clean --if-exists`.
    pub clean: bool,
    /// pg_restore `--no-owner`.
    pub no_owner: bool,
}

/// What `restore_start` will run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Type)]
pub struct RestorePreview {
    pub plan_id: Uuid,
    pub path: RestorePath,
    pub file: FileFacts,
    pub compressed: bool,
    /// Text files: the statement counts.
    pub stats: Option<ScanStats>,
    /// pg_restore: the archive's TOC entries (at most 2,000).
    pub toc: Vec<String>,
    pub toc_truncated: bool,
    /// psql / pg_restore: the command line (never a secret).
    pub command: Option<String>,
    pub tool: Option<ToolInfo>,
    /// The restore runs in one transaction (a failure restores nothing).
    pub transactional: bool,
    pub warnings: Vec<String>,
    pub confirm: ConfirmLevel,
    pub expires_in_seconds: u32,
}

/// Tools and spawner used instead of discovery (tests).
pub(crate) struct FixedTools {
    pub(crate) tools: Vec<ToolInfo>,
    pub(crate) spawner: Arc<dyn Spawner>,
}

struct StoredRestore {
    preview: RestorePreview,
    request: RestoreRequest,
    rt: Arc<ConnectionRuntime>,
    generation: u64,
    cfg: ConnectionConfig,
    dialect: Dialect,
    expires: Instant,
}

/// Single-use restore plans by id (they live `plan_ttl`).
#[derive(Default)]
pub struct RestorePlanStore {
    plans: Mutex<HashMap<Uuid, StoredRestore>>,
    /// `Some`: the tools are these (no discovery, no real process).
    pub(crate) fixed_tools: Mutex<Option<Arc<FixedTools>>>,
}

impl RestorePlanStore {
    /// Closing `conn`: its plans are dropped.
    pub(crate) fn drop_conn(&self, conn: Uuid) {
        lock(&self.plans).retain(|_, p| p.request.connection_id != conn);
    }
}

fn file_name(path: &str) -> String {
    Path::new(path)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string())
}

/// The file is not the one the preview read (R21).
fn file_changed() -> DbError {
    DbError::PlanStale {
        reason: "The file changed since the analysis; analyze again.".into(),
    }
}

fn file_refused(message: &str) -> DbError {
    config_error("file", message)
}

async fn hash(path: PathBuf) -> Result<FileFacts, DbError> {
    tokio::task::spawn_blocking(move || dnt_transfer::file_facts(&path))
        .await
        .map_err(|e| DbError::Internal {
            message: format!("hashing the file failed: {e}"),
        })?
        .map_err(|e| config_error("path", &format!("Could not read the file: {e}.")))
}

/// The scan's refusals as one message.
fn refusal_message(scan: &ScanResult) -> String {
    let mut text = scan.refusals.join(" ");
    let listed = scan.refusals.len() as u64;
    if scan.refused > listed {
        text.push_str(&format!(" ({} more.)", scan.refused - listed));
    }
    text
}

/// The invocation of a tool restore (the secret file at `secret`).
fn restore_invocation(
    info: &ToolInfo,
    conn: &ToolConn,
    req: &RestoreRequest,
    single_transaction: bool,
    secret: &Path,
) -> Result<Invocation, DbError> {
    match info.tool {
        Tool::Psql => psql_args(info, conn, &PsqlOptions { single_transaction }, secret),
        Tool::PgRestore => pg_restore_args(
            info,
            conn,
            &PgRestoreOptions {
                single_transaction,
                clean: req.clean,
                no_owner: req.no_owner,
                no_privileges: false,
            },
            secret,
            Path::new(&req.path),
        ),
        other => {
            return Err(config_error(
                "tool",
                &format!("{} cannot restore.", other.name()),
            ))
        }
    }
    .map_err(tool_error)
}

/// What the scan and the job check statements against.
fn scan_target(database: &str, dialect: Dialect, server: &ServerInfo) -> ScanTarget {
    ScanTarget {
        kind: server.kind,
        database: database.to_string(),
        dialect,
        server: (server.kind == DbKind::MySql).then_some(MySqlServer {
            version: server.version,
            mariadb: server.variant == ServerVariant::MariaDb,
        }),
    }
}

/// Collects a preview tool's stderr (never stopped).
#[derive(Default)]
struct ListCtx {
    lines: Mutex<Vec<String>>,
}

impl RunCtx for ListCtx {
    fn tool_line(&self, _level: JobLogLevel, line: String) {
        lock(&self.lines).push(line);
    }
    fn set_kill(&self, _kill: Option<Box<dyn Fn() + Send + Sync>>) {}
    fn is_stopped(&self) -> bool {
        false
    }
    fn bytes_out(&self, _bytes: u64) {}
}

/// The job's guard inputs (unbounded writes always confirmed: the job confirmation
/// covers `Confirm` verdicts).
fn guard_ctx(env: Option<EnvTag>, read_only: bool) -> GuardCtx {
    GuardCtx {
        env,
        read_only,
        analyze: false,
        confirm_unbounded: true,
    }
}

/// One statement as a plan of its own: the text verbatim, never re-split (R33).
pub(crate) fn one_statement_plan(text: &str, dialect: Dialect) -> ScriptPlan {
    let stmt = PlannedStmt {
        index: 0,
        range: (0, text.encode_utf16().count() as u32),
        text: text.to_string(),
        kind: dnt_query::plan::statement_kind(text, dialect),
        verdict: Verdict::Allow,
        source_range: (0, text.len()),
        unbounded: dnt_query::unbounded_writes(text, dialect),
        drops: dnt_query::drop_targets(text, dialect),
        confirm_name: None,
        estimate: None,
        implicit_commit: false,
    };
    ScriptPlan {
        dialect,
        source: text.to_string(),
        base: 0,
        stmts: vec![stmt],
        explain: false,
        guard: None,
    }
}

impl SessionManager {
    fn restore_fixed(&self) -> Option<Arc<FixedTools>> {
        lock(&self.restore_plans.fixed_tools).clone()
    }

    fn restore_spawner(&self) -> Arc<dyn Spawner> {
        match self.restore_fixed() {
            Some(f) => Arc::clone(&f.spawner),
            None => Arc::new(OsSpawner),
        }
    }

    /// The tool, found and probed; `None` when it is not installed.
    async fn restore_tool(&self, tool: Tool) -> Result<Option<ToolInfo>, DbError> {
        if let Some(f) = self.restore_fixed() {
            return Ok(f.tools.iter().find(|t| t.tool == tool).cloned());
        }
        let configured = saved_external_tools_dir(&self.repo);
        let path_env = std::env::var_os("PATH");
        match find_tool(tool, configured.as_deref(), path_env.as_deref()) {
            Some(p) => probe(tool, &p).await.map(Some),
            None => Ok(None),
        }
    }

    /// `pg_restore --list <file>`: the TOC entries (comment lines dropped).
    async fn pg_restore_toc(
        &self,
        info: &ToolInfo,
        file: &Path,
    ) -> Result<(Vec<String>, bool), DbError> {
        let inv = pg_restore_list_args(info, file).map_err(tool_error)?;
        let ctx = ListCtx::default();
        let spawner = self.restore_spawner();
        let report = run_tool(
            spawner.as_ref(),
            &ctx,
            ToolRun {
                inv: &inv,
                password: None,
                stdin: None,
                stdout: StdoutSink::Capture {
                    limit: MAX_LIST_BYTES,
                },
            },
        )
        .await?;
        if report.code != Some(0) {
            let first = lock(&ctx.lines).first().cloned().unwrap_or_default();
            return Err(file_refused(&format!(
                "pg_restore could not read the archive ({}). {first}",
                report
                    .code
                    .map(|c| format!("exit code {c}"))
                    .unwrap_or_else(|| "no exit code".into())
            )));
        }
        let text = String::from_utf8_lossy(&report.captured);
        let mut lines: Vec<String> = text
            .lines()
            .map(str::trim_end)
            .filter(|l| !l.trim().is_empty() && !l.starts_with(';'))
            .map(str::to_string)
            .collect();
        let mut truncated = report.captured_truncated;
        if lines.len() > MAX_TOC_LINES {
            lines.truncate(MAX_TOC_LINES);
            truncated = true;
        }
        Ok((lines, truncated))
    }

    /// Analyzes the file for a restore into `request.database` and stores a single-use
    /// plan (R30–R32). Nothing is sent to the target. Read-only connections are refused.
    pub async fn restore_preview(
        &self,
        request: RestoreRequest,
    ) -> Result<RestorePreview, DbError> {
        let conn = request.connection_id;
        let cfg = self.repo.get(conn)?;
        if cfg.read_only {
            return Err(DbError::Blocked { count: 1 });
        }
        if request.database.trim().is_empty() {
            return Err(config_error(
                "database",
                "Choose the database to restore into.",
            ));
        }
        if request.path.trim().is_empty() {
            return Err(config_error("path", "Choose the file to restore."));
        }
        let rt = self.runtime(conn)?;
        let (session, generation) = rt
            .open_query_session(Some(request.database.clone()))
            .await?;
        let dialect = session.dialect();
        close_quietly(session).await;
        let kind = cfg.kind;
        let target = scan_target(&request.database, dialect, rt.server_info());
        let path = PathBuf::from(&request.path);
        let analysis = {
            let path = path.clone();
            tokio::task::spawn_blocking(move || analyze_file(&path, &target))
                .await
                .map_err(|e| DbError::Internal {
                    message: format!("reading the file failed: {e}"),
                })?
                .map_err(|e| config_error("path", &format!("Could not read the file: {e}.")))?
        };
        let pg = kind == DbKind::Postgres;
        let mut warnings: Vec<String> = Vec::new();
        let partial = "Not in one transaction: a failure leaves a partial restore.".to_string();
        let (restore_path, stats, transactional) = match analysis.container {
            Container::PgCustom | Container::PgTar => {
                if !pg {
                    return Err(file_refused(
                        "This is a PostgreSQL archive (pg_dump custom or tar format); it cannot \
                         be restored into MySQL.",
                    ));
                }
                if analysis.gzip {
                    return Err(file_refused(
                        "This pg_dump archive is gzip-compressed; pg_restore reads archives as \
                         they are. Decompress it first.",
                    ));
                }
                if !request.single_transaction {
                    warnings.push(partial.clone());
                }
                if request.clean {
                    warnings.push(
                        "Objects of the archive that exist in the database are dropped first \
                         (--clean)."
                            .into(),
                    );
                }
                (RestorePath::PgRestore, None, request.single_transaction)
            }
            Container::Text => {
                let scan = match analysis.scan {
                    Some(Ok(s)) => s,
                    Some(Err(e)) => return Err(file_refused(&e)),
                    None => {
                        return Err(DbError::Internal {
                            message: "a text file was not scanned".into(),
                        })
                    }
                };
                if scan.refused > 0 {
                    return Err(file_refused(&refusal_message(&scan)));
                }
                let transactional = match scan.path {
                    // psql's --single-transaction covers the first connection only.
                    RestorePath::Psql => {
                        request.single_transaction && scan.transaction_ok() && scan.connects == 0
                    }
                    _ => pg && scan.transaction_ok(),
                };
                if !pg {
                    warnings.push(
                        "MySQL restores are not transactional (DDL commits at once): a failure \
                         leaves a partial restore."
                            .into(),
                    );
                } else if scan.connects > 0 {
                    warnings.push(
                        "The file reconnects (\\connect): it does not run in one transaction, a \
                         failure leaves a partial restore."
                            .into(),
                    );
                } else if !scan.transaction_ok() {
                    warnings.push(
                        "The file controls its own transactions or holds statements that cannot \
                         run in a transaction: a failure leaves a partial restore."
                            .into(),
                    );
                } else if !transactional {
                    warnings.push(partial.clone());
                }
                if scan.stats.drops > 0 {
                    warnings.push(format!(
                        "The file drops {} objects before it creates them.",
                        scan.stats.drops
                    ));
                }
                (scan.path, Some(scan.stats), transactional)
            }
        };
        let needed = match restore_path {
            RestorePath::AppRunner => None,
            RestorePath::Psql => Some((Tool::Psql, PSQL_MISSING)),
            RestorePath::PgRestore => Some((Tool::PgRestore, PG_RESTORE_MISSING)),
        };
        let mut tool = None;
        let mut command = None;
        let (mut toc, mut toc_truncated) = (Vec::new(), false);
        if let Some((t, missing)) = needed {
            let Some(info) = self.restore_tool(t).await? else {
                return Err(DbError::Unsupported {
                    feature: missing.to_string(),
                    version: rt.server_info().version_string.clone(),
                });
            };
            let (resolved, tunnelled) = rt.tool_endpoint(Some(request.database.clone())).await?;
            let inv = restore_invocation(
                &info,
                &ToolConn::from_resolved(&resolved, tunnelled),
                &request,
                transactional,
                Path::new(PGPASS_FILE),
            )?;
            drop(resolved);
            command = Some(shown_command(&inv));
            if t == Tool::PgRestore {
                (toc, toc_truncated) = self.pg_restore_toc(&info, &path).await?;
            }
            tool = Some(info);
        }
        let confirm = match cfg.env_tag {
            Some(EnvTag::Production) => ConfirmLevel::TypeName {
                name: request.database.clone(),
            },
            Some(EnvTag::Staging) => ConfirmLevel::Click,
            _ => ConfirmLevel::None,
        };
        let preview = RestorePreview {
            plan_id: Uuid::new_v4(),
            path: restore_path,
            file: analysis.file,
            compressed: analysis.gzip,
            stats,
            toc,
            toc_truncated,
            command,
            tool,
            transactional,
            warnings,
            confirm,
            expires_in_seconds: self.plan_ttl.as_secs().min(u32::MAX as u64) as u32,
        };
        let mut plans = lock(&self.restore_plans.plans);
        plans.retain(|_, p| p.expires > Instant::now());
        if plans.len() >= MAX_RESTORE_PLANS {
            return Err(DbError::Busy);
        }
        plans.insert(
            preview.plan_id,
            StoredRestore {
                preview: preview.clone(),
                request,
                rt,
                generation,
                cfg,
                dialect,
                expires: Instant::now() + self.plan_ttl,
            },
        );
        Ok(preview)
    }

    /// Re-checks the plan (connection, settings, read-only, the confirmation, the file's
    /// SHA-256) and submits the restore job. The plan is used up whatever the outcome;
    /// nothing is sent before every check passed.
    pub async fn restore_start(
        &self,
        plan_id: Uuid,
        confirmed: bool,
        name: Option<String>,
    ) -> Result<Uuid, DbError> {
        let stored = lock(&self.restore_plans.plans)
            .remove(&plan_id)
            .ok_or(DbError::PlanExpired)?;
        if stored.expires <= Instant::now() {
            return Err(DbError::PlanExpired);
        }
        self.check_restore_context(&stored)?;
        let name_ok = match &stored.preview.confirm {
            ConfirmLevel::TypeName { name: want } => name.as_ref() == Some(want),
            _ => true,
        };
        if !confirmed || !name_ok {
            return Err(DbError::ConfirmationRequired { count: 1 });
        }
        let now = hash(PathBuf::from(&stored.request.path)).await?;
        if now.sha256 != stored.preview.file.sha256 || now.size != stored.preview.file.size {
            return Err(file_changed());
        }
        if stored.preview.path == RestorePath::AppRunner {
            let (session, generation) = stored
                .rt
                .open_query_session(Some(stored.request.database.clone()))
                .await?;
            let same = generation == stored.generation && session.dialect() == stored.dialect;
            close_quietly(session).await;
            if !same {
                return Err(stale());
            }
        }
        self.check_restore_context(&stored)?;
        let request = stored.request;
        let preview = stored.preview;
        let tool_name = preview.tool.as_ref().map(|t| t.tool.name());
        let title = match tool_name {
            Some(t) => format!(
                "Restore {} into {} with {t}",
                file_name(&request.path),
                request.database
            ),
            None => format!(
                "Restore {} into {}",
                file_name(&request.path),
                request.database
            ),
        };
        let params = serde_json::to_value(&request).ok();
        let context = Context {
            repo: Arc::clone(&self.repo),
            rt: stored.rt,
            conn: request.connection_id,
            cfg: stored.cfg,
            generation: stored.generation,
        };
        match (preview.path, preview.tool.clone()) {
            (RestorePath::AppRunner, _) => {
                let spec = JobSpec {
                    kind: JobKind::Restore,
                    connection_id: Some(request.connection_id),
                    title,
                    params: params.map(JobParams::Restore),
                    rerunnable: true,
                };
                let job = AppRestoreJob {
                    file: preview.file,
                    transactional: preview.transactional,
                    dialect: stored.dialect,
                    context,
                    grace: self.cancel_grace,
                    request,
                };
                self.jobs.submit(spec, Box::new(job))
            }
            (_, Some(info)) => {
                let spec = JobSpec {
                    kind: JobKind::ToolRestore,
                    connection_id: Some(request.connection_id),
                    title,
                    params: params.map(JobParams::ToolRestore),
                    rerunnable: true,
                };
                let job = ToolRestoreJob {
                    file: preview.file,
                    gzip: preview.compressed,
                    transactional: preview.transactional,
                    info,
                    context,
                    spawner: self.restore_spawner(),
                    request,
                };
                self.jobs.submit(spec, Box::new(job))
            }
            (_, None) => Err(DbError::Internal {
                message: "a tool restore plan without its tool".into(),
            }),
        }
    }

    fn check_restore_context(&self, stored: &StoredRestore) -> Result<(), DbError> {
        let conn = stored.request.connection_id;
        let cfg = self.repo.get(conn)?;
        if cfg.read_only {
            return Err(DbError::Blocked { count: 1 });
        }
        if cfg != stored.cfg || cfg != stored.rt.cfg {
            return Err(stale());
        }
        let rt = self.runtime(conn)?;
        if !Arc::ptr_eq(&rt, &stored.rt)
            || rt.closed.load(Ordering::SeqCst)
            || rt.tunnel_dead()
            || rt.tunnel_gen.load(Ordering::SeqCst) != stored.generation
        {
            return Err(stale());
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------------------
// Context, history
// ---------------------------------------------------------------------------------------

/// What a job re-checks before it sends: the connection's settings and runtime are the
/// ones the plan bound, and the connection is not read-only.
pub(crate) struct Context {
    pub(crate) repo: Arc<ConnectionRepo>,
    pub(crate) rt: Arc<ConnectionRuntime>,
    pub(crate) conn: Uuid,
    pub(crate) cfg: ConnectionConfig,
    pub(crate) generation: u64,
}

impl Context {
    pub(crate) fn check(&self) -> Result<(), DbError> {
        let cfg = self.repo.get(self.conn)?;
        if cfg.read_only {
            return Err(DbError::Blocked { count: 1 });
        }
        if cfg != self.cfg || cfg != self.rt.cfg {
            return Err(stale());
        }
        if self.rt.closed.load(Ordering::SeqCst)
            || self.rt.tunnel_dead()
            || self.rt.tunnel_gen.load(Ordering::SeqCst) != self.generation
        {
            return Err(stale());
        }
        Ok(())
    }
}

struct HistoryLine<'a> {
    context: &'a Context,
    request: &'a RestoreRequest,
    file: &'a FileFacts,
    started_at: String,
    elapsed: Duration,
    summary: String,
    error: Option<AppError>,
}

/// The job's one History entry (R10): a summary comment, never a statement of the file.
async fn write_history(h: HistoryLine<'_>) -> bool {
    let text = format!(
        "-- Restore {} ({}) into {}: {}",
        h.file.name,
        &h.file.sha256[..h.file.sha256.len().min(12)],
        h.request.database,
        h.summary
    );
    let d = match h.context.cfg.kind {
        DbKind::MySql => Dialect::MySql(Default::default()),
        DbKind::Postgres => Dialect::Postgres,
    };
    let sql = dnt_query::redact::redact(&text, d);
    let error = h.error.as_ref();
    let entry = NewHistoryEntry {
        connection_id: h.context.conn,
        database: Some(h.request.database.clone()),
        sql_text: sql.text,
        redacted: sql.changed,
        kind: "write".into(),
        started_at: h.started_at,
        elapsed_ms: Some(h.elapsed.as_millis().min(u64::MAX as u128) as u64),
        status: if error.is_none() {
            HistoryStatus::Ok
        } else {
            HistoryStatus::Error
        },
        row_count: None,
        affected: None,
        error_kind: error.map(|e| e.kind.clone()),
        error_code: error.and_then(|e| e.server_code.clone().or_else(|| e.sqlstate.clone())),
        origin: HistoryOrigin::Restore,
        outcome_unknown: error.is_some_and(|e| e.outcome_unknown),
    };
    let repo = Arc::clone(&h.context.repo);
    matches!(
        tokio::task::spawn_blocking(move || repo.history_insert(&entry)).await,
        Ok(Ok(_))
    )
}

fn failure_app_error(f: &JobFailure) -> AppError {
    let mut e = AppError::from(f.error.clone());
    e.outcome_unknown = f.outcome_unknown;
    e
}

fn sentence(s: &str) -> String {
    let mut s = s.trim_end().to_string();
    if !s.ends_with('.') {
        s.push('.');
    }
    s
}

fn plural(n: u64, one: &str, many: &str) -> String {
    if n == 1 {
        format!("{n} {one}")
    } else {
        format!("{n} {many}")
    }
}

// ---------------------------------------------------------------------------------------
// App runner
// ---------------------------------------------------------------------------------------

/// The file as the job reads it: decompressed chunks, then the raw bytes' hash.
enum Chunk {
    Data { bytes: Vec<u8>, raw: u64 },
    End { sha256: String },
    Failed(String),
}

/// Reads the file on a blocking thread (gzip decoded, BOM stripped, raw bytes hashed).
fn spawn_file_reader(path: PathBuf) -> mpsc::Receiver<Chunk> {
    let (tx, rx) = mpsc::channel(CHANNEL_DEPTH);
    tokio::task::spawn_blocking(move || {
        let fail = |m: String| {
            let _ = tx.blocking_send(Chunk::Failed(m));
        };
        let file = match std::fs::File::open(&path) {
            Ok(f) => f,
            Err(e) => return fail(format!("Could not read the file: {e}.")),
        };
        let mut content = match Content::open(BufReader::with_capacity(CHUNK, file)) {
            Ok(c) => c,
            Err(e) => return fail(format!("Could not read the file: {e}.")),
        };
        let mut first = true;
        loop {
            let mut buf = vec![0u8; CHUNK];
            let mut n = 0;
            while n < buf.len() {
                match content.read(&mut buf[n..]) {
                    Ok(0) => break,
                    Ok(k) => n += k,
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                    Err(e) => return fail(format!("Could not read the file: {e}.")),
                }
            }
            if n == 0 {
                break;
            }
            buf.truncate(n);
            if first {
                first = false;
                if buf.starts_with(b"\xEF\xBB\xBF") {
                    buf.drain(..3);
                }
            }
            let raw = content.raw_bytes();
            if tx.blocking_send(Chunk::Data { bytes: buf, raw }).is_err() {
                return;
            }
        }
        match content.finish() {
            Ok((sha256, _)) => {
                let _ = tx.blocking_send(Chunk::End { sha256 });
            }
            Err(e) => fail(format!("Could not read the file: {e}.")),
        }
    });
    rx
}

/// Raises the run's stop signal, then cancels the running statement (the job's Stop).
struct StopRun(RunCancel);

#[async_trait]
impl Canceller for StopRun {
    async fn cancel(&self) -> Result<(), DbError> {
        self.0.cancel().await
    }
}

/// Server notices into the job log; results dropped.
struct JobSink<'a> {
    ctx: &'a JobCtx,
}

impl RowSink for JobSink<'_> {
    fn result_start(&mut self, _cols: &[ColumnMeta]) {}
    fn rows(&mut self, _batch: Vec<Vec<Value>>) {}
    fn result_end(&mut self, _summary: &ResultSummary) {}
}

impl RunSink for JobSink<'_> {
    fn statement_start(&mut self, _stmt: &PlannedStmt) {}
    fn statement_end(&mut self, _stmt: &PlannedStmt, _result: &Result<StmtOutcome, AppError>) {}
    fn notice(&mut self, _index: u32, message: &str) {
        self.ctx.log_server(JobLogLevel::Info, message, None);
    }
    fn tx_state(&mut self, _state: TxState) {}
}

/// The `DbError` of a statement's `AppError` (what `run_with` reports), for the job.
fn db_error(app: &AppError) -> DbError {
    let detail = app.detail.clone().unwrap_or_else(|| app.message.clone());
    match app.kind.as_str() {
        "server" => DbError::Server {
            code: app.server_code.clone(),
            sqlstate: app.sqlstate.clone(),
            message: detail,
            position: None,
        },
        "timeout" => DbError::Timeout {
            stage: ConnectStage::Execute,
            outcome_unknown: app.outcome_unknown,
            was_in_tx: false,
        },
        "connection_lost" => DbError::ConnectionLost {
            was_in_tx: false,
            outcome_unknown: app.outcome_unknown,
            message: app.detail.clone().unwrap_or_default(),
        },
        "cancelled" => DbError::Cancelled {
            outcome_unknown: app.outcome_unknown,
            was_in_tx: false,
        },
        "blocked" => DbError::Blocked { count: 1 },
        _ => DbError::Internal { message: detail },
    }
}

fn may_have_applied(e: &DbError) -> bool {
    match e {
        DbError::Timeout {
            outcome_unknown, ..
        }
        | DbError::ConnectionLost {
            outcome_unknown, ..
        }
        | DbError::Cancelled {
            outcome_unknown, ..
        } => *outcome_unknown,
        _ => false,
    }
}

fn with_outcome(e: DbError, unknown: bool) -> DbError {
    match e {
        DbError::Timeout {
            stage, was_in_tx, ..
        } => DbError::Timeout {
            stage,
            outcome_unknown: unknown,
            was_in_tx,
        },
        DbError::ConnectionLost {
            was_in_tx, message, ..
        } => DbError::ConnectionLost {
            was_in_tx,
            outcome_unknown: unknown,
            message,
        },
        DbError::Cancelled { was_in_tx, .. } => DbError::Cancelled {
            outcome_unknown: unknown,
            was_in_tx,
        },
        e => e,
    }
}

/// Sends the statements of one restore and keeps its counters.
struct Sender<'a> {
    ctx: &'a JobCtx,
    session: &'a dyn Session,
    context: &'a Context,
    cancel: RunCancel,
    opts: ExecOptions,
    reconnects: u64,
    in_tx: bool,
    commit_sent: bool,
    inflight: bool,
    sent_any: bool,
    statements: u64,
}

impl Sender<'_> {
    /// Sends one statement: never after Stop, never on a session that reconnected (the
    /// restore's session settings would be lost), never on a connection that changed or
    /// became read-only, never when the guard blocks it.
    async fn send(&mut self, text: &str, commit: bool) -> Result<(), DbError> {
        if self.ctx.is_stopped() || self.cancel.is_cancelled() {
            return Err(stopped());
        }
        if self.session.reconnects() != self.reconnects {
            return Err(DbError::ConnectionLost {
                was_in_tx: self.in_tx,
                outcome_unknown: false,
                message: "The connection was re-established during the restore; its session \
                          settings were lost."
                    .into(),
            });
        }
        self.context.check()?;
        let dialect = self.session.dialect();
        let mut plan = one_statement_plan(text, dialect);
        guard_with(
            &mut plan,
            guard_ctx(self.context.cfg.env_tag, self.context.cfg.read_only),
        );
        // `Confirm` verdicts are covered by the job's confirmation (R20, R32).
        if plan.stmts[0].verdict == Verdict::Block {
            return Err(DbError::Blocked { count: 1 });
        }
        self.sent_any = true;
        self.inflight = true;
        if commit {
            self.commit_sent = true;
        }
        let mut sink = JobSink { ctx: self.ctx };
        let report = run_with(
            self.session,
            &plan,
            &self.opts,
            RunConfig::default(),
            &self.cancel,
            &mut sink,
        )
        .await;
        let Some(done) = report.stmts.first() else {
            // Stopped before it was sent.
            self.inflight = false;
            if commit {
                self.commit_sent = false;
            }
            return Err(stopped());
        };
        match &done.result {
            Ok(_) => {
                self.inflight = false;
                self.statements += 1;
                if self.session.reconnects() != self.reconnects {
                    return Err(DbError::ConnectionLost {
                        was_in_tx: self.in_tx,
                        outcome_unknown: false,
                        message: "The connection was re-established during the restore; its \
                                  session settings were lost."
                            .into(),
                    });
                }
                Ok(())
            }
            Err(app) => {
                let e = db_error(app);
                if !may_have_applied(&e) {
                    self.inflight = false;
                }
                if let Some(code) = app.server_code.as_deref().or(app.sqlstate.as_deref()) {
                    self.ctx.log_server(
                        JobLogLevel::Error,
                        format!("Statement {}: {}", self.statements + 1, app.message),
                        Some(code),
                    );
                }
                Err(e)
            }
        }
    }

    async fn cleanup(&self, sql: &str, grace: Duration) -> Result<(), DbError> {
        execute(
            self.session,
            sql,
            StopSignal::new(),
            grace,
            CLEANUP_TIMEOUT_S,
        )
        .await
    }
}

/// The app runner's restore job.
pub(crate) struct AppRestoreJob {
    pub(crate) request: RestoreRequest,
    pub(crate) file: FileFacts,
    pub(crate) transactional: bool,
    pub(crate) dialect: Dialect,
    pub(crate) context: Context,
    pub(crate) grace: Duration,
}

#[async_trait]
impl JobRunner for AppRestoreJob {
    async fn run(self: Box<Self>, ctx: JobCtx) -> Result<JobDone, JobFailure> {
        let started = Instant::now();
        let started_at = crate::run_events::utc_millis_now();
        let (session, generation) = self
            .context
            .rt
            .open_query_session(Some(self.request.database.clone()))
            .await
            .map_err(JobFailure::from)?;
        let cancel = RunCancel::new(session.cancel_handle());
        ctx.set_cancel(Some(Arc::new(StopRun(cancel.clone()))));
        let timeout_s = self.context.cfg.options.query_timeout_s;
        let mut w = Sender {
            ctx: &ctx,
            session: session.as_ref(),
            context: &self.context,
            cancel,
            opts: ExecOptions {
                timeout: (timeout_s > 0).then(|| Duration::from_secs(u64::from(timeout_s))),
                cancel_grace: self.grace,
                ..ExecOptions::default()
            },
            reconnects: session.reconnects(),
            in_tx: false,
            commit_sent: false,
            inflight: false,
            sent_any: false,
            statements: 0,
        };
        let result = self.body(&mut w, generation).await;
        let outcome = match result {
            Ok(()) => Ok(JobDone {
                summary: format!(
                    "Restored {} into {} ({}{}).",
                    self.file.name,
                    self.request.database,
                    plural(w.statements, "statement", "statements"),
                    if self.transactional {
                        ", one transaction"
                    } else {
                        ""
                    }
                ),
                result_path: None,
            }),
            Err(e) => Err(self.failure(&w, e).await),
        };
        let sent_any = w.sent_any;
        drop(w);
        ctx.set_cancel(None);
        close_quietly(session).await;
        if sent_any {
            let (summary, error) = match &outcome {
                Ok(d) => (d.summary.clone(), None),
                Err(f) => (f.summary.clone(), Some(failure_app_error(f))),
            };
            let saved = write_history(HistoryLine {
                context: &self.context,
                request: &self.request,
                file: &self.file,
                started_at,
                elapsed: started.elapsed(),
                summary,
                error,
            })
            .await;
            if !saved {
                ctx.log(JobLogLevel::Warn, HISTORY_FAILED);
            }
        }
        outcome
    }
}

impl AppRestoreJob {
    async fn body(&self, w: &mut Sender<'_>, generation: u64) -> Result<(), DbError> {
        if generation != self.context.generation || w.session.dialect() != self.dialect {
            return Err(stale());
        }
        // The file as it is now (R21): a job that waited in the queue may find it changed.
        let now = hash(PathBuf::from(&self.request.path)).await?;
        if now.sha256 != self.file.sha256 || now.size != self.file.size {
            return Err(file_changed());
        }
        self.context.check()?;
        let target = scan_target(
            &self.request.database,
            self.dialect,
            self.context.rt.server_info(),
        );
        // The scan's prediction of the session dialect (R33): the job splits with it and
        // stops before sending when the session disagrees.
        let mut mode = ModeTracker::new(self.dialect, target.server);
        let ctx = w.ctx;
        if self.transactional {
            w.send("BEGIN", false).await?;
            w.in_tx = true;
        }
        let mut rx = spawn_file_reader(PathBuf::from(&self.request.path));
        let mut splitter = StreamSplitter::new(
            self.dialect,
            SplitOptions {
                detect_client: true,
                ..SplitOptions::default()
            },
        );
        let total = self.file.size;
        let mut raw = 0u64;
        let mut sha256: Option<String> = None;
        loop {
            if ctx.is_stopped() {
                return Err(stopped());
            }
            let event = splitter
                .next_event()
                .map_err(|e| file_refused(&format!("The file is not a SQL script: {e}")))?;
            match event {
                Some(SplitEvent::Statement(s)) => {
                    let d = mode.current();
                    let text = sent_text(&s.text, d);
                    if text.is_empty() {
                        continue;
                    }
                    if w.session.dialect() != d {
                        return Err(DbError::PlanStale {
                            reason: "The session's sql_mode is not what the analysis predicted; \
                                     nothing more was sent. Analyze the file again."
                                .into(),
                        });
                    }
                    if let Some(r) = statement_refusal(text, d, &target) {
                        return Err(file_refused(&r.0));
                    }
                    // The file may have been swapped after the start's check (R21): inside
                    // the job's transaction nothing may end it or refuse to run in it.
                    if self.transactional
                        && (matches!(classify(text, d), StatementKind::Transaction(_))
                            || pg_outside_tx_block(text))
                    {
                        return Err(file_refused(
                            "The file controls its own transaction or holds a statement that \
                             cannot run in one; it is not the file that was analyzed.",
                        ));
                    }
                    w.send(text, false).await?;
                    // R33: a `SET sql_mode` changes how the rest is lexed.
                    mode.after(text).map_err(|r| file_refused(&r.0))?;
                    if mode.current() != splitter.dialect() {
                        splitter.set_dialect(mode.current());
                    }
                    ctx.progress("Restoring", raw, Some(total), ProgressUnit::Bytes);
                }
                Some(SplitEvent::CopyData { .. }) | Some(SplitEvent::ClientCommand(_)) => {
                    return Err(file_refused(
                        "The file holds COPY data or psql commands and needs psql; analyze it \
                         again.",
                    ));
                }
                None if splitter.is_done() => break,
                None if sha256.is_some() => {
                    return Err(DbError::Internal {
                        message: "the statement splitter did not finish".into(),
                    })
                }
                None => match rx.recv().await {
                    Some(Chunk::Data { bytes, raw: r }) => {
                        raw = r;
                        splitter.push(&bytes);
                    }
                    Some(Chunk::End { sha256: s }) => {
                        sha256 = Some(s);
                        splitter.finish();
                    }
                    Some(Chunk::Failed(m)) => return Err(file_refused(&m)),
                    None => {
                        return Err(DbError::Internal {
                            message: "the file reader ended unexpectedly".into(),
                        })
                    }
                },
            }
        }
        ctx.progress("Restoring", total, Some(total), ProgressUnit::Bytes);
        if sha256.as_deref() != Some(self.file.sha256.as_str()) {
            return Err(DbError::PlanStale {
                reason: "The file changed while it was restored.".into(),
            });
        }
        if self.transactional {
            self.context.check()?;
            if ctx.is_stopped() {
                return Err(stopped());
            }
            w.send("COMMIT", true).await?;
            w.in_tx = false;
        }
        ctx.log(
            JobLogLevel::Info,
            format!(
                "{} sent.",
                plural(w.statements, "statement was", "statements were")
            ),
        );
        Ok(())
    }

    /// What a failed restore did (§6.6). An open transaction is rolled back.
    async fn failure(&self, w: &Sender<'_>, e: DbError) -> JobFailure {
        let base = match &e {
            DbError::PlanStale { reason } => reason.clone(),
            e => JobFailure::from(e.clone()).summary,
        };
        let base = sentence(&base);
        let (unknown, note) = if w.commit_sent && matches!(e, DbError::Server { .. }) {
            if w.session.in_transaction() {
                let _ = w.cleanup("ROLLBACK", self.grace).await;
            }
            (
                false,
                "The server refused the COMMIT; nothing was restored.".to_string(),
            )
        } else if w.commit_sent {
            (
                true,
                "COMMIT was sent but not answered; the outcome is unknown.".to_string(),
            )
        } else if w.in_tx {
            let rolled_back =
                w.session.in_transaction() && w.cleanup("ROLLBACK", self.grace).await.is_ok();
            (
                false,
                if rolled_back {
                    "The restore was rolled back; nothing was restored.".to_string()
                } else {
                    "Not committed; the server rolls the transaction back when it notices the \
                     lost connection."
                        .to_string()
                },
            )
        } else {
            let unknown = w.inflight && may_have_applied(&e);
            let mut note = if w.statements == 0 {
                "No statement ran.".to_string()
            } else {
                format!(
                    "{} ran before the error; the target may be partially restored.",
                    plural(w.statements, "statement", "statements")
                )
            };
            if unknown {
                note.push_str(" The last statement may or may not have been applied.");
            }
            (unknown, note)
        };
        JobFailure {
            error: with_outcome(e, unknown),
            outcome_unknown: unknown,
            summary: format!("{base} {note}"),
        }
    }
}

// ---------------------------------------------------------------------------------------
// Tools
// ---------------------------------------------------------------------------------------

/// A restore through psql or pg_restore (N7).
pub(crate) struct ToolRestoreJob {
    pub(crate) request: RestoreRequest,
    pub(crate) file: FileFacts,
    pub(crate) gzip: bool,
    pub(crate) transactional: bool,
    pub(crate) info: ToolInfo,
    pub(crate) context: Context,
    pub(crate) spawner: Arc<dyn Spawner>,
}

#[async_trait]
impl JobRunner for ToolRestoreJob {
    async fn run(self: Box<Self>, ctx: JobCtx) -> Result<JobDone, JobFailure> {
        let started = Instant::now();
        let started_at = crate::run_events::utc_millis_now();
        let mut spawned = false;
        let outcome = self.body(&ctx, &mut spawned).await;
        if spawned {
            let (summary, error) = match &outcome {
                Ok(d) => (d.summary.clone(), None),
                Err(f) => (f.summary.clone(), Some(failure_app_error(f))),
            };
            let saved = write_history(HistoryLine {
                context: &self.context,
                request: &self.request,
                file: &self.file,
                started_at,
                elapsed: started.elapsed(),
                summary: format!("{} ({summary})", self.info.tool.name()),
                error,
            })
            .await;
            if !saved {
                ctx.log(JobLogLevel::Warn, HISTORY_FAILED);
            }
        }
        outcome
    }
}

impl ToolRestoreJob {
    async fn body(&self, ctx: &JobCtx, spawned: &mut bool) -> Result<JobDone, JobFailure> {
        let name = self.info.tool.name();
        let now = hash(PathBuf::from(&self.request.path))
            .await
            .map_err(JobFailure::from)?;
        if now.sha256 != self.file.sha256 || now.size != self.file.size {
            return Err(JobFailure::from(file_changed()));
        }
        self.context.check().map_err(JobFailure::from)?;
        let (resolved, tunnelled) = self
            .context
            .rt
            .tool_endpoint(Some(self.request.database.clone()))
            .await
            .map_err(JobFailure::from)?;
        let tmp = ctx.tmp_dir().map_err(JobFailure::from)?;
        let inv = restore_invocation(
            &self.info,
            &ToolConn::from_resolved(&resolved, tunnelled),
            &self.request,
            self.transactional,
            &tmp.join(PGPASS_FILE),
        )
        .map_err(JobFailure::from)?;
        ctx.log(JobLogLevel::Info, format!("Tool: {}", self.info.version));
        ctx.log(
            JobLogLevel::Info,
            format!("Command: {}", shown_command(&inv)),
        );
        // Checked again right before the spawn: nothing runs on a read-only connection.
        self.context.check().map_err(JobFailure::from)?;
        if ctx.is_stopped() {
            return Err(JobFailure::from(stopped()));
        }
        *spawned = true;
        let stdin = (self.info.tool == Tool::Psql).then(|| StdinFeed {
            path: PathBuf::from(&self.request.path),
            gzip: self.gzip,
        });
        let ran = run_tool(
            self.spawner.as_ref(),
            ctx,
            ToolRun {
                inv: &inv,
                password: resolved.password.as_ref(),
                stdin,
                stdout: StdoutSink::Discard,
            },
        )
        .await;
        drop(resolved);
        let report = match ran {
            Ok(r) => r,
            Err(e) => {
                // Nothing was started (the secret file or the spawn failed).
                *spawned = false;
                return Err(JobFailure::from(e));
            }
        };
        if let Some(e) = &report.in_error {
            ctx.log(
                JobLogLevel::Error,
                format!("Reading the file for {name} failed: {e}"),
            );
        }
        let killed = report.killed || ctx.is_stopped();
        let outcome = if report.in_error.is_some() && report.code == Some(0) {
            // The tool saw a short input: it may have restored only part of the file.
            ToolOutcome::Partial
        } else {
            restore_outcome(self.info.tool, report.code, killed, self.transactional)
        };
        let code = report
            .code
            .map(|c| format!("exit code {c}"))
            .unwrap_or_else(|| "no exit code".into());
        match outcome {
            ToolOutcome::Done => Ok(JobDone {
                summary: format!(
                    "{name} restored {} into {}.",
                    self.file.name, self.request.database
                ),
                result_path: None,
            }),
            ToolOutcome::FailedRolledBack => {
                let summary = format!(
                    "{name} failed ({code}); the tool reported an error, the transaction was \
                     rolled back. The job log has its messages."
                );
                Err(JobFailure {
                    error: config_error("tool", &summary),
                    outcome_unknown: false,
                    summary,
                })
            }
            ToolOutcome::Partial if killed => Err(JobFailure {
                error: DbError::Cancelled {
                    outcome_unknown: true,
                    was_in_tx: false,
                },
                outcome_unknown: true,
                summary: format!(
                    "{name} was stopped part-way; the target may be partially restored."
                ),
            }),
            ToolOutcome::Partial => {
                let summary = format!(
                    "{name} stopped part-way ({code}); the target may be partially restored. \
                     The job log has its messages."
                );
                Err(JobFailure {
                    error: config_error("tool", &summary),
                    outcome_unknown: true,
                    summary,
                })
            }
        }
    }
}

#[cfg(test)]
#[path = "restore_tests.rs"]
mod tests;
