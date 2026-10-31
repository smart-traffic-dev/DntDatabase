//! P4a object actions against the six-server local Docker matrix.

use dnt_core::config::{ConnOptions, ConnectionConfig, SecretMode};
use dnt_core::driver::{Driver, ExecOptions, ResultSummary, RowSink, Session};
use dnt_core::meta::{ObjectKind, ObjectRef};
use dnt_core::secret::MemorySecretStore;
use dnt_core::server::DbKind;
use dnt_core::sql::{
    classify,
    ddl::ObjectAction,
    ident::{qualified_name, quote_ident},
};
use dnt_core::value::{ColumnMeta, Value};
use dnt_driver_mysql::MySqlDriver;
use dnt_driver_pg::PgDriver;
use dnt_testkit::{require_db, Target, FIXTURE_DB, TEST_PASSWORD};
use dntdatabase::commands::connections::{
    create_connection_impl, open_connection_impl, SecretsInput,
};
use dntdatabase::object_actions::ActionOutcome;
use dntdatabase::state::{AppPaths, AppState};
use serde_json::json;
use std::sync::Arc;
use uuid::Uuid;

fn app_state(dir: &std::path::Path) -> AppState {
    AppState::open(
        AppPaths::in_dir(dir.join("data")),
        Arc::new(MemorySecretStore::default()),
    )
    .unwrap()
}

async fn open(state: &AppState, t: Target) -> Uuid {
    open_on(state, t, Some(FIXTURE_DB)).await
}

async fn open_on(state: &AppState, t: Target, default_db: Option<&str>) -> Uuid {
    let cfg = ConnectionConfig {
        id: Uuid::nil(),
        group_id: None,
        name: format!("{t:?}"),
        sort_order: 0,
        kind: t.kind(),
        host: "127.0.0.1".into(),
        port: t.host_port(),
        username: t.admin_user().into(),
        default_db: default_db.map(str::to_string),
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

#[derive(Default)]
struct Collect(Vec<Vec<Value>>);
impl RowSink for Collect {
    fn result_start(&mut self, _: &[ColumnMeta]) {}
    fn rows(&mut self, rows: Vec<Vec<Value>>) {
        self.0.extend(rows);
    }
    fn result_end(&mut self, _: &ResultSummary) {}
}
async fn sql(s: &dyn Session, text: &str) -> Result<Collect, dnt_core::error::DbError> {
    let s = s.sql().unwrap();
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
async fn action(
    state: &AppState,
    id: Uuid,
    a: ObjectAction,
) -> dntdatabase::object_actions::ActionReport {
    let p = state.sessions.object_action_preview(id, a).await.unwrap();
    state
        .sessions
        .object_action_apply(
            p.plan_id,
            true,
            p.confirm_name.clone(),
            Uuid::new_v4(),
            &|_| {},
        )
        .await
        .unwrap()
}
#[tokio::test]
async fn namespace_relation_lifecycle_and_dependencies_on_every_server() {
    require_db!();
    for target in Target::all() {
        let dir = tempfile::tempdir().unwrap();
        let state = app_state(dir.path());
        let id = open(&state, target).await;
        let driver: Box<dyn Driver> = if target.kind() == DbKind::MySql {
            Box::new(MySqlDriver)
        } else {
            Box::new(PgDriver)
        };
        let session = driver
            .connect(&target.resolved(Some(FIXTURE_DB)))
            .await
            .unwrap();
        let pg = target.kind() == DbKind::Postgres;
        let ns = format!("p4a_{}", Uuid::new_v4().simple());
        let db = if pg { FIXTURE_DB } else { &ns };
        let schema = pg.then_some(ns.as_str());
        let rel = |name: &str| qualified_name(target.kind().as_sql(), db, schema, name);
        let obj = |name: &str, kind| ObjectRef {
            database: db.into(),
            schema: schema.map(str::to_string),
            name: name.into(),
            kind,
        };
        let cleanup = if pg {
            format!(
                "DROP SCHEMA IF EXISTS {} CASCADE",
                quote_ident(target.kind().as_sql(), &ns)
            )
        } else {
            format!(
                "DROP DATABASE IF EXISTS {}",
                quote_ident(target.kind().as_sql(), &ns)
            )
        };
        // Run assertions in a task: cleanup still runs after an assertion panic.
        let checked = std::panic::AssertUnwindSafe(async {
            assert_eq!(
                action(
                    &state,
                    id,
                    ObjectAction::CreateNamespace {
                        database: FIXTURE_DB.into(),
                        name: ns.clone(),
                        charset: None,
                        collation: None,
                        owner: None,
                    }
                )
                .await
                .outcome,
                ActionOutcome::Applied,
                "{target:?}"
            );
            let identity = if pg {
                "BIGINT GENERATED BY DEFAULT AS IDENTITY PRIMARY KEY"
            } else {
                "BIGINT AUTO_INCREMENT PRIMARY KEY"
            };
            sql(
                session.as_ref(),
                &format!(
                    "CREATE TABLE {} (id {identity}, label VARCHAR(30))",
                    rel("items")
                ),
            )
            .await
            .unwrap();
            sql(
                session.as_ref(),
                &format!("INSERT INTO {} (label) VALUES ('first')", rel("items")),
            )
            .await
            .unwrap();
            assert_eq!(
                action(
                    &state,
                    id,
                    ObjectAction::RenameTable {
                        object: obj("items", ObjectKind::Table),
                        name: "renamed".into()
                    }
                )
                .await
                .outcome,
                ActionOutcome::Applied
            );
            assert_eq!(
                sql(
                    session.as_ref(),
                    &format!("SELECT label FROM {}", rel("renamed"))
                )
                .await
                .unwrap()
                .0
                .len(),
                1
            );
            assert_eq!(
                action(
                    &state,
                    id,
                    ObjectAction::TruncateTable {
                        object: obj("renamed", ObjectKind::Table)
                    }
                )
                .await
                .outcome,
                ActionOutcome::Applied
            );
            assert!(sql(
                session.as_ref(),
                &format!("SELECT * FROM {}", rel("renamed"))
            )
            .await
            .unwrap()
            .0
            .is_empty());
            sql(
                session.as_ref(),
                &format!("INSERT INTO {} (label) VALUES ('second')", rel("renamed")),
            )
            .await
            .unwrap();
            let rows = sql(
                session.as_ref(),
                &format!("SELECT id FROM {}", rel("renamed")),
            )
            .await
            .unwrap();
            let rendered = format!("{:?}", rows.0[0][0]);
            assert!(
                rendered.contains(if pg { '2' } else { '1' }),
                "identity {target:?}: {rendered}"
            );
            sql(
                session.as_ref(),
                &format!(
                    "CREATE VIEW {} AS SELECT * FROM {}",
                    rel("v"),
                    rel("renamed")
                ),
            )
            .await
            .unwrap();
            if pg {
                let r = action(
                    &state,
                    id,
                    ObjectAction::DropObject {
                        object: obj("renamed", ObjectKind::Table),
                    },
                )
                .await;
                assert_eq!(r.outcome, ActionOutcome::RolledBack);
                assert_eq!(
                    sql(session.as_ref(), &format!("SELECT * FROM {}", rel("v")))
                        .await
                        .unwrap()
                        .0
                        .len(),
                    1
                );
                let r = action(
                    &state,
                    id,
                    ObjectAction::DropNamespace {
                        database: FIXTURE_DB.into(),
                        name: ns.clone(),
                    },
                )
                .await;
                assert_eq!(r.outcome, ActionOutcome::RolledBack);
                sql(
                    session.as_ref(),
                    &format!(
                        "CREATE MATERIALIZED VIEW {} AS SELECT * FROM {}",
                        rel("mv"),
                        rel("renamed")
                    ),
                )
                .await
                .unwrap();
                assert_eq!(
                    action(
                        &state,
                        id,
                        ObjectAction::DropObject {
                            object: obj("mv", ObjectKind::MaterializedView)
                        }
                    )
                    .await
                    .outcome,
                    ActionOutcome::Applied
                );
            }
            assert_eq!(
                action(
                    &state,
                    id,
                    ObjectAction::DropObject {
                        object: obj("v", ObjectKind::View)
                    }
                )
                .await
                .outcome,
                ActionOutcome::Applied
            );
            assert_eq!(
                action(
                    &state,
                    id,
                    ObjectAction::DropObject {
                        object: obj("renamed", ObjectKind::Table)
                    }
                )
                .await
                .outcome,
                ActionOutcome::Applied
            );
            assert_eq!(
                action(
                    &state,
                    id,
                    ObjectAction::DropNamespace {
                        database: FIXTURE_DB.into(),
                        name: ns.clone()
                    }
                )
                .await
                .outcome,
                ActionOutcome::Applied
            );
        });
        use futures_util::FutureExt;
        let result = checked.catch_unwind().await;
        let cleaned = sql(session.as_ref(), &cleanup).await;
        session.close().await;
        state.sessions.close(id).await;
        assert!(cleaned.is_ok(), "cleanup {target:?}");
        if let Err(panic) = result {
            std::panic::resume_unwind(panic);
        }
    }
}

/// The tree's "Create database…" sends an empty `database` on MySQL; a hyphenated
/// name must be created without any error or warning.
#[tokio::test]
async fn mysql_create_database_from_the_tree_reports_no_error() {
    require_db!();
    for target in Target::all()
        .into_iter()
        .filter(|t| t.kind() == DbKind::MySql)
    {
        let dir = tempfile::tempdir().unwrap();
        let state = app_state(dir.path());
        let id = open_on(&state, target, None).await;
        let name = format!("p4a-{}", Uuid::new_v4().simple());
        let report = state
            .sessions
            .object_action_preview(
                id,
                ObjectAction::CreateNamespace {
                    database: String::new(),
                    name: name.clone(),
                    charset: None,
                    collation: None,
                    owner: None,
                },
            )
            .await;
        let report = match report {
            Ok(p) => {
                state
                    .sessions
                    .object_action_apply(p.plan_id, true, None, Uuid::new_v4(), &|_| {})
                    .await
            }
            Err(e) => Err(e),
        };
        // Fill it like a restored dump and browse it in the tree.
        let filler = MySqlDriver
            .connect(&target.resolved(Some(FIXTURE_DB)))
            .await
            .unwrap();
        let q = quote_ident(DbKind::MySql.as_sql(), &name);
        for stmt in [
            format!("CREATE TABLE {q}.t (id INT PRIMARY KEY, v INT)"),
            format!("CREATE TABLE {q}.u (id INT PRIMARY KEY)"),
            format!("CREATE VIEW {q}.v AS SELECT id FROM {q}.t"),
            format!("CREATE TRIGGER {q}.tr BEFORE INSERT ON {q}.t FOR EACH ROW SET NEW.v = 1"),
            format!("CREATE PROCEDURE {q}.p() SELECT 1"),
            format!("CREATE FUNCTION {q}.f() RETURNS INT DETERMINISTIC RETURN 1"),
            format!("CREATE EVENT {q}.e ON SCHEDULE EVERY 1 DAY DISABLE DO SELECT 1"),
        ] {
            if let Err(e) = sql(filler.as_ref(), &stmt).await {
                panic!("{target:?} setup {stmt}: {e:?}");
            }
        }
        filler.close().await;
        use dntdatabase::commands::browse::{
            list_databases_impl, list_objects_impl, table_detail_impl,
        };
        list_databases_impl(&state, id, false).await.unwrap();
        for kind in [
            ObjectKind::Table,
            ObjectKind::View,
            ObjectKind::Trigger,
            ObjectKind::Procedure,
            ObjectKind::Function,
            ObjectKind::Event,
        ] {
            list_objects_impl(
                &state,
                id,
                dnt_core::meta::Scope {
                    database: name.clone(),
                    schema: None,
                },
                kind,
            )
            .await
            .unwrap();
        }
        table_detail_impl(
            &state,
            id,
            ObjectRef {
                database: name.clone(),
                schema: None,
                name: "t".into(),
                kind: ObjectKind::Table,
            },
        )
        .await
        .unwrap();
        state.sessions.schema_cache_for(id, &name).await.unwrap();
        // The tree's "Drop database…" on the new database.
        let dropped = match state
            .sessions
            .object_action_preview(
                id,
                ObjectAction::DropNamespace {
                    database: name.clone(),
                    name: name.clone(),
                },
            )
            .await
        {
            Ok(p) => {
                state
                    .sessions
                    .object_action_apply(p.plan_id, true, p.confirm_name, Uuid::new_v4(), &|_| {})
                    .await
            }
            Err(e) => Err(e),
        };
        let listed = list_databases_impl(&state, id, false)
            .await
            .map(|d| d.iter().any(|d| d.name == name));
        let session = MySqlDriver
            .connect(&target.resolved(Some(FIXTURE_DB)))
            .await
            .unwrap();
        let cleaned = sql(
            session.as_ref(),
            &format!(
                "DROP DATABASE IF EXISTS {}",
                quote_ident(DbKind::MySql.as_sql(), &name)
            ),
        )
        .await;
        session.close().await;
        state.sessions.close(id).await;
        let report = report.unwrap();
        assert_eq!(report.outcome, ActionOutcome::Applied, "{target:?}");
        assert!(report.error.is_none(), "{target:?}: {:?}", report.error);
        assert!(
            report.warnings.is_empty(),
            "{target:?}: {:?}",
            report.warnings
        );
        let dropped = dropped.unwrap();
        assert_eq!(
            dropped.outcome,
            ActionOutcome::Applied,
            "{target:?}: {dropped:?}"
        );
        assert!(dropped.error.is_none(), "{target:?}: {:?}", dropped.error);
        assert!(!listed.unwrap(), "{target:?}: still listed after the drop");
        assert!(cleaned.is_ok(), "cleanup {target:?}");
    }
}

#[tokio::test]
async fn stop_cancels_a_real_metadata_lock_wait_without_replaying_ddl() {
    require_db!();
    for target in [Target::MySql8, Target::Pg17] {
        let dir = tempfile::tempdir().unwrap();
        let state = app_state(dir.path());
        let id = open(&state, target).await;
        let driver: Box<dyn Driver> = if target.kind() == DbKind::MySql {
            Box::new(MySqlDriver)
        } else {
            Box::new(PgDriver)
        };
        let holder = driver
            .connect(&target.resolved(Some(FIXTURE_DB)))
            .await
            .unwrap();
        let name = format!("p4a_lock_{}", Uuid::new_v4().simple());
        let pg = target.kind() == DbKind::Postgres;
        let object = ObjectRef {
            database: FIXTURE_DB.into(),
            schema: pg.then(|| "public".into()),
            name,
            kind: ObjectKind::Table,
        };
        let rel = qualified_name(
            target.kind().as_sql(),
            FIXTURE_DB,
            object.schema.as_deref(),
            &object.name,
        );
        sql(
            holder.as_ref(),
            &format!("CREATE TABLE {rel} (id INT PRIMARY KEY)"),
        )
        .await
        .unwrap();
        let p = state
            .sessions
            .object_action_preview(id, ObjectAction::TruncateTable { object })
            .await
            .unwrap();
        sql(holder.as_ref(), "BEGIN").await.unwrap();
        sql(holder.as_ref(), &format!("SELECT * FROM {rel}"))
            .await
            .unwrap();
        let operation = Uuid::new_v4();
        let entered = tokio::sync::Notify::new();
        let started = std::time::Instant::now();
        let emit = |stage: &str| {
            if stage == "executing" {
                entered.notify_one();
            }
        };
        let apply =
            state
                .sessions
                .object_action_apply(p.plan_id, true, p.confirm_name, operation, &emit);
        let cancel = async {
            tokio::time::timeout(std::time::Duration::from_secs(10), entered.notified())
                .await
                .unwrap();
            tokio::time::sleep(std::time::Duration::from_millis(150)).await;
            state
                .sessions
                .object_action_cancel(operation)
                .await
                .unwrap();
        };
        let (report, ()) = tokio::join!(apply, cancel);
        // Always release our lock and remove only this test's table before assertions.
        sql(holder.as_ref(), "ROLLBACK").await.unwrap();
        let still_exists = sql(holder.as_ref(), &format!("SELECT * FROM {rel}"))
            .await
            .is_ok();
        sql(holder.as_ref(), &format!("DROP TABLE {rel}"))
            .await
            .unwrap();
        holder.close().await;
        state.sessions.close(id).await;
        let r = report.unwrap();
        assert_ne!(r.outcome, ActionOutcome::Applied, "{target:?}");
        assert!(still_exists);
        assert!(
            started.elapsed() < std::time::Duration::from_secs(10),
            "Stop did not promptly cancel the lock wait: {target:?}"
        );
    }
}

#[tokio::test]
#[ignore = "manual local PostgreSQL performance observation; requires DNT_TEST_DB=1"]
async fn review_probe_result_budget() {
    require_db!();
    let session = PgDriver
        .connect(&Target::Pg17.resolved(Some(FIXTURE_DB)))
        .await
        .unwrap();
    for text in [
        "SELECT i, pg_sleep(0.003) FROM generate_series(1, 200) i",
        "SELECT i, pg_sleep(0.003) FROM generate_series(1, 200) i LIMIT 1",
    ] {
        let started = std::time::Instant::now();
        let mut sink = Collect::default();
        let s = session.sql().unwrap();
        s.execute_one(
            text,
            classify(text, s.dialect()),
            &ExecOptions {
                max_rows: 1,
                ..ExecOptions::default()
            },
            &mut sink,
        )
        .await
        .unwrap();
        println!(
            "REVIEW row limit: {} ms, {} delivered row(s): {text}",
            started.elapsed().as_millis(),
            sink.0.len()
        );
        assert_eq!(sink.0.len(), 1);
    }
    let mut sink = Collect::default();
    let text = "SELECT repeat('x', 131072) FROM generate_series(1, 500)";
    let s = session.sql().unwrap();
    s.execute_one(
        text,
        classify(text, s.dialect()),
        &ExecOptions {
            max_rows: 500,
            batch_size: 500,
            ..ExecOptions::default()
        },
        &mut sink,
    )
    .await
    .unwrap();
    let bytes: usize = sink
        .0
        .iter()
        .flat_map(|r| r.iter())
        .map(|v| match v {
            Value::Text(t) => t.len(),
            _ => 0,
        })
        .sum();
    println!(
        "REVIEW query preview: {} rows, {} text bytes retained",
        sink.0.len(),
        bytes
    );
    assert_eq!(bytes, 65_536_000);
    session.close().await;
}

#[tokio::test]
async fn a_dropped_target_is_reported_missing_at_preview_and_apply() {
    require_db!();
    for target in [Target::MySql8, Target::Pg17] {
        let dir = tempfile::tempdir().unwrap();
        let state = app_state(dir.path());
        let id = open(&state, target).await;
        let driver: Box<dyn Driver> = if target.kind() == DbKind::MySql {
            Box::new(MySqlDriver)
        } else {
            Box::new(PgDriver)
        };
        let session = driver
            .connect(&target.resolved(Some(FIXTURE_DB)))
            .await
            .unwrap();
        let pg = target.kind() == DbKind::Postgres;
        let schema = pg.then(|| "public".to_string());
        let table = format!("gone_{}", &Uuid::new_v4().simple().to_string()[..8]);
        let rel = qualified_name(
            target.kind().as_sql(),
            FIXTURE_DB,
            schema.as_deref(),
            &table,
        );
        let object = ObjectRef {
            database: FIXTURE_DB.into(),
            schema,
            name: table.clone(),
            kind: ObjectKind::Table,
        };
        let missing = |e: dnt_core::error::DbError| {
            let e = dnt_core::error::AppError::from(e);
            assert_eq!(e.kind, "config", "{target:?}: {e:?}");
            assert_eq!(e.field.as_deref(), Some("object_missing"), "{target:?}");
            assert!(e.message.contains("no longer exists"), "{}", e.message);
        };
        sql(session.as_ref(), &format!("CREATE TABLE {rel} (id INT)"))
            .await
            .unwrap();
        for a in [
            ObjectAction::DropObject {
                object: object.clone(),
            },
            ObjectAction::RenameTable {
                object: object.clone(),
                name: format!("{table}_x"),
            },
            ObjectAction::TruncateTable {
                object: object.clone(),
            },
        ] {
            // Dropped between Preview and Apply: nothing is sent, the error says why.
            let p = state
                .sessions
                .object_action_preview(id, a.clone())
                .await
                .unwrap();
            sql(session.as_ref(), &format!("DROP TABLE {rel}"))
                .await
                .unwrap();
            let applied = state
                .sessions
                .object_action_apply(
                    p.plan_id,
                    true,
                    p.confirm_name.clone(),
                    Uuid::new_v4(),
                    &|_| {},
                )
                .await;
            missing(applied.unwrap_err());
            // Already gone at Preview.
            missing(
                state
                    .sessions
                    .object_action_preview(id, a)
                    .await
                    .unwrap_err(),
            );
            sql(session.as_ref(), &format!("CREATE TABLE {rel} (id INT)"))
                .await
                .unwrap();
        }
        let _ = sql(session.as_ref(), &format!("DROP TABLE IF EXISTS {rel}")).await;
        session.close().await;
        state.sessions.close(id).await;
    }
}
