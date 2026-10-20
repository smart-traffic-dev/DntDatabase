//! Table DDL rebuilt from a [`TableDetail`] (PostgreSQL has no `SHOW CREATE TABLE`).
//!
//! Index convention (set by the introspector): an index backing a table constraint
//! (primary key, unique, exclusion) carries the constraint's definition
//! (`pg_get_constraintdef`, e.g. `UNIQUE (email)`); any other index carries its full
//! `CREATE INDEX` statement (`pg_get_indexdef`).

use dnt_core::meta::{ColumnDetail, ForeignKeyDetail, IndexDetail, TableDetail};
use std::fmt::Write as _;

/// Keywords that cannot be used as bare identifiers: PostgreSQL's `RESERVED_KEYWORD` and
/// `TYPE_FUNC_NAME_KEYWORD` categories (PG 12–17), sorted for binary search.
const RESERVED: &[&str] = &[
    "all",
    "analyse",
    "analyze",
    "and",
    "any",
    "array",
    "as",
    "asc",
    "asymmetric",
    "authorization",
    "binary",
    "both",
    "case",
    "cast",
    "check",
    "collate",
    "collation",
    "column",
    "concurrently",
    "constraint",
    "create",
    "cross",
    "current_catalog",
    "current_date",
    "current_role",
    "current_schema",
    "current_time",
    "current_timestamp",
    "current_user",
    "default",
    "deferrable",
    "desc",
    "distinct",
    "do",
    "else",
    "end",
    "except",
    "false",
    "fetch",
    "for",
    "foreign",
    "freeze",
    "from",
    "full",
    "grant",
    "group",
    "having",
    "ilike",
    "in",
    "initially",
    "inner",
    "intersect",
    "into",
    "is",
    "isnull",
    "join",
    "lateral",
    "leading",
    "left",
    "like",
    "limit",
    "localtime",
    "localtimestamp",
    "natural",
    "not",
    "notnull",
    "null",
    "offset",
    "on",
    "only",
    "or",
    "order",
    "outer",
    "overlaps",
    "placing",
    "primary",
    "references",
    "returning",
    "right",
    "select",
    "session_user",
    "similar",
    "some",
    "symmetric",
    "system_user",
    "table",
    "tablesample",
    "then",
    "to",
    "trailing",
    "true",
    "union",
    "unique",
    "user",
    "using",
    "variadic",
    "verbose",
    "when",
    "where",
    "window",
    "with",
];

/// Quotes a PostgreSQL identifier when needed: characters outside `[a-z0-9_]` (upper
/// case included), a leading digit, a reserved keyword or an empty name. Embedded `"`
/// are doubled.
pub fn quote_ident(name: &str) -> String {
    let plain = !name.is_empty()
        && !name.starts_with(|c: char| c.is_ascii_digit())
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
        && RESERVED.binary_search(&name).is_err();
    if plain {
        name.to_string()
    } else {
        format!("\"{}\"", name.replace('"', "\"\""))
    }
}

/// A string literal (standard-conforming: only `'` is doubled).
fn quote_literal(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

fn qualified(schema: Option<&str>, name: &str) -> String {
    match schema {
        Some(s) => format!("{}.{}", quote_ident(s), quote_ident(name)),
        None => quote_ident(name),
    }
}

fn ident_list(names: &[String]) -> String {
    names
        .iter()
        .map(|n| quote_ident(n))
        .collect::<Vec<_>>()
        .join(", ")
}

/// True when the index is a plain `CREATE INDEX` (not backing a table constraint).
fn is_create_index(i: &IndexDetail) -> bool {
    i.definition
        .as_deref()
        .is_some_and(|d| d.trim_start().starts_with("CREATE"))
}

fn column_line(c: &ColumnDetail) -> String {
    let mut line = format!("{} {}", quote_ident(&c.name), c.db_type);
    if let Some(mode) = &c.identity {
        let _ = write!(line, " GENERATED {mode} AS IDENTITY");
    } else if let Some(expr) = &c.generated {
        let _ = write!(line, " GENERATED ALWAYS AS ({expr}) STORED");
    }
    if !c.nullable {
        line.push_str(" NOT NULL");
    }
    if c.identity.is_none() && c.generated.is_none() {
        if let Some(d) = &c.default {
            let _ = write!(line, " DEFAULT {d}");
        }
    }
    line
}

fn foreign_key_line(fk: &ForeignKeyDetail, table_schema: Option<&str>) -> String {
    let ref_schema = fk.ref_schema.as_deref().or(table_schema);
    let mut line = format!(
        "CONSTRAINT {} FOREIGN KEY ({}) REFERENCES {} ({})",
        quote_ident(&fk.name),
        ident_list(&fk.columns),
        qualified(ref_schema, &fk.ref_table),
        ident_list(&fk.ref_columns)
    );
    // `NO ACTION` is the default and left implicit.
    for (clause, action) in [("ON UPDATE", &fk.on_update), ("ON DELETE", &fk.on_delete)] {
        if !action.is_empty() && action != "NO ACTION" {
            let _ = write!(line, " {clause} {action}");
        }
    }
    line
}

/// `CREATE TABLE` + indexes + comments for `t`, each statement ending with `;`:
/// 1. `CREATE TABLE` with columns, then constraints (primary key, unique / exclusion,
///    checks, foreign keys) and `PARTITION BY`;
/// 2. every index that does not back a constraint;
/// 3. `COMMENT ON TABLE` / `COMMENT ON COLUMN`.
pub fn build_table_ddl(t: &TableDetail) -> String {
    let schema = t.object.schema.as_deref();
    let table = qualified(schema, &t.object.name);

    let mut lines: Vec<String> = t.columns.iter().map(column_line).collect();
    let constraint_indexes = t
        .primary_key
        .iter()
        .chain(t.indexes.iter().filter(|i| !is_create_index(i)));
    for idx in constraint_indexes {
        if let Some(def) = &idx.definition {
            lines.push(format!("CONSTRAINT {} {def}", quote_ident(&idx.name)));
        }
    }
    // A verbatim server definition keeps every option (NOT VALID, NO INHERIT, MATCH,
    // DEFERRABLE, `SET NULL (cols)`); the rebuild from parts is only a fallback.
    for check in &t.checks {
        let body = match &check.definition {
            Some(def) => def.clone(),
            None => format!("CHECK ({})", check.expression),
        };
        lines.push(format!("CONSTRAINT {} {body}", quote_ident(&check.name)));
    }
    for fk in &t.foreign_keys {
        lines.push(match &fk.definition {
            Some(def) => format!("CONSTRAINT {} {def}", quote_ident(&fk.name)),
            None => foreign_key_line(fk, schema),
        });
    }

    let mut out = format!("CREATE TABLE {table} (\n    {}\n)", lines.join(",\n    "));
    if let Some(by) = &t.options.partitioned_by {
        let _ = write!(out, " PARTITION BY {by}");
    }
    out.push_str(";\n");

    for idx in t.indexes.iter().filter(|i| is_create_index(i)) {
        if let Some(def) = &idx.definition {
            let _ = writeln!(out, "{};", def.trim_end_matches(';'));
        }
    }

    if let Some(comment) = &t.comment {
        let _ = writeln!(
            out,
            "COMMENT ON TABLE {table} IS {};",
            quote_literal(comment)
        );
    }
    for c in &t.columns {
        if let Some(comment) = &c.comment {
            let _ = writeln!(
                out,
                "COMMENT ON COLUMN {table}.{} IS {};",
                quote_ident(&c.name),
                quote_literal(comment)
            );
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use dnt_core::meta::{
        CheckDetail, ColumnDetail, ForeignKeyDetail, IndexDetail, ObjectKind, ObjectRef,
        TableOptions,
    };

    fn column(name: &str, ordinal: u32, db_type: &str) -> ColumnDetail {
        ColumnDetail {
            name: name.into(),
            ordinal,
            db_type: db_type.into(),
            nullable: false,
            default: None,
            auto_increment: false,
            identity: None,
            generated: None,
            invisible: false,
            collation: None,
            comment: None,
        }
    }

    fn index(name: &str, unique: bool, definition: &str) -> IndexDetail {
        IndexDetail {
            name: name.into(),
            unique,
            primary: false,
            partial: false,
            columns: Vec::new(),
            expression: None,
            method: Some("btree".into()),
            definition: Some(definition.into()),
        }
    }

    fn sample() -> TableDetail {
        TableDetail {
            object: ObjectRef {
                database: "dnt_fixture".into(),
                schema: Some("public".into()),
                name: "events".into(),
                kind: ObjectKind::Table,
            },
            columns: vec![
                ColumnDetail {
                    identity: Some("ALWAYS".into()),
                    auto_increment: true,
                    ..column("id", 1, "bigint")
                },
                ColumnDetail {
                    nullable: true,
                    comment: Some("it's odd".into()),
                    ..column("we\"ird", 2, "text")
                },
                ColumnDetail {
                    default: Some("0".into()),
                    ..column("amount", 3, "numeric(12,2)")
                },
                ColumnDetail {
                    nullable: true,
                    generated: Some("round((amount * 1.1), 2)".into()),
                    ..column("amount_tax", 4, "numeric")
                },
                column("customer_id", 5, "integer"),
                column("created_at", 6, "date"),
                ColumnDetail {
                    identity: Some("BY DEFAULT".into()),
                    auto_increment: true,
                    ..column("Seq", 7, "integer")
                },
            ],
            primary_key: Some(IndexDetail {
                primary: true,
                ..index("events_pkey", true, "PRIMARY KEY (id, created_at)")
            }),
            indexes: vec![
                index(
                    "events_customer_uq",
                    true,
                    "UNIQUE (customer_id, created_at)",
                ),
                index(
                    "idx_events_amount",
                    false,
                    "CREATE INDEX idx_events_amount ON public.events USING btree (amount)",
                ),
            ],
            foreign_keys: vec![ForeignKeyDetail {
                name: "events_customer_id_fkey".into(),
                columns: vec!["customer_id".into()],
                ref_schema: Some("sales".into()),
                ref_table: "customers".into(),
                ref_columns: vec!["id".into()],
                on_update: "NO ACTION".into(),
                on_delete: "CASCADE".into(),
                definition: None,
            }],
            checks: vec![CheckDetail {
                name: "events_amount_check".into(),
                expression: "(amount >= (0)::numeric)".into(),
                definition: None,
            }],
            triggers: Vec::new(),
            options: TableOptions {
                engine: None,
                collation: None,
                tablespace: None,
                partitioned_by: Some("RANGE (created_at)".into()),
                system_versioned: false,
            },
            row_estimate: None,
            size_bytes: None,
            comment: Some("Event log".into()),
        }
    }

    #[test]
    fn builds_table_ddl() {
        insta::assert_snapshot!(build_table_ddl(&sample()));
    }

    /// Without definitions (above) FKs and checks are rebuilt from their parts; with them
    /// the server's text is used as is.
    #[test]
    fn verbatim_constraint_definitions_win() {
        let mut t = sample();
        let fk_def = "FOREIGN KEY (customer_id) REFERENCES sales.customers(id) \
                      MATCH FULL ON DELETE CASCADE DEFERRABLE INITIALLY DEFERRED";
        let check_def = "CHECK ((amount >= (0)::numeric)) NOT VALID";
        t.foreign_keys[0].definition = Some(fk_def.into());
        t.checks[0].definition = Some(check_def.into());
        let ddl = build_table_ddl(&t);
        assert!(
            ddl.contains(&format!(
                "    CONSTRAINT events_customer_id_fkey {fk_def}\n)"
            )),
            "{ddl}"
        );
        assert!(
            ddl.contains(&format!(
                "    CONSTRAINT events_amount_check {check_def},\n"
            )),
            "{ddl}"
        );
        assert_eq!(ddl.matches("FOREIGN KEY").count(), 1, "{ddl}");
        assert_eq!(ddl.matches("CHECK").count(), 1, "{ddl}");
    }

    #[test]
    fn unqualified_without_schema() {
        let mut t = sample();
        t.object.schema = None;
        t.options.partitioned_by = None;
        let ddl = build_table_ddl(&t);
        assert!(ddl.starts_with("CREATE TABLE events (\n"), "{ddl}");
        assert!(ddl.contains("\n);\n"), "{ddl}");
    }

    #[test]
    fn reserved_list_is_sorted() {
        assert!(RESERVED.windows(2).all(|w| w[0] < w[1]));
    }

    #[test]
    fn quotes_only_when_needed() {
        assert_eq!(quote_ident("orders"), "orders");
        assert_eq!(quote_ident("t_00001"), "t_00001");
        assert_eq!(quote_ident("Orders"), "\"Orders\"");
        assert_eq!(quote_ident("1st"), "\"1st\"");
        assert_eq!(quote_ident("order"), "\"order\"");
        assert_eq!(quote_ident("user"), "\"user\"");
        assert_eq!(quote_ident("we`ird\"col"), "\"we`ird\"\"col\"");
        assert_eq!(quote_ident("đơn_hàng"), "\"đơn_hàng\"");
        assert_eq!(quote_ident("a b"), "\"a b\"");
        assert_eq!(quote_ident(""), "\"\"");
    }
}
