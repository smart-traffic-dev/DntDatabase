//! PostgreSQL rendering: one statement per clause. The backend sends them inside one
//! `BEGIN … COMMIT` (Task 10); `render` returns only the inner statements.
//!
//! Statements follow the spec order whatever order the changes come in (ruling T6-3):
//! drops (foreign keys, checks, indexes, primary key, then columns) → rename table →
//! rename columns, then indexes → alter columns → add columns → add primary key,
//! unique / index, check, foreign key → `COMMENT ON` (table, columns, indexes).
//! Statements after a table rename name the table by its new name; impact counts run
//! before anything is applied, so they name the old table and the old column names.
//!
//! Element fragments are spelled like `pg_get_constraintdef` / `pg_get_indexdef` where
//! the syntax allows (`, ` between columns, `REFERENCES t(c)`, `NO ACTION` and
//! `MATCH SIMPLE` left out, `USING btree` always written, method in lower case) so
//! Task 9 can compare them with the server; identifiers are always quoted.
//!
//! An index keeps the form the server has (`IndexDef.constraint`): a constraint index is
//! `CONSTRAINT n UNIQUE (…) [INCLUDE (…)]`, added and dropped with `ALTER TABLE`; a
//! plain index is created with `CREATE [UNIQUE] INDEX n ON <table> <fragment>`, where
//! its fragment is the `pg_get_indexdef` tail `USING method (…) [INCLUDE (…)]`, and
//! dropped with a schema-qualified `DROP INDEX`.

use super::{fk_action, invalid, is_number, unsupported, Impacts, RenderError, Rendered};
use crate::diff::Change;
use crate::model::*;
use crate::types::type_text;
use dnt_core::meta::IdentityKind;
use dnt_core::server::{DbKind, ServerInfo};
use dnt_core::sql::build::{literal, BuildError, ColumnTarget, TableRef};
use dnt_core::sql::ident::{count_where_sql, qualified_name, quote_ident};
use dnt_core::sql::Dialect;
use dnt_core::value::{FullValue, LogicalType, Value};

fn q(name: &str) -> String {
    quote_ident(DbKind::Postgres, name)
}

fn check_dialect(d: Dialect) -> Result<(), RenderError> {
    if d == Dialect::Postgres {
        Ok(())
    } else {
        Err(invalid(
            "The SQL dialect does not belong to a PostgreSQL server.",
        ))
    }
}

/// A standard string literal `'…'` (core literal builder: `'` doubled, NUL refused,
/// never `E'…'`).
fn string(s: &str) -> Result<String, RenderError> {
    let text = ColumnTarget {
        name: String::new(),
        db_type: "text".into(),
        logical: LogicalType::Text,
        collation: None,
    };
    let v = FullValue::try_from_value(Value::Text(s.into()))
        .map_err(|e| invalid(format!("Cannot write this text: {e}.")))?;
    literal(&v, &text, Dialect::Postgres).map_err(|e| match e {
        BuildError::Invalid { reason, .. } => invalid(format!("Cannot write this text: {reason}.")),
        other => invalid(other.to_string()),
    })
}

fn names(cols: &[String]) -> String {
    cols.iter().map(|c| q(c)).collect::<Vec<_>>().join(", ")
}

/// `COMMENT ON <what> <target> IS '…'`; an empty or missing comment is `NULL`.
fn comment_sql(what: &str, target: &str, comment: Option<&str>) -> Result<String, RenderError> {
    let value = match comment.filter(|c| !c.is_empty()) {
        Some(c) => string(c)?,
        None => "NULL".into(),
    };
    Ok(format!("COMMENT ON {what} {target} IS {value}"))
}

const OPTIONS_MYSQL_ONLY: &str =
    "Table options (engine, character set, collation, row format) are MySQL only.";

/// The spec position of a change (ruling T6-3); the sort is stable, so changes of one
/// kind keep the order the diff gave them. Changes PostgreSQL refuses sort first.
fn rank(c: &Change) -> u8 {
    match c {
        Change::CreateTable | Change::SetOptions { .. } | Change::MoveColumn { .. } => 0,
        Change::DropForeignKey { .. } => 1,
        Change::DropCheck { .. } => 2,
        Change::DropIndex { .. } => 3,
        Change::DropPrimaryKey { .. } => 4,
        Change::DropColumn { .. } => 5,
        Change::RenameTable { .. } => 6,
        Change::RenameColumn { .. } => 7,
        Change::RenameIndex { .. } => 8,
        Change::AlterColumn { .. } => 9,
        Change::AddColumn { .. } => 10,
        Change::AddPrimaryKey { .. } => 11,
        Change::AddIndex { .. } => 12,
        Change::AddCheck { .. } => 13,
        Change::AddForeignKey { .. } => 14,
        Change::SetComment { .. } => 15,
    }
}

pub(super) fn render(
    table: &TableRef,
    old: &TableDef,
    changes: &[Change],
    server: &ServerInfo,
    d: Dialect,
) -> Result<Rendered, RenderError> {
    check_dialect(d)?;
    let schema = table.schema.as_deref();
    let rel = |name: &str| qualified_name(DbKind::Postgres, &table.database, schema, name);
    let mut ordered: Vec<&Change> = changes.iter().collect();
    ordered.sort_by_key(|c| rank(c));

    let mut out = Alter {
        server,
        d,
        old_relation: rel(&table.name),
        relation: rel(&table.name),
        statements: Vec::new(),
        table_comment: None,
        comments: Vec::new(),
        impacts: Impacts::default(),
    };
    for c in ordered {
        let cur = out.relation.clone();
        match c {
            Change::CreateTable => {
                return Err(invalid("A new table is rendered with render_create."))
            }
            Change::SetOptions { .. } => return Err(invalid(OPTIONS_MYSQL_ONLY)),
            Change::MoveColumn { .. } => {
                return Err(unsupported("column order", "not supported by PostgreSQL"))
            }
            Change::DropForeignKey { name } | Change::DropCheck { name } => out
                .statements
                .push(format!("ALTER TABLE {cur} DROP CONSTRAINT {}", q(name))),
            Change::DropIndex { name } => {
                let index = old
                    .indexes
                    .iter()
                    .find(|i| &i.name == name)
                    .ok_or_else(|| invalid(format!("Index \"{name}\" is not in the table.")))?;
                out.statements.push(if index.constraint {
                    format!("ALTER TABLE {cur} DROP CONSTRAINT {}", q(name))
                } else {
                    format!("DROP INDEX {}", rel(name))
                });
            }
            Change::DropPrimaryKey { name } => {
                let name = name
                    .as_deref()
                    .filter(|n| !n.is_empty())
                    .ok_or_else(|| invalid("The primary key has no constraint name."))?;
                out.statements
                    .push(format!("ALTER TABLE {cur} DROP CONSTRAINT {}", q(name)));
            }
            Change::DropColumn { name } => {
                out.statements
                    .push(format!("ALTER TABLE {cur} DROP COLUMN {}", q(name)));
                out.impacts.data_loss(
                    name,
                    format!("Dropping column \"{name}\" deletes its values."),
                    Some(count_where_sql(
                        &out.old_relation,
                        &format!(" WHERE {} IS NOT NULL", q(name)),
                    )),
                );
            }
            Change::RenameTable { to, .. } => {
                out.statements
                    .push(format!("ALTER TABLE {cur} RENAME TO {}", q(to)));
                out.relation = rel(to);
            }
            Change::RenameColumn { from, to } => out.statements.push(format!(
                "ALTER TABLE {cur} RENAME COLUMN {} TO {}",
                q(from),
                q(to)
            )),
            Change::RenameIndex { from, to } => {
                out.statements
                    .push(format!("ALTER INDEX {} RENAME TO {}", rel(from), q(to)))
            }
            Change::AlterColumn {
                old: oc, new: nc, ..
            } => out.column(oc, nc)?,
            Change::AddColumn { column, .. } => {
                // PostgreSQL appends: the position is not used (column order is hidden).
                out.statements.push(format!(
                    "ALTER TABLE {cur} ADD COLUMN {}",
                    column_sql(column, server, d)?
                ));
                let name = &column.name;
                if column.identity.is_some() {
                    out.impacts.rebuild(
                        name,
                        format!("Adding identity column \"{name}\" rewrites the table."),
                    );
                } else if matches!(column.default, Some(DefaultSpec::Expression { .. })) {
                    out.impacts.rebuild(
                        name,
                        format!(
                            "Adding column \"{name}\" with a computed default may rewrite \
                             the table."
                        ),
                    );
                }
                if column.comment.as_deref().is_some_and(|c| !c.is_empty()) {
                    out.comments.push(comment_sql(
                        "COLUMN",
                        &format!("{cur}.{}", q(name)),
                        column.comment.as_deref(),
                    )?);
                }
            }
            Change::AddPrimaryKey { key } => out.statements.push(format!(
                "ALTER TABLE {cur} ADD {}",
                primary_key_sql(key, server, d)?
            )),
            Change::AddIndex { index } => {
                out.statements.push(if index.constraint {
                    format!("ALTER TABLE {cur} ADD {}", index_sql(index, server, d)?)
                } else {
                    create_index_sql(&cur, index, server, d)?
                });
                if index.comment.as_deref().is_some_and(|c| !c.is_empty()) {
                    out.comments.push(comment_sql(
                        "INDEX",
                        &rel(&index.name),
                        index.comment.as_deref(),
                    )?);
                }
            }
            Change::AddCheck { check } => out.statements.push(format!(
                "ALTER TABLE {cur} ADD {}",
                check_sql(check, server, d)?
            )),
            Change::AddForeignKey { fk } => out.statements.push(format!(
                "ALTER TABLE {cur} ADD {}",
                fk_sql(&in_schema(fk, schema), server, d)?
            )),
            Change::SetComment { comment } => {
                out.table_comment = Some(comment_sql("TABLE", &cur, comment.as_deref())?)
            }
        }
    }
    let mut statements = out.statements;
    statements.extend(out.table_comment);
    statements.extend(out.comments);
    Ok(Rendered {
        statements,
        impacts: out.impacts.finish(),
    })
}

/// `fk` with a missing referenced schema (= the table's own) written out, so the
/// statement does not depend on the session's `search_path`.
fn in_schema(fk: &FkDef, schema: Option<&str>) -> FkDef {
    FkDef {
        ref_schema: fk.ref_schema.clone().or_else(|| schema.map(str::to_string)),
        ..fk.clone()
    }
}

/// The statements of one `render` while they are built.
struct Alter<'a> {
    server: &'a ServerInfo,
    d: Dialect,
    /// The table as stored (impact counts run before applying).
    old_relation: String,
    /// The table as the next statement names it (after a rename, the new name).
    relation: String,
    statements: Vec<String>,
    table_comment: Option<String>,
    comments: Vec<String>,
    impacts: Impacts,
}

impl Alter<'_> {
    /// The statements that turn column `oc` into `nc` (renamed already, so named
    /// `nc.name`), in an order each one can run in: identity off, default off, type,
    /// nullability, default on, identity on.
    fn column(&mut self, oc: &ColumnDef, nc: &ColumnDef) -> Result<(), RenderError> {
        column_sql(nc, self.server, self.d)?;
        let name = nc.name.as_str();
        let col = q(name);
        let at = format!("ALTER TABLE {} ALTER COLUMN {col}", self.relation);
        let old_type = type_text(&oc.data_type, self.server);
        let new_type = type_text(&nc.data_type, self.server);
        let type_changed = old_type != new_type;
        let collation_changed = oc.collation != nc.collation;
        let default_changed = oc.default != nc.default;

        if oc.identity.is_some() && nc.identity.is_none() {
            self.statements.push(format!("{at} DROP IDENTITY"));
        }
        // A changing default goes before a type change, which would convert it.
        if default_changed && oc.default.is_some() && (nc.default.is_none() || type_changed) {
            self.statements.push(format!("{at} DROP DEFAULT"));
        }
        if type_changed || collation_changed {
            // Without COLLATE the column takes the new type's default collation.
            let mut s = format!("{at} TYPE {}", type_sql(&nc.data_type, self.server)?);
            if let Some(c) = &nc.collation {
                s.push_str(&format!(" COLLATE {}", q(c)));
            }
            if type_changed {
                match nc.using.as_deref().map(str::trim).filter(|u| !u.is_empty()) {
                    Some(u) => s.push_str(&format!(" USING {u}")),
                    None if safe_cast(&oc.data_type, &nc.data_type) => {
                        s.push_str(&format!(" USING {col}::{new_type}"))
                    }
                    None => {
                        return Err(invalid(format!(
                            "Changing {old_type} to {new_type} needs a USING expression"
                        )))
                    }
                }
            }
            self.statements.push(s);
            let original = q(&oc.name);
            if type_changed {
                match lossy(&oc.data_type, &nc.data_type) {
                    Some(Some(n)) => self.impacts.data_loss(
                        name,
                        format!("Values of \"{name}\" longer than {n} characters are cut."),
                        Some(count_where_sql(
                            &self.old_relation,
                            &format!(" WHERE char_length({original}) > {n}"),
                        )),
                    ),
                    Some(None) => self.impacts.data_loss(
                        name,
                        format!(
                            "Changing the type of \"{name}\" may lose values or fail on \
                             values that do not convert."
                        ),
                        None,
                    ),
                    None => {}
                }
                self.impacts.rebuild(
                    name,
                    format!("Changing the type of \"{name}\" rewrites the table and its indexes."),
                );
            } else {
                self.impacts.rebuild(
                    name,
                    format!("Changing the collation of \"{name}\" rebuilds its indexes."),
                );
            }
        }
        if oc.nullable != nc.nullable {
            if nc.nullable {
                self.statements.push(format!("{at} DROP NOT NULL"));
            } else {
                self.statements.push(format!("{at} SET NOT NULL"));
                self.impacts.data_loss(
                    name,
                    format!(
                        "Column \"{name}\" becomes NOT NULL: rows holding NULL make the \
                         change fail."
                    ),
                    Some(count_where_sql(
                        &self.old_relation,
                        &format!(" WHERE {} IS NULL", q(&oc.name)),
                    )),
                );
            }
        }
        if default_changed {
            if let Some(def) = &nc.default {
                self.statements
                    .push(format!("{at} SET DEFAULT {}", default_sql(nc, def)?));
            }
        }
        match (oc.identity, nc.identity) {
            (None, Some(k)) => self.statements.push(format!(
                "{at} ADD GENERATED {} AS IDENTITY",
                identity_word(k)
            )),
            (Some(a), Some(b)) if a != b => self
                .statements
                .push(format!("{at} SET GENERATED {}", identity_word(b))),
            _ => {}
        }
        if oc.comment != nc.comment {
            self.comments.push(comment_sql(
                "COLUMN",
                &format!("{}.{col}", self.relation),
                nc.comment.as_deref(),
            )?);
        }
        Ok(())
    }
}

fn identity_word(k: IdentityKind) -> &'static str {
    match k {
        IdentityKind::Always => "ALWAYS",
        IdentityKind::ByDefault => "BY DEFAULT",
    }
}

const INT: &[&str] = &["smallint", "integer", "bigint"];
const TEXT: &[&str] = &["text", "varchar"];
const ANY: &[&str] = &[];

/// `(from, to)` base types a type change converts with an automatic `USING
/// col::<type>`; an empty list matches any type, custom ones included. The same base
/// with other parameters (`varchar(10)` → `varchar(20)`) always does. Anything else
/// needs the user's `ColumnDef.using`.
const SAFE_CASTS: &[(&[&str], &[&str])] = &[
    (INT, INT),
    (&["numeric"], INT),
    (INT, &["numeric"]),
    (ANY, TEXT),
    (&["timestamp"], &["timestamptz"]),
    (&["timestamptz"], &["timestamp"]),
    (&["json"], &["jsonb"]),
    (
        TEXT,
        &[
            "json",
            "jsonb",
            "uuid",
            "date",
            "timestamp",
            "numeric",
            "smallint",
            "integer",
            "bigint",
        ],
    ),
];

fn base(t: &TypeSpec) -> Option<&str> {
    match t {
        TypeSpec::Known { base, .. } => Some(base.as_str()),
        TypeSpec::Custom { .. } => None,
    }
}

fn safe_cast(old: &TypeSpec, new: &TypeSpec) -> bool {
    let Some(n) = base(new) else {
        return false;
    };
    let o = base(old);
    o == Some(n)
        || SAFE_CASTS.iter().any(|(from, to)| {
            (from.is_empty() || o.is_some_and(|o| from.contains(&o))) && to.contains(&n)
        })
}

#[derive(PartialEq)]
enum Family {
    /// Decimal digits the integer type holds.
    Int(u32),
    Numeric,
    Float(u8),
    Str,
    /// Fractional-second types; `date` is `Date`.
    Temporal,
    Date,
    Json,
    Other,
}

fn family(base: &str) -> Family {
    match base {
        "smallint" => Family::Int(5),
        "integer" => Family::Int(10),
        "bigint" => Family::Int(19),
        "numeric" => Family::Numeric,
        "real" => Family::Float(1),
        "double precision" => Family::Float(2),
        "char" | "varchar" | "text" => Family::Str,
        "time" | "timetz" | "timestamp" | "timestamptz" | "interval" => Family::Temporal,
        "date" => Family::Date,
        "json" | "jsonb" => Family::Json,
        _ => Family::Other,
    }
}

/// Most characters a string type holds (`None` = unlimited).
fn text_cap(base: &str, length: Option<u32>) -> Option<u64> {
    match base {
        "char" => Some(u64::from(length.unwrap_or(1))),
        "varchar" => length.map(u64::from),
        _ => None,
    }
}

/// What a type change may lose: `None` when nothing, `Some(Some(n))` when values longer
/// than `n` characters are cut (countable), `Some(None)` when it cannot be counted.
fn lossy(old: &TypeSpec, new: &TypeSpec) -> Option<Option<u64>> {
    // Every value reads back as unbounded text.
    if let TypeSpec::Known { base, length, .. } = new {
        if base == "text" || (base == "varchar" && length.is_none()) {
            return None;
        }
    }
    let (
        TypeSpec::Known {
            base: ob,
            length: ol,
            precision: op,
            scale: os,
            ..
        },
        TypeSpec::Known {
            base: nb,
            length: nl,
            precision: np,
            scale: ns,
            ..
        },
    ) = (old, new)
    else {
        return Some(None);
    };
    let maybe = |lossy: bool| lossy.then_some(None);
    let (os, ns) = (os.unwrap_or(0), ns.unwrap_or(0));
    match (family(ob), family(nb)) {
        (Family::Int(a), Family::Int(b)) => maybe(b < a),
        (Family::Int(a), Family::Numeric) => maybe(np.is_some_and(|p| p.saturating_sub(ns) < a)),
        (Family::Numeric, Family::Numeric) => match (op, np) {
            (_, None) => None,
            (None, Some(_)) => Some(None),
            (Some(o), Some(n)) => maybe(n.saturating_sub(ns) < o.saturating_sub(os) || ns < os),
        },
        (Family::Float(a), Family::Float(b)) => maybe(b < a),
        (Family::Str, Family::Str) => match (text_cap(ob, *ol), text_cap(nb, *nl)) {
            (_, None) => None,
            (Some(o), Some(n)) if o <= n => None,
            (_, Some(n)) => Some(Some(n)),
        },
        (Family::Temporal, Family::Temporal) => {
            let related = ob == nb
                || matches!(
                    (ob.as_str(), nb.as_str()),
                    ("timestamp", "timestamptz")
                        | ("timestamptz", "timestamp")
                        | ("time", "timetz")
                        | ("timetz", "time")
                );
            maybe(!related || np.unwrap_or(6) < op.unwrap_or(6))
        }
        (Family::Date, Family::Temporal) => maybe(!nb.starts_with("timestamp")),
        (Family::Json, Family::Json) => None,
        _ => Some(None),
    }
}

/// `CREATE [UNIQUE] INDEX n ON <relation> USING … (…)` for a plain index.
fn create_index_sql(
    relation: &str,
    i: &IndexDef,
    server: &ServerInfo,
    d: Dialect,
) -> Result<String, RenderError> {
    Ok(format!(
        "CREATE {}INDEX {} ON {relation} {}",
        if i.unique { "UNIQUE " } else { "" },
        q(&i.name),
        index_sql(i, server, d)?
    ))
}

pub(super) fn render_create(
    table: &TableRef,
    def: &TableDef,
    server: &ServerInfo,
    d: Dialect,
) -> Result<Rendered, RenderError> {
    check_dialect(d)?;
    if def.columns.is_empty() {
        return Err(invalid("A table needs at least one column."));
    }
    let o = &def.options;
    if o.engine.is_some() || o.charset.is_some() || o.collation.is_some() || o.row_format.is_some()
    {
        return Err(invalid(OPTIONS_MYSQL_ONLY));
    }
    let schema = table.schema.as_deref();
    let rel = |name: &str| qualified_name(DbKind::Postgres, &table.database, schema, name);
    let relation = rel(&def.name);
    let mut items = Vec::new();
    for c in &def.columns {
        items.push(column_sql(c, server, d)?);
    }
    if let Some(k) = &def.primary_key {
        items.push(primary_key_sql(k, server, d)?);
    }
    for i in def.indexes.iter().filter(|i| i.constraint) {
        items.push(index_sql(i, server, d)?);
    }
    for f in &def.foreign_keys {
        items.push(fk_sql(&in_schema(f, schema), server, d)?);
    }
    for c in &def.checks {
        items.push(check_sql(c, server, d)?);
    }
    let mut statements = vec![format!(
        "CREATE TABLE {relation} (\n  {}\n)",
        items.join(",\n  ")
    )];
    for i in def.indexes.iter().filter(|i| !i.constraint) {
        statements.push(create_index_sql(&relation, i, server, d)?);
    }
    let has = |c: &Option<String>| c.as_deref().is_some_and(|c| !c.is_empty());
    if has(&def.comment) {
        statements.push(comment_sql("TABLE", &relation, def.comment.as_deref())?);
    }
    for c in def.columns.iter().filter(|c| has(&c.comment)) {
        statements.push(comment_sql(
            "COLUMN",
            &format!("{relation}.{}", q(&c.name)),
            c.comment.as_deref(),
        )?);
    }
    for i in def.indexes.iter().filter(|i| has(&i.comment)) {
        statements.push(comment_sql("INDEX", &rel(&i.name), i.comment.as_deref())?);
    }
    Ok(Rendered {
        statements,
        impacts: vec![],
    })
}

/// The column type: the catalog spelling, or a custom type as typed.
fn type_sql(t: &TypeSpec, server: &ServerInfo) -> Result<String, RenderError> {
    match t {
        TypeSpec::Custom { text } if text.trim().is_empty() => Err(invalid("A type is required.")),
        TypeSpec::Custom { text } => Ok(text.clone()),
        TypeSpec::Known {
            base,
            unsigned,
            values,
            ..
        } => {
            let plain = !base.is_empty()
                && base
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b' ');
            if !plain {
                return Err(invalid(format!("Invalid type \"{base}\".")));
            }
            if *unsigned {
                return Err(invalid("UNSIGNED is MySQL only."));
            }
            if !values.is_empty() {
                return Err(invalid(
                    "ENUM and SET are MySQL only; use a PostgreSQL enum type.",
                ));
            }
            Ok(type_text(t, server))
        }
    }
}

pub(super) fn column_sql(
    c: &ColumnDef,
    server: &ServerInfo,
    d: Dialect,
) -> Result<String, RenderError> {
    check_dialect(d)?;
    if c.generated.is_some() {
        return Err(invalid(format!(
            "Generated column \"{}\" cannot be written by the designer.",
            c.name
        )));
    }
    if c.charset.is_some() {
        return Err(invalid("Column character sets are MySQL only."));
    }
    if c.on_update.is_some() {
        return Err(invalid("ON UPDATE is MySQL only."));
    }
    if c.identity.is_some() && c.default.is_some() {
        return Err(invalid(format!(
            "Identity column \"{}\" cannot have a default.",
            c.name
        )));
    }
    // `auto_increment` is not read: introspection sets it for identity and serial
    // columns, which `identity` and the `nextval(…)` default already describe.
    let mut out = format!("{} {}", q(&c.name), type_sql(&c.data_type, server)?);
    if let Some(co) = &c.collation {
        out.push_str(&format!(" COLLATE {}", q(co)));
    }
    if let Some(k) = c.identity {
        out.push_str(&format!(" GENERATED {} AS IDENTITY", identity_word(k)));
    }
    if !c.nullable {
        out.push_str(" NOT NULL");
    }
    if let Some(def) = &c.default {
        out.push_str(&format!(" DEFAULT {}", default_sql(c, def)?));
    }
    Ok(out)
}

const NUMERIC: &[&str] = &[
    "smallint",
    "integer",
    "bigint",
    "numeric",
    "real",
    "double precision",
];

/// A value default is a number on a numeric column, `true` / `false` on a boolean
/// column (as `pg_get_expr` spells them), else a string literal the server converts
/// to the column type. An expression default is spliced verbatim.
fn default_sql(c: &ColumnDef, def: &DefaultSpec) -> Result<String, RenderError> {
    match def {
        DefaultSpec::Value { text } => {
            let b = base(&c.data_type).unwrap_or("");
            if NUMERIC.contains(&b) && is_number(text) {
                Ok(text.clone())
            } else if b == "boolean"
                && (text.eq_ignore_ascii_case("true") || text.eq_ignore_ascii_case("false"))
            {
                Ok(text.to_ascii_lowercase())
            } else {
                string(text)
            }
        }
        DefaultSpec::Expression { text } => {
            let t = text.trim();
            if t.is_empty() {
                return Err(invalid(format!("The default of \"{}\" is empty.", c.name)));
            }
            Ok(t.to_string())
        }
    }
}

pub(super) fn primary_key_sql(
    k: &KeyDef,
    _server: &ServerInfo,
    d: Dialect,
) -> Result<String, RenderError> {
    check_dialect(d)?;
    if k.columns.is_empty() {
        return Err(invalid("A primary key needs at least one column."));
    }
    let body = format!("PRIMARY KEY ({})", names(&k.columns));
    Ok(match k.name.as_deref().filter(|n| !n.is_empty()) {
        Some(n) => format!("CONSTRAINT {} {body}", q(n)),
        None => body,
    })
}

/// A constraint index: `CONSTRAINT n UNIQUE (…) [INCLUDE (…)]`; a plain index: the
/// `pg_get_indexdef` tail `USING method (…) [INCLUDE (…)]` (see the module docs).
pub(super) fn index_sql(
    i: &IndexDef,
    _server: &ServerInfo,
    d: Dialect,
) -> Result<String, RenderError> {
    check_dialect(d)?;
    if i.parts.is_empty() {
        return Err(invalid(format!(
            "Index \"{}\" needs at least one column.",
            i.name
        )));
    }
    if !i.visible {
        return Err(invalid("Invisible indexes are MySQL only."));
    }
    if i.parts.iter().any(|p| p.prefix.is_some()) {
        return Err(invalid("Index prefix lengths are MySQL only."));
    }
    let include = if i.include.is_empty() {
        String::new()
    } else {
        format!(" INCLUDE ({})", names(&i.include))
    };
    if i.constraint {
        if !i.unique {
            return Err(invalid(format!(
                "Constraint index \"{}\" must be unique.",
                i.name
            )));
        }
        if i.method
            .as_deref()
            .is_some_and(|m| !m.eq_ignore_ascii_case("btree"))
        {
            return Err(invalid("A UNIQUE constraint always uses a btree index."));
        }
        let mut cols = Vec::new();
        for p in &i.parts {
            match (&p.column, &p.expression, p.descending) {
                (Some(c), None, false) => cols.push(c.clone()),
                _ => {
                    return Err(invalid(
                        "A UNIQUE constraint takes plain ascending columns only.",
                    ))
                }
            }
        }
        return Ok(format!(
            "CONSTRAINT {} UNIQUE ({}){include}",
            q(&i.name),
            names(&cols)
        ));
    }
    let method = match &i.method {
        Some(m) => super::word("index method", m)?.to_ascii_lowercase(),
        None => "btree".into(),
    };
    let mut parts = Vec::new();
    for p in &i.parts {
        let mut s = match (&p.column, &p.expression) {
            (Some(c), None) => q(c),
            (None, Some(e)) if !e.trim().is_empty() => format!("({})", e.trim()),
            _ => {
                return Err(invalid(
                    "Each index part is either a column or an expression.",
                ))
            }
        };
        if p.descending {
            s.push_str(" DESC");
        }
        parts.push(s);
    }
    Ok(format!("USING {method} ({}){include}", parts.join(", ")))
}

/// `CONSTRAINT n FOREIGN KEY (…) REFERENCES [s.]t(…) [MATCH FULL|PARTIAL] [ON UPDATE
/// …] [ON DELETE …] [DEFERRABLE [INITIALLY DEFERRED]]`, the order of
/// `pg_get_constraintdef`; a missing `ref_schema` is left unqualified (`render`
/// qualifies it with the table's schema).
pub(super) fn fk_sql(f: &FkDef, _server: &ServerInfo, d: Dialect) -> Result<String, RenderError> {
    check_dialect(d)?;
    if f.columns.is_empty() || f.columns.len() != f.ref_columns.len() {
        return Err(invalid(format!(
            "Foreign key \"{}\" needs as many referenced columns as columns.",
            f.name
        )));
    }
    if f.initially_deferred && !f.deferrable {
        return Err(invalid(format!(
            "Foreign key \"{}\" can only be INITIALLY DEFERRED when it is DEFERRABLE.",
            f.name
        )));
    }
    let target = match &f.ref_schema {
        Some(s) => format!("{}.{}", q(s), q(&f.ref_table)),
        None => q(&f.ref_table),
    };
    let mut out = format!(
        "CONSTRAINT {} FOREIGN KEY ({}) REFERENCES {target}({})",
        q(&f.name),
        names(&f.columns),
        names(&f.ref_columns)
    );
    if let Some(m) = &f.match_type {
        match m.trim().to_ascii_uppercase().as_str() {
            "SIMPLE" => {}
            "FULL" => out.push_str(" MATCH FULL"),
            "PARTIAL" => out.push_str(" MATCH PARTIAL"),
            _ => return Err(invalid(format!("Unknown match type \"{m}\"."))),
        }
    }
    for (kw, action) in [("ON UPDATE", &f.on_update), ("ON DELETE", &f.on_delete)] {
        let a = if action.trim().is_empty() {
            "NO ACTION"
        } else {
            fk_action(action)?
        };
        if a != "NO ACTION" {
            out.push_str(&format!(" {kw} {a}"));
        }
    }
    if f.deferrable {
        out.push_str(" DEFERRABLE");
    }
    if f.initially_deferred {
        out.push_str(" INITIALLY DEFERRED");
    }
    Ok(out)
}

pub(super) fn check_sql(
    c: &CheckDef,
    _server: &ServerInfo,
    d: Dialect,
) -> Result<String, RenderError> {
    check_dialect(d)?;
    if c.expression.trim().is_empty() {
        return Err(invalid(format!("Check \"{}\" is empty.", c.name)));
    }
    Ok(format!(
        "CONSTRAINT {} CHECK ({})",
        q(&c.name),
        c.expression.trim()
    ))
}

#[cfg(test)]
mod tests {
    use super::super::{Impact, ImpactKind};
    use super::*;
    use crate::diff;
    use crate::test_util::*;
    use dnt_core::meta::IdentityKind;
    use dnt_core::server::DbKind;

    const PG: Dialect = Dialect::Postgres;

    fn ty(base: &str, length: Option<u32>, precision: Option<u32>, scale: Option<u32>) -> TypeSpec {
        TypeSpec::Known {
            base: base.into(),
            length,
            precision,
            scale,
            unsigned: false,
            values: vec![],
        }
    }

    fn known(base: &str) -> TypeSpec {
        ty(base, None, None, None)
    }

    fn custom(text: &str) -> TypeSpec {
        TypeSpec::Custom { text: text.into() }
    }

    fn value(text: &str) -> Option<DefaultSpec> {
        Some(DefaultSpec::Value { text: text.into() })
    }

    fn expr(text: &str) -> Option<DefaultSpec> {
        Some(DefaultSpec::Expression { text: text.into() })
    }

    /// A nullable column that exists on the server.
    fn col(name: &str, data_type: TypeSpec) -> ColumnDef {
        ColumnDef {
            original: Some(name.into()),
            name: name.into(),
            data_type,
            nullable: true,
            default: None,
            auto_increment: false,
            identity: None,
            on_update: None,
            charset: None,
            collation: None,
            comment: None,
            using: None,
            generated: None,
            locked: false,
        }
    }

    fn new_col(name: &str, data_type: TypeSpec) -> ColumnDef {
        ColumnDef {
            original: None,
            ..col(name, data_type)
        }
    }

    fn part(column: &str) -> IndexPartDef {
        IndexPartDef {
            column: Some(column.into()),
            expression: None,
            prefix: None,
            descending: false,
        }
    }

    fn index(name: &str, unique: bool, constraint: bool, cols: &[&str]) -> IndexDef {
        IndexDef {
            original: None,
            name: name.into(),
            unique,
            parts: cols.iter().map(|c| part(c)).collect(),
            method: None,
            include: vec![],
            visible: true,
            comment: None,
            constraint,
            locked: false,
        }
    }

    fn table(name: &str) -> TableRef {
        TableRef {
            kind: DbKind::Postgres,
            database: "dnt_fixture".into(),
            schema: Some("public".into()),
            name: name.into(),
        }
    }

    /// A PostgreSQL `orders` table as stored.
    fn orders() -> TableDef {
        TableDef {
            original_name: Some("orders".into()),
            name: "orders".into(),
            columns: vec![
                ColumnDef {
                    nullable: false,
                    auto_increment: true,
                    identity: Some(IdentityKind::Always),
                    ..col("id", known("bigint"))
                },
                ColumnDef {
                    nullable: false,
                    default: value("0"),
                    ..col("qty", known("integer"))
                },
                col("total", ty("numeric", None, Some(12), Some(2))),
                col("note", known("text")),
                col("customer_id", known("bigint")),
                col("code", ty("varchar", Some(40), None, None)),
            ],
            primary_key: Some(KeyDef {
                original: Some("orders_pkey".into()),
                name: Some("orders_pkey".into()),
                columns: vec!["id".into()],
                locked: false,
            }),
            indexes: vec![],
            foreign_keys: vec![FkDef {
                original: Some("fk_customer".into()),
                name: "fk_customer".into(),
                columns: vec!["customer_id".into()],
                ref_schema: None,
                ref_table: "customers".into(),
                ref_columns: vec!["id".into()],
                on_update: "NO ACTION".into(),
                on_delete: "NO ACTION".into(),
                match_type: None,
                deferrable: false,
                initially_deferred: false,
                locked: false,
            }],
            checks: vec![],
            options: TableOptionsDef::default(),
            comment: None,
            read_only_reason: None,
        }
    }

    fn render_ok(old: &TableDef, new: &TableDef) -> Rendered {
        let changes = diff(old, new).expect("diff");
        render(&table(&old.name), old, &changes, &pg(), PG).expect("render")
    }

    fn render_err(old: &TableDef, new: &TableDef) -> RenderError {
        let changes = diff(old, new).expect("diff");
        render(&table(&old.name), old, &changes, &pg(), PG).expect_err("render error")
    }

    fn kinds(impacts: &[Impact]) -> Vec<(ImpactKind, ElementRef)> {
        impacts
            .iter()
            .map(|i| (i.kind, i.element.clone()))
            .collect()
    }

    #[test]
    fn statement_order_is_fixed() {
        let old = orders();
        let mut new = old.clone();
        new.foreign_keys.clear();
        new.columns[3].name = "memo".into();
        new.columns[3].comment = Some("Ghi chú của khách".into());
        new.columns[1].data_type = known("bigint");
        new.columns
            .push(new_col("channel", ty("varchar", Some(20), None, None)));
        new.indexes
            .push(index("ix_channel", false, false, &["channel"]));
        let expected = vec![
            r#"ALTER TABLE "public"."orders" DROP CONSTRAINT "fk_customer""#,
            r#"ALTER TABLE "public"."orders" RENAME COLUMN "note" TO "memo""#,
            r#"ALTER TABLE "public"."orders" ALTER COLUMN "qty" TYPE bigint USING "qty"::bigint"#,
            r#"ALTER TABLE "public"."orders" ADD COLUMN "channel" varchar(20)"#,
            r#"CREATE INDEX "ix_channel" ON "public"."orders" USING btree ("channel")"#,
            r#"COMMENT ON COLUMN "public"."orders"."memo" IS 'Ghi chú của khách'"#,
        ];
        let r = render_ok(&old, &new);
        assert_eq!(r.statements, expected);
        assert_eq!(
            kinds(&r.impacts),
            vec![(ImpactKind::Rebuild, ElementRef::Column("qty".into()))]
        );
        // The order is the spec's, not the order of the changes given (ruling T6-3).
        let mut changes = diff(&old, &new).unwrap();
        changes.reverse();
        let r = render(&table("orders"), &old, &changes, &pg(), PG).unwrap();
        assert_eq!(r.statements, expected);
    }

    #[test]
    fn type_change_auto_using() {
        let old = orders();
        let mut new = old.clone();
        new.columns[1].data_type = known("bigint");
        new.columns[3].data_type = ty("varchar", Some(10), None, None);
        new.columns[2].data_type = known("integer");
        let r = render_ok(&old, &new);
        assert_eq!(
            r.statements,
            vec![
                r#"ALTER TABLE "public"."orders" ALTER COLUMN "qty" TYPE bigint USING "qty"::bigint"#,
                r#"ALTER TABLE "public"."orders" ALTER COLUMN "total" TYPE integer USING "total"::integer"#,
                r#"ALTER TABLE "public"."orders" ALTER COLUMN "note" TYPE varchar(10) USING "note"::varchar(10)"#,
            ]
        );
        assert_eq!(
            kinds(&r.impacts),
            vec![
                (ImpactKind::DataLoss, ElementRef::Column("total".into())),
                (ImpactKind::DataLoss, ElementRef::Column("note".into())),
                (ImpactKind::Rebuild, ElementRef::Column("qty".into())),
                (ImpactKind::Rebuild, ElementRef::Column("total".into())),
                (ImpactKind::Rebuild, ElementRef::Column("note".into())),
            ]
        );
        assert_eq!(r.impacts[0].count_sql, None);
        assert_eq!(
            r.impacts[1].count_sql.as_deref(),
            Some(r#"SELECT COUNT(*) FROM "public"."orders" WHERE char_length("note") > 10"#)
        );
        // Any type to text, text to the listed types, timestamp <-> timestamptz.
        for (from, to) in [
            (custom("integer[]"), known("text")),
            (known("text"), known("uuid")),
            (known("text"), known("jsonb")),
            (known("json"), known("jsonb")),
            (known("timestamp"), known("timestamptz")),
            (
                ty("numeric", None, Some(10), Some(2)),
                ty("numeric", None, Some(14), Some(2)),
            ),
        ] {
            let mut old = orders();
            old.columns[3].data_type = from;
            let mut new = old.clone();
            new.columns[3].data_type = to.clone();
            let r = render_ok(&old, &new);
            assert_eq!(
                r.statements,
                vec![format!(
                    r#"ALTER TABLE "public"."orders" ALTER COLUMN "note" TYPE {t} USING "note"::{t}"#,
                    t = crate::types::type_text(&to, &pg())
                )]
            );
        }
    }

    #[test]
    fn type_change_needs_using() {
        let old = orders();
        let mut new = old.clone();
        new.columns[3].data_type = custom("integer[]");
        assert_eq!(
            render_err(&old, &new),
            RenderError::Invalid {
                message: "Changing text to integer[] needs a USING expression".into()
            }
        );
        new.columns[3].using = Some("string_to_array(note, ',')::integer[]".into());
        assert_eq!(
            render_ok(&old, &new).statements,
            vec![
                r#"ALTER TABLE "public"."orders" ALTER COLUMN "note" TYPE integer[] USING string_to_array(note, ',')::integer[]"#
            ]
        );
        // Not on the list either: jsonb back to json, boolean to integer.
        for (from, to) in [
            (known("jsonb"), known("json")),
            (known("boolean"), known("integer")),
        ] {
            let mut old = orders();
            old.columns[3].data_type = from;
            let mut new = old.clone();
            new.columns[3].data_type = to;
            assert!(matches!(
                render_err(&old, &new),
                RenderError::Invalid { .. }
            ));
        }
    }

    #[test]
    fn move_column_unsupported() {
        let old = orders();
        let mut new = old.clone();
        new.columns.swap(1, 2);
        assert_eq!(
            render_err(&old, &new),
            RenderError::Unsupported {
                feature: "column order".into(),
                min_version: "not supported by PostgreSQL".into(),
            }
        );
    }

    #[test]
    fn unique_constraint_kept_as_constraint() {
        let mut old = orders();
        old.indexes = vec![
            IndexDef {
                original: Some("uq_code".into()),
                ..index("uq_code", true, true, &["code"])
            },
            IndexDef {
                original: Some("ux_note".into()),
                ..index("ux_note", true, false, &["note"])
            },
        ];
        let mut new = old.clone();
        new.indexes[0].parts.push(part("qty"));
        new.indexes[1].parts.push(part("qty"));
        assert_eq!(
            render_ok(&old, &new).statements,
            vec![
                r#"ALTER TABLE "public"."orders" DROP CONSTRAINT "uq_code""#,
                r#"DROP INDEX "public"."ux_note""#,
                r#"ALTER TABLE "public"."orders" ADD CONSTRAINT "uq_code" UNIQUE ("code", "qty")"#,
                r#"CREATE UNIQUE INDEX "ux_note" ON "public"."orders" USING btree ("note", "qty")"#,
            ]
        );
        // Dropped, renamed.
        let mut new = old.clone();
        new.indexes[0].name = "uq_code2".into();
        new.indexes.remove(1);
        assert_eq!(
            render_ok(&old, &new).statements,
            vec![
                r#"DROP INDEX "public"."ux_note""#,
                r#"ALTER INDEX "public"."uq_code" RENAME TO "uq_code2""#,
            ]
        );
        // A constraint takes no method, descending part or non-unique flag.
        let bad = IndexDef {
            method: Some("hash".into()),
            ..index("u", true, true, &["a"])
        };
        assert!(matches!(
            index_sql(&bad, &pg(), PG),
            Err(RenderError::Invalid { .. })
        ));
        let bad = index("u", false, true, &["a"]);
        assert!(matches!(
            index_sql(&bad, &pg(), PG),
            Err(RenderError::Invalid { .. })
        ));
    }

    #[test]
    fn quotes_hostile_identifiers() {
        let mut old = orders();
        old.original_name = Some("we\"ird".into());
        old.name = "we\"ird".into();
        let mut new = old.clone();
        new.columns.push(new_col("đơn_hàng", known("integer")));
        new.indexes
            .push(index("ix\"x", false, false, &["đơn_hàng"]));
        new.name = "Wé\"2".into();
        let t = TableRef {
            schema: Some("sch\"x".into()),
            ..table("we\"ird")
        };
        let changes = diff(&old, &new).unwrap();
        let r = render(&t, &old, &changes, &pg(), PG).unwrap();
        assert_eq!(
            r.statements,
            vec![
                r#"ALTER TABLE "sch""x"."we""ird" RENAME TO "Wé""2""#,
                r#"ALTER TABLE "sch""x"."Wé""2" ADD COLUMN "đơn_hàng" integer"#,
                r#"CREATE INDEX "ix""x" ON "sch""x"."Wé""2" USING btree ("đơn_hàng")"#,
            ]
        );
        // Literals double their quotes and never use E'…'.
        let mut new = old.clone();
        new.comment = Some("it's \\ fine".into());
        assert_eq!(
            render(&t, &old, &diff(&old, &new).unwrap(), &pg(), PG)
                .unwrap()
                .statements,
            vec![r#"COMMENT ON TABLE "sch""x"."we""ird" IS 'it''s \ fine'"#]
        );
    }

    #[test]
    fn create_table_full_pg() {
        let mut def = orders();
        def.original_name = None;
        for c in &mut def.columns {
            c.original = None;
        }
        def.columns[1].comment = Some("Số lượng".into());
        def.columns[4].nullable = false;
        def.columns[5].collation = Some("C".into());
        def.columns.push(ColumnDef {
            nullable: false,
            default: expr("now()"),
            ..new_col("created_at", ty("timestamptz", None, Some(3), None))
        });
        def.primary_key.as_mut().unwrap().original = None;
        def.foreign_keys[0].original = None;
        def.foreign_keys[0].ref_schema = Some("crm".into());
        def.foreign_keys[0].on_delete = "cascade".into();
        def.foreign_keys[0].match_type = Some("FULL".into());
        def.foreign_keys[0].deferrable = true;
        def.foreign_keys[0].initially_deferred = true;
        def.indexes = vec![
            index("uq_code", true, true, &["code"]),
            IndexDef {
                include: vec!["total".into()],
                comment: Some("lookup".into()),
                parts: vec![
                    part("customer_id"),
                    IndexPartDef {
                        descending: true,
                        ..part("created_at")
                    },
                ],
                ..index("ix_customer", false, false, &[])
            },
        ];
        def.checks.push(CheckDef {
            original: None,
            name: "ck_qty".into(),
            expression: "qty >= 0".into(),
            locked: false,
        });
        def.comment = Some("Đơn hàng".into());
        let r = render_create(&table("orders"), &def, &pg(), PG).unwrap();
        assert_eq!(
            r.statements,
            vec![
                "CREATE TABLE \"public\".\"orders\" (\n\
                 \x20 \"id\" bigint GENERATED ALWAYS AS IDENTITY NOT NULL,\n\
                 \x20 \"qty\" integer NOT NULL DEFAULT 0,\n\
                 \x20 \"total\" numeric(12,2),\n\
                 \x20 \"note\" text,\n\
                 \x20 \"customer_id\" bigint NOT NULL,\n\
                 \x20 \"code\" varchar(40) COLLATE \"C\",\n\
                 \x20 \"created_at\" timestamptz(3) NOT NULL DEFAULT now(),\n\
                 \x20 CONSTRAINT \"orders_pkey\" PRIMARY KEY (\"id\"),\n\
                 \x20 CONSTRAINT \"uq_code\" UNIQUE (\"code\"),\n\
                 \x20 CONSTRAINT \"fk_customer\" FOREIGN KEY (\"customer_id\") REFERENCES \
                 \"crm\".\"customers\"(\"id\") MATCH FULL ON DELETE CASCADE DEFERRABLE \
                 INITIALLY DEFERRED,\n\
                 \x20 CONSTRAINT \"ck_qty\" CHECK (qty >= 0)\n\
                 )",
                "CREATE INDEX \"ix_customer\" ON \"public\".\"orders\" USING btree \
                 (\"customer_id\", \"created_at\" DESC) INCLUDE (\"total\")",
                "COMMENT ON TABLE \"public\".\"orders\" IS 'Đơn hàng'",
                "COMMENT ON COLUMN \"public\".\"orders\".\"qty\" IS 'Số lượng'",
                "COMMENT ON INDEX \"public\".\"ix_customer\" IS 'lookup'",
            ]
        );
        assert!(r.impacts.is_empty());
        // A same-schema reference is qualified with the table's schema.
        let mut def2 = def.clone();
        def2.foreign_keys[0].ref_schema = None;
        let r = render_create(&table("orders"), &def2, &pg(), PG).unwrap();
        assert!(r.statements[0].contains("REFERENCES \"public\".\"customers\"(\"id\")"));
        // The fragment itself stays unqualified, like pg_get_constraintdef.
        assert_eq!(
            fk_sql(&def2.foreign_keys[0], &pg(), PG).unwrap(),
            "CONSTRAINT \"fk_customer\" FOREIGN KEY (\"customer_id\") REFERENCES \
             \"customers\"(\"id\") MATCH FULL ON DELETE CASCADE DEFERRABLE INITIALLY DEFERRED"
        );
    }

    #[test]
    fn element_fragments() {
        let s = pg();
        let i = IndexDef {
            method: Some("GIN".into()),
            include: vec!["b".into()],
            ..index("ix", false, false, &["a"])
        };
        assert_eq!(
            index_sql(&i, &s, PG).unwrap(),
            r#"USING gin ("a") INCLUDE ("b")"#
        );
        let u = IndexDef {
            include: vec!["b".into()],
            ..index("uq", true, true, &["a", "c"])
        };
        assert_eq!(
            index_sql(&u, &s, PG).unwrap(),
            r#"CONSTRAINT "uq" UNIQUE ("a", "c") INCLUDE ("b")"#
        );
        let e = IndexDef {
            parts: vec![IndexPartDef {
                column: None,
                expression: Some("lower(note)".into()),
                prefix: None,
                descending: false,
            }],
            ..index("ix", false, false, &[])
        };
        assert_eq!(
            index_sql(&e, &s, PG).unwrap(),
            "USING btree ((lower(note)))"
        );
        // MySQL-only index attributes.
        let prefix = IndexDef {
            parts: vec![IndexPartDef {
                prefix: Some(5),
                ..part("a")
            }],
            ..index("ix", false, false, &[])
        };
        assert!(matches!(
            index_sql(&prefix, &s, PG),
            Err(RenderError::Invalid { .. })
        ));
        let invisible = IndexDef {
            visible: false,
            ..index("ix", false, false, &["a"])
        };
        assert!(matches!(
            index_sql(&invisible, &s, PG),
            Err(RenderError::Invalid { .. })
        ));
        let pk = KeyDef {
            original: None,
            name: None,
            columns: vec!["a".into(), "b".into()],
            locked: false,
        };
        assert_eq!(
            primary_key_sql(&pk, &s, PG).unwrap(),
            r#"PRIMARY KEY ("a", "b")"#
        );
        let fk = FkDef {
            original: None,
            name: "fk".into(),
            columns: vec!["a".into()],
            ref_schema: None,
            ref_table: "u".into(),
            ref_columns: vec!["x".into()],
            on_update: "restrict".into(),
            on_delete: "SET NULL".into(),
            match_type: None,
            deferrable: false,
            initially_deferred: true,
            locked: false,
        };
        assert!(matches!(
            fk_sql(&fk, &s, PG),
            Err(RenderError::Invalid { .. })
        ));
        let fk = FkDef {
            initially_deferred: false,
            ..fk
        };
        assert_eq!(
            fk_sql(&fk, &s, PG).unwrap(),
            r#"CONSTRAINT "fk" FOREIGN KEY ("a") REFERENCES "u"("x") ON UPDATE RESTRICT ON DELETE SET NULL"#
        );
        let ck = CheckDef {
            original: None,
            name: "ck".into(),
            expression: " a > 0 ".into(),
            locked: false,
        };
        assert_eq!(
            check_sql(&ck, &s, PG).unwrap(),
            r#"CONSTRAINT "ck" CHECK (a > 0)"#
        );
        // A MySQL dialect never renders PostgreSQL SQL.
        assert!(matches!(
            check_sql(&ck, &s, Dialect::MYSQL),
            Err(RenderError::Invalid { .. })
        ));
    }

    #[test]
    fn column_fragments_and_defaults() {
        let s = pg();
        let c = |data_type, default| ColumnDef {
            default,
            ..col("c", data_type)
        };
        assert_eq!(
            column_sql(&c(known("text"), value("it's")), &s, PG).unwrap(),
            r#""c" text DEFAULT 'it''s'"#
        );
        assert_eq!(
            column_sql(
                &c(ty("numeric", None, Some(5), Some(2)), value("-1.5")),
                &s,
                PG
            )
            .unwrap(),
            r#""c" numeric(5,2) DEFAULT -1.5"#
        );
        assert_eq!(
            column_sql(&c(known("boolean"), value("TRUE")), &s, PG).unwrap(),
            r#""c" boolean DEFAULT true"#
        );
        assert_eq!(
            column_sql(&c(known("date"), value("2024-01-01")), &s, PG).unwrap(),
            r#""c" date DEFAULT '2024-01-01'"#
        );
        assert_eq!(
            column_sql(&c(known("uuid"), expr("gen_random_uuid()")), &s, PG).unwrap(),
            r#""c" uuid DEFAULT gen_random_uuid()"#
        );
        let identity = ColumnDef {
            identity: Some(IdentityKind::ByDefault),
            auto_increment: true,
            nullable: false,
            ..col("id", known("bigint"))
        };
        assert_eq!(
            column_sql(&identity, &s, PG).unwrap(),
            r#""id" bigint GENERATED BY DEFAULT AS IDENTITY NOT NULL"#
        );
        // MySQL-only attributes and impossible combinations.
        for bad in [
            ColumnDef {
                charset: Some("utf8mb4".into()),
                ..col("c", known("text"))
            },
            ColumnDef {
                on_update: Some("CURRENT_TIMESTAMP".into()),
                ..col("c", known("timestamp"))
            },
            ColumnDef {
                default: value("1"),
                ..identity.clone()
            },
            col(
                "c",
                TypeSpec::Known {
                    base: "integer".into(),
                    length: None,
                    precision: None,
                    scale: None,
                    unsigned: true,
                    values: vec![],
                },
            ),
            ColumnDef {
                generated: Some("a + 1".into()),
                ..col("c", known("integer"))
            },
            c(known("text"), value("a\0b")),
        ] {
            assert!(
                matches!(column_sql(&bad, &s, PG), Err(RenderError::Invalid { .. })),
                "{bad:?}"
            );
        }
    }

    #[test]
    fn column_alterations_and_impacts() {
        let old = orders();
        let mut new = old.clone();
        // qty: drop default, nullable stays; total: NOT NULL + default; note: dropped;
        // id: identity BY DEFAULT; code: shorter, renamed; customer_id: comment cleared.
        new.columns[1].default = None;
        new.columns[2].nullable = false;
        new.columns[2].default = value("0");
        new.columns[0].identity = Some(IdentityKind::ByDefault);
        new.columns[5].data_type = ty("varchar", Some(8), None, None);
        new.columns[5].name = "sku".into();
        new.columns.remove(3);
        let r = render_ok(&old, &new);
        assert_eq!(
            r.statements,
            vec![
                r#"ALTER TABLE "public"."orders" DROP COLUMN "note""#,
                r#"ALTER TABLE "public"."orders" RENAME COLUMN "code" TO "sku""#,
                r#"ALTER TABLE "public"."orders" ALTER COLUMN "id" SET GENERATED BY DEFAULT"#,
                r#"ALTER TABLE "public"."orders" ALTER COLUMN "qty" DROP DEFAULT"#,
                r#"ALTER TABLE "public"."orders" ALTER COLUMN "total" SET NOT NULL"#,
                r#"ALTER TABLE "public"."orders" ALTER COLUMN "total" SET DEFAULT 0"#,
                r#"ALTER TABLE "public"."orders" ALTER COLUMN "sku" TYPE varchar(8) USING "sku"::varchar(8)"#,
            ]
        );
        assert_eq!(
            kinds(&r.impacts),
            vec![
                (ImpactKind::DataLoss, ElementRef::Column("note".into())),
                (ImpactKind::DataLoss, ElementRef::Column("total".into())),
                (ImpactKind::DataLoss, ElementRef::Column("sku".into())),
                (ImpactKind::Rebuild, ElementRef::Column("sku".into())),
            ]
        );
        // Counts run before the change: old table, old column names.
        let counts: Vec<_> = r.impacts.iter().map(|i| i.count_sql.as_deref()).collect();
        assert_eq!(
            counts,
            vec![
                Some(r#"SELECT COUNT(*) FROM "public"."orders" WHERE "note" IS NOT NULL"#),
                Some(r#"SELECT COUNT(*) FROM "public"."orders" WHERE "total" IS NULL"#),
                Some(r#"SELECT COUNT(*) FROM "public"."orders" WHERE char_length("code") > 8"#),
                None,
            ]
        );

        // Identity on and off, NOT NULL dropped, type change with a new default, a
        // collation; a renamed table is named by its new name afterwards.
        let mut new = old.clone();
        new.name = "orders2".into();
        new.columns[0].identity = None;
        new.columns[0].nullable = true;
        new.columns[4].identity = Some(IdentityKind::Always);
        new.columns[4].nullable = false;
        new.columns[1].data_type = known("bigint");
        new.columns[1].default = value("1");
        new.columns[5].collation = Some("C".into());
        let r = render_ok(&old, &new);
        assert_eq!(
            r.statements,
            vec![
                r#"ALTER TABLE "public"."orders" RENAME TO "orders2""#,
                r#"ALTER TABLE "public"."orders2" ALTER COLUMN "id" DROP IDENTITY"#,
                r#"ALTER TABLE "public"."orders2" ALTER COLUMN "id" DROP NOT NULL"#,
                r#"ALTER TABLE "public"."orders2" ALTER COLUMN "qty" DROP DEFAULT"#,
                r#"ALTER TABLE "public"."orders2" ALTER COLUMN "qty" TYPE bigint USING "qty"::bigint"#,
                r#"ALTER TABLE "public"."orders2" ALTER COLUMN "qty" SET DEFAULT 1"#,
                r#"ALTER TABLE "public"."orders2" ALTER COLUMN "customer_id" SET NOT NULL"#,
                r#"ALTER TABLE "public"."orders2" ALTER COLUMN "customer_id" ADD GENERATED ALWAYS AS IDENTITY"#,
                r#"ALTER TABLE "public"."orders2" ALTER COLUMN "code" TYPE varchar(40) COLLATE "C""#,
            ]
        );
        assert_eq!(
            kinds(&r.impacts),
            vec![
                (
                    ImpactKind::DataLoss,
                    ElementRef::Column("customer_id".into())
                ),
                (ImpactKind::Rebuild, ElementRef::Column("qty".into())),
                (ImpactKind::Rebuild, ElementRef::Column("code".into())),
            ]
        );
        assert_eq!(
            r.impacts[0].count_sql.as_deref(),
            Some(r#"SELECT COUNT(*) FROM "public"."orders" WHERE "customer_id" IS NULL"#)
        );
    }

    #[test]
    fn adds_drops_and_comments() {
        let mut old = orders();
        old.checks.push(CheckDef {
            original: Some("ck".into()),
            name: "ck".into(),
            expression: "qty > 0".into(),
            locked: false,
        });
        old.comment = Some("x".into());
        old.columns[3].comment = Some("old".into());
        let mut new = old.clone();
        new.checks.clear();
        new.primary_key = Some(KeyDef {
            original: Some("orders_pkey".into()),
            name: Some("orders_pkey".into()),
            columns: vec!["id".into(), "qty".into()],
            locked: false,
        });
        new.foreign_keys[0].on_delete = "CASCADE".into();
        new.comment = None;
        new.columns[3].comment = Some(String::new());
        new.columns.push(ColumnDef {
            comment: Some("new".into()),
            default: value("x"),
            nullable: false,
            ..new_col("a", known("text"))
        });
        new.columns.push(ColumnDef {
            identity: Some(IdentityKind::Always),
            nullable: false,
            ..new_col("b", known("bigint"))
        });
        new.columns.push(ColumnDef {
            default: expr("random()"),
            ..new_col("c", known("double precision"))
        });
        new.checks.push(CheckDef {
            original: None,
            name: "ck2".into(),
            expression: "qty >= 0".into(),
            locked: false,
        });
        let r = render_ok(&old, &new);
        assert_eq!(
            r.statements,
            vec![
                r#"ALTER TABLE "public"."orders" DROP CONSTRAINT "fk_customer""#,
                r#"ALTER TABLE "public"."orders" DROP CONSTRAINT "ck""#,
                r#"ALTER TABLE "public"."orders" DROP CONSTRAINT "orders_pkey""#,
                r#"ALTER TABLE "public"."orders" ADD COLUMN "a" text NOT NULL DEFAULT 'x'"#,
                r#"ALTER TABLE "public"."orders" ADD COLUMN "b" bigint GENERATED ALWAYS AS IDENTITY NOT NULL"#,
                r#"ALTER TABLE "public"."orders" ADD COLUMN "c" double precision DEFAULT random()"#,
                r#"ALTER TABLE "public"."orders" ADD CONSTRAINT "orders_pkey" PRIMARY KEY ("id", "qty")"#,
                r#"ALTER TABLE "public"."orders" ADD CONSTRAINT "ck2" CHECK (qty >= 0)"#,
                r#"ALTER TABLE "public"."orders" ADD CONSTRAINT "fk_customer" FOREIGN KEY ("customer_id") REFERENCES "public"."customers"("id") ON DELETE CASCADE"#,
                r#"COMMENT ON TABLE "public"."orders" IS NULL"#,
                r#"COMMENT ON COLUMN "public"."orders"."note" IS NULL"#,
                r#"COMMENT ON COLUMN "public"."orders"."a" IS 'new'"#,
            ]
        );
        // An added column rewrites the table only with an identity or a computed default.
        assert_eq!(
            kinds(&r.impacts),
            vec![
                (ImpactKind::Rebuild, ElementRef::Column("b".into())),
                (ImpactKind::Rebuild, ElementRef::Column("c".into())),
            ]
        );
        // MySQL table options are refused; no change, no statement.
        let mut new = old.clone();
        new.options.engine = Some("InnoDB".into());
        assert!(matches!(
            render_err(&old, &new),
            RenderError::Invalid { .. }
        ));
        let r = render_ok(&old, &old.clone());
        assert!(r.statements.is_empty() && r.impacts.is_empty());
    }
}
