use super::*;
use flate2::write::GzEncoder;
use flate2::Compression;
use std::io::Write;

const MY: Dialect = Dialect::MySql(MySqlMode {
    ansi_quotes: false,
    no_backslash_escapes: false,
});
const PG: Dialect = Dialect::Postgres;

fn target(kind: DbKind, db: &str) -> ScanTarget {
    ScanTarget {
        kind,
        database: db.into(),
        dialect: match kind {
            DbKind::MySql => MY,
            DbKind::Postgres => PG,
        },
    }
}

fn scan_text(text: &str, kind: DbKind, db: &str) -> ScanResult {
    scan(text.as_bytes(), &target(kind, db)).unwrap()
}

fn gzip(bytes: &[u8]) -> Vec<u8> {
    let mut e = GzEncoder::new(Vec::new(), Compression::default());
    e.write_all(bytes).unwrap();
    e.finish().unwrap()
}

fn tar_header(name: &str) -> Vec<u8> {
    let mut h = vec![0u8; 1024];
    h[..name.len()].copy_from_slice(name.as_bytes());
    h[257..263].copy_from_slice(b"ustar\0");
    h
}

#[test]
fn detect_gzip_magic() {
    assert!(is_gzip(&gzip(b"SELECT 1;")));
    assert!(!is_gzip(b"SELECT 1;"));
    assert!(!is_gzip(b""));
}

#[test]
fn detect_pgdmp_magic_is_pg_restore() {
    assert_eq!(container(b"PGDMP\x01\x0e\x00"), Container::PgCustom);
    assert_eq!(container(b"-- PGDMP in a comment"), Container::Text);
}

#[test]
fn detect_tar_with_toc_dat_is_pg_restore() {
    assert_eq!(container(&tar_header("toc.dat")), Container::PgTar);
    assert_eq!(container(&tar_header("other.dat")), Container::Text);
    let mut not_ustar = tar_header("toc.dat");
    not_ustar[257..262].copy_from_slice(b"xxxxx");
    assert_eq!(container(&not_ustar), Container::Text);
    assert_eq!(container(b"toc.dat"), Container::Text);
}

#[test]
fn detect_app_dump_header() {
    let text = "-- DntDatabase dump v1\n-- Source: PostgreSQL 17.0, database shop\n\
                -- Created: 2026-10-25T00:00:00Z\n-- Objects: 1 tables (1 with data), 0 views\n\
                SET client_encoding = 'UTF8';\nCREATE TABLE \"public\".\"t\" (id integer);\n\
                -- End of dump: 2 statements\n";
    let r = scan_text(text, DbKind::Postgres, "shop");
    assert!(r.stats.app_dump);
    assert_eq!(r.engine, Some(DbKind::Postgres));
    assert_eq!(r.path, RestorePath::AppRunner);
    assert_eq!(r.stats.statements, 2);
    assert!(r.refusals.is_empty(), "{:?}", r.refusals);
    // With a BOM too.
    let r = scan(
        [BOM, text.as_bytes()].concat().as_slice(),
        &target(DbKind::Postgres, "shop"),
    )
    .unwrap();
    assert!(r.stats.app_dump);
    // Not the app's: a plain script.
    assert!(
        !scan_text("SELECT 1;", DbKind::Postgres, "shop")
            .stats
            .app_dump
    );
}

#[test]
fn detect_copy_block_is_psql() {
    let text = "SET client_encoding = 'UTF8';\nCREATE TABLE t (id int, name text);\n\
                COPY public.t (id, name) FROM stdin;\n1\ta\n2\t\\N\n\\.\n\nSELECT 1;\n";
    let r = scan_text(text, DbKind::Postgres, "shop");
    assert_eq!(r.path, RestorePath::Psql);
    assert_eq!(r.stats.copy_blocks, 1);
    assert!(r.refusals.is_empty(), "{:?}", r.refusals);
}

#[test]
fn detect_connect_is_psql() {
    let r = scan_text(
        "\\connect shop\nCREATE TABLE t (id int);\n",
        DbKind::Postgres,
        "shop",
    );
    assert_eq!(r.path, RestorePath::Psql);
    assert_eq!(r.stats.client_commands, 1);
    assert!(r.refusals.is_empty(), "{:?}", r.refusals);
    // pg_dump 17.6+ restrict lines are allowed.
    let r = scan_text(
        "\\restrict abc\nSELECT 1;\n\\unrestrict abc\n",
        DbKind::Postgres,
        "shop",
    );
    assert_eq!(r.path, RestorePath::Psql);
    assert!(r.refusals.is_empty(), "{:?}", r.refusals);
}

#[test]
fn psql_commands_other_than_connect_are_refused() {
    for (line, refused) in [
        ("\\connect shop", false),
        ("\\c \"shop\"", false),
        ("\\c -", false),
        ("\\connect", false),
        ("\\connect other", true),
        ("\\c shop postgres otherhost", true),
        ("\\connect -reuse-previous=on \"dbname='shop'\"", true),
        ("\\! del /q C:\\*", true),
        ("\\i other.sql", true),
        ("\\o out.txt", true),
        ("\\set ON_ERROR_STOP 0", true),
        ("\\copy t from program 'x'", true),
    ] {
        assert_eq!(
            psql_command_refusal(line, "shop").is_some(),
            refused,
            "{line}"
        );
    }
    let r = scan_text("\\! calc.exe\nSELECT 1;\n", DbKind::Postgres, "shop");
    assert_eq!(r.refused, 1);
    assert!(r.refusals[0].contains("\\!"), "{:?}", r.refusals);
}

#[test]
fn mysql_file_with_client_command_is_refused() {
    let r = scan_text("SELECT * FROM t\\G\nSELECT 1;\n", DbKind::MySql, "shop");
    assert!(r.refused >= 1, "{r:?}");
    assert!(r.refusals[0].contains("\\G"), "{:?}", r.refusals);
    assert_eq!(r.path, RestorePath::AppRunner);
    let r = scan_text("\\. other.sql\nSELECT 1;\n", DbKind::MySql, "shop");
    assert_eq!(r.refused, 1, "{r:?}");
    // A backslash inside a string is data.
    let r = scan_text("INSERT INTO t VALUES ('a\\\\b');\n", DbKind::MySql, "shop");
    assert!(r.refusals.is_empty(), "{:?}", r.refusals);
}

#[test]
fn database_statements_are_refused_for_the_app_runner() {
    for (sql, kind) in [
        ("CREATE DATABASE other", DbKind::MySql),
        (
            "CREATE DATABASE /*!32312 IF NOT EXISTS*/ `shop`",
            DbKind::MySql,
        ),
        ("CREATE SCHEMA other", DbKind::MySql),
        ("CREATE OR REPLACE DATABASE x", DbKind::MySql),
        ("DROP DATABASE IF EXISTS shop", DbKind::MySql),
        ("DROP SCHEMA other", DbKind::MySql),
        ("USE other", DbKind::MySql),
        ("USE `other`", DbKind::MySql),
        ("/*!40000 USE other */", DbKind::MySql),
        ("ALTER DATABASE other CHARACTER SET utf8mb4", DbKind::MySql),
        ("CREATE DATABASE other", DbKind::Postgres),
        ("DROP DATABASE shop", DbKind::Postgres),
        ("ALTER DATABASE other SET x = 1", DbKind::Postgres),
    ] {
        let d = if kind == DbKind::MySql { MY } else { PG };
        assert!(
            statement_refusal(sql, d, kind, "shop").is_some(),
            "{sql} should be refused"
        );
    }
    for (sql, kind) in [
        ("USE shop", DbKind::MySql),
        ("USE `shop`", DbKind::MySql),
        ("ALTER DATABASE shop CHARACTER SET utf8mb4", DbKind::MySql),
        ("ALTER DATABASE CHARACTER SET utf8mb4", DbKind::MySql),
        ("CREATE SCHEMA sales", DbKind::Postgres),
        ("DROP SCHEMA IF EXISTS sales CASCADE", DbKind::Postgres),
        ("ALTER DATABASE shop SET x = 1", DbKind::Postgres),
        ("CREATE TABLE `database` (id int)", DbKind::MySql),
        ("SELECT 'CREATE DATABASE x'", DbKind::MySql),
    ] {
        let d = if kind == DbKind::MySql { MY } else { PG };
        assert!(
            statement_refusal(sql, d, kind, "shop").is_none(),
            "{sql} should be allowed"
        );
    }
    let r = scan_text(
        "CREATE DATABASE x;\nUSE x;\nCREATE TABLE t (id int);\n",
        DbKind::MySql,
        "shop",
    );
    assert_eq!(r.refused, 2);
    assert_eq!(r.refusals.len(), 2);
}

#[test]
fn statement_counts_by_kind() {
    let text = "-- header\nSET NAMES utf8mb4;\nDROP TABLE IF EXISTS t;\nCREATE TABLE t (id int);\n\
                INSERT INTO t VALUES (1), (2);\nSELECT * FROM t;\nSTART TRANSACTION;\n\
                UPDATE t SET id = 3 WHERE id = 1;\nCOMMIT;\n";
    let r = scan_text(text, DbKind::MySql, "shop");
    let s = &r.stats;
    assert_eq!(s.statements, 8);
    assert_eq!(s.ddl, 2);
    assert_eq!(s.writes, 2);
    assert_eq!(s.reads, 1);
    assert_eq!(s.transaction, 2);
    assert_eq!(s.other, 1);
    assert_eq!(s.drops, 1);
    assert_eq!((s.copy_blocks, s.client_commands), (0, 0));
    assert!(!r.transaction_ok());
}

#[test]
fn pg_statements_outside_a_transaction_block() {
    let r = scan_text(
        "CREATE TABLE t (id int);\nCREATE INDEX CONCURRENTLY i ON t (id);\nVACUUM t;\n",
        DbKind::Postgres,
        "shop",
    );
    assert_eq!(r.outside_tx, 2);
    assert!(!r.transaction_ok());
    assert!(scan_text("CREATE TABLE t (id int);", DbKind::Postgres, "shop").transaction_ok());
}

#[test]
fn other_engine_files_are_refused() {
    let my = "-- MySQL dump 10.13  Distrib 8.4.3, for Win64 (x86_64)\n\
              /*!40101 SET NAMES utf8mb4 */;\nCREATE TABLE `t` (id int);\n";
    let r = scan_text(my, DbKind::Postgres, "shop");
    assert_eq!(r.engine, Some(DbKind::MySql));
    assert!(r.refusals.iter().any(|m| m.contains("MySQL dump")), "{r:?}");
    // MySQL comments without a header.
    let r = scan_text("/*!40101 SET NAMES utf8 */;\n", DbKind::Postgres, "shop");
    assert_eq!(r.engine, Some(DbKind::MySql));
    assert_eq!(r.refused, 1);
    let pg = "--\n-- PostgreSQL database dump\n--\nSET statement_timeout = 0;\n";
    let r = scan_text(pg, DbKind::MySql, "shop");
    assert_eq!(r.engine, Some(DbKind::Postgres));
    assert_eq!(r.refused, 1);
    assert_eq!(
        header_engine("-- DntDatabase dump v1\n-- Source: MariaDB 11.4.2, database x\n"),
        Some(DbKind::MySql)
    );
}

#[test]
fn sql_mode_literal_feeds_the_scan() {
    let mode = |s: &str| sql_mode_after(s, MY, MY);
    let nbe = Dialect::MySql(MySqlMode {
        ansi_quotes: false,
        no_backslash_escapes: true,
    });
    assert_eq!(mode("SET sql_mode = 'NO_BACKSLASH_ESCAPES'"), Some(nbe));
    assert_eq!(
        mode("SET SESSION sql_mode = 'NO_AUTO_VALUE_ON_ZERO'"),
        Some(MY)
    );
    assert_eq!(
        mode("/*!40101 SET @OLD_SQL_MODE=@@SQL_MODE, SQL_MODE='ANSI_QUOTES' */"),
        Some(Dialect::MySql(MySqlMode {
            ansi_quotes: true,
            no_backslash_escapes: false,
        }))
    );
    assert_eq!(
        sql_mode_after("SET sql_mode = @OLD_SQL_MODE", nbe, MY),
        Some(MY)
    );
    assert_eq!(mode("SET NAMES utf8mb4"), None);
    assert_eq!(mode("SET GLOBAL sql_mode = 'ANSI'"), None);
    assert_eq!(sql_mode_after("SET x = 1", PG, PG), None);
    // The scan splits the rest with the new mode: with backslash escapes the two
    // INSERTs would be one statement.
    let text = "SET sql_mode = 'NO_BACKSLASH_ESCAPES';\nINSERT INTO t VALUES ('a\\');\n\
                INSERT INTO t VALUES ('b');\n";
    let r = scan_text(text, DbKind::MySql, "shop");
    assert_eq!(r.stats.statements, 3);
    assert_eq!(r.stats.writes, 2);
}

#[test]
fn analyze_hashes_raw_bytes_and_reads_gzip() {
    let dir = tempfile::tempdir().unwrap();
    let text = "CREATE TABLE t (id int);\nINSERT INTO t VALUES (1);\n".repeat(20_000);
    let plain = dir.path().join("a.sql");
    std::fs::write(&plain, &text).unwrap();
    let gz = dir.path().join("a.sql.gz");
    std::fs::write(&gz, gzip(text.as_bytes())).unwrap();
    let t = target(DbKind::MySql, "shop");
    for (p, zipped) in [(&plain, false), (&gz, true)] {
        let a = analyze_file(p, &t).unwrap();
        assert_eq!(a.gzip, zipped);
        assert_eq!(a.container, Container::Text);
        assert_eq!(a.file, crate::file_facts(p).unwrap());
        let s = a.scan.unwrap().unwrap();
        assert_eq!(s.stats.statements, 40_000);
        assert_eq!(s.path, RestorePath::AppRunner);
    }
    // An archive is hashed but not scanned.
    let arch = dir.path().join("a.dump");
    let mut bytes = b"PGDMP\x01\x0e\x00".to_vec();
    bytes.extend(vec![7u8; 100_000]);
    std::fs::write(&arch, &bytes).unwrap();
    let a = analyze_file(&arch, &t).unwrap();
    assert_eq!(a.container, Container::PgCustom);
    assert!(a.scan.is_none());
    assert_eq!(a.file, crate::file_facts(&arch).unwrap());
    // Gzip-wrapped archive: detected through the decompression.
    let gza = dir.path().join("a.dump.gz");
    std::fs::write(&gza, gzip(&bytes)).unwrap();
    let a = analyze_file(&gza, &t).unwrap();
    assert!(a.gzip);
    assert_eq!(a.container, Container::PgCustom);
    // Not UTF-8: not a script.
    let bin = dir.path().join("a.bin");
    std::fs::write(&bin, b"SELECT '\xff\xfe';").unwrap();
    let a = analyze_file(&bin, &t).unwrap();
    assert!(a.scan.unwrap().is_err());
    // Short files.
    let tiny = dir.path().join("tiny.sql");
    std::fs::write(&tiny, "S").unwrap();
    assert_eq!(analyze_file(&tiny, &t).unwrap().file.size, 1);
    let empty = dir.path().join("empty.sql");
    std::fs::write(&empty, "").unwrap();
    let a = analyze_file(&empty, &t).unwrap();
    assert_eq!(a.scan.unwrap().unwrap().stats.statements, 0);
}

#[test]
fn fixture_files_detect_as_planned() {
    let dir =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../src-tauri/tests/fixtures/p5");
    let copy = analyze_file(
        &dir.join("pg_dump_plain_copy.sql"),
        &target(DbKind::Postgres, "p5_dst"),
    )
    .unwrap()
    .scan
    .unwrap()
    .unwrap();
    assert_eq!(copy.path, RestorePath::Psql, "{copy:?}");
    assert_eq!(copy.stats.copy_blocks, 1);
    assert_eq!(copy.engine, Some(DbKind::Postgres));
    assert!(copy.refusals.is_empty(), "{:?}", copy.refusals);
    let my = analyze_file(
        &dir.join("mysqldump_sample.sql"),
        &target(DbKind::MySql, "p5_dst"),
    )
    .unwrap()
    .scan
    .unwrap()
    .unwrap();
    assert_eq!(my.path, RestorePath::AppRunner, "{my:?}");
    assert_eq!(my.engine, Some(DbKind::MySql));
    assert!(my.refusals.is_empty(), "{:?}", my.refusals);
    assert!(my.stats.ddl >= 2, "{:?}", my.stats);
    assert!(my.stats.writes >= 1, "{:?}", my.stats);
}
