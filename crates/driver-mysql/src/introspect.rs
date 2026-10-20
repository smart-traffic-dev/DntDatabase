//! Variant-aware [`Introspector`] for MySQL 5.7 / 8 and MariaDB 10.3+.
//!
//! Every query names its database explicitly (bound `?` parameters, or `quote_ident` in
//! `SHOW` statements), so the session's current database and transaction tracking are
//! never touched. The session mutex is held for one call at a time.

use crate::map_mysql_error;
use crate::queries::{self, quote_ident};
use crate::session::MySqlSession;
use crate::values::value_text;
use async_trait::async_trait;
use dnt_core::driver::{Introspector, Session};
use dnt_core::error::{ConnectStage, DbError};
use dnt_core::meta::{
    group_table_columns, CheckDetail, ColumnDetail, DatabaseInfo, ForeignKeyDetail, IndexDetail,
    ObjectKind, ObjectRef, ObjectSummary, SchemaInfo, Scope, TableColumns, TableDetail,
    TableOptions, TriggerDetail,
};
use dnt_core::server::ServerVariant;
use mysql_async::prelude::Queryable;
use mysql_async::{Conn, Params, Row};
use std::collections::{BTreeMap, HashMap};

/// One result row decoded as text (`None` for SQL NULL).
type TextRow = Vec<Option<String>>;

fn map_err(e: mysql_async::Error) -> DbError {
    map_mysql_error(e, ConnectStage::Execute)
}

fn text_row(row: Row) -> TextRow {
    row.unwrap().into_iter().map(value_text).collect()
}

/// Runs a parameterised `information_schema` query.
async fn fetch(conn: &mut Conn, sql: &str, params: Params) -> Result<Vec<TextRow>, DbError> {
    let rows: Vec<Row> = conn.exec(sql, params).await.map_err(map_err)?;
    Ok(rows.into_iter().map(text_row).collect())
}

/// Runs a `SHOW` statement (these cannot bind parameters).
async fn show(conn: &mut Conn, sql: &str) -> Result<Vec<TextRow>, DbError> {
    let rows: Vec<Row> = conn.query(sql).await.map_err(map_err)?;
    Ok(rows.into_iter().map(text_row).collect())
}

/// Column `i` of `row`, as an owned string (`None` for NULL or a missing column).
fn col(row: &TextRow, i: usize) -> Option<String> {
    row.get(i).cloned().flatten()
}

/// Column `i`, with NULL and the empty string both meaning "absent".
fn non_empty(row: &TextRow, i: usize) -> Option<String> {
    col(row, i).filter(|s| !s.is_empty())
}

fn num(row: &TextRow, i: usize) -> Option<u64> {
    col(row, i).and_then(|s| s.parse().ok())
}

fn params2(a: &str, b: &str) -> Params {
    Params::from((a.to_string(), b.to_string()))
}

fn params1(a: &str) -> Params {
    Params::from((a.to_string(),))
}

/// What `table_detail` returns for a table that does not exist or is not visible to the
/// user (mirrors the server's `ER_NO_SUCH_TABLE`).
fn no_such_table(obj: &ObjectRef) -> DbError {
    DbError::Server {
        code: Some("1146".into()),
        sqlstate: Some("42S02".into()),
        message: format!("Table '{}.{}' doesn't exist", obj.database, obj.name),
        position: None,
    }
}

/// `Table_type` of `SHOW FULL TABLES` / `TABLE_TYPE` to the listed kind.
fn table_kind(table_type: &str) -> Option<ObjectKind> {
    match table_type {
        "BASE TABLE" | "SYSTEM VERSIONED" => Some(ObjectKind::Table),
        "VIEW" | "SYSTEM VIEW" => Some(ObjectKind::View),
        _ => None,
    }
}

/// Name/comment rows (routines, triggers, events, sequences) to summaries.
fn summaries(rows: Vec<TextRow>, kind: ObjectKind) -> Vec<ObjectSummary> {
    let mut out: Vec<ObjectSummary> = rows
        .iter()
        .filter_map(|r| {
            Some(ObjectSummary {
                name: col(r, 0)?,
                kind,
                comment: non_empty(r, 1),
                row_estimate: None,
            })
        })
        .collect();
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

impl MySqlSession {
    async fn list_tables_or_views(
        &self,
        db: &str,
        kind: ObjectKind,
    ) -> Result<Vec<ObjectSummary>, DbError> {
        let mut conn = self.live_conn().await?;
        let shown = show(
            &mut conn,
            &format!("SHOW FULL TABLES FROM {}", quote_ident(db)),
        )
        .await?;
        // Listing never reads per-table statistics (`TABLE_ROWS` & co.): on a cold
        // cache MySQL 8 computes them per table (~10 ms each). Comments only come from
        // MySQL 8 / MariaDB, where selecting them alone touches no table; MySQL 5.7
        // opens every table for any `information_schema.TABLES` read, so lists there
        // carry no comment (`table_detail` has it). Views never have a comment.
        let read_comments =
            kind == ObjectKind::Table && self.server_info().variant != ServerVariant::MySql57;
        let comments: HashMap<String, String> = if read_comments {
            fetch(&mut conn, queries::TABLE_COMMENTS, params1(db))
                .await?
                .iter()
                .filter_map(|r| Some((col(r, 0)?, non_empty(r, 1)?)))
                .collect()
        } else {
            HashMap::new()
        };
        let mut out: Vec<ObjectSummary> = shown
            .iter()
            .filter_map(|r| {
                let name = col(r, 0)?;
                if table_kind(&col(r, 1)?)? != kind {
                    return None;
                }
                Some(ObjectSummary {
                    comment: comments.get(&name).cloned(),
                    row_estimate: None,
                    name,
                    kind,
                })
            })
            .collect();
        out.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(out)
    }

    async fn list_named(
        &self,
        sql: &str,
        params: Params,
        kind: ObjectKind,
    ) -> Result<Vec<ObjectSummary>, DbError> {
        let mut conn = self.live_conn().await?;
        Ok(summaries(fetch(&mut conn, sql, params).await?, kind))
    }

    fn checks_query(&self) -> Option<&'static str> {
        if !self.capabilities().check_constraints {
            return None;
        }
        match self.server_info().variant {
            ServerVariant::MariaDb => Some(queries::CHECKS_MARIADB),
            _ => Some(queries::CHECKS_MYSQL),
        }
    }

    fn indexes_query(&self) -> &'static str {
        if self.capabilities().functional_index {
            queries::INDEXES_WITH_EXPRESSION
        } else {
            queries::INDEXES
        }
    }
}

fn column_detail(r: &TextRow, maria: bool) -> Option<ColumnDetail> {
    let extra = col(r, 5).unwrap_or_default();
    // MariaDB reports "no default" on a nullable column as the text `NULL`
    // (a string default would be quoted: `'NULL'`).
    let default = col(r, 4).filter(|d| !(maria && d == "NULL"));
    Some(ColumnDetail {
        name: col(r, 0)?,
        ordinal: col(r, 1)?.parse().ok()?,
        db_type: col(r, 2)?,
        nullable: col(r, 3).as_deref() == Some("YES"),
        default,
        auto_increment: extra.to_ascii_lowercase().contains("auto_increment"),
        identity: None,
        generated: non_empty(r, 6),
        invisible: extra.to_ascii_uppercase().contains("INVISIBLE"),
        collation: non_empty(r, 8),
        comment: non_empty(r, 7),
    })
}

/// Groups index parts (rows ordered by index then position) into indexes by name.
fn group_indexes(rows: &[TextRow]) -> BTreeMap<String, IndexDetail> {
    let mut out: BTreeMap<String, IndexDetail> = BTreeMap::new();
    for r in rows {
        let Some(name) = col(r, 0) else { continue };
        let idx = out.entry(name.clone()).or_insert_with(|| IndexDetail {
            primary: name == "PRIMARY",
            unique: col(r, 1).as_deref() == Some("0"),
            name,
            columns: Vec::new(),
            expression: None,
            method: non_empty(r, 4),
            definition: None,
        });
        if let Some(c) = col(r, 3) {
            idx.columns.push(c);
        }
        if let Some(e) = non_empty(r, 5) {
            idx.expression = Some(match idx.expression.take() {
                Some(prev) => format!("{prev}, {e}"),
                None => e,
            });
        }
    }
    out
}

/// Groups foreign key parts (rows ordered by constraint then position) by name.
fn group_foreign_keys(rows: &[TextRow], db: &str) -> Vec<ForeignKeyDetail> {
    let mut out: BTreeMap<String, ForeignKeyDetail> = BTreeMap::new();
    for r in rows {
        let (Some(name), Some(ref_table)) = (col(r, 0), col(r, 3)) else {
            continue;
        };
        let fk = out.entry(name.clone()).or_insert_with(|| ForeignKeyDetail {
            name,
            columns: Vec::new(),
            // Only a reference into another database names it.
            ref_schema: col(r, 2).filter(|s| s != db),
            ref_table,
            ref_columns: Vec::new(),
            on_update: col(r, 5).unwrap_or_default(),
            on_delete: col(r, 6).unwrap_or_default(),
            // MySQL table DDL comes verbatim from `SHOW CREATE TABLE`.
            definition: None,
        });
        fk.columns.extend(col(r, 1));
        fk.ref_columns.extend(col(r, 4));
    }
    out.into_values().collect()
}

/// MariaDB stores `JSON` as `longtext` plus a `json_valid(col)` check: report it as json.
fn apply_json_alias(columns: &mut [ColumnDetail], checks: &[CheckDetail]) {
    for c in columns
        .iter_mut()
        .filter(|c| c.db_type.eq_ignore_ascii_case("longtext"))
    {
        let quoted = format!("json_valid({})", quote_ident(&c.name));
        let bare = format!("json_valid({})", c.name);
        if checks
            .iter()
            .any(|k| k.expression.contains(&quoted) || k.expression.contains(&bare))
        {
            c.db_type = "json".into();
        }
    }
}

#[async_trait]
impl Introspector for MySqlSession {
    async fn list_databases(&self) -> Result<Vec<DatabaseInfo>, DbError> {
        let mut conn = self.live_conn().await?;
        let mut names: Vec<String> = show(&mut conn, queries::LIST_DATABASES)
            .await?
            .iter()
            .filter_map(|r| col(r, 0))
            .collect();
        names.sort();
        Ok(names
            .into_iter()
            .map(|name| DatabaseInfo { name })
            .collect())
    }

    /// MySQL has no schemas inside a database: the database is its only schema.
    async fn list_schemas(&self, db: &str) -> Result<Vec<SchemaInfo>, DbError> {
        Ok(vec![SchemaInfo { name: db.into() }])
    }

    async fn list_objects(
        &self,
        scope: &Scope,
        kind: ObjectKind,
    ) -> Result<Vec<ObjectSummary>, DbError> {
        let db = scope.database.as_str();
        match kind {
            ObjectKind::Table | ObjectKind::View => self.list_tables_or_views(db, kind).await,
            ObjectKind::Function => {
                self.list_named(queries::ROUTINES, params2(db, "FUNCTION"), kind)
                    .await
            }
            ObjectKind::Procedure => {
                self.list_named(queries::ROUTINES, params2(db, "PROCEDURE"), kind)
                    .await
            }
            ObjectKind::Trigger => self.list_named(queries::TRIGGERS, params1(db), kind).await,
            ObjectKind::Event => self.list_named(queries::EVENTS, params1(db), kind).await,
            ObjectKind::Sequence if self.capabilities().sequences => {
                self.list_named(queries::SEQUENCES, params1(db), kind).await
            }
            ObjectKind::Sequence | ObjectKind::MaterializedView | ObjectKind::Type => {
                Ok(Vec::new())
            }
        }
    }

    async fn list_columns(&self, scope: &Scope) -> Result<Vec<TableColumns>, DbError> {
        let mut conn = self.live_conn().await?;
        let rows = fetch(&mut conn, queries::SCHEMA_COLUMNS, params1(&scope.database)).await?;
        Ok(group_table_columns(rows.iter().filter_map(|r| {
            Some((col(r, 0)?, col(r, 1)?, col(r, 2)?))
        })))
    }

    async fn table_detail(&self, obj: &ObjectRef) -> Result<TableDetail, DbError> {
        if !matches!(obj.kind, ObjectKind::Table | ObjectKind::View) {
            return Err(self.unsupported(&format!("table detail for {:?}", obj.kind)));
        }
        let (db, name) = (obj.database.as_str(), obj.name.as_str());
        let maria = self.server_info().variant == ServerVariant::MariaDb;
        let checks_sql = self.checks_query();
        let indexes_sql = self.indexes_query();

        let mut conn = self.live_conn().await?;
        let table = fetch(&mut conn, queries::TABLE, params2(db, name))
            .await?
            .into_iter()
            .next()
            .ok_or_else(|| no_such_table(obj))?;
        let table_type = col(&table, 0).unwrap_or_default();
        let is_view = table_kind(&table_type) == Some(ObjectKind::View);

        let mut columns: Vec<ColumnDetail> = fetch(&mut conn, queries::COLUMNS, params2(db, name))
            .await?
            .iter()
            .filter_map(|r| column_detail(r, maria))
            .collect();
        columns.sort_by_key(|c| c.ordinal);

        let mut indexes = group_indexes(&fetch(&mut conn, indexes_sql, params2(db, name)).await?);
        let primary_key = indexes.remove("PRIMARY");

        let fk_rows = fetch(&mut conn, queries::FOREIGN_KEYS, params2(db, name)).await?;
        let foreign_keys = group_foreign_keys(&fk_rows, db);

        let mut checks = match checks_sql {
            Some(sql) => fetch(&mut conn, sql, params2(db, name))
                .await?
                .iter()
                .filter_map(|r| {
                    Some(CheckDetail {
                        name: col(r, 0)?,
                        expression: col(r, 1).unwrap_or_default(),
                        definition: None,
                    })
                })
                .collect(),
            None => Vec::new(),
        };
        checks.sort_by(|a, b| a.name.cmp(&b.name));

        let mut triggers: Vec<TriggerDetail> =
            fetch(&mut conn, queries::TABLE_TRIGGERS, params2(db, name))
                .await?
                .iter()
                .filter_map(|r| {
                    Some(TriggerDetail {
                        name: col(r, 0)?,
                        timing: col(r, 1).unwrap_or_default(),
                        event: col(r, 2).unwrap_or_default(),
                    })
                })
                .collect();
        triggers.sort_by(|a, b| a.name.cmp(&b.name));

        let create_options = col(&table, 3).unwrap_or_default();
        let partitioned_by = if create_options.contains("partitioned") {
            fetch(&mut conn, queries::PARTITIONING, params2(db, name))
                .await?
                .first()
                .and_then(|r| {
                    let method = col(r, 0)?;
                    Some(match non_empty(r, 1) {
                        Some(expr) => format!("{method} ({expr})"),
                        None => method,
                    })
                })
        } else {
            None
        };
        drop(conn);

        if self.capabilities().json_is_alias {
            apply_json_alias(&mut columns, &checks);
        }

        let size_bytes = match (num(&table, 5), num(&table, 6)) {
            (Some(data), Some(index)) => Some(data + index),
            _ => None,
        };
        Ok(TableDetail {
            object: obj.clone(),
            columns,
            primary_key,
            indexes: indexes.into_values().collect(),
            foreign_keys,
            checks,
            triggers,
            options: TableOptions {
                engine: non_empty(&table, 1),
                collation: non_empty(&table, 2),
                tablespace: None,
                partitioned_by,
                system_versioned: table_type == "SYSTEM VERSIONED",
            },
            row_estimate: if is_view { None } else { num(&table, 4) },
            size_bytes,
            comment: if is_view { None } else { non_empty(&table, 7) },
        })
    }

    async fn object_ddl(&self, obj: &ObjectRef) -> Result<String, DbError> {
        let (what, column) = match obj.kind {
            ObjectKind::Table => ("TABLE", "Create Table"),
            ObjectKind::View => ("VIEW", "Create View"),
            ObjectKind::Function => ("FUNCTION", "Create Function"),
            ObjectKind::Procedure => ("PROCEDURE", "Create Procedure"),
            ObjectKind::Trigger => ("TRIGGER", "SQL Original Statement"),
            ObjectKind::Event => ("EVENT", "Create Event"),
            ObjectKind::Sequence if self.capabilities().sequences => ("SEQUENCE", "Create Table"),
            kind => return Err(self.unsupported(&format!("DDL for {kind:?}"))),
        };
        let sql = format!(
            "SHOW CREATE {what} {}.{}",
            quote_ident(&obj.database),
            quote_ident(&obj.name)
        );
        let row: Option<Row> = self
            .live_conn()
            .await?
            .query_first(sql)
            .await
            .map_err(map_err)?;
        let not_shown = || DbError::Server {
            code: None,
            sqlstate: None,
            message: format!(
                "the definition of {:?} {}.{} is not available to this user",
                obj.kind, obj.database, obj.name
            ),
            position: None,
        };
        let row = row.ok_or_else(not_shown)?;
        // A view's DDL is the second column; otherwise find the column by name.
        let idx = if obj.kind == ObjectKind::View {
            Some(1)
        } else {
            row.columns_ref()
                .iter()
                .position(|c| c.name_str() == column)
        };
        idx.and_then(|i| col(&text_row(row), i))
            .ok_or_else(not_shown)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(cells: &[Option<&str>]) -> TextRow {
        cells.iter().map(|c| c.map(str::to_string)).collect()
    }

    #[test]
    fn maps_table_types() {
        assert_eq!(table_kind("BASE TABLE"), Some(ObjectKind::Table));
        assert_eq!(table_kind("SYSTEM VERSIONED"), Some(ObjectKind::Table));
        assert_eq!(table_kind("VIEW"), Some(ObjectKind::View));
        assert_eq!(table_kind("SEQUENCE"), None);
    }

    #[test]
    fn mariadb_null_default_is_none() {
        let r = row(&[
            Some("c"),
            Some("2"),
            Some("varchar(5)"),
            Some("YES"),
            Some("NULL"),
            Some(""),
            None,
            Some(""),
        ]);
        assert_eq!(column_detail(&r, true).unwrap().default, None);
        assert_eq!(
            column_detail(&r, false).unwrap().default.as_deref(),
            Some("NULL")
        );
    }

    #[test]
    fn groups_index_parts_and_expressions() {
        let rows = vec![
            row(&[
                Some("PRIMARY"),
                Some("0"),
                Some("1"),
                Some("id"),
                Some("BTREE"),
                None,
            ]),
            row(&[
                Some("ix"),
                Some("1"),
                Some("1"),
                Some("a"),
                Some("BTREE"),
                None,
            ]),
            row(&[
                Some("ix"),
                Some("1"),
                Some("2"),
                None,
                Some("BTREE"),
                Some("lower(`b`)"),
            ]),
        ];
        let g = group_indexes(&rows);
        assert!(g["PRIMARY"].primary && g["PRIMARY"].unique);
        assert_eq!(g["ix"].columns, ["a"]);
        assert_eq!(g["ix"].expression.as_deref(), Some("lower(`b`)"));
        assert!(!g["ix"].unique);
    }

    #[test]
    fn json_alias_needs_a_json_valid_check() {
        let mut cols = vec![
            column_detail(
                &row(&[
                    Some("doc"),
                    Some("1"),
                    Some("longtext"),
                    Some("YES"),
                    None,
                    Some(""),
                    None,
                    None,
                ]),
                true,
            )
            .unwrap(),
            column_detail(
                &row(&[
                    Some("txt"),
                    Some("2"),
                    Some("longtext"),
                    Some("YES"),
                    None,
                    Some(""),
                    None,
                    None,
                ]),
                true,
            )
            .unwrap(),
        ];
        let checks = [CheckDetail {
            name: "c".into(),
            expression: "`doc` is null or json_valid(`doc`)".into(),
            definition: None,
        }];
        apply_json_alias(&mut cols, &checks);
        assert_eq!(cols[0].db_type, "json");
        assert_eq!(cols[1].db_type, "longtext");
    }
}
