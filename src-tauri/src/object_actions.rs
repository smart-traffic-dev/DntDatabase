//! Single-use DDL plans and dedicated, cancellable object-action sessions.
pub use crate::ddl_exec::ActionOutcome;
use crate::ddl_exec::{
    execute, failure_error, failure_outcome, mysql_statement_refused, stopped, write_history,
    DdlHistory, DdlOperations, HISTORY_FAILED,
};
use crate::session_manager::{
    close_quietly, config_error, lock, ConnectionRuntime, SessionManager, METADATA_CALL_TIMEOUT,
};
use dnt_core::config::EnvTag;
use dnt_core::driver::Session;
use dnt_core::error::{AppError, DbError};
use dnt_core::meta::{ObjectKind, ObjectRef, Scope};
use dnt_core::server::{ServerVariant, SqlKind};
use dnt_core::sql::ddl::{action_sql, ObjectAction};
use dnt_core::sql::Dialect;
use dnt_store::history::HistoryOrigin;
use serde::{Deserialize, Serialize};
use specta::Type;
use std::collections::HashMap;
use std::sync::{atomic::Ordering, Arc, Mutex};
use std::time::Instant;
use uuid::Uuid;

pub(crate) const MAX_PLANS: usize = 128;

#[derive(Debug, Clone, Serialize, Deserialize, Type)]
pub struct ActionPreview {
    pub plan_id: Uuid,
    pub connection_id: Uuid,
    pub action: ObjectAction,
    pub sql: String,
    pub confirm_name: Option<String>,
    pub warnings: Vec<String>,
    pub expires_in_seconds: u32,
}
#[derive(Debug, Clone, Serialize, Deserialize, Type)]
pub struct ActionReport {
    pub outcome: ActionOutcome,
    pub error: Option<AppError>,
    pub warnings: Vec<String>,
}
struct StoredPlan {
    preview: ActionPreview,
    rt: Arc<ConnectionRuntime>,
    generation: u64,
    env: Option<EnvTag>,
    dialect: Dialect,
    fingerprint: String,
    expires: Instant,
}
#[derive(Default)]
pub(crate) struct ActionStore {
    plans: Mutex<HashMap<Uuid, StoredPlan>>,
    pub(crate) operations: DdlOperations,
}
pub(crate) fn stale() -> DbError {
    DbError::PlanStale {
        reason: "The connection, policy, dialect or object changed; preview again.".into(),
    }
}

/// `AppError.field` of an object that is no longer on the server (the UI then refreshes
/// the tree and closes the object's tabs).
pub const OBJECT_MISSING_FIELD: &str = "object_missing";

fn object_missing(obj: &ObjectRef) -> DbError {
    let what = match obj.kind {
        ObjectKind::Table => "Table",
        ObjectKind::View => "View",
        ObjectKind::MaterializedView => "Materialized view",
        _ => "Object",
    };
    DbError::Config {
        field: Some(OBJECT_MISSING_FIELD.into()),
        message: format!(
            "{what} {} no longer exists on the server; it may have been dropped or renamed elsewhere. Nothing was changed.",
            obj.name
        ),
    }
}

/// Metadata is read on this operation's session, never from tree caches.
async fn fingerprint(session: &dyn Session, action: &ObjectAction) -> Result<String, DbError> {
    let i = session.introspector();
    let kind = session.server_info().kind.as_sql(); // R1: boundary
    if let Some(obj) = action.object() {
        let scope = Scope {
            database: obj.database.clone(),
            schema: obj.schema.clone(),
        };
        let objects = i.list_objects(&scope, obj.kind).await?;
        if !objects.iter().any(|o| o.name == obj.name) {
            return Err(object_missing(obj));
        }
        if let ObjectAction::RenameTable { name, .. } = action {
            for k in table_namespace_kinds(kind) {
                if i.list_objects(&scope, k)
                    .await?
                    .iter()
                    .any(|o| &o.name == name)
                {
                    return Err(stale());
                }
            }
        }
        return i.object_ddl(obj).await;
    }
    let names: Vec<String> = match kind {
        SqlKind::MySql => i
            .list_databases()
            .await?
            .into_iter()
            .map(|d| d.name)
            .collect(),
        SqlKind::Postgres => i
            .list_schemas(action.database())
            .await?
            .into_iter()
            .map(|s| s.name)
            .collect(),
    };
    let exists = names.iter().any(|n| n == action.name());
    let creating = matches!(action, ObjectAction::CreateNamespace { .. });
    if exists && creating {
        let what = namespace_word(kind);
        return Err(DbError::Config {
            field: Some("name".into()),
            message: format!("A {what} named {} already exists.", action.name()),
        });
    }
    if exists == creating {
        return Err(stale());
    }
    if !exists {
        return Ok("absent".into());
    }
    let scope = namespace_scope(kind, action);
    let mut entries = Vec::new();
    for k in namespace_object_kinds(kind) {
        for o in i.list_objects(&scope, k).await? {
            entries.push(format!("{k:?}:{}", o.name));
        }
    }
    entries.sort();
    Ok(entries.join("\0"))
}

/// The kinds whose names a renamed table must not take: tables share a namespace with
/// views (and materialized views on PostgreSQL).
fn table_namespace_kinds(kind: SqlKind) -> Vec<ObjectKind> {
    match kind {
        SqlKind::MySql => vec![ObjectKind::Table, ObjectKind::View],
        SqlKind::Postgres => vec![
            ObjectKind::Table,
            ObjectKind::View,
            ObjectKind::MaterializedView,
        ],
    }
}

/// What a namespace is called on `kind`.
fn namespace_word(kind: SqlKind) -> &'static str {
    match kind {
        SqlKind::MySql => "database",
        SqlKind::Postgres => "schema",
    }
}

/// The scope of the namespace `action` names (a MySQL database, a PostgreSQL schema).
fn namespace_scope(kind: SqlKind, action: &ObjectAction) -> Scope {
    match kind {
        SqlKind::MySql => Scope {
            database: action.name().into(),
            schema: None,
        },
        SqlKind::Postgres => Scope {
            database: action.database().into(),
            schema: Some(action.name().into()),
        },
    }
}

/// The object kinds a namespace's fingerprint lists.
fn namespace_object_kinds(kind: SqlKind) -> Vec<ObjectKind> {
    match kind {
        SqlKind::MySql => vec![
            ObjectKind::Table,
            ObjectKind::View,
            ObjectKind::Function,
            ObjectKind::Procedure,
            ObjectKind::Trigger,
            ObjectKind::Event,
        ],
        SqlKind::Postgres => vec![
            ObjectKind::Table,
            ObjectKind::View,
            ObjectKind::MaterializedView,
            ObjectKind::Function,
            ObjectKind::Procedure,
            ObjectKind::Sequence,
            ObjectKind::Type,
        ],
    }
}

/// The database an object-action session opens on: none on MySQL (statements name
/// their database), the action's database on PostgreSQL.
fn action_session_db(kind: SqlKind, action: &ObjectAction) -> Option<String> {
    match kind {
        SqlKind::MySql => None,
        SqlKind::Postgres => Some(action.database().to_string()),
    }
}

/// The database the "Create database/schema" options are read on: a non-empty
/// `database` on PostgreSQL, none on MySQL.
fn namespace_options_db(kind: SqlKind, database: String) -> Option<String> {
    match kind {
        SqlKind::MySql => None,
        SqlKind::Postgres => (!database.is_empty()).then_some(database),
    }
}

/// The warnings an object-action preview shows.
fn action_warnings(kind: SqlKind, action: &ObjectAction) -> Vec<String> {
    let mut warnings = vec!["Each metadata lock wait is limited to 5 seconds.".into()];
    match kind {
        SqlKind::MySql => warnings.push(
            "MySQL/MariaDB DDL may commit implicitly and cannot be undone by this app.".into(),
        ),
        SqlKind::Postgres => {}
    }
    if action.destructive() {
        warnings.push(
            "This operation removes data or an object. No CASCADE or foreign-key bypass is added."
                .into(),
        );
    }
    if matches!(action, ObjectAction::TruncateTable { .. }) {
        warnings.push(match kind { SqlKind::MySql => "TRUNCATE may reset AUTO_INCREMENT.", SqlKind::Postgres => "TRUNCATE preserves identity values, includes descendant tables and fires ON TRUNCATE triggers." }.into());
    }
    warnings
}

/// What a "Create database/schema" form can offer: MySQL character sets and collations,
/// PostgreSQL roles (each list sorted; empty on the other engine).
#[derive(Debug, Clone, Default, Serialize, Deserialize, Type)]
pub struct NamespaceOptions {
    pub charsets: Vec<CharsetOption>,
    pub collations: Vec<CollationOption>,
    pub default_charset: Option<String>,
    pub default_collation: Option<String>,
    pub roles: Vec<String>,
    pub current_role: Option<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize, Type)]
pub struct CharsetOption {
    pub name: String,
    pub default_collation: String,
}
#[derive(Debug, Clone, Serialize, Deserialize, Type)]
pub struct CollationOption {
    pub name: String,
    pub charset: String,
}

const MYSQL_CHARSETS_SQL: &str = "SELECT CHARACTER_SET_NAME, DEFAULT_COLLATE_NAME FROM information_schema.CHARACTER_SETS ORDER BY CHARACTER_SET_NAME";
const MYSQL_COLLATIONS_SQL: &str = "SELECT COLLATION_NAME, CHARACTER_SET_NAME FROM information_schema.COLLATIONS WHERE CHARACTER_SET_NAME IS NOT NULL ORDER BY COLLATION_NAME";
/// MariaDB ≥ 10.10 lists its UCA 14.0 collations in `COLLATIONS` once, without a character
/// set (`uca1400_ai_ci`); their usable names (`utf8mb4_uca1400_ai_ci`, the default of
/// utf8mb4 on 11.x) are only in the applicability table.
const MARIADB_COLLATIONS_SQL: &str = "SELECT FULL_COLLATION_NAME, CHARACTER_SET_NAME FROM information_schema.COLLATION_CHARACTER_SET_APPLICABILITY ORDER BY FULL_COLLATION_NAME";
const MYSQL_DEFAULTS_SQL: &str = "SELECT @@character_set_server, @@collation_server";
const PG_ROLES_SQL: &str =
    "SELECT rolname::text FROM pg_roles WHERE rolname !~ '^pg_' ORDER BY rolname";
const PG_CURRENT_ROLE_SQL: &str = "SELECT current_user::text";

async fn namespace_options(session: &dyn Session) -> Result<NamespaceOptions, DbError> {
    use crate::ddl_edit::catalog::rows_with;
    use dnt_core::driver::ExecOptions;
    let rows = |sql| rows_with(session, sql, ExecOptions::default());
    let pair = |r: &Vec<Option<String>>| Some((r.first()?.clone()?, r.get(1)?.clone()?));
    let mut out = NamespaceOptions::default();
    let kind = session.server_info().kind.as_sql(); // R1: boundary
    match kind {
        SqlKind::MySql => {
            out.charsets = rows(MYSQL_CHARSETS_SQL)
                .await?
                .iter()
                .filter_map(pair)
                .map(|(name, default_collation)| CharsetOption {
                    name,
                    default_collation,
                })
                .collect();
            let info = session.server_info();
            let full_names = info.variant == ServerVariant::MariaDb && info.version >= (10, 10, 0);
            out.collations = rows(if full_names {
                MARIADB_COLLATIONS_SQL
            } else {
                MYSQL_COLLATIONS_SQL
            })
            .await?
            .iter()
            .filter_map(pair)
            .map(|(name, charset)| CollationOption { name, charset })
            .collect();
            if let Some(r) = rows(MYSQL_DEFAULTS_SQL).await?.first() {
                out.default_charset = r.first().cloned().flatten();
                out.default_collation = r.get(1).cloned().flatten();
            }
        }
        SqlKind::Postgres => {
            out.roles = rows(PG_ROLES_SQL)
                .await?
                .into_iter()
                .filter_map(|r| r.into_iter().next().flatten())
                .collect();
            out.current_role = rows(PG_CURRENT_ROLE_SQL)
                .await?
                .into_iter()
                .next()
                .and_then(|r| r.into_iter().next().flatten());
        }
    }
    Ok(out)
}

impl SessionManager {
    /// The SQL `action` would run on this connection, without a session or a plan (live preview).
    pub fn object_action_render(
        &self,
        conn: Uuid,
        action: &ObjectAction,
    ) -> Result<String, DbError> {
        action_sql(self.repo.get(conn)?.kind.as_sql(), action) // R1: boundary
    }

    /// Choices for the "Create database/schema" form, read on a dedicated session.
    pub async fn namespace_options(
        &self,
        conn: Uuid,
        database: String,
    ) -> Result<NamespaceOptions, DbError> {
        let cfg = self.repo.get(conn)?;
        let rt = self.runtime(conn)?;
        let db = namespace_options_db(cfg.kind.as_sql(), database); // R1: boundary
        let (session, _) = rt.open_query_session(db).await?;
        let out =
            tokio::time::timeout(METADATA_CALL_TIMEOUT, namespace_options(session.as_ref())).await;
        close_quietly(session).await;
        out.map_err(|_| config_error("object", "Metadata timed out."))?
    }

    pub async fn object_action_preview(
        &self,
        conn: Uuid,
        action: ObjectAction,
    ) -> Result<ActionPreview, DbError> {
        let cfg = self.repo.get(conn)?;
        if cfg.read_only {
            return Err(DbError::Blocked { count: 1 });
        }
        let kind = cfg.kind.as_sql(); // R1: boundary
        let sql = action_sql(kind, &action)?;
        let rt = self.runtime(conn)?;
        rt.queries.check_ddl_idle()?;
        let db = action_session_db(kind, &action);
        let (session, generation) = rt.open_query_session(db).await?;
        let dialect = session.dialect();
        let fp = tokio::time::timeout(
            METADATA_CALL_TIMEOUT,
            fingerprint(session.as_ref(), &action),
        )
        .await;
        close_quietly(session).await;
        let fp =
            fp.map_err(|_| config_error("object", "Metadata timed out; nothing was changed."))??;
        let warnings = action_warnings(kind, &action);
        let preview = ActionPreview {
            plan_id: Uuid::new_v4(),
            connection_id: conn,
            confirm_name: (action.destructive() || cfg.env_tag == Some(EnvTag::Production))
                .then(|| action.name().into()),
            action,
            sql,
            warnings,
            expires_in_seconds: self.plan_ttl.as_secs().min(u32::MAX as u64) as u32,
        };
        let mut plans = lock(&self.object_actions.plans);
        plans.retain(|_, p| p.expires > Instant::now());
        if plans.len() >= MAX_PLANS {
            return Err(DbError::Busy);
        }
        plans.insert(
            preview.plan_id,
            StoredPlan {
                preview: preview.clone(),
                rt,
                generation,
                env: cfg.env_tag,
                dialect,
                fingerprint: fp,
                expires: Instant::now() + self.plan_ttl,
            },
        );
        Ok(preview)
    }

    pub async fn object_action_cancel(&self, id: Uuid) -> Result<(), DbError> {
        self.object_actions.operations.cancel(id).await
    }

    pub(crate) async fn object_cancel_connection(&self, conn: Uuid) {
        lock(&self.object_actions.plans).retain(|_, p| p.preview.connection_id != conn);
        self.object_actions.operations.cancel_connection(conn).await;
    }

    pub async fn object_action_apply(
        &self,
        plan: Uuid,
        confirmed: bool,
        name: Option<String>,
        operation: Uuid,
        progress: &(dyn Fn(&str) + Send + Sync),
    ) -> Result<ActionReport, DbError> {
        let stored = lock(&self.object_actions.plans)
            .remove(&plan)
            .ok_or(DbError::PlanExpired)?;
        if stored.expires <= Instant::now() {
            return Err(DbError::PlanExpired);
        }
        let conn = stored.preview.connection_id;
        let (_guard, stop) = self.object_actions.operations.register(operation, conn)?;
        self.check_action_context(&stored)?;
        if !confirmed
            || stored
                .preview
                .confirm_name
                .as_ref()
                .is_some_and(|required| Some(required) != name.as_ref())
        {
            return Err(DbError::ConfirmationRequired { count: 1 });
        }
        if stop.is_stopped() {
            return Err(stopped());
        }
        progress("checking");
        let kind = stored.rt.cfg.kind.as_sql(); // R1: boundary
        let db = action_session_db(kind, &stored.preview.action);
        let (session, generation) = stored.rt.open_query_session(db).await?;
        self.object_actions
            .operations
            .set_cancel(operation, Some(session.cancel_handle()));
        let checked = async {
            if generation != stored.generation || session.dialect() != stored.dialect {
                return Err(stale());
            }
            let before = session.reconnects();
            session.ping().await?;
            if fingerprint(session.as_ref(), &stored.preview.action).await? != stored.fingerprint {
                return Err(stale());
            }
            self.check_action_context(&stored)?;
            if before != session.reconnects() || session.dialect() != stored.dialect {
                return Err(stale());
            }
            if stop.is_stopped() {
                return Err(stopped());
            }
            Ok(())
        };
        let checked = tokio::time::timeout(METADATA_CALL_TIMEOUT, checked)
            .await
            .unwrap_or_else(|_| {
                Err(config_error(
                    "object",
                    "Metadata timed out; nothing was changed.",
                ))
            });
        if let Err(e) = checked {
            close_quietly(session).await;
            return Err(e);
        }
        let started = Instant::now();
        let started_at = crate::run_events::utc_millis_now();
        let mut sent = false;
        let mut commit_sent = false;
        let pg = match kind {
            SqlKind::MySql => false,
            SqlKind::Postgres => true,
        };
        let result = async {
            execute(
                session.as_ref(),
                if pg {
                    "SET lock_timeout = '5s'"
                } else {
                    "SET SESSION lock_wait_timeout = 5"
                },
                stop.clone(),
                self.cancel_grace,
                stored.rt.cfg.options.query_timeout_s,
            )
            .await?;
            if pg {
                execute(
                    session.as_ref(),
                    "BEGIN",
                    stop.clone(),
                    self.cancel_grace,
                    stored.rt.cfg.options.query_timeout_s,
                )
                .await?;
            }
            self.check_action_context(&stored)?;
            if session.dialect() != stored.dialect || session.reconnects() != 0 {
                return Err(stale());
            }
            if stop.is_stopped() {
                return Err(stopped());
            }
            progress("executing");
            sent = true;
            execute(
                session.as_ref(),
                &stored.preview.sql,
                stop.clone(),
                self.cancel_grace,
                stored.rt.cfg.options.query_timeout_s,
            )
            .await?;
            if session.reconnects() != 0 {
                return Err(stale());
            }
            if pg {
                if stop.is_stopped() {
                    return Err(stopped());
                }
                progress("committing");
                commit_sent = true;
                execute(
                    session.as_ref(),
                    "COMMIT",
                    stop.clone(),
                    self.cancel_grace,
                    stored.rt.cfg.options.query_timeout_s,
                )
                .await?;
            }
            Ok(())
        }
        .await;
        let mut report = ActionReport {
            outcome: ActionOutcome::Applied,
            error: None,
            warnings: Vec::new(),
        };
        if let Err(e) = result {
            // A MySQL CREATE DATABASE the server refuses (a name that exists, a collation
            // of another charset) created nothing. Other MySQL actions stay as they were:
            // DROP DATABASE without atomic DDL drops table by table.
            let create = matches!(stored.preview.action, ObjectAction::CreateNamespace { .. });
            report.outcome = if create && mysql_statement_refused(pg, &e) && !stop.is_stopped() {
                ActionOutcome::NotApplied
            } else {
                failure_outcome(
                    session.as_ref(),
                    &e,
                    pg,
                    sent,
                    commit_sent,
                    self.cancel_grace,
                )
                .await
            };
            report.error = Some(failure_error(e, report.outcome));
        }
        // No cancellation can reach a later statement or a pooled session: this session is closed.
        self.object_actions.operations.set_cancel(operation, None);
        close_quietly(session).await;
        if sent {
            let saved = write_history(
                self.repo.clone(),
                DdlHistory {
                    connection_id: conn,
                    database: Some(stored.preview.action.database().into()),
                    sql: &stored.preview.sql,
                    dialect: stored.dialect,
                    started_at,
                    elapsed: started.elapsed(),
                    outcome: report.outcome,
                    error: report.error.as_ref(),
                    origin: HistoryOrigin::ObjectAction,
                },
            )
            .await;
            if !saved {
                report.warnings.push(HISTORY_FAILED.into());
            }
        }
        if sent {
            self.after_ddl(conn, stored.preview.action.database());
        }
        progress("finished");
        Ok(report)
    }

    fn check_action_context(&self, stored: &StoredPlan) -> Result<(), DbError> {
        let conn = stored.preview.connection_id;
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
        let mut judged =
            dnt_query::plan(&stored.preview.sql, dnt_query::Target::All, stored.dialect);
        dnt_query::guard_with(
            &mut judged,
            dnt_query::GuardCtx {
                env: cfg.env_tag,
                read_only: cfg.read_only,
                analyze: false,
                confirm_unbounded: true,
            },
        );
        if judged.stmts.len() != 1
            || judged
                .stmts
                .iter()
                .any(|s| s.verdict == dnt_query::Verdict::Block)
        {
            return Err(DbError::Blocked { count: 1 });
        }
        rt.queries.check_ddl_idle()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ddl_exec::Operation;
    use dnt_core::driver::StopSignal;
    #[test]
    fn operation_ids_are_exclusive_and_raii_releases_them() {
        let store = ActionStore::default();
        let id = Uuid::new_v4();
        let conn = Uuid::new_v4();
        let (guard, stop) = store.operations.register(id, conn).unwrap();
        assert!(!stop.is_stopped());
        assert!(matches!(
            store.operations.register(id, conn),
            Err(DbError::Config { .. })
        ));
        drop(guard);
        assert!(lock(&store.operations.ops).is_empty());
    }
    #[test]
    fn precancel_is_not_overwritten_by_registration() {
        let store = ActionStore::default();
        let id = Uuid::new_v4();
        let signal = StopSignal::new();
        signal.stop();
        lock(&store.operations.ops).insert(
            id,
            Operation {
                connection: None,
                stop: signal,
                cancel: None,
                since: Instant::now(),
            },
        );
        let (_guard, signal) = store.operations.register(id, Uuid::new_v4()).unwrap();
        assert!(signal.is_stopped());
    }

    fn drop_namespace() -> ObjectAction {
        ObjectAction::DropNamespace {
            database: "shop".into(),
            name: "sales".into(),
        }
    }

    fn truncate() -> ObjectAction {
        ObjectAction::TruncateTable {
            object: ObjectRef {
                database: "shop".into(),
                schema: None,
                name: "orders".into(),
                kind: ObjectKind::Table,
            },
        }
    }

    #[test]
    fn namespaces_differ_per_engine() {
        assert_eq!(namespace_word(SqlKind::MySql), "database");
        assert_eq!(namespace_word(SqlKind::Postgres), "schema");
        let action = drop_namespace();
        let s = namespace_scope(SqlKind::MySql, &action);
        assert_eq!((s.database.as_str(), s.schema), ("sales", None));
        let s = namespace_scope(SqlKind::Postgres, &action);
        assert_eq!(
            (s.database.as_str(), s.schema.as_deref()),
            ("shop", Some("sales"))
        );
        assert_eq!(action_session_db(SqlKind::MySql, &action), None);
        assert_eq!(
            action_session_db(SqlKind::Postgres, &action).as_deref(),
            Some("shop")
        );
        assert_eq!(namespace_options_db(SqlKind::MySql, "shop".into()), None);
        assert_eq!(
            namespace_options_db(SqlKind::Postgres, "shop".into()).as_deref(),
            Some("shop")
        );
        assert_eq!(namespace_options_db(SqlKind::Postgres, String::new()), None);
    }

    #[test]
    fn fingerprinted_kinds_differ_per_engine() {
        use ObjectKind::*;
        assert_eq!(table_namespace_kinds(SqlKind::MySql), [Table, View]);
        assert_eq!(
            table_namespace_kinds(SqlKind::Postgres),
            [Table, View, MaterializedView]
        );
        assert_eq!(
            namespace_object_kinds(SqlKind::MySql),
            [Table, View, Function, Procedure, Trigger, Event]
        );
        assert_eq!(
            namespace_object_kinds(SqlKind::Postgres),
            [
                Table,
                View,
                MaterializedView,
                Function,
                Procedure,
                Sequence,
                Type
            ]
        );
    }

    #[test]
    fn action_warnings_differ_per_engine() {
        assert_eq!(
            action_warnings(SqlKind::MySql, &truncate()),
            [
                "Each metadata lock wait is limited to 5 seconds.",
                "MySQL/MariaDB DDL may commit implicitly and cannot be undone by this app.",
                "This operation removes data or an object. No CASCADE or foreign-key bypass is added.",
                "TRUNCATE may reset AUTO_INCREMENT.",
            ]
        );
        assert_eq!(
            action_warnings(SqlKind::Postgres, &truncate()),
            [
                "Each metadata lock wait is limited to 5 seconds.",
                "This operation removes data or an object. No CASCADE or foreign-key bypass is added.",
                "TRUNCATE preserves identity values, includes descendant tables and fires ON TRUNCATE triggers.",
            ]
        );
        let create = ObjectAction::CreateNamespace {
            database: "shop".into(),
            name: "sales".into(),
            charset: None,
            collation: None,
            owner: None,
        };
        assert_eq!(
            action_warnings(SqlKind::Postgres, &create),
            ["Each metadata lock wait is limited to 5 seconds."]
        );
    }
}
