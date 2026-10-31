//! The table designer (P4b) end to end on the Docker matrix (`DNT_TEST_DB=1`): every
//! test goes through `SessionManager::table_design_*` exactly as the UI does, on the
//! six servers of `Target::all()` plus MySQL 8.4.
//!
//! Each target works in its own scratch namespace `p4b_<uuid>` (a database on MySQL /
//! MariaDB, a schema of `dnt_fixture` on PostgreSQL), dropped afterwards also when an
//! assertion fails (the panic is re-raised after the cleanup).

use dnt_core::config::{ConnOptions, ConnectionConfig, SecretMode};
use dnt_core::driver::{Driver, ExecOptions, ResultSummary, RowSink, Session};
use dnt_core::error::DbError;
use dnt_core::meta::Scope;
use dnt_core::secret::MemorySecretStore;
use dnt_core::server::DbKind;
use dnt_core::sql::{
    classify,
    ident::{qualified_name, quote_ident},
};
use dnt_core::value::{ColumnMeta, Value};
use dnt_driver_mysql::MySqlDriver;
use dnt_driver_pg::PgDriver;
use dnt_schema::{
    CheckDef, DefaultSpec, ElementRef, FkDef, ImpactKind, IndexDef, IndexPartDef, TableDef,
    TypeSpec,
};
use dnt_testkit::{require_db, Target, FIXTURE_DB, TEST_PASSWORD};
use dntdatabase::commands::connections::{
    create_connection_impl, open_connection_impl, SecretsInput,
};
use dntdatabase::object_actions::ActionOutcome;
use dntdatabase::state::{AppPaths, AppState};
use dntdatabase::table_design::DesignReport;
use futures_util::FutureExt;
use serde_json::json;
use std::any::Any;
use std::panic::AssertUnwindSafe;
use std::sync::Arc;
use std::time::{Duration, Instant};
use uuid::Uuid;

fn targets() -> Vec<Target> {
    let mut t = Target::all().to_vec();
    t.push(Target::MySql84);
    t
}

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

async fn connect(t: Target) -> Box<dyn Session> {
    let driver: Box<dyn Driver> = if t.kind() == DbKind::MySql {
        Box::new(MySqlDriver)
    } else {
        Box::new(PgDriver)
    };
    driver
        .connect(&t.resolved(Some(FIXTURE_DB)))
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

/// One target: an app state with an open connection, a direct driver session and the
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
        let s = connect(t).await;
        let pg = t.kind() == DbKind::Postgres;
        let ns = format!("p4b_{}", Uuid::new_v4().simple());
        let create = if pg {
            format!("CREATE SCHEMA {}", quote_ident(t.kind().as_sql(), &ns))
        } else {
            format!(
                "CREATE DATABASE {} CHARACTER SET utf8mb4 COLLATE utf8mb4_unicode_ci",
                quote_ident(t.kind().as_sql(), &ns)
            )
        };
        sql(s.as_ref(), &create)
            .await
            .unwrap_or_else(|e| panic!("{t:?}: {create}: {e}"));
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

    fn rel(&self, name: &str) -> String {
        qualified_name(
            self.t.kind().as_sql(),
            self.db(),
            self.pg.then_some(self.ns.as_str()),
            name,
        )
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

    async fn open_table(&self, table: &str) -> dntdatabase::table_design::DesignOpen {
        self.state
            .sessions
            .table_design_open(self.id, self.scope(), Some(table.into()))
            .await
            .unwrap_or_else(|e| panic!("{:?}: open {table}: {e:?}", self.t))
    }

    async fn close_design(&self, design: Uuid) {
        self.state
            .sessions
            .table_design_close(design)
            .await
            .unwrap();
    }

    /// Preview + apply of `draft`, as the UI does with "Apply".
    async fn apply(&self, design: Uuid, draft: TableDef) -> Result<DesignReport, DbError> {
        let p = self
            .state
            .sessions
            .table_design_preview(design, draft)
            .await?;
        self.state
            .sessions
            .table_design_apply(p.plan_id, true, p.confirm_name, Uuid::new_v4(), &|_| {})
            .await
    }

    /// Drops the namespace and closes everything, then re-raises a panic of the body.
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
        // The app's and the body's sessions go first (they may hold locks or an open
        // transaction); the DROP runs on a fresh session whose lock wait is bounded, so a
        // stuck lock fails the cleanup instead of hanging the test.
        self.state.sessions.close(self.id).await;
        self.s.close().await;
        let cleanup = connect(self.t).await;
        let bound = if self.pg {
            "SET lock_timeout = '10s'"
        } else {
            "SET SESSION lock_wait_timeout = 10"
        };
        let cleaned = match sql(cleanup.as_ref(), bound).await {
            Ok(_) => sql(cleanup.as_ref(), &drop).await,
            Err(e) => Err(e),
        };
        cleanup.close().await;
        if let Err(panic) = result {
            if let Err(e) = &cleaned {
                eprintln!("cleanup {:?} failed after a panic: {drop}: {e}", self.t);
            }
            std::panic::resume_unwind(panic);
        }
        assert!(cleaned.is_ok(), "cleanup {:?}: {cleaned:?}", self.t);
        // Per-target evidence in the output (libtest names the thread after the test).
        let test = std::thread::current().name().unwrap_or("?").to_string();
        eprintln!("PASS {test} {:?}", self.t);
    }
}

/// `def` without what an apply / re-open legitimately changes: element `original`s,
/// `locked` flags and the table's own name. Indexes, foreign keys and checks have no
/// order on the server (introspection lists them by name), so they are sorted by name;
/// column order is kept and compared.
fn norm(mut d: TableDef) -> TableDef {
    d.indexes.sort_by(|a, b| a.name.cmp(&b.name));
    d.foreign_keys.sort_by(|a, b| a.name.cmp(&b.name));
    d.checks.sort_by(|a, b| a.name.cmp(&b.name));
    d.original_name = None;
    d.name = String::new();
    d.read_only_reason = None;
    for c in &mut d.columns {
        c.original = None;
        c.locked = false;
        c.using = None;
    }
    if let Some(k) = &mut d.primary_key {
        k.original = None;
        k.locked = false;
    }
    for i in &mut d.indexes {
        i.original = None;
        i.locked = false;
    }
    for f in &mut d.foreign_keys {
        f.original = None;
        f.locked = false;
    }
    for c in &mut d.checks {
        c.original = None;
        c.locked = false;
    }
    d.options.locked = false;
    d
}

fn lock_names(open: &dntdatabase::table_design::DesignOpen) -> Vec<ElementRef> {
    open.locked.iter().map(|l| l.element.clone()).collect()
}

// ---------------------------------------------------------------------------------
// round_trip_fixture_tables
// ---------------------------------------------------------------------------------

/// The fixture tables of a target: `(schema, table)`.
fn fixture_tables(t: Target) -> Vec<(Option<&'static str>, &'static str)> {
    if t.kind() == DbKind::Postgres {
        vec![
            (Some("public"), "orders"),
            (Some("public"), "customers"),
            (Some("sales"), "invoices"),
        ]
    } else {
        vec![
            (None, "orders"),
            (None, "customers"),
            (None, "weird_names"),
            (None, "all_types"),
        ]
    }
}

/// The locks a fixture table opens with: the lists of the fidelity unit tests
/// (`crates/schema/src/fidelity.rs`, Task 9) for `orders`, `weird_names`, `all_types`
/// and `sales.invoices`; `customers` (not a fidelity fixture) as listed here.
fn expected_locks(t: Target, table: &str) -> Vec<ElementRef> {
    let maria = matches!(t, Target::MariaDb106 | Target::MariaDb114);
    let mysql8 = matches!(t, Target::MySql8 | Target::MySql84);
    match table {
        // MariaDB's JSON is longtext + a column-level json_valid check.
        "all_types" if maria => vec![
            ElementRef::Column("c_json".into()),
            ElementRef::Check("c_json".into()),
        ],
        // A generated column cannot be written by the designer.
        "invoices" => vec![ElementRef::Column("amount_with_tax".into())],
        // MySQL 8 `customers` (testdata/sql/mysql8.sql): an INVISIBLE column and a
        // functional index (not editable), and a check whose server text holds a
        // charset introducer and `\'`-escaped quotes (`_utf8mb4\'\'` in CHECK_CLAUSE)
        // that cannot be reproduced exactly (same reasoning as ruling T5-10).
        "customers" if mysql8 => vec![
            ElementRef::Column("secret_note".into()),
            ElementRef::Index("idx_customers_email_lower".into()),
            ElementRef::Check("chk_customers_email".into()),
        ],
        _ => vec![],
    }
}

/// The new-table draft that copies `def` under `name`. Locked elements are left out
/// (the designer never writes them: generated columns, functional indexes, MariaDB's
/// JSON column and its check). A foreign key into the source table's own namespace is
/// qualified so it keeps pointing at the original fixture table (`ref_schema` is the
/// MySQL database / PostgreSQL schema of the source). Constraint and index names are
/// kept: the copy lives in another namespace, so they do not collide. PostgreSQL
/// identity columns round-trip as identity (no fixture table has a `nextval(…)` serial
/// default that would point at the source table's sequence).
fn copy_draft(def: &TableDef, name: &str, source_ns: &str) -> TableDef {
    let mut d = def.clone();
    d.original_name = None;
    d.name = name.into();
    d.read_only_reason = None;
    let dropped: Vec<String> = d
        .columns
        .iter()
        .filter(|c| c.locked)
        .map(|c| c.name.clone())
        .collect();
    d.columns.retain(|c| !c.locked);
    d.indexes.retain(|i| !i.locked);
    d.foreign_keys.retain(|f| !f.locked);
    d.checks.retain(|c| !c.locked && !dropped.contains(&c.name));
    for c in &mut d.columns {
        c.original = None;
    }
    if let Some(k) = &mut d.primary_key {
        k.original = None;
    }
    for i in &mut d.indexes {
        i.original = None;
    }
    for f in &mut d.foreign_keys {
        f.original = None;
        if f.ref_schema.is_none() {
            f.ref_schema = Some(source_ns.into());
        }
    }
    for c in &mut d.checks {
        c.original = None;
    }
    d.options.locked = false;
    d
}

#[tokio::test]
async fn round_trip_fixture_tables() {
    require_db!();
    for t in targets() {
        let sc = Scratch::new(t).await;
        let body = async {
            for (schema, table) in fixture_tables(t) {
                let source = Scope {
                    database: FIXTURE_DB.into(),
                    schema: schema.map(str::to_string),
                };
                let orig = sc
                    .state
                    .sessions
                    .table_design_open(sc.id, source, Some(table.into()))
                    .await
                    .unwrap_or_else(|e| panic!("{t:?} {table}: {e:?}"));
                assert_eq!(
                    lock_names(&orig),
                    expected_locks(t, table),
                    "{t:?} {table}: {:#?}",
                    orig.locked
                );
                let copy_name = format!("{table}_copy");
                let draft = copy_draft(&orig.def, &copy_name, schema.unwrap_or(FIXTURE_DB));
                let new = sc
                    .state
                    .sessions
                    .table_design_open(sc.id, sc.scope(), None)
                    .await
                    .unwrap();
                let report = sc
                    .apply(new.design_id, draft.clone())
                    .await
                    .unwrap_or_else(|e| panic!("{t:?} {table}: {e:?}"));
                assert_eq!(
                    report.outcome,
                    ActionOutcome::Applied,
                    "{t:?} {table}: {report:?}"
                );
                sc.close_design(new.design_id).await;
                sc.close_design(orig.design_id).await;
                let copy = sc.open_table(&copy_name).await;
                assert!(copy.locked.is_empty(), "{t:?} {table}: {:#?}", copy.locked);
                assert_eq!(copy.def.name, copy_name);
                assert_eq!(norm(copy.def.clone()), norm(draft), "{t:?} {table}");
                sc.close_design(copy.design_id).await;
            }
        };
        let r = AssertUnwindSafe(body).catch_unwind().await;
        sc.finish(r).await;
    }
}

// ---------------------------------------------------------------------------------
// apply_each_change_kind
// ---------------------------------------------------------------------------------

/// `parent (id)` and `t (id, code, a, b, parent_id)` with three rows and an index on
/// `parent_id` (MySQL needs one for a foreign key; without it the server would add
/// its own).
async fn create_t(sc: &Scratch) {
    let tail = if sc.pg {
        ""
    } else {
        " ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_unicode_ci"
    };
    sc.exec(&format!(
        "CREATE TABLE {} (id int NOT NULL, PRIMARY KEY (id)){tail}",
        sc.rel("parent")
    ))
    .await;
    sc.exec(&format!(
        "INSERT INTO {} (id) VALUES (1), (2)",
        sc.rel("parent")
    ))
    .await;
    sc.exec(&format!(
        "CREATE TABLE {} (id int NOT NULL, code int NOT NULL, a int NULL, \
         b varchar(20) NULL, parent_id int NULL, PRIMARY KEY (id)){tail}",
        sc.rel("t")
    ))
    .await;
    let ix = quote_ident(sc.t.kind().as_sql(), "ix_parent");
    sc.exec(&format!("CREATE INDEX {ix} ON {} (parent_id)", sc.rel("t")))
        .await;
    sc.exec(&format!(
        "INSERT INTO {} (id, code, a, b, parent_id) VALUES \
         (1, 10, 1, 'x', 1), (2, 20, 2, 'y', 2), (3, 30, NULL, 'z', NULL)",
        sc.rel("t")
    ))
    .await;
}

/// Opens `t`, applies `edit` to its def, re-opens it and checks the def is the draft
/// (after `fix`, which takes the re-opened def to account for server normalisation).
async fn step(
    sc: &Scratch,
    what: &str,
    edit: impl FnOnce(&mut TableDef),
    fix: impl FnOnce(&mut TableDef, &TableDef),
) -> TableDef {
    step_on(sc, "t", "t", what, edit, fix).await
}

/// [`step`] on table `from`, re-opened as `to` (a rename of the table).
async fn step_on(
    sc: &Scratch,
    from: &str,
    to: &str,
    what: &str,
    edit: impl FnOnce(&mut TableDef),
    fix: impl FnOnce(&mut TableDef, &TableDef),
) -> TableDef {
    let t = sc.t;
    let o = sc.open_table(from).await;
    assert!(o.locked.is_empty(), "{t:?} {what}: {:#?}", o.locked);
    let mut draft = o.def.clone();
    edit(&mut draft);
    let report = sc
        .apply(o.design_id, draft.clone())
        .await
        .unwrap_or_else(|e| panic!("{t:?} {what}: {e:?}"));
    assert_eq!(
        report.outcome,
        ActionOutcome::Applied,
        "{t:?} {what}: {report:?}"
    );
    assert!(report.warnings.is_empty(), "{t:?} {what}: {report:?}");
    sc.close_design(o.design_id).await;
    let after = sc.open_table(to).await;
    sc.close_design(after.design_id).await;
    assert_eq!(after.def.name, to, "{t:?} {what}");
    assert_eq!(after.def.original_name.as_deref(), Some(to), "{t:?} {what}");
    assert!(after.locked.is_empty(), "{t:?} {what}: {:#?}", after.locked);
    fix(&mut draft, &after.def);
    assert_eq!(norm(after.def.clone()), norm(draft), "{t:?} {what}");
    after.def
}

fn no_fix(_: &mut TableDef, _: &TableDef) {}

/// A check expression as the server stores it, without identifier quotes, outer
/// parentheses and extra whitespace: `(`code` > 0)` / `(code > 0)` -> `code > 0`.
fn norm_expression(e: &str) -> String {
    let mut s: String = e.chars().filter(|c| *c != '`' && *c != '"').collect();
    loop {
        let t = s.trim();
        let wrapped = t.starts_with('(') && t.ends_with(')') && {
            // The first `(` closes at the very end.
            let mut depth = 0i32;
            t.char_indices().all(|(i, c)| {
                match c {
                    '(' => depth += 1,
                    ')' => depth -= 1,
                    _ => {}
                }
                depth > 0 || i == t.len() - 1
            })
        };
        if !wrapped {
            break;
        }
        s = t[1..t.len() - 1].to_string();
    }
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn col_index(d: &TableDef, name: &str) -> usize {
    d.columns
        .iter()
        .position(|c| c.name == name)
        .unwrap_or_else(|| panic!("no column {name}"))
}

#[tokio::test]
async fn apply_each_change_kind() {
    require_db!();
    for t in targets() {
        let sc = Scratch::new(t).await;
        let body = async {
            create_t(&sc).await;
            let mysql = !sc.pg;
            let checks = !matches!(t, Target::MySql57);

            // Add a column with a value default.
            step(
                &sc,
                "add column",
                |d| {
                    let mut c = d.columns[col_index(d, "b")].clone();
                    c.original = None;
                    c.name = "c".into();
                    c.data_type = TypeSpec::Known {
                        base: "varchar".into(),
                        length: Some(10),
                        precision: None,
                        scale: None,
                        unsigned: false,
                        values: vec![],
                    };
                    c.default = Some(DefaultSpec::Value { text: "d'q".into() });
                    d.columns.push(c);
                },
                no_fix,
            )
            .await;
            assert_eq!(
                sc.rows(&format!("SELECT c FROM {} ORDER BY id", sc.rel("t")))
                    .await,
                vec![vec!["d'q".to_string()]; 3],
                "{t:?}"
            );

            // Rename a column: the values stay.
            step(
                &sc,
                "rename column",
                |d| {
                    let i = col_index(d, "b");
                    d.columns[i].name = "b2".into();
                },
                no_fix,
            )
            .await;
            assert_eq!(
                sc.rows(&format!("SELECT b2 FROM {} ORDER BY id", sc.rel("t")))
                    .await,
                vec![vec!["x".to_string()], vec!["y".into()], vec!["z".into()]],
                "{t:?}"
            );

            // Alter a column: type, nullability and comment.
            step(
                &sc,
                "alter column",
                |d| {
                    let i = col_index(d, "a");
                    let c = &mut d.columns[i];
                    if let TypeSpec::Known { base, .. } = &mut c.data_type {
                        *base = "bigint".into();
                    }
                    c.comment = Some("altered".into());
                    let j = col_index(d, "code");
                    d.columns[j].nullable = true;
                },
                no_fix,
            )
            .await;

            if mysql {
                // Move the added column right after `id`.
                step(
                    &sc,
                    "move column",
                    |d| {
                        let c = d.columns.remove(col_index(d, "c"));
                        d.columns.insert(1, c);
                    },
                    no_fix,
                )
                .await;
                assert_eq!(
                    sc.rows(&format!("SELECT * FROM {} WHERE id = 1", sc.rel("t")))
                        .await[0][1],
                    "d'q",
                    "{t:?}"
                );
            }

            // Drop a column.
            step(
                &sc,
                "drop column",
                |d| {
                    d.columns.remove(col_index(d, "c"));
                },
                no_fix,
            )
            .await;

            // Change the primary key: (id) -> (id, a) needs a NOT NULL column; use `id`
            // and the re-tightened `code`.
            step(
                &sc,
                "primary key",
                |d| {
                    let j = col_index(d, "code");
                    d.columns[j].nullable = false;
                    let pk = d.primary_key.as_mut().unwrap();
                    pk.columns = vec!["id".into(), "code".into()];
                    if sc.pg {
                        // One namespace for constraint names (ruling T6-2): a new name.
                        pk.name = Some("t_pk2".into());
                    }
                },
                no_fix,
            )
            .await;

            // Add, rename and drop an index.
            step(
                &sc,
                "add index",
                |d| {
                    let mut i: IndexDef = d
                        .indexes
                        .iter()
                        .find(|i| i.name == "ix_parent")
                        .unwrap()
                        .clone();
                    i.original = None;
                    i.name = "ix_a".into();
                    i.parts = vec![IndexPartDef {
                        column: Some("a".into()),
                        expression: None,
                        prefix: None,
                        descending: false,
                    }];
                    d.indexes.push(i);
                },
                no_fix,
            )
            .await;
            step(
                &sc,
                "rename index",
                |d| {
                    let i = d.indexes.iter_mut().find(|i| i.name == "ix_a").unwrap();
                    i.name = "ix_a2".into();
                },
                no_fix,
            )
            .await;
            step(
                &sc,
                "drop index",
                |d| d.indexes.retain(|i| i.name != "ix_a2"),
                no_fix,
            )
            .await;

            // Add and drop a foreign key.
            step(
                &sc,
                "add foreign key",
                |d| {
                    d.foreign_keys.push(FkDef {
                        original: None,
                        name: "fk_parent".into(),
                        columns: vec!["parent_id".into()],
                        ref_schema: None,
                        ref_table: "parent".into(),
                        ref_columns: vec!["id".into()],
                        on_update: "CASCADE".into(),
                        on_delete: "SET NULL".into(),
                        match_type: None,
                        deferrable: false,
                        initially_deferred: false,
                        locked: false,
                    })
                },
                no_fix,
            )
            .await;
            step(&sc, "drop foreign key", |d| d.foreign_keys.clear(), no_fix).await;

            // Add and drop a check (MySQL 5.7 parses and ignores checks: not offered).
            if checks {
                step(
                    &sc,
                    "add check",
                    |d| {
                        d.checks.push(CheckDef {
                            original: None,
                            name: "ck_code".into(),
                            expression: "code > 0".into(),
                            locked: false,
                        })
                    },
                    // The server stores the expression in its own form (`(`code` > 0)`,
                    // `(code > 0)`): it must normalise to the draft's, then is taken as is.
                    |draft, after| {
                        let got = after.checks.iter().find(|c| c.name == "ck_code");
                        let got = got.expect("ck_code after apply");
                        assert_eq!(
                            norm_expression(&got.expression),
                            "code > 0",
                            "{t:?}: {}",
                            got.expression
                        );
                        draft.checks.last_mut().unwrap().expression = got.expression.clone();
                    },
                )
                .await;
                step(
                    &sc,
                    "drop check",
                    |d| d.checks.retain(|c| c.name != "ck_code"),
                    no_fix,
                )
                .await;
            }

            // Table options (MySQL / MariaDB; PostgreSQL has none in the designer).
            if mysql {
                step(
                    &sc,
                    "options",
                    |d| d.options.row_format = Some("Compact".into()),
                    no_fix,
                )
                .await;
            }

            // Table comment.
            step(
                &sc,
                "comment",
                |d| d.comment = Some("designer's comment".into()),
                no_fix,
            )
            .await;

            // Rename the table, then back.
            step_on(
                &sc,
                "t",
                "t_renamed",
                "rename table",
                |d| d.name = "t_renamed".into(),
                no_fix,
            )
            .await;
            assert_eq!(
                sc.rows(&format!(
                    "SELECT id FROM {} ORDER BY id",
                    sc.rel("t_renamed")
                ))
                .await
                .len(),
                3,
                "{t:?}"
            );
            step_on(
                &sc,
                "t_renamed",
                "t",
                "rename table back",
                |d| d.name = "t".into(),
                no_fix,
            )
            .await;

            // Every row is still there.
            assert_eq!(
                sc.rows(&format!("SELECT id FROM {} ORDER BY id", sc.rel("t")))
                    .await,
                vec![vec!["1".to_string()], vec!["2".into()], vec!["3".into()]],
                "{t:?}"
            );
        };
        let r = AssertUnwindSafe(body).catch_unwind().await;
        sc.finish(r).await;
    }
}

// ---------------------------------------------------------------------------------
// locked_element_untouched
// ---------------------------------------------------------------------------------

/// The server's text of each locked index of `lk`: its `SHOW CREATE TABLE` line on
/// MySQL / MariaDB, `pg_get_indexdef` on PostgreSQL.
async fn locked_texts(sc: &Scratch, names: &[&str]) -> Vec<String> {
    if sc.pg {
        let mut out = Vec::new();
        for n in names {
            let r = sc
                .rows(&format!(
                    "SELECT pg_get_indexdef('{}.{}'::regclass)",
                    quote_ident(DbKind::Postgres.as_sql(), &sc.ns),
                    quote_ident(DbKind::Postgres.as_sql(), n)
                ))
                .await;
            out.push(r[0][0].clone());
        }
        out
    } else {
        let ddl = sc
            .rows(&format!("SHOW CREATE TABLE {}", sc.rel("lk")))
            .await
            .remove(0)
            .remove(1);
        names
            .iter()
            .map(|n| {
                ddl.lines()
                    .find(|l| l.contains(&format!("`{n}`")))
                    .unwrap_or_else(|| panic!("{:?}: no {n} in {ddl}", sc.t))
                    .to_string()
            })
            .collect()
    }
}

#[tokio::test]
async fn locked_element_untouched() {
    require_db!();
    for t in targets() {
        let sc = Scratch::new(t).await;
        let body = async {
            let mut names = Vec::new();
            if sc.pg {
                sc.exec(&format!(
                    "CREATE TABLE {} (id int PRIMARY KEY, a int NULL, name text NULL)",
                    sc.rel("lk")
                ))
                .await;
                sc.exec(&format!(
                    "CREATE INDEX ix_expr ON {} (lower(name))",
                    sc.rel("lk")
                ))
                .await;
                names.push("ix_expr");
            } else {
                let expr = if matches!(t, Target::MySql8 | Target::MySql84) {
                    names.push("ix_expr");
                    ", KEY ix_expr ((lower(name)))"
                } else {
                    ""
                };
                sc.exec(&format!(
                    "CREATE TABLE {} (id int NOT NULL, a int NULL, body text NULL, \
                     name varchar(50) NULL, PRIMARY KEY (id), FULLTEXT KEY ft_body (body){expr}) \
                     ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_unicode_ci",
                    sc.rel("lk")
                ))
                .await;
                names.push("ft_body");
            }
            let before = locked_texts(&sc, &names).await;
            let o = sc.open_table("lk").await;
            let mut locked = lock_names(&o);
            locked.sort_by_key(|e| format!("{e:?}"));
            let mut expected: Vec<ElementRef> = names
                .iter()
                .map(|n| ElementRef::Index(n.to_string()))
                .collect();
            expected.sort_by_key(|e| format!("{e:?}"));
            assert_eq!(locked, expected, "{t:?}: {:#?}", o.locked);
            let mut draft = o.def.clone();
            let i = col_index(&draft, "a");
            draft.columns[i].comment = Some("changed".into());
            let mut c = draft.columns[i].clone();
            c.original = None;
            c.name = "added".into();
            c.comment = None;
            draft.columns.push(c);
            let report = sc
                .apply(o.design_id, draft)
                .await
                .unwrap_or_else(|e| panic!("{t:?}: {e:?}"));
            assert_eq!(report.outcome, ActionOutcome::Applied, "{t:?}: {report:?}");
            let after = locked_texts(&sc, &names).await;
            assert_eq!(after, before, "{t:?}");
            let reopened = sc.open_table("lk").await;
            assert!(reopened.def.columns.iter().any(|c| c.name == "added"));
            let mut locked = lock_names(&reopened);
            locked.sort_by_key(|e| format!("{e:?}"));
            assert_eq!(locked, expected, "{t:?}");
        };
        let r = AssertUnwindSafe(body).catch_unwind().await;
        sc.finish(r).await;
    }
}

// ---------------------------------------------------------------------------------
// concurrent_alter_makes_plan_stale, pg_error_rolls_back_all, lock_wait_times_out,
// impact_counts
// ---------------------------------------------------------------------------------

/// `t` with a draft that adds column `added`.
async fn add_column_draft(sc: &Scratch) -> (Uuid, TableDef) {
    create_t(sc).await;
    let o = sc.open_table("t").await;
    let mut draft = o.def.clone();
    let mut c = draft.columns[col_index(&draft, "a")].clone();
    c.original = None;
    c.name = "added".into();
    draft.columns.push(c);
    (o.design_id, draft)
}

async fn has_column(sc: &Scratch, column: &str) -> bool {
    sql(
        sc.s.as_ref(),
        &format!(
            "SELECT {} FROM {}",
            quote_ident(sc.t.kind().as_sql(), column),
            sc.rel("t")
        ),
    )
    .await
    .is_ok()
}

#[tokio::test]
async fn concurrent_alter_makes_plan_stale() {
    require_db!();
    for t in targets() {
        let sc = Scratch::new(t).await;
        let body = async {
            let (design, draft) = add_column_draft(&sc).await;
            let p = sc
                .state
                .sessions
                .table_design_preview(design, draft)
                .await
                .unwrap_or_else(|e| panic!("{t:?}: {e:?}"));
            sc.exec(&format!("ALTER TABLE {} ADD COLUMN zz int", sc.rel("t")))
                .await;
            let r = sc
                .state
                .sessions
                .table_design_apply(p.plan_id, true, p.confirm_name, Uuid::new_v4(), &|_| {})
                .await;
            assert!(matches!(r, Err(DbError::PlanStale { .. })), "{t:?}: {r:?}");
            assert!(has_column(&sc, "zz").await, "{t:?}");
            assert!(!has_column(&sc, "added").await, "{t:?}");
        };
        let r = AssertUnwindSafe(body).catch_unwind().await;
        sc.finish(r).await;
    }
}

#[tokio::test]
async fn pg_error_rolls_back_all() {
    require_db!();
    for t in Target::pg() {
        let sc = Scratch::new(t).await;
        let body = async {
            let (design, mut draft) = add_column_draft(&sc).await;
            // Changes ordered before the failing one, which succeed and must be undone:
            // drop index `ix_parent` (drops first) and rename `b` -> `b2` (renames before
            // alters).
            draft.indexes.retain(|i| i.name != "ix_parent");
            let b = col_index(&draft, "b");
            draft.columns[b].name = "b2".into();
            // Row 3 holds NULL in `a`: SET NOT NULL fails, before ADD COLUMN `added`.
            let i = col_index(&draft, "a");
            draft.columns[i].nullable = false;
            let report = sc
                .apply(design, draft)
                .await
                .unwrap_or_else(|e| panic!("{t:?}: {e:?}"));
            assert_eq!(
                report.outcome,
                ActionOutcome::RolledBack,
                "{t:?}: {report:?}"
            );
            assert!(report.error.is_some(), "{t:?}: {report:?}");
            assert!(
                report.statements_sent >= 3,
                "{t:?}: the failing statement must come after the others: {report:?}"
            );
            assert!(!has_column(&sc, "added").await, "{t:?}");
            assert!(has_column(&sc, "b").await, "{t:?}: rename not undone");
            assert!(!has_column(&sc, "b2").await, "{t:?}: rename not undone");
            let index = sc
                .rows(&format!(
                    "SELECT count(*)::text FROM pg_indexes \
                     WHERE schemaname = '{}' AND indexname = 'ix_parent'",
                    sc.ns
                ))
                .await;
            assert_eq!(index, vec![vec!["1".to_string()]], "{t:?}: drop not undone");
            let nullable = sc
                .rows(&format!(
                    "SELECT is_nullable FROM information_schema.columns \
                     WHERE table_schema = '{}' AND table_name = 't' AND column_name = 'a'",
                    sc.ns
                ))
                .await;
            assert_eq!(nullable, vec![vec!["YES".to_string()]], "{t:?}");
        };
        let r = AssertUnwindSafe(body).catch_unwind().await;
        sc.finish(r).await;
    }
}

#[tokio::test]
async fn lock_wait_times_out() {
    require_db!();
    for t in targets() {
        let sc = Scratch::new(t).await;
        let body = async {
            let (design, draft) = add_column_draft(&sc).await;
            let p = sc
                .state
                .sessions
                .table_design_preview(design, draft)
                .await
                .unwrap_or_else(|e| panic!("{t:?}: {e:?}"));
            let holder = connect(t).await;
            if sc.pg {
                sql(holder.as_ref(), "BEGIN").await.unwrap();
                sql(
                    holder.as_ref(),
                    &format!("LOCK TABLE {} IN ACCESS EXCLUSIVE MODE", sc.rel("t")),
                )
                .await
                .unwrap();
            } else {
                sql(
                    holder.as_ref(),
                    &format!("LOCK TABLES {} WRITE", sc.rel("t")),
                )
                .await
                .unwrap();
            }
            let started = Instant::now();
            let r = tokio::time::timeout(
                Duration::from_secs(30),
                sc.state.sessions.table_design_apply(
                    p.plan_id,
                    true,
                    p.confirm_name,
                    Uuid::new_v4(),
                    &|_| {},
                ),
            )
            .await;
            let elapsed = started.elapsed();
            let release = if sc.pg { "ROLLBACK" } else { "UNLOCK TABLES" };
            sql(holder.as_ref(), release).await.unwrap();
            holder.close().await;
            let r = r.unwrap_or_else(|_| panic!("{t:?}: apply still waiting after 30 s"));
            eprintln!("{t:?}: lock wait -> {elapsed:?}, {r:?}");
            // MySQL / MariaDB: the re-check reads past `LOCK TABLES … WRITE` and the
            // ALTER is refused (1205) -> NotApplied. PostgreSQL: the re-check's own read
            // of the table waits for the ACCESS EXCLUSIVE lock and is refused by
            // `lock_timeout` (55P03) before anything is sent: an error, nothing applied.
            // Ruling T11-1: on PostgreSQL the 55P03 error itself, nothing sent.
            match r {
                Ok(r) if !sc.pg => {
                    assert_eq!(r.outcome, ActionOutcome::NotApplied, "{t:?}: {r:?}");
                    assert_eq!(r.statements_sent, 1, "{t:?}: {r:?}");
                    let e = r.error.as_ref().expect("an error");
                    assert_eq!(e.server_code.as_deref(), Some("1205"), "{t:?}: {r:?}");
                    assert!(!e.outcome_unknown, "{t:?}: {r:?}");
                }
                Err(DbError::Server {
                    sqlstate: Some(s), ..
                }) if sc.pg && s == "55P03" => {}
                other => panic!("{t:?}: {other:?}"),
            }
            assert!(
                // The 5 s lock wait, plus opening the DDL session and (MySQL, a statement
                // was sent) the History entry and the reload of the design.
                elapsed >= Duration::from_secs(4) && elapsed < Duration::from_secs(10),
                "{t:?}: {elapsed:?}"
            );
            assert!(!has_column(&sc, "added").await, "{t:?}");
        };
        let r = AssertUnwindSafe(body).catch_unwind().await;
        sc.finish(r).await;
    }
}

#[tokio::test]
async fn impact_counts() {
    require_db!();
    for t in targets() {
        let sc = Scratch::new(t).await;
        let body = async {
            create_t(&sc).await;
            // `b` holds three non-null values; one more row with NULL.
            sc.exec(&format!(
                "INSERT INTO {} (id, code) VALUES (4, 40)",
                sc.rel("t")
            ))
            .await;
            let o = sc.open_table("t").await;
            let mut draft = o.def.clone();
            draft.columns.remove(col_index(&draft, "b"));
            let p = sc
                .state
                .sessions
                .table_design_preview(o.design_id, draft)
                .await
                .unwrap_or_else(|e| panic!("{t:?}: {e:?}"));
            let loss = p
                .impacts
                .iter()
                .find(|c| {
                    c.impact.kind == ImpactKind::DataLoss
                        && c.impact.element == ElementRef::Column("b".into())
                })
                .unwrap_or_else(|| panic!("{t:?}: {:#?}", p.impacts));
            assert!(loss.counted, "{t:?}: {loss:?}");
            assert_eq!(loss.rows, Some(3.0), "{t:?}: {loss:?}");
            sc.close_design(o.design_id).await;
            assert!(has_column(&sc, "b").await, "{t:?}");
        };
        let r = AssertUnwindSafe(body).catch_unwind().await;
        sc.finish(r).await;
    }
}
