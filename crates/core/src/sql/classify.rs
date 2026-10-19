use super::split::{skip_at, Dialect, Skipped};

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
/// opening parens). Lexing uses MySQL rules, since no dialect is given.
pub fn classify(stmt: &str) -> StatementKind {
    let toks = tokenize(stmt);
    let start = toks.iter().take_while(|t| t.tok == Tok::Open).count();
    let toks = &toks[start..];
    let Some(first) = word_at(toks, 0) else {
        return StatementKind::Other;
    };
    let tx = StatementKind::Transaction;
    match first.to_ascii_uppercase().as_str() {
        "SELECT" | "SHOW" | "EXPLAIN" | "DESCRIBE" | "DESC" | "VALUES" | "TABLE" => {
            StatementKind::Read
        }
        "INSERT" | "UPDATE" | "DELETE" | "REPLACE" | "MERGE" | "CALL" | "DO" | "LOAD" | "COPY" => {
            StatementKind::Write
        }
        "CREATE" | "ALTER" | "DROP" | "TRUNCATE" | "RENAME" | "GRANT" | "REVOKE" | "COMMENT" => {
            StatementKind::Ddl
        }
        "BEGIN" => tx(TxControl::Begin),
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
        "SET" => set_autocommit(&toks[1..]).map_or(StatementKind::Other, tx),
        "WITH" => classify_with(stmt, &toks[1..]),
        _ => StatementKind::Other,
    }
}

/// After `WITH`: skips CTE definitions (balanced parens at depth 0) until the
/// main statement keyword. A data-modifying CTE body also makes it a write.
fn classify_with(src: &str, toks: &[Token]) -> StatementKind {
    let mut depth = 0usize;
    let mut body_start = 0;
    let mut cte_writes = false;
    for t in toks {
        match t.tok {
            Tok::Open => {
                if depth == 0 {
                    body_start = t.end;
                }
                depth += 1;
            }
            Tok::Close if depth > 0 => {
                depth -= 1;
                if depth == 0 && classify(&src[body_start..t.start]) == StatementKind::Write {
                    cte_writes = true;
                }
            }
            Tok::Word(w) if depth == 0 => match w.to_ascii_uppercase().as_str() {
                "INSERT" | "UPDATE" | "DELETE" | "MERGE" | "REPLACE" => {
                    return StatementKind::Write
                }
                "SELECT" | "VALUES" | "TABLE" if cte_writes => return StatementKind::Write,
                "SELECT" | "VALUES" | "TABLE" => return StatementKind::Read,
                _ => {}
            },
            _ => {}
        }
    }
    if cte_writes {
        StatementKind::Write
    } else {
        StatementKind::Other
    }
}

/// `SET` assignments; the last non-global `autocommit` assignment wins.
fn set_autocommit(toks: &[Token]) -> Option<TxControl> {
    toks.rsplit(|t| t.tok == Tok::Punct(b','))
        .find_map(autocommit_assignment)
}

/// `[SESSION|LOCAL] (autocommit | @@autocommit | @@session.autocommit | @@local.autocommit)
/// (= | := | TO) (0|1|ON|OFF)`
fn autocommit_assignment(a: &[Token]) -> Option<TxControl> {
    let mut i = 0;
    if is_kw(word_at(a, 0), "SESSION") || is_kw(word_at(a, 0), "LOCAL") {
        i += 1;
    }
    let target = word_at(a, i)?;
    if is_kw(Some(target), "autocommit") || is_kw(Some(target), "@@autocommit") {
        i += 1;
    } else if (is_kw(Some(target), "@@session") || is_kw(Some(target), "@@local"))
        && a.get(i + 1).map(|t| t.tok) == Some(Tok::Punct(b'.'))
        && is_kw(word_at(a, i + 2), "autocommit")
    {
        i += 3;
    } else {
        return None;
    }
    if a.get(i).map(|t| t.tok) == Some(Tok::Punct(b':')) {
        i += 1;
    }
    if a.get(i).map(|t| t.tok) == Some(Tok::Punct(b'=')) || is_kw(word_at(a, i), "TO") {
        i += 1;
    } else {
        return None;
    }
    if i + 1 != a.len() {
        return None;
    }
    match word_at(a, i)?.to_ascii_uppercase().as_str() {
        "0" | "OFF" => Some(TxControl::AutocommitOff),
        "1" | "ON" => Some(TxControl::AutocommitOn),
        _ => None,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tok<'a> {
    Word(&'a str),
    Open,
    Close,
    Quoted,
    Punct(u8),
}

#[derive(Debug, Clone, Copy)]
struct Token<'a> {
    tok: Tok<'a>,
    start: usize,
    end: usize,
}

/// Significant tokens of `s` (whitespace and comments dropped), built on the
/// splitter's lexer so strings and comments never affect paren depth.
fn tokenize(s: &str) -> Vec<Token<'_>> {
    let b = s.as_bytes();
    let is_word = |c: u8| c.is_ascii_alphanumeric() || matches!(c, b'_' | b'$' | b'@') || c >= 0x80;
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        let start = i;
        let tok = if let Some((kind, end)) = skip_at(b, i, Dialect::MySql) {
            i = end;
            match kind {
                Skipped::Comment => continue,
                Skipped::Quoted => Tok::Quoted,
            }
        } else if b[i].is_ascii_whitespace() {
            i += 1;
            continue;
        } else if is_word(b[i]) {
            while i < b.len() && is_word(b[i]) {
                i += 1;
            }
            Tok::Word(&s[start..i])
        } else {
            i += 1;
            match b[start] {
                b'(' => Tok::Open,
                b')' => Tok::Close,
                c => Tok::Punct(c),
            }
        };
        out.push(Token { tok, start, end: i });
    }
    out
}

fn word_at<'a>(toks: &[Token<'a>], i: usize) -> Option<&'a str> {
    match toks.get(i)?.tok {
        Tok::Word(w) => Some(w),
        _ => None,
    }
}

fn is_kw(word: Option<&str>, kw: &str) -> bool {
    word.is_some_and(|w| w.eq_ignore_ascii_case(kw))
}

#[cfg(test)]
mod tests {
    use super::StatementKind::*;
    use super::TxControl::*;
    use super::*;

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
