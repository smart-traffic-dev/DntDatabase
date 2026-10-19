use super::lexer::{is_kw, readings, tokenize, word_at, Dialect, TokKind, Token};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatementKind {
    Read,
    Write,
    Ddl,
    Transaction(TxControl),
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TxControl {
    Begin,
    Commit,
    Rollback,
    AutocommitOff,
    AutocommitOn,
}

/// Classifies one statement by its leading keyword (after comments and
/// opening parens). Strings, comments and quoted identifiers are lexed with
/// the same per-dialect rules as [`split`](super::split::split).
pub fn classify(stmt: &str, dialect: Dialect) -> StatementKind {
    Classifier { dialect }.classify(stmt)
}

/// Carries the dialect through the recursive helpers.
#[derive(Clone, Copy)]
struct Classifier {
    dialect: Dialect,
}

impl Classifier {
    fn classify(self, stmt: &str) -> StatementKind {
        let toks = tokenize(stmt, self.dialect);
        // Only MySQL lexing yields ExecComment; in PG `/*!` is a plain comment.
        // Whether the server runs a body depends on its version, so classify
        // both readings and keep the stricter (the inlined one on a tie).
        if toks.iter().any(|t| t.kind == TokKind::ExecComment) {
            let [inlined, ignored]: [String; 2] = readings(stmt, self.dialect)
                .try_into()
                .expect("an executable comment yields two readings");
            let (run, skip) = (self.classify(&inlined), self.classify(&ignored));
            return if strictness(skip) > strictness(run) {
                skip
            } else {
                run
            };
        }
        let start = toks.iter().take_while(|t| t.kind == TokKind::Open).count();
        let toks = &toks[start..];
        let Some(first) = word_at(toks, 0) else {
            return StatementKind::Other;
        };
        let tx = StatementKind::Transaction;
        match first.to_ascii_uppercase().as_str() {
            "SELECT" => select_kind(&toks[1..]),
            "EXPLAIN" => self.explain(stmt, &toks[1..]),
            "SHOW" | "DESCRIBE" | "DESC" | "VALUES" | "TABLE" => StatementKind::Read,
            // `EXECUTE` runs a prepared statement whose body is unknown here.
            "INSERT" | "UPDATE" | "DELETE" | "REPLACE" | "MERGE" | "CALL" | "DO" | "LOAD"
            | "COPY" | "EXECUTE" => StatementKind::Write,
            "CREATE" | "ALTER" | "DROP" | "TRUNCATE" | "RENAME" | "GRANT" | "REVOKE"
            | "COMMENT" => StatementKind::Ddl,
            // PG maintenance / ownership statements.
            "REFRESH" | "REINDEX" | "VACUUM" | "CLUSTER" | "IMPORT" | "SECURITY" | "REASSIGN" => {
                StatementKind::Ddl
            }
            // MySQL table maintenance and plugins.
            "OPTIMIZE" | "REPAIR" | "INSTALL" | "UNINSTALL" => StatementKind::Ddl,
            "SET" if is_kw(word_at(toks, 1), "PASSWORD") => StatementKind::Ddl,
            // MariaDB `BEGIN NOT ATOMIC … END` is a compound statement, not a transaction.
            "BEGIN" if !is_kw(word_at(toks, 1), "NOT") => tx(TxControl::Begin),
            // PG `END [WORK|TRANSACTION] [AND [NO] CHAIN]` commits.
            "END"
                if toks.get(1).is_none_or(|t| {
                    t.kind == TokKind::Punct(b';')
                        || matches!(t.kind, TokKind::Word(w) if ["WORK", "TRANSACTION", "AND"]
                    .iter()
                    .any(|k| w.eq_ignore_ascii_case(k)))
                }) =>
            {
                tx(TxControl::Commit)
            }
            "ABORT" => tx(TxControl::Rollback),
            "START" if is_kw(word_at(toks, 1), "TRANSACTION") => tx(TxControl::Begin),
            "COMMIT" if !is_kw(word_at(toks, 1), "PREPARED") => tx(TxControl::Commit),
            "ROLLBACK" => {
                let mut i = 1;
                if is_kw(word_at(toks, i), "WORK") || is_kw(word_at(toks, i), "TRANSACTION") {
                    i += 1;
                }
                if is_kw(word_at(toks, i), "TO") || is_kw(word_at(toks, i), "PREPARED") {
                    StatementKind::Other
                } else {
                    tx(TxControl::Rollback)
                }
            }
            "SET" => set_autocommit(stmt, &toks[1..]).map_or(StatementKind::Other, tx),
            "WITH" => self.classify_with(stmt, &toks[1..]),
            _ => StatementKind::Other,
        }
    }

    /// After `WITH`: skips CTE definitions until the main statement, which is
    /// either a keyword or a parenthesised query (`WITH x AS (…) (SELECT …)`).
    /// A depth-0 paren group is a CTE's column list or body while that CTE has
    /// not yet had its body (`AS [NOT] [MATERIALIZED] (…)`); after a body only a
    /// `,` starts another CTE, so any other depth-0 `(` opens the main query.
    /// A data-modifying CTE body also makes the statement a write.
    fn classify_with(self, src: &str, toks: &[Token]) -> StatementKind {
        let mut depth = 0usize;
        let mut body_start = 0;
        let mut in_body = false;
        let mut cte_done = false;
        let mut expect_name = true;
        let mut cte_writes = false;
        let combine = |main: StatementKind, cte_writes: bool| {
            if cte_writes && main == StatementKind::Read {
                StatementKind::Write
            } else {
                main
            }
        };
        for (idx, t) in toks.iter().enumerate() {
            match t.kind {
                TokKind::Open if depth == 0 && cte_done => {
                    return combine(self.classify(&src[t.start..]), cte_writes);
                }
                TokKind::Open => {
                    if depth == 0 {
                        body_start = t.end;
                        in_body = idx > 0
                            && (is_kw(word_at(toks, idx - 1), "AS")
                                || is_kw(word_at(toks, idx - 1), "MATERIALIZED"));
                    }
                    depth += 1;
                }
                TokKind::Close if depth > 0 => {
                    depth -= 1;
                    if depth == 0 && in_body {
                        cte_done = true;
                        if self.classify(&src[body_start..t.start]) == StatementKind::Write {
                            cte_writes = true;
                        }
                    }
                }
                TokKind::Punct(b',') if depth == 0 => {
                    cte_done = false;
                    expect_name = true;
                }
                // The CTE name slot may hold a word such as `values`; it is never
                // the main statement.
                TokKind::Word(w) if depth == 0 && expect_name => {
                    if !(idx == 0 && w.eq_ignore_ascii_case("RECURSIVE")) {
                        expect_name = false;
                    }
                }
                TokKind::Quoted if depth == 0 => expect_name = false,
                TokKind::Word(w) if depth == 0 => {
                    let main = match w.to_ascii_uppercase().as_str() {
                        "INSERT" | "UPDATE" | "DELETE" | "MERGE" | "REPLACE" => {
                            StatementKind::Write
                        }
                        "SELECT" => select_kind(&toks[idx + 1..]),
                        "VALUES" | "TABLE" => StatementKind::Read,
                        _ => continue,
                    };
                    return combine(main, cte_writes);
                }
                _ => {}
            }
        }
        if cte_writes {
            StatementKind::Write
        } else {
            StatementKind::Other
        }
    }
}

/// Tokens after `SELECT`: a depth-0 `INTO` that creates a file or table
/// (`INTO OUTFILE|DUMPFILE`, PG `INTO [TEMP|UNLOGGED] [TABLE] t`) is Ddl;
/// `INTO @var` stays Read.
fn select_kind(toks: &[Token]) -> StatementKind {
    let mut depth = 0usize;
    for (i, t) in toks.iter().enumerate() {
        match t.kind {
            TokKind::Open => depth += 1,
            TokKind::Close => depth = depth.saturating_sub(1),
            TokKind::Word(w) if depth == 0 && w.eq_ignore_ascii_case("INTO") => {
                return match toks.get(i + 1).map(|t| t.kind) {
                    None => StatementKind::Read,
                    Some(TokKind::Word(v)) if v.starts_with('@') => StatementKind::Read,
                    Some(_) => StatementKind::Ddl,
                };
            }
            _ => {}
        }
    }
    StatementKind::Read
}

impl Classifier {
    /// Tokens after `EXPLAIN`. With ANALYZE the statement is executed, so it takes
    /// the inner statement's kind; otherwise it is Read.
    fn explain(self, src: &str, toks: &[Token]) -> StatementKind {
        let is_analyze = |w: Option<&str>| is_kw(w, "ANALYZE") || is_kw(w, "ANALYSE");
        let inner = if is_analyze(word_at(toks, 0)) {
            // `EXPLAIN ANALYZE [VERBOSE] …` (PG) / `EXPLAIN ANALYZE [FORMAT=TREE] …` (MySQL)
            let mut i = 1;
            if is_kw(word_at(toks, i), "VERBOSE") {
                i += 1;
            }
            if is_kw(word_at(toks, i), "FORMAT")
                && toks.get(i + 1).map(|t| t.kind) == Some(TokKind::Punct(b'='))
            {
                i += 3;
            }
            Some(i)
        } else if toks.first().map(|t| t.kind) == Some(TokKind::Open) {
            // PG `EXPLAIN (option [value], …) …`
            let close = toks
                .iter()
                .position(|t| t.kind == TokKind::Close)
                .unwrap_or(toks.len());
            let analyze = toks[1..close]
                .split(|t| t.kind == TokKind::Punct(b','))
                .any(|opt| {
                    is_analyze(word_at(opt, 0))
                        && !["FALSE", "OFF", "0"]
                            .iter()
                            .any(|v| is_kw(word_at(opt, 1), v))
                });
            analyze.then_some(close + 1)
        } else {
            None
        };
        match inner.and_then(|i| toks.get(i)) {
            Some(t) => self.classify(&src[t.start..]),
            None => StatementKind::Read,
        }
    }
}

/// Ddl > Write > Transaction/Other > Read.
fn strictness(k: StatementKind) -> u8 {
    match k {
        StatementKind::Read => 0,
        StatementKind::Transaction(_) | StatementKind::Other => 1,
        StatementKind::Write => 2,
        StatementKind::Ddl => 3,
    }
}

/// `SET` assignments; the last non-global `autocommit` assignment wins.
/// Trailing `;` tokens are ignored.
fn set_autocommit(src: &str, toks: &[Token]) -> Option<TxControl> {
    let end = toks
        .iter()
        .rposition(|t| t.kind != TokKind::Punct(b';'))
        .map_or(0, |p| p + 1);
    toks[..end]
        .rsplit(|t| t.kind == TokKind::Punct(b','))
        .find_map(|a| autocommit_assignment(src, a))
}

/// `[SESSION|LOCAL] (autocommit | @@autocommit | @@session.autocommit | @@local.autocommit)
/// (= | := | TO) (0|1|ON|OFF|TRUE|FALSE, optionally quoted)`
fn autocommit_assignment(src: &str, a: &[Token]) -> Option<TxControl> {
    let mut i = 0;
    if is_kw(word_at(a, 0), "SESSION") || is_kw(word_at(a, 0), "LOCAL") {
        i += 1;
    }
    let target = word_at(a, i)?;
    if is_kw(Some(target), "autocommit") || is_kw(Some(target), "@@autocommit") {
        i += 1;
    } else if (is_kw(Some(target), "@@session") || is_kw(Some(target), "@@local"))
        && a.get(i + 1).map(|t| t.kind) == Some(TokKind::Punct(b'.'))
        && is_kw(word_at(a, i + 2), "autocommit")
    {
        i += 3;
    } else {
        return None;
    }
    if a.get(i).map(|t| t.kind) == Some(TokKind::Punct(b':')) {
        i += 1;
    }
    if a.get(i).map(|t| t.kind) == Some(TokKind::Punct(b'=')) || is_kw(word_at(a, i), "TO") {
        i += 1;
    } else {
        return None;
    }
    if i + 1 != a.len() {
        return None;
    }
    let value = match a[i].kind {
        TokKind::Word(w) => w,
        TokKind::Quoted => {
            let q = &src[a[i].start..a[i].end];
            let terminated = q.len() >= 2 && q.ends_with(&q[..1]);
            terminated.then(|| &q[1..q.len() - 1])?
        }
        _ => return None,
    };
    match value.to_ascii_uppercase().as_str() {
        "0" | "OFF" | "FALSE" => Some(TxControl::AutocommitOff),
        "1" | "ON" | "TRUE" => Some(TxControl::AutocommitOn),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::StatementKind::*;
    use super::TxControl::*;
    use super::*;

    /// The pre-dialect assertions below all use MySQL lexing.
    fn classify(s: &str) -> StatementKind {
        super::classify(s, Dialect::MYSQL)
    }

    #[test]
    fn cte_named_like_a_main_keyword() {
        for d in [Dialect::MYSQL, Dialect::Postgres] {
            let c = |s: &str| super::classify(s, d);
            assert_eq!(
                c("WITH values AS (DELETE FROM t RETURNING 1) SELECT 1"),
                Write
            );
            assert_eq!(
                c("WITH RECURSIVE table AS (DELETE FROM t RETURNING 1) SELECT 1"),
                Write
            );
            assert_eq!(
                c("WITH a AS (SELECT 1), select AS (UPDATE t SET x=1 RETURNING 1) TABLE a"),
                Write
            );
            assert_eq!(c("WITH values(v) AS (SELECT 1) SELECT * FROM values"), Read);
            assert_eq!(c("WITH x AS (SELECT 1) VALUES (1)"), Read);
        }
    }

    #[test]
    fn executable_comment_with_trailing_text() {
        assert_eq!(classify("/*! */ DELETE FROM t"), Write);
        assert_eq!(classify("/*!99999 SELECT 1 */ DELETE FROM t"), Write);
        assert_eq!(classify("SELECT 1 /*!, (DROP TABLE t) */"), Read);
        assert_eq!(classify("/*!50001 DROP VIEW v */ SELECT 1"), Ddl);
        assert_eq!(classify("/*!50001 DROP VIEW v */"), Ddl);
        assert_eq!(classify("/*!40014 SET FOREIGN_KEY_CHECKS=0 */"), Other);
        assert_eq!(
            classify("/*!40101 SET autocommit=0 */"),
            Transaction(AutocommitOff)
        );
    }

    #[test]
    fn production_gate_keywords() {
        use Dialect::Postgres;
        const MYSQL: Dialect = Dialect::MYSQL;
        let table: &[(&str, Dialect, StatementKind)] = &[
            ("REFRESH MATERIALIZED VIEW mv", Postgres, Ddl),
            ("REINDEX TABLE t", Postgres, Ddl),
            ("VACUUM FULL t", Postgres, Ddl),
            ("vacuum", Postgres, Ddl),
            ("CLUSTER t USING i", Postgres, Ddl),
            (
                "IMPORT FOREIGN SCHEMA s FROM SERVER srv INTO public",
                Postgres,
                Ddl,
            ),
            ("SECURITY LABEL ON TABLE t IS 'x'", Postgres, Ddl),
            ("REASSIGN OWNED BY a TO b", Postgres, Ddl),
            (
                "ALTER DEFAULT PRIVILEGES GRANT SELECT ON TABLES TO u",
                Postgres,
                Ddl,
            ),
            ("DISCARD ALL", Postgres, Other),
            ("OPTIMIZE TABLE t", MYSQL, Ddl),
            ("REPAIR TABLE t", MYSQL, Ddl),
            ("INSTALL PLUGIN p SONAME 'p.so'", MYSQL, Ddl),
            ("UNINSTALL PLUGIN p", MYSQL, Ddl),
            ("SET PASSWORD FOR u = 'x'", MYSQL, Ddl),
            ("set password = 'x'", MYSQL, Ddl),
            ("ALTER USER u IDENTIFIED BY 'x'", MYSQL, Ddl),
            ("CREATE USER u", MYSQL, Ddl),
            ("DROP USER u", MYSQL, Ddl),
            ("RENAME USER a TO b", MYSQL, Ddl),
            ("GRANT ALL ON *.* TO u", MYSQL, Ddl),
            ("REVOKE ALL ON *.* FROM u", MYSQL, Ddl),
            ("EXECUTE stmt USING @a", MYSQL, Write),
            ("EXECUTE p(1)", Postgres, Write),
            ("CALL p()", MYSQL, Write),
            ("CALL p()", Postgres, Write),
        ];
        for &(s, d, want) in table {
            assert_eq!(super::classify(s, d), want, "{s} ({d:?})");
        }
    }

    #[test]
    fn sql_mode_flags_change_classification() {
        use crate::sql::MySqlMode;
        let ansi = Dialect::MySql(MySqlMode {
            ansi_quotes: true,
            ..Default::default()
        });
        let nbe = Dialect::MySql(MySqlMode {
            no_backslash_escapes: true,
            ..Default::default()
        });
        let s = r#"WITH a AS (SELECT "x\") DELETE FROM t"#;
        assert_eq!(super::classify(s, ansi), Write);
        assert_eq!(super::classify(s, nbe), Write);
        assert_eq!(classify(s), Other);
        // Two readings still apply under the flags.
        assert_eq!(super::classify(r"/*!50001 DROP VIEW v */", nbe), Ddl);
        assert_eq!(
            super::classify(r"/*!40101 SET x='C:\' */ DELETE FROM t", nbe),
            Write
        );
    }

    #[test]
    fn parenthesised_main_query_after_cte() {
        assert_eq!(classify("WITH x AS (SELECT 1) (SELECT * FROM x)"), Read);
        assert_eq!(
            classify(
                "WITH x(a) AS (SELECT 1), y AS (SELECT 2) ((SELECT a FROM x) UNION (SELECT 2))"
            ),
            Read
        );
        assert_eq!(
            classify("WITH d AS (DELETE FROM t RETURNING *) (SELECT * FROM d)"),
            Write
        );
        assert_eq!(
            classify("WITH x AS (SELECT 1) (SELECT * INTO newtab FROM x)"),
            Ddl
        );
        assert_eq!(
            classify("WITH x AS NOT MATERIALIZED (SELECT 1) (SELECT 1)"),
            Read
        );
    }

    #[test]
    fn brief_cases() {
        assert_eq!(classify("  -- c\n select 1"), Read);
        assert_eq!(classify("WITH x AS (SELECT 1) SELECT * FROM x"), Read);
        assert_eq!(
            classify("WITH x AS (SELECT 1) DELETE FROM t USING x"),
            Write
        );
        assert_eq!(classify("update t set a=1"), Write);
        assert_eq!(classify("TRUNCATE t"), Ddl);
        assert_eq!(classify("start transaction"), Transaction(Begin));
        assert_eq!(classify("ROLLBACK TO SAVEPOINT s"), Other);
        assert_eq!(classify("set autocommit = 0"), Transaction(AutocommitOff));
        assert_eq!(classify("SET @@autocommit=ON"), Transaction(AutocommitOn));
        assert_eq!(classify("use shop"), Other);
    }

    #[test]
    fn keyword_groups() {
        for s in [
            "SHOW TABLES",
            "explain select 1",
            "DESCRIBE t",
            "desc t",
            "VALUES (1)",
            "TABLE t",
        ] {
            assert_eq!(classify(s), Read, "{s}");
        }
        for s in [
            "insert into t values (1)",
            "DELETE FROM t",
            "REPLACE INTO t VALUES (1)",
            "MERGE INTO t USING s ON true WHEN MATCHED THEN DELETE",
            "CALL p()",
            "DO 1",
            "LOAD DATA INFILE 'x' INTO TABLE t",
            "COPY t FROM STDIN",
        ] {
            assert_eq!(classify(s), Write, "{s}");
        }
        for s in [
            "create table t (a int)",
            "ALTER TABLE t ADD b int",
            "DROP TABLE t",
            "RENAME TABLE a TO b",
            "GRANT SELECT ON t TO u",
            "REVOKE SELECT ON t FROM u",
            "COMMENT ON TABLE t IS 'x'",
        ] {
            assert_eq!(classify(s), Ddl, "{s}");
        }
        for s in [
            "",
            "  /* only */ ",
            "SET NAMES utf8mb4",
            "set search_path = public",
            "LOCK TABLES t READ",
        ] {
            assert_eq!(classify(s), Other, "{s:?}");
        }
    }

    #[test]
    fn leading_comments_and_parens_are_skipped() {
        assert_eq!(classify("/* x */ # y\n-- z\n DELETE FROM t"), Write);
        assert_eq!(classify("(SELECT 1) UNION (SELECT 2)"), Read);
    }

    #[test]
    fn transaction_control() {
        assert_eq!(classify("BEGIN"), Transaction(Begin));
        assert_eq!(classify("begin work"), Transaction(Begin));
        assert_eq!(
            classify("BEGIN TRANSACTION ISOLATION LEVEL SERIALIZABLE"),
            Transaction(Begin)
        );
        assert_eq!(classify("START TRANSACTION READ ONLY"), Transaction(Begin));
        assert_eq!(classify("START SLAVE"), Other);
        assert_eq!(classify("commit"), Transaction(Commit));
        assert_eq!(classify("COMMIT WORK"), Transaction(Commit));
        assert_eq!(classify("rollback"), Transaction(Rollback));
        assert_eq!(classify("ROLLBACK WORK"), Transaction(Rollback));
        assert_eq!(classify("rollback to s"), Other);
        assert_eq!(classify("ROLLBACK WORK TO SAVEPOINT s"), Other);
        assert_eq!(classify("COMMIT PREPARED 'x'"), Other);
        assert_eq!(classify("ROLLBACK PREPARED 'x'"), Other);
    }

    #[test]
    fn autocommit_variants() {
        assert_eq!(classify("SET autocommit=1"), Transaction(AutocommitOn));
        assert_eq!(
            classify("set session autocommit=off"),
            Transaction(AutocommitOff)
        );
        assert_eq!(
            classify("SET LOCAL autocommit TO on"),
            Transaction(AutocommitOn)
        );
        assert_eq!(
            classify("SET @@session.autocommit = 0"),
            Transaction(AutocommitOff)
        );
        assert_eq!(classify("set @@autocommit := 1"), Transaction(AutocommitOn));
        assert_eq!(
            classify("SET sql_mode = '', autocommit = 0"),
            Transaction(AutocommitOff)
        );
        assert_eq!(classify("SET GLOBAL autocommit = 0"), Other);
        assert_eq!(classify("SET @@global.autocommit = 0"), Other);
        assert_eq!(classify("SET autocommit = DEFAULT"), Other);
        assert_eq!(classify("SET @autocommit = 0"), Other);
    }

    #[test]
    fn executable_comments_classify_their_body() {
        assert_eq!(classify("/*!50001 DROP VIEW v */"), Ddl);
        assert_eq!(classify("/*!40014 SET FOREIGN_KEY_CHECKS=0 */"), Other);
        assert_eq!(classify("/*M!100100 DELETE FROM t */"), Write);
        assert_eq!(
            classify("/*! SET autocommit=0 */"),
            Transaction(AutocommitOff)
        );
        assert_eq!(classify("-- c\n/*!50003 CREATE TRIGGER x"), Ddl);
    }

    #[test]
    fn explain_analyze_takes_inner_kind() {
        assert_eq!(classify("EXPLAIN ANALYZE DELETE FROM t"), Write);
        assert_eq!(
            classify("explain (analyze, buffers) update t set a=1"),
            Write
        );
        assert_eq!(classify("EXPLAIN ANALYZE SELECT 1"), Read);
        assert_eq!(classify("EXPLAIN DELETE FROM t"), Read);
        assert_eq!(
            classify("EXPLAIN ANALYSE VERBOSE INSERT INTO t VALUES (1)"),
            Write
        );
        assert_eq!(
            classify("EXPLAIN (ANALYZE true, BUFFERS) DELETE FROM t"),
            Write
        );
        assert_eq!(classify("EXPLAIN (BUFFERS, ANALYSE) DELETE FROM t"), Write);
        assert_eq!(classify("EXPLAIN (ANALYZE false) DELETE FROM t"), Read);
        assert_eq!(classify("EXPLAIN (FORMAT JSON) DELETE FROM t"), Read);
        assert_eq!(
            classify("EXPLAIN ANALYZE FORMAT=TREE UPDATE t SET a=1"),
            Write
        );
    }

    #[test]
    fn select_into_that_writes_is_ddl() {
        assert_eq!(classify("select * into outfile '/tmp/x' from t"), Ddl);
        assert_eq!(classify("SELECT a FROM t INTO DUMPFILE '/tmp/x'"), Ddl);
        assert_eq!(classify("SELECT a INTO newtab FROM t"), Ddl);
        assert_eq!(classify("SELECT a INTO TEMP TABLE newtab FROM t"), Ddl);
        assert_eq!(classify("select a into @v from t"), Read);
        assert_eq!(classify("select a from t into @v, @w"), Read);
        assert_eq!(classify("SELECT (SELECT 1) FROM t"), Read);
        assert_eq!(
            classify("WITH c AS (SELECT 1) SELECT * INTO newtab FROM c"),
            Ddl
        );
    }

    #[test]
    fn transaction_control_completeness() {
        assert_eq!(classify("END"), Transaction(Commit));
        assert_eq!(classify("end work"), Transaction(Commit));
        assert_eq!(classify("END TRANSACTION"), Transaction(Commit));
        assert_eq!(classify("ABORT"), Transaction(Rollback));
        assert_eq!(classify("abort transaction"), Transaction(Rollback));
        assert_eq!(classify("BEGIN NOT ATOMIC SELECT 1; END"), Other);
        assert_eq!(classify("SET autocommit = 0;"), Transaction(AutocommitOff));
        assert_eq!(
            classify("SET autocommit = 'OFF'"),
            Transaction(AutocommitOff)
        );
        assert_eq!(
            classify("SET autocommit = \"on\""),
            Transaction(AutocommitOn)
        );
        assert_eq!(classify("SET autocommit = '1'"), Transaction(AutocommitOn));
        assert_eq!(classify("SET autocommit = '0'"), Transaction(AutocommitOff));
        assert_eq!(classify("set autocommit = true"), Transaction(AutocommitOn));
        assert_eq!(
            classify("SET autocommit = FALSE ;"),
            Transaction(AutocommitOff)
        );
        assert_eq!(classify("SET autocommit = 'maybe'"), Other);
    }

    #[test]
    fn with_statements() {
        assert_eq!(
            classify("WITH RECURSIVE r(n) AS (SELECT 1 UNION ALL SELECT n+1 FROM r WHERE n < 3) SELECT * FROM r"),
            Read
        );
        assert_eq!(
            classify("with a as (select ')' x), b as (select '(' y) update t set c = 1"),
            Write
        );
        assert_eq!(
            classify("WITH a AS (SELECT 1 /* ) */ -- )\n) INSERT INTO t SELECT * FROM a"),
            Write
        );
        assert_eq!(classify("WITH a AS MATERIALIZED (SELECT 1) TABLE a"), Read);
        // A data-modifying CTE makes the whole statement a write.
        assert_eq!(
            classify("WITH d AS (DELETE FROM t RETURNING *) SELECT * FROM d"),
            Write
        );
        assert_eq!(
            classify(
                "WITH a AS (WITH b AS (UPDATE t SET x=1 RETURNING x) SELECT * FROM b) SELECT 1"
            ),
            Write
        );
    }
}

#[cfg(test)]
mod pg_dialect {
    use super::StatementKind::*;
    use super::TxControl::*;
    use super::*;

    fn pg(s: &str) -> StatementKind {
        classify(s, Dialect::Postgres)
    }

    #[test]
    fn plan_cases() {
        assert_eq!(pg("WITH a AS (SELECT 'C:\\') DELETE FROM t"), Write);
        assert_eq!(pg("SELECT E'it\\'s'"), Read);
        assert_eq!(pg("WITH a AS (SELECT $$ ) $$) UPDATE t SET x=1"), Write);
        assert_eq!(pg("SELECT '#' "), Read);
        assert_eq!(pg("/*! DELETE FROM t */"), Other);
    }

    #[test]
    fn pg_lexing_rules() {
        // `#` is an operator in PG, not a comment.
        assert_eq!(pg("# x\nDELETE FROM t"), Other);
        // Backticks are ordinary characters, so `)` inside them closes the CTE.
        assert_eq!(pg("WITH a AS (SELECT 1 AS `) DELETE FROM t"), Write);
        // Tagged dollar quotes are opaque.
        assert_eq!(
            pg("WITH a AS (SELECT $q$ ') $q$) INSERT INTO t VALUES (1)"),
            Write
        );
        // E-strings honour backslash escapes; `e` must not be an identifier tail.
        assert_eq!(pg("WITH a AS (SELECT e'\\') ') UPDATE t SET x=1"), Write);
        // PG block comments nest.
        assert_eq!(pg("/* a /* b */ ) */ DELETE FROM t"), Write);
        // Same text under MySQL: `'C:\'` swallows the rest, so it is no write.
        assert_eq!(
            classify("WITH a AS (SELECT 'C:\\') DELETE FROM t", Dialect::MYSQL),
            Other
        );
    }

    const AT_E_STRING: &str = "WITH a AS (SELECT ARRAY['x'] <@e'{\"y\\') SELECT 1 /*\"}'), d AS (DELETE FROM t RETURNING 1) SELECT 1 -- */";

    #[test]
    fn literal_after_at_operator_lexes_like_split() {
        // `<@e'…'` is the operator `<@` then an E-string, as in `split`.
        assert_eq!(pg(AT_E_STRING), Write);
        assert_eq!(
            pg("WITH a AS (SELECT ARRAY[1] <@$$ ) $$), d AS (DELETE FROM t RETURNING 1) SELECT 1"),
            Write
        );
        assert_eq!(pg("SELECT @e'\\')' FROM t"), Read);
    }

    #[test]
    fn split_and_classify_agree_after_at() {
        let script = format!("{AT_E_STRING}\n;SELECT ARRAY['x'] <@e'\\';' ; DELETE FROM t");
        let stmts: Vec<String> = crate::sql::split(&script, Dialect::Postgres)
            .into_iter()
            .map(|s| s.text)
            .collect();
        assert_eq!(
            stmts,
            [AT_E_STRING, "SELECT ARRAY['x'] <@e'\\';'", "DELETE FROM t"]
        );
        let kinds: Vec<StatementKind> = stmts.iter().map(|s| pg(s)).collect();
        assert_eq!(kinds, [Write, Read, Write]);
    }

    #[test]
    fn pg_statement_forms() {
        assert_eq!(pg("WITH x AS (SELECT 1) (SELECT * FROM x)"), Read);
        assert_eq!(pg("SELECT a INTO UNLOGGED newtab FROM t"), Ddl);
        assert_eq!(pg("EXPLAIN (ANALYZE) DELETE FROM t"), Write);
        assert_eq!(pg("END"), Transaction(Commit));
        assert_eq!(pg("ABORT"), Transaction(Rollback));
        assert_eq!(pg("COMMIT PREPARED 'x'"), Other);
        assert_eq!(pg("SET search_path = public"), Other);
        // PG has no `autocommit` setting (the server rejects it); kept as-is.
        assert_eq!(pg("SET autocommit = 0"), Transaction(AutocommitOff));
    }
}
