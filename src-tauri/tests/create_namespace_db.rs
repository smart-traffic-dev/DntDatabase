//! "Create database/schema" options (charset, collation, owner) against the Docker matrix.

use dnt_core::config::{ConnOptions, ConnectionConfig, SecretMode};
use dnt_core::driver::{Driver, ExecOptions, ResultSummary, RowSink, Session};
use dnt_core::error::DbError;
use dnt_core::secret::MemorySecretStore;
use dnt_core::server::DbKind;
use dnt_core::sql::{classify, ddl::ObjectAction, ident::quote_ident};
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

#[derive(Default)]
struct Collect(Vec<Vec<Value>>);
impl RowSink for Collect {
    fn result_start(&mut self, _: &[ColumnMeta]) {}
    fn rows(&mut self, rows: Vec<Vec<Value>>) {
        self.0.extend(rows);
    }
    fn result_end(&mut self, _: &ResultSummary) {}
}
async fn texts(s: &dyn Session, text: &str) -> Vec<String> {
    let mut sink = Collect::default();
    s.execute_one(
        text,
        classify(text, s.dialect()),
        &ExecOptions {
            full_bytes: true,
            ..ExecOptions::default()
        },
        &mut sink,
    )
    .await
    .unwrap();
    sink.0
        .into_iter()
        .flatten()
        .map(|v| match v {
            Value::Text(s) => s,
            Value::Bytes { preview, .. } => String::from_utf8(preview).unwrap(),
            other => format!("{other:?}"),
        })
        .collect()
}

#[tokio::test]
async fn create_namespace_with_options_on_every_server() {
    require_db!();
    for target in Target::all() {
        let dir = tempfile::tempdir().unwrap();
        let state = AppState::open(
            AppPaths::in_dir(dir.path().join("data")),
            Arc::new(MemorySecretStore::default()),
        )
        .unwrap();
        let id = open(&state, target).await;
        let pg = target.kind() == DbKind::Postgres;
        let tag = Uuid::new_v4().simple().to_string()[..12].to_string();
        let ns = format!("cns_{tag}");
        // PostgreSQL: a role other than the current one, with a dot in its name.
        let role = format!("cns.owner_{tag}");
        let driver: Box<dyn Driver> = if pg {
            Box::new(PgDriver)
        } else {
            Box::new(MySqlDriver)
        };
        let session = driver
            .connect(&target.resolved(Some(FIXTURE_DB)))
            .await
            .unwrap();
        if pg {
            texts(
                session.as_ref(),
                &format!("CREATE ROLE {}", quote_ident(DbKind::Postgres, &role)),
            )
            .await;
        }
        let options = state
            .sessions
            .namespace_options(id, FIXTURE_DB.into())
            .await
            .unwrap();
        let action = if pg {
            let me = options.current_role.clone().expect("current role");
            assert!(
                options.roles.contains(&me),
                "{target:?}: {:?}",
                options.roles
            );
            assert!(
                options.roles.contains(&role),
                "{target:?}: {:?}",
                options.roles
            );
            assert!(options.charsets.is_empty());
            ObjectAction::CreateNamespace {
                database: FIXTURE_DB.into(),
                name: ns.clone(),
                charset: None,
                collation: None,
                owner: Some(role.clone()),
            }
        } else {
            assert!(options.default_charset.is_some(), "{target:?}");
            assert!(options.charsets.iter().any(|c| c.name == "utf8mb4"));
            // Every charset's default is a listed collation (MariaDB 11: utf8mb4_uca1400_ai_ci).
            for cs in &options.charsets {
                assert!(
                    options
                        .collations
                        .iter()
                        .any(|c| c.name == cs.default_collation && c.charset == cs.name),
                    "{target:?}: {} default {} not listed",
                    cs.name,
                    cs.default_collation
                );
            }
            assert!(options
                .collations
                .iter()
                .any(|c| c.name == "utf8mb4_bin" && c.charset == "utf8mb4"));
            ObjectAction::CreateNamespace {
                database: String::new(),
                name: ns.clone(),
                charset: Some("utf8mb4".into()),
                collation: Some("utf8mb4_bin".into()),
                owner: None,
            }
        };
        assert!(state
            .sessions
            .object_action_render(id, &action)
            .unwrap()
            .contains(if pg {
                "AUTHORIZATION"
            } else {
                "COLLATE utf8mb4_bin"
            }));
        let p = state
            .sessions
            .object_action_preview(id, action.clone())
            .await
            .unwrap();
        let report = state
            .sessions
            .object_action_apply(p.plan_id, true, None, Uuid::new_v4(), &|_| {})
            .await
            .unwrap();
        // A second create names the conflict instead of asking to preview again.
        let again = state
            .sessions
            .object_action_preview(id, action.clone())
            .await;
        // MySQL: a create the server refuses (a collation of another character set)
        // created nothing, and says so.
        let refused = if pg {
            None
        } else {
            let bad = ObjectAction::CreateNamespace {
                database: String::new(),
                name: format!("{ns}_x"),
                charset: Some("latin1".into()),
                collation: Some("utf8mb4_bin".into()),
                owner: None,
            };
            let p = state.sessions.object_action_preview(id, bad).await.unwrap();
            Some(
                state
                    .sessions
                    .object_action_apply(p.plan_id, true, None, Uuid::new_v4(), &|_| {})
                    .await
                    .unwrap(),
            )
        };
        let facts = if pg {
            texts(
                session.as_ref(),
                &format!("SELECT nspowner::regrole::text FROM pg_namespace WHERE nspname = '{ns}'"),
            )
            .await
        } else {
            texts(
                session.as_ref(),
                &format!("SELECT DEFAULT_CHARACTER_SET_NAME, DEFAULT_COLLATION_NAME FROM information_schema.SCHEMATA WHERE SCHEMA_NAME = '{ns}'"),
            )
            .await
        };
        let cleanup = format!(
            "DROP {} IF EXISTS {}",
            if pg { "SCHEMA" } else { "DATABASE" },
            quote_ident(target.kind(), &ns)
        );
        texts(session.as_ref(), &cleanup).await;
        let cleanup = if pg {
            format!(
                "DROP ROLE IF EXISTS {}",
                quote_ident(DbKind::Postgres, &role)
            )
        } else {
            format!(
                "DROP DATABASE IF EXISTS {}",
                quote_ident(DbKind::MySql, &format!("{ns}_x"))
            )
        };
        texts(session.as_ref(), &cleanup).await;
        session.close().await;
        state.sessions.close(id).await;

        assert_eq!(
            report.outcome,
            ActionOutcome::Applied,
            "{target:?}: {report:?}"
        );
        if pg {
            // `regrole::text` quotes a name that needs it.
            assert_eq!(facts, [format!("\"{role}\"")], "{target:?}");
        } else {
            assert_eq!(facts, ["utf8mb4", "utf8mb4_bin"], "{target:?}");
        }
        if let Some(refused) = refused {
            assert_eq!(
                refused.outcome,
                ActionOutcome::NotApplied,
                "{target:?}: {refused:?}"
            );
            let error = refused.error.expect("the server's refusal");
            assert!(!error.outcome_unknown, "{target:?}: {error:?}");
        }
        match again {
            Err(DbError::Config { field, message }) => {
                assert_eq!(field.as_deref(), Some("name"));
                assert!(message.contains("already exists"), "{message}");
            }
            other => panic!("{target:?}: expected a name conflict, got {other:?}"),
        }
    }
}
