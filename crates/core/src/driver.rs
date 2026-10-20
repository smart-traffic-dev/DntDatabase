use crate::config::ResolvedConfig;
use crate::error::DbError;
use crate::meta::{
    DatabaseInfo, ObjectKind, ObjectRef, ObjectSummary, SchemaInfo, Scope, TableColumns,
    TableDetail,
};
use crate::server::{DbKind, ServerInfo};
use crate::value::{ColumnMeta, Value};
use async_trait::async_trait;
use std::sync::Arc;
use std::time::Duration;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecOptions {
    pub max_rows: usize,
    pub batch_size: usize,
    pub timeout: Option<Duration>,
}

impl Default for ExecOptions {
    fn default() -> Self {
        ExecOptions {
            max_rows: 10_000,
            batch_size: 500,
            timeout: None,
        }
    }
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
    fn result_end(&mut self, affected: Option<u64>, elapsed: Duration);
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
    /// Column names of every table and view of `scope` the user can see, in ONE query
    /// (the SchemaCache needs names only; no statistics are read).
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
        assert_eq!((o.max_rows, o.batch_size, o.timeout), (10_000, 500, None));
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
            sink.result_end(Some(0), Duration::ZERO);
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
