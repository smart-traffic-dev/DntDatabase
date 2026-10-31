//! The import target (R13): `table_detail` for nullability, defaults, identity, generated
//! columns, enum values, keys and the engine; the driver's logical type of every column
//! from the metadata of `SELECT <cols> FROM <t> WHERE 1 = 0` on the same session.
use super::{session_db, table_ref, ImportTarget};
use crate::session_manager::{close_quietly, config_error, SessionManager, METADATA_CALL_TIMEOUT};
use dnt_core::driver::{ExecOptions, ResultSummary, RowSink, Session};
use dnt_core::error::DbError;
use dnt_core::meta::{ObjectRef, TableDetail};
use dnt_core::sql::classify;
use dnt_core::sql::ident::quote_ident;
use dnt_core::value::{ColumnMeta, Value};
use dnt_query::Atomicity;
use dnt_transfer::convert::TargetColumn;
use dnt_transfer::mapping::{key_choices, KeyChoice};
use uuid::Uuid;

/// What an import needs to know about its table.
#[derive(Debug, Clone)]
pub(crate) struct TargetRead {
    pub(crate) detail: TableDetail,
    pub(crate) columns: Vec<TargetColumn>,
    pub(crate) keys: Vec<KeyChoice>,
    pub(crate) atomicity: Atomicity,
    /// `dnt_query::fingerprint` of the detail: a plan built on it is stale once it changes.
    pub(crate) fingerprint: u64,
}

/// The column metadata of the first result.
#[derive(Default)]
struct Metas(Option<Vec<ColumnMeta>>);

impl RowSink for Metas {
    fn result_start(&mut self, cols: &[ColumnMeta]) {
        if self.0.is_none() {
            self.0 = Some(cols.to_vec());
        }
    }
    fn rows(&mut self, _: Vec<Vec<Value>>) {}
    fn result_end(&mut self, _: &ResultSummary) {}
}

/// `SELECT <every column> FROM <t> WHERE 1 = 0`: no row, only the column types.
pub(crate) fn types_sql(
    detail: &TableDetail,
    obj: &ObjectRef,
    kind: dnt_core::server::DbKind,
) -> String {
    let list: Vec<String> = detail
        .columns
        .iter()
        .map(|c| quote_ident(kind.as_sql(), &c.name))
        .collect();
    format!(
        "SELECT {} FROM {} WHERE 1 = 0",
        list.join(", "),
        table_ref(kind, obj).relation()
    )
}

/// The table changed between two reads of the same job (or the server reports columns
/// the detail does not have).
pub(crate) fn table_changed() -> DbError {
    DbError::PlanStale {
        reason: "The table changed; check the mapping and preview again.".into(),
    }
}

/// Reads the target on `session` (no timeout: callers bound it).
pub(crate) async fn read_target(
    session: &dyn Session,
    obj: &ObjectRef,
) -> Result<TargetRead, DbError> {
    let kind = session.server_info().kind;
    let detail = session.introspector().table_detail(obj).await?;
    if detail.columns.is_empty() {
        return Err(config_error("table", "The table has no columns."));
    }
    let sql = types_sql(&detail, obj, kind);
    let mut metas = Metas::default();
    session
        .execute_one(
            &sql,
            classify(&sql, session.dialect()),
            &ExecOptions::default(),
            &mut metas,
        )
        .await?;
    let metas = metas.0.unwrap_or_default();
    if metas.len() != detail.columns.len() {
        return Err(table_changed());
    }
    let columns = detail
        .columns
        .iter()
        .zip(&metas)
        .map(|(cd, m)| TargetColumn::from_detail(cd, m.logical, kind.as_sql()))
        .collect();
    Ok(TargetRead {
        keys: key_choices(&detail),
        atomicity: dnt_query::atomicity(&detail, kind.as_sql()),
        fingerprint: dnt_query::fingerprint(&detail),
        columns,
        detail,
    })
}

/// [`read_target`] bounded by [`METADATA_CALL_TIMEOUT`].
pub(crate) async fn read_target_bounded(
    session: &dyn Session,
    obj: &ObjectRef,
) -> Result<TargetRead, DbError> {
    tokio::time::timeout(METADATA_CALL_TIMEOUT, read_target(session, obj))
        .await
        .map_err(|_| DbError::timeout(dnt_core::error::ConnectStage::Execute))?
}

/// Whether one statement on the table is all-or-nothing (a failed multi-row `INSERT`
/// leaves no row): PostgreSQL, and MySQL transactional engines (triggers run inside the
/// statement there).
pub(crate) fn statement_atomic(detail: &TableDetail, kind: dnt_core::server::DbKind) -> bool {
    let mut d = detail.clone();
    d.triggers.clear();
    dnt_query::atomicity(&d, kind.as_sql()) == Atomicity::Yes
}

impl SessionManager {
    /// The import target's columns (with the driver's types), upsert keys, transaction
    /// policy and the connection's guard settings, read on a short-lived dedicated session.
    pub async fn import_target(
        &self,
        conn: Uuid,
        table: ObjectRef,
    ) -> Result<ImportTarget, DbError> {
        let cfg = self.repo.get(conn)?;
        let rt = self.runtime(conn)?;
        let (session, _) = rt.open_query_session(session_db(cfg.kind, &table)).await?;
        let server = session.server_info().clone();
        let read = read_target_bounded(session.as_ref(), &table).await;
        close_quietly(session).await;
        let read = read?;
        Ok(ImportTarget {
            columns: read.columns,
            keys: read.keys,
            atomicity: read.atomicity,
            env: cfg.env_tag,
            read_only: cfg.read_only,
            server,
        })
    }
}
