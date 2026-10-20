//! Data view edits (`data_preview` / `data_apply`) against the Docker test matrix
//! (`DNT_TEST_DB=1`). Every test writes to its own scratch table `p3_<test>_<uuid8>` in
//! the fixture database and drops it afterwards.
//!
//! All tests of this binary run one at a time ([`DB_LOCK`]): a test may change a
//! server-wide setting (`SET GLOBAL sql_mode`), which must not leak into another.

use dnt_core::config::{ConnOptions, ConnectionConfig, SecretMode};
use dnt_core::driver::{Driver, ExecOptions, ResultSummary, RowSink};
use dnt_core::error::{AppError, DbError};
use dnt_core::meta::{ObjectKind, ObjectRef};
use dnt_core::server::DbKind;
use dnt_core::sql::classify;
use dnt_core::value::{ColumnMeta, Value};
use dnt_driver_mysql::MySqlDriver;
use dnt_driver_pg::PgDriver;
use dnt_query::edit::{base64_decode, Atomicity, CellInput, EditSet, Protection, RowEdit, RowRef};
use dnt_testkit::{require_db, Target, FIXTURE_DB, TEST_PASSWORD};
use dnt_tunnel::KnownHosts;
use dntdatabase::commands::connections::{
    create_connection_impl, open_connection_impl, SecretsInput,
};
use dntdatabase::commands::data::{
    data_apply_impl, data_cell_fetch_impl, data_cell_load_file_impl, data_cell_save_file_impl,
    data_editable_impl, data_preview_impl, table_data_impl, ApplyOutcome, ApplyReport, CellPayload,
    LoadedCell, TableDataPage,
};
use dntdatabase::file_picks::FileAccess;
use dntdatabase::session_manager::SessionManager;
use dntdatabase::state::default_drivers;
use dntdatabase::state::{AppPaths, AppState};
use serde_json::json;
use std::sync::Arc;
use uuid::Uuid;

/// Serialises the tests of this binary (see the module docs).
static DB_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn app_state(dir: &std::path::Path) -> AppState {
    AppState::open(
        AppPaths::in_dir(dir.join("data")),
        Arc::new(dnt_core::secret::MemorySecretStore::default()),
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

fn uuid8() -> String {
    Uuid::new_v4().simple().to_string()[..8].to_string()
}

/// `name` qualified for an admin statement on `t`.
fn scratch(t: Target, name: &str) -> String {
    match t.kind() {
        DbKind::MySql => format!("`{FIXTURE_DB}`.`{name}`"),
        DbKind::Postgres => format!("public.\"{name}\""),
    }
}

fn obj(t: Target, name: &str) -> ObjectRef {
    ObjectRef {
        database: FIXTURE_DB.into(),
        schema: (t.kind() == DbKind::Postgres).then(|| "public".to_string()),
        name: name.into(),
        kind: ObjectKind::Table,
    }
}

struct Collect(Vec<Vec<Value>>);

impl RowSink for Collect {
    fn result_start(&mut self, _cols: &[ColumnMeta]) {}
    fn rows(&mut self, batch: Vec<Vec<Value>>) {
        self.0.extend(batch);
    }
    fn result_end(&mut self, _summary: &ResultSummary) {}
}

/// Runs `sql` (a script) on a fresh admin session of the fixture database; the rows of
/// every result.
async fn admin(t: Target, sql: &str) -> Vec<Vec<Value>> {
    let driver: Box<dyn Driver> = match t.kind() {
        DbKind::MySql => Box::new(MySqlDriver),
        DbKind::Postgres => Box::new(PgDriver),
    };
    let session = driver.connect(&t.resolved(Some(FIXTURE_DB))).await.unwrap();
    let mut sink = Collect(Vec::new());
    for stmt in dnt_core::sql::split(sql, session.dialect()) {
        let kind = classify(&stmt.text, session.dialect());
        session
            .execute_one(&stmt.text, kind, &ExecOptions::default(), &mut sink)
            .await
            .unwrap_or_else(|e| panic!("{}: {e:?}", stmt.text));
    }
    session.close().await;
    sink.0
}

/// Creates the scratch table `p3_<tag>_<uuid8>` with `mysql` / `pg` column definitions
/// and `rows` (an `INSERT … VALUES` tail, or empty); returns its name.
async fn create(t: Target, tag: &str, mysql: &str, pg: &str, rows: &str) -> String {
    let name = format!("p3_{tag}_{}", uuid8());
    let q = scratch(t, &name);
    let ddl = match t.kind() {
        DbKind::MySql => format!("CREATE TABLE {q} ({mysql})"),
        DbKind::Postgres => format!("CREATE TABLE {q} ({pg})"),
    };
    admin(t, &ddl).await;
    if !rows.is_empty() {
        admin(t, &format!("INSERT INTO {q} VALUES {rows}")).await;
    }
    name
}

async fn drop_table(t: Target, name: &str) {
    admin(t, &format!("DROP TABLE {}", scratch(t, name))).await;
}

/// The standard scratch table: `id INT PK, name VARCHAR(50), qty INT`, rows
/// `(1,'a',1), (2,'b',2)`.
async fn items(t: Target, tag: &str) -> String {
    create(
        t,
        tag,
        "id INT NOT NULL PRIMARY KEY, name VARCHAR(50) NULL, qty INT NULL",
        "id INT NOT NULL PRIMARY KEY, name VARCHAR(50), qty INT",
        "(1, 'a', 1), (2, 'b', 2)",
    )
    .await
}

async fn page(state: &AppState, id: Uuid, o: ObjectRef) -> TableDataPage {
    table_data_impl(state, id, o, 100, 0, vec![], None, "tab:page".into())
        .await
        .unwrap()
}

/// Row `i` of `p` as the UI sends it: its key, every loaded cell, its `xmin`.
fn row_ref(p: &TableDataPage, i: usize, key: &[&str]) -> RowRef {
    let cells: Vec<(String, Value)> = p
        .columns
        .iter()
        .zip(&p.rows[i])
        .map(|(c, v)| (c.name.clone(), v.clone()))
        .collect();
    let hidden: Vec<(String, Value)> = p
        .hidden_key
        .iter()
        .zip(p.hidden_key_values.get(i).into_iter().flatten())
        .map(|(c, v)| (c.name.clone(), v.clone()))
        .collect();
    let key = key
        .iter()
        .map(|k| {
            cells
                .iter()
                .chain(&hidden)
                .find(|(n, _)| n == k)
                .cloned()
                .unwrap_or_else(|| panic!("no key column {k}"))
        })
        .collect();
    RowRef {
        key,
        original: cells,
        xmin: p.row_versions.as_ref().map(|v| v[i].clone()),
        loaded: vec![],
    }
}

fn text(s: &str) -> CellInput {
    CellInput::Text { text: s.into() }
}

fn cells(items: &[(&str, CellInput)]) -> Vec<(String, CellInput)> {
    items
        .iter()
        .map(|(n, v)| (n.to_string(), v.clone()))
        .collect()
}

fn set(o: ObjectRef, edits: Vec<RowEdit>) -> EditSet {
    EditSet {
        object: o,
        edits,
        overwrite: false,
    }
}

async fn apply(state: &AppState, id: Uuid, s: EditSet) -> Result<ApplyReport, DbError> {
    let p = data_preview_impl(state, id, s).await?;
    data_apply_impl(state, p.plan_id, false).await
}

fn int(v: &Value) -> i64 {
    match v {
        Value::Int(n) => *n,
        Value::UInt(n) => i64::try_from(*n).unwrap(),
        Value::Decimal(s) | Value::Text(s) => s.parse().unwrap(),
        other => panic!("not an integer: {other:?}"),
    }
}

fn txt(v: &Value) -> String {
    match v {
        Value::Text(s) => s.clone(),
        Value::Null => "NULL".into(),
        other => panic!("not text: {other:?}"),
    }
}

/// `(id, name, qty)` of every row, by id.
async fn contents(t: Target, name: &str) -> Vec<(i64, String, i64)> {
    admin(
        t,
        &format!("SELECT id, name, qty FROM {} ORDER BY id", scratch(t, name)),
    )
    .await
    .iter()
    .map(|r| (int(&r[0]), txt(&r[1]), int(&r[2])))
    .collect()
}

#[tokio::test]
async fn update_insert_delete_round_trip() {
    require_db!();
    let _serial = DB_LOCK.lock().await;
    for t in Target::all() {
        let dir = tempfile::tempdir().unwrap();
        let state = app_state(dir.path());
        let id = open(&state, t).await;
        let name = items(t, "rt").await;
        let o = obj(t, &name);
        let p = page(&state, id, o.clone()).await;
        let s = set(
            o.clone(),
            vec![
                RowEdit::Update {
                    row: row_ref(&p, 0, &["id"]),
                    set: cells(&[("name", text("a'\\x")), ("qty", CellInput::Null)]),
                },
                RowEdit::Delete {
                    row: row_ref(&p, 1, &["id"]),
                },
                RowEdit::Insert {
                    values: cells(&[("id", text("3")), ("name", text("c")), ("qty", text("7"))]),
                },
            ],
        );
        let preview = data_preview_impl(&state, id, s).await.unwrap();
        assert_eq!(preview.atomic, Atomicity::Yes, "{t:?}");
        assert_eq!(preview.statements.len(), 3, "{t:?}");
        let r = data_apply_impl(&state, preview.plan_id, false)
            .await
            .unwrap();
        assert_eq!(r.outcome, ApplyOutcome::Committed, "{t:?}: {r:?}");
        assert!(
            r.statements
                .iter()
                .all(|s| s.affected == Some(1.0) && !s.conflict && s.error.is_none()),
            "{t:?}: {r:?}"
        );
        assert_eq!(r.inserted.len(), 1, "{t:?}");
        assert_eq!(int(&r.inserted[0][0]), 3, "{t:?}");
        assert_eq!(txt(&r.inserted[0][1]), "c", "{t:?}");
        let rows = admin(
            t,
            &format!(
                "SELECT id, name, qty FROM {} ORDER BY id",
                scratch(t, &name)
            ),
        )
        .await;
        assert_eq!(rows.len(), 2, "{t:?}");
        assert_eq!(int(&rows[0][0]), 1);
        // The value went through literal() unchanged (quote and backslash).
        assert_eq!(txt(&rows[0][1]), "a'\\x", "{t:?}");
        assert_eq!(rows[0][2], Value::Null, "{t:?}");
        assert_eq!((int(&rows[1][0]), txt(&rows[1][1])), (3, "c".into()));
        // The plan is gone once applied.
        assert!(matches!(
            data_apply_impl(&state, preview.plan_id, false).await,
            Err(DbError::PlanExpired)
        ));
        drop_table(t, &name).await;
    }
}

/// Another session changes the row after the page was read: the update is a conflict
/// and the whole batch is rolled back.
#[tokio::test]
async fn concurrent_update_is_a_conflict() {
    require_db!();
    let _serial = DB_LOCK.lock().await;
    for t in Target::all() {
        let dir = tempfile::tempdir().unwrap();
        let state = app_state(dir.path());
        let id = open(&state, t).await;
        let name = items(t, "cu").await;
        let o = obj(t, &name);
        let p = page(&state, id, o.clone()).await;
        admin(
            t,
            &format!("UPDATE {} SET qty = 10 WHERE id = 2", scratch(t, &name)),
        )
        .await;
        let r = apply(
            &state,
            id,
            set(
                o,
                vec![
                    RowEdit::Update {
                        row: row_ref(&p, 0, &["id"]),
                        set: cells(&[("name", text("first"))]),
                    },
                    RowEdit::Update {
                        row: row_ref(&p, 1, &["id"]),
                        set: cells(&[("name", text("second"))]),
                    },
                ],
            ),
        )
        .await
        .unwrap();
        assert_eq!(r.outcome, ApplyOutcome::RolledBack, "{t:?}: {r:?}");
        assert_eq!(r.statements.len(), 2, "{t:?}");
        assert!(
            !r.statements[0].conflict && r.statements[1].conflict,
            "{t:?}"
        );
        assert_eq!(r.statements[1].affected, Some(0.0), "{t:?}");
        // Statement 1 was undone with the rest.
        assert_eq!(
            contents(t, &name).await,
            [(1, "a".into(), 1), (2, "b".into(), 10)],
            "{t:?}"
        );
        drop_table(t, &name).await;
    }
}

#[tokio::test]
async fn concurrent_change_then_delete_is_a_conflict() {
    require_db!();
    let _serial = DB_LOCK.lock().await;
    for t in Target::all() {
        let dir = tempfile::tempdir().unwrap();
        let state = app_state(dir.path());
        let id = open(&state, t).await;
        let name = items(t, "cd").await;
        let o = obj(t, &name);
        let p = page(&state, id, o.clone()).await;
        admin(
            t,
            &format!("UPDATE {} SET name = 'z' WHERE id = 1", scratch(t, &name)),
        )
        .await;
        let r = apply(
            &state,
            id,
            set(
                o,
                vec![RowEdit::Delete {
                    row: row_ref(&p, 0, &["id"]),
                }],
            ),
        )
        .await
        .unwrap();
        assert_eq!(r.outcome, ApplyOutcome::RolledBack, "{t:?}: {r:?}");
        assert!(r.statements[0].conflict, "{t:?}");
        assert_eq!(contents(t, &name).await.len(), 2, "{t:?}");
        drop_table(t, &name).await;
    }
}

/// MySQL compares a case-insensitive column with its `_bin` collation: a change of case
/// only is still a conflict.
#[tokio::test]
async fn case_only_change_is_a_conflict_on_ci_column() {
    require_db!();
    let _serial = DB_LOCK.lock().await;
    for t in Target::mysql_family() {
        let dir = tempfile::tempdir().unwrap();
        let state = app_state(dir.path());
        let id = open(&state, t).await;
        let name = create(
            t,
            "ci",
            "id INT NOT NULL PRIMARY KEY, \
             name VARCHAR(50) CHARACTER SET utf8mb4 COLLATE utf8mb4_general_ci NULL, \
             qty INT NULL",
            "",
            "(1, 'anna', 1)",
        )
        .await;
        let o = obj(t, &name);
        let p = page(&state, id, o.clone()).await;
        admin(
            t,
            &format!(
                "UPDATE {} SET name = 'ANNA' WHERE id = 1",
                scratch(t, &name)
            ),
        )
        .await;
        let r = apply(
            &state,
            id,
            set(
                o,
                vec![RowEdit::Update {
                    row: row_ref(&p, 0, &["id"]),
                    set: cells(&[("qty", text("5"))]),
                }],
            ),
        )
        .await
        .unwrap();
        assert_eq!(r.outcome, ApplyOutcome::RolledBack, "{t:?}: {r:?}");
        assert!(r.statements[0].conflict, "{t:?}");
        assert_eq!(contents(t, &name).await, [(1, "ANNA".into(), 1)], "{t:?}");
        drop_table(t, &name).await;
    }
}

/// A key above `i64::MAX` round-trips: read as `UInt`, written as an exact literal.
#[tokio::test]
async fn edit_row_with_unsigned_bigint_pk() {
    require_db!();
    let _serial = DB_LOCK.lock().await;
    for t in Target::mysql_family() {
        let dir = tempfile::tempdir().unwrap();
        let state = app_state(dir.path());
        let id = open(&state, t).await;
        let name = create(
            t,
            "ubig",
            "id BIGINT UNSIGNED NOT NULL PRIMARY KEY, name VARCHAR(50) NULL",
            "",
            "(18446744073709551615, 'max'), (1, 'one')",
        )
        .await;
        let o = obj(t, &name);
        let p = page(&state, id, o.clone()).await;
        assert_eq!(p.rows[1][0], Value::UInt(u64::MAX), "{t:?}");
        let r = apply(
            &state,
            id,
            set(
                o,
                vec![RowEdit::Update {
                    row: row_ref(&p, 1, &["id"]),
                    set: cells(&[("name", text("edited"))]),
                }],
            ),
        )
        .await
        .unwrap();
        assert_eq!(r.outcome, ApplyOutcome::Committed, "{t:?}: {r:?}");
        let rows = admin(
            t,
            &format!(
                "SELECT name FROM {} WHERE id = 18446744073709551615",
                scratch(t, &name)
            ),
        )
        .await;
        assert_eq!(txt(&rows[0][0]), "edited", "{t:?}");
        let rows = admin(
            t,
            &format!("SELECT name FROM {} WHERE id = 1", scratch(t, &name)),
        )
        .await;
        assert_eq!(txt(&rows[0][0]), "one", "{t:?}");
        drop_table(t, &name).await;
    }
}

/// MySQL reports changed rows: an UPDATE that writes the value a row already has
/// reports 0, and must not be taken for a conflict.
#[tokio::test]
async fn update_to_the_same_value_is_not_a_conflict() {
    require_db!();
    let _serial = DB_LOCK.lock().await;
    for t in Target::all() {
        let dir = tempfile::tempdir().unwrap();
        let state = app_state(dir.path());
        let id = open(&state, t).await;
        let name = items(t, "same").await;
        let o = obj(t, &name);
        let p = page(&state, id, o.clone()).await;
        let r = apply(
            &state,
            id,
            set(
                o.clone(),
                vec![
                    RowEdit::Update {
                        row: row_ref(&p, 0, &["id"]),
                        set: cells(&[("qty", text("1"))]),
                    },
                    RowEdit::Update {
                        row: row_ref(&p, 1, &["id"]),
                        set: cells(&[("qty", text("3"))]),
                    },
                ],
            ),
        )
        .await
        .unwrap();
        assert_eq!(r.outcome, ApplyOutcome::Committed, "{t:?}: {r:?}");
        assert!(r.statements.iter().all(|s| !s.conflict), "{t:?}: {r:?}");
        assert_eq!(
            contents(t, &name).await,
            [(1, "a".into(), 1), (2, "b".into(), 3)],
            "{t:?}"
        );
        drop_table(t, &name).await;
    }
}

/// A concurrent commit lands inside the batch, after an earlier recheck read the table:
/// the recheck of the next UPDATE must see that commit (a locking read), not the
/// transaction's snapshot, or the batch would commit and report an edit applied that was
/// never written.
///
/// Row 2 is held by another transaction; statement 1 writes row 1's own value (0
/// changed rows: rechecked, which on InnoDB would start the snapshot); statement 2 waits
/// for row 2's lock; the other transaction then changes row 2 and commits.
#[tokio::test]
async fn recheck_sees_a_commit_made_during_the_batch() {
    require_db!();
    let _serial = DB_LOCK.lock().await;
    for t in Target::mysql_family() {
        let dir = tempfile::tempdir().unwrap();
        let state = app_state(dir.path());
        let id = open(&state, t).await;
        let name = items(t, "snap").await;
        let q = scratch(t, &name);
        let o = obj(t, &name);
        let p = page(&state, id, o.clone()).await;
        // The other transaction holds row 2.
        let other = MySqlDriver
            .connect(&t.resolved(Some(FIXTURE_DB)))
            .await
            .unwrap();
        let run = |sql: String| {
            let other = &other;
            async move {
                let kind = classify(&sql, other.dialect());
                other
                    .execute_one(&sql, kind, &ExecOptions::default(), &mut Collect(vec![]))
                    .await
                    .unwrap_or_else(|e| panic!("{sql}: {e:?}"));
            }
        };
        run("START TRANSACTION".into()).await;
        run(format!("SELECT * FROM {q} WHERE id = 2 FOR UPDATE")).await;
        let edits = set(
            o,
            vec![
                RowEdit::Update {
                    row: row_ref(&p, 0, &["id"]),
                    set: cells(&[("qty", text("1"))]),
                },
                RowEdit::Update {
                    row: row_ref(&p, 1, &["id"]),
                    set: cells(&[("name", text("mine"))]),
                },
            ],
        );
        let waiting = format!(
            "SELECT COUNT(*) FROM information_schema.processlist \
             WHERE info LIKE 'UPDATE {q} SET `name`%'"
        );
        let (r, ()) = tokio::join!(apply(&state, id, edits), async {
            // Until statement 2 waits for the lock.
            let mut blocked = false;
            for _ in 0..100 {
                if int(&admin(t, &waiting).await[0][0]) > 0 {
                    blocked = true;
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
            assert!(blocked, "{t:?}: statement 2 never waited for the lock");
            run(format!("UPDATE {q} SET qty = 99 WHERE id = 2")).await;
            run("COMMIT".into()).await;
        });
        other.close().await;
        let r = r.unwrap();
        assert_eq!(r.outcome, ApplyOutcome::RolledBack, "{t:?}: {r:?}");
        assert!(
            !r.statements[0].conflict && r.statements[1].conflict,
            "{t:?}: {r:?}"
        );
        assert_eq!(
            contents(t, &name).await,
            [(1, "a".into(), 1), (2, "b".into(), 99)],
            "{t:?}"
        );
        drop_table(t, &name).await;
    }
}

/// A non-transactional table: one change at a time, no transaction, and a statement
/// that applied nothing is reported as such (statements before it would stay).
#[tokio::test]
async fn myisam_apply_is_one_statement_and_partial_is_reported() {
    require_db!();
    let _serial = DB_LOCK.lock().await;
    for t in Target::mysql_family() {
        let dir = tempfile::tempdir().unwrap();
        let state = app_state(dir.path());
        let id = open(&state, t).await;
        let name = create(
            t,
            "myisam",
            "id INT NOT NULL PRIMARY KEY, name VARCHAR(50) NULL, qty INT NULL",
            "",
            "(1, 'a', 1), (2, 'b', 2)",
        )
        .await;
        admin(
            t,
            &format!("ALTER TABLE {} ENGINE=MyISAM", scratch(t, &name)),
        )
        .await;
        let o = obj(t, &name);
        let p = page(&state, id, o.clone()).await;
        let two = set(
            o.clone(),
            vec![
                RowEdit::Delete {
                    row: row_ref(&p, 0, &["id"]),
                },
                RowEdit::Delete {
                    row: row_ref(&p, 1, &["id"]),
                },
            ],
        );
        match data_preview_impl(&state, id, two).await.unwrap_err() {
            DbError::Config { field, .. } => {
                assert_eq!(field.as_deref(), Some("one_at_a_time"), "{t:?}")
            }
            other => panic!("{t:?}: {other:?}"),
        }
        // One change: applied without a transaction.
        let one = set(
            o.clone(),
            vec![RowEdit::Update {
                row: row_ref(&p, 0, &["id"]),
                set: cells(&[("qty", text("9"))]),
            }],
        );
        let preview = data_preview_impl(&state, id, one).await.unwrap();
        assert_eq!(preview.atomic, Atomicity::No, "{t:?}");
        let r = data_apply_impl(&state, preview.plan_id, false)
            .await
            .unwrap();
        assert_eq!(r.outcome, ApplyOutcome::Committed, "{t:?}: {r:?}");
        // Changed meanwhile: nothing applied, reported as a conflict.
        admin(
            t,
            &format!("UPDATE {} SET qty = 20 WHERE id = 2", scratch(t, &name)),
        )
        .await;
        let r = apply(
            &state,
            id,
            set(
                o.clone(),
                vec![RowEdit::Update {
                    row: row_ref(&p, 1, &["id"]),
                    set: cells(&[("name", text("x"))]),
                }],
            ),
        )
        .await
        .unwrap();
        assert_eq!(
            r.outcome,
            ApplyOutcome::PartiallyApplied { applied: 0 },
            "{t:?}: {r:?}"
        );
        assert!(r.statements[0].conflict, "{t:?}");
        // A statement the server refuses (duplicate key): its error, nothing applied.
        let r = apply(
            &state,
            id,
            set(
                o,
                vec![RowEdit::Insert {
                    values: cells(&[("id", text("1")), ("name", text("dup"))]),
                }],
            ),
        )
        .await
        .unwrap();
        assert_eq!(
            r.outcome,
            ApplyOutcome::PartiallyApplied { applied: 0 },
            "{t:?}: {r:?}"
        );
        let err: &AppError = r.statements[0].error.as_ref().expect("an error");
        assert_eq!(err.kind, "server", "{t:?}");
        assert_eq!(
            contents(t, &name).await,
            [(1, "a".into(), 9), (2, "b".into(), 20)],
            "{t:?}"
        );
        drop_table(t, &name).await;
    }
}

/// MySQL ≥ 8.0.30 with `sql_generate_invisible_primary_key`: the generated invisible
/// `my_row_id` key is read and used like any other key.
#[tokio::test]
async fn gipk_table_is_editable() {
    require_db!();
    let _serial = DB_LOCK.lock().await;
    let t = Target::MySql8;
    let version = admin(t, "SELECT VERSION()").await;
    let v = txt(&version[0][0]);
    let parts: Vec<u32> = v
        .split(['.', '-'])
        .take(3)
        .map(|p| p.parse().unwrap_or(0))
        .collect();
    if (parts[0], parts[1], parts[2]) < (8, 0, 30) {
        eprintln!("skipped: MySQL {v} has no generated invisible primary keys");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let state = app_state(dir.path());
    let id = open(&state, t).await;
    let name = format!("p3_gipk_{}", uuid8());
    admin(
        t,
        &format!(
            "SET SESSION sql_generate_invisible_primary_key = ON; \
             CREATE TABLE {} (name VARCHAR(50) NULL); \
             INSERT INTO {} (name) VALUES ('a'), ('b')",
            scratch(t, &name),
            scratch(t, &name)
        ),
    )
    .await;
    let o = obj(t, &name);
    let info = data_editable_impl(&state, id, o.clone()).await.unwrap();
    assert!(info.editable, "{info:?}");
    assert_eq!(info.key, ["my_row_id"]);
    let p = page(&state, id, o.clone()).await;
    assert_eq!(p.hidden_key.len(), 1);
    assert_eq!(p.columns.len(), 1);
    let r = apply(
        &state,
        id,
        set(
            o.clone(),
            vec![
                RowEdit::Update {
                    row: row_ref(&p, 1, &["my_row_id"]),
                    set: cells(&[("name", text("B"))]),
                },
                RowEdit::Insert {
                    values: cells(&[("name", text("c"))]),
                },
            ],
        ),
    )
    .await
    .unwrap();
    assert_eq!(r.outcome, ApplyOutcome::Committed, "{r:?}");
    // Read back by LAST_INSERT_ID(): every column, the invisible key included.
    assert_eq!(r.inserted.len(), 1);
    assert_eq!(r.inserted[0].len(), 2, "{r:?}");
    assert_eq!(int(&r.inserted[0][0]), 3);
    assert_eq!(txt(&r.inserted[0][1]), "c");
    let rows = admin(
        t,
        &format!(
            "SELECT my_row_id, name FROM {} ORDER BY my_row_id",
            scratch(t, &name)
        ),
    )
    .await;
    let got: Vec<(i64, String)> = rows.iter().map(|r| (int(&r[0]), txt(&r[1]))).collect();
    assert_eq!(got, [(1, "a".into()), (2, "B".into()), (3, "c".into())]);
    drop_table(t, &name).await;
}

/// The server-assigned key of a new row comes back: MySQL `AUTO_INCREMENT` (by
/// `LAST_INSERT_ID()`), PostgreSQL identity (`RETURNING`).
#[tokio::test]
async fn inserted_row_is_read_back() {
    require_db!();
    let _serial = DB_LOCK.lock().await;
    for t in Target::all() {
        let dir = tempfile::tempdir().unwrap();
        let state = app_state(dir.path());
        let id = open(&state, t).await;
        let name = create(
            t,
            "ins",
            "id INT NOT NULL AUTO_INCREMENT PRIMARY KEY, name VARCHAR(50) NULL, \
             qty INT NOT NULL DEFAULT 42",
            "id INT GENERATED BY DEFAULT AS IDENTITY PRIMARY KEY, name VARCHAR(50), \
             qty INT NOT NULL DEFAULT 42",
            "",
        )
        .await;
        admin(
            t,
            &format!(
                "INSERT INTO {} (name, qty) VALUES ('seed', 1)",
                scratch(t, &name)
            ),
        )
        .await;
        let o = obj(t, &name);
        let r = apply(
            &state,
            id,
            set(
                o.clone(),
                vec![RowEdit::Insert {
                    values: cells(&[("id", CellInput::Default), ("name", text("new"))]),
                }],
            ),
        )
        .await
        .unwrap();
        assert_eq!(r.outcome, ApplyOutcome::Committed, "{t:?}: {r:?}");
        assert_eq!(r.inserted.len(), 1, "{t:?}");
        let row = &r.inserted[0];
        assert_eq!(row.len(), 3, "{t:?}: {row:?}");
        assert_eq!(int(&row[0]), 2, "{t:?}");
        assert_eq!(txt(&row[1]), "new", "{t:?}");
        // The column default the server filled in.
        assert_eq!(int(&row[2]), 42, "{t:?}");
        // A typed key is read back by its value.
        let r = apply(
            &state,
            id,
            set(
                o,
                vec![RowEdit::Insert {
                    values: cells(&[("id", text("10")), ("name", text("typed"))]),
                }],
            ),
        )
        .await
        .unwrap();
        assert_eq!(r.outcome, ApplyOutcome::Committed, "{t:?}: {r:?}");
        assert_eq!(int(&r.inserted[0][0]), 10, "{t:?}");
        assert_eq!(txt(&r.inserted[0][1]), "typed", "{t:?}");
        drop_table(t, &name).await;
    }
}

// ---- complete cell values (`data_cell_fetch`, load / save file) ------------------------

const MIB: usize = 1024 * 1024;

/// 1 MiB of the bytes 0..=255, repeated.
fn mib() -> Vec<u8> {
    (0..MIB).map(|i| (i % 256) as u8).collect()
}

/// `blobs`: `id INT PK, data` (binary) `, note` (long text) `, qty INT`; row 1 holds
/// [`mib`] and 100 000 `n`s, row 2 NULL and `short`.
async fn blobs(t: Target, tag: &str) -> String {
    let name = create(
        t,
        tag,
        "id INT NOT NULL PRIMARY KEY, data LONGBLOB NULL, note LONGTEXT NULL, qty INT NULL",
        "id INT NOT NULL PRIMARY KEY, data BYTEA, note TEXT, qty INT",
        "",
    )
    .await;
    let hex: String = (0..=255u8).map(|b| format!("{b:02x}")).collect();
    let blob = match t.kind() {
        DbKind::MySql => format!("UNHEX(REPEAT('{hex}', 4096))"),
        DbKind::Postgres => format!("decode(repeat('{hex}', 4096), 'hex')"),
    };
    admin(
        t,
        &format!(
            "INSERT INTO {} VALUES (1, {blob}, REPEAT('n', 100000), 1), (2, NULL, 'short', 2)",
            scratch(t, &name)
        ),
    )
    .await;
    name
}

fn bytes_of(cell: &LoadedCell) -> Vec<u8> {
    match &cell.payload {
        CellPayload::Base64 { data, len } => {
            let b = base64_decode(data).expect("valid base64");
            assert_eq!(*len, b.len() as f64);
            b
        }
        other => panic!("not binary: {other:?}"),
    }
}

fn config_field(e: &DbError) -> String {
    match e {
        DbError::Config { field, .. } => field.clone().unwrap_or_default(),
        other => panic!("not a config error: {other:?}"),
    }
}

async fn fetch(state: &AppState, id: Uuid, o: &ObjectRef, row: &RowRef, col: &str) -> LoadedCell {
    data_cell_fetch_impl(state, id, o.clone(), row.key.clone(), col.into())
        .await
        .unwrap_or_else(|e| panic!("fetch {col}: {e:?}"))
}

#[tokio::test]
async fn fetch_full_blob_by_key() {
    require_db!();
    let _serial = DB_LOCK.lock().await;
    for t in Target::all() {
        let dir = tempfile::tempdir().unwrap();
        let state = app_state(dir.path());
        let id = open(&state, t).await;
        let name = blobs(t, "fetch").await;
        let o = obj(t, &name);
        let p = page(&state, id, o.clone()).await;
        let r = row_ref(&p, 0, &["id"]);
        // The page has a preview and cut text only.
        assert!(!r.original[1].1.is_complete(), "{t:?}");
        let cell = fetch(&state, id, &o, &r, "data").await;
        assert!(bytes_of(&cell) == mib(), "{t:?}");
        let cell = fetch(&state, id, &o, &r, "note").await;
        assert_eq!(
            cell.payload,
            CellPayload::Text {
                text: "n".repeat(100_000)
            },
            "{t:?}"
        );
        let cell = fetch(&state, id, &o, &row_ref(&p, 1, &["id"]), "data").await;
        assert_eq!(cell.payload, CellPayload::Null, "{t:?}");
        drop_table(t, &name).await;
    }
}

#[tokio::test]
async fn fetch_over_limit_fails() {
    require_db!();
    let _serial = DB_LOCK.lock().await;
    for t in Target::all() {
        let dir = tempfile::tempdir().unwrap();
        let mut state = app_state(dir.path());
        state.sessions = SessionManager::new(
            Arc::clone(&state.repo),
            Arc::clone(&state.secrets),
            KnownHosts::new(state.paths.known_hosts.clone()),
            default_drivers(),
        )
        .with_cell_fetch_limit(MIB - 1);
        let id = open(&state, t).await;
        let name = blobs(t, "limit").await;
        let o = obj(t, &name);
        let p = page(&state, id, o.clone()).await;
        let r = row_ref(&p, 0, &["id"]);
        let e = data_cell_fetch_impl(&state, id, o.clone(), r.key.clone(), "data".into())
            .await
            .unwrap_err();
        assert_eq!(config_field(&e), "column:data", "{t:?}: {e:?}");
        // Below the limit: read whole.
        let cell = fetch(&state, id, &o, &r, "note").await;
        assert!(matches!(cell.payload, CellPayload::Text { .. }), "{t:?}");
        drop_table(t, &name).await;
    }
}

#[tokio::test]
async fn loaded_value_used_in_update() {
    require_db!();
    let _serial = DB_LOCK.lock().await;
    for t in Target::all() {
        let dir = tempfile::tempdir().unwrap();
        let state = app_state(dir.path());
        let id = open(&state, t).await;
        let name = blobs(t, "upd").await;
        let o = obj(t, &name);
        let p = page(&state, id, o.clone()).await;
        let cell = fetch(&state, id, &o, &row_ref(&p, 0, &["id"]), "data").await;
        let r = apply(
            &state,
            id,
            set(
                o.clone(),
                vec![RowEdit::Update {
                    row: row_ref(&p, 1, &["id"]),
                    set: cells(&[("data", CellInput::Loaded { token: cell.token })]),
                }],
            ),
        )
        .await
        .unwrap();
        assert_eq!(r.outcome, ApplyOutcome::Committed, "{t:?}: {r:?}");
        let p = page(&state, id, o.clone()).await;
        let copy = fetch(&state, id, &o, &row_ref(&p, 1, &["id"]), "data").await;
        assert!(bytes_of(&copy) == mib(), "{t:?}");
        drop_table(t, &name).await;
    }
}

#[tokio::test]
async fn token_for_other_column_rejected() {
    require_db!();
    let _serial = DB_LOCK.lock().await;
    for t in Target::all() {
        let dir = tempfile::tempdir().unwrap();
        let state = app_state(dir.path());
        let id = open(&state, t).await;
        let name = blobs(t, "other").await;
        let o = obj(t, &name);
        let p = page(&state, id, o.clone()).await;
        let cell = fetch(&state, id, &o, &row_ref(&p, 0, &["id"]), "note").await;
        let e = data_preview_impl(
            &state,
            id,
            set(
                o.clone(),
                vec![RowEdit::Update {
                    row: row_ref(&p, 1, &["id"]),
                    set: cells(&[("data", CellInput::Loaded { token: cell.token })]),
                }],
            ),
        )
        .await
        .unwrap_err();
        assert_eq!(config_field(&e), "column:data", "{t:?}: {e:?}");
        // Another table of the same connection.
        let other = blobs(t, "other2").await;
        let o2 = obj(t, &other);
        let p2 = page(&state, id, o2.clone()).await;
        let e = data_preview_impl(
            &state,
            id,
            set(
                o2,
                vec![RowEdit::Update {
                    row: row_ref(&p2, 1, &["id"]),
                    set: cells(&[("note", CellInput::Loaded { token: cell.token })]),
                }],
            ),
        )
        .await
        .unwrap_err();
        assert_eq!(config_field(&e), "column:note", "{t:?}: {e:?}");
        drop_table(t, &name).await;
        drop_table(t, &other).await;
    }
}

/// MySQL compares a snapshot of the row: cut cells fetched in full join it, so a
/// change to them is a conflict like any other.
#[tokio::test]
async fn original_snapshot_uses_loaded_full_value() {
    require_db!();
    let _serial = DB_LOCK.lock().await;
    for t in Target::mysql_family() {
        let dir = tempfile::tempdir().unwrap();
        let state = app_state(dir.path());
        let id = open(&state, t).await;
        let name = blobs(t, "snapfull").await;
        let o = obj(t, &name);
        let p = page(&state, id, o.clone()).await;
        let mut r = row_ref(&p, 0, &["id"]);
        for col in ["data", "note"] {
            let cell = fetch(&state, id, &o, &r, col).await;
            r.loaded.push((col.into(), cell.token));
        }
        let edit = |r: RowRef, qty: &str| {
            set(
                o.clone(),
                vec![RowEdit::Update {
                    row: r,
                    set: cells(&[("qty", text(qty))]),
                }],
            )
        };
        let preview = data_preview_impl(&state, id, edit(r.clone(), "5"))
            .await
            .unwrap();
        assert_eq!(
            preview.protections[0],
            Protection::Columns { unchecked: vec![] },
            "{t:?}"
        );
        let applied = data_apply_impl(&state, preview.plan_id, false)
            .await
            .unwrap();
        assert_eq!(
            applied.outcome,
            ApplyOutcome::Committed,
            "{t:?}: {applied:?}"
        );
        // The cut text changes after it was fetched: a conflict, nothing applied.
        admin(
            t,
            &format!(
                "UPDATE {} SET note = CONCAT(note, 'x') WHERE id = 1",
                scratch(t, &name)
            ),
        )
        .await;
        r.original[3].1 = Value::Int(5);
        let res = apply(&state, id, edit(r, "6")).await.unwrap();
        assert_eq!(res.outcome, ApplyOutcome::RolledBack, "{t:?}: {res:?}");
        assert!(res.statements[0].conflict, "{t:?}");
        let qty = admin(
            t,
            &format!("SELECT qty FROM {} WHERE id = 1", scratch(t, &name)),
        )
        .await;
        assert_eq!(int(&qty[0][0]), 5, "{t:?}");
        drop_table(t, &name).await;
    }
}

/// Load a value from a file picked in the backend's dialog, write it with its token,
/// and save a loaded value to a file; other paths are refused.
#[tokio::test]
async fn cell_file_round_trip() {
    require_db!();
    let _serial = DB_LOCK.lock().await;
    for t in Target::all() {
        let dir = tempfile::tempdir().unwrap();
        let state = app_state(dir.path());
        let id = open(&state, t).await;
        let name = blobs(t, "file").await;
        let o = obj(t, &name);
        let input = dir.path().join("in.bin");
        let bytes: Vec<u8> = mib().into_iter().rev().collect();
        std::fs::write(&input, &bytes).unwrap();
        let in_path = input.to_string_lossy().into_owned();
        // Not picked in the dialog yet.
        let e = data_cell_load_file_impl(&state, id, o.clone(), "data".into(), in_path.clone())
            .await
            .unwrap_err();
        assert_eq!(config_field(&e), "path", "{t:?}");
        state
            .file_picks
            .admit(FileAccess::CellValueRead, input.clone())
            .unwrap();
        let cell = data_cell_load_file_impl(&state, id, o.clone(), "data".into(), in_path)
            .await
            .unwrap();
        assert!(bytes_of(&cell) == bytes, "{t:?}");
        let p = page(&state, id, o.clone()).await;
        let r = apply(
            &state,
            id,
            set(
                o.clone(),
                vec![RowEdit::Update {
                    row: row_ref(&p, 1, &["id"]),
                    set: cells(&[("data", CellInput::Loaded { token: cell.token })]),
                }],
            ),
        )
        .await
        .unwrap();
        assert_eq!(r.outcome, ApplyOutcome::Committed, "{t:?}: {r:?}");

        // Save the fetched value of row 1 to a file.
        let out = dir.path().join("out.bin");
        let out_path = out.to_string_lossy().into_owned();
        let fetched = fetch(&state, id, &o, &row_ref(&p, 0, &["id"]), "data").await;
        let e = data_cell_save_file_impl(&state, fetched.token, out_path.clone())
            .await
            .unwrap_err();
        assert_eq!(config_field(&e), "path", "{t:?}");
        // A path admitted for reading is not one to write.
        let e =
            data_cell_save_file_impl(&state, fetched.token, input.to_string_lossy().into_owned())
                .await
                .unwrap_err();
        assert_eq!(config_field(&e), "path", "{t:?}");
        state
            .file_picks
            .admit(FileAccess::CellValueWrite, out.clone())
            .unwrap();
        data_cell_save_file_impl(&state, fetched.token, out_path)
            .await
            .unwrap();
        assert!(std::fs::read(&out).unwrap() == mib(), "{t:?}");
        let written = admin(
            t,
            &format!("SELECT data FROM {} WHERE id = 2", scratch(t, &name)),
        )
        .await;
        match &written[0][0] {
            Value::Bytes { len, preview } => {
                assert_eq!(*len, MIB as u64, "{t:?}");
                assert_eq!(preview[..4], bytes[..4], "{t:?}");
            }
            other => panic!("{t:?}: {other:?}"),
        }
        drop_table(t, &name).await;
    }
}
