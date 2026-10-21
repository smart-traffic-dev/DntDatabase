//! The table designer's backend (spec §4.3): designs opened on a table (or a new one),
//! the live SQL of a draft, and single-use plans applied on a dedicated DDL session
//! through [`crate::ddl_exec`] (see `apply`).
//!
//! The UI only ever sends a `TableDef`, never SQL. Every draft is checked against the
//! base stored here: an element locked in the base is replaced by the base's copy, a
//! client `locked` flag is never trusted, and an `original` naming nothing in the base
//! is an error (rulings T5-4, T5-7, T5-9).
//!
//! MySQL `SHOW CREATE TABLE` is read with `ANSI_QUOTES` off (ruling T9-2): when the
//! session's `sql_mode` has it, the mode is switched off for that one read and restored.
mod apply;
#[cfg(test)]
mod tests;

pub use apply::{CountedImpact, DesignPreview, DesignReport};

use crate::ddl_exec::DdlOperations;
use crate::session_manager::{
    close_quietly, config_error, lock, ConnectionRuntime, SessionManager, METADATA_CALL_TIMEOUT,
};
use dnt_core::config::EnvTag;
use dnt_core::driver::{ExecOptions, ResultSummary, RowSink, Session, StopSignal};
use dnt_core::error::DbError;
use dnt_core::meta::{ObjectKind, ObjectRef, Scope, TableDetail};
use dnt_core::server::{DbKind, ServerInfo};
use dnt_core::sql::build::TableRef;
use dnt_core::sql::Dialect;
use dnt_core::value::{ColumnMeta, Value};
use dnt_schema::{
    apply_locks, diff, fidelity, from_detail, new_table, render, render_create, type_catalog,
    validate, Change, CheckDef, ColumnDef, DiffError, DraftError, ElementRef, FkDef, Impact,
    IndexDef, KeyDef, Locked, TableDef, TypeInfo,
};
use serde::{Deserialize, Serialize};
use specta::Type;
use std::collections::HashMap;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use uuid::Uuid;

/// Designs kept open at once.
pub(crate) const MAX_DESIGNS: usize = 64;

/// Bound of each impact count query.
pub(crate) const COUNT_TIMEOUT: Duration = Duration::from_secs(2);

/// The fingerprint of a design for a table that does not exist yet.
const NEW_TABLE: &str = "new";

#[derive(Debug, Clone, Serialize, Deserialize, Type)]
pub struct DesignOpen {
    pub design_id: Uuid,
    pub def: TableDef,
    pub locked: Vec<Locked>,
    pub catalog: Vec<TypeInfo>,
    pub server: ServerInfo,
    pub env: Option<EnvTag>,
    pub read_only: bool,
}

/// The live SQL of a draft. `errors` are shown at their elements; `error` is a
/// problem of the change as a whole (a locked element, an unsupported feature, a
/// change that cannot be rendered).
#[derive(Debug, Clone, Default, Serialize, Deserialize, Type)]
pub struct DesignRender {
    pub statements: Vec<String>,
    pub impacts: Vec<Impact>,
    pub errors: Vec<DraftError>,
    pub error: Option<String>,
}

/// One open design.
#[derive(Clone)]
pub(crate) struct Design {
    conn: Uuid,
    scope: Scope,
    /// `None` for a table not created yet.
    object: Option<ObjectRef>,
    base: TableDef,
    server: ServerInfo,
    /// The dialect of the session the design was opened on (render, live SQL).
    dialect: Dialect,
    fingerprint: String,
}

impl Design {
    /// The relation a diff applies to (the stored table) or a new table is created in.
    fn table_ref(&self, draft_name: &str) -> TableRef {
        TableRef {
            kind: self.server.kind,
            database: self.scope.database.clone(),
            schema: self.scope.schema.clone(),
            name: self
                .base
                .original_name
                .clone()
                .unwrap_or_else(|| draft_name.to_string()),
        }
    }
}

pub(crate) struct DesignStore {
    designs: Mutex<HashMap<Uuid, Design>>,
    plans: Mutex<HashMap<Uuid, apply::DesignPlan>>,
    pub(crate) operations: DdlOperations,
    /// [`COUNT_TIMEOUT`]; tests set it shorter.
    pub(crate) count_timeout: Duration,
}

impl Default for DesignStore {
    fn default() -> Self {
        DesignStore {
            designs: Mutex::default(),
            plans: Mutex::default(),
            operations: DdlOperations::default(),
            count_timeout: COUNT_TIMEOUT,
        }
    }
}

impl DesignStore {
    fn get(&self, id: Uuid) -> Result<Design, DbError> {
        lock(&self.designs).get(&id).cloned().ok_or_else(|| {
            config_error(
                "design_id",
                "This designer is no longer open; open the table again.",
            )
        })
    }
}

/// The session database of a design's DDL session: MySQL needs none (names are
/// qualified), PostgreSQL the database of the table.
fn session_db(kind: DbKind, scope: &Scope) -> Option<String> {
    (kind == DbKind::Postgres).then(|| scope.database.clone())
}

fn timed_out() -> DbError {
    config_error("table", "Metadata timed out; nothing was changed.")
}

pub(crate) fn table_changed() -> DbError {
    DbError::PlanStale {
        reason: "Table changed".into(),
    }
}

/// First column of the first row, as text.
#[derive(Default)]
struct FirstText(Option<String>);

impl RowSink for FirstText {
    fn result_start(&mut self, _: &[ColumnMeta]) {}
    fn rows(&mut self, batch: Vec<Vec<Value>>) {
        if self.0.is_none() {
            if let Some(Value::Text(s)) =
                batch.into_iter().next().and_then(|r| r.into_iter().next())
            {
                self.0 = Some(s);
            }
        }
    }
    fn result_end(&mut self, _: &ResultSummary) {}
}

async fn first_text(session: &dyn Session, sql: &str) -> Result<Option<String>, DbError> {
    let mut sink = FirstText::default();
    session
        .execute_one(
            sql,
            dnt_core::sql::classify(sql, session.dialect()),
            &ExecOptions::default(),
            &mut sink,
        )
        .await?;
    Ok(sink.0)
}

/// `sql_mode` without the modes that turn `ANSI_QUOTES` on (it is stored with composite
/// modes listed next to their parts); `None` when a mode name is not a plain word.
fn without_ansi_quotes(mode: &str) -> Option<String> {
    const ANSI: &[&str] = &[
        "ANSI_QUOTES",
        "ANSI",
        "DB2",
        "MAXDB",
        "MSSQL",
        "ORACLE",
        "POSTGRESQL",
    ];
    let mut kept = Vec::new();
    for m in mode.split(',').map(str::trim).filter(|m| !m.is_empty()) {
        if !m.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') {
            return None;
        }
        if !ANSI.iter().any(|a| a.eq_ignore_ascii_case(m)) {
            kept.push(m);
        }
    }
    Some(kept.join(","))
}

/// The table's DDL as the server prints it. On MySQL with `ANSI_QUOTES` on, the
/// session's `sql_mode` is switched for this read and restored afterwards (`None`:
/// it could not be, so the caller treats the DDL as unreadable).
async fn read_ddl(session: &dyn Session, obj: &ObjectRef) -> Result<Option<String>, DbError> {
    let ansi = matches!(session.dialect(), Dialect::MySql(m) if m.ansi_quotes);
    if !ansi {
        return session.introspector().object_ddl(obj).await.map(Some);
    }
    let Some(saved) = first_text(session, "SELECT @@SESSION.sql_mode").await? else {
        return Ok(None);
    };
    // `None` also when any word of `saved` is not plain, so `saved` splices safely below.
    let Some(plain) = without_ansi_quotes(&saved) else {
        return Ok(None);
    };
    let grace = dnt_core::driver::DEFAULT_CANCEL_GRACE;
    crate::ddl_exec::execute(
        session,
        &format!("SET SESSION sql_mode = '{plain}'"),
        StopSignal::new(),
        grace,
        0,
    )
    .await?;
    let ddl = session.introspector().object_ddl(obj).await;
    crate::ddl_exec::execute(
        session,
        &format!("SET SESSION sql_mode = '{saved}'"),
        StopSignal::new(),
        grace,
        0,
    )
    .await?;
    ddl.map(Some)
}

/// The detail and the DDL text of `obj`. A DDL the server refuses to show is `None`
/// (MySQL then opens the table read-only).
async fn read_table(
    session: &dyn Session,
    obj: &ObjectRef,
) -> Result<(TableDetail, Option<String>), DbError> {
    let detail = session.introspector().table_detail(obj).await?;
    let ddl = match read_ddl(session, obj).await {
        Ok(ddl) => ddl,
        Err(DbError::Server { .. }) => None,
        Err(e) => return Err(e),
    };
    Ok((detail, ddl))
}

/// `AUTO_INCREMENT=n` of a MySQL table trailer: it moves with every insert.
fn strip_auto_increment(ddl: &str) -> String {
    const KEY: &str = "AUTO_INCREMENT=";
    let mut out = String::with_capacity(ddl.len());
    let mut rest = ddl;
    while let Some(i) = rest.find(KEY) {
        out.push_str(&rest[..i]);
        let tail = &rest[i + KEY.len()..];
        let digits = tail.bytes().take_while(u8::is_ascii_digit).count();
        if digits == 0 {
            out.push_str(KEY);
        }
        rest = &tail[digits..];
    }
    out.push_str(rest);
    out
}

/// What a plan is checked against: the detail without its statistics and counters,
/// plus the DDL text.
fn fingerprint(detail: &TableDetail, ddl: Option<&str>) -> String {
    let mut d = detail.clone();
    d.row_estimate = None;
    d.size_bytes = None;
    d.options.auto_increment = None;
    let json = serde_json::to_string(&d).unwrap_or_default();
    let mut h = DefaultHasher::new();
    json.hash(&mut h);
    ddl.map(strip_auto_increment).hash(&mut h);
    format!("{:016x}", h.finish())
}

/// A design's base as the server has it now.
struct Loaded {
    base: TableDef,
    locked: Vec<Locked>,
    fingerprint: String,
}

async fn load(session: &dyn Session, object: Option<&ObjectRef>) -> Result<Loaded, DbError> {
    let server = session.server_info().clone();
    let Some(obj) = object else {
        return Ok(Loaded {
            base: new_table("new_table", &server),
            locked: Vec::new(),
            fingerprint: NEW_TABLE.into(),
        });
    };
    let (detail, ddl) = read_table(session, obj).await?;
    let mut base = from_detail(&detail, &server);
    let mysql_create = ddl.as_deref().filter(|_| server.kind == DbKind::MySql);
    let locked = fidelity(&base, &detail, mysql_create, &server);
    apply_locks(&mut base, &locked);
    Ok(Loaded {
        base,
        locked,
        fingerprint: fingerprint(&detail, ddl.as_deref()),
    })
}

/// The fingerprint of `object` read on `session` now.
async fn current_fingerprint(
    session: &dyn Session,
    object: Option<&ObjectRef>,
) -> Result<String, DbError> {
    match object {
        None => Ok(NEW_TABLE.into()),
        Some(obj) => {
            let (detail, ddl) = read_table(session, obj).await?;
            Ok(fingerprint(&detail, ddl.as_deref()))
        }
    }
}

/// An element of a draft matched to the base by `original`.
trait Element: Clone {
    fn original(&self) -> Option<&str>;
    fn locked(&self) -> bool;
    fn set_locked(&mut self, locked: bool);
    fn element(&self) -> ElementRef;
}

macro_rules! element {
    ($t:ty, $e:expr) => {
        impl Element for $t {
            fn original(&self) -> Option<&str> {
                self.original.as_deref()
            }
            fn locked(&self) -> bool {
                self.locked
            }
            fn set_locked(&mut self, locked: bool) {
                self.locked = locked;
            }
            fn element(&self) -> ElementRef {
                #[allow(clippy::redundant_closure_call)]
                ($e)(self)
            }
        }
    };
}

element!(ColumnDef, |c: &ColumnDef| ElementRef::Column(
    c.name.clone()
));
element!(KeyDef, |_: &KeyDef| ElementRef::PrimaryKey);
element!(IndexDef, |i: &IndexDef| ElementRef::Index(i.name.clone()));
element!(FkDef, |f: &FkDef| ElementRef::ForeignKey(f.name.clone()));
element!(CheckDef, |c: &CheckDef| ElementRef::Check(c.name.clone()));

fn sync<T: Element>(base: &[T], draft: &mut [T], errors: &mut Vec<DraftError>) {
    for d in draft.iter_mut() {
        let Some(original) = d.original().map(str::to_string) else {
            d.set_locked(false);
            continue;
        };
        match base.iter().find(|b| b.original() == Some(original.as_str())) {
            None => errors.push(DraftError {
                element: d.element(),
                field: Some("original".into()),
                message: format!(
                    "\"{original}\" is not part of the table as it was opened; open the table again."
                ),
            }),
            Some(b) if b.locked() => *d = b.clone(),
            Some(_) => d.set_locked(false),
        }
    }
}

/// `draft` with every element locked in `base` replaced by the base's copy and every
/// other `locked` flag cleared; errors for `original`s that name nothing in `base`.
fn prepare(base: &TableDef, mut draft: TableDef) -> Result<TableDef, Vec<DraftError>> {
    let mut errors = Vec::new();
    if draft.original_name != base.original_name {
        errors.push(DraftError {
            element: ElementRef::Table,
            field: Some("original_name".into()),
            message: "The draft belongs to another table; open the table again.".into(),
        });
    }
    sync(&base.columns, &mut draft.columns, &mut errors);
    sync(
        base.primary_key.as_slice(),
        draft.primary_key.as_mut_slice(),
        &mut errors,
    );
    sync(&base.indexes, &mut draft.indexes, &mut errors);
    sync(&base.foreign_keys, &mut draft.foreign_keys, &mut errors);
    sync(&base.checks, &mut draft.checks, &mut errors);
    if base.options.locked {
        draft.options = base.options.clone();
    } else {
        draft.options.locked = false;
    }
    draft.read_only_reason = base.read_only_reason.clone();
    if errors.is_empty() {
        Ok(draft)
    } else {
        Err(errors)
    }
}

fn describe(e: &ElementRef) -> String {
    match e {
        ElementRef::Table => "The table".into(),
        ElementRef::Column(n) => format!("Column \"{n}\""),
        ElementRef::PrimaryKey => "The primary key".into(),
        ElementRef::Index(n) => format!("Index \"{n}\""),
        ElementRef::ForeignKey(n) => format!("Foreign key \"{n}\""),
        ElementRef::Check(n) => format!("Check \"{n}\""),
        ElementRef::Options => "The table options".into(),
    }
}

/// validate + diff + render of `draft` against the design's base.
fn compute(design: &Design, draft: TableDef, dialect: Dialect) -> DesignRender {
    let mut out = DesignRender::default();
    let draft = match prepare(&design.base, draft) {
        Ok(d) => d,
        Err(errors) => {
            out.errors = errors;
            return out;
        }
    };
    out.errors = validate(&draft, &design.server);
    if !out.errors.is_empty() {
        return out;
    }
    let changes = match diff(&design.base, &draft) {
        Ok(c) => c,
        Err(DiffError::Invalid(errors)) => {
            out.errors = errors;
            return out;
        }
        Err(DiffError::Locked { element, reason }) => {
            out.error = Some(format!("{} is locked: {reason}", describe(&element)));
            return out;
        }
    };
    let table = design.table_ref(&draft.name);
    let rendered = if matches!(changes.as_slice(), [Change::CreateTable]) {
        render_create(&table, &draft, &design.server, dialect)
    } else {
        render(&table, &design.base, &changes, &design.server, dialect)
    };
    match rendered {
        Ok(r) => {
            out.statements = r.statements;
            out.impacts = r.impacts;
        }
        Err(e) => out.error = Some(e.to_string()),
    }
    out
}

impl SessionManager {
    pub async fn table_design_open(
        &self,
        conn: Uuid,
        scope: Scope,
        table: Option<String>,
    ) -> Result<DesignOpen, DbError> {
        let cfg = self.repo.get(conn)?;
        match (cfg.kind, &scope.schema) {
            (DbKind::MySql, Some(_)) => {
                return Err(config_error(
                    "scope",
                    "MySQL has no schemas inside a database.",
                ))
            }
            (DbKind::Postgres, None) => {
                return Err(config_error("scope", "A schema is required on PostgreSQL."))
            }
            _ => {}
        }
        if scope.database.is_empty() {
            return Err(config_error("scope", "A database is required."));
        }
        let rt = self.runtime(conn)?;
        let object = table.map(|name| ObjectRef {
            database: scope.database.clone(),
            schema: scope.schema.clone(),
            name,
            kind: ObjectKind::Table,
        });
        let (session, _) = rt.open_query_session(session_db(cfg.kind, &scope)).await?;
        let server = session.server_info().clone();
        let dialect = session.dialect();
        let loaded = tokio::time::timeout(
            METADATA_CALL_TIMEOUT,
            load(session.as_ref(), object.as_ref()),
        )
        .await;
        close_quietly(session).await;
        let loaded = loaded.map_err(|_| timed_out())??;
        let design_id = Uuid::new_v4();
        {
            let mut designs = lock(&self.table_designs.designs);
            if designs.len() >= MAX_DESIGNS {
                return Err(DbError::Busy);
            }
            designs.insert(
                design_id,
                Design {
                    conn,
                    scope,
                    object,
                    base: loaded.base.clone(),
                    server: server.clone(),
                    dialect,
                    fingerprint: loaded.fingerprint,
                },
            );
        }
        Ok(DesignOpen {
            design_id,
            def: loaded.base,
            locked: loaded.locked,
            catalog: type_catalog(&server),
            server,
            env: cfg.env_tag,
            read_only: cfg.read_only,
        })
    }

    /// The SQL of `draft`, from the stored base; reads nothing from the server.
    pub async fn table_design_render(
        &self,
        design_id: Uuid,
        draft: TableDef,
    ) -> Result<DesignRender, DbError> {
        let design = self.table_designs.get(design_id)?;
        let dialect = design.dialect;
        Ok(compute(&design, draft, dialect))
    }

    pub async fn table_design_close(&self, design_id: Uuid) -> Result<(), DbError> {
        lock(&self.table_designs.designs).remove(&design_id);
        lock(&self.table_designs.plans).retain(|_, p| p.design_id != design_id);
        Ok(())
    }

    pub async fn table_design_cancel(&self, operation_id: Uuid) -> Result<(), DbError> {
        self.table_designs.operations.cancel(operation_id).await
    }

    /// Closing `conn`: its designs and plans are dropped, its applies cancelled.
    pub(crate) async fn design_cancel_connection(&self, conn: Uuid) {
        lock(&self.table_designs.designs).retain(|_, d| d.conn != conn);
        lock(&self.table_designs.plans).retain(|_, p| p.conn != conn);
        self.table_designs.operations.cancel_connection(conn).await;
    }

    /// Reads `object` again and makes it the base of `design_id` (if still open).
    async fn reload_design(
        &self,
        design_id: Uuid,
        rt: &Arc<ConnectionRuntime>,
        scope: &Scope,
        object: Option<ObjectRef>,
    ) -> Result<(), DbError> {
        let (session, _) = rt
            .open_query_session(session_db(rt.cfg.kind, scope))
            .await?;
        let dialect = session.dialect();
        let loaded = tokio::time::timeout(
            METADATA_CALL_TIMEOUT,
            load(session.as_ref(), object.as_ref()),
        )
        .await;
        close_quietly(session).await;
        let loaded = loaded.map_err(|_| timed_out())??;
        if let Some(d) = lock(&self.table_designs.designs).get_mut(&design_id) {
            d.base = loaded.base;
            d.fingerprint = loaded.fingerprint;
            d.object = object;
            d.dialect = dialect;
        }
        Ok(())
    }
}
