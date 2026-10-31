//! The Command-family contracts end to end (spec §9): `plan_commands`, `guard_commands`
//! and `run_commands` against the in-memory `FakeCommandDriver`.

use dnt_core::command::CommandClass;
use dnt_core::config::{EnvTag, ResolvedConfig};
use dnt_core::data_access::{check_read, KeyEdit};
use dnt_core::driver::{Driver, ExecOptions, ResultSummary, RowSink, Session, StmtOutcome};
use dnt_core::error::{AppError, DbError};
use dnt_core::keyspace::{KeyName, KeyType, ScanRequest};
use dnt_core::server::{DbKind, EngineFamily, NamespaceModel};
use dnt_core::tx::TxState;
use dnt_core::value::{ColumnMeta, Value};
use dnt_query::command::{guard_commands, plan_commands, run_commands, CommandRunSink};
use dnt_query::plan::{Target, Verdict};
use dnt_query::policy::{policy, PolicyCtx, RiskFacts};
use dnt_query::run::{RunCancel, RunConfig, RunReport, StopReason};
use dnt_query::PlannedCommand;
use dnt_testkit::fake_command::{FakeCommandDriver, FakeStore, FakeValue};
use std::collections::BTreeMap;
use std::time::Duration;

fn cfg(database: Option<&str>) -> ResolvedConfig {
    ResolvedConfig {
        kind: DbKind::Postgres,
        host: "fake".into(),
        port: 0,
        tls_server_name: "fake".into(),
        username: "fake".into(),
        password: None,
        database: database.map(str::to_string),
        ssl: None,
        connect_timeout: Duration::from_secs(1),
        read_only: false,
        application_name: "DntDatabase".into(),
    }
}

async fn connect() -> (Box<dyn Session>, FakeStore) {
    let driver = FakeCommandDriver::new();
    let store = driver.store();
    (driver.connect(&cfg(None)).await.unwrap(), store)
}

fn ctx(env: Option<EnvTag>, read_only: bool) -> PolicyCtx {
    PolicyCtx {
        env,
        read_only,
        confirm_unbounded: false,
        connection_name: "cache".into(),
    }
}

/// Everything a run reports, as text.
#[derive(Default)]
struct Rec {
    columns: Vec<Vec<String>>,
    rows: Vec<Vec<Value>>,
    ends: Vec<(u32, Result<(), String>)>,
    tx: Vec<TxState>,
    databases: Vec<String>,
}

impl RowSink for Rec {
    fn result_start(&mut self, cols: &[ColumnMeta]) {
        self.columns
            .push(cols.iter().map(|c| c.name.clone()).collect());
    }
    fn rows(&mut self, batch: Vec<Vec<Value>>) {
        self.rows.extend(batch);
    }
    fn result_end(&mut self, _summary: &ResultSummary) {}
}

impl CommandRunSink for Rec {
    fn statement_start(&mut self, _stmt: &PlannedCommand) {}
    fn statement_end(&mut self, stmt: &PlannedCommand, result: &Result<StmtOutcome, AppError>) {
        let result = match result {
            Ok(_) => Ok(()),
            Err(e) => Err(e.kind.clone()),
        };
        self.ends.push((stmt.index, result));
    }
    fn notice(&mut self, _index: u32, _message: &str) {}
    fn tx_state(&mut self, state: TxState) {
        self.tx.push(state);
    }
    fn database(&mut self, name: &str) {
        self.databases.push(name.to_string());
    }
}

fn run_cfg() -> RunConfig {
    RunConfig {
        continue_on_error: false,
        manual_tx: false,
    }
}

/// Plans `text` with the session's language, guards it for `ctx` and runs it.
async fn run_text(session: &dyn Session, text: &str, ctx: &PolicyCtx, rec: &mut Rec) -> RunReport {
    let c = session.command().unwrap();
    let mut plan = plan_commands(text, Target::All, c.language());
    guard_commands(&mut plan, ctx);
    let cancel = RunCancel::new(session.cancel_handle());
    run_commands(c, &plan, &ExecOptions::default(), run_cfg(), &cancel, rec).await
}

fn local() -> PolicyCtx {
    ctx(Some(EnvTag::Local), false)
}

fn text(s: &str) -> Value {
    Value::Text(s.into())
}

#[tokio::test]
async fn set_then_get_roundtrip() {
    let (s, store) = connect().await;
    assert_eq!(s.server_info().kind, DbKind::Postgres);
    let caps = s.capabilities();
    assert_eq!(
        (
            caps.family,
            caps.namespaces,
            caps.data_edit,
            caps.paged_objects
        ),
        (
            EngineFamily::Command,
            NamespaceModel::NumberedDb,
            true,
            true
        )
    );
    assert!(!caps.transactions && !caps.designer && !caps.explain);
    assert!(s.sql().is_err());

    let mut rec = Rec::default();
    let report = run_text(
        &*s,
        "SET greeting \"hello world\"\n# a comment\nGET greeting\nGET missing",
        &local(),
        &mut rec,
    )
    .await;
    assert_eq!(report.stop, None);
    assert_eq!(rec.ends, [(0, Ok(())), (1, Ok(())), (2, Ok(()))]);
    assert_eq!(rec.columns, [vec![], vec!["value"], vec!["value"]]);
    assert_eq!(rec.rows, [vec![text("hello world")], vec![Value::Null]]);
    assert_eq!(
        store.get("0", b"greeting"),
        Some(FakeValue::Str("hello world".into()))
    );

    let mut rec = Rec::default();
    run_text(
        &*s,
        "HSET h f1 v1\nHSET h f2 v2\nHGETALL h",
        &local(),
        &mut rec,
    )
    .await;
    assert_eq!(rec.columns[2], ["field", "value"]);
    assert_eq!(
        rec.rows,
        [vec![text("f1"), text("v1")], vec![text("f2"), text("v2")]]
    );
}

#[tokio::test]
async fn select_changes_db_for_the_rest() {
    let (s, store) = connect().await;
    let mut rec = Rec::default();
    let report = run_text(&*s, "SET a 1\nSELECT 2\nGET a", &local(), &mut rec).await;
    assert_eq!(report.stop, None);
    assert_eq!(rec.rows, [vec![Value::Null]]);
    assert_eq!(rec.databases, ["2"]);
    assert_eq!(store.get("0", b"a"), Some(FakeValue::Str("1".into())));
    assert_eq!(store.get("2", b"a"), None);

    let dbs = s.introspector().list_databases().await.unwrap();
    let names: Vec<_> = dbs.iter().map(|d| d.name.as_str()).collect();
    assert_eq!(names, ["0", "2"], "the dbs with keys plus the current one");

    // The configured database is where a session starts.
    let driver = FakeCommandDriver::new();
    let s = driver.connect(&cfg(Some("5"))).await.unwrap();
    run_text(&*s, "SET k v", &local(), &mut Rec::default()).await;
    assert_eq!(
        driver.store().get("5", b"k"),
        Some(FakeValue::Str("v".into()))
    );
}

#[tokio::test]
async fn stop_before_start_sends_nothing() {
    let (s, store) = connect().await;
    let c = s.command().unwrap();
    let mut plan = plan_commands("SET a 1\nSET b 2", Target::All, c.language());
    guard_commands(&mut plan, &local());
    let cancel = RunCancel::new(s.cancel_handle());
    cancel.signal().stop();
    let mut rec = Rec::default();
    let report = run_commands(
        c,
        &plan,
        &ExecOptions::default(),
        run_cfg(),
        &cancel,
        &mut rec,
    )
    .await;
    assert_eq!(report.stop, Some(StopReason::Cancelled));
    assert!(report.stmts.is_empty() && rec.ends.is_empty());

    // The driver itself refuses a command once `opts.stop` is raised.
    let opts = ExecOptions::default();
    opts.stop.stop();
    let cmd = c.language().parse("SET a 1").unwrap();
    let err = c.execute_command(&cmd, &opts, &mut rec).await.unwrap_err();
    assert!(
        matches!(
            err,
            DbError::Cancelled {
                outcome_unknown: false,
                was_in_tx: false
            }
        ),
        "{err:?}"
    );
    assert!(store.executed().is_empty());
    assert_eq!(store.get("0", b"a"), None);
}

#[tokio::test]
async fn cancel_stops_a_blocked_command() {
    let (s, store) = connect().await;
    store.block_next();
    let cancel = RunCancel::new(s.cancel_handle());
    let run = {
        let cancel = cancel.clone();
        tokio::spawn(async move {
            let c = s.command().unwrap();
            let mut plan = plan_commands("GET a\nSET b 1", Target::All, c.language());
            guard_commands(&mut plan, &local());
            let mut rec = Rec::default();
            let report = run_commands(
                c,
                &plan,
                &ExecOptions::default(),
                run_cfg(),
                &cancel,
                &mut rec,
            )
            .await;
            (report, rec)
        })
    };
    store.wait_blocked().await;
    cancel.cancel().await.unwrap();
    let (report, rec) = run.await.unwrap();
    assert_eq!(report.stop, Some(StopReason::Cancelled));
    assert_eq!(rec.ends, [(0, Err("cancelled".to_string()))]);
    let err = report.stmts[0].result.as_ref().unwrap_err();
    assert!(!err.outcome_unknown);
    assert_eq!(store.get("0", b"b"), None, "the rest is not run");
}

#[tokio::test]
async fn open_multi_reports_active() {
    let (s, store) = connect().await;
    let mut rec = Rec::default();
    let report = run_text(&*s, "MULTI\nSET a 1", &local(), &mut rec).await;
    assert_eq!(report.stop, None);
    assert_eq!(s.command().unwrap().tx_state(), TxState::Active);
    assert_eq!(rec.tx, [TxState::Active]);
    assert_eq!(store.get("0", b"a"), None, "queued, not run");
}

#[tokio::test]
async fn exec_commits_queued() {
    let (s, store) = connect().await;
    let mut rec = Rec::default();
    let report = run_text(&*s, "MULTI\nSET a 1\nHSET h f v\nEXEC", &local(), &mut rec).await;
    assert_eq!(report.stop, None);
    assert_eq!(rec.tx, [TxState::Active, TxState::Idle]);
    assert_eq!(s.command().unwrap().tx_state(), TxState::Idle);
    assert_eq!(store.get("0", b"a"), Some(FakeValue::Str("1".into())));
    assert_eq!(
        store.get("0", b"h"),
        Some(FakeValue::Hash(BTreeMap::from([("f".into(), "v".into())])))
    );

    // DISCARD drops what was queued.
    run_text(
        &*s,
        "MULTI\nSET z 1\nDISCARD",
        &local(),
        &mut Rec::default(),
    )
    .await;
    assert_eq!(store.get("0", b"z"), None);
    assert_eq!(s.command().unwrap().tx_state(), TxState::Idle);
}

#[tokio::test]
async fn flushdb_on_production_needs_db_name() {
    let (s, store) = connect().await;
    let c = s.command().unwrap();
    let mut plan = plan_commands("FLUSHDB", Target::All, c.language());
    guard_commands(&mut plan, &ctx(Some(EnvTag::Production), false));
    assert_eq!(plan.stmts[0].verdict, Verdict::Confirm);
    assert_eq!(plan.stmts[0].confirm_name.as_deref(), Some("0"));

    // Dropping a database confirms everywhere; read-only blocks it.
    guard_commands(&mut plan, &local());
    assert_eq!(plan.stmts[0].verdict, Verdict::Confirm);
    guard_commands(&mut plan, &ctx(None, true));
    assert_eq!(plan.stmts[0].verdict, Verdict::Block);

    store.set("0", b"k", FakeValue::Str("v".into()));
    store.set("1", b"k", FakeValue::Str("v".into()));
    run_text(&*s, "FLUSHDB", &local(), &mut Rec::default()).await;
    assert_eq!(store.get("0", b"k"), None);
    assert!(store.get("1", b"k").is_some(), "only the current db");
}

#[tokio::test]
async fn unknown_command_blocked_read_only() {
    let (s, store) = connect().await;
    let mut rec = Rec::default();
    let report = run_text(
        &*s,
        "GET a\nFROB x",
        &ctx(Some(EnvTag::Local), true),
        &mut rec,
    )
    .await;
    assert_eq!(report.stop, Some(StopReason::Error { index: 1 }));
    assert!(report.stmts.is_empty());
    assert!(store.executed().is_empty(), "nothing is sent");

    let c = s.command().unwrap();
    let mut plan = plan_commands("FROB x\nCONFIG GET maxmemory", Target::All, c.language());
    guard_commands(&mut plan, &ctx(Some(EnvTag::Production), false));
    assert_eq!(plan.stmts[0].verdict, Verdict::Confirm);
    assert_eq!(plan.stmts[0].confirm_name.as_deref(), Some("cache"));
    assert_eq!(plan.stmts[1].verdict, Verdict::Confirm, "Admin");
    assert_eq!(plan.stmts[1].confirm_name, None);

    // Run anyway (Local), the server refuses it.
    let mut rec = Rec::default();
    let report = run_text(&*s, "FROB x", &local(), &mut rec).await;
    assert_eq!(report.stop, Some(StopReason::Error { index: 0 }));
    assert_eq!(rec.ends, [(0, Err("server".to_string()))]);
}

#[tokio::test]
async fn parse_errors_block_with_an_offset() {
    let (s, store) = connect().await;
    let lang = s.command().unwrap().language();
    for (cmd, offset) in [
        ("GET", 3),
        ("GET a b", 6),
        ("SET k \"open", 6),
        ("SELECT 16", 7),
        ("SCAN x", 5),
        ("SCAN 0 COUNT 1", 7),
    ] {
        let err = lang.parse(cmd).unwrap_err();
        assert_eq!(err.offset, Some(offset), "{cmd}: {}", err.message);
    }
    let parsed = lang.parse("SET k \"a \\\"q\\\" \\\\ b\"").unwrap();
    assert_eq!(parsed.class, CommandClass::Write);

    let mut rec = Rec::default();
    let report = run_text(&*s, "SET a 1\nGET", &local(), &mut rec).await;
    assert_eq!(report.stop, Some(StopReason::Error { index: 1 }));
    assert!(store.executed().is_empty());
}

#[tokio::test]
async fn scan_pages_keys() {
    let (s, store) = connect().await;
    for k in ["user:3", "user:1", "order:1", "user:2"] {
        store.set("0", k.as_bytes(), FakeValue::Str("x".into()));
    }
    store.set(
        "0",
        b"user:h",
        FakeValue::Hash(BTreeMap::from([("f".into(), "v".into())])),
    );
    let ks = s.keyspace().unwrap();
    let req = |cursor: Option<String>| ScanRequest {
        cursor,
        pattern: Some("user:*".into()),
        key_type: None,
        count: 2,
    };
    let mut cursor = None;
    let mut pages = Vec::new();
    loop {
        let page = ks.scan("0", &req(cursor)).await.unwrap();
        pages.push(
            page.keys
                .iter()
                .map(|k| k.name.display.clone())
                .collect::<Vec<_>>(),
        );
        cursor = page.next_cursor;
        if cursor.is_none() {
            break;
        }
    }
    assert_eq!(pages, [vec!["user:1", "user:2"], vec!["user:3", "user:h"]]);

    let hashes = ks
        .scan(
            "0",
            &ScanRequest {
                cursor: None,
                pattern: None,
                key_type: Some(KeyType::Hash),
                count: 10,
            },
        )
        .await
        .unwrap();
    assert_eq!(hashes.keys.len(), 1);
    assert_eq!(hashes.keys[0].key_type, KeyType::Hash);
    assert!(ks.scan("1", &req(None)).await.unwrap().keys.is_empty());

    // The console's SCAN reads the same keys.
    let mut rec = Rec::default();
    run_text(&*s, "SCAN 0 MATCH order:*", &local(), &mut rec).await;
    assert_eq!(rec.rows, [vec![text("order:1")]]);
}

#[tokio::test]
async fn key_edit_builds_write_commands() {
    let (s, store) = connect().await;
    let c = s.command().unwrap();
    let key = KeyName::from_bytes(b"greeting".to_vec());
    let cmds = c
        .data_access()
        .build_key_edit(
            "3",
            &KeyEdit::SetString {
                key: key.clone(),
                value: "hi".into(),
            },
        )
        .unwrap();
    assert_eq!(cmds.len(), 1);
    assert_eq!(
        (cmds[0].name.as_str(), &cmds[0].class),
        ("SET", &CommandClass::Write)
    );
    assert!(check_read(&cmds[0]).is_err());
    let blocked = policy(&RiskFacts::from(&cmds[0]), &ctx(None, true));
    assert_eq!(blocked.verdict, Verdict::Block);

    let opts = ExecOptions::default();
    let mut rec = Rec::default();
    c.execute_command(&cmds[0], &opts, &mut rec).await.unwrap();
    assert_eq!(
        store.get("3", b"greeting"),
        Some(FakeValue::Str("hi".into()))
    );

    let da = c.data_access();
    let hset = KeyEdit::HashSet {
        key: KeyName::from_bytes(b"h".to_vec()),
        field: "f".into(),
        value: "v".into(),
    };
    let hdel = KeyEdit::HashDelete {
        key: KeyName::from_bytes(b"h".to_vec()),
        field: "f".into(),
    };
    for edit in [hset, hdel] {
        for cmd in da.build_key_edit("3", &edit).unwrap() {
            c.execute_command(&cmd, &opts, &mut rec).await.unwrap();
        }
    }
    assert_eq!(store.get("3", b"h"), Some(FakeValue::Hash(BTreeMap::new())));
    for cmd in da.build_key_edit("3", &KeyEdit::Delete { key }).unwrap() {
        c.execute_command(&cmd, &opts, &mut rec).await.unwrap();
    }
    assert_eq!(store.get("3", b"greeting"), None);

    let push = KeyEdit::ListPush {
        key: KeyName::from_bytes(b"l".to_vec()),
        value: "x".into(),
        head: true,
    };
    assert!(matches!(
        da.build_key_edit("3", &push),
        Err(DbError::Unsupported { .. })
    ));
}

#[tokio::test]
async fn redact_hides_requirepass() {
    let (s, _store) = connect().await;
    let lang = s.command().unwrap().language();
    let r = lang.redact("GET a\nconfig set requirepass s3cret\nCONFIG SET requirepass \"a b\"");
    assert_eq!(
        r.text,
        "GET a\nconfig set requirepass ***\nCONFIG SET requirepass ***"
    );
    assert!(r.changed);
    let plain = lang.redact("CONFIG GET requirepass\nSET requirepass x");
    assert_eq!(plain.text, "CONFIG GET requirepass\nSET requirepass x");
    assert!(!plain.changed);
}
