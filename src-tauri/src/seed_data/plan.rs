//! The seeder's preview, single-use plans and start (spec §5.1, §5.5): `seed_preview` (no
//! plan), `seed_prepare` (reads, checks, judges and stores a plan) and `seed_start`
//! (re-checks everything the plan depends on, then submits the job).
use super::keys::{key_id, read_fk_keys};
use super::run::{Context, SeedJob};
use super::target::transactional;
use super::{
    normalise, specs, ExistingRows, SeedKeyCount, SeedPreview, SeedRequest, SeedSample, SAMPLE_ROWS,
};
use crate::import_data::plan::pre_step;
use crate::import_data::target::{read_target_bounded, statement_atomic, TargetRead};
use crate::import_data::{
    build_error, guard_ctx, judge_all, session_db, table_ref, ConfirmLevel, ImportConfirm, Layout,
};
use crate::jobs::{JobKind, JobParams, JobSpec};
use crate::object_actions::stale;
use crate::session_manager::{
    close_quietly, config_error, lock, ConnectionRuntime, SessionManager,
};
use dnt_core::config::{ConnectionConfig, EnvTag};
use dnt_core::error::DbError;
use dnt_core::meta::{ForeignKeyDetail, TableDetail};
use dnt_core::server::SqlKind;
use dnt_core::sql::Dialect;
use dnt_query::Atomicity;
use dnt_transfer::batch::DEFAULT_BATCH_BYTES;
use dnt_transfer::mapping::ImportMode;
use dnt_transfer::seed::engine::{check_plans, FkKeys, SeedColumnSpec, Seeder};
use dnt_transfer::seed::{ColumnIssue, Generator};
use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::Instant;
use uuid::Uuid;

/// Seed plans kept at once.
pub(crate) const MAX_SEED_PLANS: usize = 64;

/// What a seed plan binds: the request, the table's fingerprint, the connection runtime /
/// tunnel generation / settings, the dialect and the templates the guard judged.
pub(crate) struct StoredSeed {
    preview: SeedPreview,
    request: SeedRequest,
    rt: Arc<ConnectionRuntime>,
    generation: u64,
    env: Option<EnvTag>,
    dialect: Dialect,
    fingerprint: u64,
    layout: Layout,
    statement_atomic: bool,
    templates: Vec<String>,
    read: TargetRead,
    expires: Instant,
}

/// Single-use seed plans by id (they live `plan_ttl`).
#[derive(Default)]
pub struct SeedPlanStore {
    plans: Mutex<HashMap<Uuid, StoredSeed>>,
}

impl SeedPlanStore {
    /// Closing `conn`: its plans are dropped.
    pub(crate) fn drop_conn(&self, conn: Uuid) {
        lock(&self.plans).retain(|_, p| p.request.connection_id != conn);
    }
}

/// The foreign keys the request's `ForeignKey` generators use (each once).
pub(crate) fn used_fks<'a>(
    req: &SeedRequest,
    detail: &'a TableDetail,
) -> Vec<&'a ForeignKeyDetail> {
    let mut out: Vec<&ForeignKeyDetail> = Vec::new();
    for plan in &req.columns {
        if let Generator::ForeignKey { constraint } = &plan.generator {
            if let Some(fk) = detail.foreign_keys.iter().find(|f| f.name == *constraint) {
                if !out.iter().any(|f| f.name == fk.name) {
                    out.push(fk);
                }
            }
        }
    }
    out
}

/// The engine's keys of one foreign key.
pub(crate) fn fk_keys(
    fk: &ForeignKeyDetail,
    keys: Arc<Vec<Vec<dnt_core::value::FullValue>>>,
) -> Arc<FkKeys> {
    Arc::new(FkKeys {
        constraint: fk.name.clone(),
        parent: fk.ref_table.clone(),
        columns: fk.columns.clone(),
        keys,
    })
}

/// The first rows of a seeder, as the preview shows them.
struct Sample {
    /// Display values of each row.
    rows: Vec<Vec<dnt_core::value::Value>>,
    /// The first batch's `INSERT`.
    sql: Option<String>,
    /// The issue of the row the engine refused (it ends the sample).
    issue: Option<ColumnIssue>,
}

/// The `INSERT` of the first rows of `seeder` (at most [`SAMPLE_ROWS`] and one batch), built
/// exactly as the job builds its first batch; also the rows' display values.
fn sample(
    seeder: &mut Seeder,
    layout: &Layout,
    rows: u32,
    batch_rows: u32,
) -> Result<Sample, DbError> {
    let mut builder =
        layout.builder(batch_rows.clamp(1, SAMPLE_ROWS), DEFAULT_BATCH_BYTES as u32)?;
    let mut out = Sample {
        rows: Vec::new(),
        sql: None,
        issue: None,
    };
    for r in 0..u64::from(rows.min(SAMPLE_ROWS)) {
        let values = match seeder.row(r) {
            Ok(v) => v,
            Err(issue) => {
                out.issue = Some(issue);
                return Ok(out);
            }
        };
        out.rows
            .push(values.iter().map(|v| v.to_display()).collect());
        if let Some(batch) = builder.push(values, r + 1).map_err(build_error)? {
            out.sql.get_or_insert(batch.sql);
        }
    }
    if out.sql.is_none() {
        out.sql = builder.flush().map(|b| b.sql);
    }
    Ok(out)
}

/// The pre-step mode of `existing`.
fn mode_of(existing: ExistingRows) -> ImportMode {
    match existing {
        ExistingRows::Keep => ImportMode::Append,
        ExistingRows::Delete => ImportMode::Replace { truncate: false },
        ExistingRows::Truncate => ImportMode::Replace { truncate: true },
    }
}

fn plural(n: u64, one: &str, many: &str) -> String {
    if n == 1 {
        format!("{n} {one}")
    } else {
        format!("{n} {many}")
    }
}

/// What the preview and the prepare read and check.
struct Read {
    cfg: ConnectionConfig,
    kind: SqlKind,
    request: SeedRequest,
    rt: Arc<ConnectionRuntime>,
    generation: u64,
    dialect: Dialect,
    read: TargetRead,
    specs: Vec<SeedColumnSpec>,
    key_counts: Vec<SeedKeyCount>,
    issues: Vec<ColumnIssue>,
}

impl SessionManager {
    /// Reads the table and the keys of its foreign keys (through the key cache) and checks
    /// the request's plans.
    async fn seed_read(&self, req: SeedRequest) -> Result<Read, DbError> {
        let cfg = self.repo.get(req.connection_id)?;
        let kind = cfg.kind.require_sql()?;
        let request = normalise(req, kind)?;
        let rt = self.runtime(request.connection_id)?;
        let (session, generation) = rt
            .open_query_session(session_db(kind, &request.table))
            .await?;
        let sql_session = session.sql()?;
        let dialect = sql_session.dialect();
        let result = async {
            let read = read_target_bounded(sql_session, &request.table).await?;
            let mut keys = HashMap::new();
            let mut key_counts = Vec::new();
            for fk in used_fks(&request, &read.detail) {
                let id = key_id(request.connection_id, generation, kind, &request.table, fk);
                let tuples = self
                    .seed_keys
                    .get_or_read(id, read_fk_keys(sql_session, &request.table, fk))
                    .await?;
                key_counts.push(SeedKeyCount {
                    constraint: fk.name.clone(),
                    count: tuples.len() as u32,
                });
                keys.insert(fk.name.clone(), fk_keys(fk, tuples));
            }
            Ok::<_, DbError>((read, keys, key_counts))
        }
        .await;
        close_quietly(session).await;
        let (read, keys, key_counts) = result?;
        let specs = specs(&request, &read.columns, &keys)?;
        let issues = check_plans(&specs, request.rows, kind);
        Ok(Read {
            cfg,
            kind,
            request,
            rt,
            generation,
            dialect,
            read,
            specs,
            key_counts,
            issues,
        })
    }

    /// The live preview (spec §5.1): the first rows exactly as the job generates them, or
    /// the column issues. No plan is stored.
    pub async fn seed_preview(&self, req: SeedRequest) -> Result<SeedSample, DbError> {
        let r = self.seed_read(req).await?;
        let mut out = SeedSample {
            columns: Vec::new(),
            rows: Vec::new(),
            sql: None,
            issues: r.issues.clone(),
            fk_keys: r.key_counts.clone(),
        };
        if !out.issues.is_empty() {
            return Ok(out);
        }
        let mut seeder = match Seeder::new(r.specs, r.request.seed, r.request.rows, r.kind) {
            Ok(s) => s,
            Err(issues) => {
                out.issues = issues;
                return Ok(out);
            }
        };
        let cols = seeder.columns();
        out.columns = cols.iter().map(|c| c.name.clone()).collect();
        if cols.is_empty() {
            return Ok(out);
        }
        let layout =
            Layout::from_columns(table_ref(r.kind, &r.request.table), cols, r.kind, r.dialect)?;
        let s = sample(&mut seeder, &layout, r.request.rows, r.request.batch_rows)?;
        out.rows = s.rows;
        out.sql = s.sql;
        out.issues.extend(s.issue);
        Ok(out)
    }

    /// Reads the table and the keys, checks the plans, builds and judges the statement
    /// templates and stores a single-use plan (spec §5.5). Read-only connections are refused.
    pub async fn seed_prepare(&self, req: SeedRequest) -> Result<SeedPreview, DbError> {
        if self.repo.get(req.connection_id)?.read_only {
            return Err(DbError::Blocked { count: 1 });
        }
        let r = self.seed_read(req).await?;
        if let Some(i) = r.issues.first() {
            return Err(config_error(
                "columns",
                &format!("{}: {}", i.column, i.message),
            ));
        }
        let kind = r.kind;
        let request = r.request;
        let mut seeder =
            Seeder::new(r.specs.clone(), request.seed, request.rows, kind).map_err(|issues| {
                let i = &issues[0];
                config_error("columns", &format!("{}: {}", i.column, i.message))
            })?;
        let cols = seeder.columns();
        match kind {
            SqlKind::Postgres if cols.is_empty() => {
                return Err(config_error(
                    "columns",
                    "Choose a generator for at least one column.",
                ));
            }
            SqlKind::MySql | SqlKind::Postgres => {}
        }
        let layout = Layout::from_columns(table_ref(kind, &request.table), cols, kind, r.dialect)?;
        let transactional = transactional(kind, &r.read);
        let mut pre = pre_step(&mode_of(request.existing), &layout.table, transactional);
        if let Some(p) = pre.as_mut() {
            p.warning = p
                .irreversible
                .then(|| "Existing rows are deleted even if the job fails later.".to_string());
        }
        let Sample {
            sql: sample_sql,
            issue,
            ..
        } = sample(&mut seeder, &layout, request.rows, request.batch_rows)?;
        if let Some(i) = issue {
            return Err(config_error(
                "columns",
                &format!("{}: {}", i.column, i.message),
            ));
        }
        let mut templates: Vec<String> = pre.iter().map(|p| p.sql.clone()).collect();
        templates.push(layout.template_statement().map_err(build_error)?);
        // `Confirm` verdicts are covered by the job's confirmation.
        judge_all(
            &templates,
            r.dialect,
            guard_ctx(r.cfg.env_tag, r.cfg.read_only),
            true,
        )?;
        let warnings = warnings(
            &request,
            &r.read,
            &r.specs,
            kind,
            transactional,
            pre.as_ref(),
        );
        let production = r.cfg.env_tag == Some(EnvTag::Production);
        let confirm = if request.existing != ExistingRows::Keep || production {
            ConfirmLevel::TypeName {
                name: request.table.name.clone(),
            }
        } else if r.cfg.env_tag == Some(EnvTag::Staging) {
            ConfirmLevel::Click
        } else {
            ConfirmLevel::None
        };
        let irreversible = pre.as_ref().is_some_and(|p| p.irreversible);
        let preview = SeedPreview {
            plan_id: Uuid::new_v4(),
            table: request.table.clone(),
            rows: request.rows,
            batches: request.rows.div_ceil(request.batch_rows.max(1)),
            transactional,
            pre_step: pre,
            insert_head: layout.head().map_err(build_error)?,
            insert_tail: layout.tail().map_err(build_error)?,
            sample: sample_sql,
            warnings,
            confirm,
            irreversible_name: (production && irreversible).then(|| request.table.name.clone()),
            expires_in_seconds: self.plan_ttl.as_secs().min(u32::MAX as u64) as u32,
        };
        let mut plans = lock(&self.seed_plans.plans);
        plans.retain(|_, p| p.expires > Instant::now());
        if plans.len() >= MAX_SEED_PLANS {
            return Err(DbError::Busy);
        }
        plans.insert(
            preview.plan_id,
            StoredSeed {
                preview: preview.clone(),
                statement_atomic: statement_atomic(&r.read.detail, kind),
                fingerprint: r.read.fingerprint,
                request,
                rt: r.rt,
                generation: r.generation,
                env: r.cfg.env_tag,
                dialect: r.dialect,
                layout,
                templates,
                read: r.read,
                expires: Instant::now() + self.plan_ttl,
            },
        );
        Ok(preview)
    }

    /// Re-checks the plan (connection, settings, read-only, guard, confirmation, the table's
    /// fingerprint) and submits the seed job. The plan is used up whatever the outcome.
    pub async fn seed_start(&self, plan_id: Uuid, confirm: ImportConfirm) -> Result<Uuid, DbError> {
        let stored = lock(&self.seed_plans.plans)
            .remove(&plan_id)
            .ok_or(DbError::PlanExpired)?;
        if stored.expires <= Instant::now() {
            return Err(DbError::PlanExpired);
        }
        self.check_seed_context(&stored)?;
        let name_ok = match &stored.preview.confirm {
            ConfirmLevel::TypeName { name } => confirm.name.as_ref() == Some(name),
            _ => true,
        };
        let irreversible = stored
            .preview
            .pre_step
            .as_ref()
            .is_some_and(|p| p.irreversible);
        if !confirm.confirmed || !name_ok || (irreversible && !confirm.irreversible_ack) {
            return Err(DbError::ConfirmationRequired { count: 1 });
        }
        let kind = stored.rt.cfg.kind.require_sql()?;
        let (session, generation) = stored
            .rt
            .open_query_session(session_db(kind, &stored.request.table))
            .await?;
        let checked = async {
            if generation != stored.generation || session.sql()?.dialect() != stored.dialect {
                return Err(stale());
            }
            let detail = tokio::time::timeout(
                crate::session_manager::METADATA_CALL_TIMEOUT,
                session.introspector().table_detail(&stored.request.table),
            )
            .await
            .map_err(|_| DbError::timeout(dnt_core::error::ConnectStage::Execute))??;
            if dnt_query::fingerprint(&detail) != stored.fingerprint {
                return Err(super::table_changed());
            }
            Ok(())
        }
        .await;
        close_quietly(session).await;
        checked?;
        self.check_seed_context(&stored)?;
        let request = stored.request;
        let spec = JobSpec {
            kind: JobKind::Seed,
            connection_id: Some(request.connection_id),
            title: format!("Generate data for {}", request.table.name),
            params: serde_json::to_value(&request).ok().map(JobParams::Seed),
            rerunnable: true,
        };
        let job = SeedJob {
            pre_step: stored.preview.pre_step.clone(),
            transactional: stored.preview.transactional,
            statement_atomic: stored.statement_atomic,
            fingerprint: stored.fingerprint,
            dialect: stored.dialect,
            templates: stored.templates,
            layout: stored.layout,
            read: stored.read,
            context: Context {
                repo: Arc::clone(&self.repo),
                rt: stored.rt,
                conn: request.connection_id,
                env: stored.env,
                generation: stored.generation,
            },
            grace: self.cancel_grace,
            request,
        };
        self.jobs.submit(spec, Box::new(job))
    }

    fn check_seed_context(&self, stored: &StoredSeed) -> Result<(), DbError> {
        let conn = stored.request.connection_id;
        let cfg = self.repo.get(conn)?;
        if cfg.read_only {
            return Err(DbError::Blocked { count: 1 });
        }
        if cfg.env_tag != stored.env || cfg != stored.rt.cfg {
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
        judge_all(
            &stored.templates,
            stored.dialect,
            guard_ctx(cfg.env_tag, cfg.read_only),
            true,
        )
    }
}

/// The review's warnings (spec §5.5).
fn warnings(
    request: &SeedRequest,
    read: &TargetRead,
    specs: &[SeedColumnSpec],
    kind: SqlKind,
    transactional: bool,
    pre: Option<&crate::import_data::PreStep>,
) -> Vec<String> {
    let mut out = Vec::new();
    if !transactional {
        out.push("Not transactional: batches already written stay if the job stops.".into());
    }
    match kind {
        SqlKind::MySql => {
            if read.atomicity == Atomicity::Unknown && !read.detail.triggers.is_empty() {
                out.push(
                    "The table has triggers: the job runs without one transaction (each batch \
                     commits on its own)."
                        .into(),
                );
            }
        }
        SqlKind::Postgres => {}
    }
    if request.existing == ExistingRows::Truncate {
        out.push(match kind {
            SqlKind::MySql => "TRUNCATE commits at once and resets AUTO_INCREMENT.".into(),
            SqlKind::Postgres => "TRUNCATE also empties the tables that inherit from this one \
                                   and fires ON TRUNCATE triggers."
                .into(),
        });
    }
    if let Some(w) = pre.and_then(|p| p.warning.clone()) {
        out.push(w);
    }
    let mut any_fk = false;
    for s in specs {
        if let (Generator::ForeignKey { .. }, Some(k)) = (&s.plan.generator, &s.fk) {
            any_fk = true;
            if k.keys.is_empty() && s.column.nullable {
                out.push(format!(
                    "{} has no rows: {} is always NULL.",
                    k.parent, s.column.name
                ));
            }
        }
    }
    if any_fk {
        out.push(
            "Keys of referenced tables are read again when the job starts; rows may differ from \
             the preview."
                .into(),
        );
    }
    let generated: Vec<&str> = specs
        .iter()
        .filter(|s| s.plan.generator != Generator::Skip)
        .map(|s| s.column.name.as_str())
        .collect();
    let composite = read
        .detail
        .primary_key
        .iter()
        .chain(&read.detail.indexes)
        .any(|ix| {
            (ix.unique || ix.primary)
                && !ix.partial
                && ix.columns.len() > 1
                && ix.columns.iter().all(|c| generated.contains(&c.as_str()))
        });
    if composite {
        out.push(
            "The table has a unique key over several columns: duplicates fail the job.".into(),
        );
    }
    out
}

/// "N rows in M batches" for the job's summary.
pub(crate) fn summary(rows: u64, table: &str, batches: u64) -> String {
    format!(
        "Inserted {} into {table} in {}",
        plural(rows, "row", "rows"),
        plural(batches, "batch", "batches")
    )
}
