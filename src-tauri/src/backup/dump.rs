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
use dnt_core::driver::{
    CancelHandle, CellMode, ExecOptions, ResultSummary, RowSink, Session, StopSignal,
};
use dnt_core::error::DbError;
use dnt_core::meta::{ForeignKeyDetail, IdentityKind, ObjectKind, ObjectRef, Scope, TableDetail};
use dnt_core::server::{DbKind, ServerInfo, ServerVariant};
use dnt_core::sql::build::{literal, ColumnTarget, TableRef};
use dnt_core::sql::ident::quote_ident;
use dnt_core::sql::{classify, Dialect};
use dnt_core::value::{ColumnMeta, FullValue, LogicalType, Value};
use dnt_transfer::dump::{
    add_fk_sql, create_schema_sql, end_comment, header, insert_head, mysql_view_ddl,
    session_epilogue, session_prologue, setval_column_sql, setval_sql, table_order,
    view_references, DumpHeader, DumpInserts, DumpWriter, TableNode, ViewNode, DUMP_BATCH_BYTES,
    DUMP_BATCH_ROWS,
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
/// Rows per batch the driver hands over while data is read (bounds memory with big cells).
pub const DATA_BATCH_ROWS: usize = 16;
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

/// Domains of schema `$1`: name.
pub(crate) const PG_DOMAINS: &str = "SELECT t.typname::text FROM pg_catalog.pg_type t \
     JOIN pg_catalog.pg_namespace n ON n.oid = t.typnamespace \
     WHERE n.nspname = $1 AND t.typtype = 'd' \
     AND pg_catalog.has_type_privilege(t.oid, 'USAGE') ORDER BY 1";

/// Domain `$1`.`$2`: base type, collation schema and name (when not the base type's),
/// default, NOT NULL, its CHECK constraints (`CONSTRAINT <name> <definition>`, by name).
pub(crate) const PG_DOMAIN: &str = "SELECT pg_catalog.format_type(t.typbasetype, t.typtypmod), \
     cn.nspname::text, co.collname::text, t.typdefault, t.typnotnull, \
     (SELECT pg_catalog.string_agg('CONSTRAINT ' || pg_catalog.quote_ident(k.conname) || ' ' \
       || pg_catalog.pg_get_constraintdef(k.oid), ' ' ORDER BY k.conname) \
      FROM pg_catalog.pg_constraint k WHERE k.contypid = t.oid AND k.contype = 'c') \
     FROM pg_catalog.pg_type t JOIN pg_catalog.pg_namespace n ON n.oid = t.typnamespace \
     JOIN pg_catalog.pg_type b ON b.oid = t.typbasetype \
     LEFT JOIN pg_catalog.pg_collation co ON co.oid = t.typcollation \
      AND t.typcollation <> b.typcollation \
     LEFT JOIN pg_catalog.pg_namespace cn ON cn.oid = co.collnamespace \
     WHERE n.nspname = $1 AND t.typname = $2 AND t.typtype = 'd'";

/// The enum, domain and stand-alone composite types the columns of table `$1` use, also
/// through arrays, domains and composite attributes: schema, name, `typtype`.
pub(crate) const PG_TABLE_TYPES: &str = "WITH RECURSIVE u(oid) AS ( \
     SELECT a.atttypid FROM pg_catalog.pg_attribute a \
     WHERE a.attrelid = $1::regclass AND a.attnum > 0 AND NOT a.attisdropped \
     UNION \
     SELECT x.next FROM u JOIN pg_catalog.pg_type ty ON ty.oid = u.oid, \
     LATERAL (SELECT ty.typelem WHERE ty.typcategory = 'A' AND ty.typelem <> 0 \
       UNION ALL SELECT ty.typbasetype WHERE ty.typtype = 'd' \
       UNION ALL SELECT a2.atttypid FROM pg_catalog.pg_attribute a2 \
       WHERE ty.typtype = 'c' AND a2.attrelid = ty.typrelid AND a2.attnum > 0 \
       AND NOT a2.attisdropped) x(next)) \
     SELECT n.nspname::text, ty.typname::text, ty.typtype::text \
     FROM u JOIN pg_catalog.pg_type ty ON ty.oid = u.oid \
     JOIN pg_catalog.pg_namespace n ON n.oid = ty.typnamespace \
     LEFT JOIN pg_catalog.pg_class c ON c.oid = ty.typrelid \
     WHERE (ty.typtype IN ('e', 'd') OR (ty.typtype = 'c' AND c.relkind = 'c')) \
     AND n.nspname NOT IN ('pg_catalog', 'information_schema') ORDER BY 1, 2";

/// Columns of table `$1` with a collation other than their type's: column, collation
/// schema, collation name.
pub(crate) const PG_TABLE_COLLATIONS: &str = "SELECT a.attname::text, n.nspname::text, \
     co.collname::text FROM pg_catalog.pg_attribute a \
     JOIN pg_catalog.pg_type t ON t.oid = a.atttypid \
     JOIN pg_catalog.pg_collation co ON co.oid = a.attcollation \
     JOIN pg_catalog.pg_namespace n ON n.oid = co.collnamespace \
     WHERE a.attrelid = $1::regclass AND a.attnum > 0 AND NOT a.attisdropped \
     AND a.attcollation <> 0 AND a.attcollation <> t.typcollation";

/// The options of sequence `$1`: start, increment, min, max, cache, cycle.
pub(crate) const PG_SEQUENCE_OPTIONS: &str = "SELECT s.seqstart, s.seqincrement, s.seqmin, \
     s.seqmax, s.seqcache, s.seqcycle FROM pg_catalog.pg_sequence s \
     WHERE s.seqrelid = $1::regclass";

/// The relations view `$1` reads (from its rewrite rule's dependencies): schema, name.
pub(crate) const PG_VIEW_USES: &str = "SELECT DISTINCT n.nspname::text, c.relname::text \
     FROM pg_catalog.pg_rewrite rw \
     JOIN pg_catalog.pg_depend d ON d.classid = 'pg_catalog.pg_rewrite'::regclass \
     AND d.objid = rw.oid AND d.refclassid = 'pg_catalog.pg_class'::regclass \
     JOIN pg_catalog.pg_class c ON c.oid = d.refobjid \
     JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
     WHERE rw.ev_class = $1::regclass AND c.oid <> rw.ev_class ORDER BY 1, 2";

/// MySQL 8.0.13+: the tables and views each view of database `?` reads: view, schema,
/// table.
pub(crate) const MYSQL_VIEW_USES: &str = "SELECT VIEW_NAME, TABLE_SCHEMA, TABLE_NAME \
     FROM information_schema.VIEW_TABLE_USAGE WHERE VIEW_SCHEMA = ?";

/// PostgreSQL types a driver decodes faithfully as array elements; arrays of any other
/// element type are read as the server's text.
const PG_FAITHFUL_ELEMENTS: &[&str] = &[
    "bool",
    "int2",
    "int4",
    "int8",
    "oid",
    "float4",
    "float8",
    "numeric",
    "json",
    "jsonb",
    "uuid",
    "bytea",
    "date",
    "time",
    "timestamp",
    "timestamptz",
    "text",
    "varchar",
    "bpchar",
    "name",
    "citext",
];

/// How a column is read for its `INSERT`s.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReadAs {
    /// As the driver decodes it.
    Value,
    /// PostgreSQL `"c"::text`: the server's text, written as `'<text>'::<type>`.
    Text,
    /// MySQL `CAST(`c` AS BINARY)`: the stored bytes, written as `X'…'` (spatial types).
    Binary,
}

/// How a column with the driver metadata `m` is read (the probe `WHERE 1 = 0`).
fn read_as(kind: DbKind, m: &ColumnMeta) -> ReadAs {
    match (kind, m.logical) {
        (DbKind::MySql, LogicalType::Other) => ReadAs::Binary,
        (DbKind::Postgres, LogicalType::Other) => ReadAs::Text,
        (DbKind::Postgres, LogicalType::Array) => {
            let elem = m.db_type.trim_end_matches("[]");
            if PG_FAITHFUL_ELEMENTS.contains(&elem) {
                ReadAs::Value
            } else {
                ReadAs::Text
            }
        }
        _ => ReadAs::Value,
    }
}

// ---------------------------------------------------------------------------------------
// Reading
// ---------------------------------------------------------------------------------------

/// The rows of one result (display values).
#[derive(Default)]
struct Collect {
    rows: Vec<Vec<Value>>,
    metas: Option<Vec<ColumnMeta>>,
    results: usize,
}

impl RowSink for Collect {
    fn result_start(&mut self, cols: &[ColumnMeta]) {
        self.results += 1;
        if self.results == 1 {
            self.metas = Some(cols.to_vec());
        }
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
        Ok(self.query(sql, params).await?.rows)
    }

    /// The column metadata of a statement's first result (`… WHERE 1 = 0`).
    async fn metas(&self, sql: &str) -> Result<Vec<ColumnMeta>, DbError> {
        Ok(self.query(sql, Vec::new()).await?.metas.unwrap_or_default())
    }

    async fn query(&self, sql: &str, params: Vec<String>) -> Result<Collect, DbError> {
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
        Ok(sink)
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
                // Domains (the introspector lists enums and composites only).
                for row in r.rows(PG_DOMAINS, vec![schema.clone()]).await? {
                    if let Some(name) = text_of(row.first()) {
                        out.objects.push(object(s, name, ObjectKind::Type));
                    }
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
    /// The same file, synced to disk before it is renamed.
    sync: std::fs::File,
    shown: String,
}

fn write_error(shown: &str, e: &std::io::Error) -> DbError {
    config_error("path", &format!("Could not write {shown}: {e}."))
}

impl DumpFile {
    /// Creates the temp file: only a NEW file (a stale temp file or anything planted there,
    /// a symbolic link included, is refused and left as it is).
    fn create(part: &Path, shown: &str, compress: bool, kind: DbKind) -> Result<DumpFile, DbError> {
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
        let sync = file.try_clone().map_err(|e| write_error(shown, &e))?;
        Ok(DumpFile {
            w: DumpWriter::new(
                Box::new(BufWriter::with_capacity(256 * 1024, file)),
                compress,
                Dialect::for_kind(kind),
            ),
            sync,
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

    /// Completes and syncs the file (its rename must not publish unwritten data).
    fn finish(self) -> Result<u64, DbError> {
        let shown = self.shown;
        let w = self.w;
        let sync = self.sync;
        block_outside_executor(|| -> std::io::Result<u64> {
            let n = w.finish()?;
            sync.sync_all()?;
            Ok(n)
        })
        .map_err(|e| write_error(&shown, &e))
    }
}

// ---------------------------------------------------------------------------------------
// Data
// ---------------------------------------------------------------------------------------

/// One inserted column of a table's data.
struct DataCol {
    name: String,
    /// As introspection reports it (the PostgreSQL cast target).
    db_type: String,
    read: ReadAs,
}

/// Turns the rows of one `SELECT` into `INSERT`s written to the file. The first error
/// (a write failure, a value that cannot be written) is kept and cancels the statement.
struct DataSink<'a> {
    file: &'a mut DumpFile,
    ctx: &'a JobCtx,
    /// The inserted columns in `SELECT` order.
    cols: Vec<DataCol>,
    kind: DbKind,
    relation: String,
    overriding: bool,
    phase: String,
    estimate: Option<u64>,
    inserts: Option<DumpInserts>,
    results: usize,
    rows: u64,
    error: Option<DbError>,
    /// Cancels the running `SELECT` once an error is kept.
    abort: Option<CancelHandle>,
}

impl DataSink<'_> {
    fn fail(&mut self, e: DbError) {
        if self.error.is_none() {
            self.error = Some(e);
            if let Some(handle) = self.abort.take() {
                tokio::spawn(async move {
                    let _ = handle.cancel().await;
                });
            }
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
            .map(|(c, m)| ColumnTarget {
                name: c.name.clone(),
                db_type: c.db_type.clone(),
                // Server text is written as `'<text>'::<type>` (a dump literal of `Other`).
                logical: if c.read == ReadAs::Text {
                    LogicalType::Other
                } else {
                    m.logical
                },
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

    fn full_rows(&mut self, mut batch: Vec<Vec<FullValue>>) {
        if self.error.is_some() || self.results != 1 {
            return;
        }
        let Some(mut inserts) = self.inserts.take() else {
            return;
        };
        let as_text: Vec<usize> = (0..self.cols.len())
            .filter(|&i| self.cols[i].read == ReadAs::Text)
            .collect();
        for row in &mut batch {
            for &i in &as_text {
                if let Some(Value::Text(text)) = row.get(i).and_then(FullValue::as_value) {
                    let other = Value::Other {
                        type_name: self.cols[i].db_type.clone(),
                        text: text.clone(),
                    };
                    if let Ok(v) = FullValue::try_from_value(other) {
                        row[i] = v;
                    }
                }
            }
        }
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
    /// PostgreSQL: the enum / domain / composite types its columns use (schema, name).
    types: Vec<(String, String)>,
    /// PostgreSQL: `(column, COLLATE clause)` of columns with a non-default collation.
    collations: Vec<(String, String)>,
    /// PostgreSQL: `(column, sequence options)` of identity columns.
    identity_options: Vec<(String, String)>,
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
    /// Neither asked for nor owned by a dumped column: only a column default uses it (the
    /// target may share it with other tables, so it is never dropped).
    needed_only: bool,
}

/// The detail `build_table_ddl` gets for a dumped table: no foreign keys (added after the
/// data), and the column parts it does not render itself: a non-default `COLLATE`, and an
/// identity column's sequence name and options.
fn pg_table_ddl_detail(t: &Table) -> TableDetail {
    let mut d = t.detail.clone();
    d.foreign_keys.clear();
    for c in &mut d.columns {
        if let Some((_, clause)) = t.collations.iter().find(|(n, _)| *n == c.name) {
            c.db_type = format!("{} {clause}", c.db_type);
        }
        if let Some((_, options)) = t.identity_options.iter().find(|(n, _)| *n == c.name) {
            let mode = match (&c.identity_kind, c.identity.as_deref()) {
                (Some(IdentityKind::Always), _) => "ALWAYS",
                (Some(IdentityKind::ByDefault), _) => "BY DEFAULT",
                (None, Some(m)) if m.eq_ignore_ascii_case("ALWAYS") => "ALWAYS",
                _ => "BY DEFAULT",
            };
            c.db_type = format!("{} GENERATED {mode} AS IDENTITY ({options})", c.db_type);
            c.identity = None;
            c.default = None;
        }
    }
    d
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

/// A catalog integer (sequence options) as a literal, through `build::literal`.
fn int_literal(v: Option<&Value>, what: &str) -> Result<String, DbError> {
    let v = v.ok_or_else(|| table_changed(what))?;
    let full = FullValue::try_from_value(v.clone()).map_err(|_| table_changed(what))?;
    literal(
        &full,
        &ColumnTarget {
            name: String::new(),
            db_type: "bigint".into(),
            logical: LogicalType::Integer,
            collation: None,
        },
        Dialect::Postgres,
    )
    .map_err(|_| table_changed(what))
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
        let kind = self.rt.cfg.kind;
        let mut file =
            tokio::task::spawn_blocking(move || DumpFile::create(&part, &shown, compress, kind))
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
                // Row security off: a table whose rows a policy would hide fails the dump
                // instead of dumping only the visible rows.
                r.exec("SET row_security = off").await?;
                // An empty search_path: every pg_get_* / format_type text the dump writes
                // is schema-qualified (R23).
                r.exec("SELECT pg_catalog.set_config('search_path', '', false)")
                    .await?;
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
                types: Vec::new(),
                collations: Vec::new(),
                identity_options: Vec::new(),
            };
            match r.kind() {
                DbKind::MySql => t.ddl = r.meta(intro.object_ddl(&oref)).await?,
                DbKind::Postgres => {
                    let rel = pg_relation(o.schema.as_deref().unwrap_or("public"), &o.name);
                    let q = |n: &str| quote_ident(DbKind::Postgres, n);
                    for row in r.rows(PG_TABLE_TYPES, vec![rel.clone()]).await? {
                        if let (Some(s), Some(n)) = (text_of(row.first()), text_of(row.get(1))) {
                            t.types.push((s, n));
                        }
                    }
                    for row in r.rows(PG_TABLE_COLLATIONS, vec![rel.clone()]).await? {
                        if let (Some(c), Some(s), Some(n)) = (
                            text_of(row.first()),
                            text_of(row.get(1)),
                            text_of(row.get(2)),
                        ) {
                            t.collations
                                .push((c, format!("COLLATE {}.{}", q(&s), q(&n))));
                        }
                    }
                    for row in r.rows(PG_TABLE_SEQUENCES, vec![rel.clone()]).await? {
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
                    // Identity columns keep their sequence's name and options.
                    for s in t.sequences.iter().filter(|s| s.identity) {
                        let Some(col) = &s.owner else { continue };
                        let seq = pg_relation(&s.schema, &s.name);
                        let rows = r.rows(PG_SEQUENCE_OPTIONS, vec![seq.clone()]).await?;
                        if let Some(row) = rows.first() {
                            let what = format!("Sequence {}.{}", s.schema, s.name);
                            let n = |i: usize| int_literal(row.get(i), &what);
                            let cycle = if bool_of(row.get(5)) {
                                "CYCLE"
                            } else {
                                "NO CYCLE"
                            };
                            let options = format!(
                                "SEQUENCE NAME {seq} START WITH {} INCREMENT BY {} MINVALUE {} \
                                 MAXVALUE {} CACHE {} {cycle}",
                                n(0)?,
                                n(1)?,
                                n(2)?,
                                n(3)?,
                                n(4)?
                            );
                            t.identity_options.push((col.clone(), options));
                        }
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
        let quoted: Vec<String> = cols.iter().map(|c| quote_ident(kind, &c.name)).collect();
        // The driver's types first: they decide how each column is read.
        let probe = format!(
            "SELECT {} FROM {select_from} WHERE 1 = 0",
            quoted.join(", ")
        );
        let metas = r.metas(&probe).await?;
        if metas.len() != cols.len() {
            return Err(table_changed(&format!("Table {label}")));
        }
        let data_cols: Vec<DataCol> = cols
            .iter()
            .zip(&metas)
            .map(|(c, m)| DataCol {
                name: c.name.clone(),
                db_type: c.db_type.clone(),
                read: read_as(kind, m),
            })
            .collect();
        let list: Vec<String> = data_cols
            .iter()
            .zip(&quoted)
            .map(|(c, q)| match c.read {
                ReadAs::Value => q.clone(),
                ReadAs::Text => format!("{q}::text"),
                ReadAs::Binary => format!("CAST({q} AS BINARY)"),
            })
            .collect();
        let sql = format!("SELECT {} FROM {select_from}", list.join(", "));
        file.comment(&format!("Data of {label}"))?;
        let phase = format!("Table {label}");
        ctx.progress(&phase, 0, t.detail.row_estimate, ProgressUnit::Rows);
        let opts = ExecOptions {
            max_rows: usize::MAX,
            // Few rows per batch: a batch of large values is held in memory whole.
            batch_size: DATA_BATCH_ROWS,
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
            cols: data_cols,
            kind,
            relation: insert_into,
            overriding,
            phase,
            estimate: t.detail.row_estimate,
            inserts: None,
            results: 0,
            rows: 0,
            error: None,
            abort: Some(r.session.cancel_handle()),
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
        let has_table = |n: &str| tables.iter().any(|t| t.obj.name.eq_ignore_ascii_case(n));
        let has_view = |n: &str| views.iter().any(|v| v.name.eq_ignore_ascii_case(n));
        for t in &tables {
            for fk in &t.detail.foreign_keys {
                let outside = match &fk.ref_schema {
                    Some(other) => Some(format!("{other}.{}", fk.ref_table)),
                    None => (!has_table(&fk.ref_table)).then(|| fk.ref_table.clone()),
                };
                if let Some(to) = outside {
                    ctx.log(
                        JobLogLevel::Warn,
                        format!(
                            "Foreign key {} of {} references {to}, which is not in the dump; \
                             restoring needs that table.",
                            fk.name, t.obj.name
                        ),
                    );
                }
            }
        }

        // What each view reads: the server's own record where it keeps one (MySQL
        // 8.0.13+), else the names in table positions of its DDL.
        let mut uses: Vec<Vec<(Option<String>, String)>> = vec![Vec::new(); views.len()];
        let recorded = match info.variant {
            ServerVariant::MySql8 => match r.rows(MYSQL_VIEW_USES, vec![db.clone()]).await {
                Ok(rows) => Some(rows),
                Err(DbError::Server { .. }) => None,
                Err(e) => return Err(e),
            },
            _ => None,
        };
        match recorded {
            Some(rows) => {
                for row in rows {
                    let (Some(view), Some(schema), Some(name)) = (
                        text_of(row.first()),
                        text_of(row.get(1)),
                        text_of(row.get(2)),
                    ) else {
                        continue;
                    };
                    if let Some(i) = views.iter().position(|v| v.name == view) {
                        uses[i].push(((schema != *db).then_some(schema), name));
                    }
                }
            }
            None => {
                for (i, v) in views.iter().enumerate() {
                    uses[i] = view_references(&v.ddl, Dialect::MYSQL);
                }
            }
        }
        for (i, v) in views.iter().enumerate() {
            for (schema, name) in &uses[i] {
                let outside = match schema {
                    Some(other) => Some(format!("{other}.{name}")),
                    None => (!has_table(name) && !has_view(name)).then(|| name.clone()),
                };
                if let Some(what) = outside {
                    ctx.log(
                        JobLogLevel::Warn,
                        format!(
                            "View {} reads {what}, which is not in the dump; restoring needs it.",
                            v.name
                        ),
                    );
                }
            }
        }
        let view_nodes: Vec<TableNode> = views
            .iter()
            .zip(&uses)
            .map(|(v, u)| TableNode {
                schema: None,
                name: v.name.clone(),
                references: u
                    .iter()
                    .filter(|(s, _)| s.is_none())
                    .map(|(_, n)| (None, n.clone()))
                    .collect(),
            })
            .collect();
        let view_seq = table_order(&view_nodes);

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
        for i in view_seq {
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
        let mut objects = objects.to_vec();
        let tables = Self::ordered(self.read_tables(r, &objects).await?);

        // The types the columns use: those of the dumped schemas are dumped too.
        for t in &tables {
            let label = shown_name(t.obj.schema.as_deref(), &t.obj.name);
            for (schema, name) in &t.types {
                let present = objects.iter().any(|o| {
                    o.kind == ObjectKind::Type
                        && o.schema.as_deref() == Some(schema.as_str())
                        && &o.name == name
                });
                if present {
                    continue;
                }
                if self.request.schemas.contains(schema) {
                    ctx.log(
                        JobLogLevel::Info,
                        format!("Type {schema}.{name} is dumped too: table {label} uses it."),
                    );
                    objects.push(DumpObject {
                        schema: Some(schema.clone()),
                        name: name.clone(),
                        kind: ObjectKind::Type,
                        data: false,
                    });
                } else {
                    ctx.log(
                        JobLogLevel::Warn,
                        format!(
                            "Table {label} uses type {schema}.{name}, whose schema is not \
                             dumped; restoring needs it."
                        ),
                    );
                }
            }
        }
        // Types: enums, then domains (an enum may be their base), then composites.
        let mut types: Vec<(DumpObject, u8, String)> = Vec::new();
        for o in objects.iter().filter(|o| o.kind == ObjectKind::Type) {
            let (rank, ddl) = match self.domain_ddl(r, &schema_of(o), &o.name).await? {
                Some(ddl) => (1, ddl),
                None => {
                    let ddl = r.meta(intro.object_ddl(&obj_ref(db, o))).await?;
                    (if ddl.contains(" AS ENUM (") { 0 } else { 2 }, ddl)
                }
            };
            types.push((o.clone(), rank, ddl));
        }
        types.sort_by_key(|(o, rank, _)| (*rank, schema_of(o), o.name.clone()));

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
                    needed_only: false,
                },
            );
        }
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
                                needed_only: false,
                            },
                        );
                    }
                    // Only a column default uses it.
                    (None, _) => {
                        if let Entry::Vacant(slot) = creates.entry(key) {
                            let ddl = self.sequence_ddl(r, slot.key()).await?;
                            slot.insert(SeqCreate {
                                ddl,
                                owner: None,
                                value: None,
                                needed_only: true,
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

        // Views, in the order of what each reads (its rewrite rule's dependencies).
        let mut views = Vec::new();
        for o in objects.iter().filter(|o| o.kind == ObjectKind::View) {
            let ddl = r.meta(intro.object_ddl(&obj_ref(db, o))).await?;
            let rel = pg_relation(&schema_of(o), &o.name);
            let mut uses = Vec::new();
            for row in r.rows(PG_VIEW_USES, vec![rel]).await? {
                if let (Some(s), Some(n)) = (text_of(row.first()), text_of(row.get(1))) {
                    uses.push((s, n));
                }
            }
            views.push((o.clone(), ddl, uses));
        }
        let dumped_tables: HashSet<(String, String)> = tables
            .iter()
            .map(|t| (schema_of(&t.obj), t.obj.name.clone()))
            .collect();
        let dumped_views: HashSet<(String, String)> = views
            .iter()
            .map(|(o, _, _)| (schema_of(o), o.name.clone()))
            .collect();
        for (o, _, uses) in &views {
            for used in uses {
                if !dumped_tables.contains(used) && !dumped_views.contains(used) {
                    ctx.log(
                        JobLogLevel::Warn,
                        format!(
                            "View {} reads {}.{}, which is not in the dump; restoring needs it.",
                            shown_name(o.schema.as_deref(), &o.name),
                            used.0,
                            used.1
                        ),
                    );
                }
            }
        }
        let view_nodes: Vec<TableNode> = views
            .iter()
            .map(|(o, _, uses)| TableNode {
                schema: o.schema.clone(),
                name: o.name.clone(),
                references: uses
                    .iter()
                    .filter(|u| dumped_views.contains(*u))
                    .map(|(s, n)| (Some(s.clone()), n.clone()))
                    .collect(),
            })
            .collect();
        let view_seq = table_order(&view_nodes);

        for t in &tables {
            for fk in &t.detail.foreign_keys {
                let to = (
                    fk.ref_schema.clone().unwrap_or_else(|| schema_of(&t.obj)),
                    fk.ref_table.clone(),
                );
                if !dumped_tables.contains(&to) {
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
            &objects,
            Some(creates.len() as u32),
        )))?;
        for s in session_prologue(DbKind::Postgres) {
            file.statement(s)?;
        }
        let include_drop = self.request.include_drop;
        if include_drop {
            // Views and tables take their dependents in the target with them; sequences
            // and types never do (a column of another table would go).
            for &i in view_seq.iter().rev() {
                let (o, _, _) = &views[i];
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
            for ((schema, name), c) in &creates {
                if !c.needed_only {
                    file.statement(&format!(
                        "DROP SEQUENCE IF EXISTS {}",
                        pg_relation(schema, name)
                    ))?;
                }
            }
            for (o, _, _) in types.iter().rev() {
                file.statement(&format!(
                    "DROP TYPE IF EXISTS {}",
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
        for (o, _, ddl) in &types {
            file.comment(&format!(
                "Type {}",
                shown_name(o.schema.as_deref(), &o.name)
            ))?;
            file.script(ddl, pg)?;
        }
        for ((schema, name), c) in &creates {
            file.comment(&format!("Sequence {schema}.{name}"))?;
            // A sequence other tables of the target may share is kept when it exists.
            let ddl = if c.needed_only && include_drop {
                c.ddl
                    .replacen("CREATE SEQUENCE ", "CREATE SEQUENCE IF NOT EXISTS ", 1)
            } else {
                c.ddl.clone()
            };
            file.script(&ddl, pg)?;
            if let Some((last, called)) = &c.value {
                file.statement(&setval_sql(&pg_relation(schema, name), last, called))?;
            }
        }
        for t in &tables {
            file.comment(&format!(
                "Table {}",
                shown_name(t.obj.schema.as_deref(), &t.obj.name)
            ))?;
            file.script(
                &dnt_driver_pg::ddl::build_table_ddl(&pg_table_ddl_detail(t)),
                pg,
            )?;
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
            let (o, ddl, _) = &views[i];
            file.comment(&format!(
                "View {}",
                shown_name(o.schema.as_deref(), &o.name)
            ))?;
            file.script(ddl, pg)?;
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

    /// `CREATE DOMAIN` of domain `schema`.`name` from the catalog; `None`: not a domain.
    async fn domain_ddl(
        &self,
        r: &Reader<'_>,
        schema: &str,
        name: &str,
    ) -> Result<Option<String>, DbError> {
        let rows = r
            .rows(PG_DOMAIN, vec![schema.to_string(), name.to_string()])
            .await?;
        let Some(row) = rows.first() else {
            return Ok(None);
        };
        let base = text_of(row.first()).ok_or_else(|| table_changed(&format!("Type {name}")))?;
        let q = |n: &str| quote_ident(DbKind::Postgres, n);
        let mut ddl = format!("CREATE DOMAIN {} AS {base}", pg_relation(schema, name));
        if let (Some(cs), Some(cn)) = (text_of(row.get(1)), text_of(row.get(2))) {
            ddl.push_str(&format!(" COLLATE {}.{}", q(&cs), q(&cn)));
        }
        if let Some(default) = text_of(row.get(3)) {
            ddl.push_str(&format!(" DEFAULT {default}"));
        }
        if bool_of(row.get(4)) {
            ddl.push_str(" NOT NULL");
        }
        if let Some(checks) = text_of(row.get(5)) {
            ddl.push(' ');
            ddl.push_str(&checks);
        }
        Ok(Some(ddl))
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
