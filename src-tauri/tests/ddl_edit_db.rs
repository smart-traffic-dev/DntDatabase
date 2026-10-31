//! DDL edits (P4c) end to end on the Docker matrix (`DNT_TEST_DB=1`): every test goes
//! through `SessionManager::ddl_edit_*` exactly as the UI does, on the six servers of
//! `Target::all()`.
//!
//! Each target works in its own scratch namespace `p4c_<uuid>` (a database on MySQL /
//! MariaDB, a schema of `dnt_fixture` on PostgreSQL), dropped afterwards also when an
//! assertion fails (the panic is re-raised after the cleanup); the cleanup then checks
//! that the namespace is gone.

use dnt_core::config::{ConnOptions, ConnectionConfig, SecretMode};
use dnt_core::driver::{Driver, ExecOptions, ResultSummary, RowSink, Session};
use dnt_core::error::DbError;
use dnt_core::meta::{ObjectKind, ObjectRef, Scope};
use dnt_core::secret::MemorySecretStore;
use dnt_core::server::DbKind;
use dnt_core::sql::classify;
use dnt_core::sql::ddl_edit::Strategy;
use dnt_core::sql::ident::quote_ident;
use dnt_core::value::{ColumnMeta, Value};
use dnt_driver_mysql::MySqlDriver;
use dnt_driver_pg::PgDriver;
use dnt_testkit::{require_db, Target, FIXTURE_DB, TEST_PASSWORD};
use dntdatabase::commands::connections::{
    create_connection_impl, open_connection_impl, SecretsInput,
};
use dntdatabase::ddl_edit::{DdlEditOpen, DdlEditPreview, DdlEditReport, EditOutcome, ObjectState};
use dntdatabase::state::{AppPaths, AppState};
use futures_util::FutureExt;
use serde_json::json;
use std::any::Any;
use std::panic::AssertUnwindSafe;
use std::sync::Arc;
use uuid::Uuid;

const OBJECT_CHANGED: &str = "The object changed since the editor opened it.";
const REPLACE_SIGNATURE_FAILURE: &str = "The argument list changed, so CREATE OR REPLACE would add a second routine instead of replacing it. Use Drop and re-create to change the signature.";

fn app_state(dir: &std::path::Path) -> AppState {
    AppState::open(
        AppPaths::in_dir(dir.join("data")),
        Arc::new(MemorySecretStore::default()),
    )
    .unwrap()
}

async fn open(state: &AppState, t: Target) -> Uuid {
    let cfg = ConnectionConfig {
        id: Uuid::nil(),
        group_id: None,
        name: format!("{t:?}"),
        sort_order: 0,
        kind: t.kind(),
        host: "127.0.0.1".into(),
        port: t.host_port(),
        username: t.admin_user().into(),
        default_db: Some(FIXTURE_DB.into()),
        env_tag: None,
        read_only: false,
        secret_mode: SecretMode::Saved,
        ssh: None,
        ssl: None,
        options: ConnOptions::default(),
    };
    let secrets: SecretsInput =
        serde_json::from_value(json!({ "db_password": TEST_PASSWORD })).unwrap();
    let id = create_connection_impl(state, cfg, secrets)
        .await
        .unwrap()
        .id;
    open_connection_impl(state, id, None).await.unwrap();
    id
}

#[derive(Debug, Default)]
struct Collect(Vec<Vec<Value>>);
impl RowSink for Collect {
    fn result_start(&mut self, _: &[ColumnMeta]) {}
    fn rows(&mut self, rows: Vec<Vec<Value>>) {
        self.0.extend(rows);
    }
    fn result_end(&mut self, _: &ResultSummary) {}
}
async fn sql(s: &dyn Session, text: &str) -> Result<Collect, DbError> {
    let mut sink = Collect::default();
    s.execute_one(
        text,
        classify(text, s.dialect()),
        &ExecOptions::default(),
        &mut sink,
    )
    .await?;
    Ok(sink)
}

async fn connect(t: Target, db: &str) -> Box<dyn Session> {
    let driver: Box<dyn Driver> = if t.kind() == DbKind::MySql {
        Box::new(MySqlDriver)
    } else {
        Box::new(PgDriver)
    };
    driver
        .connect(&t.resolved(Some(db)))
        .await
        .unwrap_or_else(|e| panic!("{t:?}: {e}"))
}

fn value_text(v: &Value) -> String {
    match v {
        Value::Text(s) | Value::Decimal(s) => s.clone(),
        Value::Int(n) => n.to_string(),
        Value::UInt(n) => n.to_string(),
        Value::Null => "NULL".into(),
        Value::Bytes { preview, .. } => String::from_utf8_lossy(preview).into_owned(),
        other => format!("{other:?}"),
    }
}

/// One target: an app state with an open connection, a direct driver session (on the
/// scratch database for MySQL / MariaDB, so its DDL is written unqualified) and the
/// scratch namespace.
struct Scratch {
    t: Target,
    pg: bool,
    _dir: tempfile::TempDir,
    state: AppState,
    id: Uuid,
    s: Box<dyn Session>,
    ns: String,
}

impl Scratch {
    async fn new(t: Target) -> Scratch {
        let dir = tempfile::tempdir().unwrap();
        let state = app_state(dir.path());
        let id = open(&state, t).await;
        let pg = t.kind() == DbKind::Postgres;
        let ns = format!("p4c_{}", Uuid::new_v4().simple());
        let create = if pg {
            format!("CREATE SCHEMA {}", quote_ident(t.kind().as_sql(), &ns))
        } else {
            format!(
                "CREATE DATABASE {} CHARACTER SET utf8mb4 COLLATE utf8mb4_unicode_ci",
                quote_ident(t.kind().as_sql(), &ns)
            )
        };
        let boot = connect(t, FIXTURE_DB).await;
        sql(boot.as_ref(), &create)
            .await
            .unwrap_or_else(|e| panic!("{t:?}: {create}: {e}"));
        let s = if pg {
            boot
        } else {
            boot.close().await;
            connect(t, &ns).await
        };
        Scratch {
            t,
            pg,
            _dir: dir,
            state,
            id,
            s,
            ns,
        }
    }

    fn db(&self) -> &str {
        if self.pg {
            FIXTURE_DB
        } else {
            &self.ns
        }
    }

    fn scope(&self) -> Scope {
        Scope {
            database: self.db().into(),
            schema: self.pg.then(|| self.ns.clone()),
        }
    }

    /// `name` as written in direct SQL: schema-qualified on PostgreSQL, bare on MySQL
    /// (the direct session is on the scratch database).
    fn q(&self, name: &str) -> String {
        if self.pg {
            format!("{}.{name}", self.ns)
        } else {
            name.to_string()
        }
    }

    async fn exec(&self, text: &str) {
        sql(self.s.as_ref(), text)
            .await
            .unwrap_or_else(|e| panic!("{:?}: {text}: {e}", self.t));
    }

    async fn rows(&self, text: &str) -> Vec<Vec<String>> {
        sql(self.s.as_ref(), text)
            .await
            .unwrap_or_else(|e| panic!("{:?}: {text}: {e}", self.t))
            .0
            .iter()
            .map(|r| r.iter().map(value_text).collect())
            .collect()
    }

    async fn one(&self, text: &str) -> String {
        let rows = self.rows(text).await;
        rows.first()
            .and_then(|r| r.first())
            .cloned()
            .unwrap_or_else(|| panic!("{:?}: {text}: no row", self.t))
    }

    fn obj(&self, kind: ObjectKind, name: &str) -> ObjectRef {
        ObjectRef {
            database: self.db().into(),
            schema: self.pg.then(|| self.ns.clone()),
            name: name.into(),
            kind,
        }
    }

    /// The object's DDL as the DDL section shows it.
    async fn ddl(&self, kind: ObjectKind, name: &str) -> String {
        self.s
            .introspector()
            .object_ddl(&self.obj(kind, name))
            .await
            .unwrap_or_else(|e| panic!("{:?}: DDL of {kind:?} {name}: {e:?}", self.t))
    }

    async fn listed(&self, kind: ObjectKind, name: &str) -> bool {
        self.s
            .introspector()
            .list_objects(&self.scope(), kind)
            .await
            .unwrap_or_else(|e| panic!("{:?}: list {kind:?}: {e:?}", self.t))
            .iter()
            .any(|o| o.name == name)
    }

    async fn edit_open(
        &self,
        kind: ObjectKind,
        name: Option<&str>,
        table: Option<&str>,
    ) -> DdlEditOpen {
        let o = self
            .state
            .sessions
            .ddl_edit_open(
                self.id,
                self.scope(),
                kind,
                name.map(Into::into),
                table.map(Into::into),
            )
            .await
            .unwrap_or_else(|e| panic!("{:?}: open {kind:?} {name:?}: {e:?}", self.t));
        assert_eq!(o.blocked_reason, None, "{:?}: {kind:?} {name:?}", self.t);
        o
    }

    async fn preview(
        &self,
        edit: Uuid,
        text: &str,
        strategy: Option<Strategy>,
    ) -> Result<DdlEditPreview, DbError> {
        self.state
            .sessions
            .ddl_edit_preview(edit, text.into(), strategy)
            .await
    }

    async fn apply_plan(&self, p: &DdlEditPreview) -> Result<DdlEditReport, DbError> {
        self.state
            .sessions
            .ddl_edit_apply(
                p.plan_id,
                true,
                p.confirm_name.clone(),
                Uuid::new_v4(),
                &|_| {},
            )
            .await
    }

    /// Preview + apply, as the UI does with "Apply"; a refused preview or apply panics.
    async fn edit_apply(
        &self,
        edit: Uuid,
        text: &str,
        strategy: Option<Strategy>,
    ) -> DdlEditReport {
        let p = self
            .preview(edit, text, strategy)
            .await
            .unwrap_or_else(|e| panic!("{:?}: preview {text}: {e:?}", self.t));
        self.apply_plan(&p)
            .await
            .unwrap_or_else(|e| panic!("{:?}: apply {:?}: {e:?}", self.t, p.statements))
    }

    async fn applied(&self, edit: Uuid, text: &str, strategy: Option<Strategy>) -> DdlEditReport {
        let r = self.edit_apply(edit, text, strategy).await;
        assert_eq!(
            r.outcome,
            EditOutcome::Applied,
            "{:?}: {text}: {r:?}",
            self.t
        );
        assert_eq!(r.object_state, ObjectState::Changed, "{:?}", self.t);
        r
    }

    /// Drops the namespace, checks it is gone and closes everything, then re-raises a
    /// panic of the body.
    async fn finish(self, result: Result<(), Box<dyn Any + Send>>) {
        let drop = if self.pg {
            format!(
                "DROP SCHEMA IF EXISTS {} CASCADE",
                quote_ident(self.t.kind().as_sql(), &self.ns)
            )
        } else {
            format!(
                "DROP DATABASE IF EXISTS {}",
                quote_ident(self.t.kind().as_sql(), &self.ns)
            )
        };
        self.state.sessions.close(self.id).await;
        self.s.close().await;
        let cleanup = connect(self.t, FIXTURE_DB).await;
        let bound = if self.pg {
            "SET lock_timeout = '10s'"
        } else {
            "SET SESSION lock_wait_timeout = 10"
        };
        let cleaned = match sql(cleanup.as_ref(), bound).await {
            Ok(_) => sql(cleanup.as_ref(), &drop).await,
            Err(e) => Err(e),
        };
        let left = if self.pg {
            format!(
                "SELECT count(*)::text FROM pg_namespace WHERE nspname = '{}'",
                self.ns
            )
        } else {
            format!(
                "SELECT CAST(COUNT(*) AS CHAR) FROM information_schema.SCHEMATA WHERE SCHEMA_NAME = '{}'",
                self.ns
            )
        };
        let left = sql(cleanup.as_ref(), &left)
            .await
            .map(|c| c.0.first().and_then(|r| r.first()).map(value_text));
        cleanup.close().await;
        if let Err(panic) = result {
            if let Err(e) = &cleaned {
                eprintln!("cleanup {:?} failed after a panic: {drop}: {e}", self.t);
            }
            std::panic::resume_unwind(panic);
        }
        assert!(cleaned.is_ok(), "cleanup {:?}: {cleaned:?}", self.t);
        assert_eq!(
            left.as_ref().ok().cloned().flatten().as_deref(),
            Some("0"),
            "{:?}: {} left behind: {left:?}",
            self.t,
            self.ns
        );
        let test = std::thread::current().name().unwrap_or("?").to_string();
        eprintln!("PASS {test} {:?}", self.t);
    }
}

/// Runs `body` on every target in its own scratch namespace.
macro_rules! each_target {
    ($targets:expr, |$sc:ident| $body:block) => {
        for t in $targets {
            let $sc = Scratch::new(t).await;
            let r = AssertUnwindSafe(async { $body }).catch_unwind().await;
            $sc.finish(r).await;
        }
    };
}

/// `t (id, a, b)` with the row `(1, 0, 0)`.
async fn create_t(sc: &Scratch) {
    sc.exec(&format!(
        "CREATE TABLE {} (id int PRIMARY KEY, a int, b int)",
        sc.q("t")
    ))
    .await;
    sc.exec(&format!("INSERT INTO {} VALUES (1, 0, 0)", sc.q("t")))
        .await;
}

/// `text` with `from` replaced by `to` exactly once.
fn swap(text: &str, from: &str, to: &str) -> String {
    assert_eq!(text.matches(from).count(), 1, "{from:?} in {text}");
    text.replacen(from, to, 1)
}

// ---------------------------------------------------------------------------------

#[tokio::test]
async fn edit_each_kind() {
    require_db!();
    each_target!(Target::all(), |sc| {
        let t = sc.t;
        create_t(&sc).await;
        if sc.pg {
            let ns = &sc.ns;
            for stmt in [
                format!("CREATE VIEW {ns}.v AS SELECT id FROM {ns}.t"),
                format!("CREATE MATERIALIZED VIEW {ns}.mv AS SELECT id FROM {ns}.t"),
                format!("CREATE FUNCTION {ns}.f(x integer) RETURNS integer LANGUAGE sql AS $$ SELECT x $$"),
                format!("CREATE PROCEDURE {ns}.p() LANGUAGE sql AS $$ UPDATE {ns}.t SET b = 1 $$"),
                format!("CREATE FUNCTION {ns}.tf() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN NEW.a := 1; RETURN NEW; END $$"),
                format!("CREATE FUNCTION {ns}.tf2() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN NEW.a := 2; RETURN NEW; END $$"),
                format!("CREATE TRIGGER tr BEFORE INSERT ON {ns}.t FOR EACH ROW EXECUTE FUNCTION {ns}.tf()"),
            ] {
                sc.exec(&stmt).await;
            }
        } else {
            for stmt in [
                "CREATE VIEW v AS SELECT id FROM t",
                "CREATE FUNCTION f(x INT) RETURNS INT DETERMINISTIC RETURN x",
                "CREATE PROCEDURE p() UPDATE t SET b = 1",
                "CREATE TRIGGER tr BEFORE INSERT ON t FOR EACH ROW SET NEW.a = 1",
                "CREATE EVENT e ON SCHEDULE EVERY 1 DAY DISABLE DO SELECT 1",
            ] {
                sc.exec(stmt).await;
            }
        }

        // View: add a column.
        let o = sc.edit_open(ObjectKind::View, Some("v"), None).await;
        let text = if sc.pg {
            format!(
                "CREATE OR REPLACE VIEW {0}.v AS SELECT id, a FROM {0}.t",
                sc.ns
            )
        } else {
            let ns = &sc.ns;
            swap(&o.text, " from ", &format!(", `{ns}`.`t`.`a` AS `a` from "))
        };
        let r = sc.applied(o.edit_id, &text, o.strategy).await;
        let ddl = sc.ddl(ObjectKind::View, "v").await;
        assert_eq!(r.text.as_deref(), Some(ddl.as_str()), "{t:?}");
        // PostgreSQL 16+ no longer qualifies the columns of a one-table view.
        let column = if sc.pg { "id,\n" } else { "AS `a`" };
        assert!(ddl.contains(column), "{t:?}: {ddl}");
        assert_eq!(
            sc.one(&format!("SELECT a FROM {} WHERE id = 1", sc.q("v")))
                .await,
            "0"
        );

        // Materialized view (PostgreSQL): add a column.
        if sc.pg {
            let o = sc
                .edit_open(ObjectKind::MaterializedView, Some("mv"), None)
                .await;
            assert_eq!(o.strategy, Some(Strategy::Recreate));
            let text = format!(
                "CREATE MATERIALIZED VIEW {0}.mv AS SELECT id, a FROM {0}.t",
                sc.ns
            );
            sc.applied(o.edit_id, &text, o.strategy).await;
            assert_eq!(
                sc.one(&format!("SELECT a FROM {} WHERE id = 1", sc.q("mv")))
                    .await,
                "0"
            );
        }

        // Function: return x + 1.
        let o = sc.edit_open(ObjectKind::Function, Some("f"), None).await;
        let body = if sc.pg { "SELECT x" } else { "RETURN x" };
        let text = swap(&o.text, body, &format!("{body} + 1"));
        sc.applied(o.edit_id, &text, o.strategy).await;
        assert!(
            sc.ddl(ObjectKind::Function, "f").await.contains("x + 1"),
            "{t:?}"
        );
        assert_eq!(
            sc.one(&format!("SELECT {}(1)", sc.q("f"))).await,
            "2",
            "{t:?}"
        );

        // Procedure: a different statement.
        let o = sc.edit_open(ObjectKind::Procedure, Some("p"), None).await;
        let text = swap(&o.text, "SET b = 1", "SET b = 2");
        sc.applied(o.edit_id, &text, o.strategy).await;
        assert!(
            sc.ddl(ObjectKind::Procedure, "p")
                .await
                .contains("SET b = 2"),
            "{t:?}"
        );
        sc.exec(&format!("CALL {}()", sc.q("p"))).await;
        assert_eq!(
            sc.one(&format!("SELECT b FROM {} WHERE id = 1", sc.q("t")))
                .await,
            "2"
        );

        // Trigger: a different SET.
        let o = sc.edit_open(ObjectKind::Trigger, Some("tr"), None).await;
        assert_eq!(o.target.table.as_deref(), Some("t"), "{t:?}");
        let text = if sc.pg {
            swap(&o.text, "tf()", "tf2()")
        } else {
            swap(&o.text, "SET NEW.a = 1", "SET NEW.a = 2")
        };
        sc.applied(o.edit_id, &text, o.strategy).await;
        sc.exec(&format!("INSERT INTO {} (id) VALUES (10)", sc.q("t")))
            .await;
        assert_eq!(
            sc.one(&format!("SELECT a FROM {} WHERE id = 10", sc.q("t")))
                .await,
            "2",
            "{t:?}"
        );

        // Event (MySQL family): a different schedule.
        if !sc.pg {
            let o = sc.edit_open(ObjectKind::Event, Some("e"), None).await;
            let text = swap(&o.text, "EVERY 1 DAY", "EVERY 2 DAY");
            sc.applied(o.edit_id, &text, o.strategy).await;
            assert!(
                sc.ddl(ObjectKind::Event, "e").await.contains("EVERY 2 DAY"),
                "{t:?}"
            );
            assert_eq!(
                sc.one(&format!(
                    "SELECT CAST(INTERVAL_VALUE AS CHAR) FROM information_schema.EVENTS WHERE EVENT_SCHEMA = '{}' AND EVENT_NAME = 'e'",
                    sc.ns
                ))
                .await,
                "2"
            );
        }
    });
}

#[tokio::test]
async fn templates_create_as_is() {
    require_db!();
    each_target!(Target::all(), |sc| {
        let t = sc.t;
        create_t(&sc).await;
        let kinds: &[(ObjectKind, &str)] = if sc.pg {
            sc.exec(&format!(
                "CREATE FUNCTION {}.trigger_function() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RETURN NEW; END $$",
                sc.ns
            ))
            .await;
            &[
                (ObjectKind::View, "new_view"),
                (ObjectKind::MaterializedView, "new_materialized_view"),
                (ObjectKind::Function, "new_function"),
                (ObjectKind::Procedure, "new_procedure"),
                (ObjectKind::Trigger, "new_trigger"),
            ]
        } else {
            &[
                (ObjectKind::View, "new_view"),
                (ObjectKind::Function, "new_function"),
                (ObjectKind::Procedure, "new_procedure"),
                (ObjectKind::Trigger, "new_trigger"),
                (ObjectKind::Event, "new_event"),
            ]
        };
        for &(kind, name) in kinds {
            let table = (kind == ObjectKind::Trigger).then_some("t");
            let o = sc.edit_open(kind, None, table).await;
            assert!(o.object.is_none());
            let r = sc.applied(o.edit_id, &o.text, None).await;
            assert_eq!(
                r.object.as_ref().map(|o| o.name.as_str()),
                Some(name),
                "{t:?}"
            );
            assert!(
                sc.listed(kind, name).await,
                "{t:?}: {kind:?} {name} not listed"
            );
        }
    });
}

#[tokio::test]
async fn pg_view_recreate_changes_columns() {
    require_db!();
    each_target!(Target::pg(), |sc| {
        let t = sc.t;
        let ns = sc.ns.clone();
        create_t(&sc).await;
        sc.exec(&format!("CREATE VIEW {ns}.v AS SELECT id, a FROM {ns}.t"))
            .await;
        let o = sc.edit_open(ObjectKind::View, Some("v"), None).await;
        // Drops column `a` and renames `id`: CREATE OR REPLACE VIEW refuses that.
        let text = format!("CREATE VIEW {ns}.v AS SELECT id AS key FROM {ns}.t");
        let r = sc
            .edit_apply(o.edit_id, &text, Some(Strategy::Replace))
            .await;
        assert_eq!(r.outcome, EditOutcome::RolledBack, "{t:?}: {r:?}");
        assert_eq!(r.object_state, ObjectState::Unchanged);
        let e = r.error.unwrap();
        assert!(
            e.server_code.is_some() || e.sqlstate.is_some(),
            "{t:?}: {e:?}"
        );
        assert_eq!(sc.one(&format!("SELECT a FROM {ns}.v")).await, "0");
        let r = sc.applied(o.edit_id, &text, Some(Strategy::Recreate)).await;
        assert!(r.text.unwrap().contains("key"), "{t:?}");
        assert_eq!(sc.one(&format!("SELECT key FROM {ns}.v")).await, "1");
    });
}

async fn pg_signatures(sc: &Scratch) -> Vec<String> {
    sc.rows(&format!(
        "SELECT pg_get_function_identity_arguments(p.oid) FROM pg_proc p JOIN pg_namespace n ON n.oid = p.pronamespace WHERE n.nspname = '{}' AND p.proname = 'f' ORDER BY 1",
        sc.ns
    ))
    .await
    .into_iter()
    .map(|r| r[0].clone())
    .collect()
}

#[tokio::test]
async fn pg_signature_change_is_rolled_back() {
    require_db!();
    each_target!(Target::pg(), |sc| {
        let t = sc.t;
        let ns = sc.ns.clone();
        sc.exec(&format!(
            "CREATE FUNCTION {ns}.f(a integer) RETURNS integer LANGUAGE sql AS $$ SELECT a $$"
        ))
        .await;
        let o = sc.edit_open(ObjectKind::Function, Some("f"), None).await;
        let text = format!(
            "CREATE OR REPLACE FUNCTION {ns}.f(a bigint) RETURNS integer LANGUAGE sql AS $$ SELECT a::integer $$"
        );
        let r = sc
            .edit_apply(o.edit_id, &text, Some(Strategy::Replace))
            .await;
        assert_eq!(r.outcome, EditOutcome::RolledBack, "{t:?}: {r:?}");
        assert_eq!(r.object_state, ObjectState::Unchanged);
        assert_eq!(r.error.unwrap().message, REPLACE_SIGNATURE_FAILURE);
        assert_eq!(pg_signatures(&sc).await, ["a integer"], "{t:?}");
        sc.applied(o.edit_id, &text, Some(Strategy::Recreate)).await;
        assert_eq!(pg_signatures(&sc).await, ["a bigint"], "{t:?}");
    });
}

#[tokio::test]
async fn pg_mv_indexes_recreated() {
    require_db!();
    each_target!(Target::pg(), |sc| {
        let t = sc.t;
        let ns = sc.ns.clone();
        create_t(&sc).await;
        sc.exec(&format!(
            "CREATE MATERIALIZED VIEW {ns}.mv AS SELECT id, a FROM {ns}.t"
        ))
        .await;
        sc.exec(&format!("CREATE UNIQUE INDEX mv_id ON {ns}.mv (id)"))
            .await;
        sc.exec(&format!("CREATE INDEX mv_a ON {ns}.mv (a)")).await;
        let o = sc
            .edit_open(ObjectKind::MaterializedView, Some("mv"), None)
            .await;
        let text = format!("CREATE MATERIALIZED VIEW {ns}.mv AS SELECT id, a, b FROM {ns}.t");
        let p = sc.preview(o.edit_id, &text, o.strategy).await.unwrap();
        assert_eq!(p.statements.len(), 4, "{t:?}: {:?}", p.statements);
        let r = sc.apply_plan(&p).await.unwrap();
        assert_eq!(r.outcome, EditOutcome::Applied, "{t:?}: {r:?}");
        let indexes: Vec<String> = sc
            .rows(&format!(
                "SELECT indexname::text FROM pg_indexes WHERE schemaname = '{ns}' AND tablename = 'mv' ORDER BY 1"
            ))
            .await
            .into_iter()
            .map(|r| r[0].clone())
            .collect();
        assert_eq!(indexes, ["mv_a", "mv_id"], "{t:?}");
        assert_eq!(sc.one(&format!("SELECT b FROM {ns}.mv")).await, "0");
    });
}

/// `pg_class.reloptions` of `ns.name`, comma-joined.
async fn pg_reloptions(sc: &Scratch, name: &str) -> String {
    sc.one(&format!(
        "SELECT coalesce(array_to_string(reloptions, ','), '') FROM pg_class WHERE oid = '{}.{name}'::regclass",
        sc.ns
    ))
    .await
}

#[tokio::test]
async fn pg_view_options_survive_edits() {
    require_db!();
    each_target!(Target::pg(), |sc| {
        let t = sc.t;
        let ns = sc.ns.clone();
        create_t(&sc).await;
        // security_invoker exists from PostgreSQL 15.
        let invoker = t == Target::Pg17;
        let with = if invoker {
            "security_barrier=true, security_invoker=true"
        } else {
            "security_barrier=true"
        };
        sc.exec(&format!(
            "CREATE VIEW {ns}.v WITH ({with}) AS SELECT id, a FROM {ns}.t WHERE a >= 0 WITH LOCAL CHECK OPTION"
        ))
        .await;
        let want = if invoker {
            "security_barrier=true,security_invoker=true,check_option=local"
        } else {
            "security_barrier=true,check_option=local"
        };
        assert_eq!(pg_reloptions(&sc, "v").await, want, "{t:?}");
        sc.exec(&format!(
            "CREATE MATERIALIZED VIEW {ns}.mv WITH (fillfactor=70) AS SELECT id FROM {ns}.t"
        ))
        .await;
        for (i, strategy) in [Strategy::Replace, Strategy::Recreate]
            .into_iter()
            .enumerate()
        {
            let o = sc.edit_open(ObjectKind::View, Some("v"), None).await;
            assert!(
                o.text.contains(&format!("WITH ({with})")),
                "{t:?}: {}",
                o.text
            );
            assert!(
                o.text.contains("WITH LOCAL CHECK OPTION"),
                "{t:?}: {}",
                o.text
            );
            let text = swap(&o.text, &format!(">= {i}"), &format!(">= {}", i + 1));
            sc.applied(o.edit_id, &text, Some(strategy)).await;
            assert_eq!(pg_reloptions(&sc, "v").await, want, "{t:?} {strategy:?}");
        }
        let o = sc
            .edit_open(ObjectKind::MaterializedView, Some("mv"), None)
            .await;
        assert!(o.text.contains("WITH (fillfactor=70)"), "{t:?}: {}", o.text);
        sc.applied(o.edit_id, &o.text, o.strategy).await;
        assert_eq!(pg_reloptions(&sc, "mv").await, "fillfactor=70", "{t:?}");
    });
}

#[tokio::test]
async fn pg_view_recreate_keeps_triggers_and_rules() {
    require_db!();
    each_target!(Target::pg(), |sc| {
        let t = sc.t;
        let ns = sc.ns.clone();
        create_t(&sc).await;
        for stmt in [
            format!("CREATE VIEW {ns}.v AS SELECT id, a FROM {ns}.t"),
            format!("CREATE FUNCTION {ns}.vi() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN INSERT INTO {ns}.t (id, a, b) VALUES (NEW.id, 7, 7); RETURN NEW; END $$"),
            format!("CREATE TRIGGER vi INSTEAD OF INSERT ON {ns}.v FOR EACH ROW EXECUTE FUNCTION {ns}.vi()"),
            format!("CREATE RULE vd AS ON DELETE TO {ns}.v DO INSTEAD NOTHING"),
        ] {
            sc.exec(&stmt).await;
        }
        let o = sc.edit_open(ObjectKind::View, Some("v"), None).await;
        let text = format!("CREATE VIEW {ns}.v AS SELECT id, a, b FROM {ns}.t");
        let p = sc
            .preview(o.edit_id, &text, Some(Strategy::Recreate))
            .await
            .unwrap();
        assert_eq!(p.statements.len(), 4, "{t:?}: {:?}", p.statements);
        assert!(
            p.statements[2].starts_with("CREATE TRIGGER vi INSTEAD OF INSERT"),
            "{t:?}: {:?}",
            p.statements
        );
        assert!(
            p.statements[3].starts_with("CREATE RULE vd AS"),
            "{t:?}: {:?}",
            p.statements
        );
        let r = sc.apply_plan(&p).await.unwrap();
        assert_eq!(r.outcome, EditOutcome::Applied, "{t:?}: {r:?}");
        // The trigger still routes inserts, the rule still swallows deletes.
        sc.exec(&format!("INSERT INTO {ns}.v (id) VALUES (5)"))
            .await;
        assert_eq!(
            sc.one(&format!("SELECT a FROM {ns}.t WHERE id = 5")).await,
            "7"
        );
        sc.exec(&format!("DELETE FROM {ns}.v")).await;
        assert_eq!(
            sc.one(&format!("SELECT count(*)::text FROM {ns}.t")).await,
            "2"
        );
        // A rule that no longer fits the new view rolls the change back.
        sc.exec(&format!(
            "CREATE RULE vu AS ON UPDATE TO {ns}.v DO INSTEAD UPDATE {ns}.t SET b = NEW.b WHERE id = OLD.id"
        ))
        .await;
        let o = sc.edit_open(ObjectKind::View, Some("v"), None).await;
        let r = sc
            .edit_apply(
                o.edit_id,
                &format!("CREATE VIEW {ns}.v AS SELECT id, a FROM {ns}.t"),
                Some(Strategy::Recreate),
            )
            .await;
        assert_eq!(r.outcome, EditOutcome::RolledBack, "{t:?}: {r:?}");
        assert_eq!(r.object_state, ObjectState::Unchanged);
        assert_eq!(
            sc.one(&format!("SELECT b FROM {ns}.v WHERE id = 1")).await,
            "0"
        );
        assert_eq!(
            sc.one(&format!(
                "SELECT count(*)::text FROM pg_rewrite WHERE ev_class = '{ns}.v'::regclass AND rulename <> '_RETURN'"
            ))
            .await,
            "2"
        );
    });
}

#[tokio::test]
async fn pg_recreate_keeps_owner() {
    require_db!();
    for t in Target::pg() {
        let sc = Scratch::new(t).await;
        let role = format!("p4c_owner_{}", &Uuid::new_v4().simple().to_string()[..12]);
        sc.exec(&format!("CREATE ROLE {role} NOLOGIN")).await;
        let r = AssertUnwindSafe(async {
            let ns = sc.ns.clone();
            create_t(&sc).await;
            for stmt in [
                format!("CREATE VIEW {ns}.v AS SELECT id FROM {ns}.t"),
                format!("CREATE MATERIALIZED VIEW {ns}.mv AS SELECT id FROM {ns}.t"),
                format!("CREATE FUNCTION {ns}.f(x integer) RETURNS integer LANGUAGE sql AS $$ SELECT x $$"),
                format!("CREATE PROCEDURE {ns}.p() LANGUAGE sql AS $$ SELECT 1 $$"),
                format!("ALTER VIEW {ns}.v OWNER TO {role}"),
                format!("ALTER MATERIALIZED VIEW {ns}.mv OWNER TO {role}"),
                format!("ALTER FUNCTION {ns}.f(integer) OWNER TO {role}"),
                format!("ALTER PROCEDURE {ns}.p() OWNER TO {role}"),
            ] {
                sc.exec(&stmt).await;
            }
            let owner_of_rel = |name: &str| {
                format!("SELECT pg_get_userbyid(relowner)::text FROM pg_class WHERE oid = '{ns}.{name}'::regclass")
            };
            let owner_of_proc = |name: &str| {
                format!("SELECT pg_get_userbyid(p.proowner)::text FROM pg_proc p JOIN pg_namespace n ON n.oid = p.pronamespace WHERE n.nspname = '{ns}' AND p.proname = '{name}'")
            };
            for (kind, name, text, owner_sql) in [
                (ObjectKind::View, "v", format!("CREATE VIEW {ns}.v AS SELECT id, a FROM {ns}.t"), owner_of_rel("v")),
                (ObjectKind::MaterializedView, "mv", format!("CREATE MATERIALIZED VIEW {ns}.mv AS SELECT id, a FROM {ns}.t"), owner_of_rel("mv")),
                // A new signature: the owner is set by name, not by the old arguments.
                (ObjectKind::Function, "f", format!("CREATE FUNCTION {ns}.f(x bigint) RETURNS bigint LANGUAGE sql AS $$ SELECT x $$"), owner_of_proc("f")),
                (ObjectKind::Procedure, "p", format!("CREATE PROCEDURE {ns}.p() LANGUAGE sql AS $$ SELECT 2 $$"), owner_of_proc("p")),
            ] {
                let o = sc.edit_open(kind, Some(name), None).await;
                let p = sc.preview(o.edit_id, &text, Some(Strategy::Recreate)).await.unwrap();
                assert!(
                    p.statements.last().unwrap().ends_with(&format!("OWNER TO \"{role}\"")),
                    "{t:?}: {:?}",
                    p.statements
                );
                let r = sc.apply_plan(&p).await.unwrap();
                assert_eq!(r.outcome, EditOutcome::Applied, "{t:?} {kind:?}: {r:?}");
                assert_eq!(sc.one(&owner_sql).await, role, "{t:?} {kind:?}");
            }
        })
        .catch_unwind()
        .await;
        let _ = sql(sc.s.as_ref(), &format!("DROP OWNED BY {role}")).await;
        let dropped = sql(sc.s.as_ref(), &format!("DROP ROLE {role}")).await;
        if r.is_ok() {
            assert!(dropped.is_ok(), "{t:?}: DROP ROLE {role}: {dropped:?}");
        }
        sc.finish(r).await;
    }
}

#[tokio::test]
async fn mysql_trigger_gap_and_restore() {
    require_db!();
    each_target!([Target::MySql57, Target::MySql8], |sc| {
        let t = sc.t;
        create_t(&sc).await;
        sc.exec("CREATE TRIGGER tr BEFORE INSERT ON t FOR EACH ROW SET NEW.a = 1")
            .await;
        let o = sc.edit_open(ObjectKind::Trigger, Some("tr"), None).await;
        let original = o.text.clone();
        let text = swap(&original, "SET NEW.a = 1", "SET NEW.a = NEW.no_such_column");
        let p = sc.preview(o.edit_id, &text, o.strategy).await.unwrap();
        assert!(p.gap, "{t:?}");
        let r = sc.apply_plan(&p).await.unwrap();
        assert_eq!(r.outcome, EditOutcome::Partial, "{t:?}: {r:?}");
        assert_eq!(r.object_state, ObjectState::Missing);
        assert_eq!(r.executed, p.statements[..1]);
        assert_eq!(r.failed.as_deref(), Some(p.statements[1].as_str()));
        assert!(!sc.listed(ObjectKind::Trigger, "tr").await, "{t:?}");
        let restore = r.restore_text.unwrap();
        assert_eq!(restore, original);
        // The restore is an ordinary create of the same name.
        let p = sc.preview(o.edit_id, &restore, None).await.unwrap();
        assert_eq!(p.statements, std::slice::from_ref(&restore));
        let r = sc.apply_plan(&p).await.unwrap();
        assert_eq!(r.outcome, EditOutcome::Applied, "{t:?}: {r:?}");
        assert_eq!(r.object.as_ref().map(|o| o.name.as_str()), Some("tr"));
        assert!(sc
            .ddl(ObjectKind::Trigger, "tr")
            .await
            .contains("SET NEW.a = 1"));
        sc.exec("INSERT INTO t (id) VALUES (10)").await;
        assert_eq!(sc.one("SELECT a FROM t WHERE id = 10").await, "1");
    });
}

#[tokio::test]
async fn mysql_procedure_rehearsal_protects() {
    require_db!();
    each_target!([Target::MySql57, Target::MySql8], |sc| {
        let t = sc.t;
        create_t(&sc).await;
        sc.exec("CREATE PROCEDURE p() UPDATE t SET b = 1").await;
        let o = sc.edit_open(ObjectKind::Procedure, Some("p"), None).await;
        let text = swap(&o.text, "SET b = 1", "SET b = = 2");
        let r = sc.edit_apply(o.edit_id, &text, o.strategy).await;
        assert_eq!(r.outcome, EditOutcome::NotApplied, "{t:?}: {r:?}");
        assert_eq!(r.object_state, ObjectState::Unchanged);
        assert!(r.executed.is_empty(), "{t:?}: {:?}", r.executed);
        assert!(r.failed.unwrap().contains("_dnt_p4c_check_"));
        assert_eq!(sc.ddl(ObjectKind::Procedure, "p").await, o.text);
        sc.exec("CALL p()").await;
        assert_eq!(sc.one("SELECT b FROM t WHERE id = 1").await, "1");
        let left = sc
            .one(&format!(
                "SELECT CAST(COUNT(*) AS CHAR) FROM information_schema.ROUTINES WHERE ROUTINE_SCHEMA = '{}' AND ROUTINE_NAME LIKE '\\_dnt\\_p4c\\_check\\_%'",
                sc.ns
            ))
            .await;
        assert_eq!(left, "0", "{t:?}");
    });
}

#[tokio::test]
async fn mariadb_or_replace_failure_keeps_object() {
    require_db!();
    each_target!([Target::MariaDb106, Target::MariaDb114], |sc| {
        let t = sc.t;
        create_t(&sc).await;
        sc.exec("CREATE PROCEDURE p() UPDATE t SET b = 1").await;
        let o = sc.edit_open(ObjectKind::Procedure, Some("p"), None).await;
        assert_eq!(o.strategy, Some(Strategy::Replace));
        let text = swap(&o.text, "SET b = 1", "SET b = = 2");
        let r = sc.edit_apply(o.edit_id, &text, o.strategy).await;
        assert_ne!(r.outcome, EditOutcome::Applied, "{t:?}: {r:?}");
        assert_eq!(r.object_state, ObjectState::Unchanged, "{t:?}: {r:?}");
        assert_eq!(r.outcome, EditOutcome::NotApplied, "{t:?}: {r:?}");
        assert_eq!(sc.ddl(ObjectKind::Procedure, "p").await, o.text);
        sc.exec("CALL p()").await;
        assert_eq!(sc.one("SELECT b FROM t WHERE id = 1").await, "1");
    });
}

#[tokio::test]
async fn delimiter_wrapped_text_applies() {
    require_db!();
    each_target!(Target::mysql_family(), |sc| {
        let t = sc.t;
        create_t(&sc).await;
        sc.exec("CREATE PROCEDURE p() UPDATE t SET b = 1").await;
        let o = sc.edit_open(ObjectKind::Procedure, Some("p"), None).await;
        let text = "DELIMITER $$\nCREATE PROCEDURE `p`()\nBEGIN\n  UPDATE t SET b = 5;\n  UPDATE t SET a = 6;\nEND$$\nDELIMITER ;\n";
        let r = sc.applied(o.edit_id, text, o.strategy).await;
        assert!(r.text.unwrap().contains("SET a = 6"), "{t:?}");
        sc.exec("CALL p()").await;
        assert_eq!(
            sc.rows("SELECT a, b FROM t WHERE id = 1").await,
            [["6", "5"]],
            "{t:?}"
        );
    });
}

#[tokio::test]
async fn concurrent_change_is_stale() {
    require_db!();
    each_target!(Target::all(), |sc| {
        let t = sc.t;
        create_t(&sc).await;
        let (v, tbl) = (sc.q("v"), sc.q("t"));
        sc.exec(&format!("CREATE VIEW {v} AS SELECT id FROM {tbl}"))
            .await;
        let o = sc.edit_open(ObjectKind::View, Some("v"), None).await;
        let text = if sc.pg {
            format!("CREATE OR REPLACE VIEW {v} AS SELECT id, a FROM {tbl}")
        } else {
            "CREATE VIEW `v` AS SELECT id, a FROM t".to_string()
        };
        let p = sc.preview(o.edit_id, &text, o.strategy).await.unwrap();
        // Another session changes the view between the preview and the apply.
        sc.exec(&format!(
            "CREATE OR REPLACE VIEW {v} AS SELECT id, b FROM {tbl}"
        ))
        .await;
        let r = sc.apply_plan(&p).await;
        assert!(
            matches!(&r, Err(DbError::PlanStale { reason }) if reason == OBJECT_CHANGED),
            "{t:?}: {r:?}"
        );
        assert_eq!(sc.one(&format!("SELECT b FROM {v}")).await, "0", "{t:?}");
        assert!(
            sql(sc.s.as_ref(), &format!("SELECT a FROM {v}"))
                .await
                .is_err(),
            "{t:?}: the edit was applied"
        );

        // Dropped by another session: also stale, not a raw "does not exist".
        let o = sc.edit_open(ObjectKind::View, Some("v"), None).await;
        let p = sc.preview(o.edit_id, &text, o.strategy).await.unwrap();
        sc.exec(&format!("DROP VIEW {v}")).await;
        let r = sc.apply_plan(&p).await;
        assert!(
            matches!(&r, Err(DbError::PlanStale { reason }) if reason == OBJECT_CHANGED),
            "{t:?}: {r:?}"
        );
        assert!(!sc.listed(ObjectKind::View, "v").await, "{t:?}");
    });
}
