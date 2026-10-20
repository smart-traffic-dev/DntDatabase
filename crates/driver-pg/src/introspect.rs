//! [`Introspector`] for PostgreSQL 12–17, reading `pg_catalog` only.
//!
//! Every name is a bound parameter (`$1`, `$2`); per-relation queries take the OID found
//! by name first. PostgreSQL cannot read another database's catalogs, so a call naming a
//! database other than the session's own is `Unsupported` (one metadata session per
//! database is opened by the caller); `list_databases` works from any database. A scope or
//! object without a schema means `public`. Objects the user holds no privilege on are
//! left out of lists and reported missing by `table_detail` / `object_ddl`. The session
//! mutex is held for one call at a time.

use crate::ddl::{build_table_ddl, quote_ident};
use crate::{map_pg_error, queries, PgConn, PgSession};
use async_trait::async_trait;
use dnt_core::driver::{Introspector, Session};
use dnt_core::error::{ConnectStage, DbError};
use dnt_core::meta::{
    group_table_columns, CheckDetail, ColumnDetail, DatabaseInfo, ForeignKeyDetail, IndexDetail,
    ObjectKind, ObjectRef, ObjectSummary, SchemaInfo, Scope, TableColumns, TableDetail,
    TableOptions, TriggerDetail,
};
use tokio::sync::MutexGuard;
use tokio_postgres::types::{FromSql, ToSql};
use tokio_postgres::{Client, Row};

/// Schema used when a scope or object names none.
const DEFAULT_SCHEMA: &str = "public";

/// `undefined_table` / `undefined_object`, as the server would report them.
const UNDEFINED_TABLE: &str = "42P01";
const UNDEFINED_OBJECT: &str = "42704";

type Params<'a> = [&'a (dyn ToSql + Sync)];

fn map_err(e: tokio_postgres::Error) -> DbError {
    map_pg_error(e, ConnectStage::Execute)
}

async fn fetch(client: &Client, sql: &str, params: &Params<'_>) -> Result<Vec<Row>, DbError> {
    client.query(sql, params).await.map_err(map_err)
}

/// Column `i` of `row`; a type mismatch (a bug in our SQL) is an internal error.
fn get<'a, T: FromSql<'a>>(row: &'a Row, i: usize) -> Result<T, DbError> {
    row.try_get(i).map_err(|e| DbError::Internal {
        message: format!("catalog column {i}: {e}"),
    })
}

/// Text column `i`, with NULL and the empty string both meaning "absent".
fn non_empty(row: &Row, i: usize) -> Result<Option<String>, DbError> {
    Ok(get::<Option<String>>(row, i)?.filter(|s| !s.is_empty()))
}

fn estimate(row: &Row, i: usize) -> Result<Option<u64>, DbError> {
    Ok(get::<Option<i64>>(row, i)?.and_then(|n| u64::try_from(n).ok()))
}

fn schema_of(schema: &Option<String>) -> &str {
    schema.as_deref().unwrap_or(DEFAULT_SCHEMA)
}

/// What is returned for an object that does not exist or that the user cannot see.
fn not_found(obj: &ObjectRef) -> DbError {
    let relation = matches!(
        obj.kind,
        ObjectKind::Table | ObjectKind::View | ObjectKind::MaterializedView
    );
    DbError::Server {
        code: None,
        sqlstate: Some(
            if relation {
                UNDEFINED_TABLE
            } else {
                UNDEFINED_OBJECT
            }
            .into(),
        ),
        message: format!(
            "{:?} {}.{} does not exist or is not visible to this user",
            obj.kind,
            schema_of(&obj.schema),
            obj.name
        ),
        position: None,
    }
}

/// `relkind`s accepted for a kind of relation.
fn relkinds(kind: ObjectKind) -> Option<&'static str> {
    match kind {
        ObjectKind::Table => Some("rp"),
        ObjectKind::View => Some("v"),
        ObjectKind::MaterializedView => Some("m"),
        _ => None,
    }
}

/// Name / comment (/ row estimate) rows to summaries, sorted by name. Names repeat for
/// overloaded routines and same-named triggers on different tables: listed once.
fn summaries(
    rows: &[Row],
    kind: ObjectKind,
    with_estimate: bool,
) -> Result<Vec<ObjectSummary>, DbError> {
    let mut out = Vec::with_capacity(rows.len());
    for r in rows {
        out.push(ObjectSummary {
            name: get(r, 0)?,
            kind,
            comment: non_empty(r, 1)?,
            row_estimate: if with_estimate { estimate(r, 2)? } else { None },
        });
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out.dedup_by(|a, b| a.name == b.name);
    Ok(out)
}

/// `confupdtype` / `confdeltype` to the SQL action.
fn fk_action(code: &str) -> &'static str {
    match code {
        "r" => "RESTRICT",
        "c" => "CASCADE",
        "n" => "SET NULL",
        "d" => "SET DEFAULT",
        _ => "NO ACTION",
    }
}

/// `tgtype` bits (`pg_trigger.h`) to timing and events.
fn trigger_timing(tgtype: i32) -> &'static str {
    if tgtype & (1 << 6) != 0 {
        "INSTEAD OF"
    } else if tgtype & (1 << 1) != 0 {
        "BEFORE"
    } else {
        "AFTER"
    }
}

fn trigger_events(tgtype: i32) -> String {
    [
        (1 << 2, "INSERT"),
        (1 << 4, "UPDATE"),
        (1 << 3, "DELETE"),
        (1 << 5, "TRUNCATE"),
    ]
    .iter()
    .filter(|(bit, _)| tgtype & bit != 0)
    .map(|(_, name)| *name)
    .collect::<Vec<_>>()
    .join(" OR ")
}

fn column_detail(r: &Row) -> Result<ColumnDetail, DbError> {
    let expr: Option<String> = get(r, 4)?;
    let identity = match non_empty(r, 5)?.as_deref() {
        Some("a") => Some("ALWAYS".to_string()),
        Some("d") => Some("BY DEFAULT".to_string()),
        _ => None,
    };
    let generated_stored = non_empty(r, 6)?.as_deref() == Some("s");
    let (default, generated) = if generated_stored {
        (None, expr)
    } else {
        (expr, None)
    };
    let serial = default
        .as_deref()
        .is_some_and(|d| d.starts_with("nextval("));
    let ordinal: i32 = get(r, 1)?;
    Ok(ColumnDetail {
        name: get(r, 0)?,
        ordinal: u32::try_from(ordinal).unwrap_or_default(),
        db_type: get(r, 2)?,
        nullable: !get::<bool>(r, 3)?,
        auto_increment: identity.is_some() || serial,
        default,
        identity,
        generated,
        invisible: false,
        collation: non_empty(r, 8)?,
        comment: non_empty(r, 7)?,
    })
}

/// Index rows to `(primary key, other indexes by name)`. See `ddl.rs` for what
/// `definition` holds.
fn index_details(
    rows: &[Row],
    partitioned: bool,
) -> Result<(Option<IndexDetail>, Vec<IndexDetail>), DbError> {
    let mut primary = None;
    let mut others = Vec::new();
    for r in rows {
        let constraint_def: Option<String> = get(r, 7)?;
        let definition = match constraint_def {
            Some(def) => Some(def),
            // On a partitioned table the definition reads `ON ONLY`, which would create
            // an invalid parent-only index.
            None => get::<Option<String>>(r, 6)?.map(|d| {
                if partitioned {
                    d.replacen(" ON ONLY ", " ON ", 1)
                } else {
                    d
                }
            }),
        };
        let idx = IndexDetail {
            name: get(r, 0)?,
            unique: get(r, 1)?,
            primary: get(r, 2)?,
            method: non_empty(r, 3)?,
            columns: get(r, 4)?,
            expression: non_empty(r, 5)?,
            definition,
        };
        if idx.primary {
            primary = Some(idx);
        } else {
            others.push(idx);
        }
    }
    others.sort_by(|a, b| a.name.cmp(&b.name));
    Ok((primary, others))
}

fn foreign_keys(rows: &[Row], schema: &str) -> Result<Vec<ForeignKeyDetail>, DbError> {
    let mut out = Vec::with_capacity(rows.len());
    for r in rows {
        let ref_schema: String = get(r, 2)?;
        let (unqualified, qualified): (String, String) = (get(r, 8)?, get(r, 9)?);
        out.push(ForeignKeyDetail {
            name: get(r, 0)?,
            columns: get(r, 1)?,
            // Only a reference into another schema names it.
            ref_schema: (ref_schema != schema).then_some(ref_schema),
            ref_table: get(r, 3)?,
            ref_columns: get(r, 4)?,
            on_update: fk_action(&get::<String>(r, 5)?).into(),
            on_delete: fk_action(&get::<String>(r, 6)?).into(),
            definition: get::<Option<String>>(r, 7)?
                .map(|def| qualify_reference(def, &unqualified, &qualified)),
        });
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(out)
}

/// `pg_get_constraintdef` names the referenced table unqualified when its schema is on
/// the session's `search_path`; the DDL must not depend on that, so the reference is
/// schema-qualified. Everything else stays verbatim.
fn qualify_reference(def: String, unqualified: &str, qualified: &str) -> String {
    let bare = format!(" REFERENCES {unqualified}(");
    match def.find(&bare) {
        Some(at) => format!(
            "{} REFERENCES {qualified}({}",
            &def[..at],
            &def[at + bare.len()..]
        ),
        None => def,
    }
}

/// Joins definitions (functions, triggers) into one script, each ending with `;`.
fn script(defs: impl IntoIterator<Item = String>) -> String {
    defs.into_iter()
        .map(|d| format!("{};", d.trim_end().trim_end_matches(';')))
        .collect::<Vec<_>>()
        .join("\n\n")
}

impl PgSession {
    /// Locks the connection once `db` is known to be the session's database.
    async fn conn_for(&self, db: &str) -> Result<MutexGuard<'_, PgConn>, DbError> {
        let conn = self.conn.lock().await;
        let row = conn
            .client
            .query_one(queries::CURRENT_DATABASE, &[])
            .await
            .map_err(map_err)?;
        let current: String = get(&row, 0)?;
        if current != db {
            return Err(DbError::Unsupported {
                feature: "cross-database introspection".into(),
                version: format!(
                    "{} (session database {current}, requested {db})",
                    self.server_info().version_string
                ),
            });
        }
        Ok(conn)
    }

    async fn list_with(
        &self,
        db: &str,
        sql: &str,
        params: &Params<'_>,
        kind: ObjectKind,
        with_estimate: bool,
    ) -> Result<Vec<ObjectSummary>, DbError> {
        let conn = self.conn_for(db).await?;
        summaries(
            &fetch(&conn.client, sql, params).await?,
            kind,
            with_estimate,
        )
    }

    async fn relation_ddl(&self, obj: &ObjectRef, prefix: &str) -> Result<String, DbError> {
        let schema = schema_of(&obj.schema);
        let kinds = relkinds(obj.kind).unwrap_or_default();
        let conn = self.conn_for(&obj.database).await?;
        let rel = fetch(
            &conn.client,
            &queries::relation(),
            &[&schema, &obj.name, &kinds],
        )
        .await?;
        let oid: u32 = get(rel.first().ok_or_else(|| not_found(obj))?, 0)?;
        let def: Option<String> = get(
            &conn
                .client
                .query_one(queries::VIEW_DEF, &[&oid])
                .await
                .map_err(map_err)?,
            0,
        )?;
        let def = def.ok_or_else(|| not_found(obj))?;
        Ok(format!(
            "{prefix} {}.{} AS\n{};",
            quote_ident(schema),
            quote_ident(&obj.name),
            def.trim_end().trim_end_matches(';')
        ))
    }

    async fn defs_ddl(
        &self,
        obj: &ObjectRef,
        sql: &str,
        params: &Params<'_>,
    ) -> Result<String, DbError> {
        let conn = self.conn_for(&obj.database).await?;
        let rows = fetch(&conn.client, sql, params).await?;
        let mut defs = Vec::with_capacity(rows.len());
        for r in &rows {
            defs.extend(get::<Option<String>>(r, 0)?);
        }
        if defs.is_empty() {
            return Err(not_found(obj));
        }
        Ok(script(defs))
    }

    async fn sequence_ddl(&self, obj: &ObjectRef) -> Result<String, DbError> {
        let schema = schema_of(&obj.schema);
        let conn = self.conn_for(&obj.database).await?;
        let rows = fetch(&conn.client, &queries::sequence(), &[&schema, &obj.name]).await?;
        let r = rows.first().ok_or_else(|| not_found(obj))?;
        let data_type: String = get(r, 0)?;
        let (start, increment, min, max, cache): (i64, i64, i64, i64, i64) =
            (get(r, 1)?, get(r, 2)?, get(r, 3)?, get(r, 4)?, get(r, 5)?);
        let cycle = if get::<bool>(r, 6)? {
            "CYCLE"
        } else {
            "NO CYCLE"
        };
        Ok(format!(
            "CREATE SEQUENCE {}.{} AS {data_type} INCREMENT BY {increment} MINVALUE {min} \
             MAXVALUE {max} START WITH {start} CACHE {cache} {cycle};",
            quote_ident(schema),
            quote_ident(&obj.name)
        ))
    }

    async fn type_ddl(&self, obj: &ObjectRef) -> Result<String, DbError> {
        let schema = schema_of(&obj.schema);
        let conn = self.conn_for(&obj.database).await?;
        let rows = fetch(&conn.client, queries::TYPE, &[&schema, &obj.name]).await?;
        let r = rows.first().ok_or_else(|| not_found(obj))?;
        let (oid, typtype, typrelid): (u32, String, u32) = (get(r, 0)?, get(r, 1)?, get(r, 2)?);
        let name = format!("{}.{}", quote_ident(schema), quote_ident(&obj.name));
        if typtype == "e" {
            let labels = fetch(&conn.client, queries::ENUM_LABELS, &[&oid]).await?;
            let mut list = Vec::with_capacity(labels.len());
            for l in &labels {
                list.push(format!("'{}'", get::<String>(l, 0)?.replace('\'', "''")));
            }
            return Ok(format!("CREATE TYPE {name} AS ENUM ({});", list.join(", ")));
        }
        let attrs = fetch(&conn.client, queries::COMPOSITE_ATTRIBUTES, &[&typrelid]).await?;
        let mut list = Vec::with_capacity(attrs.len());
        for a in &attrs {
            list.push(format!(
                "{} {}",
                quote_ident(&get::<String>(a, 0)?),
                get::<String>(a, 1)?
            ));
        }
        Ok(format!("CREATE TYPE {name} AS ({});", list.join(", ")))
    }
}

#[async_trait]
impl Introspector for PgSession {
    async fn list_databases(&self) -> Result<Vec<DatabaseInfo>, DbError> {
        let conn = self.conn.lock().await;
        let rows = fetch(&conn.client, queries::LIST_DATABASES, &[]).await?;
        let mut names = Vec::with_capacity(rows.len());
        for r in &rows {
            names.push(get::<String>(r, 0)?);
        }
        names.sort();
        Ok(names
            .into_iter()
            .map(|name| DatabaseInfo { name })
            .collect())
    }

    /// Every schema the user may use (`USAGE`) except TOAST and temporary schemas;
    /// hiding system schemas is the app's choice.
    async fn list_schemas(&self, db: &str) -> Result<Vec<SchemaInfo>, DbError> {
        let conn = self.conn_for(db).await?;
        let rows = fetch(&conn.client, queries::LIST_SCHEMAS, &[]).await?;
        let mut names = Vec::with_capacity(rows.len());
        for r in &rows {
            names.push(get::<String>(r, 0)?);
        }
        names.sort();
        Ok(names.into_iter().map(|name| SchemaInfo { name }).collect())
    }

    /// Never computes sizes; the row estimate is `reltuples` (tables and materialized
    /// views only, `None` when never analyzed on PG 14+).
    async fn list_objects(
        &self,
        scope: &Scope,
        kind: ObjectKind,
    ) -> Result<Vec<ObjectSummary>, DbError> {
        let db = scope.database.as_str();
        let schema = schema_of(&scope.schema);
        match kind {
            ObjectKind::Table | ObjectKind::View | ObjectKind::MaterializedView => {
                let kinds = relkinds(kind).unwrap_or_default();
                let with_estimate = kind != ObjectKind::View;
                self.list_with(
                    db,
                    &queries::list_relations(),
                    &[&schema, &kinds],
                    kind,
                    with_estimate,
                )
                .await
            }
            ObjectKind::Sequence => {
                self.list_with(db, &queries::list_sequences(), &[&schema], kind, false)
                    .await
            }
            ObjectKind::Function | ObjectKind::Procedure => {
                let prokind = if kind == ObjectKind::Function {
                    "f"
                } else {
                    "p"
                };
                self.list_with(
                    db,
                    queries::LIST_ROUTINES,
                    &[&schema, &prokind],
                    kind,
                    false,
                )
                .await
            }
            ObjectKind::Trigger => {
                self.list_with(db, &queries::list_triggers(), &[&schema], kind, false)
                    .await
            }
            ObjectKind::Type => {
                self.list_with(db, queries::LIST_TYPES, &[&schema], kind, false)
                    .await
            }
            ObjectKind::Event => {
                drop(self.conn_for(db).await?);
                Ok(Vec::new())
            }
        }
    }

    async fn list_columns(&self, scope: &Scope) -> Result<Vec<TableColumns>, DbError> {
        let schema = schema_of(&scope.schema);
        let conn = self.conn_for(&scope.database).await?;
        let rows = fetch(&conn.client, &queries::list_columns(), &[&schema]).await?;
        let mut triples = Vec::with_capacity(rows.len());
        for r in &rows {
            triples.push((
                get::<String>(r, 0)?,
                get::<String>(r, 1)?,
                get::<String>(r, 2)?,
            ));
        }
        Ok(group_table_columns(triples))
    }

    async fn table_detail(&self, obj: &ObjectRef) -> Result<TableDetail, DbError> {
        let Some(kinds) = relkinds(obj.kind) else {
            return Err(self.unsupported(&format!("table detail for {:?}", obj.kind)));
        };
        let schema = schema_of(&obj.schema);
        let conn = self.conn_for(&obj.database).await?;
        let client = &conn.client;
        let rel = fetch(client, &queries::relation(), &[&schema, &obj.name, &kinds]).await?;
        let rel = rel.first().ok_or_else(|| not_found(obj))?;
        let oid: u32 = get(rel, 0)?;
        let partitioned = get::<String>(rel, 1)? == "p";

        let mut columns = Vec::new();
        for r in &fetch(client, queries::COLUMNS, &[&oid]).await? {
            columns.push(column_detail(r)?);
        }
        let (primary_key, indexes) = index_details(
            &fetch(client, queries::INDEXES, &[&oid]).await?,
            partitioned,
        )?;
        let foreign_keys = foreign_keys(
            &fetch(client, queries::FOREIGN_KEYS, &[&oid]).await?,
            schema,
        )?;

        let mut checks = Vec::new();
        for r in &fetch(client, queries::CHECKS, &[&oid]).await? {
            checks.push(CheckDetail {
                name: get(r, 0)?,
                expression: get::<Option<String>>(r, 1)?.unwrap_or_default(),
                definition: get(r, 2)?,
            });
        }
        checks.sort_by(|a, b| a.name.cmp(&b.name));

        let mut triggers = Vec::new();
        for r in &fetch(client, queries::TABLE_TRIGGERS, &[&oid]).await? {
            let tgtype: i32 = get(r, 1)?;
            triggers.push(TriggerDetail {
                name: get(r, 0)?,
                timing: trigger_timing(tgtype).into(),
                event: trigger_events(tgtype),
            });
        }
        triggers.sort_by(|a, b| a.name.cmp(&b.name));

        Ok(TableDetail {
            object: obj.clone(),
            columns,
            primary_key,
            indexes,
            foreign_keys,
            checks,
            triggers,
            options: TableOptions {
                engine: None,
                collation: None,
                tablespace: non_empty(rel, 5)?,
                partitioned_by: non_empty(rel, 4)?,
                system_versioned: false,
            },
            row_estimate: estimate(rel, 2)?,
            size_bytes: estimate(rel, 6)?,
            comment: non_empty(rel, 3)?,
        })
    }

    async fn object_ddl(&self, obj: &ObjectRef) -> Result<String, DbError> {
        let schema = schema_of(&obj.schema);
        match obj.kind {
            ObjectKind::Table => Ok(build_table_ddl(&self.table_detail(obj).await?)),
            ObjectKind::View => self.relation_ddl(obj, "CREATE OR REPLACE VIEW").await,
            ObjectKind::MaterializedView => {
                self.relation_ddl(obj, "CREATE MATERIALIZED VIEW").await
            }
            ObjectKind::Function | ObjectKind::Procedure => {
                let prokind = if obj.kind == ObjectKind::Function {
                    "f"
                } else {
                    "p"
                };
                self.defs_ddl(obj, queries::ROUTINE_DEFS, &[&schema, &obj.name, &prokind])
                    .await
            }
            ObjectKind::Trigger => {
                self.defs_ddl(obj, &queries::trigger_defs(), &[&schema, &obj.name])
                    .await
            }
            ObjectKind::Sequence => self.sequence_ddl(obj).await,
            ObjectKind::Type => self.type_ddl(obj).await,
            ObjectKind::Event => Err(self.unsupported("DDL for Event")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_trigger_type() {
        // BEFORE INSERT FOR EACH ROW
        assert_eq!(trigger_timing(0b111), "BEFORE");
        assert_eq!(trigger_events(0b111), "INSERT");
        // AFTER INSERT OR UPDATE OR DELETE (statement level)
        assert_eq!(trigger_timing(0b11100), "AFTER");
        assert_eq!(trigger_events(0b11100), "INSERT OR UPDATE OR DELETE");
        // INSTEAD OF UPDATE FOR EACH ROW
        assert_eq!(trigger_timing(0b1010001), "INSTEAD OF");
        assert_eq!(trigger_events(0b1010001), "UPDATE");
        assert_eq!(trigger_events(1 << 5), "TRUNCATE");
    }

    #[test]
    fn qualifies_fk_reference_only() {
        let def = "FOREIGN KEY (a) REFERENCES orders(id) DEFERRABLE INITIALLY DEFERRED";
        assert_eq!(
            qualify_reference(def.into(), "orders", "public.orders"),
            "FOREIGN KEY (a) REFERENCES public.orders(id) DEFERRABLE INITIALLY DEFERRED"
        );
        let qualified = "FOREIGN KEY (a) REFERENCES sales.\"T(x)\"(id)";
        assert_eq!(
            qualify_reference(qualified.into(), "\"T(x)\"", "sales.\"T(x)\""),
            qualified
        );
    }

    #[test]
    fn maps_fk_actions() {
        assert_eq!(fk_action("a"), "NO ACTION");
        assert_eq!(fk_action("r"), "RESTRICT");
        assert_eq!(fk_action("c"), "CASCADE");
        assert_eq!(fk_action("n"), "SET NULL");
        assert_eq!(fk_action("d"), "SET DEFAULT");
    }

    #[test]
    fn scripts_end_each_definition() {
        assert_eq!(
            script(["CREATE A\n".to_string(), "CREATE B;".to_string()]),
            "CREATE A;\n\nCREATE B;"
        );
    }
}
