//! The real MongoDB driver registered in the app (`default_drivers()`), end to end through
//! `AppState`: the engine list and command language need no server; the rest runs against
//! the compose services `mongo60` / `mongo80` (and `ssh`) with `DNT_TEST_DB=1`.

use dnt_core::config::{ConnOptions, ConnectionConfig, DriverOptions, EnvTag, SecretMode, SshAuth};
use dnt_core::error::DbError;
use dnt_core::meta::{DatabaseInfo, ObjectKind, ObjectRef};
use dnt_core::secret::MemorySecretStore;
use dnt_core::server::DbKind;
use dnt_core::value::Value;
use dnt_query::edit::{CellInput, RowEdit, RowRef};
use dnt_query::{StmtKindLabel, Target as RunTarget, Verdict};
use dnt_store::history::{HistoryEntry, HistoryFilter, HistoryOrigin, HistoryStatus};
use dnt_store::repo::NewSecrets;
use dnt_testkit::{require_db, ssh_config, Target, FIXTURE_DB, TEST_PASSWORD};
use dntdatabase::commands::browse::list_databases_impl;
use dntdatabase::commands::connections::{
    available_engines_impl, close_connection_impl, open_connection_impl, trust_host_key_impl,
};
use dntdatabase::commands::data::{
    data_apply_impl, data_editable_impl, data_preview_impl, table_data_impl, Atomicity, EditSet,
};
use dntdatabase::commands::query::query_plan_impl;
use dntdatabase::edit_plans::ApplyOutcome;
use dntdatabase::query_sessions::PlanView;
use dntdatabase::run_events::{EventSink, RunOptions};
use dntdatabase::state::{AppPaths, AppState};
use secrecy::SecretString;
use serde_json::Value as Json;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use uuid::Uuid;

/// The scratch database of the end-to-end case (dropped and filled again each run).
const SCRATCH: &str = "dnt_app_e2e";

fn app_state(dir: &std::path::Path) -> AppState {
    AppState::open(
        AppPaths::in_dir(dir.join("data")),
        Arc::new(MemorySecretStore::default()),
    )
    .unwrap()
}

/// A saved connection to `t` as `root` (auth source `admin`), tagged `env`.
fn mongo_config(t: Target, env: Option<EnvTag>) -> ConnectionConfig {
    ConnectionConfig {
        id: Uuid::nil(),
        group_id: None,
        name: format!("{t:?}"),
        sort_order: 0,
        kind: DbKind::Mongo,
        host: "127.0.0.1".into(),
        port: t.host_port(),
        username: t.admin_user().into(),
        default_db: Some(FIXTURE_DB.into()),
        env_tag: env,
        read_only: false,
        secret_mode: SecretMode::Saved,
        ssh: None,
        ssl: None,
        options: ConnOptions {
            driver: DriverOptions::Mongo {
                srv: false,
                auth_source: Some("admin".into()),
                replica_set: None,
                direct_connection: false,
                extra_hosts: vec![],
            },
            ..ConnOptions::default()
        },
    }
}

fn db_password() -> NewSecrets {
    NewSecrets {
        db_password: Some(SecretString::from(TEST_PASSWORD)),
        ssh_password: None,
        ssh_key_passphrase: None,
    }
}

/// Saves and opens `cfg`.
async fn open(state: &AppState, cfg: ConnectionConfig) -> Uuid {
    let id = state.repo.create(cfg, db_password()).unwrap().id;
    open_connection_impl(state, id, None).await.unwrap();
    id
}

async fn plan(state: &AppState, id: Uuid, db: &str, text: &str) -> PlanView {
    query_plan_impl(
        state,
        "tab".into(),
        id,
        Some(db.into()),
        text.into(),
        RunTarget::All,
    )
    .await
    .unwrap_or_else(|e| panic!("{text}: {e:?}"))
}

/// Collects a run's events as JSON (the wire shape).
#[derive(Default)]
struct Events(Mutex<Vec<Json>>);

impl Events {
    fn of(&self, tag: &str) -> Vec<Json> {
        let list = self.0.lock().unwrap();
        list.iter().filter(|e| e["t"] == tag).cloned().collect()
    }

    async fn done(&self) -> Json {
        tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                if let Some(done) = self.of("done").pop() {
                    return done;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("no done event")
    }
}

/// Runs plan `view`, confirmed, with `names` typed: its events once `done` came.
async fn run(state: &AppState, view: &PlanView, names: &[&str]) -> Result<Arc<Events>, DbError> {
    let events = Arc::new(Events::default());
    let out = Arc::clone(&events);
    let emit: EventSink = Arc::new(move |e| {
        out.0.lock().unwrap().push(serde_json::to_value(e).unwrap());
        true
    });
    let names = names.iter().map(|s| s.to_string()).collect();
    let opts = RunOptions {
        max_rows: 10_000,
        continue_on_error: false,
    };
    state
        .sessions
        .query_run_named(view.plan_id, true, names, opts, emit)
        .await?;
    events.done().await;
    Ok(events)
}

/// Plans and runs `text` on `db`, which must end without a stop.
async fn run_ok(state: &AppState, id: Uuid, db: &str, text: &str) -> Arc<Events> {
    let view = plan(state, id, db, text).await;
    let events = run(state, &view, &[]).await.unwrap();
    let done = events.done().await;
    assert_eq!(done["stop"], Json::Null, "{text}: {done}");
    events
}

fn history(state: &AppState, id: Uuid) -> Vec<HistoryEntry> {
    let mut rows = state
        .repo
        .history_list(&HistoryFilter {
            connection_id: Some(id),
            search: None,
            origin: None,
            limit: 1000,
        })
        .unwrap();
    rows.sort_by_key(|r| r.id);
    rows
}

fn names(dbs: Vec<DatabaseInfo>) -> Vec<String> {
    dbs.into_iter().map(|d| d.name).collect()
}

fn collection(db: &str, name: &str) -> ObjectRef {
    ObjectRef {
        database: db.into(),
        schema: None,
        name: name.into(),
        kind: ObjectKind::Collection,
    }
}

#[test]
fn available_engines_include_mongo() {
    let dir = tempfile::tempdir().unwrap();
    let state = app_state(dir.path());
    assert_eq!(
        available_engines_impl(&state),
        [DbKind::MySql, DbKind::Postgres, DbKind::Mongo]
    );
}

#[test]
fn language_for_mongo_splits_and_redacts() {
    let dir = tempfile::tempdir().unwrap();
    let state = app_state(dir.path());
    let lang = state
        .sessions
        .language_for(DbKind::Mongo)
        .expect("Mongo has a language");
    let script = "use shop\ndb.orders.find({})";
    let spans = lang.split(script);
    let texts: Vec<&str> = spans.iter().map(|s| s.text.as_str()).collect();
    assert_eq!(texts, ["use shop", "db.orders.find({})"]);
    assert_eq!(spans[1].range, (9, script.len()));
    let r = lang.redact("db.runCommand({createUser: 'u', pwd: 'hunter2', roles: []})");
    assert_eq!(
        r.text,
        "db.runCommand({createUser: 'u', pwd: ***, roles: []})"
    );
    assert!(r.changed);
}

#[tokio::test]
async fn mongo_end_to_end() {
    require_db!();
    let dir = tempfile::tempdir().unwrap();
    let state = app_state(dir.path());
    let id = open(&state, mongo_config(Target::Mongo80, Some(EnvTag::Local))).await;

    // The tree: system databases only on request.
    let user = names(list_databases_impl(&state, id, false).await.unwrap());
    assert!(user.iter().any(|n| n == FIXTURE_DB), "{user:?}");
    for system in ["admin", "local", "config"] {
        assert!(!user.iter().any(|n| n == system), "{user:?}");
    }
    let all = names(list_databases_impl(&state, id, true).await.unwrap());
    assert!(all.iter().any(|n| n == "admin"), "{all:?}");

    // The console: a guarded plan, then a run whose rows are in the result store.
    let view = plan(&state, id, FIXTURE_DB, "db.orders.find({})").await;
    assert_eq!(view.stmts.len(), 1);
    assert_eq!(view.stmts[0].kind, StmtKindLabel::Read);
    assert_eq!(view.stmts[0].verdict, Verdict::Allow);
    let events = run(&state, &view, &[]).await.unwrap();
    assert_eq!(events.done().await["stop"], Json::Null);
    let end = &events.of("result_end")[0];
    assert_eq!(end["row_count"], 5, "{end}");
    let token: Uuid = events.of("result_stored")[0]["token"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    let stored = state.sessions.results().page(token, 0, 10).unwrap();
    assert_eq!(stored.rows.len(), 5);
    let columns = &events.of("result_start")[0]["columns"];
    let columns: Vec<&str> = columns
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["name"].as_str().unwrap())
        .collect();
    assert_eq!(columns.first(), Some(&"_id"), "{columns:?}");
    assert!(columns.contains(&"customer"), "{columns:?}");

    // A scratch collection for the data view (the fixture stays as seeded).
    run_ok(&state, id, SCRATCH, "db.dropDatabase()").await;
    run_ok(
        &state,
        id,
        SCRATCH,
        "db.items.insertOne({_id: 1, name: 'before'})",
    )
    .await;

    // The data view: one cell edited, applied as one transaction (`execute_atomic`).
    let items = collection(SCRATCH, "items");
    let page = || {
        table_data_impl(
            &state,
            id,
            items.clone(),
            100,
            0,
            vec![],
            None,
            "tab-1:page".into(),
        )
    };
    let before = page().await.unwrap();
    let col = |name: &str| {
        before
            .columns
            .iter()
            .position(|c| c.name == name)
            .unwrap_or_else(|| panic!("no {name} in {:?}", before.columns))
    };
    let (id_col, name_col) = (col("_id"), col("name"));
    assert_eq!(before.rows.len(), 1);
    assert_eq!(before.rows[0][name_col], Value::Text("before".into()));

    let info = data_editable_impl(&state, id, items.clone()).await.unwrap();
    assert!(info.editable, "{info:?}");
    assert_eq!(info.key, ["_id"]);
    assert_eq!(info.atomic, Atomicity::Yes, "mongo80 has transactions");

    let set = EditSet {
        object: items.clone(),
        edits: vec![RowEdit::Update {
            row: RowRef {
                key: vec![("_id".into(), before.rows[0][id_col].clone())],
                original: vec![("name".into(), before.rows[0][name_col].clone())],
                xmin: None,
                loaded: vec![],
            },
            set: vec![(
                "name".into(),
                CellInput::Text {
                    text: "after".into(),
                },
            )],
        }],
        overwrite: false,
    };
    let preview = data_preview_impl(&state, id, set).await.unwrap();
    assert_eq!(preview.verdict, Verdict::Allow, "{preview:?}");
    assert_eq!(preview.atomic, Atomicity::Yes);
    assert_eq!(preview.statements.len(), 1, "{preview:?}");
    let report = data_apply_impl(&state, preview.plan_id, false)
        .await
        .unwrap();
    assert_eq!(report.outcome, ApplyOutcome::Committed, "{report:?}");
    assert!(report.error.is_none(), "{report:?}");
    assert_eq!(report.statements[0].affected, Some(1.0), "{report:?}");
    let after = page().await.unwrap();
    assert_eq!(after.rows[0][name_col], Value::Text("after".into()));

    // A password sent through `runCommand` is redacted in the History. Users outlive
    // `dropDatabase`: one a failed earlier run left is dropped first (an error if none).
    let drop_user = r#"db.runCommand({dropUser: "u"})"#;
    let view = plan(&state, id, SCRATCH, drop_user).await;
    run(&state, &view, &[]).await.unwrap();
    let create = r#"db.runCommand({createUser: "u", pwd: "p", roles: []})"#;
    run_ok(&state, id, SCRATCH, create).await;
    run_ok(&state, id, SCRATCH, drop_user).await;

    let rows = history(&state, id);
    let edit = rows
        .iter()
        .find(|r| r.origin == HistoryOrigin::DataEdit)
        .expect("the data-view edit is in the History");
    assert_eq!(edit.status, HistoryStatus::Ok);
    assert_eq!(edit.database.as_deref(), Some(SCRATCH));
    let created: Vec<&HistoryEntry> = rows
        .iter()
        .filter(|r| r.sql_text.contains("createUser"))
        .collect();
    assert_eq!(created.len(), 1, "{rows:?}");
    assert_eq!(
        created[0].sql_text,
        r#"db.runCommand({createUser: "u", pwd: ***, roles: []})"#
    );
    assert!(created[0].sql_text.contains("***"));
    assert!(created[0].redacted);
    assert_eq!(created[0].status, HistoryStatus::Ok);

    close_connection_impl(&state, id).await.unwrap();
}

#[tokio::test]
async fn guard_names_admin_for_admin_drop() {
    require_db!();
    let dir = tempfile::tempdir().unwrap();
    let state = app_state(dir.path());
    let id = open(
        &state,
        mongo_config(Target::Mongo80, Some(EnvTag::Production)),
    )
    .await;
    // Planned only: never run.
    let view = plan(&state, id, FIXTURE_DB, "db.adminCommand({dropDatabase: 1})").await;
    assert_eq!(view.stmts[0].verdict, Verdict::Confirm);
    assert_eq!(view.stmts[0].confirm_name.as_deref(), Some("admin"));
    close_connection_impl(&state, id).await.unwrap();
}

#[tokio::test]
async fn update_many_empty_filter_needs_name_on_production() {
    require_db!();
    let dir = tempfile::tempdir().unwrap();
    let state = app_state(dir.path());
    let id = open(
        &state,
        mongo_config(Target::Mongo80, Some(EnvTag::Production)),
    )
    .await;
    let view = plan(
        &state,
        id,
        FIXTURE_DB,
        "db.orders.updateMany({}, {$set: {dnt_touched: true}})",
    )
    .await;
    assert_eq!(view.stmts[0].kind, StmtKindLabel::Write);
    assert_eq!(view.stmts[0].verdict, Verdict::Confirm);
    assert_eq!(view.stmts[0].confirm_name.as_deref(), Some("orders"));

    // Confirmed without the name (or with another one): refused, nothing is sent.
    for typed in [&[][..], &["dnt_fixture"][..]] {
        match run(&state, &view, typed).await {
            Err(DbError::ConfirmationRequired { count: 1 }) => {}
            Err(e) => panic!("{typed:?}: expected ConfirmationRequired, got {e:?}"),
            Ok(_) => panic!("{typed:?}: the update ran without its name"),
        }
    }
    let events = run_ok(
        &state,
        id,
        FIXTURE_DB,
        "db.orders.find({dnt_touched: true})",
    )
    .await;
    assert_eq!(events.of("result_end")[0]["row_count"], 0);
    close_connection_impl(&state, id).await.unwrap();
}

#[tokio::test]
async fn mongo_through_ssh_tunnel() {
    require_db!();
    let dir = tempfile::tempdir().unwrap();
    let state = app_state(dir.path());
    let (ssh, ssh_secrets) = ssh_config(SshAuth::Password);
    let (host, port) = Target::Mongo60.ssh_target_host();
    let cfg = ConnectionConfig {
        host: host.into(),
        port,
        ssh: Some(ssh),
        ..mongo_config(Target::Mongo60, None)
    };
    let id = state
        .repo
        .create(
            cfg,
            NewSecrets {
                db_password: Some(SecretString::from(TEST_PASSWORD)),
                ..ssh_secrets
            },
        )
        .unwrap()
        .id;

    // A fresh known_hosts does not trust the bastion yet: trust it, then open again.
    match open_connection_impl(&state, id, None)
        .await
        .map_err(DbError::into_unstaged)
    {
        Err(DbError::SshHostKeyUnknown {
            host,
            port,
            fingerprint,
            key_openssh,
        }) => trust_host_key_impl(&state, host, port, fingerprint, key_openssh)
            .await
            .unwrap(),
        other => panic!("expected SshHostKeyUnknown, got {other:?}"),
    }
    open_connection_impl(&state, id, None).await.unwrap();

    let events = run_ok(&state, id, FIXTURE_DB, "db.runCommand({ping: 1})").await;
    assert_eq!(events.of("statement_start").len(), 1);
    assert!(events.of("statement_error").is_empty());
    let dbs = names(list_databases_impl(&state, id, false).await.unwrap());
    assert!(dbs.iter().any(|n| n == FIXTURE_DB), "{dbs:?}");
    close_connection_impl(&state, id).await.unwrap();
}

/// Discards a command's result (the failpoint commands of the admin session).
struct Ignore;

impl dnt_core::driver::RowSink for Ignore {
    fn result_start(&mut self, _cols: &[dnt_core::value::ColumnMeta]) {}
    fn rows(&mut self, _batch: Vec<Vec<Value>>) {}
    fn result_end(&mut self, _summary: &dnt_core::driver::ResultSummary) {}
}

/// Runs `text` as root on `t` through the driver itself, outside the app.
async fn as_root(t: Target, text: &str) {
    use dnt_core::command::CommandLanguage;
    use dnt_core::driver::{Driver, ExecOptions};
    let s = dnt_driver_mongo::MongoDriver
        .connect(&t.resolved(Some("admin")))
        .await
        .unwrap_or_else(|e| panic!("{t:?}: {e}"));
    let cmd = dnt_driver_mongo::shell::MongoLanguage
        .parse(text)
        .unwrap_or_else(|e| panic!("{text}: {e:?}"));
    s.command()
        .unwrap()
        .execute_command(&cmd, &ExecOptions::default(), &mut Ignore)
        .await
        .unwrap_or_else(|e| panic!("{t:?}: {text}: {e:?}"));
    s.close().await;
}

/// A query tab whose pooled connection the server dropped (as after an idle spell): the
/// read goes again on a fresh connection, its rows arrive, and the run says it reconnected.
#[tokio::test]
async fn query_tab_read_survives_a_dropped_connection() {
    require_db!();
    const APP: &str = "dnt-app-reconnect";
    for t in Target::mongo() {
        let dir = tempfile::tempdir().unwrap();
        let state = app_state(dir.path());
        let mut cfg = mongo_config(t, Some(EnvTag::Local));
        cfg.options.application_name = Some(APP.into());
        let id = open(&state, cfg).await;
        // The tab's session is connected and has run a command.
        run_ok(&state, id, FIXTURE_DB, "db.orders.find({})").await;

        as_root(
            t,
            &format!(
                "db.adminCommand({{configureFailPoint: 'failCommand', mode: {{times: 1}}, \
                 data: {{failCommands: ['find'], closeConnection: true, appName: '{APP}'}}}})"
            ),
        )
        .await;
        let view = plan(&state, id, FIXTURE_DB, "db.orders.find({})").await;
        let events = run(&state, &view, &[]).await;
        as_root(
            t,
            "db.adminCommand({configureFailPoint: 'failCommand', mode: 'off'})",
        )
        .await;
        let events = events.unwrap_or_else(|e| panic!("{t:?}: {e:?}"));

        let done = events.done().await;
        assert_eq!(done["stop"], Json::Null, "{t:?}: {done}");
        assert!(events.of("statement_error").is_empty(), "{t:?}");
        assert_eq!(events.of("result_end")[0]["row_count"], 5, "{t:?}");
        let notices: Vec<Json> = events
            .of("notice")
            .iter()
            .map(|n| n["message"].clone())
            .collect();
        assert_eq!(
            notices,
            [Json::from(dnt_query::run::reconnect_notice(1))],
            "{t:?}"
        );
        close_connection_impl(&state, id).await.unwrap();
    }
}
