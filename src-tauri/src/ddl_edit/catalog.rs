//! Catalog facts of an edited object (spec §4.2), read with parameterised statements
//! on the edit's session (PostgreSQL `$n`, MySQL `?`): what `build` needs beyond the
//! DDL text, and whether the object can be edited at all (ruling R9).
use dnt_core::driver::{ExecOptions, ResultSummary, RowSink, Session};
use dnt_core::error::DbError;
use dnt_core::meta::ObjectKind;
use dnt_core::server::DbKind;
use dnt_core::sql::classify;
use dnt_core::sql::ddl_edit::{CatalogFacts, EditTarget};
use dnt_core::value::{ColumnMeta, Value};

pub(crate) const PG_ROUTINE_SQL: &str = "SELECT count(*) OVER ()::text, p.prokind::text, pg_get_function_identity_arguments(p.oid), (obj_description(p.oid, 'pg_proc') IS NOT NULL)::text, (p.proacl IS NOT NULL)::text FROM pg_proc p JOIN pg_namespace n ON n.oid = p.pronamespace WHERE n.nspname = $1 AND p.proname = $2";
pub(crate) const PG_RELATION_SQL: &str = "SELECT c.oid::text, c.relkind::text, (obj_description(c.oid, 'pg_class') IS NOT NULL)::text, (c.relacl IS NOT NULL)::text FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace WHERE n.nspname = $1 AND c.relname = $2";
pub(crate) const PG_INDEX_SQL: &str = "SELECT pg_get_indexdef(i.indexrelid) FROM pg_index i JOIN pg_class ic ON ic.oid = i.indexrelid WHERE i.indrelid = $1::oid ORDER BY ic.relname";
pub(crate) const PG_TRIGGER_SQL: &str = "SELECT c.relname::text, c.oid::text, (obj_description(t.oid, 'pg_trigger') IS NOT NULL)::text FROM pg_trigger t JOIN pg_class c ON c.oid = t.tgrelid JOIN pg_namespace n ON n.oid = c.relnamespace WHERE n.nspname = $1 AND t.tgname = $2 AND NOT t.tgisinternal";
pub(crate) const MYSQL_TRIGGER_SQL: &str = "SELECT EVENT_OBJECT_TABLE, ACTION_TIMING, EVENT_MANIPULATION FROM information_schema.TRIGGERS WHERE TRIGGER_SCHEMA = ? AND TRIGGER_NAME = ?";
pub(crate) const MYSQL_SAME_TIMING_SQL: &str = "SELECT COUNT(*) FROM information_schema.TRIGGERS WHERE TRIGGER_SCHEMA = ? AND EVENT_OBJECT_TABLE = ? AND ACTION_TIMING = ? AND EVENT_MANIPULATION = ? AND TRIGGER_NAME <> ?";

/// Every row of the first result, each value as text (`None`: SQL NULL or a value
/// with no text form).
#[derive(Default)]
struct TextRows(Vec<Vec<Option<String>>>);

fn text(v: Value) -> Option<String> {
    match v {
        Value::Null => None,
        Value::Bool(b) => Some(b.to_string()),
        Value::Int(n) => Some(n.to_string()),
        Value::UInt(n) => Some(n.to_string()),
        Value::Float(n) => Some(n.to_string()),
        Value::Bytes { preview, .. } => String::from_utf8(preview).ok(),
        Value::Decimal(s)
        | Value::Text(s)
        | Value::Json(s)
        | Value::Uuid(s)
        | Value::Date(s)
        | Value::Time(s)
        | Value::DateTime(s)
        | Value::TimestampTz(s)
        | Value::Other { text: s, .. } => Some(s),
        Value::Array(_) => None,
    }
}

impl RowSink for TextRows {
    fn result_start(&mut self, _: &[ColumnMeta]) {}
    fn rows(&mut self, batch: Vec<Vec<Value>>) {
        self.0
            .extend(batch.into_iter().map(|r| r.into_iter().map(text).collect()));
    }
    fn result_end(&mut self, _: &ResultSummary) {}
}

async fn rows(
    session: &dyn Session,
    sql: &str,
    params: &[&str],
) -> Result<Vec<Vec<Option<String>>>, DbError> {
    let options = ExecOptions {
        params: params.iter().map(|p| p.to_string()).collect(),
        ..ExecOptions::default()
    };
    rows_with(session, sql, options).await
}

/// Every row of `sql` run with `options` (its params, stop and timeout), as text.
pub(crate) async fn rows_with(
    session: &dyn Session,
    sql: &str,
    options: ExecOptions,
) -> Result<Vec<Vec<Option<String>>>, DbError> {
    let mut sink = TextRows::default();
    let options = ExecOptions {
        // Every byte of a binary value (MySQL may type catalog text as binary).
        full_bytes: true,
        ..options
    };
    session
        .execute_one(sql, classify(sql, session.dialect()), &options, &mut sink)
        .await?;
    Ok(sink.0)
}

fn col(row: &[Option<String>], i: usize) -> Option<String> {
    row.get(i).cloned().flatten()
}

fn flag(row: &[Option<String>], i: usize) -> bool {
    matches!(col(row, i).as_deref(), Some("true" | "t" | "1"))
}

/// The catalog facts of the existing object `t`, and the reason it cannot be edited
/// (several PostgreSQL routines of that name, a PostgreSQL trigger name used on
/// several tables). A new object (`t.name` = `None`) has default facts.
pub(crate) async fn facts(
    session: &dyn Session,
    t: &EditTarget,
) -> Result<(CatalogFacts, Option<String>), DbError> {
    let mut facts = CatalogFacts::default();
    let Some(name) = t.name.as_deref() else {
        return Ok((facts, None));
    };
    let pg = session.server_info().kind == DbKind::Postgres;
    if pg {
        let schema = t.schema.as_deref().unwrap_or_default();
        match t.kind {
            ObjectKind::Function | ObjectKind::Procedure => {
                let found = rows(session, PG_ROUTINE_SQL, &[schema, name]).await?;
                if found.len() > 1 {
                    let n = found.len();
                    return Ok((
                        facts,
                        Some(format!(
                            "{n} routines are named {name} in {schema}; editing overloaded routines is not supported. Use a query tab."
                        )),
                    ));
                }
                if let Some(r) = found.first() {
                    let expected = if t.kind == ObjectKind::Procedure {
                        "p"
                    } else {
                        "f"
                    };
                    let actual = col(r, 1).unwrap_or_default();
                    if actual != expected {
                        let what = match actual.as_str() {
                            "p" => "procedure",
                            "f" => "function",
                            "a" => "aggregate function",
                            "w" => "window function",
                            _ => "routine of another kind",
                        };
                        let wanted = if expected == "p" {
                            "procedure"
                        } else {
                            "function"
                        };
                        return Ok((
                            facts,
                            Some(format!(
                                "{name} in {schema} is a {what}, not a {wanted}; edit it with a query tab."
                            )),
                        ));
                    }
                    facts.identity_args = col(r, 2);
                    facts.has_comment = flag(r, 3);
                    facts.has_grants = flag(r, 4);
                }
            }
            ObjectKind::View | ObjectKind::MaterializedView => {
                let found = rows(session, PG_RELATION_SQL, &[schema, name]).await?;
                if let Some(r) = found.first() {
                    facts.has_comment = flag(r, 2);
                    facts.has_grants = flag(r, 3);
                    if let (Some(oid), Some("m")) = (col(r, 0), col(r, 1).as_deref()) {
                        facts.index_defs = rows(session, PG_INDEX_SQL, &[&oid])
                            .await?
                            .iter()
                            .filter_map(|r| col(r, 0))
                            .collect();
                    }
                }
            }
            ObjectKind::Trigger => {
                let found = rows(session, PG_TRIGGER_SQL, &[schema, name]).await?;
                if found.len() > 1 {
                    let n = found.len();
                    return Ok((
                        facts,
                        Some(format!(
                            "Trigger {name} exists on {n} tables in {schema}; edit it with a query tab."
                        )),
                    ));
                }
                if let Some(r) = found.first() {
                    facts.table = col(r, 0);
                    facts.table_oid = col(r, 1).and_then(|s| s.parse().ok());
                    facts.has_comment = flag(r, 2);
                }
            }
            _ => {}
        }
    } else if t.kind == ObjectKind::Trigger {
        let db = t.database.as_str();
        let found = rows(session, MYSQL_TRIGGER_SQL, &[db, name]).await?;
        if let Some(r) = found.first() {
            let (table, timing, event) = (col(r, 0), col(r, 1), col(r, 2));
            if let (Some(table), Some(timing), Some(event)) = (&table, &timing, &event) {
                let others = rows(
                    session,
                    MYSQL_SAME_TIMING_SQL,
                    &[db, table, timing, event, name],
                )
                .await?;
                facts.same_timing_triggers = others
                    .first()
                    .and_then(|r| col(r, 0))
                    .and_then(|n| n.trim().parse().ok())
                    .unwrap_or(0);
                facts.timing_event = Some(format!("{timing} {event}"));
            }
            facts.table = table;
        }
    }
    Ok((facts, None))
}
