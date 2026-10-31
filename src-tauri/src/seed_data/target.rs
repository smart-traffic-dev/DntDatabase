//! The seeder's target (spec §5.1 `seed_target`): the import's target read plus, per
//! column, its keys, CHECK values and the inferred plan.
use super::{SeedColumn, SeedForeignKey, SeedTarget};
use crate::import_data::session_db;
use crate::import_data::target::{read_target_bounded, TargetRead};
use crate::session_manager::{close_quietly, SessionManager};
use dnt_core::error::DbError;
use dnt_core::meta::ObjectRef;
use dnt_core::server::SqlKind;
use dnt_query::Atomicity;
use dnt_transfer::seed::infer::{check_values, fk_of, infer_plan, unique_single};
use uuid::Uuid;

/// The dialog's columns of a table read (`today` anchors the date ranges).
pub(crate) fn seed_columns(read: &TargetRead, today: time::Date) -> Vec<SeedColumn> {
    let detail = &read.detail;
    read.columns
        .iter()
        .map(|c| SeedColumn {
            primary: detail
                .primary_key
                .as_ref()
                .is_some_and(|pk| pk.columns.contains(&c.name)),
            unique_index: unique_single(detail, &c.name),
            foreign_key: fk_of(detail, &c.name).map(|f| SeedForeignKey {
                constraint: f.name.clone(),
                ref_schema: f.ref_schema.clone(),
                ref_table: f.ref_table.clone(),
                columns: f.columns.clone(),
                ref_columns: f.ref_columns.clone(),
            }),
            check_values: check_values(detail, &c.name),
            locked: c.generated,
            plan: infer_plan(c, detail, today),
            column: c.clone(),
        })
        .collect()
}

/// Whether the whole job runs in one transaction (as the import, R15).
pub(crate) fn transactional(kind: SqlKind, read: &TargetRead) -> bool {
    match kind {
        SqlKind::MySql => read.atomicity == Atomicity::Yes,
        SqlKind::Postgres => true,
    }
}

impl SessionManager {
    /// The table's columns with their inferred generators, read on a short-lived session.
    pub async fn seed_target(&self, conn: Uuid, table: ObjectRef) -> Result<SeedTarget, DbError> {
        let cfg = self.repo.get(conn)?;
        let rt = self.runtime(conn)?;
        let kind = cfg.kind.as_sql(); // R1: boundary
        let (session, _) = rt.open_query_session(session_db(kind, &table)).await?;
        let server = session.server_info().clone();
        let read = read_target_bounded(session.as_ref(), &table).await;
        close_quietly(session).await;
        let read = read?;
        let today = time::OffsetDateTime::now_utc().date();
        Ok(SeedTarget {
            columns: seed_columns(&read, today),
            env: cfg.env_tag,
            read_only: cfg.read_only,
            transactional: transactional(kind, &read),
            server,
            seed: Uuid::new_v4().as_u128() as u32,
        })
    }
}
