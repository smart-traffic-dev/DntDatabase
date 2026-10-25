//! Import plans (R19–R21): `import_prepare` reads the target and the file, builds the
//! statement templates, judges them and stores a single-use plan; `import_start`
//! re-checks everything the plan depends on and submits the import job. `import_dry_run`
//! submits a dry-run job (R18).
use super::run::{
    check_sources, create_temp_sql, file_changed_before, file_columns, hash_file, temp_table,
    Context, DryRunJob, ImportJob,
};
use super::target::{read_target_bounded, statement_atomic, table_changed, TargetRead};
use super::{
    build_error, guard_ctx, judge_all, normalise, session_db, table_ref, ConfirmLevel,
    DryRunRequest, ImportConfirm, ImportPreview, ImportRequest, Layout, PreStep, SAMPLE_ROWS,
};
use crate::jobs::{JobKind, JobParams, JobSpec};
use crate::object_actions::stale;
use crate::session_manager::{
    close_quietly, config_error, lock, ConnectionRuntime, SessionManager,
};
use dnt_core::config::EnvTag;
use dnt_core::error::DbError;
use dnt_core::meta::TableDetail;
use dnt_core::server::{DbKind, ServerInfo, ServerVariant};
use dnt_core::sql::build::{insert_head, ColumnTarget, InsertOptions, OnConflict, TableRef};
use dnt_core::sql::{Dialect, MySqlMode};
use dnt_core::value::LogicalType;
use dnt_query::Atomicity;
use dnt_transfer::mapping::{check_mapping, ImportMode, MappingReport};
use dnt_transfer::read::SourceOptions;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::Instant;
use uuid::Uuid;

/// Import plans kept at once.
pub(crate) const MAX_IMPORT_PLANS: usize = 64;

/// What an import plan binds (R19): the request, the file's facts, the table's
/// fingerprint, the connection runtime / tunnel generation / settings, the dialect and the
/// templates the guard judged.
pub(crate) struct StoredImport {
    preview: ImportPreview,
    request: ImportRequest,
    rt: Arc<ConnectionRuntime>,
    generation: u64,
    env: Option<EnvTag>,
    dialect: Dialect,
    fingerprint: u64,
    layout: Layout,
    statement_atomic: bool,
    templates: Vec<String>,
    expires: Instant,
}

/// Single-use import plans by id (they live `plan_ttl`).
#[derive(Default)]
pub struct ImportPlanStore {
    plans: Mutex<HashMap<Uuid, StoredImport>>,
}

impl ImportPlanStore {
    /// Closing `conn`: its plans are dropped.
    pub(crate) fn drop_conn(&self, conn: Uuid) {
        lock(&self.plans).retain(|_, p| p.request.connection_id != conn);
    }
}

/// The mapping's errors as one `config` error (field `mapping`).
pub(crate) fn mapping_error(report: &MappingReport) -> DbError {
    let text: Vec<&str> = report.errors.iter().map(|e| e.message.as_str()).collect();
    config_error("mapping", &text.join(" "))
}

/// The pre-step of a replace-all import (R17): PostgreSQL and transactional MySQL
/// targets delete inside the transaction; MySQL `TRUNCATE` (implicit commit) and
/// non-transactional targets delete first, irreversibly.
fn pre_step(mode: &ImportMode, table: &TableRef, transactional: bool) -> Option<PreStep> {
    let ImportMode::Replace { truncate } = mode else {
        return None;
    };
    let rel = table.relation();
    let sql = if *truncate {
        format!("TRUNCATE {rel}")
    } else {
        format!("DELETE FROM {rel}")
    };
    let irreversible = table.kind == DbKind::MySql && (*truncate || !transactional);
    Some(PreStep {
        sql,
        irreversible,
        warning: irreversible
            .then(|| "Existing rows are deleted even if the import fails later.".to_string()),
    })
}

/// Unique keys of the table other than `key` (MySQL upsert updates on any of them, R14).
fn other_unique_keys(detail: &TableDetail, key: &[String]) -> bool {
    let wanted: HashSet<&str> = key.iter().map(String::as_str).collect();
    detail
        .primary_key
        .iter()
        .chain(&detail.indexes)
        .filter(|ix| ix.unique || ix.primary)
        .any(|ix| {
            ix.columns.len() != wanted.len()
                || !ix.columns.iter().all(|c| wanted.contains(c.as_str()))
        })
}

fn warnings(
    request: &ImportRequest,
    read: &TargetRead,
    server: &ServerInfo,
    transactional: bool,
    pre: Option<&PreStep>,
    report: &MappingReport,
    layout: &Layout,
) -> Vec<String> {
    let (detail, atomicity) = (&read.detail, read.atomicity);
    let mysql = server.kind == DbKind::MySql;
    let mut out = Vec::new();
    if !transactional {
        out.push("Not transactional: batches already written stay if the import stops.".into());
    }
    if mysql && atomicity == Atomicity::Unknown && !detail.triggers.is_empty() {
        out.push(
            "The table has triggers: the import runs without one transaction (each batch \
             commits on its own)."
                .into(),
        );
    }
    if let ImportMode::Upsert { key } = &request.mode {
        if mysql && other_unique_keys(detail, key) {
            out.push(
                "The table has other unique keys: MySQL updates the row matching any unique key."
                    .into(),
            );
        }
        let updates = matches!(&layout.opts.on_conflict, OnConflict::Update { update, .. } if !update.is_empty());
        if mysql
            && updates
            && server.variant == ServerVariant::MySql8
            && server.version >= (8, 0, 20)
        {
            out.push(
                "MySQL 8.0.20 and later report VALUES() in ON DUPLICATE KEY UPDATE as \
                 deprecated (a warning); the import still works."
                    .into(),
            );
        }
    }
    if let ImportMode::Replace { truncate: true } = &request.mode {
        out.push(if mysql {
            "TRUNCATE commits at once and resets AUTO_INCREMENT.".into()
        } else {
            "TRUNCATE also empties the tables that inherit from this one and fires ON \
             TRUNCATE triggers."
                .into()
        });
    }
    if let Some(w) = pre.and_then(|p| p.warning.clone()) {
        out.push(w);
    }
    out.extend(report.warnings.iter().map(|w| w.message.clone()));
    out
}

/// The first rows of the file (at most [`SAMPLE_ROWS`]) as one `INSERT`, built exactly as
/// the job builds its batches; `None` when no row of the first ones converts.
async fn sample(
    path: PathBuf,
    source: SourceOptions,
    layout: Layout,
    max_bytes: u32,
) -> Result<Option<String>, DbError> {
    tokio::task::spawn_blocking(move || {
        let mut reader = dnt_transfer::read::open(&path, &source)
            .map_err(|e| config_error("file", &e.to_string()))?;
        let mut builder = layout.builder(SAMPLE_ROWS, max_bytes)?;
        let mut seen = 0u32;
        while seen < SAMPLE_ROWS * 10 {
            let Some(rec) = reader
                .next_record()
                .map_err(|e| config_error("file", &e.to_string()))?
            else {
                break;
            };
            seen += 1;
            let Ok(row) = layout.convert(&rec) else {
                continue;
            };
            if let Ok(Some(batch)) = builder.push(row, rec.number) {
                return Ok(Some(batch.sql));
            }
        }
        Ok(builder.flush().map(|b| b.sql))
    })
    .await
    .map_err(|e| DbError::Internal {
        message: format!("reading the file failed: {e}"),
    })?
}

/// The temporary table statements of a server-check dry run, as the guard sees them.
fn dry_run_statements(req: &ImportRequest, kind: DbKind) -> Result<Vec<String>, DbError> {
    let target = table_ref(kind, &req.table);
    let tmp = temp_table(kind, &target);
    let cols: Vec<ColumnTarget> = req
        .mapping
        .iter()
        .filter(|m| m.source.is_some())
        .map(|m| ColumnTarget {
            name: m.target.clone(),
            db_type: String::new(),
            logical: LogicalType::Text,
            collation: None,
        })
        .collect();
    let opts = InsertOptions {
        overriding_system_value: req.override_identity,
        on_conflict: OnConflict::Error,
    };
    let d = dialect_of(kind);
    let head = insert_head(&tmp, &cols, &opts, d).map_err(build_error)?;
    let nulls = vec!["NULL"; cols.len()].join(", ");
    Ok(vec![
        create_temp_sql(kind, &tmp, &target),
        format!("{head}({nulls})"),
    ])
}

fn dialect_of(kind: DbKind) -> Dialect {
    match kind {
        DbKind::MySql => Dialect::MySql(MySqlMode::default()),
        DbKind::Postgres => Dialect::Postgres,
    }
}

fn job_title(verb: &str, path: &str, table: &str, joiner: &str) -> String {
    let name = PathBuf::from(path)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string());
    format!("{verb} {name} {joiner} {table}")
}

impl SessionManager {
    /// Reads the target and the file, builds and judges the statement templates and
    /// stores a single-use plan (R19). Read-only connections are refused.
    pub async fn import_prepare(&self, request: ImportRequest) -> Result<ImportPreview, DbError> {
        let conn = request.connection_id;
        let cfg = self.repo.get(conn)?;
        let request = normalise(request, cfg.kind)?;
        if cfg.read_only {
            return Err(DbError::Blocked { count: 1 });
        }
        let rt = self.runtime(conn)?;
        let path = PathBuf::from(&request.path);
        let file = hash_file(path.clone()).await?;
        let (session, generation) = rt
            .open_query_session(session_db(cfg.kind, &request.table))
            .await?;
        let dialect = session.dialect();
        let server = session.server_info().clone();
        let read = read_target_bounded(session.as_ref(), &request.table).await;
        close_quietly(session).await;
        let read = read?;
        let report = check_mapping(
            &request.mapping,
            &read.columns,
            &read.keys,
            &request.mode,
            request.override_identity,
        );
        if !report.errors.is_empty() {
            return Err(mapping_error(&report));
        }
        let layout = Layout::new(&request, &read.columns, cfg.kind, dialect)?;
        check_sources(
            &request,
            file_columns(path.clone(), request.source.clone()).await?,
        )?;
        let transactional = cfg.kind == DbKind::Postgres || read.atomicity == Atomicity::Yes;
        let pre = pre_step(&request.mode, &layout.table, transactional);
        let insert_head = layout.head().map_err(build_error)?;
        let insert_tail = layout.tail().map_err(build_error)?;
        let sample = sample(
            path,
            request.source.clone(),
            layout.clone(),
            request.batch_bytes,
        )
        .await?;
        let mut templates: Vec<String> = pre.iter().map(|p| p.sql.clone()).collect();
        templates.push(layout.template_statement().map_err(build_error)?);
        // `Confirm` verdicts are covered by the job's confirmation (R20).
        judge_all(
            &templates,
            dialect,
            guard_ctx(cfg.env_tag, cfg.read_only),
            true,
        )?;
        let warnings = warnings(
            &request,
            &read,
            &server,
            transactional,
            pre.as_ref(),
            &report,
            &layout,
        );
        let production = cfg.env_tag == Some(EnvTag::Production);
        let confirm = match cfg.env_tag {
            Some(EnvTag::Production) => ConfirmLevel::TypeName {
                name: request.table.name.clone(),
            },
            Some(EnvTag::Staging) => ConfirmLevel::Click,
            _ => ConfirmLevel::None,
        };
        let irreversible = pre.as_ref().is_some_and(|p| p.irreversible);
        let preview = ImportPreview {
            plan_id: Uuid::new_v4(),
            file,
            table: request.table.clone(),
            transactional,
            pre_step: pre,
            insert_head,
            insert_tail,
            sample,
            warnings,
            confirm,
            irreversible_name: (production && irreversible).then(|| request.table.name.clone()),
            expires_in_seconds: self.plan_ttl.as_secs().min(u32::MAX as u64) as u32,
        };
        let mut plans = lock(&self.import_plans.plans);
        plans.retain(|_, p| p.expires > Instant::now());
        if plans.len() >= MAX_IMPORT_PLANS {
            return Err(DbError::Busy);
        }
        plans.insert(
            preview.plan_id,
            StoredImport {
                preview: preview.clone(),
                statement_atomic: statement_atomic(&read.detail, cfg.kind),
                request,
                rt,
                generation,
                env: cfg.env_tag,
                dialect,
                fingerprint: read.fingerprint,
                layout,
                templates,
                expires: Instant::now() + self.plan_ttl,
            },
        );
        Ok(preview)
    }

    /// Re-checks the plan (connection, settings, read-only, guard, confirmation, the
    /// file's SHA-256, the table's fingerprint) and submits the import job. The plan is
    /// used up whatever the outcome; nothing is sent before every check passed.
    pub async fn import_start(
        &self,
        plan_id: Uuid,
        confirm: ImportConfirm,
    ) -> Result<Uuid, DbError> {
        let stored = lock(&self.import_plans.plans)
            .remove(&plan_id)
            .ok_or(DbError::PlanExpired)?;
        if stored.expires <= Instant::now() {
            return Err(DbError::PlanExpired);
        }
        self.check_import_context(&stored)?;
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
        let file = hash_file(PathBuf::from(&stored.request.path)).await?;
        if file.sha256 != stored.preview.file.sha256 || file.size != stored.preview.file.size {
            return Err(file_changed_before());
        }
        let (session, generation) = stored
            .rt
            .open_query_session(session_db(stored.rt.cfg.kind, &stored.request.table))
            .await?;
        let checked = async {
            if generation != stored.generation || session.dialect() != stored.dialect {
                return Err(stale());
            }
            let detail = tokio::time::timeout(
                crate::session_manager::METADATA_CALL_TIMEOUT,
                session.introspector().table_detail(&stored.request.table),
            )
            .await
            .map_err(|_| DbError::timeout(dnt_core::error::ConnectStage::Execute))??;
            if dnt_query::fingerprint(&detail) != stored.fingerprint {
                return Err(table_changed());
            }
            Ok(())
        }
        .await;
        close_quietly(session).await;
        checked?;
        self.check_import_context(&stored)?;
        let request = stored.request;
        let spec = JobSpec {
            kind: JobKind::Import,
            connection_id: Some(request.connection_id),
            title: job_title("Import", &request.path, &request.table.name, "into"),
            params: serde_json::to_value(&request).ok().map(JobParams::Import),
            rerunnable: true,
        };
        let job = ImportJob {
            file: stored.preview.file.clone(),
            pre_step: stored.preview.pre_step.clone(),
            transactional: stored.preview.transactional,
            statement_atomic: stored.statement_atomic,
            fingerprint: stored.fingerprint,
            dialect: stored.dialect,
            templates: stored.templates,
            layout: stored.layout,
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

    /// Submits a dry-run job (R18). The server check needs a connection that is not
    /// read-only, and `confirmed` where the guard asks for it (Production).
    pub async fn import_dry_run(&self, req: DryRunRequest) -> Result<Uuid, DbError> {
        let conn = req.request.connection_id;
        let cfg = self.repo.get(conn)?;
        let request = normalise(req.request, cfg.kind)?;
        let columns = file_columns(PathBuf::from(&request.path), request.source.clone()).await?;
        check_sources(&request, columns)?;
        if req.server_check {
            if cfg.read_only {
                return Err(DbError::Blocked { count: 1 });
            }
            let statements = dry_run_statements(&request, cfg.kind)?;
            judge_all(
                &statements,
                dialect_of(cfg.kind),
                guard_ctx(cfg.env_tag, cfg.read_only),
                req.confirmed,
            )?;
        }
        let rt = self.runtime(conn)?;
        let generation = rt.tunnel_gen.load(Ordering::SeqCst);
        let params = DryRunRequest {
            request: request.clone(),
            server_check: req.server_check,
            confirmed: false,
        };
        let spec = JobSpec {
            kind: JobKind::ImportDryRun,
            connection_id: Some(conn),
            title: job_title("Check", &request.path, &request.table.name, "for"),
            params: serde_json::to_value(&params)
                .ok()
                .map(JobParams::ImportDryRun),
            rerunnable: true,
        };
        let job = DryRunJob {
            server_check: req.server_check,
            confirmed: req.confirmed,
            context: Context {
                repo: Arc::clone(&self.repo),
                rt,
                conn,
                env: cfg.env_tag,
                generation,
            },
            grace: self.cancel_grace,
            request,
        };
        self.jobs.submit(spec, Box::new(job))
    }

    fn check_import_context(&self, stored: &StoredImport) -> Result<(), DbError> {
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
