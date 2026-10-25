use super::lexer::{skip_at, Dialect, Skipped};

/// One statement of a script. `text` is trimmed and excludes the delimiter;
/// `&input[start..end] == text` always holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Statement {
    pub text: String,
    pub start: usize,
    pub end: usize,
}

/// Splits a script into statements, honouring strings, quoted identifiers,
/// comments, PG dollar quotes / E-strings and MySQL `DELIMITER` directives.
/// Fragments that contain only whitespace and comments are dropped.
pub fn split(sql: &str, dialect: Dialect) -> Vec<Statement> {
    let b = sql.as_bytes();
    let mut out = Vec::new();
    let mut delim = ";";
    let mut seg = 0;
    let mut i = 0;
    while i < b.len() {
        // Neither a delimiter nor a lexer construct starts with whitespace.
        if b[i].is_ascii_whitespace() {
            i += 1;
            continue;
        }
        if dialect.is_mysql() && b[i].eq_ignore_ascii_case(&b'd') {
            if let Some((tok, next)) = delimiter_directive(sql, i) {
                push(&mut out, sql, seg, i, dialect);
                delim = tok;
                seg = next;
                i = next;
                continue;
            }
        }
        if let Some((_, end)) = skip_at(b, i, dialect) {
            i = end;
        } else if b[i..].starts_with(delim.as_bytes()) {
            push(&mut out, sql, seg, i, dialect);
            i += delim.len();
            seg = i;
        } else {
            i += 1;
        }
    }
    push(&mut out, sql, seg, b.len(), dialect);
    out
}

fn push(out: &mut Vec<Statement>, sql: &str, from: usize, to: usize, dialect: Dialect) {
    let raw = &sql[from..to];
    let text = raw.trim();
    if is_blank(text, dialect) {
        return;
    }
    let start = from + (raw.len() - raw.trim_start().len());
    out.push(Statement {
        text: text.to_string(),
        start,
        end: start + text.len(),
    });
}

/// True when `s` holds nothing but whitespace and comments.
fn is_blank(s: &str, dialect: Dialect) -> bool {
    leading_blank_len(s, dialect) == s.len()
}

/// Byte length of the whitespace and plain comments that open `s`, i.e. the offset
/// of its first significant byte (`s.len()` when there is none). MySQL executable
/// comments (`/*! … */`) are significant: the server runs them.
pub fn leading_blank_len(s: &str, dialect: Dialect) -> usize {
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i].is_ascii_whitespace() {
            i += 1;
        } else if let Some((Skipped::Comment, end)) = skip_at(b, i, dialect) {
            i = end;
        } else {
            return i;
        }
    }
    b.len()
}

/// `stmt` with every plain comment replaced by one space. Strings, quoted identifiers,
/// dollar-quoted bodies and MySQL executable comments (`/*! … */`, which the server
/// runs) are kept verbatim; lexing follows the same per-dialect rules as [`split`].
pub fn strip_comments(stmt: &str, dialect: Dialect) -> String {
    let b = stmt.as_bytes();
    let mut out = String::with_capacity(stmt.len());
    let (mut i, mut kept) = (0, 0);
    while i < b.len() {
        match skip_at(b, i, dialect) {
            Some((Skipped::Comment, end)) => {
                out.push_str(&stmt[kept..i]);
                out.push(' ');
                i = end;
                kept = end;
            }
            Some((_, end)) => i = end,
            None => i += 1,
        }
    }
    out.push_str(&stmt[kept..]);
    out
}

/// Recognises a MySQL client `DELIMITER <tok>` line starting at `i` (only
/// spaces/tabs may precede it on its line). Returns the new delimiter and the
/// offset just past the directive's line.
pub(super) fn delimiter_directive(sql: &str, i: usize) -> Option<(&str, usize)> {
    const KW: &[u8] = b"delimiter";
    let b = sql.as_bytes();
    // Match the keyword before scanning back over the line's leading blanks: only a
    // `d` preceded by blanks pays for that scan, and the scan stops at the first
    // non-blank, so the blanks of a line are scanned at most once in all.
    if !b.get(i..i + KW.len())?.eq_ignore_ascii_case(KW) {
        return None;
    }
    let line_start = b[..i]
        .iter()
        .rev()
        .take_while(|&&c| c != b'\n')
        .all(|&c| c == b' ' || c == b'\t');
    if !line_start {
        return None;
    }
    let mut j = i + KW.len();
    if !matches!(b.get(j), Some(b' ' | b'\t')) {
        return None;
    }
    while matches!(b.get(j), Some(b' ' | b'\t')) {
        j += 1;
    }
    let tok_start = j;
    while j < b.len() && !b[j].is_ascii_whitespace() {
        j += 1;
    }
    if j == tok_start {
        return None;
    }
    let next = b[j..]
        .iter()
        .position(|&c| c == b'\n')
        .map_or(b.len(), |p| j + p + 1);
    Some((&sql[tok_start..j], next))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sql::{classify, MySqlMode, StatementKind};
    const MYSQL: Dialect = Dialect::MYSQL;
    use Dialect::Postgres;

    #[test]
    fn leading_blank_len_skips_whitespace_and_plain_comments() {
        assert_eq!(leading_blank_len("-- c\n  # d\nSELECT 1", MYSQL), 11);
        assert_eq!(leading_blank_len("/* a /* b */ */ SELECT", Postgres), 16);
        // `#` is not a comment in PG; executable comments are significant.
        assert_eq!(leading_blank_len("# x", Postgres), 0);
        assert_eq!(leading_blank_len(" /*!40101 SET x=1 */", MYSQL), 1);
        assert_eq!(leading_blank_len(" -- only\n", MYSQL), 9);
        assert_eq!(leading_blank_len("", MYSQL), 0);
    }

    fn cases() -> Vec<(&'static str, Dialect, Vec<&'static str>)> {
        vec![
            ("select 1; select 2", MYSQL, vec!["select 1", "select 2"]),
            (
                "select ';' ; select \"a;b\"; select `c;d`",
                MYSQL,
                vec!["select ';'", "select \"a;b\"", "select `c;d`"],
            ),
            (
                "select 1 -- x;y\n; select 2 # z;\n;",
                MYSQL,
                vec!["select 1 -- x;y", "select 2 # z;"],
            ),
            ("/* a;b */ select 1;", MYSQL, vec!["/* a;b */ select 1"]),
            (
                "DELIMITER $$\nCREATE PROCEDURE p() BEGIN SELECT 1; SELECT 2; END$$\nDELIMITER ;\nCALL p();",
                MYSQL,
                vec!["CREATE PROCEDURE p() BEGIN SELECT 1; SELECT 2; END", "CALL p()"],
            ),
            ("select 'it''s;'; select 2", MYSQL, vec!["select 'it''s;'", "select 2"]),
            (
                "create function f() returns int as $$ begin return 1; end $$ language plpgsql; select 1",
                Postgres,
                vec![
                    "create function f() returns int as $$ begin return 1; end $$ language plpgsql",
                    "select 1",
                ],
            ),
            (
                "select $tag$ ; $$ ; $tag$; select 2",
                Postgres,
                vec!["select $tag$ ; $$ ; $tag$", "select 2"],
            ),
            ("select E'a\\';b'; select 2", Postgres, vec!["select E'a\\';b'", "select 2"]),
            (
                "select 1 # not a comment in pg; select 2",
                Postgres,
                vec!["select 1 # not a comment in pg", "select 2"],
            ),
            ("  ;; \n", MYSQL, vec![]),
            ("select 'chưa đóng", MYSQL, vec!["select 'chưa đóng"]),
        ]
    }

    fn extra_cases() -> Vec<(&'static str, Dialect, Vec<&'static str>)> {
        vec![
            // MySQL strings honour backslash escapes.
            (
                "select 'a\\';b'; select 2",
                MYSQL,
                vec!["select 'a\\';b'", "select 2"],
            ),
            // In PG a plain string has no backslash escapes.
            (
                "select 'C:\\'; select 2",
                Postgres,
                vec!["select 'C:\\'", "select 2"],
            ),
            // PG block comments nest.
            (
                "/* a /* b; */ c; */ select 1; select 2",
                Postgres,
                vec!["/* a /* b; */ c; */ select 1", "select 2"],
            ),
            // `$1` is a parameter, not a dollar quote; `a$b$` is an identifier.
            (
                "select $1; select a$b$; select 3",
                Postgres,
                vec!["select $1", "select a$b$", "select 3"],
            ),
            // MySQL `--` needs trailing whitespace to start a comment.
            (
                "select 1--1; select 2",
                MYSQL,
                vec!["select 1--1", "select 2"],
            ),
            // Comment-only fragments are not statements.
            ("select 1; -- done\n", MYSQL, vec!["select 1"]),
            ("select 1; /* end */", Postgres, vec!["select 1"]),
            // DELIMITER is case-insensitive, may be indented, works with CRLF.
            (
                "delimiter //\r\nselect 1; select 2//\r\n  Delimiter ;\r\nselect 3;",
                MYSQL,
                vec!["select 1; select 2", "select 3"],
            ),
            // DELIMITER is not a directive in PG.
            (
                "DELIMITER $$\nselect 1",
                Postgres,
                vec!["DELIMITER $$\nselect 1"],
            ),
            // Unterminated constructs swallow the rest of the input.
            ("select $x$ ; ", Postgres, vec!["select $x$ ;"]),
            ("select 1 /* ; ", MYSQL, vec!["select 1 /* ;"]),
            // MySQL executable comments are statements, not blank fragments.
            (
                "/*!40014 SET FOREIGN_KEY_CHECKS=0 */;\nselect 1;",
                MYSQL,
                vec!["/*!40014 SET FOREIGN_KEY_CHECKS=0 */", "select 1"],
            ),
            (
                "/*!50001 CREATE VIEW v AS select 1 */;",
                MYSQL,
                vec!["/*!50001 CREATE VIEW v AS select 1 */"],
            ),
            (
                "/*M!100100 SET a=1; */;\n/*!40101 SET NAMES utf8mb4 */",
                MYSQL,
                vec!["/*M!100100 SET a=1; */", "/*!40101 SET NAMES utf8mb4 */"],
            ),
            // In PG `/*!` is an ordinary comment.
            ("select 1; /*! x */", Postgres, vec!["select 1"]),
            // Multibyte text inside strings and dollar tags (backlog T3).
            (
                "select 'ư;đ'; select 2",
                MYSQL,
                vec!["select 'ư;đ'", "select 2"],
            ),
            (
                "select '\\ư'; select 'đ'",
                MYSQL,
                vec!["select '\\ư'", "select 'đ'"],
            ),
            (
                "select $thẻ$ ; $thẻ$; select 2",
                Postgres,
                vec!["select $thẻ$ ; $thẻ$", "select 2"],
            ),
        ]
    }

    #[test]
    fn splits_statements() {
        for (input, dialect, expected) in cases().into_iter().chain(extra_cases()) {
            let texts: Vec<String> = split(input, dialect).into_iter().map(|s| s.text).collect();
            assert_eq!(texts, expected, "input: {input:?} ({dialect:?})");
        }
    }

    #[test]
    fn strips_comments_but_not_strings_or_exec_comments() {
        let cases = [
            ("/* c */ USE shop", MYSQL, "USE shop"),
            ("-- c\nUSE `a b` # tail", MYSQL, "USE `a b`"),
            ("USE x /* end */;", MYSQL, "USE x  ;"),
            (
                "select '/* kept */', `--x`",
                MYSQL,
                "select '/* kept */', `--x`",
            ),
            ("/*!40101 USE x */", MYSQL, "/*!40101 USE x */"),
            ("/* a /* b */ c */ select 1", Postgres, "select 1"),
            ("select '#'", Postgres, "select '#'"),
            ("select $$--$$ -- c", Postgres, "select $$--$$"),
            ("/* đ */ USE thẻ", MYSQL, "USE thẻ"),
        ];
        for (input, dialect, expected) in cases {
            assert_eq!(
                strip_comments(input, dialect).trim(),
                expected,
                "input: {input:?} ({dialect:?})"
            );
        }
    }

    fn texts(sql: &str, dialect: Dialect) -> Vec<String> {
        split(sql, dialect).into_iter().map(|s| s.text).collect()
    }

    #[test]
    fn ansi_quotes_makes_double_quote_an_identifier() {
        let ansi = Dialect::MySql(MySqlMode {
            ansi_quotes: true,
            ..Default::default()
        });
        assert_eq!(
            texts(r#"select "a\"; select 2"#, ansi),
            [r#"select "a\""#, "select 2"]
        );
        assert_eq!(
            texts(r#"select "a\"; select 2"#, Dialect::MYSQL),
            [r#"select "a\"; select 2"#]
        );
    }

    #[test]
    fn no_backslash_escapes_in_strings() {
        let nbe = Dialect::MySql(MySqlMode {
            no_backslash_escapes: true,
            ..Default::default()
        });
        assert_eq!(
            texts(r"select 'C:\'; select 2", nbe),
            [r"select 'C:\'", "select 2"]
        );
        assert_eq!(texts(r"select 'C:\'; select 2", Dialect::MYSQL).len(), 1);
        assert_eq!(
            classify(r"WITH a AS (SELECT 'C:\') DELETE FROM t", nbe),
            StatementKind::Write
        );
    }

    #[test]
    fn mode_flags_reach_strip_comments() {
        let ansi = Dialect::MySql(MySqlMode {
            ansi_quotes: true,
            ..Default::default()
        });
        let nbe = Dialect::MySql(MySqlMode {
            no_backslash_escapes: true,
            ..Default::default()
        });
        // Under ANSI_QUOTES `"a\"` closes, so the comment after it is stripped.
        assert_eq!(
            strip_comments(r#"select "a\" /* c */"#, ansi).trim(),
            r#"select "a\""#
        );
        assert_eq!(
            strip_comments(r#"select "a\" /* c */"#, Dialect::MYSQL).trim(),
            r#"select "a\" /* c */"#
        );
        assert_eq!(
            strip_comments(r#"select "C:\" -- c"#, nbe).trim(),
            r#"select "C:\""#
        );
        // Doubled quotes still escape under both flags.
        assert_eq!(
            texts(r#"select "a""b;"; select 2"#, ansi),
            [r#"select "a""b;""#, "select 2"]
        );
        assert_eq!(
            texts("select 'it''s;'; select 2", nbe),
            ["select 'it''s;'", "select 2"]
        );
        // Backticks never had backslash escapes.
        assert_eq!(
            texts(r"select `a\`; select 2", ansi),
            [r"select `a\`", "select 2"]
        );
    }

    /// `DELIMITER` detection must not rescan the leading blanks of a line at every
    /// byte (that was quadratic in the line length).
    #[test]
    fn long_blank_line_splits_in_linear_time() {
        let spaces = format!("{}SELECT 1", " ".repeat(1 << 20));
        let tabs = format!("{}delimiter //\nSELECT 1//", "\t".repeat(1 << 20));
        let dees = format!("{}SELECT 1", " d".repeat(1 << 19));
        for (sql, want) in [
            (spaces, "SELECT 1"),
            (tabs, "SELECT 1"),
            (dees.clone(), dees.trim()),
        ] {
            let started = std::time::Instant::now();
            assert_eq!(texts(&sql, MYSQL), [want]);
            assert!(
                started.elapsed() < std::time::Duration::from_millis(500),
                "{:?}",
                started.elapsed()
            );
        }
    }

    #[test]
    fn offsets_point_into_input() {
        for (input, dialect, _) in cases().into_iter().chain(extra_cases()) {
            for s in split(input, dialect) {
                assert_eq!(
                    &input[s.start..s.end],
                    s.text,
                    "input: {input:?} ({dialect:?})"
                );
            }
        }
    }

    /// PostgreSQL's scanner ends a `--` comment at `\r` as well as `\n`
    /// (`newline [\n\r]` in scan.l); MySQL only at `\n`.
    #[test]
    fn pg_line_comment_ends_at_carriage_return() {
        let input = "SELECT 1 --\r; DROP TABLE t";
        let stmts = split(input, Postgres);
        let texts: Vec<&str> = stmts.iter().map(|s| s.text.as_str()).collect();
        assert_eq!(texts, ["SELECT 1 --", "DROP TABLE t"]);
        assert_eq!(classify(&stmts[1].text, Postgres), StatementKind::Ddl);
        for s in &stmts {
            assert_eq!(&input[s.start..s.end], s.text);
        }
        // MySQL keeps the `;` inside the comment, as its server does.
        assert_eq!(split(input, MYSQL).len(), 1);
        // A CRLF script splits exactly as with LF.
        for (crlf, dialect) in [
            ("SELECT 1 -- c\r\nSELECT 2;", Postgres),
            ("SELECT 1; -- c\r\nSELECT 2;\r\n", Postgres),
            ("SELECT 1; -- c\r\nSELECT 2;\r\n", MYSQL),
        ] {
            let lf = crlf.replace("\r\n", "\n");
            let a: Vec<String> = split(crlf, dialect).into_iter().map(|s| s.text).collect();
            let b: Vec<String> = split(&lf, dialect).into_iter().map(|s| s.text).collect();
            assert_eq!(a.len(), b.len(), "{crlf:?}");
            for (x, y) in a.iter().zip(&b) {
                assert_eq!(x.replace("\r\n", "\n"), *y, "{crlf:?}");
            }
            for s in split(crlf, dialect) {
                assert_eq!(&crlf[s.start..s.end], s.text);
            }
        }
    }
}
