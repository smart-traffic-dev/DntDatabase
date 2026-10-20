//! Object tree browsing on the metadata sessions of an open connection.
//!
//! # NodeKey scheme (the UI must build the same keys for `refresh_node`)
//!
//! Results are cached per connection under a [`NodeKey`]: the typed path of the tree node
//! below its connection (the `conn:<uuid>` segment is never part of the key).
//!
//! | command          | key                                                                     |
//! |------------------|-------------------------------------------------------------------------|
//! | `list_databases` | `["dbs"]`                                                               |
//! | `list_schemas`   | `["db:<db>", "schemas"]`                                                |
//! | `list_objects`   | `["db:<db>", "schema:<s>"?, "folder:<Kind>"]`                           |
//! | `table_detail`   | `["db:<db>", "schema:<s>"?, "folder:<Kind>", "obj:<Kind>:<name>", "detail"]` |
//! | `object_ddl`     | `["db:<db>", "schema:<s>"?, "folder:<Kind>", "obj:<Kind>:<name>", "ddl"]`    |
//!
//! Every key is the tree path of its node (an object sits in the folder of its kind), with
//! `detail` / `ddl` appended for the object's two results. `schema:<s>` is present only
//! when the scope / object has a schema (PostgreSQL). `<Kind>` is the `ObjectKind` variant
//! name (`Table`, `MaterializedView`, ...). `refresh_node(id, key)` drops every cached
//! result whose key starts with `key` (whole segments): `[]` refreshes the whole
//! connection, `["db:shop"]` everything below database `shop`, a folder path its listing
//! and the detail / DDL of its objects, and an object's exact path both of its results.
//!
//! `show_system` is applied after the cache: MySQL system databases in `list_databases`,
//! PostgreSQL system schemas in `list_schemas` (see [`is_system`]).

use super::guard;
use crate::cache::{NodeKey, SchemaCache};
use crate::state::AppState;
use dnt_core::error::{AppError, DbError};
use dnt_core::meta::{
    DatabaseInfo, ObjectKind, ObjectRef, ObjectSummary, SchemaInfo, Scope, TableDetail,
};
use dnt_core::server::DbKind;
use tauri::State;
use uuid::Uuid;

/// Whether `name` is a system database (MySQL) or system schema (PostgreSQL).
///
/// MySQL: the virtual `information_schema` / `performance_schema` match in any case (the
/// server resolves them case-insensitively); `mysql` and `sys` are ordinary databases and
/// match exactly, so a user database `MySQL` on a case-sensitive server stays visible.
pub fn is_system(kind: DbKind, name: &str) -> bool {
    match kind {
        DbKind::MySql => {
            name.eq_ignore_ascii_case("information_schema")
                || name.eq_ignore_ascii_case("performance_schema")
                || name == "mysql"
                || name == "sys"
        }
        DbKind::Postgres => name == "information_schema" || name.starts_with("pg_"),
    }
}

/// Something listed by name in the tree.
pub trait Named {
    fn name(&self) -> &str;
}

impl Named for String {
    fn name(&self) -> &str {
        self
    }
}

impl Named for DatabaseInfo {
    fn name(&self) -> &str {
        &self.name
    }
}

impl Named for SchemaInfo {
    fn name(&self) -> &str {
        &self.name
    }
}

/// `items` without the system ones, unless `show_system`.
pub fn filter_system<T: Named>(kind: DbKind, items: Vec<T>, show_system: bool) -> Vec<T> {
    if show_system {
        return items;
    }
    items
        .into_iter()
        .filter(|item| !is_system(kind, item.name()))
        .collect()
}

fn scope_segments(database: &str, schema: Option<&str>) -> Vec<String> {
    let mut segments = vec![format!("db:{database}")];
    if let Some(schema) = schema {
        segments.push(format!("schema:{schema}"));
    }
    segments
}

pub fn databases_key() -> NodeKey {
    NodeKey(vec!["dbs".into()])
}

pub fn schemas_key(db: &str) -> NodeKey {
    NodeKey(vec![format!("db:{db}"), "schemas".into()])
}

pub fn folder_key(scope: &Scope, kind: ObjectKind) -> NodeKey {
    let mut segments = scope_segments(&scope.database, scope.schema.as_deref());
    segments.push(format!("folder:{kind:?}"));
    NodeKey(segments)
}

/// The object's tree path (inside the folder of its kind) plus `leaf` (`detail` or `ddl`).
pub fn object_key(obj: &ObjectRef, leaf: &str) -> NodeKey {
    let mut segments = scope_segments(&obj.database, obj.schema.as_deref());
    segments.push(format!("folder:{:?}", obj.kind));
    segments.push(format!("obj:{:?}:{}", obj.kind, obj.name));
    segments.push(leaf.into());
    NodeKey(segments)
}

pub async fn list_databases_impl(
    state: &AppState,
    id: Uuid,
    show_system: bool,
) -> Result<Vec<DatabaseInfo>, DbError> {
    let kind = state.repo.get(id)?.kind;
    let dbs = state
        .sessions
        .browse(id, None, databases_key(), |i| i.list_databases())
        .await?;
    // PostgreSQL lists no system databases (templates are excluded by the driver).
    Ok(match kind {
        DbKind::MySql => filter_system(kind, dbs, show_system),
        DbKind::Postgres => dbs,
    })
}

pub async fn list_schemas_impl(
    state: &AppState,
    id: Uuid,
    db: String,
    show_system: bool,
) -> Result<Vec<SchemaInfo>, DbError> {
    let kind = state.repo.get(id)?.kind;
    let schemas = state
        .sessions
        .browse(id, Some(&db), schemas_key(&db), |i| {
            let db = db.clone();
            Box::pin(async move { i.list_schemas(&db).await })
        })
        .await?;
    Ok(filter_system(kind, schemas, show_system))
}

pub async fn list_objects_impl(
    state: &AppState,
    id: Uuid,
    scope: Scope,
    kind: ObjectKind,
) -> Result<Vec<ObjectSummary>, DbError> {
    let key = folder_key(&scope, kind);
    let db = scope.database.clone();
    state
        .sessions
        .browse(id, Some(&db), key, |i| {
            let scope = scope.clone();
            Box::pin(async move { i.list_objects(&scope, kind).await })
        })
        .await
}

pub async fn table_detail_impl(
    state: &AppState,
    id: Uuid,
    obj: ObjectRef,
) -> Result<TableDetail, DbError> {
    let key = object_key(&obj, "detail");
    let db = obj.database.clone();
    state
        .sessions
        .browse(id, Some(&db), key, |i| {
            let obj = obj.clone();
            Box::pin(async move { i.table_detail(&obj).await })
        })
        .await
}

pub async fn object_ddl_impl(
    state: &AppState,
    id: Uuid,
    obj: ObjectRef,
) -> Result<String, DbError> {
    let key = object_key(&obj, "ddl");
    let db = obj.database.clone();
    state
        .sessions
        .browse(id, Some(&db), key, |i| {
            let obj = obj.clone();
            Box::pin(async move { i.object_ddl(&obj).await })
        })
        .await
}

pub async fn refresh_node_impl(
    state: &AppState,
    id: Uuid,
    key: Vec<String>,
) -> Result<(), DbError> {
    state.sessions.refresh(id, &NodeKey(key));
    Ok(())
}

pub async fn get_schema_cache_impl(
    state: &AppState,
    id: Uuid,
) -> Result<Option<SchemaCache>, DbError> {
    Ok(state.sessions.schema_cache(id))
}

#[tauri::command]
#[specta::specta]
pub async fn list_databases(
    state: State<'_, AppState>,
    id: Uuid,
    show_system: bool,
) -> Result<Vec<DatabaseInfo>, AppError> {
    guard(list_databases_impl(&state, id, show_system)).await
}

#[tauri::command]
#[specta::specta]
pub async fn list_schemas(
    state: State<'_, AppState>,
    id: Uuid,
    db: String,
    show_system: bool,
) -> Result<Vec<SchemaInfo>, AppError> {
    guard(list_schemas_impl(&state, id, db, show_system)).await
}

#[tauri::command]
#[specta::specta]
pub async fn list_objects(
    state: State<'_, AppState>,
    id: Uuid,
    scope: Scope,
    kind: ObjectKind,
) -> Result<Vec<ObjectSummary>, AppError> {
    guard(list_objects_impl(&state, id, scope, kind)).await
}

#[tauri::command]
#[specta::specta]
pub async fn table_detail(
    state: State<'_, AppState>,
    id: Uuid,
    obj: ObjectRef,
) -> Result<TableDetail, AppError> {
    guard(table_detail_impl(&state, id, obj)).await
}

#[tauri::command]
#[specta::specta]
pub async fn object_ddl(
    state: State<'_, AppState>,
    id: Uuid,
    obj: ObjectRef,
) -> Result<String, AppError> {
    guard(object_ddl_impl(&state, id, obj)).await
}

#[tauri::command]
#[specta::specta]
pub async fn refresh_node(
    state: State<'_, AppState>,
    id: Uuid,
    key: Vec<String>,
) -> Result<(), AppError> {
    guard(refresh_node_impl(&state, id, key)).await
}

#[tauri::command]
#[specta::specta]
pub async fn get_schema_cache(
    state: State<'_, AppState>,
    id: Uuid,
) -> Result<Option<SchemaCache>, AppError> {
    guard(get_schema_cache_impl(&state, id)).await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn system_schemas_filtered() {
        let mysql = names(&[
            "app",
            "information_schema",
            "mysql",
            "performance_schema",
            "sys",
            "INFORMATION_SCHEMA",
            "Performance_Schema",
            "MySQL",
            "SYS",
            "mysql_archive",
        ]);
        // The virtual schemas match in any case; `mysql` / `sys` only exactly.
        assert_eq!(
            filter_system(DbKind::MySql, mysql.clone(), false),
            names(&["app", "MySQL", "SYS", "mysql_archive"])
        );
        assert_eq!(filter_system(DbKind::MySql, mysql.clone(), true), mysql);

        let pg = names(&[
            "public",
            "information_schema",
            "pg_catalog",
            "pg_toast",
            "pg_temp_1",
            "sales",
            "pgx",
        ]);
        assert_eq!(
            filter_system(DbKind::Postgres, pg.clone(), false),
            names(&["public", "sales", "pgx"])
        );
        assert_eq!(filter_system(DbKind::Postgres, pg.clone(), true), pg);
    }

    #[test]
    fn node_keys_follow_the_documented_scheme() {
        let scope = Scope {
            database: "shop".into(),
            schema: Some("public".into()),
        };
        assert_eq!(databases_key().0, ["dbs"]);
        assert_eq!(schemas_key("shop").0, ["db:shop", "schemas"]);
        assert_eq!(
            folder_key(&scope, ObjectKind::MaterializedView).0,
            ["db:shop", "schema:public", "folder:MaterializedView"]
        );
        let obj = ObjectRef {
            database: "shop".into(),
            schema: None,
            name: "orders".into(),
            kind: ObjectKind::Table,
        };
        assert_eq!(
            object_key(&obj, "ddl").0,
            ["db:shop", "folder:Table", "obj:Table:orders", "ddl"]
        );
        assert_eq!(
            object_key(&obj, "detail").0,
            ["db:shop", "folder:Table", "obj:Table:orders", "detail"]
        );
        let view = ObjectRef {
            database: "shop".into(),
            schema: Some("public".into()),
            name: "mv".into(),
            kind: ObjectKind::MaterializedView,
        };
        let view_folder = folder_key(&scope, ObjectKind::MaterializedView);
        let view_ddl = object_key(&view, "ddl");
        assert_eq!(
            view_ddl.0,
            [
                "db:shop",
                "schema:public",
                "folder:MaterializedView",
                "obj:MaterializedView:mv",
                "ddl"
            ]
        );
        // Refreshing the folder (or the object's own path) reaches the object's results.
        assert!(view_ddl.starts_with(&view_folder));
        assert!(view_ddl.starts_with(&NodeKey(view_ddl.0[..4].to_vec())));
    }

    #[test]
    fn refreshing_a_folder_drops_its_objects_results() {
        use crate::cache::NodeCache;
        let cache = NodeCache::default();
        let scope = Scope {
            database: "shop".into(),
            schema: None,
        };
        let obj = ObjectRef {
            database: "shop".into(),
            schema: None,
            name: "orders".into(),
            kind: ObjectKind::Table,
        };
        let other = ObjectRef {
            kind: ObjectKind::View,
            name: "v".into(),
            ..obj.clone()
        };
        for key in [
            folder_key(&scope, ObjectKind::Table),
            object_key(&obj, "detail"),
            object_key(&obj, "ddl"),
            object_key(&other, "ddl"),
        ] {
            assert!(cache.insert_if_epoch(key, serde_json::json!(1), cache.epoch()));
        }
        cache.invalidate_prefix(&folder_key(&scope, ObjectKind::Table));
        assert_eq!(cache.len(), 1);
        assert!(cache.get(&object_key(&other, "ddl")).is_some());
    }
}
