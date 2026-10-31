//! Keys of foreign keys' parent tables (spec §5.3): `SELECT DISTINCT <ref cols> FROM
//! <parent> WHERE <ref cols> IS NOT NULL ORDER BY <ref cols> LIMIT 10000`, and the cache the
//! preview reads through so a debounced preview does not query the parent each time.
use crate::session_manager::{lock, METADATA_CALL_TIMEOUT};
use dnt_core::driver::{ExecOptions, ResultSummary, RowSink, SqlSession};
use dnt_core::error::{ConnectStage, DbError};
use dnt_core::meta::{ForeignKeyDetail, ObjectRef};
use dnt_core::server::SqlKind;
use dnt_core::sql::build::TableRef;
use dnt_core::sql::classify;
use dnt_core::sql::ident::quote_ident;
use dnt_core::value::{ColumnMeta, FullValue, Value};
use std::collections::HashMap;
use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use uuid::Uuid;

/// Most key tuples read per foreign key.
pub(crate) const MAX_KEYS: usize = 10_000;
/// How long the preview reuses keys it read.
pub(crate) const KEY_CACHE_TTL: Duration = Duration::from_secs(60);
/// Key sets the cache keeps at once.
pub(crate) const KEY_CACHE_MAX: usize = 32;

/// The parent table of `fk` on a child table `child`: MySQL names the parent's database in
/// `ref_schema` (default: the child's), PostgreSQL its schema (default: the child's).
pub(crate) fn parent_ref(kind: SqlKind, child: &ObjectRef, fk: &ForeignKeyDetail) -> TableRef {
    match kind {
        SqlKind::MySql => TableRef {
            kind,
            database: fk
                .ref_schema
                .clone()
                .unwrap_or_else(|| child.database.clone()),
            schema: None,
            name: fk.ref_table.clone(),
        },
        SqlKind::Postgres => TableRef {
            kind,
            database: child.database.clone(),
            schema: fk.ref_schema.clone().or_else(|| child.schema.clone()),
            name: fk.ref_table.clone(),
        },
    }
}

/// The key read of `fk` (identifiers quoted; NULL tuples left out; ordered so the preview
/// is reproducible).
pub(crate) fn fk_key_sql(kind: SqlKind, child: &ObjectRef, fk: &ForeignKeyDetail) -> String {
    let cols: Vec<String> = fk
        .ref_columns
        .iter()
        .map(|c| quote_ident(kind, c))
        .collect();
    let not_null: Vec<String> = cols.iter().map(|c| format!("{c} IS NOT NULL")).collect();
    format!(
        "SELECT DISTINCT {list} FROM {rel} WHERE {cond} ORDER BY {list} LIMIT {MAX_KEYS}",
        list = cols.join(", "),
        rel = parent_ref(kind, child, fk).relation(),
        cond = not_null.join(" AND "),
    )
}

/// The rows of the first result, as complete values.
#[derive(Default)]
struct Rows(Vec<Vec<Value>>);

impl RowSink for Rows {
    fn result_start(&mut self, _: &[ColumnMeta]) {}
    fn rows(&mut self, rows: Vec<Vec<Value>>) {
        self.0.extend(rows);
    }
    fn result_end(&mut self, _: &ResultSummary) {}
}

/// Reads the keys of `fk` on `session` (bounded by [`METADATA_CALL_TIMEOUT`]). A tuple with
/// an incomplete value is left out.
pub(crate) async fn read_fk_keys(
    session: &dyn SqlSession,
    child: &ObjectRef,
    fk: &ForeignKeyDetail,
) -> Result<Vec<Vec<FullValue>>, DbError> {
    let sql = fk_key_sql(session.sql_kind(), child, fk);
    let opts = ExecOptions {
        max_rows: MAX_KEYS,
        full_bytes: true,
        ..ExecOptions::default()
    };
    let mut rows = Rows::default();
    tokio::time::timeout(
        METADATA_CALL_TIMEOUT,
        session.execute_one(&sql, classify(&sql, session.dialect()), &opts, &mut rows),
    )
    .await
    .map_err(|_| DbError::timeout(ConnectStage::Execute))??;
    Ok(rows
        .0
        .into_iter()
        .filter_map(|row| {
            row.into_iter()
                .map(|v| FullValue::try_from_value(v).ok())
                .collect::<Option<Vec<_>>>()
        })
        .collect())
}

/// What a cached key set was read for: the connection, its tunnel generation, the session
/// database (PostgreSQL relations do not name it) and the exact key query.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct KeyId {
    pub(crate) conn: Uuid,
    pub(crate) generation: u64,
    pub(crate) database: String,
    pub(crate) sql: String,
}

/// The cache key of the keys of `fk` on table `child`.
pub(crate) fn key_id(
    conn: Uuid,
    generation: u64,
    kind: SqlKind,
    child: &ObjectRef,
    fk: &ForeignKeyDetail,
) -> KeyId {
    KeyId {
        conn,
        generation,
        database: child.database.clone(),
        sql: fk_key_sql(kind, child, fk),
    }
}

type Keys = Arc<Vec<Vec<FullValue>>>;

/// Key sets the preview read recently (spec §5.3: 60 s, 32 entries). The job never reads
/// through it.
#[derive(Default)]
pub struct SeedKeyCache {
    entries: Mutex<HashMap<KeyId, (Instant, Keys)>>,
}

impl SeedKeyCache {
    /// The cached keys of `id`, or `read`'s (then cached unless empty: an empty parent is
    /// read again next time, the user may be filling it).
    pub(crate) async fn get_or_read(
        &self,
        id: KeyId,
        read: impl Future<Output = Result<Vec<Vec<FullValue>>, DbError>>,
    ) -> Result<Keys, DbError> {
        {
            let mut entries = lock(&self.entries);
            entries.retain(|_, (at, _)| at.elapsed() < KEY_CACHE_TTL);
            if let Some((_, keys)) = entries.get(&id) {
                return Ok(Arc::clone(keys));
            }
        }
        let keys = Arc::new(read.await?);
        if keys.is_empty() {
            return Ok(keys);
        }
        let mut entries = lock(&self.entries);
        if entries.len() >= KEY_CACHE_MAX {
            if let Some(oldest) = entries
                .iter()
                .min_by_key(|(_, (at, _))| *at)
                .map(|(k, _)| k.clone())
            {
                entries.remove(&oldest);
            }
        }
        entries.insert(id, (Instant::now(), Arc::clone(&keys)));
        Ok(keys)
    }

    /// Closing `conn`: its key sets are dropped.
    pub(crate) fn drop_conn(&self, conn: Uuid) {
        lock(&self.entries).retain(|k, _| k.conn != conn);
    }
}
