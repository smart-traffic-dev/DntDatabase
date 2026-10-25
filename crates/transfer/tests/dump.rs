//! Dump rendering (spec §7.1, R22–R24): header, orders, the MySQL view rewrite, PostgreSQL
//! statement helpers, multi-row inserts and the writer (plain and gzip).

use std::io::{Read, Write};
use std::sync::{Arc, Mutex};

use dnt_core::server::DbKind;
use dnt_core::sql::build::ColumnTarget;
use dnt_core::sql::{leading_blank_len, split, Dialect};
use dnt_core::value::{FullValue, LogicalType, Value};
use dnt_transfer::dump::{
    add_fk_sql, create_schema_sql, end_comment, header, insert_head, mysql_view_ddl,
    session_epilogue, session_prologue, setval_column_sql, setval_sql, table_order, view_order,
    DumpHeader, DumpInserts, DumpWriter, TableNode, ViewNode, DUMP_MAGIC,
};

fn mysql_header() -> DumpHeader {
    DumpHeader {
        engine: "MySQL".into(),
        version: "8.0.36".into(),
        database: "shop".into(),
        schemas: vec![],
        created: "2026-10-25T10:11:12.000Z".into(),
        tables: 3,
        tables_with_data: 2,
        views: 1,
        sequences: None,
        types: None,
    }
}

#[test]
fn header_lines_are_exact() {
    assert_eq!(
        header(&mysql_header()),
        "-- DntDatabase dump v1\n\
         -- Source: MySQL 8.0.36, database shop\n\
         -- Created: 2026-10-25T10:11:12.000Z\n\
         -- Objects: 3 tables (2 with data), 1 views\n"
    );
    let pg = DumpHeader {
        engine: "PostgreSQL".into(),
        version: "17.0".into(),
        database: "shop".into(),
        schemas: vec!["public".into(), "sales".into()],
        tables: 4,
        tables_with_data: 4,
        views: 0,
        sequences: Some(2),
        types: Some(1),
        ..mysql_header()
    };
    assert_eq!(
        header(&pg),
        "-- DntDatabase dump v1\n\
         -- Source: PostgreSQL 17.0, database shop, schemas public, sales\n\
         -- Created: 2026-10-25T10:11:12.000Z\n\
         -- Objects: 4 tables (4 with data), 0 views, 2 sequences, 1 types\n"
    );
    assert!(header(&pg).starts_with(DUMP_MAGIC));
    assert_eq!(end_comment(12), "-- End of dump: 12 statements\n");
}

#[test]
fn session_statements_follow_r23() {
    assert_eq!(
        session_prologue(DbKind::MySql),
        [
            "SET NAMES utf8mb4",
            "SET SESSION sql_mode = 'NO_AUTO_VALUE_ON_ZERO'",
            "SET time_zone = '+00:00'",
            "SET FOREIGN_KEY_CHECKS = 0",
        ]
    );
    assert_eq!(
        session_epilogue(DbKind::MySql),
        ["SET FOREIGN_KEY_CHECKS = 1"]
    );
    assert_eq!(
        session_prologue(DbKind::Postgres),
        [
            "SET client_encoding = 'UTF8'",
            "SET standard_conforming_strings = on"
        ]
    );
    assert!(session_epilogue(DbKind::Postgres).is_empty());
}

fn node(name: &str, refs: &[&str]) -> TableNode {
    TableNode {
        schema: None,
        name: name.into(),
        references: refs.iter().map(|r| (None, (*r).to_string())).collect(),
    }
}

fn names(nodes: &[TableNode], order: &[usize]) -> Vec<String> {
    order.iter().map(|&i| nodes[i].name.clone()).collect()
}

#[test]
fn tables_follow_foreign_keys() {
    // c → b → a: parents first.
    let chain = [node("c", &["b"]), node("a", &[]), node("b", &["a"])];
    assert_eq!(names(&chain, &table_order(&chain)), ["a", "b", "c"]);
    // Independent tables by name.
    let free = [node("z", &[]), node("m", &[]), node("a", &[])];
    assert_eq!(names(&free, &table_order(&free)), ["a", "m", "z"]);
    // A self reference is not a dependency.
    let own = [node("tree", &["tree"]), node("leaf", &["tree"])];
    assert_eq!(names(&own, &table_order(&own)), ["tree", "leaf"]);
    // A cycle is broken at its first member by name, after what is free; a table that
    // needs a cycle member still waits for it.
    let cycle = [
        node("y", &["x", "base"]),
        node("x", &["y"]),
        node("base", &[]),
        node("after", &["x"]),
    ];
    assert_eq!(
        names(&cycle, &table_order(&cycle)),
        ["base", "x", "after", "y"]
    );
    // A reference to a table outside the dump is ignored.
    let outside = [node("b", &["elsewhere"]), node("a", &[])];
    assert_eq!(names(&outside, &table_order(&outside)), ["a", "b"]);
}

#[test]
fn schemas_tell_tables_apart() {
    let nodes = [
        TableNode {
            schema: Some("sales".into()),
            name: "orders".into(),
            references: vec![(Some("public".into()), "customers".into())],
        },
        TableNode {
            schema: Some("public".into()),
            name: "orders".into(),
            references: vec![],
        },
        TableNode {
            schema: Some("public".into()),
            name: "customers".into(),
            references: vec![],
        },
    ];
    let order = table_order(&nodes);
    let key = |i: usize| format!("{}.{}", nodes[i].schema.as_deref().unwrap(), nodes[i].name);
    let keys: Vec<String> = order.iter().map(|&i| key(i)).collect();
    assert_eq!(keys, ["public.customers", "public.orders", "sales.orders"]);
}

#[test]
fn views_follow_textual_references() {
    let views = [
        ViewNode {
            name: "v_top".into(),
            ddl: "CREATE VIEW `v_top` AS select * from `v_mid`".into(),
        },
        ViewNode {
            name: "v_mid".into(),
            ddl: "CREATE VIEW `v_mid` AS select * from `v_base` where x = 'v_top'".into(),
        },
        ViewNode {
            name: "v_base".into(),
            ddl: "CREATE VIEW `v_base` AS select 1 AS `a`".into(),
        },
        ViewNode {
            name: "a_free".into(),
            ddl: "CREATE VIEW `a_free` AS select 2 AS `b`".into(),
        },
    ];
    let order: Vec<&str> = view_order(&views, Dialect::MYSQL)
        .into_iter()
        .map(|i| views[i].name.as_str())
        .collect();
    assert_eq!(order, ["a_free", "v_base", "v_mid", "v_top"]);

    // PostgreSQL: bare (folded) and quoted names both count.
    let pg = [
        ViewNode {
            name: "Report".into(),
            ddl: "CREATE OR REPLACE VIEW public.\"Report\" AS\n SELECT * FROM public.base_v;"
                .into(),
        },
        ViewNode {
            name: "base_v".into(),
            ddl: "CREATE OR REPLACE VIEW public.base_v AS\n SELECT 1 AS one;".into(),
        },
    ];
    let order: Vec<&str> = view_order(&pg, Dialect::Postgres)
        .into_iter()
        .map(|i| pg[i].name.as_str())
        .collect();
    assert_eq!(order, ["base_v", "Report"]);
}

#[test]
fn mysql_view_loses_definer_and_source_database() {
    let ddl = "CREATE ALGORITHM=UNDEFINED DEFINER=`root`@`%` SQL SECURITY DEFINER VIEW `v` AS \
               select `dnt_fixture`.`c`.`id` AS `id` from `dnt_fixture`.`c`";
    assert_eq!(
        mysql_view_ddl(ddl, "dnt_fixture"),
        "CREATE ALGORITHM=UNDEFINED SQL SECURITY DEFINER VIEW `v` AS \
         select `c`.`id` AS `id` from `c`"
    );
    // A string holding the qualifier is data: untouched. Another database's qualifier stays.
    let ddl = "CREATE DEFINER=`app`@`localhost` VIEW `w` AS select '`dnt_fixture`.x' AS `s`, \
               `other`.`t`.`a` AS `a` from (`dnt_fixture`.`t` join `other`.`t`)";
    assert_eq!(
        mysql_view_ddl(ddl, "dnt_fixture"),
        "CREATE VIEW `w` AS select '`dnt_fixture`.x' AS `s`, \
         `other`.`t`.`a` AS `a` from (`t` join `other`.`t`)"
    );
    // DEFINER = CURRENT_USER, and a database name holding a backquote.
    let ddl = "CREATE DEFINER=CURRENT_USER VIEW `v` AS select `we``ird`.`t`.`a` from `we``ird`.`t`";
    assert_eq!(
        mysql_view_ddl(ddl, "we`ird"),
        "CREATE VIEW `v` AS select `t`.`a` from `t`"
    );
}

#[test]
fn postgres_statement_helpers() {
    assert_eq!(
        create_schema_sql("Sales"),
        "CREATE SCHEMA IF NOT EXISTS \"Sales\""
    );
    assert_eq!(
        setval_column_sql("\"public\".\"t\"", "id", "42", "TRUE"),
        "SELECT pg_catalog.setval(pg_catalog.pg_get_serial_sequence('\"public\".\"t\"', 'id'), 42, TRUE)"
    );
    assert_eq!(
        setval_column_sql("\"o'k\".\"t\"", "it's", "1", "FALSE"),
        "SELECT pg_catalog.setval(pg_catalog.pg_get_serial_sequence('\"o''k\".\"t\"', 'it''s'), 1, FALSE)"
    );
    assert_eq!(
        setval_sql("\"public\".\"seq\"", "7", "FALSE"),
        "SELECT pg_catalog.setval('\"public\".\"seq\"', 7, FALSE)"
    );
    assert_eq!(
        add_fk_sql(
            "\"public\".\"orders\"",
            "orders_customer_fk",
            "FOREIGN KEY (customer_id) REFERENCES public.customers(id)"
        ),
        "ALTER TABLE \"public\".\"orders\" ADD CONSTRAINT \"orders_customer_fk\" \
         FOREIGN KEY (customer_id) REFERENCES public.customers(id)"
    );
}

fn col(name: &str, db_type: &str, logical: LogicalType) -> ColumnTarget {
    ColumnTarget {
        name: name.into(),
        db_type: db_type.into(),
        logical,
        collation: None,
    }
}

fn int(n: i64) -> FullValue {
    FullValue::try_from_value(Value::Int(n)).unwrap()
}

fn text(s: &str) -> FullValue {
    FullValue::try_from_value(Value::Text(s.into())).unwrap()
}

#[test]
fn inserts_are_batched_by_rows_and_bytes() {
    let cols = vec![
        col("id", "int", LogicalType::Integer),
        col("name", "varchar(20)", LogicalType::Text),
    ];
    let head = insert_head(DbKind::MySql, "`t`", &cols, false);
    assert_eq!(head, "INSERT INTO `t` (`id`, `name`) VALUES ");
    let mut ins = DumpInserts::new(head, cols.clone(), Dialect::MYSQL, 2, 1024 * 1024);
    let mut out = Vec::new();
    for i in 1..=5 {
        out.extend(ins.push(&[int(i), text("a'b")]).unwrap());
    }
    out.extend(ins.flush());
    assert_eq!(
        out,
        [
            "INSERT INTO `t` (`id`, `name`) VALUES (1, _utf8mb4'a\\'b'), (2, _utf8mb4'a\\'b')",
            "INSERT INTO `t` (`id`, `name`) VALUES (3, _utf8mb4'a\\'b'), (4, _utf8mb4'a\\'b')",
            "INSERT INTO `t` (`id`, `name`) VALUES (5, _utf8mb4'a\\'b')",
        ]
    );
    // Bytes: a row that does not fit starts a new statement; a lone big row still goes.
    let mut ins = DumpInserts::new(
        insert_head(DbKind::MySql, "`t`", &cols, false),
        cols.clone(),
        Dialect::MYSQL,
        1000,
        60,
    );
    let mut out = Vec::new();
    out.extend(ins.push(&[int(1), text("x")]).unwrap());
    out.extend(ins.push(&[int(2), text(&"y".repeat(100))]).unwrap());
    out.extend(ins.push(&[int(3), text("z")]).unwrap());
    out.extend(ins.flush());
    assert_eq!(out.len(), 3, "{out:?}");
    assert!(ins.flush().is_none());

    // PostgreSQL: arrays through dump literals, OVERRIDING SYSTEM VALUE in the head.
    let cols = vec![
        col("id", "integer", LogicalType::Integer),
        col("tags", "text[]", LogicalType::Array),
    ];
    let head = insert_head(DbKind::Postgres, "\"public\".\"t\"", &cols, true);
    assert_eq!(
        head,
        "INSERT INTO \"public\".\"t\" (\"id\", \"tags\") OVERRIDING SYSTEM VALUE VALUES "
    );
    let mut ins = DumpInserts::new(head, cols, Dialect::Postgres, 1000, 1024 * 1024);
    let tags =
        FullValue::try_from_value(Value::Array(vec![Value::Text("a b".into()), Value::Null]))
            .unwrap();
    assert!(ins.push(&[int(1), tags]).unwrap().is_none());
    assert_eq!(
        ins.flush().unwrap(),
        "INSERT INTO \"public\".\"t\" (\"id\", \"tags\") OVERRIDING SYSTEM VALUE VALUES \
         (1, '{\"a b\",NULL}'::text[])"
    );
}

/// A `Write` whose bytes stay readable after the writer is dropped.
#[derive(Clone, Default)]
struct Shared(Arc<Mutex<Vec<u8>>>);

impl Write for Shared {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Shared {
    fn bytes(&self) -> Vec<u8> {
        self.0.lock().unwrap().clone()
    }
}

/// A small dump with tricky statements; returns the statements it wrote.
fn write_sample(w: &mut DumpWriter) -> Vec<String> {
    w.raw(&header(&mysql_header())).unwrap();
    let mut stmts: Vec<String> = Vec::new();
    for s in session_prologue(DbKind::MySql) {
        w.statement(s).unwrap();
        stmts.push(s.to_string());
    }
    w.comment("Table `t`").unwrap();
    let ddl = "CREATE TABLE `t` (\n  `id` int NOT NULL COMMENT 'semi; colon',\n  \
               `s` text\n) ENGINE=InnoDB /*!50100 COMMENT 'x;y' */";
    w.statement(ddl).unwrap();
    stmts.push(ddl.to_string());
    let ins = "INSERT INTO `t` (`id`, `s`) VALUES (1, _utf8mb4'a;\\'b -- c'), (2, NULL)";
    w.statement(ins).unwrap();
    stmts.push(ins.to_string());
    // A script of several statements is written one by one.
    let n = w
        .script("CREATE VIEW `v` AS select 1;\n", Dialect::MYSQL)
        .unwrap();
    assert_eq!(n, 1);
    stmts.push("CREATE VIEW `v` AS select 1".into());
    for s in session_epilogue(DbKind::MySql) {
        w.statement(s).unwrap();
        stmts.push(s.to_string());
    }
    let count = w.statements();
    w.raw(&end_comment(count)).unwrap();
    stmts
}

#[test]
fn every_statement_ends_with_a_semicolon_and_splits_back() {
    let buf = Shared::default();
    let mut w = DumpWriter::new(Box::new(buf.clone()), false);
    let stmts = write_sample(&mut w);
    assert_eq!(w.statements(), stmts.len() as u64);
    w.finish().unwrap();
    let out = String::from_utf8(buf.bytes()).unwrap();
    assert!(out.starts_with(DUMP_MAGIC), "{out}");
    assert!(
        out.ends_with(&format!("-- End of dump: {} statements\n", stmts.len())),
        "{out}"
    );
    for s in &stmts {
        assert!(out.contains(&format!("{s};\n")), "{s}");
    }
    let back: Vec<String> = split(&out, Dialect::MYSQL)
        .into_iter()
        .map(|s| s.text[leading_blank_len(&s.text, Dialect::MYSQL)..].to_string())
        .collect();
    assert_eq!(back, stmts);
}

#[test]
fn statement_text_is_normalised() {
    let buf = Shared::default();
    let mut w = DumpWriter::new(Box::new(buf.clone()), false);
    w.statement("  SELECT 1;  \n").unwrap();
    w.statement("SELECT 2").unwrap();
    // Nothing but whitespace (or a lone delimiter) is not a statement.
    w.statement(" ; ").unwrap();
    assert_eq!(w.statements(), 2);
    w.finish().unwrap();
    assert_eq!(
        String::from_utf8(buf.bytes()).unwrap(),
        "SELECT 1;\nSELECT 2;\n"
    );
}

#[test]
fn gzip_output_equals_the_plain_output() {
    let plain = Shared::default();
    let mut w = DumpWriter::new(Box::new(plain.clone()), false);
    write_sample(&mut w);
    w.finish().unwrap();

    let gz = Shared::default();
    let mut w = DumpWriter::new(Box::new(gz.clone()), true);
    write_sample(&mut w);
    let written = w.finish().unwrap();
    let bytes = gz.bytes();
    assert_eq!(&bytes[..2], &[0x1f, 0x8b], "gzip magic");
    assert_eq!(written, plain.bytes().len() as u64, "uncompressed bytes");
    let mut decoded = Vec::new();
    flate2::read::GzDecoder::new(&bytes[..])
        .read_to_end(&mut decoded)
        .unwrap();
    assert_eq!(decoded, plain.bytes());
}
