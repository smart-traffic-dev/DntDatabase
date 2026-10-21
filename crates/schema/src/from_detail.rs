//! A draft built from introspection, and the draft of a new table.

use crate::model::*;
use crate::types::parse_type;
use dnt_core::meta::{
    CheckDetail, ColumnDetail, ForeignKeyDetail, IdentityKind, IndexDetail, IndexPart, TableDetail,
    TableOptions,
};
use dnt_core::server::{DbKind, ServerInfo, ServerVariant};

/// The draft of an existing table. Every element's `original` is its current name.
///
/// Locked here (the designer cannot represent them faithfully): generated and
/// invisible columns; a primary key with a prefix, descending or expression part or
/// `INCLUDE` columns; indexes that are partial, `NULLS NOT DISTINCT` or have an
/// expression part; `NOT VALID` / `NO INHERIT` checks. The whole table is read-only
/// (`read_only_reason`) when partitioned, system-versioned, a PostgreSQL partition or
/// an inheriting table. Task 9's fidelity check locks whatever else differs from the
/// server's DDL.
pub fn from_detail(detail: &TableDetail, server: &ServerInfo) -> TableDef {
    let opts = &detail.options;
    TableDef {
        original_name: Some(detail.object.name.clone()),
        name: detail.object.name.clone(),
        columns: detail
            .columns
            .iter()
            .map(|c| column(c, opts, server))
            .collect(),
        primary_key: detail.primary_key.as_ref().map(primary_key),
        indexes: detail.indexes.iter().map(index).collect(),
        foreign_keys: detail.foreign_keys.iter().map(foreign_key).collect(),
        checks: detail.checks.iter().map(check).collect(),
        options: TableOptionsDef {
            engine: opts.engine.clone(),
            charset: opts.charset.clone(),
            collation: opts.collation.clone(),
            row_format: opts.row_format.clone(),
            locked: false,
        },
        comment: detail.comment.clone(),
        read_only_reason: read_only_reason(opts),
    }
}

fn read_only_reason(o: &TableOptions) -> Option<String> {
    let reason = if o.partitioned_by.is_some() {
        "Partitioned tables cannot be edited in the designer."
    } else if o.system_versioned {
        "System-versioned tables cannot be edited in the designer."
    } else if o.is_partition {
        "Partitions cannot be edited in the designer; edit the partitioned table instead."
    } else if !o.inherits.is_empty() {
        "Tables that inherit from another table cannot be edited in the designer."
    } else {
        return None;
    };
    Some(reason.into())
}

/// `v` unless it equals the table-level `table` value.
fn unless_table(v: &Option<String>, table: &Option<String>) -> Option<String> {
    match (v, table) {
        (Some(v), Some(t)) if v.eq_ignore_ascii_case(t) => None,
        _ => v.clone(),
    }
}

fn column(c: &ColumnDetail, opts: &TableOptions, server: &ServerInfo) -> ColumnDef {
    let mut data_type = parse_type(&c.db_type, server);
    // The driver already unescaped enum / set members: they are authoritative.
    if let TypeSpec::Known { values, .. } = &mut data_type {
        if !values.is_empty() && !c.enum_values.is_empty() {
            *values = c.enum_values.clone();
        }
    }
    let generated = c.generated.clone();
    ColumnDef {
        original: Some(c.name.clone()),
        name: c.name.clone(),
        data_type,
        nullable: c.nullable,
        default: if generated.is_some() {
            None
        } else {
            c.default.as_deref().and_then(|d| default_spec(d, server))
        },
        auto_increment: c.auto_increment,
        identity: c.identity_kind,
        on_update: c.on_update.clone(),
        charset: unless_table(&c.charset, &opts.charset),
        collation: unless_table(&c.collation, &opts.collation),
        comment: c.comment.clone(),
        using: None,
        locked: generated.is_some() || c.invisible,
        generated,
    }
}

/// A server-reported default as a value or an expression; `None` for an explicit
/// `NULL` default.
fn default_spec(d: &str, server: &ServerInfo) -> Option<DefaultSpec> {
    let value = |text: String| Some(DefaultSpec::Value { text });
    let expression = || {
        Some(DefaultSpec::Expression {
            text: d.to_string(),
        })
    };
    if is_number(d) {
        return value(d.to_string());
    }
    match (server.kind, server.variant) {
        // `COLUMN_DEFAULT` holds a literal unquoted; expressions are spelled as SQL.
        (DbKind::MySql, ServerVariant::MySql57 | ServerVariant::MySql8) => {
            if is_mysql_default_expression(d) {
                expression()
            } else {
                value(d.to_string())
            }
        }
        // MariaDB quotes string literals; anything unquoted is an expression.
        (DbKind::MySql, _) => match unquote(d) {
            Some((text, "")) => value(text),
            _ => expression(),
        },
        // `pg_get_expr`: `'text'::type`, `NULL::type`, numbers, `true`, functions.
        (DbKind::Postgres, _) => {
            if let Some((text, rest)) = unquote(d) {
                if rest.is_empty() || is_cast_suffix(rest) {
                    return value(text);
                }
            }
            let lower = d.to_ascii_lowercase();
            if lower == "null" || (lower.starts_with("null::") && is_cast_suffix(&d[4..])) {
                return None;
            }
            if lower == "true" || lower == "false" {
                return value(lower);
            }
            expression()
        }
    }
}

fn is_number(s: &str) -> bool {
    let digits = s.strip_prefix('-').unwrap_or(s);
    let mut parts = digits.splitn(2, '.');
    let int = parts.next().unwrap_or("");
    let frac = parts.next();
    !int.is_empty()
        && int.bytes().all(|b| b.is_ascii_digit())
        && frac.is_none_or(|f| !f.is_empty() && f.bytes().all(|b| b.is_ascii_digit()))
}

/// MySQL 5.7/8 report these unquoted in `COLUMN_DEFAULT` as SQL, not as a value.
fn is_mysql_default_expression(d: &str) -> bool {
    let upper = d.to_ascii_uppercase();
    let name = upper.split('(').next().unwrap_or("").trim();
    matches!(
        name,
        "CURRENT_TIMESTAMP" | "NOW" | "LOCALTIME" | "LOCALTIMESTAMP" | "CURRENT_DATE"
            | "CURRENT_TIME"
    ) || upper.starts_with("B'")
        || upper.starts_with("X'")
        || upper.starts_with("0X")
        || upper.starts_with("0B")
        // MySQL 8 expression defaults (`DEFAULT (expr)`) contain a call or operator.
        || (d.contains('(') && d.ends_with(')'))
}

/// A leading `'…'` literal (`''` unescaped) and the text after it.
fn unquote(s: &str) -> Option<(String, &str)> {
    let rest = s.strip_prefix('\'')?;
    let mut out = String::new();
    let mut chars = rest.char_indices().peekable();
    while let Some((i, c)) = chars.next() {
        if c == '\'' {
            if chars.next_if(|&(_, c)| c == '\'').is_some() {
                out.push('\'');
                continue;
            }
            return Some((out, &rest[i + 1..]));
        }
        out.push(c);
    }
    None
}

/// `::type` (possibly with spaces, parameters, `[]` or a qualified name), nothing else.
fn is_cast_suffix(s: &str) -> bool {
    s.strip_prefix("::").is_some_and(|t| {
        !t.is_empty()
            && !t.contains("::")
            && t.chars()
                .all(|c| c.is_alphanumeric() || " _.,()[]\"".contains(c))
    })
}

fn part(p: &IndexPart) -> IndexPartDef {
    IndexPartDef {
        column: p.column.clone(),
        expression: p.expression.clone(),
        prefix: p.prefix,
        descending: p.descending,
    }
}

/// The index's parts, or one part per listed column when the driver gave none.
fn parts(i: &IndexDetail) -> Vec<IndexPartDef> {
    if i.parts.is_empty() {
        i.columns
            .iter()
            .map(|c| IndexPartDef {
                column: Some(c.clone()),
                expression: None,
                prefix: None,
                descending: false,
            })
            .collect()
    } else {
        i.parts.iter().map(part).collect()
    }
}

fn primary_key(i: &IndexDetail) -> KeyDef {
    let plain = parts(i).iter().all(|p| {
        p.column.is_some() && p.expression.is_none() && p.prefix.is_none() && !p.descending
    });
    KeyDef {
        original: Some(i.name.clone()),
        name: Some(i.name.clone()),
        columns: i.columns.clone(),
        locked: !plain || !i.include.is_empty() || i.expression.is_some(),
    }
}

fn index(i: &IndexDetail) -> IndexDef {
    let parts = parts(i);
    let constraint = i
        .definition
        .as_deref()
        .is_some_and(|d| d.starts_with("UNIQUE (") || d.starts_with("PRIMARY KEY ("));
    let locked = i.partial
        || i.nulls_not_distinct
        || i.expression.is_some()
        || parts.iter().any(|p| p.expression.is_some());
    IndexDef {
        original: Some(i.name.clone()),
        name: i.name.clone(),
        unique: i.unique,
        parts,
        method: i.method.clone(),
        include: i.include.clone(),
        visible: i.visible,
        comment: i.comment.clone(),
        constraint,
        locked,
    }
}

fn foreign_key(f: &ForeignKeyDetail) -> FkDef {
    FkDef {
        original: Some(f.name.clone()),
        name: f.name.clone(),
        columns: f.columns.clone(),
        ref_schema: f.ref_schema.clone(),
        ref_table: f.ref_table.clone(),
        ref_columns: f.ref_columns.clone(),
        on_update: f.on_update.clone(),
        on_delete: f.on_delete.clone(),
        match_type: f.match_type.clone(),
        deferrable: f.deferrable,
        initially_deferred: f.initially_deferred,
        locked: false,
    }
}

fn check(c: &CheckDetail) -> CheckDef {
    let def = c.definition.as_deref().unwrap_or("").to_ascii_uppercase();
    CheckDef {
        original: Some(c.name.clone()),
        name: c.name.clone(),
        expression: c.expression.clone(),
        locked: def.contains("NOT VALID") || def.contains("NO INHERIT"),
    }
}

/// The draft of a new table: one `id` column (MySQL `bigint unsigned AUTO_INCREMENT`,
/// PostgreSQL `bigint GENERATED BY DEFAULT AS IDENTITY`) as the primary key.
pub fn new_table(name: &str, server: &ServerInfo) -> TableDef {
    let mysql = server.kind == DbKind::MySql;
    let id = ColumnDef {
        original: None,
        name: "id".into(),
        data_type: TypeSpec::Known {
            base: "bigint".into(),
            length: None,
            precision: None,
            scale: None,
            unsigned: mysql,
            values: Vec::new(),
        },
        nullable: false,
        default: None,
        auto_increment: mysql,
        identity: (!mysql).then_some(IdentityKind::ByDefault),
        on_update: None,
        charset: None,
        collation: None,
        comment: None,
        using: None,
        generated: None,
        locked: false,
    };
    TableDef {
        original_name: None,
        name: name.into(),
        columns: vec![id],
        primary_key: Some(KeyDef {
            original: None,
            name: None,
            columns: vec!["id".into()],
            locked: false,
        }),
        indexes: Vec::new(),
        foreign_keys: Vec::new(),
        checks: Vec::new(),
        options: TableOptionsDef::default(),
        comment: None,
        read_only_reason: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::*;
    use dnt_core::meta::{
        CheckDetail, ColumnDetail, ForeignKeyDetail, IdentityKind, IndexDetail, IndexPart,
        ObjectKind, ObjectRef, TableOptions,
    };

    fn column(name: &str, ordinal: u32, db_type: &str) -> ColumnDetail {
        ColumnDetail {
            name: name.into(),
            ordinal,
            db_type: db_type.into(),
            nullable: true,
            ..Default::default()
        }
    }

    fn part(column: &str) -> IndexPart {
        IndexPart {
            column: Some(column.into()),
            ..Default::default()
        }
    }

    fn detail(columns: Vec<ColumnDetail>) -> TableDetail {
        TableDetail {
            object: ObjectRef {
                database: "shop".into(),
                schema: None,
                name: "orders".into(),
                kind: ObjectKind::Table,
            },
            columns,
            primary_key: None,
            indexes: vec![],
            foreign_keys: vec![],
            checks: vec![],
            triggers: vec![],
            options: TableOptions::default(),
            row_estimate: None,
            size_bytes: None,
            comment: None,
        }
    }

    fn mysql_orders() -> TableDetail {
        let mut d = detail(vec![
            ColumnDetail {
                nullable: false,
                auto_increment: true,
                unsigned: true,
                ..column("id", 1, "bigint unsigned")
            },
            ColumnDetail {
                default: Some("new".into()),
                charset: Some("utf8mb4".into()),
                collation: Some("utf8mb4_unicode_ci".into()),
                enum_values: vec!["new".into(), "paid".into()],
                ..column("status", 2, "enum('new','paid')")
            },
            ColumnDetail {
                charset: Some("utf8mb4".into()),
                collation: Some("utf8mb4_bin".into()),
                comment: Some("note".into()),
                ..column("code", 3, "varchar(16)")
            },
            ColumnDetail {
                generated: Some("(`id` * 2)".into()),
                ..column("twice", 4, "bigint")
            },
            ColumnDetail {
                default: Some("CURRENT_TIMESTAMP".into()),
                on_update: Some("CURRENT_TIMESTAMP".into()),
                ..column("updated_at", 5, "timestamp")
            },
            ColumnDetail {
                default: Some("0.00".into()),
                ..column("total", 6, "decimal(12,2)")
            },
        ]);
        d.primary_key = Some(IndexDetail {
            name: "PRIMARY".into(),
            unique: true,
            primary: true,
            columns: vec!["id".into()],
            parts: vec![part("id")],
            method: Some("BTREE".into()),
            ..Default::default()
        });
        d.indexes = vec![
            IndexDetail {
                name: "code_uq".into(),
                unique: true,
                columns: vec!["code".into()],
                parts: vec![IndexPart {
                    prefix: Some(8),
                    ..part("code")
                }],
                method: Some("BTREE".into()),
                comment: Some("pfx".into()),
                ..Default::default()
            },
            IndexDetail {
                name: "lower_code".into(),
                parts: vec![IndexPart {
                    expression: Some("lower(`code`)".into()),
                    ..Default::default()
                }],
                ..Default::default()
            },
        ];
        d.foreign_keys = vec![ForeignKeyDetail {
            name: "fk_c".into(),
            columns: vec!["id".into()],
            ref_schema: Some("shop".into()),
            ref_table: "customers".into(),
            ref_columns: vec!["id".into()],
            on_update: "RESTRICT".into(),
            on_delete: "CASCADE".into(),
            ..Default::default()
        }];
        d.checks = vec![CheckDetail {
            name: "total_pos".into(),
            expression: "(`total` >= 0)".into(),
            definition: None,
        }];
        d.options = TableOptions {
            engine: Some("InnoDB".into()),
            charset: Some("utf8mb4".into()),
            collation: Some("utf8mb4_unicode_ci".into()),
            row_format: Some("Dynamic".into()),
            ..Default::default()
        };
        d.comment = Some("Orders".into());
        d
    }

    #[test]
    fn from_detail_marks_originals_and_locks() {
        let my = mysql8();
        let def = from_detail(&mysql_orders(), &my);
        assert_eq!(def.original_name.as_deref(), Some("orders"));
        assert_eq!(def.name, "orders");
        assert_eq!(def.read_only_reason, None);
        assert_eq!(def.comment.as_deref(), Some("Orders"));
        for c in &def.columns {
            assert_eq!(c.original.as_deref(), Some(c.name.as_str()));
        }
        let col = |n: &str| def.columns.iter().find(|c| c.name == n).unwrap();
        // Generated columns are locked.
        assert!(col("twice").locked);
        assert_eq!(col("twice").generated.as_deref(), Some("(`id` * 2)"));
        assert!(!col("id").locked);
        assert!(col("id").auto_increment);
        assert!(!col("id").nullable);
        assert_eq!(
            col("status").default,
            Some(DefaultSpec::Value { text: "new".into() })
        );
        assert_eq!(
            col("updated_at").default,
            Some(DefaultSpec::Expression {
                text: "CURRENT_TIMESTAMP".into()
            })
        );
        assert_eq!(
            col("updated_at").on_update.as_deref(),
            Some("CURRENT_TIMESTAMP")
        );
        assert_eq!(
            col("total").default,
            Some(DefaultSpec::Value {
                text: "0.00".into()
            })
        );
        // Charset / collation equal to the table's are dropped (T5-2).
        assert_eq!(col("status").charset, None);
        assert_eq!(col("status").collation, None);
        assert_eq!(col("code").charset, None);
        assert_eq!(col("code").collation.as_deref(), Some("utf8mb4_bin"));
        assert_eq!(col("code").comment.as_deref(), Some("note"));

        let pk = def.primary_key.as_ref().unwrap();
        assert_eq!(pk.original.as_deref(), Some("PRIMARY"));
        assert_eq!(pk.columns, ["id"]);
        assert!(!pk.locked);

        let code_uq = &def.indexes[0];
        assert_eq!(code_uq.original.as_deref(), Some("code_uq"));
        assert!(code_uq.unique && !code_uq.locked && !code_uq.constraint);
        assert_eq!(code_uq.parts[0].prefix, Some(8));
        assert_eq!(code_uq.comment.as_deref(), Some("pfx"));
        // Expression index parts are locked.
        assert!(def.indexes[1].locked);

        let fk = &def.foreign_keys[0];
        assert_eq!(fk.original.as_deref(), Some("fk_c"));
        assert_eq!(fk.on_delete, "CASCADE");
        assert_eq!(fk.match_type, None);
        assert!(!fk.locked);
        assert_eq!(def.checks[0].original.as_deref(), Some("total_pos"));
        assert_eq!(
            def.options,
            TableOptionsDef {
                engine: Some("InnoDB".into()),
                charset: Some("utf8mb4".into()),
                collation: Some("utf8mb4_unicode_ci".into()),
                row_format: Some("Dynamic".into()),
                locked: false,
            }
        );

        // A partitioned table opens read-only.
        let mut partitioned = mysql_orders();
        partitioned.options.partitioned_by = Some("RANGE (id)".into());
        assert!(from_detail(&partitioned, &my).read_only_reason.is_some());
        let mut versioned = mysql_orders();
        versioned.options.system_versioned = true;
        assert!(from_detail(&versioned, &mariadb())
            .read_only_reason
            .is_some());
    }

    #[test]
    fn from_detail_postgres() {
        let pg = pg();
        let mut d = detail(vec![
            ColumnDetail {
                nullable: false,
                identity_kind: Some(IdentityKind::Always),
                identity: Some("ALWAYS".into()),
                ..column("id", 1, "bigint")
            },
            ColumnDetail {
                default: Some("'new'::order_status".into()),
                ..column("status", 2, "order_status")
            },
            ColumnDetail {
                default: Some("'it''s'::character varying".into()),
                ..column("label", 3, "character varying(20)")
            },
            ColumnDetail {
                default: Some("now()".into()),
                ..column("created", 4, "timestamp with time zone")
            },
            ColumnDetail {
                default: Some("NULL::text".into()),
                ..column("note", 5, "text")
            },
            ColumnDetail {
                invisible: true,
                ..column("hidden", 6, "integer")
            },
        ]);
        d.object.schema = Some("public".into());
        d.primary_key = Some(IndexDetail {
            name: "orders_pkey".into(),
            unique: true,
            primary: true,
            columns: vec!["id".into()],
            parts: vec![part("id")],
            method: Some("btree".into()),
            definition: Some("PRIMARY KEY (id)".into()),
            ..Default::default()
        });
        d.indexes = vec![
            IndexDetail {
                name: "orders_label_key".into(),
                unique: true,
                columns: vec!["label".into()],
                parts: vec![part("label")],
                method: Some("btree".into()),
                definition: Some("UNIQUE (label)".into()),
                ..Default::default()
            },
            IndexDetail {
                name: "orders_partial".into(),
                partial: true,
                columns: vec!["status".into()],
                parts: vec![part("status")],
                method: Some("btree".into()),
                definition: Some(
                    "CREATE INDEX orders_partial ON public.orders USING btree (status) WHERE (id > 0)"
                        .into(),
                ),
                ..Default::default()
            },
            IndexDetail {
                name: "orders_nnd".into(),
                unique: true,
                nulls_not_distinct: true,
                columns: vec!["label".into()],
                parts: vec![part("label")],
                ..Default::default()
            },
            IndexDetail {
                name: "orders_inc".into(),
                columns: vec!["status".into()],
                parts: vec![part("status")],
                include: vec!["label".into()],
                ..Default::default()
            },
        ];
        d.foreign_keys = vec![ForeignKeyDetail {
            name: "fk_full".into(),
            columns: vec!["id".into()],
            ref_schema: Some("public".into()),
            ref_table: "customers".into(),
            ref_columns: vec!["id".into()],
            on_update: "NO ACTION".into(),
            on_delete: "NO ACTION".into(),
            match_type: Some("FULL".into()),
            deferrable: true,
            initially_deferred: true,
            ..Default::default()
        }];
        let def = from_detail(&d, &pg);
        let col = |n: &str| def.columns.iter().find(|c| c.name == n).unwrap();
        assert_eq!(col("id").identity, Some(IdentityKind::Always));
        assert_eq!(
            col("status").default,
            Some(DefaultSpec::Value { text: "new".into() })
        );
        assert_eq!(
            col("status").data_type,
            TypeSpec::Custom {
                text: "order_status".into()
            }
        );
        assert_eq!(
            col("label").default,
            Some(DefaultSpec::Value {
                text: "it's".into()
            })
        );
        assert_eq!(
            col("created").default,
            Some(DefaultSpec::Expression {
                text: "now()".into()
            })
        );
        assert_eq!(col("note").default, None);
        // Invisible columns are locked.
        assert!(col("hidden").locked);
        assert_eq!(
            def.primary_key.as_ref().unwrap().name.as_deref(),
            Some("orders_pkey")
        );
        let idx = |n: &str| def.indexes.iter().find(|i| i.name == n).unwrap();
        assert!(idx("orders_label_key").constraint);
        assert!(!idx("orders_label_key").locked);
        // Partial and NULLS NOT DISTINCT indexes are locked.
        assert!(idx("orders_partial").locked);
        assert!(!idx("orders_partial").constraint);
        assert!(idx("orders_nnd").locked);
        assert_eq!(idx("orders_inc").include, ["label"]);
        assert!(!idx("orders_inc").locked);
        let fk = &def.foreign_keys[0];
        assert_eq!(fk.match_type.as_deref(), Some("FULL"));
        assert!(fk.deferrable && fk.initially_deferred);

        let mut child = d.clone();
        child.options.is_partition = true;
        assert!(from_detail(&child, &pg).read_only_reason.is_some());
        let mut heir = d.clone();
        heir.options.inherits = vec!["public.base".into()];
        assert!(from_detail(&heir, &pg).read_only_reason.is_some());
    }

    #[test]
    fn mariadb_defaults_are_quoted() {
        let m = mariadb();
        let d = detail(vec![
            ColumnDetail {
                default: Some("'it''s'".into()),
                ..column("a", 1, "varchar(10)")
            },
            ColumnDetail {
                default: Some("current_timestamp()".into()),
                ..column("b", 2, "timestamp")
            },
            ColumnDetail {
                default: Some("5".into()),
                ..column("c", 3, "int(11)")
            },
        ]);
        let def = from_detail(&d, &m);
        assert_eq!(
            def.columns[0].default,
            Some(DefaultSpec::Value {
                text: "it's".into()
            })
        );
        assert_eq!(
            def.columns[1].default,
            Some(DefaultSpec::Expression {
                text: "current_timestamp()".into()
            })
        );
        assert_eq!(
            def.columns[2].default,
            Some(DefaultSpec::Value { text: "5".into() })
        );
    }

    #[test]
    fn new_table_has_an_id_key() {
        let my = new_table("t", &mysql8());
        assert_eq!(my.original_name, None);
        assert_eq!(my.name, "t");
        assert_eq!(my.columns.len(), 1);
        let id = &my.columns[0];
        assert_eq!(id.original, None);
        assert_eq!(id.name, "id");
        assert_eq!(
            id.data_type,
            TypeSpec::Known {
                base: "bigint".into(),
                length: None,
                precision: None,
                scale: None,
                unsigned: true,
                values: vec![],
            }
        );
        assert!(id.auto_increment && !id.nullable && id.identity.is_none());
        assert_eq!(my.primary_key.as_ref().unwrap().columns, ["id"]);
        assert!(crate::validate(&my, &mysql8()).is_empty());

        let p = new_table("t", &pg());
        let id = &p.columns[0];
        assert_eq!(id.identity, Some(IdentityKind::ByDefault));
        assert!(!id.auto_increment);
        assert_eq!(
            id.data_type,
            TypeSpec::Known {
                base: "bigint".into(),
                length: None,
                precision: None,
                scale: None,
                unsigned: false,
                values: vec![],
            }
        );
        assert!(crate::validate(&p, &pg()).is_empty());
    }
}
