//! `Introspector` for PostgreSQL sessions. Placeholder until Task 13: every method
//! returns `Unsupported`.

use crate::PgSession;
use async_trait::async_trait;
use dnt_core::driver::Introspector;
use dnt_core::error::DbError;
use dnt_core::meta::{
    DatabaseInfo, ObjectKind, ObjectRef, ObjectSummary, SchemaInfo, Scope, TableDetail,
};

#[async_trait]
impl Introspector for PgSession {
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
