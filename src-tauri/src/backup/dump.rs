//! The integrated dump (spec §7.1, R22–R24, R29): the objects a database offers, the
//! request checks and the dump job.
//!
//! The job opens its own session on the database, starts one read-only snapshot (R24;
//! MySQL also `SET time_zone = '+00:00'` and a clean `sql_mode` first, so `SHOW CREATE`
//! prints portable text and timestamps read in UTC), reads every object's definition, then
//! writes `<file>.dntpart`:
//!
//! - header (four comment lines), the R23 session statements;
//! - MySQL: tables in foreign-key order (`SHOW CREATE TABLE` text, unqualified), their
//!   data, views in textual dependency order (`DEFINER` and the source database's
//!   qualifiers removed), `SET FOREIGN_KEY_CHECKS = 1`;
//! - PostgreSQL: `CREATE SCHEMA IF NOT EXISTS` (schemas other than `public`), types,
//!   sequences (stand-alone ones followed by their `setval`), tables without foreign keys
//!   (plus their indexes and comments), `ALTER SEQUENCE … OWNED BY` for serial columns,
//!   data, the `setval`s of identity/serial sequences (values read in the snapshot after
//!   the data), foreign keys (`ALTER TABLE … ADD CONSTRAINT`), views;
//! - `-- End of dump: <k> statements`.
//!
//! The file is renamed onto the picked path when complete; a failed or stopped job removes
//! it. Rows are read with `CellMode::Full` (never a cut value) and written as multi-row
//! `INSERT`s through `build::dump_row_tuple` (generated columns omitted; PostgreSQL tables
//! with an identity `ALWAYS` column get `OVERRIDING SYSTEM VALUE`).
use super::{DumpObject, DumpObjects, DumpRequest, SkippedObject};
use crate::ddl_exec::{execute, stopped};
use crate::export_format::Target;
use crate::jobs::{
    JobCtx, JobDone, JobFailure, JobKind, JobLogLevel, JobParams, JobRunner, JobSpec, ProgressUnit,
};
use crate::result_store::block_outside_executor;
use crate::session_manager::{
    close_quietly, config_error, ConnectionRuntime, SessionManager, METADATA_CALL_TIMEOUT,
};
use async_trait::async_trait;
use dnt_core::driver::{CellMode, ExecOptions, ResultSummary, RowSink, Session, StopSignal};
use dnt_core::error::DbError;
use dnt_core::meta::{ForeignKeyDetail, IdentityKind, ObjectKind, ObjectRef, Scope, TableDetail};
use dnt_core::server::{DbKind, ServerInfo, ServerVariant};
use dnt_core::sql::build::{literal, ColumnTarget, TableRef};
use dnt_core::sql::ident::quote_ident;
use dnt_core::sql::{classify, Dialect};
use dnt_core::value::{ColumnMeta, FullValue, LogicalType, Value};
use dnt_transfer::dump::{
    add_fk_sql, create_schema_sql, end_comment, header, insert_head, mysql_view_ddl,
    session_epilogue, session_prologue, setval_column_sql, setval_sql, table_order, view_order,
    DumpHeader, DumpInserts, DumpWriter, TableNode, ViewNode, DUMP_BATCH_BYTES, DUMP_BATCH_ROWS,
};
use std::collections::btree_map::Entry;
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::future::Future;
use std::io::BufWriter;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use uuid::Uuid;

/// Largest cell a dump reads (`CellMode::Full`).
pub const MAX_CELL_BYTES: usize = 256 * 1024 * 1024;
/// Bound of a control statement (snapshot, `SET`, `COMMIT`).
const CONTROL_TIMEOUT_S: u32 = 30;
/// Bound of listing a database's objects for the dialog.
const LIST_TIMEOUT: Duration = Duration::from_secs(120);

const ROUTINES: &str = "Routines are not part of the built-in dump; use an external tool.";
const TRIGGERS: &str = "Triggers are not part of the built-in dump; use an external tool.";
const EVENTS: &str = "Events are not part of the built-in dump; use an external tool.";
const MARIADB_SEQUENCES: &str =
    "MariaDB sequences are not part of the built-in dump; use an external tool.";
const MATVIEWS: &str =
    "Materialized views are not part of the built-in dump; use an external tool.";
const PARTITIONED: &str =
    "Partitioned tables are not part of the built-in dump; use an external tool.";
const PARTITION: &str = "Partitions are not part of the built-in dump; use an external tool.";
const INHERITED: &str = "Inherited tables are not part of the built-in dump; use an external tool.";

/// PostgreSQL tables of schema `$1`: name, `relkind`, is a partition, inherits a table.
pub(crate) const PG_TABLE_KINDS: &str = "SELECT c.relname::text, c.relkind::text, \
     c.relispartition, EXISTS (SELECT 1 FROM pg_catalog.pg_inherits h WHERE h.inhrelid = c.oid) \
     FROM pg_catalog.pg_class c JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
     WHERE n.nspname = $1 AND c.relkind IN ('r', 'p')";

/// Sequences of schema `$1` owned by a column (serial or identity): name.
pub(crate) const PG_OWNED_SEQUENCES: &str = "SELECT DISTINCT c.relname::text \
     FROM pg_catalog.pg_class c JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
     JOIN pg_catalog.pg_depend d ON d.classid = 'pg_catalog.pg_class'::regclass \
     AND d.objid = c.oid AND d.refclassid = 'pg_catalog.pg_class'::regclass \
     AND d.deptype IN ('a', 'i') \
     WHERE n.nspname = $1 AND c.relkind = 'S'";

/// The sequences of table `$1` (a quoted qualified name): schema, name, the owning column
/// and `deptype` (`a` serial / owned by, `i` identity) of a sequence a column owns; NULL,
/// NULL for a sequence a column default only uses.
pub(crate) const PG_TABLE_SEQUENCES: &str = "SELECT sn.nspname::text, s.relname::text, \
     a.attname::text, d.deptype::text \
     FROM pg_catalog.pg_depend d \
     JOIN pg_catalog.pg_class s ON s.oid = d.objid AND s.relkind = 'S' \
     JOIN pg_catalog.pg_namespace sn ON sn.oid = s.relnamespace \
     JOIN pg_catalog.pg_attribute a ON a.attrelid = d.refobjid AND a.attnum = d.refobjsubid \
     WHERE d.classid = 'pg_catalog.pg_class'::regclass \
     AND d.refclassid = 'pg_catalog.pg_class'::regclass \
     AND d.refobjid = $1::regclass AND d.deptype IN ('a', 'i') \
     UNION \
     SELECT sn.nspname::text, s.relname::text, NULL, NULL \
     FROM pg_catalog.pg_attrdef ad \
     JOIN pg_catalog.pg_depend d ON d.classid = 'pg_catalog.pg_attrdef'::regclass \
     AND d.objid = ad.oid AND d.refclassid = 'pg_catalog.pg_class'::regclass \
     JOIN pg_catalog.pg_class s ON s.oid = d.refobjid AND s.relkind = 'S' \
     JOIN pg_catalog.pg_namespace sn ON sn.oid = s.relnamespace \
     WHERE ad.adrelid = $1::regclass \
     ORDER BY 1, 2, 3";

// ---------------------------------------------------------------------------------------
// Reading
// ---------------------------------------------------------------------------------------

/// The rows of one result (display values).
#[derive(Default)]
struct Collect {
    rows: Vec<Vec<Value>>,
    results: usize,
}

impl RowSink for Collect {
    fn result_start(&mut self, _cols: &[ColumnMeta]) {
        self.results += 1;
    }
    fn rows(&mut self, batch: Vec<Vec<Value>>) {
        if self.results == 1 {
            self.rows.extend(batch);
        }
    }
    fn result_end(&mut self, _summary: &ResultSummary) {}
}

/// Sends the dump's statements on its session: never after Stop, and never once the
/// session reconnected (the snapshot would be lost).
pub(crate) struct Reader<'a> {
    session: &'a dyn Session,
    stop: StopSignal,
    grace: Duration,
    reconnects: u64,
}

fn snapshot_lost() -> DbError {
    DbError::ConnectionLost {
        was_in_tx: true,
        outcome_unknown: false,
        message: "The connection was re-established during the dump, so its snapshot is lost. \
                  Start the dump again."
            .into(),
    }
}

impl<'a> Reader<'a> {
    pub(crate) fn new(session: &'a dyn Session, stop: StopSignal, grace: Duration) -> Reader<'a> {
        Reader {
            session,
            stop,
            grace,
            reconnects: session.reconnects(),
        }
    }

    fn check(&self) -> Result<(), DbError> {
        if self.stop.is_stopped() {
            return Err(stopped());
        }
        if self.session.reconnects() != self.reconnects {
            return Err(snapshot_lost());
        }
        Ok(())
    }

    fn kind(&self) -> DbKind {
        self.session.server_info().kind
    }

    /// A control statement.
    async fn exec(&self, sql: &str) -> Result<(), DbError> {
        self.check()?;
        execute(
            self.session,
            sql,
            self.stop.clone(),
            self.grace,
            CONTROL_TIMEOUT_S,
        )
        .await?;
        self.check()
    }

    /// Every row of a metadata query with `params` bound to its placeholders.
    async fn rows(&self, sql: &str, params: Vec<String>) -> Result<Vec<Vec<Value>>, DbError> {
        self.check()?;
        let opts = ExecOptions {
            max_rows: usize::MAX,
            timeout: Some(METADATA_CALL_TIMEOUT),
            cancel_grace: self.grace,
            stop: self.stop.clone(),
            params,
            ..ExecOptions::default()
        };
        let mut sink = Collect::default();
        self.session
            .execute_one(sql, classify(sql, self.session.dialect()), &opts, &mut sink)
            .await?;
        self.check()?;
        Ok(sink.rows)
    }

    /// An introspection call, bounded by [`METADATA_CALL_TIMEOUT`].
    async fn meta<T>(&self, fut: impl Future<Output = Result<T, DbError>>) -> Result<T, DbError> {
        self.check()?;
        let out = tokio::time::timeout(METADATA_CALL_TIMEOUT, fut)
            .await
            .map_err(|_| DbError::timeout(dnt_core::error::ConnectStage::Execute))??;
        self.check()?;
        Ok(out)
    }

    async fn list(&self, scope: &Scope, kind: ObjectKind) -> Result<Vec<String>, DbError> {
        let found = self
            .meta(self.session.introspector().list_objects(scope, kind))
            .await?;
        Ok(found.into_iter().map(|o| o.name).collect())
    }
}

fn text_of(v: Option<&Value>) -> Option<String> {
    match v {
        Some(Value::Text(s)) => Some(s.clone()),
        _ => None,
    }
}

fn bool_of(v: Option<&Value>) -> bool {
    match v {
        Some(Value::Bool(b)) => *b,
        Some(Value::Text(s)) => matches!(s.as_str(), "t" | "true" | "1"),
        Some(Value::Int(n)) => *n != 0,
        _ => false,
    }
}

fn skipped(schema: Option<&str>, name: String, kind: ObjectKind, reason: &str) -> SkippedObject {
    SkippedObject {
        schema: schema.map(str::to_string),
        name,
        kind,
        reason: reason.into(),
    }
}

fn object(schema: Option<&str>, name: String, kind: ObjectKind) -> DumpObject {
    DumpObject {
        schema: schema.map(str::to_string),
        name,
        kind,
        data: kind == ObjectKind::Table,
    }
}

/// The objects of `database` (PostgreSQL: of `schemas`) the dump can hold, and the ones
/// it skips with their reason (R22).
pub(crate) async fn list_dump_objects(
    r: &Reader<'_>,
    database: &str,
    schemas: &[String],
) -> Result<DumpObjects, DbError> {
    let mut out = DumpObjects::default();
    match r.kind() {
        DbKind::MySql => {
            let scope = Scope {
                database: database.into(),
                schema: None,
            };
            for kind in [ObjectKind::Table, ObjectKind::View] {
                for name in r.list(&scope, kind).await? {
                    out.objects.push(object(None, name, kind));
                }
            }
            for (kind, reason) in [
                (ObjectKind::Function, ROUTINES),
                (ObjectKind::Procedure, ROUTINES),
                (ObjectKind::Trigger, TRIGGERS),
                (ObjectKind::Event, EVENTS),
                (ObjectKind::Sequence, MARIADB_SEQUENCES),
            ] {
                for name in r.list(&scope, kind).await? {
                    out.skipped.push(skipped(None, name, kind, reason));
                }
            }
        }
        DbKind::Postgres => {
            for schema in schemas {
                let s = Some(schema.as_str());
                let scope = Scope {
                    database: database.into(),
                    schema: Some(schema.clone()),
                };
                let kinds = r.rows(PG_TABLE_KINDS, vec![schema.clone()]).await?;
                let reason_of = |name: &str| {
                    kinds
                        .iter()
                        .find(|row| text_of(row.first()).as_deref() == Some(name))
                        .and_then(|row| {
                            if text_of(row.get(1)).as_deref() == Some("p") {
                                Some(PARTITIONED)
                            } else if bool_of(row.get(2)) {
                                Some(PARTITION)
                            } else if bool_of(row.get(3)) {
                                Some(INHERITED)
                            } else {
                                None
                            }
                        })
                };
                for name in r.list(&scope, ObjectKind::Table).await? {
                    match reason_of(&name) {
                        Some(reason) => {
                            out.skipped
                                .push(skipped(s, name, ObjectKind::Table, reason))
                        }
                        None => out.objects.push(object(s, name, ObjectKind::Table)),
                    }
                }
                for name in r.list(&scope, ObjectKind::View).await? {
                    out.objects.push(object(s, name, ObjectKind::View));
                }
                let owned: HashSet<String> = r
                    .rows(PG_OWNED_SEQUENCES, vec![schema.clone()])
                    .await?
                    .iter()
                    .filter_map(|row| text_of(row.first()))
                    .collect();
                for name in r.list(&scope, ObjectKind::Sequence).await? {
                    // A column's own sequence comes with its table.
                    if !owned.contains(&name) {
                        out.objects.push(object(s, name, ObjectKind::Sequence));
                    }
                }
                for name in r.list(&scope, ObjectKind::Type).await? {
                    out.objects.push(object(s, name, ObjectKind::Type));
                }
                for (kind, reason) in [
                    (ObjectKind::MaterializedView, MATVIEWS),
                    (ObjectKind::Function, ROUTINES),
                    (ObjectKind::Procedure, ROUTINES),
                    (ObjectKind::Trigger, TRIGGERS),
                ] {
                    for name in r.list(&scope, kind).await? {
                        out.skipped.push(skipped(s, name, kind, reason));
                    }
                }
            }
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------------------------------
// Request
// ---------------------------------------------------------------------------------------

/// The request's own checks (no server needed): objects of the engine's kinds, no
/// duplicates, PostgreSQL schemas filled from the objects when none were named.
pub(crate) fn normalise(mut req: DumpRequest, kind: DbKind) -> Result<DumpRequest, DbError> {
    if req.database.trim().is_empty() {
        return Err(config_error("database", "Choose a database to dump."));
    }
    if req.path.trim().is_empty() {
        return Err(config_error("path", "Choose a file to dump to."));
    }
    if req.objects.is_empty() {
        return Err(config_error(
            "objects",
            "Choose at least one object to dump.",
        ));
    }
    let mut seen = HashSet::new();
    for o in &mut req.objects {
        let allowed = match kind {
            DbKind::MySql => matches!(o.kind, ObjectKind::Table | ObjectKind::View),
            DbKind::Postgres => matches!(
                o.kind,
                ObjectKind::Table | ObjectKind::View | ObjectKind::Sequence | ObjectKind::Type
            ),
        };
        if !allowed {
            return Err(config_error(
                "objects",
                &format!("The built-in dump cannot hold a {}.", kind_label(o.kind)),
            ));
        }
        match (kind, &o.schema) {
            (DbKind::MySql, Some(_)) => {
                return Err(config_error(
                    "objects",
                    "MySQL has no schemas inside a database.",
                ))
            }
            (DbKind::Postgres, None) => {
                return Err(config_error(
                    "objects",
                    "A schema is required on PostgreSQL.",
                ))
            }
            _ => {}
        }
        if o.name.is_empty() {
            return Err(config_error("objects", "An object has no name."));
        }
        o.data &= o.kind == ObjectKind::Table;
        if !seen.insert((o.schema.clone(), o.name.clone(), o.kind)) {
            return Err(config_error("objects", "An object is listed twice."));
        }
    }
    match kind {
        DbKind::MySql => req.schemas.clear(),
        DbKind::Postgres => {
            let used: BTreeSet<String> = req
                .objects
                .iter()
                .filter_map(|o| o.schema.clone())
                .collect();
            if req.schemas.is_empty() {
                req.schemas = used.into_iter().collect();
            } else if let Some(missing) = used.iter().find(|s| !req.schemas.contains(s)) {
                return Err(config_error(
                    "schemas",
                    &format!("Schema {missing} is not among the dumped schemas."),
                ));
            }
        }
    }
    Ok(req)
}

fn kind_label(kind: ObjectKind) -> &'static str {
    match kind {
        ObjectKind::Table => "table",
        ObjectKind::View => "view",
        ObjectKind::MaterializedView => "materialized view",
        ObjectKind::Function => "function",
        ObjectKind::Procedure => "procedure",
        ObjectKind::Trigger => "trigger",
        ObjectKind::Event => "event",
        ObjectKind::Sequence => "sequence",
        ObjectKind::Type => "type",
    }
}

fn shown_name(schema: Option<&str>, name: &str) -> String {
    match schema {
        Some(s) => format!("{s}.{name}"),
        None => name.to_string(),
    }
}

/// The server version for the header: `8.0.36`, `10.6.28`, `17.11`; the server's own
/// string carries build details (`version()` on PostgreSQL). PostgreSQL 10+ numbers are
/// `major.minor`, read from `server_version_num` as `(major, 0, minor)`.
fn version_label(info: &ServerInfo) -> String {
    let (major, minor, patch) = info.version;
    match info.kind {
        DbKind::Postgres if major >= 10 => format!("{major}.{patch}"),
        _ => format!("{major}.{minor}.{patch}"),
    }
}

fn engine_label(info: &ServerInfo) -> &'static str {
    match info.variant {
        ServerVariant::MySql57 | ServerVariant::MySql8 => "MySQL",
        ServerVariant::MariaDb => "MariaDB",
        ServerVariant::Postgres => "PostgreSQL",
    }
}

fn file_name(path: &str) -> String {
    Path::new(path)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string())
}

impl SessionManager {
    /// The objects `dump_start` can dump from `database` (PostgreSQL: from `schemas`) and
    /// the skipped ones with their reason (R22), read on a short-lived session.
    pub async fn dump_objects(
        &self,
        conn: Uuid,
        database: String,
        schemas: Vec<String>,
    ) -> Result<DumpObjects, DbError> {
        let cfg = self.repo.get(conn)?;
        if database.trim().is_empty() {
            return Err(config_error("database", "Choose a database to dump."));
        }
        if cfg.kind == DbKind::Postgres && schemas.is_empty() {
            return Err(config_error("schemas", "Choose at least one schema."));
        }
        let rt = self.runtime(conn)?;
        let (session, _) = rt.open_query_session(Some(database.clone())).await?;
        let r = Reader::new(session.as_ref(), StopSignal::new(), self.cancel_grace);
        let listed = tokio::time::timeout(LIST_TIMEOUT, list_dump_objects(&r, &database, &schemas))
            .await
            .map_err(|_| DbError::timeout(dnt_core::error::ConnectStage::Execute));
        close_quietly(session).await;
        listed?
    }

    /// Starts a dump job (a read: no confirmation, allowed on read-only connections);
    /// returns its id. The path must have been admitted by `dump_pick_path` (checked by
    /// the command).
    pub async fn dump_start(&self, request: DumpRequest) -> Result<Uuid, DbError> {
        let cfg = self.repo.get(request.connection_id)?;
        let request = normalise(request, cfg.kind)?;
        let rt = self.runtime(request.connection_id)?;
        let spec = JobSpec {
            kind: JobKind::Dump,
            connection_id: Some(request.connection_id),
            title: format!("Dump {} to {}", request.database, file_name(&request.path)),
            params: serde_json::to_value(&request).ok().map(JobParams::Dump),
            rerunnable: true,
        };
        let job = DumpJob {
            request,
            rt,
            grace: self.cancel_grace,
        };
        self.jobs.submit(spec, Box::new(job))
    }
}

// ---------------------------------------------------------------------------------------
// Output file
// ---------------------------------------------------------------------------------------

/// The dump being written (`<file>.dntpart`); every write runs off the async executor.
struct DumpFile {
    w: DumpWriter,
    shown: String,
}

fn write_error(shown: &str, e: &std::io::Error) -> DbError {
    config_error("path", &format!("Could not write {shown}: {e}."))
}

impl DumpFile {
    /// Creates the temp file: only a NEW file (a stale temp file or anything planted there,
    /// a symbolic link included, is refused and left as it is).
    fn create(part: &Path, shown: &str, compress: bool) -> Result<DumpFile, DbError> {
        let file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(part)
            .map_err(|e| {
                if e.kind() == std::io::ErrorKind::AlreadyExists {
                    config_error(
                        "path",
                        &format!(
                            "The temporary file {} already exists next to the file (another \
                             dump may be writing it). Remove it, or choose another file.",
                            file_name(&part.to_string_lossy())
                        ),
                    )
                } else {
                    write_error(shown, &e)
                }
            })?;
        Ok(DumpFile {
            w: DumpWriter::new(
                Box::new(BufWriter::with_capacity(256 * 1024, file)),
                compress,
            ),
            shown: shown.to_string(),
        })
    }

    fn with<T>(
        &mut self,
        f: impl FnOnce(&mut DumpWriter) -> std::io::Result<T>,
    ) -> Result<T, DbError> {
        let w = &mut self.w;
        block_outside_executor(|| f(w)).map_err(|e| write_error(&self.shown, &e))
    }

    fn raw(&mut self, text: &str) -> Result<(), DbError> {
        self.with(|w| w.raw(text))
    }

    fn comment(&mut self, text: &str) -> Result<(), DbError> {
        self.with(|w| w.comment(text))
    }

    fn statement(&mut self, sql: &str) -> Result<(), DbError> {
        self.with(|w| w.statement(sql))
    }

    fn script(&mut self, sql: &str, d: Dialect) -> Result<u64, DbError> {
        self.with(|w| w.script(sql, d))
    }

    fn statements(&self) -> u64 {
        self.w.statements()
    }

    fn finish(self) -> Result<u64, DbError> {
        let shown = self.shown;
        let w = self.w;
        block_outside_executor(|| w.finish()).map_err(|e| write_error(&shown, &e))
    }
}

// ---------------------------------------------------------------------------------------
// Data
// ---------------------------------------------------------------------------------------

/// Turns the rows of one `SELECT` into `INSERT`s written to the file.
struct DataSink<'a> {
    file: &'a mut DumpFile,
    ctx: &'a JobCtx,
    /// The inserted columns (name, `db_type`) in `SELECT` order.
    cols: Vec<(String, String)>,
    kind: DbKind,
    relation: String,
    overriding: bool,
    phase: String,
    estimate: Option<u64>,
    inserts: Option<DumpInserts>,
    results: usize,
    rows: u64,
    error: Option<DbError>,
}

impl DataSink<'_> {
    fn fail(&mut self, e: DbError) {
        if self.error.is_none() {
            self.error = Some(e);
        }
    }

    fn dialect(&self) -> Dialect {
        match self.kind {
            // The dump sets a `sql_mode` without ANSI_QUOTES / NO_BACKSLASH_ESCAPES (R23).
            DbKind::MySql => Dialect::MYSQL,
            DbKind::Postgres => Dialect::Postgres,
        }
    }
}

impl RowSink for DataSink<'_> {
    fn result_start(&mut self, metas: &[ColumnMeta]) {
        self.results += 1;
        if self.results != 1 {
            return;
        }
        if metas.len() != self.cols.len() {
            return self.fail(table_changed(&self.phase));
        }
        let targets: Vec<ColumnTarget> = self
            .cols
            .iter()
            .zip(metas)
            .map(|((name, db_type), m)| ColumnTarget {
                name: name.clone(),
                db_type: db_type.clone(),
                logical: m.logical,
                collation: None,
            })
            .collect();
        let head = insert_head(self.kind, &self.relation, &targets, self.overriding);
        self.inserts = Some(DumpInserts::new(
            head,
            targets,
            self.dialect(),
            DUMP_BATCH_ROWS,
            DUMP_BATCH_BYTES,
        ));
    }

    fn rows(&mut self, _batch: Vec<Vec<Value>>) {
        self.fail(DbError::Internal {
            message: "the driver sent display values for a dump".into(),
        });
    }

    fn full_rows(&mut self, batch: Vec<Vec<FullValue>>) {
        if self.error.is_some() || self.results != 1 {
            return;
        }
        let Some(mut inserts) = self.inserts.take() else {
            return;
        };
        for row in &batch {
            match inserts.push(row) {
                Ok(Some(sql)) => {
                    if let Err(e) = self.file.statement(&sql) {
                        self.fail(e);
                        break;
                    }
                }
                Ok(None) => {}
                Err(e) => {
                    self.fail(config_error(
                        "data",
                        &format!("{} cannot be dumped: {e}.", self.phase),
                    ));
                    break;
                }
            }
            self.rows += 1;
        }
        self.inserts = Some(inserts);
        self.ctx
            .progress(&self.phase, self.rows, self.estimate, ProgressUnit::Rows);
    }

    fn result_end(&mut self, _summary: &ResultSummary) {}
}

fn table_changed(what: &str) -> DbError {
    DbError::PlanStale {
        reason: format!("{what} changed while it was read; start the dump again."),
    }
}

// ---------------------------------------------------------------------------------------
// Job
// ---------------------------------------------------------------------------------------

/// A started dump.
pub(crate) struct DumpJob {
    pub(crate) request: DumpRequest,
    pub(crate) rt: Arc<ConnectionRuntime>,
    pub(crate) grace: Duration,
}

/// What a finished dump wrote.
struct Written {
    tables: u32,
    views: u32,
    rows: u64,
    statements: u64,
}

/// One table of the dump with what the snapshot says about it.
struct Table {
    obj: DumpObject,
    detail: TableDetail,
    /// MySQL: `SHOW CREATE TABLE`.
    ddl: String,
    /// PostgreSQL: the sequences of its columns.
    sequences: Vec<SeqRow>,
}

/// A row of [`PG_TABLE_SEQUENCES`].
struct SeqRow {
    schema: String,
    name: String,
    owner: Option<String>,
    identity: bool,
}

/// A PostgreSQL sequence the dump creates.
struct SeqCreate {
    ddl: String,
    /// Serial: the owning table's relation and column (`OWNED BY`, `setval` by column).
    owner: Option<(String, String)>,
    /// Stand-alone: `(last_value, is_called)` literals read in the snapshot.
    value: Option<(String, String)>,
}

fn obj_ref(database: &str, o: &DumpObject) -> ObjectRef {
    ObjectRef {
        database: database.into(),
        schema: o.schema.clone(),
        name: o.name.clone(),
        kind: o.kind,
    }
}

fn pg_relation(schema: &str, name: &str) -> String {
    TableRef {
        kind: DbKind::Postgres,
        database: String::new(),
        schema: Some(schema.into()),
        name: name.into(),
    }
    .relation()
}

/// The `setval` literals of `SELECT last_value, is_called FROM <seq>`.
fn seq_literals(rows: &[Vec<Value>], what: &str) -> Result<(String, String), DbError> {
    let row = rows.first().ok_or_else(|| table_changed(what))?;
    let (Some(last), Some(called)) = (row.first(), row.get(1)) else {
        return Err(table_changed(what));
    };
    let lit = |v: &Value, db_type: &str, logical: LogicalType| {
        let full = FullValue::try_from_value(v.clone()).map_err(|_| table_changed(what))?;
        literal(
            &full,
            &ColumnTarget {
                name: String::new(),
                db_type: db_type.into(),
                logical,
                collation: None,
            },
            Dialect::Postgres,
        )
        .map_err(|_| table_changed(what))
    };
    let called = match called {
        Value::Text(s) => Value::Bool(matches!(s.as_str(), "t" | "true")),
        v => v.clone(),
    };
    Ok((
        lit(last, "bigint", LogicalType::Integer)?,
        lit(&called, "boolean", LogicalType::Bool)?,
    ))
}

/// A foreign key's definition: the server's text, else rebuilt from its parts.
fn fk_definition(fk: &ForeignKeyDetail, table_schema: Option<&str>) -> String {
    if let Some(def) = &fk.definition {
        return def.clone();
    }
    let q = |n: &String| quote_ident(DbKind::Postgres, n);
    let list = |v: &[String]| v.iter().map(q).collect::<Vec<_>>().join(", ");
    let schema = fk
        .ref_schema
        .as_deref()
        .or(table_schema)
        .unwrap_or("public");
    let mut def = format!(
        "FOREIGN KEY ({}) REFERENCES {} ({})",
        list(&fk.columns),
        pg_relation(schema, &fk.ref_table),
        list(&fk.ref_columns)
    );
    for (clause, action) in [("ON UPDATE", &fk.on_update), ("ON DELETE", &fk.on_delete)] {
        if !action.is_empty() && action != "NO ACTION" {
            def.push_str(&format!(" {clause} {action}"));
        }
    }
    def
}

#[async_trait]
impl JobRunner for DumpJob {
    async fn run(self: Box<Self>, ctx: JobCtx) -> Result<JobDone, JobFailure> {
        let target = Target::new(&self.request.path).map_err(JobFailure::from)?;
        let part: PathBuf = target.part.clone();
        let shown = self.request.path.clone();
        let compress = self.request.compress;
        let mut file =
            tokio::task::spawn_blocking(move || DumpFile::create(&part, &shown, compress))
                .await
                .map_err(|e| {
                    JobFailure::from(DbError::Internal {
                        message: format!("creating the dump file failed: {e}"),
                    })
                })?
                .map_err(JobFailure::from)?;
        let outcome = async {
            let (session, _) = self
                .rt
                .open_query_session(Some(self.request.database.clone()))
                .await?;
            ctx.set_cancel(Some(session.cancel_handle()));
            let written = self.dump(&ctx, session.as_ref(), &mut file).await;
            ctx.set_cancel(None);
            if session.in_transaction() {
                let _ = execute(
                    session.as_ref(),
                    "ROLLBACK",
                    StopSignal::new(),
                    self.grace,
                    CONTROL_TIMEOUT_S,
                )
                .await;
            }
            close_quietly(session).await;
            written
        }
        .await;
        let finished = match outcome {
            Ok(w) => file.finish().map(|_| w),
            Err(e) => {
                drop(file);
                Err(e)
            }
        };
        let committed = finished.and_then(|w| {
            target.commit()?;
            Ok(w)
        });
        match committed {
            Ok(w) => Ok(JobDone {
                summary: format!(
                    "Dumped {} tables ({} rows) and {} views: {} statements.",
                    w.tables, w.rows, w.views, w.statements
                ),
                result_path: Some(self.request.path.clone()),
            }),
            Err(e) => {
                target.discard();
                Err(JobFailure::from(e))
            }
        }
    }
}

impl DumpJob {
    async fn dump(
        &self,
        ctx: &JobCtx,
        session: &dyn Session,
        file: &mut DumpFile,
    ) -> Result<Written, DbError> {
        let r = Reader::new(session, ctx.stop(), self.grace);
        let kind = r.kind();
        if kind != self.rt.cfg.kind {
            return Err(crate::object_actions::stale());
        }
        // The snapshot (R24).
        match kind {
            DbKind::MySql => {
                r.exec("SET SESSION sql_mode = 'NO_AUTO_VALUE_ON_ZERO'")
                    .await?;
                r.exec("SET time_zone = '+00:00'").await?;
                r.exec("SET SESSION TRANSACTION ISOLATION LEVEL REPEATABLE READ")
                    .await?;
                r.exec("START TRANSACTION WITH CONSISTENT SNAPSHOT, READ ONLY")
                    .await?;
            }
            DbKind::Postgres => {
                r.exec("BEGIN ISOLATION LEVEL REPEATABLE READ READ ONLY")
                    .await?;
            }
        }
        ctx.progress("Reading definitions", 0, None, ProgressUnit::None);
        let req = &self.request;
        let listed = list_dump_objects(&r, &req.database, &req.schemas).await?;
        for s in &listed.skipped {
            ctx.log(
                JobLogLevel::Info,
                format!(
                    "Skipped {} {}: {}",
                    kind_label(s.kind),
                    shown_name(s.schema.as_deref(), &s.name),
                    s.reason
                ),
            );
        }
        let mut objects = Vec::new();
        for o in &req.objects {
            let same =
                |x: &DumpObject| x.schema == o.schema && x.name == o.name && x.kind == o.kind;
            if listed.objects.iter().any(same) {
                objects.push(o.clone());
            } else if let Some(s) = listed
                .skipped
                .iter()
                .find(|s| s.schema == o.schema && s.name == o.name && s.kind == o.kind)
            {
                ctx.log(
                    JobLogLevel::Warn,
                    format!(
                        "The {} {} is not dumped: {}",
                        kind_label(o.kind),
                        shown_name(o.schema.as_deref(), &o.name),
                        s.reason
                    ),
                );
            } else {
                return Err(config_error(
                    "objects",
                    &format!(
                        "The {} {} no longer exists.",
                        kind_label(o.kind),
                        shown_name(o.schema.as_deref(), &o.name)
                    ),
                ));
            }
        }
        match kind {
            DbKind::MySql => self.mysql(ctx, &r, file, &objects).await,
            DbKind::Postgres => self.postgres(ctx, &r, file, &objects).await,
        }
    }

    fn header(&self, info: &ServerInfo, objects: &[DumpObject], seqs: Option<u32>) -> DumpHeader {
        let count = |k: ObjectKind| objects.iter().filter(|o| o.kind == k).count() as u32;
        DumpHeader {
            engine: engine_label(info).into(),
            version: version_label(info),
            database: self.request.database.clone(),
            schemas: self.request.schemas.clone(),
            created: crate::run_events::utc_millis_now(),
            tables: count(ObjectKind::Table),
            tables_with_data: objects
                .iter()
                .filter(|o| o.kind == ObjectKind::Table && o.data)
                .count() as u32,
            views: count(ObjectKind::View),
            sequences: seqs,
            types: seqs.map(|_| count(ObjectKind::Type)),
        }
    }

    async fn read_tables(
        &self,
        r: &Reader<'_>,
        objects: &[DumpObject],
    ) -> Result<Vec<Table>, DbError> {
        let intro = r.session.introspector();
        let mut tables = Vec::new();
        for o in objects.iter().filter(|o| o.kind == ObjectKind::Table) {
            let oref = obj_ref(&self.request.database, o);
            let detail = r.meta(intro.table_detail(&oref)).await?;
            let mut t = Table {
                obj: o.clone(),
                detail,
                ddl: String::new(),
                sequences: Vec::new(),
            };
            match r.kind() {
                DbKind::MySql => t.ddl = r.meta(intro.object_ddl(&oref)).await?,
                DbKind::Postgres => {
                    let rel = pg_relation(o.schema.as_deref().unwrap_or("public"), &o.name);
                    for row in r.rows(PG_TABLE_SEQUENCES, vec![rel]).await? {
                        let (Some(schema), Some(name)) =
                            (text_of(row.first()), text_of(row.get(1)))
                        else {
                            continue;
                        };
                        t.sequences.push(SeqRow {
                            schema,
                            name,
                            owner: text_of(row.get(2)),
                            identity: text_of(row.get(3)).as_deref() == Some("i"),
                        });
                    }
                }
            }
            tables.push(t);
        }
        Ok(tables)
    }

    /// The tables in foreign-key order.
    fn ordered(tables: Vec<Table>) -> Vec<Table> {
        let nodes: Vec<TableNode> = tables
            .iter()
            .map(|t| TableNode {
                schema: t.obj.schema.clone(),
                name: t.obj.name.clone(),
                references: t
                    .detail
                    .foreign_keys
                    .iter()
                    .map(|fk| (fk.ref_schema.clone(), fk.ref_table.clone()))
                    .collect(),
            })
            .collect();
        let order = table_order(&nodes);
        let mut slots: Vec<Option<Table>> = tables.into_iter().map(Some).collect();
        order.into_iter().filter_map(|i| slots[i].take()).collect()
    }

    /// Reads one table's rows into `INSERT`s.
    #[allow(clippy::too_many_arguments)]
    async fn data(
        &self,
        ctx: &JobCtx,
        r: &Reader<'_>,
        file: &mut DumpFile,
        t: &Table,
        select_from: String,
        insert_into: String,
    ) -> Result<u64, DbError> {
        let kind = r.kind();
        let label = shown_name(t.obj.schema.as_deref(), &t.obj.name);
        let cols: Vec<_> = t
            .detail
            .columns
            .iter()
            .filter(|c| c.generated.is_none())
            .collect();
        if cols.is_empty() {
            ctx.log(
                JobLogLevel::Warn,
                format!("Table {label} has only generated columns; its rows are not dumped."),
            );
            return Ok(0);
        }
        let overriding = kind == DbKind::Postgres
            && cols.iter().any(|c| {
                c.identity_kind == Some(IdentityKind::Always)
                    || c.identity
                        .as_deref()
                        .is_some_and(|i| i.eq_ignore_ascii_case("ALWAYS"))
            });
        let list: Vec<String> = cols.iter().map(|c| quote_ident(kind, &c.name)).collect();
        let sql = format!("SELECT {} FROM {select_from}", list.join(", "));
        file.comment(&format!("Data of {label}"))?;
        let phase = format!("Table {label}");
        ctx.progress(&phase, 0, t.detail.row_estimate, ProgressUnit::Rows);
        let opts = ExecOptions {
            max_rows: usize::MAX,
            timeout: None,
            cancel_grace: self.grace,
            stop: ctx.stop(),
            full_bytes: true,
            cells: CellMode::Full {
                max_cell_bytes: MAX_CELL_BYTES,
            },
            ..ExecOptions::default()
        };
        r.check()?;
        let mut sink = DataSink {
            file,
            ctx,
            cols: cols
                .iter()
                .map(|c| (c.name.clone(), c.db_type.clone()))
                .collect(),
            kind,
            relation: insert_into,
            overriding,
            phase,
            estimate: t.detail.row_estimate,
            inserts: None,
            results: 0,
            rows: 0,
            error: None,
        };
        let sent = r
            .session
            .execute_one(&sql, classify(&sql, r.session.dialect()), &opts, &mut sink)
            .await;
        if let Some(e) = sink.error.take() {
            return Err(e);
        }
        sent?;
        r.check()?;
        if sink.results == 0 {
            return Err(table_changed(&sink.phase));
        }
        let last = sink.inserts.as_mut().and_then(DumpInserts::flush);
        let rows = sink.rows;
        if let Some(sql) = last {
            file.statement(&sql)?;
        }
        ctx.progress(
            &format!("Table {label}"),
            rows,
            Some(rows),
            ProgressUnit::Rows,
        );
        Ok(rows)
    }

    async fn mysql(
        &self,
        ctx: &JobCtx,
        r: &Reader<'_>,
        file: &mut DumpFile,
        objects: &[DumpObject],
    ) -> Result<Written, DbError> {
        let db = &self.request.database;
        let tables = Self::ordered(self.read_tables(r, objects).await?);
        let mut views = Vec::new();
        for o in objects.iter().filter(|o| o.kind == ObjectKind::View) {
            let ddl = r
                .meta(r.session.introspector().object_ddl(&obj_ref(db, o)))
                .await?;
            views.push(ViewNode {
                name: o.name.clone(),
                ddl: mysql_view_ddl(&ddl, db),
            });
        }
        let info = r.session.server_info().clone();
        let q = |n: &str| quote_ident(DbKind::MySql, n);

        file.raw(&header(&self.header(&info, objects, None)))?;
        for s in session_prologue(DbKind::MySql) {
            file.statement(s)?;
        }
        for t in &tables {
            file.comment(&format!("Table {}", q(&t.obj.name)))?;
            if self.request.include_drop {
                file.statement(&format!("DROP TABLE IF EXISTS {}", q(&t.obj.name)))?;
            }
            file.statement(&t.ddl)?;
        }
        let mut rows = 0;
        for t in tables.iter().filter(|t| t.obj.data) {
            let from = TableRef {
                kind: DbKind::MySql,
                database: db.clone(),
                schema: None,
                name: t.obj.name.clone(),
            }
            .relation();
            rows += self.data(ctx, r, file, t, from, q(&t.obj.name)).await?;
        }
        for i in view_order(&views, Dialect::MYSQL) {
            let v = &views[i];
            file.comment(&format!("View {}", q(&v.name)))?;
            if self.request.include_drop {
                file.statement(&format!("DROP VIEW IF EXISTS {}", q(&v.name)))?;
            }
            file.statement(&v.ddl)?;
        }
        for s in session_epilogue(DbKind::MySql) {
            file.statement(s)?;
        }
        r.exec("COMMIT").await?;
        let statements = file.statements();
        file.raw(&end_comment(statements))?;
        Ok(Written {
            tables: tables.len() as u32,
            views: views.len() as u32,
            rows,
            statements,
        })
    }

    async fn postgres(
        &self,
        ctx: &JobCtx,
        r: &Reader<'_>,
        file: &mut DumpFile,
        objects: &[DumpObject],
    ) -> Result<Written, DbError> {
        let db = &self.request.database;
        let intro = r.session.introspector();
        let pg = Dialect::Postgres;
        let schema_of = |o: &DumpObject| o.schema.clone().unwrap_or_else(|| "public".into());

        // Types: enums first (a composite may use one), then by name.
        let mut types = Vec::new();
        for o in objects.iter().filter(|o| o.kind == ObjectKind::Type) {
            let ddl = r.meta(intro.object_ddl(&obj_ref(db, o))).await?;
            types.push((o.clone(), ddl));
        }
        types.sort_by_key(|(o, ddl)| (!ddl.contains(" AS ENUM ("), schema_of(o), o.name.clone()));

        // Sequences: the stand-alone ones asked for, then those the tables need.
        let mut creates: BTreeMap<(String, String), SeqCreate> = BTreeMap::new();
        for o in objects.iter().filter(|o| o.kind == ObjectKind::Sequence) {
            let ddl = r.meta(intro.object_ddl(&obj_ref(db, o))).await?;
            creates.insert(
                (schema_of(o), o.name.clone()),
                SeqCreate {
                    ddl,
                    owner: None,
                    value: None,
                },
            );
        }
        let tables = Self::ordered(self.read_tables(r, objects).await?);
        for t in &tables {
            let rel = pg_relation(&schema_of(&t.obj), &t.obj.name);
            for s in &t.sequences {
                let key = (s.schema.clone(), s.name.clone());
                match (&s.owner, s.identity) {
                    // Created by the column's `GENERATED … AS IDENTITY`.
                    (Some(_), true) => {}
                    (Some(col), false) => {
                        let ddl = match creates.remove(&key) {
                            Some(c) => c.ddl,
                            None => self.sequence_ddl(r, &key).await?,
                        };
                        creates.insert(
                            key,
                            SeqCreate {
                                ddl,
                                owner: Some((rel.clone(), col.clone())),
                                value: None,
                            },
                        );
                    }
                    (None, _) => {
                        if let Entry::Vacant(slot) = creates.entry(key) {
                            let ddl = self.sequence_ddl(r, slot.key()).await?;
                            slot.insert(SeqCreate {
                                ddl,
                                owner: None,
                                value: None,
                            });
                        }
                    }
                }
            }
        }
        for ((schema, name), c) in creates.iter_mut() {
            if c.owner.is_none() {
                c.value = Some(self.sequence_value(r, schema, name).await?);
            }
        }

        let mut views = Vec::new();
        for o in objects.iter().filter(|o| o.kind == ObjectKind::View) {
            let ddl = r.meta(intro.object_ddl(&obj_ref(db, o))).await?;
            views.push((
                o.clone(),
                ViewNode {
                    name: o.name.clone(),
                    ddl,
                },
            ));
        }
        let nodes: Vec<ViewNode> = views.iter().map(|(_, v)| v.clone()).collect();
        let view_seq = view_order(&nodes, pg);

        let dumped: HashSet<(String, String)> = tables
            .iter()
            .map(|t| (schema_of(&t.obj), t.obj.name.clone()))
            .collect();
        for t in &tables {
            for fk in &t.detail.foreign_keys {
                let to = (
                    fk.ref_schema.clone().unwrap_or_else(|| schema_of(&t.obj)),
                    fk.ref_table.clone(),
                );
                if !dumped.contains(&to) {
                    ctx.log(
                        JobLogLevel::Warn,
                        format!(
                            "Foreign key {} of {} references {}.{}, which is not in the dump; \
                             restoring needs that table.",
                            fk.name,
                            shown_name(t.obj.schema.as_deref(), &t.obj.name),
                            to.0,
                            to.1
                        ),
                    );
                }
            }
        }

        let info = r.session.server_info().clone();
        file.raw(&header(&self.header(
            &info,
            objects,
            Some(creates.len() as u32),
        )))?;
        for s in session_prologue(DbKind::Postgres) {
            file.statement(s)?;
        }
        if self.request.include_drop {
            for &i in view_seq.iter().rev() {
                let (o, _) = &views[i];
                file.statement(&format!(
                    "DROP VIEW IF EXISTS {} CASCADE",
                    pg_relation(&schema_of(o), &o.name)
                ))?;
            }
            for t in tables.iter().rev() {
                file.statement(&format!(
                    "DROP TABLE IF EXISTS {} CASCADE",
                    pg_relation(&schema_of(&t.obj), &t.obj.name)
                ))?;
            }
            for (schema, name) in creates.keys() {
                file.statement(&format!(
                    "DROP SEQUENCE IF EXISTS {} CASCADE",
                    pg_relation(schema, name)
                ))?;
            }
            for (o, _) in types.iter().rev() {
                file.statement(&format!(
                    "DROP TYPE IF EXISTS {} CASCADE",
                    pg_relation(&schema_of(o), &o.name)
                ))?;
            }
        }
        let schemas: BTreeSet<String> = objects
            .iter()
            .map(schema_of)
            .chain(creates.keys().map(|(s, _)| s.clone()))
            .filter(|s| s != "public")
            .collect();
        for s in &schemas {
            file.statement(&create_schema_sql(s))?;
        }
        for (o, ddl) in &types {
            file.comment(&format!(
                "Type {}",
                shown_name(o.schema.as_deref(), &o.name)
            ))?;
            file.script(ddl, pg)?;
        }
        for ((schema, name), c) in &creates {
            file.comment(&format!("Sequence {schema}.{name}"))?;
            file.script(&c.ddl, pg)?;
            if let Some((last, called)) = &c.value {
                file.statement(&setval_sql(&pg_relation(schema, name), last, called))?;
            }
        }
        for t in &tables {
            file.comment(&format!(
                "Table {}",
                shown_name(t.obj.schema.as_deref(), &t.obj.name)
            ))?;
            let mut d = t.detail.clone();
            d.foreign_keys.clear();
            file.script(&dnt_driver_pg::ddl::build_table_ddl(&d), pg)?;
        }
        for ((schema, name), c) in &creates {
            if let Some((rel, col)) = &c.owner {
                file.statement(&format!(
                    "ALTER SEQUENCE {} OWNED BY {rel}.{}",
                    pg_relation(schema, name),
                    quote_ident(DbKind::Postgres, col)
                ))?;
            }
        }
        let mut rows = 0;
        for t in tables.iter().filter(|t| t.obj.data) {
            let rel = pg_relation(&schema_of(&t.obj), &t.obj.name);
            rows += self
                .data(ctx, r, file, t, format!("ONLY {rel}"), rel.clone())
                .await?;
        }
        // Sequence values after the data: they cover every row read.
        let mut setvals = Vec::new();
        for ((schema, name), c) in &creates {
            if let Some((rel, col)) = &c.owner {
                let (last, called) = self.sequence_value(r, schema, name).await?;
                setvals.push(setval_column_sql(rel, col, &last, &called));
            }
        }
        for t in &tables {
            let rel = pg_relation(&schema_of(&t.obj), &t.obj.name);
            for s in t.sequences.iter().filter(|s| s.identity) {
                let Some(col) = &s.owner else { continue };
                let (last, called) = self.sequence_value(r, &s.schema, &s.name).await?;
                setvals.push(setval_column_sql(&rel, col, &last, &called));
            }
        }
        if !setvals.is_empty() {
            file.comment("Sequence values")?;
        }
        for s in &setvals {
            file.statement(s)?;
        }
        for t in &tables {
            let rel = pg_relation(&schema_of(&t.obj), &t.obj.name);
            for fk in &t.detail.foreign_keys {
                file.statement(&add_fk_sql(
                    &rel,
                    &fk.name,
                    &fk_definition(fk, t.obj.schema.as_deref()),
                ))?;
            }
        }
        for &i in &view_seq {
            let (o, v) = &views[i];
            file.comment(&format!(
                "View {}",
                shown_name(o.schema.as_deref(), &o.name)
            ))?;
            file.script(&v.ddl, pg)?;
        }
        for s in session_epilogue(DbKind::Postgres) {
            file.statement(s)?;
        }
        r.exec("COMMIT").await?;
        let statements = file.statements();
        file.raw(&end_comment(statements))?;
        Ok(Written {
            tables: tables.len() as u32,
            views: views.len() as u32,
            rows,
            statements,
        })
    }

    async fn sequence_ddl(
        &self,
        r: &Reader<'_>,
        key: &(String, String),
    ) -> Result<String, DbError> {
        let oref = ObjectRef {
            database: self.request.database.clone(),
            schema: Some(key.0.clone()),
            name: key.1.clone(),
            kind: ObjectKind::Sequence,
        };
        r.meta(r.session.introspector().object_ddl(&oref)).await
    }

    async fn sequence_value(
        &self,
        r: &Reader<'_>,
        schema: &str,
        name: &str,
    ) -> Result<(String, String), DbError> {
        let sql = format!(
            "SELECT last_value, is_called FROM {}",
            pg_relation(schema, name)
        );
        let rows = r.rows(&sql, Vec::new()).await?;
        seq_literals(&rows, &format!("Sequence {schema}.{name}"))
    }
}
