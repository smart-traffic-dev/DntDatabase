//! The seed job (spec §5.4). Statement sequences (`[]` = optional):
//! - PostgreSQL: `SET standard_conforming_strings = on`, `BEGIN`, [`DELETE FROM t` |
//!   `TRUNCATE t`], the key reads, batches, `COMMIT`.
//! - MySQL, transactional table: [`TRUNCATE t` (irreversible, autocommit)], `START
//!   TRANSACTION`, [`DELETE FROM t`], the key reads, batches, `COMMIT`.
//! - MySQL, other tables: [`DELETE FROM t` | `TRUNCATE t` (irreversible)], the key reads,
//!   batches in autocommit.
//!
//! Rows are generated on a blocking thread and handed to the import's `Writer` (guard before
//! every statement, never after Stop); the thread ends when the job drops its receiver.
use super::keys::read_fk_keys;
use super::plan::{fk_keys, summary, used_fks};
use super::{specs, ExistingRows, SeedRequest};
use crate::ddl_exec::{stopped, HISTORY_FAILED};
use crate::import_data::run::{describe_failure, FailureWords, Msg, PreStepState, Writer};
use crate::import_data::target::TargetRead;
use crate::import_data::{guard_ctx, judge_all, session_db, Layout, PreStep};
use crate::jobs::{JobCtx, JobDone, JobFailure, JobLogLevel, JobRunner, ProgressUnit};
use crate::object_actions::stale;
use crate::session_manager::{close_quietly, config_error, METADATA_CALL_TIMEOUT};
use async_trait::async_trait;
use dnt_core::error::{AppError, DbError};
use dnt_core::server::SqlKind;
use dnt_core::sql::Dialect;
use dnt_store::history::{HistoryOrigin, HistoryStatus, NewHistoryEntry};
use dnt_transfer::batch::DEFAULT_BATCH_BYTES;
use dnt_transfer::seed::engine::Seeder;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

pub(crate) use crate::import_data::run::Context;

/// Rows in flight between the generator thread and the job.
const CHANNEL_DEPTH: usize = 256;

const SEED_WORDS: FailureWords = FailureWords {
    rolled_back: "The job was rolled back; no row was inserted.",
    refused_commit: "The server refused the COMMIT; no row was inserted.",
};

/// Everything a started seed job needs (copied from its plan).
pub(crate) struct SeedJob {
    pub(crate) request: SeedRequest,
    pub(crate) layout: Layout,
    pub(crate) pre_step: Option<PreStep>,
    pub(crate) transactional: bool,
    pub(crate) statement_atomic: bool,
    pub(crate) fingerprint: u64,
    pub(crate) dialect: Dialect,
    /// The plan's templates, judged again before anything is sent.
    pub(crate) templates: Vec<String>,
    /// The table as the plan read it (columns, foreign keys).
    pub(crate) read: TargetRead,
    pub(crate) context: Context,
    pub(crate) grace: Duration,
}

/// Generates every row of `seeder` on a blocking thread. A row the engine refuses ends the
/// stream with its issue; the thread ends when the receiver is dropped.
fn spawn_generator(mut seeder: Seeder, rows: u32) -> mpsc::Receiver<Msg> {
    let (tx, rx) = mpsc::channel(CHANNEL_DEPTH);
    tokio::task::spawn_blocking(move || {
        let total = u64::from(rows);
        for r in 0..total {
            let msg = match seeder.row(r) {
                Ok(values) => Msg::row(r + 1, values, r + 1, total),
                Err(issue) => Msg::failed(format!("{}: {}", issue.column, issue.message)),
            };
            let failed = msg.is_failed();
            if tx.blocking_send(msg).is_err() || failed {
                return;
            }
        }
        let _ = tx.blocking_send(Msg::end(total));
    });
    rx
}

#[async_trait]
impl JobRunner for SeedJob {
    async fn run(self: Box<Self>, ctx: JobCtx) -> Result<JobDone, JobFailure> {
        let started = Instant::now();
        let started_at = crate::run_events::utc_millis_now();
        let kind = self.layout.kind;
        let (session, generation) = self
            .context
            .rt
            .open_query_session(session_db(kind, &self.request.table))
            .await
            .map_err(JobFailure::from)?;
        let sql_session = session.sql().map_err(JobFailure::from)?;
        ctx.set_cancel(Some(session.cancel_handle()));
        let mut w = Writer::new(
            &ctx,
            sql_session,
            &self.context,
            self.dialect,
            guard_ctx(self.context.env, false),
            true,
            self.grace,
            self.context.rt.cfg.options.query_timeout_s,
            "Generating",
            ProgressUnit::Rows,
        );
        let mut pre = PreStepState::NotSent;
        let result = self.body(&mut w, generation, &mut pre).await;
        let (outcome, committed) = match result {
            Ok(()) => (
                Ok(JobDone {
                    summary: summary(w.imported, &self.request.table.name, w.written),
                    result_path: None,
                }),
                Some(w.imported),
            ),
            Err(e) => {
                let (f, committed) =
                    describe_failure(&w, e, pre, self.statement_atomic, &SEED_WORDS).await;
                (Err(f), committed)
            }
        };
        let sent_any = w.sent_any;
        ctx.set_cancel(None);
        close_quietly(session).await;
        if sent_any {
            let error = outcome.as_ref().err().map(|f| {
                let mut e = AppError::from(f.error.clone());
                e.outcome_unknown = f.outcome_unknown;
                e
            });
            if !self
                .history(&started_at, started.elapsed(), committed, error.as_ref())
                .await
            {
                ctx.log(JobLogLevel::Warn, HISTORY_FAILED);
            }
        }
        outcome
    }
}

impl SeedJob {
    async fn body(
        &self,
        w: &mut Writer<'_>,
        generation: u64,
        pre: &mut PreStepState,
    ) -> Result<(), DbError> {
        if generation != self.context.generation || w.session.dialect() != self.dialect {
            return Err(stale());
        }
        let kind = self.layout.kind;
        let begin = match kind {
            SqlKind::MySql => "START TRANSACTION",
            SqlKind::Postgres => {
                w.send("SET standard_conforming_strings = on", None).await?;
                "BEGIN"
            }
        };
        // The table as it is now: a changed table is stale, nothing is written.
        let detail = tokio::time::timeout(
            METADATA_CALL_TIMEOUT,
            w.session.introspector().table_detail(&self.request.table),
        )
        .await
        .map_err(|_| DbError::timeout(dnt_core::error::ConnectStage::Execute))??;
        if dnt_query::fingerprint(&detail) != self.fingerprint {
            return Err(super::table_changed());
        }
        self.context.check()?;
        judge_all(&self.templates, self.dialect, w.guard, true)?;
        let ctx = w.ctx;
        if let Some(p) = self.pre_step.as_ref().filter(|p| p.irreversible) {
            *pre = PreStepState::Sent;
            w.send(&p.sql, Some(0)).await?;
            *pre = PreStepState::Done;
            ctx.log(
                JobLogLevel::Info,
                "Existing rows deleted (irreversible pre-step, committed).",
            );
        }
        if self.transactional {
            w.send(begin, None).await?;
            w.in_tx = true;
        }
        if let Some(p) = self.pre_step.as_ref().filter(|p| !p.irreversible) {
            w.send(&p.sql, Some(0)).await?;
            ctx.log(
                JobLogLevel::Info,
                "Existing rows deleted (in the transaction).",
            );
        }
        // The parents' keys as they are now (a fresh read, not the preview's cache).
        let mut keys = HashMap::new();
        for fk in used_fks(&self.request, &self.read.detail) {
            if ctx.is_stopped() {
                return Err(stopped());
            }
            let tuples = read_fk_keys(w.session, &self.request.table, fk).await?;
            ctx.log(
                JobLogLevel::Info,
                format!("Read {} keys of {}.", tuples.len(), fk.ref_table),
            );
            keys.insert(fk.name.clone(), fk_keys(fk, Arc::new(tuples)));
        }
        let specs = specs(&self.request, &self.read.columns, &keys)?;
        let seeder =
            Seeder::new(specs, self.request.seed, self.request.rows, kind).map_err(|issues| {
                let i = &issues[0];
                config_error("columns", &format!("{}: {}", i.column, i.message))
            })?;
        let mut builder = self
            .layout
            .builder(self.request.batch_rows, DEFAULT_BATCH_BYTES as u32)?;
        let mut rx = spawn_generator(seeder, self.request.rows);
        w.pump(&mut rx, &mut builder, &self.layout).await?;
        drop(rx);
        if self.transactional {
            self.context.check()?;
            if ctx.is_stopped() {
                return Err(stopped());
            }
            w.send_commit().await?;
            w.in_tx = false;
        }
        Ok(())
    }

    /// The job's one History entry: a summary comment, the pre-step and the `INSERT`
    /// template; never a generated value.
    async fn history(
        &self,
        started_at: &str,
        elapsed: Duration,
        committed: Option<u64>,
        error: Option<&AppError>,
    ) -> bool {
        let head = self.layout.head().unwrap_or_default();
        let tail = self.layout.tail().unwrap_or_default();
        let existing = match self.request.existing {
            ExistingRows::Keep => "keep",
            ExistingRows::Delete => "delete first",
            ExistingRows::Truncate => "truncate first",
        };
        let mut text = format!(
            "-- Generate {} rows into {}: seed {}, {existing}\n",
            self.request.rows, self.request.table.name, self.request.seed,
        );
        if let Some(p) = &self.pre_step {
            text.push_str(&p.sql);
            text.push_str(";\n");
        }
        text.push_str(&format!("{head}…{tail}"));
        let sql = dnt_query::redact::redact(&text, self.dialect);
        let entry = NewHistoryEntry {
            connection_id: self.context.conn,
            database: Some(self.request.table.database.clone()),
            sql_text: sql.text,
            redacted: sql.changed,
            kind: "write".into(),
            started_at: started_at.to_string(),
            elapsed_ms: Some(elapsed.as_millis().min(u64::MAX as u128) as u64),
            status: if error.is_none() {
                HistoryStatus::Ok
            } else {
                HistoryStatus::Error
            },
            row_count: None,
            affected: committed,
            error_kind: error.map(|e| e.kind.clone()),
            error_code: error.and_then(|e| e.server_code.clone().or_else(|| e.sqlstate.clone())),
            origin: HistoryOrigin::Seed,
            outcome_unknown: error.is_some_and(|e| e.outcome_unknown),
        };
        let repo = Arc::clone(&self.context.repo);
        matches!(
            tokio::task::spawn_blocking(move || repo.history_insert(&entry)).await,
            Ok(Ok(_))
        )
    }
}
