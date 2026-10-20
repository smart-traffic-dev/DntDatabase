//! One split statement that the server still runs as several: a MySQL `DELIMITER`
//! left on (a forgotten `DELIMITER ;`) keeps depth-0 `;` inside one statement, and
//! with multi-statements on the server runs every piece.

use super::lexer::{readings, tokenize, Dialect, TokKind, Token};
use super::split::split;

/// Whether `stmt` (one split statement) holds more than one statement for the server:
/// some reading of it, split again with the default `;` delimiter, gives several
/// pieces ([`resplit_reading`]). Compound statements never re-split: their `;` belong
/// to the body. A PostgreSQL statement never re-splits (PG has no `DELIMITER`, so
/// `split` already cut at every depth-0 `;`).
pub fn resplits(stmt: &str, dialect: Dialect) -> bool {
    readings(stmt, dialect).iter().any(|r| {
        let toks = tokenize(r, dialect);
        resplit_reading(r, &toks, dialect).is_some()
    })
}

/// The statements of reading `r` (tokens `toks`) split again with the default `;`
/// delimiter, when there are several (see [`resplits`]).
pub fn resplit_reading(r: &str, toks: &[Token], dialect: Dialect) -> Option<Vec<String>> {
    if compound_body(toks) {
        return None;
    }
    let found = split(r, dialect);
    (found.len() > 1).then(|| found.into_iter().map(|s| s.text).collect())
}

/// MySQL/MariaDB compound statements, whose body holds `;`:
/// `CREATE [OR REPLACE] [DEFINER = user] [AGGREGATE] PROCEDURE|FUNCTION|TRIGGER|EVENT`
/// and `BEGIN NOT ATOMIC`. A user is `CURRENT_USER[()]` or a name (word or quoted)
/// with an optional `@host`.
fn compound_body(toks: &[Token]) -> bool {
    if is_word(toks, 0, "BEGIN") && is_word(toks, 1, "NOT") && is_word(toks, 2, "ATOMIC") {
        return true;
    }
    if !is_word(toks, 0, "CREATE") {
        return false;
    }
    let mut i = 1;
    if is_word(toks, i, "OR") && is_word(toks, i + 1, "REPLACE") {
        i += 2;
    }
    if is_word(toks, i, "DEFINER") {
        if !is_punct(toks, i + 1, b'=') {
            return false;
        }
        match skip_user(toks, i + 2) {
            Some(next) => i = next,
            None => return false,
        }
    }
    if is_word(toks, i, "AGGREGATE") {
        i += 1;
    }
    ["PROCEDURE", "FUNCTION", "TRIGGER", "EVENT"]
        .iter()
        .any(|k| is_word(toks, i, k))
}

/// The index past the user spec of a `DEFINER =` starting at token `i`.
fn skip_user(toks: &[Token], mut i: usize) -> Option<usize> {
    let first = toks.get(i)?;
    match first.kind {
        TokKind::Word(w) if w.eq_ignore_ascii_case("CURRENT_USER") => {
            i += 1;
            if toks.get(i).is_some_and(|t| t.kind == TokKind::Open)
                && toks.get(i + 1).is_some_and(|t| t.kind == TokKind::Close)
            {
                i += 2;
            }
            return Some(i);
        }
        TokKind::Word(_) | TokKind::Quoted => {}
        _ => return None,
    }
    let ends_at = |t: &Token| matches!(t.kind, TokKind::Word(w) if w.ends_with('@'));
    let mut prev = *first;
    i += 1;
    // The host: `@host` / `@'host'` (`@` is a word byte, so `@` may stand alone or
    // open the next word).
    while let Some(t) = toks.get(i) {
        let host = match t.kind {
            TokKind::Word(w) if w.starts_with('@') => true,
            TokKind::Word(_) | TokKind::Quoted => ends_at(&prev),
            _ => false,
        };
        if !host {
            break;
        }
        prev = *t;
        i += 1;
    }
    Some(i)
}

fn is_word(toks: &[Token], i: usize, kw: &str) -> bool {
    matches!(toks.get(i).map(|t| t.kind), Some(TokKind::Word(w)) if w.eq_ignore_ascii_case(kw))
}

fn is_punct(toks: &[Token], i: usize, c: u8) -> bool {
    toks.get(i).is_some_and(|t| t.kind == TokKind::Punct(c))
}

#[cfg(test)]
mod tests {
    use super::*;

    const MY: Dialect = Dialect::MYSQL;

    #[test]
    fn several_pieces_resplit() {
        for s in [
            "BEGIN; INSERT INTO t VALUES (1)",
            "SELECT 1; COMMIT",
            "SELECT 1; COMMIT; UPDATE t SET a = 1",
            "/*!40101 SELECT 1; DELETE FROM t */",
        ] {
            assert!(resplits(s, MY), "{s}");
        }
        for s in [
            "SELECT 1",
            "SELECT 1;",
            "SELECT ';'",
            "CREATE PROCEDURE p() BEGIN SELECT 1; END",
            "BEGIN NOT ATOMIC SELECT 1; SELECT 2; END",
        ] {
            assert!(!resplits(s, MY), "{s}");
        }
    }

    /// The PG driver relies on this: it never needs to ask the server for the
    /// transaction state after a statement.
    #[test]
    fn pg_statements_never_resplit() {
        for text in [
            "SELECT 1; BEGIN",
            "BEGIN; INSERT INTO t VALUES (1); COMMIT",
            "DO $$ BEGIN PERFORM 1; END $$; SELECT 1",
        ] {
            for s in split(text, Dialect::Postgres) {
                assert!(!resplits(&s.text, Dialect::Postgres), "{}", s.text);
            }
        }
    }
}
