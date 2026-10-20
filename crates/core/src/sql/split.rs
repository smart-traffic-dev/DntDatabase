#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dialect {
    MySql,
    Postgres,
}

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
        if dialect == Dialect::MySql {
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
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i].is_ascii_whitespace() {
            i += 1;
        } else if let Some((Skipped::Comment, end)) = skip_at(b, i, dialect) {
            i = end;
        } else {
            return false;
        }
    }
    true
}

/// Recognises a MySQL client `DELIMITER <tok>` line starting at `i` (only
/// spaces/tabs may precede it on its line). Returns the new delimiter and the
/// offset just past the directive's line.
fn delimiter_directive(sql: &str, i: usize) -> Option<(&str, usize)> {
    const KW: &[u8] = b"delimiter";
    let b = sql.as_bytes();
    let line_start = b[..i]
        .iter()
        .rev()
        .take_while(|&&c| c != b'\n')
        .all(|&c| c == b' ' || c == b'\t');
    if !line_start || !b.get(i..i + KW.len())?.eq_ignore_ascii_case(KW) {
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

/// What [`skip_at`] stepped over.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Skipped {
    Comment,
    /// MySQL/MariaDB executable comment `/*!NNNNN … */` or `/*M!NNNNNN … */`:
    /// the server runs its body, so it is never blank.
    ExecComment,
    /// String literal, quoted identifier or dollar-quoted body.
    Quoted,
}

/// Shared lexer step: if a comment, string, quoted identifier, E-string or
/// dollar-quoted body starts at byte `i`, returns its kind and the offset just
/// past it (the input length when unterminated). Line comments end before
/// their newline. Every offset returned is a char boundary.
pub(super) fn skip_at(b: &[u8], i: usize, dialect: Dialect) -> Option<(Skipped, usize)> {
    let mysql = dialect == Dialect::MySql;
    let next = b.get(i + 1).copied();
    match b[i] {
        b'-' if next == Some(b'-') => {
            // MySQL needs whitespace (or end of input) after `--`.
            if mysql && !matches!(b.get(i + 2), None | Some(&(0..=b' '))) {
                return None;
            }
            Some((Skipped::Comment, line_end(b, i)))
        }
        b'#' if mysql => Some((Skipped::Comment, line_end(b, i))),
        b'/' if next == Some(b'*') => {
            let rest = &b[i + 2..];
            let kind = if mysql && (rest.starts_with(b"!") || rest.starts_with(b"M!")) {
                Skipped::ExecComment
            } else {
                Skipped::Comment
            };
            Some((kind, block_comment_end(b, i, !mysql)))
        }
        q @ (b'\'' | b'"') => Some((Skipped::Quoted, quote_end(b, i, q, mysql))),
        b'`' if mysql => Some((Skipped::Quoted, quote_end(b, i, b'`', false))),
        b'E' | b'e' if !mysql && next == Some(b'\'') && !prev_is_ident(b, i) => {
            Some((Skipped::Quoted, quote_end(b, i + 1, b'\'', true)))
        }
        b'$' if !mysql && !prev_is_ident(b, i) => {
            let body = dollar_tag_end(b, i)?;
            let tag = &b[i..body];
            let end = b[body..]
                .windows(tag.len())
                .position(|w| w == tag)
                .map_or(b.len(), |p| body + p + tag.len());
            Some((Skipped::Quoted, end))
        }
        _ => None,
    }
}

fn line_end(b: &[u8], i: usize) -> usize {
    b[i..]
        .iter()
        .position(|&c| c == b'\n')
        .map_or(b.len(), |p| i + p)
}

fn block_comment_end(b: &[u8], i: usize, nested: bool) -> usize {
    let mut depth = 1;
    let mut j = i + 2;
    while j < b.len() {
        if b[j..].starts_with(b"*/") {
            depth -= 1;
            j += 2;
            if depth == 0 {
                return j;
            }
        } else if nested && b[j..].starts_with(b"/*") {
            depth += 1;
            j += 2;
        } else {
            j += 1;
        }
    }
    b.len()
}

/// `open` is the index of the opening quote `q`. Doubled quotes always escape;
/// backslash escapes only when `backslash` is set.
fn quote_end(b: &[u8], open: usize, q: u8, backslash: bool) -> usize {
    let mut j = open + 1;
    while j < b.len() {
        if backslash && b[j] == b'\\' {
            j += 2;
        } else if b[j] == q {
            if b.get(j + 1) == Some(&q) {
                j += 2;
            } else {
                return j + 1;
            }
        } else {
            j += 1;
        }
    }
    b.len()
}

/// For `$tag$` / `$$` starting at `i`, returns the offset just past the opening tag.
fn dollar_tag_end(b: &[u8], i: usize) -> Option<usize> {
    let mut j = i + 1;
    if b.get(j)
        .is_some_and(|&c| c.is_ascii_alphabetic() || c == b'_' || c >= 0x80)
    {
        while b
            .get(j)
            .is_some_and(|&c| c.is_ascii_alphanumeric() || c == b'_' || c >= 0x80)
        {
            j += 1;
        }
    }
    (b.get(j) == Some(&b'$')).then_some(j + 1)
}

fn prev_is_ident(b: &[u8], i: usize) -> bool {
    i > 0 && {
        let c = b[i - 1];
        c.is_ascii_alphanumeric() || c == b'_' || c == b'$' || c >= 0x80
    }
}

#[cfg(test)]
mod tests {
    use super::Dialect::{MySql, Postgres};
    use super::*;

    fn cases() -> Vec<(&'static str, Dialect, Vec<&'static str>)> {
        vec![
            ("select 1; select 2", MySql, vec!["select 1", "select 2"]),
            (
                "select ';' ; select \"a;b\"; select `c;d`",
                MySql,
                vec!["select ';'", "select \"a;b\"", "select `c;d`"],
            ),
            (
                "select 1 -- x;y\n; select 2 # z;\n;",
                MySql,
                vec!["select 1 -- x;y", "select 2 # z;"],
            ),
            ("/* a;b */ select 1;", MySql, vec!["/* a;b */ select 1"]),
            (
                "DELIMITER $$\nCREATE PROCEDURE p() BEGIN SELECT 1; SELECT 2; END$$\nDELIMITER ;\nCALL p();",
                MySql,
                vec!["CREATE PROCEDURE p() BEGIN SELECT 1; SELECT 2; END", "CALL p()"],
            ),
            ("select 'it''s;'; select 2", MySql, vec!["select 'it''s;'", "select 2"]),
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
            ("  ;; \n", MySql, vec![]),
            ("select 'chưa đóng", MySql, vec!["select 'chưa đóng"]),
        ]
    }

    fn extra_cases() -> Vec<(&'static str, Dialect, Vec<&'static str>)> {
        vec![
            // MySQL strings honour backslash escapes.
            (
                "select 'a\\';b'; select 2",
                MySql,
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
                MySql,
                vec!["select 1--1", "select 2"],
            ),
            // Comment-only fragments are not statements.
            ("select 1; -- done\n", MySql, vec!["select 1"]),
            ("select 1; /* end */", Postgres, vec!["select 1"]),
            // DELIMITER is case-insensitive, may be indented, works with CRLF.
            (
                "delimiter //\r\nselect 1; select 2//\r\n  Delimiter ;\r\nselect 3;",
                MySql,
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
            ("select 1 /* ; ", MySql, vec!["select 1 /* ;"]),
            // MySQL executable comments are statements, not blank fragments.
            (
                "/*!40014 SET FOREIGN_KEY_CHECKS=0 */;\nselect 1;",
                MySql,
                vec!["/*!40014 SET FOREIGN_KEY_CHECKS=0 */", "select 1"],
            ),
            (
                "/*!50001 CREATE VIEW v AS select 1 */;",
                MySql,
                vec!["/*!50001 CREATE VIEW v AS select 1 */"],
            ),
            (
                "/*M!100100 SET a=1; */;\n/*!40101 SET NAMES utf8mb4 */",
                MySql,
                vec!["/*M!100100 SET a=1; */", "/*!40101 SET NAMES utf8mb4 */"],
            ),
            // In PG `/*!` is an ordinary comment.
            ("select 1; /*! x */", Postgres, vec!["select 1"]),
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
}
