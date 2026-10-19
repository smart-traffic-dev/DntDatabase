use crate::config::ResolvedConfig;
use crate::error::DbError;
use crate::meta::{
    DatabaseInfo, ObjectKind, ObjectRef, ObjectSummary, SchemaInfo, Scope, TableColumns,
    TableDetail,
};
use crate::server::{DbKind, ServerInfo};
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
    /// Raised by the user's Stop: no statement may be sent once it is set.
    pub stop: StopSignal,
}

impl Default for ExecOptions {
    fn default() -> Self {
        ExecOptions {
            max_rows: 10_000,
            batch_size: 500,
            timeout: None,
            cancel_grace: DEFAULT_CANCEL_GRACE,
            stop: StopSignal::new(),
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

/// Progress of a statement being stopped, reported through [`RowSink::progress`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecProgress {
    /// The server was asked to stop the statement.
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
    /// Runs a (possibly multi-statement) script. The driver splits it by its dialect
    /// and calls `result_start` / `rows` / `result_end` on `sink` for every statement.
    async fn execute(
        &self,
        sql: &str,
        opts: ExecOptions,
        sink: &mut dyn RowSink,
    ) -> Result<ExecSummary, DbError>;
    fn cancel_handle(&self) -> CancelHandle;
    /// Session types implement `Introspector` themselves and return `self`.
    fn introspector(&self) -> &dyn Introspector;
    fn in_transaction(&self) -> bool;
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
        struct Ends(Vec<ResultSummary>);
        impl RowSink for Ends {
            fn result_start(&mut self, _cols: &[ColumnMeta]) {}
            fn rows(&mut self, _batch: Vec<Vec<Value>>) {}
            fn result_end(&mut self, summary: &ResultSummary) {
                self.0.push(summary.clone());
            }
        }
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

    /// A session that is its own introspector, as the drivers are built.
    struct FakeSession {
        info: ServerInfo,
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
        async fn execute(
            &self,
            _sql: &str,
            _opts: ExecOptions,
            sink: &mut dyn RowSink,
        ) -> Result<ExecSummary, DbError> {
            sink.result_start(&[]);
            sink.result_end(&ResultSummary {
                affected: Some(0),
                row_count: 0,
                truncated: false,
                elapsed_ms: 0,
            });
            Ok(ExecSummary::default())
        }
        fn cancel_handle(&self) -> CancelHandle {
            Arc::new(NoopCancel)
        }
        fn introspector(&self) -> &dyn Introspector {
            self
        }
        fn in_transaction(&self) -> bool {
            false
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
        let s: Box<dyn Session> = Box::new(FakeSession {
            info: detect_pg(170002, "PostgreSQL 17.2"),
        });
        let i: &dyn Introspector = s.introspector();
        assert!(std::ptr::addr_eq(
            i as *const dyn Introspector,
            &*s as *const dyn Session
        ));
        assert!(!s.in_transaction());
    }
}
