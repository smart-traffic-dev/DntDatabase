use crate::config::ResolvedConfig;
use crate::error::DbError;
use crate::meta::{
    DatabaseInfo, ObjectKind, ObjectRef, ObjectSummary, SchemaInfo, Scope, TableColumns,
    TableDetail,
};
use crate::server::{DbKind, ServerInfo};
use crate::sql::{classify, split, Dialect, StatementKind};
use crate::tx::TxState;
use crate::value::{ColumnMeta, Value};
use async_trait::async_trait;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// How long a driver waits, after it asked the server to stop a statement (timeout or
/// Stop), for the statement to end before it gives the connection up.
pub const DEFAULT_CANCEL_GRACE: Duration = Duration::from_secs(10);

/// A flag the query layer raises to stop a run; every clone shares it. Two signals are
/// equal only when they are the same flag.
#[derive(Clone, Debug, Default)]
pub struct StopSignal(Arc<AtomicBool>);

impl StopSignal {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn stop(&self) {
        self.0.store(true, Ordering::SeqCst);
    }

    pub fn is_stopped(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

impl PartialEq for StopSignal {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

impl Eq for StopSignal {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecOptions {
    pub max_rows: usize,
    pub batch_size: usize,
    /// Per-statement time limit.
    pub timeout: Option<Duration>,
    /// See [`DEFAULT_CANCEL_GRACE`]; tests set it shorter.
    pub cancel_grace: Duration,
    /// Raised by the user's Stop: no statement may be sent once it is set. Use one
    /// signal per run (every statement of a script shares it): a cancel aimed at a
    /// running statement raises it too, and it stays raised, so a later run needs a
    /// fresh signal.
    pub stop: StopSignal,
    /// Values bound to the statement's placeholders (`?` on MySQL, `$1…` on
    /// PostgreSQL), in order, each sent as text: MySQL converts it like a string
    /// literal, PostgreSQL parses it with the input function of the type it infers for
    /// the placeholder (so `"col" = $1` reads the text as the column's type). Empty:
    /// the statement runs unprepared, as before (the user's SQL never has params).
    pub params: Vec<String>,
    /// `Value::Bytes` carries every byte in `preview` instead of the first 64 (export).
    pub full_bytes: bool,
}

impl Default for ExecOptions {
    fn default() -> Self {
        ExecOptions {
            max_rows: 10_000,
            batch_size: 500,
            timeout: None,
            cancel_grace: DEFAULT_CANCEL_GRACE,
            stop: StopSignal::new(),
            params: Vec::new(),
            full_bytes: false,
        }
    }
}

/// What one executed statement produced.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct StmtOutcome {
    /// One entry per result (a MySQL `CALL` may return several result sets).
    pub results: Vec<ResultSummary>,
    /// The statement completed within the cancel grace after its timeout: its results
    /// are kept, and the script must not continue.
    pub late_completion: bool,
}

/// Most server messages kept per statement ([`ServerNotices`]); the rest are counted.
pub const MAX_SERVER_NOTICES: usize = 50;

/// A message the server sent about a statement besides its result: a PostgreSQL
/// `NOTICE` / `WARNING` / `INFO`, a MySQL `Warning` / `Note` (`SHOW WARNINGS`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerNotice {
    /// The level as the server names it (`NOTICE`, `WARNING`, `Warning`, `Note`…).
    pub level: String,
    /// The MySQL code (`1292`); `None` on PostgreSQL.
    pub code: Option<String>,
    pub message: String,
}

impl ServerNotice {
    /// The Messages line: `NOTICE: …`, `Warning 1292: …`.
    pub fn line(&self) -> String {
        match &self.code {
            Some(code) => format!("{} {code}: {}", self.level, self.message),
            None => format!("{}: {}", self.level, self.message),
        }
    }
}

/// The server messages of one statement, in order: the first [`MAX_SERVER_NOTICES`]
/// kept, the rest only counted (`omitted`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ServerNotices {
    pub kept: Vec<ServerNotice>,
    pub omitted: u64,
}

impl ServerNotices {
    pub fn push(&mut self, notice: ServerNotice) {
        if self.kept.len() < MAX_SERVER_NOTICES {
            self.kept.push(notice);
        } else {
            self.omitted = self.omitted.saturating_add(1);
        }
    }

    /// Counts `n` messages that were not read at all.
    pub fn add_omitted(&mut self, n: u64) {
        self.omitted = self.omitted.saturating_add(n);
    }

    /// Appends `other` (kept up to the bound, the rest counted).
    pub fn extend(&mut self, other: ServerNotices) {
        for n in other.kept {
            self.push(n);
        }
        self.add_omitted(other.omitted);
    }

    pub fn is_empty(&self) -> bool {
        self.kept.is_empty() && self.omitted == 0
    }

    /// One Messages line per kept message, then one for the omitted ones.
    pub fn lines(&self) -> Vec<String> {
        let mut lines: Vec<String> = self.kept.iter().map(ServerNotice::line).collect();
        match self.omitted {
            0 => {}
            1 => lines.push("1 more server message not shown".into()),
            n => lines.push(format!("{n} more server messages not shown")),
        }
        lines
    }
}

/// Progress of a statement being stopped, reported through [`RowSink::progress`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecProgress {
    /// Stopping the statement was requested (its timeout expired, or a user's cancel
    /// targeted it). Emitted at once, before the cancel request is actually sent; the
    /// cancel grace runs from here.
    CancelSent,
    /// The statement did not stop within the cancel grace.
    GraceExpired,
}

/// Outcome of one statement of an executed script.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResultSummary {
    pub affected: Option<u64>,
    pub row_count: u64,
    pub truncated: bool,
    pub elapsed_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ExecSummary {
    pub results: Vec<ResultSummary>,
}

#[async_trait]
pub trait Canceller: Send + Sync {
    async fn cancel(&self) -> Result<(), DbError>;
}

pub type CancelHandle = Arc<dyn Canceller>;

#[async_trait]
pub trait Driver: Send + Sync {
    fn kind(&self) -> DbKind;
    async fn connect(&self, cfg: &ResolvedConfig) -> Result<Box<dyn Session>, DbError>;
}

#[async_trait]
pub trait Session: Send + Sync {
    fn server_info(&self) -> &ServerInfo;
    /// Runs ONE statement (`kind`: its [`classify`] result) and calls `result_start` /
    /// `rows` / `result_end` on `sink` for every result it produces (a MySQL `CALL` may
    /// produce several), and `progress` while it is being stopped.
    ///
    /// Nothing is sent when `opts.stop` is raised (or a cancel targeted this call)
    /// before the statement starts: `Cancelled { outcome_unknown: false, .. }`.
    /// `opts.timeout` applies to this statement; once it expires, or a cancel targets
    /// it, the driver waits `opts.cancel_grace` for the statement to end before giving
    /// its connection up. Every `Timeout` / `Cancelled` carries `was_in_tx`.
    async fn execute_one(
        &self,
        stmt: &str,
        kind: StatementKind,
        opts: &ExecOptions,
        sink: &mut dyn RowSink,
    ) -> Result<StmtOutcome, DbError>;
    /// Runs a (possibly multi-statement) script: splits it by [`Session::dialect`],
    /// classifies each statement and runs it with [`Session::execute_one`], stopping at
    /// the first error.
    ///
    /// A [`Canceller::cancel`] only targets a statement that is running: one that lands
    /// between two statements is a no-op and the script goes on. To stop a script for
    /// sure, raise `opts.stop` as well.
    async fn execute(
        &self,
        sql: &str,
        opts: ExecOptions,
        sink: &mut dyn RowSink,
    ) -> Result<ExecSummary, DbError> {
        let dialect = self.dialect();
        let mut summary = ExecSummary::default();
        for stmt in split(sql, dialect) {
            let kind = classify(&stmt.text, dialect);
            let outcome = self.execute_one(&stmt.text, kind, &opts, sink).await?;
            summary.results.extend(outcome.results);
        }
        Ok(summary)
    }
    fn tx_state(&self) -> TxState;
    /// The dialect statements of this session are split and classified with.
    fn dialect(&self) -> Dialect;
    fn in_transaction(&self) -> bool {
        self.tx_state() != TxState::Idle
    }
    /// How many times this session replaced its connection with a fresh one, in all.
    /// The query layer compares it around each statement to tell the user about
    /// reconnects that were otherwise silent (a statement that succeeded on a new
    /// connection). Default: 0 (a session that never reconnects).
    fn reconnects(&self) -> u64 {
        0
    }
    fn cancel_handle(&self) -> CancelHandle;
    /// Session types implement `Introspector` themselves and return `self`.
    fn introspector(&self) -> &dyn Introspector;
    async fn ping(&self) -> Result<(), DbError>;
    async fn close(self: Box<Self>);
}

pub trait RowSink: Send {
    fn result_start(&mut self, cols: &[ColumnMeta]);
    fn rows(&mut self, batch: Vec<Vec<Value>>);
    /// The result that `result_start` opened is complete (`summary` is also returned
    /// in the call's summary).
    fn result_end(&mut self, summary: &ResultSummary);
    /// Progress while a statement is being stopped. Default: ignored.
    fn progress(&mut self, _p: ExecProgress) {}
    /// The messages the server sent about the statement (notices, warnings), at most
    /// once per `execute_one`, before it returns (also when it fails). Default: ignored.
    fn server_notices(&mut self, _notices: ServerNotices) {}
}

/// A [`RowSink`] shared between a running statement (which streams into `&SharedSink`)
/// and the driver code waiting on it, which reports [`ExecProgress`] meanwhile. Every
/// call locks only for its own duration, never across an `.await`.
pub struct SharedSink<'a> {
    inner: std::sync::Mutex<&'a mut dyn RowSink>,
}

impl<'a> SharedSink<'a> {
    pub fn new(sink: &'a mut dyn RowSink) -> Self {
        SharedSink {
            inner: std::sync::Mutex::new(sink),
        }
    }

    fn with<R>(&self, f: impl FnOnce(&mut dyn RowSink) -> R) -> R {
        let mut sink = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        f(&mut **sink)
    }

    pub fn progress(&self, p: ExecProgress) {
        self.with(|s| s.progress(p));
    }
}

impl RowSink for &SharedSink<'_> {
    fn result_start(&mut self, cols: &[ColumnMeta]) {
        self.with(|s| s.result_start(cols));
    }
    fn rows(&mut self, batch: Vec<Vec<Value>>) {
        self.with(|s| s.rows(batch));
    }
    fn result_end(&mut self, summary: &ResultSummary) {
        self.with(|s| s.result_end(summary));
    }
    fn progress(&mut self, p: ExecProgress) {
        SharedSink::progress(self, p);
    }
    fn server_notices(&mut self, notices: ServerNotices) {
        self.with(|s| s.server_notices(notices));
    }
}

#[async_trait]
pub trait Introspector: Send + Sync {
    async fn list_databases(&self) -> Result<Vec<DatabaseInfo>, DbError>;
    async fn list_schemas(&self, db: &str) -> Result<Vec<SchemaInfo>, DbError>;
    async fn list_objects(
        &self,
        scope: &Scope,
        kind: ObjectKind,
    ) -> Result<Vec<ObjectSummary>, DbError>;
    /// Column names and types of every table and view of `scope` the user can see, in ONE
    /// query (the SchemaCache needs nothing more; no statistics are read).
    async fn list_columns(&self, scope: &Scope) -> Result<Vec<TableColumns>, DbError>;
    async fn table_detail(&self, obj: &ObjectRef) -> Result<TableDetail, DbError>;
    async fn object_ddl(&self, obj: &ObjectRef) -> Result<String, DbError>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::detect_pg;

    #[test]
    fn exec_options_defaults() {
        let o = ExecOptions::default();
        assert_eq!(
            (o.max_rows, o.batch_size, o.timeout, o.cancel_grace),
            (10_000, 500, None, Duration::from_secs(10))
        );
        assert_eq!(o.cancel_grace, DEFAULT_CANCEL_GRACE);
        assert!(!o.stop.is_stopped());
    }

    #[test]
    fn stop_signal_is_shared_across_clones() {
        let a = StopSignal::new();
        let b = a.clone();
        assert!(!a.is_stopped());
        b.stop();
        assert!(a.is_stopped());
        assert_eq!(a, b);
        assert_ne!(a, StopSignal::new());
    }

    #[test]
    fn stmt_outcome_default_is_empty() {
        let o = StmtOutcome::default();
        assert!(o.results.is_empty());
        assert!(!o.late_completion);
    }

    /// A sink that only implements the required methods gets a no-op `progress`.
    #[test]
    fn row_sink_progress_defaults_to_noop() {
        let mut s = Ends(vec![]);
        s.progress(ExecProgress::CancelSent);
        s.progress(ExecProgress::GraceExpired);
        let summary = ResultSummary {
            affected: Some(2),
            row_count: 0,
            truncated: false,
            elapsed_ms: 5,
        };
        s.result_end(&summary);
        assert_eq!(s.0, [summary]);
    }

    fn notice(level: &str, code: Option<&str>, message: &str) -> ServerNotice {
        ServerNotice {
            level: level.into(),
            code: code.map(Into::into),
            message: message.into(),
        }
    }

    #[test]
    fn server_notice_lines_name_the_level() {
        assert_eq!(notice("NOTICE", None, "hello").line(), "NOTICE: hello");
        assert_eq!(
            notice(
                "Warning",
                Some("1292"),
                "Truncated incorrect INTEGER value: 'abc'"
            )
            .line(),
            "Warning 1292: Truncated incorrect INTEGER value: 'abc'"
        );
    }

    /// At most [`MAX_SERVER_NOTICES`] are kept; the rest are counted and summed up in
    /// one last line.
    #[test]
    fn server_notices_are_bounded() {
        let mut n = ServerNotices::default();
        assert!(n.is_empty());
        assert!(n.lines().is_empty());
        for i in 0..MAX_SERVER_NOTICES + 3 {
            n.push(notice("NOTICE", None, &format!("n{i}")));
        }
        assert_eq!(n.kept.len(), MAX_SERVER_NOTICES);
        assert_eq!(n.omitted, 3);
        let lines = n.lines();
        assert_eq!(lines.len(), MAX_SERVER_NOTICES + 1);
        assert_eq!(lines[0], "NOTICE: n0");
        assert_eq!(
            lines[MAX_SERVER_NOTICES],
            "3 more server messages not shown"
        );
        n.add_omitted(1);
        assert_eq!(
            n.lines().last().unwrap(),
            "4 more server messages not shown"
        );
        let mut one = ServerNotices::default();
        one.add_omitted(1);
        assert!(!one.is_empty());
        assert_eq!(one.lines(), ["1 more server message not shown"]);
    }

    /// A sink without `server_notices` ignores them; `&SharedSink` forwards them.
    #[test]
    fn server_notices_reach_the_sink() {
        struct Keep(Vec<ServerNotices>);
        impl RowSink for Keep {
            fn result_start(&mut self, _cols: &[ColumnMeta]) {}
            fn rows(&mut self, _batch: Vec<Vec<Value>>) {}
            fn result_end(&mut self, _summary: &ResultSummary) {}
            fn server_notices(&mut self, notices: ServerNotices) {
                self.0.push(notices);
            }
        }
        let mut batch = ServerNotices::default();
        batch.push(notice("WARNING", None, "careful"));
        Ends(vec![]).server_notices(batch.clone());
        let mut keep = Keep(vec![]);
        {
            let shared = SharedSink::new(&mut keep);
            let mut handle = &shared;
            handle.server_notices(batch.clone());
        }
        assert_eq!(keep.0, [batch]);
    }

    /// A session that is its own introspector, as the drivers are built. Records the
    /// statements it is given; one containing `fail` fails.
    struct FakeSession {
        info: ServerInfo,
        seen: std::sync::Mutex<Vec<(String, StatementKind)>>,
    }

    fn fake() -> FakeSession {
        FakeSession {
            info: detect_pg(170002, "PostgreSQL 17.2"),
            seen: std::sync::Mutex::new(vec![]),
        }
    }

    struct Ends(Vec<ResultSummary>);
    impl RowSink for Ends {
        fn result_start(&mut self, _cols: &[ColumnMeta]) {}
        fn rows(&mut self, _batch: Vec<Vec<Value>>) {}
        fn result_end(&mut self, summary: &ResultSummary) {
            self.0.push(summary.clone());
        }
    }

    #[derive(Default)]
    struct Log(Vec<String>);
    impl RowSink for Log {
        fn result_start(&mut self, cols: &[ColumnMeta]) {
            self.0.push(format!("start {}", cols.len()));
        }
        fn rows(&mut self, batch: Vec<Vec<Value>>) {
            self.0.push(format!("rows {}", batch.len()));
        }
        fn result_end(&mut self, summary: &ResultSummary) {
            self.0.push(format!("end {}", summary.row_count));
        }
        fn progress(&mut self, p: ExecProgress) {
            self.0.push(format!("{p:?}"));
        }
    }

    /// Calls through a `&SharedSink` and the sink's own `progress` reach the same sink,
    /// in order.
    #[test]
    fn shared_sink_forwards_every_call() {
        let mut log = Log::default();
        {
            let shared = SharedSink::new(&mut log);
            let mut handle = &shared;
            handle.result_start(&[]);
            handle.rows(vec![vec![Value::Null], vec![Value::Null]]);
            shared.progress(ExecProgress::CancelSent);
            handle.result_end(&ResultSummary {
                affected: None,
                row_count: 2,
                truncated: false,
                elapsed_ms: 0,
            });
            handle.progress(ExecProgress::GraceExpired);
        }
        assert_eq!(
            log.0,
            ["start 0", "rows 2", "CancelSent", "end 2", "GraceExpired"]
        );
    }

    /// The default `execute` splits by the session's dialect, classifies each
    /// statement, runs it with `execute_one` and stops at the first error.
    #[tokio::test]
    async fn default_execute_runs_statements_until_the_first_error() {
        let s = fake();
        let mut sink = Ends(vec![]);
        let summary = s
            .execute("SELECT 1; BEGIN", ExecOptions::default(), &mut sink)
            .await
            .unwrap();
        assert_eq!(summary.results.len(), 2);
        assert_eq!(sink.0.len(), 2);
        assert_eq!(
            *s.seen.lock().unwrap(),
            [
                ("SELECT 1".to_string(), StatementKind::Read),
                (
                    "BEGIN".to_string(),
                    StatementKind::Transaction(crate::sql::TxControl::Begin)
                ),
            ]
        );
        s.seen.lock().unwrap().clear();
        let err = s
            .execute(
                "SELECT 1; SELECT fail; SELECT 3",
                ExecOptions::default(),
                &mut sink,
            )
            .await;
        assert!(matches!(err, Err(DbError::Internal { .. })), "{err:?}");
        assert_eq!(s.seen.lock().unwrap().len(), 2);
    }

    struct NoopCancel;

    #[async_trait]
    impl Canceller for NoopCancel {
        async fn cancel(&self) -> Result<(), DbError> {
            Ok(())
        }
    }

    #[async_trait]
    impl Session for FakeSession {
        fn server_info(&self) -> &ServerInfo {
            &self.info
        }
        async fn execute_one(
            &self,
            stmt: &str,
            kind: StatementKind,
            _opts: &ExecOptions,
            sink: &mut dyn RowSink,
        ) -> Result<StmtOutcome, DbError> {
            self.seen.lock().unwrap().push((stmt.to_string(), kind));
            if stmt.contains("fail") {
                return Err(DbError::Internal {
                    message: "fail".into(),
                });
            }
            let summary = ResultSummary {
                affected: Some(0),
                row_count: 0,
                truncated: false,
                elapsed_ms: 0,
            };
            sink.result_start(&[]);
            sink.result_end(&summary);
            Ok(StmtOutcome {
                results: vec![summary],
                late_completion: false,
            })
        }
        fn tx_state(&self) -> TxState {
            TxState::Idle
        }
        fn dialect(&self) -> Dialect {
            Dialect::Postgres
        }
        fn cancel_handle(&self) -> CancelHandle {
            Arc::new(NoopCancel)
        }
        fn introspector(&self) -> &dyn Introspector {
            self
        }
        async fn ping(&self) -> Result<(), DbError> {
            Ok(())
        }
        async fn close(self: Box<Self>) {}
    }

    #[async_trait]
    impl Introspector for FakeSession {
        async fn list_databases(&self) -> Result<Vec<DatabaseInfo>, DbError> {
            Ok(vec![DatabaseInfo { name: "app".into() }])
        }
        async fn list_schemas(&self, _db: &str) -> Result<Vec<SchemaInfo>, DbError> {
            Ok(vec![])
        }
        async fn list_objects(
            &self,
            _scope: &Scope,
            _kind: ObjectKind,
        ) -> Result<Vec<ObjectSummary>, DbError> {
            Ok(vec![])
        }
        async fn list_columns(&self, _scope: &Scope) -> Result<Vec<TableColumns>, DbError> {
            Ok(vec![])
        }
        async fn table_detail(&self, _obj: &ObjectRef) -> Result<TableDetail, DbError> {
            Err(DbError::Internal {
                message: "n/a".into(),
            })
        }
        async fn object_ddl(&self, _obj: &ObjectRef) -> Result<String, DbError> {
            Ok(String::new())
        }
    }

    #[test]
    fn session_can_be_its_own_introspector() {
        let s: Box<dyn Session> = Box::new(fake());
        let i: &dyn Introspector = s.introspector();
        assert!(std::ptr::addr_eq(
            i as *const dyn Introspector,
            &*s as *const dyn Session
        ));
        assert!(!s.in_transaction());
    }
}
