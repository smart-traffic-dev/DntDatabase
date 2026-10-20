//! "Unbounded write" detection: statements that change every row of a table, which
//! the guard asks to confirm on every connection (see [`crate::guard`]).
//!
//! A statement is flagged when, read with the shared lexer (comments, strings and
//! quoted identifiers never count; paren groups are subqueries):
//!
//! - it is an `UPDATE` without a WHERE at paren depth 0 (MySQL multi-table
//!   `UPDATE a JOIN b … SET …` included; `ORDER BY` / `LIMIT` alone do not bound it);
//! - it is a `DELETE` without a WHERE at depth 0 (MySQL `DELETE t1, t2 FROM …` and
//!   `DELETE FROM t1 USING …`, PG `DELETE FROM t USING …` included);
//! - it is a `TRUNCATE`;
//! - its depth-0 WHERE is trivially true: the whole condition, or one operand of a
//!   depth-0 `OR`, is (inside any wrapping parens) `TRUE`, a non-zero integer
//!   (`WHERE 1`), or `x = x` with the same integer or `'string'` on both sides
//!   (`1=1`, `'a'='a'`). Nothing else is judged.
//!
//! Where the statement runs: after a PG/MySQL `WITH …` (the main statement, and every
//! data-modifying CTE body), after `EXPLAIN ANALYZE` (which runs it) and after MariaDB
//! `SET STATEMENT … FOR`. A WHERE inside a paren group (a subquery in `SET`, `USING`,
//! a join) is not the statement's WHERE.
//!
//! Like the guard, every [`readings`] of a MySQL executable comment is judged (so
//! `DELETE FROM t /*!50000 WHERE id = 1 */` is flagged: an older server drops the
//! WHERE), and a statement that the server runs as several (a `DELIMITER` left on,
//! see [`resplit_reading`]) is judged piece by piece. Compound statements (routine,
//! trigger and event bodies, `BEGIN NOT ATOMIC`) start with `CREATE` / `BEGIN` and are
//! never flagged; PG bodies are dollar-quoted strings.

use dnt_core::sql::{readings, resplit_reading, tokenize, Dialect, TokKind, Token};
use serde::{Deserialize, Serialize};

/// The statement that changes every row.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, specta::Type)]
#[serde(rename_all = "snake_case")]
pub enum WriteOp {
    Update,
    Delete,
    Truncate,
}

/// Why it changes every row.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, specta::Type)]
#[serde(rename_all = "snake_case")]
pub enum UnboundedReason {
    /// No WHERE at depth 0.
    NoWhere,
    /// A trivially-true WHERE (`WHERE 1`, `1=1`, `true`, `'a'='a'`).
    AlwaysTrue,
    /// `TRUNCATE` has no WHERE at all.
    Truncate,
}

/// One unbounded write of a statement.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, specta::Type)]
pub struct UnboundedWrite {
    pub op: WriteOp,
    pub reason: UnboundedReason,
    /// The target table(s) as written (quotes kept), comma-separated; empty when the
    /// statement names none.
    pub table: String,
}

/// Nesting bound of data-modifying CTE bodies (deeper ones are not looked into).
const MAX_DEPTH: usize = 32;

/// The unbounded writes of `stmt` (one planned statement), in order, without repeats.
pub fn unbounded_writes(stmt: &str, dialect: Dialect) -> Vec<UnboundedWrite> {
    let mut out = Vec::new();
    for r in readings(stmt, dialect) {
        let toks = tokenize(&r, dialect);
        if toks.is_empty() {
            continue;
        }
        match resplit_reading(&r, &toks, dialect) {
            Some(pieces) => {
                for p in pieces {
                    let ptoks = tokenize(&p, dialect);
                    scan(&p, &ptoks, dialect, 0, &mut out);
                }
            }
            None => scan(&r, &toks, dialect, 0, &mut out),
        }
    }
    let mut seen = Vec::with_capacity(out.len());
    for w in out {
        if !seen.contains(&w) {
            seen.push(w);
        }
    }
    seen
}

/// Whether `stmt` holds an unbounded write ([`unbounded_writes`] is not empty).
pub fn is_unbounded(stmt: &str, dialect: Dialect) -> bool {
    !unbounded_writes(stmt, dialect).is_empty()
}

fn word<'a>(toks: &[Token<'a>], i: usize) -> Option<&'a str> {
    match toks.get(i)?.kind {
        TokKind::Word(w) => Some(w),
        _ => None,
    }
}

fn is_word(toks: &[Token], i: usize, kw: &str) -> bool {
    word(toks, i).is_some_and(|w| w.eq_ignore_ascii_case(kw))
}

fn is_punct(toks: &[Token], i: usize, c: u8) -> bool {
    toks.get(i).is_some_and(|t| t.kind == TokKind::Punct(c))
}

/// The index of the `Close` matching the `Open` at `open` (`toks.len()` when none).
fn close_of(toks: &[Token], open: usize) -> usize {
    let mut depth = 0usize;
    for (i, t) in toks.iter().enumerate().skip(open) {
        match t.kind {
            TokKind::Open => depth += 1,
            TokKind::Close => {
                depth -= 1;
                if depth == 0 {
                    return i;
                }
            }
            _ => {}
        }
    }
    toks.len()
}

/// The depth-0 token indices of `toks` (paren groups are jumped over, their `Open`
/// included), stopping at a depth-0 `;`.
fn depth0(toks: &[Token]) -> Vec<usize> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < toks.len() {
        match toks[i].kind {
            TokKind::Punct(b';') => break,
            TokKind::Open => {
                out.push(i);
                i = close_of(toks, i) + 1;
            }
            _ => {
                out.push(i);
                i += 1;
            }
        }
    }
    out
}

/// Judges the statement made of `toks` (tokens of `src`).
fn scan(src: &str, toks: &[Token], dialect: Dialect, depth: usize, out: &mut Vec<UnboundedWrite>) {
    if depth > MAX_DEPTH {
        return;
    }
    let Some(start) = statement_start(toks, dialect) else {
        return;
    };
    let toks = &toks[start..];
    let Some(first) = word(toks, 0) else {
        return;
    };
    match first.to_ascii_uppercase().as_str() {
        "UPDATE" => update(src, toks, out),
        "DELETE" => delete(src, toks, out),
        "TRUNCATE" => out.push(UnboundedWrite {
            op: WriteOp::Truncate,
            reason: UnboundedReason::Truncate,
            table: truncate_tables(src, toks),
        }),
        "WITH" => with(src, toks, dialect, depth, out),
        _ => {}
    }
}

/// Where the statement that runs starts: past `EXPLAIN ANALYZE …` options and MariaDB
/// `SET STATEMENT … FOR`. `None` for a plain `EXPLAIN` (it runs nothing).
fn statement_start(toks: &[Token], dialect: Dialect) -> Option<usize> {
    let mut i = 0;
    loop {
        let rest = &toks[i..];
        let explain = is_word(rest, 0, "EXPLAIN")
            || (dialect.is_mysql() && (is_word(rest, 0, "DESC") || is_word(rest, 0, "DESCRIBE")));
        if explain {
            if !crate::guard::explain_analyze(rest, dialect) {
                return None;
            }
            // The analyzed statement: the first depth-0 statement keyword.
            let at = depth0(rest).into_iter().skip(1).find(|&k| {
                ["UPDATE", "DELETE", "TRUNCATE", "WITH", "SELECT", "INSERT"]
                    .iter()
                    .any(|kw| is_word(rest, k, kw))
            })?;
            i += at;
            continue;
        }
        if dialect.is_mysql() && is_word(rest, 0, "SET") && is_word(rest, 1, "STATEMENT") {
            let at = depth0(rest)
                .into_iter()
                .skip(2)
                .find(|&k| is_word(rest, k, "FOR"))?;
            i += at + 1;
            continue;
        }
        return Some(i);
    }
}

/// A qualified name (`a`, `a.b`, `` `a`.`b` ``, `"s"."t"`) at `i`: its text and the
/// index past it (a trailing `.*` and PG `*` are skipped). `None` when no name is there.
fn name_at<'s>(src: &'s str, toks: &[Token], i: usize) -> Option<(&'s str, usize)> {
    let is_name = |k: usize| {
        toks.get(k)
            .is_some_and(|t| matches!(t.kind, TokKind::Word(_) | TokKind::Quoted))
    };
    if !is_name(i) {
        return None;
    }
    let mut end = i;
    while is_punct(toks, end + 1, b'.') && is_name(end + 2) {
        end += 2;
    }
    let text = &src[toks[i].start..toks[end].end];
    let mut next = end + 1;
    if is_punct(toks, next, b'.') && is_punct(toks, next + 1, b'*') {
        next += 2;
    } else if is_punct(toks, next, b'*') {
        next += 1;
    }
    Some((text, next))
}

/// Skips the words in `kws` from `i`.
fn skip_words(toks: &[Token], mut i: usize, kws: &[&str]) -> usize {
    while kws.iter().any(|k| is_word(toks, i, k)) {
        i += 1;
    }
    i
}

/// A comma-separated list of table names from `i` (each may follow `ONLY`).
fn name_list(src: &str, toks: &[Token], mut i: usize) -> (Vec<String>, usize) {
    let mut names = Vec::new();
    loop {
        i = skip_words(toks, i, &["ONLY"]);
        let Some((name, next)) = name_at(src, toks, i) else {
            break;
        };
        names.push(name.to_string());
        i = next;
        if !is_punct(toks, i, b',') {
            break;
        }
        i += 1;
    }
    (names, i)
}

/// The statement's depth-0 WHERE (`toks`: tokens of `src`): `None` when bounded, else
/// why it is not.
fn where_status(src: &str, toks: &[Token]) -> Option<UnboundedReason> {
    let idx = depth0(toks);
    let Some(pos) = idx.iter().position(|&k| is_word(toks, k, "WHERE")) else {
        return Some(UnboundedReason::NoWhere);
    };
    let from = idx[pos] + 1;
    let to = idx[pos + 1..]
        .iter()
        .copied()
        .find(|&k| {
            ["ORDER", "LIMIT", "RETURNING"]
                .iter()
                .any(|kw| is_word(toks, k, kw))
        })
        .unwrap_or_else(|| idx.last().map_or(toks.len(), |&l| close_end(toks, l)));
    always_true(src, &toks[from..to.max(from)]).then_some(UnboundedReason::AlwaysTrue)
}

/// The index past the depth-0 token at `k` (past its group for an `Open`).
fn close_end(toks: &[Token], k: usize) -> usize {
    if toks[k].kind == TokKind::Open {
        (close_of(toks, k) + 1).min(toks.len())
    } else {
        k + 1
    }
}

/// Whether the condition `cond` (tokens of `src`) is trivially true (see the module
/// docs).
fn always_true(src: &str, cond: &[Token]) -> bool {
    let mut cond = cond;
    // Wrapping parens.
    while cond.first().is_some_and(|t| t.kind == TokKind::Open)
        && close_of(cond, 0) == cond.len() - 1
    {
        cond = &cond[1..cond.len() - 1];
    }
    // A depth-0 OR: any trivially-true operand makes it true.
    let idx = depth0(cond);
    let ors: Vec<usize> = idx
        .iter()
        .copied()
        .filter(|&k| is_word(cond, k, "OR"))
        .collect();
    if !ors.is_empty() {
        let mut from = 0;
        for k in ors.into_iter().chain(std::iter::once(cond.len())) {
            if always_true(src, &cond[from..k]) {
                return true;
            }
            from = k + 1;
        }
        return false;
    }
    match cond {
        [t] => match t.kind {
            TokKind::Word(w) => w.eq_ignore_ascii_case("TRUE") || nonzero_int(w),
            _ => false,
        },
        [a, eq, b] if eq.kind == TokKind::Punct(b'=') => same_literal(src, a, b),
        _ => false,
    }
}

fn nonzero_int(w: &str) -> bool {
    !w.is_empty() && w.bytes().all(|c| c.is_ascii_digit()) && w.bytes().any(|c| c != b'0')
}

fn is_int(w: &str) -> bool {
    !w.is_empty() && w.bytes().all(|c| c.is_ascii_digit())
}

/// The same integer, or the same single-quoted string, on both sides.
fn same_literal(src: &str, a: &Token, b: &Token) -> bool {
    match (a.kind, b.kind) {
        (TokKind::Word(x), TokKind::Word(y)) => is_int(x) && x == y,
        (TokKind::Quoted, TokKind::Quoted) => {
            let (x, y) = (&src[a.start..a.end], &src[b.start..b.end]);
            // Single-quoted strings only: `"a" = "a"` may compare columns (PG,
            // ANSI_QUOTES).
            x.len() >= 2 && x.starts_with('\'') && x.ends_with('\'') && x == y
        }
        _ => false,
    }
}

fn update(src: &str, toks: &[Token], out: &mut Vec<UnboundedWrite>) {
    let i = skip_words(toks, 1, &["LOW_PRIORITY", "IGNORE", "ONLY"]);
    let idx = depth0(toks);
    let set = idx
        .iter()
        .copied()
        .find(|&k| k >= i && is_word(toks, k, "SET"))
        .unwrap_or(toks.len());
    // The table references before SET: the first name and every name after a depth-0
    // `,` or `… JOIN`.
    let mut names = Vec::new();
    let mut expect = true;
    for &k in idx.iter().filter(|&&k| k >= i && k < set) {
        if expect {
            let at = skip_words(toks, k, &["ONLY", "LATERAL"]);
            if at == k {
                if let Some((name, _)) = name_at(src, toks, k) {
                    names.push(name.to_string());
                }
            }
            expect = at != k;
            continue;
        }
        if is_punct(toks, k, b',')
            || word(toks, k).is_some_and(|w| w.to_ascii_uppercase().ends_with("JOIN"))
        {
            expect = true;
        }
    }
    if let Some(reason) = where_status(src, &toks[set.min(toks.len())..]) {
        out.push(UnboundedWrite {
            op: WriteOp::Update,
            reason,
            table: names.join(", "),
        });
    }
}

fn delete(src: &str, toks: &[Token], out: &mut Vec<UnboundedWrite>) {
    let i = skip_words(toks, 1, &["LOW_PRIORITY", "QUICK", "IGNORE"]);
    let names = if is_word(toks, i, "FROM") {
        // `DELETE FROM t …`, `DELETE FROM t1, t2 USING …`
        name_list(src, toks, i + 1).0
    } else {
        // MySQL `DELETE t1, t2 FROM …`
        name_list(src, toks, i).0
    };
    if let Some(reason) = where_status(src, &toks[i.min(toks.len())..]) {
        out.push(UnboundedWrite {
            op: WriteOp::Delete,
            reason,
            table: names.join(", "),
        });
    }
}

fn truncate_tables(src: &str, toks: &[Token]) -> String {
    let i = skip_words(toks, 1, &["TABLE"]);
    name_list(src, toks, i).0.join(", ")
}

/// `WITH …`: every data-modifying CTE body, then the main statement.
fn with(src: &str, toks: &[Token], dialect: Dialect, depth: usize, out: &mut Vec<UnboundedWrite>) {
    let mut expect_name = true;
    let mut i = 1;
    while i < toks.len() {
        match toks[i].kind {
            TokKind::Punct(b';') => return,
            TokKind::Open => {
                let close = close_of(toks, i);
                let body =
                    i > 0 && (is_word(toks, i - 1, "AS") || is_word(toks, i - 1, "MATERIALIZED"));
                if body {
                    scan(
                        src,
                        &toks[i + 1..close.min(toks.len())],
                        dialect,
                        depth + 1,
                        out,
                    );
                }
                i = close + 1;
                continue;
            }
            TokKind::Punct(b',') => expect_name = true,
            TokKind::Word(w) if expect_name => {
                if !(i == 1 && w.eq_ignore_ascii_case("RECURSIVE")) {
                    expect_name = false;
                }
            }
            TokKind::Quoted => expect_name = false,
            TokKind::Word(w) => {
                let main = [
                    "UPDATE", "DELETE", "INSERT", "SELECT", "MERGE", "REPLACE", "VALUES", "TABLE",
                ];
                if main.iter().any(|k| w.eq_ignore_ascii_case(k)) {
                    scan(src, &toks[i..], dialect, depth + 1, out);
                    return;
                }
            }
            _ => {}
        }
        i += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use UnboundedReason::{AlwaysTrue, NoWhere};
    use WriteOp::{Delete, Truncate, Update};

    const MY: Dialect = Dialect::MYSQL;
    const PG: Dialect = Dialect::Postgres;
    const BOTH: [Dialect; 2] = [MY, PG];

    fn flags(s: &str, d: Dialect) -> Vec<(WriteOp, UnboundedReason, String)> {
        unbounded_writes(s, d)
            .into_iter()
            .map(|w| (w.op, w.reason, w.table))
            .collect()
    }

    fn one(
        op: WriteOp,
        reason: UnboundedReason,
        table: &str,
    ) -> Vec<(WriteOp, UnboundedReason, String)> {
        vec![(op, reason, table.to_string())]
    }

    #[test]
    fn update_and_delete_without_where_are_flagged() {
        for d in BOTH {
            assert_eq!(
                flags("UPDATE orders SET paid = 1", d),
                one(Update, NoWhere, "orders")
            );
            assert_eq!(
                flags("DELETE FROM orders", d),
                one(Delete, NoWhere, "orders")
            );
            assert_eq!(
                flags("update shop.orders set a = 1", d),
                one(Update, NoWhere, "shop.orders")
            );
            assert_eq!(
                flags("delete from ONLY_T", d),
                one(Delete, NoWhere, "ONLY_T")
            );
        }
        assert_eq!(
            flags("UPDATE LOW_PRIORITY IGNORE `shop`.`orders` SET a = 1", MY),
            one(Update, NoWhere, "`shop`.`orders`")
        );
        assert_eq!(
            flags("DELETE LOW_PRIORITY QUICK IGNORE FROM t PARTITION (p0)", MY),
            one(Delete, NoWhere, "t")
        );
        assert_eq!(
            flags(r#"UPDATE ONLY "public"."Orders" * SET a = 1"#, PG),
            one(Update, NoWhere, r#""public"."Orders""#)
        );
        assert_eq!(flags("DELETE FROM ONLY t", PG), one(Delete, NoWhere, "t"));
    }

    #[test]
    fn limit_and_order_alone_do_not_bound() {
        assert_eq!(
            flags("DELETE FROM t ORDER BY id LIMIT 10", MY),
            one(Delete, NoWhere, "t")
        );
        assert_eq!(
            flags("UPDATE t SET a = 1 LIMIT 1", MY),
            one(Update, NoWhere, "t")
        );
        assert_eq!(
            flags("DELETE FROM t RETURNING *", PG),
            one(Delete, NoWhere, "t")
        );
    }

    #[test]
    fn a_where_bounds_the_statement() {
        for d in BOTH {
            for s in [
                "UPDATE t SET a = 1 WHERE id = 5",
                "DELETE FROM t WHERE id IN (SELECT id FROM u)",
                "DELETE FROM t WHERE id IN (1, 2, 3)",
                "update t set a = (select max(b) from u) where id = 1",
                "UPDATE t SET a = 1 WHERE 1 = 0",
                "DELETE FROM t WHERE 0",
                "DELETE FROM t WHERE false",
                "DELETE FROM t WHERE 1 = 2",
                "DELETE FROM t WHERE a = a",
                "DELETE FROM t WHERE 1=1 AND id = 3",
                "DELETE FROM t WHERE (id = 3 OR id = 4)",
                "UPDATE t SET a = 1 WHERE id = 1 ORDER BY id LIMIT 1",
                "DELETE FROM t WHERE 'a' = 'b'",
            ] {
                assert_eq!(flags(s, d), [], "{s} ({d:?})");
            }
        }
        assert_eq!(flags(r#"DELETE FROM t WHERE "a" = "a""#, PG), []);
        assert_eq!(flags("DELETE FROM t WHERE CURRENT OF c", PG), []);
    }

    #[test]
    fn a_where_in_a_subquery_is_not_the_statements_where() {
        for d in BOTH {
            assert_eq!(
                flags("UPDATE t SET a = (SELECT b FROM u WHERE u.id = t.id)", d),
                one(Update, NoWhere, "t"),
                "{d:?}"
            );
            assert_eq!(
                flags("UPDATE t SET a = 1, b = (SELECT 1 WHERE true)", d),
                one(Update, NoWhere, "t")
            );
        }
        assert_eq!(
            flags("DELETE FROM t USING (SELECT id FROM u WHERE x = 1) s", PG),
            one(Delete, NoWhere, "t")
        );
        assert_eq!(
            flags("UPDATE t SET a = s.a FROM (SELECT * FROM u WHERE b) s", PG),
            one(Update, NoWhere, "t")
        );
    }

    #[test]
    fn trivially_true_where_is_flagged() {
        for d in BOTH {
            for s in [
                "DELETE FROM t WHERE 1",
                "DELETE FROM t WHERE 1=1",
                "DELETE FROM t WHERE 1 = 1",
                "DELETE FROM t WHERE true",
                "DELETE FROM t WHERE TRUE",
                "DELETE FROM t WHERE 'a'='a'",
                "DELETE FROM t WHERE (1 = 1)",
                "DELETE FROM t WHERE ((true))",
                "DELETE FROM t WHERE id = 5 OR 1=1",
                "DELETE FROM t WHERE 1 = 1 ORDER BY id LIMIT 5",
                "DELETE FROM t WHERE 2",
            ] {
                assert_eq!(flags(s, d), one(Delete, AlwaysTrue, "t"), "{s} ({d:?})");
            }
            assert_eq!(
                flags("UPDATE t SET a = 1 WHERE 1 = 1", d),
                one(Update, AlwaysTrue, "t")
            );
        }
    }

    #[test]
    fn where_in_comments_and_strings_does_not_count() {
        for (s, d) in [
            ("UPDATE t SET a = 1 -- WHERE id = 1", MY),
            ("UPDATE t SET a = 1 # WHERE id = 1", MY),
            ("UPDATE t SET a = 1 /* WHERE id = 1 */", MY),
            ("UPDATE t SET a = 'WHERE id = 1'", MY),
            ("UPDATE t SET a = \"WHERE id = 1\"", MY),
            ("UPDATE t SET `where` = 1", MY),
            ("UPDATE t SET a = 1 -- WHERE id = 1", PG),
            ("UPDATE t SET a = 1 /* a /* WHERE */ b */", PG),
            ("UPDATE t SET a = $$ WHERE $$", PG),
            ("UPDATE t SET a = E'\\' WHERE id = 1'", PG),
            ("UPDATE t SET \"where\" = 1", PG),
        ] {
            assert_eq!(flags(s, d), one(Update, NoWhere, "t"), "{s} ({d:?})");
        }
        // A `#` is no comment in PG; the WHERE there is real.
        assert_eq!(flags("DELETE FROM t WHERE a #> '{x}' = 'y'", PG), []);
    }

    #[test]
    fn executable_comments_follow_the_guard_readings() {
        // An older server ignores the body: the WHERE may be dropped.
        assert_eq!(
            flags("DELETE FROM t /*!50000 WHERE id = 1 */", MY),
            one(Delete, NoWhere, "t")
        );
        // The body may make the WHERE always true.
        assert_eq!(
            flags("DELETE FROM t WHERE id = 1 /*!50000 OR 1=1 */", MY),
            one(Delete, AlwaysTrue, "t")
        );
        // The whole statement in an executable comment.
        assert_eq!(
            flags("/*!40000 DELETE FROM t */", MY),
            one(Delete, NoWhere, "t")
        );
        assert_eq!(flags("/*!40000 DELETE FROM t WHERE id = 1 */", MY), []);
        // In PG `/*!` is a plain comment.
        assert_eq!(
            flags("DELETE FROM t /*! WHERE id = 1 */ WHERE id = 2", PG),
            []
        );
    }

    #[test]
    fn mysql_multi_table_forms() {
        assert_eq!(
            flags("UPDATE a JOIN b ON a.id = b.id SET a.x = b.x", MY),
            one(Update, NoWhere, "a, b")
        );
        assert_eq!(
            flags(
                "UPDATE a INNER JOIN b ON (a.id = b.id) SET a.x = 1 WHERE b.y = 2",
                MY
            ),
            []
        );
        assert_eq!(
            flags("UPDATE a, b SET a.x = b.x", MY),
            one(Update, NoWhere, "a, b")
        );
        assert_eq!(
            flags(
                "UPDATE a LEFT JOIN (SELECT id FROM c WHERE z) s ON s.id = a.id SET a.x = 1",
                MY
            ),
            one(Update, NoWhere, "a")
        );
        assert_eq!(
            flags("DELETE t1, t2 FROM t1 INNER JOIN t2 ON t1.id = t2.id", MY),
            one(Delete, NoWhere, "t1, t2")
        );
        assert_eq!(
            flags(
                "DELETE t1.* FROM t1 JOIN t2 ON t1.id = t2.id WHERE t2.gone",
                MY
            ),
            []
        );
        assert_eq!(
            flags(
                "DELETE FROM t1, t2 USING t1 INNER JOIN t2 ON t1.id = t2.id",
                MY
            ),
            one(Delete, NoWhere, "t1, t2")
        );
        assert_eq!(
            flags(
                "DELETE FROM t1 USING t1 JOIN t2 ON t1.id = t2.id WHERE t2.id > 3",
                MY
            ),
            []
        );
    }

    #[test]
    fn pg_delete_using_and_update_from() {
        assert_eq!(
            flags("DELETE FROM t USING u", PG),
            one(Delete, NoWhere, "t")
        );
        assert_eq!(flags("DELETE FROM t USING u WHERE t.id = u.id", PG), []);
        assert_eq!(
            flags("UPDATE t SET a = u.a FROM u", PG),
            one(Update, NoWhere, "t")
        );
        assert_eq!(
            flags("UPDATE t AS x SET a = 1 FROM u WHERE x.id = u.id", PG),
            []
        );
    }

    #[test]
    fn truncate_is_always_flagged() {
        for d in BOTH {
            assert_eq!(
                flags("TRUNCATE t", d),
                one(Truncate, UnboundedReason::Truncate, "t")
            );
            assert_eq!(
                flags("truncate table shop.t", d),
                one(Truncate, UnboundedReason::Truncate, "shop.t")
            );
        }
        assert_eq!(
            flags("TRUNCATE ONLY a, b * RESTART IDENTITY CASCADE", PG),
            one(Truncate, UnboundedReason::Truncate, "a, b")
        );
        assert_eq!(
            flags("TRUNCATE", PG),
            one(Truncate, UnboundedReason::Truncate, "")
        );
    }

    #[test]
    fn ctes() {
        for d in BOTH {
            assert_eq!(
                flags("WITH x AS (SELECT id FROM u WHERE a = 1) DELETE FROM t", d),
                one(Delete, NoWhere, "t"),
                "{d:?}"
            );
            assert_eq!(
                flags(
                    "WITH x AS (SELECT 1) UPDATE t SET a = 1 WHERE id IN (SELECT * FROM x)",
                    d
                ),
                []
            );
            assert_eq!(
                flags("with recursive x (n) as (select 1 union all select n + 1 from x where n < 3) update t set a = 1", d),
                one(Update, NoWhere, "t")
            );
        }
        // PG data-modifying CTE bodies.
        assert_eq!(
            flags(
                "WITH gone AS (DELETE FROM t RETURNING *) SELECT count(*) FROM gone",
                PG
            ),
            one(Delete, NoWhere, "t")
        );
        assert_eq!(
            flags(
                "WITH a AS (UPDATE t SET x = 1 WHERE id = 1 RETURNING *), b AS MATERIALIZED (DELETE FROM u) INSERT INTO log SELECT * FROM a",
                PG
            ),
            one(Delete, NoWhere, "u")
        );
        assert_eq!(flags("WITH x AS (SELECT 1) SELECT * FROM x", PG), []);
    }

    #[test]
    fn explain_analyze_runs_the_statement_plain_explain_does_not() {
        assert_eq!(flags("EXPLAIN DELETE FROM t", PG), []);
        assert_eq!(flags("EXPLAIN DELETE FROM t", MY), []);
        assert_eq!(
            flags("EXPLAIN ANALYZE DELETE FROM t", PG),
            one(Delete, NoWhere, "t")
        );
        assert_eq!(
            flags("EXPLAIN (ANALYZE, BUFFERS) UPDATE t SET a = 1", PG),
            one(Update, NoWhere, "t")
        );
        assert_eq!(flags("EXPLAIN (ANALYZE false) UPDATE t SET a = 1", PG), []);
        assert_eq!(
            flags("EXPLAIN ANALYZE FORMAT=TREE DELETE FROM t", MY),
            one(Delete, NoWhere, "t")
        );
    }

    #[test]
    fn mariadb_set_statement_for() {
        assert_eq!(
            flags("SET STATEMENT max_statement_time = 1 FOR DELETE FROM t", MY),
            one(Delete, NoWhere, "t")
        );
        assert_eq!(
            flags(
                "SET STATEMENT a = 1, b = (SELECT 1) FOR DELETE FROM t WHERE id = 1",
                MY
            ),
            []
        );
    }

    #[test]
    fn case_insensitive() {
        for s in [
            "delete from t",
            "DeLeTe FrOm t",
            "uPdAtE t sEt a = 1 wHeRe TrUe",
        ] {
            assert_eq!(flags(s, MY).len(), 1, "{s}");
        }
        assert_eq!(flags("delete from t where id = 1", MY), []);
    }

    #[test]
    fn other_statements_are_never_flagged() {
        for d in BOTH {
            for s in [
                "SELECT * FROM t",
                "INSERT INTO t SELECT * FROM u",
                "CREATE TABLE t (a int)",
                "DROP TABLE t",
                "SELECT 'DELETE FROM t'",
                "",
                "-- DELETE FROM t",
            ] {
                assert_eq!(flags(s, d), [], "{s} ({d:?})");
            }
        }
        assert_eq!(flags("PREPARE p FROM 'DELETE FROM t'", MY), []);
        assert_eq!(flags("DO $$ BEGIN DELETE FROM t; END $$", PG), []);
        assert_eq!(
            flags(
                "CREATE FUNCTION f() RETURNS void AS $$ DELETE FROM t $$ LANGUAGE sql",
                PG
            ),
            []
        );
    }

    #[test]
    fn routine_bodies_are_not_flagged() {
        for s in [
            "CREATE PROCEDURE p() BEGIN DELETE FROM t; UPDATE u SET a = 1; END",
            "CREATE DEFINER=`root`@`localhost` TRIGGER tr AFTER INSERT ON t FOR EACH ROW BEGIN DELETE FROM log; END",
            "CREATE EVENT e ON SCHEDULE EVERY 1 DAY DO BEGIN TRUNCATE t; END",
            "BEGIN NOT ATOMIC DELETE FROM t; END",
            "/*!50003 CREATE*/ /*!50003 TRIGGER tr BEFORE INSERT ON t FOR EACH ROW BEGIN DELETE FROM u; END */",
        ] {
            assert_eq!(flags(s, MY), [], "{s}");
        }
    }

    #[test]
    fn delimiter_pieces_are_judged_one_by_one() {
        // A forgotten `DELIMITER ;`: the server runs each piece.
        assert_eq!(
            flags("SELECT 1; DELETE FROM t", MY),
            one(Delete, NoWhere, "t")
        );
        assert_eq!(
            flags(
                "DELETE FROM t WHERE id = 1; UPDATE u SET a = 1; TRUNCATE v",
                MY
            ),
            vec![
                (Update, NoWhere, "u".to_string()),
                (Truncate, UnboundedReason::Truncate, "v".to_string()),
            ]
        );
        assert_eq!(flags("DELETE FROM t WHERE id = 1; SELECT 1", MY), []);
    }

    #[test]
    fn delimiter_script_through_plan() {
        use crate::plan::{plan, Target};
        let script = "DELIMITER $$
CREATE PROCEDURE p() BEGIN DELETE FROM t; END$$
DELETE FROM u$$
UPDATE v SET a = 1 WHERE id = 2$$
SELECT 1; TRUNCATE w$$
DELIMITER ;
DELETE FROM x WHERE 1=1;";
        let p = plan(script, Target::All, MY);
        let got: Vec<Vec<(WriteOp, String)>> = p
            .stmts
            .iter()
            .map(|s| {
                s.unbounded
                    .iter()
                    .map(|w| (w.op, w.table.clone()))
                    .collect()
            })
            .collect();
        assert_eq!(
            got,
            vec![
                vec![],
                vec![(Delete, "u".to_string())],
                vec![],
                vec![(Truncate, "w".to_string())],
                vec![(Delete, "x".to_string())],
            ]
        );
    }

    #[test]
    fn repeats_across_readings_are_dropped() {
        // Both readings flag the same DELETE.
        assert_eq!(
            flags("DELETE FROM t /*!40101 LIMIT 1 */", MY),
            one(Delete, NoWhere, "t")
        );
        assert!(is_unbounded("DELETE FROM t", MY));
        assert!(!is_unbounded("DELETE FROM t WHERE id = 1", MY));
    }

    #[test]
    fn serde_shape() {
        let w = UnboundedWrite {
            op: Update,
            reason: AlwaysTrue,
            table: "t".into(),
        };
        assert_eq!(
            serde_json::to_string(&w).unwrap(),
            r#"{"op":"update","reason":"always_true","table":"t"}"#
        );
    }
}
