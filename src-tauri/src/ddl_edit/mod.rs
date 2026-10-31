//! Editing the DDL of views, materialized views, routines, triggers and events (P4c,
//! spec §4.2, §5): edits opened on an object (or a new one), previews that validate
//! the edited text, build the statements, judge them and store a single-use plan.
//!
//! The UI sends the edited text and a strategy, never statements. The text only runs
//! after `isolate` + `parse_header` + `check_target` accepted it, and the statements
//! previewed are exactly the ones a plan stores (and the apply sends).
mod apply;
pub(crate) mod catalog;
#[cfg(test)]
mod tests;

pub use apply::{DdlEditReport, EditOutcome, ObjectState};

use crate::ddl_exec::DdlOperations;
use crate::object_actions::{stale, MAX_PLANS};
use crate::session_manager::{
    close_quietly, config_error, lock, sql_or_close, ConnectionRuntime, SessionManager,
    METADATA_CALL_TIMEOUT,
};
use crate::table_design::ddl_not_shown;
use dnt_core::config::EnvTag;
use dnt_core::driver::SqlSession;
use dnt_core::error::DbError;
use dnt_core::meta::{ObjectKind, ObjectRef, Scope};
use dnt_core::server::{Cap, ServerInfo, SqlKind};
use dnt_core::sql::ddl_edit::{
    build, check_target, editable_kind, isolate, notes, parse_header, strategies, template,
    BuildInput, CatalogFacts, EditTarget, PostCheck, Strategy,
};
use dnt_core::sql::{classify, Dialect};
use dnt_store::history::HistoryOrigin;
use serde::{Deserialize, Serialize};
use specta::Type;
use std::collections::HashMap;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::sync::{Arc, Mutex};
use std::time::Instant;
use uuid::Uuid;

/// Edits kept open at once.
pub(crate) const MAX_EDITS: usize = 64;

/// The History origin of DDL edit applies (ruling R1: no store migration in P4c; a
/// dedicated `ddl_edit` origin is a follow-up).
pub(crate) const HISTORY_ORIGIN: HistoryOrigin = HistoryOrigin::ObjectAction;

/// The fingerprint of an edit of an object that does not exist yet.
pub(crate) const ABSENT: &str = "absent";

const NOT_SHOWN: &str = "The server does not show this definition to this user.";
const READ_ONLY: &str = "This connection is read-only.";
const OBJECT_CHANGED: &str = "The object changed since the editor opened it.";

#[derive(Debug, Clone, Serialize, Deserialize, Type)]
pub struct DdlEditOpen {
    pub edit_id: Uuid,
    pub text: String,
    pub target: EditTarget,
    pub object: Option<ObjectRef>,
    pub server: ServerInfo,
    pub env: Option<EnvTag>,
    pub read_only: bool,
    pub strategies: Vec<Strategy>,
    pub strategy: Option<Strategy>,
    /// The edit is not possible (read-only, overloads, DDL not shown).
    pub blocked_reason: Option<String>,
    /// Engine/kind notes shown in the editor header.
    pub notes: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Type)]
pub struct DdlEditPreview {
    pub plan_id: Uuid,
    pub statements: Vec<String>,
    pub warnings: Vec<String>,
    /// One sentence per check run before COMMIT (PostgreSQL).
    pub checks: Vec<String>,
    pub transactional: bool,
    pub gap: bool,
    pub confirm_name: Option<String>,
    pub expires_in_seconds: u32,
}

/// One open edit.
#[derive(Clone)]
pub(crate) struct Edit {
    pub(crate) conn: Uuid,
    pub(crate) target: EditTarget,
    /// `None`: a new object.
    pub(crate) object: Option<ObjectRef>,
    /// The server's DDL at open (empty for a new object or a DDL not shown).
    pub(crate) base_text: String,
    pub(crate) server: ServerInfo,
    /// The dialect of the session the edit was opened on.
    pub(crate) dialect: Dialect,
    pub(crate) fingerprint: String,
    pub(crate) facts: CatalogFacts,
    /// Why the object cannot be edited (overloads, DDL not shown); read-only is
    /// checked against the connection at every preview instead.
    pub(crate) blocked: Option<String>,
}

/// A single-use plan of an edit: exactly what the apply sends and checks (the preview
/// holds the warnings, check notes and flags shown).
pub(crate) struct EditPlan {
    pub(crate) preview: DdlEditPreview,
    pub(crate) statements: Vec<String>,
    pub(crate) checks: Vec<PostCheck>,
    pub(crate) edit_id: Uuid,
    pub(crate) conn: Uuid,
    pub(crate) scope: Scope,
    /// The edit's target with the name (and a new trigger's table) resolved from the text.
    pub(crate) target: EditTarget,
    /// The plan creates the object.
    pub(crate) new: bool,
    pub(crate) kind: ObjectKind,
    pub(crate) rt: Arc<ConnectionRuntime>,
    pub(crate) generation: u64,
    pub(crate) env: Option<EnvTag>,
    pub(crate) dialect: Dialect,
    pub(crate) fingerprint: String,
    pub(crate) base_text: String,
    /// MySQL routine edits: the (unquoted) name of the rehearsal routine (R5).
    pub(crate) rehearsal: Option<String>,
    pub(crate) expires: Instant,
}

#[derive(Default)]
pub(crate) struct DdlEditStore {
    pub(crate) edits: Mutex<HashMap<Uuid, Edit>>,
    pub(crate) plans: Mutex<HashMap<Uuid, EditPlan>>,
    pub(crate) operations: DdlOperations,
}

impl DdlEditStore {
    pub(crate) fn get(&self, id: Uuid) -> Result<Edit, DbError> {
        lock(&self.edits).get(&id).cloned().ok_or_else(edit_closed)
    }
}

fn edit_closed() -> DbError {
    config_error(
        "edit_id",
        "This editor is no longer open; open the object again.",
    )
}

/// A metadata read that ran past [`METADATA_CALL_TIMEOUT`].
fn timed_out() -> DbError {
    DbError::timeout(dnt_core::error::ConnectStage::Execute)
}

pub(crate) fn object_changed() -> DbError {
    DbError::PlanStale {
        reason: OBJECT_CHANGED.into(),
    }
}

fn object_ref(t: &EditTarget, name: &str) -> ObjectRef {
    ObjectRef {
        database: t.database.clone(),
        schema: t.schema.clone(),
        name: name.to_string(),
        kind: t.kind,
    }
}

fn scope_of(t: &EditTarget) -> Scope {
    Scope {
        database: t.database.clone(),
        schema: t.schema.clone(),
    }
}

/// Hash of the object's DDL text and its catalog facts.
fn fingerprint_of(text: &str, facts: &CatalogFacts) -> String {
    let mut h = DefaultHasher::new();
    text.hash(&mut h);
    serde_json::to_string(facts)
        .unwrap_or_default()
        .hash(&mut h);
    format!("{:016x}", h.finish())
}

/// An existing object as the server has it now.
pub(crate) struct ObjectRead {
    pub(crate) text: String,
    pub(crate) facts: CatalogFacts,
    pub(crate) fingerprint: String,
    pub(crate) blocked: Option<String>,
}

/// Reads the DDL (as the DDL section shows it) and the catalog facts of the existing
/// object `t`. A DDL the server will not show is an empty text with a blocked reason;
/// any other failure is an error.
pub(crate) async fn read_object(
    session: &dyn SqlSession,
    t: &EditTarget,
    name: &str,
) -> Result<ObjectRead, DbError> {
    let obj = object_ref(t, name);
    let (text, facts, blocked) = match session.introspector().object_ddl(&obj).await {
        Ok(text) => {
            let (facts, blocked) = catalog::facts(session, t).await?;
            (text, facts, blocked)
        }
        Err(e) if ddl_not_shown(&e) => (
            String::new(),
            CatalogFacts::default(),
            Some(NOT_SHOWN.into()),
        ),
        Err(e) => return Err(e),
    };
    Ok(ObjectRead {
        fingerprint: fingerprint_of(&text, &facts),
        text,
        facts,
        blocked,
    })
}

/// The fingerprint of the edited object read on `session` now (`"absent"` for a new one).
/// An object that is no longer listed (dropped by another session) changed: the
/// server's "does not exist" error of the DDL read is [`object_changed`].
pub(crate) async fn current_fingerprint(
    session: &dyn SqlSession,
    t: &EditTarget,
    object: Option<&ObjectRef>,
) -> Result<String, DbError> {
    let Some(o) = object else {
        return Ok(ABSENT.into());
    };
    match read_object(session, t, &o.name).await {
        Ok(read) => Ok(read.fingerprint),
        Err(e) => match object_listed(session, t, &o.name).await {
            Ok(false) => Err(object_changed()),
            _ => Err(e),
        },
    }
}

/// The kinds whose names a new object of `kind` must not take.
fn namespace_kinds(kind: ObjectKind, engine: SqlKind) -> Vec<ObjectKind> {
    use ObjectKind::*;
    match (engine, kind) {
        (SqlKind::MySql, View) => vec![Table, View],
        (SqlKind::Postgres, View | MaterializedView) => vec![Table, View, MaterializedView],
        (SqlKind::Postgres, Function | Procedure) => vec![Function, Procedure],
        (SqlKind::MySql | SqlKind::Postgres, k) => vec![k],
    }
}

/// Whether a catalog name is `name`: MySQL / MariaDB compare ASCII case-insensitively
/// (routine and event names always are; table, view and trigger names are on servers
/// with `lower_case_table_names` set, e.g. Windows), PostgreSQL exactly.
pub(crate) fn same_catalog_name(engine: SqlKind, catalog: &str, name: &str) -> bool {
    match engine {
        SqlKind::MySql => catalog.eq_ignore_ascii_case(name),
        SqlKind::Postgres => catalog == name,
    }
}

/// Whether an object of `t.kind` named `name` is listed in `t`'s scope.
pub(crate) async fn object_listed(
    session: &dyn SqlSession,
    t: &EditTarget,
    name: &str,
) -> Result<bool, DbError> {
    let engine = session.sql_kind();
    Ok(session
        .introspector()
        .list_objects(&scope_of(t), t.kind)
        .await?
        .iter()
        .any(|o| same_catalog_name(engine, &o.name, name)))
}

/// Whether `name` is taken among the kinds sharing the namespace of `t.kind`.
pub(crate) async fn name_taken(
    session: &dyn SqlSession,
    t: &EditTarget,
    name: &str,
) -> Result<bool, DbError> {
    let scope = scope_of(t);
    let i = session.introspector();
    let engine = session.sql_kind();
    for k in namespace_kinds(t.kind, engine) {
        if i.list_objects(&scope, k)
            .await?
            .iter()
            .any(|o| same_catalog_name(engine, &o.name, name))
        {
            return Ok(true);
        }
    }
    Ok(false)
}

/// A new object's name must not be taken among the kinds sharing its namespace.
pub(crate) async fn check_name_free(
    session: &dyn SqlSession,
    t: &EditTarget,
    name: &str,
) -> Result<(), DbError> {
    if name_taken(session, t, name).await? {
        return Err(config_error(
            "text",
            &format!("An object named {name} already exists."),
        ));
    }
    Ok(())
}

/// Judges every statement with the query guard; any `Block` blocks the plan.
fn judge(
    statements: &[String],
    env: Option<EnvTag>,
    read_only: bool,
    dialect: Dialect,
) -> Result<(), DbError> {
    let blocked = statements
        .iter()
        .filter(|s| {
            dnt_query::verdict(s, classify(s, dialect), dialect, env, read_only)
                == dnt_query::Verdict::Block
        })
        .count();
    if blocked > 0 {
        Err(DbError::Blocked {
            count: blocked.min(u32::MAX as usize) as u32,
        })
    } else {
        Ok(())
    }
}

/// The rehearsal name of a MySQL routine edit (ruling R5).
fn rehearsal_name() -> String {
    format!(
        "_dnt_p4c_check_{}",
        &Uuid::new_v4().simple().to_string()[..8]
    )
}

/// The name a plan's confirmation must type: Production always, Staging when the plan
/// has a non-transactional drop gap (ruling R10).
fn confirm_name(env: Option<EnvTag>, gap: bool, name: &str) -> Option<String> {
    match env {
        Some(EnvTag::Production) => Some(name.to_string()),
        Some(EnvTag::Staging) if gap => Some(name.to_string()),
        _ => None,
    }
}

impl SessionManager {
    pub async fn ddl_edit_open(
        &self,
        conn: Uuid,
        scope: Scope,
        kind: ObjectKind,
        name: Option<String>,
        table: Option<String>,
    ) -> Result<DdlEditOpen, DbError> {
        let engine = self.require_cap(conn, Cap::DdlEdit)?;
        let cfg = self.repo.get(conn)?;
        match (engine, &scope.schema) {
            (SqlKind::MySql, Some(_)) => {
                return Err(config_error(
                    "scope",
                    "MySQL has no schemas inside a database.",
                ))
            }
            (SqlKind::Postgres, None) => {
                return Err(config_error("scope", "A schema is required on PostgreSQL."))
            }
            (SqlKind::MySql, None) | (SqlKind::Postgres, Some(_)) => {}
        }
        if scope.database.is_empty() {
            return Err(config_error("scope", "A database is required."));
        }
        if name.as_deref().is_some_and(str::is_empty) {
            return Err(config_error("name", "An object name is required."));
        }
        let rt = self.runtime(conn)?;
        if lock(&self.ddl_edits.edits).len() >= MAX_EDITS {
            return Err(DbError::Busy);
        }
        let (session, _) = rt.open_query_session(Some(scope.database.clone())).await?;
        let session = sql_or_close(session).await?;
        let sql_session = session.sql()?;
        let server = session.server_info().clone();
        let dialect = sql_session.dialect();
        let mut target = EditTarget {
            kind,
            database: scope.database.clone(),
            schema: scope.schema.clone(),
            name: name.clone(),
            table: table.filter(|_| kind == ObjectKind::Trigger),
        };
        let read = tokio::time::timeout(METADATA_CALL_TIMEOUT, async {
            editable_kind(kind, &server)?;
            match name.as_deref() {
                Some(n) => read_object(sql_session, &target, n).await.map(Some),
                None => Ok(None),
            }
        })
        .await;
        close_quietly(session).await;
        let read = read.map_err(|_| timed_out())??;
        let (text, fingerprint, facts, blocked) = match read {
            Some(state) => {
                if kind == ObjectKind::Trigger && state.facts.table.is_some() {
                    target.table = state.facts.table.clone();
                }
                (state.text, state.fingerprint, state.facts, state.blocked)
            }
            None => (
                template(&target, &server)?,
                ABSENT.to_string(),
                CatalogFacts::default(),
                None,
            ),
        };
        let object = name.as_deref().map(|n| object_ref(&target, n));
        let offered = strategies(&target, &server)?;
        let notes = notes(&target, &server)?;
        let edit_id = Uuid::new_v4();
        {
            let mut edits = lock(&self.ddl_edits.edits);
            if edits.len() >= MAX_EDITS {
                return Err(DbError::Busy);
            }
            edits.insert(
                edit_id,
                Edit {
                    conn,
                    target: target.clone(),
                    object: object.clone(),
                    base_text: if object.is_some() {
                        text.clone()
                    } else {
                        String::new()
                    },
                    server: server.clone(),
                    dialect,
                    fingerprint,
                    facts,
                    blocked: blocked.clone(),
                },
            );
        }
        Ok(DdlEditOpen {
            edit_id,
            text,
            object,
            env: cfg.env_tag,
            read_only: cfg.read_only,
            strategy: offered.first().copied(),
            strategies: offered,
            blocked_reason: blocked.or_else(|| cfg.read_only.then(|| READ_ONLY.to_string())),
            notes,
            target,
            server,
        })
    }

    /// Spec §5: validates `text`, re-reads the object (stale check), builds and judges
    /// the statements and stores a single-use plan.
    pub async fn ddl_edit_preview(
        &self,
        edit_id: Uuid,
        text: String,
        strategy: Option<Strategy>,
    ) -> Result<DdlEditPreview, DbError> {
        let edit = self.ddl_edits.get(edit_id)?;
        let conn = edit.conn;
        let cfg = self.repo.get(conn)?;
        if cfg.read_only {
            return Err(DbError::Blocked { count: 1 });
        }
        if let Some(reason) = &edit.blocked {
            return Err(config_error("text", reason));
        }
        let rt = self.runtime(conn)?;
        rt.queries.check_ddl_idle()?;
        let (session, generation) = rt
            .open_query_session(Some(edit.target.database.clone()))
            .await?;
        let session = sql_or_close(session).await?;
        let sql_session = session.sql()?;
        let dialect = sql_session.dialect();
        let checked = tokio::time::timeout(METADATA_CALL_TIMEOUT, async {
            if dialect != edit.dialect {
                return Err(stale());
            }
            let fp = current_fingerprint(sql_session, &edit.target, edit.object.as_ref()).await?;
            if fp != edit.fingerprint {
                return Err(object_changed());
            }
            let stmt = isolate(&text, edit.target.kind, dialect, &edit.server)?;
            let header = parse_header(&stmt, dialect)?;
            check_target(&header, &edit.target, &edit.server)?;
            if edit.object.is_none() {
                check_name_free(sql_session, &edit.target, &header.name).await?;
            }
            Ok((stmt, header))
        })
        .await;
        close_quietly(session).await;
        let (stmt, header) = checked.map_err(|_| timed_out())??;
        let new = edit.object.is_none();
        let strategy = if new {
            None
        } else {
            match strategy {
                Some(s) => Some(s),
                None => strategies(&edit.target, &edit.server)?.first().copied(),
            }
        };
        let rehearsal = rehearsal_name();
        let built = build(&BuildInput {
            target: &edit.target,
            stmt: &stmt,
            header: &header,
            strategy,
            server: &edit.server,
            facts: &edit.facts,
            rehearsal_name: &rehearsal,
        })?;
        judge(&built.statements, cfg.env_tag, cfg.read_only, dialect)?;
        let name = edit
            .target
            .name
            .clone()
            .unwrap_or_else(|| header.name.clone());
        let mut target = edit.target.clone();
        target.name = Some(name.clone());
        if new && target.kind == ObjectKind::Trigger {
            if let Some((_, table)) = &header.on_table {
                target.table = Some(table.clone());
            }
        }
        let preview = DdlEditPreview {
            plan_id: Uuid::new_v4(),
            statements: built.statements.clone(),
            warnings: built.warnings.clone(),
            checks: built.check_notes.clone(),
            transactional: built.transactional,
            gap: built.gap,
            confirm_name: confirm_name(cfg.env_tag, built.gap, &name),
            expires_in_seconds: self.plan_ttl.as_secs().min(u32::MAX as u64) as u32,
        };
        let engine = edit.server.kind.require_sql()?;
        let rehearsed = match engine {
            SqlKind::MySql => {
                built.gap && matches!(target.kind, ObjectKind::Function | ObjectKind::Procedure)
            }
            SqlKind::Postgres => false,
        };
        let rehearsal = rehearsed.then_some(rehearsal);
        // The edit may have been closed (or changed by an apply) while this preview ran:
        // its plan is stored only while the edit is still the one previewed. The edits
        // lock is held across the insert, so a concurrent close drops this plan too.
        let edits = lock(&self.ddl_edits.edits);
        match edits.get(&edit_id) {
            None => return Err(edit_closed()),
            Some(e) if e.fingerprint != edit.fingerprint || e.object != edit.object => {
                return Err(object_changed())
            }
            Some(_) => {}
        }
        let mut plans = lock(&self.ddl_edits.plans);
        plans.retain(|_, p| p.expires > Instant::now());
        if plans.len() >= MAX_PLANS {
            return Err(DbError::Busy);
        }
        plans.insert(
            preview.plan_id,
            EditPlan {
                preview: preview.clone(),
                statements: built.statements,
                checks: built.checks,
                edit_id,
                conn,
                scope: scope_of(&target),
                kind: target.kind,
                target,
                new,
                rt,
                generation,
                env: cfg.env_tag,
                dialect,
                fingerprint: edit.fingerprint.clone(),
                base_text: edit.base_text.clone(),
                rehearsal,
                expires: Instant::now() + self.plan_ttl,
            },
        );
        Ok(preview)
    }

    /// Forgets the edit and its plans.
    pub async fn ddl_edit_close(&self, edit_id: Uuid) -> Result<(), DbError> {
        lock(&self.ddl_edits.edits).remove(&edit_id);
        lock(&self.ddl_edits.plans).retain(|_, p| p.edit_id != edit_id);
        Ok(())
    }

    pub async fn ddl_edit_cancel(&self, operation_id: Uuid) -> Result<(), DbError> {
        self.ddl_edits.operations.cancel(operation_id).await
    }

    /// Closing `conn`: its edits and plans are dropped, its applies cancelled.
    pub(crate) async fn ddl_edit_cancel_connection(&self, conn: Uuid) {
        lock(&self.ddl_edits.edits).retain(|_, e| e.conn != conn);
        lock(&self.ddl_edits.plans).retain(|_, p| p.conn != conn);
        self.ddl_edits.operations.cancel_connection(conn).await;
    }
}
