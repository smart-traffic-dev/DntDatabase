//! MySQL / MariaDB rendering: every change in ONE `ALTER TABLE`, a new table in ONE
//! `CREATE TABLE`.
//!
//! Element fragments (`column_sql`, `index_sql`, …) are spelled like `SHOW CREATE
//! TABLE` lines where the syntax allows (`,` without a space in column lists, keywords
//! upper case, `RESTRICT` actions left out) so Task 9 can compare them with the server.
//!
//! Inside one `ALTER TABLE`, a clause names a column by its ORIGINAL name, while
//! `AFTER x`, key and index column lists use the NEW names (checked on MySQL 5.7, 8.0
//! and MariaDB 10.6: an `AFTER` naming the old name of a column renamed in the same
//! statement fails with "Unknown column"). A column's rename, alteration and move are
//! one `CHANGE COLUMN` / `MODIFY COLUMN` clause (ruling T6-1).

use super::{fk_action, invalid, is_number, unsupported, word, Impact, ImpactKind, Impacts};
use super::{RenderError, Rendered};
use crate::diff::{Change, Position};
use crate::model::*;
use crate::types::{type_info, type_text};
use dnt_core::server::{ServerInfo, ServerVariant, SqlKind};
use dnt_core::sql::build::{mysql_quote, BuildError, TableRef};
use dnt_core::sql::ident::{count_where_sql, qualified_name, quote_ident};
use dnt_core::sql::Dialect;

fn q(name: &str) -> String {
    quote_ident(SqlKind::MySql, name)
}

fn check_dialect(d: Dialect) -> Result<(), RenderError> {
    if d.is_mysql() {
        Ok(())
    } else {
        Err(invalid(
            "The SQL dialect does not belong to a MySQL or MariaDB server.",
        ))
    }
}

/// A plain string literal for the session's `sql_mode` (core escaper).
fn string(s: &str, d: Dialect) -> Result<String, RenderError> {
    mysql_quote(s, d).map_err(|e| match e {
        BuildError::Invalid { reason, .. } => invalid(format!("Cannot write this text: {reason}.")),
        other => invalid(other.to_string()),
    })
}

fn names(cols: &[String]) -> String {
    cols.iter().map(|c| q(c)).collect::<Vec<_>>().join(",")
}

fn mysql8_at_least(server: &ServerInfo, v: (u32, u32, u32)) -> bool {
    server.variant == ServerVariant::MySql8 && server.version >= v
}

fn check_supported(server: &ServerInfo) -> Result<(), RenderError> {
    if server.variant == ServerVariant::MariaDb || mysql8_at_least(server, (8, 0, 16)) {
        Ok(())
    } else {
        Err(unsupported("CHECK constraint", "MySQL 8.0.16"))
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
    if changes.is_empty() {
        return Ok(Rendered {
            statements: vec![],
            impacts: vec![],
        });
    }
    let relation = qualified_name(SqlKind::MySql, &table.database, None, &table.name);
    for c in changes {
        if let Change::AddForeignKey { fk } = c {
            let dropped = changes.iter().any(
                |c| matches!(c, Change::DropForeignKey { name } if name.eq_ignore_ascii_case(&fk.name)),
            );
            if dropped {
                return Err(invalid(format!(
                    "MySQL cannot drop and re-create foreign key \"{}\" in one statement. \
                     Give the changed foreign key a new name, or apply the change in two steps.",
                    fk.name
                )));
            }
        }
    }

    // Under T5-2 a column with no charset / collation follows the table's: when the
    // table default changes, a re-written column keeps the old one explicitly.
    let keep_charset = changes.iter().find_map(|c| match c {
        Change::SetOptions { old: o, new: n }
            if o.charset != n.charset || o.collation != n.collation =>
        {
            Some((o.charset.clone(), o.collation.clone()))
        }
        _ => None,
    });
    let mut alter = Alter {
        old,
        changes,
        server,
        d,
        relation: &relation,
        keep_charset,
        done: Vec::new(),
        impacts: Impacts::default(),
    };
    let mut clauses = Vec::new();
    for c in changes {
        match c {
            Change::CreateTable => {
                return Err(invalid("A new table is rendered with render_create."))
            }
            Change::RenameTable { to, .. } => clauses.push(format!(
                "RENAME TO {}",
                qualified_name(SqlKind::MySql, &table.database, None, to)
            )),
            Change::SetOptions { old: o, new: n } => {
                let options = changed_options(o, n)?;
                if options.is_empty() {
                    return Err(invalid(
                        "The engine, character set and collation of a table cannot be \
                         cleared; pick a value instead.",
                    ));
                }
                clauses.extend(options);
            }
            Change::SetComment { comment } => clauses.push(format!(
                "COMMENT={}",
                string(comment.as_deref().unwrap_or(""), d)?
            )),
            Change::DropForeignKey { name } => {
                clauses.push(format!("DROP FOREIGN KEY {}", q(name)))
            }
            Change::DropCheck { name } => {
                let kw = match server.variant {
                    ServerVariant::MariaDb => "DROP CONSTRAINT",
                    _ => {
                        check_supported(server)?;
                        "DROP CHECK"
                    }
                };
                clauses.push(format!("{kw} {}", q(name)));
            }
            Change::DropIndex { name } => clauses.push(format!("DROP INDEX {}", q(name))),
            Change::DropPrimaryKey { .. } => clauses.push("DROP PRIMARY KEY".into()),
            // A column that also moves gets its one clause at the move, so moves keep
            // the order diff gave them (a move may name a column moved before it).
            Change::RenameColumn { from, .. } if !alter.moves(from) => {
                clauses.extend(alter.column(from)?)
            }
            Change::AlterColumn { old: oc, .. } if !alter.moves(&oc.name) => {
                clauses.extend(alter.column(&oc.name)?)
            }
            Change::RenameColumn { .. } | Change::AlterColumn { .. } => {}
            Change::MoveColumn { name, .. } => {
                let original = alter.original_of(name);
                clauses.extend(alter.column(&original)?);
            }
            Change::DropColumn { name } => {
                clauses.push(format!("DROP COLUMN {}", q(name)));
                alter.impacts.data_loss.push(Impact {
                    kind: ImpactKind::DataLoss,
                    element: ElementRef::Column(name.clone()),
                    message: format!("Dropping column \"{name}\" deletes its values."),
                    count_sql: Some(count_where_sql(
                        &relation,
                        &format!(" WHERE {} IS NOT NULL", q(name)),
                    )),
                });
            }
            Change::AddColumn { column, position } => clauses.push(format!(
                "ADD COLUMN {}{}",
                column_sql(column, server, d)?,
                position_sql(position)
            )),
            Change::AddPrimaryKey { key } => {
                clauses.push(format!("ADD {}", primary_key_sql(key, server, d)?))
            }
            Change::AddIndex { index } => {
                clauses.push(format!("ADD {}", index_sql(index, server, d)?))
            }
            Change::RenameIndex { from, to } => {
                if server.variant == ServerVariant::MariaDb && server.version < (10, 5, 2) {
                    return Err(unsupported("index rename", "MariaDB 10.5.2"));
                }
                clauses.push(format!("RENAME INDEX {} TO {}", q(from), q(to)));
            }
            Change::AddCheck { check } => {
                clauses.push(format!("ADD {}", check_sql(check, server, d)?))
            }
            Change::AddForeignKey { fk } => clauses.push(format!("ADD {}", fk_sql(fk, server, d)?)),
        }
    }

    let engine = changes
        .iter()
        .find_map(|c| match c {
            Change::SetOptions { new, .. } => Some(new.engine.clone()),
            _ => None,
        })
        .unwrap_or_else(|| old.options.engine.clone());
    let mut impacts = alter.impacts;
    if let Some(e) = engine.filter(|e| !e.eq_ignore_ascii_case("InnoDB")) {
        impacts.not_transactional.push(Impact {
            kind: ImpactKind::NotTransactional,
            element: ElementRef::Options,
            message: format!(
                "{e} tables are not transactional: a change that fails part-way cannot be \
                 rolled back."
            ),
            count_sql: None,
        });
    }
    let statements = if clauses.is_empty() {
        vec![]
    } else {
        vec![format!("ALTER TABLE {relation} {}", clauses.join(", "))]
    };
    Ok(Rendered {
        statements,
        impacts: impacts.finish(),
    })
}

/// The state of one `ALTER TABLE` while its clauses are built.
struct Alter<'a> {
    old: &'a TableDef,
    changes: &'a [Change],
    server: &'a ServerInfo,
    d: Dialect,
    relation: &'a str,
    /// The table's old `(charset, collation)` when this statement changes them.
    keep_charset: Option<(Option<String>, Option<String>)>,
    /// Original names of the columns whose clause is already out.
    done: Vec<String>,
    impacts: Impacts,
}

impl Alter<'_> {
    /// The original name of the column the draft calls `name`.
    fn original_of(&self, name: &str) -> String {
        self.changes
            .iter()
            .find_map(|c| match c {
                Change::RenameColumn { from, to } if to == name => Some(from.clone()),
                _ => None,
            })
            .unwrap_or_else(|| name.to_string())
    }

    /// Whether column `original` has a `MoveColumn`.
    fn moves(&self, original: &str) -> bool {
        self.changes.iter().any(
            |c| matches!(c, Change::MoveColumn { name, .. } if self.original_of(name) == original),
        )
    }

    /// The one clause for the rename / alteration / move of column `original`
    /// (`None` when it was already emitted).
    fn column(&mut self, original: &str) -> Result<Option<String>, RenderError> {
        if self.done.iter().any(|n| n == original) {
            return Ok(None);
        }
        self.done.push(original.to_string());
        let mut renamed = None;
        let mut altered = None;
        let mut moved = None;
        for c in self.changes {
            match c {
                Change::RenameColumn { from, to } if from == original => renamed = Some(to),
                Change::AlterColumn { old, new, .. } if old.name == original => {
                    altered = Some((old, new))
                }
                Change::MoveColumn { name, position } if self.original_of(name) == original => {
                    moved = Some(position)
                }
                _ => {}
            }
        }
        let new_name = renamed.map_or(original, |n| n.as_str()).to_string();
        let mut def = match altered {
            Some((_, new)) => new.clone(),
            None => {
                let stored = self
                    .old
                    .columns
                    .iter()
                    .find(|c| c.name == original)
                    .ok_or_else(|| {
                        invalid(format!("Column \"{original}\" is not in the table."))
                    })?;
                ColumnDef {
                    name: new_name.clone(),
                    ..stored.clone()
                }
            }
        };
        if let Some((charset, collation)) = &self.keep_charset {
            let has_collation = matches!(&def.data_type, TypeSpec::Known { base, .. }
                if type_info(base, self.server).is_some_and(|t| t.collation));
            if has_collation && def.charset.is_none() {
                def.charset = charset.clone();
                if def.collation.is_none() {
                    def.collation = collation.clone();
                }
            }
        }
        let body = column_sql(&def, self.server, self.d)?;
        let position = moved.map_or(String::new(), position_sql);
        let clause = match renamed {
            Some(_) => format!("CHANGE COLUMN {} {body}{position}", q(original)),
            None => format!("MODIFY COLUMN {body}{position}"),
        };
        if let Some((old, new)) = altered {
            self.alter_impacts(old, new, original, &new_name);
        }
        if moved.is_some() {
            self.impacts.rebuild(
                &new_name,
                format!("Moving column \"{new_name}\" rebuilds the table."),
            );
        }
        Ok(Some(clause))
    }

    fn alter_impacts(&mut self, old: &ColumnDef, new: &ColumnDef, original: &str, name: &str) {
        let col = q(original);
        if old.nullable && !new.nullable {
            self.impacts.data_loss.push(Impact {
                kind: ImpactKind::DataLoss,
                element: ElementRef::Column(name.into()),
                message: format!(
                    "Column \"{name}\" becomes NOT NULL: rows holding NULL make the change \
                     fail or get a default value."
                ),
                count_sql: Some(count_where_sql(
                    self.relation,
                    &format!(" WHERE {col} IS NULL"),
                )),
            });
        }
        let type_changed =
            type_text(&old.data_type, self.server) != type_text(&new.data_type, self.server);
        if type_changed {
            match lossy(&old.data_type, &new.data_type) {
                Some(Some(n)) => self.impacts.data_loss.push(Impact {
                    kind: ImpactKind::DataLoss,
                    element: ElementRef::Column(name.into()),
                    message: format!(
                        "Values of \"{name}\" longer than {n} characters may be truncated \
                         or make the change fail."
                    ),
                    count_sql: Some(count_where_sql(
                        self.relation,
                        &format!(" WHERE CHAR_LENGTH({col}) > {n}"),
                    )),
                }),
                Some(None) => self.impacts.data_loss.push(Impact {
                    kind: ImpactKind::DataLoss,
                    element: ElementRef::Column(name.into()),
                    message: format!("Changing the type of \"{name}\" may truncate values."),
                    count_sql: None,
                }),
                None => {}
            }
        }
        let charset_changed = old.charset != new.charset || old.collation != new.collation;
        if type_changed || charset_changed {
            self.impacts.rebuild(
                name,
                format!("Changing the type of \"{name}\" rebuilds the table."),
            );
        }
    }
}

fn position_sql(p: &Position) -> String {
    match p {
        Position::First => " FIRST".into(),
        Position::After(c) => format!(" AFTER {}", q(c)),
        Position::Last => String::new(),
    }
}

/// The option clauses for the fields that change; a cleared row format goes back to
/// `DEFAULT`, other cleared fields have no "unset" and are left as they are.
fn changed_options(
    old: &TableOptionsDef,
    new: &TableOptionsDef,
) -> Result<Vec<String>, RenderError> {
    let mut out = Vec::new();
    let fields = [
        ("ENGINE", "engine", &old.engine, &new.engine),
        (
            "DEFAULT CHARSET",
            "character set",
            &old.charset,
            &new.charset,
        ),
        ("COLLATE", "collation", &old.collation, &new.collation),
        ("ROW_FORMAT", "row format", &old.row_format, &new.row_format),
    ];
    for (key, what, o, n) in fields {
        if o == n {
            continue;
        }
        match n {
            Some(v) => out.push(format!("{key}={}", word(what, v)?)),
            None if key == "ROW_FORMAT" => out.push("ROW_FORMAT=DEFAULT".into()),
            None => {}
        }
    }
    Ok(out)
}

/// The table options of `CREATE TABLE`, as `SHOW CREATE TABLE` orders them.
fn create_options(o: &TableOptionsDef) -> Result<Vec<String>, RenderError> {
    let mut out = Vec::new();
    let fields = [
        ("ENGINE", "engine", &o.engine),
        ("DEFAULT CHARSET", "character set", &o.charset),
        ("COLLATE", "collation", &o.collation),
        ("ROW_FORMAT", "row format", &o.row_format),
    ];
    for (key, what, v) in fields {
        if let Some(v) = v {
            out.push(format!("{key}={}", word(what, v)?));
        }
    }
    Ok(out)
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
    let mut items = Vec::new();
    for c in &def.columns {
        items.push(column_sql(c, server, d)?);
    }
    if let Some(k) = &def.primary_key {
        items.push(primary_key_sql(k, server, d)?);
    }
    for i in &def.indexes {
        items.push(index_sql(i, server, d)?);
    }
    for f in &def.foreign_keys {
        items.push(fk_sql(f, server, d)?);
    }
    for c in &def.checks {
        items.push(check_sql(c, server, d)?);
    }
    let mut options = create_options(&def.options)?;
    if let Some(c) = def.comment.as_deref().filter(|c| !c.is_empty()) {
        options.push(format!("COMMENT={}", string(c, d)?));
    }
    let mut sql = format!(
        "CREATE TABLE {} (\n  {}\n)",
        qualified_name(SqlKind::MySql, &table.database, None, &def.name),
        items.join(",\n  ")
    );
    if !options.is_empty() {
        sql.push(' ');
        sql.push_str(&options.join(" "));
    }
    Ok(Rendered {
        statements: vec![sql],
        impacts: vec![],
    })
}

/// The column type: the catalog spelling, with enum / set members written through the
/// core escaper for the session's `sql_mode`; a custom type as typed.
fn type_sql(t: &TypeSpec, server: &ServerInfo, d: Dialect) -> Result<String, RenderError> {
    match t {
        TypeSpec::Custom { text } if text.trim().is_empty() => Err(invalid("A type is required.")),
        TypeSpec::Custom { text } => Ok(text.clone()),
        TypeSpec::Known { base, values, .. } => {
            let plain = !base.is_empty()
                && base
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b' ');
            if !plain {
                return Err(invalid(format!("Invalid type \"{base}\".")));
            }
            if values.is_empty() {
                return Ok(type_text(t, server));
            }
            let list = values
                .iter()
                .map(|v| string(v, d))
                .collect::<Result<Vec<_>, _>>()?;
            Ok(format!("{base}({})", list.join(",")))
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
    if c.identity.is_some() {
        return Err(invalid(
            "Identity columns are PostgreSQL only; use AUTO_INCREMENT.",
        ));
    }
    if c.using.is_some() {
        return Err(invalid("USING is PostgreSQL only."));
    }
    let mut out = format!("{} {}", q(&c.name), type_sql(&c.data_type, server, d)?);
    if let Some(cs) = &c.charset {
        out.push_str(&format!(" CHARACTER SET {}", word("character set", cs)?));
    }
    if let Some(co) = &c.collation {
        out.push_str(&format!(" COLLATE {}", word("collation", co)?));
    }
    out.push_str(if c.nullable { " NULL" } else { " NOT NULL" });
    if let Some(def) = &c.default {
        out.push_str(&format!(" DEFAULT {}", default_sql(c, def, server, d)?));
    }
    if let Some(on) = &c.on_update {
        if !is_now(on.trim()) {
            return Err(invalid("ON UPDATE can only be CURRENT_TIMESTAMP."));
        }
        out.push_str(&format!(" ON UPDATE {}", on.trim()));
    }
    if c.auto_increment {
        out.push_str(" AUTO_INCREMENT");
    }
    if let Some(cm) = c.comment.as_deref().filter(|s| !s.is_empty()) {
        out.push_str(&format!(" COMMENT {}", string(cm, d)?));
    }
    Ok(out)
}

const NUMERIC: &[&str] = &[
    "tinyint",
    "smallint",
    "mediumint",
    "int",
    "bigint",
    "decimal",
    "float",
    "double",
];

/// A value default is a quoted literal (as `SHOW CREATE TABLE` spells it, numbers
/// included, after a number check on numeric columns). An expression default stays
/// bare when it is `CURRENT_TIMESTAMP[(n)]`, `NULL` or a literal, and is wrapped in
/// parentheses where the server takes expression defaults (MySQL 8.0.13+, MariaDB).
fn default_sql(
    c: &ColumnDef,
    def: &DefaultSpec,
    server: &ServerInfo,
    d: Dialect,
) -> Result<String, RenderError> {
    match def {
        DefaultSpec::Value { text } => {
            if server.variant != ServerVariant::MariaDb && no_literal_default(&c.data_type) {
                return Err(unsupported(
                    "a value default on a TEXT, BLOB, JSON or spatial column",
                    "an expression default (MySQL 8.0.13)",
                ));
            }
            let numeric = matches!(&c.data_type, TypeSpec::Known { base, .. } if NUMERIC.contains(&base.as_str()));
            if numeric && !is_number(text) {
                return Err(invalid(format!(
                    "The default of \"{}\" must be a number.",
                    c.name
                )));
            }
            string(text, d)
        }
        DefaultSpec::Expression { text } => {
            let t = text.trim();
            if t.is_empty() {
                return Err(invalid(format!("The default of \"{}\" is empty.", c.name)));
            }
            if is_now(t) || t.eq_ignore_ascii_case("NULL") || is_literal(t) {
                return Ok(t.to_string());
            }
            if server.variant == ServerVariant::MariaDb || mysql8_at_least(server, (8, 0, 13)) {
                Ok(format!("({t})"))
            } else {
                Err(unsupported("expression default", "MySQL 8.0.13"))
            }
        }
    }
}

/// MySQL refuses a literal `DEFAULT` on TEXT, BLOB, JSON and spatial columns.
fn no_literal_default(t: &TypeSpec) -> bool {
    const SPATIAL: &[&str] = &[
        "geometry",
        "point",
        "linestring",
        "polygon",
        "multipoint",
        "multilinestring",
        "multipolygon",
        "geometrycollection",
        "geomcollection",
    ];
    let base = match t {
        TypeSpec::Known { base, .. } => base.to_ascii_lowercase(),
        TypeSpec::Custom { text } => text
            .split(|c: char| !c.is_ascii_alphanumeric())
            .next()
            .unwrap_or("")
            .to_ascii_lowercase(),
    };
    let blob_like = matches!(
        base.as_str(),
        "tinytext"
            | "text"
            | "mediumtext"
            | "longtext"
            | "tinyblob"
            | "blob"
            | "mediumblob"
            | "longblob"
            | "json"
    );
    blob_like || SPATIAL.contains(&base.as_str())
}

/// `CURRENT_TIMESTAMP`, `NOW`, `LOCALTIME`, `LOCALTIMESTAMP`, with `()` or `(n)`.
fn is_now(t: &str) -> bool {
    let upper = t.to_ascii_uppercase();
    let (name, args) = match upper.find('(') {
        Some(i) => (&upper[..i], &upper[i..]),
        None => (upper.as_str(), ""),
    };
    let args_ok = args.is_empty()
        || args
            .strip_prefix('(')
            .and_then(|a| a.strip_suffix(')'))
            .is_some_and(|n| n.bytes().all(|b| b.is_ascii_digit()));
    let name_ok = match name {
        "CURRENT_TIMESTAMP" | "LOCALTIME" | "LOCALTIMESTAMP" => true,
        "NOW" => !args.is_empty(),
        _ => false,
    };
    name_ok && args_ok
}

/// A number, `TRUE` / `FALSE`, a `'…'` string without backslashes (the same under
/// every `sql_mode`), `b'…'`, `x'…'`, `0x…` or `0b…`.
fn is_literal(t: &str) -> bool {
    if is_number(t) || t.eq_ignore_ascii_case("TRUE") || t.eq_ignore_ascii_case("FALSE") {
        return true;
    }
    let hex = |s: &str| s.bytes().all(|b| b.is_ascii_hexdigit());
    let bits = |s: &str| s.bytes().all(|b| b == b'0' || b == b'1');
    let quoted = |s: &str| {
        s.len() >= 2
            && s.starts_with('\'')
            && s.ends_with('\'')
            && !s.contains('\\')
            && s[1..s.len() - 1].replace("''", "").find('\'').is_none()
    };
    let lower = t.to_ascii_lowercase();
    if let Some(rest) = lower.strip_prefix("0x") {
        return !rest.is_empty() && hex(rest);
    }
    if let Some(rest) = lower.strip_prefix("0b") {
        return !rest.is_empty() && bits(rest);
    }
    match lower.as_bytes().first() {
        Some(b'x') => quoted(&lower[1..]) && hex(&lower[2..lower.len() - 1]),
        Some(b'b') => quoted(&lower[1..]) && bits(&lower[2..lower.len() - 1]),
        _ => quoted(t),
    }
}

/// What a type change may lose: `None` when nothing, `Some(Some(n))` when values longer
/// than `n` characters are cut (countable), `Some(None)` when it cannot be counted.
fn lossy(old: &TypeSpec, new: &TypeSpec) -> Option<Option<u64>> {
    let (
        TypeSpec::Known {
            base: ob,
            length: ol,
            precision: op,
            scale: os,
            unsigned: ou,
            values: ov,
        },
        TypeSpec::Known {
            base: nb,
            length: nl,
            precision: np,
            scale: ns,
            unsigned: nu,
            values: nv,
        },
    ) = (old, new)
    else {
        return Some(None);
    };
    let maybe = |lossy: bool| lossy.then_some(None);
    match (family(ob), family(nb)) {
        (Family::Int(a), Family::Int(b)) => maybe(b < a || ou != nu),
        (Family::Decimal, Family::Decimal) => {
            let (op, os) = (op.unwrap_or(10), os.unwrap_or(0));
            let (np, ns) = (np.unwrap_or(10), ns.unwrap_or(0));
            maybe(np.saturating_sub(ns) < op.saturating_sub(os) || ns < os || ou != nu)
        }
        (Family::Float(a), Family::Float(b)) => {
            let narrower = match (op, np) {
                (None, Some(_)) => true,
                (Some(o), Some(n)) => n < o,
                _ => false,
            };
            maybe(b < a || narrower || os.unwrap_or(0) > ns.unwrap_or(0) || ou != nu)
        }
        (Family::Str, Family::Str) => {
            let is_list = |b: &str| b == "enum" || b == "set";
            if is_list(nb) {
                let kept = is_list(ob) && ov.iter().all(|v| nv.contains(v));
                return maybe(!kept);
            }
            let old_cap = if is_list(ob) {
                let lens = ov.iter().map(|v| v.chars().count() as u64);
                if ob == "set" {
                    lens.sum::<u64>() + (ov.len() as u64).saturating_sub(1)
                } else {
                    lens.max().unwrap_or(0)
                }
            } else {
                text_cap(ob, *ol)
            };
            let new_cap = text_cap(nb, *nl);
            if new_cap >= old_cap {
                None
            } else if nb == "char" || nb == "varchar" {
                Some(Some(new_cap))
            } else {
                Some(None)
            }
        }
        (Family::Bin, Family::Bin) => maybe(text_cap(nb, *nl) < text_cap(ob, *ol)),
        (Family::Bit, Family::Bit) => maybe(nl.unwrap_or(1) < ol.unwrap_or(1)),
        (Family::Temporal, Family::Temporal) => {
            let keeps_fraction = np.unwrap_or(0) >= op.unwrap_or(0);
            let safe = match (ob.as_str(), nb.as_str()) {
                (a, b) if a == b => keeps_fraction,
                ("date", "datetime") => true,
                ("timestamp", "datetime") => keeps_fraction,
                _ => false,
            };
            maybe(!safe)
        }
        (Family::Json, Family::Json) => None,
        _ => Some(None),
    }
}

#[derive(PartialEq)]
enum Family {
    Int(u8),
    Decimal,
    Float(u8),
    Bit,
    Str,
    Bin,
    Temporal,
    Json,
    Other,
}

fn family(base: &str) -> Family {
    match base {
        "tinyint" | "bool" => Family::Int(1),
        "smallint" => Family::Int(2),
        "mediumint" => Family::Int(3),
        "int" => Family::Int(4),
        "bigint" => Family::Int(5),
        "decimal" => Family::Decimal,
        "float" => Family::Float(1),
        "double" => Family::Float(2),
        "bit" => Family::Bit,
        "char" | "varchar" | "tinytext" | "text" | "mediumtext" | "longtext" | "enum" | "set" => {
            Family::Str
        }
        "binary" | "varbinary" | "tinyblob" | "blob" | "mediumblob" | "longblob" => Family::Bin,
        "date" | "time" | "datetime" | "timestamp" | "year" => Family::Temporal,
        "json" => Family::Json,
        _ => Family::Other,
    }
}

/// The most characters (bytes for binary types) a string type holds.
fn text_cap(base: &str, length: Option<u32>) -> u64 {
    match base {
        "char" | "binary" => u64::from(length.unwrap_or(1)),
        "varchar" | "varbinary" => u64::from(length.unwrap_or(0)),
        "tinytext" | "tinyblob" => 255,
        "text" | "blob" => 65_535,
        "mediumtext" | "mediumblob" => 16_777_215,
        _ => 4_294_967_295,
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
    Ok(format!("PRIMARY KEY ({})", names(&k.columns)))
}

pub(super) fn index_sql(
    i: &IndexDef,
    server: &ServerInfo,
    d: Dialect,
) -> Result<String, RenderError> {
    check_dialect(d)?;
    if !i.include.is_empty() {
        return Err(invalid("INCLUDE is PostgreSQL only."));
    }
    if i.parts.is_empty() {
        return Err(invalid(format!(
            "Index \"{}\" needs at least one column.",
            i.name
        )));
    }
    let method = match &i.method {
        Some(m) => Some(word("index method", m)?.to_ascii_uppercase()),
        None => None,
    };
    let (keyword, using) = match method.as_deref() {
        Some("FULLTEXT") => ("FULLTEXT KEY", None),
        Some("SPATIAL") => ("SPATIAL KEY", None),
        _ if i.unique => ("UNIQUE KEY", method),
        _ => ("KEY", method),
    };
    let mut parts = Vec::new();
    for p in &i.parts {
        let mut s = match (&p.column, &p.expression) {
            (Some(c), None) => match p.prefix {
                Some(n) => format!("{}({n})", q(c)),
                None => q(c),
            },
            (None, Some(e)) => {
                match server.variant {
                    ServerVariant::MariaDb => {
                        return Err(unsupported(
                            "expression index",
                            "MySQL 8.0.13 (not available on MariaDB)",
                        ))
                    }
                    _ if !mysql8_at_least(server, (8, 0, 13)) => {
                        return Err(unsupported("expression index", "MySQL 8.0.13"))
                    }
                    _ => {}
                }
                if e.trim().is_empty() || p.prefix.is_some() {
                    return Err(invalid(format!(
                        "Invalid expression part in \"{}\".",
                        i.name
                    )));
                }
                format!("({})", e.trim())
            }
            _ => {
                return Err(invalid(
                    "Each index part is either a column or an expression.",
                ))
            }
        };
        if p.descending {
            match server.variant {
                ServerVariant::MySql57 => return Err(unsupported("descending index", "MySQL 8.0")),
                ServerVariant::MariaDb if server.version < (10, 8, 0) => {
                    return Err(unsupported("descending index", "MariaDB 10.8"))
                }
                _ => {}
            }
            s.push_str(" DESC");
        }
        parts.push(s);
    }
    let mut out = format!("{keyword} {} ({})", q(&i.name), parts.join(","));
    if let Some(m) = using {
        out.push_str(&format!(" USING {m}"));
    }
    if let Some(c) = i.comment.as_deref().filter(|c| !c.is_empty()) {
        out.push_str(&format!(" COMMENT {}", string(c, d)?));
    }
    if !i.visible {
        match server.variant {
            ServerVariant::MySql8 => {}
            ServerVariant::MariaDb => {
                return Err(unsupported(
                    "invisible index",
                    "MySQL 8.0 (MariaDB has IGNORED indexes, which the designer does not edit)",
                ))
            }
            _ => return Err(unsupported("invisible index", "MySQL 8.0")),
        }
        out.push_str(" INVISIBLE");
    }
    Ok(out)
}

pub(super) fn fk_sql(f: &FkDef, _server: &ServerInfo, d: Dialect) -> Result<String, RenderError> {
    check_dialect(d)?;
    if f.match_type.is_some() {
        return Err(invalid("MATCH is PostgreSQL only."));
    }
    if f.deferrable || f.initially_deferred {
        return Err(invalid("Deferrable constraints are PostgreSQL only."));
    }
    if f.columns.is_empty() || f.columns.len() != f.ref_columns.len() {
        return Err(invalid(format!(
            "Foreign key \"{}\" needs as many referenced columns as columns.",
            f.name
        )));
    }
    let target = match &f.ref_schema {
        Some(s) => format!("{}.{}", q(s), q(&f.ref_table)),
        None => q(&f.ref_table),
    };
    let mut out = format!(
        "CONSTRAINT {} FOREIGN KEY ({}) REFERENCES {target} ({})",
        q(&f.name),
        names(&f.columns),
        names(&f.ref_columns)
    );
    for (kw, action) in [("ON DELETE", &f.on_delete), ("ON UPDATE", &f.on_update)] {
        let a = fk_action(action)?;
        if a != "RESTRICT" {
            out.push_str(&format!(" {kw} {a}"));
        }
    }
    Ok(out)
}

pub(super) fn check_sql(
    c: &CheckDef,
    server: &ServerInfo,
    d: Dialect,
) -> Result<String, RenderError> {
    check_dialect(d)?;
    check_supported(server)?;
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
    use dnt_core::server::SqlKind;
    use dnt_core::sql::MySqlMode;

    const NBE: Dialect = Dialect::MySql(MySqlMode {
        ansi_quotes: false,
        no_backslash_escapes: true,
    });
    const MY: Dialect = Dialect::MYSQL;

    fn ty(
        base: &str,
        length: Option<u32>,
        precision: Option<u32>,
        scale: Option<u32>,
        unsigned: bool,
        values: &[&str],
    ) -> TypeSpec {
        TypeSpec::Known {
            base: base.into(),
            length,
            precision,
            scale,
            unsigned,
            values: values.iter().map(|v| v.to_string()).collect(),
        }
    }

    fn known(base: &str) -> TypeSpec {
        ty(base, None, None, None, false, &[])
    }

    fn sized(base: &str, length: u32) -> TypeSpec {
        ty(base, Some(length), None, None, false, &[])
    }

    fn decimal(p: u32, s: u32) -> TypeSpec {
        ty("decimal", None, Some(p), Some(s), false, &[])
    }

    fn enumeration(values: &[&str]) -> TypeSpec {
        ty("enum", None, None, None, false, values)
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

    fn table(name: &str) -> TableRef {
        TableRef {
            kind: SqlKind::MySql,
            database: "dnt_fixture".into(),
            schema: None,
            name: name.into(),
        }
    }

    /// The board-10 `orders` table as stored.
    fn orders() -> TableDef {
        TableDef {
            original_name: Some("orders".into()),
            name: "orders".into(),
            columns: vec![
                ColumnDef {
                    nullable: false,
                    auto_increment: true,
                    ..col("id", ty("bigint", None, None, None, true, &[]))
                },
                ColumnDef {
                    nullable: false,
                    default: value("0.00"),
                    ..col("total", decimal(12, 2))
                },
                col("shipped_at", known("datetime")),
                col("note", sized("varchar", 255)),
            ],
            primary_key: Some(KeyDef {
                original: Some("PRIMARY".into()),
                name: Some("PRIMARY".into()),
                columns: vec!["id".into()],
                locked: false,
            }),
            indexes: vec![],
            foreign_keys: vec![],
            checks: vec![],
            options: TableOptionsDef {
                engine: Some("InnoDB".into()),
                charset: Some("utf8mb4".into()),
                collation: Some("utf8mb4_unicode_ci".into()),
                row_format: None,
                locked: false,
            },
            comment: None,
            read_only_reason: None,
        }
    }

    fn render_ok(old: &TableDef, new: &TableDef, server: &ServerInfo) -> Rendered {
        let changes = diff(old, new).expect("diff");
        render(&table(&old.name), old, &changes, server, MY).expect("render")
    }

    fn one(r: &Rendered) -> &str {
        assert_eq!(r.statements.len(), 1, "{:?}", r.statements);
        &r.statements[0]
    }

    fn kinds(impacts: &[Impact]) -> Vec<(ImpactKind, ElementRef)> {
        impacts
            .iter()
            .map(|i| (i.kind, i.element.clone()))
            .collect()
    }

    #[test]
    fn alter_combines_into_one_statement() {
        let old = orders();
        let mut new = old.clone();
        new.columns[1].data_type = decimal(14, 2);
        new.columns[2].comment = Some("Ngày giao cho đơn vị vận chuyển".into());
        new.columns.remove(3);
        new.columns.push(ColumnDef {
            nullable: false,
            default: value("web"),
            ..new_col("channel", enumeration(&["web", "app", "pos"]))
        });
        let r = render_ok(&old, &new, &mysql8());
        assert_eq!(
            one(&r),
            "ALTER TABLE `dnt_fixture`.`orders` MODIFY COLUMN `total` decimal(14,2) NOT NULL \
             DEFAULT '0.00', MODIFY COLUMN `shipped_at` datetime NULL COMMENT 'Ngày giao cho \
             đơn vị vận chuyển', DROP COLUMN `note`, ADD COLUMN `channel` enum('web','app',\
             'pos') NOT NULL DEFAULT 'web' AFTER `shipped_at`"
        );
        assert_eq!(
            kinds(&r.impacts),
            vec![
                (ImpactKind::DataLoss, ElementRef::Column("note".into())),
                (ImpactKind::Rebuild, ElementRef::Column("total".into())),
            ]
        );
        assert_eq!(
            r.impacts[0].count_sql.as_deref(),
            Some("SELECT COUNT(*) FROM `dnt_fixture`.`orders` WHERE `note` IS NOT NULL")
        );
        assert!(r.impacts.iter().all(|i| !i.message.is_empty()));
    }

    #[test]
    fn rename_column_uses_change() {
        let old = orders();
        // Rename and retype: one CHANGE with the new definition.
        let mut new = old.clone();
        new.columns[3].name = "memo".into();
        new.columns[3].data_type = sized("varchar", 500);
        assert_eq!(
            one(&render_ok(&old, &new, &mysql8())),
            "ALTER TABLE `dnt_fixture`.`orders` CHANGE COLUMN `note` `memo` varchar(500) NULL"
        );
        // A pure rename keeps the stored definition.
        let mut new = old.clone();
        new.columns[3].name = "memo".into();
        let r = render_ok(&old, &new, &mysql8());
        assert_eq!(
            one(&r),
            "ALTER TABLE `dnt_fixture`.`orders` CHANGE COLUMN `note` `memo` varchar(255) NULL"
        );
        assert!(r.impacts.is_empty());
        // Rename + move: still one clause, naming the original column.
        let mut new = old.clone();
        let mut memo = new.columns.remove(3);
        memo.name = "memo".into();
        new.columns.insert(0, memo);
        let r = render_ok(&old, &new, &mysql8());
        assert_eq!(
            one(&r),
            "ALTER TABLE `dnt_fixture`.`orders` CHANGE COLUMN `note` `memo` varchar(255) NULL \
             FIRST"
        );
        assert_eq!(
            kinds(&r.impacts),
            vec![(ImpactKind::Rebuild, ElementRef::Column("memo".into()))]
        );
    }

    #[test]
    fn move_column_after_uses_new_names() {
        // MySQL resolves AFTER against the new definition (checked on 5.7, 8.0 and
        // MariaDB 10.6: the old name of a renamed column is "Unknown column").
        let old = orders();
        let mut new = old.clone();
        new.columns[2].name = "shipped".into();
        let total = new.columns.remove(1);
        new.columns.insert(2, total);
        assert_eq!(
            one(&render_ok(&old, &new, &mysql8())),
            "ALTER TABLE `dnt_fixture`.`orders` CHANGE COLUMN `shipped_at` `shipped` datetime \
             NULL, MODIFY COLUMN `total` decimal(12,2) NOT NULL DEFAULT '0.00' AFTER `shipped`"
        );
    }

    #[test]
    fn rename_table_inside_alter() {
        let old = orders();
        let mut new = old.clone();
        new.name = "orders_v2".into();
        new.comment = Some("Đơn hàng".into());
        assert_eq!(
            one(&render_ok(&old, &new, &mysql8())),
            "ALTER TABLE `dnt_fixture`.`orders` RENAME TO `dnt_fixture`.`orders_v2`, \
             COMMENT='Đơn hàng'"
        );
    }

    #[test]
    fn check_unsupported_on_57() {
        let unsupported = RenderError::Unsupported {
            feature: "CHECK constraint".into(),
            min_version: "MySQL 8.0.16".into(),
        };
        let mut new = orders();
        new.original_name = None;
        new.checks.push(CheckDef {
            original: None,
            name: "ck_total".into(),
            expression: "total >= 0".into(),
            locked: false,
        });
        assert_eq!(
            render_create(&table("orders"), &new, &mysql57(), MY),
            Err(unsupported.clone())
        );
        let old = orders();
        let mut new = old.clone();
        new.checks.push(CheckDef {
            original: None,
            name: "ck_total".into(),
            expression: "total >= 0".into(),
            locked: false,
        });
        let changes = diff(&old, &new).unwrap();
        assert_eq!(
            render(&table("orders"), &old, &changes, &mysql57(), MY),
            Err(unsupported)
        );
        assert_eq!(
            one(&render_ok(&old, &new, &mysql8())),
            "ALTER TABLE `dnt_fixture`.`orders` ADD CONSTRAINT `ck_total` CHECK (total >= 0)"
        );
    }

    #[test]
    fn quotes_hostile_identifiers() {
        let mut old = orders();
        old.original_name = Some("we`ird".into());
        old.name = "we`ird".into();
        let mut new = old.clone();
        new.columns.push(new_col("đơn_hàng", known("int")));
        new.indexes.push(IndexDef {
            original: None,
            name: "ix`x".into(),
            unique: false,
            parts: vec![IndexPartDef {
                column: Some("đơn_hàng".into()),
                expression: None,
                prefix: None,
                descending: false,
            }],
            method: None,
            include: vec![],
            visible: true,
            comment: None,
            constraint: false,
            locked: false,
        });
        assert_eq!(
            one(&render_ok(&old, &new, &mysql8())),
            "ALTER TABLE `dnt_fixture`.`we``ird` ADD COLUMN `đơn_hàng` int NULL AFTER `note`, \
             ADD KEY `ix``x` (`đơn_hàng`)"
        );
    }

    #[test]
    fn no_backslash_escapes_default_literal() {
        let c = ColumnDef {
            default: value("a\\b"),
            comment: Some("it's".into()),
            ..col("c", sized("varchar", 10))
        };
        assert_eq!(
            column_sql(&c, &mysql8(), NBE).unwrap(),
            "`c` varchar(10) NULL DEFAULT 'a\\b' COMMENT 'it''s'"
        );
        assert_eq!(
            column_sql(&c, &mysql8(), MY).unwrap(),
            "`c` varchar(10) NULL DEFAULT 'a\\\\b' COMMENT 'it\\'s'"
        );
        // Enum members are escaped the same way.
        let e = col("e", enumeration(&["it's"]));
        assert_eq!(
            column_sql(&e, &mysql8(), MY).unwrap(),
            "`e` enum('it\\'s') NULL"
        );
        assert_eq!(
            column_sql(&e, &mysql8(), NBE).unwrap(),
            "`e` enum('it''s') NULL"
        );
    }

    #[test]
    fn create_table_full() {
        let mut def = orders();
        def.original_name = None;
        for c in &mut def.columns {
            c.original = None;
        }
        def.primary_key.as_mut().unwrap().original = None;
        def.columns.push(ColumnDef {
            nullable: false,
            ..new_col("customer_id", known("bigint"))
        });
        def.columns.push(ColumnDef {
            default: expr("CURRENT_TIMESTAMP(3)"),
            on_update: Some("CURRENT_TIMESTAMP(3)".into()),
            ..new_col(
                "updated_at",
                ty("datetime", None, Some(3), None, false, &[]),
            )
        });
        def.indexes.push(IndexDef {
            original: None,
            name: "idx_customer".into(),
            unique: false,
            parts: vec![IndexPartDef {
                column: Some("customer_id".into()),
                expression: None,
                prefix: None,
                descending: false,
            }],
            method: None,
            include: vec![],
            visible: true,
            comment: None,
            constraint: false,
            locked: false,
        });
        def.foreign_keys.push(FkDef {
            original: None,
            name: "fk_customer".into(),
            columns: vec!["customer_id".into()],
            ref_schema: None,
            ref_table: "customers".into(),
            ref_columns: vec!["id".into()],
            on_update: "RESTRICT".into(),
            on_delete: "cascade".into(),
            match_type: None,
            deferrable: false,
            initially_deferred: false,
            locked: false,
        });
        def.checks.push(CheckDef {
            original: None,
            name: "ck_total".into(),
            expression: "total >= 0".into(),
            locked: false,
        });
        def.comment = Some("Orders".into());
        let r = render_create(&table("orders"), &def, &mysql8(), MY).unwrap();
        assert_eq!(
            one(&r),
            "CREATE TABLE `dnt_fixture`.`orders` (\n\
             \x20 `id` bigint unsigned NOT NULL AUTO_INCREMENT,\n\
             \x20 `total` decimal(12,2) NOT NULL DEFAULT '0.00',\n\
             \x20 `shipped_at` datetime NULL,\n\
             \x20 `note` varchar(255) NULL,\n\
             \x20 `customer_id` bigint NOT NULL,\n\
             \x20 `updated_at` datetime(3) NULL DEFAULT CURRENT_TIMESTAMP(3) ON UPDATE \
             CURRENT_TIMESTAMP(3),\n\
             \x20 PRIMARY KEY (`id`),\n\
             \x20 KEY `idx_customer` (`customer_id`),\n\
             \x20 CONSTRAINT `fk_customer` FOREIGN KEY (`customer_id`) REFERENCES `customers` \
             (`id`) ON DELETE CASCADE,\n\
             \x20 CONSTRAINT `ck_total` CHECK (total >= 0)\n\
             ) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_unicode_ci COMMENT='Orders'"
        );
        assert!(r.impacts.is_empty());
    }

    #[test]
    fn element_fragments() {
        let s = mysql8();
        let idx = IndexDef {
            original: None,
            name: "ix".into(),
            unique: true,
            parts: vec![
                IndexPartDef {
                    column: Some("a".into()),
                    expression: None,
                    prefix: Some(10),
                    descending: true,
                },
                IndexPartDef {
                    column: None,
                    expression: Some("lower(`b`)".into()),
                    prefix: None,
                    descending: false,
                },
            ],
            method: Some("BTREE".into()),
            include: vec![],
            visible: false,
            comment: Some("c".into()),
            constraint: false,
            locked: false,
        };
        assert_eq!(
            index_sql(&idx, &s, MY).unwrap(),
            "UNIQUE KEY `ix` (`a`(10) DESC,(lower(`b`))) USING BTREE COMMENT 'c' INVISIBLE"
        );
        let invisible = RenderError::Unsupported {
            feature: "invisible index".into(),
            min_version:
                "MySQL 8.0 (MariaDB has IGNORED indexes, which the designer does not edit)".into(),
        };
        let plain = IndexDef {
            parts: vec![idx.parts[0].clone()],
            ..idx.clone()
        };
        assert_eq!(index_sql(&plain, &mariadb(), MY), Err(invisible));
        let visible = IndexDef {
            visible: true,
            ..plain
        };
        assert_eq!(
            index_sql(&visible, &mysql57(), MY),
            Err(RenderError::Unsupported {
                feature: "descending index".into(),
                min_version: "MySQL 8.0".into(),
            })
        );
        let fk = FkDef {
            original: None,
            name: "fk".into(),
            columns: vec!["a".into(), "b".into()],
            ref_schema: Some("other".into()),
            ref_table: "u".into(),
            ref_columns: vec!["x".into(), "y".into()],
            on_update: "set null".into(),
            on_delete: "NO ACTION".into(),
            match_type: None,
            deferrable: false,
            initially_deferred: false,
            locked: false,
        };
        assert_eq!(
            fk_sql(&fk, &s, MY).unwrap(),
            "CONSTRAINT `fk` FOREIGN KEY (`a`,`b`) REFERENCES `other`.`u` (`x`,`y`) \
             ON DELETE NO ACTION ON UPDATE SET NULL"
        );
        let pk = KeyDef {
            original: None,
            name: Some("PRIMARY".into()),
            columns: vec!["a".into(), "b".into()],
            locked: false,
        };
        assert_eq!(
            primary_key_sql(&pk, &s, MY).unwrap(),
            "PRIMARY KEY (`a`,`b`)"
        );
    }

    #[test]
    fn defaults_and_raw_words() {
        let s8 = mysql8();
        let s57 = mysql57();
        let c = |default| ColumnDef {
            default,
            ..col("c", known("int"))
        };
        // Literal expressions stay bare; others are parenthesised on 8.0.13+.
        assert_eq!(
            column_sql(&c(expr("b'1'")), &s57, MY).unwrap(),
            "`c` int NULL DEFAULT b'1'"
        );
        assert_eq!(
            column_sql(&c(expr("NULL")), &s57, MY).unwrap(),
            "`c` int NULL DEFAULT NULL"
        );
        assert_eq!(
            column_sql(&c(expr("rand() * 10")), &s8, MY).unwrap(),
            "`c` int NULL DEFAULT (rand() * 10)"
        );
        assert_eq!(
            column_sql(&c(expr("rand() * 10")), &s57, MY),
            Err(RenderError::Unsupported {
                feature: "expression default".into(),
                min_version: "MySQL 8.0.13".into(),
            })
        );
        assert_eq!(
            column_sql(&c(expr("rand()")), &mariadb(), MY).unwrap(),
            "`c` int NULL DEFAULT (rand())"
        );
        // A number default on a numeric column must be a number.
        assert!(matches!(
            column_sql(&c(value("abc")), &s8, MY),
            Err(RenderError::Invalid { .. })
        ));
        assert_eq!(
            column_sql(&c(value("-5")), &s8, MY).unwrap(),
            "`c` int NULL DEFAULT '-5'"
        );
        // Charset / collation are spliced only as plain words.
        let cs = ColumnDef {
            charset: Some("utf8mb4".into()),
            collation: Some("utf8mb4_bin".into()),
            ..col("c", sized("varchar", 5))
        };
        assert_eq!(
            column_sql(&cs, &s8, MY).unwrap(),
            "`c` varchar(5) CHARACTER SET utf8mb4 COLLATE utf8mb4_bin NULL"
        );
        let bad = ColumnDef {
            collation: Some("x NOT NULL".into()),
            ..cs
        };
        assert!(matches!(
            column_sql(&bad, &s8, MY),
            Err(RenderError::Invalid { .. })
        ));
        // ON UPDATE is only the current-timestamp family.
        let on = ColumnDef {
            on_update: Some("now() + 1".into()),
            ..col("c", known("datetime"))
        };
        assert!(matches!(
            column_sql(&on, &s8, MY),
            Err(RenderError::Invalid { .. })
        ));
        // A PostgreSQL dialect never renders MySQL literals.
        assert!(matches!(
            column_sql(&c(value("1")), &s8, Dialect::Postgres),
            Err(RenderError::Invalid { .. })
        ));
    }

    #[test]
    fn keys_options_and_impacts() {
        let mut old = orders();
        old.indexes.push(IndexDef {
            original: Some("ix_note".into()),
            name: "ix_note".into(),
            unique: false,
            parts: vec![IndexPartDef {
                column: Some("note".into()),
                expression: None,
                prefix: None,
                descending: false,
            }],
            method: None,
            include: vec![],
            visible: true,
            comment: None,
            constraint: false,
            locked: false,
        });
        let mut new = old.clone();
        new.primary_key.as_mut().unwrap().columns = vec!["id".into(), "total".into()];
        new.indexes[0].name = "ix_memo".into();
        new.options.engine = Some("MyISAM".into());
        new.options.row_format = Some("DYNAMIC".into());
        new.columns[2].nullable = false;
        new.columns[3].data_type = sized("varchar", 20);
        let r = render_ok(&old, &new, &mysql8());
        assert_eq!(
            one(&r),
            "ALTER TABLE `dnt_fixture`.`orders` ENGINE=MyISAM, ROW_FORMAT=DYNAMIC, \
             DROP PRIMARY KEY, MODIFY COLUMN `shipped_at` datetime NOT NULL, \
             MODIFY COLUMN `note` varchar(20) NULL, ADD PRIMARY KEY (`id`,`total`), \
             RENAME INDEX `ix_note` TO `ix_memo`"
        );
        assert_eq!(
            kinds(&r.impacts),
            vec![
                (
                    ImpactKind::DataLoss,
                    ElementRef::Column("shipped_at".into())
                ),
                (ImpactKind::DataLoss, ElementRef::Column("note".into())),
                (ImpactKind::Rebuild, ElementRef::Column("note".into())),
                (ImpactKind::NotTransactional, ElementRef::Options),
            ]
        );
        assert_eq!(
            r.impacts[0].count_sql.as_deref(),
            Some("SELECT COUNT(*) FROM `dnt_fixture`.`orders` WHERE `shipped_at` IS NULL")
        );
        assert_eq!(
            r.impacts[1].count_sql.as_deref(),
            Some("SELECT COUNT(*) FROM `dnt_fixture`.`orders` WHERE CHAR_LENGTH(`note`) > 20")
        );
        // A family change cannot be counted.
        let mut new = old.clone();
        new.columns[3].data_type = known("int");
        let r = render_ok(&old, &new, &mysql8());
        assert_eq!(r.impacts[0].kind, ImpactKind::DataLoss);
        assert_eq!(r.impacts[0].count_sql, None);
        assert!(r.impacts[0].message.contains("may truncate values"));
    }

    #[test]
    fn drops_and_same_name_foreign_keys() {
        let mut old = orders();
        old.columns.push(col("customer_id", known("bigint")));
        old.foreign_keys.push(FkDef {
            original: Some("fk_c".into()),
            name: "fk_c".into(),
            columns: vec!["customer_id".into()],
            ref_schema: None,
            ref_table: "customers".into(),
            ref_columns: vec!["id".into()],
            on_update: "RESTRICT".into(),
            on_delete: "RESTRICT".into(),
            match_type: None,
            deferrable: false,
            initially_deferred: false,
            locked: false,
        });
        old.checks.push(CheckDef {
            original: Some("ck".into()),
            name: "ck".into(),
            expression: "total > 0".into(),
            locked: false,
        });
        let mut new = old.clone();
        new.foreign_keys.clear();
        new.checks.clear();
        assert_eq!(
            one(&render_ok(&old, &new, &mysql8())),
            "ALTER TABLE `dnt_fixture`.`orders` DROP FOREIGN KEY `fk_c`, DROP CHECK `ck`"
        );
        assert_eq!(
            one(&render_ok(&old, &new, &mariadb())),
            "ALTER TABLE `dnt_fixture`.`orders` DROP FOREIGN KEY `fk_c`, DROP CONSTRAINT `ck`"
        );
        // MySQL and MariaDB refuse to drop and re-add one FK name in one statement.
        let mut new = old.clone();
        new.foreign_keys[0].on_delete = "CASCADE".into();
        let changes = diff(&old, &new).unwrap();
        assert!(matches!(
            render(&table("orders"), &old, &changes, &mysql8(), MY),
            Err(RenderError::Invalid { message }) if message.contains("fk_c")
        ));
        // Under another name it works.
        new.foreign_keys[0].name = "fk_c2".into();
        assert_eq!(
            one(&render_ok(&old, &new, &mysql8())),
            "ALTER TABLE `dnt_fixture`.`orders` DROP FOREIGN KEY `fk_c`, ADD CONSTRAINT \
             `fk_c2` FOREIGN KEY (`customer_id`) REFERENCES `customers` (`id`) ON DELETE CASCADE"
        );
        // No change, no statement.
        let r = render_ok(&old, &old.clone(), &mysql8());
        assert!(r.statements.is_empty() && r.impacts.is_empty());
    }

    #[test]
    fn altered_or_renamed_moves_keep_diff_order() {
        // Draft order id, note, shipped_at, total: `shipped_at` moves after `note`, then
        // `total` after `shipped_at`. `total` is also altered: its clause must still
        // come after the move it depends on.
        let old = orders();
        let reorder = |t: &mut TableDef| {
            let cols = t.columns.clone();
            t.columns = vec![
                cols[0].clone(),
                cols[3].clone(),
                cols[2].clone(),
                cols[1].clone(),
            ];
        };
        let mut new = old.clone();
        reorder(&mut new);
        new.columns[3].comment = Some("x".into());
        let r = render_ok(&old, &new, &mysql8());
        assert_eq!(
            one(&r),
            "ALTER TABLE `dnt_fixture`.`orders` MODIFY COLUMN `shipped_at` datetime NULL AFTER \
             `note`, MODIFY COLUMN `total` decimal(12,2) NOT NULL DEFAULT '0.00' COMMENT 'x' \
             AFTER `shipped_at`"
        );
        // Renamed instead of altered.
        let mut new = old.clone();
        reorder(&mut new);
        new.columns[3].name = "amount".into();
        assert_eq!(
            one(&render_ok(&old, &new, &mysql8())),
            "ALTER TABLE `dnt_fixture`.`orders` MODIFY COLUMN `shipped_at` datetime NULL AFTER \
             `note`, CHANGE COLUMN `total` `amount` decimal(12,2) NOT NULL DEFAULT '0.00' \
             AFTER `shipped_at`"
        );
    }

    #[test]
    fn table_charset_change_keeps_column_charset() {
        let old = orders();
        let mut new = old.clone();
        new.options.charset = Some("latin1".into());
        new.options.collation = Some("latin1_swedish_ci".into());
        new.columns[1].comment = Some("t".into());
        new.columns[3].comment = Some("n".into());
        new.columns.push(new_col("extra", sized("varchar", 5)));
        assert_eq!(
            one(&render_ok(&old, &new, &mysql8())),
            "ALTER TABLE `dnt_fixture`.`orders` DEFAULT CHARSET=latin1, \
             COLLATE=latin1_swedish_ci, MODIFY COLUMN `total` decimal(12,2) NOT NULL DEFAULT \
             '0.00' COMMENT 't', MODIFY COLUMN `note` varchar(255) CHARACTER SET utf8mb4 \
             COLLATE utf8mb4_unicode_ci NULL COMMENT 'n', ADD COLUMN `extra` varchar(5) NULL \
             AFTER `note`"
        );
        // An explicit column charset is kept as it is.
        let mut old = orders();
        old.columns[3].charset = Some("ascii".into());
        let mut new = old.clone();
        new.options.collation = Some("utf8mb4_bin".into());
        new.columns[3].comment = Some("n".into());
        assert_eq!(
            one(&render_ok(&old, &new, &mysql8())),
            "ALTER TABLE `dnt_fixture`.`orders` COLLATE=utf8mb4_bin, MODIFY COLUMN `note` \
             varchar(255) CHARACTER SET ascii NULL COMMENT 'n'"
        );
    }

    #[test]
    fn value_default_on_blob_like_columns() {
        let c = |data_type| ColumnDef {
            default: value("x"),
            ..col("c", data_type)
        };
        for t in [
            known("text"),
            known("json"),
            known("blob"),
            TypeSpec::Custom {
                text: "geometry".into(),
            },
        ] {
            assert!(matches!(
                column_sql(&c(t.clone()), &mysql8(), MY),
                Err(RenderError::Unsupported { .. })
            ));
        }
        // MariaDB takes them; an expression default is the MySQL route.
        assert_eq!(
            column_sql(&c(known("text")), &mariadb(), MY).unwrap(),
            "`c` text NULL DEFAULT 'x'"
        );
        let e = ColumnDef {
            default: expr("concat('a', 'b')"),
            ..col("c", known("text"))
        };
        assert_eq!(
            column_sql(&e, &mysql8(), MY).unwrap(),
            "`c` text NULL DEFAULT (concat('a', 'b'))"
        );
    }

    #[test]
    fn clearing_options_only_is_invalid() {
        let old = orders();
        let mut new = old.clone();
        new.options.engine = None;
        let changes = diff(&old, &new).unwrap();
        assert!(matches!(
            render(&table("orders"), &old, &changes, &mysql8(), MY),
            Err(RenderError::Invalid { .. })
        ));
    }

    #[test]
    fn mariadb_unsupported_messages() {
        let i = IndexDef {
            original: None,
            name: "ix".into(),
            unique: false,
            parts: vec![IndexPartDef {
                column: None,
                expression: Some("lower(`a`)".into()),
                prefix: None,
                descending: false,
            }],
            method: None,
            include: vec![],
            visible: true,
            comment: None,
            constraint: false,
            locked: false,
        };
        assert_eq!(
            index_sql(&i, &mariadb(), MY),
            Err(RenderError::Unsupported {
                feature: "expression index".into(),
                min_version: "MySQL 8.0.13 (not available on MariaDB)".into(),
            })
        );
    }
}
