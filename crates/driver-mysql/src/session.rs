use crate::map_mysql_error;
use async_trait::async_trait;
use dnt_core::driver::{
    CancelHandle, Canceller, ExecOptions, ExecSummary, Introspector, RowSink, Session,
};
use dnt_core::error::{ConnectStage, DbError};
use dnt_core::meta::{
    DatabaseInfo, ObjectKind, ObjectRef, ObjectSummary, SchemaInfo, Scope, TableDetail,
};
use dnt_core::server::{MySqlCapabilities, ServerInfo};
use mysql_async::prelude::Queryable;
use mysql_async::{Conn, Opts};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::sync::Mutex;

/// One MySQL-family connection. It is its own [`Introspector`].
pub struct MySqlSession {
    conn: Mutex<Conn>,
    info: ServerInfo,
    caps: MySqlCapabilities,
    /// Options of the established connection (TLS fallback applied); used to reconnect (Task 10).
    #[allow(dead_code)]
    opts: Opts,
    in_tx: AtomicBool,
    /// Set when the user runs `SET autocommit = 0` (Task 10).
    #[allow(dead_code)]
    autocommit_off: AtomicBool,
}

impl MySqlSession {
    pub(crate) fn new(conn: Conn, info: ServerInfo, caps: MySqlCapabilities, opts: Opts) -> Self {
        MySqlSession {
            conn: Mutex::new(conn),
            info,
            caps,
            opts,
            in_tx: AtomicBool::new(false),
            autocommit_off: AtomicBool::new(false),
        }
    }

    pub fn capabilities(&self) -> MySqlCapabilities {
        self.caps
    }

    /// Test-only (`test-helpers` feature): runs one statement and returns its first row as
    /// text (`None` for SQL NULL). Bypasses transaction tracking; `execute` is the real path.
    #[cfg(feature = "test-helpers")]
    pub async fn query_row_text(&self, sql: &str) -> Result<Option<Vec<Option<String>>>, DbError> {
        let mut conn = self.conn.lock().await;
        let row: Option<mysql_async::Row> = conn
            .query_first(sql)
            .await
            .map_err(|e| map_mysql_error(e, ConnectStage::Execute))?;
        Ok(row.map(|r| r.unwrap().into_iter().map(value_text).collect()))
    }

    fn unsupported(&self, feature: &str) -> DbError {
        unsupported(feature, &self.info.version_string)
    }
}

fn unsupported(feature: &str, version: &str) -> DbError {
    DbError::Unsupported {
        feature: feature.into(),
        version: version.into(),
    }
}

#[cfg(feature = "test-helpers")]
fn value_text(v: mysql_async::Value) -> Option<String> {
    use mysql_async::Value;
    match v {
        Value::NULL => None,
        Value::Bytes(b) => Some(String::from_utf8_lossy(&b).into_owned()),
        other => Some(other.as_sql(true)),
    }
}

/// Placeholder until Task 10 implements `KILL QUERY` on a temporary connection.
struct PendingCanceller {
    version: String,
}

#[async_trait]
impl Canceller for PendingCanceller {
    async fn cancel(&self) -> Result<(), DbError> {
        Err(unsupported("cancel", &self.version))
    }
}

#[async_trait]
impl Session for MySqlSession {
    fn server_info(&self) -> &ServerInfo {
        &self.info
    }

    /// Implemented in Task 10.
    async fn execute(
        &self,
        _sql: &str,
        _opts: ExecOptions,
        _sink: &mut dyn RowSink,
    ) -> Result<ExecSummary, DbError> {
        Err(self.unsupported("execute"))
    }

    fn cancel_handle(&self) -> CancelHandle {
        Arc::new(PendingCanceller {
            version: self.info.version_string.clone(),
        })
    }

    fn introspector(&self) -> &dyn Introspector {
        self
    }

    fn in_transaction(&self) -> bool {
        self.in_tx.load(Ordering::Relaxed)
    }

    async fn ping(&self) -> Result<(), DbError> {
        self.conn
            .lock()
            .await
            .ping()
            .await
            .map_err(|e| map_mysql_error(e, ConnectStage::Execute))
    }

    async fn close(self: Box<Self>) {
        // Best effort: the server drops the session either way.
        let _ = self.conn.into_inner().disconnect().await;
    }
}

/// Implemented in Task 11.
#[async_trait]
impl Introspector for MySqlSession {
    async fn list_databases(&self) -> Result<Vec<DatabaseInfo>, DbError> {
        Err(self.unsupported("list_databases"))
    }

    async fn list_schemas(&self, _db: &str) -> Result<Vec<SchemaInfo>, DbError> {
        Err(self.unsupported("list_schemas"))
    }

    async fn list_objects(
        &self,
        _scope: &Scope,
        _kind: ObjectKind,
    ) -> Result<Vec<ObjectSummary>, DbError> {
        Err(self.unsupported("list_objects"))
    }

    async fn table_detail(&self, _obj: &ObjectRef) -> Result<TableDetail, DbError> {
        Err(self.unsupported("table_detail"))
    }

    async fn object_ddl(&self, _obj: &ObjectRef) -> Result<String, DbError> {
        Err(self.unsupported("object_ddl"))
    }
}
